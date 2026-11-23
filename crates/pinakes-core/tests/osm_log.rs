//! The OSM request log line, in a test binary of its own. A callsite's interest is cached for the
//! whole process: while one dispatcher exists (this test's, scoped to its thread), another
//! thread's first hit on the `OSM upstream request` callsite registers it against that thread's
//! default (none), caches `never`, and this test's line is dropped (tracing-core's
//! `Rebuilder::JustOne`). Every geo_osm test makes OSM requests on its own thread, so beside them
//! this test failed now and then; alone in its binary, no other thread runs.
use axum::{http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use pinakes_core::{
    config::{OsmConfig, QueueConfig},
    queue,
    resolver::osm::OsmResolver,
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};

/// A Nominatim configured under a base path, which may hold a token: its requests are logged by
/// the path below the base (`lookup`), never the base's own path or the query. That holds for an
/// answer, a refusal (503) and a server out of reach, and for every line down to TRACE.
#[sqlx::test(migrations = "../../migrations")]
async fn an_osm_request_is_logged_below_its_configured_base(pool: PgPool) {
    #[derive(Clone, Default)]
    struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/BASEPATH-CANARY/", listener.local_addr().unwrap());
    // What the server answers: the place, or this status.
    let answer = Arc::new(AtomicU16::new(200));
    let app = Router::new().route(
        "/BASEPATH-CANARY/lookup",
        get({
            let answer = answer.clone();
            move || {
                let answer = answer.clone();
                async move {
                    match answer.load(Ordering::SeqCst) {
                        200 => Json(json!([{"osm_type":"node","osm_id":1,"lat":"46","lon":"8"}]))
                            .into_response(),
                        status => StatusCode::from_u16(status).unwrap().into_response(),
                    }
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = |base: String| OsmConfig {
        nominatim_url: base.into(),
        requests_per_second: 1000.0,
        ..Default::default()
    };
    let resolver = OsmResolver::new(pool.clone(), config(base)).unwrap();
    // A second Nominatim, under the same base path, at an address nothing listens on.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nowhere = format!("http://{}/BASEPATH-CANARY/", closed.local_addr().unwrap());
    drop(closed);
    let unreachable = OsmResolver::new(pool.clone(), config(nowhere)).unwrap();
    let cfg = QueueConfig::default();
    let logs = Lines::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    // This thread's default: the test's runtime runs the batches on it.
    let guard = tracing::subscriber::set_default(subscriber);
    for (node, status, resolver) in [
        (1, 200, &resolver),
        (2, 503, &resolver),
        (3, 200, &unreachable),
    ] {
        answer.store(status, Ordering::SeqCst);
        store::ensure_resource(&pool, &format!("https://www.openstreetmap.org/node/{node}"))
            .await
            .unwrap();
        let jobs = queue::claim_batch(&pool, "osm", &cfg, 50).await.unwrap();
        assert_eq!(jobs.len(), 1, "node {node}");
        // A failed read is the job's (put off with its class), not the batch's.
        queue::run_batch(&pool, resolver, &cfg, &jobs)
            .await
            .unwrap();
    }
    drop(guard);
    server.abort();
    // The requests ran: a missing line below is the log's, not a request that never happened.
    let stored: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT status, last_error FROM resource ORDER BY uri")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored,
        [
            ("ok".to_owned(), None),
            ("pending".to_owned(), Some("http_5xx".to_owned())),
            ("pending".to_owned(), Some("connect".to_owned())),
        ]
    );
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let requests: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|l| l["fields"]["message"] == "OSM upstream request")
        .collect();
    // One line per answer; the server out of reach gave none.
    assert_eq!(requests.len(), 2, "{text}");
    for (request, status) in requests.iter().zip([200, 503]) {
        assert_eq!(request["fields"]["path"], "lookup", "{text}");
        assert_eq!(request["fields"]["status"], status, "{text}");
    }
    for secret in ["BASEPATH-CANARY", "osm_ids", "jsonv2"] {
        assert!(!text.contains(secret), "{secret}: {text}");
    }
}
