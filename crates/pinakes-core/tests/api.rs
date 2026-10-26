use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, Request, StatusCode},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
mod common;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, IngestConfig},
    ingest,
    model::{identity, ResourceMeta},
    nexus::{Change, Changes, FeedPage, Kind, Nexus, Op},
    queue, store,
    telemetry::Metrics,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tower::ServiceExt;

fn author(n: usize) -> String {
    let alphabet = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut v = vec![b'y'; 52];
    for i in 0..5 {
        v[50 - i] = alphabet[(n >> (5 * i)) & 31];
    }
    String::from_utf8(v).unwrap()
}
async fn seed(pool: &PgPool) -> Vec<String> {
    let saved = ingest::cursor(pool, "fixture").await.unwrap();
    let mut lines = vec![];
    let mut ids = vec![];
    for i in 0..5 {
        let (id, uri, _) = identity(&format!("https://example.org/item/{i}")).unwrap();
        ids.push(id.clone());
        for label in if i == 0 {
            vec!["rust", "music"]
        } else {
            vec!["rust"]
        } {
            lines.push(Change {
                op: Op::Put,
                uri: uri.clone(),
                kind: Kind::Tag,
                author: author(i),
                object_path: format!("/pub/app/tags/{i}-{label}"),
                label: Some(label.into()),
                at: 1_790_000_000_000,
                recurrence_id: None,
                status: None,
                start: None,
                end: None,
            });
        }
    }
    let next = lines.len() as i64;
    ingest::apply(
        pool,
        "fixture",
        &saved,
        &FeedPage {
            page: Changes {
                lines,
                next_cursor: next,
            },
            epoch: Some("fixture".into()),
            length: next,
        },
        &IngestConfig::default(),
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap();
    for (i, id) in ids.iter().enumerate() {
        let uri = format!("https://example.org/item/{i}");
        let mut doc = ResourceMeta::new(&uri, "web", 86400);
        doc.name = Some(if i == 0 {
            "Rust Music Evening".into()
        } else {
            format!("Rust Article {i}")
        });
        doc.description = Some("A reference document".into());
        doc.set_type(if i == 0 { "MusicEvent" } else { "Article" });
        if i == 0 {
            doc.start = Some("2026-10-20T18:00:00Z".parse().unwrap());
        }
        store::save_document(pool, id, 0, &doc).await.unwrap();
    }
    ids
}
async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    send(app, method, path, body, common::check_response).await
}
/// `call` on the admin listener, which has no OpenAPI document to check against.
async fn admin_call(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    send(app, method, path, body, |_, _, _, _| {}).await
}
async fn send(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    check: fn(&str, &str, u16, &Value),
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    // Every JSON response matches the schema its listener's document declares for it.
    if let Some(value) = &parsed {
        check(method, path, status.as_u16(), value);
    }
    let value = parsed.unwrap_or_else(|| json!({"body":String::from_utf8_lossy(&bytes)}));
    (status, value)
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn every_read_route_and_document_projection(pool: PgPool) {
    let ids = seed(&pool).await;
    let state = AppState::new(pool, Config::default()).unwrap();
    let app = api::router(state);
    let (code, info) = call(&app, "GET", "/v1/info", None).await;
    assert_eq!(code, 200);
    // What a client needs, nothing an operator does.
    let keys: Vec<&String> = info.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["limits", "osm_licence", "resolves", "user_agent", "version"],
        "{info}"
    );
    // The User-Agent its requests carry, so a harness can send the same: with no contact set,
    // without one.
    assert_eq!(
        info["user_agent"],
        format!("pinakes/{} (+https://pubky.app)", env!("CARGO_PKG_VERSION"))
    );
    assert!(info["resolves"]
        .as_array()
        .unwrap()
        .iter()
        .any(|k| k == "web"));
    assert_eq!(info["limits"]["lookup"], 500);
    assert_eq!(
        info["limits"]["requests_per_minute"],
        Config::default().api.requests_per_minute
    );
    let (code, spec) = call(&app, "GET", "/v1/openapi.json", None).await;
    assert_eq!(code, 200);
    assert!(spec["paths"]["/v1/resolve"]["post"].is_object());
    assert!(spec["components"]["schemas"]["Card"]["properties"]
        .get("score")
        .is_none());
    assert_eq!(spec["paths"].as_object().unwrap().len(), 15);
    // Swagger UI and client generators key operations by operationId: one each.
    let mut operation_ids = std::collections::BTreeSet::new();
    for item in spec["paths"].as_object().unwrap().values() {
        for operation in item.as_object().unwrap().values() {
            let id = operation["operationId"].as_str().unwrap();
            assert!(
                operation_ids.insert(id.to_owned()),
                "operationId `{id}` twice"
            );
        }
    }
    let (code, item) = call(&app, "GET", &format!("/v1/resources/{}", ids[0]), None).await;
    assert_eq!(code, 200);
    assert_eq!(item["type"], "MusicEvent");
    assert!(item.get("score").is_none());
    assert!(item.get("document").is_none());
    assert!(item["details"].is_object(), "{item}");
    assert_eq!(call(&app, "GET", "/v1/resources/bad", None).await.0, 400);
    assert_eq!(
        call(
            &app,
            "GET",
            "/v1/resources/00000000000000000000000000000000",
            None
        )
        .await
        .0,
        404
    );
    let (code, items) = call(
        &app,
        "POST",
        "/v1/resources/lookup",
        Some(json!({"ids":[ids[0],ids[0],"00000000000000000000000000000000"]})),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(items.as_array().unwrap().len(), 2);
    assert_eq!(items[0]["id"], ids[0]);
    assert!(items[0].get("details").is_none());
    assert!(items
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item.get("score").is_none()));
    let (_, items) = call(
        &app,
        "POST",
        "/v1/resources/lookup",
        Some(json!({"uris":["https://EXAMPLE.org:443/item/0#ignored"]})),
    )
    .await;
    assert_eq!(items[0]["id"], ids[0]);
    assert!(items[0].get("score").is_none());
    for body in [
        json!({}),
        json!({"ids":[]}),
        json!({"ids":[ids[0]],"uris":[]}),
        json!({"ids":["bad"]}),
        json!({"uris":["invalid"]}),
        json!({"ids":vec![ids[0].clone();501]}),
    ] {
        assert_eq!(
            call(&app, "POST", "/v1/resources/lookup", Some(body))
                .await
                .0,
            400
        );
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn search_filters_facets_true_keyset_and_invalid_options(pool: PgPool) {
    let ids = seed(&pool).await;
    // Force ties to test the ID tie-breaker, with one common decay baseline.
    sqlx::query("UPDATE resource_score SET score=1,updated_at='2026-01-01'")
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, page) = call(
        &app,
        "GET",
        "/v1/search?type=Event&tags=rust,music&match=all&facet=tags",
        None,
    )
    .await;
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], ids[0]);
    assert!(page["items"][0].get("score").is_none());
    assert_eq!(page["facets"]["tags"].as_array().unwrap().len(), 2);
    assert!(page["facets"].get("type").is_none());
    // Card types over the whole filtered set, most frequent first; both facets at once.
    let (_, page) = call(&app, "GET", "/v1/search?facet=type&limit=1", None).await;
    // Each entry says where its type sits, from Thing down, for a client's type tree.
    assert_eq!(
        page["facets"]["type"],
        json!([
            {"type":"Article","type_path":["Thing","CreativeWork","Article"],"count":4},
            {"type":"MusicEvent","type_path":["Thing","Event","MusicEvent"],"count":1}
        ])
    );
    assert!(page["facets"].get("tags").is_none());
    // Several types: any of them, each with its subtypes; one type as before.
    for (types, found) in [
        ("MusicEvent,Article", 5),
        ("Event,Book", 1),
        ("CreativeWork", 4),
        ("Article, Article", 4),
        ("Book,Place", 0),
    ] {
        let (code, page) = call(
            &app,
            "GET",
            &format!("/v1/search?type={}&limit=50", types.replace(' ', "%20")),
            None,
        )
        .await;
        assert_eq!(code, 200, "{types}: {page}");
        assert_eq!(page["items"].as_array().unwrap().len(), found, "{types}");
    }
    let eleven = ["Article"; 11].join(",");
    for bad in [eleven.as_str(), "Article,", "Article,NotAType", ","] {
        let (code, page) = call(&app, "GET", &format!("/v1/search?type={bad}"), None).await;
        assert_eq!(code, 400, "{bad}: {page}");
    }
    let (_, page) = call(&app, "GET", "/v1/search?type=Article,NotAType", None).await;
    assert!(page["error"].as_str().unwrap().contains("NotAType"));
    let (_, page) = call(&app, "GET", "/v1/search?facet=type,tags", None).await;
    assert!(page["facets"]["type"].is_array() && page["facets"]["tags"].is_array());
    assert_eq!(
        call(&app, "GET", "/v1/search?facet=host", None).await.0,
        400
    );
    let (_, page) = call(&app, "GET", "/v1/search?q=Even&facet=tags", None).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let (_, page) = call(
        &app,
        "GET",
        &format!("/v1/search?author={}", author(3)),
        None,
    )
    .await;
    assert_eq!(page["items"][0]["id"], ids[3]);
    // `scheme` is not a filter: `type` covers it.
    assert_eq!(
        call(&app, "GET", "/v1/search?scheme=nostr", None).await.0,
        400
    );
    for sort in ["activity", "recent"] {
        let mut path = format!("/v1/search?limit=2&facet=tags&sort={sort}");
        let mut seen = vec![];
        loop {
            let (code, page) = call(&app, "GET", &path, None).await;
            assert_eq!(code, 200, "{page}");
            assert_eq!(page["facets"]["tags"][0]["count"], 5);
            assert!(page["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item.get("score").is_none()));
            seen.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["id"].as_str().unwrap().to_owned()),
            );
            match page["cursor"].as_str() {
                Some(cursor) => {
                    path = format!("/v1/search?limit=2&facet=tags&sort={sort}&cursor={cursor}")
                }
                None => break,
            }
        }
        assert_eq!(seen.len(), 5);
        let mut expected = ids.clone();
        expected.sort_by(|a, b| b.cmp(a));
        assert_eq!(seen, expected, "internal rank/ID tie-breaking is unchanged");
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 5);
    }
    let (_, page) = call(&app, "GET", "/v1/search?limit=2", None).await;
    let cursor = page["cursor"].as_str().unwrap();
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("/v1/search?type=Event&cursor={cursor}"),
            None
        )
        .await
        .0,
        400
    );
    for query in [
        "bbox=0,0,1",
        "near=1,2",
        "radius=100",
        "from=2026-01-01",
        "to=2026-02-01",
        "cluster=3",
        "venue=x",
        "collapse=invalid",
        "reach=wot_2",
        "reach=following",
        "reach=strangers",
        "sort=distance",
        "sort=unknown",
        "limit=0",
        "limit=501",
        "match=bad",
        "tags=a,b,c,d&match=all",
        "tags=a,b,c,d,e,f",
        "tags=",
        "facet=bad",
        "type=Invented",
        "author=bad",
        "cursor=broken",
        "unknown=true",
    ] {
        let (code, v) = call(&app, "GET", &format!("/v1/search?{query}"), None).await;
        assert_eq!(code, 400, "{query}: {v}");
    }
}
#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    lines: Vec<Change>,
}
async fn following(
    State(state): State<Mock>,
    Query(q): Query<HashMap<String, usize>>,
) -> Json<Vec<String>> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let skip = q["skip"];
    assert_eq!(q["limit"], 200);
    Json((skip..(skip + 200).min(1001)).map(author).collect())
}
async fn friends(
    State(state): State<Mock>,
    Query(q): Query<HashMap<String, usize>>,
) -> Json<Vec<String>> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(q["limit"], 200);
    Json(vec![author(0)])
}
async fn changes(
    State(state): State<Mock>,
    Query(q): Query<HashMap<String, usize>>,
) -> (HeaderMap, Json<Changes>) {
    let cursor = q["cursor"];
    assert_eq!(q["limit"], 1000);
    let lines = state
        .lines
        .iter()
        .skip(cursor)
        .take(1000)
        .cloned()
        .collect::<Vec<_>>();
    let next_cursor = (cursor + lines.len()) as i64;
    let mut headers = HeaderMap::new();
    headers.insert("x-events-epoch", "fixture-epoch".parse().unwrap());
    headers.insert(
        "x-events-length",
        state.lines.len().to_string().parse().unwrap(),
    );
    (headers, Json(Changes { lines, next_cursor }))
}
async fn mock_nexus(lines: Vec<Change>) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v0/user/{id}/following", get(following))
        .route("/v0/user/{id}/friends", get(friends))
        .route("/v0/resources/changes", get(changes))
        .with_state(Mock {
            calls: calls.clone(),
            lines,
        });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls, task)
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn nexus_reach_paginates_caches_and_scopes_tag_facets(pool: PgPool) {
    seed(&pool).await;
    let (nexus_url, calls, task) = mock_nexus(vec![]).await;
    let app = api::router(
        AppState::new(
            pool,
            Config {
                nexus_url,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let path = format!(
        "/v1/search?reach=following&viewer={}&facet=tags",
        author(9999)
    );
    let (code, response) = call(&app, "GET", &path, None).await;
    assert_eq!(code, 200, "{response}");
    assert_eq!(response["reach_truncated"], true);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    call(&app, "GET", &path, None).await;
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    let (_, page) = call(
        &app,
        "GET",
        &format!(
            "/v1/search?reach=friends&viewer={}&tags=music&facet=tags",
            author(9999)
        ),
        None,
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["reach_truncated"], false);
    assert_eq!(calls.load(Ordering::SeqCst), 7);
    let (_, page) = call(
        &app,
        "GET",
        &format!(
            "/v1/search?reach=friends&viewer={}&author={}",
            author(9999),
            author(3)
        ),
        None,
    )
    .await;
    assert!(page["items"].as_array().unwrap().is_empty());
    task.abort();
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn polls_the_http_feed_and_replays_without_duplicates(pool: PgPool) {
    let (_, uri, _) = identity("https://example.org/feed").unwrap();
    let line = Change {
        op: Op::Put,
        uri,
        kind: Kind::Tag,
        author: author(0),
        object_path: "/pub/app/tags/one".into(),
        label: None,
        at: Utc::now().timestamp_millis(),
        recurrence_id: None,
        status: None,
        start: None,
        end: None,
    };
    let (url, _, task) = mock_nexus(vec![line.clone(), line]).await;
    let nexus = Nexus::new(&url).unwrap();
    assert_eq!(
        ingest::poll_once(
            &pool,
            &url,
            &nexus,
            &IngestConfig::default(),
            &Metrics::new().unwrap()
        )
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        ingest::poll_once(
            &pool,
            &url,
            &nexus,
            &IngestConfig::default(),
            &Metrics::new().unwrap()
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reference")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    task.abort();
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn preview_pending_cached_completion_and_per_ip_uri_budgets(pool: PgPool) {
    let config = Config {
        preview: pinakes_core::config::PreviewConfig {
            requests_per_minute: 2,
            uri_requests_per_minute: 2,
            cache_capacity: 100,
        },
        ..Default::default()
    };
    let state = AppState::new(pool.clone(), config.clone()).unwrap();
    let app = api::router(state);
    let start = std::time::Instant::now();
    let (code, preview) = call(
        &app,
        "POST",
        "/v1/resolve",
        Some(json!({"uri":"https://example.org/preview"})),
    )
    .await;
    assert_eq!(code, 202);
    assert_eq!(preview["status"], "pending");
    assert!(preview.get("score").is_none());
    assert!(start.elapsed() >= std::time::Duration::from_secs(3));
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    let job = queue::claim(&pool, "web", &config.queue)
        .await
        .unwrap()
        .unwrap();
    let mut doc = ResourceMeta::new(&job.uri, "web", 86400);
    doc.name = Some("A preview".into());
    store::save_document(&pool, &job.id, job.generation, &doc)
        .await
        .unwrap();
    let (code, preview) = call(
        &app,
        "POST",
        "/v1/resolve",
        Some(json!({"uri":"https://example.org/preview#same"})),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(preview["name"], "A preview");
    assert!(preview.get("score").is_none());
    assert!(queue::claim(&pool, "web", &config.queue)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/resolve",
            Some(json!({"uri":"https://example.org/another"}))
        )
        .await
        .0,
        429
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/resolve",
            Some(json!({"uri":"urn:unsupported"}))
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(&app, "POST", "/v1/resolve", Some(json!({"uri":"invalid"})))
            .await
            .0,
        400
    );
    // A link pasted with its quotes is refused with the reason, never the input: its user info,
    // query and fragment stay out of the answer.
    let (code, answer) = call(
        &app,
        "POST",
        "/v1/resolve",
        Some(json!({"uri":"\"https://user:CANARY@example.org/p?token=CANARY#CANARY\""})),
    )
    .await;
    assert_eq!(code, 400);
    assert!(!answer.to_string().contains("CANARY"), "{answer}");
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/resolve")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"uri":"https://example.org/preview"}).to_string(),
        ))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.3:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "60");
    // A preview with no references is never included in public ranking search.
    let (_, search) = call(&app, "GET", "/v1/search", None).await;
    assert!(search["items"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn preview_returns_completion_within_synchronous_window(pool: PgPool) {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let worker = tokio::spawn(async move {
        loop {
            if let Some(job) = queue::claim(&pool, "web", &Default::default())
                .await
                .unwrap()
            {
                let mut doc = ResourceMeta::new(&job.uri, "web", 100);
                doc.name = Some("Fast card".into());
                store::save_document(&pool, &job.id, job.generation, &doc)
                    .await
                    .unwrap();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
    let start = std::time::Instant::now();
    let (code, body) = call(
        &app,
        "POST",
        "/v1/resolve",
        Some(json!({"uri":"https://example.org/fast"})),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["name"], "Fast card");
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
    worker.await.unwrap();
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn public_budget_covers_reads_and_evicts_old_ips(pool: PgPool) {
    let config = Config {
        api: pinakes_core::config::ApiConfig {
            requests_per_minute: 2,
            cache_capacity: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    let app = api::router(AppState::new(pool, config).unwrap());
    async fn read_from(app: &Router, path: &str, peer: &str) -> u16 {
        let mut r = Request::builder().uri(path).body(Body::empty()).unwrap();
        r.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
        app.clone().oneshot(r).await.unwrap().status().as_u16()
    }
    assert_eq!(read_from(&app, "/v1/info", "127.0.0.1:1").await, 200);
    assert_eq!(
        read_from(&app, "/v1/openapi.json", "127.0.0.1:2").await,
        200
    );
    assert_eq!(read_from(&app, "/v1/search", "127.0.0.1:3").await, 429);
    assert_eq!(read_from(&app, "/v1/info", "127.0.0.2:1").await, 200);
    assert_eq!(read_from(&app, "/v1/info", "127.0.0.3:1").await, 200);
    assert_eq!(
        read_from(&app, "/v1/info", "127.0.0.1:1").await,
        200,
        "oldest IP is evicted instead of denying all new clients"
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn api_pool_enforces_query_timeout(pool: PgPool) {
    let api_pool = api::pool_options()
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SHOW statement_timeout")
            .fetch_one(&api_pool)
            .await
            .unwrap(),
        "5s"
    );
    let start = std::time::Instant::now();
    let error = sqlx::query("SELECT pg_sleep(6)")
        .execute(&api_pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("57014")
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(6));
    api_pool.close().await;
}

/// A request body over 2 MB is refused before any handler reads it.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_body_over_two_megabytes_is_refused(pool: PgPool) {
    let app = api::router(AppState::new(pool, Config::default()).unwrap());
    let uris: Vec<String> = (0..1000)
        .map(|n| format!("https://example.org/{n}/{}", "x".repeat(2100)))
        .collect();
    let body = json!({ "uris": uris });
    assert!(body.to_string().len() > 2 * 1024 * 1024);
    let (code, _) = call(&app, "POST", "/v1/resources/lookup", Some(body)).await;
    assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
}

/// A query or body axum cannot read is refused with what the route takes, never the input: a
/// link given as a parameter's name, as a value of the wrong type or as a bare JSON string to
/// `/v1/resolve` (Codex's case) stays out of the answer.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_rejected_request_quotes_no_input(pool: PgPool) {
    let app = api::router(AppState::new(pool, Config::default()).unwrap());
    let link = "HTTPS://user:CANARY@example.org/calendar?token=QUERYCANARY#FRAGMENTCANARY";
    let canary = |answer: &Value| {
        let text = answer.to_string();
        assert!(
            !text.contains("CANARY") && !text.contains("example.org"),
            "{text}"
        );
    };
    let name: String =
        percent_encoding::utf8_percent_encode(link, percent_encoding::NON_ALPHANUMERIC).to_string();
    let (code, answer) = call(&app, "GET", &format!("/v1/search?{name}=1"), None).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(
        answer["error"]
            .as_str()
            .unwrap()
            .contains("query parameter"),
        "{answer}"
    );
    canary(&answer);
    let (code, answer) = call(&app, "POST", "/v1/resolve", Some(json!(link))).await;
    assert!(code.is_client_error(), "{code}");
    assert_eq!(
        answer["error"],
        "the body must be a JSON object with a `uri` string"
    );
    canary(&answer);
    let (code, answer) = call(
        &app,
        "POST",
        "/v1/resources/lookup",
        Some(json!({ "uris": link })),
    )
    .await;
    assert!(code.is_client_error(), "{code}");
    assert!(
        answer["error"].as_str().unwrap().contains("`uris`"),
        "{answer}"
    );
    canary(&answer);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reach_searches_pay_for_upstream_pages_before_fetching(pool: PgPool) {
    let (nexus_url, calls, task) = mock_nexus(vec![]).await;
    let mut config = Config {
        nexus_url,
        ..Default::default()
    };
    config.api.requests_per_minute = 7;
    let app = api::router(AppState::new(pool, config).unwrap());
    let path = format!("/v1/search?%72each=following&viewer={}", author(0));
    assert_eq!(call(&app, "GET", &path, None).await.0, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    let rotated = format!("/v1/search?reach=following&viewer={}", author(1));
    assert_eq!(call(&app, "GET", &rotated, None).await.0, 429);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "rejected before Nexus fetch"
    );
    assert_eq!(call(&app, "GET", "/v1/info", None).await.0, 200);
    assert_eq!(call(&app, "GET", "/v1/search", None).await.0, 429);
    task.abort();
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn related_searches_cost_their_own_units_and_with_reach_both(pool: PgPool) {
    let seed = "0".repeat(32);
    let mut config = Config::default();
    config.api.requests_per_minute = 7;
    let app = api::router(AppState::new(pool.clone(), config).unwrap());
    let path = format!("/v1/search?%72elated={seed}");
    assert_eq!(call(&app, "GET", &path, None).await.0, 200);
    assert_eq!(call(&app, "GET", &path, None).await.0, 429);
    assert_eq!(call(&app, "GET", "/v1/info", None).await.0, 200);
    assert_eq!(call(&app, "GET", "/v1/search", None).await.0, 429);
    // With reach too, the search costs both: 12 units.
    let (nexus_url, calls, task) = mock_nexus(vec![]).await;
    let both = format!(
        "/v1/search?related={seed}&reach=following&viewer={}",
        author(0)
    );
    for (budget, status, fetched) in [(11, 429, 0), (12, 200, 6)] {
        let mut config = Config {
            nexus_url: nexus_url.clone(),
            ..Default::default()
        };
        config.api.requests_per_minute = budget;
        let app = api::router(AppState::new(pool.clone(), config).unwrap());
        assert_eq!(call(&app, "GET", &both, None).await.0, status, "{budget}");
        assert_eq!(calls.load(Ordering::SeqCst), fetched, "{budget}");
    }
    task.abort();
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cursors_reject_payload_tampering_unsigned_forgery_and_another_daemon_key(pool: PgPool) {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    seed(&pool).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (_, first) = call(&app, "GET", "/v1/search?limit=2", None).await;
    let cursor = first["cursor"].as_str().unwrap();
    let signed = URL_SAFE_NO_PAD.decode(cursor).unwrap();
    let original: Value = serde_json::from_slice(&signed[32..]).unwrap();
    for (field, value) in [
        ("score", json!(99999)),
        ("id", json!("f".repeat(32))),
        ("at", json!("2025-01-01T00:00:00Z")),
        ("filter", json!("0".repeat(64))),
    ] {
        let mut edited = original.clone();
        edited[field] = value;
        let payload = serde_json::to_vec(&edited).unwrap();
        for tag in [
            signed[..32].to_vec(),
            blake3::hash(&payload).as_bytes().to_vec(),
        ] {
            let mut forged = tag;
            forged.extend_from_slice(&payload);
            let cursor = URL_SAFE_NO_PAD.encode(forged);
            assert_eq!(
                call(
                    &app,
                    "GET",
                    &format!("/v1/search?limit=2&cursor={cursor}"),
                    None
                )
                .await
                .0,
                400
            );
        }
    }
    let unsigned = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&original).unwrap());
    assert_eq!(
        call(&app, "GET", &format!("/v1/search?cursor={unsigned}"), None)
            .await
            .0,
        400
    );
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("/v1/search?limit=2&cursor={cursor}"),
            None
        )
        .await
        .0,
        200
    );
    let restarted = api::router(AppState::new(pool, Config::default()).unwrap());
    assert_eq!(
        call(
            &restarted,
            "GET",
            &format!("/v1/search?limit=2&cursor={cursor}"),
            None
        )
        .await
        .0,
        400
    );
}

/// The search ETag follows exactly what a reader can see. A write in flight changes neither the
/// results nor the ETag; its commit changes both. Two writers that commit in the other order
/// than they began each change it, and folding the counted writes changes nothing.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_in_flight_writer_changes_the_etag_only_when_it_commits(pool: PgPool) {
    let ids = seed(&pool).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let search = |etag: Option<String>| {
        let app = app.clone();
        async move {
            let mut request = Request::builder().uri("/v1/search?tags=rust");
            if let Some(etag) = etag {
                request = request.header("if-none-match", etag);
            }
            let mut request = request.body(Body::empty()).unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                "127.0.0.1:1".parse::<std::net::SocketAddr>().unwrap(),
            ));
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let etag = response.headers()["etag"].to_str().unwrap().to_owned();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (status, etag, String::from_utf8_lossy(&body).into_owned())
        }
    };
    let revision = || async {
        sqlx::query_scalar::<_, i64>("SELECT revision FROM search_revision")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let (_, first, _) = search(None).await;
    let start = revision().await;
    // The first writer to begin commits last.
    let mut early = pool.begin().await.unwrap();
    sqlx::query("UPDATE resource SET name='Early rename' WHERE id=$1")
        .bind(&ids[0])
        .execute(&mut *early)
        .await
        .unwrap();
    let mut late = pool.begin().await.unwrap();
    sqlx::query("UPDATE resource SET name='Late rename' WHERE id=$1")
        .bind(&ids[1])
        .execute(&mut *late)
        .await
        .unwrap();
    assert_eq!(
        search(Some(first.clone())).await.0,
        304,
        "nothing visible changed"
    );
    assert_eq!(revision().await, start);
    late.commit().await.unwrap();
    let (status, second, body) = search(Some(first.clone())).await;
    assert_eq!(status, 200);
    assert!(body.contains("Late rename") && !body.contains("Early rename"));
    assert_eq!(revision().await, start + 1);
    early.commit().await.unwrap();
    let (status, third, body) = search(Some(second.clone())).await;
    assert_eq!(status, 200, "the earlier writer's commit is a change too");
    assert!(body.contains("Early rename"));
    assert!(third != first && third != second);
    assert_eq!(revision().await, start + 2);
    // A fold keeps every reader's revision.
    store::compact_search_writes(&pool).await.unwrap();
    assert_eq!(revision().await, start + 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM search_write")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(search(Some(third)).await.0, 304);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reach_membership_changes_invalidate_etags_without_resource_writes(pool: PgPool) {
    seed(&pool).await;
    let expanded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = expanded.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        nexus_url: format!("http://{}", listener.local_addr().unwrap()),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v0/user/{id}/following",
                get(move || {
                    let flag = flag.clone();
                    async move {
                        Json(if flag.load(Ordering::SeqCst) {
                            vec![author(0), author(1)]
                        } else {
                            vec![author(0)]
                        })
                    }
                }),
            ),
        )
        .await
        .unwrap()
    });
    let path = format!("/v1/search?reach=following&viewer={}", author(999));
    let first = api::router(AppState::new(pool.clone(), config.clone()).unwrap());
    let mut request = Request::builder().uri(&path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:1".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = first.oneshot(request).await.unwrap();
    let etag = response.headers()["etag"].clone();
    assert_eq!(response.status(), 200);
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM search_revision")
        .fetch_one(&pool)
        .await
        .unwrap();
    expanded.store(true, Ordering::SeqCst);
    // A new state represents the next reach refresh after the normal five-minute cache expires.
    let refreshed = api::router(AppState::new(pool.clone(), config).unwrap());
    let mut request = Request::builder()
        .uri(path)
        .header("if-none-match", &etag)
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:1".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = refreshed.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_ne!(response.headers()["etag"], etag);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM search_revision")
            .fetch_one(&pool)
            .await
            .unwrap(),
        revision
    );
    server.abort();
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn admin_bearer_and_network_bind_policy(pool: sqlx::PgPool) {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let mut cfg = pinakes_core::config::Config {
        admin_listen: "0.0.0.0:9090".into(),
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
    cfg.admin_token = Some("review-only-operator-token".into());
    assert!(cfg.validate().is_ok());
    let app = pinakes_core::api::admin_router(pinakes_core::api::AppState::new(pool, cfg).unwrap());
    for credential in [
        None,
        Some("Bearer wrong"),
        Some("Basic review-only-operator-token"),
        Some("Bearer review-only-operator-token"),
    ] {
        for path in [
            "/v1/info",
            "/metrics",
            "/v1/groups/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let mut r = Request::builder().uri(path);
            if let Some(value) = credential {
                r = r.header("authorization", value);
            }
            let response = app
                .clone()
                .oneshot(r.body(Body::empty()).unwrap())
                .await
                .unwrap();
            if credential == Some("Bearer review-only-operator-token") {
                assert_ne!(response.status(), 401);
            } else {
                assert_eq!(response.status(), 401);
            }
        }
    }
}

async fn raw(
    app: &Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let body = if method == "GET" {
        Body::empty()
    } else {
        Body::from("{}")
    };
    let mut request = request.body(body).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.9:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    (parts.status, parts.headers, bytes)
}

fn refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref") {
                out.push(r.clone());
            }
            map.values().for_each(|v| refs(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
        _ => {}
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn swagger_ui_and_the_openapi_document_are_on_the_public_listener_only(pool: PgPool) {
    let token = "swagger-test-operator-token";
    let config = Config {
        admin_token: Some(token.into()),
        ..Default::default()
    };
    let state = AppState::new(pool.clone(), config.clone()).unwrap();
    let public = api::router(state.clone());
    let admin = api::admin_router(state);

    // Swagger UI is on by default, from the vendored bundle, under restrictive headers.
    let (code, headers, _) = raw(&public, "GET", "/swagger-ui", None).await;
    assert!(code.is_redirection());
    assert_eq!(headers["location"], "/swagger-ui/");
    let (code, headers, body) = raw(&public, "GET", "/swagger-ui/", None).await;
    assert_eq!(code, 200);
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert!(String::from_utf8(body).unwrap().contains("<html"));
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(csp.starts_with("default-src 'self';") && !csp.contains("unsafe"));
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let (code, _, body) = raw(&public, "GET", "/swagger-ui/swagger-initializer.js", None).await;
    assert_eq!(code, 200);
    let initializer = String::from_utf8(body).unwrap();
    assert!(initializer.contains("\"/v1/openapi.json\"") && !initializer.contains("petstore"));
    // No validator badge (an image from validator.swagger.io off localhost).
    assert!(
        initializer.contains("\"validatorUrl\": \"none\""),
        "{initializer}"
    );
    // Redirect, assets and asset 404s carry the headers; other public paths, 404s included, do not.
    for (path, expected) in [
        ("/swagger-ui", 303),
        ("/swagger-ui/swagger-ui.css", 200),
        ("/swagger-ui/swagger-initializer.js", 200),
        ("/swagger-ui/missing.js", 404),
    ] {
        let (code, headers, _) = raw(&public, "GET", path, None).await;
        assert_eq!(code.as_u16(), expected, "{path}");
        assert_eq!(
            headers["content-security-policy"],
            api::SWAGGER_UI_CSP,
            "{path}"
        );
        assert_eq!(headers["x-content-type-options"], "nosniff", "{path}");
    }
    for path in ["/", "/metrics", "/favicon.ico", "/swagger-uix", "/v1/info"] {
        let (_, headers, _) = raw(&public, "GET", path, None).await;
        assert!(headers.get("content-security-policy").is_none(), "{path}");
        assert!(headers.get("x-content-type-options").is_none(), "{path}");
    }
    // The CSP's style hashes are exactly those of the <style> elements the served bundle renders,
    // so a utoipa-swagger-ui bump that changes them fails here, not silently in browsers.
    let mut rendered = std::collections::BTreeSet::new();
    let (mut elements, mut literals) = (0, 0);
    for asset in ["swagger-ui-bundle.js", "swagger-ui-standalone-preset.js"] {
        let (code, _, body) = raw(&public, "GET", &format!("/swagger-ui/{asset}"), None).await;
        assert_eq!(code, 200, "{asset}");
        let js = String::from_utf8(body).unwrap();
        let needle = "createElement(\"style\",null,\"";
        // Every <style> element must be a plain literal the pattern sees; any other form (a
        // variable, props, a template) would escape the hash check.
        elements += js.matches("createElement(\"style\"").count();
        for (at, _) in js.match_indices(needle) {
            literals += 1;
            let css = &js[at + needle.len()..];
            let css = &css[..css.find('"').unwrap()];
            use base64::Engine;
            use sha2::Digest;
            rendered.insert(format!(
                "'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(css))
            ));
        }
    }
    let allowed: std::collections::BTreeSet<String> = api::SWAGGER_UI_CSP
        .split(';')
        .find_map(|d| d.trim().strip_prefix("style-src "))
        .unwrap()
        .split(' ')
        .filter(|s| s.starts_with("'sha256-"))
        .map(str::to_owned)
        .collect();
    assert!(!rendered.is_empty());
    assert_eq!(
        elements, literals,
        "a <style> element is not a plain string literal"
    );
    assert_eq!(rendered, allowed);

    // The document matches its snapshot (without the sources' own examples) and every $ref
    // resolves.
    let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/");
    let public_doc = serde_json::to_value(api::openapi_document()).unwrap();
    let snapshot_doc = api::openapi_snapshot();
    for (doc, file) in [(&snapshot_doc, "openapi.json")] {
        let snapshot: Value =
            serde_json::from_str(&std::fs::read_to_string(format!("{docs}{file}")).unwrap())
                .unwrap();
        assert_eq!(doc, &snapshot, "{file} is stale; run scripts/openapi");
        let mut found = vec![];
        refs(doc, &mut found);
        for r in found {
            let pointer = r.strip_prefix('#').expect("local $ref");
            assert!(doc.pointer(pointer).is_some(), "{file}: unresolved {r}");
        }
    }
    assert_eq!(
        call(&public, "GET", "/v1/openapi.json", None).await.1,
        public_doc
    );
    // The admin listener serves no document; its routes are in docs/api.md.
    assert_eq!(raw(&admin, "GET", "/v1/openapi.json", None).await.0, 401);
    assert_eq!(
        raw(&admin, "GET", "/v1/openapi.json", Some(token)).await.0,
        404
    );

    // "Try it out" can only target operations the listener serves: the router's own fallback
    // (404, empty body) and 405 never answer a documented operation.
    let id = "00000000000000000000000000000000";
    for (app, doc, bearer) in [(&public, &public_doc, None::<&str>)] {
        for (path, item) in doc["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys() {
                let uri = path.replace("{id}", id);
                let method = method.to_uppercase();
                let (code, _, body) = raw(app, &method, &uri, bearer).await;
                assert!(
                    code != 405 && !(code == 404 && body.is_empty()),
                    "{method} {path} is documented but not served ({code})"
                );
            }
        }
    }
    assert!(public_doc["paths"].get("/metrics").is_none());
    assert!(public_doc["paths"]["/v1/groups/{id}"].get("put").is_none());
    assert_eq!(raw(&public, "GET", "/metrics", None).await.0, 404);
    assert_eq!(
        raw(&public, "PUT", &format!("/v1/groups/{id}"), None)
            .await
            .0,
        405
    );

    // Switched off, the route is gone.
    let mut off = config;
    off.api.swagger_ui = false;
    let public = api::router(AppState::new(pool.clone(), off).unwrap());
    for path in [
        "/swagger-ui",
        "/swagger-ui/",
        "/swagger-ui/swagger-initializer.js",
    ] {
        let (code, headers, _) = raw(&public, "GET", path, None).await;
        assert_eq!(code, 404, "{path}");
        assert!(headers.get("content-security-policy").is_none(), "{path}");
    }
    assert_eq!(call(&public, "GET", "/v1/openapi.json", None).await.0, 200);

    // Swagger UI shares the per-IP budget of the /v1 routes.
    let mut tight = Config::default();
    tight.api.requests_per_minute = 2;
    let public = api::router(AppState::new(pool, tight).unwrap());
    assert_eq!(raw(&public, "GET", "/swagger-ui/", None).await.0, 200);
    assert_eq!(raw(&public, "GET", "/v1/openapi.json", None).await.0, 200);
    let (code, headers, _) = raw(&public, "GET", "/swagger-ui/", None).await;
    assert_eq!(code, 429);
    assert_eq!(headers["content-security-policy"], api::SWAGGER_UI_CSP);
    assert_eq!(headers["x-content-type-options"], "nosniff");
}

async fn cors_call(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap) {
    let mut request = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let mut request = request.body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.9:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    (response.status(), response.headers().clone())
}

/// CORS on the public listener only, for any origin by default, for listed origins,
/// or not at all; preflights spend the per-IP budget and 429s stay readable cross-origin.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cors_for_browser_clients_on_the_public_listener_only(pool: PgPool) {
    let app_with = |origins: Vec<&str>, per_minute: usize| {
        let mut config = Config::default();
        config.api.cors_allow_origins = origins.into_iter().map(str::to_owned).collect();
        config.api.requests_per_minute = per_minute;
        config.validate().unwrap();
        api::router(AppState::new(pool.clone(), config).unwrap())
    };
    let preflight = [
        ("origin", "https://pubky.app"),
        ("access-control-request-method", "POST"),
        ("access-control-request-headers", "content-type"),
    ];
    // Default: any origin, no credentials, Retry-After and ETag exposed.
    assert_eq!(Config::default().api.cors_allow_origins, ["*"]);
    let any = app_with(vec!["*"], 120);
    let (code, h) = cors_call(&any, "GET", "/v1/info", &[("origin", "https://pubky.app")]).await;
    assert_eq!(code, 200);
    assert_eq!(h["access-control-allow-origin"], "*");
    assert_eq!(h["access-control-expose-headers"], "retry-after, etag");
    assert!(h.get("access-control-allow-credentials").is_none());
    assert!(h.get_all("vary").iter().all(|v| v != "origin"));
    // In `*` mode a request without Origin gets the headers too, so a shared cache
    // filled by curl or a server still serves browsers.
    let (_, h) = cors_call(&any, "GET", "/v1/search?q=x", &[]).await;
    assert_eq!(h["access-control-allow-origin"], "*");
    assert_eq!(h["access-control-expose-headers"], "retry-after, etag");
    assert!(h.get_all("vary").iter().all(|v| v != "origin"));
    let (code, h) = cors_call(&any, "OPTIONS", "/v1/resolve", &preflight).await;
    assert_eq!(code, 204);
    assert_eq!(h["access-control-allow-origin"], "*");
    assert_eq!(h["access-control-allow-methods"], "GET, POST");
    assert_eq!(h["access-control-allow-headers"], "content-type");
    assert_eq!(h["access-control-max-age"], "600");
    // A list: the allowed origin is echoed with Vary: Origin; another origin gets nothing.
    let listed = app_with(vec!["https://pubky.app"], 120);
    let (_, h) = cors_call(
        &listed,
        "GET",
        "/v1/info",
        &[("origin", "https://pubky.app")],
    )
    .await;
    assert_eq!(h["access-control-allow-origin"], "https://pubky.app");
    assert!(h.get_all("vary").iter().any(|v| v == "origin"));
    let (_, h) = cors_call(
        &listed,
        "GET",
        "/v1/info",
        &[("origin", "https://evil.example")],
    )
    .await;
    assert!(h.get("access-control-allow-origin").is_none());
    assert!(h.get_all("vary").iter().any(|v| v == "origin"));
    let refused = [
        ("origin", "https://evil.example"),
        ("access-control-request-method", "POST"),
    ];
    let (code, h) = cors_call(&listed, "OPTIONS", "/v1/resolve", &refused).await;
    assert_eq!(code, 204);
    assert!(h.get("access-control-allow-origin").is_none());
    assert!(h.get("access-control-allow-methods").is_none());
    // Preflights share the per-IP budget, and the 429 is readable cross-origin.
    let tight = app_with(vec!["*"], 2);
    for _ in 0..2 {
        assert_eq!(
            cors_call(&tight, "OPTIONS", "/v1/resolve", &preflight)
                .await
                .0,
            204
        );
    }
    let (code, h) = cors_call(
        &tight,
        "GET",
        "/v1/info",
        &[("origin", "https://pubky.app")],
    )
    .await;
    assert_eq!(code, 429);
    assert!(h.contains_key("retry-after"));
    assert_eq!(h["access-control-allow-origin"], "*");
    assert_eq!(h["access-control-expose-headers"], "retry-after, etag");
    // Disabled: no CORS headers, and OPTIONS is not answered as a preflight.
    let off = app_with(vec![], 120);
    let (_, h) = cors_call(&off, "GET", "/v1/info", &[("origin", "https://pubky.app")]).await;
    assert!(h.get("access-control-allow-origin").is_none());
    let (code, h) = cors_call(&off, "OPTIONS", "/v1/resolve", &preflight).await;
    assert_ne!(code, 204);
    assert!(h.get("access-control-allow-methods").is_none());
    // The admin listener never sends CORS headers.
    let admin = api::admin_router(AppState::new(pool.clone(), Config::default()).unwrap());
    for (method, headers) in [
        ("GET", &[("origin", "https://pubky.app")][..]),
        ("OPTIONS", &preflight[..]),
    ] {
        let (_, h) = cors_call(&admin, method, "/v1/info", headers).await;
        assert!(h.get("access-control-allow-origin").is_none(), "{method}");
        assert!(h.get("access-control-allow-methods").is_none(), "{method}");
    }
    // Only "*" alone or exact origins are accepted.
    for bad in [
        vec!["*", "https://pubky.app"],
        vec!["https://pubky.app/"],
        vec!["https://pubky.app/path"],
        vec!["ftp://pubky.app"],
        vec!["pubky.app"],
        vec!["https://*.pubky.app"],
        vec!["https://pubky.*"],
    ] {
        let mut config = Config::default();
        config.api.cors_allow_origins = bad.iter().map(|s| s.to_string()).collect();
        assert!(config.validate().is_err(), "{bad:?}");
    }
}

/// Request instants Postgres cannot bind (outside UTC years 0001–9999) answer 400
/// `timestamp out of range` instead of 500, as integers and as RFC 3339.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn out_of_range_request_timestamps_answer_400(pool: PgPool) {
    let ids = seed(&pool).await;
    let app = api::router(AppState::new(pool, Config::default()).unwrap());
    let refused = |code: StatusCode, body: &Value, what: &str| {
        assert_eq!(code, 400, "{what}: {body}");
        assert!(
            body.to_string().contains("timestamp out of range"),
            "{what}: {body}"
        );
    };
    for value in [
        "-8000000000000000",
        "8000000000000000",
        "-62135596800001",
        "0000-01-01T00:00:00Z",
        "9999-12-31T23:59:59-05:00",
    ] {
        for key in ["from", "to"] {
            let (code, body) = call(&app, "GET", &format!("/v1/search?{key}={value}"), None).await;
            refused(code, &body, &format!("search {key}={value}"));
        }
        let since = value
            .parse::<i64>()
            .map_or_else(|_| json!(value), |n| json!(n));
        let (code, body) = call(
            &app,
            "POST",
            "/v1/changes/lookup",
            Some(json!({"resource_ids":[ids[0]],"since":since})),
        )
        .await;
        refused(code, &body, &format!("lookup since={since}"));
    }
    for value in ["0000-01-01T00:00:00Z", "9999-12-31T23:59:59-05:00"] {
        for key in ["from", "to"] {
            let (code, body) = call(
                &app,
                "GET",
                &format!("/v1/resources/{}/occurrences?{key}={value}", ids[0]),
                None,
            )
            .await;
            refused(code, &body, &format!("occurrences {key}={value}"));
        }
    }
    // The edges themselves are accepted.
    for value in [
        "0001-01-01T00:00:00Z",
        "9999-12-31T23:59:59Z",
        "-62135596800000",
    ] {
        let (code, body) = call(&app, "GET", &format!("/v1/search?from={value}"), None).await;
        assert_eq!(code, 200, "search from={value}: {body}");
    }
    let (code, body) = call(
        &app,
        "POST",
        "/v1/changes/lookup",
        Some(json!({"resource_ids":[ids[0]],"since":"0001-01-01T00:00:00Z"})),
    )
    .await;
    assert_eq!(code, 200, "{body}");
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn startup_accepts_the_installed_h3_extension(pool: PgPool) {
    pinakes_core::check_extensions(&pool).await.unwrap();
    // Installed but off this connection's search_path: reported as such, not as too old.
    let schema: String = sqlx::query_scalar(
        "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='h3'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let narrow = PgPool::connect_with(
        (*pool.connect_options())
            .clone()
            .options([("search_path", "pg_catalog")]),
    )
    .await
    .unwrap();
    let error = pinakes_core::check_extensions(&narrow)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!("in schema {schema}")) && error.contains("search_path"),
        "{error}"
    );
    assert!(!error.contains("Upgrade"), "{error}");
    narrow.close().await;
    // The function decides, whatever the version string says.
    let installed: String =
        sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname='h3'")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query(
        "ALTER FUNCTION h3_latlng_to_cell(point,integer) RENAME TO h3_latlng_to_cell_hidden",
    )
    .execute(&pool)
    .await
    .unwrap();
    let error = pinakes_core::check_extensions(&pool)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!("h3 extension {installed} is installed")),
        "{error}"
    );
    assert!(error.contains("ALTER EXTENSION h3 UPDATE"), "{error}");
}

async fn send_raw(
    app: &Router,
    method: &str,
    target: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(target);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let mut request = request
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.11:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
    )
}

/// A test clock for relative times and search cursors.
fn test_clock(at: DateTime<Utc>) -> (Arc<std::sync::Mutex<DateTime<Utc>>>, api::Clock) {
    let now = Arc::new(std::sync::Mutex::new(at));
    let read = now.clone();
    (now, Arc::new(move || *read.lock().unwrap()))
}

/// Every query parameter, path parameter and body field of
/// every public operation, taken from the served document, gets multibyte, emoji, U+FFFD and
/// invalid-UTF-8 values, including `x:`/`geo:` and coordinate-shaped ones, with the companions
/// each parameter requires so the value reaches its parser. Every answer is 2xx or 4xx, never a
/// panic; relative times answer 400 with the relative-time message.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn multibyte_input_never_panics_any_public_route(pool: PgPool) {
    let mut config = Config::default();
    config.api.requests_per_minute = 1_000_000;
    config.preview.requests_per_minute = 1_000_000;
    config.preview.uri_requests_per_minute = 1_000_000;
    let state = AppState::new(pool, config).unwrap();
    let panics = state.metrics.api_panics.clone();
    let app = api::router(state);
    let id = "0123456789abcdef0123456789abcdef";
    for (method, target, body) in [
        ("GET", "/v1/search?from=now%C3%A9", None),
        ("GET", "/v1/search?to=now%2B%C3%A9", None),
        ("GET", "/v1/search?from=now%2B1%F0%9F%98%80", None),
        ("GET", "/v1/search?from=now%FF", None),
        (
            "POST",
            "/v1/changes/lookup",
            Some(json!({"resource_ids":[id],"since":"nowé"})),
        ),
        (
            "POST",
            "/v1/changes/lookup",
            Some(json!({"resource_ids":[id],"since":"now-é"})),
        ),
    ] {
        let (status, body) = send_raw(&app, method, target, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {target}");
        assert!(
            body["error"].as_str().unwrap().contains("relative time"),
            "{method} {target}: {body}"
        );
    }
    let values = [
        "é",
        "😀",
        "nowé",
        "now+é",
        "now+1é",
        "now-😀",
        "a\u{FFFD}b",
        "日本語",
        "now 😀d",
        "x:é",
        "geo:😀,1",
        "46.0,é",
        "é,1,2,3",
    ];
    // Raw invalid UTF-8 in query strings (decoded to U+FFFD by the extractor).
    let raw_values = ["%FF", "now%FF", "now%2B1%FF", "%FF,1"];
    let doc = api::openapi_document();
    let encode = |v: &str| url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
    // Query parameters that only reach their parser with a companion.
    let companions = |name: &str| match name {
        "radius" => "&near=46.0,8.9",
        "near" => "&radius=1000",
        "cluster" => "&bbox=8.9,45.9,9.1,46.1",
        "reach" => "&viewer=yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
        "viewer" => "&reach=following",
        _ => "",
    };
    // Relative times exist on search (`from`, `to`) and the change lookup (`since`); the
    // occurrence window takes plain RFC 3339 and answers with the query extractor's message.
    let relative_field = |what: &str, name: &str| {
        (what.contains("/v1/search") && matches!(name, "from" | "to"))
            || (what.contains("/v1/changes/lookup") && name == "since")
    };
    let mut sent = 0;
    let check = |status: StatusCode, body: &Value, what: &str, name: &str, value: &str| {
        // 2xx or 4xx; 502 only where the request reaches the Nexus upstream (`reach=`).
        assert!(
            status.is_success()
                || status.is_client_error()
                || (status == StatusCode::BAD_GATEWAY && what.contains("reach=")),
            "{what}: {status} {body}"
        );
        if relative_field(what, name) && value.starts_with("now") {
            assert_eq!(status, StatusCode::BAD_REQUEST, "{what}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("relative time"),
                "{what}: {body}"
            );
        }
    };
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            let method = method.to_ascii_uppercase();
            let parameters = operation["parameters"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let fill = |except: Option<&str>, value: &str| {
                let mut target = path.clone();
                for p in &parameters {
                    if p["in"] == "path" {
                        let name = p["name"].as_str().unwrap();
                        let v = if Some(name) == except {
                            encode(value)
                        } else {
                            id.into()
                        };
                        target = target.replace(&format!("{{{name}}}"), &v);
                    }
                }
                target
            };
            let body_schema = {
                let schema = &operation["requestBody"]["content"]["application/json"]["schema"];
                match schema["$ref"].as_str() {
                    Some(r) => doc["components"]["schemas"][r.rsplit('/').next().unwrap()].clone(),
                    None => schema.clone(),
                }
            };
            let is_array = |schema: &Value| {
                schema["type"] == "array"
                    || schema["type"]
                        .as_array()
                        .is_some_and(|t| t.iter().any(|t| t == "array"))
            };
            // A minimal valid body for the operation, so query and path checks reach handlers.
            let base_body = body_schema["properties"].as_object().map(|properties| {
                let mut body = serde_json::Map::new();
                for name in body_schema["required"].as_array().into_iter().flatten() {
                    let name = name.as_str().unwrap();
                    let value = if is_array(&properties[name]) {
                        json!([id])
                    } else if name == "since" {
                        json!("now")
                    } else {
                        json!("https://example.org/")
                    };
                    body.insert(name.to_owned(), value);
                }
                if body.is_empty() {
                    // Exactly one of the optional lists (resources/lookup).
                    if let Some((name, _)) = properties.iter().find(|(_, s)| is_array(s)) {
                        body.insert(name.clone(), json!([id]));
                    }
                }
                body
            });
            for p in &parameters {
                let name = p["name"].as_str().unwrap();
                let targets = values.iter().map(|v| (v.to_string(), encode(v))).chain(
                    raw_values.iter().filter(|_| p["in"] == "query").map(|v| {
                        (
                            v.replace("%2B", "+").replace("%FF", "\u{FFFD}"),
                            v.to_string(),
                        )
                    }),
                );
                for (value, encoded) in targets {
                    let target = if p["in"] == "path" {
                        fill(Some(name), &value)
                    } else {
                        format!("{}?{name}={encoded}{}", fill(None, ""), companions(name))
                    };
                    let body = base_body.clone().map(Value::Object);
                    let (status, response) = send_raw(&app, &method, &target, body).await;
                    check(
                        status,
                        &response,
                        &format!("{method} {target}"),
                        name,
                        &value,
                    );
                    sent += 1;
                }
            }
            if let (Some(properties), Some(base)) =
                (body_schema["properties"].as_object(), &base_body)
            {
                for (field, schema) in properties {
                    for value in values {
                        let mut body = base.clone();
                        if !base.contains_key(field) && is_array(schema) {
                            // The other list goes, so exactly one list is sent.
                            body.retain(|k, _| !is_array(&properties[k.as_str()]));
                        }
                        body.insert(
                            field.clone(),
                            if is_array(schema) {
                                json!([value])
                            } else {
                                json!(value)
                            },
                        );
                        let target = fill(None, "");
                        let what = format!("{method} {target} {field}={value}");
                        let (status, response) =
                            send_raw(&app, &method, &target, Some(Value::Object(body))).await;
                        check(status, &response, &what, field, value);
                        // Text fields (strings, string lists, string-or-integer) must reach the
                        // handler; integer fields rightly refuse a string at the JSON shape.
                        let textual = is_array(schema)
                            || schema["oneOf"].is_array()
                            || schema["type"] == "string"
                            || schema["type"]
                                .as_array()
                                .is_some_and(|t| t.iter().any(|t| t == "string"));
                        if textual {
                            assert_ne!(
                                status,
                                StatusCode::UNPROCESSABLE_ENTITY,
                                "{what}: the value must reach the handler, not stop at JSON shape"
                            );
                        } else {
                            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{what}");
                        }
                        sent += 1;
                    }
                }
            }
        }
    }
    assert!(
        sent > 400,
        "only {sent} requests: the sweep lost its routes"
    );
    assert_eq!(panics.get(), 0, "no handler panicked");
}

