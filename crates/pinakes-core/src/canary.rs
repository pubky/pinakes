use crate::{
    adapter::Manifest,
    config::CanaryConfig,
    model::{ResourceMeta, Status},
    resolver::Resolver,
    telemetry::Metrics,
};
use anyhow::Result;
use sqlx::PgPool;
use std::time::Duration;

pub async fn claim(
    pool: &PgPool,
    manifest: &Manifest,
    config: &CanaryConfig,
) -> Result<Option<i64>> {
    if !config.enabled || manifest.canary.is_none() {
        return Ok(None);
    }
    sqlx::query("INSERT INTO source_state(source,canary_due_at) VALUES($1,now()+$2*interval '1 second') ON CONFLICT(source) DO UPDATE SET canary_due_at=excluded.canary_due_at WHERE source_state.canary_due_at IS NULL")
        .bind(&manifest.source).bind(config.interval_secs as f64).execute(pool).await?;
    Ok(sqlx::query_scalar("UPDATE source_state SET canary_lease_until=now()+interval '60 seconds',canary_generation=canary_generation+1 WHERE source=$1 AND canary_due_at<=now() AND (canary_lease_until IS NULL OR canary_lease_until<now()) RETURNING canary_generation").bind(&manifest.source).fetch_optional(pool).await?)
}
pub async fn record(
    pool: &PgPool,
    manifest: &Manifest,
    generation: i64,
    result: &Result<ResourceMeta>,
    interval: u64,
) -> Result<()> {
    if result
        .as_ref()
        .err()
        .is_some_and(crate::resolver::egress::over_cap)
    {
        // An over-budget sample is inconclusive. Keep the size policy and retry the
        // next moving-window sample, without claiming either health or an outage.
        sqlx::query("UPDATE source_state SET canary_at=now(),canary_ok=NULL,canary_missing='{}',canary_failures=0,canary_due_at=now()+$3*interval '1 second',canary_lease_until=NULL WHERE source=$1 AND canary_generation=$2")
            .bind(&manifest.source).bind(generation).bind(interval as f64).execute(pool).await?;
        return Ok(());
    }
    let missing = result
        .as_ref()
        .map(|doc| manifest.missing_fields(doc))
        .unwrap_or_else(|_| manifest.expected_fields.clone());
    let ok = result
        .as_ref()
        .is_ok_and(|doc| doc.status == Status::Ok && missing.is_empty());
    sqlx::query("UPDATE source_state SET canary_at=now(),canary_ok=$3,canary_missing=$4,canary_failures=CASE WHEN $3 THEN 0 ELSE canary_failures+1 END,canary_due_at=now()+$5*interval '1 second',canary_lease_until=NULL WHERE source=$1 AND canary_generation=$2")
        .bind(&manifest.source).bind(generation).bind(ok).bind(missing).bind(interval as f64).execute(pool).await?;
    Ok(())
}
pub async fn run(
    pool: &PgPool,
    resolver: &dyn Resolver,
    config: &CanaryConfig,
    metrics: &Metrics,
    generation: i64,
) -> Result<()> {
    let manifest = resolver.manifest();
    run_manifest(pool, resolver, &manifest, config, metrics, generation).await
}
pub async fn run_manifest(
    pool: &PgPool,
    resolver: &dyn Resolver,
    manifest: &Manifest,
    config: &CanaryConfig,
    metrics: &Metrics,
    generation: i64,
) -> Result<()> {
    let (result, observations) = crate::telemetry::observe(metrics, manifest, async {
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            let target = resolver.canary_target(manifest).await?;
            resolver.probe_manifest(&target, manifest).await
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
        // An over-budget sample is inconclusive (`record`), never a parse failure.
        if result
            .as_ref()
            .err()
            .is_some_and(|e| !crate::resolver::egress::over_cap(e))
        {
            crate::telemetry::parser_error();
        }
        result
    })
    .await;
    let over_cap = result
        .as_ref()
        .err()
        .is_some_and(crate::resolver::egress::over_cap);
    if !observations.is_empty() && !over_cap {
        crate::source::outcome(
            pool,
            &manifest.source,
            match &result {
                Err(e) => Some(crate::resolver::error_class(e).to_owned()),
                Ok(doc) if matches!(doc.status, Status::Unreachable | Status::Error) => Some(
                    if doc.status == Status::Unreachable {
                        "http_4xx"
                    } else {
                        "http_5xx"
                    }
                    .into(),
                ),
                _ => None,
            }
            .as_deref(),
        )
        .await?;
    }
    record(pool, manifest, generation, &result, config.interval_secs).await
}

/// Policy-disabled probes do not leave a stale failing gauge behind after reconfiguration.
pub async fn suspend(pool: &PgPool, manifest: &Manifest) -> Result<()> {
    sqlx::query("UPDATE source_state SET canary_ok=NULL,canary_missing='{}',canary_failures=0,canary_lease_until=NULL WHERE source=$1 AND (canary_ok IS NOT NULL OR canary_failures<>0)").bind(&manifest.source).execute(pool).await?;
    Ok(())
}
