//! What the hint passes (same_event, same_recording) share: their queues' claims, how a change
//! to the pairs is committed, and how a pass picks candidates with room.
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::time::Duration;

/// Candidates a read examines at most, in both hints: those passed over for having no room
/// (`has_room`) count, so a read in a crowd of one recording's or one event's pages is bounded
/// however dense the crowd. A read that runs out with room left stores the position it stopped
/// at and is read again from there.
pub const SCAN: i64 = 100;

/// Commits a change to the pairs with the search revision moved in the same transaction: a
/// search page reads the revision and the pairs in one snapshot (`api::REVISION`), so a page that
/// has not seen the pairs is never answered 304 (`store::note_search_write`).
pub async fn commit_changed(mut tx: sqlx::Transaction<'_, sqlx::Postgres>) -> Result<()> {
    crate::store::note_search_write(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

/// SQL: whether `candidate` has room among the units that name it (the pairs in `table`, whose
/// naming column is `owner`) for a pair from `me` scoring `score`: fewer than `cap` of them rank
/// above it in the order the incoming cap keeps, the score, then the namer's id. A candidate
/// without room is never named, so the cap pushes out only a pair that a better one displaces,
/// and a crowd settles instead of trimming and refilling the same pairs over and over.
pub fn has_room(
    table: &str,
    owner: &str,
    candidate: &str,
    me: &str,
    score: &str,
    cap: &str,
) -> String {
    format!("(SELECT count(*) FROM {table} p WHERE p.other_id={candidate} AND (p.score>{score} OR (p.score={score} AND p.{owner}<{me})))<{cap}")
}

// A hint's queue is its `table` (`same_event_dirty`, `same_recording_dirty`): one row per due
// resource. A claim is settled only while it is still valid: the entry holds it and it has not
// run out. A change saved after it ran out leaves the entry's time as it was (the trigger only
// moves it earlier), so an expired claim is left due and the resource is read again.

/// Claims up to `n` due resources of `table` (never one a save is still marking: its row is
/// locked) for `lease`: each one's entry stays, moved `lease` ahead, and the time it was moved to
/// is the claim.
pub async fn claim(
    pool: &PgPool,
    table: &str,
    n: i64,
    lease: Duration,
) -> Result<Vec<(String, DateTime<Utc>)>> {
    let rows = sqlx::query(&format!("UPDATE {table} d SET next_attempt_at=now()+$2*interval '1 millisecond' \
        WHERE d.resource_id IN (SELECT resource_id FROM {table} WHERE next_attempt_at<=now() ORDER BY next_attempt_at,resource_id LIMIT $1 FOR UPDATE SKIP LOCKED) \
        RETURNING d.resource_id,d.next_attempt_at"))
        .bind(n)
        .bind(lease.as_millis() as f64)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("resource_id")?, r.try_get("next_attempt_at")?)))
        .collect()
}

/// A claimed resource read in full: its entry in `table` goes, if the claim is still valid.
pub async fn settle(pool: &PgPool, table: &str, id: &str, claimed: &DateTime<Utc>) -> Result<()> {
    sqlx::query(&format!(
        "DELETE FROM {table} WHERE resource_id=$1 AND next_attempt_at=$2 AND next_attempt_at>now()"
    ))
    .bind(id)
    .bind(claimed)
    .execute(pool)
    .await?;
    Ok(())
}

/// A claimed resource due again `after_secs` from now, if the claim is still valid.
pub async fn release(
    pool: &PgPool,
    table: &str,
    id: &str,
    claimed: &DateTime<Utc>,
    after_secs: i64,
) -> Result<()> {
    sqlx::query(&format!("UPDATE {table} SET next_attempt_at=now()+$3*interval '1 second' WHERE resource_id=$1 AND next_attempt_at=$2 AND next_attempt_at>now()"))
        .bind(id)
        .bind(claimed)
        .bind(after_secs)
        .execute(pool)
        .await?;
    Ok(())
}
