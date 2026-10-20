//! Cards read from a service's own keyless API instead of its HTML page: exact where the page is
//! bot-walled, thin or moved.
//!
//! A source is one module, `sources/<name>.rs`, whose unit struct implements [`Source`] and is
//! listed once in [`REGISTRY`]. Its documentation is `docs/sources/<name>.md` (with a line in
//! `docs/sources/README.md`), its recorded fixtures are in `tests/fixtures/sources/<name>/`, its
//! switch is `[sources.<name>] enabled` and its OpenAPI examples come from
//! [`Source::examples`].
//!
//! Links a source reads go to the `official` card route. A switched-off source's links get
//! ordinary web cards. A card records the source that answered (`details.from`) and the fields
//! its API names (`details.facts`).
use crate::{
    config::Config,
    model::{ResourceMeta, Status},
    resolver::egress::{Egress, FetchOptions, Fetched},
};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

pub mod artic;
pub mod bluesky;
pub mod cleveland;
pub mod clinicaltrials;
pub mod commons;
pub mod courtlistener;
pub mod discourse;
pub mod docker_hub;
pub mod ecfr;
pub mod esplora;
pub mod eur_lex;
pub mod federal_register;
pub mod fedlex;
pub mod feed;
pub mod figshare;
pub mod forgejo;
pub mod gbif;
pub mod ghost;
pub mod gitlab;
pub mod gleif;
pub mod govinfo;
pub mod hacker_news;
pub mod huggingface;
pub mod inaturalist;
pub mod internet_archive;
pub mod kit;
pub mod legislation_gov_uk;
pub mod lobsters;
pub mod mastodon;
pub mod media_records;
pub mod mempool;
pub mod met;
pub mod pdb;
pub mod pubchem;
pub mod public_inbox;
pub mod ror;
pub mod sec;
pub mod stacker_news;
pub mod swiss_parliament;
pub mod uk_parliament_bills;
pub mod uniprot;
pub mod usgs;
pub mod wordpress;
pub mod zefix;
pub mod zenodo;

/// Every source, one line each, in alphabetical order of name.
#[rustfmt::skip]
pub static REGISTRY: &[&dyn Source] = &[
    &artic::Artic,
    &bluesky::Bluesky,
    &cleveland::Cleveland,
    &clinicaltrials::ClinicalTrials,
    &commons::Commons,
    &courtlistener::Courtlistener,
    &discourse::Discourse,
    &docker_hub::DockerHub,
    &ecfr::Ecfr,
    &eur_lex::EurLex,
    &federal_register::FederalRegister,
    &fedlex::Fedlex,
    &feed::Feed,
    &figshare::Figshare,
    &forgejo::Forgejo,
    &gbif::Gbif,
    &ghost::Ghost,
    &gitlab::Gitlab,
    &gleif::Gleif,
    &govinfo::Govinfo,
    &hacker_news::HackerNews,
    &huggingface::HuggingFace,
    &inaturalist::Inaturalist,
    &internet_archive::InternetArchive,
    &legislation_gov_uk::LegislationGovUk,
    &lobsters::Lobsters,
    &mastodon::Mastodon,
    &media_records::MediaRecords,
    &mempool::Mempool,
    &met::Met,
    &pdb::Pdb,
    &pubchem::Pubchem,
    &public_inbox::PublicInbox,
    &ror::Ror,
    &sec::Sec,
    &stacker_news::StackerNews,
    &swiss_parliament::SwissParliament,
    &uk_parliament_bills::UkParliamentBills,
    &uniprot::Uniprot,
    &usgs::Usgs,
    &wordpress::Wordpress,
    &zefix::Zefix,
    &zenodo::Zenodo,
];

