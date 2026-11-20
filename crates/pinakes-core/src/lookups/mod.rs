//! Exact lookups on external services, as pure request builders and response parsers, and the
//! "also on" sources that run them ([`sources`]).
//!
//! The family modules perform no I/O. A builder returns the `Request` to send; a source fetches
//! it through Egress and hands the status and body to the matching parser. Parsers return
//! `Ok(None)` (or an empty result) when the service answers that the object does not exist, and
//! an error for anything else they cannot read, so the caller retries later.
//!
//! Every join is exact: a key read from the service that owns it, compared byte for byte (after
//! the key's own normalisation). No title or name matching.
use anyhow::{bail, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::fmt;
use url::Url;

pub mod apps;
pub mod archive;
pub mod games;
pub mod packages;
pub mod papers;
pub mod places;
pub mod wikidata;

/// A lookup family: the pure parsers of one kind of service (`lookups/<family>.rs`) and the also-on
/// sources that run them (its `also_on` module). Listed once in [`FAMILIES`].
pub trait Family: Send + Sync {
    /// Its module, its page in docs/sources and its fixture directory.
    fn name(&self) -> &'static str;
    /// Its also-on sources, in the order they run (after the other also-on sources).
    fn sources(&self) -> Vec<std::sync::Arc<dyn crate::also_on::Source>>;
}
/// Every lookup family, one line each, in the order their sources run.
pub static FAMILIES: &[&dyn Family] = &[
    &places::Lookups,
    &papers::Lookups,
    &packages::Lookups,
    &games::Lookups,
    &apps::Lookups,
    &archive::Lookups,
];

/// One HTTP GET to an external API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub url: Url,
    /// The `Accept` header, when the API needs a specific one.
    pub accept: Option<&'static str>,
    /// Whether the request follows redirects; `false` answers the redirect itself (its status
    /// and `location`), where the redirect target is the answer.
    pub follow_redirects: bool,
}
impl Request {
    fn get(url: &str) -> Option<Self> {
        Some(Self {
            url: Url::parse(url).ok()?,
            accept: None,
            follow_redirects: true,
        })
    }
}

/// A link to the same object on another service, derived from an exact lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The service's name as shown to people, e.g. "Software Heritage".
    pub site: String,
    pub uri: String,
    /// The source the link was derived from, e.g. `wikidata` or `unpaywall`.
    pub via: &'static str,
    /// What the link is, when the service offers more than one kind, e.g. `pdf`.
    pub kind: Option<&'static str>,
    /// When the linked copy was made (RFC 3339), for archived copies.
    pub at: Option<String>,
}
impl Link {
    fn new(site: impl Into<String>, uri: impl Into<String>, via: &'static str) -> Self {
        Self {
            site: site.into(),
            uri: uri.into(),
            via,
            kind: None,
            at: None,
        }
    }
    fn kind(mut self, kind: &'static str) -> Self {
        self.kind = Some(kind);
        self
    }
}

