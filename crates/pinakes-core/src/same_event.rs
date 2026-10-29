//! The same-event hint: event units (an event resource, or one occurrence of a feed) on different
//! pages that are possibly the same event, such as one meetup on Meetup, Luma and its organiser's
//! site. A hint only: never a group, never counted, never ranks anything. Two units are a pair
//! when all of these hold:
//! - they start within `SECONDS` of each other (an all-day event: on the same local date);
//! - both located: within `METRES` of each other; else the same venue or organizer;
//! - their normalised names (`same_event_name` in SQL) are at least `SIMILARITY` alike (pg_trgm),
//!   and the numbers they keep (`#50`) agree;
//! - they are on different pages (two occurrences of one feed never pair).
//!
//! The pass reads the units of resources whose events changed (`same_event_dirty`, marked by
//! triggers and by the sweep) and starting in the window (a
//! day ago to a year ahead), bounded per pass and going on at once while more is due; an entry
//! is claimed for a while and dropped once read, so a stopped pass loses nothing; an hourly
//! sweep drops units that left the window and marks those that entered it, changed unseen or
//! became an alias. A unit that changes or goes (out of the window, or with its resource) takes
//! its pairs from the units that named it, and those are read again to refill their lists (see
//! `refill` and `units_going`), as is a unit whose pair a cap pushed out.
//! Cards only read the stored pairs (`same_event_hint`).
use crate::config::{Config, SameEventHint};
use crate::hints::commit_changed;
use anyhow::Result;
use sqlx::{PgPool, Row};
use std::time::Duration;
use tokio::sync::watch;

/// How far apart two starts may be, in seconds.
pub const SECONDS: i64 = 300;
/// How far apart two places may be, in metres.
pub const METRES: f64 = 500.0;
/// The least pg_trgm similarity of the two normalised names.
pub const SIMILARITY: f64 = 0.5;
/// Pairs kept per unit, best first (a card shows three, its details ten).
const KEPT: i64 = 20;
/// Candidates examined per read at most (`hints::SCAN`): those passed over for having no room
/// count, so a read in a crowd of one event's pages is bounded however dense it is. A read that
/// runs out with room left goes on from the position it stopped at in a later one (`scan_after`,
/// `scan_after_start`).
pub const SCAN: i64 = crate::hints::SCAN;
/// How far round a unit its ring of candidates reaches, in hours, either way (both edges
/// inclusive): any two units that may pair start within it. Two timed units start within
/// `SECONDS`; with an all-day one the two are on one local date, each in its own zone: the two
/// midnights are at most 26 hours apart (the zones run from UTC-12 to UTC+14), and a timed start
/// is under 24 hours after its own midnight, so under 50 hours after the other's.
const RING_HOURS: i64 = 50;
/// Resources claimed per pass.
const CLAIMED: i64 = 25;

/// The caps an operator sets (`[limits]`): the most units that may name one unit, so a change to
/// it marks at most this many for a refill (`same_event_incoming_kept`, 40), and the units
/// dropped per transaction (`same_event_drop_batch`, 500), so a long backlog goes in bounded steps.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub incoming_kept: i64,
    pub drop_batch: i64,
}
impl Caps {
    pub fn of(config: &Config) -> Self {
        Self {
            incoming_kept: config.event.hint_incoming_kept,
            drop_batch: config.event.hint_drop_batch,
        }
    }
}
impl Default for Caps {
    fn default() -> Self {
        Self::of(&Config::default())
    }
}
/// The window of starts: from a day ago to a year ahead.
const WINDOW: &str = "s.start_at BETWEEN now()-interval '1 day' AND now()+interval '1 year'";
/// An event unit the hint reads: it has a start (in the window) and a name.
const EVENT: &str = "s.type_path @> ARRAY['Event'] AND s.name IS NOT NULL";
/// What a unit's pairs are computed from.
const FINGERPRINT: &str = "md5(jsonb_build_array(s.start_at,ST_AsText(s.point),s.venue_id,s.doc#>>'{venue,osm_id}',s.doc#>>'{organizer,url}',s.name,s.doc->>'all_day',s.doc->>'tz',s.type_path)::text)";

fn enabled(config: &Config) -> bool {
    config.event.same_event_hint == SameEventHint::On
}

