use crate::{
    nexus::{FeedPage, Nexus, Op},
    store,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use std::collections::BTreeSet;

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
    for line in &feed.page.lines {
        // Feed IDs already encode Nexus's identity decision; never fold or migrate them here.
        if !crate::model::valid_id(&line.resource_id)
            || pubky_resource_id::resource_id(&line.uri) != line.resource_id
        {
            bail!("Nexus resource ID disagrees with the identity crate");
        }
        let old: Option<String> = sqlx::query_scalar(
            "SELECT resource_id FROM reference WHERE author=$1 AND object_path=$2",
        )
        .bind(&line.author)
        .bind(&line.object_path)
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
                sqlx::query(r#"INSERT INTO reference(resource_id,kind,author,label,object_path,recurrence_id,start_at,end_at,created_at)
                    VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)
                    ON CONFLICT(author,object_path) DO UPDATE SET resource_id=excluded.resource_id, kind=excluded.kind,
                    label=excluded.label, recurrence_id=excluded.recurrence_id, start_at=excluded.start_at,
                    end_at=excluded.end_at, created_at=excluded.created_at"#)
                    .bind(&line.resource_id).bind(line.kind.as_str()).bind(&line.author).bind(&line.label).bind(&line.object_path).bind(&line.recurrence_id)
                    .bind(timestamp(line.start)?).bind(timestamp(line.end)?).bind(timestamp(Some(line.at))?)
                    .execute(&mut *tx).await?;
                affected.insert(line.resource_id.clone());
            }
            Op::Del => {
                sqlx::query("DELETE FROM reference WHERE author=$1 AND object_path=$2")
                    .bind(&line.author)
                    .bind(&line.object_path)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    store::recompute_scores(&mut tx, &affected.into_iter().collect::<Vec<_>>()).await?;
    sqlx::query("UPDATE feed_cursor SET cursor=$2, epoch=$3, feed_length=$4, updated_at=now() WHERE nexus=$1")
        .bind(nexus).bind(feed.page.next_cursor).bind(epoch).bind(feed.length).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
fn timestamp(ms: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    ms.map(|ms| DateTime::from_timestamp_millis(ms).context("invalid feed timestamp"))
        .transpose()
}
pub async fn poll_once(pool: &PgPool, nexus_url: &str, nexus: &Nexus) -> Result<usize> {
    let cursor = cursor(pool, nexus_url).await?;
    let page = nexus.changes(cursor.cursor).await?;
    let applied = apply(pool, nexus_url, &cursor, &page).await?;
    Ok(if applied { page.page.lines.len() } else { 1000 })
}