/// Page two of a relative-window search resolves `now` at the
/// cursor's pinned instant, so an event that started between the two page fetches is not skipped.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn relative_windows_are_pinned_by_the_search_cursor(pool: PgPool) {
    let start: DateTime<Utc> = "2036-06-01T10:00:20Z".parse().unwrap();
    let (now, clock) = test_clock(start);
    let mut ids = vec![];
    for (i, offset) in [10i64, 20].into_iter().enumerate() {
        let uri = format!("https://events.example/soon-{i}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let mut doc = ResourceMeta::new(&uri, "web", 86400);
        doc.set_type("Event");
        doc.name = Some(format!("Soon {i}"));
        doc.start = Some(start + chrono::Duration::seconds(offset));
        doc.end = doc.start.map(|s| s + chrono::Duration::hours(1));
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        sqlx::query("INSERT INTO reference(resource_id,kind,author,label,object_path,created_at) VALUES($1,'tag',$2,'soon',$1,now())")
            .bind(&id)
            .bind("y".repeat(52))
            .execute(&pool)
            .await
            .unwrap();
        ids.push(id);
    }
    let mut state = AppState::new(pool, Config::default()).unwrap();
    state.clock = Some(clock);
    let app = api::router(state);
    let path = "/v1/search?type=Event&from=now&sort=start&limit=1";
    let (code, page) = call(&app, "GET", path, None).await;
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["items"][0]["id"], ids[0]);
    let cursor = page["cursor"].as_str().unwrap().to_owned();
    // A minute later `now` alone would resolve after both starts.
    *now.lock().unwrap() = start + chrono::Duration::minutes(1);
    let (code, next) = call(&app, "GET", &format!("{path}&cursor={cursor}"), None).await;
    assert_eq!(code, 200, "{next}");
    assert_eq!(
        next["items"][0]["id"], ids[1],
        "page two keeps page one's window"
    );
    let (_, fresh) = call(&app, "GET", path, None).await;
    assert_eq!(fresh["items"], json!([]), "a new query uses the new minute");
}