/// The pass and the hourly sweep, until shutdown.
pub async fn run(pool: PgPool, config: Config, mut shutdown: watch::Receiver<bool>) {
    if !enabled(&config) {
        return;
    }
    let mut swept: Option<tokio::time::Instant> = None;
    loop {
        if swept.is_none_or(|at| at.elapsed() >= Duration::from_secs(3600)) {
            match sweep(&pool, &config).await {
                Ok(_) => swept = Some(tokio::time::Instant::now()),
                Err(error) => {
                    tracing::warn!(error = %crate::resolver::log_error(&error), "same-event sweep will retry")
                }
            }
        }
        let pass = tokio::select! { _=shutdown.changed()=>break, result=drain(&pool, &config, 200)=>result };
        match pass {
            // There was work: more may be due, so go on at once.
            Ok(n) if n > 0 => {
                tokio::task::yield_now().await;
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(error = %crate::resolver::log_error(&error), "same-event pass will retry")
            }
        }
        tokio::select! { _=shutdown.changed()=>break, _=tokio::time::sleep(Duration::from_secs(5))=>{} }
    }
}

/// Drops the units that left the window (their pairs go with them, and the units that named them
/// refill), `limits.same_event_drop_batch` a transaction, and marks the resources whose units
/// entered it or changed without a mark (a unit still marked for a refill among them), and those
/// of stored units that are no longer events: an alias, a missing feed, a gone occurrence.
/// Returns the resources marked.
pub async fn sweep(pool: &PgPool, config: &Config) -> Result<u64> {
    drop_expired(pool, Caps::of(config).drop_batch).await?;
    let entered = sqlx::query(&format!(
        "INSERT INTO same_event_dirty(resource_id) SELECT DISTINCT s.id FROM search_unit s \
         WHERE {EVENT} AND {WINDOW} AND NOT EXISTS(SELECT 1 FROM same_event_unit x WHERE x.unit_id=s.unit_id AND x.fingerprint={FINGERPRINT}) \
         ON CONFLICT DO NOTHING"
    ))
    .execute(pool)
    .await?;
    let stale = sqlx::query(
        "INSERT INTO same_event_dirty(resource_id) SELECT DISTINCT x.resource_id FROM same_event_unit x JOIN resource r ON r.id=x.resource_id \
         WHERE EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=x.resource_id) \
         OR (x.uid IS NULL AND NOT coalesce(r.type_path @> ARRAY['Event'],false)) \
         OR (x.uid IS NOT NULL AND (r.status='missing' OR NOT EXISTS(SELECT 1 FROM occurrence o WHERE o.resource_id=x.resource_id AND o.uid=x.uid AND o.recurrence_id=x.recurrence_id AND o.status<>'superseded'))) \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await?;
    Ok(entered.rows_affected() + stale.rows_affected())
}

/// How long a claimed resource is the pass's: one that is not done by then (the pass was
/// stopped, the process ended) is due again, without waiting for the sweep.
const LEASE: Duration = Duration::from_secs(300);

/// Claims up to `n` due resources (never one a save is still marking: its row is locked) for
/// `lease`: each one's entry stays, moved `lease` ahead, and the time it was moved to is the
/// claim. Exposed for the interruption test; `drain` claims with [`LEASE`].
pub async fn claim(
    pool: &PgPool,
    n: i64,
    lease: Duration,
) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>> {
    let rows = sqlx::query("UPDATE same_event_dirty d SET next_attempt_at=now()+$2*interval '1 millisecond' \
        WHERE d.resource_id IN (SELECT resource_id FROM same_event_dirty WHERE next_attempt_at<=now() ORDER BY next_attempt_at,resource_id LIMIT $1 FOR UPDATE SKIP LOCKED) \
        RETURNING d.resource_id,d.next_attempt_at")
        .bind(n)
        .bind(lease.as_millis() as f64)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("resource_id")?, r.try_get("next_attempt_at")?)))
        .collect()
}

