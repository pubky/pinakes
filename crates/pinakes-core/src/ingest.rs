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
use std::collections::{BTreeMap, BTreeSet};

pub const REJECT_REASONS: &[&str] = &[
    "author",
    "label",
    "object_path",
    "timestamp",
    "event",
    "uri",
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
    } else if line
        .recurrence_id
        .as_ref()
        .is_some_and(|s| s.len() > 128 || s.contains('\0'))
        || line
            .start
            .into_iter()
            .chain(line.end)
            .any(invalid_timestamp)
        || line.start.zip(line.end).is_some_and(|(a, b)| b < a)
    {
        Some("event")
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
    sqlx::query("SELECT pg_advisory_xact_lock(73822916)")
        .execute(&mut *tx)
        .await?;
    store::lock_writers(&mut tx).await?;
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
    if changed || requested.cursor > feed.length {
        sqlx::query("TRUNCATE reference, resource_score")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE resource SET orphaned_at=coalesce(orphaned_at,now())")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE feed_cursor SET cursor=0, epoch=$2, feed_length=$3, updated_at=now(),resets=resets+1 WHERE nexus=$1")
            .bind(nexus).bind(epoch).bind(feed.length).execute(&mut *tx).await?;
        tx.commit().await?;
        tracing::warn!(nexus, "Nexus feed reset; rebuilding references");
        return Ok(false);
    }
    if feed.page.lines.len() > 1000
        || feed.page.next_cursor != requested.cursor + feed.page.lines.len() as i64
        || feed.page.next_cursor > feed.length
    {
        bail!("invalid Nexus cursor progression");
    }
    let mut affected = BTreeSet::new();
    let mut rejected = BTreeMap::<&'static str, u64>::new();
    for (i, line) in feed.page.lines.iter().enumerate() {
        let mut savepoint = sqlx::Connection::begin(&mut *tx).await?;
        let mut touched = BTreeSet::new();
        let work =
            std::panic::AssertUnwindSafe(apply_line(&mut savepoint, line, limits, &mut touched));
        match work.catch_unwind().await {
            Ok(outcome) => {
                let reason = outcome?;
                savepoint.commit().await?;
                affected.extend(touched);
                if let Some(reason) = reason {
                    *rejected.entry(reason).or_default() += 1;
                }
            }
            Err(_) => {
                // Only this line's writes go; the position only, never the line's content.
                savepoint.rollback().await?;
                *rejected.entry("panic").or_default() += 1;
                tracing::error!(
                    nexus,
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
) -> Result<Option<&'static str>> {
    if let Some(reason) = rejection_reason(line, limits) {
        return Ok(Some(reason));
    }
    // The line's URI is the contract with Nexus; the ID is Pinakes's own, computed here.
    let Ok((id, uri, _)) = crate::model::identity(&line.uri) else {
        return Ok(Some("uri"));
    };
    let unit_key = if matches!(line.kind, crate::nexus::Kind::Event) {
        format!("{}:{}", id, line.recurrence_id.as_deref().unwrap_or(""))
    } else {
        String::new()
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
            store::insert_resource(tx, &id, &uri).await?;
            #[cfg(test)]
            tests::maybe_panic(line);
            if old.as_deref() != Some(&id) {
                sqlx::query("UPDATE resource SET final=false,refresh_at=now(),next_attempt_at=now(),dead_letter=false,attempts=0 WHERE id=$1 AND final AND resolver<>'geo' AND $2>resolved_at").bind(&id).bind(timestamp(Some(line.at))?).execute(&mut **tx).await?;
            }
            sqlx::query(r#"INSERT INTO reference(resource_id,kind,author,label,object_path,recurrence_id,start_at,end_at,created_at,unit_key,event_status)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                ON CONFLICT(author,object_path,unit_key) DO UPDATE SET resource_id=excluded.resource_id, kind=excluded.kind,
                label=excluded.label, recurrence_id=excluded.recurrence_id, start_at=excluded.start_at,
                end_at=excluded.end_at, created_at=excluded.created_at,event_status=excluded.event_status"#)
                .bind(&id).bind(line.kind.as_str()).bind(&line.author).bind(&line.label).bind(&line.object_path).bind(&line.recurrence_id)
                .bind(timestamp(line.start)?).bind(timestamp(line.end)?).bind(timestamp(Some(line.at))?).bind(&unit_key).bind(line.status.as_deref().and_then(crate::event::EventStatus::parse).map(|s|s.as_str()))
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
        }
    }
    Ok(None)
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
            recurrence_id: None,
            status: None,
            start: None,
            end: None,
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_line_that_panics_is_skipped_and_the_cursor_moves_past_it(pool: PgPool) {
        let bad = "https://panic.example/ingest";
        PANICS_ON.lock().unwrap().push(bad.into());
        let lines = vec![
            line("https://example.org/before", "/a"),
            line(bad, "/b"),
            line("https://example.org/after", "/c"),
            line(bad, "/d"),
        ];
        let saved = cursor(&pool, "fixture").await.unwrap();
        let feed = FeedPage {
            page: Changes {
                next_cursor: lines.len() as i64,
                lines,
            },
            epoch: None,
            length: 4,
        };
        let metrics = Metrics::new().unwrap();
        let limits = IngestConfig::default();
        let applied = apply(&pool, "fixture", &saved, &feed, &limits, &metrics).await;
        assert!(applied.unwrap());
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
        assert_eq!(cursor(&pool, "fixture").await.unwrap().cursor, 4);
        let panics = metrics.ingest_rejected.with_label_values(&["panic"]).get();
        assert_eq!(panics, 2);
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
