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
    assert_eq!(store::cleanup(&pool, 7, 7 * 24).await.unwrap(), 1);
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
/// One missing answer for a social post is not yet `missing`: its change lines and its also-on
/// results stay, as its card does. The second, which confirms it, removes them.
#[sqlx::test(migrations = "../../migrations")]
async fn an_unconfirmed_missing_answer_keeps_a_social_post_s_lines_and_facts(pool: PgPool) {
    let uri = "https://social.example/@author/1";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut post = ResourceMeta::new(uri, "activitypub", 86400);
    post.set_type("SocialMediaPosting");
    post.name = Some("A post".into());
    store::save_document(&pool, &id, 0, &post).await.unwrap();
    sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) VALUES($1,'isrc','GBAYE0601498','musicbrainz',now()+interval '1 day')")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let kept = || async {
        let facts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM resource_fact WHERE resource_id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        (classes(&pool, &id).await, facts)
    };
    assert_eq!(kept().await, (vec!["created".to_owned()], 1));
    let mut missing = post.clone();
    missing.status = Status::Missing;
    missing.fetched_at += 1;
    store::save_document(&pool, &id, 0, &missing).await.unwrap();
    assert_eq!(kept().await, (vec!["created".to_owned()], 1));
    missing.fetched_at += 1;
    store::save_document(&pool, &id, 0, &missing).await.unwrap();
    assert_eq!(kept().await, (vec![], 0));
}

/// Orphan cleanup keeps no text of an opted-out post either: its `deleted` line holds no
/// document, as its other lines hold none.
#[sqlx::test(migrations = "../../migrations")]
async fn cleanup_keeps_no_document_of_an_opted_out_post(pool: PgPool) {
    let mut ids = vec![];
    for (uri, opted_out) in [
        ("https://example.org/opted-out", true),
        ("https://example.org/plain", false),
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut doc = ResourceMeta::new(uri, "web", 86400);
        doc.name = Some("The text of the post".into());
        if opted_out {
            doc.attributes.insert("opted_out".into(), "true".into());
        }
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        ids.push(id);
    }
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '10 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(store::cleanup(&pool, 7, 7 * 24).await.unwrap(), 2);
    let deleted: Vec<(String, bool)> = sqlx::query_as(
        "SELECT resource_id,before IS NULL FROM resource_change WHERE class='deleted' ORDER BY seq",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected = vec![(ids[0].clone(), true), (ids[1].clone(), false)];
    expected.sort();
    let mut deleted = deleted;
    deleted.sort();
    assert_eq!(deleted, expected);
    let texts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM resource_change WHERE resource_id=$1 AND (before IS NOT NULL OR after IS NOT NULL)",
    )
    .bind(&ids[0])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(texts, 0);
}

