//! The listeners' controls: the admin bearer check and bind policy, the per-IP and preview
//! budgets, the body limit, the statement timeout and the panic backstops.
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, IngestConfig},
    ingest,
    model::{identity, ResourceMeta},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    queue, store,
    telemetry::Metrics,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    net::SocketAddr,
    sync::atomic::{AtomicUsize, Ordering},
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
        lines.push(Change {
            op: Op::Put,
            uri: uri.clone(),
            kind: Kind::Tag,
            author: author(i),
            object_path: format!("/pub/app/tags/{i}-rust"),
            label: Some("rust".into()),
            at: 1_790_000_000_000,
            recurrence_id: None,
            status: None,
            start: None,
            end: None,
        });
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
        doc.name = Some(format!("Rust Article {i}"));
        doc.set_type("Article");
        store::save_document(pool, id, 0, &doc).await.unwrap();
    }
    ids
}
async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let mut request = request
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
}

#[sqlx::test(migrations = "../../migrations")]
async fn public_budget_covers_reads_and_evicts_old_ips(pool: PgPool) {
    let config = Config {
        api: pinakes_core::config::ApiConfig {
            requests_per_minute: 2,
            cache_capacity: 2,
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
        read_from(
            &app,
            "/v1/resources/00000000000000000000000000000000",
            "127.0.0.1:2"
        )
        .await,
        404
    );
    assert_eq!(read_from(&app, "/v1/info", "127.0.0.1:3").await, 429);
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
async fn admin_bearer_and_network_bind_policy(pool: sqlx::PgPool) {
    let mut cfg = Config {
        admin_listen: "0.0.0.0:9090".into(),
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
    cfg.admin_token = Some("review-only-operator-token".into());
    assert!(cfg.validate().is_ok());
    let app = api::admin_router(AppState::new(pool, cfg).unwrap());
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

/// A request body over 2 MB is refused before any handler reads it.
#[sqlx::test(migrations = "../../migrations")]
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

/// Multibyte, emoji, U+FFFD and invalid-UTF-8 values in every path parameter, query parameter
/// and body field of the public routes: every answer is 2xx or 4xx, never a panic, and a
/// malformed relative time answers 400 with the relative-time message.
#[sqlx::test(migrations = "../../migrations")]
async fn multibyte_input_never_panics_any_public_route(pool: PgPool) {
    let mut config = Config::default();
    config.api.requests_per_minute = 1_000_000;
    config.preview.requests_per_minute = 1_000_000;
    config.preview.uri_requests_per_minute = 1_000_000;
    let state = AppState::new(pool, config).unwrap();
    let panics = state.metrics.api_panics.clone();
    let app = api::router(state);
    let id = "0123456789abcdef0123456789abcdef";
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
    let encode = |v: &str| url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
    let mut sent = 0;
    for value in values {
        let v = encode(value);
        let mut requests: Vec<(&str, String, Option<Value>)> = vec![
            ("GET", format!("/v1/resources/{v}"), None),
            ("GET", format!("/v1/resources/by-uri?uri={v}"), None),
            ("GET", format!("/v1/groups/{v}"), None),
            (
                "POST",
                "/v1/resources/lookup".into(),
                Some(json!({"ids":[value]})),
            ),
            (
                "POST",
                "/v1/resources/lookup".into(),
                Some(json!({"uris":[value]})),
            ),
            ("POST", "/v1/resolve".into(), Some(json!({"uri":value}))),
            (
                "POST",
                "/v1/changes/lookup".into(),
                Some(json!({"resource_ids":[value],"since":"now"})),
            ),
            (
                "POST",
                "/v1/changes/lookup".into(),
                Some(json!({"resource_ids":[id],"since":value})),
            ),
            (
                "POST",
                "/v1/changes/lookup".into(),
                Some(json!({"resource_ids":[id],"since":"now","cursor":value})),
            ),
            (
                "POST",
                "/v1/occurrences/lookup".into(),
                Some(json!({"resource_ids":[value]})),
            ),
        ];
        for name in ["from", "to", "cursor"] {
            requests.push((
                "POST",
                "/v1/occurrences/lookup".into(),
                Some(json!({"resource_ids":[id],name:value})),
            ));
        }
        // A feed's occurrences: the path, the window (`from`, `to`, in RFC 3339), `limit` and
        // `cursor`.
        requests.push(("GET", format!("/v1/resources/{v}/occurrences"), None));
        for name in ["from", "to", "limit", "cursor"] {
            requests.push((
                "GET",
                format!("/v1/resources/{id}/occurrences?{name}={v}"),
                None,
            ));
        }
        for raw in ["%FF", "now%FF", "%FF,1"] {
            requests.push((
                "GET",
                format!("/v1/resources/{id}/occurrences?from={raw}"),
                None,
            ));
        }
        for (method, target, body) in requests {
            let since = body
                .as_ref()
                .is_some_and(|b| b["since"].as_str() == Some(value));
            let (status, response) = call(&app, method, &target, body).await;
            assert!(
                status.is_success() || status.is_client_error(),
                "{method} {target}: {status} {response}"
            );
            if since && value.starts_with("now") {
                assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {target}");
                assert!(
                    response["error"]
                        .as_str()
                        .unwrap_or("")
                        .contains("relative time"),
                    "{method} {target}: {response}"
                );
            }
            sent += 1;
        }
    }
    // Thirteen body and path requests, five occurrence queries and three raw values per value.
    assert_eq!(sent, values.len() * 21, "the sweep lost requests");
    assert_eq!(panics.get(), 0, "no handler panicked");
}

/// A handler panic on the public listener answers 500 with a fixed JSON body and increments
/// `pinakes_api_panics_total`.
#[sqlx::test(migrations = "../../migrations")]
async fn the_panic_backstop_answers_500_and_counts(pool: PgPool) {
    let state = AppState::new(pool, Config::default()).unwrap();
    let panics = state.metrics.api_panics.clone();
    async fn boom() -> &'static str {
        panic!("test panic")
    }
    let app = api::with_public_layers(Router::new().route("/boom", get(boom)), state);
    let (code, body) = call(&app, "GET", "/boom", None).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error":"internal server error"}));
    assert_eq!(panics.get(), 1);
}

/// The admin listener's backstop, behind the bearer check: without the token a 401 and the
/// handler never runs; with it a 500 with the fixed body and the panic counter incremented.
#[sqlx::test(migrations = "../../migrations")]
async fn the_admin_backstop_sits_behind_the_bearer_check(pool: PgPool) {
    let config = Config {
        admin_token: Some("admin-test-token".into()),
        ..Default::default()
    };
    let state = AppState::new(pool, config).unwrap();
    let panics = state.metrics.api_panics.clone();
    static RAN: AtomicUsize = AtomicUsize::new(0);
    async fn boom() -> &'static str {
        RAN.fetch_add(1, Ordering::SeqCst);
        panic!("test panic")
    }
    let app = api::with_admin_layers(Router::new().route("/boom", get(boom)), state);
    let send = |token: Option<&'static str>| {
        let app = app.clone();
        async move {
            let mut request = Request::builder().uri("/boom");
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error":"internal server error"})
    );
    assert_eq!(RAN.load(Ordering::SeqCst), 1);
    assert_eq!(panics.get(), 1);
}

/// Public `/v1/info` says what a client needs; the admin one adds what an operator does.
#[sqlx::test(migrations = "../../migrations")]
async fn public_info_says_what_a_client_needs(pool: PgPool) {
    let state = AppState::new(pool, Config::default()).unwrap();
    let (public, admin) = (api::router(state.clone()), api::admin_router(state));
    let (code, info) = call(&public, "GET", "/v1/info", None).await;
    assert_eq!(code, 200);
    let keys: Vec<&String> = info.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["limits", "osm_licence", "resolves", "version"],
        "{info}"
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
    let (code, full) = call(&admin, "GET", "/v1/info", None).await;
    assert_eq!(code, 200);
    assert!(
        full["manifests"].is_array() && full.get("sources").is_some(),
        "{full}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_stored_document_is_an_admin_read(pool: PgPool) {
    let ids = seed(&pool).await;
    let state = AppState::new(pool, Config::default()).unwrap();
    let (public, admin) = (api::router(state.clone()), api::admin_router(state));
    let path = format!("/v1/resources/{}/document", ids[0]);
    let (code, doc) = call(&admin, "GET", &path, None).await;
    assert_eq!(code, 200, "{doc}");
    assert_eq!(doc["source"], "web");
    assert!(doc["ttl_secs"].is_number(), "{doc}");
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
    assert_eq!(call(&admin, "GET", unknown, None).await.0, 404);
    assert_eq!(
        call(&admin, "GET", "/v1/resources/bad/document", None)
            .await
            .0,
        400
    );
}

/// A tracking variant is an alias: read by its ID or URI it answers with the root's card, and
/// the root's changes are its changes.
#[sqlx::test(migrations = "../../migrations")]
async fn an_alias_reads_as_its_root_everywhere(pool: PgPool) {
    let clean = "https://example.org/talk";
    let variant = "https://example.org/talk?utm_source=newsletter&fbclid=x";
    let alias = store::ensure_resource(&pool, variant).await.unwrap();
    let root = identity(clean).unwrap().0;
    assert_ne!(alias, root);
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
    let doc = pinakes_core::resolver::web::extract(clean, &fetched, 86400).doc;
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
