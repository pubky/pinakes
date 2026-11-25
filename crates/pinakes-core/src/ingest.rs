use crate::{
    config::IngestConfig,
    nexus::{Change, FeedPage, Nexus, Op},
    store,
    telemetry::Metrics,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use std::collections::BTreeSet;

pub const REJECT_REASONS: &[&str] = &[
    "unknown_kind",
    "malformed",
    "author",
    "label",
    "object_path",
    "timestamp",
    "uri",
    "never_fetched",
    "collection_items",
    "panic",
];
fn rejection_reason(line: &Change, limits: &IngestConfig) -> Option<&'static str> {
    if !crate::nexus::valid_author(&line.author) {
        Some("author")
    } else if line.label.as_ref().is_some_and(|label| {
        label.is_empty() || label.len() > limits.max_label_bytes || label.contains('\0')
    }) {
        Some("label")
    } else if line.object_path.is_empty()
        || line.object_path.len() > limits.max_object_path_bytes
        || line.object_path.contains('\0')
    {
        Some("object_path")
    } else if invalid_timestamp(line.at) {
        Some("timestamp")
    } else {
        None
    }
}

fn invalid_timestamp(ms: i64) -> bool {
    DateTime::<Utc>::from_timestamp_millis(ms).is_none_or(|t| {
        use chrono::Datelike;
        !(1..=9999).contains(&t.year())
    })
}

