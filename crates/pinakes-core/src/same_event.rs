//! The same-event hint: event units (an event resource, or one occurrence of a feed) on different
//! pages that are possibly the same event, such as one meetup on Meetup, Luma and its organiser's
//! site. A hint only: never a group, never counted, never ranks anything. Two units are a pair
//! when all of these hold:
//! - they start within `SECONDS` of each other (an all-day event: on the same local date);
//! - they are within `METRES` of each other, or name the same venue or organizer;
//! - their normalised names (`same_event_name` in SQL) are at least `SIMILARITY` alike (pg_trgm);
//! - they are on different pages (two occurrences of one feed never pair).
//!
//! The pass reads the units of resources whose events changed (`same_event_dirty`, marked by
//! triggers) and starting in the window (`PAST` before now to `FUTURE` after), bounded per pass;
//! an hourly sweep drops units that left the window and marks those that entered it or changed
//! unseen. Cards only read the stored pairs (`same_event_hint`).
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
            match sweep(&pool, 1000).await {
                Ok(_) => swept = Some(tokio::time::Instant::now()),
                Err(error) => tracing::warn!(%error, "same-event sweep will retry"),
            }
        }
        let pass = tokio::select! { _=shutdown.changed()=>break, result=drain(&pool, &config, 200)=>result };
        match pass {
            // More is due: go on at once.
            Ok(n) if n >= 200 => {
                tokio::task::yield_now().await;
                continue;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "same-event pass will retry"),
        }
        tokio::select! { _=shutdown.changed()=>break, _=tokio::time::sleep(Duration::from_secs(5))=>{} }
    }
}

/// Drops the units that left the window (their pairs go with them) and marks the resources of up
/// to `limit` units that entered it or changed without a mark. Returns the resources marked.
pub async fn sweep(pool: &PgPool, limit: i64) -> Result<u64> {
    sqlx::query("DELETE FROM same_event_unit WHERE start_at<now()-interval '1 day'")
        .execute(pool)
        .await?;
    let marked = sqlx::query(&format!(
        "INSERT INTO same_event_dirty(resource_id) SELECT DISTINCT id FROM (SELECT s.id FROM search_unit s \
         WHERE {EVENT} AND {WINDOW} AND NOT EXISTS(SELECT 1 FROM same_event_unit x WHERE x.unit_id=s.unit_id AND x.fingerprint={FINGERPRINT}) \
         LIMIT $1) m ON CONFLICT DO NOTHING"
    ))
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(marked.rows_affected())
}

