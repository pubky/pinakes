//! Products: a recorded shop page becomes a Product card with its barcode, brand and dated
//! offer; an Amazon link gets its card from the link alone, never fetched.
use pinakes_core::{
    config::EgressConfig,
    kinds,
    model::ResourceMeta,
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        web::{self, WebResolver},
        Resolver,
    },
};
use std::{io::Read, net::SocketAddr, sync::Arc};

const ALTERNATE: &str =
    "https://www.alternate.ch/Corsair/SABRE-v3-PRO-PureVector-Ultralight-Ergo-Gaming-Maus/html/product/100222861";

fn recorded(file: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/web/products/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut out = vec![];
    flate2::read::GzDecoder::new(std::fs::read(&path).unwrap().as_slice())
        .read_to_end(&mut out)
        .unwrap();
    out
}

#[test]
fn a_shop_page_names_its_barcode_and_dated_offer() {
    let fetched = Fetched {
        permanent_redirect: false,
        truncated: false,
        url: ALTERNATE.parse().unwrap(),
        status: 200,
        content_type: "text/html".into(),
        body: recorded("alternate-100222861.html.gz"),
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    };
    let doc: ResourceMeta = web::extract(ALTERNATE, &fetched, 3600).unwrap().doc;
    assert_eq!(doc.type_name, "Product");
    assert_eq!(
        doc.name.as_deref(),
        Some("SABRE v3 PRO PureVector Ultralight Ergo, Gaming-Maus")
    );
    let a = &doc.attributes;
    // The shop states the GTIN-13 under `gtin8`.
    assert_eq!(a["gtin"], "0840440422266");
    assert_eq!(a["brand"], "Corsair");
    assert_eq!(a["sku"], "100222861");
    assert_eq!(a["price"], "152.90");
    assert_eq!(a["price_currency"], "CHF");
    assert_eq!(a["availability"], "pre_order");
    assert_eq!(a["seller"], "ALTERNATE GmbH");
    assert!(a["price_at"]
        .parse::<chrono::DateTime<chrono::Utc>>()
        .is_ok());
    let bounded = doc.clone().bounded(web::ATTRIBUTES).unwrap();
    assert_eq!(bounded.attributes, doc.attributes, "facts kept");
    let facts = kinds::facts(None, &doc, chrono::Utc::now()).unwrap();
    assert_eq!(facts["gtin"], "0840440422266");
}

#[test]
fn only_the_pages_own_product_gives_its_barcode() {
    // A category page's body script lists another product (its own page elsewhere): the page
    // takes none of its identity, which would join it to that product's group.
    let listed = |url: &str| {
        format!(
            r#"<html><head><title>Spreads</title></head><body><script type="application/ld+json">{{"@type": "Product", "name": "Nutella 400 g", "url": "{url}", "gtin13": "3017620422003", "sku": "N-400", "offers": {{"price": "4.95", "priceCurrency": "CHF"}}}}</script></body></html>"#
        )
    };
    let extract = |page: &str, body: String| {
        let fetched = Fetched {
            permanent_redirect: false,
            truncated: false,
            url: page.parse().unwrap(),
            status: 200,
            content_type: "text/html".into(),
            body: body.into_bytes(),
            location: None,
            retry_after_secs: 60,
            validators: Default::default(),
        };
        web::extract(page, &fetched, 3600).unwrap().doc
    };
    let doc = extract(
        "https://shop.example/c/spreads",
        listed("https://shop.example/p/nutella"),
    );
    assert!(!doc.attributes.contains_key("gtin"), "{:?}", doc.attributes);
    assert!(!doc.attributes.contains_key("sku"));
    // The same script naming the page is the page's own product.
    let doc = extract(
        "https://shop.example/p/nutella",
        listed("https://shop.example/p/nutella"),
    );
    assert_eq!(doc.attributes["gtin"], "3017620422003");
    assert_eq!(doc.attributes["sku"], "N-400");
}

struct AnyDns;
#[async_trait::async_trait]
impl Dns for AnyDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("93.184.216.34".parse()?, port)])
    }
}
/// A transport that is never to be asked.
struct Never;
#[async_trait::async_trait]
impl Transport for Never {
    async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, a, &FetchOptions::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        panic!("fetched {url}")
    }
}

/// A link shortener answering with a redirect to Amazon; asked for Amazon itself, it panics.
struct ToAmazon;
#[async_trait::async_trait]
impl Transport for ToAmazon {
    async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, a, &FetchOptions::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        assert_eq!(url.host_str(), Some("link.example"), "fetched {url}");
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            status: 302,
            content_type: "text/html".into(),
            body: vec![],
            location: Some("https://www.amazon.de/dp/B08N5WRWNW".into()),
            retry_after_secs: 60,
            validators: Default::default(),
        })
    }
}

#[tokio::test]
async fn amazon_is_never_fetched() {
    let resolver = WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        egress: Egress::with_backends(EgressConfig::default(), Arc::new(AnyDns), Arc::new(Never)),
        config: Default::default(),
        events: None,
    };
    let doc = resolver
        .resolve("https://www.amazon.de/Matilda-Roald-Dahl/dp/0140328726?tag=x")
        .await
        .unwrap();
    assert_eq!(doc.type_name, "Product");
    assert_eq!(doc.site, "Amazon");
    assert_eq!(doc.attributes["asin"], "0140328726");
    let doc = resolver.resolve("https://amzn.to/3xyzabc").await.unwrap();
    assert_eq!(doc.type_name, "WebPage");
    // Nor where another link redirects to it: the egress refuses the redirect's target.
    let resolver = WebResolver {
        egress: Egress::with_backends(
            EgressConfig::default(),
            Arc::new(AnyDns),
            Arc::new(ToAmazon),
        ),
        ..resolver
    };
    let error = resolver
        .resolve("https://link.example/item")
        .await
        .unwrap_err();
    assert_eq!(
        pinakes_core::resolver::error_class(&error),
        "policy",
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("Amazon is never fetched"));
}
