//! The Tranco catalogue: the homepages of the sites on Tranco's daily top-million list that
//! answer with a page, one card each.
//!
//! The list itself is the import: each site goes into `catalogue_site` with its rank, the last
//! list that named it and when its homepage is read next. The homepages are then read in the
//! background, [`READS`] at a time and for at most [`READ_BUDGET`] per run, through the daemon's
//! egress (pinned DNS, no private address, each host's pace, the body caps) and only where the
//! site's `robots.txt` lets the agent `pinakes` read `/` ([`robots`]). A homepage that answers
//! with an HTML page that is no bot wall (`resolver::web::extract`), on the site itself, becomes
//! a card: its title and description, the link `https://{site}/`, the key `homepage:{site}`
//! (`join::link_keys`), and fame from the rank. Everything else (a wall, a robots refusal, no
//! answer, another type, a redirect to another site) makes no card, and a card the site had is
//! removed. Homepages are read again after [`REREAD`]. One that failed to answer, answered 429
//! (too many requests), a timeout or a server error is tried again the next day (a 5xx's or 408's
//! not before its `Retry-After`; the egress holds a 429's to an hour), three times in a row,
//! before its card goes; so does a wall in front of a site that has a card. A read the egress gave no turn in
//! time is no read of the site: it is due again in an hour (or when the egress's wait ends).
//! Reads wait while a large catalogue that is on (and has its importer) has not finished its
//! first full import or runs one ([`reads_wait`]); meanwhile Tranco's list ID is asked at most
//! once a day.
//!
//! An update loads a newer list when there is one, removes the sites that have been off the
//! list for [`OFF_LIST`], then reads the homepages that are due.
use super::{
    failure::Failure,
    fetch::{self, Input},
    robots::{self, Robots},
    run::{Importer, Job, Plan, Run, UpdateEnd},
    store::State,
    Card, Catalogue,
};
use crate::{config::Config, resolver::egress::Egress};
use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{io::BufRead, sync::Arc, time::Duration};

/// The list's ID of the day, and the list.
pub const LATEST_ID: &str = "https://tranco-list.eu/top-1m-id";
pub fn list_url(id: &str) -> String {
    format!("https://tranco-list.eu/download/{id}/1000000")
}
/// Homepages read at once.
pub const READS: usize = 16;
/// How long one run reads homepages before it ends (the next run goes on).
pub const READ_BUDGET: Duration = Duration::from_secs(3600);
/// How long a homepage read stays current.
pub const REREAD: Duration = Duration::from_secs(90 * 86400);
/// How long a site may be off the list before its card goes.
pub const OFF_LIST: i64 = 30;
/// How often a site still on the list has its "last listed" day and its rank moved (so a daily
/// list does not rewrite every row every day).
const LISTED_REFRESH: i64 = 7;
/// The longest wait a `Retry-After` or the egress may set before a site is read again (a site
/// that asks for longer still loses its card after three transient failures in a row).
const LONGEST_WAIT: u64 = 7 * 86400;
/// The most sites a list may name (1,000,000).
pub const SITES_CAP: usize = 2_000_000;
/// Reads that failed in a row (no answer, a server error) before the card goes.
const FAILURES: i32 = 3;
/// The least time between two runs that read what fell due ([`plan_after`]).
pub const WAKE_AFTER: chrono::Duration = chrono::Duration::minutes(10);
/// The longest a homepage read waits for its host's or address's turn at the egress.
const SLOT_WAIT: Duration = Duration::from_secs(2);
/// Sites claimed per round of reads.
const CLAIM: i64 = 64;
/// The longest list read (22 MB today).
const LIST_MAX: u64 = 256 << 20;

/// Where the list comes from.
#[derive(Clone, Debug)]
pub enum ListSource {
    /// Tranco's site: the day's list ID, then the list.
    Official,
    /// A list file or URL as it is (`--from`).
    Given(Input),
}

