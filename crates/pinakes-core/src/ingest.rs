use crate::{
    config::IngestConfig,
    nexus::{Change, FeedPage, Nexus, Op},
    store,
    telemetry::Metrics,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use std::collections::{BTreeMap, BTreeSet};

pub const REJECT_REASONS: &[&str] = &["author", "label", "object_path", "timestamp", "event"];
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
    store::lock_search_revision(&mut tx).await?;
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
    for line in &feed.page.lines {
        if let Some(reason) = rejection_reason(line, limits) {
            *rejected.entry(reason).or_default() += 1;
            continue;
        }
        // Feed IDs already encode Nexus's identity decision; never fold or migrate them here.
        if !crate::model::valid_id(&line.resource_id)
            || pubky_resource_id::resource_id(&line.uri) != line.resource_id
        {
            bail!("Nexus resource ID disagrees with the identity crate");
        }
        let unit_key = if matches!(line.kind, crate::nexus::Kind::Event) {
            format!(
                "{}:{}",
                line.resource_id,
                line.recurrence_id.as_deref().unwrap_or("")
            )
        } else {
            String::new()
        };
        let old: Option<String> = sqlx::query_scalar(
            "SELECT resource_id FROM reference WHERE author=$1 AND object_path=$2 AND unit_key=$3",
        )
        .bind(&line.author)
        .bind(&line.object_path)
        .bind(&unit_key)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(old) = &old {
            affected.insert(old.clone());
        }
        match line.op {
            Op::Put => {
                let (_, scheme) =
                    pubky_resource_id::normalize_uri(&line.uri).map_err(anyhow::Error::msg)?;
                store::insert_resource(&mut tx, &line.resource_id, &line.uri, &scheme).await?;
                if old.as_deref() != Some(&line.resource_id) {
                    sqlx::query("UPDATE resource SET final=false,refresh_at=now(),next_attempt_at=now(),dead_letter=false,attempts=0 WHERE id=$1 AND final AND resolver<>'geo' AND $2>resolved_at").bind(&line.resource_id).bind(timestamp(Some(line.at))?).execute(&mut *tx).await?;
                }
                sqlx::query(r#"INSERT INTO reference(resource_id,kind,author,label,object_path,recurrence_id,start_at,end_at,created_at,unit_key,event_status)
                    VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                    ON CONFLICT(author,object_path,unit_key) DO UPDATE SET resource_id=excluded.resource_id, kind=excluded.kind,
                    label=excluded.label, recurrence_id=excluded.recurrence_id, start_at=excluded.start_at,
                    end_at=excluded.end_at, created_at=excluded.created_at,event_status=excluded.event_status"#)
                    .bind(&line.resource_id).bind(line.kind.as_str()).bind(&line.author).bind(&line.label).bind(&line.object_path).bind(&line.recurrence_id)
                    .bind(timestamp(line.start)?).bind(timestamp(line.end)?).bind(timestamp(Some(line.at))?).bind(&unit_key).bind(line.status.as_deref().and_then(crate::event::EventStatus::parse).map(|s|s.as_str()))
                    .execute(&mut *tx).await?;
                affected.insert(line.resource_id.clone());
            }
            Op::Del => {
                sqlx::query(
                    "DELETE FROM reference WHERE author=$1 AND object_path=$2 AND unit_key=$3",
                )
                .bind(&line.author)
                .bind(&line.object_path)
                .bind(&unit_key)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    store::recompute_scores(&mut tx, &affected.into_iter().collect::<Vec<_>>()).await?;
    sqlx::query("UPDATE feed_cursor SET cursor=$2, epoch=$3, feed_length=$4, updated_at=now() WHERE nexus=$1")
        .bind(nexus).bind(feed.page.next_cursor).bind(epoch).bind(feed.length).execute(&mut *tx).await?;
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
