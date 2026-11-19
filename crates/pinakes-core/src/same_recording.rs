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
use crate::hints::commit_changed;
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
/// The most recordings that may name one recording, so a change to it marks at most this many
/// for a refill.
const INCOMING_KEPT: i64 = 2 * KEPT;
/// Candidates examined per read at most (`hints::SCAN`): those passed over for having no room
/// count, so a read in a crowd of recordings with one title is bounded however dense it is. A
/// read that runs out with room left goes on from the position it stopped at in a later one
/// (`scan_after`, `scan_after_length`).
pub const SCAN: i64 = crate::hints::SCAN;
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
/// the hint was off), the stored units whose resource is no longer a recording root (an alias,
/// a page that is not ok, a retyped page) and those still waiting for a refill (see `refill`).
/// Returns the resources marked.
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
         OR u.fingerprint LIKE 'refill:%' \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await?;
    Ok(entered.rows_affected() + stale.rows_affected())
}

/// How long a claimed resource is the pass's: one that is not done by then (the pass was
/// stopped, the process ended) is due again.
const LEASE: Duration = Duration::from_secs(300);
/// The pass's queue: one row per due resource.
const DIRTY: &str = "same_recording_dirty";

/// Claims up to `n` due resources for `lease`: each one's entry stays, moved `lease` ahead, and
/// the time it was moved to is the claim. Exposed for the interruption test; `drain` claims with
/// [`LEASE`].
pub async fn claim(
    pool: &PgPool,
    n: i64,
    lease: Duration,
) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>> {
    crate::hints::claim(pool, DIRTY, n, lease).await
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
    for (id, claimed) in &due {
        match resource(pool, id).await {
            Ok(_) => settle(pool, id, claimed).await?,
            Err(error) => {
                tracing::warn!(resource_id = %id, error = %crate::resolver::log_error(&error), "same-recording pairs will retry");
                crate::hints::release(pool, DIRTY, id, claimed, 60).await?;
            }
        }
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
    crate::hints::settle(pool, DIRTY, id, claimed).await
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

/// What one read did: whether any pair or unit changed, and how many candidates it examined
/// (`SCAN` at most).
#[derive(Debug, Default, PartialEq)]
pub struct Read {
    pub changed: bool,
    pub examined: usize,
}

/// One resource, in its own transaction: committed with the search revision when it changed
/// anything, else rolled back.
async fn resource(pool: &PgPool, id: &str) -> Result<Read> {
    let mut tx = pool.begin().await?;
    let read = read_in(&mut tx, id).await?;
    if read.changed {
        commit_changed(tx).await?;
    } else {
        tx.rollback().await?;
    }
    Ok(read)
}

/// One resource read in `tx`: its unit is dropped (with its pairs) when it is no longer a
/// recording root with a title, else written when its values changed, and its pairs computed
/// anew (or, for a read that stopped short, from where it stopped). The caller commits a read
/// that `changed` with `commit_changed`. Its candidates are taken `FOR KEY SHARE … SKIP LOCKED`,
/// so it never waits for a unit another transaction holds (an orphan being deleted, whose
/// `before_delete` may be waiting for this unit in turn). Exposed for the lock test.
pub async fn read_in(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: &str) -> Result<Read> {
    let row = sqlx::query(
        "SELECT r.name,(r.type_path @> ARRAY['MusicRecording'] AND r.status='ok' AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id)) AS recording,\
         r.doc#>>'{attributes,artist}' AS page_artist,r.doc#>>'{attributes,duration}' AS page_duration,\
         (SELECT min(f.value) FROM resource_fact f WHERE f.resource_id=r.id AND f.name='artist' AND f.expires_at>now() AND f.source NOT IN (SELECT source FROM also_on_source_off)) AS artist,\
         (SELECT min(f.value) FROM resource_fact f WHERE f.resource_id=r.id AND f.name='duration' AND f.expires_at>now() AND f.source NOT IN (SELECT source FROM also_on_source_off)) AS duration,\
         u.fingerprint,u.scan_after AS stopped_at,u.scan_after_length AS stopped_at_length \
         FROM resource r LEFT JOIN same_recording_unit u ON u.resource_id=r.id WHERE r.id=$1",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(Read::default());
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
    let stored: Option<String> = row.try_get("fingerprint")?;
    let unit = match unit {
        Some(unit) if !unit.title_key.is_empty() => unit,
        _ => {
            // No longer a recording: its unit goes, its pairs with it (on both sides), and the
            // recordings that named it are read again to refill their lists.
            if stored.is_none() {
                return Ok(Read::default());
            }
            before_delete(tx, id).await?;
            sqlx::query("DELETE FROM same_recording_unit WHERE resource_id=$1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            return Ok(Read {
                changed: true,
                examined: 0,
            });
        }
    };
    let fingerprint = unit.fingerprint();
    if stored.as_deref() == Some(fingerprint.as_str()) {
        return Ok(Read::default());
    }
    // A refill (marked `refill:{fingerprint}` because a recording it named changed) whose values
    // are still those replaces its own list only: what names it stays right, and nothing else is
    // marked, so refills never cascade. A recording whose values changed (a refill's too) drops
    // the pairs naming it.
    let previous = stored
        .as_deref()
        .map(|f| f.strip_prefix("refill:").unwrap_or(f));
    let values_changed = previous.is_some_and(|f| f != fingerprint);
    // The other recordings already read, on the values they were read with (a change marks a
    // recording to be read again); a recording read later finds this one in turn. The best
    // `KEPT`, from the ring of candidates (`Ring`): those whose lengths agree (score 1), then
    // those with a length unknown (0.7), each segment read in index order from just after this
    // recording and wrapping round, so a crowd of recordings with one title spreads its pairs
    // over the crowd instead of all naming its first twenty. A recording already named by
    // INCOMING_KEPT that all rank above this one has no room and is passed over (`has_room`),
    // so the cap below pushes out only a pair this one displaces.
    let mut ring = Ring::of(&unit, id);
    // A read examines at most `SCAN` candidates (a dense crowd's full ones are examined and
    // passed over, so a read of it is bounded all the same). One that ran out with room left
    // stored the position it stopped at (`scan_after` with its length then, as the index orders
    // candidates) and marked itself to be read again: that read keeps the pairs it has and goes
    // on from that position, while the values are the same. The position is a place in the
    // index, not the candidate: one that went, or whose length moved it, changes nothing; a
    // position no segment holds (a length window that moved) starts the read over.
    let stopped_at = match row.try_get::<Option<String>, _>("stopped_at")? {
        Some(id) => Some(Pos {
            length: row.try_get("stopped_at_length")?,
            id,
        }),
        None => None,
    };
    let resumed = !values_changed && stopped_at.is_some_and(|at| ring.resume_after(at));
    sqlx::query("INSERT INTO same_recording_unit(resource_id,fingerprint,title_key,artist_key,length_secs,scan_after,scan_after_length) VALUES($1,$2,$3,$4,$5,NULL,NULL) \
        ON CONFLICT(resource_id) DO UPDATE SET fingerprint=excluded.fingerprint,title_key=excluded.title_key,artist_key=excluded.artist_key,length_secs=excluded.length_secs,scan_after=NULL,scan_after_length=NULL")
        .bind(id)
        .bind(&fingerprint)
        .bind(&unit.title_key)
        .bind(&unit.artist_key)
        .bind(unit.length_secs.map(|s| s as i32))
        .execute(&mut **tx)
        .await?;
    // The pairs a resumed read keeps (left out of the candidates, so they take no place among
    // those a read may still take); any other read starts from none.
    let mut existing: Vec<String> = vec![];
    if resumed {
        existing = sqlx::query_scalar("SELECT other_id FROM same_recording WHERE resource_id=$1")
            .bind(id)
            .fetch_all(&mut **tx)
            .await?;
    } else {
        sqlx::query("DELETE FROM same_recording WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        if values_changed {
            // The pairs naming it go, and their recordings refill their lists (at most
            // INCOMING_KEPT of them, the cap below).
            let owners: Vec<String> = sqlx::query_scalar(
                "DELETE FROM same_recording WHERE other_id=$1 RETURNING resource_id",
            )
            .bind(id)
            .fetch_all(&mut **tx)
            .await?;
            refill(tx, &owners).await?;
        }
    }
    let mut best: Vec<(String, f32)> = vec![];
    let mut scan_left = SCAN;
    let mut examined = 0usize;
    let mut last_examined: Option<(String, Option<i32>)> = None;
    if !unit.artist_key.is_empty() {
        for segment in &ring.segments {
            let room = KEPT - (existing.len() + best.len()) as i64;
            if room <= 0 || scan_left <= 0 {
                break;
            }
            let mut args = vec![
                Arg::Text(id.to_owned()),
                Arg::Text(unit.title_key.clone()),
                Arg::Text(unit.artist_key.clone()),
                Arg::TextArray(existing.clone()),
            ];
            // The segment's next candidates, as many as may still be examined, in index order;
            // one another transaction holds (an orphan being deleted) is skipped, never waited
            // for, so a pass holding its own unit and a delete waiting for that unit never wait
            // for each other.
            let slice_sql = format!(
                "SELECT u.resource_id,u.length_secs FROM same_recording_unit u WHERE u.title_key=$2 AND u.artist_key=$3 AND u.resource_id<>$1 AND NOT u.resource_id=ANY($4) AND {} \
                 ORDER BY u.length_secs,u.resource_id LIMIT {scan_left} FOR KEY SHARE OF u SKIP LOCKED",
                segment.predicate(&mut args)
            );
            let slice: Vec<(String, Option<i32>)> = bind_all(sqlx::query(&slice_sql), &args)
                .fetch_all(&mut **tx)
                .await?
                .iter()
                .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
                .collect::<Result<_, sqlx::Error>>()?;
            if slice.is_empty() {
                continue;
            }
            // Of those, the first with room, up to the room left: the check stops at the last one
            // taken, so what it examined is the slice up to that one, or all of it.
            let score = arg(&mut args, Arg::Real(segment.score()));
            let cap = arg(&mut args, Arg::Int8(INCOMING_KEPT));
            let room_sql = format!(
                "SELECT c.resource_id FROM ({slice_sql}) c WHERE {} ORDER BY c.length_secs,c.resource_id LIMIT {room}",
                crate::hints::has_room("same_recording", "resource_id", "c.resource_id", "$1", &score, &cap)
            );
            let taken: Vec<String> = bind_all(sqlx::query(&room_sql), &args)
                .fetch_all(&mut **tx)
                .await?
                .iter()
                .map(|r| r.try_get(0))
                .collect::<Result<_, _>>()?;
            let up_to = match taken.last() {
                Some(last) if taken.len() as i64 == room => slice
                    .iter()
                    .position(|r| r.0 == *last)
                    .map_or(slice.len(), |i| i + 1),
                _ => slice.len(),
            };
            scan_left -= up_to as i64;
            examined += up_to;
            last_examined = Some(slice[up_to - 1].clone());
            best.extend(
                taken
                    .into_iter()
                    .filter(|r| !existing.contains(r))
                    .map(|r| (r, segment.score())),
            );
        }
    }
    // Out of candidates to examine with room left: this read stops here and is read again from
    // this position (due now, after what is due already).
    if KEPT - (existing.len() + best.len()) as i64 > 0 && scan_left <= 0 {
        if let Some((at, length)) = &last_examined {
            sqlx::query("UPDATE same_recording_unit SET scan_after=$2,scan_after_length=$3 WHERE resource_id=$1")
                .bind(id)
                .bind(at)
                .bind(length)
                .execute(&mut **tx)
                .await?;
            refill(tx, std::slice::from_ref(&id.to_owned())).await?;
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
    .execute(&mut **tx)
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
    .execute(&mut **tx)
    .await?;
    // And no recording is named by more than INCOMING_KEPT others, so a change to it marks at
    // most that many for a refill. A recording whose pair is pushed out refills its list (never
    // this one: it took only places it had room for).
    let mut named = others.clone();
    named.push(id.to_owned());
    let pushed_out: Vec<String> = sqlx::query_scalar(
        "DELETE FROM same_recording p USING (SELECT resource_id,other_id,\
         row_number() OVER (PARTITION BY other_id ORDER BY score DESC,resource_id) AS n \
         FROM same_recording WHERE other_id=ANY($1)) r \
         WHERE p.resource_id=r.resource_id AND p.other_id=r.other_id AND r.n>$2 RETURNING p.resource_id",
    )
    .bind(&named)
    .bind(INCOMING_KEPT)
    .fetch_all(&mut **tx)
    .await?;
    let pushed_out: Vec<String> = pushed_out.into_iter().filter(|r| r != id).collect();
    refill(tx, &pushed_out).await?;
    Ok(Read {
        changed: true,
        examined,
    })
}

/// A place in the index's (length, id) order: a candidate examined last, where a read stopped.
struct Pos {
    length: Option<i32>,
    id: String,
}

/// The ring of a recording's candidates, in segments read in turn, each in index order from a
/// start (exclusive) to an end (exclusive). With a length: the window of agreeing lengths after
/// this recording, then the window before it, then the lengths unknown after its id, then those
/// before it. Without one, every candidate scores the same, so the ring runs through the whole
/// index from just after it: the lengths unknown after it, every length known, the lengths
/// unknown before it.
struct Ring {
    segments: Vec<Segment>,
}

enum Segment {
    /// Lengths within `lo..=hi`, in (length, id) order.
    Agree {
        lo: i32,
        hi: i32,
        after: Option<(i32, String)>,
        before: Option<(i32, String)>,
    },
    /// Lengths unknown, in id order.
    Unknown {
        after: Option<String>,
        before: Option<String>,
    },
    /// Every length known, in (length, id) order.
    Known { after: Option<(i32, String)> },
}

impl Ring {
    fn of(unit: &Unit, me: &str) -> Self {
        let me = me.to_owned();
        let segments = match unit.length_secs {
            Some(len) => {
                let lo = (len - LENGTH_WINDOW_SECS) as i32;
                let hi = (len + LENGTH_WINDOW_SECS) as i32;
                let at = (len as i32, me.clone());
                vec![
                    Segment::Agree {
                        lo,
                        hi,
                        after: Some(at.clone()),
                        before: None,
                    },
                    Segment::Agree {
                        lo,
                        hi,
                        after: None,
                        before: Some(at),
                    },
                    Segment::Unknown {
                        after: Some(me.clone()),
                        before: None,
                    },
                    Segment::Unknown {
                        after: None,
                        before: Some(me),
                    },
                ]
            }
            None => vec![
                Segment::Unknown {
                    after: Some(me.clone()),
                    before: None,
                },
                Segment::Known { after: None },
                Segment::Unknown {
                    after: None,
                    before: Some(me),
                },
            ],
        };
        Self { segments }
    }

    /// Goes on from just after `at`: the segments before the one holding it go. False (the ring
    /// unchanged) when no segment holds it.
    fn resume_after(&mut self, at: Pos) -> bool {
        let Some(i) = self.segments.iter().position(|s| s.holds(&at)) else {
            return false;
        };
        self.segments.drain(..i);
        self.segments[0].start_after(at);
        true
    }
}

impl Segment {
    fn holds(&self, at: &Pos) -> bool {
        match self {
            Segment::Agree {
                lo,
                hi,
                after,
                before,
            } => at.length.is_some_and(|l| {
                let pos = (l, at.id.as_str());
                (*lo..=*hi).contains(&l)
                    && after.as_ref().is_none_or(|a| pos > (a.0, a.1.as_str()))
                    && before.as_ref().is_none_or(|b| pos < (b.0, b.1.as_str()))
            }),
            Segment::Unknown { after, before } => {
                at.length.is_none()
                    && after.as_ref().is_none_or(|a| at.id > *a)
                    && before.as_ref().is_none_or(|b| at.id < *b)
            }
            Segment::Known { after } => at.length.is_some_and(|l| {
                after
                    .as_ref()
                    .is_none_or(|a| (l, at.id.as_str()) > (a.0, a.1.as_str()))
            }),
        }
    }

    fn start_after(&mut self, at: Pos) {
        match self {
            Segment::Agree { after, .. } | Segment::Known { after } => {
                *after = at.length.map(|l| (l, at.id));
            }
            Segment::Unknown { after, .. } => *after = Some(at.id),
        }
    }

    fn score(&self) -> f32 {
        match self {
            Segment::Agree { .. } => SCORE_LENGTHS_AGREE,
            _ => SCORE_LENGTH_UNKNOWN,
        }
    }

    /// The segment as a condition on `u`, its bounds added to `args`.
    fn predicate(&self, args: &mut Vec<Arg>) -> String {
        let mut sql = match self {
            Segment::Agree { lo, hi, .. } => format!(
                "u.length_secs BETWEEN {} AND {}",
                arg(args, Arg::Int(*lo)),
                arg(args, Arg::Int(*hi))
            ),
            Segment::Unknown { .. } => "u.length_secs IS NULL".to_owned(),
            Segment::Known { .. } => "u.length_secs IS NOT NULL".to_owned(),
        };
        match self {
            Segment::Agree { after, before, .. } => {
                for (bound, op) in [(after, '>'), (before, '<')] {
                    if let Some((length, id)) = bound {
                        sql += &format!(
                            " AND (u.length_secs,u.resource_id){op}({},{})",
                            arg(args, Arg::Int(*length)),
                            arg(args, Arg::Text(id.clone()))
                        );
                    }
                }
            }
            Segment::Unknown { after, before } => {
                for (bound, op) in [(after, '>'), (before, '<')] {
                    if let Some(id) = bound {
                        sql +=
                            &format!(" AND u.resource_id{op}{}", arg(args, Arg::Text(id.clone())));
                    }
                }
            }
            Segment::Known { after } => {
                if let Some((length, id)) = after {
                    sql += &format!(
                        " AND (u.length_secs,u.resource_id)>({},{})",
                        arg(args, Arg::Int(*length)),
                        arg(args, Arg::Text(id.clone()))
                    );
                }
            }
        }
        sql
    }
}

/// A bound value of a query built from segments, in the order the placeholders number them.
enum Arg {
    Text(String),
    TextArray(Vec<String>),
    Int(i32),
    Int8(i64),
    Real(f32),
}

/// Adds a value and returns its placeholder.
fn arg(args: &mut Vec<Arg>, value: Arg) -> String {
    args.push(value);
    format!("${}", args.len())
}

fn bind_all<'q>(
    mut query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    args: &'q [Arg],
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for value in args {
        query = match value {
            Arg::Text(s) => query.bind(s.as_str()),
            Arg::TextArray(v) => query.bind(v.as_slice()),
            Arg::Int(i) => query.bind(*i),
            Arg::Int8(i) => query.bind(*i),
            Arg::Real(f) => query.bind(*f),
        };
    }
    query
}

/// Before a recording's unit goes, with its resource (an orphan, `changes::delete_orphans`) or
/// alone (no longer a recording): its pairs go with it, so the recordings that named it are marked
/// to refill. Done in the code before the delete, never by a trigger on it: a trigger that
/// updated a row the same statement deletes aborts the statement (PostgreSQL: "tuple to be
/// updated was already modified by an operation triggered by the current command"). Nothing
/// happens for a resource without a unit.
pub async fn before_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    resource_id: &str,
) -> Result<()> {
    // The unit is locked first, so a pass that would pair with it meanwhile waits for the delete
    // (its pair's FK check needs the unit): a pair added between the namers' collection and the
    // cascade would go unmarked.
    sqlx::query("SELECT resource_id FROM same_recording_unit WHERE resource_id=$1 FOR UPDATE")
        .bind(resource_id)
        .execute(&mut **tx)
        .await?;
    let owners: Vec<String> =
        sqlx::query_scalar("SELECT resource_id FROM same_recording WHERE other_id=$1")
            .bind(resource_id)
            .fetch_all(&mut **tx)
            .await?;
    refill(tx, &owners).await
}

/// Recordings whose lists lost a pair are read again: their fingerprint is marked `refill:` (so
/// the read recomputes their list instead of finding it current, and still knows their values)
/// and their entry is due now.
async fn refill(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE same_recording_unit SET fingerprint='refill:'||fingerprint WHERE resource_id=ANY($1) AND fingerprint NOT LIKE 'refill:%'")
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    // Distinct: a recording pushed out of two lists at once is one row.
    sqlx::query("INSERT INTO same_recording_dirty(resource_id) SELECT DISTINCT x FROM unnest($1::text[]) AS t(x) \
        ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=excluded.next_attempt_at WHERE same_recording_dirty.next_attempt_at>excluded.next_attempt_at")
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Reads one resource's recording now, as a pass does (without its claim). Exposed for tests.
pub async fn read(pool: &PgPool, id: &str) -> Result<Read> {
    resource(pool, id).await
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
