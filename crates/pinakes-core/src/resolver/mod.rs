use crate::model::ResourceMeta;
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

pub mod cards;
pub mod egress;
pub mod geo;
pub mod interstitial;
pub mod osm;
pub mod pubky;
mod pubky_policy;
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
/// only these constants, so a new route is added here and `EXTRACTOR_VERSIONS` must cover it.
pub const ROUTES: [&str; 6] = ["web", "pubky", "geo", "osm", "event", "event_platform"];
const WEB: &str = ROUTES[0];
const PUBKY: &str = ROUTES[1];
const GEO: &str = ROUTES[2];
const OSM: &str = ROUTES[3];
const EVENT: &str = ROUTES[4];
const EVENT_PLATFORM: &str = ROUTES[5];

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
        "pubky"
            if url.path().starts_with("/pub/")
                && url
                    .path()
                    .split('/')
                    .nth(2)
                    .is_some_and(|app| !app.is_empty() && app != "pubky.app") =>
        {
            Some(PUBKY)
        }
        "http" | "https"
            if url.host_str() == Some("api.nostr.wine")
                && crate::event::adapters::detect(uri).is_none() =>
        {
            None
        }
        "http" | "https" if osm::parse_id(uri).is_some() => Some(OSM),
        // Map links that name a point; never identity, so they never group.
        "http" | "https" if geo::location_link(uri).is_some() => Some(GEO),
        "nostr" => Some(EVENT_PLATFORM),
        "http" | "https" | "webcal" | "webcals"
            if crate::event::adapters::detect(uri).is_some_and(|d| d.id != "ics") =>
        {
            Some(EVENT_PLATFORM)
        }
        "webcal" | "webcals" => Some(EVENT),
        "http" | "https" if crate::event::service::is_feed(uri) => Some(EVENT),
        "http" | "https" => Some(WEB),
        "geo" => Some(GEO),
        _ => None,
    }
}

/// The extractor version of each resolver kind. Bump a resolver's number in the same commit as
/// any change to what it extracts or how it builds group evidence: at startup, `ok` documents
/// saved by an older version of the worker kind that extracted them (`extracted_by`; web workers
/// also serve the event route with calendars off, the event_platform route with adapters off,
/// and disabled card kinds) are re-fetched
/// (`queue::reextract_outdated`), because a re-plan reuses the stored document and only a
/// re-fetch re-runs the parser. Every `classify` route and card kind needs an entry: a missing
/// one compares 0 < 0 and never re-extracts.
/// `event` and `event_platform` run the same ICS expander, so they share one number (the
/// calendar expansion signature reads `event`); split out an expander version if they diverge.
pub const EXTRACTOR_VERSIONS: &[(&str, i32)] = &[
    ("web", 1),
    ("pubky", 1),
    ("geo", 1),
    ("osm", 1),
    // 2: RFC 5545 expansion of UTC-DTSTART series and DST-ambiguous first instances (round 15).
    // 3: UNTIL as an instant, TZID on UTC values, floating RECURRENCE-ID per series (round 16).
    // 4: floating values in UTC series, embedded zones in the repeated hour (round 17).
    // 5: an unconvertible embedded zone expands its series in UTC (round 18).
    // 6: four-digit years only (round 19).
    // 7: bounded NIP-52 dates and integer timestamps, leading whitespace, `YYYYMMDDZ` (round 20).
    // 8: trimmed JSON-LD date-only ends, skipped failed NIP-52 calendar members (round 21).
    // 9: storable NIP-52 all-day dates, start-less calendar members skipped (round 22).
    // 10: NIP-52 calendars stop waiting for a member pending past the configured wait (round 23).
    // 11: the NIP-52 member wait per calendar and member, bounded per calendar; leap seconds
    // normalised before the year check (round 24).
    // 12: event documents keep the page card's identity claims (round 25).
    ("event", 12),
    ("event_platform", 12),
    ("media", 1),
    ("isbn", 1),
    ("doi", 1),
    // 2: NIP-52 events parse their dates like the event_platform route (round 20).
    // 3: NIP-52 all-day dates must be storable instants (round 22).
    ("nostr", 3),
    ("activitypub", 1),
];
/// The extractor kind recorded for `doc` when the `worker` kind produced it. The web worker
/// builds calendars and event pages with the event service, so those are the `event`
/// extractor's output and follow its version; everything else is the worker's own.
pub fn extracted_by<'a>(worker: &'a str, doc: &crate::model::ResourceMeta) -> &'a str {
    if worker == "web" && (doc.container.is_some() || doc.type_path.iter().any(|t| t == "Event")) {
        "event"
    } else {
        worker
    }
}
/// `EXTRACTOR_VERSIONS` as a JSON object for SQL (`versions->>resolver`).
pub fn extractor_versions() -> serde_json::Value {
    EXTRACTOR_VERSIONS
        .iter()
        .map(|(name, v)| ((*name).to_owned(), serde_json::json!(v)))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// Persisted routing hints are configuration-independent; disabled calendars use web workers.
pub fn classify_config(uri: &str, config: &crate::config::Config) -> Option<&'static str> {
    match classify(uri) {
        Some(kind) if cards::KINDS.contains(&kind) && !config.cards.resolver(kind).enabled => {
            if url::Url::parse(uri).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
                Some("web")
            } else {
                None
            }
        }
        Some("event") if !config.event.enabled || !config.event.calendars => Some("web"),
        Some("event_platform")
            if crate::event::adapters::detect(uri)
                .is_some_and(|d| !crate::event::adapters::manifest(d, &config.event).enabled)
                && url::Url::parse(uri).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "https" | "webcal" | "webcals")
                }) =>
        {
            Some("web")
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
        return "blocked";
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
    fn every_route_and_card_kind_has_an_extractor_version() {
        let versions = extractor_versions();
        for kind in ROUTES.iter().chain(cards::KINDS.iter()) {
            assert!(
                versions[kind].as_i64().is_some_and(|v| v > 0),
                "{kind} has no extractor version"
            );
        }
        assert_eq!(
            EXTRACTOR_VERSIONS.len(),
            ROUTES.len() + cards::KINDS.len(),
            "versions name only routes and card kinds"
        );
        // Every route is reachable, so ROUTES lists nothing classify cannot return.
        let samples = [
            "https://example.org/page",
            "https://www.openstreetmap.org/node/1",
            "https://osm.org/go/0EEQjE--",
            "geo:46.0,8.9",
            "webcal://example.org/calendar.ics",
            "nostr:naddr1qqxnzd3cxqmrzv3exgmr2wfeqgsxu35yyt0mwjjh8pcz4zprhxegz69t4wr9t74vk6zne58wzh0waycrqsqqqa28pccpzu",
            "pubky://o1gg96ewuojmopcjbz8895478wdtxtzzber7aezq6ror5a91j7dy/pub/example.app/x",
        ];
        let routed: std::collections::BTreeSet<_> =
            samples.iter().filter_map(|uri| classify(uri)).collect();
        assert_eq!(routed, ROUTES.into_iter().collect());
    }
    #[test]
    fn calendar_routes_share_the_expander_version() {
        let versions = extractor_versions();
        assert_eq!(
            versions["event"], versions["event_platform"],
            "event and event_platform run the same ICS expander and the expansion signature reads \
             `event`: if they must diverge, split out an expander version for calendar::expansion"
        );
    }
}
