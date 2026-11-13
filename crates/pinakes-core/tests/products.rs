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
mod common;

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
    let fetched = common::fetched(
        ALTERNATE.parse().unwrap(),
        200,
        "text/html".into(),
        recorded("alternate-100222861.html.gz"),
    );
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
        let fetched = common::fetched(
            page.parse().unwrap(),
            200,
            "text/html".into(),
            body.into_bytes(),
        );
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

    // Products of the page's own that name no page, in the head: one gives its barcode; two (an
    // array, a graph, or two scripts) give none, but the card keeps the first one's offer.
    let unnamed = |gtin: &str, price: &str| {
        format!(
            r#"{{"@type": "Product", "name": "Spread {gtin}", "gtin13": "{gtin}", "sku": "S-{gtin}", "mpn": "M-{gtin}", "offers": {{"price": "{price}", "priceCurrency": "CHF"}}}}"#
        )
    };
    let (a, b) = (
        unnamed("3017620422003", "4.95"),
        unnamed("4006381333931", "3.50"),
    );
    let head = |scripts: &[String]| {
        format!(
            "<html><head><title>Spreads</title>{}</head><body></body></html>",
            scripts
                .iter()
                .map(|s| format!(r#"<script type="application/ld+json">{s}</script>"#))
                .collect::<String>()
        )
    };
    let page = "https://shop.example/c/spreads";
    let doc = extract(page, head(std::slice::from_ref(&a)));
    assert_eq!(doc.attributes["gtin"], "3017620422003");
    assert_eq!(doc.attributes["sku"], "S-3017620422003");
    assert_eq!(doc.attributes["mpn"], "M-3017620422003");
    for scripts in [
        vec![format!("[{a}, {b}]")],
        vec![format!(r#"{{"@graph": [{a}, {b}]}}"#)],
        vec![a.clone(), b.clone()],
    ] {
        let doc = extract(page, head(&scripts));
        assert_eq!(doc.type_name, "Product", "{scripts:?}");
        for key in ["gtin", "sku", "mpn"] {
            assert!(!doc.attributes.contains_key(key), "{key} {scripts:?}");
        }
        assert_eq!(doc.attributes["price"], "4.95", "{scripts:?}");
    }
    // One of the page's own beside another product's (named elsewhere): the own one's.
    let other = unnamed("4006381333931", "3.50").replacen(
        r#""@type": "Product","#,
        r#""@type": "Product", "url": "https://shop.example/p/pen","#,
        1,
    );
    let doc = extract(page, head(&[format!("[{a}, {other}]")]));
    assert_eq!(doc.attributes["gtin"], "3017620422003");
    // Listed after it, in one array or as an earlier script (bare, or wrapped in a graph, an
    // array or a page's main entity): still the own one's.
    for scripts in [
        vec![format!("[{other}, {a}]")],
        vec![other.clone(), a.clone()],
        vec![format!(r#"{{"@graph": [{other}]}}"#), a.clone()],
        vec![format!("[{other}]"), a.clone()],
        vec![
            format!(r#"{{"@type": "WebPage", "mainEntity": {other}}}"#),
            a.clone(),
        ],
        // Beside the page's metadata, which names the page.
        vec![
            format!(r##"{{"@graph": [{{"@type": "WebPage", "@id": "#webpage"}}, {other}]}}"##),
            a.clone(),
        ],
        vec![
            format!(
                r##"{{"@graph": [{{"@type": "BreadcrumbList", "@id": "{page}#breadcrumb", "itemListElement": []}}, {other}]}}"##
            ),
            a.clone(),
        ],
        vec![
            format!(
                r##"{{"@graph": [{{"@type": "WebPage", "@id": "{page}#webpage", "url": "{page}", "breadcrumb": {{"@id": "{page}#breadcrumb"}}}}, {{"@type": "BreadcrumbList", "@id": "{page}#breadcrumb"}}, {other}]}}"##
            ),
            a.clone(),
        ],
    ] {
        let doc = extract(page, head(&scripts));
        assert_eq!(
            doc.name.as_deref(),
            Some("Spread 3017620422003"),
            "{scripts:?}"
        );
        assert_eq!(doc.attributes["gtin"], "3017620422003", "{scripts:?}");
        assert_eq!(doc.attributes["sku"], "S-3017620422003", "{scripts:?}");
        assert_eq!(doc.attributes["mpn"], "M-3017620422003", "{scripts:?}");
        assert_eq!(doc.attributes["price"], "4.95", "{scripts:?}");
    }
}

/// The page's card from `body`, served at `page`.
fn extracted(page: &str, body: &str) -> ResourceMeta {
    let fetched = common::fetched(
        page.parse().unwrap(),
        200,
        "text/html".into(),
        body.as_bytes().to_vec(),
    );
    web::extract(page, &fetched, 3600).unwrap().doc
}

fn scripts(head: &[&str], body: &[&str]) -> String {
    let tags = |all: &[&str]| {
        all.iter()
            .map(|s| format!(r#"<script type="application/ld+json">{s}</script>"#))
            .collect::<String>()
    };
    format!(
        "<html><head><title>Shop</title>{}</head><body>{}</body></html>",
        tags(head),
        tags(body)
    )
}

#[test]
fn one_product_described_twice_keeps_its_barcode() {
    // A shop's theme and a plugin both describe the page's product: one product, so its
    // barcode stays. Two products of other `@id`s (sharing the page's `url` or not), or one
    // naming none beside it, are two.
    let page = "https://shop.example/p/nutella";
    let product = |id: &str, url: Option<&str>, gtin: &str| {
        let url = url
            .map(|u| format!(r#""url": "{u}", "#))
            .unwrap_or_default();
        format!(
            r#"{{"@type": "Product", "@id": "{id}", {url}"name": "Nutella", "gtin13": "{gtin}", "sku": "S-{gtin}"}}"#
        )
    };
    let nutella = product(&format!("{page}#product"), Some(page), "3017620422003");
    let relative = product("#product", None, "3017620422003");
    // The same `@id`, one description with the page's `url` and one without; under a head
    // declaring a collection too, as one description names the page by its `url`.
    let with_url = product("#product", Some(page), "3017620422003");
    let collection = r#"{"@type": "CollectionPage", "name": "Spreads"}"#;
    for (head, body) in [
        (vec![nutella.as_str(), nutella.as_str()], vec![]),
        (vec![nutella.as_str()], vec![nutella.as_str()]),
        (vec![relative.as_str()], vec![relative.as_str()]),
        (vec![with_url.as_str(), relative.as_str()], vec![]),
        (vec![relative.as_str(), with_url.as_str()], vec![]),
        (vec![collection], vec![relative.as_str(), with_url.as_str()]),
        (vec![collection], vec![with_url.as_str(), relative.as_str()]),
    ] {
        let doc = extracted(page, &scripts(&head, &body));
        assert_eq!(doc.attributes["gtin"], "3017620422003", "{head:?} {body:?}");
        assert_eq!(
            doc.attributes["sku"], "S-3017620422003",
            "{head:?} {body:?}"
        );
    }
    let (a, b) = (
        product("#a", None, "3017620422003"),
        product("#b", None, "4006381333931"),
    );
    let unnamed = r#"{"@type": "Product", "name": "Pen", "gtin13": "4006381333931"}"#;
    // Two `@id`s sharing the page's `url` are two products.
    let (a_url, b_url) = (
        product("#a", Some(page), "3017620422003"),
        product("#b", Some(page), "4006381333931"),
    );
    for (head, body) in [
        (vec![a_url.as_str(), b_url.as_str()], vec![]),
        (vec![], vec![a_url.as_str(), b_url.as_str()]),
        (vec![a.as_str(), b.as_str()], vec![]),
        (vec![], vec![a.as_str(), b.as_str()]),
        (vec![nutella.as_str(), unnamed], vec![]),
    ] {
        let doc = extracted(page, &scripts(&head, &body));
        for key in ["gtin", "sku"] {
            assert!(!doc.attributes.contains_key(key), "{key} {head:?} {body:?}");
        }
    }
}

#[test]
fn a_product_a_category_page_lists_by_a_fragment_is_not_its_own() {
    // The head says the page is a collection (or about another entity); a body product named
    // only by a fragment of the page is one it lists. A product page's `#product`, declared as
    // its main entity or with nothing declared, is its own.
    let named = |named: &str| {
        format!(
            r##"{{"@type": "Product", {named}, "name": "Nutella", "gtin13": "3017620422003", "sku": "N-400", "mpn": "NUT-400", "offers": {{"price": "4.95", "priceCurrency": "CHF"}}}}"##
        )
    };
    let nutella = |id: &str| named(&format!(r#""@id": "{id}""#));
    let category = "https://shop.example/c/spreads";
    let collection =
        format!(r#"{{"@type": "CollectionPage", "url": "{category}", "name": "Spreads"}}"#);
    for head in [
        collection.clone(),
        r#"{"@graph": [{"@type": "CollectionPage", "name": "Spreads"}]}"#.to_owned(),
        r##"{"@type": "WebPage", "mainEntity": {"@id": "#list"}}"##.to_owned(),
    ] {
        // Named by its `@id`, or by a fragment `url`, relative or absolute.
        for body in [
            nutella("#nutella"),
            named(r##""url": "#nutella""##),
            named(&format!(r##""url": "{category}#nutella""##)),
            named(&format!(
                r##""url": "{category}#nutella", "@id": "#nutella""##
            )),
        ] {
            let doc = extracted(category, &scripts(&[&head], &[&body]));
            for key in ["gtin", "sku", "mpn"] {
                assert!(!doc.attributes.contains_key(key), "{key} {head} {body}");
            }
            assert_eq!(doc.attributes["price"], "4.95", "{head} {body}");
        }
    }
    // Named by the page's own `url`, whatever its `@id`: its own.
    let doc = extracted(
        category,
        &scripts(
            &[&collection],
            &[&named(&format!(
                r##""url": "{category}", "@id": "#nutella""##
            ))],
        ),
    );
    assert_eq!(doc.attributes["gtin"], "3017620422003");
    assert_eq!(doc.attributes["mpn"], "NUT-400");
    let page = "https://shop.example/p/nutella";
    for head in [
        vec![r##"{"@type": "ItemPage", "mainEntity": {"@id": "#product"}}"##.to_owned()],
        vec![format!(
            r##"{{"@type": "WebPage", "url": "{page}", "mainEntity": {{"@id": "{page}#product"}}}}"##
        )],
        vec![r#"{"@type": "WebPage", "name": "Nutella"}"#.to_owned()],
        vec![],
    ] {
        let head: Vec<&str> = head.iter().map(String::as_str).collect();
        let doc = extracted(page, &scripts(&head, &[&nutella("#product")]));
        assert_eq!(doc.attributes["gtin"], "3017620422003", "{head:?}");
        assert_eq!(doc.attributes["sku"], "N-400", "{head:?}");
        assert_eq!(doc.attributes["mpn"], "NUT-400", "{head:?}");
    }
}

#[test]
fn a_json_document_is_guarded_as_a_pages_head_is() {
    // A page served as JSON-LD (`application/ld+json`) is all head: a product it names only by a
    // fragment while it declares itself a collection is one it lists, as in HTML.
    let served = |page: &str, body: String| {
        let fetched = common::fetched(
            page.parse().unwrap(),
            200,
            "application/ld+json".into(),
            body.into_bytes(),
        );
        web::extract(page, &fetched, 3600).unwrap().doc
    };
    let product = |named: &str| {
        format!(
            r##"{{"@type": "Product", {named}, "name": "Nutella", "gtin13": "3017620422003", "sku": "N-400", "mpn": "NUT-400", "offers": {{"price": "4.95", "priceCurrency": "CHF"}}}}"##
        )
    };
    let category = "https://shop.example/c/spreads";
    for kind in ["CollectionPage", "SearchResultsPage"] {
        let listing = format!(r#"{{"@type": "{kind}", "url": "{category}", "name": "Spreads"}}"#);
        for named in [
            r##""url": "#nutella""##.to_owned(),
            format!(r##""url": "{category}#nutella""##),
            r##""@id": "#nutella""##.to_owned(),
        ] {
            let item = product(&named);
            for document in [
                format!(r#"{{"@graph": [{listing}, {item}]}}"#),
                format!("[{listing}, {item}]"),
            ] {
                let doc = served(category, document.clone());
                assert_eq!(doc.type_name, "Product", "{document}");
                for key in ["gtin", "sku", "mpn"] {
                    assert!(!doc.attributes.contains_key(key), "{key} {document}");
                }
                assert_eq!(doc.attributes["price"], "4.95", "{document}");
            }
        }
        // A product naming the page by its own `url` is its own, whatever its `@id`.
        let own = product(&format!(r##""url": "{category}", "@id": "#nutella""##));
        let doc = served(category, format!(r#"{{"@graph": [{listing}, {own}]}}"#));
        assert_eq!(doc.attributes["mpn"], "NUT-400", "{kind}");
    }
    // A product page's `#product`, declared its main entity or with nothing declared: its own.
    let page = "https://shop.example/p/nutella";
    let item = product(r##""@id": "#product""##);
    for document in [
        format!(
            r##"{{"@graph": [{{"@type": "ItemPage", "mainEntity": {{"@id": "#product"}}}}, {item}]}}"##
        ),
        item.clone(),
    ] {
        let doc = served(page, document.clone());
        assert_eq!(doc.attributes["gtin"], "3017620422003", "{document}");
        assert_eq!(doc.attributes["sku"], "N-400", "{document}");
        assert_eq!(doc.attributes["mpn"], "NUT-400", "{document}");
    }
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

/// A link shortener answering with a redirect to its Amazon address; asked for Amazon itself,
/// it panics.
struct ToAmazon(&'static str);
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
            location: Some(self.0.into()),
            ..common::fetched(url.clone(), 302, "text/html".into(), vec![])
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
    // Every shop of Amazon's, and its short links.
    for uri in [
        "https://www.amazon.ie/dp/B08N5WRWNW",
        "https://www.amazon.co.za/dp/B08N5WRWNW",
        "https://www.amazon.cn/dp/B08N5WRWNW",
        "https://AMAZON.IE./dp/B08N5WRWNW",
        "https://amzn.asia/d/3xyzabc",
    ] {
        resolver.resolve(uri).await.unwrap();
    }
    // Nor where another link redirects to it: the egress refuses the redirect's target.
    for target in [
        "https://www.amazon.de/dp/B08N5WRWNW",
        "https://www.amazon.ie/dp/B08N5WRWNW",
    ] {
        let resolver = WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress: Egress::with_backends(
                EgressConfig::default(),
                Arc::new(AnyDns),
                Arc::new(ToAmazon(target)),
            ),
            config: Default::default(),
            events: None,
        };
        let error = resolver
            .resolve("https://link.example/item")
            .await
            .unwrap_err();
        assert_eq!(
            pinakes_core::resolver::error_class(&error),
            "policy",
            "{target} {error:#}"
        );
        assert!(format!("{error:#}").contains("Amazon is never fetched"));
    }
}
