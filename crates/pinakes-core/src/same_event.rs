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
//! day ago to a year ahead), bounded per pass and going on at once while more is due; an hourly
//! sweep drops units that left the window and marks those that entered it, changed unseen or
//! became an alias. Cards only read the stored pairs (`same_event_hint`).
use crate::config::{Config, SameEventHint};
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
/// Resources claimed per pass.
const CLAIMED: i64 = 25;
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

/// Drops the units that left the window (their pairs go with them) and marks the resources whose
/// units entered it or changed without a mark, and those of stored units that are no longer
/// events: an alias, a missing feed, a gone occurrence. Returns the resources marked.
pub async fn sweep(pool: &PgPool) -> Result<u64> {
    let dropped = sqlx::query("DELETE FROM same_event_unit WHERE start_at<now()-interval '1 day'")
        .execute(pool)
        .await?
        .rows_affected();
    if dropped > 0 {
        bump(pool).await?;
    }
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

/// One pass: claims up to `CLAIMED` due resources (never one a save is still marking) and pairs
/// their new or changed units, at most `budget` of them; a resource with more, or one the budget
/// did not reach, is marked again. Returns the resources claimed.
pub async fn drain(pool: &PgPool, config: &Config, budget: i64) -> Result<usize> {
    if !enabled(config) {
        return Ok(0);
    }
    // Unmarked first: a change while the units are read marks the resource again.
    let due: Vec<String> = sqlx::query_scalar("DELETE FROM same_event_dirty WHERE resource_id IN (SELECT resource_id FROM same_event_dirty WHERE next_attempt_at<=now() ORDER BY next_attempt_at,resource_id LIMIT $1 FOR UPDATE SKIP LOCKED) RETURNING resource_id")
        .bind(CLAIMED)
        .fetch_all(pool)
        .await?;
    let mut read = 0;
    let mut changed = false;
    for id in &due {
        if read as i64 >= budget {
            mark(pool, id, 0).await?;
            continue;
        }
        match resource(pool, id, budget - read as i64).await {
            Ok((n, dropped, more)) => {
                read += n;
                changed |= n > 0 || dropped;
                if more {
                    mark(pool, id, 0).await?;
                }
            }
            Err(error) => {
                tracing::warn!(resource_id = %id, error = %crate::resolver::log_error(&error), "same-event pairs will retry");
                mark(pool, id, 60).await?;
            }
        }
    }
    if changed {
        bump(pool).await?;
    }
    Ok(due.len())
}

/// Search pages hash this into their ETag, so a changed hint is never answered with a 304.
async fn bump(pool: &PgPool) -> Result<()> {
    sqlx::query("SELECT nextval('same_event_revision')")
        .execute(pool)
        .await?;
    Ok(())
}

async fn mark(pool: &PgPool, id: &str, after_secs: i64) -> Result<()> {
    sqlx::query("INSERT INTO same_event_dirty(resource_id,next_attempt_at) SELECT id,now()+$2*interval '1 second' FROM resource WHERE id=$1 ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(after_secs)
        .execute(pool)
        .await?;
    Ok(())
}

/// One resource: drops its units that are gone or out of the window, then pairs up to `budget`
/// of its new or changed units. Returns the units paired, whether any was dropped and whether
/// more are left.
async fn resource(pool: &PgPool, id: &str, budget: i64) -> Result<(usize, bool, bool)> {
    let dropped = sqlx::query(&format!(
        "WITH current AS (SELECT s.unit_id FROM search_unit s WHERE s.id=$1 AND {EVENT} AND {WINDOW}) \
         DELETE FROM same_event_unit WHERE resource_id=$1 AND unit_id NOT IN (SELECT unit_id FROM current)"
    ))
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    // Each unit with what pairs are matched on: its normalised name and the numbers it keeps, its
    // place, venue and organizer, computed once here.
    let units = sqlx::query(&format!(
        "SELECT * FROM (SELECT s.unit_id,s.occurrence->>'uid' AS uid,coalesce(s.occurrence->>'recurrence_id','') AS rid,s.start_at,{FINGERPRINT} AS fingerprint,\
         same_event_name(s.name) AS name_key,same_event_numbers(s.name) AS numbers,ST_AsEWKT(s.point) AS point,s.venue_id,s.doc#>>'{{venue,osm_id}}' AS osm_venue,\
         lower(rtrim(s.doc#>>'{{organizer,url}}','/')) AS organizer,coalesce(s.doc->>'all_day','false')='true' AS all_day,s.doc->>'tz' AS tz \
         FROM search_unit s WHERE s.id=$1 AND {EVENT} AND {WINDOW}) u \
         WHERE NOT EXISTS(SELECT 1 FROM same_event_unit x WHERE x.unit_id=u.unit_id AND x.fingerprint=u.fingerprint) ORDER BY start_at,unit_id LIMIT $2"
    ))
    .bind(id)
    .bind(budget + 1)
    .fetch_all(pool)
    .await?;
    let more = units.len() as i64 > budget;
    let mut paired = 0;
    for u in units.iter().take(budget as usize) {
        let unit: String = u.try_get("unit_id")?;
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
        sqlx::query("DELETE FROM same_event WHERE unit_id=$1 OR other_id=$1")
            .bind(&unit)
            .execute(&mut *tx)
            .await?;
        // The other units already read, on the values they were read with (a change marks a unit
        // to be read again); a unit read later finds this one in turn. Two located units must be
        // near each other; a venue or organizer counts only when one has no place. A candidate
        // another transaction holds (an orphan delete) is skipped, never waited for.
        sqlx::query(
            "WITH me AS (SELECT * FROM same_event_unit WHERE unit_id=$1), best AS (SELECT x.unit_id,similarity(me.name_key,x.name_key) AS score FROM me,same_event_unit x \
             WHERE x.resource_id<>me.resource_id AND me.name_key<>'' AND x.name_key<>'' \
             AND x.start_at BETWEEN me.start_at-interval '36 hours' AND me.start_at+interval '36 hours' \
             AND CASE WHEN me.all_day OR x.all_day THEN same_event_day(me.start_at,me.tz)=same_event_day(x.start_at,x.tz) \
              ELSE abs(extract(epoch FROM x.start_at-me.start_at))<=$2 END \
             AND ((x.point IS NOT NULL AND me.point IS NOT NULL AND ST_DWithin(x.point::geography,me.point::geography,$3)) \
              OR ((x.point IS NULL OR me.point IS NULL) AND (x.venue_id=me.venue_id OR x.osm_venue=me.osm_venue OR x.organizer=me.organizer))) \
             AND similarity(me.name_key,x.name_key)>=$4 \
             AND (cardinality(me.numbers)=0 OR cardinality(x.numbers)=0 OR x.numbers=me.numbers) \
             ORDER BY score DESC,x.unit_id LIMIT $5 FOR KEY SHARE OF x SKIP LOCKED) \
             INSERT INTO same_event(unit_id,other_id,score) SELECT $1,unit_id,score FROM best UNION ALL SELECT unit_id,$1,score FROM best \
             ON CONFLICT(unit_id,other_id) DO UPDATE SET score=excluded.score",
        )
        .bind(&unit)
        .bind(SECONDS as f64)
        .bind(METRES)
        .bind(SIMILARITY as f32)
        .bind(KEPT)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        paired += 1;
    }
    Ok((paired, dropped, more))
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