/// An exact join key, `{namespace}:{value}`, e.g. `wikidata:Q7024` or `osm:relation/2888417`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(String);
impl Key {
    fn new(namespace: &str, value: &str) -> Self {
        Self(format!("{namespace}:{value}"))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one lookup found: the keys it proved and the links it derived.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub keys: Vec<Key>,
    pub links: Vec<Link>,
}
impl Found {
    fn push_key(&mut self, key: Key) {
        if !self.keys.contains(&key) {
            self.keys.push(key);
        }
    }
    fn push_link(&mut self, link: Link) {
        if !self.links.iter().any(|l| l.uri == link.uri) {
            self.links.push(link);
        }
    }
}

/// The plain name of a service the also-on sources read or link to, by its link: the name their
/// derived links carry ("OpenStreetMap", "Google Play", "crates.io", "DOI"), so a tagged member
/// of a group and a derived link to the same service read as one service. Only an object's own
/// page is named (a game, an app, a package, a repository, a paper, a place: the forms its source
/// recognizes); any other page on those hosts (a topic list, a search, a user's page, a page
/// under an object) is `None`, so it takes no service's place on a card. OpenStreetMap, Wikidata
/// and DOI are named here; every other service by the source that reads it
/// (`also_on::Source::service_name`).
pub fn service_name(uri: &str) -> Option<&'static str> {
    if uri.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("doi:")) {
        return crate::resolver::cards::doi(uri).map(|_| "DOI");
    }
    let url = web_url(uri)?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    match host {
        "openstreetmap.org" | "osm.org" => {
            return crate::resolver::osm::parse_id(uri).map(|_| "OpenStreetMap")
        }
        "wikidata.org" | "m.wikidata.org" => {
            return wikidata::wikidata_item(uri).map(|_| "Wikidata")
        }
        "doi.org" | "dx.doi.org" => return crate::resolver::cards::doi(uri).map(|_| "DOI"),
        _ => {}
    }
    crate::also_on::registered()
        .iter()
        .find_map(|s| s.service_name(host, &url))
}

/// The join key a search names (`key={namespace}:{value}`), normalized as the sources record
/// it, or `None` when no source records that namespace: an OSM object and a DOI here, every
/// other namespace by the source that records it (`also_on::Source::search_key`).
pub fn search_key(namespace: &str, value: &str) -> Option<String> {
    let key = match namespace {
        "osm" => {
            let (kind, id) = value.split_once('/')?;
            (matches!(kind, "node" | "way" | "relation") && digits(id))
                .then(|| Key::new("osm", value))?
        }
        "doi" => Key::new(
            "doi",
            &crate::resolver::cards::doi(&format!("doi:{value}"))?,
        ),
        _ => {
            return crate::also_on::registered()
                .iter()
                .find_map(|s| s.search_key(namespace, value))
        }
    };
    Some(key.0)
}

/// Positive decimal digits without a leading zero, at most 20 (an ID).
pub(crate) fn digits(value: &str) -> bool {
    plain(value, 20, |c| c.is_ascii_digit()) && !value.starts_with('0')
}

/// The body of a response, `None` for a status that means "no such object", or an error.
fn body<'a>(status: u16, body: &'a [u8], missing: &[u16]) -> Result<Option<&'a [u8]>> {
    match status {
        200 => Ok(Some(body)),
        s if missing.contains(&s) => Ok(None),
        s => bail!("unexpected status {s}"),
    }
}

/// Characters kept as they are in a path segment or query value.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
/// As `COMPONENT`, keeping `/` (a DOI or a repository path).
const PATH: &AsciiSet = &COMPONENT.remove(b'/');

fn encode(value: &str) -> String {
    utf8_percent_encode(value, COMPONENT).to_string()
}
fn encode_path(value: &str) -> String {
    utf8_percent_encode(value, PATH).to_string()
}

/// A plain identifier: non-empty, at most `max` bytes, and only the characters `ok` allows.
fn plain(value: &str, max: usize, ok: impl Fn(char) -> bool) -> bool {
    !value.is_empty() && value.len() <= max && value.chars().all(ok)
}

/// Whether a link's query only picks a tab or marks where the link was shared from (`tab`,
/// `ref`, `utm_*`, …), so that the page is the one its path names.
pub(crate) fn harmless_query(url: &Url) -> bool {
    url.query_pairs().all(|(k, _)| {
        matches!(
            &*k,
            "tab" | "ref" | "ref_src" | "ref_type" | "source" | "fbclid" | "gclid"
        ) || k.starts_with("utm_")
    })
}

/// An `https` or `http` URL with a host, as given (no rewriting).
fn web_url(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "https" | "http") && url.host_str().is_some()).then_some(url)
}

// What the also-on sources running these lookups share.
use crate::{
    also_on::{self, Context},
    hubs,
    resolver::{egress::FetchOptions, RetryLater},
};
use serde_json::Value;
use std::time::Duration;