/// The importer.
pub struct Tranco {
    client: reqwest::Client,
    egress: Egress,
    list: ListSource,
    /// At most this many homepages read per run (`--limit`; none: the time budget alone).
    reads: Option<u64>,
    /// The robots.txt rules read, by origin, kept across runs ([`RobotsCache`]).
    robots: RobotsCache,
}
impl Tranco {
    pub fn new(list: ListSource, egress: Egress, contact: &str) -> Result<Self> {
        Ok(Self {
            client: fetch::client(contact, &[])?,
            egress,
            list,
            reads: None,
            robots: RobotsCache::default(),
        })
    }
    /// Read at most `n` homepages in a run.
    pub fn reads(mut self, n: u64) -> Self {
        self.reads = Some(n);
        self
    }
    /// The list to read now: its version (Tranco's list ID, or the file's), its input, and its day.
    async fn latest(&self) -> Result<(String, Input, Option<chrono::DateTime<chrono::Utc>>)> {
        match &self.list {
            ListSource::Official => {
                let id_input = Input::Url(url::Url::parse(LATEST_ID).expect("a URL"));
                let id = id_input.read_small(&self.client, 64).await?;
                let id = String::from_utf8_lossy(&id).trim().to_owned();
                if id.is_empty() || id.len() > 16 || !id.bytes().all(|b| b.is_ascii_alphanumeric())
                {
                    anyhow::bail!(Failure::new("version").at("tranco-list.eu"));
                }
                let input = Input::Url(url::Url::parse(&list_url(&id))?);
                let (_, at) = input
                    .version(&self.client)
                    .await
                    .unwrap_or((String::new(), None));
                Ok((id, input, at))
            }
            ListSource::Given(input) => {
                let (version, at) = input.version(&self.client).await?;
                Ok((version, input.clone(), at))
            }
        }
    }
    /// Load the list into `catalogue_site`: new sites due now, a known site's rank and last
    /// listed day moved when that day is more than a week old (a row is written at most weekly).
    async fn load(&self, job: &mut Job<'_>, input: &Input, day: chrono::NaiveDate) -> Result<u64> {
        // The list (22 MB) is read whole: Tranco sends it without a length, so not as a download
        // that resumes.
        let body = input.read_small(&self.client, LIST_MAX).await?;
        let at = input.at();
        let sites = tokio::task::spawn_blocking(move || -> Result<Vec<(String, i32)>> {
            list(&body[..], &at)
        })
        .await
        .map_err(|_| Failure::new("panic"))??;
        // A full import whose list names no site reads no record: the runner refuses it.
        job.count_read(sites.len() as u64);
        let pool = job.pool().clone();
        for part in sites.chunks(10_000) {
            let hosts: Vec<&str> = part.iter().map(|(h, _)| h.as_str()).collect();
            let ranks: Vec<i32> = part.iter().map(|(_, r)| *r).collect();
            sqlx::query(
                "INSERT INTO catalogue_site(host,rank,listed_on,due_at) SELECT h,r,$3,now() FROM unnest($1::text[],$2::int[]) AS x(h,r) \
                 ON CONFLICT (host) DO UPDATE SET rank=excluded.rank,listed_on=excluded.listed_on \
                 WHERE catalogue_site.listed_on < excluded.listed_on - $4::int",
            )
            .bind(&hosts)
            .bind(&ranks)
            .bind(day)
            .bind(LISTED_REFRESH as i32)
            .execute(&pool)
            .await?;
        }
        Ok(sites.len() as u64)
    }
    /// The sites off the list for [`OFF_LIST`] days (from `day`) lose their card and their row;
    /// when their cards are more than a tenth of the catalogue's (and more than ten,
    /// `diff::too_many`: a short list after weeks without one), the list `list` is refused instead
    /// and nothing is removed ([`Job::refuse`]).
    async fn drop_off_list(
        &self,
        job: &mut Job<'_>,
        day: chrono::NaiveDate,
        list: &str,
    ) -> Result<u64> {
        let pool = job.pool().clone();
        let limit = day - chrono::Duration::days(OFF_LIST + LISTED_REFRESH);
        let gone: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM catalogue_card c JOIN catalogue_site s ON s.host=c.source_id WHERE c.catalogue='tranco' AND s.listed_on < $1",
        )
        .bind(limit)
        .fetch_one(&pool)
        .await?;
        let held = job.state().cards.max(0) as u64;
        if super::diff::too_many(gone.max(0) as u64, held) {
            let note = format!("list {list}: {gone} of {held} cards' sites off the list");
            return Err(job.refuse(json!(list), "refused", note));
        }
        let mut removed = 0;
        loop {
            let hosts: Vec<String> = sqlx::query_scalar(
                "SELECT host FROM catalogue_site WHERE listed_on < $1 LIMIT 1000",
            )
            .bind(limit)
            .fetch_all(&pool)
            .await?;
            if hosts.is_empty() {
                break;
            }
            removed += job.remove(&hosts, json!({"phase": "off_list"})).await?;
            sqlx::query("DELETE FROM catalogue_site WHERE host=ANY($1)")
                .bind(&hosts)
                .execute(&pool)
                .await?;
        }
        Ok(removed)
    }
    /// Read due homepages until none is due, the budget is spent, or a large import starts;
    /// what is next ([`Next`]), and the catalogue reads wait for. [`READS`] reads stay in flight:
    /// sites are claimed [`CLAIM`] at a time as the reads need more, and written as they end.
    async fn read_due(&self, job: &mut Job<'_>, list: &str) -> Result<(Next, Option<String>)> {
        let started = std::time::Instant::now();
        let pool = job.pool().clone();
        let config = job.config().clone();
        let limit = self.reads;
        // Why the claims stopped.
        let stopped: Arc<std::sync::Mutex<(Next, Option<String>)>> =
            Arc::new(std::sync::Mutex::new((Next::Nothing, None)));
        let stop = stopped.clone();
        let claims = futures_util::stream::unfold(Some(0u64), move |claimed| {
            let (pool, config, stop) = (pool.clone(), config.clone(), stop.clone());
            async move {
                let claimed = claimed?;
                let halt = |next: Next, waits: Option<String>| {
                    *stop.lock().expect("the stop") = (next, waits);
                    None
                };
                if started.elapsed() > READ_BUDGET || limit.is_some_and(|n| claimed >= n) {
                    return halt(Next::Reads, None);
                }
                match reads_wait(&config, &pool).await {
                    Err(error) => return Some((Err(error), None)),
                    Ok(Some(waits)) => return halt(Next::Large, Some(waits)),
                    Ok(None) => {}
                }
                let claim = limit.map_or(CLAIM, |n| CLAIM.min((n - claimed) as i64));
                // A claim holds its sites for an hour, so another daemon (or a cut-off run)
                // passes them over meanwhile.
                let sites: Result<Vec<(String, i32)>, sqlx::Error> = sqlx::query_as(
                    "UPDATE catalogue_site s SET due_at=now()+interval '1 hour' FROM (SELECT host FROM catalogue_site WHERE due_at<=now() ORDER BY due_at,rank LIMIT $1 FOR UPDATE SKIP LOCKED) d WHERE s.host=d.host RETURNING s.host,s.rank",
                )
                .bind(claim)
                .fetch_all(&pool)
                .await;
                match sites {
                    Err(error) => Some((Err(error.into()), None)),
                    Ok(sites) if sites.is_empty() => halt(Next::Nothing, None),
                    Ok(sites) => {
                        let next = claimed + sites.len() as u64;
                        Some((Ok(sites), Some(next)))
                    }
                }
            }
        });
        let mut reads = Box::pin(
            claims
                .flat_map(|claim| {
                    futures_util::stream::iter(match claim {
                        Ok(sites) => sites.into_iter().map(Ok).collect::<Vec<_>>(),
                        Err(error) => vec![Err(error)],
                    })
                })
                .map(|site| async move {
                    let (host, rank) = site?;
                    let read = homepage(&self.egress, &host, &self.robots).await;
                    Ok::<_, anyhow::Error>((host, rank, read))
                })
                .buffer_unordered(READS)
                .ready_chunks(CLAIM as usize),
        );
        while let Some(chunk) = reads.next().await {
            let chunk: Vec<(String, i32, Homepage)> = chunk.into_iter().collect::<Result<_>>()?;
            self.record(job, chunk, list).await?;
        }
        let stopped = stopped.lock().expect("the stop").clone();
        Ok(stopped)
    }
    /// Keep what a round of reads found: the cards, the sites' outcomes and when each is due. A
    /// round whose removals take the run's past a tenth of the catalogue's cards (and past ten,
    /// `diff::too_many`: the egress or the network broken, every read failing) refuses the list
    /// `list` instead of removing them ([`Job::refuse`]): the reads wait for the next list.
    async fn record(
        &self,
        job: &mut Job<'_>,
        reads: Vec<(String, i32, Homepage)>,
        list: &str,
    ) -> Result<()> {
        let pool = job.pool().clone();
        let mut cards = Vec::new();
        let mut gone = Vec::new();
        let mut reads: Vec<(String, i32, Homepage)> = reads
            .into_iter()
            .map(|(host, rank, outcome)| {
                // A page that makes no card (no name, or it ended on another site).
                let outcome = match outcome {
                    Homepage::Page {
                        name,
                        description,
                        at,
                    } => match card(&host, rank, &name, description.as_deref(), &at) {
                        Some(c) => {
                            cards.push(c);
                            Homepage::Page {
                                name,
                                description,
                                at,
                            }
                        }
                        None => Homepage::Other,
                    },
                    other => other,
                };
                (host, rank, outcome)
            })
            .collect();
        for (_, _, o) in &reads {
            crate::telemetry::homepage_read(o.outcome());
        }
        // Not given its turn: no read of the site at all; due again in an hour, or when the
        // egress's wait for it ends.
        let (deferred, waits): (Vec<String>, Vec<f64>) = reads
            .iter()
            .filter_map(|(h, _, o)| match o {
                Homepage::Deferred(_) => Some((h.clone(), o.wait() as f64)),
                _ => None,
            })
            .unzip();
        reads.retain(|(_, _, o)| !matches!(o, Homepage::Deferred(_)));
        // A wall in front of a site that has a card may be a challenge for a while: it counts as
        // a transient failure (the card stays until three in a row), not as a no.
        let walled: Vec<&str> = reads
            .iter()
            .filter(|(_, _, o)| *o == Homepage::Wall)
            .map(|(h, _, _)| h.as_str())
            .collect();
        let carded: std::collections::HashSet<String> = if walled.is_empty() {
            Default::default()
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT source_id FROM catalogue_card WHERE catalogue='tranco' AND source_id=ANY($1)",
            )
            .bind(&walled)
            .fetch_all(&pool)
            .await?
            .into_iter()
            .collect()
        };
        let transient = |host: &str, outcome: &Homepage| {
            outcome.transient() || (*outcome == Homepage::Wall && carded.contains(host))
        };
        for (host, _, outcome) in &reads {
            if !matches!(outcome, Homepage::Page { .. }) && !transient(host, outcome) {
                gone.push(host.clone());
            }
        }
        job.write(&cards, json!({"phase": "reading"})).await?;
        // A transient failure keeps its card until it failed three times in a row; it is tried
        // again the next day, or after the site's `Retry-After`.
        let hosts: Vec<&str> = reads.iter().map(|(h, _, _)| h.as_str()).collect();
        let outcomes: Vec<&str> = reads.iter().map(|(_, _, o)| o.outcome()).collect();
        let transient: Vec<bool> = reads.iter().map(|(h, _, o)| transient(h, o)).collect();
        let after: Vec<f64> = reads.iter().map(|(_, _, o)| o.wait() as f64).collect();
        let failed: Vec<Option<String>> = sqlx::query_scalar(
            "UPDATE catalogue_site s SET read_at=now(),outcome=x.o,failures=CASE WHEN x.t THEN s.failures+1 ELSE 0 END,\
             due_at=now()+greatest(make_interval(secs=>x.w),CASE WHEN x.t AND s.failures+1<$4 THEN interval '1 day' ELSE make_interval(secs=>$5) END) \
             FROM unnest($1::text[],$2::text[],$3::bool[],$6::float8[]) AS x(h,o,t,w) WHERE s.host=x.h RETURNING CASE WHEN x.t AND s.failures>=$4 THEN s.host END",
        )
        .bind(&hosts)
        .bind(&outcomes)
        .bind(&transient)
        .bind(FAILURES)
        .bind(REREAD.as_secs_f64())
        .bind(&after)
        .fetch_all(&pool)
        .await?;
        gone.extend(failed.into_iter().flatten());
        if !gone.is_empty() {
            let carded: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM catalogue_card WHERE catalogue='tranco' AND source_id=ANY($1)",
            )
            .bind(&gone)
            .fetch_one(&pool)
            .await?;
            let report = job.report();
            let held = job.state().cards.max(0) as u64 + report.inserted;
            let removing = report.removed + carded.max(0) as u64;
            if super::diff::too_many(removing, held) {
                let note =
                    format!("list {list}: homepage reads would remove {removing} of {held} cards");
                return Err(job.refuse(json!(list), "refused", note));
            }
            job.remove(&gone, json!({"phase": "reading"})).await?;
        }
        if !deferred.is_empty() {
            sqlx::query(
                "UPDATE catalogue_site s SET due_at=now()+greatest(interval '1 hour',make_interval(secs=>x.w)) \
                 FROM unnest($1::text[],$2::float8[]) AS x(h,w) WHERE s.host=x.h",
            )
            .bind(&deferred)
            .bind(&waits)
            .execute(&pool)
            .await?;
        }
        Ok(())
    }
    /// The run: a newer list, then the sites off it, then the reads. While reads wait for a large
    /// import ([`reads_wait`]), the list is looked for at most once a day. An update whose list
    /// can't be looked up (tranco-list.eu down) reads the homepages due on the list it has.
    async fn run(&self, job: &mut Job<'_>) -> Result<()> {
        let mut position = job.position().cloned().unwrap_or_else(|| json!({}));
        let checked = position
            .get("checked")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&chrono::Utc));
        let fresh = checked.is_some_and(|t| chrono::Utc::now() - t < chrono::Duration::days(1));
        // A refusal stands until the next list: within the day the list is not looked up again,
        // so nothing is read.
        if fresh && job.run() == Run::Update && job.refused().is_some() {
            return Err(job.still_refused("the list (its removals)"));
        }
        if job.run() == Run::Update && fresh {
            if let Some(waits) = reads_wait(job.config(), job.pool()).await? {
                crate::telemetry::homepage_reads_waiting(true);
                position["next"] = json!(Next::Large.name());
                position["waits_for"] = json!(waits);
                job.set_position(position);
                return Ok(());
            }
        }
        // The day's list is looked up once a day: a run that wakes for the sites due in between
        // reads them from the list it has.
        let looked_up = if job.run() == Run::Update && fresh {
            None
        } else {
            Some(self.latest().await)
        };
        let latest = match looked_up {
            None => None,
            Some(Ok(latest)) => Some(latest),
            Some(Err(error)) if job.run() == Run::Update && position.get("list").is_some() => {
                let shown = super::failure::shown(&error);
                tracing::warn!(catalogue = "tranco", error = %shown, "Tranco's list could not be looked up; the homepages due are read from the list loaded");
                None
            }
            Some(Err(error)) => return Err(error),
        };
        if let Some((version, input, at)) = latest {
            // A list refused before is not loaded again, and no homepage is read on it.
            if job.refused_before(&json!(version)) {
                return Err(job.still_refused(format!("list {version}")));
            }
            job.reading(json!(version));
            let known = position.get("list").and_then(Value::as_str) == Some(version.as_str());
            // A list's day is never ahead of now (a server clock or a file's time could say so,
            // and every site not on it would then count as off the list).
            let now = chrono::Utc::now();
            let at = at.map_or(now, |at| at.min(now));
            let day = at.date_naive();
            position["checked"] = json!(now.to_rfc3339());
            if !known || job.run() == Run::Import {
                self.load(job, &input, day).await?;
                if job.run() == Run::Import {
                    stamp(job).await?;
                }
                self.drop_off_list(job, day, &version).await?;
                // As current as the list's own time (not its day's start: the lag would pass two
                // days near the end of each day's cycle).
                job.set_current_as_of(at);
                position = json!({"list": version, "day": day, "checked": now.to_rfc3339()});
            }
        }
        let on_list = position
            .get("list")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let (next, waits) = self.read_due(job, &on_list).await?;
        crate::telemetry::homepage_reads_waiting(next == Next::Large);
        position["next"] = json!(next.name());
        // When the next site falls due: the plan wakes then ([`plan_after`]).
        let next_due: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT min(due_at) FROM catalogue_site")
                .fetch_one(job.pool())
                .await?;
        match next_due {
            Some(due) => position["next_due"] = json!(due.to_rfc3339()),
            None => {
                if let Some(p) = position.as_object_mut() {
                    p.remove("next_due");
                }
            }
        }
        match waits {
            Some(waits) => position["waits_for"] = json!(waits),
            None => {
                if let Some(p) = position.as_object_mut() {
                    p.remove("waits_for");
                }
            }
        }
        // `catalogue status` shows the reads so far.
        let (sites, read): (i64, i64) =
            sqlx::query_as("SELECT count(*),count(read_at) FROM catalogue_site")
                .fetch_one(job.pool())
                .await?;
        let list = position
            .get("list")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        job.upstream_version(&format!(
            "{list} ({}, {sites} sites, {read} read)",
            position.get("day").and_then(Value::as_str).unwrap_or("")
        ))
        .await?;
        job.set_position(position);
        Ok(())
    }
}
/// What the next run of reads waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Homepages are still due (the run's budget ended it): the next run starts at once.
    Reads,
    /// A large import runs: the next run waits ten minutes.
    Large,
    /// Nothing is due now: the next run is when the next site falls due (and the day's list).
    Nothing,
}
impl Next {
    fn name(self) -> &'static str {
        match self {
            Next::Reads => "reads",
            Next::Large => "large_import",
            Next::Nothing => "list",
        }
    }
}
/// A full import keeps the cards of homepages already read: it stamps them as read by this
/// generation (the sweep at its end removes only cards of sites no longer listed), and counts
/// them as read (`generation_cards`, which the runner's shrink check compares with the cards).
async fn stamp(job: &mut Job<'_>) -> Result<()> {
    let pool = job.pool().clone();
    let generation = job.generation();
    let mut after = String::new();
    loop {
        let last: Option<String> = sqlx::query_scalar(
            "WITH page AS (SELECT c.id,c.source_id FROM catalogue_card c JOIN catalogue_site s ON s.host=c.source_id \
             WHERE c.catalogue='tranco' AND c.source_id>$1 ORDER BY c.source_id LIMIT 10000), \
             stamped AS (UPDATE catalogue_card c SET seen_generation=$2 FROM page WHERE c.id=page.id AND c.seen_generation<$2 RETURNING 1), \
             counted AS (UPDATE catalogue_state SET generation_cards=generation_cards+(SELECT count(*) FROM stamped) WHERE catalogue='tranco') \
             SELECT max(source_id) FROM page",
        )
        .bind(&after)
        .bind(generation)
        .fetch_one(&pool)
        .await?;
        match last {
            Some(last) => after = last,
            None => return Ok(()),
        }
    }
}
/// The large catalogue homepage reads wait for, if any: one that is on, has an importer that
/// starts (`http.contact` set) and has not finished its first full import, or one whose full
/// import runs now (any large catalogue's).
pub async fn reads_wait(config: &Config, pool: &sqlx::PgPool) -> Result<Option<String>> {
    let large: Vec<&str> = Catalogue::ALL
        .into_iter()
        .filter(|c| c.large())
        .map(Catalogue::name)
        .collect();
    let first: Vec<&str> = super::run::WITH_IMPORTER
        .into_iter()
        .filter(|c| c.large() && config.catalogues.enabled(*c) && !config.egress.contact.is_empty())
        .map(Catalogue::name)
        .collect();
    Ok(sqlx::query_scalar(
        "SELECT coalesce((SELECT catalogue FROM catalogue_state WHERE catalogue=ANY($1) AND run='import' AND phase<>'disabled' ORDER BY catalogue LIMIT 1), \
         (SELECT c.name FROM unnest($2::text[]) AS c(name) LEFT JOIN catalogue_state s ON s.catalogue=c.name WHERE s.last_import_at IS NULL ORDER BY c.name LIMIT 1))",
    )
    .bind(&large)
    .bind(&first)
    .fetch_one(pool)
    .await?)
}
/// When the next run is due, `last` the end of the last run, `next` what it left
/// ([`Next::name`]), `due` when the next site falls due (the earliest `due_at` it left) and
/// `checked` when the list was last looked up: at once while homepages are due, ten minutes after
/// one that stopped for a large import; else when the next site falls due, and at the latest a
/// day after the list was looked up (the day's list: the cards' lag stays within a day of it),
/// never sooner than [`WAKE_AFTER`] after the last run. A wait is at most an hour (the plan is
/// asked again).
pub fn plan_after(
    last: chrono::DateTime<chrono::Utc>,
    next: Option<&str>,
    due: Option<chrono::DateTime<chrono::Utc>>,
    checked: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Plan {
    let wake = match next {
        Some("reads") => last,
        Some("large_import") => last + chrono::Duration::minutes(10),
        _ => {
            let soonest = last + WAKE_AFTER;
            let day = checked.unwrap_or(last) + chrono::Duration::days(1);
            let latest = day.max(soonest);
            due.map_or(latest, |due| due.max(soonest).min(latest))
        }
    };
    let wait = (wake - now).to_std().unwrap_or_default();
    if wait.is_zero() {
        Plan::Run(Run::Update)
    } else {
        Plan::Wait(wait.min(Duration::from_secs(3600)))
    }
}
/// The robots.txt rules read on the way to a page that was then deferred (its robots.txt took the
/// host's turn), kept [`ROBOTS_TTL`] across runs, so its next try reads the page without asking
/// the file again. At most [`ROBOTS_CACHED`] origins (the oldest goes first), each with rules of
/// at most [`ROBOTS_CACHED_BYTES`] counted with each rule's own memory: about 33 MiB at worst.
#[derive(Default)]
pub struct RobotsCache(std::sync::Mutex<RobotsEntries>);
/// The cached rules by origin, each with when it was read and its place in the order of reads.
#[derive(Default)]
struct RobotsEntries {
    next: u64,
    by_origin: std::collections::HashMap<String, (std::time::Instant, u64, Robots)>,
}
/// How long robots.txt rules read are kept.
const ROBOTS_TTL: Duration = Duration::from_secs(86400);
const ROBOTS_CACHED: usize = 2_000;
/// What one origin's rules may hold to be kept, as [`Robots::weight`] counts it (each rule's
/// pattern and its own memory).
const ROBOTS_CACHED_BYTES: usize = 16 << 10;
impl RobotsCache {
    fn get(&self, origin: &str) -> Option<Robots> {
        let cache = self.0.lock().expect("the robots cache");
        cache
            .by_origin
            .get(origin)
            .filter(|(at, _, _)| at.elapsed() < ROBOTS_TTL)
            .map(|(_, _, rules)| rules.clone())
    }
    fn put(&self, origin: &str, rules: &Robots) {
        if rules.weight() > ROBOTS_CACHED_BYTES {
            return;
        }
        let mut cache = self.0.lock().expect("the robots cache");
        let entries = &mut cache.by_origin;
        if entries.len() >= ROBOTS_CACHED && !entries.contains_key(origin) {
            entries.retain(|_, (at, _, _)| at.elapsed() < ROBOTS_TTL);
            // Still full: the oldest goes (the first read of those kept).
            if entries.len() >= ROBOTS_CACHED {
                if let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, (_, n, _))| *n)
                    .map(|(o, _)| o.clone())
                {
                    entries.remove(&oldest);
                }
            }
        }
        let n = cache.next;
        cache.next += 1;
        cache.by_origin.insert(
            origin.to_owned(),
            (std::time::Instant::now(), n, rules.clone()),
        );
    }
}