/// One official API.
#[async_trait]
pub trait Source: Send + Sync {
    /// The name in `[sources.<name>]`, `details.from`, the stored document's `source`, the
    /// docs file and the fixture directory, e.g. `federal_register`.
    fn name(&self) -> &'static str;
    /// Whether this source reads `uri`, from the link alone (no I/O).
    fn reads(&self, uri: &str) -> bool;
    /// The one URI every other spelling of the same object is an alias of, from the link alone,
    /// when it differs from `uri` (as normalized by `model::identity`).
    fn alias_root(&self, _uri: &str) -> Option<String> {
        None
    }
    /// A link in this source's own identifier scheme that can never name an object (a final
    /// `missing`, never fetched again).
    fn invalid(&self, _uri: &str) -> bool {
        false
    }
    /// The attribute keys its cards publish as `details.facts`.
    fn facts(&self) -> &'static [&'static str] {
        &[]
    }
    /// The service's published limits per host, as `(host, requests_per_second,
    /// max_in_flight)`; a windowed limit is given as its average rate (60 per minute → 1.0).
    /// The rate becomes the host's default (`limits.host_rates` overrides it). Egress never has
    /// more than one request in flight per host, which meets any `max_in_flight`.
    fn budgets(&self) -> &'static [Budget] {
        &[]
    }
    /// Whether a card quotes people's own posts (a comment, a social post): social content keeps
    /// a 200-character excerpt, refreshes within the hour and is purged once the object is gone.
    fn social(&self, _doc: &ResourceMeta) -> bool {
        false
    }
    /// Plain example links with the card type they resolve to: the OpenAPI examples, also
    /// listed in `docs/sources/<name>.md`.
    fn examples(&self) -> &'static [Example];
    /// The card of `uri`, a link this source reads.
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta>;
    /// Whether this source improves web cards of pages its service serves (`page`), found by
    /// what a fetched page says about itself rather than by its link. Its examples are then
    /// links of such pages, read by the web route.
    fn hooks_pages(&self) -> bool {
        false
    }
    /// A page hook: the card of a fetched page (`doc`, from its own metadata) improved from this
    /// source's service, or `None` when the page is not the service's. It runs on the web route
    /// with a short deadline; an error leaves the page's own card.
    async fn page(
        &self,
        _cx: &Context<'_>,
        _uri: &str,
        _page: &Fetched,
        _doc: &ResourceMeta,
    ) -> Result<Option<ResourceMeta>> {
        Ok(None)
    }
    /// Whether this source answers for pages behind an access wall (`walled`).
    fn hooks_walls(&self) -> bool {
        false
    }
    /// A wall hook: the card of `uri` when its page answered with an access wall (a challenge,
    /// 401 or 403), from what its site publishes elsewhere, or `None`. It runs on the web route
    /// with a short deadline; an error leaves the wall.
    async fn walled(&self, _cx: &Context<'_>, _uri: &str) -> Result<Option<ResourceMeta>> {
        Ok(None)
    }
}

/// The longest a wall hook may take, within the web fetch's own deadline.
pub const WALL_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// Run the switched-on wall hooks for a walled page; the first card wins.
pub async fn wall_hooks(
    egress: &Egress,
    uri: &str,
    ttl: u64,
    deadline: tokio::time::Instant,
) -> Option<ResourceMeta> {
    let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner()).clone();
    let hooks: Vec<&dyn Source> = REGISTRY
        .iter()
        .copied()
        .filter(|s| s.hooks_walls() && switches.as_ref().is_none_or(|c| c.enabled(s.name())))
        .collect();
    let cx = Context { egress, ttl };
    for hook in hooks {
        let left = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .saturating_sub(std::time::Duration::from_millis(25))
            .min(WALL_HOOK_TIMEOUT);
        match tokio::time::timeout(left, hook.walled(&cx, uri)).await {
            Ok(Ok(Some(doc))) => {
                crate::telemetry::discovery("wall_hook", "applied");
                return doc.bounded(attributes()).ok();
            }
            Ok(Ok(None)) => crate::telemetry::discovery("wall_hook", "none"),
            Ok(Err(error)) => {
                tracing::debug!(source = hook.name(), uri, error = %format!("{error:#}"), "wall hook failed");
                crate::telemetry::discovery("wall_hook", "failed");
            }
            Err(_) => {
                tracing::debug!(source = hook.name(), uri, "wall hook timed out");
                crate::telemetry::discovery("wall_hook", "failed");
            }
        }
    }
    None
}

