//! Cards read from a service's own keyless API instead of its HTML page: exact where the page is
//! bot-walled, thin or moved.
//!
//! A source is one module, `sources/<name>.rs`, whose unit struct implements [`Source`] and is
//! listed once in [`REGISTRY`] (with its `pub mod` line). Its documentation is
//! `docs/sources/<name>.md` (with a line in `docs/sources/README.md`), its recorded fixtures are
//! in `tests/fixtures/sources/<name>/`, its switch is `[sources.<name>] enabled`, its OpenAPI
//! examples come from [`Source::examples`], its mock end-to-end sample from [`Source::e2e`] and
//! its live one is `e2e/samples/<name>.json`. A source imports only shared code (`kit`,
//! `biodiversity`, the crate's shared modules), never another source, and nothing outside the
//! registry names it: removing it is deleting those files and lines
//! (`docs/sources/README.md#removing-a-source`).
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

pub mod app_store;
pub mod artic;
pub mod biodiversity;
pub mod bluesky;
pub mod cleveland;
pub mod clinicaltrials;
pub mod commons;
pub mod courtlistener;
pub mod discogs;
pub mod discourse;
pub mod docker_hub;
pub mod ecfr;
pub mod eur_lex;
pub mod fdroid;
pub mod federal_register;
pub mod fedlex;
pub mod feed;
pub mod figshare;
pub mod flathub;
pub mod forgejo;
pub mod fountain;
pub mod gbif;
pub mod ghost;
pub mod github;
pub mod gitlab;
pub mod gleif;
pub mod google_play;
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
pub mod podcast_index;
pub mod pubchem;
pub mod public_inbox;
pub mod ror;
pub mod sec;
pub mod snap_store;
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
    &app_store::AppStore,
    &artic::Artic,
    &bluesky::Bluesky,
    &cleveland::Cleveland,
    &clinicaltrials::ClinicalTrials,
    &commons::Commons,
    &courtlistener::Courtlistener,
    &discogs::Discogs,
    &discourse::Discourse,
    &docker_hub::DockerHub,
    &ecfr::Ecfr,
    &eur_lex::EurLex,
    &fdroid::FDroid,
    &federal_register::FederalRegister,
    &fedlex::Fedlex,
    &feed::Feed,
    &figshare::Figshare,
    &flathub::Flathub,
    &forgejo::Forgejo,
    &fountain::Fountain,
    &gbif::Gbif,
    &ghost::Ghost,
    &github::Github,
    &gitlab::Gitlab,
    &gleif::Gleif,
    &google_play::GooglePlay,
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
    &podcast_index::PodcastIndex,
    &pubchem::Pubchem,
    &public_inbox::PublicInbox,
    &ror::Ror,
    &sec::Sec,
    &snap_store::SnapStore,
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
    /// Whether this source reads its service with the operator's token (`[sources.<name>]
    /// token`): it is then on only with one, and its links get ordinary web cards without it.
    fn takes_token(&self) -> bool {
        false
    }
    /// A variable of its own (`PINAKES_NAME`) that fills its token when the config file and the
    /// `PINAKES__SOURCES__…` form leave it unset.
    fn token_variable(&self) -> Option<&'static str> {
        None
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
    /// Take what this source needs from the daemon's configuration at startup (a token another
    /// table holds, say).
    fn configure(&self, _config: &Config) {}
    /// What the mock end-to-end run tags for this source and the answers its service gives there
    /// (`examples/e2e_stack.rs`); `None` for a source that run leaves out.
    fn e2e(&self) -> Option<E2e> {
        None
    }
    /// A finished card of `uri` named from the title the also-on sources found since its page
    /// was read (`also_on::media::type_card`), for a page whose card this source names: whether
    /// it applied.
    fn apply_title(
        &self,
        _uri: &str,
        _title: &str,
        _description: Option<&str>,
        _doc: &mut ResourceMeta,
    ) -> bool {
        false
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
                tracing::debug!(source = hook.name(), host = %crate::resolver::log_host(uri), kind = crate::resolver::error_class(&error), "wall hook failed");
                crate::telemetry::discovery("wall_hook", "failed");
            }
            Err(_) => {
                tracing::debug!(source = hook.name(), host = %crate::resolver::log_host(uri), "wall hook timed out");
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
                tracing::debug!(source = hook.name(), host = %crate::resolver::log_host(uri), kind = crate::resolver::error_class(&error), "page hook failed");
                crate::telemetry::discovery("page_hook", "failed");
            }
            Err(_) => {
                tracing::debug!(source = hook.name(), host = %crate::resolver::log_host(uri), "page hook timed out");
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

/// What the mock end-to-end run tags for one source and answers for it: sample links with the
/// card types they resolve to, and the recorded answers (files in
/// `tests/fixtures/sources/<name>/`) its requests get.
#[derive(Clone, Copy, Debug)]
pub struct E2e {
    pub samples: &'static [E2eSample],
    pub answers: &'static [E2eAnswer],
}
/// A link the end-to-end run tags.
#[derive(Clone, Copy, Debug)]
pub struct E2eSample {
    /// Its name in the run's report: the source's name, or another for a second link.
    pub kind: &'static str,
    pub uri: &'static str,
    pub card_type: &'static str,
    /// The kind of a sample whose resource it groups with.
    pub group_with: Option<&'static str>,
}
/// A recorded answer of the mock run, for the requests it matches.
#[derive(Clone, Copy, Debug)]
pub struct E2eAnswer {
    pub host: &'static str,
    /// The request's path; with a trailing `*`, its start.
    pub path: &'static str,
    /// Query parameters the request carries, decoded.
    pub query: &'static [(&'static str, &'static str)],
    /// A request header it carries (a service's own identification header).
    pub header: Option<&'static str>,
    pub content_type: &'static str,
    /// The answer's file in the source's fixture directory (`.gz` is unpacked).
    pub file: &'static str,
}
impl E2eAnswer {
    /// Whether this answers a request for `url` with `headers`.
    pub fn matches(&self, url: &url::Url, headers: &[(&'static str, String)]) -> bool {
        let path = match self.path.strip_suffix('*') {
            Some(start) => url.path().starts_with(start),
            None => url.path() == self.path,
        };
        url.host_str() == Some(self.host)
            && path
            && self
                .query
                .iter()
                .all(|(k, v)| url.query_pairs().any(|(qk, qv)| qk == *k && qv == *v))
            && self
                .header
                .is_none_or(|h| headers.iter().any(|(name, _)| name.eq_ignore_ascii_case(h)))
    }
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

/// A source's switch, and its token for a source that takes one (`Source::takes_token`): a
/// secret that never appears in logs, errors or `Debug` output.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Switch {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<crate::event::adapters::SecretKey>,
}
impl Default for Switch {
    fn default() -> Self {
        Self {
            enabled: true,
            token: None,
        }
    }
}
/// `[sources.<name>]`: every source is on unless its table says `enabled = false`; a source that
/// takes a token is on only with one.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SourcesConfig(pub BTreeMap<String, Switch>);
impl SourcesConfig {
    pub fn enabled(&self, name: &str) -> bool {
        let needs_token = by_name(name).is_some_and(|s| s.takes_token());
        match self.0.get(name) {
            Some(s) => s.enabled && (!needs_token || self.token(name).is_some()),
            None => !needs_token,
        }
    }
    /// The token of `[sources.<name>]`, when it has a non-empty one.
    pub fn token(&self, name: &str) -> Option<&str> {
        self.0.get(name)?.token.as_deref().filter(|t| !t.is_empty())
    }
    pub fn validate(&self) -> Result<()> {
        for (name, switch) in &self.0 {
            let source =
                by_name(name).ok_or_else(|| anyhow::anyhow!("unknown source [sources.{name}]"))?;
            if let Some(token) = switch.token.as_deref().filter(|t| !t.is_empty()) {
                anyhow::ensure!(source.takes_token(), "[sources.{name}] takes no token");
                anyhow::ensure!(
                    token.len() <= 256 && token.bytes().all(|b| b.is_ascii_graphic()),
                    "[sources.{name}] token must be at most 256 printable characters"
                );
            }
        }
        Ok(())
    }
}

/// How often each source's service refused its configured token since the daemon started
/// (`pinakes_source_token_refusals_total`).
static TOKEN_REFUSALS: std::sync::Mutex<BTreeMap<&'static str, u64>> =
    std::sync::Mutex::new(BTreeMap::new());
/// Count one refusal of `source`'s token.
pub fn token_refused(source: &'static str) {
    *TOKEN_REFUSALS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(source)
        .or_default() += 1;
}
/// Every source's token refusals so far.
pub fn token_refusals() -> Vec<(&'static str, u64)> {
    TOKEN_REFUSALS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, v)| (*k, *v))
        .collect()
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
/// (its links then get ordinary web cards, as its routes do), and its sources' tokens; and let
/// every source take what it needs from the configuration (`Source::configure`).
pub fn configure(config: &Config) {
    *SWITCHES.write().unwrap_or_else(|e| e.into_inner()) = Some(config.sources.clone());
    for source in REGISTRY {
        source.configure(config);
    }
}
/// A finished card named from the title the also-on sources found (`Source::apply_title`), by
/// the first source that names that page's cards: whether one applied.
pub fn apply_title(
    uri: &str,
    title: &str,
    description: Option<&str>,
    doc: &mut ResourceMeta,
) -> bool {
    REGISTRY
        .iter()
        .any(|s| s.apply_title(uri, title, description, doc))
}
/// The card the switched-on source reading `uri` makes of it, `None` when no source reads it,
/// its source declined it or the object does not exist: for a source whose card builds on another
/// object's (an SEC release on the Federal Register document that publishes it), by the other
/// object's identifier alone.
pub async fn card_of(cx: &Context<'_>, uri: &str) -> Result<Option<ResourceMeta>> {
    let source = {
        let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner());
        reading(uri).filter(|s| switches.as_ref().is_none_or(|c| c.enabled(s.name())))
    };
    let Some(source) = source else {
        return Ok(None);
    };
    match source.resolve(cx, uri).await {
        Ok(doc) if doc.status == Status::Ok => Ok(Some(doc)),
        Ok(_) => Ok(None),
        Err(error) if error.is::<Declined>() => Ok(None),
        Err(error) => Err(error),
    }
}
/// The token the daemon started with for `source` (`configure`).
pub fn token(source: &str) -> Option<crate::event::adapters::SecretKey> {
    let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner());
    switches.as_ref()?.token(source).map(Into::into)
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
// Helpers for any source's tests: one only some source uses stays when that source goes.
#[allow(dead_code)]
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
    /// remembers the URLs asked, in order. A redirect's body is its `Location`; the content type
    /// `!too-large` answers a body over its read's cap. Pair it with `#[tokio::test(start_paused = true)]`:
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
            if kind == "!too-large" {
                return Err(crate::resolver::egress::BodyTooLarge {
                    cap: crate::hubs::WIKIDATA_CAP,
                }
                .into());
            }
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
    pub struct PublicDns;
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

    /// `scripts/check-removals --self-test`, run from `root`: it must pass.
    fn check_removals_self_test(root: &Path) {
        let output = std::process::Command::new("python3")
            .arg(root.join("scripts/check-removals"))
            .arg("--self-test")
            .current_dir(root)
            .output()
            .expect("python3");
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A scratch directory, removed when dropped (a failed test too).
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A scratch directory holding a copy of `scripts/check-removals` and the given files, and
    /// nothing else of the repository.
    fn scratch_with_script(label: &str, files: &[(&str, &str)]) -> Scratch {
        let root = std::env::temp_dir().join(format!(
            "check-removals-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(
            root.join("scripts/check-removals"),
            repo("scripts/check-removals"),
        )
        .unwrap();
        let scratch = Scratch(root);
        for (path, text) in files {
            let path = scratch.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        scratch
    }

    /// `scripts/check-removals`'s leftover check catches what its deletion would not, on a
    /// synthetic tree of the self-test's own: each kind of source removes and verifies clean, and
    /// each leftover the deletion misses, put back alone, is named.
    #[test]
    fn check_removals_finds_leftovers_its_deletion_missed() {
        check_removals_self_test(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    }

    /// The self-test reads nothing of the repository: a new official source with an also-on half
    /// that is not committed (a git repository whose HEAD has only the script) leaves it passing.
    #[test]
    fn check_removals_self_test_ignores_an_uncommitted_source() {
        let scratch = scratch_with_script("uncommitted", &[]);
        let root = &scratch.0;
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(root)
                .output()
                .expect("git");
            assert!(status.status.success(), "git {args:?}: {status:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "scripts/check-removals"]);
        git(&["commit", "-q", "-m", "the script alone"]);
        for (path, text) in [
            (
                "crates/pinakes-core/src/sources/mod.rs",
                "pub mod review_source;\npub static REGISTRY: &[&dyn Source] = &[\n    &review_source::ReviewSource,\n];\n",
            ),
            (
                "crates/pinakes-core/src/also_on.rs",
                "pub fn registry() -> Vec<Arc<dyn Source>> {\n    vec![\n        Arc::new(crate::sources::review_source::also_on::Keys),\n    ]\n}\n",
            ),
            (
                "crates/pinakes-core/src/sources/review_source.rs",
                "pub struct ReviewSource;\n",
            ),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        check_removals_self_test(root);
    }

    /// The self-test needs no official source with an also-on half: with the registries of a
    /// build that has none (each such source removed), it passes.
    #[test]
    fn check_removals_self_test_needs_no_also_on_half() {
        let scratch = scratch_with_script(
            "no-halves",
            &[
                (
                    "crates/pinakes-core/src/sources/mod.rs",
                    "pub mod plain_source;\npub static REGISTRY: &[&dyn Source] = &[\n    &plain_source::PlainSource,\n];\n",
                ),
                (
                    "crates/pinakes-core/src/also_on.rs",
                    "pub fn registry() -> Vec<Arc<dyn Source>> {\n    vec![]\n}\n",
                ),
                (
                    "crates/pinakes-core/src/lookups/mod.rs",
                    "pub static FAMILIES: &[&dyn Family] = &[];\n",
                ),
            ],
        );
        check_removals_self_test(&scratch.0);
    }

    /// A source's mock end-to-end samples are links it reads (or pages it improves), each named
    /// once in the run; their answers are recorded files of its own that match a request.
    #[test]
    fn each_sources_e2e_samples_and_answers_are_its_own() {
        let mut kinds = std::collections::BTreeSet::new();
        for source in REGISTRY {
            let Some(e2e) = source.e2e() else {
                continue;
            };
            let name = source.name();
            assert!(!e2e.samples.is_empty() && !e2e.answers.is_empty(), "{name}");
            for sample in e2e.samples {
                assert!(
                    sample.kind == name || sample.kind.starts_with(&format!("{name}_")),
                    "{name}: sample {}",
                    sample.kind
                );
                assert!(kinds.insert(sample.kind), "{} twice", sample.kind);
                assert!(
                    source.reads(sample.uri) || source.hooks_pages() || source.hooks_walls(),
                    "{name} does not read {}",
                    sample.uri
                );
                assert!(!crate::model::type_path(sample.card_type).is_empty());
            }
            for answer in e2e.answers {
                fixture::read(name, answer.file);
                let mut url = url::Url::parse(&format!(
                    "https://{}{}",
                    answer.host,
                    answer.path.trim_end_matches('*')
                ))
                .unwrap();
                for (k, v) in answer.query {
                    url.query_pairs_mut().append_pair(k, v);
                }
                let headers: Vec<(&'static str, String)> = answer
                    .header
                    .map(|h| (h, String::new()))
                    .into_iter()
                    .collect();
                assert!(answer.matches(&url, &headers), "{name}: {url}");
                if answer.header.is_some() {
                    assert!(
                        !answer.matches(&url, &[]),
                        "{name}: {url} without its header"
                    );
                }
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

    /// A registered source (whichever this build has) and an example link of it whose alias root
    /// differs from the link.
    fn aliased_example() -> Option<(&'static dyn Source, &'static str, String)> {
        REGISTRY.iter().find_map(|s| {
            s.examples()
                .iter()
                .find_map(|e| Some((*s, e.uri, s.alias_root(e.uri)?)))
        })
    }

    #[test]
    fn a_switched_off_source_merges_nothing() {
        let Some((source, uri, root)) = aliased_example() else {
            return;
        };
        assert_eq!(alias_root_with(uri, None), Some(root.clone()));
        let mut off = SourcesConfig::default();
        assert_eq!(alias_root_with(uri, Some(&off)), Some(root));
        off.0.insert(
            source.name().into(),
            Switch {
                enabled: false,
                ..Default::default()
            },
        );
        assert_eq!(alias_root_with(uri, Some(&off)), None);
    }

    #[test]
    fn switches_and_names() {
        let mut config = Config::default();
        let Some(source) = REGISTRY
            .iter()
            .find(|s| !s.takes_token() && s.reads(s.examples()[0].uri))
        else {
            return;
        };
        let uri = source.examples()[0].uri;
        assert_eq!(
            enabled_source(uri, &config).map(|s| s.name()),
            Some(source.name())
        );
        config.sources.0.insert(
            source.name().into(),
            Switch {
                enabled: false,
                ..Default::default()
            },
        );
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
        let Some(source) = REGISTRY.first() else {
            return;
        };
        let mut doc = ResourceMeta::new("fr:2026-18291", source.name(), 60);
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
        // A source with two facts, neither of them a web attribute.
        let web = crate::resolver::web::ATTRIBUTES;
        let Some((source, fact, blank)) = REGISTRY.iter().find_map(|s| {
            if s.facts().contains(&"author") {
                return None;
            }
            let mut own = s.facts().iter().filter(|f| !web.contains(f));
            Some((*s, *own.next()?, *own.next()?))
        }) else {
            return;
        };
        let mut doc = ResourceMeta::new("fr:2026-18291", source.name(), 60);
        doc.attributes.insert(fact.into(), "91 FR 57392".into());
        doc.attributes.insert("author".into(), "x".into());
        doc.attributes.insert(blank.into(), " ".into());
        assert_eq!(from(&doc), Some(source.name()));
        let facts = facts(&doc).unwrap();
        assert_eq!(facts.len(), 1, "{facts:?}");
        assert_eq!(facts[fact], "91 FR 57392");
        doc.source = "web".into();
        assert_eq!((from(&doc), self::facts(&doc)), (None, None));
        assert!(attributes().contains(&"author") && attributes().contains(&fact));
        let values = ["a ".to_owned(), String::new(), "b".to_owned()];
        assert_eq!(list(&values).as_deref(), Some("a; b"));
        assert_eq!(list(&[]), None);
    }
}