/// One pass: claims up to `CLAIMED` due resources and pairs their new or changed units, at most
/// `budget` of them; a resource with more, or one the budget did not reach, is due again at once.
/// A claim is settled only while it is valid (see `settle`): a change marked meanwhile moved the
/// entry back to now (the trigger), and a claim that ran out is left due; either way the next pass
/// reads the resource again. An interrupted pass leaves its claims to run out. Returns the
/// resources claimed.
pub async fn drain(pool: &PgPool, config: &Config, budget: i64) -> Result<usize> {
    if !enabled(config) {
        return Ok(0);
    }
    let due = claim(pool, CLAIMED, LEASE).await?;
    let caps = Caps::of(config);
    let mut read = 0;
    for (id, claimed) in &due {
        if read as i64 >= budget {
            release(pool, id, claimed, 0).await?;
            continue;
        }
        match resource(pool, id, budget - read as i64, caps).await {
            Ok((n, more, _)) => {
                read += n;
                if more {
                    release(pool, id, claimed, 0).await?;
                } else {
                    settle(pool, id, claimed).await?;
                }
            }
            Err(error) => {
                tracing::warn!(resource_id = %id, error = %crate::resolver::log_error(&error), "same-event pairs will retry");
                release(pool, id, claimed, 60).await?;
            }
        }
    }
    Ok(due.len())
}

// A claim is settled only while it is still valid: the entry holds it and it has not run out.
// A change saved after it ran out leaves the entry's time as it was (the trigger only moves it
// earlier), so an expired claim is left due and the resource is read again.

/// A claimed resource read in full: its entry goes, if the claim is still valid. Exposed for the
/// expiry test.
pub async fn settle(
    pool: &PgPool,
    id: &str,
    claimed: &chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query("DELETE FROM same_event_dirty WHERE resource_id=$1 AND next_attempt_at=$2 AND next_attempt_at>now()")
        .bind(id)
        .bind(claimed)
        .execute(pool)
        .await?;
    Ok(())
}

/// A claimed resource due again `after_secs` from now, if the claim is still valid.
async fn release(
    pool: &PgPool,
    id: &str,
    claimed: &chrono::DateTime<chrono::Utc>,
    after_secs: i64,
) -> Result<()> {
    sqlx::query("UPDATE same_event_dirty SET next_attempt_at=now()+$3*interval '1 second' WHERE resource_id=$1 AND next_attempt_at=$2 AND next_attempt_at>now()")
        .bind(id)
        .bind(claimed)
        .bind(after_secs)
        .execute(pool)
        .await?;
    Ok(())
}

/// Units whose lists lost a pair are read again: their fingerprint is marked `refill:` (so the
/// read recomputes their list instead of finding it current, and still knows their values) and
/// their resources are due now. The sweep marks a unit still marked, as one whose fingerprint
/// differs.
async fn refill(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, units: &[String]) -> Result<()> {
    if units.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE same_event_unit SET fingerprint='refill:'||fingerprint WHERE unit_id=ANY($1) AND fingerprint NOT LIKE 'refill:%'")
        .bind(units)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO same_event_dirty(resource_id) SELECT DISTINCT resource_id FROM same_event_unit WHERE unit_id=ANY($1) \
        ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=excluded.next_attempt_at WHERE same_event_dirty.next_attempt_at>excluded.next_attempt_at")
        .bind(units)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Marks the units that name `units`, other than those among them, to refill: their pairs go
/// with the units when these are deleted. Done in the code before the delete, never by a trigger
/// on it: a trigger that updated a row the same statement deletes aborted a batch dropping two
/// paired units (PostgreSQL: "tuple to be updated was already modified by an operation triggered
/// by the current command").
async fn units_going(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    units: &[String],
) -> Result<()> {
    if units.is_empty() {
        return Ok(());
    }
    let owners: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT unit_id FROM same_event WHERE other_id=ANY($1) AND NOT unit_id=ANY($1)",
    )
    .bind(units)
    .fetch_all(&mut **tx)
    .await?;
    refill(tx, &owners).await
}