/// The longest a page hook may take, within the web fetch's own deadline.
pub const PAGE_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Run the switched-on page hooks on a fetched page's card; the first that answers wins.
pub async fn page_hooks(
    egress: &Egress,
    uri: &str,
    page: &Fetched,
    doc: ResourceMeta,
    deadline: tokio::time::Instant,
) -> ResourceMeta {
    // A card ActivityPub or a podcast feed already made is not a plain page's to improve.
    if doc.status != Status::Ok
        || page.status != 200
        || !page.content_type.contains("html")
        || matches!(doc.source.as_str(), "activitypub" | "media")
    {
        return doc;
    }
    let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner()).clone();
    page_hooks_with(egress, uri, page, doc, deadline, switches.as_ref()).await
}
pub(crate) async fn page_hooks_with(
    egress: &Egress,
    uri: &str,
    page: &Fetched,
    doc: ResourceMeta,
    deadline: tokio::time::Instant,
    switches: Option<&SourcesConfig>,
) -> ResourceMeta {
    let hooks = REGISTRY
        .iter()
        .copied()
        .filter(|s| s.hooks_pages() && switches.is_none_or(|c| c.enabled(s.name())));
    let cx = Context {
        egress,
        ttl: doc.ttl_secs,
    };
    for hook in hooks {
        let left = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .saturating_sub(std::time::Duration::from_millis(25))
            .min(PAGE_HOOK_TIMEOUT);
        match tokio::time::timeout(left, hook.page(&cx, uri, page, &doc)).await {
            Ok(Ok(Some(improved))) => {
                crate::telemetry::discovery("page_hook", "applied");
                return improved;
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                tracing::debug!(source = hook.name(), uri, error = %format!("{error:#}"), "page hook failed");
                crate::telemetry::discovery("page_hook", "failed");
            }
            Err(_) => {
                tracing::debug!(source = hook.name(), uri, "page hook timed out");
                crate::telemetry::discovery("page_hook", "failed");
            }
        }
    }
    doc
}

/// `(host, requests_per_second, max_in_flight)`.
pub type Budget = (&'static str, f64, Option<usize>);

/// Request headers a source may not set: they belong to the connection, carry credentials or
/// replace the User-Agent.
const REFUSED_HEADERS: &[&str] = &[
    "authorization",
    "connection",
    "cookie",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
];
/// The most extra headers one source request may carry.
pub const MAX_HEADERS: usize = 4;

/// An example link and the card type it resolves to.
#[derive(Clone, Copy, Debug)]
pub struct Example {
    pub uri: &'static str,
    pub card_type: &'static str,
}

/// What a source's `resolve` works with.
pub struct Context<'a> {
    pub egress: &'a Egress,
    /// The card's lifetime in seconds (`limits.resolver_ttl_secs`).
    pub ttl: u64,
}
impl Context<'_> {
    /// The operator's contact address (`http.contact`). A source whose service refuses requests
    /// without one answers `RetryLater` (class `disabled`) while it is empty.
    pub fn contact(&self) -> Option<&str> {
        self.egress.contact()
    }
    /// A GET (or, with `options.json_body`, a POST) through the egress policy. Rate limits and
    /// server errors become `RetryLater`; other statuses are the caller's to read. At most
    /// `MAX_HEADERS` extra headers (`options.headers`, e.g. a service's own identification
    /// header); connection, credential and User-Agent headers are refused.
    pub async fn fetch(&self, url: &str, options: &FetchOptions) -> Result<Fetched> {
        check_headers(&options.headers)?;
        let fetched = self.egress.fetch_with(url, options).await?;
        retry(&fetched)?;
        Ok(fetched)
    }
    /// A new card of `source` for `uri`.
    pub fn card(&self, uri: &str, source: &dyn Source) -> ResourceMeta {
        ResourceMeta::new(uri, source.name(), self.ttl)
    }
    /// The card of a link whose object the source says does not exist.
    pub fn missing(&self, uri: &str, source: &dyn Source) -> ResourceMeta {
        let mut doc = self.card(uri, source);
        doc.status = Status::Missing;
        doc
    }
}

fn check_headers(headers: &[(&'static str, String)]) -> Result<()> {
    anyhow::ensure!(headers.len() <= MAX_HEADERS, "too many request headers");
    for (name, _) in headers {
        anyhow::ensure!(
            !REFUSED_HEADERS.contains(&name.to_ascii_lowercase().as_str()),
            "request header {name} refused"
        );
    }
    Ok(())
}

/// Whether an error is this daemon's own host budget (the slot is further away than a fetch
/// may wait), which says nothing about the host's answer.
pub fn own_rate_limit(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::resolver::RetryLater>()
        .is_some_and(|r| r.class == "rate_limited")
}

/// A source looked at a link it claimed from its shape alone (a forum path, a post path on any
/// host) and found the site is not its service: the link gets an ordinary web card instead.
#[derive(Debug)]
pub struct Declined;
impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not this source's site")
    }
}
impl std::error::Error for Declined {}

