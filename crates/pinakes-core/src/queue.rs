use crate::{
    config::{Config, QueueConfig},
    model::Status,
    resolver::{
        egress::Egress, geo::GeoResolver, osm::OsmResolver, pubky::PubkyResolver, web::WebResolver,
        Resolver,
    },
};
use anyhow::Result;
use futures_util::FutureExt;
use sqlx::{FromRow, PgPool};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{watch, Mutex, Notify},
    task::JoinSet,
    time::Instant,
};

/// How long one link's read may take. Fixed, not a limit: the resolvers set their own inner
/// deadlines below it (a card's enrichment, a relay's answer), and a job's lease is sized from it.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, FromRow)]
pub struct Job {
    pub id: String,
    pub uri: String,
    pub attempts: i32,
    pub generation: i64,
}
/// One job of `resolver`, for tests: the daemon's workers claim through [`claim_sources`].
pub async fn claim(pool: &PgPool, resolver: &str, cfg: &QueueConfig) -> Result<Option<Job>> {
    Ok(claim_batch(pool, resolver, cfg, 1).await?.pop())
}
pub async fn claim_batch(
    pool: &PgPool,
    resolver: &str,
    cfg: &QueueConfig,
    size: usize,
) -> Result<Vec<Job>> {
    claim_sources(pool, &[resolver], cfg, size).await
}
pub async fn claim_sources(
    pool: &PgPool,
    sources: &[&str],
    cfg: &QueueConfig,
    size: usize,
) -> Result<Vec<Job>> {
    let size = size.clamp(1, 50);
    Ok(sqlx::query_as(r#"UPDATE resource SET lease_until=now()+$2*interval '1 second', generation=generation+1
        WHERE id IN (SELECT id FROM resource WHERE resolver=ANY($1) AND NOT dead_letter AND NOT final AND next_attempt_at<=now()
          AND (lease_until IS NULL OR lease_until<now())
          AND (NOT EXISTS(SELECT 1 FROM resource_derivation d WHERE d.child_id=resource.id)
               OR EXISTS(SELECT 1 FROM resource_derivation d WHERE d.child_id=resource.id AND (d.expires_at IS NULL OR d.expires_at>now()))
               OR EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=resource.id))
          ORDER BY priority_at DESC NULLS LAST,next_attempt_at,id FOR UPDATE SKIP LOCKED LIMIT $3)
        RETURNING id,uri,attempts,generation"#).bind(sources).bind(cfg.lease_secs.max(15 * size as u64 + 10) as f64).bind(size as i64).fetch_all(pool).await?)
}

/// `reextract`'s arguments: `all`, `also-on`, or route and card-kind names.
pub fn check_routes(routes: &[String]) -> Result<()> {
    anyhow::ensure!(
        !routes.is_empty(),
        "reextract needs a route, `all` or `also-on`"
    );
    let known: Vec<&str> = crate::resolver::ROUTES
        .iter()
        .chain(crate::resolver::cards::KINDS.iter())
        .copied()
        .collect();
    for route in routes.iter().filter(|r| *r != "all" && *r != "also-on") {
        anyhow::ensure!(
            known.contains(&route.as_str()),
            "unknown route {route}; routes are {}, all or also-on",
            known.join(", ")
        );
    }
    Ok(())
}
/// Queue these resources' also-on lookups now (a fresh run of every source).
async fn queue_also_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ids: &[String],
) -> Result<()> {
    sqlx::query("INSERT INTO also_on_dirty(resource_id) SELECT unnest($1::text[]) ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now(),attempts=0,last_error=NULL")
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// A row `reextract` reads: its ID, URI and route, and its alias's root and basis.
type ReextractRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);
/// `pinakesd reextract`: re-run `classify` on the selected rows (every row for `all`, else the
/// rows routed to or classified as one of `routes`) and re-queue them now, in batches of 1000.
/// Each loses its HTTP validators and, on the OSM route, its cache entry, so its next fetch
/// re-extracts instead of answering 304 or reading the cache. A row entering or leaving the OSM
/// route swaps its identity envelope and becomes pending, as does an OSM-host URL moving to web
/// (which may hold no facts there); other rows keep their document until the fetch replaces it.
/// Leases are fenced by the generation bump. The re-queued rows' also-on lookups are queued too:
/// a card that comes back unchanged would not run them, and their results may predate a source
/// or a key the upgrade added. `also-on` queues every resource's also-on lookups, without a
/// fetch. A selected alias whose URI a rule now gives another root moves to it, and one no rule
/// names any more answers for itself again and is re-queued (`alias::release`). The daemon must
/// not be running (the caller holds the instance lock).
pub async fn reextract(pool: &PgPool, routes: &[String]) -> Result<u64> {
    check_routes(routes)?;
    if routes.iter().any(|r| r == "also-on") {
        // Every resource but an alias (whose root holds its results), in batches of 1000.
        let mut last = String::new();
        let mut queued = 0;
        loop {
            let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM resource r WHERE id>$1 AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id) ORDER BY id LIMIT 1000")
                .bind(&last)
                .fetch_all(pool)
                .await?;
            let Some(end) = ids.last().cloned() else {
                break;
            };
            let mut tx = pool.begin().await?;
            queue_also_on(&mut tx, &ids).await?;
            tx.commit().await?;
            queued += ids.len() as u64;
            last = end;
        }
        let rest: Vec<String> = routes.iter().filter(|r| *r != "also-on").cloned().collect();
        return Ok(if rest.is_empty() {
            queued
        } else {
            Box::pin(reextract(pool, &rest)).await?
        });
    }
    let all = routes.iter().any(|r| r == "all");
    // Two passes over every row: the first releases the aliases no rule names any more and
    // re-queues, the second moves aliases. A move follows its new root's own alias
    // (`alias::assign`), so it must never meet one the first pass is still to release, in its
    // batch or a later one.
    let mut total = 0u64;
    for moves in [false, true] {
        total += reextract_pass(pool, routes, all, moves).await?;
    }
    Ok(total)
}
/// One pass of `reextract`: with `moves`, only its alias moves; without, everything else.
async fn reextract_pass(pool: &PgPool, routes: &[String], all: bool, moves: bool) -> Result<u64> {
    let (mut last, mut total) = (String::new(), 0u64);
    loop {
        let rows: Vec<ReextractRow> = sqlx::query_as(
            "SELECT r.id,r.uri,r.resolver,a.root_id,a.basis FROM resource r LEFT JOIN resource_alias a ON a.alias_id=r.id WHERE r.id>$1 ORDER BY r.id LIMIT 1000",
        )
        .bind(&last)
        .fetch_all(pool)
        .await?;
        let Some(end) = rows.last().map(|r| r.0.clone()) else {
            return Ok(total);
        };
        let (mut ids, mut new_routes, mut types, mut osm_ids) = (vec![], vec![], vec![], vec![]);
        let mut aliases = vec![];
        let mut alias_rows = vec![];
        let mut released = vec![];
        for (id, uri, stored, root, basis) in rows {
            // An alias has no route of its own. A selected one whose URI a rule now gives another
            // root moves to it; one no rule names any more (a source that read it as another
            // object's spelling reads it at its own address now) answers for itself again and is
            // re-queued on its route.
            if let Some(root) = root {
                // A link whose source is switched off keeps what that source merged: its rule
                // is only silent, so a weaker one (a tracking parameter) must not move it, and
                // nothing releases it.
                if crate::sources::switched_off(&uri) {
                    if !moves {
                        alias_rows.push(id);
                    }
                    continue;
                }
                let route = crate::resolver::classify(&uri).map(str::to_owned);
                let selected = all || route.as_ref().is_some_and(|r| routes.contains(r));
                match (crate::alias::root(&uri), route) {
                    (Some(alias), _)
                        if selected
                            && crate::model::identity(&alias.root)
                                .is_ok_and(|(named, _, _)| named != root) =>
                    {
                        if moves {
                            aliases.push((id, alias))
                        }
                    }
                    // Only what a rule of this build would name: never a page's canonical or
                    // an official API's answer (no rule of the URI). A rule that names a root
                    // moves those too (above), as a fresh read would follow it.
                    (None, Some(route))
                        if !moves
                            && selected
                            && !matches!(basis.as_deref(), Some("canonical" | "source")) =>
                    {
                        released.push(id.clone());
                        ids.push(id);
                        new_routes.push(route);
                        types.push(None);
                        osm_ids.push(None);
                    }
                    _ if !moves => alias_rows.push(id),
                    _ => {}
                }
                continue;
            }
            if let Some(alias) = crate::alias::root(&uri) {
                if moves && (all || stored.as_ref().is_some_and(|s| routes.contains(s))) {
                    aliases.push((id, alias));
                }
                continue;
            }
            if moves {
                continue;
            }
            // A page its own card promoted to media or activitypub stays on that route: the
            // classifier only knows the URL, not what the page turned out to be.
            let promoted = stored
                .as_deref()
                .filter(|s| matches!(*s, "media" | "activitypub"));
            let route = promoted
                .map(str::to_owned)
                .or_else(|| crate::resolver::classify(&uri).map(str::to_owned))
                .or(stored.clone());
            let Some(route) = route else { continue };
            if !all
                && !routes
                    .iter()
                    .any(|r| Some(r) == stored.as_ref() || *r == route)
            {
                continue;
            }
            let osm = (route == "osm")
                .then(|| crate::resolver::osm::parse_id(&uri))
                .flatten();
            ids.push(id);
            new_routes.push(route);
            types.push(osm.as_ref().map(|o| o.osm_type.clone()));
            osm_ids.push(osm.map(|o| o.id));
        }
        // An alias row that holds a failure gets an alias's queue state (`alias::CLEAR_QUEUE`).
        if !alias_rows.is_empty() {
            sqlx::query(crate::alias::CLEAR_QUEUE)
                .bind(&alias_rows)
                .execute(pool)
                .await?;
        }
        if !aliases.is_empty() || !released.is_empty() {
            let mut tx = pool.begin().await?;
            crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
            crate::alias::lock(&mut tx).await?;
            let mut lines = vec![];
            for (id, alias) in &aliases {
                lines.extend(crate::alias::assign(&mut tx, id, alias).await?);
            }
            for id in &released {
                lines.extend(crate::alias::release(&mut tx, id).await?);
            }
            crate::alias::record(&mut tx, lines).await?;
            tx.commit().await?;
            total += aliases.len() as u64;
        }
        if !ids.is_empty() {
            let mut tx = pool.begin().await?;
            // Many resources' rows, in no particular order.
            crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Exclusive).await?;
            sqlx::query("UPDATE event_fetch SET validators='{}' WHERE resource_id=ANY($1) AND validators<>'{}'")
                .bind(&ids)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE osm_place o SET refresh_at=least(o.refresh_at,now()) FROM resource r WHERE r.id=ANY($1) AND o.osm_type=r.osm_type AND o.osm_id=r.osm_id")
                .bind(&ids)
                .execute(&mut *tx)
                .await?;
            total += sqlx::query(r#"WITH v AS (
                  SELECT v.*,
                    (v.osm_id IS NOT NULL)<>(r.osm_id IS NOT NULL)
                      OR (v.route='web' AND lower(rtrim(r.host,'.')) ~ '(^|[.])(openstreetmap[.]org|osm[.]org)$') AS reset
                  FROM unnest($1::text[],$2::text[],$3::text[],$4::bigint[]) v(id,route,osm_type,osm_id) JOIN resource r ON r.id=v.id)
                UPDATE resource r SET resolver=v.route,osm_type=v.osm_type,osm_id=v.osm_id,
                doc=CASE WHEN v.osm_id IS NOT NULL THEN jsonb_build_object('type','Place','url',r.uri,'osm_id',v.osm_type||'/'||v.osm_id,'source','osm','status','pending')
                  WHEN v.reset THEN '{}'::jsonb ELSE r.doc END,
                type_path=CASE WHEN v.osm_id IS NOT NULL THEN NULL WHEN v.reset THEN '{}' ELSE r.type_path END,
                name=CASE WHEN v.reset THEN NULL ELSE r.name END,
                point=CASE WHEN v.reset THEN NULL ELSE r.point END,
                category=CASE WHEN v.reset THEN NULL ELSE r.category END,
                type=CASE WHEN v.reset THEN NULL ELSE r.type END,
                start_at=CASE WHEN v.reset THEN NULL ELSE r.start_at END,
                end_at=CASE WHEN v.reset THEN NULL ELSE r.end_at END,
                venue_id=CASE WHEN v.reset THEN NULL ELSE r.venue_id END,
                status=CASE WHEN v.reset THEN 'pending' ELSE r.status END,
                resolved_at=CASE WHEN v.reset THEN NULL ELSE r.resolved_at END,
                refresh_at=CASE WHEN v.reset THEN NULL ELSE r.refresh_at END,
                osm_version=CASE WHEN v.reset THEN NULL ELSE r.osm_version END,
                attempts=0,timeout_streak=0,dead_letter=false,final=false,last_error=NULL,
                lease_until=NULL,priority_at=NULL,generation=r.generation+1,next_attempt_at=now()
                FROM v WHERE r.id=v.id"#)
                .bind(&ids).bind(&new_routes).bind(&types).bind(&osm_ids).execute(&mut *tx).await?.rows_affected();
            queue_also_on(&mut tx, &ids).await?;
            tx.commit().await?;
        }
        last = end;
    }
}

