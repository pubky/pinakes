use crate::{
    adapter::Manifest,
    model::{ResourceMeta, Status},
};
use anyhow::{Context, Result};
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
    /// Whether a refresh ever succeeded, readable without waiting for one that runs.
    refreshed_once: Arc<std::sync::atomic::AtomicBool>,
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
    /// Resources whose last grouping run parked a key over the budget, stopped on it, or settled
    /// after stopping repeatedly on unchanged input.
    pub group_budget: IntGaugeVec,
    /// Resources whose last grouping run refused a join for conflicting single-object keys.
    pub group_key_conflicts: IntGauge,
    /// API handler panics (either listener) answered 500 by the backstop layer.
    pub api_panics: IntCounter,
    /// Requests answered, by listener (`public`, `admin`), route (its path template, `unmatched`
    /// when no route has the path; never the path asked) and status. A tool call on `/mcp` counts
    /// once, there: the REST routes it calls are not counted again.
    pub api_requests: IntCounterVec,
    /// How long a request took to its response's head, by listener and route.
    pub api_duration: HistogramVec,
    /// `/metrics` scrapes whose read of the database failed: the gauges kept their last values.
    pub refresh_failures: IntCounter,
    /// New previews `POST /v1/resolve` refused, by the limit that refused them: the instance's
    /// hour (`instance`) or one client's share of it (`client`).
    pub preview_refused: IntCounterVec,
    /// Resources nothing ever referenced (only previewed), each kept for the preview grace; not
    /// derived children or venues.
    pub preview_backlog: IntGauge,
    pub ingest_lag: IntGauge,
    pub ingest_success: IntGauge,
    pub ingest_resets: IntCounter,
    /// Nexus polls whose batch work panicked outside a feed line (the cursor holds; retried).
    pub ingest_panics: IntCounter,
    pub ingest_rejected: IntCounterVec,
    /// Refusals of an operator's configured key, token or app credentials by its service, as it
    /// signals one (`sources::token_refused`; never a 429 or 5xx), by keyed source
    /// (`sources::KEYED`), each at 0 from the start.
    pub token_refusals: IntCounterVec,
    /// Per catalogue (`catalogue::Catalogue`), each from the start: its cards, whether it is on
    /// in the daemon's configuration ([`Metrics::catalogue_switches`]), when its last full
    /// import and its last update finished (0: never), how long ago it was last current (0
    /// before its first finished import) and its failed runs.
    pub catalogue_cards: IntGaugeVec,
    pub catalogue_enabled: IntGaugeVec,
    pub catalogue_import: IntGaugeVec,
    pub catalogue_update: IntGaugeVec,
    pub catalogue_lag: IntGaugeVec,
    pub catalogue_errors: IntCounterVec,
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
        let group_key_conflicts = IntGauge::new(
            "pinakes_group_key_conflict_resources",
            "Resources whose last grouping run refused a join that would put two MBIDs, Q-ids or Discogs IDs of one kind in a group",
        )?;
        registry.register(Box::new(group_key_conflicts.clone()))?;
        let ingest_resets = IntCounter::new(
            "pinakes_ingest_epoch_resets_total",
            "Nexus projection resets",
        )?;
        registry.register(Box::new(ingest_resets.clone()))?;
        let ingest_panics = IntCounter::new(
            "pinakes_ingest_panics_total",
            "Nexus polls whose batch work panicked outside a feed line (the cursor holds)",
        )?;
        registry.register(Box::new(ingest_panics.clone()))?;
        let api_panics = IntCounter::new(
            "pinakes_api_panics_total",
            "API requests (public or admin listener) whose handler panicked (answered 500)",
        )?;
        registry.register(Box::new(api_panics.clone()))?;
        let api_requests = IntCounterVec::new(
            Opts::new(
                "pinakes_api_requests_total",
                "API requests answered, by listener, route (path template; unmatched when no route has the path) and status",
            ),
            &["listener", "route", "status"],
        )?;
        registry.register(Box::new(api_requests.clone()))?;
        let api_duration = HistogramVec::new(
            HistogramOpts::new(
                "pinakes_api_request_duration_seconds",
                "Time from an API request to its response's head, by listener and route",
            ),
            &["listener", "route"],
        )?;
        registry.register(Box::new(api_duration.clone()))?;
        // The paths no route has, from 0: both series exist before the first request.
        for listener in ["public", "admin"] {
            api_requests.with_label_values(&[listener, "unmatched", "404"]);
            api_duration.with_label_values(&[listener, "unmatched"]);
        }
        let refresh_failures = IntCounter::new(
            "pinakes_metrics_refresh_failures_total",
            "Metrics scrapes whose read of the database failed (the gauges kept their last values)",
        )?;
        registry.register(Box::new(refresh_failures.clone()))?;
        let preview_refused = IntCounterVec::new(
            Opts::new(
                "pinakes_preview_refused_total",
                "New previews refused, by limit: the instance's hourly capacity or one client's share",
            ),
            &["limit"],
        )?;
        registry.register(Box::new(preview_refused.clone()))?;
        for limit in ["instance", "client"] {
            preview_refused.with_label_values(&[limit]);
        }
        let preview_backlog = IntGauge::new(
            "pinakes_preview_backlog",
            "Resources nothing ever referenced (only previewed, not a derived child or a venue), kept for the preview grace",
        )?;
        registry.register(Box::new(preview_backlog.clone()))?;
        registry.register(Box::new(ALSO_ON_SERVICE_ERRORS.clone()))?;
        registry.register(Box::new(ALSO_ON_RECORDING_KEY_CONFLICTS.clone()))?;
        registry.register(Box::new(RESOLUTION_DEFERRED.clone()))?;
        registry.register(Box::new(AFTER_SAVE_FAILURES.clone()))?;
        let out = Self {
            api_panics,
            api_requests,
            api_duration,
            refresh_failures,
            preview_refused,
            preview_backlog,
            ingest_rejected: metric!(
                IntCounterVec,
                "pinakes_ingest_rejected_total",
                "Rejected feed lines in committed batches by bounded reason",
                &["reason"]
            ),
            token_refusals: metric!(
                IntCounterVec,
                "pinakes_source_token_refusals_total",
                "Refusals of an operator's configured key, token or app credentials by its service (an auth refusal, never a 429 or 5xx), by keyed source",
                &["source"]
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
                "Resource jobs due by the claim's condition (pending; a switched-off route's rows too), leased and dead",
                &["resolver", "state"]
            ),
            overdue: metric!(
                IntGaugeVec,
                "pinakes_refresh_overdue",
                "Resources past refresh time",
                &["resolver"]
            ),
            resources: metric!(
                IntGaugeVec,
                "pinakes_resources",
                "Resources by resolver and retrieval status",
                &["resolver", "status"]
            ),
            group_budget: metric!(
                IntGaugeVec,
                "pinakes_group_budget_resources",
                "Resources whose last grouping run parked a key over the budget, stopped on it, or settled after repeated stops",
                &["state"]
            ),
            group_key_conflicts,
            catalogue_cards: metric!(
                IntGaugeVec,
                "pinakes_catalogue_cards",
                "Cards each catalogue holds",
                &["catalogue"]
            ),
            catalogue_enabled: metric!(
                IntGaugeVec,
                "pinakes_catalogue_enabled",
                "Whether each catalogue is on in the daemon's configuration (1) or off (0)",
                &["catalogue"]
            ),
            catalogue_import: metric!(
                IntGaugeVec,
                "pinakes_catalogue_last_import_timestamp_seconds",
                "When each catalogue's last full import finished (0: never)",
                &["catalogue"]
            ),
            catalogue_update: metric!(
                IntGaugeVec,
                "pinakes_catalogue_last_update_timestamp_seconds",
                "When each catalogue's last update finished (0: never)",
                &["catalogue"]
            ),
            catalogue_lag: metric!(
                IntGaugeVec,
                "pinakes_catalogue_update_lag_seconds",
                "Seconds since each catalogue's last finished full import or update (0 before the first full import finished)",
                &["catalogue"]
            ),
            catalogue_errors: metric!(
                IntCounterVec,
                "pinakes_catalogue_import_errors_total",
                "Failed catalogue import and update runs, by catalogue",
                &["catalogue"]
            ),
            registry,
            refresh_lock: Arc::default(),
            refreshed_once: Arc::default(),
            ingest_lag,
            ingest_success,
            ingest_resets,
            ingest_panics,
        };
        for reason in crate::ingest::REJECT_REASONS {
            out.ingest_rejected.with_label_values(&[reason]);
        }
        for manifest in crate::adapter::manifests() {
            out.source_series(&manifest);
        }
        // The base resolvers' failed steps after a save from 0; the workers add theirs
        // (`queue::spawn_with_egress`).
        for resolver in crate::resolver::ROUTES {
            after_save_series(resolver);
        }
        // Every keyed source's refusals from 0, so a key refused later is a rise an alert reads.
        for source in crate::sources::KEYED {
            out.token_refusals.with_label_values(&[source]);
        }
        for catalogue in crate::catalogue::Catalogue::ALL {
            let name = [catalogue.name()];
            out.catalogue_cards.with_label_values(&name);
            out.catalogue_enabled.with_label_values(&name);
            out.catalogue_import.with_label_values(&name);
            out.catalogue_update.with_label_values(&name);
            out.catalogue_lag.with_label_values(&name);
            out.catalogue_errors.with_label_values(&name);
        }
        Ok(out)
    }
    /// Which catalogues are on (`pinakes_catalogue_enabled`), from the configuration the daemon
    /// runs with; the alerts read only those.
    pub fn catalogue_switches(&self, config: &crate::catalogue::CataloguesConfig) {
        for catalogue in crate::catalogue::Catalogue::ALL {
            self.catalogue_enabled
                .with_label_values(&[catalogue.name()])
                .set(i64::from(config.enabled(catalogue)));
        }
    }
    /// A source's series from the start, each at 0: its fetches by outcome, changes by class,
    /// held snapshots and cursor resets. A rule on `increase()` reads a rise from 0; a series
    /// that first appears at 1 shows none. Called for the base manifests here and for every
    /// manifest of the resolvers the daemon runs (`queue::spawn_with_egress`).
    pub fn source_series(&self, manifest: &Manifest) {
        for outcome in OUTCOMES {
            self.fetch
                .with_label_values(&[&manifest.source, manifest.mode.as_str(), outcome]);
        }
        self.duration.with_label_values(&[&manifest.source]);
        for class in crate::changes::CLASSES {
            self.changes
                .with_label_values(&[manifest.source.as_str(), *class]);
        }
        self.held.with_label_values(&[&manifest.source]);
        self.resets.with_label_values(&[&manifest.source]);
        self.success.with_label_values(&[&manifest.source]);
    }
    /// Whether the database gauges were ever read (all but the queue's, which have no series
    /// until their own read): before that they hold no value, only zeros.
    pub fn ever_refreshed(&self) -> bool {
        self.refreshed_once
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Read the database gauges, at most once in 15 seconds. In two steps, each all or nothing:
    /// every gauge but the queue's, then the queue's (`pinakes_queue_depth`,
    /// `pinakes_refresh_overdue`). An error is a refresh that did not finish; the caller counts
    /// it.
    pub async fn refresh(&self, pool: &PgPool) -> Result<()> {
        let mut refreshed = self.refresh_lock.lock().await;
        if refreshed.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(15)) {
            return Ok(());
        }
        // Every read of a step first, then its gauges: a read that fails (the API pool's
        // statement timeout, a database restart) leaves its gauges at their last values, none
        // half reset.
        let feed=sqlx::query("SELECT coalesce(sum(greatest(feed_length-cursor,0)),0)::bigint AS lag,coalesce(extract(epoch FROM max(updated_at)),0)::bigint AS success,coalesce(sum(resets),0)::bigint AS resets FROM feed_cursor").fetch_one(pool).await?;
        let sources=sqlx::query("SELECT *,coalesce(extract(epoch FROM last_success_at),0)::bigint AS success FROM source_state").fetch_all(pool).await?;
        let resources=sqlx::query("SELECT resolver,status,count(*) AS n FROM resource WHERE resolver IS NOT NULL GROUP BY resolver,status").fetch_all(pool).await?;
        let groups=sqlx::query("SELECT (SELECT count(*) FROM group_input WHERE parked_keys>0 AND NOT settled_stopped) AS parked,(SELECT count(*) FROM group_dirty WHERE last_error='work_budget') AS stopped,(SELECT count(*) FROM group_input WHERE settled_stopped) AS settled,(SELECT count(*) FROM group_input WHERE key_conflicts>0) AS conflicts").fetch_one(pool).await?;
        // A resource nothing ever referenced keeps the instant it was created as its orphan time;
        // a derived child and a venue are kept for their parent and event, not as previews. The
        // partial orphan index bounds the scan to orphans.
        let catalogues=sqlx::query("SELECT catalogue,cards,error_count,coalesce(extract(epoch FROM last_import_at),0)::bigint AS import,coalesce(extract(epoch FROM last_update_at),0)::bigint AS update,CASE WHEN last_import_at IS NULL THEN 0 ELSE extract(epoch FROM now()-greatest(last_import_at,last_update_at))::bigint END AS lag FROM catalogue_state").fetch_all(pool).await?;
        let previews: i64 = sqlx::query_scalar("SELECT count(*) FROM resource r WHERE r.orphaned_at IS NOT NULL AND r.orphaned_at=r.created_at AND NOT EXISTS(SELECT 1 FROM resource_derivation d WHERE d.child_id=r.id) AND NOT EXISTS(SELECT 1 FROM resource v WHERE v.venue_id=r.id) AND NOT EXISTS(SELECT 1 FROM occurrence o WHERE o.venue_id=r.id)")
            .fetch_one(pool)
            .await?;
        for (source, refusals) in crate::sources::token_refusals() {
            sync_counter(
                &self.token_refusals.with_label_values(&[source]),
                refusals as i64,
            );
        }
        self.ingest_lag.set(feed.try_get("lag")?);
        self.ingest_success.set(feed.try_get("success")?);
        sync_counter(&self.ingest_resets, feed.try_get("resets")?);
        self.canary_ok.reset();
        self.canary_missing.reset();
        self.canary_failures.reset();
        for source in sources {
            let name: String = source.try_get("source")?;
            self.success
                .with_label_values(&[&name])
                .set(source.try_get("success")?);
            sync_counter(
                &self.held.with_label_values(&[&name]),
                source.try_get("feed_held_total")?,
            );
            sync_counter(
                &self.resets.with_label_values(&[&name]),
                source.try_get("cursor_resets_total")?,
            );
            let changes: serde_json::Value = source.try_get("change_totals")?;
            for (class, count) in changes.as_object().into_iter().flatten() {
                sync_counter(
                    &self.changes.with_label_values(&[&name, class]),
                    count.as_i64().unwrap_or(0),
                );
            }
            if let Some(ok) = source.try_get::<Option<bool>, _>("canary_ok")? {
                self.canary_ok
                    .with_label_values(&[&name])
                    .set(i64::from(ok));
                self.canary_missing
                    .with_label_values(&[&name])
                    .set(source.try_get::<Vec<String>, _>("canary_missing")?.len() as i64);
                self.canary_failures
                    .with_label_values(&[&name])
                    .set(source.try_get::<i32, _>("canary_failures")? as i64);
            }
        }
        self.resources.reset();
        for manifest in crate::adapter::manifests() {
            for status in ["ok", "pending", "missing", "unreachable", "error"] {
                self.resources
                    .with_label_values(&[&manifest.source, status])
                    .set(0);
            }
        }
        for row in resources {
            self.resources
                .with_label_values(&[
                    &row.try_get::<String, _>("resolver")?,
                    &row.try_get::<String, _>("status")?,
                ])
                .set(row.try_get("n")?);
        }
        for catalogue in crate::catalogue::Catalogue::ALL {
            let name = catalogue.name();
            let row = catalogues.iter().find(|row| {
                row.try_get::<String, _>("catalogue")
                    .is_ok_and(|c| c == name)
            });
            let value = |column: &str| -> Result<i64> {
                Ok(match row {
                    Some(row) => row.try_get(column)?,
                    None => 0,
                })
            };
            self.catalogue_cards
                .with_label_values(&[name])
                .set(value("cards")?);
            self.catalogue_import
                .with_label_values(&[name])
                .set(value("import")?);
            self.catalogue_update
                .with_label_values(&[name])
                .set(value("update")?);
            self.catalogue_lag
                .with_label_values(&[name])
                .set(value("lag")?);
            sync_counter(
                &self.catalogue_errors.with_label_values(&[name]),
                value("error_count")?,
            );
        }
        self.group_key_conflicts.set(groups.try_get("conflicts")?);
        self.preview_backlog.set(previews);
        for state in ["parked", "stopped", "settled"] {
            self.group_budget
                .with_label_values(&[state])
                .set(groups.try_get(state)?);
        }
        self.refreshed_once
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // The queue's gauges last, from a statement of their own: it is the one read that
        // weighs every row of `resource` by the claim's condition. When it fails or is cut off,
        // every gauge above holds what was just read, the queue's gauges keep their last values
        // (no series before their first read), and the refresh as a whole has failed.
        let queue = sqlx::query(&QUEUE_DEPTH)
            .fetch_all(pool)
            .await
            .context("the queue gauges were not read")?;
        self.queue.reset();
        self.overdue.reset();
        for manifest in crate::adapter::manifests() {
            for state in ["pending", "leased", "dead"] {
                self.queue
                    .with_label_values(&[&manifest.source, state])
                    .set(0);
            }
            self.overdue.with_label_values(&[&manifest.source]).set(0);
        }
        for row in queue {
            let resolver: String = row.try_get("resolver")?;
            for state in ["pending", "leased", "dead"] {
                self.queue
                    .with_label_values(&[&resolver, state])
                    .set(row.try_get(state)?);
            }
            self.overdue
                .with_label_values(&[&resolver])
                .set(row.try_get("overdue")?);
        }
        *refreshed = Some(Instant::now());
        Ok(())
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
/// The queue's gauges by resolver: dead letters, leased rows, rows due by the claim's own
/// condition (`pending`: a derived link nothing keeps is never claimed and is not counted; a
/// row of a route that is switched off is, though no worker claims it) and refreshes overdue.
pub(crate) static QUEUE_DEPTH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "SELECT resolver,count(*) FILTER(WHERE dead_letter) AS dead,\
         count(*) FILTER(WHERE NOT dead_letter AND lease_until>now()) AS leased,\
         count(*) FILTER(WHERE {}) AS pending,\
         count(*) FILTER(WHERE NOT final AND NOT dead_letter AND refresh_at<now()) AS overdue \
         FROM resource WHERE resolver IS NOT NULL GROUP BY resolver",
        crate::queue::CLAIMABLE
    )
});
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
/// Requests to a service an also-on source asks besides its own (Podcast Index, iTunes, Wikidata,
/// …) that failed, by service and error class: the lookup went on without them. No URL or
/// credential is a label.
static ALSO_ON_SERVICE_ERRORS: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(
    || {
        IntCounterVec::new(
        Opts::new(
            "pinakes_also_on_service_errors_total",
            "Failed also-on requests to a secondary service, by service and class (the lookup went on)",
        ),
        &["service", "class"],
    )
    .expect("static metric")
    },
);
pub fn also_on_service_error(service: &'static str, class: &'static str) {
    ALSO_ON_SERVICE_ERRORS
        .with_label_values(&[service, class])
        .inc();
}
/// Also-on runs that keyed a MusicBrainz recording another source keys differently on the same
/// resource (or two in one run), by source: both keys stay, and the resource joins no recording
/// group (a group holds one). Every producer keys the recording that stands for a set of
/// duplicates, so this counts sources that truly disagree. Observed only: nothing is dropped. No
/// resource or URL is a label.
static ALSO_ON_RECORDING_KEY_CONFLICTS: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        IntCounterVec::new(
            Opts::new(
                "pinakes_also_on_recording_key_conflicts_total",
                "Also-on runs that keyed a recording another source keys differently on the same resource, by source",
            ),
            &["source"],
        )
        .expect("static metric")
    });
