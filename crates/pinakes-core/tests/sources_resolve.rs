//! Official-API sources through the cards resolver, with a scripted transport: how a slow or
//! failing service ends.
use anyhow::Result;
use async_trait::async_trait;
use pinakes_core::{
    config::Config,
    model::Status,
    resolver::{
        cards::CardResolver,
        egress::{Egress, FetchOptions, Fetched, Transport},
        error_class, Resolver, RetryLater,
    },
};
use std::{net::SocketAddr, sync::Arc};
use url::Url;
mod common;
use common::media::PublicDns;

/// USGS never answers; mempool.space answers the record of `BUSY` with 429 and that of `SLOW`
/// with 408, fails every other node record with a server error, and its search lists only
/// `LISTED`, `BUSY` and `SLOW`; it answers the record of the address `STALLED` but never its
/// transaction list. It answers the record of the transaction `LIMITED` with 429 and that of
/// `FAILING` with 503, and knows no other transaction; blockstream.info knows none.
struct Scripted;
const LISTED: &str = "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f";
const UNKNOWN: &str = "020000000000000000000000000000000000000000000000000000000000000001";
const BUSY: &str = "021111111111111111111111111111111111111111111111111111111111111111";
const SLOW: &str = "022222222222222222222222222222222222222222222222222222222222222222";
const STALLED: &str = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";
/// Blocks whose tip and first-txid requests mempool.space answers 404: `DEEP` has a next block,
/// `TIP` has none yet.
const DEEP: &str = "00000000000000000000000000000000000000000000000000000000000000d1";
const TIP: &str = "00000000000000000000000000000000000000000000000000000000000000d2";
const NEXT: &str = "00000000000000000000000000000000000000000000000000000000000000d3";
const LIMITED: &str = "00000000000000000000000000000000000000000000000000000000000000e1";
const FAILING: &str = "00000000000000000000000000000000000000000000000000000000000000e2";
const GONE: &str = "00000000000000000000000000000000000000000000000000000000000000e3";

fn answer(url: &Url, status: u16, body: String) -> Fetched {
    Fetched {
        retry_after_secs: 0,
        ..common::fetched(
            url.clone(),
            status,
            "application/json".into(),
            body.into_bytes(),
        )
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
            (Some("mempool.space"), path) if path.strip_prefix("/api/address/") == Some(STALLED) => {
                Ok(answer(
                    u,
                    200,
                    format!(
                        r#"{{"address":"{STALLED}","chain_stats":{{"funded_txo_sum":5000000000,"spent_txo_sum":0,"tx_count":1}},"mempool_stats":{{"tx_count":0}}}}"#
                    ),
                ))
            }
            (Some("mempool.space"), path)
                if path.strip_prefix("/api/address/").and_then(|p| p.strip_suffix("/txs")) == Some(STALLED) =>
            {
                std::future::pending().await
            }
            (Some("mempool.space"), path)
                if path.strip_prefix("/api/v1/block/") == Some(DEEP)
                    || path.strip_prefix("/api/v1/block/") == Some(TIP) =>
            {
                let id = &path["/api/v1/block/".len()..];
                Ok(answer(
                    u,
                    200,
                    format!(
                        r#"{{"id":"{id}","height":5,"timestamp":1231006505,"tx_count":1,"size":285,"weight":1140,"previousblockhash":"00000000000000000000000000000000000000000000000000000000000000d0"}}"#
                    ),
                ))
            }
            (Some("mempool.space"), path) if path.strip_prefix("/api/block/").and_then(|p| p.strip_suffix("/status")) == Some(DEEP) => {
                Ok(answer(
                    u,
                    200,
                    format!(r#"{{"in_best_chain":true,"height":5,"next_best":"{NEXT}"}}"#),
                ))
            }
            (Some("mempool.space"), path) if path.strip_prefix("/api/block/").and_then(|p| p.strip_suffix("/status")) == Some(TIP) => Ok(
                answer(u, 200, r#"{"in_best_chain":true,"height":5,"next_best":null}"#.into()),
            ),
            (Some("mempool.space"), path)
                if path == "/api/blocks/tip/height" || path.ends_with("/txid/0") =>
            {
                Ok(answer(u, 404, "Not found".into()))
            }
            (Some("mempool.space"), path) if path.strip_prefix("/api/tx/") == Some(LIMITED) => {
                Ok(answer(u, 429, "Too Many Requests".into()))
            }
            (Some("mempool.space"), path) if path.strip_prefix("/api/tx/") == Some(FAILING) => {
                Ok(answer(u, 503, "Service Unavailable".into()))
            }
            (Some("mempool.space" | "blockstream.info"), path) if path.starts_with("/api/tx/") => {
                Ok(answer(u, 404, "Transaction not found".into()))
            }
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

/// While mempool.space fails a transaction's record (a rate limit, a server error), the
/// fallback's "not found" does not make the transaction missing: the read ends as
/// mempool.space's error, a retry. mempool.space's own "not found" is `missing`.
#[tokio::test(start_paused = true)]
async fn a_transaction_only_the_fallback_calls_unknown_is_a_retry() {
    if !has("mempool") {
        return;
    }
    for (txid, class) in [(LIMITED, "http_4xx"), (FAILING, "upstream")] {
        let error = official()
            .resolve(&format!("btc:tx/{txid}"))
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<RetryLater>().expect("a retry");
        assert_eq!(retry.class, class, "{txid}");
    }
    let doc = official().resolve(&format!("btc:tx/{GONE}")).await.unwrap();
    assert_eq!(doc.status, Status::Missing);
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

/// A card whose `related` part never answers is still read in time: the address card comes back
/// without its transaction list, to be read again within the hour, never as the route's own
/// timeout (which counts toward dead letter).
#[tokio::test(start_paused = true)]
async fn a_stalled_related_part_leaves_the_card_in_time() {
    if !has("mempool") {
        return;
    }
    let started = tokio::time::Instant::now();
    let doc = official()
        .resolve(&format!("btc:address/{STALLED}"))
        .await
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(doc.status, Status::Ok);
    assert_eq!(doc.attributes["tx_count"], "1");
    assert!(doc.related.is_empty());
    let soon = doc.refresh_by.expect("read again soon") - chrono::Utc::now();
    assert!(soon <= chrono::Duration::hours(1));
}

/// A part that fails for good (a 404) is left out without an early read again: a block with a
/// next block whose tip the API refuses keeps its previous block and is read at its own time. The
/// best chain's tip itself is read again within the hour, for its next block.
#[tokio::test(start_paused = true)]
async fn a_part_refused_for_good_is_not_read_again_soon() {
    if !has("mempool") {
        return;
    }
    let deep = official()
        .resolve(&format!("btc:block/{DEEP}"))
        .await
        .unwrap();
    assert_eq!(deep.status, Status::Ok);
    let rels: Vec<&str> = deep.related.iter().map(|r| r.rel.as_str()).collect();
    assert_eq!(rels, ["previous"]);
    assert_eq!(deep.related[0].name.as_deref(), Some("Block 4"));
    assert_eq!(deep.refresh_by, None);
    let tip = official()
        .resolve(&format!("btc:block/{TIP}"))
        .await
        .unwrap();
    assert_eq!(tip.status, Status::Ok);
    assert!(tip.refresh_by.is_some());
}
