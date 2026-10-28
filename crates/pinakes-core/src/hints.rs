//! What the hint passes (same_event, same_recording) share: how a change to the pairs is
//! committed, and how a pass picks candidates with room.
use anyhow::Result;

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
