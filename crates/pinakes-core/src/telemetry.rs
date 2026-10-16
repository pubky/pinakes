use crate::{
    adapter::Manifest,
    model::{ResourceMeta, Status},
};
use anyhow::Result;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};
use sqlx::{PgPool, Row};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Instant,
};

pub const OUTCOMES: &[&str] = &[
    "ok",
    "not_modified",
    "missing",
    "unreachable",
    "blocked",
    "rate_limited",
    "error",
    "drift",
];
#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    refresh_lock: Arc<tokio::sync::Mutex<Option<Instant>>>,
    pub fetch: IntCounterVec,
    pub discovery: IntCounterVec,
    pub duration: HistogramVec,
    pub changes: IntCounterVec,
    pub held: IntCounterVec,
    pub resets: IntCounterVec,
    pub success: IntGaugeVec,
    pub canary_ok: IntGaugeVec,
    pub canary_missing: IntGaugeVec,
    pub canary_failures: IntGaugeVec,
    pub queue: IntGaugeVec,
    pub overdue: IntGaugeVec,
    pub resources: IntGaugeVec,
    pub blocked: IntGaugeVec,
    blocked_hosts: usize,
    pub group_url_demotions: IntCounter,
    pub group_pair_budget: IntCounter,
    pub ingest_lag: IntGauge,
    pub ingest_success: IntGauge,
    pub ingest_resets: IntCounter,
    pub ingest_rejected: IntCounterVec,
}
impl Metrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();
        macro_rules! metric {
            ($kind:ident,$name:literal,$help:literal,$labels:expr) => {{
                let value = $kind::new(Opts::new($name, $help), $labels)?;
                registry.register(Box::new(value.clone()))?;
                value
            }};
        }
        let duration = HistogramVec::new(
            HistogramOpts::new(
                "pinakes_fetch_duration_seconds",
                "Upstream request duration including response body",
            ),
            &["source"],
        )?;
        registry.register(Box::new(duration.clone()))?;
        let ingest_lag = IntGauge::new("pinakes_ingest_lag_lines", "Observed Nexus cursor lag")?;
        registry.register(Box::new(ingest_lag.clone()))?;
        let ingest_success = IntGauge::new(
            "pinakes_ingest_last_success_timestamp_seconds",
            "Last successful Nexus projection update",
        )?;
        registry.register(Box::new(ingest_success.clone()))?;
        let ingest_resets = IntCounter::new(
            "pinakes_ingest_epoch_resets_total",
            "Nexus projection resets",
        )?;
        registry.register(Box::new(ingest_resets.clone()))?;
        let group_url_demotions = IntCounter::new(
            "pinakes_group_url_demotions_total",
            "Shared URL identity keys demoted by publisher",
        )?;
        registry.register(Box::new(group_url_demotions.clone()))?;
        let group_pair_budget = IntCounter::new(
            "pinakes_group_pair_budget_total",
            "Grouping plans committed with bounded partial evidence",
        )?;
        registry.register(Box::new(group_pair_budget.clone()))?;
        let out = Self {
            group_pair_budget,
            group_url_demotions,
            ingest_rejected: metric!(
                IntCounterVec,
                "pinakes_ingest_rejected_total",
                "Rejected feed lines in committed batches by bounded reason",
                &["reason"]
            ),
            discovery: metric!(
                IntCounterVec,
                "pinakes_discovery_decisions_total",
                "Optional metadata discovery decisions (requests may use cache)",
                &["kind", "decision"]
            ),
            fetch: metric!(
                IntCounterVec,
                "pinakes_fetch_total",
                "Upstream requests by parser outcome",
                &["source", "mode", "outcome"]
            ),
            duration,
            changes: metric!(
                IntCounterVec,
                "pinakes_changes_total",
                "Accepted document changes",
                &["source", "class"]
            ),
            held: metric!(
                IntCounterVec,
                "pinakes_feed_held_total",
                "Quarantined container snapshots",
                &["source"]
            ),
            resets: metric!(
                IntCounterVec,
                "pinakes_source_cursor_resets_total",
                "Upstream cursor resets",
                &["source"]
            ),
            success: metric!(
                IntGaugeVec,
                "pinakes_source_last_success_timestamp_seconds",
                "Last successful source fetch",
                &["source"]
            ),
            canary_ok: metric!(
                IntGaugeVec,
                "pinakes_canary_ok",
                "Most recent canary result",
                &["source"]
            ),
            canary_missing: metric!(
                IntGaugeVec,
                "pinakes_canary_fields_missing",
                "Expected fields missing in latest canary",
                &["source"]
            ),
            canary_failures: metric!(
                IntGaugeVec,
                "pinakes_canary_consecutive_failures",
                "Consecutive failed canary runs",
                &["source"]
            ),
            queue: metric!(
                IntGaugeVec,
                "pinakes_queue_depth",
                "Ready, leased and dead resource jobs",
                &["resolver", "state"]
            ),
            overdue: metric!(
                IntGaugeVec,
                "pinakes_refresh_overdue",
                "Resources past refresh time",
                &["resolver"]
            ),
            blocked: metric!(
                IntGaugeVec,
                "pinakes_blocked_resources",
                "Resources blocked by upstream access walls, by registrable domain (top N, then other)",
                &["host"]
            ),
            blocked_hosts: 20,
            resources: metric!(
                IntGaugeVec,
                "pinakes_resources",
                "Resources by resolver and retrieval status",
                &["resolver", "status"]
            ),
            registry,
            refresh_lock: Arc::default(),
            ingest_lag,
            ingest_success,
            ingest_resets,
        };
        for reason in crate::ingest::REJECT_REASONS {
            out.ingest_rejected.with_label_values(&[reason]);
        }
        for manifest in crate::adapter::manifests() {
            for outcome in OUTCOMES {
                out.fetch
                    .with_label_values(&[&manifest.source, manifest.mode.as_str(), outcome]);
            }
            out.duration.with_label_values(&[&manifest.source]);
            for class in crate::changes::CLASSES {
                out.changes
                    .with_label_values(&[manifest.source.as_str(), *class]);
            }
            out.held.with_label_values(&[&manifest.source]);
            out.resets.with_label_values(&[&manifest.source]);
            out.success.with_label_values(&[&manifest.source]);
        }
        Ok(out)
    }
    pub async fn refresh(&self, pool: &PgPool) -> Result<()> {
        let mut refreshed = self.refresh_lock.lock().await;
        if refreshed.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(15)) {
            return Ok(());
        }
        let row=sqlx::query("SELECT coalesce(sum(greatest(feed_length-cursor,0)),0)::bigint AS lag,coalesce(extract(epoch FROM max(updated_at)),0)::bigint AS success,coalesce(sum(resets),0)::bigint AS resets FROM feed_cursor").fetch_one(pool).await?;
        self.ingest_lag.set(row.try_get("lag")?);
        self.ingest_success.set(row.try_get("success")?);
        sync_counter(&self.ingest_resets, row.try_get("resets")?);
        self.canary_ok.reset();
        self.canary_missing.reset();
        self.canary_failures.reset();
        for source in sqlx::query("SELECT *,coalesce(extract(epoch FROM last_success_at),0)::bigint AS success FROM source_state").fetch_all(pool).await? {
            let name:String=source.try_get("source")?;self.success.with_label_values(&[&name]).set(source.try_get("success")?);
            sync_counter(&self.held.with_label_values(&[&name]),source.try_get("feed_held_total")?);sync_counter(&self.resets.with_label_values(&[&name]),source.try_get("cursor_resets_total")?);
            let changes:serde_json::Value=source.try_get("change_totals")?;for (class,count) in changes.as_object().into_iter().flatten() {sync_counter(&self.changes.with_label_values(&[&name,class]),count.as_i64().unwrap_or(0));}
            if let Some(ok)=source.try_get::<Option<bool>,_>("canary_ok")? {self.canary_ok.with_label_values(&[&name]).set(i64::from(ok));self.canary_missing.with_label_values(&[&name]).set(source.try_get::<Vec<String>,_>("canary_missing")?.len() as i64);self.canary_failures.with_label_values(&[&name]).set(source.try_get::<i32,_>("canary_failures")? as i64);}
        }
        self.queue.reset();
        self.overdue.reset();
        self.resources.reset();
        self.blocked.reset();
        sync_counter(
            &self.group_pair_budget,
            sqlx::query_scalar("SELECT pair_budget_total FROM group_runtime WHERE id")
                .fetch_one(pool)
                .await?,
        );
        sync_counter(
            &self.group_url_demotions,
            sqlx::query_scalar("SELECT count(*) FROM group_demoted_url")
                .fetch_one(pool)
                .await?,
        );
        let rows = sqlx::query_as::<_, (String, i64)>("SELECT coalesce(host,scheme),count(*) FROM resource WHERE status='blocked' GROUP BY coalesce(host,scheme)").fetch_all(pool).await?;
        for (host, count) in blocked_series(rows, self.blocked_hosts) {
            self.blocked.with_label_values(&[&host]).set(count);
        }
        for manifest in crate::adapter::manifests() {
            for state in ["pending", "leased", "dead"] {
                self.queue
                    .with_label_values(&[&manifest.source, state])
                    .set(0);
            }
            self.overdue.with_label_values(&[&manifest.source]).set(0);
            for status in [
                "ok",
                "pending",
                "missing",
                "unreachable",
                "blocked",
                "error",
            ] {
                self.resources
                    .with_label_values(&[&manifest.source, status])
                    .set(0);
            }
        }
        for row in sqlx::query("SELECT resolver,status,count(*) AS n FROM resource WHERE resolver IS NOT NULL GROUP BY resolver,status").fetch_all(pool).await? {self.resources.with_label_values(&[&row.try_get::<String,_>("resolver")?,&row.try_get::<String,_>("status")?]).set(row.try_get("n")?);}
        for row in sqlx::query("SELECT resolver,count(*) FILTER(WHERE dead_letter) AS dead,count(*) FILTER(WHERE NOT dead_letter AND lease_until>now()) AS leased,count(*) FILTER(WHERE NOT dead_letter AND NOT final AND next_attempt_at<=now() AND (lease_until IS NULL OR lease_until<=now())) AS pending,count(*) FILTER(WHERE NOT final AND NOT dead_letter AND refresh_at<now()) AS overdue FROM resource WHERE resolver IS NOT NULL GROUP BY resolver").fetch_all(pool).await? {let resolver:String=row.try_get("resolver")?;for state in ["pending","leased","dead"] {self.queue.with_label_values(&[&resolver,state]).set(row.try_get(state)?);}self.overdue.with_label_values(&[&resolver]).set(row.try_get("overdue")?);}
        *refreshed = Some(Instant::now());
        Ok(())
    }
    pub fn with_blocked_hosts(mut self, n: usize) -> Self {
        self.blocked_hosts = n.max(1);
        self
    }
    pub fn encode(&self) -> Result<String> {
        let mut bytes = Vec::new();
        TextEncoder::new().encode(&self.registry.gather(), &mut bytes)?;
        Ok(String::from_utf8(bytes)?)
    }
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!(self.registry.gather().iter().map(|family|serde_json::json!({"name":family.name(),"samples":family.get_metric().iter().map(|m|serde_json::json!({"labels":m.get_label().iter().map(|l|(l.name(),l.value())).collect::<std::collections::BTreeMap<_,_>>(),"counter":m.get_counter().value(),"gauge":m.get_gauge().value(),"histogram_count":m.get_histogram().sample_count()})).collect::<Vec<_>>()})).collect::<Vec<_>>())
    }
}
fn sync_counter(counter: &IntCounter, value: i64) {
    let target = value.max(0) as u64;
    let old = counter.get();
    if target > old {
        counter.inc_by(target - old);
    }
}
#[derive(Clone, Debug)]
pub struct Observation {
    pub outcome: &'static str,
    pub seconds: f64,
}
tokio::task_local! {static FETCHES: Arc<Mutex<Vec<Observation>>>; static DISCOVERY: IntCounterVec;}
/// Bounded label set: requested hosts fold to registrable domains, then the top `n` by
/// count (ties by name) keep their own series and the remainder sums into `other`.
pub fn blocked_series(rows: Vec<(String, i64)>, n: usize) -> Vec<(String, i64)> {
    let mut domains = std::collections::BTreeMap::<String, i64>::new();
    for (host, count) in rows {
        let domain = psl::domain_str(&host).unwrap_or(&host).to_owned();
        *domains.entry(domain).or_default() += count;
    }
    let mut ranked: Vec<_> = domains.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let rest: i64 = ranked.iter().skip(n).map(|(_, c)| c).sum();
    ranked.truncate(n);
    if rest > 0 {
        ranked.push(("other".into(), rest));
    }
    ranked
}
pub fn discovery(kind: &'static str, decision: &'static str) {
    let _ = DISCOVERY.try_with(|counter| counter.with_label_values(&[kind, decision]).inc());
}
pub struct FetchGuard {
    start: Instant,
    status: Option<u16>,
    sink: Option<Arc<Mutex<Vec<Observation>>>>,
}
impl FetchGuard {
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
            status: None,
            sink: FETCHES.try_with(Clone::clone).ok(),
        }
    }
    pub fn status(&mut self, status: u16) {
        self.status = Some(status);
    }
}
impl Drop for FetchGuard {
    fn drop(&mut self) {
        if let Some(sink) = &self.sink {
            sink.lock().unwrap().push(Observation {
                outcome: match self.status {
                    Some(304) => "not_modified",
                    Some(404 | 410) => "missing",
                    Some(401 | 403 | 451) => "unreachable",
                    Some(429) => "rate_limited",
                    Some(200..=399) => "ok",
                    _ => "error",
                },
                seconds: self.start.elapsed().as_secs_f64(),
            });
        }
    }
}
pub fn last_fetch() -> Option<usize> {
    FETCHES
        .try_with(|v| v.lock().unwrap().len().checked_sub(1))
        .ok()
        .flatten()
}
pub fn document(index: Option<usize>, manifest: &Manifest, doc: &ResourceMeta) {
    if doc.status == Status::Ok
        && (!manifest.missing_fields(doc).is_empty()
            || doc.event.warnings.iter().any(|w| w == "truncated"))
    {
        let _ = FETCHES.try_with(|v| {
            let mut records = v.lock().unwrap();
            if let Some(observation) = index.and_then(|i| records.get_mut(i)) {
                if observation.outcome == "ok" {
                    observation.outcome = "drift";
                }
            }
        });
    }
}
pub fn set_outcome(index: Option<usize>, outcome: &'static str) {
    let _ = FETCHES.try_with(|v| {
        let mut records = v.lock().unwrap();
        if let Some(record) = index.and_then(|i| records.get_mut(i)) {
            record.outcome = outcome;
        }
    });
}
pub fn parser_error() {
    let _ = FETCHES.try_with(|v| {
        if let Some(last) = v.lock().unwrap().last_mut() {
            if last.outcome == "ok" {
                last.outcome = "error";
            }
        }
    });
}
pub async fn observe<F, T>(
    metrics: &Metrics,
    manifest: &Manifest,
    future: F,
) -> (T, Vec<Observation>)
where
    F: Future<Output = T>,
{
    let sink = Arc::new(Mutex::new(Vec::new()));
    let result = DISCOVERY
        .scope(
            metrics.discovery.clone(),
            FETCHES.scope(sink.clone(), future),
        )
        .await;
    let observations = sink.lock().unwrap().clone();
    for observation in &observations {
        metrics
            .fetch
            .with_label_values(&[
                &manifest.source,
                manifest.mode.as_str(),
                observation.outcome,
            ])
            .inc();
        metrics
            .duration
            .with_label_values(&[&manifest.source])
            .observe(observation.seconds);
    }
    (result, observations)
}
