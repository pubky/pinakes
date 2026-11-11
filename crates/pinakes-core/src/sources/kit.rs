//! What the data sources share on top of `super`: the request a builder returns, the card a
//! parser returns (with its conversion to a resource document), exact keys and derived links,
//! and small readers for JSON values.
//!
//! Builders and parsers perform no I/O. A builder returns the `Request` to send; `fetch` sends it
//! through Egress and the status and body go to the matching parser. A parser returns `Ok(None)`
//! when the service answers that the object does not exist, and an error for anything else it
//! cannot read (a malformed body, or an answer for a different object); rate limits and server
//! errors never reach it (`Context::fetch` turns them into `RetryLater`).
use super::Context;
use crate::{
    model::{LocationSource, ResourceMeta},
    resolver::egress::{FetchOptions, Fetched},
};
use anyhow::{bail, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::Value;
use std::{collections::BTreeMap, fmt};
use url::Url;

#[cfg(test)]
pub(crate) use super::fixture;
pub use super::{text, texts};

/// An `https` or `http` URL with a host and no port, user name or password, as given. A link
/// that names one of these is not the service's own link form.
pub fn web_url(value: &str) -> Option<Url> {
    let url = super::web_url(value)?;
    (url.port().is_none() && url.username().is_empty() && url.password().is_none()).then_some(url)
}

/// One HTTP GET to an external API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub url: Url,
    /// The `Accept` header, when the API needs a specific one.
    pub accept: Option<&'static str>,
    /// Extra headers the service asks for, e.g. `AIC-User-Agent`. A request with extra headers
    /// never follows a redirect.
    pub headers: Vec<(&'static str, String)>,
    /// Content types the answer may have beyond the page set, e.g. `text/plain`.
    pub extra_types: &'static [&'static str],
}
impl Request {
    pub fn get(url: &str) -> Option<Self> {
        Some(Self {
            url: Url::parse(url).ok()?,
            accept: None,
            headers: vec![],
            extra_types: &[],
        })
    }
    pub fn accept(mut self, accept: &'static str) -> Self {
        self.accept = Some(accept);
        self
    }
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
    pub fn admit(mut self, types: &'static [&'static str]) -> Self {
        self.extra_types = types;
        self
    }
    /// The egress options of this request.
    pub fn options(&self) -> FetchOptions {
        FetchOptions {
            accept: Some(self.accept.unwrap_or("application/json").into()),
            headers: self.headers.clone(),
            extra_types: self.extra_types,
            ..Default::default()
        }
    }
}

/// Sends a request through the egress policy (`Context::fetch`).
pub async fn fetch(cx: &Context<'_>, request: &Request) -> Result<Fetched> {
    cx.fetch(request.url.as_str(), &request.options()).await
}

/// A link to the same object on another service. The step-8 foundation shows these; the sources
/// only derive them from exact ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The service's name as shown to people, e.g. "Blockstream Explorer".
    pub site: String,
    pub uri: String,
    /// The source the link was derived from, e.g. `mempool`.
    pub via: &'static str,
    /// What the link is, when the service offers more than one kind, e.g. `pdf`.
    pub kind: Option<&'static str>,
}
impl Link {
    pub fn new(site: impl Into<String>, uri: impl Into<String>, via: &'static str) -> Self {
        Self {
            site: site.into(),
            uri: uri.into(),
            via,
            kind: None,
        }
    }
}

