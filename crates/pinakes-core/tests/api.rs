use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, Request, StatusCode},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
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
                resource_id: id.clone(),
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
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"body":String::from_utf8_lossy(&bytes)}));
    (status, value)
}
#[sqlx::test(migrations = "../../migrations")]
async fn every_read_route_and_document_projection(pool: PgPool) {
    let ids = seed(&pool).await;
    let state = AppState::new(pool, Config::default()).unwrap();
    let app = api::router(state);
    let (code, info) = call(&app, "GET", "/v1/info", None).await;
    assert_eq!(code, 200);
    assert_eq!(info["counts"]["resources"], 5);
    assert_eq!(info["counts"]["references"], 6);
    let (code, spec) = call(&app, "GET", "/v1/openapi.json", None).await;
    assert_eq!(code, 200);
    assert!(spec["paths"]["/v1/resolve"]["post"].is_object());
    assert!(spec["components"]["schemas"]["ResourceView"]["properties"]
        .get("score")
        .is_none());
    assert_eq!(spec["paths"].as_object().unwrap().len(), 12);
    for field in ["card", "facets", "document"] {
        let (code, item) = call(
            &app,
            "GET",
            &format!("/v1/resources/{}?fields={field}", ids[0]),
            None,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(item["type"], "MusicEvent");
        assert!(item.get("score").is_none());
        assert_eq!(item.get("document").is_some(), field == "document");
        assert_eq!(item.get("type_path").is_some(), field != "card");
    }
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
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("/v1/resources/{}?fields=secrets", ids[0]),
            None
        )
        .await
        .0,
        400
    );
    let (code, items) = call(
        &app,
        "POST",
        "/v1/resources/lookup?fields=document",
        Some(json!({"ids":[ids[0],ids[0],"00000000000000000000000000000000"]})),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(items.as_array().unwrap().len(), 2);
    assert!(items[0]["document"].is_object());
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
    assert_eq!(items[0]["resource_id"], ids[0]);
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
#[sqlx::test(migrations = "../../migrations")]
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
        "/v1/search?type=Event&tags=rust,music&match=all&facet=tags&fields=facets",
        None,
    )
    .await;
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["resource_id"], ids[0]);
    assert!(page["items"][0].get("score").is_none());
    assert_eq!(page["facets"]["tags"].as_array().unwrap().len(), 2);
    let (_, page) = call(&app, "GET", "/v1/search?q=Even&facet=tags", None).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let (_, page) = call(
        &app,
        "GET",
        &format!("/v1/search?author={}", author(3)),
        None,
    )
    .await;
    assert_eq!(page["items"][0]["resource_id"], ids[3]);
    let (_, page) = call(&app, "GET", "/v1/search?scheme=nostr", None).await;
    assert!(page["items"].as_array().unwrap().is_empty());
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
                    .map(|i| i["resource_id"].as_str().unwrap().to_owned()),
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
async fn polls_the_http_feed_and_replays_without_duplicates(pool: PgPool) {
    let (resource_id, uri, _) = identity("https://example.org/feed").unwrap();
    let line = Change {
        op: Op::Put,
        resource_id,
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
#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
async fn slowly_consumed_dump_outlives_api_sql_timeout(pool: PgPool) {
    // Enough data to fill Postgres/the driver's buffers so SELECT remains open
    // as the consumer reads: Talos's 4,000 rows of roughly 6 KB at 2 ms/frame.
    sqlx::query("INSERT INTO osm_place(osm_type,osm_id,type,type_path,doc,fetched_at,refresh_at,status) SELECT 'node',n,'Place',ARRAY['Thing','Place'],jsonb_build_object('type','Place','name',repeat('x',6000)),now(),now(),'ok' FROM generate_series(1,4000) n")
        .execute(&pool).await.unwrap();
    let api_pool = api::pool_options()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let app = api::router(AppState::new(api_pool.clone(), Config::default()).unwrap());
    let mut request = Request::builder()
        .uri("/v1/dumps/osm_place.ndjson")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let start = std::time::Instant::now();
    let mut body = response.into_body();
    let mut lines = Vec::new();
    while let Some(frame) = body.frame().await {
        let bytes = frame
            .expect("slow but active dump must complete")
            .into_data()
            .unwrap();
        let row: Value = serde_json::from_slice(&bytes).unwrap();
        lines.push(row["osm_id"].as_i64());
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(start.elapsed() > std::time::Duration::from_secs(5));
    assert_eq!(lines[0], None, "first line is the licence notice");
    assert_eq!(lines[1..], (1..=4000).map(Some).collect::<Vec<_>>());
    // The transaction-local override must not relax subsequent API queries.
    assert_eq!(
        sqlx::query_scalar::<_, String>("SHOW statement_timeout")
            .fetch_one(&api_pool)
            .await
            .unwrap(),
        "5s"
    );
    api_pool.close().await;
}

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
async fn review_admin_bearer_and_network_bind_policy(pool: sqlx::PgPool) {
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

#[sqlx::test(migrations = "../../migrations")]
async fn swagger_ui_and_each_listener_documents_only_its_own_operations(pool: PgPool) {
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

    // Both documents match their snapshots and every $ref resolves.
    let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/");
    let public_doc = serde_json::to_value(api::openapi_document()).unwrap();
    let admin_doc = serde_json::to_value(api::admin_openapi_document()).unwrap();
    for (doc, file) in [
        (&public_doc, "openapi.json"),
        (&admin_doc, "openapi.admin.json"),
    ] {
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
    assert_eq!(raw(&admin, "GET", "/v1/openapi.json", None).await.0, 401);
    let (code, _, body) = raw(&admin, "GET", "/v1/openapi.json", Some(token)).await;
    assert_eq!(code, 200);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), admin_doc);
    assert!(admin_doc["components"]["securitySchemes"]["admin_bearer"].is_object());

    // "Try it out" can only target operations the listener serves: the router's own fallback
    // (404, empty body) and 405 never answer a documented operation.
    let id = "00000000000000000000000000000000";
    for (app, doc, bearer) in [
        (&public, &public_doc, None),
        (&admin, &admin_doc, Some(token)),
    ] {
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

/// Round 14 C1: CORS on the public listener only, for any origin by default, for listed origins,
/// or not at all; preflights spend the per-IP budget and 429s stay readable cross-origin.
#[sqlx::test(migrations = "../../migrations")]
async fn round14_cors_for_browser_clients_on_the_public_listener_only(pool: PgPool) {
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
    // Round 15 Z1: in `*` mode a request without Origin gets the headers too, so a shared cache
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

/// Round 21 H4: request instants Postgres cannot bind (outside UTC years 0001–9999) answer 400
/// `timestamp out of range` instead of 500, as integers and as RFC 3339.
#[sqlx::test(migrations = "../../migrations")]
async fn round21_out_of_range_request_timestamps_answer_400(pool: PgPool) {
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

#[sqlx::test(migrations = "../../migrations")]
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

/// A test clock for relative times and search cursors (round 6 S20).
fn test_clock(at: DateTime<Utc>) -> (Arc<std::sync::Mutex<DateTime<Utc>>>, api::Clock) {
    let now = Arc::new(std::sync::Mutex::new(at));
    let read = now.clone();
    (now, Arc::new(move || *read.lock().unwrap()))
}

/// Step 6 rounds 5–6 (S10, S11, S16, S18): every query parameter, path parameter and body field of
/// every public operation, taken from the served document, gets multibyte, emoji, U+FFFD and
/// invalid-UTF-8 values, including `x:`/`geo:` and coordinate-shaped ones, with the companions
/// each parameter requires so the value reaches its parser. Every answer is 2xx or 4xx, never a
/// panic; relative times answer 400 with the relative-time message.
#[sqlx::test(migrations = "../../migrations")]
async fn round5_multibyte_input_never_panics_any_public_route(pool: PgPool) {
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

/// Step 6 rounds 5–6 (S12, S20): page two of a relative-window search resolves `now` at the
/// cursor's pinned instant, so an event that started between the two page fetches is not skipped.
#[sqlx::test(migrations = "../../migrations")]
async fn round5_relative_windows_are_pinned_by_the_search_cursor(pool: PgPool) {
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
    assert_eq!(page["items"][0]["resource_id"], ids[0]);
    let cursor = page["cursor"].as_str().unwrap().to_owned();
    // A minute later `now` alone would resolve after both starts.
    *now.lock().unwrap() = start + chrono::Duration::minutes(1);
    let (code, next) = call(&app, "GET", &format!("{path}&cursor={cursor}"), None).await;
    assert_eq!(code, 200, "{next}");
    assert_eq!(
        next["items"][0]["resource_id"], ids[1],
        "page two keeps page one's window"
    );
    let (_, fresh) = call(&app, "GET", path, None).await;
    assert_eq!(fresh["items"], json!([]), "a new query uses the new minute");
}

/// Step 6 round 6 S19: a relative `since` in /v1/changes/lookup keeps the first page's window on
/// later cursor pages instead of re-resolving `now`.
#[sqlx::test(migrations = "../../migrations")]
async fn round6_change_lookup_pins_a_relative_since_per_cursor(pool: PgPool) {
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
        second["lines"][0]["after"]["name"], "v20",
        "page two keeps the first page's window: {second}"
    );
    let (_, fresh) = send_raw(&app, "POST", "/v1/changes/lookup", Some(page(None))).await;
    assert_eq!(
        fresh["lines"],
        json!([]),
        "a new lookup uses the new minute"
    );
}

/// Step 6 round 6 S17: a panicking handler under the public layer stack answers the fixed 500
/// body with the CORS header and increments `pinakes_api_panics_total`.
#[sqlx::test(migrations = "../../migrations")]
async fn round6_the_panic_backstop_answers_500_with_cors_and_counts(pool: PgPool) {
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

/// Step 6 round 7 S21: the admin listener's backstop, behind the bearer check: without the
/// token a 401 and the handler never runs; with it a 500 with the fixed body, no CORS header,
/// and the panic counter incremented.
#[sqlx::test(migrations = "../../migrations")]
async fn round7_the_admin_backstop_sits_behind_the_bearer_check(pool: PgPool) {
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

/// Step 6 round 7 S23: the cursor cap sits one second below the cursor line, so `at`/`seq`
/// inversions from concurrent inserts are absorbed.
#[test]
fn round7_the_lookup_cursor_cap_absorbs_inversions() {
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
}
