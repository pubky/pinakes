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

/// Claim `manifest`'s canary when it is due and no worker holds it: its generation.
pub async fn claim(
    pool: &PgPool,
    manifest: &Manifest,
    config: &CanaryConfig,
) -> Result<Option<i64>> {
    Ok(claim_due(pool, std::slice::from_ref(manifest), config)
        .await?
        .map(|(_, generation)| generation))
}
/// Claim one canary among `manifests`: the first, in their order, that is due and that no
/// worker holds, with its generation. The claim holds the source for 60 seconds, so no other
/// worker runs it meanwhile. A source seen for the first time is due one interval from now.
/// Two statements whatever the number of manifests; none when canaries are off or no manifest
/// has one.
pub async fn claim_due<'a>(
    pool: &PgPool,
    manifests: &'a [Manifest],
    config: &CanaryConfig,
) -> Result<Option<(&'a Manifest, i64)>> {
    if !config.enabled {
        return Ok(None);
    }
    let sources: Vec<&str> = manifests
        .iter()
        .filter(|m| m.canary.is_some())
        .map(|m| m.source.as_str())
        .collect();
    if sources.is_empty() {
        return Ok(None);
    }
    // Only a source without a due time is written, in source order (two workers seed the same
    // rows).
    sqlx::query("INSERT INTO source_state(source,canary_due_at) SELECT wanted.source,now()+$2*interval '1 second' FROM (SELECT DISTINCT unnest($1::text[]) AS source) wanted WHERE NOT EXISTS(SELECT 1 FROM source_state s WHERE s.source=wanted.source AND s.canary_due_at IS NOT NULL) ORDER BY wanted.source ON CONFLICT(source) DO UPDATE SET canary_due_at=excluded.canary_due_at WHERE source_state.canary_due_at IS NULL")
        .bind(&sources).bind(config.interval_secs as f64).execute(pool).await?;
    // A row another worker is claiming now is passed over for the next one that is due.
    let claimed: Option<(String, i64)> = sqlx::query_as("UPDATE source_state SET canary_lease_until=now()+interval '60 seconds',canary_generation=canary_generation+1 WHERE source=(SELECT source FROM source_state WHERE source=ANY($1) AND canary_due_at<=now() AND (canary_lease_until IS NULL OR canary_lease_until<now()) ORDER BY array_position($1::text[],source) LIMIT 1 FOR UPDATE SKIP LOCKED) AND canary_due_at<=now() AND (canary_lease_until IS NULL OR canary_lease_until<now()) RETURNING source,canary_generation")
        .bind(&sources).fetch_optional(pool).await?;
    Ok(claimed.and_then(|(source, generation)| {
        manifests
            .iter()
            .find(|m| m.canary.is_some() && m.source == source)
            .map(|m| (m, generation))
    }))
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
        let class = match &result {
            Err(e) => Some(crate::resolver::error_class(e)),
            Ok(doc) if doc.status == Status::Unreachable => Some("http_4xx"),
            Ok(doc) if doc.status == Status::Error => Some("http_5xx"),
            _ => None,
        };
        // The source's health is bookkeeping: when it cannot be written, the canary's own
        // result is still recorded.
        if crate::source::outcome(pool, &manifest.source, class)
            .await
            .is_err()
        {
            tracing::warn!(
                source = %manifest.source,
                class = class.unwrap_or("ok"),
                host = manifest.canary.as_ref().and_then(|u| u.host_str()).unwrap_or_default(),
                "source health not recorded"
            );
        }
    }
    record(pool, manifest, generation, &result, config.interval_secs).await
}

/// Policy-disabled probes do not leave a stale failing gauge behind after reconfiguration: the
/// canary results of `sources` are cleared, in one statement (none without sources). Rows are
/// locked in source order, so two workers clearing the same sources never deadlock.
pub async fn suspend(pool: &PgPool, sources: &[&str]) -> Result<()> {
    if sources.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE source_state SET canary_ok=NULL,canary_missing='{}',canary_failures=0,canary_lease_until=NULL WHERE source IN (SELECT source FROM source_state WHERE source=ANY($1) AND (canary_ok IS NOT NULL OR canary_failures<>0) ORDER BY source FOR UPDATE) AND (canary_ok IS NOT NULL OR canary_failures<>0)").bind(sources).execute(pool).await?;
    Ok(())
}