/// An exact join key, `{namespace}:{value}`, e.g. `doi:10.5281/zenodo.1234567` or
/// `wikidata:Q12418`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(String);
impl Key {
    pub fn new(namespace: &str, value: &str) -> Self {
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

/// A point, in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub lat: f64,
    pub lon: f64,
}
impl Point {
    /// A point inside the valid ranges, never (0, 0).
    pub fn new(lat: f64, lon: f64) -> Option<Self> {
        let ok = lat.is_finite()
            && lon.is_finite()
            && (-90.0..=90.0).contains(&lat)
            && (-180.0..=180.0).contains(&lon)
            && (lat, lon) != (0.0, 0.0);
        ok.then_some(Self { lat, lon })
    }
}

/// A card as one source reads it, before it becomes a resource document.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    /// A schema.org type (`Event`, `Dataset`, …) or a local one (`BitcoinBlock`, …).
    pub type_name: &'static str,
    /// The object's identifier, which every link form of it is an alias of, e.g.
    /// `usgs:us7000abcd` or `btc:block/0000…`.
    pub id: String,
    /// The object's page on the service.
    pub url: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// An image the licence lets us show.
    pub image: Option<String>,
    /// The audio a client can play (`model::Stream`): an episode's enclosure.
    pub stream: Option<crate::model::Stream>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub point: Option<Point>,
    /// The credit or licence the source asks us to show, e.g. "CC BY 4.0".
    pub attribution: Option<String>,
    /// Source-specific values (magnitude, licence, capacity…), by stable name.
    pub details: BTreeMap<&'static str, Value>,
    /// Other ids of the same object that it proves, e.g. `doi:…`.
    pub keys: Vec<Key>,
    /// The same object elsewhere, for the step-8 foundation.
    pub also_on: Vec<Link>,
    /// The objects it points at (`model::Related`), in the order a client lists them.
    pub related: Vec<crate::model::Related>,
    /// How many entries of a `rel` there are when `related` holds fewer of them.
    pub related_total: BTreeMap<String, u64>,
}
impl Card {
    pub fn new(type_name: &'static str, id: String, url: String) -> Self {
        Self {
            type_name,
            id,
            url,
            name: None,
            description: None,
            image: None,
            stream: None,
            start: None,
            end: None,
            point: None,
            attribution: None,
            details: BTreeMap::new(),
            keys: vec![],
            also_on: vec![],
            related: vec![],
            related_total: BTreeMap::new(),
        }
    }
    /// Sets a detail, skipping nulls and empty strings and arrays.
    pub fn detail(&mut self, name: &'static str, value: impl Into<Value>) {
        let value = value.into();
        let empty = match &value {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        };
        if !empty {
            self.details.insert(name, value);
        }
    }
    pub fn key(&mut self, key: Key) {
        if !self.keys.contains(&key) {
            self.keys.push(key);
        }
    }
    pub fn link(&mut self, link: Link) {
        if link.uri != self.url && !self.also_on.iter().any(|l| l.uri == link.uri) {
            self.also_on.push(link);
        }
    }

    /// The resource document of this card, from the source's new card for the link
    /// (`Context::card`). Its facts are the details as strings (lists joined with "; "), each
    /// key's value under its namespace (`doi`, `wikidata`, …) and the object's page as
    /// `page_url`; `related` and `related_total` as they are. The card's `id` is the link's root
    /// (`source_root`): when the link is another spelling, it becomes an alias of it.
    pub fn into_doc(self, mut doc: ResourceMeta, site: &str) -> ResourceMeta {
        doc.set_type(self.type_name);
        doc.site = site.into();
        doc.name = self.name;
        doc.description = self.description;
        doc.image = self.image;
        doc.stream = self.stream;
        doc.start = self.start;
        doc.end = self.end;
        if let Some(p) = self.point {
            doc.location = Some(crate::model::Point {
                lat: p.lat,
                lon: p.lon,
            });
            doc.location_source = Some(LocationSource::Page);
        }
        doc.attribution = self.attribution;
        for (name, value) in &self.details {
            if let Some(value) = fact(value) {
                doc.attributes.insert((*name).into(), value);
            }
        }
        let mut keys: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for key in &self.keys {
            if let Some((namespace, value)) = key.as_str().split_once(':') {
                keys.entry(namespace).or_default().push(value.into());
            }
        }
        for (namespace, values) in keys {
            if let Some(value) = super::list(&values) {
                doc.attributes.insert(namespace.into(), value);
            }
        }
        doc.attributes.insert("page_url".into(), self.url);
        doc.related = self.related;
        doc.related_total = self.related_total;
        doc.source_root = Some(self.id);
        doc
    }
}