pub fn also_on_recording_key_conflict(source: &str) {
    ALSO_ON_RECORDING_KEY_CONFLICTS
        .with_label_values(&[source])
        .inc();
}
/// Resolutions put off without a charged attempt, by reason (the row's `last_error` class): a
/// missing answer waiting for the fetch that confirms it (`missing_unconfirmed`).
static RESOLUTION_DEFERRED: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "pinakes_resolution_deferred_total",
            "Resolutions put off without a charged attempt, by reason",
        ),
        &["reason"],
    )
    .expect("static metric")
});
pub fn resolution_deferred(reason: &'static str) {
    RESOLUTION_DEFERRED.with_label_values(&[reason]).inc();
}
/// Saved cards whose steps after the save failed (the alias its page names, an episode's pages),
/// by resolver: the card stays as saved and the link is read again in full within
/// `max_backoff_secs`. A step that fails every time shows as a steady rise.
static AFTER_SAVE_FAILURES: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "pinakes_after_save_failures_total",
            "Saved cards whose steps after the save failed (the card stays; the link is read again), by resolver",
        ),
        &["resolver"],
    )
    .expect("static metric")
});
pub fn after_save_failure(resolver: &str) {
    AFTER_SAVE_FAILURES.with_label_values(&[resolver]).inc();
}
/// The failed steps after a save counted for `resolver` so far.
pub fn after_save_failures(resolver: &str) -> u64 {
    AFTER_SAVE_FAILURES.with_label_values(&[resolver]).get()
}
/// A resolver's `pinakes_after_save_failures_total` series from the start, at 0.
pub fn after_save_series(resolver: &str) {
    AFTER_SAVE_FAILURES.with_label_values(&[resolver]);
}
tokio::task_local! {static FETCHES: Arc<Mutex<Vec<Observation>>>; static DISCOVERY: IntCounterVec;}
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
