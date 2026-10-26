//! The same-recording hint: recordings on different pages that are possibly the same one when
//! nothing exact joins them (no shared ISRC, MBID or barcode), such as a re-release whose ISRC
//! MusicBrainz does not know beside the recording's own page. A hint only: never a group, never
//! counted, never ranks anything. Two recordings (`MusicRecording` roots) are a pair when:
//! - their titles are equal as two cards compare them (`matching::recording_title`: identity-
//!   keeping tags such as "Remastered 2011" go, "Live" or "Remix" stay);
//! - their first credited artists are equal (`matching::artist_name`; a recording without one
//!   pairs with nothing);
//! - their lengths, when both are known, are within [`LENGTH_WINDOW_SECS`] (score 1); with one
//!   unknown they still pair (score 0.7).
//!
//! A member of the card's own group is left out when the card is read, so a pair that a key
//! later joins needs no change here.
//!
//! The pass reads the resources whose recording may have changed (`same_recording_dirty`,
//! marked by triggers on the resource and its facts, and by the sweep), bounded per pass and
//! going on at once while more is due; an entry is claimed for a while and dropped once read, so
//! a stopped pass loses nothing; an hourly sweep marks the recordings never read and those
//! that are no longer recordings (an alias, a retyped or gone page). Cards only read the stored
//! pairs (`same_recording_hint`).
use crate::config::{Config, SameEventHint};
use anyhow::Result;
use sqlx::{PgPool, Row};
use std::time::Duration;
use tokio::sync::watch;

/// How far apart two lengths may be, in seconds (as a service's ISRC is matched to a recording).
pub const LENGTH_WINDOW_SECS: i64 = crate::music_sources::LENGTH_WINDOW_SECS as i64;
/// The score of a pair whose lengths both are known and agree, and of one with a length unknown.
pub const SCORE_LENGTHS_AGREE: f32 = 1.0;
pub const SCORE_LENGTH_UNKNOWN: f32 = 0.7;
/// Pairs kept per recording, best first (a card shows three, its details ten).
const KEPT: i64 = 20;
/// Resources claimed per pass.
const CLAIMED: i64 = 25;

fn enabled(config: &Config) -> bool {
    config.groups.same_recording_hint == SameEventHint::On
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
                    tracing::warn!(error = %crate::resolver::log_error(&error), "same-recording sweep will retry")
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
                tracing::warn!(error = %crate::resolver::log_error(&error), "same-recording pass will retry")
            }
        }
        tokio::select! { _=shutdown.changed()=>break, _=tokio::time::sleep(Duration::from_secs(5))=>{} }
    }
}

/// Marks the recordings never read (a database from before the hint, a recording saved while
/// the hint was off) and the stored units whose resource is no longer a recording root: an
/// alias, a page that is not ok, a retyped page. Returns the resources marked.
pub async fn sweep(pool: &PgPool) -> Result<u64> {
    let entered = sqlx::query(
        "INSERT INTO same_recording_dirty(resource_id) SELECT r.id FROM resource r \
         WHERE r.type_path @> ARRAY['MusicRecording'] AND r.status='ok' \
         AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id) \
         AND NOT EXISTS(SELECT 1 FROM same_recording_unit u WHERE u.resource_id=r.id) \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await?;
    let stale = sqlx::query(
        "INSERT INTO same_recording_dirty(resource_id) SELECT u.resource_id FROM same_recording_unit u JOIN resource r ON r.id=u.resource_id \
         WHERE EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id) OR r.status<>'ok' OR NOT coalesce(r.type_path @> ARRAY['MusicRecording'],false) \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await?;
    Ok(entered.rows_affected() + stale.rows_affected())
}

/// How long a claimed resource is the pass's: one that is not done by then (the pass was
/// stopped, the process ended) is due again.
const LEASE: Duration = Duration::from_secs(300);

/// Claims up to `n` due resources for `lease`: each one's entry stays, moved `lease` ahead, and
/// the time it was moved to is the claim. Exposed for the interruption test; `drain` claims with
/// [`LEASE`].
pub async fn claim(
    pool: &PgPool,
    n: i64,
    lease: Duration,
) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>> {
    let rows = sqlx::query("UPDATE same_recording_dirty d SET next_attempt_at=now()+$2*interval '1 millisecond' \
        WHERE d.resource_id IN (SELECT resource_id FROM same_recording_dirty WHERE next_attempt_at<=now() ORDER BY next_attempt_at,resource_id LIMIT $1 FOR UPDATE SKIP LOCKED) \
        RETURNING d.resource_id,d.next_attempt_at")
        .bind(n)
        .bind(lease.as_millis() as f64)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("resource_id")?, r.try_get("next_attempt_at")?)))
        .collect()
}