#[derive(Clone, Debug, FromRow)]
pub struct Cursor {
    pub cursor: i64,
    pub epoch: Option<String>,
}
/// The key of a feed's cursor row: `nexus_url` without a trailing `/`, the same URL to the
/// Nexus client.
pub fn feed_key(nexus: &str) -> &str {
    nexus.trim_end_matches('/')
}
pub async fn cursor(pool: &PgPool, nexus: &str) -> Result<Cursor> {
    let nexus = feed_key(nexus);
    sqlx::query("INSERT INTO feed_cursor(nexus) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(nexus)
        .execute(pool)
        .await?;
    Ok(
        sqlx::query_as("SELECT cursor, epoch FROM feed_cursor WHERE nexus=$1")
            .bind(nexus)
            .fetch_one(pool)
            .await?,
    )
}
/// The section of docs/operations.md that says how to move a database to another `nexus_url`.
pub const CHANGING_NEXUS: &str = "docs/operations.md, \"Changing nexus_url\"";
/// Refuses to read `nexus` into a database that holds the cursor of another Nexus feed and none
/// of this one: the references are the projection of one feed, and reading a second on top of
/// them would mix both. The error names the other feeds by host and the documented step
/// ([`CHANGING_NEXUS`]). A database the operator prepared, with a cursor row for `nexus`, starts;
/// another feed's row left beside it is logged, by host, at each start. A row under another
/// spelling of `nexus` (a trailing `/`) is the same feed: it is moved to the key [`feed_key`]
/// gives, or dropped beside a row already there.
pub async fn check_feed(pool: &PgPool, nexus: &str) -> Result<()> {
    let key = feed_key(nexus);
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::INGEST)
        .execute(&mut *tx)
        .await?;
    let rows: Vec<String> = sqlx::query_scalar("SELECT nexus FROM feed_cursor ORDER BY nexus")
        .fetch_all(&mut *tx)
        .await?;
    let (same, others): (Vec<&String>, Vec<&String>) =
        rows.iter().partition(|row| feed_key(row) == key);
    // The host only, as every log line names a URL (docs/operations.md).
    let hosts = others
        .iter()
        .map(|url| crate::resolver::log_host(url))
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    if !others.is_empty() && same.is_empty() {
        bail!(
            "this database holds the feed cursor of another Nexus ({hosts}) and none of nexus_url ({}): its references were read from that feed. Before reading this one, reset the projection, or give this nexus_url a cursor row ({CHANGING_NEXUS})",
            crate::resolver::log_host(nexus),
        );
    }
    if !others.is_empty() {
        tracing::warn!(other_feeds = %hosts, "this database still holds a feed cursor for another nexus_url beside the configured one; delete its row ({CHANGING_NEXUS})");
    }
    let spelt: Vec<&String> = same.into_iter().filter(|row| *row != key).collect();
    if !spelt.is_empty() {
        let kept = rows.iter().any(|row| row == key);
        for (i, row) in spelt.into_iter().enumerate() {
            let query = if kept || i > 0 {
                sqlx::query("DELETE FROM feed_cursor WHERE nexus=$1").bind(row)
            } else {
                sqlx::query("UPDATE feed_cursor SET nexus=$2 WHERE nexus=$1")
                    .bind(row)
                    .bind(key)
            };
            query.execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}
/// What `apply` did with a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Applied {
    /// The page's lines and the cursor past them are committed.
    Batch,
    /// Another poller moved the cursor first: nothing was written.
    Lost,
    /// The feed started over: the references are cleared and the cursor is at zero.
    Reset,
}
/// Returns false when another poller won the cursor or a reset needs a fresh page zero.
///
/// A page that starts the feed over ([`starts_over`]) resets at once here: the caller has
/// confirmed it. The daemon's poll ([`poll_once`]) confirms one with a later read first.
///
/// Each line runs in a savepoint: a line whose work panics rolls back alone and is skipped,
/// counted as rejected (`panic`) and logged by feed position only, and the cursor moves past it.
/// A panic in the batch's own work after the lines is an error (`pinakes_ingest_panics_total`):
/// the batch rolls back and the cursor holds.
pub async fn apply(
    pool: &PgPool,
    nexus: &str,
    requested: &Cursor,
    feed: &FeedPage,
    limits: &IngestConfig,
    metrics: &Metrics,
) -> Result<bool> {
    Ok(apply_page(pool, nexus, requested, feed, limits, metrics)
        .await?
        .0
        == Applied::Batch)
}
async fn apply_page(
    pool: &PgPool,
    nexus: &str,
    requested: &Cursor,
    feed: &FeedPage,
    limits: &IngestConfig,
    metrics: &Metrics,
) -> Result<(Applied, Vec<String>)> {
    let key = feed_key(nexus);
    let mut tx = pool.begin().await?;
    // Reference state is a projection of one configured Nexus; serialize rebuilds with batches.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::INGEST)
        .execute(&mut *tx)
        .await?;
    let saved: Cursor =
        sqlx::query_as("SELECT cursor, epoch FROM feed_cursor WHERE nexus=$1 FOR UPDATE")
            .bind(key)
            .fetch_one(&mut *tx)
            .await?;
    if saved.cursor != requested.cursor || saved.epoch != requested.epoch {
        return Ok((Applied::Lost, vec![]));
    }
    let epoch = feed.epoch.as_ref().or(saved.epoch.as_ref());
    let reset = starts_over(&saved, feed);
    // A batch locks its lines' resources in feed order, and may assign aliases: it runs
    // alongside other writers but not other alias assignments. A reset touches every resource.
    let order = if reset {
        store::WriteOrder::Exclusive
    } else {
        store::WriteOrder::Shared
    };
    store::lock_writers(&mut tx, order).await?;
    crate::alias::lock(&mut tx).await?;
    if reset {
        // Every resource row is rewritten first: `TRUNCATE` takes its tables' exclusive locks,
        // which then last only for the cursor update and the commit, not for the rewrite.
        sqlx::query("UPDATE resource SET orphaned_at=coalesce(orphaned_at,now())")
            .execute(&mut *tx)
            .await?;
        sqlx::query("TRUNCATE reference, resource_score")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE feed_cursor SET cursor=0, epoch=$2, feed_length=$3, updated_at=now(),resets=resets+1 WHERE nexus=$1")
            .bind(key).bind(epoch).bind(feed.length).execute(&mut *tx).await?;
        tx.commit().await?;
        // The host only, as every log line names a URL (docs/operations.md).
        tracing::warn!(
            nexus = %crate::resolver::log_host(nexus),
            "Nexus feed reset; rebuilding references"
        );
        return Ok((Applied::Reset, vec![]));
    }
    // The page's lines, those skipped while parsing included: the cursor moves past them all.
    let read = feed.page.read();
    if read > crate::nexus::FEED_PAGE
        || feed.page.next_cursor != requested.cursor + read as i64
        || feed.page.next_cursor > feed.length
    {
        bail!("invalid Nexus cursor progression");
    }
    let mut affected = BTreeSet::new();
    let mut aliased = vec![];
    let mut rejected = feed.page.skipped.clone();
    for (i, line) in feed.page.lines.iter().enumerate() {
        let mut savepoint = sqlx::Connection::begin(&mut *tx).await?;
        let mut touched = BTreeSet::new();
        let mut alias = None;
        let work = std::panic::AssertUnwindSafe(apply_line(
            &mut savepoint,
            line,
            limits,
            &mut touched,
            &mut alias,
        ));
        match work.catch_unwind().await {
            Ok(outcome) => {
                let reason = outcome?;
                savepoint.commit().await?;
                affected.extend(touched);
                aliased.extend(alias);
                if let Some(reason) = reason {
                    *rejected.entry(reason).or_default() += 1;
                }
            }
            Err(_) => {
                // Only this line's writes go; the position only, never the line's content.
                savepoint.rollback().await?;
                *rejected.entry("panic").or_default() += 1;
                tracing::error!(
                    nexus = %crate::resolver::log_host(nexus),
                    position = requested.cursor + i as i64,
                    "a Nexus feed line panicked; skipped"
                );
            }
        }
    }
    let finish = std::panic::AssertUnwindSafe(async {
        #[cfg(test)]
        tests::maybe_panic_after_lines(feed);
        store::recompute_scores(&mut tx, &affected.into_iter().collect::<Vec<_>>()).await?;
        sqlx::query("UPDATE feed_cursor SET cursor=$2, epoch=$3, feed_length=$4, updated_at=now() WHERE nexus=$1")
            .bind(key).bind(feed.page.next_cursor).bind(epoch).bind(feed.length).execute(&mut *tx).await?;
        anyhow::Ok(())
    });
    let finished = finish.catch_unwind().await.map_err(|_| {
        metrics.ingest_panics.inc();
        anyhow::anyhow!("a Nexus batch's work after its lines panicked; the cursor holds")
    })?;
    finished?;
    crate::alias::record(&mut tx, aliased).await?;
    tx.commit().await?;
    // Count only after cursor commit: rollbacks and losing concurrent pollers
    // must not count the same rejected line on each retry.
    for (reason, count) in rejected {
        metrics
            .ingest_rejected
            .with_label_values(&[reason])
            .inc_by(count);
        tracing::warn!(reason, count, "rejected Nexus feed lines");
    }
    // The links the batch's new references name, for the catalogue join.
    let referenced = feed
        .page
        .lines
        .iter()
        .filter(|line| matches!(line.op, Op::Put))
        .filter_map(|line| crate::model::identity(&line.uri).ok().map(|(id, _, _)| id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok((Applied::Batch, referenced))
}
/// One line's work inside the batch's transaction: the reason when it is rejected.
async fn apply_line(
    tx: &mut Transaction<'_, Postgres>,
    line: &Change,
    limits: &IngestConfig,
    affected: &mut BTreeSet<String>,
    alias: &mut Option<crate::alias::Line>,
) -> Result<Option<&'static str>> {
    if let Some(reason) = rejection_reason(line, limits) {
        return Ok(Some(reason));
    }
    // The line's URI is the contract with Nexus; the ID is Pinakes's own, computed here.
    let Ok((id, uri, _)) = crate::model::identity(&line.uri) else {
        return Ok(Some("uri"));
    };
    let post = matches!(line.kind, crate::nexus::Kind::Post);
    if post {
        if let Some(reason) = post_rejection(line, &uri) {
            return Ok(Some(reason));
        }
    }
    let (object_path, unit_key) = reference_key(line);
    let old: Option<String> = sqlx::query_scalar(
        "SELECT resource_id FROM reference WHERE author=$1 AND object_path=$2 AND unit_key=$3",
    )
    .bind(&line.author)
    .bind(object_path)
    .bind(&unit_key)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(old) = &old {
        affected.insert(old.clone());
    }
    let collection = matches!(line.kind, crate::nexus::Kind::Collection);
    // A collection holds at most `collections::ITEMS` links (the specs' cap): a new item past
    // it is refused, so every read of one post's items is bounded.
    if collection
        && matches!(line.op, Op::Put)
        && old.is_none()
        && crate::collections::full(tx, &line.author, &line.object_path).await?
    {
        return Ok(Some("collection_items"));
    }
    match line.op {
        Op::Put => {
            *alias = store::insert_resource(tx, &id, &uri).await?;
            #[cfg(test)]
            tests::maybe_panic(line);
            if old.as_deref() != Some(&id) {
                // A new reference reopens a finished card, and revives a dead letter: the line's
                // own, or, when the line names an alias, its root, which holds the card. Only a
                // reference made after the card's last read (a dead letter never read, after
                // the link was first stored): a replayed feed revives only links referenced since.
                sqlx::query("UPDATE resource SET final=false,refresh_at=now(),next_attempt_at=now(),dead_letter=false,attempts=0,timeout_streak=0 WHERE id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1) AND resolver<>'geo' AND ((final AND $2>resolved_at) OR (dead_letter AND $2>coalesce(resolved_at,created_at)))").bind(&id).bind(timestamp(Some(line.at))?).execute(&mut **tx).await?;
            }
            if post {
                expedite(tx, &id, line.at).await?;
            }
            sqlx::query(r#"INSERT INTO reference(resource_id,kind,author,label,object_path,created_at,unit_key)
                VALUES($1,$2,$3,$4,$5,$6,$7)
                ON CONFLICT(author,object_path,unit_key) DO UPDATE SET resource_id=excluded.resource_id, kind=excluded.kind,
                label=excluded.label, created_at=excluded.created_at"#)
                .bind(&id).bind(line.kind.as_str()).bind(&line.author).bind(&line.label).bind(object_path)
                .bind(timestamp(Some(line.at))?).bind(&unit_key)
                .execute(&mut **tx).await?;
            affected.insert(id.clone());
            if collection {
                affected
                    .extend(crate::collections::settle(tx, &line.author, &line.object_path).await?);
            }
        }
        Op::Del => {
            sqlx::query("DELETE FROM reference WHERE author=$1 AND object_path=$2 AND unit_key=$3")
                .bind(&line.author)
                .bind(object_path)
                .bind(&unit_key)
                .execute(&mut **tx)
                .await?;
            // The post's card is read again: in its latest remaining version, or gone.
            if post {
                expedite(tx, &id, line.at).await?;
            }
        }
    }
    Ok(None)
}
/// The key of the reference a line puts or deletes, `(object_path, unit_key)` beside its author.
///
/// A tag is the author's label on a link: Nexus keys it by author, `uri` (its normalized
/// Resource URI) and label, and its lines' `object_path` (the tag file the event concerns) is for
/// information only. A del can name another file than the put did for the same tag (a user's
/// `pubky.app` tags rewritten to `social/v1`, two files on one tag), so a tag's row is keyed by
/// label and link alone, under the fixed path [`TAG_PATH`]. A post's version names itself and is
/// keyed by its file. A post's version can also name several links: one reply, its embeds and a
/// collection's items, each a line of its own, which Nexus keys by kind and link besides the file;
/// those key on both here, so an item put or dropped is that item alone. The link (and a tag's
/// label) is hashed: the key stays short beside the path.
fn reference_key(line: &Change) -> (&str, String) {
    use crate::nexus::Kind;
    let short = |hash: blake3::Hash| hash.to_hex()[..32].to_owned();
    match line.kind {
        Kind::Reply | Kind::Embed | Kind::Collection => (
            line.object_path.as_str(),
            format!(
                "{}:{}",
                line.kind.as_str(),
                short(blake3::hash(line.uri.as_bytes()))
            ),
        ),
        Kind::Tag => {
            // Length-prefixed, so no label and link pair hashes as another's.
            let mut hasher = blake3::Hasher::new();
            match &line.label {
                Some(label) => hasher
                    .update(&[1])
                    .update(&(label.len() as u64).to_le_bytes())
                    .update(label.as_bytes()),
                None => hasher.update(&[0]),
            };
            hasher.update(line.uri.as_bytes());
            (TAG_PATH, format!("tag:{}", short(hasher.finalize())))
        }
        Kind::Post => (line.object_path.as_str(), String::new()),
    }
}
/// The `object_path` of every tag's reference row: a tag is keyed by its label and link
/// (`unit_key`), never by a file ([`reference_key`]). No other kind's row has it.
pub const TAG_PATH: &str = "";
/// Why a `post` line is rejected: its URI must be a post in its author's storage
/// (`pubky://<author>/pub/<namespace>/v1/posts/{id}`), its object path a version file directly in
/// that post (`…/{id}/{editId}.json`). A post in a namespace Pinakes never fetches
/// (`pubky_apps::never_fetched`: the social specs' events and rsvps are Nexus's, `pubky.app`'s too)
/// is neither fetched nor indexed (`never_fetched`), however its namespace is spelt.
fn post_rejection(line: &Change, uri: &str) -> Option<&'static str> {
    let Some(path) = uri
        .strip_prefix("pubky://")
        .and_then(|rest| rest.strip_prefix(line.author.as_str()))
        .filter(|path| {
            matches!(path.split('/').collect::<Vec<_>>()[..],
                ["", "pub", namespace, "v1", "posts", id] if !namespace.is_empty() && !id.is_empty())
        })
    else {
        return Some("uri");
    };
    // The namespace as the resolver reads it, decoded: `%73ocial` is `social`.
    let segments = url::Url::parse(uri)
        .ok()
        .and_then(|url| crate::resolver::path_segments(&url));
    match segments.as_deref() {
        Some([_, namespace, ..]) if crate::resolver::pubky_apps::never_fetched(namespace) => {
            return Some("never_fetched")
        }
        Some(_) => {}
        None => return Some("uri"),
    }
    let file = line
        .object_path
        .strip_prefix(path)
        .and_then(|rest| rest.strip_prefix('/'));
    match file {
        Some(file)
            if file.len() > ".json".len() && file.ends_with(".json") && !file.contains('/') =>
        {
            None
        }
        _ => Some("object_path"),
    }
}
/// A post's card is due now: a new version, or one deleted, changes it whatever its TTL, and
/// whatever its earlier reads did (a post never read, dead-lettered after its homeserver failed,
/// is read again too). A read in flight is fenced as `reextract` fences it (the generation bump):
/// its answer, which may be the older version, is never saved over the newer one's due time. A
/// line older than the card's last read changes nothing.
async fn expedite(tx: &mut Transaction<'_, Postgres>, id: &str, at: i64) -> Result<()> {
    sqlx::query("UPDATE resource SET final=false,refresh_at=now(),next_attempt_at=now(),dead_letter=false,attempts=0,timeout_streak=0,lease_until=NULL,priority_at=NULL,generation=generation+1 WHERE id=$1 AND (resolved_at IS NULL OR $2>resolved_at)")
        .bind(id)
        .bind(timestamp(Some(at))?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
fn timestamp(ms: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    ms.map(|ms| DateTime::from_timestamp_millis(ms).context("invalid feed timestamp"))
        .transpose()
}
/// Whether `feed`, read from `requested`, says the feed started over: its epoch is another than
/// the saved one, or it is shorter than the cursor.
pub fn starts_over(requested: &Cursor, feed: &FeedPage) -> bool {
    matches!((&requested.epoch, &feed.epoch), (Some(a), Some(b)) if a != b)
        || requested.cursor > feed.length
}
/// The feed answer that would start the references over, held until a later read confirms it.
///
/// One answer with another epoch (or shorter than the cursor) clears nothing: the next read, at
/// least `wait` later and from the same cursor, must report the same epoch and still start over.
/// An answer in between that does not start over forgets it. Kept in memory: after a restart
/// the confirmation starts again.
#[derive(Debug)]
pub struct Resets {
    wait: std::time::Duration,
    seen: Option<(Cursor, Option<String>, std::time::Instant)>,
}
impl Resets {
    /// Confirmation at least `wait` after the first answer (the daemon's `limits.poll_secs`).
    pub fn new(wait: std::time::Duration) -> Self {
        Self { wait, seen: None }
    }
    /// Whether `feed`, which starts over, confirms the answer seen before it.
    fn confirms(&mut self, requested: &Cursor, feed: &FeedPage) -> bool {
        let same = |(cursor, epoch, _): &(Cursor, Option<String>, std::time::Instant)| {
            cursor.cursor == requested.cursor
                && cursor.epoch == requested.epoch
                && *epoch == feed.epoch
        };
        match &self.seen {
            Some(seen) if same(seen) => {
                if seen.2.elapsed() < self.wait {
                    return false;
                }
                self.seen = None;
                true
            }
            _ => {
                self.seen = Some((
                    requested.clone(),
                    feed.epoch.clone(),
                    std::time::Instant::now(),
                ));
                false
            }
        }
    }
}
/// What one poll did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Poll {
    /// The feed lines the committed batch read, those it skipped or rejected included: how far
    /// the cursor moved. 0 when no batch was committed.
    pub lines: usize,
    /// Whether the next poll follows at once: the cursor moved, so more may wait, or another
    /// poller moved it. False when the feed held nothing new, when an answer that starts the
    /// feed over waits for its confirming read, and after a reset, which waits
    /// `limits.poll_secs` before the feed is read again from zero.
    pub again: bool,
}
/// One read of the feed, applied. An answer that starts the feed over resets the references
/// only when `resets` confirms it ([`Resets`]); until then nothing is written.
pub async fn poll_once(
    pool: &PgPool,
    nexus_url: &str,
    nexus: &Nexus,
    limits: &IngestConfig,
    metrics: &Metrics,
    resets: &mut Resets,
) -> Result<Poll> {
    Ok(
        poll_referenced(pool, nexus_url, nexus, limits, metrics, resets)
            .await?
            .0,
    )
}
/// [`poll_once`], and the resource IDs of the links the committed batch's new references name
/// (the line's spelling: an alias's own ID), for the catalogue join (`catalogue::join::referenced`).
pub async fn poll_referenced(
    pool: &PgPool,
    nexus_url: &str,
    nexus: &Nexus,
    limits: &IngestConfig,
    metrics: &Metrics,
    resets: &mut Resets,
) -> Result<(Poll, Vec<String>)> {
    let cursor = cursor(pool, nexus_url).await?;
    let page = nexus.changes(cursor.cursor).await?;
    if !starts_over(&cursor, &page) {
        resets.seen = None;
    } else if !resets.confirms(&cursor, &page) {
        // The host only, as every log line names a URL (docs/operations.md).
        tracing::warn!(
            nexus = %crate::resolver::log_host(nexus_url),
            "the Nexus feed answered as if it started over; waiting for a confirming read"
        );
        return Ok((
            Poll {
                lines: 0,
                again: false,
            },
            vec![],
        ));
    }
    let (applied, referenced) =
        apply_page(pool, nexus_url, &cursor, &page, limits, metrics).await?;
    Ok((
        match applied {
            Applied::Batch => {
                let lines = page.page.read();
                Poll {
                    lines,
                    again: lines > 0,
                }
            }
            Applied::Lost => Poll {
                lines: 0,
                again: true,
            },
            Applied::Reset => Poll {
                lines: 0,
                again: false,
            },
        },
        referenced,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{Changes, Kind};

    /// Links whose line panics, standing in for a classifier that panics on them.
    static PANICS_ON: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    pub(super) fn maybe_panic(line: &Change) {
        if PANICS_ON.lock().unwrap().contains(&line.uri) {
            panic!("classifier panicked");
        }
    }
    /// A batch naming this link panics in its work after the lines.
    const PANICS_AFTER: &str = "https://panic-after.example/batch";
    pub(super) fn maybe_panic_after_lines(feed: &FeedPage) {
        if feed.page.lines.iter().any(|l| l.uri == PANICS_AFTER) {
            panic!("score recomputation panicked");
        }
    }

    fn line(uri: &str, path: &str) -> Change {
        let (_, uri, _) = crate::model::identity(uri).unwrap();
        Change {
            op: Op::Put,
            uri,
            kind: Kind::Tag,
            author: "y".repeat(52),
            object_path: path.into(),
            label: Some("rust".into()),
            at: Utc::now().timestamp_millis(),
        }
    }

    /// The links the references name, in order.
    async fn tagged(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT r.uri FROM reference f JOIN resource r ON r.id=f.resource_id ORDER BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// A tag's key is its label and link, whatever file the line names; replies, embeds,
    /// collection items and posts keep the keys earlier builds stored: their file, and for the
    /// first three the kind and a hash of the link.
    #[test]
    fn reference_keys() {
        let tag = line("https://example.org/a", "/pub/pubky.app/tags/X");
        let mut moved = tag.clone();
        moved.object_path = "/pub/social/v1/tags/Y".into();
        let (path, key) = reference_key(&tag);
        assert_eq!((path, key.clone()), reference_key(&moved));
        assert_eq!(path, TAG_PATH);
        let mut relabelled = tag.clone();
        relabelled.label = Some("music".into());
        let mut unlabelled = tag.clone();
        unlabelled.label = None;
        let mut elsewhere = tag.clone();
        elsewhere.uri = "https://example.org/b".into();
        let keys: BTreeSet<_> = [&tag, &relabelled, &unlabelled, &elsewhere]
            .map(|l| reference_key(l).1)
            .into();
        assert_eq!(keys.len(), 4);
        // A label that ends where a link starts is not another pair's prefix.
        let mut a = tag.clone();
        a.label = Some("ab".into());
        a.uri = "c".into();
        let mut b = tag.clone();
        b.label = Some("a".into());
        b.uri = "bc".into();
        assert_ne!(reference_key(&a).1, reference_key(&b).1);
        let path = "/pub/social/v1/posts/0034A/0034B.json";
        for kind in [Kind::Reply, Kind::Embed, Kind::Collection] {
            let mut item = line("https://example.org/a", path);
            item.kind = kind;
            let stored = format!(
                "{}:{}",
                item.kind.as_str(),
                &blake3::hash(item.uri.as_bytes()).to_hex()[..32]
            );
            assert_eq!(reference_key(&item), (path, stored));
        }
        let mut post = line("https://example.org/a", path);
        post.kind = Kind::Post;
        assert_eq!(reference_key(&post), (path, String::new()));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_line_that_panics_is_skipped_and_the_cursor_moves_past_it(pool: PgPool) {
        use tracing::instrument::WithSubscriber;
        // A Nexus URL with a token in its path: only its host may reach the log.
        let nexus = "https://u:SECRET@panic.nexus.example.org/private-token?CANARY";
        let bad = "https://panic.example/ingest";
        PANICS_ON.lock().unwrap().push(bad.into());
        let lines = vec![
            line("https://example.org/before", "/a"),
            line(bad, "/b"),
            line("https://example.org/after", "/c"),
            line(bad, "/d"),
        ];
        let saved = cursor(&pool, nexus).await.unwrap();
        let feed = FeedPage {
            page: Changes {
                next_cursor: lines.len() as i64,
                lines,
                skipped: Default::default(),
            },
            epoch: None,
            length: 4,
        };
        let metrics = Metrics::new().unwrap();
        let limits = IngestConfig::default();
        #[derive(Clone, Default)]
        struct Lines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Lines::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || writer.clone())
            .finish();
        let applied = apply(&pool, nexus, &saved, &feed, &limits, &metrics)
            .with_subscriber(subscriber)
            .await;
        assert!(applied.unwrap());
        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let panicked: Vec<serde_json::Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|l| l["fields"]["message"] == "a Nexus feed line panicked; skipped")
            .collect();
        assert_eq!(panicked.len(), 2, "{text}");
        for line in &panicked {
            assert_eq!(line["fields"]["nexus"], "panic.nexus.example.org", "{line}");
        }
        for secret in ["SECRET", "private-token", "CANARY", "https"] {
            assert!(!text.contains(secret), "{secret}: {text}");
        }
        assert_eq!(
            tagged(&pool).await,
            ["https://example.org/after", "https://example.org/before"]
        );
        // What the panicking lines wrote before they panicked rolled back with them alone.
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM resource WHERE uri=$1")
            .bind(bad)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 0);
        assert_eq!(cursor(&pool, nexus).await.unwrap().cursor, 4);
        let panics = metrics.ingest_rejected.with_label_values(&["panic"]).get();
        assert_eq!(panics, 2);
    }

    /// A page that mixes lines this build reads with a line of an unknown kind and malformed ones
    /// is applied: the good lines land, the others are skipped and counted, and the cursor moves
    /// past them all, so one line never stops the feed. Through the real client and parser.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_page_with_unknown_and_malformed_lines_moves_the_cursor_past_them(pool: PgPool) {
        use axum::{routing::get, Router};
        let good = |uri: &str, path: &str| serde_json::to_value(line(uri, path)).unwrap();
        let mut unknown = good("https://example.org/listen", "/pub/music/v1/posts/1");
        unknown["kind"] = "listen".into();
        let mut no_author = good("https://example.org/no-author", "/x");
        no_author.as_object_mut().unwrap().remove("author");
        let mut bad_op = good("https://example.org/bad-op", "/y");
        bad_op["op"] = "upsert".into();
        let page = serde_json::json!({
            "lines": [
                good("https://example.org/first", "/a"),
                unknown,
                no_author,
                "not an object",
                bad_op,
                good("https://example.org/last", "/b"),
            ],
            "next_cursor": 6,
        });
        let app = Router::new().route(
            "/v1/resources/changes",
            get(move || {
                let page = page.clone();
                async move {
                    (
                        [("x-events-epoch", "e1"), ("x-events-length", "6")],
                        axum::Json(page),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let nexus = Nexus::new(&url, None).unwrap();
        let metrics = Metrics::new().unwrap();
        let limits = IngestConfig::default();
        let mut resets = Resets::new(std::time::Duration::ZERO);
        let applied = poll_once(&pool, &url, &nexus, &limits, &metrics, &mut resets)
            .await
            .unwrap();
        // The six lines of the page were read, four of them skipped: more may wait.
        assert_eq!(
            applied,
            Poll {
                lines: 6,
                again: true
            }
        );
        assert_eq!(
            tagged(&pool).await,
            ["https://example.org/first", "https://example.org/last"]
        );
        assert_eq!(cursor(&pool, &url).await.unwrap().cursor, 6);
        let skipped = |reason| metrics.ingest_rejected.with_label_values(&[reason]).get();
        assert_eq!((skipped("unknown_kind"), skipped("malformed")), (1, 3));
        server.abort();
    }

    /// A Nexus feed of one tag line whose epoch header the test sets, and its URL.
    async fn feed_with_epoch(
        epoch: std::sync::Arc<std::sync::Mutex<&'static str>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{extract::Query, routing::get, Router};
        let line = serde_json::to_value(line("https://example.org/epoch", "/a")).unwrap();
        let app = Router::new().route(
            "/v1/resources/changes",
            get(
                move |Query(q): Query<std::collections::HashMap<String, i64>>| async move {
                    let at = q["cursor"];
                    let lines = if at == 0 { vec![line] } else { vec![] };
                    let epoch = *epoch.lock().unwrap();
                    (
                        [("x-events-epoch", epoch), ("x-events-length", "1")],
                        axum::Json(serde_json::json!({ "lines": lines, "next_cursor": 1 })),
                    )
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, server)
    }
    /// Moves the held answer's sighting back by the whole wait, as if the wait had passed: the
    /// tests set a one-minute wait and never sleep.
    fn backdate(resets: &mut Resets) {
        let wait = resets.wait;
        let seen = resets
            .seen
            .as_mut()
            .expect("an answer waits for its confirmation");
        seen.2 = seen
            .2
            .checked_sub(wait)
            .expect("the clock reaches back by the wait");
    }
    /// The references, and the resets the cursor row counts.
    async fn projection(pool: &PgPool) -> (Vec<String>, i64) {
        let resets = sqlx::query_scalar("SELECT resets FROM feed_cursor")
            .fetch_one(pool)
            .await
            .unwrap();
        (tagged(pool).await, resets)
    }

    /// An answer with a new epoch clears nothing by itself: the references go only when a read
    /// at least the wait later reports the same new epoch, and the feed is then read from zero.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_new_epoch_resets_only_once_a_later_read_confirms_it(pool: PgPool) {
        let epoch = std::sync::Arc::new(std::sync::Mutex::new("first"));
        let (url, server) = feed_with_epoch(epoch.clone()).await;
        let nexus = Nexus::new(&url, None).unwrap();
        let (metrics, limits) = (Metrics::new().unwrap(), IngestConfig::default());
        let mut resets = Resets::new(std::time::Duration::from_secs(60));
        macro_rules! poll {
            () => {
                poll_once(&pool, &url, &nexus, &limits, &metrics, &mut resets)
                    .await
                    .unwrap()
            };
        }
        let read = Poll {
            lines: 1,
            again: true,
        };
        let idle = Poll {
            lines: 0,
            again: false,
        };
        assert_eq!(poll!(), read);
        let kept = (vec!["https://example.org/epoch".to_owned()], 0);
        *epoch.lock().unwrap() = "second";
        assert_eq!(poll!(), idle);
        assert_eq!(projection(&pool).await, kept);
        // The same answer again before the wait is over confirms nothing yet.
        assert_eq!(poll!(), idle);
        assert_eq!(projection(&pool).await, kept);
        backdate(&mut resets);
        assert_eq!(poll!(), idle);
        assert_eq!(projection(&pool).await, (vec![], 1));
        let saved = cursor(&pool, &url).await.unwrap();
        assert_eq!((saved.cursor, saved.epoch.as_deref()), (0, Some("second")));
        assert_eq!(poll!(), read);
        server.abort();
    }

    /// One odd answer followed by the old epoch truncates nothing, and is forgotten: a later
    /// answer with another epoch waits for its own confirmation.
    #[sqlx::test(migrations = "../../migrations")]
    async fn one_odd_epoch_followed_by_the_old_one_resets_nothing(pool: PgPool) {
        let epoch = std::sync::Arc::new(std::sync::Mutex::new("first"));
        let (url, server) = feed_with_epoch(epoch.clone()).await;
        let nexus = Nexus::new(&url, None).unwrap();
        let (metrics, limits) = (Metrics::new().unwrap(), IngestConfig::default());
        let mut resets = Resets::new(std::time::Duration::from_secs(60));
        macro_rules! poll {
            () => {
                poll_once(&pool, &url, &nexus, &limits, &metrics, &mut resets)
                    .await
                    .unwrap()
            };
        }
        poll!();
        let kept = (vec!["https://example.org/epoch".to_owned()], 0);
        *epoch.lock().unwrap() = "odd";
        poll!();
        backdate(&mut resets);
        *epoch.lock().unwrap() = "first";
        let idle = Poll {
            lines: 0,
            again: false,
        };
        assert_eq!(poll!(), idle);
        assert_eq!(projection(&pool).await, kept);
        // The odd answer again, past the wait of its first sighting: a sighting of its own.
        *epoch.lock().unwrap() = "odd";
        assert_eq!(poll!(), idle);
        assert_eq!(projection(&pool).await, kept);
        let saved = cursor(&pool, &url).await.unwrap();
        assert_eq!((saved.cursor, saved.epoch.as_deref()), (1, Some("first")));
        server.abort();
    }

    /// A panic in the batch's work after its lines is an error: counted, the batch rolled back
    /// and the cursor held, so the operator sees a stalled feed instead of a silent retry loop.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_panic_after_the_lines_holds_the_cursor_and_is_counted(pool: PgPool) {
        let lines = vec![
            line("https://example.org/kept-back", "/a"),
            line(PANICS_AFTER, "/b"),
        ];
        let saved = cursor(&pool, "fixture").await.unwrap();
        let feed = FeedPage {
            page: Changes {
                next_cursor: lines.len() as i64,
                lines,
                skipped: Default::default(),
            },
            epoch: None,
            length: 2,
        };
        let metrics = Metrics::new().unwrap();
        let limits = IngestConfig::default();
        let error = apply(&pool, "fixture", &saved, &feed, &limits, &metrics)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("the cursor holds"),
            "{error:#}"
        );
        assert_eq!(metrics.ingest_panics.get(), 1);
        assert_eq!(cursor(&pool, "fixture").await.unwrap().cursor, 0);
        let references: i64 = sqlx::query_scalar("SELECT count(*) FROM reference")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(references, 0);
    }
}