/// A credit line for a record with creators and a licence: up to three creators (then "et al."),
/// a slash and the licence, e.g. "Writer, A.; Writer, B. / CC BY 4.0". Names are joined with
/// "; ", since a name may itself hold a comma ("Family, Given").
pub fn credit(creators: &[String], licence: Option<&str>) -> Option<String> {
    let names = match creators {
        [] => None,
        [a] => Some(a.clone()),
        [a, b] => Some(format!("{a}; {b}")),
        [a, b, c] => Some(format!("{a}; {b}; {c}")),
        [a, b, c, ..] => Some(format!("{a}; {b}; {c} et al.")),
    };
    match (names, licence) {
        (Some(n), Some(l)) => Some(format!("{n} / {l}")),
        (n, l) => n.or_else(|| l.map(str::to_owned)),
    }
}

/// A detail as a fact string: text as it is, numbers and booleans written out, lists joined
/// with "; ". Objects are not facts.
pub fn fact(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(items) => super::list(&items.iter().filter_map(fact).collect::<Vec<_>>()),
        Value::Null | Value::Object(_) => None,
    }
}

/// The card types that happen at a time, and so carry `start` and `end`: a card with a past
/// start or end is final, so every other date is a fact.
pub const DATED: &[&str] = &[
    "Event",
    "Observation",
    "MedicalTrial",
    "BitcoinBlock",
    "BitcoinTransaction",
];

/// The document of a parsed card, checked the way the `official` route checks it: every
/// attribute is one of the source's facts, no detail is an object, the type is known, and the
/// document fits its bounds.
#[cfg(test)]
pub fn checked(source: &dyn super::Source, card: &Card) -> ResourceMeta {
    for (name, value) in &card.details {
        assert!(
            !value.is_object(),
            "{}: detail {name} is an object",
            source.name()
        );
    }
    let doc = card
        .clone()
        .into_doc(ResourceMeta::new(&card.id, source.name(), 60), "site");
    for name in doc.attributes.keys() {
        assert!(
            source.facts().contains(&name.as_str()),
            "{} lacks the fact {name}",
            source.name()
        );
    }
    assert_eq!(doc.type_name, card.type_name, "an unknown type");
    // A past start or end makes a card final (`scheduler::next`): only things that happen at a
    // time carry them; every other date is a fact.
    if !DATED.contains(&card.type_name) {
        assert!(
            card.start.is_none() && card.end.is_none(),
            "{}: a {} has no start or end",
            source.name(),
            card.type_name
        );
    }
    let bounded = doc.clone().bounded(super::attributes()).expect("bounded");
    assert_eq!(
        bounded.attributes.len(),
        doc.attributes.len(),
        "facts dropped"
    );
    assert_eq!(bounded.related, doc.related, "related entries dropped");
    assert_eq!(
        bounded.related_total, doc.related_total,
        "related totals changed"
    );
    bounded
}

/// The body of a response, `None` for a status that means "no such object", or an error.
pub fn body<'a>(status: u16, body: &'a [u8], missing: &[u16]) -> Result<Option<&'a [u8]>> {
    match status {
        200 => Ok(Some(body)),
        s if missing.contains(&s) => Ok(None),
        s => bail!("unexpected status {s}"),
    }
}

/// A JSON body, `None` for a "missing" status.
pub fn json(status: u16, raw: &[u8], missing: &[u16]) -> Result<Option<Value>> {
    match body(status, raw, missing)? {
        Some(raw) => Ok(Some(serde_json::from_slice(raw)?)),
        None => Ok(None),
    }
}

/// Characters kept as they are in a path segment or query value.
pub const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

pub fn encode(value: &str) -> String {
    utf8_percent_encode(value, COMPONENT).to_string()
}

/// A plain identifier: non-empty, at most `max` bytes, and only the characters `ok` allows.
pub fn plain(value: &str, max: usize, ok: impl Fn(char) -> bool) -> bool {
    !value.is_empty() && value.len() <= max && value.chars().all(ok)
}

/// The host of a web URL in lower case without a leading `www.`, and its non-empty path
/// segments (percent-decoding is left to the caller).
pub fn host_and_segments(uri: &str) -> Option<(String, Vec<String>)> {
    let url = web_url(uri)?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_owned();
    let segments = url
        .path_segments()?
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    Some((host, segments))
}