/// A relative `since` in /v1/changes/lookup keeps the first page's window on
/// later cursor pages instead of re-resolving `now`.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn change_lookup_pins_a_relative_since_per_cursor(pool: PgPool) {
    let minute: DateTime<Utc> = "2036-06-01T10:00:00Z".parse().unwrap();
    let uri = "https://example.org/changing";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    for second in [10, 20, 30] {
        sqlx::query("INSERT INTO resource_change(resource_id,class,fields,after,source,at) VALUES($1,'details',ARRAY['name'],$2,'web',$3)")
            .bind(&id)
            .bind(json!({"name": format!("v{second}")}))
            .bind(minute + chrono::Duration::seconds(second))
            .execute(&pool)
            .await
            .unwrap();
    }
    let (now, clock) = test_clock(minute + chrono::Duration::seconds(40));
    let mut state = AppState::new(pool, Config::default()).unwrap();
    state.clock = Some(clock);
    let app = api::router(state);
    let page = |cursor: Option<i64>| {
        let mut body = json!({"resource_ids":[id],"since":"now","limit":1});
        if let Some(c) = cursor {
            body["cursor"] = json!(c);
        }
        body
    };
    let (status, first) = send_raw(&app, "POST", "/v1/changes/lookup", Some(page(None))).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["lines"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_i64().unwrap();
    // The next minute: `now` alone would start after every line.
    *now.lock().unwrap() = minute + chrono::Duration::seconds(70);
    let (status, second) =
        send_raw(&app, "POST", "/v1/changes/lookup", Some(page(Some(cursor)))).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(
        second["lines"][0]["at"],
        (minute + chrono::Duration::seconds(20)).timestamp_millis(),
        "page two keeps the first page's window: {second}"
    );
    let (_, fresh) = send_raw(&app, "POST", "/v1/changes/lookup", Some(page(None))).await;
    assert_eq!(
        fresh["lines"],
        json!([]),
        "a new lookup uses the new minute"
    );
}

