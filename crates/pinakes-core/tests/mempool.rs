//! The `mempool` source's also-on half on a stored card: a block card read through the API lists
//! the other explorers in `also_on`, each of those links reads back as the card, and the block's
//! links on other explorers that people tag are aliases of it. And a chain card's `related` list,
//! read with the card from recorded answers (mempool.space, or Esplora when it fails), on the
//! resource read, with the mempool.space page that opens it in `open_in`.
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Source},
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    sources::mempool::{self, also_on::Explorers},
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
mod common;

const HASH: &str = "0000000000000000001c8018d9cb3b742ef25114f27563e3fc4a1902167f9893";

async fn by_uri(app: &axum::Router, uri: &str) -> Value {
    let path = format!(
        "/v1/resources/by-uri?uri={}",
        url::form_urlencoded::byte_serialize(uri.as_bytes()).collect::<String>()
    );
    let mut request = Request::builder().uri(&path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200, "{uri}");
    let v: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    common::check_response("GET", &path, 200, &v);
    v
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_block_card_lists_the_explorers_and_their_links_are_its_aliases(pool: PgPool) {
    let raw = std::fs::read(format!(
        "{}/tests/fixtures/sources/mempool/block-481824.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let card = mempool::parse_block(200, &raw, mempool::Network::Mainnet, HASH)
        .unwrap()
        .unwrap();
    let root = card.id.clone();
    assert_eq!(root, format!("btc:block/{HASH}"));
    let doc = card.into_doc(ResourceMeta::new(&root, "mempool", 3600), mempool::SITE);
    let id = store::ensure_resource(&pool, &root).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();

    // The also-on half runs on the stored card, as the worker runs it.
    let subject = common::media::subject(&root, Some("BitcoinBlock"), Some(doc));
    assert!(Explorers.applies(&subject));
    let cx = common::media::cx(common::media::Recorded::with_bodies(&[]), Config::default());
    let found = Explorers.lookup(&cx, &subject).await.unwrap();
    also_on::record(
        &pool,
        &id,
        "mempool",
        found,
        std::time::Duration::from_secs(86_400),
        &Config::default().also_on,
    )
    .await
    .unwrap();

    // People tag the block on four other explorers, mempool.emzy.de among them, which the card
    // no longer lists but still reads.
    let tagged = [
        format!("https://blockstream.info/block/{HASH}"),
        format!("https://mempool.emzy.de/block/{HASH}"),
        format!("https://www.blockchain.com/explorer/blocks/btc/{HASH}"),
        format!("https://blockchair.com/bitcoin/block/{HASH}"),
    ];
    for uri in &tagged {
        let alias = store::ensure_resource(&pool, uri).await.unwrap();
        assert_ne!(alias, id, "{uri}");
    }

    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = by_uri(&app, &root).await;
    assert_eq!(read["id"], id.as_str());
    assert_eq!(read["site"], "mempool.space");
    let also_on = read["also_on"].as_array().unwrap();
    let mut sites: Vec<&str> = also_on
        .iter()
        .map(|e| e["site"].as_str().unwrap())
        .collect();
    sites.sort_unstable();
    assert_eq!(
        sites,
        [
            "BTCScan",
            "Blockchain.com",
            "Blockstream Explorer",
            "OKLink"
        ],
        "{read}"
    );
    for e in also_on {
        let uri = e["uri"].as_str().unwrap();
        assert_eq!(e["derived"], true, "{uri}");
        assert_eq!(
            mempool::from_uri(uri)
                .as_ref()
                .and_then(mempool::identifier),
            Some(root.clone()),
            "{uri}"
        );
    }
    // Each tagged explorer link answers with the card.
    for uri in &tagged {
        let read = by_uri(&app, uri).await;
        assert_eq!(read["id"], id.as_str(), "{uri}");
        let spelling = pinakes_core::model::identity(uri).unwrap().1;
        assert!(
            read["aliases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == spelling.as_str()),
            "{read}"
        );
    }
}

/// A recorded answer: URL, status, content type, file.
type Answer = (String, u16, &'static str, String);

/// The answers `source_card` serves for a read of the example block by hash: on `host`'s API
/// (mempool.space's v1 record, or Esplora's), with the files of `dir`.
fn block_answers(api: &str, record: &str, dir: &str) -> Vec<Answer> {
    vec![
        (
            format!("{api}/{record}/{HASH}"),
            200,
            "application/json",
            format!("{dir}block-481824.json"),
        ),
        (
            format!("{api}/block/{HASH}/status"),
            200,
            "application/json",
            format!("{dir}block-481824-status.json"),
        ),
        // The tip as mempool.space gave it (Esplora's is the same chain's).
        (
            format!("{api}/blocks/tip/height"),
            200,
            "text/plain",
            "tip-height-1111.txt".into(),
        ),
        (
            format!("{api}/block/{HASH}/txid/0"),
            200,
            "text/plain",
            format!("{dir}block-481824-txid-0.txt"),
        ),
    ]
}

async fn read_card(uri: &str, answers: &[Answer]) -> ResourceMeta {
    let answers: Vec<(&str, u16, &str, &str)> = answers
        .iter()
        .map(|(u, s, k, f)| (u.as_str(), *s, *k, f.as_str()))
        .collect();
    common::source_card("mempool", uri, &answers).await.unwrap()
}

/// A block card read from mempool.space lists its previous block, its settled next block and its
/// coinbase transaction (with the reward), and the resource read serves them as the OpenAPI
/// document says; each entry is a root the source reads. When mempool.space fails, Esplora's
/// answers give the same list, without the reward it does not know.
#[sqlx::test(migrations = "../../migrations")]
async fn a_block_card_lists_its_neighbours_and_coinbase(pool: PgPool) {
    let root = format!("btc:block/{HASH}");
    let doc = read_card(
        &root,
        &block_answers("https://mempool.space/api", "v1/block", ""),
    )
    .await;
    assert_eq!(doc.status, pinakes_core::model::Status::Ok);
    let coinbase = "da917699942e4a96272401b534381a75512eeebe8403084500bd637bd47168b3";
    let expected = serde_json::json!([
        {"rel": "previous", "uri": "btc:block/000000000000000000cbeff0b533f8e1189cf09dfbebf57a8ebe349362811b80", "name": "Block 481,823"},
        {"rel": "next", "uri": "btc:block/000000000000000000daf7a26d903543377d5cdddb962077e58fd11212479eea", "name": "Block 481,825"},
        {"rel": "coinbase", "uri": format!("btc:tx/{coinbase}"), "amount_sat": 1462514269},
    ]);
    assert_eq!(serde_json::to_value(&doc.related).unwrap(), expected);
    assert!(doc.related_total.is_empty());
    // Settled and complete: no early read again.
    assert_eq!(doc.refresh_by, None);
    let source = pinakes_core::sources::by_name("mempool").unwrap();
    for r in &doc.related {
        let uri = r.uri.as_deref().unwrap();
        assert!(source.reads(uri) && !source.invalid(uri), "{uri}");
    }

    let id = store::ensure_resource(&pool, &root).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = by_uri(&app, &root).await;
    assert_eq!(read["related"], expected, "{read}");
    assert_eq!(read["related_total"], Value::Null);
    assert_eq!(read["facts"]["pool"], "BTCC");
    // Its link is not a web page: the card opens in the mempool.space page it was read from.
    assert_eq!(
        read["open_in"],
        serde_json::json!([{"client": "mempool.space", "url": format!("https://mempool.space/block/{HASH}")}]),
        "{read}"
    );

    // mempool.space answers a server error (any body): the fallback reads the same parts.
    let mut answers = block_answers("https://blockstream.info/api", "block", "esplora/");
    answers.push((
        format!("https://mempool.space/api/v1/block/{HASH}"),
        503,
        "application/json",
        "tx-missing.txt".into(),
    ));
    let esplora = read_card(&root, &answers).await;
    let mut without_reward = expected.clone();
    without_reward[2]
        .as_object_mut()
        .unwrap()
        .remove("amount_sat");
    assert_eq!(
        serde_json::to_value(&esplora.related).unwrap(),
        without_reward
    );
}

/// An address card lists its latest 10 transactions and counts them all; when the list's request
/// is put off (a server error), the card is still read and comes back within the hour; a list
/// the API refuses (404) is left out until the card's own refresh.
#[sqlx::test(migrations = "../../migrations")]
async fn an_address_card_lists_its_latest_transactions(pool: PgPool) {
    let addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    let root = format!("btc:address/{addr}");
    let record = (
        format!("https://mempool.space/api/address/{addr}"),
        200,
        "application/json",
        "address-bech32-1111.json".to_owned(),
    );
    let txs = format!("https://mempool.space/api/address/{addr}/txs");
    let doc = read_card(
        &root,
        &[
            record.clone(),
            (
                txs.clone(),
                200,
                "application/json",
                "address-bech32-txs.json.gz".into(),
            ),
        ],
    )
    .await;
    assert_eq!(doc.related.len(), 10);
    assert!(doc.related.iter().all(|r| r.rel == "tx" && r.at.is_some()));
    assert_eq!(doc.related[0].amount_sat, Some(100000));
    assert_eq!(doc.related_total.get("tx"), Some(&125));
    assert_eq!(doc.refresh_by, None);

    let id = store::ensure_resource(&pool, &root).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = by_uri(&app, &root).await;
    assert_eq!(read["related"].as_array().unwrap().len(), 10, "{read}");
    assert_eq!(
        read["related"][0]["uri"],
        "btc:tx/d54945326574a95dab44fafcb1c3b04d239f2de106a8b1aa9313208ee2cfa80a"
    );
    assert_eq!(read["related_total"], serde_json::json!({"tx": 125}));
    assert_eq!(read["facts"]["tx_count"], "125");

    let put_off = read_card(
        &root,
        &[
            record.clone(),
            (
                txs.clone(),
                503,
                "application/json",
                "tx-missing.txt".into(),
            ),
        ],
    )
    .await;
    assert_eq!(put_off.status, pinakes_core::model::Status::Ok);
    assert!(put_off.related.is_empty());
    let soon = put_off.refresh_by.unwrap() - chrono::Utc::now();
    assert!(soon <= chrono::Duration::hours(1) && soon > chrono::Duration::minutes(50));
    // The list refused (a 404 for it): the card stands without it, and is not read again early.
    let refused = read_card(
        &root,
        &[record, (txs, 404, "text/plain", "tx-missing.txt".into())],
    )
    .await;
    assert_eq!(refused.status, pinakes_core::model::Status::Ok);
    assert!(refused.related.is_empty());
    assert_eq!(refused.refresh_by, None);
}

/// Other cards' reads carry an empty list and no totals.
#[sqlx::test(migrations = "../../migrations")]
async fn other_cards_have_no_related_objects(pool: PgPool) {
    let uri = "https://example.org/page";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "web", 3600);
    doc.name = Some("A page".into());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = by_uri(&app, uri).await;
    assert_eq!(read["related"], serde_json::json!([]));
    assert_eq!(read["related_total"], Value::Null);
}