/// An integer given as a number or as a decimal string.
pub fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
/// A number given as a number or as a decimal string.
pub fn float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}
/// Plain text of an HTML fragment: tags dropped, entities decoded, scripts and styles skipped,
/// one line per block (`p`, `div`, `li`, `br`, …), white space collapsed. `None` when nothing
/// is left.
pub fn html_text(html: &str) -> Option<String> {
    use scraper::Node;
    const BLOCKS: &[&str] = &[
        "address",
        "blockquote",
        "br",
        "dd",
        "div",
        "dl",
        "dt",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "li",
        "ol",
        "p",
        "pre",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    ];
    // Iterative: a fragment's depth is the input's to decide, never the stack's. Each element
    // is entered (a block opens its line) and left (a block closes it).
    enum Step<N> {
        Enter(N),
        Leave,
    }
    let html = crate::resolver::html::fragment(html).ok()?;
    let mut out = String::new();
    let mut steps: Vec<Step<_>> = html
        .root_element()
        .children()
        .rev()
        .map(Step::Enter)
        .collect();
    while let Some(step) = steps.pop() {
        match step {
            Step::Leave => out.push('\n'),
            Step::Enter(node) => match node.value() {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) if !matches!(e.name(), "script" | "style") => {
                    if BLOCKS.contains(&e.name()) {
                        out.push('\n');
                        steps.push(Step::Leave);
                    }
                    steps.extend(node.children().rev().map(Step::Enter));
                }
                _ => {}
            },
        }
    }
    let lines: Vec<String> = out
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// A time from Unix seconds.
pub fn unix(v: &Value) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(int(v)?, 0).single()
}
/// A time from Unix milliseconds.
pub fn unix_ms(v: &Value) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(int(v)?).single()
}
/// A time from RFC 3339, or from a date (`YYYY-MM-DD`, read as midnight UTC).
pub fn time(v: &Value) -> Option<DateTime<Utc>> {
    let s = v.as_str()?.trim();
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight").and_utc())
        })
}

