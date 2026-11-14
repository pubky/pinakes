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
    pub requires_key: bool,
    pub fragile: bool,
    pub enabled: bool,
    /// The feed that lists the dates of an event page's organiser, when the platform derives it
    /// from the page's path (`container_feed`).
    #[serde(default)]
    pub container_feed: Option<ContainerFeed>,
}
/// An event page's organiser feed on the page's own origin: a page whose path has the form
/// `page` (as `canary_link_matches` reads it, a trailing slash aside) names the feed at `feed`,
/// where `{n}` is the n-th `*` segment of the page's path (Meetup: `/*/events/*` names
/// `/{1}/events/ical/`, the group's calendar).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerFeed {
    pub page: String,
    pub feed: String,
}
impl ContainerFeed {
    /// Both paths absolute, the feed's without a query, fragment or empty segment, and each
    /// `{n}` naming one of the page's `*` segments.
    fn valid(&self) -> bool {
        let wild = self.page.split('/').filter(|s| *s == "*").count();
        let mut literal = String::new();
        let mut rest = self.feed.as_str();
        while let Some((before, after)) = rest.split_once('{') {
            let Some((n, tail)) = after.split_once('}') else {
                return false;
            };
            if !n.parse::<usize>().is_ok_and(|n| (1..=wild).contains(&n)) {
                return false;
            }
            literal.push_str(before);
            rest = tail;
        }
        literal.push_str(rest);
        self.page.starts_with('/')
            && wild > 0
            && self.feed.starts_with('/')
            && !self.feed.contains("//")
            && !literal.contains(['?', '#', '}', '*'])
    }
}
/// The platform whose organiser calendar `uri` is (`Definition::container_feed`): its path has
/// the feed's form, each `{n}` one segment, without a query.
fn organiser_calendar_of(uri: &str) -> Option<&'static Definition> {
    let d = detect(uri)?;
    let u = Url::parse(uri).ok()?;
    (u.query().is_none() && feed_form(d, u.path())).then_some(d)
}
/// Whether `path` has the form of `d`'s organiser calendar path.
fn feed_form(d: &Definition, path: &str) -> bool {
    let Some(spec) = d.container_feed.as_ref() else {
        return false;
    };
    let mut pattern = String::new();
    let mut rest = spec.feed.as_str();
    while let Some((before, after)) = rest.split_once('{') {
        pattern.push_str(before);
        pattern.push('*');
        rest = after.split_once('}').map_or("", |(_, tail)| tail);
    }
    pattern.push_str(rest);
    canary_link_matches(pattern.trim_end_matches('/'), path.trim_end_matches('/'))
}
/// Whether `uri` is an organiser calendar a platform derives for its event pages
/// (`Definition::container_feed`).
pub fn names_organiser_calendar(uri: &str) -> bool {
    organiser_calendar_of(uri).is_some()
}
/// Whether an answer for `uri` that came from `answered` (where redirects ended) is no answer of
/// an organiser calendar (`names_organiser_calendar`). The platform's own answer comes over https
/// from a host the platform declares (`Definition::domains`, the host or a subdomain, as
/// `detect` reads them), at the organiser calendar's path form, without a query: every spelling
/// of the calendar (`webcal:`, `http:`, the bare domain) that the platform answers so is read.
/// Anything else is off its origin. Grouping trusts such a calendar's dates by its address
/// (`groups`), so it must hold the platform's own.
pub fn off_origin_calendar(uri: &str, answered: &str) -> bool {
    let Some(d) = organiser_calendar_of(uri) else {
        return false;
    };
    let Ok(answer) = Url::parse(answered) else {
        return true;
    };
    let host = answer.host_str().unwrap_or("").trim_end_matches('.');
    let platform_host = d.domains.iter().any(|h| {
        host == h
            || host
                .strip_suffix(h.as_str())
                .is_some_and(|p| p.ends_with('.'))
    });
    !(answer.scheme() == "https"
        && answer.port().is_none()
        && answer.username().is_empty()
        && answer.password().is_none()
        && platform_host
        && answer.query().is_none()
        && feed_form(d, answer.path()))
}
/// The organiser feed of the event page `uri` on platform `d` (`Definition::container_feed`):
/// on the page's own scheme, host and port, without its query or fragment. `None` when the
/// platform declares none, the page's path has another form, or the feed is the page itself.
pub fn container_feed(d: &Definition, uri: &str) -> Option<String> {
    let spec = d.container_feed.as_ref()?;
    let page = Url::parse(uri).ok()?;
    if !matches!(page.scheme(), "http" | "https") {
        return None;
    }
    let path = page.path().trim_end_matches('/');
    let pattern = spec.page.trim_end_matches('/');
    if !canary_link_matches(pattern, path) {
        return None;
    }
    let captured: Vec<&str> = pattern
        .split('/')
        .zip(path.split('/'))
        .filter(|(p, _)| *p == "*")
        .map(|(_, s)| s)
        .collect();
    let mut feed = spec.feed.clone();
    for (n, segment) in captured.iter().enumerate() {
        feed = feed.replace(&format!("{{{}}}", n + 1), segment);
    }
    let mut url = page.clone();
    url.set_path(&feed);
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    (url.path().trim_end_matches('/') != path).then(|| url.to_string())
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
        requires_key: d.requires_key,
        enabled: reason.is_none(),
        disabled_reason: reason.map(str::to_owned),
        fragile: d.fragile,
    }
}
pub fn manifests(cfg: &EventConfig) -> Vec<Manifest> {
    definitions().iter().map(|d| manifest(d, cfg)).collect()
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
                    "ticketmaster",
                    "skiddle",
                    "billetto_api",
                    "eventfrog_api",
                    "google_calendar_api",
                    "resident_advisor"
                ]
                .contains(&d.parser.as_str())
                && d.canary_link.as_ref().is_none_or(|l| {
                    d.canary.is_some() && l.starts_with('/') && l.split('/').any(|s| s == "*")
                })
                && d.container_feed.as_ref().is_none_or(ContainerFeed::valid),
            "invalid adapter declaration {}",
            d.id
        );
    }
    for id in cfg.adapters.platforms.keys() {
        anyhow::ensure!(definition(id).is_some(), "unknown event adapter {id}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A Meetup date's page, by number or token, names its group's calendar on its own origin;
    /// other paths name none, and the calendar names no other.
    #[test]
    fn a_meetup_date_names_its_groups_calendar() {
        let meetup = definition("meetup").unwrap();
        let feed = "https://www.meetup.com/bitcoin-meetup-switzerland/events/ical/";
        for page in [
            "https://www.meetup.com/bitcoin-meetup-switzerland/events/316908442/",
            "https://www.meetup.com/bitcoin-meetup-switzerland/events/316908442",
            "https://www.meetup.com/bitcoin-meetup-switzerland/events/frxnbvyjcqbmb/?recId=1#x",
        ] {
            assert_eq!(
                container_feed(meetup, page).as_deref(),
                Some(feed),
                "{page}"
            );
        }
        for page in [
            feed,
            "https://www.meetup.com/bitcoin-meetup-switzerland/",
            "https://www.meetup.com/bitcoin-meetup-switzerland/events/",
            "https://www.meetup.com/bitcoin-meetup-switzerland/events/316908442/attendees/",
            "https://www.meetup.com/find/events/",
        ] {
            assert_eq!(container_feed(meetup, page), None, "{page}");
        }
        assert_eq!(
            container_feed(definition("luma").unwrap(), "https://luma.com/x/events/1/"),
            None
        );
        // Declarations: a reference names a `*` of the page; the feed carries no query.
        let spec = |page: &str, feed: &str| ContainerFeed {
            page: page.into(),
            feed: feed.into(),
        };
        assert!(spec("/*/events/*", "/{1}/events/ical/").valid());
        assert!(spec("/*/events/*", "/{2}.ics").valid());
        for (page, feed) in [
            ("/*/events/*", "/{3}/ical/"),
            ("/*/events/*", "/{0}/ical/"),
            ("/*/events/*", "/{1/ical/"),
            ("/*/events/*", "/{1}/ical/?x=1"),
            ("/*/events/*", "{1}/ical/"),
            ("/*/events/*", "//{1}/ical/"),
            ("/events/x", "/ical/"),
        ] {
            assert!(!spec(page, feed).valid(), "{page} {feed}");
        }
        validate(&EventConfig::default()).unwrap();
    }
}
