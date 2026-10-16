use crate::{
    config::{Config, QueueConfig},
    model::Status,
    resolver::{
        egress::Egress, geo::GeoResolver, osm::OsmResolver, pubky::PubkyResolver, web::WebResolver,
        Resolver,
    },
};
use anyhow::{bail, Result};
use futures_util::FutureExt;
use sqlx::{FromRow, PgPool};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{watch, Mutex, Notify},
    task::JoinSet,
    time::Instant,
};

#[derive(Debug, FromRow)]
pub struct Job {
    pub id: String,
    pub uri: String,
    pub attempts: i32,
    pub generation: i64,
}
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
          AND (lease_until IS NULL OR lease_until<now()) ORDER BY priority_at DESC NULLS LAST,next_attempt_at,id FOR UPDATE SKIP LOCKED LIMIT $3)
        RETURNING id,uri,attempts,generation"#).bind(sources).bind(cfg.lease_secs.max(15 * size as u64 + 10) as f64).bind(size as i64).fetch_all(pool).await?)
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
    let attempt = job.attempts as u32 + 1;
    let message = error;
    sqlx::query(r#"UPDATE resource SET attempts=attempts+1, dead_letter=$3, status='error', lease_until=NULL,priority_at=NULL,
        next_attempt_at=now()+$4*interval '1 second', last_error=$5 WHERE id=$1 AND generation=$2"#)
        .bind(&job.id).bind(job.generation).bind(attempt>=cfg.max_failures).bind(backoff(cfg,attempt) as f64).bind(message).execute(pool).await?;
    Ok(())
}
pub async fn run_job(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    job: &Job,
) -> Result<()> {
    let result = std::panic::AssertUnwindSafe(tokio::time::timeout(
        Duration::from_secs(15),
        resolver.resolve(&job.uri),
    ))
    .catch_unwind()
    .await
    .map_err(|_| anyhow::anyhow!("resolver panic"))
    .and_then(|r| r.map_err(Into::into))
    .and_then(|r| r);
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
    let result = match result {
        Err(error) if error.is::<crate::resolver::NotModified>() => {
            let value: Option<serde_json::Value> =
                sqlx::query_scalar("SELECT doc FROM resource_document WHERE id=$1")
                    .bind(&job.id)
                    .fetch_optional(pool)
                    .await?;
            value
                .ok_or_else(|| anyhow::anyhow!("304 without an accepted document"))
                .and_then(|v| {
                    serde_json::from_value::<crate::model::ResourceMeta>(v).map_err(Into::into)
                })
                .map(|mut doc| {
                    doc.fetched_at = chrono::Utc::now().timestamp_millis();
                    doc
                })
        }
        other => other,
    };
    let result = async {
        let mut doc = result?;
        if doc.status == Status::Error {
            bail!("upstream returned a transient HTTP error");
        }
        if resolver.name() != "osm" {
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
            "web" | "event" | "event_platform" | "pubky" | "geo" | "osm"
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
        let attrs = match resolver.name() {
            "web" | "event" | "event_platform" => crate::resolver::web::ATTRIBUTES,
            "pubky" => crate::resolver::pubky::ATTRIBUTES,
            "geo" => crate::resolver::geo::ATTRIBUTES,
            "osm" => crate::resolver::osm::ATTRIBUTES,
            _ => &[],
        };
        doc.validate(attrs)?;
        crate::changes::save(pool, &job.id, job.generation, &doc, cfg).await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(resource_id=%job.id,resolver=resolver.name(),error=%error,"resolution failed");
        if let Some(retry) = error.downcast_ref::<crate::resolver::RetryLater>() {
            sqlx::query("UPDATE resource SET lease_until=NULL,priority_at=NULL,next_attempt_at=now()+$3*interval '1 second',last_error=$4 WHERE id=$1 AND generation=$2")
                .bind(&job.id).bind(job.generation).bind(retry.seconds.clamp(1,3600) as f64).bind(if retry.class=="rate_limited" {"http_4xx"} else {retry.class}).execute(pool).await?;
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
async fn run_batch_config(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    refresh: &crate::config::RefreshConfig,
    jobs: &[Job],
) -> Result<()> {
    let uris = jobs.iter().map(|j| j.uri.clone()).collect::<Vec<_>>();
    let results = tokio::time::timeout(
        Duration::from_secs(15 * jobs.len() as u64),
        std::panic::AssertUnwindSafe(resolver.resolve_batch(&uris)).catch_unwind(),
    )
    .await;
    let results = match results {
        Ok(Ok(results)) if results.len() == jobs.len() => results,
        Err(_) if resolver.name() == "osm" => jobs
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
            .map(|_| Err(anyhow::anyhow!("batch timeout or invalid response count")))
            .collect(),
    };
    if results.iter().any(|r| {
        r.as_ref()
            .is_err_and(|e| !e.is::<crate::resolver::NotModified>())
    }) {
        crate::telemetry::parser_error();
    }
    let error = results.iter().find_map(|r| match r {
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
    if crate::telemetry::last_fetch().is_some() {
        crate::source::outcome(
            pool,
            &resolver.manifest_for(&jobs[0].uri).source,
            error.as_deref(),
        )
        .await?;
    }
    for (job, result) in jobs.iter().zip(results) {
        guarded_finish_job(pool, resolver, cfg, refresh, job, result).await?;
    }
    Ok(())
}
/// Limits new work attributable to a public author, across both resolver kinds.
#[derive(Default)]
pub struct AuthorBudget(Mutex<HashMap<String, Instant>>);
impl AuthorBudget {
    pub async fn admit(&self, pool: &PgPool, job: &Job, rate: f64) -> Result<bool> {
        let authors: Vec<String> = sqlx::query_scalar(
            "WITH inputs AS (SELECT $1::text AS id UNION SELECT alias_id FROM cursor_alias WHERE root_id=$1)
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
            return Ok(true);
        }
        let now = Instant::now();
        let mut budgets = self.0.lock().await;
        budgets.retain(|_, next| *next > now);
        // Any referring author can sponsor the shared fetch; never charge it once per reference.
        let author = authors.iter().find(|a| !budgets.contains_key(*a));
        if let Some(author) = author {
            budgets.insert(author.clone(), now + Duration::from_secs_f64(1.0 / rate));
            return Ok(true);
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
        Ok(false)
    }
}

pub fn spawn(
    pool: PgPool,
    config: Config,
    wake: Arc<Notify>,
    shutdown: watch::Receiver<bool>,
    metrics: Arc<crate::telemetry::Metrics>,
) -> Result<JoinSet<()>> {
    let mut workers = JoinSet::new();
    let authors = Arc::new(AuthorBudget::default());
    let mut resolvers: Vec<(Arc<dyn Resolver>, crate::config::ResolverConfig)> = vec![];
    let egress = Egress::new(config.egress.clone());
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
                    egress: egress.clone(),
                    config: config.web.clone(),
                    events: Some(events),
                },
            }),
            config.web.clone(),
        ));
    }
    if config.pubky.enabled {
        resolvers.push((
            Arc::new(PubkyResolver::new(config.pubky.clone())?),
            config.pubky.clone(),
        ));
    }
    if config.geo_enabled {
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
            },
        ));
    }
    for (resolver, budget) in resolvers {
        let rate = Arc::new(Mutex::new(Instant::now()));
        for _ in 0..budget.concurrency {
            let authors = authors.clone();
            let metrics = metrics.clone();
            let refresh = config.refresh.clone();
            let canary = config.canary.clone();
            let (pool, resolver, cfg, wake, mut shutdown, rate) = (
                pool.clone(),
                resolver.clone(),
                config.queue.clone(),
                wake.clone(),
                shutdown.clone(),
                rate.clone(),
            );
            workers.spawn(async move {
                let mut canary_check=Instant::now();
                loop {
                    tokio::select! {
                        _=shutdown.changed()=>break,
                        _=async {
                            { let mut next=rate.lock().await; tokio::time::sleep_until(*next).await; *next=Instant::now()+Duration::from_secs_f64(1.0/budget.requests_per_second); }
                            if Instant::now()>=canary_check {
                                canary_check=Instant::now()+Duration::from_secs(60);
                            for manifest in resolver.manifests() {
                            if resolver.canary_allowed_for(&manifest) {
                            match crate::canary::claim(&pool,&manifest,&canary).await {
                                Ok(Some(generation))=>{if let Err(e)=crate::canary::run_manifest(&pool,resolver.as_ref(),&manifest,&canary,&metrics,generation).await {tracing::warn!(error=%e,"canary bookkeeping failed");}return;},
                                Err(e)=>tracing::warn!(error=%e,"canary claim failed"),
                                _=>{}
                            }
                            } else if let Err(e)=crate::canary::suspend(&pool,&manifest).await {tracing::warn!(error=%e,"canary suspension failed");}
                            }
                            }
                            match claim_sources(&pool,&resolver.queue_sources(),&cfg,resolver.batch_size()).await {
                                Ok(jobs) if !jobs.is_empty()=>{
                                    let result=async {
                                        if resolver.name()=="geo" || authors.admit(&pool,&jobs[0],cfg.author_requests_per_second).await? {
                                            let (result,_)=crate::telemetry::observe(&metrics,&resolver.manifest_for(&jobs[0].uri),run_batch_config(&pool,resolver.as_ref(),&cfg,&refresh,&jobs)).await;
                                            result?;
                                        } else {
                                            for job in jobs.iter().skip(1) {
                                                sqlx::query("UPDATE resource SET lease_until=NULL,next_attempt_at=now()+interval '1 second' WHERE id=$1 AND generation=$2").bind(&job.id).bind(job.generation).execute(&pool).await?;
                                            }
                                        }
                                        Ok::<_,anyhow::Error>(())
                                    }.await;
                                    if let Err(e)=result { tracing::error!(error=%e,"queue persistence failed"); }
                                },
                                Ok(_)=>{ tokio::select! { _=wake.notified()=>{},_=tokio::time::sleep(Duration::from_millis(250))=>{} } },
                                Err(e)=>{ tracing::error!(error=%e,"queue claim failed"); tokio::time::sleep(Duration::from_secs(1)).await; }
                            }
                        }=>{}
                    }
                }
            });
        }
    }
    Ok(workers)
}