/// What a source learned by asking a host (whether it runs that service), kept for a day so the
/// question is not asked for every link. Bounded: the oldest answers go first.
#[derive(Default)]
pub struct HostMemo(std::sync::Mutex<BTreeMap<String, (bool, std::time::Instant)>>);
impl HostMemo {
    const TTL: std::time::Duration = std::time::Duration::from_secs(86_400);
    const CAP: usize = 10_000;
    pub fn get(&self, host: &str) -> Option<bool> {
        let memo = self.0.lock().unwrap_or_else(|e| e.into_inner());
        memo.get(host)
            .filter(|(_, at)| at.elapsed() < Self::TTL)
            .map(|(is, _)| *is)
    }
    pub fn set(&self, host: &str, is: bool) {
        let mut memo = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if memo.len() >= Self::CAP {
            if let Some(oldest) = memo
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(h, _)| h.clone())
            {
                memo.remove(&oldest);
            }
        }
        memo.insert(host.to_owned(), (is, std::time::Instant::now()));
    }
}

/// A source's switch.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Switch {
    pub enabled: bool,
}
impl Default for Switch {
    fn default() -> Self {
        Self { enabled: true }
    }
}
/// `[sources.<name>]`: every source is on unless its table says `enabled = false`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SourcesConfig(pub BTreeMap<String, Switch>);
impl SourcesConfig {
    pub fn enabled(&self, name: &str) -> bool {
        self.0.get(name).is_none_or(|s| s.enabled)
    }
    pub fn validate(&self) -> Result<()> {
        for name in self.0.keys() {
            anyhow::ensure!(by_name(name).is_some(), "unknown source [sources.{name}]");
        }
        Ok(())
    }
}

pub fn by_name(name: &str) -> Option<&'static dyn Source> {
    REGISTRY.iter().copied().find(|s| s.name() == name)
}
/// The source that reads `uri`, switched on or not.
pub fn reading(uri: &str) -> Option<&'static dyn Source> {
    REGISTRY.iter().copied().find(|s| s.reads(uri))
}
/// The switched-on source that reads `uri`.
pub fn enabled_source(uri: &str, config: &Config) -> Option<&'static dyn Source> {
    reading(uri).filter(|s| config.sources.enabled(s.name()))
}
/// The switches the daemon started with (`configure`); every source is on until then.
static SWITCHES: std::sync::RwLock<Option<SourcesConfig>> = std::sync::RwLock::new(None);

/// Record the daemon's switches, so a switched-off source's address rules no longer merge links
/// (its links then get ordinary web cards, as its routes do).
pub fn configure(config: &SourcesConfig) {
    *SWITCHES.write().unwrap_or_else(|e| e.into_inner()) = Some(config.clone());
}

/// The static alias root of a source's spelling, from the link alone, while that source is on.
pub fn alias_root(uri: &str) -> Option<String> {
    let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner());
    alias_root_with(uri, switches.as_ref())
}
fn alias_root_with(uri: &str, switches: Option<&SourcesConfig>) -> Option<String> {
    REGISTRY
        .iter()
        .filter(|s| switches.is_none_or(|c| c.enabled(s.name())))
        .find_map(|s| s.alias_root(uri))
}
/// A link no source could ever resolve (a malformed identifier of a source's own scheme).
pub fn invalid(uri: &str) -> bool {
    REGISTRY.iter().any(|s| s.invalid(uri))
}
/// Every source's default host rates, for hosts the operator did not set.
pub fn budgets() -> impl Iterator<Item = (&'static str, f64)> {
    REGISTRY
        .iter()
        .flat_map(|s| s.budgets().iter().map(|(host, rate, _)| (*host, *rate)))
}

/// Every attribute a card on the `official` or web route may store: the web keys and every
/// source's facts.
pub fn attributes() -> &'static [&'static str] {
    static ALL: OnceLock<Vec<&'static str>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut all: Vec<&str> = crate::resolver::web::ATTRIBUTES.to_vec();
        for key in REGISTRY.iter().flat_map(|s| s.facts().iter().copied()) {
            if !all.contains(&key) {
                all.push(key);
            }
        }
        all
    })
}

