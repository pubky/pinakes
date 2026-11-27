//! Cards read from a service's own API instead of its HTML page: exact where the page is
//! bot-walled, thin or moved. Most APIs need no key; a few sources read with the operator's
//! token (`Source::takes_token`) and are off without it. Where a service has no API for an
//! object, its source reads the service's own page (its tags or embedded record); page hooks and
//! wall hooks improve a web card instead of claiming links.
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
pub mod castro;
pub mod cleveland;
pub mod clinicaltrials;
pub mod commons;
pub mod courtlistener;
pub mod discogs;
pub mod discourse;
pub mod docker_hub;
pub mod ecfr;
pub mod eur_lex;
pub mod facebook;
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
pub mod greenhouse;
pub mod hacker_news;
pub mod huggingface;
pub mod inaturalist;
pub mod instagram;
pub mod internet_archive;
pub mod kit;
pub mod legislation_gov_uk;
pub mod linkedin;
pub mod lobsters;
pub mod mastodon;
pub mod media_records;
pub mod mempool;
pub mod met;
pub mod open_food_facts;
pub mod pdb;
pub mod pinterest;
pub mod podcast_index;
pub mod pubchem;
pub mod public_inbox;
pub mod reddit;
pub mod ror;
pub mod sec;
pub mod shopify;
pub mod snap_store;
pub mod social;
pub mod stack_exchange;
pub mod stacker_news;
pub mod substack;
pub mod swiss_parliament;
pub mod telegram;
pub mod threads;
pub mod tiktok;
pub mod twitch;
pub mod uk_parliament_bills;
pub mod uniprot;
pub mod usgs;
pub mod wordpress;
pub mod x;
pub mod zefix;
pub mod zenodo;

