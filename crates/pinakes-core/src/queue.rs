use crate::{
    config::{Config, QueueConfig},
    model::Status,
    resolver::{egress::Egress, pubky::PubkyResolver, web::WebResolver, Resolver},
    store,
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
    Ok(sqlx::query_as(r#"UPDATE resource SET lease_until=now()+$2*interval '1 second', generation=generation+1
        WHERE id=(SELECT id FROM resource WHERE resolver=$1 AND NOT dead_letter AND next_attempt_at<=now()
          AND (lease_until IS NULL OR lease_until<now()) ORDER BY next_attempt_at,id FOR UPDATE SKIP LOCKED LIMIT 1)
        RETURNING id,uri,attempts,generation"#).bind(resolver).bind(cfg.lease_secs as f64).fetch_optional(pool).await?)
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
    let resolved = tokio::time::timeout(Duration::from_secs(15), resolver.resolve(&job.uri)).await;
    let result = async {
        let mut doc = resolved??;
        if doc.status == Status::Error {
            bail!("upstream returned a transient HTTP error");
        }
        doc.ttl_secs = if doc.status == Status::Missing {
            cfg.missing_ttl_secs
        } else {
            resolver.refresh_after(&doc).as_secs()
        };
        let attrs = match resolver.name() {
            "web" => crate::resolver::web::ATTRIBUTES,
            "pubky" => crate::resolver::pubky::ATTRIBUTES,
            _ => &[],
        };
        doc.validate(attrs)?;
        store::save_document(pool, &job.id, job.generation, &doc).await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(resource_id=%job.id,resolver=resolver.name(),error=%error,"resolution failed");
        failed(pool, job, cfg, &error.to_string()).await?;
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
    for (resolver, budget) in resolvers {
        let rate = Arc::new(Mutex::new(Instant::now()));
        for _ in 0..budget.concurrency {
            let authors = authors.clone();
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
                            match claim(&pool,resolver.name(),&cfg).await {
                                Ok(Some(job))=>{
                                    let result=async {
                                        if authors.admit(&pool,&job,cfg.author_requests_per_second).await? {
                                            run_job(&pool,resolver.as_ref(),&cfg,&job).await?;
                                        }
                                        Ok::<_,anyhow::Error>(())
                                    }.await;
                                    if let Err(e)=result { tracing::error!(error=%e,"queue persistence failed"); }
                                },
                                Ok(None)=>{ tokio::select! { _=wake.notified()=>{},_=tokio::time::sleep(Duration::from_millis(250))=>{} } },
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