/// A panicking handler under the public layer stack answers the fixed 500
/// body with the CORS header and increments `pinakes_api_panics_total`.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_panic_backstop_answers_500_with_cors_and_counts(pool: PgPool) {
    let state = AppState::new(pool, Config::default()).unwrap();
    let panics = state.metrics.api_panics.clone();
    async fn boom() -> &'static str {
        panic!("test panic")
    }
    let app = api::with_public_layers(Router::new().route("/boom", get(boom)), state);
    let mut request = Request::builder()
        .uri("/boom")
        .header("origin", "https://pubky.app")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.12:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error":"internal server error"})
    );
    assert_eq!(panics.get(), 1);
}

/// The admin listener's backstop, behind the bearer check: without the
/// token a 401 and the handler never runs; with it a 500 with the fixed body, no CORS header,
/// and the panic counter incremented.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_admin_backstop_sits_behind_the_bearer_check(pool: PgPool) {
    let config = Config {
        admin_token: Some("admin-test-token".into()),
        ..Default::default()
    };
    let state = AppState::new(pool, config).unwrap();
    let panics = state.metrics.api_panics.clone();
    static RAN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    async fn boom() -> &'static str {
        RAN.fetch_add(1, Ordering::SeqCst);
        panic!("test panic")
    }
    let app = api::with_admin_layers(Router::new().route("/boom", get(boom)), state);
    let send = |token: Option<&'static str>| {
        let app = app.clone();
        async move {
            let mut request = Request::builder()
                .uri("/boom")
                .header("origin", "https://pubky.app");
            if let Some(t) = token {
                request = request.header("authorization", format!("Bearer {t}"));
            }
            let mut request = request.body(Body::empty()).unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                "127.0.0.13:1234".parse::<SocketAddr>().unwrap(),
            ));
            app.oneshot(request).await.unwrap()
        }
    };
    let refused = send(None).await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(RAN.load(Ordering::SeqCst), 0, "the handler never ran");
    assert_eq!(panics.get(), 0);
    let response = send(Some("admin-test-token")).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error":"internal server error"})
    );
    assert_eq!(RAN.load(Ordering::SeqCst), 1);
    assert_eq!(panics.get(), 1);
}