/// Every source, one line each, in alphabetical order of name.
#[rustfmt::skip]
pub static REGISTRY: &[&dyn Source] = &[
    &app_store::AppStore,
    &artic::Artic,
    &bluesky::Bluesky,
    &castro::Castro,
    &cleveland::Cleveland,
    &clinicaltrials::ClinicalTrials,
    &commons::Commons,
    &courtlistener::Courtlistener,
    &discogs::Discogs,
    &discourse::Discourse,
    &docker_hub::DockerHub,
    &ecfr::Ecfr,
    &eur_lex::EurLex,
    &facebook::Facebook,
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
    &greenhouse::Greenhouse,
    &hacker_news::HackerNews,
    &huggingface::HuggingFace,
    &inaturalist::Inaturalist,
    &instagram::Instagram,
    &internet_archive::InternetArchive,
    &legislation_gov_uk::LegislationGovUk,
    &linkedin::LinkedIn,
    &lobsters::Lobsters,
    &mastodon::Mastodon,
    &media_records::MediaRecords,
    &mempool::Mempool,
    &met::Met,
    &open_food_facts::OpenFoodFacts,
    &pdb::Pdb,
    &pinterest::Pinterest,
    &podcast_index::PodcastIndex,
    &pubchem::Pubchem,
    &public_inbox::PublicInbox,
    &reddit::Reddit,
    &ror::Ror,
    &sec::Sec,
    &shopify::Shopify,
    &snap_store::SnapStore,
    &stack_exchange::StackExchange,
    &stacker_news::StackerNews,
    &substack::Substack,
    &swiss_parliament::SwissParliament,
    &telegram::Telegram,
    &threads::Threads,
    &tiktok::TikTok,
    &twitch::Twitch,
    &uk_parliament_bills::UkParliamentBills,
    &uniprot::Uniprot,
    &usgs::Usgs,
    &wordpress::Wordpress,
    &x::X,
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
    /// The web page of a root in this source's own identifier scheme (`artic:…`, `btc:…`), from
    /// the root alone: an address of the object that [`alias_root`](Source::alias_root) folds
    /// back to the root. `root` is one of the source's roots ([`root_page_by`] folds any other
    /// spelling first); `None` for a root that is a web page itself.
    fn root_page(&self, _root: &str) -> Option<String> {
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
    /// The namespaces of its cards' ids (`kit::Card::key`) that join groups ([`own_keys`]):
    /// its own DOIs, by the prefix it issues them under, or the Wikidata item its record names.
    /// Every other id stays a fact.
    fn own_ids(&self) -> &'static [kit::OwnId] {
        &[]
    }
    /// The facts of its stored card a read of the link sees again (`Context::stored`): what an
    /// earlier read learnt that the link alone does not say. Each is one of `facts`.
    fn reads_back(&self) -> &'static [&'static str] {
        &[]
    }
    /// The service's published limits per host, as `(host, requests_per_second)`; a windowed
    /// limit is given as its average rate (60 per minute → 1.0). The rate lowers the host's
    /// default (`limits.host_requests_per_second`), never raises it: one above the default
    /// changes nothing until the operator raises the default. `limits.host_rates` overrides
    /// both. Egress never has more than one request in flight per host, which meets any limit on
    /// concurrent requests.
    fn budgets(&self) -> &'static [Budget] {
        &[]
    }
    /// Whether a card quotes people's own posts (a comment, a social post): social content keeps
    /// a 200-character excerpt, refreshes within `social_refresh_secs` (an hour) and is purged
    /// once the object is gone.
    fn social(&self, _doc: &ResourceMeta) -> bool {
        false
    }
    /// Whether the card of `uri` is social content ([`Source::social`]), from what the link
    /// names alone, when the link tells (a post or a profile against a community's page); `None`:
    /// the link does not tell.
    fn social_link(&self, _uri: &str) -> Option<bool> {
        None
    }
    /// The longest a social card of this source goes before it is read again: an hour, or longer
    /// for a service that refuses frequent requests.
    fn social_refresh_secs(&self) -> u64 {
        3600
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
    /// Whether this source sends the operator's app key when one is set (`[sources.<name>]
    /// key`): an app's identifier, not a credential, that raises the service's quota. The source
    /// is on with or without it, and the key is kept out of logs and errors as a token is.
    fn takes_key(&self) -> bool {
        false
    }
    /// The host rates while the operator's app key is set (`takes_key`): the service's keyed
    /// quota as its average rate.
    fn keyed_budgets(&self) -> &'static [Budget] {
        self.budgets()
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
        let deadline = tokio::time::Instant::now() + left;
        match tokio::time::timeout_at(deadline, within(deadline, hook.walled(&cx, uri))).await {
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
        let deadline = tokio::time::Instant::now() + left;
        match tokio::time::timeout_at(deadline, within(deadline, hook.page(&cx, uri, page, &doc)))
            .await
        {
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

/// `(host, requests_per_second)`.
pub type Budget = (&'static str, f64);

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
tokio::task_local! {
    /// The deadline of the read a hook serves (`wall_hooks`, `page_hooks`), for what the hook
    /// fetches beyond its card.
    static DEADLINE: tokio::time::Instant;
}
tokio::task_local! {
    /// The facts the jobs' sources read back of their stored cards (`Source::reads_back`), by link.
    static STORED: BTreeMap<String, BTreeMap<String, String>>;
}
/// What the sources of `jobs` read back of their stored cards in `pool` (`Source::reads_back`),
/// by link, for `with_stored`. It asks the database only when such a source reads one of them.
pub async fn stored(
    pool: &sqlx::PgPool,
    jobs: &[crate::queue::Job],
) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let wanted: BTreeMap<&str, (&str, &'static [&'static str])> = jobs
        .iter()
        .filter_map(|j| {
            let facts = reading(&j.uri)?.reads_back();
            (!facts.is_empty()).then_some((j.id.as_str(), (j.uri.as_str(), facts)))
        })
        .collect();
    let mut stored = BTreeMap::new();
    if wanted.is_empty() {
        return Ok(stored);
    }
    let ids: Vec<&str> = wanted.keys().copied().collect();
    let rows: Vec<(String, Option<serde_json::Value>)> =
        sqlx::query_as("SELECT id, doc->'attributes' FROM resource WHERE id=ANY($1)")
            .bind(&ids)
            .fetch_all(pool)
            .await?;
    for (id, attributes) in rows {
        let (Some((uri, facts)), Some(attributes)) = (wanted.get(id.as_str()), attributes) else {
            continue;
        };
        let kept: BTreeMap<String, String> = facts
            .iter()
            .filter_map(|f| Some(((*f).to_owned(), attributes.get(*f)?.as_str()?.to_owned())))
            .collect();
        stored.insert((*uri).to_owned(), kept);
    }
    Ok(stored)
}
/// Run `read` with `stored` (from `stored()`) as what its sources' contexts see
/// (`Context::stored`).
pub async fn with_stored<F: std::future::Future>(
    stored: BTreeMap<String, BTreeMap<String, String>>,
    read: F,
) -> F::Output {
    STORED.scope(stored, read).await
}
/// Run `hook` with `deadline` as the deadline its context reports (`Context::deadline`).
pub async fn within<F: std::future::Future>(deadline: tokio::time::Instant, hook: F) -> F::Output {
    DEADLINE.scope(deadline, hook).await
}
impl Context<'_> {
    /// The operator's contact address (`http.contact`). A source whose service refuses requests
    /// without one answers `RetryLater` (class `disabled`) while it is empty.
    pub fn contact(&self) -> Option<&str> {
        self.egress.contact()
    }
    /// The deadline of the read this context serves, when its caller set one (a wall or page
    /// hook runs under the web fetch's): what a source fetches beyond its card, such as a show's
    /// episodes, stays within it so the card is handed back in time. `None` on the official
    /// route, which has the fetch to itself.
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        DEADLINE.try_with(|d| *d).ok()
    }
    /// Fact `name` of the stored card of `uri`, when its source reads it back
    /// (`Source::reads_back`) and the queue's job of `uri` runs this read.
    pub fn stored(&self, uri: &str, name: &str) -> Option<String> {
        STORED
            .try_with(|s| s.get(uri).and_then(|facts| facts.get(name)).cloned())
            .ok()
            .flatten()
    }
    /// A GET (or, with `options.json_body`, a POST) through the egress policy. A rate limit
    /// (429), a request timeout (408) and a server error become `RetryLater` (`retry`, classes
    /// `http_4xx` and `upstream`); other statuses are the caller's to read. At most
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

/// The options of a JSON API read whose error answers are read too. The transport drops the
/// body of an error answer unless the read asks for it (`FetchOptions::error_json`): a source
/// that tells "no such object" from another refusal by what the answer says reads with these.
/// A rate limit (429) then comes back as an answer, which `Context::fetch` turns into the retry.
pub fn json_with_errors() -> FetchOptions {
    FetchOptions {
        error_json: true,
        ..FetchOptions::json()
    }
}

/// Whether an error is this daemon's own host budget (the slot is further away than a fetch
/// may wait, which egress answers with the class `rate_limited`), which says nothing about the
/// host's answer.
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
        // A new host at the cap takes the oldest answer's place; a known host only renews its own.
        if memo.len() >= Self::CAP && !memo.contains_key(host) {
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

/// A source's switch, its token for a source that takes one (`Source::takes_token`) and its app
/// key for a source that sends one (`Source::takes_key`): values that never appear in logs,
/// errors or `Debug` output.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Switch {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<crate::event::adapters::SecretKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<crate::event::adapters::SecretKey>,
}
impl Default for Switch {
    fn default() -> Self {
        Self {
            enabled: true,
            token: None,
            key: None,
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
    /// The app key of `[sources.<name>]`, when it has a non-empty one.
    pub fn key(&self, name: &str) -> Option<&str> {
        self.0.get(name)?.key.as_deref().filter(|k| !k.is_empty())
    }
    pub fn validate(&self) -> Result<()> {
        for (name, switch) in &self.0 {
            let Some(source) = by_name(name) else {
                // A block left from a source the build no longer has is ignored when it only
                // switches it; one with a token or key is an error, as a misspelled name may hide
                // where the operator meant the value to go.
                anyhow::ensure!(
                    switch.token.is_none() && switch.key.is_none(),
                    "unknown source [sources.{name}]"
                );
                tracing::warn!(source = %name, "ignoring [sources.{name}]: no such source in this build");
                continue;
            };
            if let Some(token) = switch.token.as_deref().filter(|t| !t.is_empty()) {
                anyhow::ensure!(source.takes_token(), "[sources.{name}] takes no token");
                anyhow::ensure!(
                    token.len() <= 256 && token.bytes().all(|b| b.is_ascii_graphic()),
                    "[sources.{name}] token must be at most 256 printable characters"
                );
            }
            if let Some(key) = switch.key.as_deref().filter(|k| !k.is_empty()) {
                anyhow::ensure!(source.takes_key(), "[sources.{name}] takes no key");
                anyhow::ensure!(
                    key.len() <= 256 && key.bytes().all(|b| b.is_ascii_graphic()),
                    "[sources.{name}] key must be at most 256 printable characters"
                );
            }
        }
        Ok(())
    }
}

/// Every source, also-on service and event platform that reads with an operator's key, token or
/// app credentials, and Nominatim (`osm`, a hosted one's key in its URL), as
/// `pinakes_source_token_refusals_total{source}` names it: the series exists (at 0) for each from
/// the start, and no other label is ever counted.
pub const KEYED: &[&str] = &[
    "courtlistener",
    "discogs",
    "github",
    "osm",
    "podcast_index",
    "spotify",
    "ticketmaster",
    "tidal",
    "tmdb",
];

/// How often each keyed service refused the operator's credential since the daemon started
/// (`pinakes_source_token_refusals_total`).
static TOKEN_REFUSALS: std::sync::Mutex<BTreeMap<&'static str, u64>> =
    std::sync::Mutex::new(BTreeMap::new());
/// Count one refusal of `source`'s configured credential, as its service signals one: a 401, or
/// a 403 where the service uses it for a refused key (not GitHub's, its rate limit; not a 403
/// to one request Spotify withholds), or a token endpoint answering anything but 200, a 408, a 429
/// or a 5xx. A rate limit (429) or an outage (5xx) is never a refusal. `source` is one of [`KEYED`];
/// any other name is not counted.
pub fn token_refused(source: &'static str) {
    if !KEYED.contains(&source) {
        return;
    }
    *TOKEN_REFUSALS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(source)
        .or_default() += 1;
}
/// A keyed service's refusals of the operator's key or token: each one is counted
/// (`token_refused`) and switches the key off for an hour, during which the source answers
/// `Declined` (its links get web cards) or reads without it. A refusal is what the service's
/// own answer says is one: a 401, or a 403 whose body names the key (each source says how).
pub struct Refusals {
    source: &'static str,
    until: std::sync::Mutex<Option<tokio::time::Instant>>,
}
impl Refusals {
    /// How long a refused key stays off.
    pub const LATCH: std::time::Duration = std::time::Duration::from_secs(3600);
    pub const fn new(source: &'static str) -> Self {
        Self {
            source,
            until: std::sync::Mutex::new(None),
        }
    }
    /// Whether the key is off: the service refused it less than an hour ago.
    pub fn latched(&self) -> bool {
        let until = *self.until.lock().unwrap_or_else(|e| e.into_inner());
        until.is_some_and(|u| tokio::time::Instant::now() < u)
    }
    /// Count one refusal and switch the key off for an hour from now; true when it was on, so
    /// the caller warns once per latch.
    pub fn refused(&self) -> bool {
        token_refused(self.source);
        let now = tokio::time::Instant::now();
        let mut until = self.until.lock().unwrap_or_else(|e| e.into_inner());
        let was_on = until.is_none_or(|u| now >= u);
        *until = Some(now + Self::LATCH);
        was_on
    }
    /// `Declined` while the key is off: the link gets a web card.
    pub fn check(&self) -> Result<()> {
        if self.latched() {
            return Err(Declined.into());
        }
        Ok(())
    }
}

/// Whether an error answer's JSON names the key or token as its reason (`field` holds the
/// service's message: "Invalid token.", "Bad credentials", …).
pub fn names_the_key(body: &[u8], field: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v[field].as_str().map(str::to_ascii_lowercase))
        .is_some_and(|m| {
            ["token", "credential", "key", "authoriz", "authentic"]
                .iter()
                .any(|w| m.contains(w))
        })
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
/// The join keys of a card an official source read, and that source: its facts in the
/// namespaces the source declares its own ([`Source::own_ids`]). A DOI is normalized
/// (`resolver::cards::doi`, lower case) before its prefix is checked; a Wikidata item is
/// `wikidata:Q…`. A fact holding several values (joined with `; `) gives keys only when every
/// value is an own id: one value with a `; ` that the record's author typed never yields a part
/// of it. `None` when there are none, and for a card of a page hook: what a page says about
/// itself joins nothing.
pub fn own_keys(doc: &ResourceMeta) -> Option<(&'static str, Vec<String>)> {
    let source = by_name(&doc.source).filter(|s| !s.hooks_pages())?;
    // Every value of a fact as `own` reads it, or none when one is not.
    let all = |name: &str, own: &dyn Fn(&str) -> Option<String>| -> Vec<String> {
        doc.attributes
            .get(name)
            .and_then(|v| v.split("; ").map(|v| own(v.trim())).collect())
            .unwrap_or_default()
    };
    let mut keys = vec![];
    for own in source.own_ids() {
        match own {
            kit::OwnId::Doi(prefix) => keys.extend(all("doi", &|d| {
                crate::resolver::cards::doi(&format!("doi:{d}"))
                    .filter(|d| d.starts_with(prefix))
                    .map(|d| format!("doi:{d}"))
            })),
            kit::OwnId::Wikidata => keys.extend(all("wikidata", &|q| {
                crate::hubs::qid(q).map(|q| format!("wikidata:{q}"))
            })),
        }
    }
    keys.sort();
    keys.dedup();
    (!keys.is_empty()).then_some((source.name(), keys))
}
/// The source that reads `uri`, switched on or not.
pub fn reading(uri: &str) -> Option<&'static dyn Source> {
    REGISTRY.iter().copied().find(|s| s.reads(uri))
}
/// Whether the source that reads `uri` is registered but switched off: what it merged before
/// stays merged.
pub fn switched_off(uri: &str) -> bool {
    let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner());
    reading(uri).is_some_and(|s| switches.as_ref().is_some_and(|c| !c.enabled(s.name())))
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
/// The token the daemon started with for `source` (`configure`).
pub fn token(source: &str) -> Option<crate::event::adapters::SecretKey> {
    let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner());
    switches.as_ref()?.token(source).map(Into::into)
}
/// The app key the daemon started with for `source` (`configure`).
pub fn key(source: &str) -> Option<crate::event::adapters::SecretKey> {
    let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner());
    switches.as_ref()?.key(source).map(Into::into)
}

