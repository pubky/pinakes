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
//! `refill` and the `same_event_unit_gone` trigger), as is a unit whose pair a cap pushed out.
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
/// The most units that may name one unit, so a change to it marks at most this many for a refill.
const INCOMING_KEPT: i64 = 2 * KEPT;
/// Resources claimed per pass.
const CLAIMED: i64 = 25;
/// Units dropped per transaction (`drop_units`): a long backlog goes in bounded steps.
const DROP_BATCH: i64 = 500;
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
            match sweep(&pool).await {
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
/// refill), `DROP_BATCH` a transaction, and marks the resources whose units entered it or changed
/// without a mark (a unit still marked for a refill among them), and those of stored units that
/// are no longer events: an alias, a missing feed, a gone occurrence. Returns the resources marked.
pub async fn sweep(pool: &PgPool) -> Result<u64> {
    drop_expired(pool, DROP_BATCH).await?;
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
    let mut read = 0;
    for (id, claimed) in &due {
        if read as i64 >= budget {
            release(pool, id, claimed, 0).await?;
            continue;
        }
        match resource(pool, id, budget - read as i64).await {
            Ok((n, more)) => {
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

/// Drops the units `which` selects (SQL over `$1`, the resource, when given), `batch` to a
/// transaction until none is left, so a long backlog (the sweep after a long stop) is never
/// read or locked whole: their pairs go with them, and the units that named them refill (the
/// `same_event_unit_gone` trigger). Returns how many were dropped.
async fn drop_units(pool: &PgPool, which: &str, resource: Option<&str>, batch: i64) -> Result<u64> {
    let limit = if resource.is_some() { "$2" } else { "$1" };
    let sql = format!(
        "DELETE FROM same_event_unit WHERE unit_id IN \
         (SELECT unit_id FROM same_event_unit WHERE unit_id IN ({which}) ORDER BY unit_id LIMIT {limit} FOR UPDATE)"
    );
    let mut dropped = 0;
    loop {
        let mut tx = pool.begin().await?;
        let mut delete = sqlx::query(&sql);
        if let Some(resource) = resource {
            delete = delete.bind(resource);
        }
        let n = delete.bind(batch).execute(&mut *tx).await?.rows_affected();
        if n > 0 {
            commit_changed(tx).await?;
        }
        dropped += n;
        if (n as i64) < batch {
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
/// of its new, changed or refilled units. Returns the units paired and whether more are left.
async fn resource(pool: &PgPool, id: &str, budget: i64) -> Result<(usize, bool)> {
    drop_units(
        pool,
        &format!(
            "SELECT unit_id FROM same_event_unit WHERE resource_id=$1 AND unit_id NOT IN \
             (SELECT s.unit_id FROM search_unit s WHERE s.id=$1 AND {EVENT} AND {WINDOW})"
        ),
        Some(id),
        DROP_BATCH,
    )
    .await?;
    // Each unit with what pairs are matched on: its normalised name and the numbers it keeps, its
    // place, venue and organizer, computed once here.
    let units = sqlx::query(&format!(
        "SELECT * FROM (SELECT s.unit_id,s.occurrence->>'uid' AS uid,coalesce(s.occurrence->>'recurrence_id','') AS rid,s.start_at,{FINGERPRINT} AS fingerprint,\
         same_event_name(s.name) AS name_key,same_event_numbers(s.name) AS numbers,ST_AsEWKT(s.point) AS point,s.venue_id,s.doc#>>'{{venue,osm_id}}' AS osm_venue,\
         lower(rtrim(s.doc#>>'{{organizer,url}}','/')) AS organizer,coalesce(s.doc->>'all_day','false')='true' AS all_day,s.doc->>'tz' AS tz \
         ,(SELECT x.fingerprint FROM same_event_unit x WHERE x.unit_id=s.unit_id) AS stored \
         FROM search_unit s WHERE s.id=$1 AND {EVENT} AND {WINDOW}) u \
         WHERE stored IS DISTINCT FROM fingerprint ORDER BY start_at,unit_id LIMIT $2"
    ))
    .bind(id)
    .bind(budget + 1)
    .fetch_all(pool)
    .await?;
    let more = units.len() as i64 > budget;
    let mut paired = 0;
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
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO same_event_unit(unit_id,resource_id,uid,recurrence_id,fingerprint,start_at,name_key,numbers,point,venue_id,osm_venue,organizer,all_day,tz) \
            SELECT unit_id,$2,uid,CASE WHEN uid IS NULL THEN NULL ELSE rid END,fingerprint,start_at,name_key,numbers,ST_GeomFromEWKT(point),venue_id,osm_venue,organizer,all_day,tz \
            FROM jsonb_to_record($1) AS u(unit_id text,uid text,rid text,fingerprint text,start_at timestamptz,name_key text,numbers text[],point text,venue_id text,osm_venue text,organizer text,all_day boolean,tz text) \
            ON CONFLICT(unit_id) DO UPDATE SET fingerprint=excluded.fingerprint,start_at=excluded.start_at,name_key=excluded.name_key,numbers=excluded.numbers,point=excluded.point,\
            venue_id=excluded.venue_id,osm_venue=excluded.osm_venue,organizer=excluded.organizer,all_day=excluded.all_day,tz=excluded.tz")
            .bind(row(u)?)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM same_event WHERE unit_id=$1")
            .bind(&unit)
            .execute(&mut *tx)
            .await?;
        if values_changed {
            // The pairs naming it go, and their units refill their lists (at most INCOMING_KEPT
            // of them, the cap below).
            let owners: Vec<String> =
                sqlx::query_scalar("DELETE FROM same_event WHERE other_id=$1 RETURNING unit_id")
                    .bind(&unit)
                    .fetch_all(&mut *tx)
                    .await?;
            refill(&mut tx, &owners).await?;
        }
        // The other units already read, on the values they were read with (a change marks a unit
        // to be read again); a unit read later finds this one in turn. Two located units must be
        // near each other; a venue or organizer counts only when one has no place. Equal scores
        // are taken round the units in ID order, starting just after this one, so a crowd of
        // one event's pages spreads its pairs instead of all naming its first twenty; and a unit
        // already named by INCOMING_KEPT that all rank above this one has no room and is passed
        // over (`has_room`), so the cap below pushes out only a pair this one displaces. A
        // candidate another transaction holds (an orphan delete) is skipped, never waited for.
        let room = crate::hints::has_room(
            "same_event",
            "unit_id",
            "x.unit_id",
            "me.unit_id",
            "similarity(me.name_key,x.name_key)",
            "$6",
        );
        let others: Vec<String> = sqlx::query_scalar(&format!(
            "WITH me AS (SELECT * FROM same_event_unit WHERE unit_id=$1), best AS (SELECT x.unit_id,similarity(me.name_key,x.name_key) AS score FROM me,same_event_unit x \
             WHERE x.resource_id<>me.resource_id AND me.name_key<>'' AND x.name_key<>'' \
             AND x.start_at BETWEEN me.start_at-interval '36 hours' AND me.start_at+interval '36 hours' \
             AND CASE WHEN me.all_day OR x.all_day THEN same_event_day(me.start_at,me.tz)=same_event_day(x.start_at,x.tz) \
              ELSE abs(extract(epoch FROM x.start_at-me.start_at))<=$2 END \
             AND ((x.point IS NOT NULL AND me.point IS NOT NULL AND ST_DWithin(x.point::geography,me.point::geography,$3)) \
              OR ((x.point IS NULL OR me.point IS NULL) AND (x.venue_id=me.venue_id OR x.osm_venue=me.osm_venue OR x.organizer=me.organizer))) \
             AND similarity(me.name_key,x.name_key)>=$4 \
             AND (cardinality(me.numbers)=0 OR cardinality(x.numbers)=0 OR x.numbers=me.numbers) \
             AND {room} \
             ORDER BY score DESC,x.unit_id<=me.unit_id,x.unit_id LIMIT $5 FOR KEY SHARE OF x SKIP LOCKED), \
             added AS (INSERT INTO same_event(unit_id,other_id,score) SELECT $1,unit_id,score FROM best UNION ALL SELECT unit_id,$1,score FROM best \
             ON CONFLICT(unit_id,other_id) DO UPDATE SET score=excluded.score) \
             SELECT unit_id FROM best"
        ))
        .bind(&unit)
        .bind(SECONDS as f64)
        .bind(METRES)
        .bind(SIMILARITY as f32)
        .bind(KEPT)
        .bind(INCOMING_KEPT)
        .fetch_all(&mut *tx)
        .await?;
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
        // And no unit is named by more than INCOMING_KEPT others, so a change to it marks at most
        // that many for a refill. A unit whose pair is pushed out refills its list (never this
        // one: it took only places it had room for).
        let mut named = others;
        named.push(unit.clone());
        let pushed_out: Vec<String> = sqlx::query_scalar(
            "DELETE FROM same_event p USING (SELECT unit_id,other_id,\
             row_number() OVER (PARTITION BY other_id ORDER BY score DESC,unit_id) AS n \
             FROM same_event WHERE other_id=ANY($1)) r \
             WHERE p.unit_id=r.unit_id AND p.other_id=r.other_id AND r.n>$2 RETURNING p.unit_id",
        )
        .bind(&named)
        .bind(INCOMING_KEPT)
        .fetch_all(&mut *tx)
        .await?;
        let pushed_out: Vec<String> = pushed_out.into_iter().filter(|u| *u != unit).collect();
        refill(&mut tx, &pushed_out).await?;
        commit_changed(tx).await?;
        paired += 1;
    }
    Ok((paired, more))
}

/// Reads one resource's units now, as a pass does (without its claim or budget). Returns the
/// units paired. Exposed for tests.
pub async fn read(pool: &PgPool, id: &str) -> Result<usize> {
    Ok(resource(pool, id, i64::from(i32::MAX)).await?.0)
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