/// The cursor cap sits one second below the cursor line, so `at`/`seq`
/// inversions from concurrent inserts are absorbed.
#[test]
fn the_lookup_cursor_cap_absorbs_inversions() {
    let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
    let cursor = at("2036-06-01T10:00:10.500Z");
    assert_eq!(
        pinakes_core::change_api::capped_since(at("2036-06-01T10:01:00Z"), cursor),
        at("2036-06-01T10:00:09.500Z")
    );
    // A window already older than the cursor stays as it is.
    assert_eq!(
        pinakes_core::change_api::capped_since(at("2036-06-01T09:00:00Z"), cursor),
        at("2036-06-01T09:00:00Z")
    );
    // No second to subtract at the earliest instant; the cap is the cursor's time.
    assert_eq!(
        pinakes_core::change_api::capped_since(
            at("2036-06-01T10:01:00Z"),
            DateTime::<Utc>::MIN_UTC
        ),
        DateTime::<Utc>::MIN_UTC
    );
}

/// A card's attribution: OSM data and timezone boundaries derived from coordinates are ODbL, so
/// an event whose zone came from its coordinates carries the boundary data's attribution.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cards_carry_the_timezone_boundary_attribution(pool: PgPool) {
    let uri = "https://example.org/zoned-event";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.type_name = "Event".into();
    doc.type_path = vec!["Thing".into(), "Event".into()];
    doc.name = Some("Zoned".into());
    doc.start = Some("2036-06-01T10:00:00Z".parse().unwrap());
    doc.event.tz = Some("Europe/Zurich".into());
    doc.event.timezone_attribution = Some(pinakes_core::event::time::ATTRIBUTION.into());
    // A nameless organizer without a URL is no organizer, never `""`.
    doc.event.organizer = Some(pinakes_core::event::Organizer {
        name: " ".into(),
        url: None,
    });
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, card) = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    assert_eq!(code, 200, "{card}");
    assert_eq!(card["attribution"], pinakes_core::event::time::ATTRIBUTION);
    assert_eq!(card["details"]["tz"], "Europe/Zurich");
    assert!(card["details"]["organizer"].is_null(), "{card}");
    // A venue that is an OSM place supplies its own credit; both are kept.
    let mut placed = doc.clone();
    placed.attribution = Some("© OpenStreetMap contributors (ODbL)".into());
    placed.fetched_at += 1000;
    store::save_document(&pool, &id, 0, &placed).await.unwrap();
    let (_, card) = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    let credit = card["attribution"].as_str().unwrap();
    assert!(
        credit.contains("OpenStreetMap contributors (ODbL)"),
        "{credit}"
    );
    assert!(
        credit.contains(pinakes_core::event::time::ATTRIBUTION),
        "{credit}"
    );
}

