//! The `mempool` source's also-on half on a stored card: a block card read through the API lists
//! the other explorers in `also_on`, each of those links reads back as the card, and the block's
//! links on other explorers that people tag are aliases of it.
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

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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

    // People tag the block on three other explorers.
    let tagged = [
        format!("https://blockstream.info/block/{HASH}"),
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
            "OKLink",
            "mempool.emzy.de"
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
