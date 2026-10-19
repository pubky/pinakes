//! Official-API sources through the cards resolver, with a scripted transport: how a slow or
//! failing service ends.
use anyhow::Result;
use async_trait::async_trait;
use pinakes_core::{
    config::Config,
    model::Status,
    resolver::{
        cards::CardResolver,
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        error_class, Resolver, RetryLater,
    },
};
use std::{net::SocketAddr, sync::Arc};
use url::Url;

struct PublicDns;
#[async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

/// USGS never answers; mempool.space fails every node record with a server error and its search
/// lists only `LISTED`.
struct Scripted;
const LISTED: &str = "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f";
const UNKNOWN: &str = "020000000000000000000000000000000000000000000000000000000000000001";

fn answer(url: &Url, status: u16, body: String) -> Fetched {
    Fetched {
        permanent_redirect: false,
        truncated: false,
        validators: Default::default(),
        url: url.clone(),
        content_type: "application/json".into(),
        status,
        retry_after_secs: 0,
        location: None,
        body: body.into_bytes(),
    }
}

#[async_trait]
impl Transport for Scripted {
    async fn get(&self, u: &Url, addrs: &[SocketAddr]) -> Result<Fetched> {
        self.get_with(u, addrs, &Default::default()).await
    }
    async fn get_with(&self, u: &Url, _: &[SocketAddr], _: &FetchOptions) -> Result<Fetched> {
        match (u.host_str(), u.path()) {
            (Some("earthquake.usgs.gov"), _) => std::future::pending().await,
            (Some("mempool.space"), path) if path.starts_with("/api/v1/lightning/nodes/") => {
                Ok(answer(u, 500, r#"{"error":"Failed to get node"}"#.into()))
            }
            (Some("mempool.space"), "/api/v1/lightning/search") => {
                let asked = u
                    .query_pairs()
                    .find(|(k, _)| k == "searchText")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let nodes = if asked == LISTED {
                    format!(r#"[{{"public_key":"{LISTED}","alias":"ACINQ"}}]"#)
                } else {
                    "[]".into()
                };
                Ok(answer(
                    u,
                    200,
                    format!(r#"{{"nodes":{nodes},"channels":[]}}"#),
                ))
            }
            _ => anyhow::bail!("no script for {u}"),
        }
    }
}

fn official() -> CardResolver {
    CardResolver::new(
        "official",
        Arc::new(Config::default()),
        Egress::with_backends(Default::default(), Arc::new(PublicDns), Arc::new(Scripted)),
    )
}

/// A USGS fetch that outlasts its timeout is the non-counting retry, never the route's own
/// `timeout` (which counts toward dead letter): the route leaves the fetch room to time out first.
#[tokio::test(start_paused = true)]
async fn a_slow_usgs_answer_is_a_retry_that_never_counts() {
    let error = official()
        .resolve("https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511")
        .await
        .unwrap_err();
    let retry = error.downcast_ref::<RetryLater>().expect("a retry");
    assert_eq!(retry.class, "upstream_denied");
    assert_eq!(retry.seconds, pinakes_core::sources::usgs::SLOW_RETRY_SECS);
}

/// mempool.space answers a server error for a node it doesn't know: the search decides.
#[tokio::test(start_paused = true)]
async fn an_unknown_lightning_node_is_missing_and_a_listed_one_counts() {
    let doc = official()
        .resolve(&format!("https://mempool.space/lightning/node/{UNKNOWN}"))
        .await
        .unwrap();
    assert_eq!(doc.status, Status::Missing);
    let error = official()
        .resolve(&format!("https://mempool.space/lightning/node/{LISTED}"))
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<RetryLater>().is_none(), "{error:#}");
    assert_eq!(error_class(&error), "http_5xx");
}
