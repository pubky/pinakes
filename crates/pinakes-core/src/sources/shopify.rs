//! Shopify stores' product pages, improved from the store's own product record
//! (`/products/{handle}.json`), which names each variant's barcode where the page's JSON-LD
//! often does not. A page hook: it claims no links. No key.
use super::{Context, Example, Source};
use crate::{
    kinds::products::{self, Product},
    model::ResourceMeta,
    resolver::egress::{FetchOptions, Fetched},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

pub struct Shopify;

/// The product record of a store's product page (`[/{locale}][/collections/{c}]/products/{handle}`)
/// that names its store (`Shopify.shop = "…myshopify.com"`), and the variant its address picks.
pub fn record(page: &Fetched) -> Option<(String, Option<u64>)> {
    let head = String::from_utf8_lossy(&page.body[..page.body.len().min(512 * 1024)]);
    if !head.contains("Shopify.shop =") && !head.contains("Shopify.shop=") {
        return None;
    }
    let url = &page.url;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let at = segments.iter().position(|s| *s == "products")?;
    let [handle] = &segments[at + 1..] else {
        return None;
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 255
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'%'))
    };
    if !ok(handle) || !segments[..at].iter().all(|s| ok(s)) || at > 3 {
        return None;
    }
    let variant = url
        .query_pairs()
        .find(|(k, _)| k == "variant")
        .and_then(|(_, v)| v.parse().ok());
    let mut record = url.clone();
    record.set_query(None);
    record.set_fragment(None);
    record.set_path(&format!("{}.json", url.path().trim_end_matches('/')));
    Some((record.to_string(), variant))
}