/// `details.from`: the source a card's metadata came from, when not the page itself.
pub fn from(doc: &ResourceMeta) -> Option<&'static str> {
    by_name(&doc.source).map(|s| s.name())
}
/// `details.facts`: a source card's published attributes.
pub fn facts(doc: &ResourceMeta) -> Option<BTreeMap<String, String>> {
    let source = by_name(&doc.source)?;
    let facts: BTreeMap<String, String> = doc
        .attributes
        .iter()
        .filter(|(k, v)| source.facts().contains(&k.as_str()) && !v.trim().is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!facts.is_empty()).then_some(facts)
}

/// Resolve a link on the `official` route. `None` when no switched-on source reads it (it was
/// queued before its source was switched off) or its source declined it: the caller falls back
/// to the web fetch.
pub async fn resolve(
    egress: &Egress,
    config: &Config,
    uri: &str,
    ttl: u64,
) -> Result<Option<ResourceMeta>> {
    let Some(source) = enabled_source(uri, config) else {
        return Ok(None);
    };
    let doc = match source.resolve(&Context { egress, ttl }, uri).await {
        Ok(doc) => doc,
        Err(error) if error.is::<Declined>() => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(doc.bounded(attributes())?))
}

/// Rate limits and timeouts (408, 429: class `rate_limited`) and server errors (class `upstream`):
/// retry later, after at least a minute.
pub(crate) fn retry(f: &Fetched) -> Result<()> {
    let class = match f.status {
        408 | 429 => "rate_limited",
        s if s >= 500 => "upstream",
        _ => return Ok(()),
    };
    Err(crate::resolver::RetryLater {
        class,
        seconds: f.retry_after_secs.max(60),
    }
    .into())
}

/// Text with every e-mail address removed (`name@host.tld`, also in `<…>` or after
/// `mailto:`). A backstop for excerpts of mail, commits and release notes: a card never
/// publishes an address.
pub fn without_addresses(text: &str) -> String {
    let address = |token: &str| {
        let token = token
            .trim_matches(|c: char| "<>()[]{},;:\"'".contains(c))
            .trim_start_matches("mailto:");
        token.rsplit_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        })
    };
    text.split_whitespace()
        .filter(|token| !address(token))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Markdown as plain text for a 200-character excerpt: images, heading and quote marks, emphasis
/// and code marks dropped, links kept as their text.
pub fn markdown_excerpt(markdown: &str) -> String {
    // 2 KB is ample for 200 characters, and bounds the work on any input.
    let mut end = markdown.len().min(2048);
    while !markdown.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::new();
    for line in markdown[..end].lines() {
        let line = line.trim().trim_start_matches(['#', '>']).trim();
        if line.is_empty() || line.starts_with("![") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&markdown_inline(line));
        if out.len() > 400 {
            break;
        }
    }
    let out = without_addresses(&out);
    let mut cut = out.chars().take(200).collect::<String>();
    if cut.len() < out.len() {
        cut.push('…');
    }
    cut
}

/// `[text](url)` as `text`, without `**`, `__` and backticks.
fn markdown_inline(line: &str) -> String {
    let mut line = line.replace("**", "").replace("__", "").replace('`', "");
    while let Some(mid) = line.find("](") {
        let (Some(open), Some(close)) = (line[..mid].rfind('['), line[mid..].find(')')) else {
            break;
        };
        let text = line[open + 1..mid].to_owned();
        line.replace_range(open..mid + close + 1, &text);
    }
    line
}