impl Found {
    /// As the also-on machinery records it: every key vouched for by `via`.
    pub fn into_also_on(self, via: &str) -> also_on::Found {
        also_on::Found {
            keys: self
                .keys
                .into_iter()
                .map(|k| also_on::Key {
                    key: k.0,
                    via: via.into(),
                })
                .collect(),
            links: self
                .links
                .into_iter()
                .map(|l| {
                    let mut link = also_on::Link::new(l.uri, l.site, l.via);
                    link.kind = l.kind.map(str::to_owned);
                    link.at = l.at.and_then(|at| at.parse().ok());
                    link
                })
                .collect(),
            ..Default::default()
        }
    }
}

/// A JSON GET through the shared discovery cache (an hour). A busy service (429, 5xx) answers
/// `RetryLater` with `class`; every other status is the caller's. A Wikidata read takes the
/// hubs' larger cap (a much-edited item), as the hubs' own reads of the same URL do.
pub(crate) async fn get(cx: &Context, url: &str, class: &'static str) -> Result<(u16, Vec<u8>)> {
    let options = FetchOptions {
        accept: Some("application/json".into()),
        body_cap: url
            .starts_with("https://www.wikidata.org/")
            .then_some(hubs::WIKIDATA_CAP),
        ..Default::default()
    };
    let f = cx.egress.discover(url, &options, 3600).await?;
    if f.status == 429 || f.status >= 500 {
        return Err(RetryLater {
            class,
            seconds: f.retry_after_secs.clamp(60, 3600),
        }
        .into());
    }
    Ok((f.status, f.body))
}

/// One Wikidata item, as the raw entity (for every claim type, URLs included) and as the hub
/// reads it (external IDs and sitelinks), or `None` when it has none, Wikidata does not know the
/// ID, or its answer is over the cap. The request is the hub's own and is read as the hub reads
/// it, so both share its cache: a busy Wikidata, one that says so with 200 too (`maxlag`,
/// `ratelimited`), is a retry and no answer the cache keeps.
pub(crate) async fn item(cx: &Context, qid: &str) -> Result<Option<(Value, hubs::wikidata::Item)>> {
    // An item over its read's cap is no answer: asking again would read the same megabytes.
    let answer = match hubs::wikidata::entities_answer(&cx.egress, &[qid.to_owned()]).await {
        Err(error) if crate::resolver::egress::over_cap(&error) => return Ok(None),
        answer => answer?,
    };
    let Some(answer) = answer else {
        return Ok(None);
    };
    if let Some(error) = answer.get("error") {
        if error["code"] == "no-such-entity" {
            return Ok(None);
        }
        bail!("wikidata error: {}", error["code"]);
    }
    // One parse of the body (up to the hubs' cap) for both readings.
    let hub = hubs::wikidata::parse_entities(&answer)
        .into_iter()
        .find(|i| i.qid == qid);
    let Some(raw) = wikidata::entities(answer)?.remove(qid) else {
        return Ok(None);
    };
    Ok(hub.map(|hub| (raw, hub)))
}

/// A request a builder made, through the shared discovery cache (an hour), with its `Accept`.
/// A busy service (429, 5xx) answers `RetryLater` with `class`.
pub(crate) async fn send(
    cx: &Context,
    request: &Request,
    class: &'static str,
) -> Result<crate::resolver::egress::Fetched> {
    let options = FetchOptions {
        accept: Some(request.accept.unwrap_or("application/json").into()),
        // JSON under its own media types: PyPI's provenance, JSON:API.
        extra_types: &[
            "application/vnd.pypi.integrity.v1+json",
            "application/vnd.api+json",
        ],
        keep_redirect: !request.follow_redirects,
        ..Default::default()
    };
    let f = cx
        .egress
        .discover(request.url.as_str(), &options, 3600)
        .await?;
    if f.status == 429 || f.status >= 500 {
        return Err(RetryLater {
            class,
            seconds: f.retry_after_secs.clamp(60, 3600),
        }
        .into());
    }
    Ok(f)
}