#[async_trait]
impl Importer for Tranco {
    fn catalogue(&self) -> Catalogue {
        Catalogue::Tranco
    }
    /// A list file or URL given (`--from`).
    fn source(&self) -> Option<String> {
        match &self.list {
            ListSource::Official => None,
            ListSource::Given(input) => super::diff::source_of(input, LATEST_ID),
        }
    }
    /// None: the list is read into memory, and nothing is written under `catalogues.dir` (the
    /// list and the cards' 1.1 GB of the database are sized apart).
    fn disk_need(&self, _run: Run, _state: &State) -> u64 {
        0
    }
    /// A full import first; then an update ([`plan_after`]): at once while the last run left
    /// homepages due, ten minutes after one that stopped for a large import, else when the next
    /// site falls due (ten minutes to a day after the last run; the day's list once a day).
    async fn plan(&self, state: &State) -> Result<Plan> {
        let Some(last) = state.current_at() else {
            return Ok(Plan::Run(Run::Import));
        };
        let position = state.position.as_ref();
        let next = position.and_then(|p| p.get("next")).and_then(Value::as_str);
        let at = |key: &str| {
            position
                .and_then(|p| p.get(key))
                .and_then(Value::as_str)
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
        };
        Ok(plan_after(
            last,
            next,
            at("next_due"),
            at("checked"),
            chrono::Utc::now(),
        ))
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        self.run(job).await
    }
    /// An update; a full import instead when one was asked for (`--request`).
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        if job.state().import_requested.is_some() {
            return Ok(UpdateEnd::NeedsImport(super::musicbrainz::REQUESTED));
        }
        self.run(job).await?;
        Ok(UpdateEnd::Done)
    }
}

