//! The `open_food_facts` source and its also-on half against recorded answers
//! (`tests/fixtures/sources/open_food_facts/`): a database's product page is a Product card with
//! its credit, and a shop's barcode finds the product on the database that holds it.

use pinakes_core::{
    also_on::{Context, Fact, Found, Source as _, Subject},
    config::{Config, EgressConfig},
    model::ResourceMeta,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
    sources::open_food_facts::also_on::OpenFoodFacts,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

const DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/sources/open_food_facts"
);

/// The recorded answers by URL; anything else fails the test.
struct Recorded {
    answers: BTreeMap<String, (u16, Vec<u8>)>,
    seen: Mutex<Vec<String>>,
}
impl Recorded {
    fn new() -> Arc<Self> {
        let provenance: Value =
            serde_json::from_slice(&std::fs::read(format!("{DIR}/provenance.json")).unwrap())
                .unwrap();
        let answers = provenance
            .as_object()
            .unwrap()
            .values()
            .map(|p| {
                let body = std::fs::read(format!("{DIR}/{}", p["file"].as_str().unwrap())).unwrap();
                (
                    p["url"].as_str().unwrap().to_owned(),
                    (p["status"].as_u64().unwrap() as u16, body),
                )
            })
            .collect();
        Arc::new(Self {
            answers,
            seen: Mutex::default(),
        })
    }
}
#[async_trait::async_trait]
impl Transport for Recorded {
    async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, a, &FetchOptions::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen.lock().unwrap().push(url.to_string());
        let (status, body) = self
            .answers
            .get(url.as_str())
            .cloned()
            .unwrap_or_else(|| panic!("no recorded answer for {url}"));
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            status,
            content_type: "application/json".into(),
            body,
            location: None,
            retry_after_secs: 60,
            validators: Default::default(),
        })
    }
}
struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("93.184.216.34".parse()?, port)])
    }
}
fn egress(recorded: Arc<Recorded>) -> Egress {
    Egress::with_backends(
        EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        },
        Arc::new(PublicDns),
        recorded,
    )
}
fn shop(gtin: &str) -> Subject {
    let uri = "https://shop.example/p/1";
    let mut doc = ResourceMeta::new(uri, "jsonld", 3600);
    doc.set_type("Product");
    doc.attributes.insert("gtin".into(), gtin.into());
    Subject {
        id: "r1".into(),
        uri: uri.into(),
        status: "ok".into(),
        type_name: Some("Product".into()),
        doc: Some(doc),
        keys: vec![],
        first_seen_at: None,
        candidates: vec![],
    }
}
fn links(found: &Found) -> Vec<(&str, &str, Option<&str>)> {
    found
        .links
        .iter()
        .map(|l| (l.site.as_str(), l.uri.as_str(), l.key.as_deref()))
        .collect()
}

#[tokio::test]
async fn a_database_page_is_a_credited_product() {
    let recorded = Recorded::new();
    let egress = egress(recorded.clone());
    let cx = pinakes_core::sources::Context {
        egress: &egress,
        ttl: 3600,
    };
    let source = pinakes_core::sources::by_name("open_food_facts").unwrap();
    let doc = source
        .resolve(
            &cx,
            "https://ch.openfoodfacts.org/product/3017620422003/nutella-ferrero",
        )
        .await
        .unwrap();
    assert_eq!(doc.type_name, "Product");
    assert_eq!(doc.name.as_deref(), Some("Nutella"));
    assert_eq!(doc.attributes["gtin"], "3017620422003");
    assert_eq!(
        doc.attribution.as_deref(),
        Some("© Open Food Facts contributors, ODbL 1.0")
    );
    // A product page on the food database that holds a beauty product: Open Beauty Facts' card.
    let doc = source
        .resolve(&cx, "https://world.openfoodfacts.org/product/80466468")
        .await
        .unwrap();
    assert_eq!(doc.site, "Open Beauty Facts");
    assert_eq!(doc.attributes["gtin"], "0000080466468");
    let doc = source
        .resolve(&cx, "https://world.openfoodfacts.org/product/0602557656527")
        .await
        .unwrap();
    assert_eq!(doc.status, pinakes_core::model::Status::Missing);
}

#[tokio::test]
async fn a_shops_barcode_finds_the_database_that_holds_it() {
    let recorded = Recorded::new();
    let cx = Context {
        egress: egress(recorded.clone()),
        config: Arc::new(Config::default()),
    };
    let found = OpenFoodFacts
        .lookup(&cx, &shop("3017620422003"))
        .await
        .unwrap();
    assert_eq!(
        links(&found),
        [(
            "Open Food Facts",
            "https://world.openfoodfacts.org/product/3017620422003",
            Some("gtin:3017620422003")
        )]
    );
    assert!(found.keys.is_empty(), "the barcode is the card's own key");
    let facts: Vec<(&str, &str)> = found
        .facts
        .iter()
        .map(|f| (f.name.as_str(), f.value.as_str()))
        .collect();
    assert_eq!(
        facts,
        [
            ("product_name", "Nutella"),
            ("brands", "Nutella, Ferrero"),
            ("quantity", "400 g e"),
            ("nutriscore", "e"),
            ("attribution", "© Open Food Facts contributors, ODbL 1.0"),
        ]
    );
    // Open Food Facts names the database of another type of product; the link is that
    // database's own spelling of the barcode.
    let found = OpenFoodFacts
        .lookup(&cx, &shop("0000080466468"))
        .await
        .unwrap();
    assert_eq!(
        links(&found),
        [(
            "Open Beauty Facts",
            "https://world.openbeautyfacts.org/product/80466468",
            Some("gtin:0000080466468")
        )]
    );
    assert!(found.facts.contains(&Fact::new(
        "attribution",
        "© Open Beauty Facts contributors, ODbL 1.0"
    )));
    let found = OpenFoodFacts
        .lookup(&cx, &shop("6111259733749"))
        .await
        .unwrap();
    assert_eq!(links(&found)[0].0, "Open Products Facts");
    assert!(!found.facts.iter().any(|f| f.name == "nutriscore"));
    // A barcode no database holds.
    let found = OpenFoodFacts
        .lookup(&cx, &shop("0602557656527"))
        .await
        .unwrap();
    assert!(found.links.is_empty() && found.facts.is_empty());
    // A database's own page is not asked again.
    let mut page = shop("3017620422003");
    page.uri = "https://world.openfoodfacts.org/product/3017620422003".into();
    assert!(!OpenFoodFacts.applies(&page));
    assert_eq!(recorded.seen.lock().unwrap().len(), 6);
}
