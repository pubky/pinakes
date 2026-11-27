//! The Wikidata importer's API requests take the daemon's egress turn for their host
//! (`net::share_egress_pace`), for each HTTP exchange only: a wait Wikidata asks for (`maxlag`)
//! leaves the host to the egress's own requests. Its own test binary: the shared egress is set
//! once for the whole process.
use axum::{response::IntoResponse, routing::get, Json, Router};
use pinakes_core::{
    catalogue::wikidata::net,
    config::EgressConfig,
    resolver::egress::{Dns, Egress, Fetched, Transport},
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Nothing is ever fetched through the test's egress: only its turns are taken.
struct Offline;
#[async_trait::async_trait]
impl Dns for Offline {
    async fn lookup(&self, _: &str, _: u16) -> anyhow::Result<Vec<SocketAddr>> {
        anyhow::bail!("offline")
    }
}
#[async_trait::async_trait]
impl Transport for Offline {
    async fn get(&self, _: &url::Url, _: &[SocketAddr]) -> anyhow::Result<Fetched> {
        anyhow::bail!("offline")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_egress_turn_is_held_per_exchange_never_through_a_wait() {
    // The API: first Wikidata's `maxlag` refusal (wait 2 s), then an answer it holds until the
    // test lets it go.
    let hits = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let (seen, go) = (hits.clone(), release.clone());
    let app = Router::new().route(
        "/w/api.php",
        get(move || {
            let (seen, go) = (seen.clone(), go.clone());
            async move {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    (
                        [("retry-after", "2")],
                        Json(serde_json::json!({"error": {"code": "maxlag"}})),
                    )
                        .into_response()
                } else {
                    go.notified().await;
                    Json(serde_json::json!({"batchcomplete": true})).into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/w/api.php", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut cfg = EgressConfig::default();
    cfg.host_rates.insert("127.0.0.1".into(), 50.0);
    let egress = Egress::with_backends(cfg, Arc::new(Offline), Arc::new(Offline));
    net::share_egress_pace(egress.clone());
    let client = net::client("ops@example.org", true, true).unwrap();
    let asked = tokio::spawn(async move {
        net::api_capped(&client, &base, &[("action", "query")], 1 << 20).await
    });
    while hits.load(Ordering::SeqCst) < 1 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // During the wait Wikidata asked for, the egress gets the host, and the importer's next
    // request waits for it: held past the importer's wait, no second request comes.
    let turn = tokio::time::timeout(Duration::from_secs(10), egress.host_turn("127.0.0.1"))
        .await
        .expect("the egress waits through the importer's maxlag wait")
        .unwrap();
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the importer asked while the egress had the host"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the importer asked while the egress had the host"
    );
    drop(turn);
    // The importer's next exchange holds the host until its answer is read.
    while hits.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let turn =
        tokio::time::timeout(Duration::from_millis(500), egress.host_turn("127.0.0.1")).await;
    assert!(
        turn.is_err(),
        "the egress went beside the importer's request"
    );
    release.notify_one();
    asked.await.unwrap().unwrap();
}
