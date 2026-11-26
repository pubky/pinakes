//! Official-API sources through the cards resolver, with a scripted transport: how a slow or
//! failing service ends.
use anyhow::Result;
use async_trait::async_trait;
use pinakes_core::test_support::needs_sources;
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
            // The US storefront does not sell the app; Switzerland does; `zz` is no storefront.
            (Some("itunes.apple.com"), "/lookup") => {
                let file = match u.query() {
                    Some("id=1225135057") => "lookup-1225135057.json",
                    Some("id=1225135057&country=ch") => "lookup-1225135057-ch.json",
                    Some("id=1225135057&country=zz") => "lookup-1225135057-zz.json",
                    _ => anyhow::bail!("no script for {u}"),
                };
                let body = std::fs::read(format!(
                    "{}/tests/fixtures/sources/app_store/{file}",
                    env!("CARGO_MANIFEST_DIR")
                ))?;
                let status = if file.ends_with("-zz.json") { 400 } else { 200 };
                let mut lookup = answer(u, status, String::from_utf8(body)?);
                lookup.content_type = "text/javascript; charset=utf-8".into();
                Ok(lookup)
            }
            // X walls every profile page.
            (Some("x.com"), _) => {
                let mut page = answer(u, 403, "<html>Sign in to X</html>".into());
                page.content_type = "text/html".into();
                Ok(page)
            }
            (Some("pubchem.ncbi.nlm.nih.gov"), "/compound/glucose") => {
                let mut page = answer(u, 200, "<title>Glucose</title>".into());
                page.content_type = "text/html".into();
                Ok(page)
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
    if !needs_sources(&["usgs"]) {
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
    if !needs_sources(&["mempool"]) {
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
    if !needs_sources(&["mempool"]) {
        return;
    }
    for (key, class) in [(BUSY, "http_4xx"), (SLOW, "http_4xx")] {
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
    if !needs_sources(&["mempool"]) {
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
    if !needs_sources(&["pubchem"]) {
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
    if !needs_sources(&["mempool"]) {
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
    if !needs_sources(&["mempool"]) {
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

/// After a job, a card of a thing that happened (a mined block, its time long past) is final,
/// and a card of a thing with no time of its own (a compound) is read again at the route's
/// lifetime.
#[sqlx::test(migrations = "../../migrations")]
async fn a_past_dated_official_card_is_final_after_its_job(pool: sqlx::PgPool) {
    use pinakes_core::{queue, store};
    if !needs_sources(&["mempool", "pubchem"]) {
        return;
    }
    let resolver = official();
    let mut rows = vec![];
    for uri in [
        format!("btc:block/{DEEP}"),
        "https://pubchem.ncbi.nlm.nih.gov/compound/aspirin".into(),
    ] {
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let job = queue::Job {
            id: id.clone(),
            uri,
            attempts: 0,
            generation: 0,
        };
        queue::run_job(&pool, &resolver, &Default::default(), &job)
            .await
            .unwrap();
        let row: (String, bool, f64) = sqlx::query_as(
            "SELECT status,final,coalesce(extract(epoch FROM refresh_at-now()),0)::float8 FROM resource WHERE id=$1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        rows.push(row);
    }
    assert_eq!((rows[0].0.as_str(), rows[0].1), ("ok", true), "the block");
    assert_eq!(
        (rows[1].0.as_str(), rows[1].1),
        ("ok", false),
        "the compound"
    );
    // The official route's lifetime (a day), not the dateless page's week.
    assert!((82_800.0..=86_400.0).contains(&rows[1].2), "{}", rows[1].2);
}

/// A social site that walls a refresh keeps the last good card, as a wall on the web route does,
/// and the link keeps refreshing (never dead-lettered); a link never read gets the placeholder of
/// its shape.
#[sqlx::test(migrations = "../../migrations")]
async fn a_walled_refresh_keeps_the_last_good_card(pool: sqlx::PgPool) {
    use pinakes_core::{model::ResourceMeta, queue, store};
    if !needs_sources(&["x"]) {
        return;
    }
    let resolver = official();
    let job = |id: &str, uri: &str| queue::Job {
        id: id.into(),
        uri: uri.into(),
        attempts: 0,
        generation: 0,
    };
    let row = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Option<String>, String)>(
                "SELECT doc->>'name', doc->'attributes'->>'blocked', status FROM resource WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let read = "https://x.com/someone";
    let id = store::ensure_resource(&pool, read).await.unwrap();
    let mut good = ResourceMeta::new(read, "x", 3600);
    good.set_type("Person");
    good.name = Some("Some One".into());
    good.description = Some("Builds bridges.".into());
    store::save_document(&pool, &id, 0, &good).await.unwrap();
    // Walls again and again: the card stays, and the link is never counted toward dead letter.
    for _ in 0..6 {
        queue::run_job(&pool, &resolver, &Default::default(), &job(&id, read))
            .await
            .unwrap();
    }
    let (name, blocked, status) = row(id.clone()).await;
    assert_eq!((name.as_deref(), blocked), (Some("Some One"), None));
    assert_eq!(status, "ok");
    let (attempts, dead, error): (i32, bool, Option<String>) =
        sqlx::query_as("SELECT attempts, dead_letter, last_error FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((attempts, dead, error.as_deref()), (0, false, Some("wall")));

    let never = "https://x.com/someone_else";
    let id = store::ensure_resource(&pool, never).await.unwrap();
    queue::run_job(&pool, &resolver, &Default::default(), &job(&id, never))
        .await
        .unwrap();
    let (name, blocked, status) = row(id).await;
    assert_eq!(
        (name.as_deref(), blocked.as_deref(), status.as_str()),
        (Some("Profile on x.com"), Some("true"), "ok")
    );
}

/// A profile is social content: once it is gone it keeps nothing of itself (no name, bio or
/// picture), as a gone post does.
#[sqlx::test(migrations = "../../migrations")]
async fn a_gone_profile_is_purged_like_a_post(pool: sqlx::PgPool) {
    use pinakes_core::{model::ResourceMeta, store};
    if !needs_sources(&["x"]) {
        return;
    }
    let uri = "https://x.com/someone";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut good = ResourceMeta::new(uri, "x", 3600);
    good.set_type("Person");
    good.name = Some("Some One".into());
    good.description = Some("Builds bridges.".into());
    good.image = Some("https://pbs.twimg.com/profile_images/1/a.jpg".into());
    store::save_document(&pool, &id, 0, &good).await.unwrap();
    // Gone, and confirmed by a second fresh read.
    for later in [1, 2] {
        let mut gone = ResourceMeta::new(uri, "x", 3600);
        gone.status = Status::Missing;
        gone.fetched_at = good.fetched_at + later;
        store::save_document(&pool, &id, 0, &gone).await.unwrap();
    }
    let (status, name, description, image): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, doc->>'name', doc->>'description', doc->>'image' FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "missing");
    assert_eq!((name, description, image), (None, None, None));
}

/// A Stack Exchange question is social content: once it is deleted (confirmed by a second read)
/// its card keeps nothing of the question (no title, excerpt or asker), as a gone post does.
#[sqlx::test(migrations = "../../migrations")]
async fn a_deleted_stack_exchange_question_is_purged(pool: sqlx::PgPool) {
    use pinakes_core::{model::ResourceMeta, store};
    if !needs_sources(&["stack_exchange"]) {
        return;
    }
    let uri = "https://stackoverflow.com/questions/10567404";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut good = ResourceMeta::new(uri, "stack_exchange", 3600);
    good.set_type("Question");
    good.name = Some("Python global list".into());
    good.description = Some("How do I change a list defined outside a function…".into());
    good.attributes.insert("author".into(), "Someone".into());
    store::save_document(&pool, &id, 0, &good).await.unwrap();
    for later in [1, 2] {
        let mut gone = ResourceMeta::new(uri, "stack_exchange", 3600);
        gone.status = Status::Missing;
        gone.fetched_at = good.fetched_at + later;
        store::save_document(&pool, &id, 0, &gone).await.unwrap();
    }
    let (status, name, description, author): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, doc->>'name', doc->>'description', doc->'attributes'->>'author' FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "missing");
    assert_eq!((name, description, author), (None, None, None));
}

/// An App Store card read from the storefront a link named keeps that storefront with the stored
/// card: the daemon's next jobs, with nothing in memory naming it (as after a restart), read the
/// same record, and a link tagged with a code that is no storefront leaves the card ok.
#[sqlx::test(migrations = "../../migrations")]
async fn an_app_store_card_keeps_its_storefront_across_restarts(pool: sqlx::PgPool) {
    use pinakes_core::{queue, sources, store};
    if !needs_sources(&["app_store"]) {
        return;
    }
    let app_store = sources::by_name("app_store").unwrap();
    let resolver = official();
    let root = "https://apps.apple.com/app/id1225135057";
    let id = store::ensure_resource(&pool, root).await.unwrap();
    let job = queue::Job {
        id: id.clone(),
        uri: root.into(),
        attempts: 0,
        generation: 0,
    };
    let row = || {
        let pool = pool.clone();
        let id = id.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
                "SELECT status, doc->>'name', doc->'attributes'->>'storefront' FROM resource WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let ok = (
        "ok".to_owned(),
        Some("PostFinance TWINT".to_owned()),
        Some("ch".to_owned()),
    );
    // A link names the storefront; the root's job reads it there.
    app_store.alias_root("https://apps.apple.com/ch/app/postfinance-twint/id1225135057");
    queue::run_job(&pool, &resolver, &Default::default(), &job)
        .await
        .unwrap();
    assert_eq!(row().await, ok);
    // The latest link names a made-up code: nothing in memory names Switzerland any more, only
    // the stored card does, and the card stays ok.
    app_store.alias_root("https://apps.apple.com/zz/app/id1225135057");
    for _ in 0..3 {
        queue::run_job(&pool, &resolver, &Default::default(), &job)
            .await
            .unwrap();
        assert_eq!(row().await, ok);
    }
}

/// An official card whose date passed is final only when it was read whole: a "not found" for
/// it (a transaction the service lost track of) keeps the `missing_ttl_secs` re-read.
#[sqlx::test(migrations = "../../migrations")]
async fn a_past_dated_official_card_found_missing_is_read_again(pool: sqlx::PgPool) {
    use pinakes_core::{model::ResourceMeta, queue, store};
    if !needs_sources(&["mempool"]) {
        return;
    }
    let resolver = official();
    let uri = format!("btc:tx/{GONE}");
    let id = store::ensure_resource(&pool, &uri).await.unwrap();
    let mut good = ResourceMeta::new(&uri, "mempool", 86_400);
    good.set_type("Event");
    good.name = Some("Transaction".into());
    good.start = Some("2020-01-03T18:15:05Z".parse().unwrap());
    store::save_document(&pool, &id, 0, &good).await.unwrap();
    let job = queue::Job {
        id: id.clone(),
        uri,
        attempts: 0,
        generation: 0,
    };
    // Not found twice: missing, confirmed.
    for _ in 0..2 {
        queue::run_job(&pool, &resolver, &Default::default(), &job)
            .await
            .unwrap();
    }
    let (status, last, refresh): (String, bool, f64) = sqlx::query_as(
        "SELECT status,final,coalesce(extract(epoch FROM refresh_at-now()),0)::float8 FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), last), ("missing", false));
    assert!((3_000.0..=3_600.0).contains(&refresh), "{refresh}");
}