/// The stored document, with its internal fields, is on the admin listener only; the public
/// card leaves those fields out.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_stored_document_is_an_admin_read(pool: PgPool) {
    let ids = seed(&pool).await;
    let state = AppState::new(pool, Config::default()).unwrap();
    let (public, admin) = (api::router(state.clone()), api::admin_router(state));
    let path = format!("/v1/resources/{}/document", ids[0]);
    let (code, doc) = admin_call(&admin, "GET", &path, None).await;
    assert_eq!(code, 200, "{doc}");
    assert_eq!(doc["source"], "web");
    assert!(doc["ttl_secs"].is_number(), "{doc}");
    // The public check refuses it: the operation is admin-only.
    assert!(!common::validate(&[common::document()], "GET", &path, 200, &doc).is_empty());
    assert_eq!(call(&public, "GET", &path, None).await.0, 404);
    let (_, card) = call(&public, "GET", &format!("/v1/resources/{}", ids[0]), None).await;
    for internal in ["source", "ttl_secs", "group_keys", "attributes", "warnings"] {
        assert!(card.get(internal).is_none(), "{internal}: {card}");
        assert!(
            card["details"].get(internal).is_none(),
            "{internal}: {card}"
        );
    }
    let unknown = "/v1/resources/00000000000000000000000000000000/document";
    assert_eq!(admin_call(&admin, "GET", unknown, None).await.0, 404);
    assert_eq!(
        admin_call(&admin, "GET", "/v1/resources/bad/document", None)
            .await
            .0,
        400
    );
}

