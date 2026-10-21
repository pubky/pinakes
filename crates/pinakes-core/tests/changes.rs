use axum::{body::Body, extract::ConnectInfo, http::Request, Router};
use http_body_util::BodyExt;
use pinakes_core::{
    adapter,
    api::{self, AppState},
    canary, changes,
    config::{CanaryConfig, Config, QueueConfig},
    model::{ResourceMeta, Status},
    queue,
    resolver::Resolver,
    store,
    telemetry::{self, Metrics},
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tower::ServiceExt;
mod common;
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Option<Value>, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    (status, parsed, String::from_utf8_lossy(&bytes).to_string())
}
/// A public listener request; its JSON is checked against the public document.
async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let (status, parsed, text) = request(app, method, path, body).await;
    if let Some(value) = &parsed {
        common::check_response(method, path, status, value);
    }
    (status, parsed.unwrap_or_else(|| json!(text)))
}
/// An admin listener request (the admin listener has no OpenAPI document).
async fn admin_call(app: &Router, method: &str, path: &str) -> (u16, Value) {
    let (status, parsed, text) = request(app, method, path, None).await;
    (status, parsed.unwrap_or_else(|| json!(text)))
}
/// The change lines of `ids` since the epoch, one page.
async fn changes_of(app: &Router, ids: &[&str], cursor: i64, limit: usize) -> (u16, Value) {
    call(
        app,
        "POST",
        "/v1/changes/lookup",
        Some(json!({"resource_ids":ids,"since":0,"cursor":cursor,"limit":limit})),
    )
    .await
}
async fn seed(pool: &PgPool, uri: &str) -> (String, ResourceMeta) {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "web", 86400);
    doc.name = Some("Initial".into());
    doc.set_type("MusicEvent");
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    (id, doc)
}
async fn classes(pool: &PgPool, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT class FROM resource_change WHERE resource_id=$1 ORDER BY seq")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}
#[sqlx::test(migrations = "../../migrations")]
async fn dated_event_guard_counts_drift_and_allows_confirmed_missing(pool: PgPool) {
    let (id, mut accepted) = seed(&pool, "https://example.org/dated").await;
    accepted.start = Some(chrono::Utc::now() + chrono::Duration::days(1));
    store::save_document(&pool, &id, 0, &accepted)
        .await
        .unwrap();
    let before: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let history = classes(&pool, &id).await;
    let metrics = Metrics::new().unwrap();
    for kind in ["WebPage", "Article", "Event"] {
        let mut doc = ResourceMeta::new(&accepted.url, "web", 3600);
        doc.set_type(kind);
        let (result, observations) =
            telemetry::observe(&metrics, &adapter::manifest("web"), async {
                let mut fetch = telemetry::FetchGuard::start();
                fetch.status(200);
                drop(fetch);
                store::save_document(&pool, &id, 0, &doc).await
            })
            .await;
        assert_eq!(
            result
                .unwrap_err()
                .downcast_ref::<pinakes_core::resolver::RetryLater>()
                .unwrap()
                .class,
            "drift"
        );
        assert_eq!(observations[0].outcome, "drift");
        let after: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(classes(&pool, &id).await, history);
    }
    let mut missing = ResourceMeta::new(&accepted.url, "web", 3600);
    missing.status = Status::Missing;
    for status in ["ok", "missing"] {
        missing.fetched_at += 1;
        store::save_document(&pool, &id, 0, &missing).await.unwrap();
        let (actual, start): (String, bool) =
            sqlx::query_as("SELECT status,start_at IS NOT NULL FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(actual, status);
        assert!(start);
    }
    store::save_document(&pool, &id, 0, &accepted)
        .await
        .unwrap();
    assert_eq!(classes(&pool, &id).await.last().unwrap(), "uncancelled");
}
#[sqlx::test(migrations = "../../migrations")]
async fn confirmations_failures_recovery_and_fencing(pool: PgPool) {
    let (id, mut doc) = seed(&pool, "https://example.org/event").await;
    doc.fetched_at += 100;
    doc.attributes.insert("content_length".into(), "999".into());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    assert_eq!(classes(&pool, &id).await, vec!["created"]);
    let original = doc.clone();
    doc.status = Status::Missing;
    doc.fetched_at += 1;
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let (status, streak): (String, i32) =
        sqlx::query_as("SELECT status,missing_streak FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), streak), ("ok", 1));
    let mut failure = doc.clone();
    failure.status = Status::Unreachable;
    store::save_document(&pool, &id, 0, &failure).await.unwrap();
    assert_eq!(classes(&pool, &id).await, vec!["created"]);
    let accepted: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(accepted["status"], "ok");
    assert_eq!(accepted["name"], "Initial");
    doc.fetched_at += 1;
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    assert_eq!(classes(&pool, &id).await, vec!["created", "cancelled"]);
    store::save_document(&pool, &id, 0, &failure).await.unwrap();
    assert_eq!(classes(&pool, &id).await.len(), 2);
    store::save_document(&pool, &id, 0, &original)
        .await
        .unwrap();
    assert_eq!(
        classes(&pool, &id).await,
        vec!["created", "cancelled", "uncancelled"]
    );
    doc = original;
    doc.name = Some("Renamed".into());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    doc.start = Some("2026-10-20T12:00:00Z".parse().unwrap());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    doc.location = Some(pinakes_core::model::Point {
        lat: 46.0,
        lon: 8.0,
    });
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    doc.description = Some("New details".into());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    assert_eq!(
        classes(&pool, &id).await,
        vec![
            "created",
            "cancelled",
            "uncancelled",
            "renamed",
            "rescheduled",
            "relocated",
            "details"
        ]
    );
    doc.name = Some("Stale completion".into());
    store::save_document(&pool, &id, 99, &doc).await.unwrap();
    assert_eq!(classes(&pool, &id).await.len(), 7);
    let name: String = sqlx::query_scalar("SELECT name FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Renamed");
}
#[sqlx::test(migrations = "../../migrations")]
async fn paging_lookup_retention_and_deleted_history(pool: PgPool) {
    let (id, mut doc) = seed(&pool, "https://example.org/one").await;
    let (other, _) = seed(&pool, "https://example.org/two").await;
    for name in ["Two", "Three", "Four"] {
        doc.name = Some(name.into());
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
    }
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, first) = changes_of(&app, &[&id, &other], 0, 2).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["lines"].as_array().unwrap().len(), 2);
    assert_eq!(first["has_more"], true);
    let (_, second) = changes_of(
        &app,
        &[&id, &other],
        first["next_cursor"].as_i64().unwrap(),
        2,
    )
    .await;
    assert!(
        second["lines"][0]["seq"].as_i64().unwrap() > first["lines"][1]["seq"].as_i64().unwrap()
    );
    let (_, all) = changes_of(&app, &[&id], 0, 500).await;
    let renamed: Vec<&Value> = all["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["class"] == "renamed")
        .collect();
    assert_eq!(renamed.len(), 3);
    assert!(renamed[0]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f != "fetched_at"));
    let (_, own) = call(
        &app,
        "POST",
        "/v1/changes/lookup",
        Some(json!({"resource_ids":[id],"since":0,"limit":2})),
    )
    .await;
    assert_eq!(own["lines"].as_array().unwrap().len(), 2);
    assert_eq!(own["has_more"], true);
    assert!(own["lines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["resource_id"] == id));
    let (_, only) = changes_of(&app, &[&other], 0, 100).await;
    assert_eq!(only["lines"].as_array().unwrap().len(), 1);
    assert_eq!(changes_of(&app, &[&id], -1, 100).await.0, 400);
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/changes/lookup",
            Some(json!({"resource_ids":vec![id.clone();501],"since":0}))
        )
        .await
        .0,
        400
    );
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '10 days' WHERE id=$1")
        .bind(&other)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(store::cleanup(&pool, 7).await.unwrap(), 1);
    assert_eq!(classes(&pool, &other).await, vec!["created", "deleted"]);
    assert_eq!(
        changes_of(&app, &[&other], 0, 100).await.1["lines"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    sqlx::query("UPDATE resource_change SET at=now()-interval '91 days' WHERE seq<=3")
        .execute(&pool)
        .await
        .unwrap();
    changes::retain(&pool, 90).await.unwrap();
    assert_eq!(changes_of(&app, &[&id], 1, 100).await.0, 410);
    assert_eq!(changes_of(&app, &[&id], 0, 100).await.0, 200);
}
/// A change lookup takes links as `POST /v1/resources/lookup` does, IDs, or both: a link reads
/// as its ID, an alias spelling as its alias (its line names the root, then the root's lines
/// follow), and an unknown link adds nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_change_lookup_takes_links_ids_or_both(pool: PgPool) {
    let (id, mut doc) = seed(&pool, "https://example.org/one").await;
    let (other, _) = seed(&pool, "https://example.org/two").await;
    doc.name = Some("Renamed".into());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let variant = "https://example.org/one?utm_source=newsletter";
    let alias = store::ensure_resource(&pool, variant).await.unwrap();
    assert_ne!(alias, id);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let ask = |body: Value| {
        let app = app.clone();
        async move {
            let mut body = body;
            body["since"] = json!(0);
            call(&app, "POST", "/v1/changes/lookup", Some(body)).await
        }
    };
    let seqs = |v: &Value| -> Vec<i64> {
        v["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["seq"].as_i64().unwrap())
            .collect()
    };
    let (code, by_ids) = ask(json!({"resource_ids":[id, other]})).await;
    assert_eq!(code, 200, "{by_ids}");
    assert_eq!(seqs(&by_ids).len(), 3, "{by_ids}");
    let (_, by_links) =
        ask(json!({"uris":["https://example.org/one","https://example.org/two"]})).await;
    assert_eq!(seqs(&by_links), seqs(&by_ids));
    let (_, both) = ask(json!({"resource_ids":[id],"uris":["https://example.org/two"]})).await;
    assert_eq!(seqs(&both), seqs(&by_ids));
    // An alias spelling: its own line names the root, and the root's lines are its lines.
    let (_, aliased) = ask(json!({"uris":[variant]})).await;
    let lines = aliased["lines"].as_array().unwrap();
    let own: Vec<&Value> = lines
        .iter()
        .filter(|l| l["resource_id"] == alias.as_str())
        .collect();
    assert_eq!(own.len(), 1, "{aliased}");
    assert_eq!(
        (&own[0]["class"], &own[0]["root"]),
        (&json!("aliased"), &json!(id))
    );
    let (_, root) = ask(json!({"resource_ids":[id]})).await;
    assert_eq!(lines.len(), seqs(&root).len() + 1, "{aliased}");
    // The owner's body: links and nothing else. Without `since`, every retained change, paged.
    let (code, plain) = call(
        &app,
        "POST",
        "/v1/changes/lookup",
        Some(json!({"uris":["https://example.org/one","https://example.org/two"]})),
    )
    .await;
    assert_eq!(code, 200, "{plain}");
    assert_eq!(seqs(&plain), seqs(&by_ids));
    let (_, first) = call(
        &app,
        "POST",
        "/v1/changes/lookup",
        Some(json!({"uris":["https://example.org/one","https://example.org/two"],"limit":1})),
    )
    .await;
    assert_eq!((seqs(&first).len(), &first["has_more"]), (1, &json!(true)));
    let (_, rest) = call(
        &app,
        "POST",
        "/v1/changes/lookup",
        Some(json!({"uris":["https://example.org/one","https://example.org/two"],"cursor":first["next_cursor"]})),
    )
    .await;
    assert_eq!(
        [seqs(&first), seqs(&rest)].concat(),
        seqs(&by_ids),
        "the pages add up"
    );
    // An unknown link adds nothing.
    let (code, unknown) = ask(json!({"uris":["https://example.org/never-seen"]})).await;
    assert_eq!((code, seqs(&unknown)), (200, vec![]));
    let (_, with_unknown) =
        ask(json!({"resource_ids":[id, other],"uris":["https://example.org/never-seen"]})).await;
    assert_eq!(seqs(&with_unknown), seqs(&by_ids));
    for bad in [
        json!({}),
        json!({"resource_ids":[],"uris":[]}),
        json!({"resource_ids":vec![id.clone(); 300],"uris":vec!["https://example.org/one"; 201]}),
        json!({"resource_ids":["x"]}),
    ] {
        assert_eq!(ask(bad.clone()).await.0, 400, "{bad}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn overdue_priority_respects_backoff_and_final(pool: PgPool) {
    let (old, _) = seed(&pool, "https://example.org/old").await;
    let (wanted, _) = seed(&pool, "https://example.org/wanted").await;
    sqlx::query("UPDATE resource SET refresh_at=now()-interval '1 hour',next_attempt_at=now()-interval '1 hour'").execute(&pool).await.unwrap();
    sqlx::query("UPDATE resource SET next_attempt_at=now()-interval '2 hours' WHERE id=$1")
        .bind(&old)
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/resources/lookup",
            Some(json!({"ids":[wanted]}))
        )
        .await
        .0,
        200
    );
    let job = queue::claim(&pool, "web", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.id, wanted);
    sqlx::query(
        "UPDATE resource SET lease_until=NULL,next_attempt_at=now()+interval '1 hour' WHERE id=$1",
    )
    .bind(&wanted)
    .execute(&pool)
    .await
    .unwrap();
    store::expedite(&pool, std::slice::from_ref(&wanted))
        .await
        .unwrap();
    assert_eq!(
        queue::claim(&pool, "web", &QueueConfig::default())
            .await
            .unwrap()
            .unwrap()
            .id,
        old
    );
    sqlx::query(
        "UPDATE resource SET final=true,lease_until=NULL,next_attempt_at=now()-interval '1 hour'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(queue::claim(&pool, "web", &QueueConfig::default())
        .await
        .unwrap()
        .is_none());
}
struct Probe {
    doc: ResourceMeta,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Resolver for Probe {
    fn manifest(&self) -> adapter::Manifest {
        let mut manifest = adapter::manifest("web");
        manifest.expected_fields = vec!["name".into()];
        manifest
    }
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut fetch = telemetry::FetchGuard::start();
        fetch.status(200);
        drop(fetch);
        telemetry::document(telemetry::last_fetch(), &self.manifest(), &self.doc);
        Ok(self.doc.clone())
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(3600)
    }
}
/// A canary whose sample is over its cap: fetched, then refused.
struct OverCap;
#[async_trait::async_trait]
impl Resolver for OverCap {
    fn manifest(&self) -> adapter::Manifest {
        adapter::manifest("web")
    }
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        let mut fetch = telemetry::FetchGuard::start();
        fetch.status(200);
        drop(fetch);
        Err(
            anyhow::Error::new(pinakes_core::resolver::egress::BodyTooLarge { cap: 1024 * 1024 })
                .context("reading the sample"),
        )
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(3600)
    }
}
/// An over-budget canary sample is inconclusive: no health, no outage, no parse failure and no
/// source outcome, even when the refusal is wrapped in context.
#[sqlx::test(migrations = "../../migrations")]
async fn an_over_cap_canary_is_inconclusive(pool: PgPool) {
    let config = CanaryConfig::default();
    let manifest = adapter::manifest("web");
    let metrics = Metrics::new().unwrap();
    assert!(canary::claim(&pool, &manifest, &config)
        .await
        .unwrap()
        .is_none());
    sqlx::query("UPDATE source_state SET canary_due_at=now() WHERE source='web'")
        .execute(&pool)
        .await
        .unwrap();
    let generation = canary::claim(&pool, &manifest, &config)
        .await
        .unwrap()
        .unwrap();
    canary::run(&pool, &OverCap, &config, &metrics, generation)
        .await
        .unwrap();
    let (ok, failures, last_error, source_failures): (Option<bool>, i32, Option<String>, i32) =
        sqlx::query_as(
            "SELECT canary_ok,canary_failures,last_error,consecutive_failures FROM source_state WHERE source='web'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((ok, failures), (None, 0));
    assert_eq!((last_error, source_failures), (None, 0));
    metrics.refresh(&pool).await.unwrap();
    let text = metrics.encode().unwrap();
    // The sample's fetch counts as fetched, never as a parse error.
    assert!(
        text.contains("pinakes_fetch_total{mode=\"resource\",outcome=\"error\",source=\"web\"} 0"),
        "{text}"
    );
    assert!(
        text.contains("pinakes_fetch_total{mode=\"resource\",outcome=\"ok\",source=\"web\"} 1"),
        "{text}"
    );
}
#[sqlx::test(migrations = "../../migrations")]
async fn canary_bookkeeping_metrics_and_separate_listener(pool: PgPool) {
    let config = CanaryConfig::default();
    let mut manifest = adapter::manifest("web");
    manifest.expected_fields = vec!["name".into()];
    let metrics = Metrics::new().unwrap();
    let mut probe = Probe {
        doc: ResourceMeta::new("https://example.org/", "web", 86400),
        calls: AtomicUsize::new(0),
    };
    assert!(canary::claim(&pool, &manifest, &config)
        .await
        .unwrap()
        .is_none());
    for expected in 1..=3 {
        sqlx::query("UPDATE source_state SET canary_due_at=now() WHERE source='web'")
            .execute(&pool)
            .await
            .unwrap();
        let generation = canary::claim(&pool, &manifest, &config)
            .await
            .unwrap()
            .unwrap();
        assert!(canary::claim(&pool, &manifest, &config)
            .await
            .unwrap()
            .is_none());
        canary::run(&pool, &probe, &config, &metrics, generation)
            .await
            .unwrap();
        let failures: i32 =
            sqlx::query_scalar("SELECT canary_failures FROM source_state WHERE source='web'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(failures, expected);
    }
    metrics.refresh(&pool).await.unwrap();
    let text = metrics.encode().unwrap();
    assert!(text.contains("pinakes_canary_ok{source=\"web\"} 0"));
    assert!(text.contains("pinakes_canary_fields_missing{source=\"web\"} 1"));
    assert!(text.contains("outcome=\"drift\",source=\"web\"} 3"));
    probe.doc.name = Some("Healthy".into());
    sqlx::query("UPDATE source_state SET canary_due_at=now() WHERE source='web'")
        .execute(&pool)
        .await
        .unwrap();
    let generation = canary::claim(&pool, &manifest, &config)
        .await
        .unwrap()
        .unwrap();
    canary::run(&pool, &probe, &config, &metrics, generation)
        .await
        .unwrap();
    let (ok, failures): (bool, i32) =
        sqlx::query_as("SELECT canary_ok,canary_failures FROM source_state WHERE source='web'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(ok);
    assert_eq!(failures, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let state = AppState::new(pool.clone(), Config::default()).unwrap();
    let admin = api::admin_router(state.clone());
    let public = api::router(state);
    assert_eq!(call(&public, "GET", "/metrics", None).await.0, 404);
    let (status, body) = admin_call(&admin, "GET", "/metrics").await;
    assert_eq!(status, 200);
    assert!(body
        .as_str()
        .unwrap()
        .contains("# TYPE pinakes_fetch_total counter"));
    assert_eq!(admin_call(&admin, "GET", "/v1/info").await.0, 200);
}

#[sqlx::test(migrations = "../../migrations")]
async fn ops_artifacts_reference_registered_metrics(pool: PgPool) {
    sqlx::query("INSERT INTO source_state(source,canary_ok) VALUES('web',true)")
        .execute(&pool)
        .await
        .unwrap();
    let metrics = Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    let exposition = metrics.encode().unwrap();
    let names = exposition
        .lines()
        .filter_map(|line| {
            line.strip_prefix("# HELP ")
                .and_then(|s| s.split_whitespace().next())
        })
        .collect::<std::collections::BTreeSet<_>>();
    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(include_str!("../../../ops/alerts.yml")).unwrap();
    let rules = yaml["groups"][0]["rules"].as_sequence().unwrap();
    assert!(rules.len() >= 10);
    for rule in rules {
        for name in rule["expr"]
            .as_str()
            .unwrap()
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|s| s.starts_with("pinakes_"))
        {
            assert!(names.contains(name), "unregistered alert metric {name}");
        }
    }
    let dashboard: Value =
        serde_json::from_str(include_str!("../../../ops/grafana/pinakes.json")).unwrap();
    assert_eq!(dashboard["panels"].as_array().unwrap().len(), 6);
    for panel in dashboard["panels"].as_array().unwrap() {
        for target in panel["targets"].as_array().unwrap() {
            for name in target["expr"]
                .as_str()
                .unwrap()
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|s| s.starts_with("pinakes_"))
            {
                assert!(names.contains(name), "unregistered dashboard metric {name}");
            }
        }
    }
}

struct Unchanged;
#[async_trait::async_trait]
impl Resolver for Unchanged {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        Err(pinakes_core::resolver::NotModified.into())
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(86400)
    }
}
#[sqlx::test(migrations = "../../migrations")]
async fn not_modified_renews_budget_without_replacing_facts(pool: PgPool) {
    let (id, _) = seed(&pool, "https://example.org/conditional").await;
    sqlx::query("UPDATE resource SET next_attempt_at=now(),status='unreachable' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let job = queue::claim(&pool, "web", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    queue::run_job(&pool, &Unchanged, &QueueConfig::default(), &job)
        .await
        .unwrap();
    let (name, status, attempts, ttl): (String, String, i32, i64) = sqlx::query_as(
        "SELECT name,status,attempts,(doc->>'ttl_secs')::bigint FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (name.as_str(), status.as_str(), attempts, ttl),
        ("Initial", "ok", 0, 7 * 86400)
    );
    assert_eq!(classes(&pool, &id).await, vec!["created"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn unreachable_backoff_stops_and_missing_never_invents_existence(pool: PgPool) {
    let (id, mut doc) = seed(&pool, "https://example.org/blocked").await;
    doc.status = Status::Unreachable;
    let probe = Probe {
        doc,
        calls: AtomicUsize::new(0),
    };
    let cfg = QueueConfig {
        max_failures: 2,
        backoff_secs: 2,
        ..Default::default()
    };
    for attempt in 1..=2 {
        sqlx::query("UPDATE resource SET next_attempt_at=now()")
            .execute(&pool)
            .await
            .unwrap();
        let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
        queue::run_job(&pool, &probe, &cfg, &job).await.unwrap();
        let (tries,dead,delay,status,name):(i32,bool,f64,String,String)=sqlx::query_as("SELECT attempts,dead_letter,extract(epoch FROM next_attempt_at-now())::float8,status,name FROM resource WHERE id=$1").bind(&id).fetch_one(&pool).await.unwrap();
        assert_eq!(tries, attempt);
        assert_eq!(dead, attempt == 2);
        assert!(delay > (queue::backoff(&cfg, attempt as u32) - 1) as f64);
        assert_eq!(status, "unreachable");
        assert_eq!(name, "Initial");
    }
    assert!(queue::claim(&pool, "web", &cfg).await.unwrap().is_none());
    let metrics = Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    assert!(metrics
        .encode()
        .unwrap()
        .contains("pinakes_refresh_overdue{resolver=\"web\"} 0"));
    let uri = "https://example.org/never-existed";
    let never = store::ensure_resource(&pool, uri).await.unwrap();
    let mut missing = ResourceMeta::new(uri, "web", 3600);
    missing.status = Status::Missing;
    for _ in 0..3 {
        missing.fetched_at += 1;
        store::save_document(&pool, &never, 0, &missing)
            .await
            .unwrap();
    }
    assert!(classes(&pool, &never).await.is_empty());
    missing.status = Status::Ok;
    store::save_document(&pool, &never, 0, &missing)
        .await
        .unwrap();
    assert_eq!(classes(&pool, &never).await, vec!["created"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn new_preview_is_prioritized_and_change_time_follows_lock(pool: PgPool) {
    let _old = store::ensure_resource(&pool, "https://example.org/backlog")
        .await
        .unwrap();
    let uri = "https://example.org/new-preview";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    store::expedite(&pool, std::slice::from_ref(&id))
        .await
        .unwrap();
    let job = queue::claim(&pool, "web", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.id, id);
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(pinakes_core::locks::CHANGE_SEQUENCE)
        .execute(&mut *lock)
        .await
        .unwrap();
    let worker_pool = pool.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        store::save_document(
            &worker_pool,
            &worker_id,
            job.generation,
            &ResourceMeta::new(uri, "web", 3600),
        )
        .await
        .unwrap()
    });
    for _ in 0..100 {
        let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory')").fetch_one(&pool).await.unwrap();
        if blocked {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!worker.is_finished());
    let released = chrono::Utc::now();
    lock.commit().await.unwrap();
    worker.await.unwrap();
    let at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT at FROM resource_change WHERE resource_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(at >= released, "timestamp must follow lock acquisition");
}

#[sqlx::test(migrations = "../../migrations")]
async fn maintenance_runs_without_an_idle_ingestion_tick(pool: PgPool) {
    let (id, _) = seed(&pool, "https://example.org/orphan").await;
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '10 days'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource_change SET at=now()-interval '100 days'")
        .execute(&pool)
        .await
        .unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(pinakes_core::maintenance::run(
        pool.clone(),
        7,
        90,
        Duration::from_millis(20),
        rx,
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource")
                .fetch_one(&pool)
                .await
                .unwrap()
                == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(classes(&pool, &id).await, vec!["deleted"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn public_info_is_redacted_and_source_errors_are_classes(pool: PgPool) {
    let secret = "http://private-relay.internal:8080/?token=do-not-publish";
    sqlx::query("INSERT INTO feed_cursor(nexus) VALUES($1)")
        .bind(secret)
        .execute(&pool)
        .await
        .unwrap();
    pinakes_core::source::outcome(
        &pool,
        "web",
        Some("connect https://upstream.example/private?token=secret"),
    )
    .await
    .unwrap();
    let error: String =
        sqlx::query_scalar("SELECT last_error FROM source_state WHERE source='web'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(error, "parse");
    assert!(
        sqlx::query("UPDATE source_state SET last_error='https://secret'")
            .execute(&pool)
            .await
            .is_err()
    );
    let state = AppState::new(pool.clone(), Config::default()).unwrap();
    let public = api::router(state.clone());
    let admin = api::admin_router(state);
    let (_, body) = call(&public, "GET", "/v1/info", None).await;
    assert!(body.get("sources").is_none() && body.get("ingestion").is_none());
    assert!(!body.to_string().contains("private-relay"));
    let (code, full) = admin_call(&admin, "GET", "/v1/info").await;
    assert_eq!(code, 200);
    assert_eq!(full["ingestion"][0]["nexus"], secret);
    assert!(full.get("sources").is_some() && full.get("metrics").is_some());
    store::ensure_resource(&pool, "https://example.org/new")
        .await
        .unwrap();
    assert_eq!(
        admin_call(&admin, "GET", "/v1/info").await.1["counts"]["resources"],
        0,
        "info snapshot is cached"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn retention_preserves_normalized_current_and_cached_osm_versions(pool: PgPool) {
    let mut current = ResourceMeta::new("https://www.openstreetmap.org/node/77", "osm", 3600);
    current.set_type("Place");
    current.event.warnings.push("diagnostic".into());
    current
        .attributes
        .insert("content_length".into(), "42".into());
    current.aliases = vec![
        "https://example.org/z".into(),
        "https://example.org/a".into(),
        "https://example.org/z".into(),
    ];
    let mut fetched = current.clone();
    fetched.name = Some("New cached name".into());
    sqlx::query("INSERT INTO osm_place(osm_type,osm_id,type,type_path,doc,fetch_doc,versions,fetched_at,refresh_at,status) VALUES('node',77,'Place',ARRAY['Thing','Place'],$1,$2,jsonb_build_object(md5($3::jsonb::text),$3::jsonb,md5($4::jsonb::text),$4::jsonb,'unreferenced','{}'::jsonb),now(),now(),'ok')").bind(json!(current)).bind(json!(fetched)).bind(changes::normalize(&current)).bind(changes::normalize(&fetched)).execute(&pool).await.unwrap();
    changes::retain(&pool, 90).await.unwrap();
    let kept: Value = sqlx::query_scalar("SELECT versions FROM osm_place WHERE osm_id=77")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept.as_object().unwrap().len(), 2);
    assert!(kept.get("unreferenced").is_none());
    assert!(kept
        .as_object()
        .unwrap()
        .values()
        .any(|v| v == &changes::normalize(&current)));
    assert!(kept
        .as_object()
        .unwrap()
        .values()
        .any(|v| v == &changes::normalize(&fetched)));
}

/// Retention holds the change sequence while it deletes old lines. A save of a post whose author
/// opted out deletes that post's whole history (old lines too) and takes the sequence afterwards
/// to append its own line. Retention skips the lines such a save holds instead of waiting for
/// them, so the two never deadlock; the skipped lines go with the save, or at the next run.
#[sqlx::test(migrations = "../../migrations")]
async fn retention_and_an_opted_out_save_of_the_same_post_never_deadlock(pool: PgPool) {
    use pinakes_core::sources::mastodon;
    let uri = "https://mastodon.social/@Gargron/1";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let target = mastodon::Target {
        host: "mastodon.social".into(),
        id: "1".into(),
    };
    let card = |file: &str, at: i64| {
        let body = std::fs::read(format!(
            "{}/tests/fixtures/sources/mastodon/{file}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut doc = ResourceMeta::new(uri, "mastodon", 3600);
        doc.fetched_at = at;
        mastodon::card(doc, &target, 200, &body).unwrap().unwrap()
    };
    store::save_document(&pool, &id, 0, &card("status-1.json", 1000))
        .await
        .unwrap();
    // A spelling that became an alias of the post: the opted-out save clears its row too.
    let spelling = "https://mastodon.social/@Gargron/1/";
    let alias_id = store::ensure_resource(&pool, spelling).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let line = pinakes_core::alias::assign(
        &mut tx,
        &alias_id,
        &pinakes_core::alias::Alias {
            root: uri.into(),
            basis: "identifier",
        },
    )
    .await
    .unwrap();
    pinakes_core::alias::record(&mut tx, line.into_iter().collect())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sqlx::query("UPDATE resource SET doc='{}'::jsonb WHERE id=$1")
        .bind(&alias_id)
        .execute(&pool)
        .await
        .unwrap();
    // The post's lines are past retention.
    sqlx::query("UPDATE resource_change SET at=now()-interval '200 days' WHERE resource_id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    // Holding the alias's row stops the save between deleting the post's lines and appending.
    let mut hold = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR UPDATE")
        .bind(&alias_id)
        .execute(&mut *hold)
        .await
        .unwrap();
    let save = tokio::spawn({
        let (pool, id) = (pool.clone(), id.clone());
        let opted = card("status-1-opted-out.json", 2000);
        async move { store::save_document(&pool, &id, 0, &opted).await }
    });
    let held = || async {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE 'UPDATE resource SET name=NULL%')")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !held().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the save has deleted the post's lines and waits for the alias");
    // Retention runs while the save holds the post's old lines.
    let retained = tokio::spawn({
        let pool = pool.clone();
        async move { changes::retain(&pool, 90).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    hold.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        save.await.unwrap().expect("the save commits");
        retained.await.unwrap().expect("retention commits");
    })
    .await
    .expect("neither waits forever");
    let (lines, with_documents): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE before IS NOT NULL OR after IS NOT NULL) FROM resource_change WHERE resource_id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((lines, with_documents), (1, 0));
}
