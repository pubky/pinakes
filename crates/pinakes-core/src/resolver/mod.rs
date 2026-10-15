use crate::model::ResourceMeta;
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

pub mod egress;
pub mod geo;
pub mod osm;
pub mod pubky;
pub mod web;

#[async_trait]
pub trait Resolver: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, uri: &str) -> bool;
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta>;
    fn batch_size(&self) -> usize {
        1
    }
    async fn resolve_batch(&self, uris: &[String]) -> Vec<Result<ResourceMeta>> {
        let mut out = Vec::with_capacity(uris.len());
        for uri in uris {
            out.push(self.resolve(uri).await);
        }
        out
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration;
}
/// Scheme modules and host adapters go before the generic web fallback.
pub fn classify(uri: &str) -> Option<&'static str> {
    let url = url::Url::parse(uri).ok()?;
    match url.scheme() {
        "pubky"
            if url.path().starts_with("/pub/")
                && url
                    .path()
                    .split('/')
                    .nth(2)
                    .is_some_and(|app| !app.is_empty() && app != "pubky.app") =>
        {
            Some("pubky")
        }
        "https" if osm::parse_id(uri).is_some() => Some("osm"),
        "http" | "https" => Some("web"),
        "geo" => Some("geo"),
        _ => None,
    }
}