/// Before a resource's row is deleted (an orphan, `changes::delete_orphans`): its units go with
/// it (the cascade), so the units that named them are marked to refill. The units are locked
/// first (FOR UPDATE, in id order, as `drop_units` locks its batch), so a pass that would pair
/// with them meanwhile skips them (its candidates are taken FOR KEY SHARE … SKIP LOCKED) or waits
/// for the delete: a pair added between the namers' collection and the cascade would go unmarked.
/// Nothing happens for a resource without units.
pub async fn before_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    resource_id: &str,
) -> Result<()> {
    let units: Vec<String> = sqlx::query_scalar(
        "SELECT unit_id FROM same_event_unit WHERE resource_id=$1 ORDER BY unit_id FOR UPDATE",
    )
    .bind(resource_id)
    .fetch_all(&mut **tx)
    .await?;
    units_going(tx, &units).await
}

/// Drops the units `which` selects (SQL over `$1`, the resource, when given), `batch` to a
/// transaction until none is left, so a long backlog (the sweep after a long stop) is never
/// read or locked whole: their pairs go with them, and the units that named them refill
/// (`units_going`). Returns how many were dropped.
async fn drop_units(pool: &PgPool, which: &str, resource: Option<&str>, batch: i64) -> Result<u64> {
    let limit = if resource.is_some() { "$2" } else { "$1" };
    let select = format!(
        "SELECT unit_id FROM same_event_unit WHERE unit_id IN ({which}) ORDER BY unit_id LIMIT {limit} FOR UPDATE"
    );
    let mut dropped = 0;
    loop {
        let mut tx = pool.begin().await?;
        let mut query = sqlx::query_scalar::<_, String>(&select);
        if let Some(resource) = resource {
            query = query.bind(resource);
        }
        let units = query.bind(batch).fetch_all(&mut *tx).await?;
        if !units.is_empty() {
            units_going(&mut tx, &units).await?;
            sqlx::query("DELETE FROM same_event_unit WHERE unit_id=ANY($1)")
                .bind(&units)
                .execute(&mut *tx)
                .await?;
            commit_changed(tx).await?;
        }
        dropped += units.len() as u64;
        if (units.len() as i64) < batch {
            return Ok(dropped);
        }
    }
}

/// Drops the units that started over a day ago, `batch` to a transaction (the sweep's first
/// step). Exposed for the bound's test.
pub async fn drop_expired(pool: &PgPool, batch: i64) -> Result<u64> {
    drop_units(
        pool,
        "SELECT unit_id FROM same_event_unit WHERE start_at<now()-interval '1 day'",
        None,
        batch,
    )
    .await
}