/// The static alias root of a source's spelling, from the link alone, while that source is on.
pub fn alias_root(uri: &str) -> Option<String> {
    alias_root_by(uri).map(|(_, root)| root)
}
/// [`alias_root`] with the name of the source whose rule names the root.
pub fn alias_root_by(uri: &str) -> Option<(&'static str, String)> {
    let switches = SWITCHES.read().unwrap_or_else(|e| e.into_inner());
    alias_root_by_with(uri, switches.as_ref())
}
/// The web page of a source's root in its own identifier scheme, from the root or another
/// spelling of it, with the source's name ([`Source::root_page`]), whichever sources are on.
pub fn root_page_by(uri: &str) -> Option<(&'static str, String)> {
    REGISTRY.iter().find_map(|s| {
        let root = s.alias_root(uri).unwrap_or_else(|| uri.to_owned());
        Some((s.name(), s.root_page(&root)?))
    })
}
#[cfg(test)]
fn alias_root_with(uri: &str, switches: Option<&SourcesConfig>) -> Option<String> {
    alias_root_by_with(uri, switches).map(|(_, root)| root)
}
fn alias_root_by_with(
    uri: &str,
    switches: Option<&SourcesConfig>,
) -> Option<(&'static str, String)> {
    REGISTRY
        .iter()
        .filter(|s| switches.is_none_or(|c| c.enabled(s.name())))
        .find_map(|s| Some((s.name(), s.alias_root(uri)?)))
}
/// A link no source could ever resolve (a malformed identifier of a source's own scheme).
pub fn invalid(uri: &str) -> bool {
    REGISTRY.iter().any(|s| s.invalid(uri))
}
/// The host rates of the sources that ask their hosts, for hosts the operator did not set: one
/// rate per host, the lowest when two sources name it. A source asks its hosts while it is on, or
/// while its also-on half (the also-on source of the same name, which reads the same service) is;
/// one that is off with its half (or without its token) asks nothing, so it paces nothing.
pub fn budgets(config: &Config) -> impl Iterator<Item = (&'static str, f64)> {
    lowest(
        REGISTRY
            .iter()
            .filter(|s| {
                config.sources.enabled(s.name()) || crate::also_on::enabled_by_id(config, s.name())
            })
            .flat_map(|s| {
                if s.takes_key() && config.sources.key(s.name()).is_some() {
                    s.keyed_budgets()
                } else {
                    s.budgets()
                }
                .iter()
                .copied()
            }),
    )
    .into_iter()
}
/// One rate per host: the lowest named for it.
fn lowest(rates: impl Iterator<Item = (&'static str, f64)>) -> BTreeMap<&'static str, f64> {
    let mut lowest: BTreeMap<&'static str, f64> = BTreeMap::new();
    for (host, rate) in rates {
        lowest
            .entry(host)
            .and_modify(|r| *r = r.min(rate))
            .or_insert(rate);
    }
    lowest
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
    let mut doc = match source.resolve(&Context { egress, ttl }, uri).await {
        Ok(doc) => doc,
        Err(error) if error.is::<Declined>() => return Ok(None),
        Err(error) => return Err(error),
    };
    // A social profile's bio is its author's own text, published: never with an e-mail address.
    if matches!(doc.type_name.as_str(), "Person" | "Organization") && source.social(&doc) {
        doc.description = doc.description.as_deref().map(without_addresses);
    }
    Ok(Some(doc.bounded(attributes())?))
}

/// A request timeout or a rate limit (408, 429: class `http_4xx`, as egress classes the 429 it
/// answers itself) and a server error (class `upstream`): retry later, after at least a minute.
/// A 429 reaches here only from a read that asked for the error's body
/// (`FetchOptions::error_json`). The class `rate_limited` is the daemon's own budget wait alone
/// (`own_rate_limit`), never a service's answer.
pub(crate) fn retry(f: &Fetched) -> Result<()> {
    let class = if f.status >= 500 {
        "upstream"
    } else {
        "http_4xx"
    };
    crate::resolver::busy(f, class)
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
    cut(&without_addresses(&out), 200)
}

/// Markdown as plain text, whole: its lines kept, heading, quote and list marks dropped, inline
/// marks as in `markdown_excerpt`. For a site whose descriptions are Markdown (Meetup); not for
/// text in general, whose asterisks may be its own. Callers pass a bounded document's text.
pub fn markdown_plain(markdown: &str) -> String {
    let mut out: Vec<String> = vec![];
    for line in markdown.lines() {
        let mut line = line.trim();
        let heading = line.trim_start_matches('#');
        if heading.len() < line.len() && (heading.is_empty() || heading.starts_with(' ')) {
            line = heading.trim_start();
        }
        line = line.trim_start_matches('>').trim_start();
        for bullet in ["- ", "* ", "+ "] {
            if let Some(rest) = line.strip_prefix(bullet) {
                line = rest.trim_start();
                break;
            }
        }
        if line.starts_with("![") && line.ends_with(')') {
            continue;
        }
        let line = markdown_inline(line);
        if line.trim().is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line.trim().to_owned());
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out.join("\n")
}

/// At most `max` characters of `text`, with an ellipsis when cut.
pub fn cut(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

/// At most `max` characters of `text`, its whitespace flattened, with an ellipsis when cut.
pub fn excerpt(text: &str, max: usize) -> String {
    cut(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// `text` with its line breaks kept: the white space inside each line collapsed to single
/// spaces, at most one blank line in a row (a paragraph break), none at the start or end.
pub fn lines(text: &str) -> String {
    let mut out: Vec<String> = vec![];
    let mut blank = false;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank = !out.is_empty();
        } else {
            if blank {
                out.push(String::new());
                blank = false;
            }
            out.push(line);
        }
    }
    out.join("\n")
}

/// At most `max` characters of `text`, its line breaks kept ([`lines`]), with an ellipsis when
/// cut.
pub fn excerpt_lines(text: &str, max: usize) -> String {
    let all = lines(text);
    let mut out: String = all.chars().take(max).collect();
    if out.len() < all.len() {
        // A cut at a line break ends the text where it stops, not on a line of its own.
        out.truncate(out.trim_end().len());
        out.push('…');
    }
    out
}

/// `[text](url)` as `text`, without `**`, `__`, backticks and single `*`/`_` emphasis around
/// words (`*local*`, `_here_`); a backslash-escaped mark (`\*`) is the mark itself. A mark
/// between spaces or inside a word (`5 * 3`, `snake_case`, `2*3`) stays.
fn markdown_inline(line: &str) -> String {
    let mut line = line.to_owned();
    while let Some(mid) = line.find("](") {
        let (Some(open), Some(close)) = (line[..mid].rfind('['), line[mid..].find(')')) else {
            break;
        };
        let text = line[open + 1..mid].to_owned();
        line.replace_range(open..mid + close + 1, &text);
    }
    // Each character, and whether it is a mark (not escaped).
    let mut chars: Vec<(char, bool)> = vec![];
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        match (c, it.peek()) {
            ('\\', Some(&next)) if next.is_ascii_punctuation() => {
                chars.push((next, false));
                it.next();
            }
            ('`', _) => {}
            _ => chars.push((c, matches!(c, '*' | '_'))),
        }
    }
    // `**` and `__` go wherever they are, as before (one pass, left to right).
    let mut kept = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].1 && chars.get(i + 1).is_some_and(|n| n.1 && n.0 == chars[i].0) {
            i += 2;
        } else {
            kept.push(chars[i]);
            i += 1;
        }
    }
    let chars = kept;
    // Single emphasis: a mark that opens a word, up to the next same mark that closes one. One
    // pass: the latest opening mark of each kind waits for its closing one.
    let edge = |c: Option<&(char, bool)>| c.is_none_or(|(c, _)| !c.is_alphanumeric());
    let mut drop = vec![false; chars.len()];
    let mut open: [Option<usize>; 2] = [None, None];
    for i in 0..chars.len() {
        let (c, mark) = chars[i];
        if !mark {
            continue;
        }
        let slot = usize::from(c == '_');
        let closes = i > 0 && !chars[i - 1].0.is_whitespace() && edge(chars.get(i + 1));
        let opens = (i == 0 || edge(chars.get(i - 1)))
            && chars.get(i + 1).is_some_and(|(c, _)| !c.is_whitespace());
        match open[slot] {
            Some(o) if closes && o + 1 < i => {
                drop[o] = true;
                drop[i] = true;
                open[slot] = None;
            }
            _ if opens => open[slot] = Some(i),
            _ => {}
        }
    }
    chars
        .iter()
        .zip(drop)
        .filter(|(_, d)| !d)
        .map(|((c, _), _)| *c)
        .collect()
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
/// An `https` or `http` URL with a host, as given: for a URL a service's answer names (an image,
/// a website) and a page of any site. A source parses its own links with `kit::web_url`, which
/// refuses a port, a user name and a password.
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
    /// remembers the URLs asked, in order. An error answer's body is dropped as the real read
    /// drops it (kept for a wall's page and for a read with `error_json`). A redirect's body is
    /// its `Location`; the content type
    /// `!too-large` answers a body over its read's cap, and `!large-xml` an XML body over the page
    /// cap as the real read treats one of a read without a feed cap: refused when the read sets
    /// `body_cap`, otherwise cut (the route's body is the cut text). Pair it with `#[tokio::test(start_paused = true)]`: the
    /// per-host spacing then costs no time.
    pub fn egress(
        routes: Vec<(&str, u16, &'static str, Vec<u8>)>,
    ) -> (crate::resolver::egress::Egress, std::sync::Arc<Recorded>) {
        let recorded = std::sync::Arc::new(Recorded {
            routes: routes
                .into_iter()
                .map(|(url, status, kind, body)| (url.to_owned(), (status, kind, body)))
                .collect(),
            asked: Default::default(),
            delays: Default::default(),
            raw: std::sync::atomic::AtomicBool::new(false),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(PublicDns),
            recorded.clone(),
        );
        (egress, recorded)
    }

    /// `egress` whose transport hands every answer back as recorded, an error's body included:
    /// for tests of the egress itself, which say what it does with whatever a transport gives.
    pub fn egress_raw(
        routes: Vec<(&str, u16, &'static str, Vec<u8>)>,
    ) -> (crate::resolver::egress::Egress, std::sync::Arc<Recorded>) {
        let (egress, recorded) = egress(routes);
        recorded
            .raw
            .store(true, std::sync::atomic::Ordering::Relaxed);
        (egress, recorded)
    }

    pub struct Recorded {
        routes: std::collections::BTreeMap<String, (u16, &'static str, Vec<u8>)>,
        pub asked: std::sync::Mutex<Vec<String>>,
        delays: std::sync::Mutex<std::collections::BTreeMap<String, std::time::Duration>>,
        /// Whether an error answer keeps its body whatever the read asked for (`egress_raw`).
        raw: std::sync::atomic::AtomicBool,
    }
    impl Recorded {
        pub fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
        /// Answer `url` only after `delay` (tokio time, so a paused test's clock): a slow host.
        pub fn delay(&self, url: &str, delay: std::time::Duration) {
            self.delays.lock().unwrap().insert(url.to_owned(), delay);
        }
    }
    #[async_trait::async_trait]
    impl crate::resolver::egress::Transport for Recorded {
        async fn get_with(
            &self,
            url: &url::Url,
            addresses: &[std::net::SocketAddr],
            options: &crate::resolver::egress::FetchOptions,
        ) -> anyhow::Result<crate::resolver::egress::Fetched> {
            match self.routes.get(url.as_str()) {
                Some((status, "!large-xml", body)) => {
                    self.asked.lock().unwrap().push(url.to_string());
                    if let Some(cap) = options.body_cap {
                        return Err(crate::resolver::egress::BodyTooLarge { cap }.into());
                    }
                    Ok(crate::resolver::egress::Fetched {
                        truncated: true,
                        ..crate::resolver::egress::Fetched::answer(
                            url.clone(),
                            *status,
                            "application/xml".into(),
                            body.clone(),
                        )
                    })
                }
                _ => {
                    // As the real read does: an error answer keeps its body only when it is a
                    // wall's page (403, 429 or 503 as HTML or text) or the read asked for an
                    // error's JSON.
                    let mut fetched = self.get(url, addresses).await?;
                    let kind = fetched
                        .content_type
                        .split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase();
                    let wall = matches!(fetched.status, 403 | 429 | 503)
                        && matches!(
                            kind.as_str(),
                            "text/html" | "application/xhtml+xml" | "text/plain" | ""
                        );
                    let json = options.error_json
                        && (kind == "application/json" || kind.ends_with("+json"));
                    let raw = self.raw.load(std::sync::atomic::Ordering::Relaxed);
                    if fetched.status >= 400 && !wall && !json && !raw {
                        fetched.body.clear();
                    }
                    Ok(fetched)
                }
            }
        }
        async fn get(
            &self,
            url: &url::Url,
            _: &[std::net::SocketAddr],
        ) -> anyhow::Result<crate::resolver::egress::Fetched> {
            self.asked.lock().unwrap().push(url.to_string());
            let delay = self.delays.lock().unwrap().get(url.as_str()).copied();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let (status, kind, body) = self.routes.get(url.as_str()).cloned().unwrap_or((
                404,
                "text/html",
                b"<html>not found</html>".to_vec(),
            ));
            if kind == "!too-large" {
                return Err(crate::resolver::egress::BodyTooLarge {
                    cap: crate::hubs::DEFAULT_BODY_CAP,
                }
                .into());
            }
            let location = (300..400)
                .contains(&status)
                .then(|| String::from_utf8_lossy(&body).into_owned());
            Ok(crate::resolver::egress::Fetched {
                location,
                ..crate::resolver::egress::Fetched::answer(url.clone(), status, kind.into(), body)
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
            validators: crate::resolver::egress::Validators {
                link: link.map(str::to_owned),
                ..Default::default()
            },
            ..crate::resolver::egress::Fetched::answer(
                url::Url::parse(url).unwrap(),
                200,
                content_type.into(),
                body,
            )
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

    /// A source's page of a root answers for its root and for the source's other spellings of it,
    /// folded with the source's own rule; `root_page_by` takes no switches, while the same
    /// spelling folds to nothing through `alias_root_with` once the source is off.
    #[test]
    fn root_pages_fold_a_spelling() {
        if by_name("pdb").is_none() {
            return;
        }
        let page = Some(("pdb", "https://www.rcsb.org/structure/4HHB".to_owned()));
        assert_eq!(root_page_by("pdb:4HHB"), page);
        assert_eq!(root_page_by("pdb:4hhb"), page);
        assert_eq!(root_page_by("https://www.rcsb.org/structure/4hhb"), page);
        let mut off = SourcesConfig::default();
        off.0.insert(
            "pdb".into(),
            Switch {
                enabled: false,
                ..Default::default()
            },
        );
        assert_eq!(alias_root_with("pdb:4hhb", Some(&off)), None);
        assert_eq!(root_page_by("pdb:0000"), None);
        assert_eq!(root_page_by("https://example.org/"), None);
    }

    /// A source's own DOI prefix is a DOI prefix in lower case that ends at a separator, so it
    /// takes no other registrant's DOIs; an id it declares is one of its facts; a page hook
    /// declares none.
    #[test]
    fn own_ids_are_facts_and_whole_prefixes() {
        for source in REGISTRY {
            for own in source.own_ids() {
                let fact = match own {
                    kit::OwnId::Doi(prefix) => {
                        let doi = crate::resolver::cards::doi(&format!("doi:{prefix}1"));
                        assert_eq!(doi, Some(format!("{prefix}1")), "{}", source.name());
                        assert!(prefix.ends_with(['.', '/']), "{}: {prefix}", source.name());
                        "doi"
                    }
                    kit::OwnId::Wikidata => "wikidata",
                };
                assert!(source.facts().contains(&fact), "{}", source.name());
                assert!(!source.hooks_pages(), "{}", source.name());
            }
        }
    }

    /// The join keys of a card: its own ids, each normalized, and none of a card that no
    /// official source read or that a page hook made.
    #[test]
    fn own_keys_read_the_declared_facts() {
        let Some((source, prefix)) = REGISTRY.iter().find_map(|s| {
            s.own_ids().iter().find_map(|o| match o {
                kit::OwnId::Doi(prefix) => Some((*s, *prefix)),
                kit::OwnId::Wikidata => None,
            })
        }) else {
            return;
        };
        let mut doc = ResourceMeta::new("https://example.org/r/1", source.name(), 60);
        let own = format!("{}1", prefix.to_ascii_uppercase());
        doc.attributes
            .insert("doi".into(), format!("{own}; {prefix}1; {prefix}2"));
        let (via, keys) = own_keys(&doc).unwrap();
        assert_eq!(via, source.name());
        assert_eq!(keys, [format!("doi:{prefix}1"), format!("doi:{prefix}2")]);
        // A value that is no own DOI: none of the values is taken, so one typed value holding
        // `; ` never yields a part of it.
        for typed in [
            "10.1000/other".to_owned(),
            format!("10.1000/other; {prefix}1"),
            format!("{prefix}1; not a doi"),
        ] {
            doc.attributes.insert("doi".into(), typed.clone());
            assert_eq!(own_keys(&doc), None, "{typed}");
        }
        for name in ["web", "jsonld"] {
            let mut doc = doc.clone();
            doc.source = name.into();
            doc.attributes.insert("doi".into(), format!("{prefix}1"));
            assert_eq!(own_keys(&doc), None, "{name}");
        }
        if let Some(hook) = REGISTRY.iter().find(|s| s.hooks_pages()) {
            let mut doc = doc.clone();
            doc.source = hook.name().into();
            doc.attributes.insert("doi".into(), format!("{prefix}1"));
            doc.attributes.insert("wikidata".into(), "Q42".into());
            assert_eq!(own_keys(&doc), None, "{}", hook.name());
        }
    }

    /// Everything that reads with an operator's credential has its refusals series: each official
    /// source and also-on source that takes a token or an app's key, the client-credentials
    /// services, Podcast Index, TMDB and each event platform that requires a key. No other name
    /// is counted, so the series' labels stay these.
    #[test]
    fn every_keyed_service_is_counted_by_name() {
        let mut keyed: Vec<&str> = REGISTRY
            .iter()
            .filter(|s| s.takes_token())
            .map(|s| s.name())
            .collect();
        keyed.extend(
            crate::also_on::registered()
                .iter()
                .filter(|s| s.takes_token() || s.app_key_variables().is_some())
                .map(|s| s.id()),
        );
        keyed.extend(
            crate::event::adapters::definitions()
                .iter()
                .filter(|d| d.requires_key)
                .map(|d| d.id.as_str()),
        );
        keyed.extend([
            crate::also_on::spotify_api::SPOTIFY.service,
            crate::also_on::tidal_api::TIDAL.service,
            "podcast_index",
            "tmdb",
        ]);
        for name in &keyed {
            assert!(KEYED.contains(name), "{name} is not in KEYED");
        }
        assert!(keyed.contains(&"ticketmaster"), "{keyed:?}");
        token_refused("not-a-keyed-source");
        assert!(token_refusals().iter().all(|(s, _)| KEYED.contains(s)));
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
                // A link is one source's: the first in the registry that reads it gets it, so
                // an example no other source may read is never shadowed.
                if source.reads(example.uri) {
                    for other in REGISTRY.iter().filter(|s| s.name() != name) {
                        assert!(
                            !other.reads(example.uri),
                            "{} also reads {name}'s {}",
                            other.name(),
                            example.uri
                        );
                    }
                    assert_eq!(reading(example.uri).map(|s| s.name()), Some(name));
                }
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
            for (host, rate) in source.budgets() {
                assert!(rate.is_finite() && *rate > 0.0 && !host.is_empty());
            }
        }
    }

    /// A link with a port, a user name or a password is not a service's own link form: the
    /// source that reads the plain link neither reads it nor merges it into the object's card.
    #[test]
    fn a_link_with_a_port_or_a_user_name_is_no_sources_link() {
        let mut checked = 0;
        for source in REGISTRY {
            for example in source.examples() {
                let Some(rest) = example.uri.strip_prefix("https://") else {
                    continue;
                };
                if !source.reads(example.uri) {
                    continue;
                }
                let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
                for odd in [
                    format!("https://{host}:8443/{path}"),
                    format!("https://user@{host}/{path}"),
                    format!("https://user:secret@{host}/{path}"),
                ] {
                    assert!(!source.reads(&odd), "{} reads {odd}", source.name());
                    assert_eq!(source.alias_root(&odd), None, "{odd}");
                    checked += 1;
                }
            }
        }
        assert!(checked > 0 || REGISTRY.is_empty());
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

    /// Link and root forms beyond the sources' examples, one or more per id shape.
    const FORMS: &[&str] = &[
        "btc:tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d",
        "btc-testnet4:block/00000000b747d47c3b38161693ad05e26924b3775a8be669751f969da836311e",
        "btc:address/bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
        "ln:node/03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
        "ln:channel/892785849701564416",
        "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.001",
        "https://mempool.space/de/signet/lightning/channel/811984x2037x0",
        "https://mempool.space/api/v1/block/0000000000000000001c8018d9cb3b742ef25114f27563e3fc4a1902167f9893",
        "usgs:official20110311054624120_30",
        "https://earthquake.usgs.gov/fdsnws/event/1/query?eventid=ci38457511&format=geojson",
        "https://earthquake.usgs.gov/earthquakes/feed/v1.0/detail/ci38457511.geojson",
        "docker:library/nginx",
        "https://hub.docker.com/v2/namespaces/lncm/repositories/bitcoind",
        "ror:03yrm5c26",
        "https://api.ror.org/v2/organizations/03yrm5c26",
        "hf:model/meta-llama/llama-2-7b-hf",
        "https://hf.co/meta-llama/Llama-2-7b-hf/tree/main",
        "zenodo:1234567",
        "https://zenodo.org/api/records/1234567",
        "figshare:1018700",
        "https://api.figshare.com/v2/articles/1018700/versions/1",
        "met:436535",
        "https://collectionapi.metmuseum.org/public/collection/v1/objects/436535",
        "cleveland:1958.39",
        "https://openaccess-api.clevelandart.org/api/artworks/135382",
        "artic:27992",
        "https://api.artic.edu/api/v1/artworks/27992/a-sunday-on-la-grande-jatte",
        "ia:bitcoin-whitepaper",
        "https://archive.org/metadata/bitcoin-whitepaper/metadata",
        "commons:File:%C3%89t%C3%A9_%C3%A0_Paris.jpg",
        "https://commons.wikimedia.org/w/index.php?title=File:Bitcoin.svg&action=history",
        "https://commons.wikimedia.org/w/index.php?curid=1234",
        "pdb:4HHB",
        "https://www.rcsb.org/pdb/explore.do?structureId=pdb_00004hhb",
        "uniprot:P69905",
        "https://rest.uniprot.org/uniprotkb/HBA_HUMAN.json",
        "pubchem:cid/2244",
        "https://pubchem.ncbi.nlm.nih.gov/summary/summary.cgi?cid=2244",
        "gbif:occurrence/5938027305",
        "https://www.gbif.org/zh-tw/species/1341976/metrics",
        "inat:observation/333069440",
        "https://www.inaturalist.org/taxa/47219-Apis-mellifera",
        "nct:NCT04368728",
        "https://classic.clinicaltrials.gov/ct2/show/record/NCT04368728?term=x",
    ];

    /// Every spelling of `link` with one multibyte character in it, at every position: two-,
    /// three- and four-byte characters, a combining mark alone and on a letter, and the two- and
    /// four-byte ones percent-encoded (a decoded string's offsets differ from the written
    /// one's) inserted; and a character in place of as many ASCII bytes as it is long, so a
    /// length check in bytes passes and only a byte-based slice is unsafe.
    fn multibyte_spellings(link: &str) -> Vec<String> {
        let mut out = vec![];
        for (at, _) in link.char_indices().chain([(link.len(), ' ')]) {
            for insert in [
                "é",
                "€",
                "😀",
                "\u{301}",
                "e\u{301}",
                "%C3%A9",
                "%F0%9F%98%80",
            ] {
                out.push(format!("{}{insert}{}", &link[..at], &link[at..]));
            }
            for replace in ["é", "€", "😀"] {
                let end = at + replace.len();
                if link.get(at..end).is_some_and(|s| s.is_ascii()) {
                    out.push(format!("{}{replace}{}", &link[..at], &link[end..]));
                }
            }
        }
        out
    }

    /// Link-derived text may hold any character: nothing the ingest path asks of a link (which
    /// source reads it, its static root, whether it is invalid, its route) may slice a string
    /// inside one. Every example, its root, every form above and every address the sources'
    /// docs pages show, in every multibyte spelling; the spellings are shared out over the
    /// cores (about two milliseconds each across the sources).
    #[test]
    fn a_multibyte_character_anywhere_in_a_link_never_panics() {
        let mut links: Vec<String> = FORMS.iter().map(|s| (*s).to_owned()).collect();
        for source in REGISTRY {
            for example in source.examples() {
                links.push(example.uri.to_owned());
                links.extend(source.alias_root(example.uri));
            }
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
        links.sort_unstable();
        links.dedup();
        assert!(links.len() > 40, "{}", links.len());
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(16));
        let checked: usize = std::thread::scope(|scope| {
            let workers: Vec<_> = links
                .chunks(links.len().div_ceil(threads))
                .map(|links| {
                    scope.spawn(move || {
                        let mut checked = 0;
                        for link in links {
                            for uri in multibyte_spellings(link) {
                                let outcome = std::panic::catch_unwind(|| {
                                    for source in REGISTRY {
                                        source.reads(&uri);
                                        source.alias_root(&uri);
                                        source.invalid(&uri);
                                    }
                                    alias_root(&uri);
                                    crate::alias::root(&uri);
                                    crate::resolver::classify(&uri);
                                });
                                assert!(outcome.is_ok(), "panics on {uri:?}");
                                checked += 1;
                            }
                        }
                        checked
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).sum()
        });
        assert!(checked > 10_000, "{checked}");
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

    /// A refusal is counted and switches the key off for an hour from the latest refusal; the
    /// latch then expires on its own (a paused clock stands in for the hour).
    #[tokio::test(start_paused = true)]
    async fn a_refused_key_is_off_for_an_hour() {
        let count = || {
            token_refusals()
                .into_iter()
                .find(|(s, _)| *s == "tmdb")
                .map_or(0, |(_, n)| n)
        };
        let refusals = Refusals::new("tmdb");
        let before = count();
        assert!(!refusals.latched() && refusals.check().is_ok());
        assert!(refusals.refused(), "the first refusal of a latch warns");
        assert!(count() > before);
        assert!(refusals.check().is_err_and(|e| e.is::<Declined>()));
        tokio::time::advance(std::time::Duration::from_secs(1800)).await;
        assert!(!refusals.refused(), "a refusal while latched warns no more");
        tokio::time::advance(std::time::Duration::from_secs(3599)).await;
        assert!(refusals.latched(), "an hour from the latest refusal");
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert!(!refusals.latched() && refusals.check().is_ok());
        assert!(refusals.refused());
    }

    #[test]
    fn a_403_names_the_key_in_its_own_words() {
        assert!(names_the_key(br#"{"detail":"Invalid token."}"#, "detail"));
        assert!(names_the_key(
            br#"{"detail":"Authentication credentials were not provided."}"#,
            "detail"
        ));
        assert!(!names_the_key(
            br#"{"detail":"You do not have permission to perform this action."}"#,
            "detail"
        ));
        assert!(!names_the_key(b"<html>Forbidden</html>", "detail"));
        assert!(!names_the_key(br#"{"message":"Invalid token."}"#, "detail"));
    }

    /// One case rule for identifier roots: the lower-case spelling of a root whose canonical form
    /// is in upper case is a spelling of that root (its alias), never an invalid link.
    #[test]
    fn a_root_in_another_letter_case_is_its_spelling() {
        for (source, spelling, root) in [
            ("clinicaltrials", "nct:nct04368728", "nct:NCT04368728"),
            ("pdb", "pdb:4hhb", "pdb:4HHB"),
            ("uniprot", "uniprot:p69905", "uniprot:P69905"),
            (
                "gleif",
                "lei:ky37lus27qqx7bb93l28",
                "lei:KY37LUS27QQX7BB93L28",
            ),
            ("federal_register", "fr:e9-12345", "fr:E9-12345"),
        ] {
            if by_name(source).is_none() {
                continue;
            }
            assert!(!invalid(spelling), "{spelling}");
            assert_eq!(
                alias_root_with(spelling, None).as_deref(),
                Some(root),
                "{spelling}"
            );
            assert_eq!(alias_root_with(root, None), None, "{root}");
            assert!(!invalid(root), "{root}");
        }
    }

    /// The social shapes whose pages never show the object to a request without an account are
    /// link rules: their card is the link's root and shape (with what the link spells), and no
    /// request is made.
    #[tokio::test(start_paused = true)]
    async fn link_only_social_shapes_ask_nothing() {
        for (source, uri, type_name, author) in [
            (
                "facebook",
                "https://www.facebook.com/facebook/posts/10162533193146729",
                "SocialMediaPosting",
                Some("facebook"),
            ),
            (
                "facebook",
                "https://www.facebook.com/watch/?v=123456789",
                "VideoObject",
                None,
            ),
            (
                "tiktok",
                "https://www.tiktok.com/@scout2015/video/6718335390845095173",
                "VideoObject",
                Some("scout2015"),
            ),
            (
                "linkedin",
                "https://www.linkedin.com/posts/linkedin_activity-7517918134654218241-7OWA",
                "SocialMediaPosting",
                Some("linkedin"),
            ),
            (
                "twitch",
                "https://www.twitch.tv/twitch/clip/AwkwardHelplessSalamanderSwiftRage",
                "VideoObject",
                Some("twitch"),
            ),
        ] {
            let Some(s) = by_name(source) else {
                continue;
            };
            let (egress, recorded) = fixture::egress(vec![]);
            let cx = Context {
                egress: &egress,
                ttl: 3600,
            };
            let doc = s.resolve(&cx, uri).await.unwrap();
            assert!(recorded.asked().is_empty(), "{uri}");
            assert_eq!(doc.status, Status::Ok, "{uri}");
            assert_eq!(doc.type_name, type_name, "{uri}");
            assert!(doc.source_root.is_some(), "{uri}");
            assert!(!doc.attributes.contains_key("blocked"), "{uri}");
            assert_eq!(
                doc.attributes.get("author").map(String::as_str),
                author,
                "{uri}"
            );
        }
    }

    /// A social profile is social content, and its bio is published without e-mail addresses.
    #[tokio::test(start_paused = true)]
    async fn a_profiles_bio_publishes_no_address() {
        let Some(x) = by_name("x") else {
            return;
        };
        let uri = "https://x.com/someone";
        let page = br#"<html><head><meta property="og:title" content="Some One (@someone) on X"><meta property="og:description" content="Builds bridges. Write to someone@example.org or mailto:me@example.org"></head></html>"#;
        let (egress, _) = fixture::egress(vec![(uri, 200, "text/html", page.to_vec())]);
        let doc = resolve(&egress, &Config::default(), uri, 3600)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "Person");
        assert!(x.social(&doc), "a profile is social content");
        let bio = doc.description.unwrap();
        assert!(bio.starts_with("Builds bridges."), "{bio}");
        assert!(!bio.contains('@'), "{bio}");
    }

    /// A social card takes its name from the object's own page only. Each recorded page (one real
    /// page per platform, as the platform served it to this source's request) names its object;
    /// the same page reached from another object's link (a redirect) names nothing, and that link
    /// gets the plain card of its shape.
    #[tokio::test(start_paused = true)]
    async fn a_social_card_is_named_from_the_objects_own_page_only() {
        for (source, own, file, name, moved, plain) in [
            (
                "facebook",
                "https://www.facebook.com/facebook",
                "own-page-facebook.html.gz",
                "Facebook",
                "https://www.facebook.com/moved.name",
                "Profile on facebook.com",
            ),
            (
                "linkedin",
                "https://www.linkedin.com/company/linkedin",
                "own-page-company-linkedin.html.gz",
                "LinkedIn",
                "https://www.linkedin.com/company/moved-name",
                "Profile on linkedin.com",
            ),
            (
                "pinterest",
                "https://www.pinterest.com/pinterest/",
                "own-page-profile-pinterest.html.gz",
                "Pinterest",
                "https://www.pinterest.com/moved_name/",
                "Profile on pinterest.com",
            ),
            (
                "twitch",
                "https://www.twitch.tv/twitch",
                "own-page-channel-twitch.html.gz",
                "Twitch",
                "https://www.twitch.tv/moved_name",
                "Profile on twitch.tv",
            ),
            (
                "telegram",
                "https://t.me/telegram",
                "own-page-channel-telegram.html",
                "Telegram News",
                "https://t.me/moved_name",
                "Profile on t.me",
            ),
            (
                "x",
                "https://x.com/jack",
                "own-page-profile-x.html.gz",
                "jack",
                "https://x.com/moved_name",
                "Profile on x.com",
            ),
        ] {
            let Some(s) = by_name(source) else {
                continue;
            };
            let (egress, _) = fixture::egress(vec![
                (own, 200, "text/html", fixture::read(source, file)),
                (moved, 302, "text/html", own.as_bytes().to_vec()),
            ]);
            let cx = Context {
                egress: &egress,
                ttl: 3600,
            };
            let doc = s.resolve(&cx, own).await.unwrap();
            assert_eq!(doc.name.as_deref(), Some(name), "{own}");
            let doc = s.resolve(&cx, moved).await.unwrap();
            assert_eq!(doc.status, Status::Ok, "{moved}");
            assert_eq!(doc.name.as_deref(), Some(plain), "{moved}");
            assert_eq!(doc.description, None, "{moved}");
            assert_eq!(doc.image, None, "{moved}");
            assert_eq!(doc.source_root.as_deref(), Some(moved), "{moved}");
        }
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
        // A block for a source the build does not have: ignored while it only switches it, an
        // error once it carries a token.
        config.sources.0.insert(
            "nonesuch".into(),
            Switch {
                enabled: false,
                ..Default::default()
            },
        );
        assert!(config.sources.validate().is_ok());
        config.sources.0.insert(
            "nonesuch".into(),
            Switch {
                token: Some(String::from("abc").into()),
                ..Default::default()
            },
        );
        assert!(config.sources.validate().is_err());
        config.sources.0.insert(
            "nonesuch".into(),
            Switch {
                key: Some(String::from("abc").into()),
                ..Default::default()
            },
        );
        assert!(config.sources.validate().is_err());
        config.sources.0.remove("nonesuch");
        // An app key only where the source sends one.
        config.sources.0.insert(
            source.name().into(),
            Switch {
                key: Some(String::from("abc").into()),
                ..Default::default()
            },
        );
        assert_eq!(config.sources.validate().is_ok(), source.takes_key());
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
        assert_eq!(
            markdown_excerpt("*New* in _this_ release: `x`"),
            "New in this release: x"
        );
        // Hostile input costs no more than its first 2 KB: milliseconds of this thread's CPU
        // time, where a pass over the whole input took seconds.
        for hostile in [
            "[]()".repeat(262_144),
            "[a](b)".repeat(174_763),
            "é".repeat(600_000),
        ] {
            let started = crate::capability::thread_cpu_time();
            let excerpt = markdown_excerpt(&hostile);
            let used = crate::capability::thread_cpu_time() - started;
            assert!(used < std::time::Duration::from_secs(1), "{used:?}");
            assert!(excerpt.chars().count() <= 201);
        }
        assert_eq!(
            without_addresses("Mail <ann@example.org> or mailto:bob@x.org, not @user@host or a@b"),
            "Mail or not @user@host or a@b"
        );
    }

    #[test]
    fn markdown_as_plain_text() {
        let meetup = "Bitcoin Switzerland\n*This event is local and held in German and English*\n\n**Our casual Bitcoin Zurich Meetup is for newcomers.**\n\nOpen discussion ;-)\n*\\*\\*\\* Don't be a NO SHOW ! \\*\\*\\**\n\n- [Map](https://x.example/m)\n* item two\n## Agenda\n> quoted";
        assert_eq!(
            markdown_plain(meetup),
            "Bitcoin Switzerland\nThis event is local and held in German and English\n\nOur casual Bitcoin Zurich Meetup is for newcomers.\n\nOpen discussion ;-)\n*** Don't be a NO SHOW ! ***\n\nMap\nitem two\nAgenda\nquoted"
        );
        for kept in [
            "5 * 3 = 15",
            "2*3*4",
            "snake_case_name",
            "a * b * c",
            "price *",
            "_ alone",
            "C:\\path",
            "#hashtag",
        ] {
            assert_eq!(markdown_plain(kept), kept, "{kept}");
        }
        assert_eq!(markdown_plain("say *hi*, _you_."), "say hi, you.");
        assert_eq!(markdown_plain("a *b* 5 * 3 *c d*"), "a b 5 * 3 c d");
        assert_eq!(markdown_plain("\n\n\nx\n\n\n\ny\n\n"), "x\n\ny");
        assert_eq!(markdown_plain(""), "");
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
        // At the cap, renewing a known host's answer evicts nothing.
        let full = HostMemo::default();
        for n in 0..HostMemo::CAP {
            full.set(&format!("host{n}.example"), true);
        }
        full.set("host7.example", false);
        assert_eq!(full.get("host0.example"), Some(true));
        assert_eq!(full.get("host7.example"), Some(false));
        // A new host takes the oldest answer's place.
        full.set("new.example", true);
        assert_eq!(full.get("host0.example"), None);
        assert_eq!(full.get("new.example"), Some(true));
    }

    /// What a service answers is never the daemon's own budget wait: a request timeout and a
    /// rate limit are `http_4xx`, a server error `upstream`.
    #[test]
    fn a_services_answer_is_not_the_daemons_own_wait() {
        let answer = |status: u16| {
            Fetched::answer(
                url::Url::parse("https://api.example/x").unwrap(),
                status,
                "application/json".into(),
                vec![],
            )
        };
        for (status, class) in [(408, "http_4xx"), (429, "http_4xx"), (503, "upstream")] {
            let error = retry(&answer(status)).unwrap_err();
            let wait = error
                .downcast_ref::<crate::resolver::RetryLater>()
                .expect("a retry");
            assert_eq!(wait.class, class, "{status}");
            assert!(wait.seconds >= 60);
            assert!(!own_rate_limit(&error), "{status}");
        }
        assert!(retry(&answer(404)).is_ok());
        let own = anyhow::Error::new(crate::resolver::RetryLater {
            class: "rate_limited",
            seconds: 7,
        });
        assert!(own_rate_limit(&own));
    }

    /// A host two sources name gets the lower rate, in either order.
    #[test]
    fn the_lowest_rate_of_a_host_wins() {
        let rates = [("a.example", 2.0), ("b.example", 0.5), ("a.example", 0.2)];
        for rates in [rates.to_vec(), rates.iter().rev().copied().collect()] {
            let lowest: Vec<_> = lowest(rates.into_iter()).into_iter().collect();
            assert_eq!(lowest, [("a.example", 0.2), ("b.example", 0.5)]);
        }
    }

    /// A switched-off source paces no host while its also-on half (if it has one) is off too, and
    /// a source that needs a token paces none without one. While the half is on, it reads the
    /// same service, so the host keeps its pace.
    #[test]
    fn only_a_switched_on_source_paces_its_host() {
        let paced = |config: &Config, host: &str| config.egress.source_rates.contains_key(host);
        let elsewhere = |name: &str, host: &str| {
            REGISTRY
                .iter()
                .filter(|s| s.name() != name)
                .any(|s| s.budgets().iter().any(|(h, _)| *h == host))
                || crate::also_on::budgets().any(|(h, _)| h == host)
        };
        let on = Config::default();
        for source in REGISTRY {
            for (host, rate) in source.budgets() {
                if elsewhere(source.name(), host) {
                    continue;
                }
                let name = source.name();
                assert_eq!(
                    paced(&on, host),
                    !source.takes_token(),
                    "{name}: {host} with the default switches"
                );
                if source.takes_token() {
                    continue;
                }
                assert_eq!(on.egress.source_rates[*host], *rate, "{name}");
                let mut value = Config::defaults().unwrap();
                value["sources"][name] = serde_json::json!({ "enabled": false });
                let off = Config::from_value(value.clone()).unwrap();
                let half = crate::also_on::registered().iter().any(|s| s.id() == name);
                assert_eq!(
                    paced(&off, host),
                    half,
                    "{name} is off; its also-on half reads {host}: {half}"
                );
                if half {
                    value["also_on"]["sources"][name] = serde_json::json!({ "enabled": false });
                    let both = Config::from_value(value).unwrap();
                    assert!(!paced(&both, host), "{name} and its half are off: {host}");
                }
            }
        }
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