pub fn backoff(cfg: &QueueConfig, attempt: u32) -> u64 {
    cfg.backoff_secs
        .saturating_mul(
            1u64.checked_shl(attempt.saturating_sub(1))
                .unwrap_or(u64::MAX),
        )
        .min(cfg.max_backoff_secs)
}
pub async fn failed(pool: &PgPool, job: &Job, cfg: &QueueConfig, error: &str) -> Result<()> {
    failed_as(pool, job, cfg, error, Status::Error).await
}
async fn failed_as(
    pool: &PgPool,
    job: &Job,
    cfg: &QueueConfig,
    error: &str,
    status: Status,
) -> Result<()> {
    let attempt = job.attempts as u32 + 1;
    sqlx::query(r#"UPDATE resource SET attempts=attempts+1, timeout_streak=0, dead_letter=$3, status=$6, lease_until=NULL,priority_at=NULL,
        next_attempt_at=now()+$4*interval '1 second', last_error=$5 WHERE id=$1 AND generation=$2"#)
        .bind(&job.id).bind(job.generation).bind(attempt>=cfg.max_failures).bind(backoff(cfg,attempt) as f64).bind(error).bind(status.as_str()).execute(pool).await?;
    Ok(())
}
/// A fetch that never got an answer from the upstream (`resolver::unreached`: the connection was
/// refused, reset or aborted, or the host unreachable). It says nothing about the link, so it never counts toward
/// the dead letter: it is read again with the queue's backoff, longer each time in a row up to
/// `max_backoff_secs`, for as long as it lasts. The streak is kept in `timeout_streak` while
/// `last_error` is `connect`, never in `attempts`, so it never brings a later answer about the
/// link nearer to the dead letter, and every other outcome starts it over. An accepted card and
/// its status stay; a link with no card yet turns `error` once the streak reaches `max_failures`,
/// and is still read again.
async fn unreached(pool: &PgPool, job: &Job, cfg: &QueueConfig) -> Result<()> {
    let mut tx = pool.begin().await?;
    let streak: Option<i32> = sqlx::query_scalar("SELECT CASE WHEN last_error='connect' THEN timeout_streak ELSE 0 END FROM resource WHERE id=$1 AND generation=$2 FOR UPDATE")
        .bind(&job.id)
        .bind(job.generation)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(streak) = streak {
        let streak = streak.saturating_add(1);
        sqlx::query("UPDATE resource SET timeout_streak=$2,status=CASE WHEN $4 AND status='pending' THEN 'error' ELSE status END,lease_until=NULL,priority_at=NULL,last_error='connect',next_attempt_at=now()+$3*interval '1 second' WHERE id=$1")
            .bind(&job.id)
            .bind(streak)
            .bind(backoff(cfg, streak as u32) as f64)
            .bind(streak as u32 >= cfg.max_failures)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
/// An organiser calendar answered from another origin (`event::adapters::fetch_calendar`): its
/// stored dates and status stay, and it is read again with the queue's backoff, longer each time
/// in a row. The streak is kept in `timeout_streak` while `last_error` is `off_origin`, never in
/// `attempts`, so it never brings a later ordinary failure nearer to the dead letter, and every
/// other answer (a read the platform answers, any failure) starts it over.
async fn off_origin(pool: &PgPool, job: &Job, cfg: &QueueConfig) -> Result<()> {
    let mut tx = pool.begin().await?;
    let streak: Option<i32> = sqlx::query_scalar("SELECT CASE WHEN last_error='off_origin' THEN timeout_streak ELSE 0 END FROM resource WHERE id=$1 AND generation=$2 FOR UPDATE")
        .bind(&job.id)
        .bind(job.generation)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(streak) = streak {
        let streak = streak.saturating_add(1);
        sqlx::query("UPDATE resource SET timeout_streak=$2,lease_until=NULL,priority_at=NULL,last_error='off_origin',next_attempt_at=now()+$3*interval '1 second' WHERE id=$1")
            .bind(&job.id)
            .bind(streak)
            .bind(backoff(cfg, streak as u32) as f64)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
async fn timed_out(pool: &PgPool, job: &Job, cfg: &QueueConfig, seconds: u64) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row: Option<(i32, i32)> = sqlx::query_as("SELECT CASE WHEN last_error='timeout' THEN timeout_streak ELSE 0 END,attempts FROM resource WHERE id=$1 AND generation=$2 FOR UPDATE")
        .bind(&job.id).bind(job.generation).fetch_optional(&mut *tx).await?;
    if let Some((streak, attempts)) = row {
        let streak = streak.saturating_add(1);
        let charged = streak as u32 > cfg.timeout_grace;
        let attempts = attempts.saturating_add(i32::from(charged));
        let base = cfg.backoff_secs.max(seconds.clamp(1, 3600));
        let delay = base
            .saturating_mul(
                1u64.checked_shl(streak.saturating_sub(1) as u32)
                    .unwrap_or(u64::MAX),
            )
            .min(cfg.max_backoff_secs.max(base));
        sqlx::query("UPDATE resource SET timeout_streak=$2,attempts=$3,dead_letter=$4,status=CASE WHEN $5 THEN 'error' ELSE status END,lease_until=NULL,priority_at=NULL,last_error='timeout',next_attempt_at=now()+$6*interval '1 second' WHERE id=$1")
            .bind(&job.id).bind(streak).bind(attempts).bind(charged && attempts as u32>=cfg.max_failures).bind(charged).bind(delay as f64).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
async fn denied(
    pool: &PgPool,
    job: &Job,
    cfg: &QueueConfig,
    retry: &crate::resolver::RetryLater,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row: Option<(i32,i32,bool)> = sqlx::query_as("SELECT denial_streak,attempts,coalesce(denied_since<=now()-interval '3 days',false) FROM resource WHERE id=$1 AND generation=$2 FOR UPDATE")
        .bind(&job.id).bind(job.generation).fetch_optional(&mut *tx).await?;
    if let Some((streak, attempts, expired)) = row {
        let attempts = attempts.saturating_add(i32::from(expired));
        let delay = if expired {
            backoff(cfg, attempts as u32).max(retry.seconds)
        } else {
            retry.seconds.clamp(1, 3600)
        };
        // Continue recovery probes at normal backoff; denial never dead-letters a
        // shared feed, and accepted facts remain available with an honest status.
        sqlx::query("UPDATE resource SET denial_streak=$2,denied_since=coalesce(denied_since,now()),attempts=$3,status=CASE WHEN $4 THEN 'unreachable' ELSE status END,timeout_streak=0,dead_letter=false,lease_until=NULL,priority_at=NULL,last_error=$5,next_attempt_at=now()+$6*interval '1 second' WHERE id=$1")
            .bind(&job.id).bind(streak.saturating_add(1)).bind(attempts).bind(expired).bind(retry.class).bind(delay as f64).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
/// One link read by its resolver, within `RESOLVE_TIMEOUT`, as the fetch that confirms a
/// missing answer when `confirming` (`egress::confirming`); a panic is an error.
async fn resolve(
    resolver: &dyn Resolver,
    uri: &str,
    confirming: bool,
) -> Result<crate::model::ResourceMeta> {
    let resolve = resolver.resolve(uri);
    let resolve = async {
        if confirming {
            crate::resolver::egress::confirming(resolve).await
        } else {
            resolve.await
        }
    };
    std::panic::AssertUnwindSafe(tokio::time::timeout(RESOLVE_TIMEOUT, resolve))
        .catch_unwind()
        .await
        .map_err(|_| anyhow::anyhow!("resolver panic"))
        .and_then(|r| {
            r.map_err(|_| {
                crate::resolver::RetryLater {
                    class: "timeout",
                    seconds: 60,
                }
                .into()
            })
        })
        .and_then(|r| r)
}
/// One job run alone with the default refresh tiers, for tests: the daemon's workers run every
/// job, single or batched, through `run_batch_config`.
pub async fn run_job(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    job: &Job,
) -> Result<()> {
    // Heap-allocate the large persistence/reconciliation future before composing it
    // with resolver futures, keeping default worker and test thread stacks bounded.
    Box::pin(run_job_inner(pool, resolver, cfg, job)).await
}
async fn run_job_inner(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    job: &Job,
) -> Result<()> {
    let confirming = confirming(pool, std::slice::from_ref(job)).await?;
    let result = resolve(resolver, &job.uri, confirming).await;
    guarded_finish_job(
        pool,
        resolver,
        cfg,
        &crate::config::RefreshConfig::default(),
        job,
        result,
    )
    .await
}
async fn guarded_finish_job(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    refresh: &crate::config::RefreshConfig,
    job: &Job,
    result: Result<crate::model::ResourceMeta>,
) -> Result<()> {
    // An unchanged page is its accepted document, typed again like a new one (what the also-on
    // sources found since may name or type it). One that carries a finding of a source that no
    // longer gives it (switched off, or its row expired: `media::outdated`) is not: its
    // validators go and the page is read again in full, once.
    let result = match result {
        Err(error) if error.is::<crate::resolver::NotModified>() => {
            let value: Option<serde_json::Value> =
                sqlx::query_scalar("SELECT doc FROM resource_document WHERE id=$1")
                    .bind(&job.id)
                    .fetch_optional(pool)
                    .await?;
            let cached = value
                .ok_or_else(|| anyhow::anyhow!("304 without an accepted document"))
                .and_then(|v| {
                    serde_json::from_value::<crate::model::ResourceMeta>(v).map_err(Into::into)
                })
                .map(|mut doc| {
                    doc.fetched_at = chrono::Utc::now().timestamp_millis();
                    doc
                });
            match cached {
                Ok(doc) if crate::also_on::media::outdated(pool, &job.id, &doc).await? => {
                    sqlx::query("UPDATE event_fetch SET validators='{}' WHERE resource_id=$1 AND validators<>'{}'")
                        .bind(&job.id)
                        .execute(pool)
                        .await?;
                    let confirming = confirming(pool, std::slice::from_ref(job)).await?;
                    match resolve(resolver, &job.uri, confirming).await {
                        Err(error) if error.is::<crate::resolver::NotModified>() => Ok(doc),
                        other => other,
                    }
                }
                other => other,
            }
        }
        other => other,
    };
    // Every finished job, single or batched: a music link's card is typed and named by what it
    // is, whatever its page said, and other services' cards are named after the service.
    let result = match result {
        Ok(mut doc) if crate::music::parse(&job.uri).is_some() => {
            // What the also-on sources said about the artist (a person or a group), if anything.
            let artist = crate::also_on::media::artist_type(pool, &job.id).await?;
            crate::music::type_card(&job.uri, artist.as_ref().map(|(t, _)| t.as_str()), &mut doc);
            if let Some((t, source)) =
                artist.filter(|(t, _)| t == "Person" && doc.type_name == "Person")
            {
                let fingerprint = crate::also_on::media::fingerprint(&[&t]);
                crate::also_on::media::hinted(&mut doc, &source, fingerprint);
            }
            Ok(doc)
        }
        // A card of a service the lookups know is named after it, as its derived links are.
        Ok(mut doc) if doc.status == crate::model::Status::Ok => {
            if let Some(name) = crate::lookups::service_name(&job.uri) {
                doc.site = name.into();
            }
            Ok(doc)
        }
        other => other,
    };
    // The service its URL names, which alone lets it take a service's place on a card.
    let result = result.map(|mut doc| {
        if doc.status == crate::model::Status::Ok {
            doc.service = crate::api::service_site(&job.uri);
        }
        doc
    });
    // So is a book, podcast or film link, by what the media sources found (an episode, a title's
    // type).
    let result = match result {
        Ok(mut doc) => {
            let hints = crate::also_on::media::type_hints(pool, std::slice::from_ref(&job.id))
                .await?
                .remove(&job.id)
                .unwrap_or_default();
            crate::also_on::media::type_card(&job.uri, &hints, &mut doc);
            Ok(doc)
        }
        other => other,
    };
    // An enrichment a transient failure left unread keeps what the stored card has of it.
    let result = match result {
        Ok(mut doc) if !doc.unread_enriched.is_empty() => {
            let stored: Option<serde_json::Value> =
                sqlx::query_scalar("SELECT doc FROM resource_document WHERE id=$1")
                    .bind(&job.id)
                    .fetch_optional(pool)
                    .await?;
            match stored.and_then(|v| serde_json::from_value::<crate::model::ResourceMeta>(v).ok())
            {
                Some(stored) => doc.keep_enriched(&stored),
                None => doc.unread_enriched.clear(),
            }
            Ok(doc)
        }
        other => other,
    };
    match std::panic::AssertUnwindSafe(finish_job(pool, resolver, cfg, refresh, job, result))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(_) => {
            crate::telemetry::parser_error();
            tracing::error!(resource_id=%job.id, resolver=resolver.name(), "job completion panicked");
            failed(pool, job, cfg, "parse").await
        }
    }
}
async fn finish_job(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    refresh: &crate::config::RefreshConfig,
    job: &Job,
    result: Result<crate::model::ResourceMeta>,
) -> Result<()> {
    let slow_cfg;
    let manifest = resolver.manifest_for(&job.uri);
    let cfg = if resolver.name() == "event_platform" && manifest.mode == crate::adapter::Mode::None
    {
        slow_cfg = QueueConfig {
            backoff_secs: cfg.backoff_secs.max(manifest.cadence_floor),
            max_backoff_secs: cfg.max_backoff_secs.max(manifest.cadence_floor),
            ..cfg.clone()
        };
        &slow_cfg
    } else {
        cfg
    };
    let result = async {
        let mut doc = result?;
        if doc.status == Status::Error {
            // Recorded by the answer's status, not as a parser failure.
            let class = match doc.http_status {
                Some(status) if status < 500 => "http_4xx",
                _ => "http_5xx",
            };
            let error = anyhow::Error::new(crate::resolver::ErrorClass(class));
            return Err(match doc.http_status {
                Some(status) => error.context(crate::resolver::Upstream(status)),
                None => error.context("upstream returned a transient HTTP error"),
            });
        }
        let invalid_identifier =
            doc.status == Status::Missing && crate::resolver::cards::invalid_identifier(&job.uri);
        if resolver.name() != "osm" && !invalid_identifier {
            doc.ttl_secs = if doc.status == Status::Missing {
                cfg.missing_ttl_secs
            } else {
                resolver.refresh_after(&doc).as_secs()
            };
        }
        if doc.status == Status::Missing {
            let dates: (
                Option<chrono::DateTime<chrono::Utc>>,
                Option<chrono::DateTime<chrono::Utc>>,
            ) = sqlx::query_as("SELECT start_at,end_at FROM resource WHERE id=$1")
                .bind(&job.id)
                .fetch_one(pool)
                .await?;
            doc.start = doc.start.or(dates.0);
            doc.end = doc.end.or(dates.1);
        }
        if matches!(
            resolver.name(),
            "web"
                | "event"
                | "event_platform"
                | "pubky"
                | "geo"
                | "osm"
                | "media"
                | "isbn"
                | "doi"
                | "nostr"
                | "activitypub"
        ) {
            let now = chrono::Utc::now();
            let mut manifest = resolver.manifest_for(&job.uri);
            if resolver.name() == "osm" {
                manifest.cadence_floor = doc.ttl_secs;
            }
            if doc.status != Status::Missing
                || doc.end.or(doc.start).is_some_and(|end| {
                    end.checked_add_signed(chrono::Duration::hours(24))
                        .is_some_and(|at| now >= at)
                })
            {
                let next = if let Some(container) = &doc.container {
                    if container.cursor.is_some() {
                        manifest.mode = crate::adapter::Mode::Cursor;
                    } else {
                        manifest.mode = crate::adapter::Mode::Container;
                    }
                    crate::scheduler::container_next(
                        &manifest,
                        container
                            .occurrences
                            .iter()
                            .map(|o| (o.doc.start, o.doc.end)),
                        container.feed_ttl,
                        now,
                        refresh,
                    )
                } else {
                    if manifest.mode == crate::adapter::Mode::None
                        && resolver.name() == "event_platform"
                    {
                        now.checked_add_signed(
                            chrono::Duration::try_seconds(
                                manifest.cadence_floor.min(i64::MAX as u64) as i64,
                            )
                            .unwrap_or(chrono::Duration::MAX),
                        )
                    } else {
                        crate::scheduler::next(&manifest, &doc, now, refresh)
                    }
                };
                doc.ttl_secs = next.map_or(0, |at| (at - now).num_seconds().max(0) as u64);
            } else if resolver.name() != "osm" {
                doc.ttl_secs = doc.ttl_secs.max(3600);
            }
        }
        doc.ttl_secs = refresh_by(doc.ttl_secs, doc.refresh_by, chrono::Utc::now());
        if invalid_identifier {
            doc.ttl_secs = 0;
        }
        if crate::resolver::cards::social(&doc) {
            doc.ttl_secs = doc.ttl_secs.min(3600);
        }
        let attrs = match resolver.name() {
            "web" | "official" => crate::sources::attributes(),
            // These routes fall back to the web fetch, whose page hooks may name a source.
            "event" | "event_platform" | "media" | "isbn" | "doi" | "nostr" | "activitypub" => {
                if crate::sources::by_name(&doc.source).is_some() {
                    crate::sources::attributes()
                } else {
                    crate::resolver::web::ATTRIBUTES
                }
            }
            "pubky" => crate::resolver::pubky_apps::attributes(),
            "geo" => crate::resolver::geo::ATTRIBUTES,
            "osm" => crate::resolver::osm::ATTRIBUTES,
            _ => &[],
        };
        doc.validate(attrs)?;
        crate::changes::save(pool, &job.id, job.generation, &doc, cfg).await?;
        // The save is done. What follows it (the alias its page names, an episode's pages) are
        // transactions of their own: when one fails, what the save wrote stays and the job is
        // no failure.
        let after = std::panic::AssertUnwindSafe(after_save(pool, job, &doc))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the steps after the save panicked")));
        if let Err(error) = after {
            tracing::warn!(resource_id=%job.id,host=%crate::resolver::log_host(&job.uri),kind=crate::resolver::error_class(&error),resolver=resolver.name(),"a step after the save failed (an alias, an episode's pages)");
            // An accepted card's step has its inputs (the page's canonical, an episode's page)
            // only in a full read: the link is read again, without its validators, no later
            // than `max_backoff_secs` from now and no earlier than it was due anyway. Once an
            // hour at most, so a step that fails every time costs one fetch an hour; no attempt
            // is charged. When this cannot be written either, the step waits for the card's
            // own next full read.
            if doc.status == Status::Ok {
                let again = async {
                    sqlx::query("UPDATE event_fetch SET validators='{}' WHERE resource_id=$1 AND validators<>'{}' AND EXISTS(SELECT 1 FROM resource WHERE id=$1 AND generation=$2)")
                        .bind(&job.id).bind(job.generation).execute(pool).await?;
                    sqlx::query("UPDATE resource SET final=false,refresh_at=least(refresh_at,now()+$3*interval '1 second'),next_attempt_at=least(next_attempt_at,now()+$3*interval '1 second') WHERE id=$1 AND generation=$2")
                        .bind(&job.id).bind(job.generation).bind(cfg.max_backoff_secs as f64).execute(pool).await?;
                    anyhow::Ok(())
                };
                if let Err(error) = again.await {
                    tracing::warn!(resource_id=%job.id,error=%crate::resolver::log_error(&error),"the full re-read after a failed step was not scheduled");
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        // The host, the stored error kind and the upstream status, so a failure can be found by
        // site or by kind; never the error's text, which may hold what an upstream answered.
        tracing::warn!(resource_id=%job.id,host=%crate::resolver::log_host(&job.uri),kind=crate::resolver::error_class(&error),status=crate::resolver::status_of(&error).map(|s| s.to_string()).unwrap_or_default(),resolver=resolver.name(),"resolution failed");
        if error.is::<crate::resolver::interstitial::Blocked>() {
            // A challenge page: unreachable, with the normal failure backoff and dead letter.
            // The last accepted document stays; a wall never confirms disappearance.
            failed_as(pool, job, cfg, "wall", Status::Unreachable).await?;
        } else if let Some(retry) = error.downcast_ref::<crate::resolver::RetryLater>() {
            if retry.class == "timeout" {
                timed_out(pool, job, cfg, retry.seconds).await?;
            } else if matches!(retry.class, "upstream_denied" | "graphql") {
                denied(pool, job, cfg, retry).await?;
            } else if retry.class == "off_origin" {
                off_origin(pool, job, cfg).await?;
            } else {
                // Egress's own waits (its host budget, a cached failure replayed) say nothing new
                // about the link: a refused-connection streak stays, so its backoff keeps growing.
                let own = matches!(retry.class, "rate_limited" | "discovery_cached");
                sqlx::query("UPDATE resource SET timeout_streak=CASE WHEN $5 AND last_error='connect' THEN timeout_streak ELSE 0 END,lease_until=NULL,priority_at=NULL,next_attempt_at=now()+$3*interval '1 second',last_error=CASE WHEN $5 AND last_error='connect' THEN last_error ELSE $4 END WHERE id=$1 AND generation=$2")
                    .bind(&job.id).bind(job.generation).bind(retry.seconds.clamp(1,3600) as f64).bind(retry.class).bind(own).execute(pool).await?;
            }
        } else if crate::resolver::unreached(&error) {
            // A refused, reset or unreachable connection: retried with backoff, never dead-lettered.
            unreached(pool, job, cfg).await?;
        } else if crate::resolver::error_class(&error) == "timeout" {
            timed_out(pool, job, cfg, 60).await?;
        } else if crate::resolver::error_class(&error) == "too_complex" {
            // The parse did not end within its limits, and would not next time: final, with no
            // card (an earlier one is cleared), until a re-extraction revives it.
            crate::changes::clear(pool, &job.id, job.generation, "too_complex").await?;
        } else if crate::resolver::error_class(&error) == "policy"
            && url::Url::parse(&job.uri)
                .ok()
                .and_then(|u| u.host_str().map(crate::resolver::osm::osm_host))
                .unwrap_or(false)
        {
            // Web refuses every OSM-host page that is not an object or a location link before any
            // fetch. The refusal is local and permanent: store it once as final, not toward
            // dead-letter; a later routing change revives it through the reroute trigger.
            sqlx::query("UPDATE resource SET status='error',attempts=attempts+1,timeout_streak=0,dead_letter=false,final=true,lease_until=NULL,priority_at=NULL,last_error='unsupported',next_attempt_at='infinity' WHERE id=$1 AND generation=$2")
                .bind(&job.id).bind(job.generation).execute(pool).await?;
        } else {
            let mut cfg = cfg.clone();
            if resolver.name() == "geo"
                || error
                    .downcast_ref::<crate::resolver::ErrorClass>()
                    .is_some_and(|c| c.0 == "policy")
            {
                cfg.max_failures = 1;
            }
            failed(pool, job, &cfg, crate::resolver::error_class(&error)).await?;
        }
    }
    Ok(())
}
/// What follows a saved card: its alias and, for a podcast episode, its pages.
async fn after_save(pool: &PgPool, job: &Job, doc: &crate::model::ResourceMeta) -> Result<()> {
    #[cfg(test)]
    tests::maybe_fail_after_save(&job.uri)?;
    // An official API names the object's root (`source_root`), or the page names itself (its
    // head canonical, or the end of an all-permanent redirect chain) up to a trailing slash
    // and tracking, or as the other spelling of its event on a platform that gives each
    // event two (`alias::event_spelling`): the link is an alias of that URL, which is fetched
    // in its place from now on. Other claimed URLs are never candidates.
    if doc.status == Status::Ok {
        if let Some(alias) = doc
            .source_root
            .as_deref()
            .and_then(|root| crate::alias::source(&job.uri, root))
            .or_else(|| {
                doc.alias_candidates
                    .iter()
                    .find_map(|a| crate::alias::canonical(&job.uri, a))
                    .or_else(|| {
                        doc.event_spellings
                            .iter()
                            .find_map(|a| crate::alias::event_spelling(&job.uri, a))
                    })
            })
        {
            let mut tx = pool.begin().await?;
            crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
            crate::alias::lock(&mut tx).await?;
            // The generation fence covers the alias too: a stale job writes nothing.
            let current: Option<String> = sqlx::query_scalar(
                "SELECT id FROM resource WHERE id=$1 AND generation=$2 FOR UPDATE",
            )
            .bind(&job.id)
            .bind(job.generation)
            .fetch_optional(&mut *tx)
            .await?;
            if current.is_some() {
                let line = crate::alias::assign(&mut tx, &job.id, &alias).await?;
                crate::alias::record(&mut tx, line.into_iter().collect()).await?;
            }
            tx.commit().await?;
        }
        // A podcast episode read from its feed item: the page the item links, on the feed's
        // own site, becomes an alias of this card, and a page it no longer names lets go
        // (`episodes::join_page`, `episodes::release_pages`).
        if doc.episode_item {
            let page = doc.episode_page.as_deref();
            crate::episodes::release_pages(pool, &job.id, job.generation, page, false).await?;
            if let Some(page) = page {
                crate::episodes::join_page(pool, &job.id, job.generation, &job.uri, page).await?;
            }
        }
    } else if doc.status == Status::Missing && crate::episodes::names_an_item(&job.uri) {
        // The item has left its feed: the pages joined to its card answer for themselves.
        crate::episodes::release_pages(pool, &job.id, job.generation, None, true).await?;
    }
    Ok(())
}
/// A batch run with the default refresh tiers, for tests and the e2e stack.
pub async fn run_batch(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    jobs: &[Job],
) -> Result<()> {
    run_batch_config(
        pool,
        resolver,
        cfg,
        &crate::config::RefreshConfig::default(),
        jobs,
    )
    .await
}
/// Whether any of `jobs` is the fetch that confirms a missing answer (one missing answer, not
/// yet `missing`): it must ask the upstream again, never take a cached failure as the second.
async fn confirming(pool: &PgPool, jobs: &[Job]) -> Result<bool> {
    let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource WHERE id=ANY($1) AND missing_streak>0 AND status<>'missing')")
        .bind(&ids)
        .fetch_one(pool)
        .await?)
}
async fn run_batch_config(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    refresh: &crate::config::RefreshConfig,
    jobs: &[Job],
) -> Result<()> {
    let deadline = RESOLVE_TIMEOUT * jobs.len() as u32;
    run_batch_within(pool, resolver, cfg, refresh, jobs, deadline).await
}
/// `run_batch_config` with the batch's deadline given. A batch that passes it is a timeout for
/// each of its jobs, as a resolver's own timeout is (`timed_out`: the first `timeout_grace` in a
/// row charge no attempt), and is not counted as a parser error.
async fn run_batch_within(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    refresh: &crate::config::RefreshConfig,
    jobs: &[Job],
    deadline: Duration,
) -> Result<()> {
    let uris = jobs.iter().map(|j| j.uri.clone()).collect::<Vec<_>>();
    let confirming = confirming(pool, jobs).await?;
    let resolve = async {
        let resolve = std::panic::AssertUnwindSafe(resolver.resolve_batch(&uris)).catch_unwind();
        if confirming {
            crate::resolver::egress::confirming(resolve).await
        } else {
            resolve.await
        }
    };
    let results = tokio::time::timeout(deadline, resolve).await;
    let late = results.is_err();
    let results = match results {
        Ok(Ok(results)) if results.len() == jobs.len() => results,
        Err(_) => jobs
            .iter()
            .map(|_| {
                Err(crate::resolver::RetryLater {
                    class: "timeout",
                    seconds: 60,
                }
                .into())
            })
            .collect(),
        _ => jobs
            .iter()
            .map(|_| Err(anyhow::anyhow!("batch panic or invalid response count")))
            .collect(),
    };
    if !late
        && results.iter().any(|r| {
            r.as_ref()
                .is_err_and(|e| !e.is::<crate::resolver::NotModified>())
        })
    {
        crate::telemetry::parser_error();
    }
    let class = results.iter().find_map(|r| match r {
        Err(e) if !e.is::<crate::resolver::NotModified>() => {
            Some(crate::resolver::error_class(e).to_owned())
        }
        Ok(doc) if matches!(doc.status, Status::Unreachable | Status::Error) => Some(
            if doc.status == Status::Unreachable {
                "http_4xx"
            } else {
                "http_5xx"
            }
            .into(),
        ),
        _ => None,
    });
    // The source's health is bookkeeping: when it cannot be written, the jobs still finish.
    if crate::telemetry::last_fetch().is_some() {
        let source = resolver.manifest_for(&jobs[0].uri).source;
        if crate::source::outcome(pool, &source, class.as_deref())
            .await
            .is_err()
        {
            tracing::warn!(
                source = %source,
                class = class.as_deref().unwrap_or("ok"),
                host = %crate::resolver::log_host(&jobs[0].uri),
                "source health not recorded"
            );
        }
    }
    // Every job of the batch is finished, whichever one fails; the first failure is returned.
    let mut failure = None;
    for (job, result) in jobs.iter().zip(results) {
        if let Err(error) = guarded_finish_job(pool, resolver, cfg, refresh, job, result).await {
            if failure.is_some() {
                tracing::error!(resource_id = %job.id, error = %crate::resolver::log_error(&error), "queue persistence failed");
            } else {
                failure = Some(error);
            }
        }
    }
    failure.map_or(Ok(()), Err)
}
/// The links one refusal puts off at most (`AuthorBudget::admit_or_spread`). Fixed: it bounds
/// one statement's row writes, and a longer backlog is put off a batch at a time.
const SPREAD_BATCH: i64 = 1000;
/// Limits new work attributable to a public author, across both resolver kinds.
#[derive(Default)]
pub struct AuthorBudget(Mutex<HashMap<String, Instant>>);
impl AuthorBudget {
    /// Whether `job` may run now: one of its sponsors has budget left. A refused job goes back
    /// to the queue for when the first of them has.
    pub async fn admit(&self, pool: &PgPool, job: &Job, rate: f64) -> Result<bool> {
        Ok(self.admit_or_spread(pool, job, rate).await?.is_none())
    }
    /// `admit`; for a refused job, the other due links it put off (`Some`). A refused job's
    /// sponsors are all over budget, so the other due links only they tagged would be refused
    /// in turn, one claim each, ahead of every other author's links: each is given its turn at
    /// the sponsors' rate instead, in one statement, and the queue goes on with the others'.
    /// Links with another sponsor (an alias, a derived link's parent, another author) are left
    /// to their own claim.
    async fn admit_or_spread(&self, pool: &PgPool, job: &Job, rate: f64) -> Result<Option<u64>> {
        let authors: Vec<String> = sqlx::query_scalar(
            "WITH inputs AS (SELECT $1::text AS id UNION SELECT alias_id FROM resource_alias WHERE root_id=$1)
            SELECT DISTINCT author FROM (
                SELECT f.author FROM inputs i JOIN effective_reference f ON f.resource_id=i.id
                UNION ALL SELECT f.author FROM inputs i JOIN resource_derivation d ON d.child_id=i.id JOIN effective_reference f ON f.resource_id=d.parent_id
                UNION ALL SELECT 'adapter:'||d.adapter FROM inputs i JOIN resource_derivation d ON d.child_id=i.id WHERE NOT EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=d.parent_id)
            ) sponsors ORDER BY author",
        )
        .bind(&job.id)
        .fetch_all(pool)
        .await?;
        if authors.is_empty() {
            return Ok(None);
        }
        let now = Instant::now();
        let mut budgets = self.0.lock().await;
        budgets.retain(|_, next| *next > now);
        // Any referring author can sponsor the shared fetch; never charge it once per reference.
        let author = authors.iter().find(|a| !budgets.contains_key(*a));
        if let Some(author) = author {
            budgets.insert(author.clone(), now + Duration::from_secs_f64(1.0 / rate));
            return Ok(None);
        }
        let delay = authors
            .iter()
            .filter_map(|a| budgets.get(a))
            .min()
            .expect("nonempty authors")
            .duration_since(now);
        drop(budgets);
        sqlx::query("UPDATE resource SET lease_until=NULL,next_attempt_at=now()+$3*interval '1 second' WHERE id=$1 AND generation=$2")
            .bind(&job.id).bind(job.generation).bind(delay.as_secs_f64()).execute(pool).await?;
        // The n-th of the other due links waits n turns of the sponsors' budgets after the job,
        // `SPREAD_BATCH` of them at a time (the next refusal takes the next). Rows a worker
        // holds are passed over; a lookup's or a preview's place is kept, and so are `geo:`
        // links, which no budget covers. Only a refused link that is itself such a plain link
        // puts the others off: one that is not (a derived link, an alias's root, a looked-up
        // link) waits its own turn and moves no other.
        let spread = sqlx::query(
            "WITH due AS (
                SELECT r.id,r.next_attempt_at FROM resource r
                WHERE r.id<>$1 AND NOT r.final AND NOT r.dead_letter AND r.next_attempt_at<=now()
                  AND r.priority_at IS NULL AND (r.lease_until IS NULL OR r.lease_until<now())
                  AND r.resolver IS DISTINCT FROM 'geo'
                  AND r.id IN (SELECT f.resource_id FROM reference f WHERE f.author=ANY($2) AND f.kind<>'post')
                  AND NOT EXISTS(SELECT 1 FROM reference f WHERE f.resource_id=r.id AND f.kind<>'post' AND f.author<>ALL($2))
                  AND NOT EXISTS(SELECT 1 FROM resource_derivation d WHERE d.child_id=r.id)
                  AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.root_id=r.id)
                  AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id)
                  AND (SELECT j.priority_at IS NULL
                         AND NOT EXISTS(SELECT 1 FROM resource_derivation d WHERE d.child_id=j.id)
                         AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.root_id=j.id)
                       FROM resource j WHERE j.id=$1)
                ORDER BY r.next_attempt_at,r.id LIMIT $5
                FOR UPDATE OF r SKIP LOCKED
            ), turns AS (SELECT id,row_number() OVER (ORDER BY next_attempt_at,id) AS n FROM due)
            UPDATE resource SET next_attempt_at=now()+($3+turns.n*$4)*interval '1 second' FROM turns WHERE resource.id=turns.id",
        )
        .bind(&job.id)
        .bind(&authors)
        .bind(delay.as_secs_f64())
        .bind(1.0 / (rate * authors.len() as f64))
        .bind(SPREAD_BATCH)
        .execute(pool)
        .await;
        // The refused job is back in the queue either way: a deferral that failed puts nothing
        // off, and the other links are refused at their own claims.
        Ok(Some(match spread {
            Ok(done) => done.rows_affected(),
            Err(error) => {
                let error = anyhow::Error::from(error);
                tracing::warn!(error=%crate::resolver::log_error(&error),"an over-budget author's other links were not put off");
                0
            }
        }))
    }
}

/// The queue's workers on `egress`: the daemon's one, whose per-host budgets the also-on sources
/// share, or a test stack's injected transport.
pub fn spawn_with_egress(
    pool: PgPool,
    mut config: Config,
    wake: Arc<Notify>,
    shutdown: watch::Receiver<bool>,
    metrics: Arc<crate::telemetry::Metrics>,
    egress: Egress,
) -> Result<JoinSet<()>> {
    config.queue.groups = config.groups.clone();
    let mut workers = JoinSet::new();
    let authors = Arc::new(AuthorBudget::default());
    let mut resolvers: Vec<(Arc<dyn Resolver>, crate::config::ResolverConfig)> = vec![];
    let osm = Arc::new(OsmResolver::new(pool.clone(), config.osm.clone())?);
    let events = config.event.enabled.then(|| {
        Arc::new(crate::event::service::EventService {
            pool: pool.clone(),
            config: config.event.clone(),
            osm: osm.clone(),
        })
    });
    if config.web.enabled {
        resolvers.push((
            Arc::new(WebResolver {
                oembed_policy: config.media.oembed,
                cards: Some(Arc::new(config.cards.clone())),
                events: events.clone(),
                egress: egress.clone(),
                config: config.web.clone(),
            }),
            config.web.clone(),
        ));
    }
    if let Some(events) = events.clone().filter(|_| config.event.adapters.enabled) {
        resolvers.push((
            Arc::new(crate::event::adapters::service::PlatformResolver {
                web: WebResolver {
                    oembed_policy: config.media.oembed,
                    cards: Some(Arc::new(config.cards.clone())),
                    egress: egress.clone(),
                    config: config.web.clone(),
                    events: Some(events),
                },
            }),
            config.web.clone(),
        ));
    }
    if let Some(events) = events.filter(|_| config.event.calendars) {
        resolvers.push((
            Arc::new(crate::event::service::CalendarResolver {
                web: WebResolver {
                    oembed_policy: config.media.oembed,
                    cards: Some(Arc::new(config.cards.clone())),
                    egress: egress.clone(),
                    config: config.web.clone(),
                    events: Some(events),
                },
            }),
            config.web.clone(),
        ));
    }
    for kind in crate::resolver::cards::KINDS {
        if config.cards.resolver(kind).enabled {
            resolvers.push((
                Arc::new(crate::resolver::cards::CardResolver::new(
                    kind,
                    Arc::new(config.clone()),
                    egress.clone(),
                )),
                config.cards.resolver(kind).clone(),
            ));
        }
    }
    if config.pubky.enabled {
        resolvers.push((
            Arc::new(PubkyResolver::new(config.pubky.clone(), egress)?),
            config.pubky.clone(),
        ));
    }
    if config.geo.enabled {
        resolvers.push((
            Arc::new(GeoResolver),
            crate::config::ResolverConfig {
                concurrency: 1,
                requests_per_second: 100.0,
                ..Default::default()
            },
        ));
    }
    if config.osm.enabled {
        resolvers.push((
            osm,
            crate::config::ResolverConfig {
                concurrency: 1,
                requests_per_second: config.osm.requests_per_second,
                ttl_secs: config.osm.ttl_secs,
                enabled: true,
                ..Default::default()
            },
        ));
    }
    for (resolver, budget) in resolvers {
        // Every source this resolver reads has its series from the start, at 0, so its first
        // drift, 429 or cancellation is a rise an alert reads.
        for manifest in resolver.manifests() {
            metrics.source_series(&manifest);
        }
        let rate = Arc::new(Mutex::new(Instant::now()));
        for _ in 0..budget.concurrency {
            let worker = Worker {
                pool: pool.clone(),
                resolver: resolver.clone(),
                requests_per_second: budget.requests_per_second,
                cfg: config.queue.clone(),
                refresh: config.refresh.clone(),
                canary: config.canary.clone(),
                authors: authors.clone(),
                metrics: metrics.clone(),
                wake: wake.clone(),
                rate: rate.clone(),
            };
            workers.spawn(worker.run(shutdown.clone()));
        }
    }
    Ok(workers)
}

/// One worker of a resolver; the resolver's workers share its pacing slot (`rate`).
struct Worker {
    pool: PgPool,
    resolver: Arc<dyn Resolver>,
    requests_per_second: f64,
    cfg: QueueConfig,
    refresh: crate::config::RefreshConfig,
    canary: crate::config::CanaryConfig,
    authors: Arc<AuthorBudget>,
    metrics: Arc<crate::telemetry::Metrics>,
    wake: Arc<Notify>,
    rate: Arc<Mutex<Instant>>,
}
impl Worker {
    /// Until shutdown: wait for the pacing slot, run a due canary or claim a batch, and run it.
    /// Shutdown ends the wait for the slot at once. A claim, a canary and a claimed batch are
    /// finished first, so no row stays leased with no worker and no job stops between its save
    /// and the steps that follow it.
    async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut canary_check = Instant::now();
        while !*shutdown.borrow() {
            // Only the wait for the pacing slot is cut off: a claim is a row already leased.
            let slot = tokio::select! {
                _ = shutdown.changed() => break,
                slot = self.pace() => slot,
            };
            let jobs = self.next(&mut canary_check).await;
            if jobs.is_empty() {
                continue;
            }
            self.finish(&jobs, slot).await;
        }
    }
    /// Wait for the resolver's pacing slot and take it: the instant the next one is free.
    async fn pace(&self) -> Instant {
        let mut next = self.rate.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = Instant::now() + Duration::from_secs_f64(1.0 / self.requests_per_second);
        *next
    }
    /// The next batch; none when a canary ran instead, the queue held nothing due (after a
    /// short wait) or the claim failed.
    async fn next(&self, canary_check: &mut Instant) -> Vec<Job> {
        let (pool, resolver) = (&self.pool, self.resolver.as_ref());
        if Instant::now() >= *canary_check {
            *canary_check = Instant::now() + Duration::from_secs(60);
            // One suspension for the manifests whose canary may not run, one claim among the rest.
            let (allowed, refused): (Vec<_>, Vec<_>) = resolver
                .manifests()
                .into_iter()
                .partition(|m| resolver.canary_allowed_for(m));
            let refused: Vec<&str> = refused.iter().map(|m| m.source.as_str()).collect();
            if let Err(e) = crate::canary::suspend(pool, &refused).await {
                tracing::warn!(error=%crate::resolver::log_error(&e),"canary suspension failed");
            }
            match crate::canary::claim_due(pool, &allowed, &self.canary).await {
                Ok(Some((manifest, generation))) => {
                    if let Err(e) = crate::canary::run_manifest(
                        pool,
                        resolver,
                        manifest,
                        &self.canary,
                        &self.metrics,
                        generation,
                    )
                    .await
                    {
                        tracing::warn!(error=%crate::resolver::log_error(&e),"canary bookkeeping failed");
                    }
                    return vec![];
                }
                Err(e) => {
                    tracing::warn!(error=%crate::resolver::log_error(&e),"canary claim failed")
                }
                _ => {}
            }
        }
        let sources = resolver.queue_sources();
        match claim_sources(pool, &sources, &self.cfg, resolver.batch_size()).await {
            Ok(jobs) if !jobs.is_empty() => jobs,
            Ok(_) => {
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
                vec![]
            }
            Err(e) => {
                tracing::error!(error=%crate::resolver::log_error(&e),"queue claim failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                vec![]
            }
        }
    }
    /// Run a claimed batch, or put it back when its first job's author is over budget. `slot`
    /// is the pacing slot this worker took for it.
    async fn finish(&self, jobs: &[Job], slot: Instant) {
        let (pool, resolver) = (&self.pool, self.resolver.as_ref());
        let result = async {
            let refused = if resolver.name() == "geo" {
                None
            } else {
                let rate = self.cfg.author_requests_per_second;
                self.authors.admit_or_spread(pool, &jobs[0], rate).await?
            };
            let Some(spread) = refused else {
                let manifest = resolver.manifest_for(&jobs[0].uri);
                let run = run_batch_config(pool, resolver, &self.cfg, &self.refresh, jobs);
                let (result, _) = crate::telemetry::observe(&self.metrics, &manifest, run).await;
                return result;
            };
            for job in jobs.iter().skip(1) {
                sqlx::query("UPDATE resource SET lease_until=NULL,next_attempt_at=now()+interval '1 second' WHERE id=$1 AND generation=$2").bind(&job.id).bind(job.generation).execute(pool).await?;
            }
            // The refusal fetched nothing. When it put the author's other links off, the next
            // claim finds another author's link: it need not wait for the pacing slot, which
            // is given back unless another worker has taken the next one since.
            if spread > 0 {
                let mut next = self.rate.lock().await;
                if *next == slot {
                    *next = Instant::now();
                }
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::error!(error=%crate::resolver::log_error(&e),"queue persistence failed");
        }
    }
}

/// The seconds to a card's next fetch when it changes by itself at `by` (an incident expires, an
/// auction ends): no later than then, and at least a minute away. A card otherwise final (`0`)
/// is fetched then too.
fn refresh_by(
    ttl: u64,
    by: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> u64 {
    let Some(by) = by else { return ttl };
    let seconds = (by - now).num_seconds().max(60) as u64;
    if ttl == 0 {
        seconds
    } else {
        ttl.min(seconds)
    }
}
#[cfg(test)]
mod refresh_by_tests {
    use super::refresh_by;
    use chrono::{Duration, Utc};

    #[test]
    fn a_card_that_changes_by_itself_is_fetched_again_then() {
        let now = Utc::now();
        let week = 7 * 86400;
        // An incident expiring in two hours, on a card refreshed weekly: in two hours.
        assert_eq!(refresh_by(week, Some(now + Duration::hours(2)), now), 7200);
        // A deadline after the next fetch changes nothing; none at all neither.
        assert_eq!(refresh_by(3600, Some(now + Duration::days(3)), now), 3600);
        assert_eq!(refresh_by(week, None, now), week);
        // A deadline passed or a moment away: in a minute. A final card is fetched then too.
        assert_eq!(refresh_by(week, Some(now - Duration::hours(1)), now), 60);
        assert_eq!(refresh_by(0, Some(now + Duration::hours(1)), now), 3600);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceMeta;

    /// Links whose steps after the save fail, standing in for a database error there.
    static FAILS_AFTER_SAVE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    pub(super) fn maybe_fail_after_save(uri: &str) -> Result<()> {
        let fails = FAILS_AFTER_SAVE.lock().unwrap();
        anyhow::ensure!(!fails.iter().any(|u| u == uri), "the alias step failed");
        Ok(())
    }

    /// A web resolver that answers a page after `0` (never, within a test, when it is long).
    struct After(Duration);
    #[async_trait::async_trait]
    impl Resolver for After {
        fn name(&self) -> &'static str {
            "web"
        }
        fn matches(&self, _: &str) -> bool {
            true
        }
        async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
            tokio::time::sleep(self.0).await;
            let mut doc = ResourceMeta::new(uri, "web", 100);
            doc.name = Some("A page".into());
            Ok(doc)
        }
        fn refresh_after(&self, _: &ResourceMeta) -> Duration {
            Duration::from_secs(100)
        }
    }
    async fn claimed(pool: &PgPool, uri: &str) -> Job {
        let id = crate::store::ensure_resource(pool, uri).await.unwrap();
        sqlx::query("UPDATE resource SET orphaned_at=NULL WHERE id=$1")
            .bind(&id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO reference(resource_id,kind,author,label,object_path,created_at,unit_key) VALUES($1,'tag',$2,'t','',now(),'tag:t')")
            .bind(&id)
            .bind("y".repeat(52))
            .execute(pool)
            .await
            .unwrap();
        let job = claim(pool, "web", &QueueConfig::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.id, id);
        job
    }
    async fn row(pool: &PgPool, id: &str) -> (String, i32, i32, bool, Option<String>) {
        sqlx::query_as(
            "SELECT status,attempts,timeout_streak,dead_letter,last_error FROM resource WHERE id=$1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The daemon's path (a batch): a job that passes its deadline is a timeout, which the first
    /// `timeout_grace` times charges no attempt, whatever the resolver.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_batch_past_its_deadline_is_a_timeout_not_a_parse_failure(pool: PgPool) {
        let cfg = QueueConfig::default();
        let job = claimed(&pool, "https://slow.example.org/page").await;
        let slow = After(Duration::from_secs(3600));
        run_batch_within(
            &pool,
            &slow,
            &cfg,
            &Default::default(),
            std::slice::from_ref(&job),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(
            row(&pool, &job.id).await,
            ("pending".into(), 0, 1, false, Some("timeout".into()))
        );
    }

    /// A failure in what follows the save (the alias a page names) leaves the saved card as it
    /// is: `ok`, with no attempt charged.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_failure_after_the_save_leaves_the_saved_card(pool: PgPool) {
        let cfg = QueueConfig::default();
        let uri = "https://after-save.example.org/page";
        let job = claimed(&pool, uri).await;
        FAILS_AFTER_SAVE.lock().unwrap().push(uri.into());
        sqlx::query(
            "INSERT INTO event_fetch(resource_id,validators) VALUES($1,'{\"etag\":\"x\"}')",
        )
        .bind(&job.id)
        .execute(&pool)
        .await
        .unwrap();
        run_batch(
            &pool,
            &After(Duration::ZERO),
            &cfg,
            std::slice::from_ref(&job),
        )
        .await
        .unwrap();
        assert_eq!(row(&pool, &job.id).await, ("ok".into(), 0, 0, false, None));
        // The link is read again in full (its validators are gone) within `max_backoff_secs`,
        // so the step runs again.
        let (again, validators): (f64, serde_json::Value) = sqlx::query_as(
            "SELECT extract(epoch FROM r.next_attempt_at-now())::float8,e.validators FROM resource r JOIN event_fetch e ON e.resource_id=r.id WHERE r.id=$1",
        )
        .bind(&job.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            again > 0.0 && again <= cfg.max_backoff_secs as f64 + 1.0,
            "{again}"
        );
        assert_eq!(validators, serde_json::json!({}));
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM resource WHERE id=$1")
            .bind(&job.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some("A page"));
    }
}
