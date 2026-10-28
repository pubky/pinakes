//! The OSM request log line, in a test binary of its own. A callsite's interest is cached for the
//! whole process: while one dispatcher exists (this test's, scoped to its thread), another
//! thread's first hit on the `OSM upstream request` callsite registers it against that thread's
//! default (none), caches `never`, and this test's line is dropped (tracing-core's
//! `Rebuilder::JustOne`). Every geo_osm test makes OSM requests on its own thread, so beside them
//! this test failed now and then; alone in its binary, no other thread runs.
use axum::{routing::get, Json, Router};
use pinakes_core::{
    config::{OsmConfig, QueueConfig},
    queue,
    resolver::osm::OsmResolver,
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;

/// A Nominatim configured under a base path, which may hold a token: its requests are logged by
/// the path below the base (`lookup`), never the base's own path or the query.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
    let app = Router::new().route(
        "/BASEPATH-CANARY/lookup",
        get(|| async { Json(json!([{"osm_type":"node","osm_id":1,"lat":"46","lon":"8"}])) }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = OsmConfig {
        nominatim_url: base,
        requests_per_second: 1000.0,
        ..Default::default()
    };
    let resolver = OsmResolver::new(pool.clone(), config).unwrap();
    store::ensure_resource(&pool, "https://www.openstreetmap.org/node/1")
        .await
        .unwrap();
    let cfg = QueueConfig::default();
    let jobs = queue::claim_batch(&pool, "osm", &cfg, 50).await.unwrap();
    let logs = Lines::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    // This thread's default: the test's runtime runs the batch on it.
    let guard = tracing::subscriber::set_default(subscriber);
    queue::run_batch(&pool, &resolver, &cfg, &jobs)
        .await
        .unwrap();
    drop(guard);
    server.abort();
    // The request ran: a missing line below is the log's, not a request that never happened.
    let status: String = sqlx::query_scalar("SELECT status FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "ok");
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let requests: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|l| l["fields"]["message"] == "OSM upstream request")
        .collect();
    assert_eq!(requests.len(), 1, "{text}");
    assert_eq!(requests[0]["fields"]["path"], "lookup", "{text}");
    assert_eq!(requests[0]["fields"]["status"], 200, "{text}");
    for secret in ["BASEPATH-CANARY", "osm_ids", "jsonv2"] {
        assert!(!text.contains(secret), "{secret}: {text}");
    }
}
