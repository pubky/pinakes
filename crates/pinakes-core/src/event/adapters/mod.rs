//! Platform declarations are data; all network paths share the production egress policy.
use crate::{
    adapter::{Manifest, Mode},
    config::EventConfig,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};
use url::Url;

pub mod cursor;
pub mod nostr;
pub mod service;
pub mod state;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdapterConfig {
    pub enabled: bool,
    pub derive_resources: bool,
    pub websocket: bool,
    #[serde(skip)]
    pub websocket_deadline_ms: u64,
    #[serde(skip)]
    pub max_cursor_pages: usize,
    #[serde(skip)]
    pub max_indexer_pages: usize,
    #[serde(skip)]
    pub max_derived_resources: usize,
    #[serde(skip)]
    pub max_calendar_members: usize,
    /// Hours a NIP-52 calendar waits for a member that has never resolved before applying
    /// without it (`calendar_member_unavailable`).
    #[serde(skip)]
    pub calendar_member_wait_hours: u64,
    #[serde(skip)]
    pub max_linked_resources: usize,
    pub platforms: BTreeMap<String, Options>,
}
impl Default for AdapterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            derive_resources: true,
            websocket: true,
            websocket_deadline_ms: 2000,
            max_cursor_pages: 10,
            max_indexer_pages: 10,
            max_derived_resources: 10000,
            max_calendar_members: 50,
            calendar_member_wait_hours: 48,
            max_linked_resources: 100,
            platforms: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct SecretKey(String);
impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl Serialize for SecretKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("[REDACTED]")
    }
}
impl std::ops::Deref for SecretKey {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl From<String> for SecretKey {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl From<&str> for SecretKey {
    fn from(s: &str) -> Self {
        Self(s.into())
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub enabled: Option<bool>,
    // Never serialize adapter config into the public info response.
    pub key: Option<SecretKey>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub id: String,
    pub domains: Vec<String>,
    pub paths: Vec<String>,
    pub parser: String,
    pub mode: Mode,
    pub cadence_floor: u64,
    pub identity: String,
    pub cancellation: String,
    pub expected_fields: Vec<String>,
    pub canary: Option<Url>,
    /// When set, `canary` is a listing page and the canary reads its first link whose path has
    /// this form (`canary_link_matches`). Only the host and path are checked, so the event is as
    /// current as the provider keeps its listing.
    #[serde(default)]
    pub canary_link: Option<String>,
    pub canary_note: Option<String>,
    pub ua_profile: String,
    pub requires_key: bool,
    pub fragile: bool,
    pub enabled: bool,
}
/// Whether `path` has a canary link's form (`Definition::canary_link`): as many segments, each
/// equal to the pattern's, where `*` stands for any one non-empty segment.
pub fn canary_link_matches(pattern: &str, path: &str) -> bool {
    let (pattern, path): (Vec<&str>, Vec<&str>) =
        (pattern.split('/').collect(), path.split('/').collect());
    pattern.len() == path.len()
        && pattern
            .iter()
            .zip(&path)
            .all(|(p, s)| if *p == "*" { !s.is_empty() } else { p == s })
}
pub fn definitions() -> &'static [Definition] {
    static ALL: OnceLock<Vec<Definition>> = OnceLock::new();
    ALL.get_or_init(|| {
        serde_json::from_str(include_str!("../../../../../data/event-adapters.json"))
            .expect("bundled adapter declarations")
    })
}
pub fn definition(id: &str) -> Option<&'static Definition> {
    definitions().iter().find(|d| d.id == id)
}
pub fn detect(uri: &str) -> Option<&'static Definition> {
    let u = Url::parse(uri).ok()?;
    let host = u.host_str().unwrap_or("").trim_end_matches('.');
    let path = u.path();
    if host == "api.nostr.wine" {
        return definition("nip52")
            .filter(|d| d.canary.as_ref().is_some_and(|c| c.as_str() == uri));
    }
    if u.scheme() == "nostr" {
        return definition("nip52");
    }
    if path.contains("/remote.php/dav/public-calendars/")
        || (path.contains("/apps/calendar/p/") || path.contains("/apps/calendar/embed/"))
    {
        return definition("nextcloud");
    }
    if (host == "icloud.com" || host.ends_with(".icloud.com")) && path.contains("/published/") {
        return definition("icloud");
    }
    for (id, matches) in [
        ("google_calendar_api", host == "www.googleapis.com"),
        (
            "billetto_api",
            host == "api.billetto.com" || host == "billetto.dk" && path.starts_with("/api/"),
        ),
        ("eventfrog_api", host == "api.eventfrog.net"),
    ] {
        if matches {
            return service::keyed_url(id, uri)
                .ok()
                .and_then(|_| definition(id));
        }
    }
    if host == "api.btcmap.org" {
        return (path == "/v4/events"
            || path
                .strip_prefix("/v4/events/")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())))
        .then(|| definition("btcmap"))
        .flatten();
    }
    if host == "portal.einundzwanzig.space" {
        return (matches!(path, "/api/changes" | "/api/meetup-events")
            || path.contains("/meetup/")
                && path.rsplit_once("/event/").is_some_and(|(_, id)| {
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
                }))
        .then(|| definition("einundzwanzig"))
        .flatten();
    }
    if [
        "flockstr.com",
        "njump.me",
        "nostr.at",
        "honey.hivetalk.org",
        "letsmiti.app",
    ]
    .contains(&host)
    {
        return nostr::Target::decode(uri)
            .ok()
            .filter(|t| t.kind.is_none() || t.kind.is_some_and(|k| (31922..=31925).contains(&k)))
            .and_then(|_| definition("nip52"));
    }
    if path == "/starticket/wwwroot/ticketing/feeds/starticketfeed_D.xml" {
        return definition("starticket");
    }
    if let Some(d) = definitions().iter().find(|d| {
        d.domains
            .iter()
            .any(|h| host == h || host.strip_suffix(h).is_some_and(|p| p.ends_with('.')))
            && (d.paths.is_empty() || d.paths.iter().any(|p| path.starts_with(p)))
    }) {
        return Some(d);
    }
    if crate::event::service::is_feed(uri) || matches!(u.scheme(), "webcal" | "webcals") {
        return definition("ics");
    }
    None
}
pub fn detected_html(uri: &str, html: &str) -> Option<&'static Definition> {
    detect(uri).or_else(|| {
        let s = html.to_ascii_lowercase();
        if s.contains("pretix") && s.contains("/ical/") {
            definition("pretix")
        } else if s.contains("eventyay") {
            definition("eventyay")
        } else if s.contains("gancio") {
            definition("gancio")
        } else if s.contains("gathio") || s.contains("/export/event/") {
            definition("gathio")
        } else if s.contains("mobilizon") {
            definition("mobilizon")
        } else if s.contains("hitobito") {
            definition("hitobito")
        } else {
            None
        }
    })
}
pub fn options<'a>(cfg: &'a EventConfig, id: &str) -> Option<&'a Options> {
    cfg.adapters.platforms.get(id)
}
pub fn manifest(d: &Definition, cfg: &EventConfig) -> Manifest {
    let opts = options(cfg, &d.id);
    let reason = if !cfg.enabled || !cfg.adapters.enabled {
        Some("event adapters disabled")
    } else if !opts.and_then(|o| o.enabled).unwrap_or(d.enabled) {
        Some("disabled by configuration")
    } else if d.requires_key
        && opts
            .and_then(|o| o.key.as_deref())
            .is_none_or(str::is_empty)
    {
        Some("missing API key")
    } else if d.mode == Mode::Container && !cfg.calendars {
        Some("calendars disabled")
    } else {
        None
    };
    Manifest {
        source: d.id.clone(),
        mode: d.mode,
        cadence_floor: d.cadence_floor.max(if d.mode == Mode::None {
            7 * 86400
        } else if d.mode == Mode::Cursor {
            60
        } else {
            3600
        }),
        identity: d.identity.clone(),
        cancellation: d.cancellation.clone(),
        expected_fields: d.expected_fields.clone(),
        canary: d.canary.clone().map(|mut url| {
            if d.id == "btcmap" && url.path() == "/v4/events" {
                url.set_query(None);
                url.query_pairs_mut().append_pair(
                    "updated_since",
                    &(chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
                );
            }
            url
        }),
        canary_note: d.canary_note.clone(),
        ua_profile: d.ua_profile.clone(),
        requires_key: d.requires_key,
        enabled: reason.is_none(),
        disabled_reason: reason.map(str::to_owned),
        fragile: d.fragile,
    }
}
pub fn manifests(cfg: &EventConfig) -> Vec<Manifest> {
    definitions().iter().map(|d| manifest(d, cfg)).collect()
}
pub fn profile(name: &str) -> &'static str {
    match name {
        "browser" => "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        "firefox" => "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0",
        "plain" => "curl/8.10.1",
        _ => crate::resolver::egress::USER_AGENT,
    }
}
pub fn validate(cfg: &EventConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=100).contains(&cfg.adapters.max_cursor_pages)
            && (1..=20).contains(&cfg.adapters.max_indexer_pages)
            && (1..=100000).contains(&cfg.adapters.max_derived_resources)
            && (1..=500).contains(&cfg.adapters.max_calendar_members)
            && (1..=720).contains(&cfg.adapters.calendar_member_wait_hours)
            && (1..=1000).contains(&cfg.adapters.max_linked_resources)
            && (100..=5000).contains(&cfg.adapters.websocket_deadline_ms),
        "invalid adapter limits"
    );
    for d in definitions() {
        anyhow::ensure!(
            d.domains
                .iter()
                .all(|h| *h == h.to_ascii_lowercase() && !h.contains('/'))
                && d.paths.iter().all(|p| p.starts_with('/'))
                && [
                    "page",
                    "card",
                    "dice",
                    "ents24",
                    "ticketino",
                    "xceed",
                    "calendar",
                    "portal",
                    "btcmap",
                    "nostr",
                    "luma",
                    "meetup",
                    "eventbrite",
                    "partiful",
                    "posh",
                    "ringcentral",
                    "airmeet",
                    "mobilizon",
                    "gancio",
                    "wikidata",
                    "ics_link",
                    "jsonld",
                    "ticketmaster",
                    "skiddle",
                    "billetto_api",
                    "eventfrog_api",
                    "google_calendar_api",
                    "resident_advisor",
                    "none",
                    "state"
                ]
                .contains(&d.parser.as_str())
                && ["pinakes", "plain", "browser", "firefox"].contains(&d.ua_profile.as_str())
                && d.canary_link.as_ref().is_none_or(|l| {
                    d.canary.is_some() && l.starts_with('/') && l.split('/').any(|s| s == "*")
                }),
            "invalid adapter declaration {}",
            d.id
        );
    }
    for id in cfg.adapters.platforms.keys() {
        anyhow::ensure!(definition(id).is_some(), "unknown event adapter {id}");
    }
    Ok(())
}