/// One pass: claims up to `CLAIMED` due resources (at most `budget`) and reads each one's
/// recording. A claim is settled only while it is valid (see `settle`): a change marked meanwhile
/// moved the entry back to now (the trigger), and a claim that ran out is left due; either way
/// the next pass reads the resource again. A failed read is due again a minute later; an
/// interrupted pass leaves its claims to run out. Returns the resources claimed.
pub async fn drain(pool: &PgPool, config: &Config, budget: i64) -> Result<usize> {
    if !enabled(config) {
        return Ok(0);
    }
    let due = claim(pool, CLAIMED.min(budget.max(1)), LEASE).await?;
    let mut changed = false;
    for (id, claimed) in &due {
        match resource(pool, id).await {
            Ok(moved) => {
                changed |= moved;
                settle(pool, id, claimed).await?;
            }
            Err(error) => {
                tracing::warn!(resource_id = %id, error = %crate::resolver::log_error(&error), "same-recording pairs will retry");
                sqlx::query("UPDATE same_recording_dirty SET next_attempt_at=now()+interval '60 seconds' WHERE resource_id=$1 AND next_attempt_at=$2 AND next_attempt_at>now()")
                    .bind(id)
                    .bind(claimed)
                    .execute(pool)
                    .await?;
            }
        }
    }
    if changed {
        sqlx::query("SELECT nextval('same_recording_revision')")
            .execute(pool)
            .await?;
    }
    Ok(due.len())
}

/// A claimed resource read: its entry goes, if the claim is still valid (the entry holds it and it
/// has not run out). A change saved after a claim ran out leaves the entry's time as it was (the
/// trigger only moves it earlier), so an expired claim is left due and read again. Exposed for
/// the expiry test.
pub async fn settle(
    pool: &PgPool,
    id: &str,
    claimed: &chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query("DELETE FROM same_recording_dirty WHERE resource_id=$1 AND next_attempt_at=$2 AND next_attempt_at>now()")
        .bind(id)
        .bind(claimed)
        .execute(pool)
        .await?;
    Ok(())
}

/// A recording as the pass compares it: its title and artist keys and its length, from its
/// name, the artist and duration facts the sources read (first), else what its page states.
#[derive(Debug, PartialEq)]
pub struct Unit {
    pub title_key: String,
    pub artist_key: String,
    pub length_secs: Option<i64>,
}
impl Unit {
    pub fn new(
        name: &str,
        artist: Option<&str>,
        duration_fact: Option<&str>,
        page_duration: Option<&str>,
    ) -> Self {
        let length_secs = duration_fact
            .and_then(|d| d.parse::<i64>().ok())
            .filter(|s| (1..=86_400).contains(s))
            .or_else(|| page_duration.and_then(crate::music_sources::duration_secs));
        Self {
            title_key: crate::matching::recording_title(name),
            artist_key: crate::matching::artist_name(artist.unwrap_or_default()),
            length_secs,
        }
    }
    fn fingerprint(&self) -> String {
        let bytes =
            serde_json::to_vec(&(&self.title_key, &self.artist_key, self.length_secs)).unwrap();
        blake3::hash(&bytes).to_hex()[..32].to_owned()
    }
}

