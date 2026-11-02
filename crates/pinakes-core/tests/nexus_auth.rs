//! The Nexus's `Authorization` header (`PINAKES_NEXUS_AUTHORIZATION`), in a test binary of its
//! own: it captures every log line, and a callsite's interest is cached for the whole process
//! (see `osm_log.rs`). The daemon's Nexus sends it on every call, and it shows nowhere: not in a
//! log line, an error, `/v1/info` (public or admin), the metrics or an agent's answer.
use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
    Json, Router,
};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    ingest,
    telemetry::Metrics,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

const SECRET: &str = "Bearer NEXUS-CANARY";

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The status and body text of one request, as from a client at a non-loopback address.
async fn send(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, String) {
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
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_nexus_authorization_is_sent_on_every_call_and_shown_nowhere(pool: PgPool) {
    // A Nexus that records what each call carried: the reach list answers, the batch view and
    // the feed fail, so their errors are logged and answered too.
    // Each call's path and the header it carried.
    type Seen = Arc<Mutex<Vec<(&'static str, Option<String>)>>>;
    let seen: Seen = Arc::default();
    let record = |path: &'static str| {
        let seen = seen.clone();
        move |headers: HeaderMap| {
            let auth = headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned());
            seen.lock().unwrap().push((path, auth));
        }
    };
    let (following, by_ids, changes) = (record("following"), record("by_ids"), record("changes"));
    let nexus = Router::new()
        .route(
            "/v0/user/{viewer}/following",
            get(move |headers: HeaderMap| async move {
                following(headers);
                Json(Vec::<String>::new())
            }),
        )
        .route(
            "/v0/stream/resources/by_ids",
            post(move |headers: HeaderMap| async move {
                by_ids(headers);
                (StatusCode::UNAUTHORIZED, "NEXUS-CANARY refused")
            }),
        )
        .route(
            "/v0/resources/changes",
            get(move |headers: HeaderMap| async move {
                changes(headers);
                (StatusCode::UNAUTHORIZED, "NEXUS-CANARY refused")
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nexus_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, nexus).await.unwrap() });
    let config = Config {
        nexus_url: nexus_url.clone(),
        nexus_authorization: Some(SECRET.into()),
        admin_token: Some("operator-token".into()),
        ..Config::default()
    };
    config.validate().unwrap();
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let (app, admin) = (api::router(state.clone()), api::admin_router(state.clone()));

    let logs = Lines::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let mut shown = vec![];
    let viewer = "y".repeat(52);
    let (status, body) = send(
        &app,
        "GET",
        &format!("/v1/search?reach=following&viewer={viewer}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    shown.push(body);
    let mcp = [
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
        ("host", "pinakes.example"),
        ("mcp-protocol-version", "2025-06-18"),
    ];
    let (status, body) = send(
        &app,
        "POST",
        "/mcp",
        &mcp,
        Some(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "social", "arguments": {"uris": ["https://example.org/"]}}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("by_ids"), "the batch view failed: {body}");
    shown.push(body);
    // The feed's error as the daemon logs it (`pinakesd`), and in full.
    let error = ingest::poll_once(
        &pool,
        &nexus_url,
        &state.nexus,
        &Default::default(),
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap_err();
    tracing::warn!(error = %pinakes_core::resolver::log_error(&error), "ingestion will retry");
    shown.push(format!("{error:#} {error:?}"));
    for path in ["/v1/info", "/llms.txt"] {
        let (status, body) = send(&app, "GET", path, &[], None).await;
        assert_eq!(status, 200, "{path}");
        shown.push(body);
    }
    for path in ["/v1/info", "/metrics"] {
        let (status, body) = send(
            &admin,
            "GET",
            path,
            &[("authorization", "Bearer operator-token")],
            None,
        )
        .await;
        assert_eq!(status, 200, "{path}: {body}");
        shown.push(body);
    }
    drop(guard);
    server.abort();

    let seen = seen.lock().unwrap().clone();
    let paths: Vec<&str> = seen.iter().map(|(p, _)| *p).collect();
    assert_eq!(paths, ["following", "by_ids", "changes"]);
    assert!(
        seen.iter().all(|(_, a)| a.as_deref() == Some(SECRET)),
        "{seen:?}"
    );
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("ingestion will retry"), "{logs}");
    shown.push(logs);
    for text in shown {
        assert!(!text.contains("CANARY"), "{text}");
    }
}
