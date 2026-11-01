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
pub async fn cursor(pool: &PgPool, nexus: &str) -> Result<Cursor> {
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
/// Returns false when another poller won the cursor or a reset needs a fresh page zero.
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
    let mut tx = pool.begin().await?;
    // Reference state is a projection of one configured Nexus; serialize rebuilds with batches.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::INGEST)
        .execute(&mut *tx)
        .await?;
    let saved: Cursor =
        sqlx::query_as("SELECT cursor, epoch FROM feed_cursor WHERE nexus=$1 FOR UPDATE")
            .bind(nexus)
            .fetch_one(&mut *tx)
            .await?;
    if saved.cursor != requested.cursor || saved.epoch != requested.epoch {
        return Ok(false);
    }
    let changed = matches!((&saved.epoch, &feed.epoch), (Some(a),Some(b)) if a != b);
    let epoch = feed.epoch.as_ref().or(saved.epoch.as_ref());
    let reset = changed || requested.cursor > feed.length;
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
        sqlx::query("TRUNCATE reference, resource_score")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE resource SET orphaned_at=coalesce(orphaned_at,now())")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE feed_cursor SET cursor=0, epoch=$2, feed_length=$3, updated_at=now(),resets=resets+1 WHERE nexus=$1")
            .bind(nexus).bind(epoch).bind(feed.length).execute(&mut *tx).await?;
        tx.commit().await?;
        // The host only, as every log line names a URL (docs/operations.md).
        tracing::warn!(
            nexus = %crate::resolver::log_host(nexus),
            "Nexus feed reset; rebuilding references"
        );
        return Ok(false);
    }
    // The page's lines, those skipped while parsing included: the cursor moves past them all.
    let read = feed.page.read();
    if read > 1000
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
            .bind(nexus).bind(feed.page.next_cursor).bind(epoch).bind(feed.length).execute(&mut *tx).await?;
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
    Ok(true)
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
    // One reference per object (a tag file, a post version naming itself), but a post's version
    // can name several links: one reply, its embeds and a collection's items, each a line of its
    // own, which Nexus keys by kind and link besides the file. Those key on both here, so an item
    // put or dropped is that item alone. The link is hashed: the key stays short beside the path.
    let unit_key = match line.kind {
        crate::nexus::Kind::Reply | crate::nexus::Kind::Embed | crate::nexus::Kind::Collection => {
            format!(
                "{}:{}",
                line.kind.as_str(),
                &blake3::hash(line.uri.as_bytes()).to_hex()[..32]
            )
        }
        crate::nexus::Kind::Tag | crate::nexus::Kind::Post => String::new(),
    };
    let old: Option<String> = sqlx::query_scalar(
        "SELECT resource_id FROM reference WHERE author=$1 AND object_path=$2 AND unit_key=$3",
    )
    .bind(&line.author)
    .bind(&line.object_path)
    .bind(&unit_key)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(old) = &old {
        affected.insert(old.clone());
    }
    match line.op {
        Op::Put => {
            *alias = store::insert_resource(tx, &id, &uri).await?;
            #[cfg(test)]
            tests::maybe_panic(line);
            if old.as_deref() != Some(&id) {
                sqlx::query("UPDATE resource SET final=false,refresh_at=now(),next_attempt_at=now(),dead_letter=false,attempts=0 WHERE id=$1 AND final AND resolver<>'geo' AND $2>resolved_at").bind(&id).bind(timestamp(Some(line.at))?).execute(&mut **tx).await?;
            }
            if post {
                expedite(tx, &id, line.at).await?;
            }
            sqlx::query(r#"INSERT INTO reference(resource_id,kind,author,label,object_path,created_at,unit_key)
                VALUES($1,$2,$3,$4,$5,$6,$7)
                ON CONFLICT(author,object_path,unit_key) DO UPDATE SET resource_id=excluded.resource_id, kind=excluded.kind,
                label=excluded.label, created_at=excluded.created_at"#)
                .bind(&id).bind(line.kind.as_str()).bind(&line.author).bind(&line.label).bind(&line.object_path)
                .bind(timestamp(Some(line.at))?).bind(&unit_key)
                .execute(&mut **tx).await?;
            affected.insert(id.clone());
        }
        Op::Del => {
            sqlx::query("DELETE FROM reference WHERE author=$1 AND object_path=$2 AND unit_key=$3")
                .bind(&line.author)
                .bind(&line.object_path)
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
/// Why a `post` line is rejected: its URI must be a post in its author's storage
/// (`pubky://<author>/pub/<namespace>/v1/posts/{id}`), its object path a version file directly in
/// that post (`…/{id}/{editId}.json`). A post in a namespace Pinakes never fetches
/// (`pubky_apps::never_fetched`: the social specs' events and rsvps are Nexus's, `pubky.app`'s too,
/// and the old Mapky and Eventky layouts are disregarded) is neither fetched nor indexed
/// (`never_fetched`), however its namespace is spelt.
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
pub async fn poll_once(
    pool: &PgPool,
    nexus_url: &str,
    nexus: &Nexus,
    limits: &IngestConfig,
    metrics: &Metrics,
) -> Result<usize> {
    let cursor = cursor(pool, nexus_url).await?;
    let page = nexus.changes(cursor.cursor).await?;
    let applied = apply(pool, nexus_url, &cursor, &page, limits, metrics).await?;
    Ok(if applied { page.page.lines.len() } else { 1000 })
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

    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../tests/fixtures/require_h3.sql")
    )]
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
        let paths: Vec<String> =
            sqlx::query_scalar("SELECT object_path FROM reference ORDER BY object_path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(paths, ["/a", "/c"]);
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
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../tests/fixtures/require_h3.sql")
    )]
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
            "/v0/resources/changes",
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
        let applied = poll_once(&pool, &url, &nexus, &limits, &metrics)
            .await
            .unwrap();
        assert_eq!(applied, 2);
        let paths: Vec<String> =
            sqlx::query_scalar("SELECT object_path FROM reference ORDER BY object_path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(paths, ["/a", "/b"]);
        assert_eq!(cursor(&pool, &url).await.unwrap().cursor, 6);
        let skipped = |reason| metrics.ingest_rejected.with_label_values(&[reason]).get();
        assert_eq!((skipped("unknown_kind"), skipped("malformed")), (1, 3));
        server.abort();
    }

    /// A panic in the batch's work after its lines is an error: counted, the batch rolled back
    /// and the cursor held, so the operator sees a stalled feed instead of a silent retry loop.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../tests/fixtures/require_h3.sql")
    )]
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