/// The importer `pinakesd` runs, reading homepages through `egress`; none without the
/// operator's contact (`http.contact`).
pub fn importer(config: &Config, egress: &Egress) -> Option<Arc<dyn Importer>> {
    if config.egress.contact.is_empty() {
        if !config.catalogues.enabled(Catalogue::Tranco) {
            return None;
        }
        tracing::warn!(
            catalogue = "tranco",
            "the Tranco importer starts only with http.contact set (the list and the homepages are asked for with the operator's contact)"
        );
        return None;
    }
    match Tranco::new(ListSource::Official, egress.clone(), &config.egress.contact) {
        Ok(importer) => Some(Arc::new(importer)),
        Err(error) => {
            tracing::warn!(catalogue = "tranco", error = %crate::resolver::error_class(&error), "the Tranco importer did not start");
            None
        }
    }
}

/// A site as the list names it: a lower-case host name of letters, digits and hyphens (IDNs in
/// their `xn--` form), at least two labels, at most 253 bytes, no label over 63 or starting or
/// ending with a hyphen.
pub fn site(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').count() >= 2
        // A name, not an address: the last label holds a letter.
        && host.rsplit('.').next().is_some_and(|tld| tld.bytes().any(|b| b.is_ascii_lowercase()))
        && host.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        // And a host a URL keeps as it is (an `xn--` label that is no valid punycode is not).
        && url::Url::parse(&format!("https://{host}/")).is_ok_and(|u| u.host_str() == Some(host))
}
/// The list: `rank,site` lines (a line that is not one is passed over), at most [`SITES_CAP`].
pub fn list<R: BufRead>(body: R, at: &str) -> Result<Vec<(String, i32)>> {
    let mut lines = super::bulk::Lines::new(body, 1024);
    let mut line = Vec::new();
    let mut out = Vec::new();
    // The sites seen, as 64-bit hashes (the list holds each host once more).
    let mut seen: std::collections::HashSet<u64> =
        std::collections::HashSet::with_capacity(1 << 20);
    while lines
        .next_line(&mut line)
        .map_err(|_| Failure::new("download").at(at.to_owned()))?
    {
        let text = String::from_utf8_lossy(&line);
        let Some((rank, host)) = text.trim().split_once(',') else {
            continue;
        };
        let Ok(rank) = rank.trim().parse::<i32>() else {
            continue;
        };
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        // A rank past the list's own size is no list's (and would overflow the rank's sums).
        if rank <= 0 || rank as usize > SITES_CAP || !site(&host) || !seen.insert(host_hash(&host))
        {
            continue;
        }
        if out.len() >= SITES_CAP {
            anyhow::bail!(Failure::new("size").at(at.to_owned()));
        }
        out.push((host, rank));
    }
    Ok(out)
}
fn host_hash(host: &str) -> u64 {
    u64::from_le_bytes(
        blake3::hash(host.as_bytes()).as_bytes()[..8]
            .try_into()
            .expect("eight bytes"),
    )
}
/// A site's fame from its rank: 1 - ln(rank) / ln(1,000,001), within 0-1.
pub fn rank_fame(rank: i32) -> f32 {
    (1.0 - (rank.max(1) as f64).ln() / (1_000_001_f64).ln()).clamp(0.0, 1.0) as f32
}

