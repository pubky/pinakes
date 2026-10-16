use crate::{
    config::{Config, QueueConfig},
    model::Status,
    resolver::{
        egress::Egress, geo::GeoResolver, osm::OsmResolver, pubky::PubkyResolver, web::WebResolver,
        Resolver,
    },
};
use anyhow::{bail, Result};
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
    let size = size.clamp(1, 50);
    Ok(sqlx::query_as(r#"UPDATE resource SET lease_until=now()+$2*interval '1 second', generation=generation+1
        WHERE id IN (SELECT id FROM resource WHERE resolver=$1 AND NOT dead_letter AND NOT final AND next_attempt_at<=now() AND NOT EXISTS(SELECT 1 FROM source_state WHERE source=$1 AND held_until>now())
          AND (lease_until IS NULL OR lease_until<now()) ORDER BY priority_at DESC NULLS LAST,next_attempt_at,id FOR UPDATE SKIP LOCKED LIMIT $3)
        RETURNING id,uri,attempts,generation"#).bind(resolver).bind(cfg.lease_secs.max(15 * size as u64 + 10) as f64).bind(size as i64).fetch_all(pool).await?)
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
    let mut message = error.to_string();
    crate::model::truncate(&mut message, 512);
    sqlx::query(r#"UPDATE resource SET attempts=attempts+1, dead_letter=$3, status='error', lease_until=NULL,
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
    let result = tokio::time::timeout(Duration::from_secs(15), resolver.resolve(&job.uri))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
    finish_job(
        pool,
        resolver,
        cfg,
        &crate::config::RefreshConfig::default(),
        job,
        result,
    )
    .await
}
async fn finish_job(
    pool: &PgPool,
    resolver: &dyn Resolver,
    cfg: &QueueConfig,
    refresh: &crate::config::RefreshConfig,
    job: &Job,
    result: Result<crate::model::ResourceMeta>,
) -> Result<()> {
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
        if matches!(resolver.name(), "web" | "pubky" | "geo" | "osm") {
            let now = chrono::Utc::now();
            let mut manifest = resolver.manifest();
            if resolver.name() == "osm" {
                manifest.cadence_floor = doc.ttl_secs;
            }
            if doc.status != Status::Missing
                || doc
                    .end
                    .or(doc.start)
                    .is_some_and(|end| now >= end + chrono::Duration::hours(24))
            {
                doc.ttl_secs = crate::scheduler::next(&manifest, &doc, now, refresh)
                    .map_or(0, |at| (at - now).num_seconds().max(0) as u64);
            } else if resolver.name() != "osm" {
                doc.ttl_secs = doc.ttl_secs.max(3600);
            }
        }
        let attrs = match resolver.name() {
            "web" => crate::resolver::web::ATTRIBUTES,
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
        failed(pool, job, cfg, &error.to_string()).await?;
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
        resolver.resolve_batch(&uris),
    )
    .await;
    let results = match results {
        Ok(results) if results.len() == jobs.len() => results,
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
        Err(e) if !e.is::<crate::resolver::NotModified>() => Some(e.to_string()),
        Ok(doc) if matches!(doc.status, Status::Unreachable | Status::Error) => {
            Some(doc.status.as_str().into())
        }
        _ => None,
    });
    if crate::telemetry::last_fetch().is_some() {
        crate::source::outcome(pool, resolver.name(), error.as_deref()).await?;
    }
    for (job, result) in jobs.iter().zip(results) {
        finish_job(pool, resolver, cfg, refresh, job, result).await?;
    }
    Ok(())
}
/// Limits new work attributable to a public author, across both resolver kinds.
#[derive(Default)]
pub struct AuthorBudget(Mutex<HashMap<String, Instant>>);
impl AuthorBudget {
    pub async fn admit(&self, pool: &PgPool, job: &Job, rate: f64) -> Result<bool> {
        let authors: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT author FROM reference WHERE resource_id=$1 ORDER BY author",
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
    if config.web.enabled {
        resolvers.push((
            Arc::new(WebResolver {
                egress: Egress::new(config.egress.clone()),
                config: config.web.clone(),
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
            Arc::new(OsmResolver::new(pool.clone(), config.osm.clone())?),
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
                loop {
                    tokio::select! {
                        _=shutdown.changed()=>break,
                        _=async {
                            { let mut next=rate.lock().await; tokio::time::sleep_until(*next).await; *next=Instant::now()+Duration::from_secs_f64(1.0/budget.requests_per_second); }
                            match crate::canary::claim(&pool,&resolver.manifest(),&canary).await {
                                Ok(Some(generation))=>{if let Err(e)=crate::canary::run(&pool,resolver.as_ref(),&canary,&metrics,generation).await {tracing::warn!(error=%e,"canary bookkeeping failed");}return;},
                                Err(e)=>tracing::warn!(error=%e,"canary claim failed"),
                                _=>{}
                            }
                            match claim_batch(&pool,resolver.name(),&cfg,resolver.batch_size()).await {
                                Ok(jobs) if !jobs.is_empty()=>{
                                    let result=async {
                                        if resolver.name()=="geo" || authors.admit(&pool,&jobs[0],cfg.author_requests_per_second).await? {
                                            let (result,_)=crate::telemetry::observe(&metrics,&resolver.manifest(),run_batch_config(&pool,resolver.as_ref(),&cfg,&refresh,&jobs)).await;
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