/// A tracking variant is an alias: read by its ID or URI it answers with the root's card, its
/// tag counts for the root in search, and the root's changes are its changes.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_alias_reads_as_its_root_everywhere(pool: PgPool) {
    let clean = "https://example.org/talk";
    let variant = "https://example.org/talk?utm_source=newsletter&fbclid=x";
    let alias = store::ensure_resource(&pool, variant).await.unwrap();
    let root = identity(clean).unwrap().0;
    assert_ne!(alias, root);
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','talks','/pub/app/tags/t',now())")
        .bind(&alias)
        .bind(author(1))
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::recompute_scores(&mut tx, std::slice::from_ref(&alias))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // The page claims other URLs as itself (`@id`, `sameAs`), as Wikipedia's Lugano article does:
    // claims, not aliases.
    let page = r#"<title>Talk</title><script type="application/ld+json">{"@type":"Article","@id":"https://www.wikidata.org/entity/Q7024","name":"Talk","url":"https://example.org/talk","sameAs":["https://www.wikidata.org/entity/Q7024","https://www.lugano.ch/"]}</script>"#;
    let fetched = pinakes_core::resolver::egress::Fetched {
        permanent_redirect: false,
        truncated: false,
        validators: Default::default(),
        url: url::Url::parse(clean).unwrap(),
        status: 200,
        retry_after_secs: 60,
        content_type: "text/html".into(),
        location: None,
        body: page.as_bytes().into(),
    };
    let doc = pinakes_core::resolver::html::off_runtime(|| {
        pinakes_core::resolver::web::extract(clean, &fetched, 86400).unwrap()
    })
    .doc;
    assert_eq!(doc.name.as_deref(), Some("Talk"));
    store::save_document(&pool, &root, 0, &doc).await.unwrap();
    let pool_for_checks = pool.clone();
    let app = api::router(AppState::new(pool, Config::default()).unwrap());
    let (code, card) = call(&app, "GET", &format!("/v1/resources/{alias}"), None).await;
    assert_eq!(code, 200, "{card}");
    assert_eq!(
        (card["id"].as_str(), card["name"].as_str()),
        (Some(root.as_str()), Some("Talk"))
    );
    // Its aliases are the spellings that resolve to it, never the page's claims.
    assert_eq!(card["details"]["aliases"], json!([variant]), "{card}");
    for claimed in [
        "https://www.wikidata.org/entity/Q7024",
        "https://www.lugano.ch/",
    ] {
        let by = format!(
            "/v1/resources/by-uri?uri={}",
            url::form_urlencoded::byte_serialize(claimed.as_bytes()).collect::<String>()
        );
        assert_eq!(call(&app, "GET", &by, None).await.0, 404, "{claimed}");
    }
    let (_, cards) = call(
        &app,
        "POST",
        "/v1/resources/lookup",
        Some(json!({"uris":[variant]})),
    )
    .await;
    assert_eq!(cards[0]["id"], root.as_str());
    // By URI: the variant answers with its root's card, an unknown link is a 404 and nothing is
    // fetched, and the read carries an ETag that a revalidation answers with 304.
    let encode = |v: &str| url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
    let by = format!("/v1/resources/by-uri?uri={}", encode(variant));
    let (code, card) = call(&app, "GET", &by, None).await;
    assert_eq!(code, 200, "{card}");
    assert_eq!(card["id"], root.as_str());
    let unknown = format!(
        "/v1/resources/by-uri?uri={}",
        encode("https://example.org/never-seen")
    );
    assert_eq!(call(&app, "GET", &unknown, None).await.0, 404);
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM resource WHERE uri='https://example.org/never-seen')",
    )
    .fetch_one(&pool_for_checks)
    .await
    .unwrap();
    assert!(!known, "a by-uri read never creates or fetches a resource");
    assert_eq!(
        call(&app, "GET", "/v1/resources/by-uri?uri=x", None)
            .await
            .0,
        400
    );
    let send = |etag: Option<String>| {
        let app = app.clone();
        let by = by.clone();
        async move {
            let mut request = Request::builder().uri(&by);
            if let Some(etag) = etag {
                request = request.header("if-none-match", etag);
            }
            let mut request = request.body(Body::empty()).unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
            ));
            app.oneshot(request).await.unwrap()
        }
    };
    let first = send(None).await;
    assert_eq!(first.headers()["cache-control"], "public, max-age=30");
    let etag = first.headers()["etag"].to_str().unwrap().to_owned();
    assert_eq!(send(Some(etag)).await.status(), 304);
    let (_, found) = call(&app, "GET", "/v1/search?tags=talks", None).await;
    let ids: Vec<&str> = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![root.as_str()], "{found}");
    let (_, lines) = call(
        &app,
        "POST",
        "/v1/changes/lookup",
        Some(json!({"resource_ids":[alias],"since":"0001-01-01T00:00:00Z"})),
    )
    .await;
    // The alias's own line says which root answers for it, then the root's own lines follow.
    assert_eq!(
        lines["lines"][0],
        json!({"seq":lines["lines"][0]["seq"],"resource_id":alias,"at":lines["lines"][0]["at"],"class":"aliased","fields":["root"],"root":root}),
        "{lines}"
    );
    assert_eq!(lines["lines"][1]["resource_id"], root.as_str(), "{lines}");
    assert!(lines["lines"][1].get("root").is_none());
}