/// A secondary service's answer, or nothing when its request failed: the lookup goes on with
/// what the link and the other services gave, the failure is counted
/// (`pinakes_also_on_service_errors_total{service,class}`), and `retry` is set, so the worker
/// brings the run back after its backoff (or the wait the service named, when longer) instead
/// of after the source's lifetime.
pub(crate) fn secondary<T>(
    service: &'static str,
    retry: &mut Option<Duration>,
    result: Result<T>,
) -> Option<T> {
    match result {
        Ok(answer) => Some(answer),
        Err(error) => {
            let class = crate::resolver::error_class(&error);
            crate::telemetry::also_on_service_error(service, class);
            tracing::debug!(
                service,
                class,
                "secondary service failed; the lookup goes on without it"
            );
            let asked = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<RetryLater>())
                .map_or(Duration::ZERO, |r| Duration::from_secs(r.seconds));
            *retry = Some(retry.map_or(asked, |r| r.max(asked)));
            None
        }
    }
}

/// `found` as recorded, with `retry` set when a secondary service failed.
pub(crate) fn with_retry(mut found: also_on::Found, retry: Option<Duration>) -> also_on::Found {
    found.retry_after = retry;
    found
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Recorded responses: `tests/fixtures/lookups/<family>/` (each family's, gone with it) and
    //! `tests/fixtures/lookups/shared/` (Wikidata items several families read); see each
    //! directory's `provenance.json`.
    use crate::{
        also_on::{self, Context, Subject},
        model::ResourceMeta,
    };
    use serde_json::Value;
    use std::io::Read;

    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lookups");
    /// The fixture directories this build has.
    fn dirs() -> Vec<std::path::PathBuf> {
        let mut dirs: Vec<_> = std::fs::read_dir(DIR)
            .expect("fixtures")
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        dirs
    }

    /// A recorded response, from whichever directory holds it.
    pub fn read(name: &str) -> Vec<u8> {
        let path = dirs()
            .into_iter()
            .map(|d| d.join(name))
            .find(|p| p.exists())
            .unwrap_or_else(|| panic!("no fixture {name}"));
        let raw = std::fs::read(path).expect("fixture");
        if name.ends_with(".gz") {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw.as_slice())
                .read_to_end(&mut out)
                .expect("gzip fixture");
            out
        } else {
            raw
        }
    }
    pub const JSON: &str = "application/json";

    /// `link` with a multibyte character inserted at every boundary, for checks that may never
    /// slice a string inside one.
    pub fn multibyte_spellings(links: &[&str]) -> Vec<String> {
        let mut out = vec![];
        for link in links {
            for at in (0..=link.len()).filter(|i| link.is_char_boundary(*i)) {
                for insert in ["é", "€", "😀", "\u{301}"] {
                    out.push(format!("{}{insert}{}", &link[..at], &link[at..]));
                }
            }
        }
        out
    }
    /// Answers no parser expects (JSON of the wrong shape, other types, no JSON), with the
    /// statuses they may come with.
    pub fn malformed_answers() -> Vec<(u16, &'static [u8])> {
        let raws: [&'static [u8]; 9] = [
            b"[]",
            b"false",
            b"null",
            b"1",
            b"\"x\"",
            b"{}",
            b"{\"data\": 1, \"message\": [], \"entities\": false, \"records\": {}}",
            b"[{}]",
            b"not json",
        ];
        raws.iter()
            .flat_map(|raw| [(200, *raw), (404, *raw)])
            .collect()
    }

    pub fn subject(uri: &str, wikidata: Option<&str>) -> Subject {
        let mut doc = ResourceMeta::new(uri, "osm", 3600);
        if let Some(q) = wikidata {
            doc.attributes.insert("wikidata".into(), q.into());
        }
        Subject {
            id: "x".into(),
            uri: uri.into(),
            status: "ok".into(),
            type_name: Some("Place".into()),
            doc: Some(doc),
            keys: vec![],
            first_seen_at: None,
            candidates: vec![],
        }
    }

    pub fn context(routes: Vec<(&str, u16, &'static str, Vec<u8>)>) -> Context {
        let (egress, _) = crate::sources::fixture::egress(routes);
        Context {
            egress,
            config: Default::default(),
        }
    }

    /// Every recorded lookup answer, by its request URL (a redirect's body is its `Location`),
    /// plus `extra` routes for requests made differently from the recording.
    pub fn recorded(extra: Vec<(String, &str)>) -> Context {
        recorded_but_busy(extra, &[])
    }

    /// As `recorded`, with `busy` URLs answering 503.
    pub fn recorded_but_busy(extra: Vec<(String, &str)>, busy: &[&str]) -> Context {
        // Every directory's provenance: the routes of every family this build has.
        let mut provenance = serde_json::Map::new();
        for dir in dirs() {
            let raw = std::fs::read(dir.join("provenance.json")).expect("provenance");
            let entries: serde_json::Map<String, Value> = serde_json::from_slice(&raw).unwrap();
            provenance.extend(entries);
        }
        let provenance = Value::Object(provenance);
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let mut routes: Vec<(String, u16, &'static str, Vec<u8>)> = vec![];
        for entry in provenance.as_object().unwrap().values() {
            let status = entry["status"].as_u64().unwrap() as u16;
            let kind = leak(
                entry["content_type"]
                    .as_str()
                    .unwrap_or("text/html")
                    .to_owned(),
            );
            let body = match (entry["location"].as_str(), entry["file"].as_str()) {
                (Some(location), _) => location.as_bytes().to_vec(),
                (None, Some(file)) => read(file),
                (None, None) => vec![],
            };
            routes.push((
                entry["url"].as_str().unwrap().to_owned(),
                status,
                kind,
                body,
            ));
        }
        for (url, like) in extra {
            let entry = &provenance[like];
            let kind = leak(entry["content_type"].as_str().unwrap().to_owned());
            routes.push((
                url,
                entry["status"].as_u64().unwrap() as u16,
                kind,
                read(entry["file"].as_str().unwrap()),
            ));
        }
        for url in busy {
            routes.push((url.to_string(), 503, "text/html", vec![]));
        }
        context(
            routes
                .iter()
                .map(|(u, s, k, b)| (u.as_str(), *s, *k, b.clone()))
                .collect(),
        )
    }

    pub fn at(uri: &str) -> Subject {
        Subject {
            doc: None,
            ..subject(uri, None)
        }
    }

    pub fn sites(found: &also_on::Found) -> Vec<String> {
        found
            .links
            .iter()
            .map(|l| format!("{} {}", l.site, l.uri))
            .collect()
    }

    pub fn keys(found: &also_on::Found) -> Vec<&str> {
        found.keys.iter().map(|k| k.key.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Link-derived text may hold any character: no link or id check may slice a string inside
    /// one (each family checks its own the same way).
    #[test]
    fn a_multibyte_character_anywhere_in_a_link_never_panics() {
        for s in fixture::multibyte_spellings(&["https://www.wikidata.org/wiki/Q7024", "Q7024"]) {
            wikidata::qid(&s);
            wikidata::wikidata_item(&s);
            service_name(&s);
        }
    }

    /// Wikidata's `maxlag` comes with 200: the item's read is a retry, and the next ask fetches
    /// again instead of reading the error from the cache for an hour. An ID Wikidata says it
    /// does not know is an answer: no item.
    #[tokio::test(start_paused = true)]
    async fn a_wikidata_error_sent_with_200_is_a_retry_and_is_asked_again() {
        let url = hubs::wikidata::entities_lookup(&["Q243".to_owned()]);
        let context = |body: &[u8]| {
            let (egress, recorded) = crate::sources::fixture::egress(vec![(
                url.as_str(),
                200,
                "application/json",
                body.to_vec(),
            )]);
            let cx = Context {
                egress,
                config: Default::default(),
            };
            (cx, recorded)
        };
        let (cx, recorded) =
            context(br#"{"error":{"code":"maxlag","info":"Waiting for a database server"}}"#);
        for _ in 0..2 {
            let error = item(&cx, "Q243").await.unwrap_err();
            assert!(error.downcast_ref::<RetryLater>().is_some(), "{error:#}");
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        assert_eq!(recorded.asked().len(), 2);
        let (cx, _) = context(br#"{"error":{"code":"no-such-entity","info":"no such entity"}}"#);
        assert!(item(&cx, "Q243").await.unwrap().is_none());
    }

    #[test]
    fn search_keys() {
        for (namespace, value, key) in [
            ("osm", "node/240109189", "osm:node/240109189"),
            ("doi", "10.1038/NATURE12373", "doi:10.1038/nature12373"),
        ] {
            assert_eq!(
                search_key(namespace, value).as_deref(),
                Some(key),
                "{namespace}:{value}"
            );
        }
        for (namespace, value) in [
            ("osm", "area/1"),
            ("osm", "node/01"),
            ("isbn", "1"),
            ("nonesuch", "1"),
        ] {
            assert_eq!(search_key(namespace, value), None, "{namespace}:{value}");
        }
    }

    #[test]
    fn statuses() {
        assert_eq!(body(200, b"x", &[404]).unwrap(), Some(&b"x"[..]));
        assert_eq!(body(404, b"x", &[404]).unwrap(), None);
        assert!(body(429, b"x", &[404]).is_err());
        assert!(body(500, b"x", &[404]).is_err());
        assert!(body(404, b"x", &[]).is_err());
    }

    #[test]
    fn encoding() {
        assert_eq!(encode("a/b c?d"), "a%2Fb%20c%3Fd");
        assert_eq!(encode_path("10.1000/x<y>"), "10.1000/x%3Cy%3E");
        assert_eq!(encode("Stardew_Valley-1.5~"), "Stardew_Valley-1.5~");
    }

    #[test]
    fn found_deduplicates() {
        let mut f = Found::default();
        f.push_key(Key::new("doi", "10.1/x"));
        f.push_key(Key::new("doi", "10.1/x"));
        f.push_link(Link::new("A", "https://a/", "x"));
        f.push_link(Link::new("B", "https://a/", "y"));
        assert_eq!((f.keys.len(), f.links.len()), (1, 1));
        assert_eq!(f.keys[0].to_string(), "doi:10.1/x");
    }

    /// OpenStreetMap, Wikidata and DOI are named here, an object's own page only; each family
    /// tests the names of its own services.
    #[test]
    fn a_service_card_has_its_plain_name() {
        for (uri, name) in [
            (
                "https://www.openstreetmap.org/node/240109189",
                Some("OpenStreetMap"),
            ),
            ("https://www.openstreetmap.org/about", None),
            ("https://www.wikidata.org/wiki/Q64", Some("Wikidata")),
            ("doi:10.1038/nature12373", Some("DOI")),
            ("https://doi.org/10.1038/nature12373", Some("DOI")),
            ("https://example.org/", None),
            ("https://doi.org/", None),
            ("https://www.openstreetmap.org/user/someone", None),
        ] {
            assert_eq!(service_name(uri), name, "{uri}");
        }
    }

    /// Every parser takes any answer: JSON of the wrong shape, other types or no JSON at all is an
    /// error or nothing, never a panic (a panic would end the also-on run). Each family checks
    /// its own parsers the same way.
    #[test]
    fn no_parser_panics_on_a_malformed_answer() {
        for (status, raw) in fixture::malformed_answers() {
            let _ = wikidata::parse_entities(status, raw);
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) {
                let _ = wikidata::best_values(&v, "P402");
            }
        }
    }
}
