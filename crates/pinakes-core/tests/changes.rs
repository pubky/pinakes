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
    source, store,
    telemetry::{self, Metrics},
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tower::ServiceExt;
async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
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
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string())),
    )
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
    let (status, first) = call(&app, "GET", "/v1/changes?limit=2", None).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["lines"].as_array().unwrap().len(), 2);
    assert_eq!(first["has_more"], true);
    let (_, second) = call(
        &app,
        "GET",
        &format!("/v1/changes?limit=2&cursor={}", first["next_cursor"]),
        None,
    )
    .await;
    assert!(
        second["lines"][0]["seq"].as_i64().unwrap() > first["lines"][1]["seq"].as_i64().unwrap()
    );
    let (_, filtered) = call(
        &app,
        "GET",
        "/v1/changes?class=renamed&type=Event&source=web",
        None,
    )
    .await;
    assert_eq!(filtered["lines"].as_array().unwrap().len(), 3);
    assert!(filtered["lines"][0]["before"].get("fetched_at").is_none());
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
    let (_, only) = call(&app, "GET", &format!("/v1/resources/{other}/changes"), None).await;
    assert_eq!(only["lines"].as_array().unwrap().len(), 1);
    assert_eq!(
        call(&app, "GET", "/v1/changes?class=invalid", None).await.0,
        400
    );
    assert_eq!(
        call(&app, "GET", "/v1/changes?cursor=-1", None).await.0,
        400
    );
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
        call(&app, "GET", &format!("/v1/resources/{other}/changes"), None)
            .await
            .1["lines"]
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
    assert_eq!(call(&app, "GET", "/v1/changes?cursor=1", None).await.0, 410);
    assert_eq!(call(&app, "GET", "/v1/changes?cursor=0", None).await.0, 200);
}
#[sqlx::test(migrations = "../../migrations")]
async fn held_containers_and_cursor_state(pool: PgPool) {
    let all = (0..10).map(|n| n.to_string()).collect::<Vec<_>>();
    let few = all[..3].to_vec();
    assert_eq!(
        source::container_keys(&pool, "calendar", &all, 3600)
            .await
            .unwrap()
            .created
            .len(),
        10
    );
    let first = source::container_keys(&pool, "calendar", &few, 3600)
        .await
        .unwrap();
    assert!(first.held);
    assert!(first.cancelled.is_empty());
    let saved: Vec<String> =
        sqlx::query_scalar("SELECT keys FROM source_state WHERE source='calendar'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(saved.len(), 10);
    source::outcome(&pool, "calendar", Some("timeout"))
        .await
        .unwrap();
    let second = source::container_keys(&pool, "calendar", &few, 3600)
        .await
        .unwrap();
    assert!(!second.held);
    assert_eq!(second.cancelled.len(), 7);
    let two = few[..2].to_vec();
    assert!(source::container_keys(&pool, "calendar", &two, 3600)
        .await
        .unwrap()
        .cancelled
        .is_empty());
    assert_eq!(
        source::container_keys(&pool, "calendar", &two, 3600)
            .await
            .unwrap()
            .cancelled,
        vec!["2"]
    );
    source::cursor(&pool, "calendar", Some("new-cursor"), true)
        .await
        .unwrap();
    let (cursor,resets,held):(String,i64,i64)=sqlx::query_as("SELECT cursor,cursor_resets_total,feed_held_total FROM source_state WHERE source='calendar'").fetch_one(&pool).await.unwrap();
    assert_eq!((cursor.as_str(), resets, held), ("new-cursor", 1, 1));
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
#[sqlx::test(migrations = "../../migrations")]
async fn canary_bookkeeping_metrics_and_separate_listener(pool: PgPool) {
    let config = CanaryConfig::default();
    let manifest = adapter::manifest("web");
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
    let (status, body) = call(&admin, "GET", "/metrics", None).await;
    assert_eq!(status, 200);
    assert!(body
        .as_str()
        .unwrap()
        .contains("# TYPE pinakes_fetch_total counter"));
    assert_eq!(call(&admin, "GET", "/v1/info", None).await.0, 404);
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