/// One resource: drops its units that are gone or out of the window, then pairs up to `budget`
/// of its new, changed or refilled units. Returns the units paired, whether more are left, and
/// how many candidates the reads examined (`SCAN` each at most).
async fn resource(
    pool: &PgPool,
    id: &str,
    budget: i64,
    caps: Caps,
) -> Result<(usize, bool, usize)> {
    drop_units(
        pool,
        &format!(
            "SELECT unit_id FROM same_event_unit WHERE resource_id=$1 AND unit_id NOT IN \
             (SELECT s.unit_id FROM search_unit s WHERE s.id=$1 AND {EVENT} AND {WINDOW})"
        ),
        Some(id),
        caps.drop_batch,
    )
    .await?;
    // Each unit with what pairs are matched on: its normalised name and the numbers it keeps, its
    // place, venue and organizer, computed once here.
    let units = sqlx::query(&format!(
        "SELECT * FROM (SELECT s.unit_id,s.occurrence->>'uid' AS uid,coalesce(s.occurrence->>'recurrence_id','') AS rid,s.start_at,{FINGERPRINT} AS fingerprint,\
         same_event_name(s.name) AS name_key,same_event_numbers(s.name) AS numbers,ST_AsEWKT(s.point) AS point,s.venue_id,s.doc#>>'{{venue,osm_id}}' AS osm_venue,\
         lower(rtrim(s.doc#>>'{{organizer,url}}','/')) AS organizer,coalesce(s.doc->>'all_day','false')='true' AS all_day,s.doc->>'tz' AS tz \
         ,(SELECT x.fingerprint FROM same_event_unit x WHERE x.unit_id=s.unit_id) AS stored \
         ,(SELECT x.scan_after FROM same_event_unit x WHERE x.unit_id=s.unit_id) AS stopped_at \
         ,(SELECT x.scan_after_start FROM same_event_unit x WHERE x.unit_id=s.unit_id) AS stopped_at_start \
         FROM search_unit s WHERE s.id=$1 AND {EVENT} AND {WINDOW}) u \
         WHERE stored IS DISTINCT FROM fingerprint ORDER BY start_at,unit_id LIMIT $2"
    ))
    .bind(id)
    .bind(budget + 1)
    .fetch_all(pool)
    .await?;
    let more = units.len() as i64 > budget;
    let mut paired = 0;
    let mut examined_total = 0usize;
    for u in units.iter().take(budget as usize) {
        let unit: String = u.try_get("unit_id")?;
        // A refill (marked `refill:{fingerprint}` because a unit it named changed or went) whose
        // values are still those replaces its own list only: what names it stays right, and
        // nothing else is marked, so refills never cascade. A unit whose values changed (a
        // refill's too) drops the pairs naming it.
        let fingerprint: String = u.try_get("fingerprint")?;
        let stored: Option<String> = u.try_get("stored")?;
        let values_changed = stored
            .as_deref()
            .is_some_and(|f| f.strip_prefix("refill:").unwrap_or(f) != fingerprint);
        // A read examines at most `SCAN` candidates (a dense crowd's full ones are examined and
        // passed over, so a read of it is bounded all the same). One that ran out with room left
        // stored the position it stopped at (`scan_after`, `scan_after_start`) and marked itself
        // to be read again: that read keeps the pairs it has and goes on round the ring from
        // there, while the values are the same. One whose values changed starts over (its ring
        // untouched, the position cleared with the upsert): a start that moved changes the ring,
        // and a candidate that moved with it may now lie before the position.
        let start: chrono::DateTime<chrono::Utc> = u.try_get("start_at")?;
        let mut ring = Ring::of(&unit, start);
        let stopped_at = match (
            u.try_get::<Option<String>, _>("stopped_at")?,
            u.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("stopped_at_start")?,
        ) {
            (Some(id), Some(start)) => Some(Pos { start, id }),
            _ => None,
        };
        let resumed = !values_changed && stopped_at.is_some_and(|at| ring.resume_after(at));
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO same_event_unit(unit_id,resource_id,uid,recurrence_id,fingerprint,start_at,name_key,numbers,point,venue_id,osm_venue,organizer,all_day,tz,scan_after,scan_after_start) \
            SELECT unit_id,$2,uid,CASE WHEN uid IS NULL THEN NULL ELSE rid END,fingerprint,start_at,name_key,numbers,ST_GeomFromEWKT(point),venue_id,osm_venue,organizer,all_day,tz,NULL,NULL \
            FROM jsonb_to_record($1) AS u(unit_id text,uid text,rid text,fingerprint text,start_at timestamptz,name_key text,numbers text[],point text,venue_id text,osm_venue text,organizer text,all_day boolean,tz text) \
            ON CONFLICT(unit_id) DO UPDATE SET fingerprint=excluded.fingerprint,start_at=excluded.start_at,name_key=excluded.name_key,numbers=excluded.numbers,point=excluded.point,\
            venue_id=excluded.venue_id,osm_venue=excluded.osm_venue,organizer=excluded.organizer,all_day=excluded.all_day,tz=excluded.tz,scan_after=NULL,scan_after_start=NULL")
            .bind(row(u)?)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // The pairs a resumed read keeps (left out of the candidates, so they take no place
        // among those a read may still take); any other read starts from none.
        let mut existing: Vec<String> = vec![];
        if resumed {
            existing = sqlx::query_scalar("SELECT other_id FROM same_event WHERE unit_id=$1")
                .bind(&unit)
                .fetch_all(&mut *tx)
                .await?;
        } else {
            sqlx::query("DELETE FROM same_event WHERE unit_id=$1")
                .bind(&unit)
                .execute(&mut *tx)
                .await?;
            if values_changed {
                // The pairs naming it go, and their units refill their lists (at most
                // `incoming_kept` of them, the cap below).
                let owners: Vec<String> = sqlx::query_scalar(
                    "DELETE FROM same_event WHERE other_id=$1 RETURNING unit_id",
                )
                .bind(&unit)
                .fetch_all(&mut *tx)
                .await?;
                refill(&mut tx, &owners).await?;
            }
        }
        // The other units already read, on the values they were read with (a change marks a unit
        // to be read again); a unit read later finds this one in turn. The candidates are
        // examined round the ring (`Ring`): the units starting within `RING_HOURS` of this one in
        // (start, id) order, from just after it (or after the position a stopped read stopped
        // at) to the end of that window and from its start back to this one. Each segment is a
        // cursor over the walk of the start index (`RING`), fetched as far as may still be
        // examined: a read visits the rows up to its last candidate and sorts none, so a crowd
        // of one event's pages costs each read a hundred rows however large it is, and spreads
        // its pairs instead of all naming its first twenty. Two located units must be near each
        // other; a venue or organizer counts only when one has no place.
        let room = KEPT - existing.len() as i64;
        let mut cands: Vec<(String, f32)> = vec![];
        let mut scan_left = if room > 0 { SCAN } else { 0 };
        let mut last_examined: Option<Pos> = None;
        for segment in &ring.segments {
            if scan_left <= 0 {
                break;
            }
            sqlx::query(&format!("DECLARE ring NO SCROLL CURSOR FOR {RING}"))
                .bind(&unit)
                .bind(SECONDS as f64)
                .bind(METRES)
                .bind(SIMILARITY as f32)
                .bind(&existing)
                .bind(segment.after.start)
                .bind(&segment.after.id)
                .bind(segment.before.start)
                .bind(&segment.before.id)
                .execute(&mut *tx)
                .await?;
            let rows = sqlx::query(&format!("FETCH FORWARD {scan_left} FROM ring"))
                .persistent(false)
                .fetch_all(&mut *tx)
                .await?;
            sqlx::query("CLOSE ring").execute(&mut *tx).await?;
            for r in &rows {
                let at = Pos {
                    start: r.try_get("start_at")?,
                    id: r.try_get("unit_id")?,
                };
                cands.push((at.id.clone(), r.try_get("score")?));
                last_examined = Some(at);
            }
            scan_left -= rows.len() as i64;
        }
        examined_total += cands.len();
        // Of the candidates examined, the best by score with room are taken (on a tie, the
        // nearer round the ring): a unit already named by `incoming_kept` that all rank above
        // this one has no room and is passed over (`has_room`), so the cap below pushes out only
        // a pair this one displaces. A candidate another transaction holds (an orphan delete) is
        // skipped, never waited for: each candidate's row is held through a lateral subquery, by
        // its key (joined instead, the planner hashes the whole table for them on a small one).
        let (ids, scores): (Vec<String>, Vec<f32>) = cands.into_iter().unzip();
        let has_room =
            crate::hints::has_room("same_event", "unit_id", "c.unit_id", "$1", "c.score", "$5");
        let others: Vec<String> = if ids.is_empty() {
            vec![]
        } else {
            sqlx::query_scalar(&format!(
                "WITH c AS (SELECT * FROM unnest($2::text[],$3::real[]) WITH ORDINALITY AS c(unit_id,score,n)), \
                 best AS (SELECT c.unit_id,c.score FROM c,LATERAL (SELECT 1 FROM same_event_unit x WHERE x.unit_id=c.unit_id FOR KEY SHARE SKIP LOCKED) l \
                 WHERE {has_room} ORDER BY c.score DESC,c.n LIMIT $4), \
                 added AS (INSERT INTO same_event(unit_id,other_id,score) SELECT $1,unit_id,score FROM best UNION ALL SELECT unit_id,$1,score FROM best \
                 ON CONFLICT(unit_id,other_id) DO UPDATE SET score=excluded.score) \
                 SELECT unit_id FROM best"
            ))
            .bind(&unit)
            .bind(&ids)
            .bind(&scores)
            .bind(room)
            .bind(caps.incoming_kept)
            .fetch_all(&mut *tx)
            .await?
        };
        // Out of candidates to examine with room left: this read stops here and is read again
        // from the position it stopped at (due now, after what is due already).
        if room > 0 && scan_left <= 0 && (others.len() as i64) < room {
            if let Some(at) = &last_examined {
                sqlx::query(
                    "UPDATE same_event_unit SET scan_after=$2,scan_after_start=$3 WHERE unit_id=$1",
                )
                .bind(&unit)
                .bind(&at.id)
                .bind(at.start)
                .execute(&mut *tx)
                .await?;
                refill(&mut tx, std::slice::from_ref(&unit)).await?;
            }
        }
        // Each other unit keeps its best `KEPT` too: this one may push its last one out.
        sqlx::query(
            "DELETE FROM same_event p USING (SELECT unit_id,other_id,\
             row_number() OVER (PARTITION BY unit_id ORDER BY score DESC,other_id) AS n \
             FROM same_event WHERE unit_id=ANY($1)) r \
             WHERE p.unit_id=r.unit_id AND p.other_id=r.other_id AND r.n>$2",
        )
        .bind(&others)
        .bind(KEPT)
        .execute(&mut *tx)
        .await?;
        // And no unit is named by more than `incoming_kept` others, so a change to it marks at
        // most that many for a refill. A unit whose pair is pushed out refills its list (never
        // this one: it took only places it had room for).
        let mut named = others;
        named.push(unit.clone());
        let pushed_out: Vec<String> = sqlx::query_scalar(
            "DELETE FROM same_event p USING (SELECT unit_id,other_id,\
             row_number() OVER (PARTITION BY other_id ORDER BY score DESC,unit_id) AS n \
             FROM same_event WHERE other_id=ANY($1)) r \
             WHERE p.unit_id=r.unit_id AND p.other_id=r.other_id AND r.n>$2 RETURNING p.unit_id",
        )
        .bind(&named)
        .bind(caps.incoming_kept)
        .fetch_all(&mut *tx)
        .await?;
        let pushed_out: Vec<String> = pushed_out.into_iter().filter(|u| *u != unit).collect();
        refill(&mut tx, &pushed_out).await?;
        commit_changed(tx).await?;
        paired += 1;
    }
    Ok((paired, more, examined_total))
}