/// What a homepage read found.
#[derive(Clone, Debug, PartialEq)]
pub enum Homepage {
    /// An HTML page that is no wall: its name and description, and the address it ended at.
    Page {
        name: Option<String>,
        description: Option<String>,
        at: String,
    },
    /// A `robots.txt` on the way does not let `pinakes` read the page.
    Robots,
    /// A bot wall or a challenge.
    Wall,
    /// Another type than HTML, or a page too large.
    NotHtml,
    /// A status other than 2xx (a 429 or a 5xx is transient).
    Status(u16),
    /// The site is busy (a 429, which the egress answers as `RetryLater`; a 5xx or a 408):
    /// transient, tried again after the next day or its `Retry-After` (seconds, at most a week),
    /// whichever is later.
    Busy(u64),
    /// No answer (DNS, refused, timed out, TLS).
    Unreached,
    /// The egress gave the read no turn in time (the daemon's other fetches came first, or the
    /// site's pause runs): not a read of the site; it is due again in an hour, or after the
    /// egress's wait (seconds) when longer.
    Deferred(u64),
    /// A redirect off the site, too many redirects, or a page without a name: no card.
    Other,
}
impl Homepage {
    pub fn outcome(&self) -> &'static str {
        match self {
            Homepage::Page { .. } => "ok",
            Homepage::Robots => "robots",
            Homepage::Wall => "wall",
            Homepage::NotHtml => "not_html",
            Homepage::Status(s) if *s == 429 || *s >= 500 => "busy",
            Homepage::Busy(_) => "busy",
            Homepage::Status(_) => "status",
            Homepage::Unreached => "unreached",
            Homepage::Deferred(_) => "deferred",
            Homepage::Other => "other",
        }
    }
    /// A failure that may pass: tried again the next day (three in a row remove the card).
    pub fn transient(&self) -> bool {
        matches!(self, Homepage::Unreached | Homepage::Busy(_))
            || matches!(self, Homepage::Status(s) if *s == 429 || *s >= 500)
    }
    /// The least time before the site is read again (a `Retry-After`, an egress wait), seconds.
    pub fn wait(&self) -> u64 {
        match self {
            Homepage::Busy(s) | Homepage::Deferred(s) => (*s).min(LONGEST_WAIT),
            _ => 0,
        }
    }
}
/// The most redirects a homepage read follows (each hop on the site, each host's robots.txt
/// asked).
const HOPS: usize = 5;
/// How an egress error reads: a 429 or a server error the egress answers as `RetryLater` is
/// [`Homepage::Busy`] with its `Retry-After`; any other `RetryLater` (no turn in time, the site's
/// pause) [`Homepage::Deferred`]; a body over its cap `over_cap`; no answer
/// [`Homepage::Unreached`]; a status that status.
fn failed(error: &anyhow::Error, over_cap: Homepage) -> Homepage {
    if error.is::<crate::resolver::interstitial::Blocked>() {
        return Homepage::Wall;
    }
    if crate::resolver::egress::over_cap(error) {
        return over_cap;
    }
    if let Some(retry) = error.downcast_ref::<crate::resolver::RetryLater>() {
        return match retry.class {
            "http_4xx" | "http_5xx" => Homepage::Busy(retry.seconds),
            _ => Homepage::Deferred(retry.seconds),
        };
    }
    if crate::resolver::unreached(error) {
        return Homepage::Unreached;
    }
    match crate::resolver::status_of(error) {
        Some(status) => Homepage::Status(status),
        None => Homepage::Unreached,
    }
}
/// The `robots.txt` rules for `pinakes` of `origin` (`https://host`); `Err` with what the read
/// is when the file can't say (a 429 or 5xx, no answer, no turn).
async fn robots_rules(egress: &Egress, origin: &str) -> Result<Robots, Homepage> {
    use crate::resolver::egress::FetchOptions;
    let options = FetchOptions {
        accept: Some("text/plain".into()),
        extra_types: &["text/plain"],
        body_cap: Some(robots::CAP),
        slot_wait: Some(SLOT_WAIT),
        ..Default::default()
    };
    match egress
        .fetch_with(&format!("{origin}/robots.txt"), &options)
        .await
    {
        // Parsed off the async workers (a file may hold many rules).
        Ok(f) if f.status == 200 => {
            crate::resolver::html::blocking(move || Robots::parse(&f.body, robots::AGENT))
                .await
                .map_err(|_| Homepage::Other)
        }
        // A 429, a timeout or a server error says nothing yet: tried again, not before its
        // `Retry-After` (RFC 9309 reads a lasting server error as "allow nothing").
        Ok(f) if crate::resolver::busy_status(f.status) => Err(Homepage::Busy(f.retry_after_secs)),
        // A missing file (a 4xx) allows all.
        Ok(f) if (400..500).contains(&f.status) => Ok(Robots::allow_all()),
        Ok(_) => Ok(Robots::disallow_all()),
        // A file over the cap, or a wall in front of it, counts as a no.
        Err(error) => match failed(&error, Homepage::Robots) {
            Homepage::Robots | Homepage::Wall => Ok(Robots::disallow_all()),
            other => Err(other),
        },
    }
}
/// Read `https://{host}/` through the egress, following redirects by hand (at most [`HOPS`],
/// each on the site): each host's `robots.txt` is asked before its page is read, and the page
/// must be HTML with a 2xx status and no wall. A read waits at most [`SLOT_WAIT`] for its turn.
pub async fn homepage(egress: &Egress, host: &str, cache: &RobotsCache) -> Homepage {
    let mut fresh: Vec<(String, Robots)> = Vec::new();
    let read = read_homepage(egress, host, cache, &mut fresh).await;
    // Deferred after a robots.txt it read: kept for the page's next try.
    if matches!(read, Homepage::Deferred(_)) {
        for (origin, rules) in &fresh {
            cache.put(origin, rules);
        }
    }
    read
}
/// [`homepage`]; `fresh` gets the robots.txt rules it read (not cached ones).
async fn read_homepage(
    egress: &Egress,
    host: &str,
    cache: &RobotsCache,
    fresh: &mut Vec<(String, Robots)>,
) -> Homepage {
    use crate::resolver::egress::FetchOptions;
    let site = registrable(host);
    let mut url = match url::Url::parse(&format!("https://{host}/")) {
        Ok(url) => url,
        Err(_) => return Homepage::Other,
    };
    let page = FetchOptions {
        slot_wait: Some(SLOT_WAIT),
        keep_redirect: true,
        ..Default::default()
    };
    let mut asked: Vec<(String, Robots)> = Vec::new();
    for _ in 0..=HOPS {
        // The robots.txt of each origin on the way (its port too): kept from a deferred try
        // ([`RobotsCache`]), else read.
        let origin = url.origin().ascii_serialization();
        if !asked.iter().any(|(o, _)| *o == origin) {
            let rules = match cache.get(&origin) {
                Some(rules) => rules,
                None => match robots_rules(egress, &origin).await {
                    Ok(rules) => {
                        fresh.push((origin.clone(), rules.clone()));
                        rules
                    }
                    Err(read) => return read,
                },
            };
            asked.push((origin.clone(), rules));
        }
        let path = path_and_query(&url);
        if path.len() > robots::PATH_CAP {
            return Homepage::Other;
        }
        let Some(rules) = asked
            .iter()
            .find(|(o, _)| *o == origin)
            .map(|(_, r)| r.clone())
        else {
            return Homepage::Other;
        };
        // Matched off the async workers too: rules times path bytes at worst.
        let allowed = crate::resolver::html::blocking(move || rules.allows(&path))
            .await
            .unwrap_or(false);
        if !allowed {
            return Homepage::Robots;
        }
        let fetched = match egress.fetch_with(url.as_str(), &page).await {
            Ok(fetched) => fetched,
            Err(error) => return failed(&error, Homepage::NotHtml),
        };
        if let Some(next) = fetched
            .location
            .as_deref()
            .filter(|_| (300..400).contains(&fetched.status) || fetched.status == 200)
            // A 200 that names its own address is the page (a stray `Location`), no redirect.
            .filter(|next| fetched.status != 200 || url.join(next).is_ok_and(|n| n != url))
        {
            let Ok(next) = url.join(next) else {
                return Homepage::Other;
            };
            // Only on the site itself, over the web.
            let on_site = matches!(next.scheme(), "http" | "https")
                && next
                    .host_str()
                    .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
                    .and_then(|h| registrable(&h))
                    == site;
            if !on_site || site.is_none() {
                return Homepage::Other;
            }
            url = next;
            continue;
        }
        // A 5xx, a timeout (or a 429 the egress passed on): transient, not before its
        // `Retry-After`.
        if crate::resolver::busy_status(fetched.status) {
            return Homepage::Busy(fetched.retry_after_secs);
        }
        if !(200..300).contains(&fetched.status) {
            return Homepage::Status(fetched.status);
        }
        let html = fetched.content_type.starts_with("text/html")
            || fetched.content_type.starts_with("application/xhtml+xml");
        if !html {
            return Homepage::NotHtml;
        }
        let at = fetched.url.to_string();
        let page_uri = format!("https://{host}/");
        let extracted = crate::resolver::html::blocking(move || {
            crate::resolver::web::extract(&page_uri, &fetched, 0)
        })
        .await;
        let Ok(Ok(extracted)) = extracted else {
            return Homepage::NotHtml;
        };
        if extracted.doc.status == crate::model::Status::Unreachable {
            return Homepage::Wall;
        }
        let name = extracted.doc.name.clone().or(extracted.title);
        let description = extracted.doc.description.clone().or(extracted.description);
        return Homepage::Page {
            name,
            description,
            at,
        };
    }
    Homepage::Other
}
/// A URL's path and query, as robots.txt rules match them.
fn path_and_query(url: &url::Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    }
}
/// The registrable domain of a host (`www.example.co.uk` is `example.co.uk`).
fn registrable(host: &str) -> Option<String> {
    psl::domain_str(host).map(str::to_owned)
}
/// The card of a homepage read at `at`; none when the page ended on another site or has no name.
pub fn card(
    host: &str,
    rank: i32,
    name: &Option<String>,
    description: Option<&str>,
    at: &str,
) -> Option<Card> {
    let landed = url::Url::parse(at).ok()?;
    let landed = landed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if registrable(&landed)? != registrable(host)? {
        return None;
    }
    let name = name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())?
        .to_owned();
    let uri = format!("https://{host}/");
    let mut details = serde_json::Map::new();
    details.insert("rank".into(), json!(rank));
    Some(Card {
        source_id: host.to_owned(),
        keys: super::join::link_keys(&uri)
            .into_iter()
            .map(|(k, _)| k)
            .collect(),
        uri,
        r#type: "WebSite".into(),
        name,
        description: description.map(str::to_owned),
        fame: rank_fame(rank),
        details,
        ..Card::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn the_list_reads_its_sites() {
        let text = b"1,google.com\n2,Cloudflare.com\n3,bad host\n4,localhost\nx,y.com\n5,google.com\n6,xn--80ak6aa92e.com.\n7,-x.com\n8,203.0.113.7\n9,xn--0.com\n";
        let sites = list(&text[..], "t").unwrap();
        assert_eq!(
            sites,
            [
                ("google.com".to_owned(), 1),
                ("cloudflare.com".to_owned(), 2),
                ("xn--80ak6aa92e.com".to_owned(), 6)
            ]
        );
        // A list of no site is read as one (a full import of it is refused, `empty`).
        assert!(list(&b"nothing\n"[..], "t").unwrap().is_empty());
        // A rank past the cap is no list's.
        let sites = list(
            &b"2000001,big.example\n2147483647,bigger.example\n2,ok.example\n"[..],
            "t",
        )
        .unwrap();
        assert_eq!(sites, [("ok.example".to_owned(), 2)]);
    }

    /// The next run: at once while homepages are due, ten minutes after a run that stopped for a
    /// large import, else when the next site falls due (and a day after the list was looked up at
    /// the latest); a wait of at most an hour.
    #[test]
    fn runs_are_planned_by_what_the_last_left() {
        let now = chrono::Utc::now();
        let ago = |m: i64| now - chrono::Duration::minutes(m);
        let at = |m: i64| now + chrono::Duration::minutes(m);
        let wait = |m: u64| Plan::Wait(Duration::from_secs(m * 60));
        assert_eq!(
            plan_after(ago(0), Some("reads"), None, None, now),
            Plan::Run(Run::Update)
        );
        assert_eq!(
            plan_after(ago(4), Some("large_import"), None, None, now),
            wait(6)
        );
        assert_eq!(
            plan_after(ago(10), Some("large_import"), None, None, now),
            Plan::Run(Run::Update)
        );
        // Nothing due: the day's list, a day after the last run (a wait of an hour at most).
        assert_eq!(plan_after(ago(60), Some("list"), None, None, now), wait(60));
        assert_eq!(
            plan_after(ago(23 * 60 + 30), None, None, None, now),
            wait(30)
        );
        assert_eq!(
            plan_after(ago(24 * 60), Some("list"), None, None, now),
            Plan::Run(Run::Update)
        );
        // A site falls due between runs: the run wakes then, not a day later...
        assert_eq!(
            plan_after(ago(120), Some("list"), Some(at(20)), None, now),
            wait(20)
        );
        assert_eq!(
            plan_after(ago(120), Some("list"), Some(ago(5)), None, now),
            Plan::Run(Run::Update)
        );
        // ...but never sooner than ten minutes after the last run, nor later than a day.
        assert_eq!(
            plan_after(ago(4), Some("list"), Some(ago(30)), None, now),
            wait(6)
        );
        assert_eq!(
            plan_after(ago(23 * 60 + 30), Some("list"), Some(at(5 * 60)), None, now),
            wait(30)
        );
        // The day's list a day after it was last looked up, though a run came since.
        assert_eq!(
            plan_after(
                ago(30),
                Some("list"),
                Some(at(5 * 60)),
                Some(ago(23 * 60 + 40)),
                now
            ),
            wait(20)
        );
        assert_eq!(
            plan_after(ago(30), Some("list"), None, Some(ago(25 * 60)), now),
            Plan::Run(Run::Update)
        );
        // Never sooner than ten minutes after the last run, whatever the lookup's time.
        assert_eq!(
            plan_after(ago(5), Some("list"), None, Some(ago(30 * 60)), now),
            wait(5)
        );
    }

    /// The robots cache keeps at most its count of origins, the oldest going first, and no
    /// origin whose rules weigh more than its cap.
    #[test]
    fn the_robots_cache_drops_the_oldest_and_the_heavy() {
        let cache = RobotsCache::default();
        let origin = |n: usize| format!("https://h{n}.example");
        for n in 0..ROBOTS_CACHED {
            cache.put(&origin(n), &Robots::allow_all());
        }
        assert!(cache.get(&origin(0)).is_some());
        cache.put("https://new.example", &Robots::allow_all());
        assert!(cache.get(&origin(0)).is_none(), "the oldest goes");
        assert!(cache.get(&origin(1)).is_some() && cache.get("https://new.example").is_some());
        // Putting an origin again is no new entry: nothing goes, not even the oldest left.
        cache.put(&origin(5), &Robots::allow_all());
        assert!(cache.get(&origin(1)).is_some());
        let heavy = format!(
            "User-agent: *\n{}",
            "Disallow: /some/long/path/to/hide\n".repeat(400)
        );
        let heavy = Robots::parse(heavy.as_bytes(), robots::AGENT);
        assert!(heavy.weight() > ROBOTS_CACHED_BYTES);
        cache.put("https://heavy.example", &heavy);
        assert!(cache.get("https://heavy.example").is_none());
    }

    #[test]
    fn a_429_is_busy_with_its_wait_and_no_turn_is_deferred() {
        use crate::resolver::RetryLater;
        let retry = |class, seconds| anyhow::Error::new(RetryLater { class, seconds });
        let busy = failed(&retry("http_4xx", 7200), Homepage::NotHtml);
        assert_eq!(busy, Homepage::Busy(7200));
        assert!(busy.transient());
        assert_eq!((busy.outcome(), busy.wait()), ("busy", 7200));
        let deferred = failed(&retry("rate_limited", 30), Homepage::NotHtml);
        assert_eq!(deferred, Homepage::Deferred(30));
        assert!(!deferred.transient());
        assert_eq!(deferred.outcome(), "deferred");
        // A wait past 90 days is held to 90 days.
        assert_eq!(Homepage::Busy(u64::MAX).wait(), LONGEST_WAIT);
        assert!(!Homepage::Status(404).transient());
        assert!(Homepage::Status(503).transient());
    }

    #[test]
    fn a_homepage_card_is_the_site_s_own() {
        let made = card(
            "example.org",
            1,
            &Some(" Example ".into()),
            Some("A site"),
            "https://www.example.org/en/",
        )
        .unwrap();
        let row = made.checked(Catalogue::Tranco).unwrap();
        assert_eq!(row.uri, "https://example.org/");
        assert!(
            row.keys.contains(&"homepage:example.org".to_owned()),
            "{:?}",
            row.keys
        );
        assert_eq!(row.fame, 1.0);
        assert_eq!(row.name, "Example");
        // Landing on another site, or no name: no card.
        assert!(card(
            "fbcdn.net",
            5,
            &Some("Facebook".into()),
            None,
            "https://www.facebook.com/"
        )
        .is_none());
        assert!(card("example.org", 5, &None, None, "https://example.org/").is_none());
        assert!(rank_fame(1_000_000) < 0.01 && rank_fame(1000) > 0.4);
    }

    #[test]
    fn outcomes() {
        assert!(
            Homepage::Unreached.transient()
                && Homepage::Status(503).transient()
                && Homepage::Status(429).transient()
        );
        assert!(
            !Homepage::Status(404).transient()
                && !Homepage::Wall.transient()
                && !Homepage::Robots.transient()
        );
        assert_eq!(Homepage::Status(503).outcome(), "busy");
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 3000, ..ProptestConfig::default() })]
        /// Property: a site's card link reads back the site's homepage key, and the `www.` and
        /// `http` spellings of its homepage read the same key (`join::link_keys`).
        #[test]
        fn a_homepage_link_names_its_site(labels in proptest::collection::vec("[a-z0-9]([a-z0-9-]{0,10}[a-z0-9])?", 1..3), tld in "(com|org|net|ch|io|co\\.uk)") {
            let host = format!("{}.{tld}", labels.join("."));
            // A host that is itself a public suffix (`uk.com`) has no site of its own.
            prop_assume!(site(&host) && registrable(&host).is_some());
            let card = card(&host, 10, &Some("N".into()), None, &format!("https://{host}/")).unwrap();
            let row = card.checked(Catalogue::Tranco).unwrap();
            let key = format!("homepage:{}", host.strip_prefix("www.").unwrap_or(&host));
            prop_assert!(row.keys.contains(&key), "{:?}", row.keys);
            for spelling in [format!("https://www.{host}/"), format!("http://{host}/")] {
                let read: Vec<String> = super::super::join::link_keys(&spelling).into_iter().map(|(k, _)| k).collect();
                prop_assert!(read.contains(&key), "{spelling}: {read:?}");
            }
        }
        /// Fuzz: any bytes as a list read without a panic, every site valid.
        #[test]
        fn any_list_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..2000)) {
            if let Ok(sites) = list(&bytes[..], "t") {
                prop_assert!(sites.iter().all(|(h, r)| site(h) && *r > 0));
            }
        }
    }
}
