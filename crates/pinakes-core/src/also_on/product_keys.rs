//! The `product_keys` also-on source: a product's own barcode as its join key.
//!
//! A product card states its barcode (`gtin`, from its page's JSON-LD, its shop's own product
//! record or a product database's record: `kinds::products`) for the one product it is; that
//! barcode is the key `gtin:`, so the same product on several shops is one group (search's
//! `key=gtin:` is the API's own namespace). No I/O.
use crate::also_on::{Context, Found, Key, Source, Subject};
use anyhow::Result;
use async_trait::async_trait;

pub struct ProductKeys;
#[async_trait]
impl Source for ProductKeys {
    fn id(&self) -> &'static str {
        "product_keys"
    }
    fn applies(&self, subject: &Subject) -> bool {
        own_gtin(subject).is_some()
    }
    fn single_namespaces(&self) -> &'static [&'static str] {
        &["gtin"]
    }
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        &[("gtin", "3017620422003")]
    }
    async fn lookup(&self, _cx: &Context, subject: &Subject) -> Result<Found> {
        let mut found = Found::default();
        let Some(code) = own_gtin(subject) else {
            return Ok(found);
        };
        // The record that stated it: an official source's, else the page's.
        let via = subject
            .doc
            .as_ref()
            .and_then(|d| crate::sources::by_name(&d.source))
            .map_or("page", |s| s.name());
        found.keys.push(Key {
            key: format!("gtin:{code}"),
            via: via.into(),
        });
        Ok(found)
    }
}

/// A product card's own barcode, as GTIN-13.
fn own_gtin(subject: &Subject) -> Option<String> {
    let doc = subject.doc.as_ref()?;
    if !crate::kinds::products::is_product(doc) {
        return None;
    }
    crate::barcode::gtin13(doc.attributes.get("gtin")?)
}