/// A stored card this build cannot read (a value it does not know) does not fail the save over
/// it: the new reading replaces it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_stored_card_that_does_not_read_is_replaced(pool: PgPool) {
    let (id, mut doc) = seed(&pool, "https://example.org/unreadable").await;
    sqlx::query(
        "UPDATE resource SET doc=jsonb_set(doc,'{embed}','{\"kind\":\"no-such-kind\"}') WHERE id=$1",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    doc.name = Some("Read again".into());
    doc.fetched_at += 1;
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let (name, embed): (Option<String>, Option<Value>) =
        sqlx::query_as("SELECT name,doc->'embed' FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(name.as_deref(), Some("Read again"));
    assert_eq!(embed, None);
    // A missing answer over such a card is saved too.
    sqlx::query(
        "UPDATE resource SET doc=jsonb_set(doc,'{embed}','{\"kind\":\"no-such-kind\"}') WHERE id=$1",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    doc.status = Status::Missing;
    for _ in 0..2 {
        doc.fetched_at += 1;
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
    }
    let status: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "missing");
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
    // A body of links and nothing else. Without `since`, every retained change, paged.
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
    // Two failed runs came before it: an inconclusive run neither adds to them nor clears them.
    sqlx::query("UPDATE source_state SET canary_failures=2 WHERE source='web'")
        .execute(&pool)
        .await
        .unwrap();
    canary::run_manifest(&pool, &OverCap, &manifest, &config, &metrics, generation)
        .await
        .unwrap();
    let (ok, failures, last_error, source_failures): (Option<bool>, i32, Option<String>, i32) =
        sqlx::query_as(
            "SELECT canary_ok,canary_failures,last_error,consecutive_failures FROM source_state WHERE source='web'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((ok, failures), (None, 2));
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
/// A canary whose reader panics on the sample, or answers a page with no field read.
struct Sample(Option<ResourceMeta>);
#[async_trait::async_trait]
impl Resolver for Sample {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        Ok(self.0.clone().expect("the reader panicked on the sample"))
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(3600)
    }
}
async fn run_web_canary(pool: &PgPool, sample: &Sample) -> (Option<bool>, i32, Vec<String>) {
    let config = CanaryConfig::default();
    let manifest = adapter::manifest("web");
    let metrics = Metrics::new().unwrap();
    let _ = canary::claim(pool, &manifest, &config).await.unwrap();
    sqlx::query("UPDATE source_state SET canary_due_at=now() WHERE source='web'")
        .execute(pool)
        .await
        .unwrap();
    let generation = canary::claim(pool, &manifest, &config)
        .await
        .unwrap()
        .unwrap();
    canary::run_manifest(pool, sample, &manifest, &config, &metrics, generation)
        .await
        .unwrap();
    sqlx::query_as(
        "SELECT canary_ok,canary_failures,canary_missing FROM source_state WHERE source='web'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}
/// A reader that panics on the canary's sample is a failed canary: the run returns (so the
/// worker that ran it goes on) and the failure is recorded.
#[sqlx::test(migrations = "../../migrations")]
async fn a_canary_whose_reader_panics_is_a_failed_canary(pool: PgPool) {
    let (ok, failures, _) = run_web_canary(&pool, &Sample(None)).await;
    assert_eq!((ok, failures), (Some(false), 1));
}
/// The web manifest expects no field of a page, but its canary's page has a title: a canary
/// that fetched the page and read no name has failed.
#[sqlx::test(migrations = "../../migrations")]
async fn the_web_canary_needs_a_name(pool: PgPool) {
    let mut page = ResourceMeta::new("https://example.org/", "web", 86400);
    let (ok, failures, missing) = run_web_canary(&pool, &Sample(Some(page.clone()))).await;
    assert_eq!((ok, failures), (Some(false), 1));
    assert_eq!(missing, ["name"]);
    page.name = Some("Example Domain".into());
    let (ok, failures, missing) = run_web_canary(&pool, &Sample(Some(page))).await;
    assert_eq!((ok, failures), (Some(true), 0));
    assert!(missing.is_empty());
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
        canary::run_manifest(&pool, &probe, &manifest, &config, &metrics, generation)
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
    canary::run_manifest(&pool, &probe, &manifest, &config, &metrics, generation)
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
        // Every fetch outcome a rule matches is one the daemon records.
        let expr = rule["expr"].as_str().unwrap();
        for matcher in expr.split("outcome=").skip(1) {
            let value = matcher
                .trim_start_matches('~')
                .strip_prefix('"')
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_else(|| panic!("an outcome matcher without a value in {expr}"));
            for outcome in value.split('|') {
                assert!(
                    telemetry::OUTCOMES.contains(&outcome),
                    "{}: no fetch has the outcome {outcome}",
                    rule["alert"].as_str().unwrap()
                );
            }
        }
    }
    // A recognised bot wall is its own outcome (`blocked`): the alert named for walls reads it.
    assert!(rules.iter().any(|r| r["alert"].as_str() == Some("WallUp")
        && r["expr"].as_str().unwrap().contains("blocked")));
    // A link never read has no refresh time and is in no overdue count: the queue's own depth
    // is alerted on too.
    assert!(rules
        .iter()
        .any(|r| r["alert"].as_str() == Some("QueueBacklog")
            && r["expr"]
                .as_str()
                .unwrap()
                .contains("pinakes_queue_depth{state=\"pending\"}")));
    // A missing series is its own alert with a wait, never the stalled-ingestion alert.
    for rule in rules {
        let expr = rule["expr"].as_str().unwrap();
        if expr.contains("absent(") {
            assert_eq!(rule["alert"].as_str(), Some("MetricsMissing"), "{expr}");
            assert!(rule["for"].is_string(), "{expr}");
        }
    }
    // Every keyed source's refusals are a series from the start (at 0), and an alert reads them.
    for source in pinakes_core::sources::KEYED {
        assert!(
            exposition.contains(&format!(
                "pinakes_source_token_refusals_total{{source=\"{source}\"}}"
            )),
            "{source}"
        );
    }
    assert!(rules
        .iter()
        .any(|r| r["alert"].as_str() == Some("ProviderKeyRefused")
            && r["expr"]
                .as_str()
                .unwrap()
                .contains("pinakes_source_token_refusals_total")));
    // The catalogue alerts read only catalogues that are on and have finished a full import.
    for (alert, reads) in [
        (
            "CatalogueImportFailed",
            &[
                "pinakes_catalogue_import_errors_total",
                "pinakes_catalogue_enabled == 1",
                "pinakes_catalogue_last_import_timestamp_seconds > 0",
            ][..],
        ),
        (
            "CatalogueTurnBlocked",
            &[
                "pinakes_catalogue_waiting_for_turn == 1",
                "and on (instance, job) (sum by (instance, job) (increase(pinakes_catalogue_import_errors_total",
            ],
        ),
        (
            "CatalogueUpdateLag",
            &[
                "pinakes_catalogue_update_lag_seconds > on (instance, job, catalogue) pinakes_catalogue_max_update_lag_seconds",
                "and on (instance, job, catalogue) (pinakes_catalogue_enabled == 1)",
                "pinakes_catalogue_last_import_timestamp_seconds > 0",
            ],
        ),
    ] {
        let rule = rules
            .iter()
            .find(|r| r["alert"].as_str() == Some(alert))
            .unwrap_or_else(|| panic!("{alert}"));
        for read in reads {
            assert!(
                rule["expr"].as_str().unwrap().contains(read),
                "{alert}: {read}"
            );
        }
    }
    // A Prometheus may scrape several instances: every catalogue rule that matches vectors
    // matches them per instance and job, never across instances (`ops/alerts.test.yml` runs
    // them with two).
    for rule in rules {
        let expr = rule["expr"].as_str().unwrap();
        if expr.contains("pinakes_catalogue_") {
            for clause in expr.split(" on (").skip(1) {
                assert!(
                    clause.starts_with("instance, job"),
                    "{}: on ({clause}",
                    rule["alert"].as_str().unwrap()
                );
            }
        }
    }
    let dashboard: Value =
        serde_json::from_str(include_str!("../../../ops/grafana/pinakes.json")).unwrap();
    assert_eq!(dashboard["panels"].as_array().unwrap().len(), 9);
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

/// A card of the link's shape alone (a social page that answered without its tags) never
/// replaces a stored card that says more, and adds no change line; a link never read gets it,
/// and so does one whose stored card says no more than the link's shape.
#[sqlx::test(migrations = "../../migrations")]
async fn a_shape_only_card_never_replaces_a_card_that_says_more(pool: PgPool) {
    let stored = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Option<String>)>(
                "SELECT doc->>'name', doc->>'description' FROM resource WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let plain = |uri: &str, at: i64| {
        let mut doc = ResourceMeta::new(uri, "web", 3600);
        doc.set_type("Person");
        doc.name = Some("Profile on social.example".into());
        doc.fetched_at = at;
        doc.shape_only = true;
        doc
    };
    let uri = "https://social.example/in/someone";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut good = ResourceMeta::new(uri, "web", 3600);
    good.set_type("Person");
    good.name = Some("Some One".into());
    good.description = Some("Builds bridges.".into());
    store::save_document(&pool, &id, 0, &good).await.unwrap();
    let lines = classes(&pool, &id).await;
    store::save_document(&pool, &id, 0, &plain(uri, good.fetched_at + 1))
        .await
        .unwrap();
    assert_eq!(
        stored(id.clone()).await,
        (Some("Some One".into()), Some("Builds bridges.".into()))
    );
    assert_eq!(classes(&pool, &id).await, lines);

    // A link never read gets the plain card.
    let fresh = "https://social.example/in/someone-else";
    let fresh_id = store::ensure_resource(&pool, fresh).await.unwrap();
    store::save_document(&pool, &fresh_id, 0, &plain(fresh, good.fetched_at))
        .await
        .unwrap();
    assert_eq!(
        stored(fresh_id.clone()).await,
        (Some("Profile on social.example".into()), None)
    );
    // A stored card of the shape alone says no more than a new one: it is replaced as any card
    // is (here by one with a fact the link names).
    let mut more = plain(fresh, good.fetched_at + 1);
    more.attributes
        .insert("handle".into(), "someone-else".into());
    store::save_document(&pool, &fresh_id, 0, &more)
        .await
        .unwrap();
    let handle: Option<String> =
        sqlx::query_scalar("SELECT doc->'attributes'->>'handle' FROM resource WHERE id=$1")
            .bind(&fresh_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(handle.as_deref(), Some("someone-else"));
}

/// The stored row of a card: its name, `fetched_at`, whether it is final and when it is read next
/// (seconds from now).
async fn kept_row(pool: &PgPool, id: &str) -> (Option<String>, Option<i64>, bool, f64) {
    sqlx::query_as(
        "SELECT doc->>'name', (doc->>'fetched_at')::bigint, final, \
         extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}
/// A person's card of `uri` named `name`, read at `at` (ms); `shape_only` for the plain card of
/// a page answered without its tags.
fn person(uri: &str, name: &str, at: i64, shape_only: bool) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, "web", 3600);
    doc.set_type("Person");
    doc.name = Some(name.into());
    doc.fetched_at = at;
    doc.shape_only = shape_only;
    doc
}
const PLAIN: &str = "Profile on social.example";

/// A stored card that says more by its name alone, or by a fact alone, is kept for a page
/// answered without its tags.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_that_says_more_by_its_name_or_a_fact_is_kept(pool: PgPool) {
    let now = chrono::Utc::now().timestamp_millis();
    for (path, name, fact) in [
        ("name-only", "Some One", None),
        ("fact-only", PLAIN, Some("12")),
    ] {
        let uri = format!("https://social.example/in/{path}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let mut good = person(&uri, name, now - 3600 * 1000, false);
        if let Some(fact) = fact {
            good.attributes.insert("followers".into(), fact.into());
        }
        store::save_document(&pool, &id, 0, &good).await.unwrap();
        store::save_document(&pool, &id, 0, &person(&uri, PLAIN, now, true))
            .await
            .unwrap();
        assert_eq!(
            kept_row(&pool, &id).await.0.as_deref(),
            Some(name),
            "{path}"
        );
        let stored: Option<String> =
            sqlx::query_scalar("SELECT doc->'attributes'->>'followers' FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), fact, "{path}");
    }
}

/// A page answered without its tags, on a stored card that says more, is a first missing answer:
/// the card stays as last read (no change line, its `fetched_at` that read's) and the confirming
/// read comes `limits.missing_ttl_secs` later, whatever the card's own lifetime (a profile's is a
/// day); the confirming read stores the plain card. A good read in between clears the observation.
#[sqlx::test(migrations = "../../migrations")]
async fn a_tagless_read_waits_for_its_confirming_read_as_a_missing_one(pool: PgPool) {
    let cfg = QueueConfig::default();
    let streak = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i32>("SELECT missing_streak FROM resource WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let day = 86_400;
    let at = |doc: ResourceMeta| ResourceMeta {
        ttl_secs: day,
        ..doc
    };
    let uri = "https://social.example/in/day-old";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let mut good = at(person(uri, "Some One", now - day as i64 * 1000, false));
    good.description = Some("Builds bridges.".into());
    store::save_document(&pool, &id, 0, &good).await.unwrap();
    let lines = classes(&pool, &id).await;

    // The first tagless read, a day after the last good one: kept, read again an hour later.
    store::save_document(&pool, &id, 0, &at(person(uri, PLAIN, now, true)))
        .await
        .unwrap();
    let (name, fetched, last, next) = kept_row(&pool, &id).await;
    assert_eq!(name.as_deref(), Some("Some One"));
    assert_eq!(
        fetched,
        Some(good.fetched_at),
        "fetched_at renewed by a keep"
    );
    let wait = cfg.missing_ttl_secs.max(3600) as f64;
    assert!(
        !last && next > wait - 60.0 && next <= wait,
        "next read in {next} s"
    );
    assert_eq!(streak(id.clone()).await, 1);
    assert_eq!(classes(&pool, &id).await, lines);

    // A good read in between clears the observation; the next tagless read is a first one again.
    let mut again = good.clone();
    again.fetched_at = now + 1;
    store::save_document(&pool, &id, 0, &again).await.unwrap();
    assert_eq!(streak(id.clone()).await, 0);
    store::save_document(&pool, &id, 0, &at(person(uri, PLAIN, now + 2, true)))
        .await
        .unwrap();
    assert_eq!(kept_row(&pool, &id).await.0.as_deref(), Some("Some One"));
    assert_eq!(streak(id.clone()).await, 1);

    // The confirming read stores the plain card, and the observation is over.
    let confirming = now + 2 + cfg.missing_ttl_secs as i64 * 1000;
    store::save_document(&pool, &id, 0, &at(person(uri, PLAIN, confirming, true)))
        .await
        .unwrap();
    let (name, fetched, ..) = kept_row(&pool, &id).await;
    assert_eq!(name.as_deref(), Some(PLAIN));
    assert_eq!(fetched, Some(confirming));
    assert_eq!(streak(id.clone()).await, 0);
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
    common::lock_waiters_reach(&pool, 1, "the save").await;
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
        7 * 24,
        90,
        Duration::from_millis(20),
        rx,
    ));
    tokio::time::timeout(Duration::from_secs(20), async {
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
    let uri = "https://mastodon.social/@Gargron/1";
    // The post's card from the Mastodon source; without it in this build there is nothing to test.
    let card = |file: &'static str, at: i64| async move {
        let api = "https://mastodon.social/api/v1/statuses/1";
        let mut doc =
            common::source_card("mastodon", uri, &[(api, 200, "application/json", file)]).await?;
        doc.fetched_at = at;
        Some(doc)
    };
    let Some(first) = card("status-1.json", 1000).await else {
        return;
    };
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    store::save_document(&pool, &id, 0, &first).await.unwrap();
    // A spelling saved with the post's card before it became its alias: the opted-out save
    // clears its row too.
    let spelling = "https://mastodon.social/@Gargron/1/";
    let alias_id = store::ensure_resource(&pool, spelling).await.unwrap();
    let mut early = card("status-1.json", 900).await.unwrap();
    early.url = spelling.into();
    store::save_document(&pool, &alias_id, 0, &early)
        .await
        .unwrap();
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
        let opted = card("status-1-opted-out.json", 2000).await.unwrap();
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

/// A stored event re-read as no event on a recorded decision (`not_event`: the page lists
/// several events, or is a creative work) is replaced, type and all; without the decision a
/// reading that merely lost the start is drift and keeps the card, as before.
#[sqlx::test(migrations = "../../migrations")]
async fn a_decided_no_event_replaces_a_stored_event_and_a_lost_start_is_still_drift(pool: PgPool) {
    let (id, mut accepted) = seed(&pool, "https://www.wikidata.org/wiki/Q1079").await;
    accepted.start = Some(chrono::Utc::now() + chrono::Duration::days(1));
    store::save_document(&pool, &id, 0, &accepted)
        .await
        .unwrap();
    // The same page read as a page without a start and without a decision: drift.
    let mut lost = ResourceMeta::new(&accepted.url, "web", 3600);
    lost.set_type("WebPage");
    let err = store::save_document(&pool, &id, 0, &lost)
        .await
        .unwrap_err();
    assert_eq!(
        err.downcast_ref::<pinakes_core::resolver::RetryLater>()
            .unwrap()
            .class,
        "drift"
    );
    // A decision that is still an event (the type path keeps Event) is no re-interpretation.
    let mut still = ResourceMeta::new(&accepted.url, "web", 3600);
    still.set_type("MusicEvent");
    still.event.not_event = Some("lists_events".into());
    let err = store::save_document(&pool, &id, 0, &still)
        .await
        .unwrap_err();
    assert_eq!(
        err.downcast_ref::<pinakes_core::resolver::RetryLater>()
            .unwrap()
            .class,
        "drift"
    );
    // Breaking Bad's item read as the series it is: the decision replaces the card.
    let series = pinakes_core::event::adapters::service::wiki_value(
        &accepted.url,
        &json!({"entities": {"Q1079": {"id": "Q1079", "labels": {"en": {"value": "Breaking Bad"}}, "claims": {
            "P31": [{"rank":"normal","mainsnak":{"datavalue":{"value":{"id":"Q5398426"}}}}],
            "P580": [{"rank":"normal","mainsnak":{"datavalue":{"value":{"time":"+2008-01-20T00:00:00Z","precision":11}}}}],
            "P582": [{"rank":"normal","mainsnak":{"datavalue":{"value":{"time":"+2013-09-29T00:00:00Z","precision":11}}}}]}}}}),
    )
    .unwrap();
    assert_eq!(series.event.not_event.as_deref(), Some("creative_work"));
    store::save_document(&pool, &id, 0, &series).await.unwrap();
    let (kind, path, start): (String, Vec<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT type,type_path,start_at FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind, "TVSeries");
    assert!(!path.iter().any(|t| t == "Event"), "{path:?}");
    assert!(start.is_none());
    // An artist's page stored as its first concert, re-read as the artist (the page lists
    // events): replaced too.
    let (id, mut concert) = seed(&pool, "https://www.songkick.com/artists/253846-radiohead").await;
    concert.start = Some(chrono::Utc::now() + chrono::Duration::days(3));
    store::save_document(&pool, &id, 0, &concert).await.unwrap();
    let mut artist = ResourceMeta::new(&concert.url, "jsonld", 3600);
    artist.set_type("MusicGroup");
    artist.name = Some("Radiohead".into());
    artist.event.not_event = Some("lists_events".into());
    store::save_document(&pool, &id, 0, &artist).await.unwrap();
    let (kind, start): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT type,start_at FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((kind.as_str(), start), ("MusicGroup", None));
}

/// A resource nothing ever referenced (one only previewed) goes after the preview grace; one that
/// lost its references keeps the full grace, and so do a derived child and a venue of the same age.
#[sqlx::test(migrations = "../../migrations")]
async fn a_preview_goes_after_its_own_grace(pool: PgPool) {
    let mut ids = std::collections::HashMap::new();
    for name in ["preview", "fresh", "dropped", "calendar", "child", "venue"] {
        let uri = format!("https://example.org/{name}");
        ids.insert(name, store::ensure_resource(&pool, &uri).await.unwrap());
    }
    let age = |name: &str, created: &str, orphaned: Option<&str>| {
        let id = ids[name].clone();
        let (created, orphaned) = (created.to_owned(), orphaned.map(str::to_owned));
        let pool = pool.clone();
        async move {
            sqlx::query("UPDATE resource SET created_at=now()-$2::interval, orphaned_at=now()-$3::interval WHERE id=$1")
                .bind(&id)
                .bind(&created)
                .bind(orphaned.as_deref().unwrap_or(&created))
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    // Never referenced: created and orphaned at the same instant.
    age("preview", "25 hours", None).await;
    age("fresh", "23 hours", None).await;
    age("child", "25 hours", None).await;
    age("venue", "25 hours", None).await;
    // Referenced once, its last reference gone 25 hours ago.
    age("dropped", "3 days", Some("25 hours")).await;
    // The calendar is referenced; it derives the child and is held at the venue.
    sqlx::query("UPDATE resource SET orphaned_at=NULL, venue_id=$2 WHERE id=$1")
        .bind(&ids["calendar"])
        .bind(&ids["venue"])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_derivation(parent_id,child_id,adapter) VALUES($1,$2,'ics')")
        .bind(&ids["calendar"])
        .bind(&ids["child"])
        .execute(&pool)
        .await
        .unwrap();
    let metrics = Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    // The preview and the fresh one; the child and the venue are kept for their calendar.
    assert_eq!(metrics.preview_backlog.get(), 2);
    assert_eq!(store::cleanup(&pool, 7, 24).await.unwrap(), 1);
    let left: Vec<String> = sqlx::query_scalar("SELECT id FROM resource")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(!left.contains(&ids["preview"]));
    for name in ["fresh", "dropped", "calendar", "child", "venue"] {
        assert!(left.contains(&ids[name]), "{name}");
    }
}

/// One cleanup run deletes every eligible resource, however many: it no longer stops at a thousand.
#[sqlx::test(migrations = "../../migrations")]
async fn one_cleanup_run_deletes_past_a_thousand(pool: PgPool) {
    sqlx::query("INSERT INTO resource(id,uri,created_at,orphaned_at) SELECT md5('p'||n),'https://example.org/p'||n,now()-interval '8 days',now()-interval '8 days' FROM generate_series(1,1250) n")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(changes::delete_orphans(&pool, 7).await.unwrap(), 1250);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// A cleanup told to stop (the daemon shutting down) ends after the batch in hand.
#[sqlx::test(migrations = "../../migrations")]
async fn a_stopped_cleanup_ends_after_its_batch(pool: PgPool) {
    sqlx::query("INSERT INTO resource(id,uri,created_at,orphaned_at) SELECT md5('s'||n),'https://example.org/s'||n,now()-interval '8 days',now()-interval '8 days' FROM generate_series(1,250) n")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        changes::delete_orphans_within(&pool, 7, 7 * 24, || true)
            .await
            .unwrap(),
        100
    );
    assert_eq!(changes::delete_orphans(&pool, 7).await.unwrap(), 150);
}

/// A preview nothing referenced goes after the preview grace, but what it held (a calendar's
/// derived events, its venue, its occurrences' venues) is not taken for a preview once it is
/// gone: however many cleanup runs follow, they keep the full grace, counted from then.
#[sqlx::test(migrations = "../../migrations")]
async fn what_a_preview_held_keeps_the_full_grace_after_it_goes(pool: PgPool) {
    let mut ids = std::collections::HashMap::new();
    for name in ["calendar", "child", "venue", "hall"] {
        let uri = format!("https://example.org/held/{name}");
        ids.insert(name, store::ensure_resource(&pool, &uri).await.unwrap());
    }
    // Never referenced, all created 25 hours ago.
    sqlx::query("UPDATE resource SET created_at=now()-interval '25 hours', orphaned_at=now()-interval '25 hours'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET venue_id=$2 WHERE id=$1")
        .bind(&ids["calendar"])
        .bind(&ids["venue"])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_derivation(parent_id,child_id,adapter) VALUES($1,$2,'ics')")
        .bind(&ids["calendar"])
        .bind(&ids["child"])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO occurrence(resource_id,uid,start_at,status,venue_id) VALUES($1,'u1',now()+interval '1 day','scheduled',$2)")
        .bind(&ids["calendar"])
        .bind(&ids["hall"])
        .execute(&pool)
        .await
        .unwrap();
    let left = || async {
        sqlx::query_scalar::<_, String>("SELECT id FROM resource")
            .fetch_all(&pool)
            .await
            .unwrap()
    };
    for _ in 0..3 {
        store::cleanup(&pool, 7, 24).await.unwrap();
    }
    let kept = left().await;
    assert!(!kept.contains(&ids["calendar"]));
    for name in ["child", "venue", "hall"] {
        assert!(kept.contains(&ids[name]), "{name}");
    }
    // They are ordinary orphans now: gone once the full grace has passed.
    sqlx::query("UPDATE resource SET orphaned_at=orphaned_at-interval '8 days'")
        .execute(&pool)
        .await
        .unwrap();
    store::cleanup(&pool, 7, 24).await.unwrap();
    assert!(left().await.is_empty());
}

/// A change line names its occurrence by the key as it is stored (`changes::stored_key`): a UID
/// that holds a link carrying a credential (a percent-encoded key, a link start split by a tab
/// or a newline included) is written by its stand-in, the same on every read, its recurrence id
/// as written. Every other key stays byte for byte.
#[test]
fn an_occurrence_key_names_no_credential_and_others_stay_as_written() {
    let secret = "https://cal.example/event?token=SECRET";
    let null_key = format!("[{},null]", serde_json::to_string(secret).unwrap());
    for key in [
        serde_json::to_string(&(secret, "")).unwrap(),
        null_key.clone(),
        serde_json::to_string(&("ht\ttps://cal.example/e?%74oken=SECRET", "r")).unwrap(),
        serde_json::to_string(&("web\ncal:cal.example/feed.ics?token=SECRET", "")).unwrap(),
    ] {
        let stored = changes::stored_key(&key);
        assert_ne!(stored, key);
        assert!(stored.starts_with("[\"uid:"), "{stored}");
        assert!(!stored.contains("SECRET"), "{stored}");
        assert_eq!(changes::stored_key(&stored), stored);
    }
    // A non-string recurrence id is kept as written.
    assert!(changes::stored_key(&null_key).ends_with(",null]"));
    for key in [
        serde_json::to_string(&("https://u@cal.example/e", "")).unwrap(),
        serde_json::to_string(&("plain", "")).unwrap(),
        format!("[\"uid:{}\",\"\"]", "0".repeat(64)),
    ] {
        assert_eq!(changes::stored_key(&key), key);
    }
}

/// Expediting moves an overdue resource to the front of the queue once: asked again, its row is
/// not written again (the same row version) and it keeps the place it took. A resource that is
/// fresh, final, dead-lettered or leased is left as it is.
#[sqlx::test(migrations = "../../migrations")]
async fn expediting_writes_an_overdue_row_once(pool: PgPool) {
    // The row's version (its inserting transaction and its place in the table) and its place
    // in the queue.
    async fn version(pool: &PgPool, id: &str) -> (String, String, Option<String>) {
        sqlx::query_as("SELECT xmin::text,ctid::text,priority_at::text FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }
    let id = store::ensure_resource(&pool, "https://example.org/overdue")
        .await
        .unwrap();
    let new = version(&pool, &id).await;
    assert_eq!(new.2, None);
    store::expedite(&pool, std::slice::from_ref(&id))
        .await
        .unwrap();
    let first = version(&pool, &id).await;
    assert!(first.2.is_some());
    assert_ne!((&first.0, &first.1), (&new.0, &new.1));
    for _ in 0..3 {
        store::expedite(&pool, std::slice::from_ref(&id))
            .await
            .unwrap();
        assert_eq!(version(&pool, &id).await, first);
    }
    let mut ids = vec![id];
    for (uri, state, expedited) in [
        (
            "https://example.org/fresh",
            "refresh_at=now()+interval '1 hour'",
            false,
        ),
        ("https://example.org/final", "final=true", false),
        ("https://example.org/dead", "dead_letter=true", false),
        (
            "https://example.org/leased",
            "lease_until=now()+interval '1 minute'",
            false,
        ),
        (
            "https://example.org/due",
            "refresh_at=now()-interval '1 second'",
            true,
        ),
        (
            "https://example.org/lapsed",
            "lease_until=now()-interval '1 second'",
            true,
        ),
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        sqlx::query(&format!("UPDATE resource SET {state} WHERE id=$1"))
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let before = version(&pool, &id).await;
        store::expedite(&pool, std::slice::from_ref(&id))
            .await
            .unwrap();
        let after = version(&pool, &id).await;
        assert_eq!(after.2.is_some(), expedited, "{uri}");
        assert_eq!(after != before, expedited, "{uri}");
        ids.push(id);
    }
    // The whole list in one call, as a lookup asks: no row is written again.
    let mut versions = vec![];
    for id in &ids {
        versions.push(version(&pool, id).await);
    }
    store::expedite(&pool, &ids).await.unwrap();
    for (id, was) in ids.iter().zip(versions) {
        assert_eq!(version(&pool, id).await, was, "{id}");
    }
}

fn canary_manifest(source: &str, canary: bool) -> adapter::Manifest {
    let mut manifest = adapter::manifest("web");
    manifest.source = source.into();
    manifest.canary = canary.then(|| {
        format!("https://{source}.example.org/sample")
            .parse()
            .unwrap()
    });
    manifest
}

/// One claim among a resolver's manifests takes the first canary that is due, in their order,
/// and holds its source so the next claim takes the next one. A source seen first is due an
/// interval later; nothing is claimed while canaries are off, and a manifest without a canary
/// gets no row.
#[sqlx::test(migrations = "../../migrations")]
async fn one_claim_takes_the_first_due_canary_among_its_manifests(pool: PgPool) {
    let config = CanaryConfig::default();
    // In the manifests' order, not the sources' alphabetical one.
    let manifests = [
        canary_manifest("mid", true),
        canary_manifest("zeta", true),
        canary_manifest("alpha", true),
        canary_manifest("bare", false),
    ];
    assert!(canary::claim_due(&pool, &manifests, &config)
        .await
        .unwrap()
        .is_none());
    let seeded: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT source,canary_due_at BETWEEN now()+interval '5 hours' AND now()+interval '7 hours',canary_generation=0 AND canary_lease_until IS NULL FROM source_state WHERE source IN ('mid','zeta','alpha','bare') ORDER BY source",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        seeded,
        [
            ("alpha".to_owned(), true, true),
            ("mid".to_owned(), true, true),
            ("zeta".to_owned(), true, true)
        ]
    );
    // A due time already set is never moved by another claim.
    sqlx::query("UPDATE source_state SET canary_due_at=now()-interval '1 minute' WHERE source IN ('zeta','alpha')")
        .execute(&pool)
        .await
        .unwrap();
    let off = CanaryConfig {
        enabled: false,
        ..Default::default()
    };
    assert!(canary::claim_due(&pool, &manifests, &off)
        .await
        .unwrap()
        .is_none());
    assert!(canary::claim_due(&pool, &manifests[3..], &config)
        .await
        .unwrap()
        .is_none());
    let claimed = |pool: PgPool, manifests: Vec<adapter::Manifest>| async move {
        canary::claim_due(&pool, &manifests, &CanaryConfig::default())
            .await
            .unwrap()
            .map(|(manifest, generation)| (manifest.source.clone(), generation))
    };
    let all = manifests.to_vec();
    assert_eq!(
        claimed(pool.clone(), all.clone()).await,
        Some(("zeta".to_owned(), 1))
    );
    assert_eq!(
        claimed(pool.clone(), all.clone()).await,
        Some(("alpha".to_owned(), 1))
    );
    assert_eq!(claimed(pool.clone(), all.clone()).await, None);
    // The one-manifest claim is the same claim: a held source is not claimed again.
    assert!(canary::claim(&pool, &manifests[1], &config)
        .await
        .unwrap()
        .is_none());
    // A lease that ran out frees its source, and the first manifest goes first once it is due.
    sqlx::query(
        "UPDATE source_state SET canary_lease_until=now()-interval '1 second' WHERE source='alpha'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE source_state SET canary_due_at=now() WHERE source='mid'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        claimed(pool.clone(), all.clone()).await,
        Some(("mid".to_owned(), 1))
    );
    assert_eq!(
        claimed(pool.clone(), all.clone()).await,
        Some(("alpha".to_owned(), 2))
    );
    assert_eq!(claimed(pool.clone(), all.clone()).await, None);
    // A recorded result frees the source and sets its next time.
    canary::record(
        &pool,
        &manifests[1],
        1,
        &Ok(ResourceMeta::new(
            "https://zeta.example.org/sample",
            "web",
            60,
        )),
        config.interval_secs,
    )
    .await
    .unwrap();
    let (ok, due_later, leased): (Option<bool>, bool, bool) = sqlx::query_as(
        "SELECT canary_ok,canary_due_at>now()+interval '5 hours',canary_lease_until IS NOT NULL FROM source_state WHERE source='zeta'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((ok, due_later, leased), (Some(true), true, false));
    // Workers claiming at once each take a source of their own.
    sqlx::query("UPDATE source_state SET canary_due_at=now(),canary_lease_until=NULL WHERE source IN ('mid','zeta','alpha')")
        .execute(&pool)
        .await
        .unwrap();
    let (a, b, c, d) = tokio::join!(
        claimed(pool.clone(), all.clone()),
        claimed(pool.clone(), all.clone()),
        claimed(pool.clone(), all.clone()),
        claimed(pool.clone(), all.clone()),
    );
    let mut sources: Vec<String> = [a, b, c, d].into_iter().flatten().map(|c| c.0).collect();
    sources.sort();
    assert_eq!(sources, ["alpha", "mid", "zeta"]);
}

/// A tick's canary bookkeeping does not grow with the resolver's manifests: one claim is two
/// statements for sixty of them, and one suspension is one. Counted as the transactions this
/// database committed.
#[sqlx::test(migrations = "../../migrations")]
async fn canary_bookkeeping_does_not_grow_with_the_manifests(pool: PgPool) {
    let connection = common::one_connection(&pool).await;
    let config = CanaryConfig::default();
    let manifests: Vec<adapter::Manifest> = (0..60)
        .map(|n| canary_manifest(&format!("platform{n}"), true))
        .collect();
    let sources: Vec<&str> = manifests.iter().map(|m| m.source.as_str()).collect();
    // The first claim seeds every source.
    assert!(canary::claim_due(&connection, &manifests, &config)
        .await
        .unwrap()
        .is_none());
    let commits = "SELECT xact_commit FROM pg_stat_database WHERE datname=current_database()";
    let before = common::counted(&connection, commits).await;
    for _ in 0..5 {
        canary::suspend(&connection, &sources).await.unwrap();
        assert!(canary::claim_due(&connection, &manifests, &config)
            .await
            .unwrap()
            .is_none());
    }
    // Fifteen statements and the two of the count itself; the bound leaves room for what other
    // backends of this database flush meanwhile. One statement per manifest and tick would be
    // three hundred.
    let committed = common::counted(&connection, commits).await - before;
    assert!((15..60).contains(&committed), "{committed} transactions");
    let seeded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM source_state WHERE source LIKE 'platform%' AND canary_due_at>now()",
    )
    .fetch_one(&connection)
    .await
    .unwrap();
    assert_eq!(seeded, 60);
}

/// A suspension clears the canary results of its sources alone, and writes no row that has
/// none.
#[sqlx::test(migrations = "../../migrations")]
async fn a_suspension_clears_its_sources_canary_results(pool: PgPool) {
    sqlx::query("INSERT INTO source_state(source,canary_ok,canary_failures,canary_missing,canary_lease_until,canary_due_at) VALUES('failing',false,3,'{name}',now()+interval '1 minute',now()),('passing',true,0,'{}',NULL,now()),('other',false,2,'{start}',NULL,now()),('quiet',NULL,0,'{}',now()+interval '1 minute',now())")
        .execute(&pool)
        .await
        .unwrap();
    // Each source: whether it has no canary result, whether it is held, and its row's version.
    let state = |pool: PgPool| async move {
        sqlx::query_as::<_, (String, bool, bool, String)>(
            "SELECT source,canary_ok IS NULL AND canary_failures=0 AND canary_missing='{}',canary_lease_until IS NOT NULL,xmin::text FROM source_state WHERE source IN ('failing','passing','other','quiet') ORDER BY source",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    let before = state(pool.clone()).await;
    let sources: Vec<&str> = before.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(sources, ["failing", "other", "passing", "quiet"]);
    canary::suspend(&pool, &[]).await.unwrap();
    assert_eq!(state(pool.clone()).await, before);
    canary::suspend(&pool, &["failing", "passing", "quiet", "unknown"])
        .await
        .unwrap();
    let after = state(pool.clone()).await;
    // A source with a result loses it, and its hold.
    for cleared in [&after[0], &after[2]] {
        assert_eq!((cleared.1, cleared.2), (true, false), "{}", cleared.0);
    }
    // Another source's result stays, and a source without one is not written (its hold stays).
    assert_eq!(after[1], before[1]);
    assert_eq!(after[3], before[3]);
    assert_eq!((after[3].1, after[3].2), (true, true));
}
