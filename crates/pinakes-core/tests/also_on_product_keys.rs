//! The `product_keys` also-on source: a product card's own barcode is its key, so one product on
//! two shops is one group, found by `key=gtin:`.
mod common;

use common::media::*;
use pinakes_core::{
    also_on::{self, product_keys::ProductKeys, AlsoOnConfig, Source},
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    store,
};
use serde_json::Value;
use sqlx::PgPool;

fn shop_page(uri: &str, gtin: &str) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, "jsonld", 3600);
    doc.set_type("Product");
    doc.name = Some("A product".into());
    doc.attributes.insert("gtin".into(), gtin.into());
    doc
}

#[tokio::test]
async fn only_a_products_own_barcode_is_a_key() {
    let cx = cx(Recorded::new(), Config::default());
    let uri = "https://shop.example/nutella-400";
    let s = subject(uri, Some("Product"), Some(shop_page(uri, "3017620422003")));
    assert!(ProductKeys.applies(&s));
    let found = ProductKeys.lookup(&cx, &s).await.unwrap();
    assert_eq!(keys(&found), ["gtin:3017620422003"]);
    assert_eq!(found.keys[0].via, "page");
    assert!(found.links.is_empty() && found.facts.is_empty());
    // Another card's barcode, or one with a wrong check digit, is none.
    let mut page = shop_page(uri, "3017620422003");
    page.set_type("WebPage");
    assert!(!ProductKeys.applies(&subject(uri, Some("WebPage"), Some(page))));
    assert!(!ProductKeys.applies(&subject(
        uri,
        Some("Product"),
        Some(shop_page(uri, "3017620422004"))
    )));
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn one_barcode_on_two_shops_is_one_group(pool: PgPool) {
    let cx = cx(Recorded::new(), Config::default());
    let mut ids = vec![];
    // The same barcode, spelled as a UPC-A and as an EAN-13.
    for (uri, gtin) in [
        ("https://shop-a.example/mouse", "840440422266"),
        ("https://shop-b.example/p/0840440422266", "0840440422266"),
    ] {
        let mut doc = shop_page(uri, gtin);
        doc.attributes
            .insert("gtin".into(), pinakes_core::barcode::gtin13(gtin).unwrap());
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        let found = ProductKeys
            .lookup(&cx, &subject(uri, Some("Product"), Some(doc)))
            .await
            .unwrap();
        also_on::record(
            &pool,
            &id,
            "product_keys",
            found,
            std::time::Duration::from_secs(86400),
            &AlsoOnConfig::default(),
        )
        .await
        .unwrap();
        pinakes_core::groups::reconcile(&pool, &id, &Default::default()).await;
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','shop',$3,now())")
            .bind(&id)
            .bind("a".repeat(52))
            .bind(format!("/pub/app/tags/{id}"))
            .execute(&pool)
            .await
            .unwrap();
        ids.push(id);
    }
    pinakes_core::groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let card: Value = get(&app, &format!("/v1/resources/{}", ids[0])).await;
    assert_eq!(card["group"]["size"], 2, "{card}");
    assert!(card["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == ids[1].as_str()));
    assert_eq!(card["facts"]["gtin"], "0840440422266");
    // Search by the barcode in any of its spellings.
    for code in ["840440422266", "0840440422266", "00840440422266"] {
        let found: Value = get(&app, &format!("/v1/search?key=gtin:{code}")).await;
        let mut listed: Vec<&str> = found["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        listed.sort();
        let mut expected: Vec<&str> = ids.iter().map(String::as_str).collect();
        expected.sort();
        assert_eq!(listed, expected, "{code}");
    }
}

async fn get(app: &axum::Router, path: &str) -> Value {
    let (status, value) = common::get_from(app, "127.0.0.1:1111", path).await;
    assert_eq!(status, 200, "{path}");
    value
}
