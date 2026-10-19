use crate::model::ResourceMeta;
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

pub mod cards;
pub mod egress;
pub mod geo;
pub mod interstitial;
pub mod osm;
pub mod pkarr_relays;
pub mod pubky;
pub mod web;

#[async_trait]
pub trait Resolver: Send + Sync {
    fn manifest(&self) -> crate::adapter::Manifest {
        crate::adapter::manifest(self.name())
    }
    fn manifest_for(&self, _uri: &str) -> crate::adapter::Manifest {
        self.manifest()
    }
    fn manifests(&self) -> Vec<crate::adapter::Manifest> {
        vec![self.manifest()]
    }
    fn canary_allowed_for(&self, manifest: &crate::adapter::Manifest) -> bool {
        manifest.enabled && self.canary_allowed()
    }
    async fn probe_manifest(
        &self,
        uri: &str,
        _manifest: &crate::adapter::Manifest,
    ) -> Result<ResourceMeta> {
        self.probe(uri).await
    }
    async fn probe(&self, uri: &str) -> Result<ResourceMeta> {
        self.resolve(uri).await
    }
    fn name(&self) -> &'static str;
    fn queue_sources(&self) -> Vec<&'static str> {
        vec![self.name()]
    }
    fn matches(&self, uri: &str) -> bool;
    fn canary_allowed(&self) -> bool {
        true
    }
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
/// Every route `classify` returns besides the card kinds (`cards::KINDS`). `classify` returns
/// only these constants, so a new route is added here.
pub const ROUTES: [&str; 4] = ["web", "pubky", "geo", "osm"];
const WEB: &str = ROUTES[0];
const PUBKY: &str = ROUTES[1];
const GEO: &str = ROUTES[2];
const OSM: &str = ROUTES[3];

/// The app of a `pubky://` object (`/pub/{app}/…`), its path decoded as a homeserver decodes it.
/// `None` outside `/pub/`, for an empty app, and for any segment that decodes to a separator or
/// a dot segment: the homeserver would read another path than the one classified.
pub(crate) fn pubky_app(url: &url::Url) -> Option<String> {
    let segments: Vec<String> = url
        .path()
        .split('/')
        .skip(1)
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .ok()
                .map(|s| s.into_owned())
        })
        .collect::<Option<_>>()?;
    if segments
        .iter()
        .any(|s| s.contains(['/', '\\']) || s == "." || s == "..")
    {
        return None;
    }
    match segments.as_slice() {
        [public, app, ..] if public == "pub" && !app.is_empty() => Some(app.clone()),
        _ => None,
    }
}
/// Scheme modules and host adapters go before the generic web fallback.
pub fn classify(uri: &str) -> Option<&'static str> {
    let route = classify_route(uri);
    debug_assert!(route.is_none_or(|r| ROUTES.contains(&r) || cards::KINDS.contains(&r)));
    route
}
fn classify_route(uri: &str) -> Option<&'static str> {
    let url = url::Url::parse(uri).ok()?;
    if let Some(kind) = cards::classify(uri) {
        return Some(kind);
    }
    match url.scheme() {
        "pubky" if pubky_app(&url).is_some_and(|app| app != "pubky.app") => Some(PUBKY),
        "http" | "https" if osm::parse_id(uri).is_some() => Some(OSM),
        // Map links that name a point; never identity, so they never group.
        "http" | "https" if geo::location_link(uri).is_some() => Some(GEO),
        "http" | "https" => Some(WEB),
        "geo" => Some(GEO),
        _ => None,
    }
}

/// Persisted routing hints are configuration-independent; a disabled card kind uses web workers.
pub fn classify_config(uri: &str, config: &crate::config::Config) -> Option<&'static str> {
    match classify(uri) {
        Some(kind) if cards::KINDS.contains(&kind) && !config.cards.resolver(kind).enabled => {
            if url::Url::parse(uri).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
                Some("web")
            } else {
                None
            }
        }
        other => other,
    }
}

#[derive(Debug)]
pub struct NotModified;
impl std::fmt::Display for NotModified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream not modified")
    }
}
impl std::error::Error for NotModified {}

#[derive(Clone, Debug)]
pub struct RetryLater {
    pub class: &'static str,
    pub seconds: u64,
}
impl std::fmt::Display for RetryLater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; retry later", self.class)
    }
}
impl std::error::Error for RetryLater {}

#[derive(Clone, Copy, Debug)]
pub struct ErrorClass(pub &'static str);
impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ErrorClass {}
pub fn error_class(error: &anyhow::Error) -> &'static str {
    if error.is::<interstitial::Blocked>() {
        return "wall";
    }
    if let Some(class) = error.downcast_ref::<ErrorClass>() {
        return class.0;
    }
    if let Some(retry) = error.downcast_ref::<RetryLater>() {
        return retry.class;
    }
    if error.is::<tokio::time::error::Elapsed>() {
        return "timeout";
    }
    for cause in error.chain() {
        if let Some(request) = cause.downcast_ref::<reqwest::Error>() {
            if request.is_timeout() {
                return "timeout";
            }
            if request.is_connect() {
                return "connect";
            }
            if let Some(status) = request.status() {
                return if status.is_server_error() {
                    "http_5xx"
                } else {
                    "http_4xx"
                };
            }
        }
    }
    "parse"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_reachable() {
        // Every route is reachable, so ROUTES lists nothing classify cannot return.
        let samples = [
            "https://example.org/page",
            "https://www.openstreetmap.org/node/1",
            "https://osm.org/go/0EEQjE--",
            "geo:46.0,8.9",
            "pubky://o1gg96ewuojmopcjbz8895478wdtxtzzber7aezq6ror5a91j7dy/pub/example.app/x",
        ];
        let routed: std::collections::BTreeSet<_> =
            samples.iter().filter_map(|uri| classify(uri)).collect();
        assert_eq!(routed, ROUTES.into_iter().collect());
    }
}
