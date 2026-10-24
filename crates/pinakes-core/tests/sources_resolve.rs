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

/// USGS never answers; mempool.space answers the record of `BUSY` with 429 and that of `SLOW`
/// with 408, fails every other node record with a server error, and its search lists only
/// `LISTED`, `BUSY` and `SLOW`.
struct Scripted;
const LISTED: &str = "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f";
const UNKNOWN: &str = "020000000000000000000000000000000000000000000000000000000000000001";
const BUSY: &str = "021111111111111111111111111111111111111111111111111111111111111111";
const SLOW: &str = "022222222222222222222222222222222222222222222222222222222222222222";

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
                Ok(if path.ends_with(BUSY) {
                    answer(u, 429, "Too Many Requests".into())
                } else if path.ends_with(SLOW) {
                    answer(u, 408, "Request Timeout".into())
                } else {
                    answer(u, 500, r#"{"error":"Failed to get node"}"#.into())
                })
            }
            (Some("mempool.space"), "/api/v1/lightning/search") => {
                let asked = u
                    .query_pairs()
                    .find(|(k, _)| k == "searchText")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let nodes = if [LISTED, BUSY, SLOW].contains(&asked.as_str()) {
                    format!(r#"[{{"public_key":"{asked}","alias":"ACINQ"}}]"#)
                } else {
                    "[]".into()
                };
                Ok(answer(
                    u,
                    200,
                    format!(r#"{{"nodes":{nodes},"channels":[]}}"#),
                ))
            }
            (Some("pubchem.ncbi.nlm.nih.gov"), "/rest/pug/compound/name/glucose/cids/JSON") => Ok(
                answer(u, 200, r#"{"IdentifierList":{"CID":[5793,79025]}}"#.into()),
            ),
            (Some("pubchem.ncbi.nlm.nih.gov"), "/rest/pug/compound/name/aspirin/cids/JSON") => Ok(
                answer(u, 200, r#"{"IdentifierList":{"CID":[2244]}}"#.into()),
            ),
            (Some("pubchem.ncbi.nlm.nih.gov"), path) if path.starts_with("/rest/pug/compound/cid/2244/property/") => Ok(
                answer(u, 200, r#"{"PropertyTable":{"Properties":[{"CID":2244,"MolecularFormula":"C9H8O4","Title":"Aspirin","InChIKey":"BSYNRYMUTXBXSQ-UHFFFAOYSA-N"}]}}"#.into()),
            ),
            (Some("pubchem.ncbi.nlm.nih.gov"), "/compound/glucose") => {
                let mut page = answer(u, 200, "<title>Glucose</title>".into());
                page.content_type = "text/html".into();
                Ok(page)
            }
            _ => anyhow::bail!("no script for {u}"),
        }
    }
}

/// Whether this build has the source a test needs: a test of one source returns early without it.
fn has(source: &str) -> bool {
    pinakes_core::sources::by_name(source).is_some()
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
    if !has("usgs") {
        return;
    }
    let error = official()
        .resolve("https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511")
        .await
        .unwrap_err();
    let retry = error.downcast_ref::<RetryLater>().expect("a retry");
    assert_eq!(retry.class, "upstream_denied");
    // USGS's own slow-answer wait: ten minutes.
    assert_eq!(retry.seconds, 600);
}

/// mempool.space answers a server error for a node it doesn't know: the search decides.
#[tokio::test(start_paused = true)]
async fn an_unknown_lightning_node_is_missing_and_a_listed_one_counts() {
    if !has("mempool") {
        return;
    }
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

/// A rate-limited (429) or timed-out (408) node record is only a retry: the search runs only
/// for a server error, so a listed node is never counted as failing for it.
#[tokio::test(start_paused = true)]
async fn a_rate_limited_lightning_node_is_only_a_retry() {
    if !has("mempool") {
        return;
    }
    for (key, class) in [(BUSY, "http_4xx"), (SLOW, "rate_limited")] {
        let error = official()
            .resolve(&format!("https://mempool.space/lightning/node/{key}"))
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<RetryLater>().expect("a retry");
        assert_eq!(retry.class, class, "{key}");
    }
}

/// A compound name is no identifier: one compound gives its card without merging the page into
/// it, and several give the page an ordinary web card.
#[tokio::test(start_paused = true)]
async fn compound_names_never_merge() {
    if !has("pubchem") {
        return;
    }
    let one = official()
        .resolve("https://pubchem.ncbi.nlm.nih.gov/compound/aspirin")
        .await
        .unwrap();
    assert_eq!(
        (one.type_name.as_str(), one.name.as_deref()),
        ("MolecularEntity", Some("Aspirin"))
    );
    assert_eq!(one.source_root, None, "never an alias of the compound");
    let several = official()
        .resolve("https://pubchem.ncbi.nlm.nih.gov/compound/glucose")
        .await
        .unwrap();
    assert_eq!(several.source, "web");
    assert_eq!(several.name.as_deref(), Some("Glucose"));
}