/// Reads one resource's units now, as a pass does (without its claim or budget, with the
/// default caps). Returns the units paired. Exposed for tests.
pub async fn read(pool: &PgPool, id: &str) -> Result<usize> {
    Ok(resource(pool, id, i64::from(i32::MAX), Caps::default())
        .await?
        .0)
}

/// As `read`, returning the units paired and the candidates the reads examined. Exposed for
/// the bound's test.
pub async fn read_examined(pool: &PgPool, id: &str) -> Result<(usize, usize)> {
    let (paired, _, examined) = resource(pool, id, i64::from(i32::MAX), Caps::default()).await?;
    Ok((paired, examined))
}

/// A segment of the ring, as a cursor's query: the units from just after `$6,$7` to just before
/// `$8,$9` in (start, id) order, the candidates of the unit `$1` among them with their scores.
/// Planned for a cursor (fetched a little at a time), it is the walk of the start index with the
/// conditions on each unit as it comes, whatever the planner's estimate of the rows in range
/// (one it would sort and cut for a `LIMIT` whenever that estimate is near the limit). A candidate
/// starts within `$2` seconds (an all-day unit: on the same local date), within `$3` metres or
/// else at the same venue or organizer, with a name at least `$4` alike and its numbers agreeing,
/// and is not among the pairs `$5` kept.
const RING: &str = "WITH me AS MATERIALIZED (SELECT * FROM same_event_unit WHERE unit_id=$1) \
    SELECT x.unit_id,x.start_at,similarity(me.name_key,x.name_key)::real AS score FROM me,same_event_unit x \
    WHERE (x.start_at,x.unit_id)>($6,$7) AND (x.start_at,x.unit_id)<($8,$9) \
    AND x.resource_id<>me.resource_id AND me.name_key<>'' AND x.name_key<>'' \
    AND CASE WHEN me.all_day OR x.all_day THEN same_event_day(me.start_at,me.tz)=same_event_day(x.start_at,x.tz) \
     ELSE abs(extract(epoch FROM x.start_at-me.start_at))<=$2 END \
    AND ((x.point IS NOT NULL AND me.point IS NOT NULL AND ST_DWithin(x.point::geography,me.point::geography,$3)) \
     OR ((x.point IS NULL OR me.point IS NULL) AND (x.venue_id=me.venue_id OR x.osm_venue=me.osm_venue OR x.organizer=me.organizer))) \
    AND similarity(me.name_key,x.name_key)>=$4 \
    AND (cardinality(me.numbers)=0 OR cardinality(x.numbers)=0 OR x.numbers=me.numbers) \
    AND NOT x.unit_id=ANY($5) \
    ORDER BY x.start_at,x.unit_id";

