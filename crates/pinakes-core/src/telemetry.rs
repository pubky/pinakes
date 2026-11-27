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
    /// When the text index last committed the queue or found nothing to commit, in Unix seconds;
    /// 0 until it is open (`search_index`).
    pub search_index_pass: IntGauge,
    /// 1 once the daemon's text index is open, 0 before (and while it is built).
    pub search_index_open: IntGauge,
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
    /// The catalogue joins (`joins`, `catalogue_join_due`) and extras (`extras`,
    /// `catalogue_extras`) due now, and how long the oldest of each has been due.
    pub catalogue_backlog: IntGaugeVec,
    pub catalogue_backlog_age: IntGaugeVec,
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
    /// import and its last update finished (0: never), how far its cards are behind upstream
    /// (`current_as_of`; 0 before its first finished import), its failed runs, and whether it
    /// waits for the large-import turn.
    pub catalogue_cards: IntGaugeVec,
    pub catalogue_enabled: IntGaugeVec,
    /// Whether a catalogue that is on has no importer for want of `http.contact` (MusicBrainz).
    pub catalogue_needs_contact: IntGaugeVec,
    pub catalogue_import: IntGaugeVec,
    pub catalogue_update: IntGaugeVec,
    pub catalogue_lag: IntGaugeVec,
    pub catalogue_errors: IntCounterVec,
    pub catalogue_turn: IntGaugeVec,
    /// Whether the catalogue's importer refused the dump it waits behind: `refused` in its
    /// position, newer than the version applied (or before any), until a dump is applied. A
    /// refusal of an older dump (a trial by hand) is not what the importer waits for.
    pub catalogue_refused: IntGaugeVec,
    /// How far behind upstream each catalogue may fall before the lag alert fires
    /// (`Catalogue::max_update_lag`), fixed per catalogue.
    pub catalogue_max_lag: IntGaugeVec,
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
        let search_index_pass = IntGauge::new(
            "pinakes_search_index_last_pass_timestamp_seconds",
            "Last pass of the text index that read its queue without an error",
        )?;
        registry.register(Box::new(search_index_pass.clone()))?;
        let search_index_open = IntGauge::new(
            "pinakes_search_index_open",
            "1 once the text index is open, 0 while it is being built or cannot be",
        )?;
        registry.register(Box::new(search_index_open.clone()))?;
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
        let catalogue_backlog = IntGaugeVec::new(
            Opts::new(
                "pinakes_catalogue_backlog",
                "Catalogue join checks (joins) and extras reads (extras) due now",
            ),
            &["queue"],
        )?;
        registry.register(Box::new(catalogue_backlog.clone()))?;
        let catalogue_backlog_age = IntGaugeVec::new(
            Opts::new(
                "pinakes_catalogue_backlog_age_seconds",
                "How long the oldest due catalogue join check (joins) or extras read (extras) has been due (0: none)",
            ),
            &["queue"],
        )?;
        registry.register(Box::new(catalogue_backlog_age.clone()))?;
        for queue in ["joins", "extras"] {
            catalogue_backlog.with_label_values(&[queue]);
            catalogue_backlog_age.with_label_values(&[queue]);
        }
        registry.register(Box::new(ALSO_ON_SERVICE_ERRORS.clone()))?;
        registry.register(Box::new(ALSO_ON_RECORDING_KEY_CONFLICTS.clone()))?;
        registry.register(Box::new(RESOLUTION_DEFERRED.clone()))?;
        registry.register(Box::new(AFTER_SAVE_FAILURES.clone()))?;
        registry.register(Box::new(CATALOGUE_JOINS.clone()))?;
        registry.register(Box::new(CATALOGUE_JOIN_ERRORS.clone()))?;
        for stage in ["referenced", "before_fetch", "drain", "extras"] {
            CATALOGUE_JOIN_ERRORS.with_label_values(&[stage]);
        }
        // Every catalogue's joins from 0, so a dashboard and a rate see them before the first.
        for catalogue in crate::catalogue::Catalogue::ALL {
            for outcome in ["built", "released", "invalid"] {
                CATALOGUE_JOINS.with_label_values(&[catalogue.name(), outcome]);
            }
        }
        registry.register(Box::new(CATALOGUE_CARDS.clone()))?;
        registry.register(Box::new(SEARCH_INDEX_QUEUE.clone()))?;
        registry.register(Box::new(SEARCH_INDEX_REREAD.clone()))?;
        registry.register(Box::new(SEARCH_INDEX_BUSY.clone()))?;
        for queue in ["resources", "catalogue"] {
            SEARCH_INDEX_QUEUE.with_label_values(&[queue]);
        }
        for catalogue in crate::catalogue::Catalogue::ALL {
            SEARCH_INDEX_REREAD.with_label_values(&[catalogue.name()]);
        }
        for reason in ["turn", "budget", "share"] {
            SEARCH_INDEX_BUSY.with_label_values(&[reason]);
        }
        for catalogue in crate::catalogue::Catalogue::ALL {
            for outcome in CATALOGUE_CARD_OUTCOMES {
                CATALOGUE_CARDS.with_label_values(&[catalogue.name(), outcome]);
            }
        }
        registry.register(Box::new(BACKGROUND_FAILURES.clone()))?;
        registry.register(Box::new(BACKGROUND_SUCCESS.clone()))?;
        for name in BACKGROUND_LOOPS {
            BACKGROUND_FAILURES.with_label_values(&[name]);
            BACKGROUND_SUCCESS.with_label_values(&[name]);
        }
        let out = Self {
            api_panics,
            api_requests,
            api_duration,
            refresh_failures,
            preview_refused,
            preview_backlog,
            catalogue_backlog,
            catalogue_backlog_age,
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
            catalogue_needs_contact: metric!(
                IntGaugeVec,
                "pinakes_catalogue_needs_contact",
                "Whether each catalogue is on but its importer does not start without http.contact (1) or not (0)",
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
                "Seconds each catalogue's cards are behind upstream: since the upstream time of the data its last finished run left (0 before the first full import finished)",
                &["catalogue"]
            ),
            catalogue_errors: metric!(
                IntCounterVec,
                "pinakes_catalogue_import_errors_total",
                "Failed catalogue import and update runs, by catalogue",
                &["catalogue"]
            ),
            catalogue_max_lag: metric!(
                IntGaugeVec,
                "pinakes_catalogue_max_update_lag_seconds",
                "How far behind upstream each catalogue's cards may fall before CatalogueUpdateLag fires: the source's own publication rhythm",
                &["catalogue"]
            ),
            catalogue_turn: metric!(
                IntGaugeVec,
                "pinakes_catalogue_waiting_for_turn",
                "Whether each catalogue's full import waits for another large catalogue's (1) or not (0)",
                &["catalogue"]
            ),
            catalogue_refused: metric!(
                IntGaugeVec,
                "pinakes_catalogue_refused",
                "Whether each catalogue's importer refused the newest dump it read and waits for another (1) or not (0)",
                &["catalogue"]
            ),
            search_index_pass,
            search_index_open,
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
            out.catalogue_needs_contact.with_label_values(&name);
            out.catalogue_import.with_label_values(&name);
            out.catalogue_update.with_label_values(&name);
            out.catalogue_lag.with_label_values(&name);
            out.catalogue_errors.with_label_values(&name);
            out.catalogue_turn.with_label_values(&name);
            out.catalogue_refused.with_label_values(&name);
            out.catalogue_max_lag
                .with_label_values(&name)
                .set(catalogue.max_update_lag().as_secs() as i64);
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
    /// Which catalogues that are on have no importer for want of `http.contact`
    /// (`pinakes_catalogue_needs_contact`).
    pub fn catalogue_contact(&self, config: &crate::config::Config) {
        self.catalogue_needs_contact
            .with_label_values(&[crate::catalogue::Catalogue::Musicbrainz.name()])
            .set(i64::from(crate::catalogue::musicbrainz::needs_contact(
                config,
            )));
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
        let catalogues=sqlx::query("SELECT catalogue,cards,error_count,coalesce(extract(epoch FROM last_import_at),0)::bigint AS import,coalesce(extract(epoch FROM last_update_at),0)::bigint AS update,CASE WHEN last_import_at IS NULL OR current_as_of IS NULL THEN 0 ELSE greatest(extract(epoch FROM now()-current_as_of),0)::bigint END AS lag,(coalesce(strpos(waiting,$1),0)>0)::int::bigint AS turn,coalesce((position ? 'refused' AND (position->>'version' IS NULL OR position->>'refused'>position->>'version'))::int,0)::bigint AS refused FROM catalogue_state").bind(crate::catalogue::run::TURN).fetch_all(pool).await?;
        // A resource nothing ever referenced keeps the instant it was created as its orphan time;
        // a derived child and a venue are kept for their parent and event, not as previews. The
        // partial orphan index bounds the scan to orphans.
        // The text index's queues by their sequence span (read from each one's key, not counted
        // over the table): rows written and not yet read by a pass.
        let index_queues = sqlx::query("SELECT (SELECT coalesce(max(seq)-min(seq)+1,0) FROM search_index_queue) AS resources,(SELECT coalesce(max(seq)-min(seq)+1,0) FROM search_index_catalogue_queue) AS catalogue").fetch_one(pool).await?;
        for queue in ["resources", "catalogue"] {
            SEARCH_INDEX_QUEUE
                .with_label_values(&[queue])
                .set(index_queues.try_get(queue)?);
        }
        let backlog = sqlx::query("SELECT (SELECT count(*) FROM catalogue_join_due WHERE due_at<=now()) AS joins,(SELECT coalesce(extract(epoch FROM now()-min(due_at)),0)::bigint FROM catalogue_join_due WHERE due_at<=now()) AS joins_age,(SELECT count(*) FROM catalogue_extras WHERE due_at<=now()) AS extras,(SELECT coalesce(extract(epoch FROM now()-min(due_at)),0)::bigint FROM catalogue_extras WHERE due_at<=now()) AS extras_age").fetch_one(pool).await?;
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
            self.catalogue_turn
                .with_label_values(&[name])
                .set(value("turn")?);
            self.catalogue_refused
                .with_label_values(&[name])
                .set(value("refused")?);
        }
        self.group_key_conflicts.set(groups.try_get("conflicts")?);
        self.preview_backlog.set(previews);
        for queue in ["joins", "extras"] {
            self.catalogue_backlog
                .with_label_values(&[queue])
                .set(backlog.try_get(queue)?);
            self.catalogue_backlog_age
                .with_label_values(&[queue])
                .set(backlog.try_get(format!("{queue}_age").as_str())?);
        }
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
/// Resources built from a catalogue card (`built`: joined, or built again after the card
/// changed) and given back to the fetch queue (`released`: the card went or its catalogue is
/// off), by catalogue.
static CATALOGUE_JOINS: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "pinakes_catalogue_joins_total",
            "Resources built from a catalogue card (built) and given back to the fetch queue (released), by catalogue",
        ),
        &["catalogue", "outcome"],
    )
    .expect("static metric")
});
pub fn catalogue_join(catalogue: &str, outcome: &'static str) {
    CATALOGUE_JOINS
        .with_label_values(&[catalogue, outcome])
        .inc();
}
/// The rows the text index's queues hold (`search_index_queue`: resources; `search_index_catalogue_queue`:
/// catalogue cards), by their sequence span, from each metrics refresh.
static SEARCH_INDEX_QUEUE: std::sync::LazyLock<IntGaugeVec> = std::sync::LazyLock::new(|| {
    IntGaugeVec::new(
        Opts::new(
            "pinakes_search_index_queue",
            "Rows the text index's queues hold, not yet read by a pass: resources (search_index_queue) and catalogue cards (search_index_catalogue_queue)",
        ),
        &["queue"],
    )
    .expect("static metric")
});
/// The cards of each catalogue the text index has read again so far in the re-read under way
/// after the catalogue's first full import (0 when none runs).
static SEARCH_INDEX_REREAD: std::sync::LazyLock<IntGaugeVec> = std::sync::LazyLock::new(|| {
    IntGaugeVec::new(
        Opts::new(
            "pinakes_search_index_reread_cards",
            "Cards of each catalogue the text index has read again so far after its first full import (0: no re-read under way)",
        ),
        &["catalogue"],
    )
    .expect("static metric")
});
/// Add `read` cards to the re-read of `catalogue`; `None`: no re-read runs (every gauge 0).
pub fn search_index_reread(catalogue: Option<&str>, read: u64) {
    match catalogue {
        Some(catalogue) => SEARCH_INDEX_REREAD
            .with_label_values(&[catalogue])
            .add(read as i64),
        None => {
            for catalogue in crate::catalogue::Catalogue::ALL {
                SEARCH_INDEX_REREAD
                    .with_label_values(&[catalogue.name()])
                    .set(0);
            }
        }
    }
}
/// Text searches the text index answered 503, by why: no turn within the budget (`turn`), the
/// search ran past it (`budget`), or its client ran its share of turns already (`share`).
static SEARCH_INDEX_BUSY: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "pinakes_search_index_busy_total",
            "Text searches answered 503 because the text index was busy: no turn within the budget (turn), past the budget (budget), or the client's share of turns taken (share)",
        ),
        &["reason"],
    )
    .expect("static metric")
});
pub fn search_index_busy(reason: &'static str) {
    SEARCH_INDEX_BUSY.with_label_values(&[reason]).inc();
}
/// The busy searches counted for `reason` so far.
pub fn search_index_busy_count(reason: &'static str) -> u64 {
    SEARCH_INDEX_BUSY.with_label_values(&[reason]).get()
}
/// The cards each catalogue's writes read, by what became of them: `inserted`, `changed`,
/// `unchanged` (stamped only), `rejected` (refused by `Card::checked` or the table's checks) and
/// `removed` (by an update, or as unread at the end of a full import).
static CATALOGUE_CARDS: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "pinakes_catalogue_cards_total",
            "Cards each catalogue's imports and updates wrote, by result: inserted, changed, unchanged (stamped only), rejected (refused as invalid) or removed",
        ),
        &["catalogue", "result"],
    )
    .expect("static metric")
});
/// The outcomes of [`catalogue_cards`].
pub const CATALOGUE_CARD_OUTCOMES: [&str; 5] =
    ["inserted", "changed", "unchanged", "rejected", "removed"];