/// A Pubky object described by its content is social content: a 200-character excerpt that
/// refreshes within the hour, and once the object is confirmed gone the card loses the excerpt
/// and search no longer matches it.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_deleted_pubky_post_loses_its_excerpt(pool: PgPool) {
    let uri = format!("pubky://{}/pub/mapky.app/posts/0034A0X7NJ52G", author(7));
    let id = store::ensure_resource(&pool, &uri).await.unwrap();
    let body = json!({"content": format!("Sunrise over the lake {}", "x".repeat(400))});
    let doc = pinakes_core::resolver::pubky_apps::card(
        &uri,
        "application/json",
        body.to_string().as_bytes(),
        86400,
    );
    assert!(pinakes_core::resolver::cards::social(&doc));
    assert_eq!(doc.description.as_ref().unwrap().chars().count(), 200);
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','lake','/pub/app/tags/l',now())")
        .bind(&id)
        .bind(author(1))
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let found = |q: &'static str| {
        let app = app.clone();
        async move {
            call(&app, "GET", &format!("/v1/search?q={q}"), None)
                .await
                .1["items"]
                .as_array()
                .unwrap()
                .len()
        }
    };
    assert_eq!(found("sunrise").await, 1);
    let mut gone = ResourceMeta::new(&uri, "pubky", 3600);
    gone.status = pinakes_core::model::Status::Missing;
    for _ in 0..2 {
        gone.fetched_at += 1000;
        store::save_document(&pool, &id, 0, &gone).await.unwrap();
    }
    let (_, card) = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    assert_eq!(card["status"], "missing", "{card}");
    assert!(card["description"].is_null(), "{card}");
    assert_eq!(found("sunrise").await, 0);
}

/// A Mastodon post whose author opted out of search indexing keeps its card, but its text is
/// never found by `q=`, and its change history keeps no text: what was kept before goes, and the
/// opt-out line itself carries only its kind and time.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_opted_out_post_is_not_searched_and_keeps_no_history_text(pool: PgPool) {
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
    // The usual `/@{user}/{id}` spelling, saved with the full card before it became an alias of
    // the post's ActivityPub id.
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
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','hello','/pub/app/tags/h',now())")
        .bind(&id)
        .bind(author(1))
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let found = |q: &'static str| {
        let app = app.clone();
        async move {
            call(&app, "GET", &format!("/v1/search?q={q}"), None)
                .await
                .1["items"]
                .as_array()
                .unwrap()
                .len()
        }
    };
    assert_eq!(found("hello").await, 1);
    let texts = || {
        sqlx::query_scalar::<_, i64>("SELECT (SELECT count(*) FROM resource_change WHERE resource_id IN ($1,$2) AND (before::text ILIKE '%hello%' OR after::text ILIKE '%hello%' OR before::text ILIKE '%Rochko%' OR after::text ILIKE '%Rochko%')) + (SELECT count(*) FROM resource WHERE id=$2 AND (doc::text ILIKE '%hello%' OR doc::text ILIKE '%Rochko%' OR name ILIKE '%hello%'))")
            .bind(&id)
            .bind(&alias_id)
            .fetch_one(&pool)
    };
    assert!(texts().await.unwrap() > 1);

    // The author opts out: the card stays, its text is no longer searched or kept.
    let opted = card("status-1-opted-out.json", 2000).await.unwrap();
    assert_eq!(opted.attributes["opted_out"], "true");
    assert_eq!(opted.description.as_deref(), Some("Hello world"));
    store::save_document(&pool, &id, 0, &opted).await.unwrap();
    assert_eq!(found("hello").await, 0);
    assert_eq!(found("Gargron").await, 0);
    assert_eq!(texts().await.unwrap(), 0);
    let (lines, with_documents): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE before IS NOT NULL OR after IS NOT NULL) FROM resource_change WHERE resource_id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((lines, with_documents), (1, 0));
    // The alias keeps its `aliased` line (the root it points at) and nothing of the post.
    let alias_lines: Vec<(String, Option<serde_json::Value>)> = sqlx::query_as(
        "SELECT class, after FROM resource_change WHERE resource_id=$1 ORDER BY seq",
    )
    .bind(&alias_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        alias_lines
            .iter()
            .any(|(class, after)| class == "aliased" && after.is_some()),
        "{alias_lines:?}"
    );
    assert!(
        alias_lines
            .iter()
            .all(|(class, after)| class == "aliased" || after.is_none()),
        "{alias_lines:?}"
    );
    let (_, read) = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    assert_eq!(read["name"], "Hello world", "{read}");
}