/// A place in the start index's (start, id) order: a candidate examined last, where a read stopped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    start: chrono::DateTime<chrono::Utc>,
    id: String,
}

/// The ring of a unit's candidates: the units starting within `RING_HOURS` of it, in (start, id)
/// order from just after it to the end of that window, then from the window's start to just
/// before it; each segment runs from `after` to `before`, both exclusive. The window's edges are
/// inclusive: its first position is the one before every id at the first start, its last the one
/// before every id at the microsecond (a timestamp's resolution) after the last start.
struct Ring {
    segments: Vec<Segment>,
}

struct Segment {
    after: Pos,
    before: Pos,
}

impl Ring {
    fn of(me: &str, start: chrono::DateTime<chrono::Utc>) -> Self {
        let me = Pos {
            start,
            id: me.to_owned(),
        };
        let edge = |start| Pos {
            start,
            id: String::new(),
        };
        let reach = chrono::Duration::hours(RING_HOURS);
        Self {
            segments: vec![
                Segment {
                    after: me.clone(),
                    before: edge(start + reach + chrono::Duration::microseconds(1)),
                },
                Segment {
                    after: edge(start - reach),
                    before: me,
                },
            ],
        }
    }

    /// Goes on from just after `at`: the segments before the one holding it go. False (the ring
    /// unchanged) when no segment holds it.
    fn resume_after(&mut self, at: Pos) -> bool {
        let Some(i) = self
            .segments
            .iter()
            .position(|s| at > s.after && at < s.before)
        else {
            return false;
        };
        self.segments.drain(..i);
        self.segments[0].after = at;
        true
    }
}

