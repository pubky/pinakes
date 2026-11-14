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
const BLOCK: &str = "btc:block/00000000000000000024fb37364cbf81fd49cc2d51c09c75c35433c3a1945d04";
const MEMPOOL: &str =
    "https://mempool.space/block/00000000000000000024fb37364cbf81fd49cc2d51c09c75c35433c3a1945d04";
const PROTEIN: &str = "uniprot:P69905";
const UNIPROT_PAGE: &str = "https://www.uniprot.org/uniprotkb/P69905/entry";
const BOOK: &str = "isbn:9780140328721";
const OPENLIBRARY: &str = "https://openlibrary.org/isbn/9780140328721";

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
                skipped: Default::default(),
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

/// The links a fake Nexus knows, with the label and the taggers each carries.
const NEXUS_KNOWS: &[(&str, &str, &[&str])] = &[
    ("https://example.org/concert", "music", &[AUTHOR]),
    (MEMPOOL, "halving", &[AUTHOR, OTHER]),
    (UNIPROT_PAGE, "protein", &[OTHER]),
    (OPENLIBRARY, "reading", &[AUTHOR]),
];
const OTHER: &str = "ybbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A Nexus whose batch resource view knows [`NEXUS_KNOWS`].
async fn nexus() -> String {
    let app = Router::new().route(
        "/v0/stream/resources/by_ids",
        post(|Json(body): Json<Value>| async move {
            let views: Vec<Value> = body["uris"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|u| {
                    let (uri, label, taggers) = NEXUS_KNOWS.iter().find(|(k, _, _)| u == k)?;
                    Some(json!({"details": {"id": "n1", "uri": uri}, "taggers_count": taggers.len(),
                           "tags": [{"label": label, "taggers_count": taggers.len(), "taggers": taggers,
                                     "relationship": taggers.contains(&body["viewer_id"].as_str().unwrap_or_default())}]}))
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
    let instructions = answer["result"]["instructions"].as_str().unwrap();
    for text in [
        "nexus-scout",
        "`refs`",
        "`occurrence.feed_uri`",
        "`aliases`",
    ] {
        assert!(instructions.contains(text), "{text}");
    }
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

#[sqlx::test(migrations = "../../migrations")]
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
    // The join with Nexus names every link a card's tags can be on.
    for field in ["`refs`", "`occurrence.feed_uri`", "`aliases`"] {
        assert!(guide.contains(field), "{field}");
    }

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

#[sqlx::test(migrations = "../../migrations")]
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
    let social = &listed["result"]["tools"][5]["description"];
    for field in ["refs", "occurrence.feed_uri", "aliases on its full read"] {
        assert!(social.as_str().unwrap().contains(field), "{field}");
    }

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
    let url = page["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://pinakes.example/v1/search?") && url.contains("type=Event"),
        "{url}"
    );

    // card: a known link in another spelling; an unknown one is a 404 with a hint.
    let (error, read) = call(&app, "card", json!({"uri": "HTTPS://Example.org/article"})).await;
    assert!(!error, "{read}");
    assert_eq!(read["card"]["id"], ids[1]);
    assert_eq!(read["card"]["name"], "An article");
    // A read is flat: the card tool carries its fields, such as the keys `lookup` takes.
    let (error, book) = call(
        &app,
        "card",
        json!({"uri": "https://openlibrary.org/books/OL7353617M/Matilda"}),
    )
    .await;
    assert!(!error, "{book}");
    assert_eq!(book["card"]["keys"], json!(["isbn:9780140328721"]));
    let (error, unknown) = call(&app, "card", json!({"uri": "https://example.org/nobody"})).await;
    assert!(error);
    assert_eq!(unknown["status"], 404);
    assert!(unknown["hint"].as_str().unwrap().contains("resolve"));

    // search with several types: any of them.
    let (error, kinds) = call(&app, "search", json!({"type": ["Book", "Article"]})).await;
    assert!(!error, "{kinds}");
    let mut found: Vec<&str> = kinds["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["type"].as_str().unwrap())
        .collect();
    found.sort_unstable();
    assert_eq!(found, ["Article", "Book"]);

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

    // social by links: tags per link from Nexus, with the viewer's own; a link Nexus does not
    // know has none.
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
        json!({"items": [
            {"uri": "https://example.org/concert", "asked": ["https://example.org/concert"],
             "found": ["https://example.org/concert"], "taggers_count": 1,
             "tags": [{"label": "music", "taggers_count": 1, "viewer_tagged": true}]},
            {"uri": "https://example.org/article", "asked": ["https://example.org/article"],
             "found": [], "taggers_count": 0, "tags": []}
        ]})
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

#[sqlx::test(migrations = "../../migrations")]
async fn social_without_a_nexus_is_a_plain_error(pool: PgPool) {
    // Nothing listens at the first; the second is a Nexus without the batch resource view, whose
    // 404 is no answer either, never an empty one (every URI "unknown").
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let older = Router::new().route("/v0/info", axum::routing::get(|| async { "{}" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let older_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, older).await.unwrap() });
    for nexus_url in [format!("http://{free}"), older_url] {
        let config = Config {
            nexus_url: nexus_url.clone(),
            ..Config::default()
        };
        let app = api::router(AppState::new(pool.clone(), config).unwrap());
        let (error, answer) = call(&app, "social", json!({"uris": ["https://example.org/"]})).await;
        assert!(error, "{nexus_url}: {answer}");
        assert_eq!(answer["status"], 502, "{nexus_url}");
        assert!(answer["error"].as_str().unwrap().contains("by_ids"));
    }
}

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
async fn a_browser_opening_mcp_sees_how_to_connect(pool: PgPool) {
    let app = api::router(AppState::new(pool, Config::default()).unwrap());
    let browser = [
        ("host", "pinakes.example"),
        ("accept", "text/html,application/xhtml+xml,*/*;q=0.8"),
    ];
    let (code, headers, body) = raw(&app, "GET", "/mcp", &browser, None).await;
    assert_eq!(code, 200);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(headers["content-security-policy"], "default-src 'none'");
    let page = String::from_utf8(body).unwrap();
    assert!(page.contains("This is the Pinakes MCP endpoint. Add it to your MCP client."));
    assert!(page.contains("claude mcp add --transport http pinakes https://pinakes.example/mcp"));
    assert!(page.contains(r#""url": "https://pinakes.example/mcp""#));
    for tool in ["search", "card", "lookup", "group", "types", "social"] {
        assert!(page.contains(&format!("<code>{tool}</code>")), "{tool}");
    }
    assert!(page.contains(r#"<a href="https://pinakes.example/llms.txt">"#));
    assert!(page.contains(r#"<a href="https://pinakes.example/skill.md">"#));

    // Any other GET keeps the 405 MCP allows, naming POST, in one line of plain text.
    for accept in [
        None,
        Some("application/json, text/event-stream"),
        Some("*/*"),
    ] {
        let mut headers = vec![("host", "pinakes.example")];
        if let Some(accept) = accept {
            headers.push(("accept", accept));
        }
        let (code, response, body) = raw(&app, "GET", "/mcp", &headers, None).await;
        assert_eq!(code, 405, "{accept:?}");
        assert_eq!(response["allow"], "POST");
        assert_eq!(response["vary"], "accept");
        assert!(response["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain"));
        let text = String::from_utf8(body).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("MCP clients POST"), "{text}");
    }
    // A HEAD is a GET's: the browser's page or the 405, never MCP's.
    let (code, response, _) = raw(&app, "HEAD", "/mcp", &[("host", "pinakes.example")], None).await;
    assert_eq!(code, 405);
    assert_eq!(response["allow"], "POST");
    let (code, _, _) = raw(&app, "HEAD", "/mcp", &browser, None).await;
    assert_eq!(code, 200);
    // MCP clients are unchanged: a POST is still MCP's.
    let (status, answer) = mcp(
        &app,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(answer["result"]["tools"].as_array().unwrap().len(), 6);
}

/// Typed cards whose own link nobody wrote: a block, a protein and a book, each tagged through
/// another link that is its alias (the book also by its own `isbn:` once). Returns the cards' ids.
async fn seed_typed(pool: &PgPool) -> Vec<String> {
    let saved = ingest::cursor(pool, "fixture").await.unwrap();
    let tags = [
        (MEMPOOL, AUTHOR, "halving"),
        (MEMPOOL, OTHER, "halving"),
        (UNIPROT_PAGE, OTHER, "protein"),
        (OPENLIBRARY, AUTHOR, "reading"),
        (OPENLIBRARY, OTHER, "reading"),
        (BOOK, OTHER, "books"),
    ];
    let lines: Vec<Change> = tags
        .iter()
        .enumerate()
        .map(|(i, (uri, author, label))| Change {
            op: Op::Put,
            uri: (*uri).into(),
            kind: Kind::Tag,
            author: (*author).into(),
            object_path: format!("/pub/app/tags/typed-{i}"),
            label: Some((*label).into()),
            at: 1_790_000_000_000,
        })
        .collect();
    let next = lines.len() as i64;
    ingest::apply(
        pool,
        "fixture",
        &saved,
        &FeedPage {
            page: Changes {
                lines,
                next_cursor: next,
                skipped: Default::default(),
            },
            epoch: Some("fixture".into()),
            length: next,
        },
        &IngestConfig::default(),
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap();
    let mut ids = vec![];
    for (root, alias, kind) in [
        (BLOCK, MEMPOOL, "Thing"),
        (PROTEIN, UNIPROT_PAGE, "Thing"),
        (BOOK, OPENLIBRARY, "Book"),
    ] {
        let root_id = store::ensure_resource(pool, root).await.unwrap();
        let (alias_id, _, _) = identity(alias).unwrap();
        sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'identifier') ON CONFLICT (alias_id) DO NOTHING")
            .bind(&alias_id)
            .bind(&root_id)
            .execute(pool)
            .await
            .unwrap();
        let mut doc = ResourceMeta::new(root, "web", 86400);
        doc.name = Some(format!("The card of {root}"));
        doc.set_type(kind);
        store::save_document(pool, &root_id, 0, &doc).await.unwrap();
        ids.push(root_id);
    }
    ids
}

/// A typed card names the links people wrote for it (`refs`), on every read, and `social`
/// asks Nexus by the card's own link and those, merging their tags per card.
#[sqlx::test(migrations = "../../migrations")]
async fn typed_cards_name_the_links_people_wrote_and_social_joins_by_them(pool: PgPool) {
    let ids = seed_typed(&pool).await;
    let config = Config {
        nexus_url: nexus().await,
        ..Config::default()
    };
    let app = api::router(AppState::new(pool.clone(), config).unwrap());
    let get = |path: String| {
        let app = app.clone();
        async move {
            let (code, _, body) = raw(&app, "GET", &path, &[], None).await;
            assert_eq!(code, 200, "{path}");
            serde_json::from_slice::<Value>(&body).unwrap()
        }
    };
    // Each read names the referenced links, most referenced first: the book's Open Library page
    // (two references) before its own isbn: link (one).
    let expected = [vec![MEMPOOL], vec![UNIPROT_PAGE], vec![OPENLIBRARY, BOOK]];
    for (id, refs) in ids.iter().zip(&expected) {
        let card = get(format!("/v1/resources/{id}")).await;
        assert_eq!(card["refs"], json!(refs), "{id}");
    }
    let (code, _, body) = raw(
        &app,
        "POST",
        "/v1/resources/lookup",
        &[("content-type", "application/json")],
        Some(json!({ "ids": ids })),
    )
    .await;
    assert_eq!(code, 200);
    let cards: Value = serde_json::from_slice(&body).unwrap();
    for (card, refs) in cards.as_array().unwrap().iter().zip(&expected) {
        assert_eq!(card["refs"], json!(refs));
    }
    let page = get("/v1/search?type=Book".into()).await;
    assert_eq!(page["items"][0]["refs"], json!([OPENLIBRARY, BOOK]));

    // social by card: asked by the card's link and its refs, merged per card.
    let (error, social) = call(&app, "social", json!({"ids": ids, "viewer": AUTHOR})).await;
    assert!(!error, "{social}");
    let items = social["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["id"], ids[0]);
    assert_eq!(items[0]["asked"], json!([BLOCK, MEMPOOL]));
    assert_eq!(items[0]["found"], json!([MEMPOOL]));
    assert_eq!(
        items[0]["tags"],
        json!([{"label": "halving", "taggers_count": 2, "viewer_tagged": true}])
    );
    assert_eq!(items[1]["found"], json!([UNIPROT_PAGE]));
    assert_eq!(
        items[1]["tags"],
        json!([{"label": "protein", "taggers_count": 1, "viewer_tagged": false}])
    );
    assert_eq!(items[2]["asked"], json!([BOOK, OPENLIBRARY]));
    assert_eq!(items[2]["taggers_count"], 1);
    // An id Pinakes does not know is named, not guessed.
    let (error, unknown) = call(&app, "social", json!({"ids": ["0".repeat(32)]})).await;
    assert!(!error, "{unknown}");
    assert_eq!(unknown["unknown_ids"], json!(["0".repeat(32)]));
    // An alias's id (the mempool page's) is answered under the id asked, with the root card
    // that answers for it; so is a card that became an alias after a client cached its id.
    let (mempool, _, _) = identity(MEMPOOL).unwrap();
    let old = store::ensure_resource(&pool, "https://example.org/an-old-card")
        .await
        .unwrap();
    let mut doc = ResourceMeta::new("https://example.org/an-old-card", "web", 86400);
    doc.name = Some("An old card".into());
    store::save_document(&pool, &old, 0, &doc).await.unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&old)
        .bind(&ids[1])
        .execute(&pool)
        .await
        .unwrap();
    let (error, aliased) = call(&app, "social", json!({"ids": [mempool, old]})).await;
    assert!(!error, "{aliased}");
    assert!(aliased.get("unknown_ids").is_none(), "{aliased}");
    let items = aliased["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], json!(mempool));
    assert_eq!(items[0]["root"], ids[0]);
    assert_eq!(items[0]["asked"], json!([BLOCK, MEMPOOL]));
    assert_eq!(items[1]["id"], json!(old));
    assert_eq!(items[1]["root"], ids[1]);
    assert_eq!(items[1]["found"], json!([UNIPROT_PAGE]));
    // A card asked by its own id names no root.
    assert!(social["items"][0].get("root").is_none());
}

/// `refs` ranks a card's links by their references counted only up to a bound, so a read never
/// counts every reference of a much-referenced link: links past the bound rank by URI.
#[sqlx::test(migrations = "../../migrations")]
async fn refs_count_each_link_only_up_to_a_bound(pool: PgPool) {
    let ids = seed_typed(&pool).await;
    // Counted in full, the book's own isbn: link (1,501 references) would lead its Open Library
    // page (1,202); both are past the bound, so the page leads by URI.
    for (uri, n) in [(BOOK, 1500), (OPENLIBRARY, 1200)] {
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) SELECT $1,$2,'tag','bulk','/pub/app/tags/bulk-'||$1||'-'||n,now() FROM generate_series(1,$3) n")
            .bind(identity(uri).unwrap().0)
            .bind(AUTHOR)
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
    }
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, _, body) = raw(&app, "GET", &format!("/v1/resources/{}", ids[2]), &[], None).await;
    assert_eq!(code, 200);
    let card: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(card["refs"], json!([OPENLIBRARY, BOOK]));
}

/// `refs` counts the references of a card's most recently referenced aliases only, so a card
/// with many referenced spellings costs a read a bounded number of counts. The book gets 40
/// referenced spellings; the 9 least recently referenced have the most references (3 each, the
/// others 2), and its Open Library page (2, last referenced before any spelling) is older still.
/// Counted over every alias, those would lead; examined are the 31 most recent alone.
#[sqlx::test(migrations = "../../migrations")]
async fn refs_count_only_a_cards_most_recently_referenced_aliases(pool: PgPool) {
    let ids = seed_typed(&pool).await;
    let mut recent = vec![];
    for k in 0..40i32 {
        let (id, uri, _) = identity(&format!("{OPENLIBRARY}?edition={k:02}")).unwrap();
        sqlx::query("INSERT INTO resource(id,uri) VALUES($1,$2)")
            .bind(&id)
            .bind(&uri)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
            .bind(&id)
            .bind(&ids[2])
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) SELECT $1,$2,'tag','edition','/pub/app/tags/edition-'||$1||'-'||n,now()-$3*interval '1 minute' FROM generate_series(1,$4) n")
            .bind(&id)
            .bind(AUTHOR)
            .bind(k)
            .bind(if k < 31 { 2 } else { 3 })
            .execute(&pool)
            .await
            .unwrap();
        if k < 31 {
            recent.push(uri);
        }
    }
    recent.sort();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, _, body) = raw(&app, "GET", &format!("/v1/resources/{}", ids[2]), &[], None).await;
    assert_eq!(code, 200);
    let card: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(card["refs"], json!(recent[..5]));
}

/// The `card` tool names the public collections that hold a link: their count, the newest few
/// and the route that lists them all. A card nobody collected carries neither.
#[sqlx::test(migrations = "../../migrations")]
async fn the_card_tool_names_the_collections_that_hold_a_link(pool: PgPool) {
    seed(&pool).await;
    let (id, uri, _) = identity("https://example.org/article").unwrap();
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    let lines: Vec<Change> = (0..7)
        .map(|n| Change {
            op: Op::Put,
            uri: uri.clone(),
            kind: Kind::Collection,
            author: AUTHOR.into(),
            object_path: format!("/pub/social/v1/posts/C{n}/C{n}.json"),
            label: None,
            at: 1_790_000_000_000 + n,
        })
        .collect();
    let next = saved.cursor + lines.len() as i64;
    ingest::apply(
        &pool,
        "fixture",
        &saved,
        &FeedPage {
            page: Changes {
                lines,
                next_cursor: next,
                skipped: Default::default(),
            },
            epoch: Some("fixture".into()),
            length: next,
        },
        &IngestConfig::default(),
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap();
    let app = api::router(AppState::new(pool, Config::default()).unwrap());
    initialize(&app).await;
    let (error, read) = call(&app, "card", json!({"uri": "https://example.org/article"})).await;
    assert!(!error, "{read}");
    let card = &read["card"];
    assert_eq!(card["collections"], 7);
    let held: Vec<&str> = card["in_collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["uri"].as_str().unwrap())
        .collect();
    let newest: Vec<String> = (2..7)
        .rev()
        .map(|n| format!("pubky://{AUTHOR}/pub/social/v1/posts/C{n}"))
        .collect();
    assert_eq!(held, newest);
    assert_eq!(card["in_collections"][0]["author"], AUTHOR);
    assert_eq!(
        card["collections_url"],
        format!("https://pinakes.example/v1/resources/{id}/collections")
    );
    // search: the links one collection holds.
    let (error, page) = call(&app, "search", json!({"in_collection": newest[0]})).await;
    assert!(!error, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1, "{page}");
    assert_eq!(page["items"][0]["id"], id.as_str());
    assert_eq!(page["items"][0]["collections"], 7);
    let (error, read) = call(&app, "card", json!({"uri": "https://example.org/concert"})).await;
    assert!(!error, "{read}");
    for key in ["collections", "in_collections", "collections_url"] {
        assert!(read["card"].get(key).is_none(), "{key}: {read}");
    }
}

/// The social tool never sends a link that carries a credential to the Nexus: a call naming one
/// is refused before any batch is asked, and its answer does not hold the secret.
#[sqlx::test(migrations = "../../migrations")]
async fn social_never_sends_a_link_that_carries_a_credential_to_the_nexus(pool: PgPool) {
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = asked.clone();
    let app = Router::new().route(
        "/v0/stream/resources/by_ids",
        post(move |Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body.to_string());
                Json(json!([]))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nexus_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = Config {
        nexus_url,
        ..Config::default()
    };
    let app = api::router(AppState::new(pool, config).unwrap());
    let (error, answer) = call(
        &app,
        "social",
        json!({"uris": ["https://example.org/concert", "https://example.org/private?token=SECRET"]}),
    )
    .await;
    assert!(error, "{answer}");
    assert_eq!(answer["status"], 400);
    assert!(!answer.to_string().contains("SECRET"));
    assert!(asked.lock().unwrap().is_empty());
}

/// A card's own link or ref that carries a credential (a link someone's post made, read by its
/// URI, here an alias of the card with references) is left out of what `social` asks the Nexus:
/// the card is asked by its plain links, and nothing sent names the secret.
#[sqlx::test(migrations = "../../migrations")]
async fn social_asks_the_nexus_by_a_cards_plain_links_only(pool: PgPool) {
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = asked.clone();
    let app = Router::new().route(
        "/v0/stream/resources/by_ids",
        post(move |Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body.to_string());
                Json(json!([]))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nexus_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = "https://example.org/concert";
    let private = "https://example.org/private?token=SECRET";
    // A calendar provider's share link written as `webcal:` without `//`: no host to a URL
    // parser, a credential to `names_credential`.
    let probes: Value =
        serde_json::from_str(include_str!("fixtures/capability_urls.json")).unwrap();
    let share = probes["providers"][1]
        .as_str()
        .unwrap()
        .replacen("https://", "webcal:", 1);
    assert!(share.starts_with("webcal:outlook."), "{share}");
    let root_id = store::ensure_resource(&pool, root).await.unwrap();
    let private_id = store::ensure_resource(&pool, private).await.unwrap();
    let share_id = store::ensure_resource(&pool, &share).await.unwrap();
    let mut doc = ResourceMeta::new(root, "web", 86400);
    doc.name = Some("Concert".into());
    doc.set_type("Event");
    store::save_document(&pool, &root_id, 0, &doc)
        .await
        .unwrap();
    for alias in [&private_id, &share_id] {
        sqlx::query(
            "INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'identifier')",
        )
        .bind(alias)
        .bind(&root_id)
        .execute(&pool)
        .await
        .unwrap();
    }
    for id in [&root_id, &private_id, &share_id] {
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','gig','/pub/app/tags/'||$1,now())")
            .bind(id)
            .bind(AUTHOR)
            .execute(&pool)
            .await
            .unwrap();
    }
    let config = Config {
        nexus_url,
        ..Config::default()
    };
    let app = api::router(AppState::new(pool, config).unwrap());
    let (_, _, body) = raw(&app, "GET", &format!("/v1/resources/{root_id}"), &[], None).await;
    let card: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        card["refs"].as_array().is_some_and(
            |refs| refs.iter().any(|r| r == private) && refs.iter().any(|r| *r == share)
        ),
        "both credential spellings are refs of the card: {}",
        card["refs"]
    );
    let (error, answer) = call(&app, "social", json!({"ids": [root_id]})).await;
    assert!(!error, "{answer}");
    let sent = asked.lock().unwrap().concat();
    assert!(sent.contains(root), "{sent}");
    assert!(!sent.contains("SECRET"), "{sent}");
    assert!(!answer.to_string().contains("SECRET"), "{answer}");
    // Asked by such a link, the tool refuses before any batch.
    let before = asked.lock().unwrap().len();
    let (error, answer) = call(&app, "social", json!({"uris": [share]})).await;
    assert!(error, "{answer}");
    assert_eq!(answer["status"], 400);
    assert!(!answer.to_string().contains("SECRET"), "{answer}");
    assert_eq!(asked.lock().unwrap().len(), before);
}
