//! Pinakes for agents on the public listener: the guide, the index and the MCP endpoint, its
//! handshake and every tool, the budget it shares with the REST routes, and no way through it to
//! the admin routes.
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Json, Router,
};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, IngestConfig},
    ingest,
    model::{identity, ResourceMeta},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    store,
    telemetry::Metrics,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::net::SocketAddr;
use tower::ServiceExt;

const AUTHOR: &str = "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy";

/// Three tagged links: a music event in Zürich, an article and a book with an ISBN.
async fn seed(pool: &PgPool) -> Vec<String> {
    let saved = ingest::cursor(pool, "fixture").await.unwrap();
    let uris = [
        "https://example.org/concert",
        "https://example.org/article",
        "https://openlibrary.org/books/OL7353617M/Matilda",
    ];
    let mut lines = vec![];
    let mut ids = vec![];
    for (i, uri) in uris.iter().enumerate() {
        let (id, uri, _) = identity(uri).unwrap();
        ids.push(id);
        lines.push(Change {
            op: Op::Put,
            uri,
            kind: Kind::Tag,
            author: AUTHOR.into(),
            object_path: format!("/pub/app/tags/{i}"),
            label: Some(if i == 0 { "music" } else { "reading" }.into()),
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
    for (i, (id, uri)) in ids.iter().zip(uris).enumerate() {
        let mut doc = ResourceMeta::new(uri, "web", 86400);
        match i {
            0 => {
                doc.name = Some("Zürich concert".into());
                doc.set_type("MusicEvent");
                doc.start = Some("2026-10-30T19:00:00Z".parse().unwrap());
                doc.location = Some(pinakes_core::model::Point {
                    lat: 47.3769,
                    lon: 8.5417,
                });
            }
            1 => {
                doc.name = Some("An article".into());
                doc.set_type("Article");
            }
            _ => {
                doc.name = Some("Matilda".into());
                doc.set_type("Book");
            }
        }
        doc.description = Some("A reference document".into());
        store::save_document(pool, id, 0, &doc).await.unwrap();
    }
    // The book's ISBN, as the books source records it.
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES ($1,'isbn:9780140328721','test','test',now()+interval '1 day')")
        .bind(&ids[2])
        .execute(pool)
        .await
        .unwrap();
    ids
}

/// A Nexus whose batch resource view knows the concert, tagged `music` once.
async fn nexus() -> String {
    let app = Router::new().route(
        "/v0/stream/resources/by_ids",
        post(|Json(body): Json<Value>| async move {
            let views: Vec<Value> = body["uris"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|u| u.as_str() == Some("https://example.org/concert"))
                .map(|u| {
                    json!({"details": {"id": "n1", "uri": u}, "taggers_count": 1,
                           "tags": [{"label": "music", "taggers_count": 1, "taggers": [AUTHOR],
                                     "relationship": body["viewer_id"] == AUTHOR}]})
                })
                .collect();
            Json(Value::Array(views))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn raw(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

/// One MCP message to `POST /mcp`, as a client sends it, and the JSON-RPC answer (`Null` for an
/// answer with no body, a notification's).
async fn mcp(app: &Router, message: Value, extra: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut headers = vec![
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
        ("host", "pinakes.example"),
        ("mcp-protocol-version", "2025-06-18"),
    ];
    headers.extend_from_slice(extra);
    let (status, response_headers, body) = raw(app, "POST", "/mcp", &headers, Some(message)).await;
    let text = String::from_utf8(body).unwrap();
    let event_stream = response_headers
        .get("content-type")
        .is_some_and(|t| t.to_str().unwrap().starts_with("text/event-stream"));
    let json = if event_stream {
        text.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str(d.trim()).ok())
            .next_back()
            .unwrap_or(Value::Null)
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    (status, json)
}

async fn initialize(app: &Router) {
    let (status, answer) = mcp(
        app,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}}}),
        &[],
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["result"]["serverInfo"]["name"], "pinakes");
    assert!(answer["result"]["capabilities"]["tools"].is_object());
    assert!(answer["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("nexus-scout"));
}

/// A tool's answer: whether it is an error, and its JSON text.
async fn call(app: &Router, tool: &str, arguments: Value) -> (bool, Value) {
    let (status, answer) = mcp(
        app,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": tool, "arguments": arguments}}),
        &[],
    )
    .await;
    assert_eq!(status, 200, "{tool}: {answer}");
    let result = &answer["result"];
    assert!(result.is_object(), "{tool}: {answer}");
    let text = result["content"][0]["text"].as_str().unwrap();
    (
        result["isError"] == true,
        serde_json::from_str(text).unwrap(),
    )
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_guide_and_the_index_name_the_host_that_asked(pool: PgPool) {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let host = [("host", "pinakes.example")];
    let (code, headers, body) = raw(&app, "GET", "/llms.txt", &host, None).await;
    assert_eq!(code, 200);
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    let guide = String::from_utf8(body).unwrap();
    assert!(guide.starts_with("# Searching the links people share on Pubky"));
    assert!(guide.contains("curl -s 'https://pinakes.example/v1/search?q=lugano&limit=5'"));
    assert!(guide.contains("POST https://pinakes.example/mcp"));
    assert!(guide.contains("https://nexus-scout.pubky.app/llms.txt"));
    assert!(!guide.contains("{{BASE}}"));

    let (code, headers, body) = raw(
        &app,
        "GET",
        "/skill.md",
        &[("host", "localhost:8099")],
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(headers["content-type"], "text/markdown; charset=utf-8");
    let skill = String::from_utf8(body).unwrap();
    assert!(skill.starts_with("---\nname: pinakes\ndescription: "));
    assert!(skill.contains("http://localhost:8099/v1/search"));

    let (code, _, body) = raw(&app, "GET", "/", &host, None).await;
    assert_eq!(code, 200);
    let index: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(index["start_here"], "https://pinakes.example/llms.txt");
    assert_eq!(index["links"]["skill"], "https://pinakes.example/skill.md");
    assert_eq!(index["links"]["mcp"], "https://pinakes.example/mcp");
    assert_eq!(
        index["links"]["social_graph"],
        "https://nexus-scout.pubky.app/llms.txt"
    );

    // With `api.mcp` off, the index names no endpoint and there is none.
    let mut config = Config::default();
    config.api.mcp = false;
    let off = api::router(AppState::new(pool, config).unwrap());
    let (_, _, body) = raw(&off, "GET", "/", &host, None).await;
    let index: Value = serde_json::from_slice(&body).unwrap();
    assert!(index["links"].get("mcp").is_none());
    let (status, _) = mcp(
        &off,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[],
    )
    .await;
    assert_eq!(status, 404);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_mcp_handshake_and_every_tool(pool: PgPool) {
    let ids = seed(&pool).await;
    let config = Config {
        nexus_url: nexus().await,
        ..Config::default()
    };
    let app = api::router(AppState::new(pool, config).unwrap());
    initialize(&app).await;
    let (status, _) = mcp(
        &app,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        &[],
    )
    .await;
    assert_eq!(status, 202);

    let (_, listed) = mcp(
        &app,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        &[],
    )
    .await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["search", "card", "lookup", "group", "types", "social"]
    );

    // search: the event near Zürich in its week, as a compact card with its full read's URL.
    let (error, page) = call(
        &app,
        "search",
        json!({"type": "Event", "near": {"lat": 47.37, "lon": 8.54, "radius": 5000},
               "from": "2026-10-26T00:00:00Z", "to": "2026-11-02T00:00:00Z", "sort": "start"}),
    )
    .await;
    assert!(!error, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1, "{page}");
    let card = &page["items"][0];
    assert_eq!(card["name"], "Zürich concert");
    assert_eq!(card["type"], "MusicEvent");
    assert_eq!(
        card["url"],
        format!("https://pinakes.example/v1/resources/{}", ids[0])
    );
    assert!(page["url"]
        .as_str()
        .unwrap()
        .starts_with("https://pinakes.example/v1/search?type=Event&"));

    // card: a known link in another spelling; an unknown one is a 404 with a hint.
    let (error, read) = call(&app, "card", json!({"uri": "HTTPS://Example.org/article"})).await;
    assert!(!error, "{read}");
    assert_eq!(read["card"]["id"], ids[1]);
    assert_eq!(read["card"]["name"], "An article");
    let (error, unknown) = call(&app, "card", json!({"uri": "https://example.org/nobody"})).await;
    assert!(error);
    assert_eq!(unknown["status"], 404);
    assert!(unknown["hint"].as_str().unwrap().contains("resolve"));

    // lookup: every link with one identifier.
    let (error, books) = call(&app, "lookup", json!({"key": "isbn:0140328726"})).await;
    assert!(!error, "{books}");
    assert_eq!(books["items"][0]["id"], ids[2]);

    // group: an unknown group is a 404, a malformed id a 400 before any request.
    let (error, missing) = call(&app, "group", json!({"id": "0".repeat(32)})).await;
    assert!(error);
    assert_eq!(missing["status"], 404);
    let (error, bad) = call(&app, "group", json!({"id": "../../metrics"})).await;
    assert!(error);
    assert_eq!(bad["status"], 400);

    // types: the card types with their counts, and what this instance resolves.
    let (error, types) = call(&app, "types", json!({})).await;
    assert!(!error, "{types}");
    let counts: Vec<(&str, i64)> = types["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["type"].as_str().unwrap(), t["count"].as_i64().unwrap()))
        .collect();
    assert_eq!(counts.len(), 3, "{types}");
    assert!(types["resolves"]
        .as_array()
        .unwrap()
        .contains(&json!("web")));

    // social: tags per URI from Nexus, with the viewer's own; unknown links listed.
    let (error, social) = call(
        &app,
        "social",
        json!({"uris": ["https://example.org/concert", "https://example.org/article"],
               "viewer": AUTHOR}),
    )
    .await;
    assert!(!error, "{social}");
    assert_eq!(
        social,
        json!({"items": [{"uri": "https://example.org/concert", "taggers_count": 1,
                          "tags": [{"label": "music", "taggers_count": 1, "viewer_tagged": true}]}],
               "unknown": ["https://example.org/article"]})
    );
    let (error, refused) = call(&app, "social", json!({"uris": []})).await;
    assert!(error);
    assert_eq!(refused["status"], 400);

    // A wrong argument is the tool's error, an unknown tool the protocol's.
    let (error, wrong) = call(&app, "search", json!({"radius": 5})).await;
    assert!(error);
    assert!(wrong["error"].as_str().unwrap().contains("`radius`"));
    // boost is search's too: without a viewer the route refuses it, as it would over REST.
    let (error, boost) = call(&app, "search", json!({"boost": "friends"})).await;
    assert!(error);
    assert!(
        boost["error"].as_str().unwrap().contains("requires viewer"),
        "{boost}"
    );
    let (_, answer) = mcp(
        &app,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": "curate_group", "arguments": {}}}),
        &[],
    )
    .await;
    assert!(answer["error"].is_object(), "{answer}");
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn social_without_a_nexus_is_a_plain_error(pool: PgPool) {
    // Nothing listens there.
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let config = Config {
        nexus_url: format!("http://{free}"),
        ..Config::default()
    };
    let app = api::router(AppState::new(pool, config).unwrap());
    let (error, answer) = call(&app, "social", json!({"uris": ["https://example.org/"]})).await;
    assert!(error);
    assert_eq!(answer["status"], 502);
    assert!(answer["error"].as_str().unwrap().contains("by_ids"));
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_tool_call_spends_the_same_per_ip_budget(pool: PgPool) {
    seed(&pool).await;
    let mut config = Config::default();
    config.api.requests_per_minute = 1;
    let app = api::router(AppState::new(pool, config).unwrap());
    // The MCP message takes the one unit; the tool's search finds the budget spent.
    let (error, answer) = call(&app, "search", json!({"q": "concert"})).await;
    assert!(error);
    assert_eq!(answer["status"], 429, "{answer}");
    // The next message is over budget itself.
    let (code, headers, _) = raw(
        &app,
        "POST",
        "/mcp",
        &[
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
        ],
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})),
    )
    .await;
    assert_eq!(code, 429);
    assert_eq!(headers["retry-after"], "60");
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn no_admin_route_is_reachable_through_mcp(pool: PgPool) {
    let ids = seed(&pool).await;
    let config = Config {
        admin_token: Some("admin-secret-token".into()),
        ..Config::default()
    };
    let state = AppState::new(pool, config).unwrap();
    let public = api::router(state.clone());
    let admin = api::admin_router(state);
    // The admin routes answer on their own listener only, with the token.
    let bearer = [("authorization", "Bearer admin-secret-token")];
    let document = format!("/v1/resources/{}/document", ids[0]);
    assert_eq!(raw(&admin, "GET", &document, &bearer, None).await.0, 200);
    for (method, path) in [("GET", document.as_str()), ("GET", "/metrics")] {
        assert_eq!(
            raw(&public, method, path, &bearer, None).await.0,
            404,
            "{path}"
        );
    }
    // No tool names an admin operation, and the token changes nothing about a tool's answer.
    let (_, listed) = mcp(
        &public,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &bearer,
    )
    .await;
    for tool in listed["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert!(
            ["search", "card", "lookup", "group", "types", "social"].contains(&name),
            "{name}"
        );
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{name}");
    }
    let card = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                      "params": {"name": "card", "arguments": {"uri": "https://example.org/concert"}}});
    let (_, with_token) = mcp(&public, card.clone(), &bearer).await;
    let (_, without) = mcp(&public, card, &[]).await;
    assert_eq!(with_token["result"], without["result"]);
    let text = with_token["result"]["content"][0]["text"].as_str().unwrap();
    // The card a tool reads is the public one: none of the stored document's internal fields.
    assert!(
        !text.contains("ttl_secs") && !text.contains("group_keys"),
        "{text}"
    );
}