/// One resource: its unit is dropped (with its pairs) when it is no longer a recording root
/// with a title, else written when its values changed, and its pairs computed anew. Returns
/// whether any pair or unit changed.
async fn resource(pool: &PgPool, id: &str) -> Result<bool> {
    let row = sqlx::query(
        "SELECT r.name,(r.type_path @> ARRAY['MusicRecording'] AND r.status='ok' AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id)) AS recording,\
         r.doc#>>'{attributes,artist}' AS page_artist,r.doc#>>'{attributes,duration}' AS page_duration,\
         (SELECT min(f.value) FROM resource_fact f WHERE f.resource_id=r.id AND f.name='artist' AND f.expires_at>now()) AS artist,\
         (SELECT min(f.value) FROM resource_fact f WHERE f.resource_id=r.id AND f.name='duration' AND f.expires_at>now()) AS duration,\
         (SELECT u.fingerprint FROM same_recording_unit u WHERE u.resource_id=r.id) AS fingerprint \
         FROM resource r WHERE r.id=$1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let name: Option<String> = row.try_get("name")?;
    let recording: bool = row.try_get("recording")?;
    let unit = name.as_deref().filter(|_| recording).map(|name| {
        Unit::new(
            name,
            row.try_get::<Option<String>, _>("artist")
                .ok()
                .flatten()
                .or_else(|| row.try_get("page_artist").ok().flatten())
                .as_deref(),
            row.try_get::<Option<String>, _>("duration")
                .ok()
                .flatten()
                .as_deref(),
            row.try_get::<Option<String>, _>("page_duration")
                .ok()
                .flatten()
                .as_deref(),
        )
    });
    let unit = match unit {
        Some(unit) if !unit.title_key.is_empty() => unit,
        _ => {
            let dropped = sqlx::query("DELETE FROM same_recording_unit WHERE resource_id=$1")
                .bind(id)
                .execute(pool)
                .await?
                .rows_affected();
            return Ok(dropped > 0);
        }
    };
    let fingerprint = unit.fingerprint();
    if row.try_get::<Option<String>, _>("fingerprint")? == Some(fingerprint.clone()) {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO same_recording_unit(resource_id,fingerprint,title_key,artist_key,length_secs) VALUES($1,$2,$3,$4,$5) \
        ON CONFLICT(resource_id) DO UPDATE SET fingerprint=excluded.fingerprint,title_key=excluded.title_key,artist_key=excluded.artist_key,length_secs=excluded.length_secs")
        .bind(id)
        .bind(&fingerprint)
        .bind(&unit.title_key)
        .bind(&unit.artist_key)
        .bind(unit.length_secs.map(|s| s as i32))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM same_recording WHERE resource_id=$1 OR other_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // The other recordings already read, on the values they were read with (a change marks a
    // recording to be read again); a recording read later finds this one in turn. The best
    // `KEPT`: those whose lengths agree (score 1), then those with a length unknown (0.7). Each
    // half reads the index in order and stops at `KEPT`, so a crowd of recordings with one title
    // costs no more than twenty.
    let candidates = "SELECT u.resource_id FROM same_recording_unit u \
        WHERE u.title_key=$2 AND u.artist_key=$3 AND u.resource_id<>$1";
    let mut best: Vec<(String, f32)> = vec![];
    if !unit.artist_key.is_empty() {
        if let Some(len) = unit.length_secs {
            let agree: Vec<String> = sqlx::query_scalar(&format!(
                "{candidates} AND u.length_secs BETWEEN $4 AND $5 ORDER BY u.length_secs,u.resource_id LIMIT $6"
            ))
            .bind(id)
            .bind(&unit.title_key)
            .bind(&unit.artist_key)
            .bind((len - LENGTH_WINDOW_SECS) as i32)
            .bind((len + LENGTH_WINDOW_SECS) as i32)
            .bind(KEPT)
            .fetch_all(&mut *tx)
            .await?;
            best.extend(agree.into_iter().map(|r| (r, SCORE_LENGTHS_AGREE)));
        }
        let room = KEPT - best.len() as i64;
        if room > 0 {
            let unknown_len = if unit.length_secs.is_some() {
                " AND u.length_secs IS NULL"
            } else {
                ""
            };
            let unknown: Vec<String> = sqlx::query_scalar(&format!(
                "{candidates}{unknown_len} ORDER BY u.length_secs,u.resource_id LIMIT $4"
            ))
            .bind(id)
            .bind(&unit.title_key)
            .bind(&unit.artist_key)
            .bind(room)
            .fetch_all(&mut *tx)
            .await?;
            best.extend(unknown.into_iter().map(|r| (r, SCORE_LENGTH_UNKNOWN)));
        }
    }
    let (others, scores): (Vec<String>, Vec<f32>) = best.into_iter().unzip();
    sqlx::query(
        "INSERT INTO same_recording(resource_id,other_id,score) \
         SELECT $1,o,s FROM unnest($2::text[],$3::real[]) AS t(o,s) \
         UNION ALL SELECT o,$1,s FROM unnest($2::text[],$3::real[]) AS t(o,s) \
         ON CONFLICT(resource_id,other_id) DO UPDATE SET score=excluded.score",
    )
    .bind(id)
    .bind(&others)
    .bind(&scores)
    .execute(&mut *tx)
    .await?;
    // Each other recording keeps its best `KEPT` too: this one may push its last one out.
    sqlx::query(
        "DELETE FROM same_recording p USING (SELECT resource_id,other_id,\
         row_number() OVER (PARTITION BY resource_id ORDER BY score DESC,other_id) AS n \
         FROM same_recording WHERE resource_id=ANY($1)) r \
         WHERE p.resource_id=r.resource_id AND p.other_id=r.other_id AND r.n>$2",
    )
    .bind(&others)
    .bind(KEPT)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit takes the sources' artist and length first, the page's when they have none; a
    /// length is seconds (a fact) or an ISO duration (the page); the keys are the normalised
    /// title and first artist.
    #[test]
    fn a_unit_is_the_title_the_first_artist_and_the_length() {
        let unit = Unit::new(
            "Bohemian Rhapsody - Remastered 2011",
            Some("Queen & David Bowie"),
            Some("355"),
            Some("PT5M54S"),
        );
        assert_eq!(
            unit,
            Unit {
                title_key: "bohemian rhapsody".into(),
                artist_key: "queen".into(),
                length_secs: Some(355),
            }
        );
        let page = Unit::new("Bohemian Rhapsody", None, None, Some("PT5M54S"));
        assert_eq!(
            (page.artist_key.as_str(), page.length_secs),
            ("", Some(354))
        );
        assert_eq!(Unit::new("x", None, Some("0"), None).length_secs, None);
        assert_ne!(unit.fingerprint(), page.fingerprint());
    }
}