/// The Q-id of a Wikidata entity URL (`https://www.wikidata.org/wiki/Q…`), and nothing else.
pub fn wikidata_id(v: &Value) -> Option<String> {
    let (host, segments) = host_and_segments(v.as_str()?)?;
    let [kind, q] = segments.as_slice() else {
        return None;
    };
    let digits = q.strip_prefix('Q')?;
    (host == "wikidata.org"
        && matches!(kind.as_str(), "wiki" | "entity")
        && plain(digits, 12, |c| c.is_ascii_digit())
        && !digits.starts_with('0'))
    .then(|| q.clone())
}
/// First path segments that are a GitLab instance's own pages, never a namespace: whoever reads
/// GitLab project links (the GitLab source, the package lookups) skips them.
pub const GITLAB_RESERVED: &[&str] = &[
    "-",
    ".well-known",
    "abuse_reports",
    "admin",
    "api",
    "assets",
    "dashboard",
    "explore",
    "groups",
    "help",
    "import",
    "jwt",
    "oauth",
    "profile",
    "projects",
    "public",
    "s",
    "search",
    "sitemap",
    "snippets",
    "uploads",
    "users",
    "v2",
];
/// First path segments that are GitHub's own pages, never an owner: whoever reads github.com
/// repository links (the GitHub source, the package lookups) skips them.
pub const GITHUB_RESERVED: &[&str] = &[
    "about",
    "apps",
    "collections",
    "contact",
    "customer-stories",
    "enterprise",
    "events",
    "explore",
    "features",
    "login",
    "marketplace",
    "new",
    "notifications",
    "orgs",
    "organizations",
    "pricing",
    "pulls",
    "issues",
    "search",
    "security",
    "settings",
    "site",
    "sponsors",
    "topics",
    "trending",
    "users",
];
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wikidata_ids() {
        let q = |s: &str| wikidata_id(&serde_json::json!(s));
        assert_eq!(
            q("https://www.wikidata.org/wiki/Q18689458").as_deref(),
            Some("Q18689458")
        );
        assert_eq!(
            q("http://www.wikidata.org/entity/Q5582").as_deref(),
            Some("Q5582")
        );
        for bad in [
            "",
            "https://www.wikidata.org/wiki/P31",
            "https://www.wikidata.org/wiki/Q",
            "https://www.wikidata.org/wiki/Q01",
            "https://www.wikidata.org/wiki/Q1/x",
            "https://example.org/wiki/Q5582",
        ] {
            assert_eq!(q(bad), None, "{bad}");
        }
    }

    use serde_json::json;

    #[test]
    fn statuses() {
        assert_eq!(body(200, b"x", &[404]).unwrap(), Some(&b"x"[..]));
        assert_eq!(body(404, b"x", &[404]).unwrap(), None);
        assert!(body(429, b"x", &[404]).is_err());
        assert!(body(500, b"x", &[404]).is_err());
        assert!(json(200, b"{", &[404]).is_err());
    }

    #[test]
    fn values() {
        assert_eq!(int(&json!("36108321892")), Some(36108321892));
        assert_eq!(int(&json!(7)), Some(7));
        assert_eq!(float(&json!("1.5")), Some(1.5));
        assert_eq!(text(&json!("  ")), None);
        assert_eq!(
            unix(&json!(1503539857)).unwrap().to_rfc3339(),
            "2017-08-24T01:57:37+00:00"
        );
        assert_eq!(
            time(&json!("2023-10-13T09:32:44.000Z"))
                .unwrap()
                .to_rfc3339(),
            "2023-10-13T09:32:44+00:00"
        );
        assert_eq!(
            time(&json!("2020-04-30")).unwrap().to_rfc3339(),
            "2020-04-30T00:00:00+00:00"
        );
        assert_eq!(Point::new(0.0, 0.0), None);
        assert_eq!(Point::new(91.0, 0.0), None);
    }

    #[test]
    fn credits() {
        let names = |n: &[&str]| n.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(credit(&[], None), None);
        assert_eq!(credit(&[], Some("CC0 1.0")).as_deref(), Some("CC0 1.0"));
        assert_eq!(credit(&names(&["A"]), None).as_deref(), Some("A"));
        assert_eq!(
            credit(&names(&["Writer, A.", "Writer, B."]), Some("CC BY 4.0")).as_deref(),
            Some("Writer, A.; Writer, B. / CC BY 4.0")
        );
        assert_eq!(
            credit(&names(&["A", "B", "C", "D"]), Some("CC BY 4.0")).as_deref(),
            Some("A; B; C et al. / CC BY 4.0")
        );
    }

    #[test]
    fn card_skips_empty_details_and_self_links() {
        let mut card = Card::new("Thing", "x:1".into(), "https://a/1".into());
        card.detail("empty", "");
        card.detail("none", Value::Null);
        card.detail("n", 3);
        card.link(Link::new("A", "https://a/1", "x"));
        assert_eq!(card.details.keys().collect::<Vec<_>>(), [&"n"]);
        assert!(card.also_on.is_empty());
    }

    #[test]
    fn html_to_text() {
        assert_eq!(
            html_text("<p>A&nbsp;<em>B</em> &amp; C</p><p>D<br>E</p><script>x</script>").as_deref(),
            Some("A B & C\nD\nE")
        );
        assert_eq!(html_text(" <span> </span> "), None);
        assert_eq!(html_text("plain"), Some("plain".into()));
    }

    #[test]
    fn urls() {
        assert_eq!(encode("a/b c"), "a%2Fb%20c");
        let (host, segments) = host_and_segments("https://WWW.Example.org/a//b/").unwrap();
        assert_eq!(
            (host.as_str(), segments),
            ("example.org", vec!["a".into(), "b".into()])
        );
        assert!(host_and_segments("ftp://example.org/a").is_none());
        // A port, a user name or a password is never a service's own link form.
        for uri in [
            "https://example.org:8443/a",
            "https://user@example.org/a",
            "https://user:secret@example.org/a",
        ] {
            assert!(host_and_segments(uri).is_none(), "{uri}");
        }
        assert!(host_and_segments("https://example.org:443/a").is_some());
    }
}