/// One pass: the due resources' units that are new or changed, at most `budget` of them; a
/// resource with more stays due. Returns the units read.
pub async fn drain(pool: &PgPool, config: &Config, budget: i64) -> Result<usize> {
    if !enabled(config) {
        return Ok(0);
    }
    let due: Vec<String> = sqlx::query_scalar("SELECT resource_id FROM same_event_dirty WHERE next_attempt_at<=now() ORDER BY next_attempt_at,resource_id LIMIT 25")
        .fetch_all(pool)
        .await?;
    let mut read = 0;
    for id in due {
        if read as i64 >= budget {
            break;
        }
        // Unmarked first: a change while the units are read marks the resource again.
        sqlx::query("DELETE FROM same_event_dirty WHERE resource_id=$1")
            .bind(&id)
            .execute(pool)
            .await?;
        match resource(pool, &id, budget - read as i64).await {
            Ok((n, more)) => {
                read += n;
                if more {
                    mark(pool, &id, 0).await?;
                }
            }
            Err(error) => {
                tracing::warn!(resource_id = %id, %error, "same-event pairs will retry");
                mark(pool, &id, 60).await?;
            }
        }
    }
    Ok(read)
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
/// of its new or changed units. Returns the units paired and whether more are left.
async fn resource(pool: &PgPool, id: &str, budget: i64) -> Result<(usize, bool)> {
    sqlx::query(&format!(
        "WITH current AS (SELECT s.unit_id FROM search_unit s WHERE s.id=$1 AND {EVENT} AND {WINDOW}) \
         DELETE FROM same_event_unit WHERE resource_id=$1 AND unit_id NOT IN (SELECT unit_id FROM current)"
    ))
    .bind(id)
    .execute(pool)
    .await?;
    let units = sqlx::query(&format!(
        "SELECT * FROM (SELECT s.unit_id,s.occurrence->>'uid' AS uid,coalesce(s.occurrence->>'recurrence_id','') AS rid,s.start_at,{FINGERPRINT} AS fingerprint,\
         s.name,ST_Y(s.point) AS lat,ST_X(s.point) AS lon,s.venue_id,s.doc#>>'{{venue,osm_id}}' AS osm_venue,lower(rtrim(s.doc#>>'{{organizer,url}}','/')) AS organizer,\
         coalesce(s.doc->>'all_day','false')='true' AS all_day,s.doc->>'tz' AS tz FROM search_unit s WHERE s.id=$1 AND {EVENT} AND {WINDOW}) u \
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
        let uid: Option<String> = u.try_get("uid")?;
        sqlx::query("INSERT INTO same_event_unit(unit_id,resource_id,uid,recurrence_id,fingerprint,start_at) VALUES($1,$2,$3,CASE WHEN $3 IS NULL THEN NULL ELSE $4 END,$5,$6) \
            ON CONFLICT(unit_id) DO UPDATE SET fingerprint=excluded.fingerprint,start_at=excluded.start_at")
            .bind(&unit)
            .bind(id)
            .bind(&uid)
            .bind(u.try_get::<String, _>("rid")?)
            .bind(u.try_get::<String, _>("fingerprint")?)
            .bind(u.try_get::<chrono::DateTime<chrono::Utc>, _>("start_at")?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM same_event WHERE unit_id=$1 OR other_id=$1")
            .bind(&unit)
            .execute(&mut *tx)
            .await?;
        // The other units already read, matched against their current values; a unit read later
        // finds this one in turn.
        sqlx::query(&format!(
            "WITH best AS (SELECT s.unit_id,similarity(same_event_name($2),same_event_name(s.name)) AS score FROM search_unit s JOIN same_event_unit x ON x.unit_id=s.unit_id \
             WHERE s.id<>$3 AND {EVENT} \
             AND s.start_at BETWEEN $4-interval '36 hours' AND $4+interval '36 hours' \
             AND CASE WHEN $10 OR coalesce(s.doc->>'all_day','false')='true' THEN same_event_day($4,$11)=same_event_day(s.start_at,s.doc->>'tz') \
              ELSE abs(extract(epoch FROM s.start_at-$4))<=$12 END \
             AND ((s.point IS NOT NULL AND $5::float8 IS NOT NULL AND ST_DWithin(s.point::geography,ST_SetSRID(ST_MakePoint($6,$5),4326)::geography,$13)) \
              OR s.venue_id=$7 OR s.doc#>>'{{venue,osm_id}}'=$8 OR lower(rtrim(s.doc#>>'{{organizer,url}}','/'))=$9) \
             AND same_event_name($2)<>'' AND similarity(same_event_name($2),same_event_name(s.name))>=$14 \
             ORDER BY score DESC,s.unit_id LIMIT $15) \
             INSERT INTO same_event(unit_id,other_id,score) SELECT $1,unit_id,score FROM best UNION ALL SELECT unit_id,$1,score FROM best \
             ON CONFLICT(unit_id,other_id) DO UPDATE SET score=excluded.score"
        ))
        .bind(&unit)
        .bind(u.try_get::<Option<String>, _>("name")?)
        .bind(id)
        .bind(u.try_get::<chrono::DateTime<chrono::Utc>, _>("start_at")?)
        .bind(u.try_get::<Option<f64>, _>("lat")?)
        .bind(u.try_get::<Option<f64>, _>("lon")?)
        .bind(u.try_get::<Option<String>, _>("venue_id")?)
        .bind(u.try_get::<Option<String>, _>("osm_venue")?)
        .bind(u.try_get::<Option<String>, _>("organizer")?)
        .bind(u.try_get::<bool, _>("all_day")?)
        .bind(u.try_get::<Option<String>, _>("tz")?)
        .bind(SECONDS as f64)
        .bind(METRES)
        .bind(SIMILARITY as f32)
        .bind(KEPT)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        paired += 1;
    }
    Ok((paired, more))
}
