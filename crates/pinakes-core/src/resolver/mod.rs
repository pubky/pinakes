use crate::model::ResourceMeta;
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

pub mod egress;
pub mod pubky;
pub mod web;

#[async_trait]
pub trait Resolver: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, uri: &str) -> bool;
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta>;
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
        "http" | "https" if !is_osm(&url) => Some("web"),
        // Reserved for the geo and osm modules; never send OSM object pages to the web fetcher.
        "geo" | "http" | "https" => None,
        _ => None,
    }
}
fn is_osm(url: &url::Url) -> bool {
    matches!(
        url.host_str(),
        Some("openstreetmap.org" | "www.openstreetmap.org")
    ) && matches!(
        url.path().split('/').nth(1),
        Some("node" | "way" | "relation")
    )
}