/// A trimmed, non-empty JSON string.
pub fn text(v: &serde_json::Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
/// The trimmed, non-empty strings of a JSON array.
pub fn texts(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(text)
        .collect()
}
/// An `https` or `http` URL with a host, as given.
pub fn web_url(value: &str) -> Option<url::Url> {
    let url = url::Url::parse(value).ok()?;
    (matches!(url.scheme(), "https" | "http") && url.host_str().is_some()).then_some(url)
}
/// A SPARQL query as a GET (`{endpoint}?query=…`), for `Context::fetch` with `SPARQL_JSON`.
pub fn sparql_url(endpoint: &str, query: &str) -> String {
    let mut url = url::Url::parse(endpoint).expect("SPARQL endpoint");
    url.query_pairs_mut().append_pair("query", query);
    url.to_string()
}
/// The `Accept` header and content type of SPARQL JSON results.
pub const SPARQL_JSON: &str = "application/sparql-results+json";
/// One SPARQL result row: each bound variable's `value`, with its language tag if it has one.
pub type Row = BTreeMap<String, (String, Option<String>)>;
/// The rows of a SPARQL JSON result (`results.bindings`).
pub fn sparql_rows(body: &[u8]) -> Result<Vec<Row>> {
    let v: serde_json::Value = serde_json::from_slice(body)?;
    let bindings = v["results"]["bindings"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("not SPARQL results"))?;
    Ok(bindings
        .iter()
        .filter_map(|b| b.as_object())
        .map(|b| {
            b.iter()
                .filter_map(|(var, cell)| {
                    let value = cell["value"].as_str()?.to_owned();
                    let lang = cell["xml:lang"].as_str().map(str::to_owned);
                    Some((var.clone(), (value, lang)))
                })
                .collect()
        })
        .collect())
}
/// A SPARQL string literal's content, escaped (`"`, `\` and line breaks); identifiers are
/// validated first, this is a second guard.
pub fn sparql_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// A list fact: the values joined with "; ".
pub fn list(values: &[String]) -> Option<String> {
    let joined = values
        .iter()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Recorded responses: `tests/fixtures/sources/<source>/` (see its `provenance.json`).
    use std::io::Read;

    pub fn read(source: &str, file: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/sources/{source}/{file}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        if file.ends_with(".gz") {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw.as_slice())
                .read_to_end(&mut out)
                .expect("gzip fixture");
            out
        } else {
            raw
        }
    }

    /// An egress that answers recorded responses by exact URL (anything else is a 404) and
    /// remembers the URLs asked, in order. A redirect's body is its `Location`. Pair it with `#[tokio::test(start_paused = true)]`:
    /// the per-host spacing then costs no time.
    pub fn egress(
        routes: Vec<(&str, u16, &'static str, Vec<u8>)>,
    ) -> (crate::resolver::egress::Egress, std::sync::Arc<Recorded>) {
        let recorded = std::sync::Arc::new(Recorded {
            routes: routes
                .into_iter()
                .map(|(url, status, kind, body)| (url.to_owned(), (status, kind, body)))
                .collect(),
            asked: Default::default(),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(PublicDns),
            recorded.clone(),
        );
        (egress, recorded)
    }

    pub struct Recorded {
        routes: std::collections::BTreeMap<String, (u16, &'static str, Vec<u8>)>,
        pub asked: std::sync::Mutex<Vec<String>>,
    }
    impl Recorded {
        pub fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }
    #[async_trait::async_trait]
    impl crate::resolver::egress::Transport for Recorded {
        async fn get(
            &self,
            url: &url::Url,
            _: &[std::net::SocketAddr],
        ) -> anyhow::Result<crate::resolver::egress::Fetched> {
            self.asked.lock().unwrap().push(url.to_string());
            let (status, kind, body) = self.routes.get(url.as_str()).cloned().unwrap_or((
                404,
                "text/html",
                b"<html>not found</html>".to_vec(),
            ));
            let location = (300..400)
                .contains(&status)
                .then(|| String::from_utf8_lossy(&body).into_owned());
            Ok(crate::resolver::egress::Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Default::default(),
                url: url.clone(),
                content_type: kind.into(),
                status,
                retry_after_secs: 60,
                location,
                body,
            })
        }
    }
    /// A fetched page, as the web route has it.
    pub fn page(
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        link: Option<&str>,
    ) -> crate::resolver::egress::Fetched {
        crate::resolver::egress::Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: crate::resolver::egress::Validators {
                link: link.map(str::to_owned),
                ..Default::default()
            },
            url: url::Url::parse(url).unwrap(),
            content_type: content_type.into(),
            status: 200,
            retry_after_secs: 60,
            location: None,
            body,
        }
    }
    /// The recorded `Link` headers of a fixture (`provenance.json`, key `link`).
    pub fn link(source: &str, name: &str) -> String {
        let provenance: serde_json::Value =
            serde_json::from_slice(&read(source, "provenance.json")).unwrap();
        provenance[name]["link"].as_str().unwrap().to_owned()
    }
    struct PublicDns;
    #[async_trait::async_trait]
    impl crate::resolver::egress::Dns for PublicDns {
        async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<std::net::SocketAddr>> {
            Ok(vec![std::net::SocketAddr::new(
                [93, 184, 216, 34].into(),
                port,
            )])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn repo(path: &str) -> String {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn the_registry_is_alphabetical_and_each_source_is_complete() {
        let names: Vec<&str> = REGISTRY.iter().map(|s| s.name()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names, sorted,
            "REGISTRY must be alphabetical, one entry per source"
        );
        let index = repo("docs/sources/README.md");
        let example = repo("pinakes.example.toml");
        let mut previous = 0;
        for source in REGISTRY {
            let name = source.name();
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{name}"
            );
            let page = repo(&format!("docs/sources/{name}.md"));
            assert!(
                index.contains(&format!("[`{name}`]({name}.md)")),
                "docs/sources/README.md lacks {name}"
            );
            let table = example
                .find(&format!("[sources.{name}]\nenabled = true\n"))
                .unwrap_or_else(|| panic!("pinakes.example.toml lacks [sources.{name}]"));
            assert!(
                table > previous,
                "pinakes.example.toml: [sources.*] out of order"
            );
            previous = table;
            assert!(!source.examples().is_empty(), "{name} has no example");
            for example in source.examples() {
                assert!(
                    source.reads(example.uri) || source.hooks_pages() || source.hooks_walls(),
                    "{name} does not read {}",
                    example.uri
                );
                assert!(
                    !crate::model::type_path(example.card_type).is_empty(),
                    "{}",
                    example.card_type
                );
                assert!(
                    page.contains(example.uri),
                    "docs/sources/{name}.md lacks {}",
                    example.uri
                );
            }
            for (host, rate, in_flight) in source.budgets() {
                assert!(rate.is_finite() && *rate > 0.0 && !host.is_empty());
                assert!(in_flight.is_none_or(|n| n >= 1));
            }
        }
    }

    /// Link-derived text may hold any character: no address check may slice a string inside one.
    #[test]
    fn a_multibyte_character_anywhere_in_a_link_never_panics() {
        let mut links: Vec<String> = vec![];
        for source in REGISTRY {
            links.extend(source.examples().iter().map(|e| e.uri.to_owned()));
            // Every address the source's page shows without a placeholder.
            let page = repo(&format!("docs/sources/{}.md", source.name()));
            for token in page.split('`').skip(1).step_by(2) {
                let token = token.split_whitespace().next().unwrap_or_default();
                let link = token.starts_with("http")
                    || token.split_once(':').is_some_and(|(s, r)| {
                        s.len() <= 4 && s.bytes().all(|b| b.is_ascii_lowercase()) && !r.is_empty()
                    });
                if link && !token.contains(['{', '…']) {
                    links.push(token.to_owned());
                }
            }
        }
        assert!(links.len() > 40, "{}", links.len());
        for link in &links {
            for at in (0..=link.len()).filter(|i| link.is_char_boundary(*i)) {
                for insert in ["é", "€", "😀", "\u{301}"] {
                    let uri = format!("{}{insert}{}", &link[..at], &link[at..]);
                    for source in REGISTRY {
                        source.reads(&uri);
                        source.alias_root(&uri);
                        source.invalid(&uri);
                    }
                    crate::alias::root(&uri);
                    crate::resolver::classify(&uri);
                }
            }
        }
    }

    #[test]
    fn a_switched_off_source_merges_nothing() {
        let uri = "https://www.federalregister.gov/d/2026-18291";
        assert_eq!(alias_root_with(uri, None).as_deref(), Some("fr:2026-18291"));
        let mut off = SourcesConfig::default();
        assert_eq!(
            alias_root_with(uri, Some(&off)).as_deref(),
            Some("fr:2026-18291")
        );
        off.0
            .insert("federal_register".into(), Switch { enabled: false });
        assert_eq!(alias_root_with(uri, Some(&off)), None);
    }

    #[test]
    fn switches_and_names() {
        let mut config = Config::default();
        let uri = federal_register::FederalRegister.examples()[0].uri;
        assert_eq!(
            enabled_source(uri, &config).map(|s| s.name()),
            Some("federal_register")
        );
        config
            .sources
            .0
            .insert("federal_register".into(), Switch { enabled: false });
        assert!(enabled_source(uri, &config).is_none());
        assert!(reading(uri).is_some());
        assert!(config.sources.validate().is_ok());
        config
            .sources
            .0
            .insert("nonesuch".into(), Switch::default());
        assert!(config.sources.validate().is_err());
    }

    #[test]
    fn sparql() {
        assert_eq!(
            sparql_url(
                "https://example.org/sparql",
                "SELECT ?a WHERE { ?a ?b \"c\" }"
            ),
            "https://example.org/sparql?query=SELECT+%3Fa+WHERE+%7B+%3Fa+%3Fb+%22c%22+%7D"
        );
        let body = br#"{"head":{"vars":["t","n"]},"results":{"bindings":[
            {"t":{"type":"literal","value":"Titel","xml:lang":"de"},"n":{"type":"literal","value":"1"}},
            {"t":{"type":"uri","value":"https://x/"}}]}}"#;
        let rows = sparql_rows(body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["t"], ("Titel".to_owned(), Some("de".to_owned())));
        assert_eq!(rows[0]["n"].0, "1");
        assert!(!rows[1].contains_key("n"));
        assert!(sparql_rows(b"{}").is_err());
        assert_eq!(sparql_literal("a\"b\\"), "a\\\"b\\\\");
        assert_eq!(sparql_literal("a\nb\rc"), "a\\nb\\rc");
        assert_eq!(
            markdown_excerpt("## Release highlights\n\n- **Filter Gallery**: see [the docs](https://x.example/a) and `code`\n![img](i.png)"),
            "Release highlights - Filter Gallery: see the docs and code"
        );
        assert_eq!(markdown_excerpt("a ](b) [c"), "a ](b) [c");
        // Hostile input costs no more than its first 2 KB.
        for hostile in [
            "[]()".repeat(262_144),
            "[a](b)".repeat(174_763),
            "é".repeat(600_000),
        ] {
            let start = std::time::Instant::now();
            let excerpt = markdown_excerpt(&hostile);
            assert!(
                start.elapsed() < std::time::Duration::from_millis(200),
                "{:?}",
                start.elapsed()
            );
            assert!(excerpt.chars().count() <= 201);
        }
        assert_eq!(
            without_addresses("Mail <ann@example.org> or mailto:bob@x.org, not @user@host or a@b"),
            "Mail or not @user@host or a@b"
        );
    }

    #[test]
    fn host_memo() {
        let memo = HostMemo::default();
        assert_eq!(memo.get("forum.example"), None);
        memo.set("forum.example", true);
        memo.set("blog.example", false);
        assert_eq!(memo.get("forum.example"), Some(true));
        assert_eq!(memo.get("blog.example"), Some(false));
        let error = anyhow::Error::from(Declined);
        assert!(error.is::<Declined>());
    }

    #[test]
    fn request_headers() {
        let ok = vec![("AIC-User-Agent", "pinakes".to_owned())];
        assert!(check_headers(&ok).is_ok());
        for name in [
            "Authorization",
            "cookie",
            "Host",
            "Connection",
            "User-Agent",
        ] {
            assert!(check_headers(&[(name, "x".to_owned())]).is_err(), "{name}");
        }
        let many: Vec<_> = (0..=MAX_HEADERS).map(|_| ("X-A", String::new())).collect();
        assert!(check_headers(&many).is_err());
    }

    #[test]
    fn attribution_reaches_the_card() {
        let mut doc = ResourceMeta::new("fr:2026-18291", "federal_register", 60);
        doc.set_type("Legislation");
        doc.attribution = Some("Source: an official register (CC BY 4.0)".into());
        let doc = doc.bounded(attributes()).unwrap();
        let row = serde_json::json!({
            "id": crate::model::identity("fr:2026-18291").unwrap().0,
            "uri": "fr:2026-18291",
            "status": "ok",
            "type": "Legislation",
            "doc": doc,
        });
        let card = crate::api::card(&row).unwrap();
        assert_eq!(
            card.attribution.as_deref(),
            Some("Source: an official register (CC BY 4.0)")
        );
    }

    #[test]
    fn facts_only_on_source_cards() {
        let mut doc = ResourceMeta::new("fr:2026-18291", "federal_register", 60);
        doc.attributes
            .insert("citation".into(), "91 FR 57392".into());
        doc.attributes.insert("author".into(), "x".into());
        doc.attributes.insert("docket_ids".into(), " ".into());
        assert_eq!(from(&doc), Some("federal_register"));
        let facts = facts(&doc).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts["citation"], "91 FR 57392");
        doc.source = "web".into();
        assert_eq!((from(&doc), self::facts(&doc)), (None, None));
        assert!(attributes().contains(&"author") && attributes().contains(&"citation"));
        let values = ["a ".to_owned(), String::new(), "b".to_owned()];
        assert_eq!(list(&values).as_deref(), Some("a; b"));
        assert_eq!(list(&[]), None);
    }
}