/// A unit row as JSON for `jsonb_to_record` (its point as EWKT).
fn row(u: &sqlx::postgres::PgRow) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "unit_id": u.try_get::<String, _>("unit_id")?,
        "uid": u.try_get::<Option<String>, _>("uid")?,
        "rid": u.try_get::<String, _>("rid")?,
        "fingerprint": u.try_get::<String, _>("fingerprint")?,
        "start_at": u.try_get::<chrono::DateTime<chrono::Utc>, _>("start_at")?,
        "name_key": u.try_get::<String, _>("name_key")?,
        "numbers": u.try_get::<Vec<String>, _>("numbers")?,
        "point": u.try_get::<Option<String>, _>("point")?,
        "venue_id": u.try_get::<Option<String>, _>("venue_id")?,
        "osm_venue": u.try_get::<Option<String>, _>("osm_venue")?,
        "organizer": u.try_get::<Option<String>, _>("organizer")?,
        "all_day": u.try_get::<bool, _>("all_day")?,
        "tz": u.try_get::<Option<String>, _>("tz")?,
    }))
}

#[cfg(test)]
mod ring {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    /// A unit starting exactly `RING_HOURS` from the read one, either way, is in its ring
    /// whatever its id; one a microsecond further is not.
    #[test]
    fn the_edges_of_the_window_are_inclusive() {
        let start = Utc.with_ymd_and_hms(2026, 11, 2, 0, 0, 0).unwrap();
        let at = |from: Duration, id: &str| Pos {
            start: start + from,
            id: id.to_owned(),
        };
        let reach = Duration::hours(RING_HOURS);
        let held = |at: Pos| Ring::of("me", start).resume_after(at);
        assert!(held(at(reach, "0")));
        assert!(held(at(reach, "\u{10ffff}")));
        assert!(held(at(-reach, "0")));
        assert!(!held(at(reach + Duration::microseconds(1), "")));
        assert!(!held(at(-reach - Duration::microseconds(1), "\u{10ffff}")));
        // The segment holding the position is kept from it on; those before it go.
        let mut ring = Ring::of("me", start);
        assert!(ring.resume_after(at(reach, "0")));
        assert_eq!(ring.segments.len(), 2);
        let mut ring = Ring::of("me", start);
        assert!(ring.resume_after(at(-reach, "0")));
        assert_eq!(ring.segments.len(), 1);
        assert_eq!(ring.segments[0].after, at(-reach, "0"));
    }
}
