use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, Request, StatusCode},
    routing::get,
    Json, Router,
};
use chrono::Utc;
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
        assert_eq!(raw(&public, "GET", path, None).await.0, 404, "{path}");
    }
    assert_eq!(call(&public, "GET", "/v1/openapi.json", None).await.0, 200);

    // Swagger UI shares the per-IP budget of the /v1 routes.
    let mut tight = Config::default();
    tight.api.requests_per_minute = 2;
    let public = api::router(AppState::new(pool, tight).unwrap());
    assert_eq!(raw(&public, "GET", "/swagger-ui/", None).await.0, 200);
    assert_eq!(raw(&public, "GET", "/v1/openapi.json", None).await.0, 200);
    assert_eq!(raw(&public, "GET", "/swagger-ui/", None).await.0, 429);
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