/// Count what one write of `catalogue`'s cards did.
pub fn catalogue_cards(catalogue: &str, report: crate::catalogue::store::WriteReport) {
    for (outcome, n) in CATALOGUE_CARD_OUTCOMES.into_iter().zip([
        report.inserted,
        report.changed,
        report.unchanged,
        report.rejected,
        report.removed,
    ]) {
        if n > 0 {
            CATALOGUE_CARDS
                .with_label_values(&[catalogue, outcome])
                .inc_by(n);
        }
    }
}
/// The cards counted for `catalogue` and `outcome` so far.
pub fn catalogue_cards_counted(catalogue: &str, outcome: &'static str) -> u64 {
    CATALOGUE_CARDS
        .with_label_values(&[catalogue, outcome])
        .get()
}
/// Failed catalogue joins and extras reads, by where they ran (`referenced`, `before_fetch`,
/// `drain`, `extras`): each is tried again (a fetch, a later check).
static CATALOGUE_JOIN_ERRORS: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "pinakes_catalogue_join_errors_total",
            "Failed catalogue joins and extras reads, by where they ran (each is tried again)",
        ),
        &["stage"],
    )
    .expect("static metric")
});
pub fn catalogue_join_error(stage: &'static str) {
    CATALOGUE_JOIN_ERRORS.with_label_values(&[stage]).inc();
}
/// The failed joins counted for `stage` so far.
pub fn catalogue_join_errors(stage: &'static str) -> u64 {
    CATALOGUE_JOIN_ERRORS.with_label_values(&[stage]).get()
}
/// The joins counted for `catalogue` and `outcome` so far.
pub fn catalogue_joins(catalogue: &str, outcome: &'static str) -> u64 {
    CATALOGUE_JOINS
        .with_label_values(&[catalogue, outcome])
        .get()
}
/// The daemon's background loops, as `pinakes_background_failures_total` and
/// `pinakes_background_last_success_timestamp_seconds` name them (`loop`): the grouping writer,
/// the catalogue sweep, joins and extras, the also-on worker, the same-event and same-recording
/// passes and their hourly sweeps, each step of the hourly maintenance (change retention, orphan
/// cleanup, OSM place pruning, also-on pruning, refill, the switched-off sources' values on cards
/// no refresh reads) and its minutely search revision fold,
/// and the ODbL dump.
pub const BACKGROUND_LOOPS: &[&str] = &[
    "grouping",
    "catalogue_sweep",
    "catalogue_joins",
    "catalogue_extras",
    "also_on",
    "same_event",
    "same_event_sweep",
    "same_recording",
    "same_recording_sweep",
    "retention",
    "cleanup",
    "osm_prune",
    "also_on_prune",
    "refill",
    "unhint",
    "search_fold",
    "osm_dump",
];
/// Failed passes of each background loop ([`BACKGROUND_LOOPS`]): the loop logs a warning and
/// tries again, so one that fails on every pass shows here, its last success standing still.
static BACKGROUND_FAILURES: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "pinakes_background_failures_total",
            "Failed passes of each background loop (logged and tried again), by loop",
        ),
        &["loop"],
    )
    .expect("static metric")
});
/// When each background loop last finished a pass without an error, in Unix seconds; 0 until
/// it has.
static BACKGROUND_SUCCESS: std::sync::LazyLock<IntGaugeVec> = std::sync::LazyLock::new(|| {
    IntGaugeVec::new(
        Opts::new(
            "pinakes_background_last_success_timestamp_seconds",
            "When each background loop last finished a pass without an error, Unix seconds (0 until it has), by loop",
        ),
        &["loop"],
    )
    .expect("static metric")
});
/// One pass of the background loop `name` ([`BACKGROUND_LOOPS`]): a failure counted, a success's
/// time kept. The loop logs its own warning.
pub fn background_pass<T, E>(name: &'static str, result: &std::result::Result<T, E>) {
    match result {
        Ok(_) => BACKGROUND_SUCCESS
            .with_label_values(&[name])
            .set(chrono::Utc::now().timestamp()),
        Err(_) => BACKGROUND_FAILURES.with_label_values(&[name]).inc(),
    }
}
/// The failed passes counted for background loop `name` so far.
pub fn background_failures(name: &str) -> u64 {
    BACKGROUND_FAILURES.with_label_values(&[name]).get()
}
/// When background loop `name` last finished a pass without an error (0 before it has).
pub fn background_success(name: &str) -> i64 {
    BACKGROUND_SUCCESS.with_label_values(&[name]).get()
}
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