/// `doc` with the barcode, article number, price and maker the product record gives for the
/// page's variant (the one its address picks, or the product's only one); `None` when it picks
/// none.
pub fn card(doc: &ResourceMeta, variant: Option<u64>, body: &[u8]) -> Result<Option<ResourceMeta>> {
    let v: Value = serde_json::from_slice(body)?;
    let product = &v["product"];
    let variants = product["variants"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let chosen = match variant {
        Some(id) => variants.iter().find(|x| x["id"].as_u64() == Some(id)),
        None if variants.len() == 1 => variants.first(),
        None => None,
    };
    let Some(chosen) = chosen else {
        return Ok(None);
    };
    let text = |v: &Value| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let mut doc = doc.clone();
    if !products::is_product(&doc) {
        doc.set_type(products::TYPE);
    }
    if doc.name.is_none() {
        doc.name = text(&product["title"]);
    }
    doc.source = "shopify".into();
    Product {
        gtin: text(&chosen["barcode"]).and_then(|b| crate::barcode::gtin13(&b)),
        brand: text(&product["vendor"]),
        sku: text(&chosen["sku"]),
        price: text(&chosen["price"]).and_then(|p| p.parse().ok()),
        currency: text(&chosen["price_currency"]),
        ..Default::default()
    }
    .replace(&mut doc);
    Ok(Some(doc))
}

#[async_trait]
impl Source for Shopify {
    fn name(&self) -> &'static str {
        "shopify"
    }
    fn reads(&self, _uri: &str) -> bool {
        false
    }
    fn hooks_pages(&self) -> bool {
        true
    }
    fn facts(&self) -> &'static [&'static str] {
        products::FACTS
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.allbirds.com/products/mens-tree-runners?variant=33179624669264",
            card_type: "ProductGroup",
        }]
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "shopify",
                uri: "https://www.allbirds.com/products/mens-tree-runners?variant=33179624669264",
                card_type: "ProductGroup",
                group_with: None,
            }],
            answers: &[
                E2eAnswer {
                    host: "www.allbirds.com",
                    path: "/products/mens-tree-runners.json",
                    query: &[],
                    header: None,
                    content_type: "application/json",
                    file: "allbirds-mens-tree-runners.json",
                },
                E2eAnswer {
                    host: "www.allbirds.com",
                    path: "/products/mens-tree-runners",
                    query: &[],
                    header: None,
                    content_type: "text/html; charset=utf-8",
                    file: "allbirds-mens-tree-runners.html",
                },
            ],
        })
    }
    async fn resolve(&self, _cx: &Context<'_>, _uri: &str) -> Result<ResourceMeta> {
        anyhow::bail!("a page hook reads no links")
    }
    async fn page(
        &self,
        cx: &Context<'_>,
        _uri: &str,
        page: &Fetched,
        doc: &ResourceMeta,
    ) -> Result<Option<ResourceMeta>> {
        let Some((record, variant)) = record(page) else {
            return Ok(None);
        };
        let f = cx
            .fetch(
                &record,
                &FetchOptions {
                    accept: Some("application/json".into()),
                    ..Default::default()
                },
            )
            .await?;
        if f.status != 200 {
            return Ok(None);
        }
        let doc = doc.clone();
        crate::resolver::html::blocking(move || card(&doc, variant, &f.body)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "https://www.allbirds.com/products/mens-tree-runners?variant=33179624669264";
    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("shopify", file)
    }
    fn fetched(url: &str, body: Vec<u8>) -> Fetched {
        Fetched::answer(url.parse().unwrap(), 200, "text/html".into(), body)
    }

    #[test]
    fn a_product_page_names_its_record_and_variant() {
        let page = fetched(PAGE, read("allbirds-mens-tree-runners.html"));
        assert_eq!(
            record(&page),
            Some((
                "https://www.allbirds.com/products/mens-tree-runners.json".into(),
                Some(33179624669264)
            ))
        );
        let localized = fetched(
            "https://shop.example/de-ch/collections/new/products/runner",
            b"<script>Shopify.shop = \"x.myshopify.com\";</script>".to_vec(),
        );
        assert_eq!(
            record(&localized).unwrap().0,
            "https://shop.example/de-ch/collections/new/products/runner.json"
        );
        for (url, body) in [
            (
                "https://shop.example/products/runner",
                "<title>Not a store</title>",
            ),
            (
                "https://shop.example/products",
                "<script>Shopify.shop = \"x\";</script>",
            ),
            (
                "https://shop.example/products/a/b",
                "<script>Shopify.shop = \"x\";</script>",
            ),
        ] {
            assert_eq!(
                record(&fetched(url, body.as_bytes().to_vec())),
                None,
                "{url}"
            );
        }
    }

    #[test]
    fn the_record_gives_the_variants_barcode() {
        let page = fetched(PAGE, read("allbirds-mens-tree-runners.html"));
        let doc = crate::resolver::web::extract(PAGE, &page, 3600)
            .unwrap()
            .doc;
        assert_eq!(doc.type_name, "ProductGroup");
        assert!(!doc.attributes.contains_key("gtin"), "the page names none");
        let body = read("allbirds-mens-tree-runners.json");
        let improved = card(&doc, Some(33179624669264), &body).unwrap().unwrap();
        assert_eq!(improved.source, "shopify");
        let a = &improved.attributes;
        assert_eq!(a["gtin"], "0843416184854");
        assert_eq!(a["brand"], "Allbirds");
        assert_eq!(a["sku"], "TR3MJBW080");
        assert_eq!(a["price"], "100");
        assert_eq!(a["price_currency"], "USD");
        assert!(a.contains_key("price_at"));
        // No variant picked of several: nothing to add.
        assert!(card(&doc, None, &body).unwrap().is_none());
        assert!(card(&doc, Some(1), &body).unwrap().is_none());
        assert!(improved.bounded(crate::sources::attributes()).is_ok());
    }

    #[test]
    fn a_variant_without_a_barcode_keeps_none_of_another() {
        // The page's card names its default variant (1); variant 2's record has an empty, then
        // an invalid barcode: the card takes variant 2's article number and price, no barcode.
        let mut doc = ResourceMeta::new("https://shop.example/products/runner", "jsonld", 3600);
        doc.set_type(products::TYPE);
        Product {
            gtin: Some("0602557656527".into()),
            sku: Some("R-1".into()),
            price: Some(10.0),
            currency: Some("USD".into()),
            ..Default::default()
        }
        .apply(&mut doc);
        for barcode in ["", "4006381333932"] {
            let body = serde_json::json!({"product": {"title": "Runner", "vendor": "Birds",
                "variants": [
                    {"id": 1, "barcode": "0602557656527", "sku": "R-1", "price": "10.00"},
                    {"id": 2, "barcode": barcode, "sku": "R-2", "price": "20.00",
                        "price_currency": "USD"}]}});
            let improved = card(&doc, Some(2), &serde_json::to_vec(&body).unwrap())
                .unwrap()
                .unwrap();
            let a = &improved.attributes;
            assert!(!a.contains_key("gtin"), "{barcode:?}: {a:?}");
            assert_eq!((&*a["sku"], &*a["price"]), ("R-2", "20"), "{barcode:?}");
        }
    }
}
