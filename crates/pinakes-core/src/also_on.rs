//! "Also on": the same thing on other services, as a hint on each card (`Card::also_on`).
//!
//! A [`Source`] reads one resource and returns [`Found`]: exact join keys it vouches for (two
//! resources with the same key join a group) and derived links (the same thing on a service
//! nobody tagged). The worker ([`run`]) runs every enabled source that applies to a resource
//! after a save that changed it and again when the shortest source lifetime has passed; each
//! run replaces that source's rows for the resource ([`record`]). Derived links are never
//! fetched, never counted as references and never ranked; the card lists them after the tagged
//! members of its group (`also_on()` in the schema).
//!
//! A source is a struct implementing [`Source`], listed in [`registry`]. Its requests go
//! through [`Egress`] (the per-host budgets apply), and a page's own claims (`sameAs`, an ISRC
//! in its text) are never keys: a key comes from the service that owns the identifier, or from
//! an exact hub lookup (MusicBrainz, Wikidata).
use crate::{config::Config, model::ResourceMeta, resolver::egress::Egress};

pub mod books;
pub mod client_credentials;
pub mod deezer;
pub mod editions;
pub mod films;
pub mod itunes;
pub mod itunes_song;
pub mod media;
pub mod musicbrainz;
pub mod musicbrainz_containment;
pub mod musicbrainz_entity;
pub mod performers;
pub mod podcasts;
pub mod product_keys;
pub mod service;
pub mod spotify;
pub mod spotify_api;
pub mod spotify_isrc;
pub mod tidal;
pub mod tidal_isrc;
pub mod tmdb;
pub mod wikidata;
pub mod wikidata_item;
pub mod wikipedia;
pub mod youtube_match;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::watch;

/// `[also_on]`: the switch, whether derived links are kept, and per-source switches.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlsoOnConfig {
    pub enabled: bool,
    /// Keep the derived links sources find (off: keys only, so groups still form).
    pub derive_links: bool,
    /// `[also_on.sources.<id>] enabled = false` turns one source off; the IDs are
    /// [`registry`]'s.
    pub sources: BTreeMap<String, SourceOptions>,
    /// The iTunes storefront (country) the album and book lookups ask, `us` by default. Apple
    /// Music links carry no storefront (`music::parse`).
    pub apple_storefront: String,
    /// Read the public page of a Spotify track a card may show for the album it is on
    /// (`editions`), where Spotify's Web API does not answer for it: on by default.
    pub spotify_pages: bool,
    #[serde(skip)]
    pub links_per_source: usize,
    #[serde(skip)]
    pub keys_per_source: usize,
    #[serde(skip)]
    pub facts_per_source: usize,
    #[serde(skip)]
    pub concurrency: usize,
    #[serde(skip)]
    pub retry_secs: u64,
}
impl Default for AlsoOnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            derive_links: true,
            sources: BTreeMap::new(),
            apple_storefront: "us".into(),
            spotify_pages: true,
            links_per_source: 32,
            keys_per_source: 16,
            facts_per_source: 32,
            concurrency: 2,
            retry_secs: 300,
        }
    }
}
impl AlsoOnConfig {
    pub fn source_enabled(&self, source: &dyn Source) -> bool {
        self.enabled && self.source_on(source)
    }
    /// Whether the operator has the source on (`[also_on.sources.<id>] enabled`, else its
    /// default), whether or not the worker runs (`[also_on] enabled`).
    pub fn source_on(&self, source: &dyn Source) -> bool {
        self.sources
            .get(source.id())
            .and_then(|s| s.enabled)
            .unwrap_or(source.enabled_by_default())
    }
    /// The MusicBrainz web service: `[also_on.sources.musicbrainz] base_url`, or the public one.
    pub fn musicbrainz_base(&self) -> &str {
        self.sources
            .get("musicbrainz")
            .and_then(|s| s.base_url.as_deref())
            .unwrap_or(crate::hubs::musicbrainz::BASE)
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceOptions {
    pub enabled: Option<bool>,
    /// The web service a hub source asks (`musicbrainz` only: a mirror's `…/ws/2`).
    pub base_url: Option<String>,
    /// A service's personal token, for a source that takes one (`Source::takes_token`); never
    /// shown or logged.
    pub token: Option<crate::event::adapters::SecretKey>,
    /// An app's consumer key and secret, for a source that takes them
    /// (`Source::app_key_variables`); never shown or logged.
    pub key: Option<crate::event::adapters::SecretKey>,
    pub secret: Option<crate::event::adapters::SecretKey>,
}

/// One resource as a source sees it.
#[derive(Clone, Debug)]
pub struct Subject {
    pub id: String,
    pub uri: String,
    /// `ok`, `missing`, `unreachable`, … (a source for gone pages reads the others).
    pub status: String,
    /// The schema.org type, if resolved.
    pub type_name: Option<String>,
    /// The stored document, if resolved.
    pub doc: Option<ResourceMeta>,
    /// The keys other sources found for this resource (so a source can start from a hub's
    /// MBID or Q-id), with the source that found each.
    pub keys: Vec<(String, String)>,
    /// When the resource was first referenced (for an archived copy closest to that time).
    pub first_seen_at: Option<DateTime<Utc>>,
    /// The links the same thing has elsewhere, as its card may show them: those the sources before
    /// in this run (and their earlier runs) derived for it and for the other members of its group,
    /// and those members' own links; at most [`CANDIDATES`]. Loaded only for a source that reads
    /// them ([`Source::reads_candidates`], `editions`), just before it runs.
    pub candidates: Vec<String>,
}
/// At most this many links a [`Subject`] carries as candidates.
pub const CANDIDATES: i64 = 256;
impl Subject {
    /// The keys in one namespace (`mb:recording`, `wikidata`, …), without the prefix.
    pub fn keys_in<'a>(&'a self, namespace: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.keys.iter().filter_map(move |(key, _)| {
            key.strip_prefix(namespace)
                .and_then(|rest| rest.strip_prefix(':'))
        })
    }
}

/// What a source found for one resource. Empty means the service has nothing for it; an error
/// from [`Source::lookup`] means "retry later" and keeps the previous rows until they expire.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub keys: Vec<Key>,
    pub links: Vec<Link>,
    /// Facts about the object (`isrc`, `upc`, `artist`, …): shown and searchable, never joined.
    pub facts: Vec<Fact>,
    /// What was found changes the card itself (a media page's title): the resource is read again
    /// now, and its finished card takes it.
    pub reread: bool,
    /// A secondary service the source asks failed, so its part is missing from this run: what
    /// the run found stands, and the worker brings the run back after its backoff, as for a
    /// failed run (five minutes, doubling to an hour), or after this wait (the one the service
    /// named) when longer; never after the source's lifetime.
    pub retry_after: Option<Duration>,
    /// Where links of the resource's group sit among its releases (`link_release`).
    pub editions: Vec<Edition>,
    /// Links whose place this run could not tell (a lookup failed, credentials were refused, a
    /// list was cut short, a cap was reached): their earlier places stay as recorded.
    pub keep: Vec<String>,
}
/// A link placed for one card: on a MusicBrainz release, `rank` 0 when it is the card's own, then
/// 1, 2, … for the original edition's releases by date; or (rank 1000, no `release`) on another
/// release, as its service says. The lowest rank of a service shows (`also_on()`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edition {
    pub uri: String,
    pub release: Option<String>,
    pub rank: i16,
}
/// What a run placed, as stored: each placed link's ID and place, and the IDs of the links whose
/// places stay.
#[derive(Default)]
struct Placed {
    editions: Vec<(String, Edition)>,
    keep: Vec<String>,
}
/// A fact a source reads for a resource: a lower-case `name` (`[a-z][a-z0-9_]`, at most 32
/// bytes) and a printable `value` (at most 512 bytes). Several values of one name are kept.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fact {
    pub name: String,
    pub value: String,
}
impl Fact {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}
/// An exact join key, `{namespace}:{value}` (`mb:recording:{mbid}`, `wikidata:Q42`, `isrc:…`,
/// `upc:…`). An ISRC or a barcode is a key only where a source vouches for it: a service's own
/// record of its track or album (TIDAL, Deezer), or a MusicBrainz match that names one recording
/// (`music_sources::isrc_key`; a shared ISRC stays a fact). `via` names the hub it came through.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub key: String,
    pub via: String,
}
/// A derived link: `site` is the service's name as a card shows it ("Apple Music"), `via` the
/// hub or service it came through (`musicbrainz`, `wikidata`, `itunes`), `kind` a qualifier
/// such as `archived`, `at` a time that belongs to the link (an archived copy's capture).
/// `key` is the exact key that named it: the hub entity or item it is a relation or claim of
/// (`mb:recording:…`, `wikidata:Q…`), or the barcode or ISRC a service's lookup answered with
/// exactly this link. A tagged resource that is this link takes the key and joins the group;
/// one that two entities of one single-object namespace name carries both and joins no group
/// (`groups::single_object`). Set it only for
/// an exact derivation, never for a search or a name match.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub uri: String,
    pub site: String,
    pub via: String,
    pub kind: Option<String>,
    pub at: Option<DateTime<Utc>>,
    pub key: Option<String>,
}
impl Link {
    pub fn new(uri: impl Into<String>, site: impl Into<String>, via: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            site: site.into(),
            via: via.into(),
            kind: None,
            at: None,
            key: None,
        }
    }
    /// This link, named by an exact lookup of `key`.
    pub fn keyed(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }
    /// This link as a match by metadata (an artist, a title, a length), not an exact ID: possibly
    /// the same thing. Its `kind` is [`MAYBE`]; it never carries a key, so it never joins a
    /// group, and a card shows it marked so.
    pub fn maybe(mut self) -> Self {
        self.kind = Some(MAYBE.into());
        self.key = None;
        self
    }
}
/// The `kind` of a derived link that is a match by metadata, possibly the same thing
/// ([`Link::maybe`]).
pub const MAYBE: &str = "maybe";

/// What a source's lookup may use.
#[derive(Clone)]
pub struct Context {
    pub egress: Egress,
    pub config: Arc<Config>,
}

/// One source of keys and derived links. `id` is its config name (`[also_on.sources.<id>]`)
/// and the `source` of its rows: lower-case letters, digits and `_`, at most 32 bytes.
#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &'static str;
    /// Whether this source reads `subject` (by its URI's service, type or keys). No I/O.
    fn applies(&self, subject: &Subject) -> bool;
    /// How long its rows live before it runs again (they expire after twice that, so one
    /// failed refresh keeps them).
    fn ttl(&self) -> Duration {
        Duration::from_secs(30 * 86_400)
    }
    /// Before a batch of resources (their URIs): ask the service, in as few requests as it
    /// allows, what this source will ask for each, so their lookups find it cached. Best effort.
    async fn prefetch(&self, _cx: &Context, _uris: &[String]) {}
    /// Whether it runs without `[also_on.sources.<id>] enabled = true`.
    fn enabled_by_default(&self) -> bool {
        true
    }
    /// Whether the configuration lets it run (a service that needs the operator's credentials
    /// is off without them).
    fn ready(&self, _config: &Config) -> bool {
        true
    }
    /// Whether it counts as switched off while it is not [`ready`](Source::ready): its stored
    /// rows are hidden at once (`also_on_source_off`) and its API hosts refused, as with
    /// `enabled = false`, instead of being read until they expire. For a service whose data may
    /// be shown only while the operator uses its API.
    fn off_until_ready(&self) -> bool {
        false
    }
    /// The facts of its own that a resolved card takes into its fields when it is typed
    /// ([`fill_card`](Source::fill_card)): loaded with the card's type hints
    /// (`media::TypeHints::filled`) and fingerprinted in `ResourceMeta::hinted_by`.
    fn fills(&self) -> &'static [&'static str] {
        &[]
    }
    /// Write into a resolved, typed card what its facts of [`fills`](Source::fills) give
    /// (`facts`: the live ones, none when it is off), first taking out the values it wrote before
    /// (those credited to it). Whether it wrote anything. No I/O.
    fn fill_card(&self, _facts: &[Fact], _doc: &mut ResourceMeta) -> bool {
        false
    }
    /// The longest `limits.change_retention_days` its values may be kept in change lines while
    /// it is ready (`Config::validate` refuses more): for a service that limits how long its data
    /// is cached.
    fn retention_days_cap(&self) -> Option<u32> {
        None
    }
    /// The credit provider of the values it writes ([`fill_card`](Source::fill_card)): one of
    /// `CreditProvider::FROM_FACTS`. A card holding such a value whose source this build lacks
    /// loses it (`media::fill`, [`refill`]).
    fn fills_as(&self) -> Option<crate::model::CreditProvider> {
        None
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found>;
    /// The key namespaces it records that name one object each: a group holds at most one value
    /// of each (`groups::single_object`).
    fn single_namespaces(&self) -> &'static [&'static str] {
        &[]
    }
    /// The join key a search names in one of its namespaces (`key={namespace}:{value}`),
    /// normalized as it records it; `None` for another namespace or a value that is no key.
    fn search_key(&self, _namespace: &str, _value: &str) -> Option<String> {
        None
    }
    /// Every form of key it records, on its resource or on a derived link: the namespace (the key
    /// up to its value, `openlibrary:work`) and an example value. A source that records keys
    /// lists them: each searches back as recorded (`key=`, tested by `api`'s
    /// `every_key_form_searches_back`), and in a debug build it may record no other form.
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }
    /// The plain name of a service it reads or links to, by a link's host (lower case, without
    /// `www.`) and URL: the name its derived links carry (`lookups::service_name`). Only for an
    /// object's own page in a form it recognizes; any other page on that host is on no service.
    fn service_name(&self, _host: &str, _url: &url::Url) -> Option<&'static str> {
        None
    }
    /// Its page in docs/sources: its own (`<id>.md`), unless it is the also-on half of an official
    /// source of another name. A lookup family's sources share their family's page.
    fn page(&self) -> &'static str {
        self.id()
    }
    /// The published limits of the services only it reads, as `(host, requests_per_second)`:
    /// each lowers its host's default pace (`limits.host_rates` overrides it), as an official
    /// source's budgets do. Services several sources or the resolver read keep theirs in
    /// `resolver::egress::PUBLISHED_RATES`.
    fn budgets(&self) -> &'static [(&'static str, f64)] {
        &[]
    }
    /// The hosts that serve its API and nothing else (no page anyone links to): while it is
    /// switched off the egress refuses them ([`off_hosts`]), so no path calls its API. Its site's
    /// own pages stay readable, and links to them from other sources stay.
    fn api_hosts(&self) -> &'static [&'static str] {
        &[]
    }
    /// The type a media link it reads names, whatever its page calls itself
    /// (`media::link_type`): `Some(None)` for a link it reads that names no one type.
    fn link_type(&self, _uri: &str) -> Option<Option<&'static str>> {
        None
    }
    /// Whether it reads [`Subject::candidates`], which are loaded for it alone.
    fn reads_candidates(&self) -> bool {
        false
    }
    /// The top-level section of the config file it owns ([`OwnSection`]), if any.
    fn section(&self) -> Option<Section> {
        None
    }
    /// Whether `[also_on.sources.<id>]` takes a `token` for it.
    fn takes_token(&self) -> bool {
        false
    }
    /// The variables of its own (`PINAKES_NAME`) that fill `[also_on.sources.<id>] key` and
    /// `secret`, an app's consumer pair, when the file leaves them unset; `None` for a source
    /// that takes no such pair.
    fn app_key_variables(&self) -> Option<(&'static str, &'static str)> {
        None
    }
}

/// Whether `source` runs under `config`: the worker and the source on, and the configuration
/// lets it ([`Source::ready`]).
pub fn runs(config: &Config, source: &dyn Source) -> bool {
    config.also_on.source_enabled(source) && source.ready(config)
}
/// Whether `source` counts as switched off under `config`: by the operator, or for want of what
/// it needs to run when that switches it off ([`Source::off_until_ready`]). A source that is not
/// switched off shows its stored rows, and what cards took from them, whether or not the worker
/// runs.
pub fn switched_off(config: &Config, source: &dyn Source) -> bool {
    !config.also_on.source_on(source) || (source.off_until_ready() && !source.ready(config))
}

/// A top-level section of the config file that one source owns and alone reads (`[tidal]`):
/// its name, the variables of its own (`PINAKES_NAME`) that set one of its keys, replacing the
/// file's value, and its checks. Its secrets are `SecretKey`s, so it shows them redacted.
pub trait OwnSection: Default + Serialize + serde::de::DeserializeOwned {
    const NAME: &'static str;
    const VARIABLES: &'static [(&'static str, &'static str)] = &[];
    fn check(&self) -> Result<()> {
        Ok(())
    }
}
/// What the configuration needs of an [`OwnSection`], whatever its type.
#[derive(Clone, Copy)]
pub struct Section {
    pub name: &'static str,
    pub variables: &'static [(&'static str, &'static str)],
    /// The section with its defaults, in its file form.
    pub default: fn() -> Value,
    /// The section as written, checked, in the form it may be shown (secrets redacted).
    pub shown: fn(&Value) -> Result<Value>,
}
impl Section {
    pub fn of<T: OwnSection>() -> Self {
        fn default<T: OwnSection>() -> Value {
            serde_json::to_value(T::default()).expect("a config section serializes")
        }
        fn shown<T: OwnSection>(value: &Value) -> Result<Value> {
            let section: T = serde_json::from_value(value.clone())?;
            section.check()?;
            Ok(serde_json::to_value(section)?)
        }
        Self {
            name: T::NAME,
            variables: T::VARIABLES,
            default: default::<T>,
            shown: shown::<T>,
        }
    }
}
/// The config sections the sources own ([`Source::section`]).
pub fn sections() -> impl Iterator<Item = Section> {
    registered().iter().filter_map(|s| s.section())
}

/// A hub's answer over its read's cap is no answer: asking again would read the same megabytes.
/// The run records `fallback` (what the link's URL alone proves, if anything) instead of failing.
pub fn unless_over_cap(found: Result<Found>, fallback: impl FnOnce() -> Found) -> Result<Found> {
    match found {
        Err(e) if crate::resolver::egress::over_cap(&e) => Ok(fallback()),
        found => found,
    }
}
/// Every source this build has: these, then each lookup family's ([`crate::lookups::FAMILIES`]).
pub fn registry() -> Vec<Arc<dyn Source>> {
    // In this order: each source sees the keys the ones before it found.
    let own: Vec<Arc<dyn Source>> = vec![
        Arc::new(service::ServiceSource),
        Arc::new(tidal::Tidal),
        Arc::new(spotify::Spotify),
        Arc::new(musicbrainz::MusicBrainzSource),
        Arc::new(musicbrainz_entity::MusicBrainzEntitySource),
        Arc::new(musicbrainz_containment::MusicBrainzContainment),
        Arc::new(performers::PerformersSource),
        Arc::new(wikidata::WikidataSource),
        // Before the barcode lookups: a Discogs release's barcodes are keys.
        Arc::new(crate::sources::discogs::also_on::Discogs),
        Arc::new(deezer::DeezerSource),
        // After every source that keys a track's ISRC or an album's barcode (a service's own
        // record, Deezer's, the hubs'), before `editions` places its links.
        Arc::new(spotify_isrc::SpotifyIsrc),
        Arc::new(tidal_isrc::TidalIsrc),
        Arc::new(itunes::ItunesSource),
        Arc::new(itunes_song::ItunesSong),
        Arc::new(youtube_match::YoutubeMatch),
        Arc::new(wikipedia::WikipediaSource),
        Arc::new(wikidata_item::WikidataItemSource),
        Arc::new(books::Books),
        Arc::new(films::Films),
        // After `films`: it starts from the IMDb ID `films` keys.
        Arc::new(tmdb::Tmdb),
        Arc::new(podcasts::Podcasts),
        Arc::new(product_keys::ProductKeys),
        Arc::new(crate::sources::open_food_facts::also_on::OpenFoodFacts),
        Arc::new(crate::sources::stack_exchange::also_on::StackExchange),
        Arc::new(crate::sources::media_records::titles::Titles),
        Arc::new(crate::sources::federal_register::also_on::FederalRegister),
        Arc::new(crate::sources::mempool::also_on::Explorers),
    ];
    own.into_iter()
        .chain(crate::lookups::FAMILIES.iter().flat_map(|f| f.sources()))
        // Last: it places the links every source before it found.
        .chain(std::iter::once(
            Arc::new(editions::Editions) as Arc<dyn Source>
        ))
        .collect()
}
/// Every source this build has, built once: for the questions a source answers from a link alone
/// (its key namespaces, service names, media types).
pub fn registered() -> &'static [Arc<dyn Source>] {
    static ALL: std::sync::OnceLock<Vec<Arc<dyn Source>>> = std::sync::OnceLock::new();
    ALL.get_or_init(registry)
}
/// Write the sources this build has that are switched off (by the operator, or a source
/// [`Source::off_until_ready`] that is not ready) to `also_on_source_off`,
/// which every read of keys, facts and derived links leaves out (cards, `also_on`, search,
/// grouping): a source switched off stops showing at once, its rows kept until they expire, and
/// shows again when switched on. A change regroups what its rows name (the table's trigger), and
/// the cards holding its keys or facts are due at once: what it gave a card itself (a title's
/// name, a type) leaves, or comes back, at that read (`media::outdated` keeps a 304 from holding
/// it). The rows of a source this build no longer has are read as before. Run at
/// startup, before the workers.
pub async fn configure(pool: &PgPool, config: &Config) -> Result<()> {
    let off: Vec<&str> = registered()
        .iter()
        .filter(|s| switched_off(config, s.as_ref()))
        .map(|s| s.id())
        .collect();
    let mut tx = pool.begin().await?;
    let mut changed: Vec<String> = sqlx::query_scalar(
        "DELETE FROM also_on_source_off WHERE NOT source=ANY($1) RETURNING source",
    )
    .bind(&off)
    .fetch_all(&mut *tx)
    .await?;
    changed.extend(
        sqlx::query_scalar::<_, String>(
            "INSERT INTO also_on_source_off(source) SELECT unnest($1::text[]) ON CONFLICT DO NOTHING RETURNING source",
        )
        .bind(&off)
        .fetch_all(&mut *tx)
        .await?,
    );
    if !changed.is_empty() {
        // Many resources' rows, in no particular order.
        crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Exclusive).await?;
        let due = sqlx::query("UPDATE resource SET next_attempt_at=now(),refresh_at=now(),priority_at=coalesce(priority_at,now()) WHERE id IN (SELECT resource_id FROM resource_key WHERE source=ANY($1) UNION SELECT resource_id FROM resource_fact WHERE source=ANY($1)) AND NOT final AND NOT dead_letter")
            .bind(&changed)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tracing::info!(sources = ?changed, due, "also-on sources switched: the cards holding their rows are read again");
    }
    tx.commit().await?;
    if !off.is_empty() {
        tracing::info!(sources = ?off, "also-on sources switched off: their stored rows are not read");
    }
    Ok(())
}
/// The API hosts ([`Source::api_hosts`]) of the sources that do not run under `config`: switched
/// off (or not ready, for a source [`Source::off_until_ready`]), or the worker off. The egress
/// refuses them (`EgressConfig::off_hosts`).
pub fn off_hosts(config: &Config) -> Vec<String> {
    registered()
        .iter()
        .filter(|s| !config.also_on.enabled || switched_off(config, s.as_ref()))
        .flat_map(|s| s.api_hosts().iter().map(|h| (*h).to_owned()))
        .collect()
}
/// Every also-on source's published host limits ([`Source::budgets`]).
pub fn budgets() -> impl Iterator<Item = (&'static str, f64)> {
    registered()
        .iter()
        .flat_map(|s| s.budgets().iter().copied())
}
/// The source IDs `[also_on.sources]` may name.
pub fn source_ids() -> Vec<&'static str> {
    registry().iter().map(|s| s.id()).collect()
}

/// Whether a key is of one of `forms` (`Source::key_forms`): its namespace, then `:` and a value.
pub fn of_form(forms: &[(&str, &str)], key: &str) -> bool {
    forms.iter().any(|(namespace, _)| {
        key.strip_prefix(namespace)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|value| !value.is_empty())
    })
}
/// In a debug build, every key a source that lists its forms (`Source::key_forms`) records, on
/// its resource or on a derived link, is of one of them: a form it does not list might not
/// search back.
fn check_key_forms(source: &dyn Source, found: &Found) {
    let forms = source.key_forms();
    if !cfg!(debug_assertions) || forms.is_empty() {
        return;
    }
    let links = found.links.iter().filter_map(|l| l.key.as_deref());
    for key in found.keys.iter().map(|k| k.key.as_str()).chain(links) {
        if let Some(key) = normal_key(key).filter(|k| valid_key(k)) {
            assert!(
                of_form(forms, &key),
                "{} records {key}, a form its key_forms does not list",
                source.id()
            );
        }
    }
}

fn token(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
/// A key as stored: a lower-case namespace (letters, digits, `-`, `_`, and `:` between its
/// parts, e.g. `mb:recording`), then `:` and a value of printable characters without spaces;
/// 3–512 bytes. A DOI's value may hold `:` itself (`doi:10.46298/lmcs-20(1:8)2024`).
pub fn valid_key(key: &str) -> bool {
    let split = match key.split_once(':') {
        Some(("doi", value)) => Some(("doi", value)),
        _ => key.rsplit_once(':'),
    };
    let Some((namespace, value)) = split else {
        return false;
    };
    (3..=512).contains(&key.len())
        && !namespace.is_empty()
        && namespace.split(':').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        })
        && !value.is_empty()
        && value.chars().all(|c| !c.is_control() && !c.is_whitespace())
}

/// A derived link as stored: its resource ID, its normalised URI and the link.
type CheckedLink = (String, String, Link);
/// A fact's name and value as stored.
fn valid_fact(f: &Fact) -> bool {
    let name = f.name.as_bytes();
    (1..=32).contains(&name.len())
        && name[0].is_ascii_lowercase()
        && name
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        && (1..=512).contains(&f.value.len())
        && f.value.trim() == f.value
        && !f.value.chars().any(char::is_control)
}
/// A source's result, checked and bounded: keys valid, links HTTP(S) URLs that are not the
/// resource itself (normalised, deduplicated by their resource ID), sites and vias well formed,
/// facts well formed and each once.
fn checked(
    resource_id: &str,
    found: Found,
    cfg: &AlsoOnConfig,
) -> (Vec<Key>, Vec<CheckedLink>, Vec<Fact>, Placed) {
    let mut facts: Vec<Fact> = found
        .facts
        .into_iter()
        .filter_map(|mut f| {
            if let Some(normal) = identifier(&f.name) {
                f.value = normal(&f.value)?;
            }
            Some(f)
        })
        .filter(valid_fact)
        .collect();
    facts.sort();
    facts.dedup();
    // The cap cuts the many values of one name (a recording's ISRCs) before a name's one value
    // (its MBID, the MBID it was merged from, its artist), so those always fit.
    let mut per_name: std::collections::HashMap<String, usize> = Default::default();
    for f in &facts {
        *per_name.entry(f.name.clone()).or_default() += 1;
    }
    facts.sort_by_key(|f| per_name[&f.name] > 1);
    facts.truncate(cfg.facts_per_source);
    facts.sort();
    let mut keys: Vec<Key> = found
        .keys
        .into_iter()
        .filter_map(|mut k| {
            k.key = normal_key(&k.key)?;
            Some(k)
        })
        .filter(|k| valid_key(&k.key) && token(&k.via))
        .collect();
    keys.sort();
    keys.dedup_by(|a, b| a.key == b.key);
    keys.truncate(cfg.keys_per_source);
    // In the source's order (its best first), each link once.
    let mut links = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if cfg.derive_links {
        for mut link in found.links {
            if links.len() == cfg.links_per_source {
                break;
            }
            let site = link.site.trim().to_owned();
            if site.is_empty()
                || site.len() > 128
                || site.chars().any(char::is_control)
                || !token(&link.via)
                || link.kind.as_deref().is_some_and(|k| {
                    !(1..=32).contains(&k.len())
                        || !k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                })
                || refused_link(&link.uri)
            {
                continue;
            }
            // A match is never a key.
            if link.kind.as_deref() == Some(MAYBE) {
                link.key = None;
            }
            // A link is stored under its root spelling (`alias::root`, when that is a URL): a
            // tagged resource that is the same object under another spelling is its root, and
            // the link names that root.
            let root = link_root(&link.uri);
            let Ok((id, uri, scheme)) = crate::model::identity(&root) else {
                continue;
            };
            if !matches!(scheme.as_str(), "http" | "https") || id == resource_id || uri.len() > 2048
            {
                continue;
            }
            // A key that is not a valid key is dropped; the link stays.
            link.key = link
                .key
                .as_deref()
                .and_then(normal_key)
                .filter(|k| valid_key(k));
            if seen.insert(id.clone()) {
                link.site = site;
                links.push((id, uri, link));
            }
        }
    }
    // Each placed link once, at its best rank, under the ID its link or member card has.
    let stored = |uri: &str| {
        crate::model::identity(&link_root(uri))
            .ok()
            .map(|(id, ..)| id)
            .filter(|id| id != resource_id)
    };
    let mut editions: Vec<(String, Edition)> = found
        .editions
        .into_iter()
        .filter_map(|mut e| {
            e.release = match e.release {
                Some(release) => Some(crate::hubs::mbid(&release)?),
                None => None,
            };
            let id = stored(&e.uri)?;
            (0..=1000).contains(&e.rank).then_some((id, e))
        })
        .collect();
    editions.sort_by(|a, b| (&a.0, a.1.rank, &a.1.release).cmp(&(&b.0, b.1.rank, &b.1.release)));
    editions.dedup_by(|a, b| a.0 == b.0);
    editions.truncate(CANDIDATES as usize);
    let mut keep: Vec<String> = found.keep.iter().filter_map(|uri| stored(uri)).collect();
    keep.sort();
    keep.dedup();
    keep.truncate(CANDIDATES as usize);
    (keys, links, facts, Placed { editions, keep })
}
/// Whether a source's link is refused for a credential: one that does not parse or has user
/// info, or that carries a credential as written or under its root spelling (the rule documents
/// follow, `capability::names_credential`). Refused, never stripped.
pub fn refused_link(uri: &str) -> bool {
    url::Url::parse(uri).map_or(true, |u| !u.username().is_empty() || u.password().is_some())
        || crate::capability::names_credential(uri)
        || crate::capability::names_credential(&link_root(uri))
}
/// The spelling a link is stored under: its root (`alias::root`) when that is a URL.
fn link_root(uri: &str) -> String {
    crate::alias::root(uri)
        .map(|a| a.root)
        .filter(|r| r.starts_with("http"))
        .unwrap_or_else(|| uri.to_owned())
}

/// ISRCs and barcodes have one stored form (`music::isrc`, `barcode::gtin13`), as keys and facts.
fn identifier(namespace: &str) -> Option<fn(&str) -> Option<String>> {
    match namespace {
        "isrc" => Some(crate::music::isrc),
        "upc" => Some(crate::barcode::gtin13),
        _ => None,
    }
}
/// A key in its stored form; a malformed ISRC or barcode is no key.
fn normal_key(key: &str) -> Option<String> {
    match key.split_once(':') {
        Some((namespace, value)) => match identifier(namespace) {
            Some(normal) => Some(format!("{namespace}:{}", normal(value)?)),
            None => Some(key.to_owned()),
        },
        None => Some(key.to_owned()),
    }
}
/// Replace `source`'s keys and links for `resource_id` with `found` (checked and bounded); they
/// live for twice `ttl`.
pub async fn record(
    pool: &PgPool,
    resource_id: &str,
    source: &str,
    found: Found,
    ttl: Duration,
    cfg: &AlsoOnConfig,
) -> Result<()> {
    anyhow::ensure!(token(source), "invalid source id {source}");
    let (mut keys, mut links, mut facts, mut placed) = checked(resource_id, found, cfg);
    let expires = Utc::now()
        + chrono::Duration::from_std(ttl.saturating_mul(2)).unwrap_or(chrono::Duration::days(60));
    let mut tx = pool.begin().await?;
    // The write order first, then the resource rows this run's triggers update (its own, and
    // those its keyed links name, now and before) in ID order, so neither a save nor two runs
    // naming each other deadlock.
    lock_writers(&mut tx).await?;
    let named: Vec<&str> = std::iter::once(resource_id)
        .chain(
            links
                .iter()
                .filter(|(_, _, l)| l.key.is_some())
                .map(|(id, ..)| id.as_str()),
        )
        .collect();
    sqlx::query("SELECT 1 FROM resource WHERE id=ANY($1::text[] || ARRAY(SELECT link_id FROM derived_link WHERE resource_id=$2 AND source=$3 AND key IS NOT NULL)) ORDER BY id FOR NO KEY UPDATE")
        .bind(&named)
        .bind(resource_id)
        .bind(source)
        .execute(&mut *tx)
        .await?;
    // Read again under the locks, for a lookup that ran while its resource changed. One that
    // became an alias meanwhile gets nothing: an alias is read as its root and runs no lookups
    // (`subject`). A social post found gone meanwhile writes nothing, and its rows go.
    let now: Option<(Value, bool)> = sqlx::query_as("SELECT doc,EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1) FROM resource WHERE id=$1")
        .bind(resource_id)
        .fetch_optional(&mut *tx)
        .await?;
    if now.as_ref().is_some_and(|(_, alias)| *alias) {
        return Ok(());
    }
    if now
        .and_then(|(doc, _)| serde_json::from_value::<ResourceMeta>(doc).ok())
        .is_some_and(|doc| crate::changes::gone_social(&doc))
    {
        (keys, links, facts, placed) = Default::default();
    }
    // Observed, never decided: every producer keys the recording that stands for a set of
    // duplicates (`music_sources::recording_key`), so two MusicBrainz recordings on one resource
    // are sources that truly disagree. Both keys are stored and `groups::single_object` refuses
    // the join, as for any conflicting single-object key; the run only counts it.
    let mut recordings: Vec<&str> = keys
        .iter()
        .filter(|k| k.key.starts_with("mb:recording:"))
        .map(|k| k.key.as_str())
        .collect();
    recordings.sort_unstable();
    recordings.dedup();
    let disagrees = match recordings.as_slice() {
        [] => false,
        [mine] => sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM resource_key WHERE resource_id=$1 AND source<>$2 AND key LIKE 'mb:recording:%' AND key<>$3 AND expires_at>now() AND source NOT IN (SELECT source FROM also_on_source_off))")
            .bind(resource_id)
            .bind(source)
            .bind(*mine)
            .fetch_one(&mut *tx)
            .await?,
        _ => true,
    };
    if disagrees {
        crate::telemetry::also_on_recording_key_conflict(source);
        tracing::warn!(resource = %resource_id, source, "sources key different recordings; the resource joins no recording group");
    }
    let key_rows: Vec<Value> = keys
        .iter()
        .map(|k| serde_json::json!({"key": k.key, "via": k.via}))
        .collect();
    sqlx::query("DELETE FROM resource_key WHERE resource_id=$1 AND source=$2 AND NOT key=ANY(SELECT v->>'key' FROM jsonb_array_elements($3) v)")
        .bind(resource_id).bind(source).bind(Value::from(key_rows.clone())).execute(&mut *tx).await?;
    // A key another source already vouches for keeps that source; this one adds nothing to it.
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) SELECT $1,v->>'key',$2,v->>'via',$4 FROM jsonb_array_elements($3) v ON CONFLICT(resource_id,key) DO UPDATE SET seen_at=now(),expires_at=greatest(resource_key.expires_at,excluded.expires_at) WHERE resource_key.source=excluded.source")
        .bind(resource_id).bind(source).bind(Value::from(key_rows)).bind(expires).execute(&mut *tx).await?;
    let link_rows: Vec<Value> = links
        .iter()
        .map(|(id, uri, l)| serde_json::json!({"id": id, "uri": uri, "site": l.site, "via": l.via, "kind": l.kind, "at": l.at, "key": l.key}))
        .collect();
    sqlx::query("DELETE FROM derived_link WHERE resource_id=$1 AND source=$2 AND NOT link_id=ANY(SELECT v->>'id' FROM jsonb_array_elements($3) v)")
        .bind(resource_id).bind(source).bind(Value::from(link_rows.clone())).execute(&mut *tx).await?;
    // A link another source derived first stays that source's, unless it is a maybe (a match by
    // metadata) and this one is not: the exact row takes it over, its source, kind and key too, so
    // the maybe source neither writes it back nor deletes it when it stops finding the link.
    sqlx::query("INSERT INTO derived_link(resource_id,link_id,uri,site,source,via,kind,at,key,expires_at) SELECT $1,v->>'id',v->>'uri',v->>'site',$2,v->>'via',v->>'kind',(v->>'at')::timestamptz,v->>'key',$4 FROM jsonb_array_elements($3) v ON CONFLICT(resource_id,link_id) DO UPDATE SET source=excluded.source,uri=excluded.uri,site=excluded.site,via=excluded.via,kind=excluded.kind,at=excluded.at,key=excluded.key,seen_at=now(),expires_at=excluded.expires_at WHERE derived_link.source=excluded.source OR (derived_link.kind='maybe' AND excluded.kind IS DISTINCT FROM 'maybe')")
        .bind(resource_id).bind(source).bind(Value::from(link_rows)).bind(expires).execute(&mut *tx).await?;
    let fact_rows: Vec<Value> = facts
        .iter()
        .map(|f| serde_json::json!({"name": f.name, "value": f.value}))
        .collect();
    sqlx::query("DELETE FROM resource_fact WHERE resource_id=$1 AND source=$2 AND NOT (name,value) IN (SELECT v->>'name',v->>'value' FROM jsonb_array_elements($3) v)")
        .bind(resource_id).bind(source).bind(Value::from(fact_rows.clone())).execute(&mut *tx).await?;
    // A fact another source already reports keeps that source.
    sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) SELECT $1,v->>'name',v->>'value',$2,$4 FROM jsonb_array_elements($3) v ON CONFLICT(resource_id,name,value) DO UPDATE SET seen_at=now(),expires_at=greatest(resource_fact.expires_at,excluded.expires_at) WHERE resource_fact.source=excluded.source")
        .bind(resource_id).bind(source).bind(Value::from(fact_rows)).bind(expires).execute(&mut *tx).await?;
    let edition_rows: Vec<Value> = placed
        .editions
        .iter()
        .map(|(id, e)| serde_json::json!({"id": id, "release": e.release, "rank": e.rank}))
        .collect();
    // A link whose place this run could not tell keeps the one recorded before.
    sqlx::query("DELETE FROM link_release WHERE resource_id=$1 AND source=$2 AND NOT link_id=ANY(SELECT v->>'id' FROM jsonb_array_elements($3) v) AND NOT link_id=ANY($4)")
        .bind(resource_id).bind(source).bind(Value::from(edition_rows.clone())).bind(&placed.keep).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO link_release(resource_id,link_id,release,rank,source,expires_at) SELECT $1,v->>'id',v->>'release',(v->>'rank')::smallint,$2,$4 FROM jsonb_array_elements($3) v ON CONFLICT(resource_id,link_id) DO UPDATE SET release=excluded.release,rank=excluded.rank,seen_at=now(),expires_at=excluded.expires_at WHERE link_release.source=excluded.source")
        .bind(resource_id).bind(source).bind(Value::from(edition_rows)).bind(expires).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Run the sources of `ids` again in ten minutes, those `editions` reads (a MusicBrainz key): a change of
/// their groups (a member joining or leaving, a link becoming an alias) changes the links their
/// cards may show, which it places.
pub(crate) async fn requeue_placed(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[String],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    // In ten minutes: the changes a regrouping makes at once come to one run per card.
    sqlx::query("INSERT INTO also_on_dirty(resource_id,next_attempt_at) SELECT DISTINCT k.resource_id,now()+interval '10 minutes' FROM resource_key k WHERE k.resource_id=ANY($1) AND (k.key LIKE 'mb:recording:%' OR k.key LIKE 'mb:release:%' OR k.key LIKE 'mb:release-group:%') ORDER BY 1 ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=least(also_on_dirty.next_attempt_at,excluded.next_attempt_at)")
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The write order (`store::lock_writers`) shared, as saves take it, and the alias lock: a run
/// locks rows of several resources in an order of its own (ID order), which a shared writer may
/// meet the other way round.
async fn lock_writers(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    crate::store::lock_writers(tx, crate::store::WriteOrder::Shared).await?;
    crate::alias::lock(tx).await
}

/// Take out of every stored card the values written from facts (`Source::fill_card`) that its
/// sources' live facts no longer give (`media::unfill`): their rows expired (a TMDB answer older
/// than its rows live), the source is switched off or gone from the build, or it answers
/// otherwise now. For cards no reading brings back (final, dead-lettered, orphaned, or not due
/// yet); a card that is read again takes what the live facts give then. It only removes: a stale
/// card is rewritten under its row lock, checked again against the facts read under it, with its
/// change line (`changes::rewrite`); nothing else of it changes, and a card that is current is
/// never locked. It reads only cards of the types such values fill
/// (`CreditProvider::filled_types`, by the `type_path` index), never every card. In batches of
/// [`REFILL_BATCH`], until none is left or `stop`; the number rewritten.
pub async fn refill(pool: &PgPool, stop: impl Fn() -> bool) -> Result<u64> {
    let fillers: Vec<&str> = registered()
        .iter()
        .filter(|s| !s.fills().is_empty())
        .map(|s| s.id())
        .collect();
    // The providers whose values are written from facts, as stored, and the only card types
    // that can hold them.
    let from_facts = crate::model::CreditProvider::FROM_FACTS;
    let providers: Vec<String> = from_facts
        .iter()
        .filter_map(|p| serde_json::to_value(p).ok()?.as_str().map(str::to_owned))
        .collect();
    let mut types: Vec<&str> = from_facts
        .iter()
        .flat_map(|p| p.filled_types())
        .copied()
        .collect();
    types.sort();
    types.dedup();
    let mut done = 0;
    let mut after = String::new();
    while !types.is_empty() && !stop() {
        // Only cards of those types (the `type_path` index), in ID order.
        let rows: Vec<(String, Value)> = sqlx::query_as("SELECT id,doc FROM resource WHERE type_path && $5::text[] AND id>$1 AND osm_id IS NULL AND (doc->'hinted_by' ?| $2 OR EXISTS(SELECT 1 FROM jsonb_array_elements(CASE jsonb_typeof(doc->'credits') WHEN 'array' THEN doc->'credits' ELSE '[]' END) c WHERE c->>'provider'=ANY($3))) ORDER BY id LIMIT $4")
            .bind(&after)
            .bind(&fillers)
            .bind(&providers)
            .bind(REFILL_BATCH)
            .bind(&types)
            .fetch_all(pool)
            .await?;
        let Some((last, _)) = rows.last() else { break };
        after = last.clone();
        let ids: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
        let mut hints = media::type_hints(pool, &ids).await?;
        for (id, doc) in rows {
            let hints = hints.remove(&id).unwrap_or_default();
            let Ok(mut doc) = serde_json::from_value::<ResourceMeta>(doc) else {
                continue;
            };
            // Current: not locked.
            if !media::unfill(&hints, &mut doc) {
                continue;
            }
            // Under the row lock, against the facts as they are then: a fill that landed
            // meanwhile is not taken out.
            let rewritten = crate::changes::rewrite(
                pool,
                &id,
                || async {
                    Ok(media::type_hints(pool, std::slice::from_ref(&id))
                        .await?
                        .remove(&id)
                        .unwrap_or_default())
                },
                |doc, hints| media::unfill(&hints, doc),
            )
            .await?;
            done += u64::from(rewritten);
        }
    }
    if done > 0 {
        tracing::info!(
            cards = done,
            "values their sources' live facts no longer give taken out of cards"
        );
    }
    Ok(done)
}
/// The cards [`refill`] reads at once.
pub const REFILL_BATCH: i64 = 100;

/// Delete keys and links nobody refreshed in time (their deletion regroups as a change would).
pub async fn prune(pool: &PgPool) -> Result<u64> {
    // Each deletion takes the write order, then locks what its triggers update, as `record` does.
    let mut tx = pool.begin().await?;
    lock_writers(&mut tx).await?;
    sqlx::query("SELECT 1 FROM resource WHERE id IN (SELECT resource_id FROM resource_key WHERE expires_at<=now()) ORDER BY id FOR NO KEY UPDATE")
        .execute(&mut *tx)
        .await?;
    let keys = sqlx::query("DELETE FROM resource_key WHERE expires_at<=now()")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    let mut tx = pool.begin().await?;
    lock_writers(&mut tx).await?;
    sqlx::query("SELECT 1 FROM resource WHERE id IN (SELECT link_id FROM derived_link WHERE expires_at<=now() AND key IS NOT NULL) ORDER BY id FOR NO KEY UPDATE")
        .execute(&mut *tx)
        .await?;
    let links = sqlx::query("DELETE FROM derived_link WHERE expires_at<=now()")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    let facts = sqlx::query("DELETE FROM resource_fact WHERE expires_at<=now()")
        .execute(pool)
        .await?
        .rows_affected();
    let placed = sqlx::query("DELETE FROM link_release WHERE expires_at<=now()")
        .execute(pool)
        .await?
        .rows_affected();
    Ok(keys + links + facts + placed)
}

async fn subject(pool: &PgPool, id: &str) -> Result<Option<Subject>> {
    let row: Option<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',r.id,'uri',r.uri,'status',r.status,'type',r.type,'doc',r.doc,'first_seen_at',(SELECT min(created_at) FROM reference f WHERE f.resource_id=r.id),'keys',coalesce((SELECT jsonb_agg(jsonb_build_array(k.key,k.source) ORDER BY k.key) FROM resource_key k WHERE k.resource_id=r.id AND k.expires_at>now() AND k.source NOT IN (SELECT source FROM also_on_source_off)),'[]')) FROM resource_document r WHERE r.id=$1 AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id)")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(Subject {
        id: row["id"].as_str().unwrap_or_default().into(),
        uri: row["uri"].as_str().unwrap_or_default().into(),
        status: row["status"].as_str().unwrap_or_default().into(),
        type_name: row["type"].as_str().map(str::to_owned),
        doc: row["doc"]
            .get("type")
            .and_then(|_| serde_json::from_value(row["doc"].clone()).ok()),
        keys: serde_json::from_value(row["keys"].clone()).unwrap_or_default(),
        first_seen_at: row["first_seen_at"].as_str().and_then(|s| s.parse().ok()),
        candidates: vec![],
    }))
}
/// The links a card may show besides its own (`Subject::candidates`): tagged members first, then
/// the derived links most members name, then by URI.
async fn candidates(pool: &PgPool, id: &str) -> Result<Vec<String>> {
    // Tagged members first, then the derived links most members name.
    Ok(sqlx::query_scalar("WITH owners AS (SELECT $1::text AS id UNION SELECT u.resource_id FROM group_unit g JOIN group_unit u USING(group_id) WHERE g.unit_id=$1 AND g.group_id IS NOT NULL) SELECT uri FROM (SELECT r.uri,0 AS tier,0::bigint AS named FROM owners o JOIN resource r ON r.id=o.id WHERE o.id<>$1 UNION ALL SELECT l.uri,1,count(*) FROM derived_link l JOIN owners o ON o.id=l.resource_id WHERE l.expires_at>now() AND l.source NOT IN (SELECT source FROM also_on_source_off) GROUP BY l.uri) x GROUP BY uri ORDER BY min(tier),max(named) DESC,uri LIMIT $2")
        .bind(id)
        .bind(CANDIDATES)
        .fetch_all(pool)
        .await?)
}

/// How far ahead a claimed resource's next attempt moves, so a crashed worker's rows come back.
pub const LEASE: Duration = Duration::from_secs(600);

/// Run the due sources of up to `limit` resources; returns how many were handled.
pub async fn drain(
    pool: &PgPool,
    cx: &Context,
    sources: &[Arc<dyn Source>],
    limit: i64,
) -> Result<usize> {
    drain_leased(pool, cx, sources, limit, LEASE).await
}

/// `drain` with claims held for `lease` (tests use a short one).
pub async fn drain_leased(
    pool: &PgPool,
    cx: &Context,
    sources: &[Arc<dyn Source>],
    limit: i64,
    lease: Duration,
) -> Result<usize> {
    let cfg = &cx.config.also_on;
    // Claimed rows move one lease ahead, so a crashed worker's rows come back. Each row's lease
    // is kept: its run's outcome is written against it, not against the clock.
    let due: Vec<(String, DateTime<Utc>)> = sqlx::query_as("UPDATE also_on_dirty SET next_attempt_at=now()+make_interval(secs => $2) WHERE resource_id IN (SELECT resource_id FROM also_on_dirty WHERE next_attempt_at<=now() ORDER BY next_attempt_at,resource_id LIMIT $1 FOR UPDATE SKIP LOCKED) RETURNING resource_id,next_attempt_at")
        .bind(limit)
        .bind(lease.as_secs_f64())
        .fetch_all(pool)
        .await?;
    let handled = due.len();
    let (due, leases): (Vec<String>, Vec<_>) = due.into_iter().unzip();
    // Batched hub lookups for the whole batch first (MusicBrainz URLs, Wikidata items).
    let uris: Vec<String> = sqlx::query_scalar("SELECT uri FROM resource WHERE id=ANY($1)")
        .bind(&due)
        .fetch_all(pool)
        .await?;
    // A link that carries a credential is never sent to another service.
    let uris: Vec<String> = uris
        .into_iter()
        .filter(|uri| !crate::capability::is_capability(uri))
        .collect();
    for source in sources
        .iter()
        .filter(|s| cfg.source_enabled(s.as_ref()) && s.ready(&cx.config))
    {
        // A panicking prefetch costs only its batch (each lookup still asks alone).
        if std::panic::AssertUnwindSafe(source.prefetch(cx, &uris))
            .catch_unwind()
            .await
            .is_err()
        {
            tracing::error!(source = source.id(), "also-on prefetch panicked");
        }
    }
    let work = due.into_iter().zip(leases).map(|(id, lease)| async move {
        let result = one(pool, cx, sources, &id).await;
        (id, lease, result)
    });
    let results = futures_util::stream::iter(work)
        .buffer_unordered(cfg.concurrency.max(1))
        .collect::<Vec<_>>()
        .await;
    // A change saved while a resource was read marked it due again (its lease became now): that
    // mark stands, so the change is read too. So does another worker's claim after this lease
    // ran out. The next attempt is written only while the row still holds this run's lease,
    // however long the batch took; the count of failed runs and the error are always written.
    let next = |at: &str| {
        format!("next_attempt_at=CASE WHEN next_attempt_at=$2 THEN {at} ELSE next_attempt_at END")
    };
    for (id, lease, result) in results {
        match result {
            Ok(Some(Next {
                lifetime,
                partial: None,
            })) => {
                sqlx::query(&format!(
                    "UPDATE also_on_dirty SET attempts=0,last_error=NULL,{} WHERE resource_id=$1",
                    next("now()+$3")
                ))
                .bind(&id)
                .bind(lease)
                .bind(lifetime)
                .execute(pool)
                .await?;
            }
            // A secondary service failed: what the run found is recorded, and the run comes back
            // after a failed run's backoff, so a service that keeps failing is asked less and
            // less often (up to hourly), or after the wait it named when longer; never after the
            // lifetime. A run with every service answering resets the count.
            Ok(Some(Next {
                lifetime,
                partial: Some(asked),
            })) => {
                sqlx::query(&format!(
                    "UPDATE also_on_dirty SET attempts=attempts+1,last_error=NULL,{} WHERE resource_id=$1",
                    next("now()+least($3,make_interval(secs => greatest($5::float8,least($4::float8*power(2,least(attempts,6)),3600))))")
                ))
                .bind(&id)
                .bind(lease)
                .bind(lifetime)
                .bind(cfg.retry_secs as f64)
                .bind(asked.as_secs().min(86_400) as f64)
                .execute(pool)
                .await?;
            }
            Ok(None) => {
                sqlx::query(
                    "DELETE FROM also_on_dirty WHERE resource_id=$1 AND next_attempt_at=$2",
                )
                .bind(&id)
                .bind(lease)
                .execute(pool)
                .await?;
            }
            Err(error) => {
                tracing::warn!(resource=%id, kind=crate::resolver::error_class(&error), "also-on lookup will retry");
                // A service that said how long to wait is waited for at least that long.
                let asked = error
                    .downcast_ref::<crate::resolver::RetryLater>()
                    .map_or(0, |r| r.seconds.min(86_400)) as f64;
                sqlx::query(&format!(
                    "UPDATE also_on_dirty SET attempts=attempts+1,last_error=$3,{} WHERE resource_id=$1",
                    next("now()+make_interval(secs => greatest($5::float8,least($4::float8*power(2,least(attempts,6)),3600)))")
                ))
                .bind(&id)
                .bind(lease)
                .bind(crate::resolver::error_class(&error))
                .bind(cfg.retry_secs as f64)
                .bind(asked)
                .execute(pool)
                .await?;
            }
        }
    }
    Ok(handled)
}
/// When a resource's next also-on run is due.
struct Next {
    /// The shortest lifetime among the sources that ran.
    lifetime: chrono::Duration,
    /// A secondary service failed in this run: the longest wait one named (zero when none).
    partial: Option<Duration>,
}

/// The sources that hold rows of `resource_id`, in every table `record` writes (keys, derived
/// links, facts, places among releases), whatever their expiry and whether the source is on.
async fn stored_sources(
    pool: &PgPool,
    resource_id: &str,
) -> Result<std::collections::HashSet<String>> {
    Ok(sqlx::query_scalar::<_, String>("SELECT source FROM resource_key WHERE resource_id=$1 UNION SELECT source FROM derived_link WHERE resource_id=$1 UNION SELECT source FROM resource_fact WHERE resource_id=$1 UNION SELECT source FROM link_release WHERE resource_id=$1")
        .bind(resource_id)
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect())
}

/// One resource: every enabled source that applies runs; each success replaces its rows. Ok
/// with when the next run is due, Ok(None) when no source applies, Err when any source failed
/// (the others' results are kept).
async fn one(
    pool: &PgPool,
    cx: &Context,
    sources: &[Arc<dyn Source>],
    id: &str,
) -> Result<Option<Next>> {
    let Some(mut subject) = subject(pool, id).await? else {
        return Ok(None);
    };
    let cfg = &cx.config.also_on;
    // A social post found gone gets no lookups: nothing is derived from it (an archived copy
    // included), and what was is deleted.
    let gone = subject
        .doc
        .as_ref()
        .is_some_and(crate::changes::gone_social);
    // Nor does a link that carries a credential: no other service is ever sent it.
    let withheld = crate::capability::is_capability(&subject.uri);
    let mut next: Option<Duration> = None;
    let mut partial: Option<Duration> = None;
    let mut failed = None;
    let mut reread = false;
    // The sources holding rows of this resource (`stored_sources`): read once for every source
    // in a row that asks, and again after a lookup, which takes long enough for another run to
    // have recorded meanwhile. Rows this run takes over from another source only leave that
    // source listed, and clearing a source that holds nothing changes nothing.
    let mut stored: Option<std::collections::HashSet<String>> = None;
    // In registry order: each source sees the keys the ones before it found in this run (a
    // barcode MusicBrainz names, then the services looked up by it).
    for source in sources
        .iter()
        .filter(|s| cfg.source_enabled(s.as_ref()) && s.ready(&cx.config))
    {
        if gone || withheld || !source.applies(&subject) {
            // It read the resource before (a key it starts from is gone): its rows go too.
            if stored.is_none() {
                stored = Some(stored_sources(pool, id).await?);
            }
            let had = stored.as_ref().is_some_and(|s| s.contains(source.id()));
            if had {
                record(pool, id, source.id(), Found::default(), source.ttl(), cfg)
                    .await
                    .with_context(|| format!("clear {}", source.id()))?;
            }
            continue;
        }
        if source.reads_candidates() {
            subject.candidates = candidates(pool, id).await?;
        }
        // A source that panics (a parser meeting an answer of a shape it doesn't expect) fails
        // this run like an error: the others' results are kept and the worker goes on.
        let lookup = std::panic::AssertUnwindSafe(source.lookup(cx, &subject))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                tracing::error!(resource = %id, source = source.id(), "also-on lookup panicked");
                Err(anyhow::anyhow!("lookup panicked"))
            });
        stored = None;
        match lookup {
            Ok(found) => {
                reread |= found.reread;
                // A secondary service failed: this run's results stand, and the run comes back
                // early (`drain` sets when).
                if let Some(asked) = found.retry_after {
                    partial = Some(partial.map_or(asked, |p| p.max(asked)));
                }
                let lifetime = source.ttl();
                check_key_forms(source.as_ref(), &found);
                // Nothing found replaces nothing unless the source holds rows: only then is
                // there anything to clear, and a transaction with its locks to take.
                let nothing = found.keys.is_empty()
                    && found.links.is_empty()
                    && found.facts.is_empty()
                    && found.editions.is_empty();
                let clears = if nothing {
                    let held = stored_sources(pool, id).await?;
                    let clears = held.contains(source.id());
                    stored = Some(held);
                    clears
                } else {
                    true
                };
                if clears {
                    record(pool, id, source.id(), found, source.ttl(), cfg)
                        .await
                        .with_context(|| format!("record {}", source.id()))?;
                }
                next = Some(next.map_or(lifetime, |n| n.min(lifetime)));
                subject.keys = sqlx::query_as("SELECT key,source FROM resource_key WHERE resource_id=$1 AND expires_at>now() AND source NOT IN (SELECT source FROM also_on_source_off) ORDER BY key")
                    .bind(id)
                    .fetch_all(pool)
                    .await?;
            }
            Err(error) => {
                // The longest wait any failing source asked for sets the retry.
                let wait = |e: &anyhow::Error| {
                    e.downcast_ref::<crate::resolver::RetryLater>()
                        .map_or(0, |r| r.seconds)
                };
                let error = error.context(format!("source {}", source.id()));
                failed = match failed.take() {
                    Some(prev) if wait(&prev) > wait(&error) => Some(prev),
                    _ => Some(error),
                };
            }
        }
    }
    if reread {
        sqlx::query("UPDATE resource SET next_attempt_at=now(),refresh_at=now(),priority_at=coalesce(priority_at,now()) WHERE id=$1 AND NOT final AND NOT dead_letter")
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(next.map(|n| Next {
        lifetime: chrono::Duration::from_std(n).unwrap_or(chrono::Duration::days(30)),
        partial,
    }))
}

/// The worker: drains due resources until shutdown, pausing when nothing is due.
pub async fn run(
    pool: PgPool,
    cx: Context,
    sources: Vec<Arc<dyn Source>>,
    mut shutdown: watch::Receiver<bool>,
) {
    if !cx.config.also_on.enabled || sources.is_empty() {
        return;
    }
    loop {
        // Up to 100 resources a tick, so their hub lookups batch.
        let handled = tokio::select! { _=shutdown.changed()=>break, result=drain(&pool,&cx,&sources,100)=>result };
        match handled {
            Ok(100) => continue,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    kind = crate::resolver::error_class(&error),
                    "also-on worker will retry"
                )
            }
        }
        tokio::select! { _=shutdown.changed()=>break, _=tokio::time::sleep(Duration::from_secs(5))=>{} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each also-on source and lookup family has its page in docs/sources, listed in the index
    /// there: a family's sources their family's page, an official source's also-on half that
    /// source's.
    #[test]
    fn each_source_and_family_has_its_page() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/sources");
        let index = std::fs::read_to_string(dir.join("README.md")).unwrap();
        let listed = |page: &str| {
            assert!(
                dir.join(format!("{page}.md")).exists(),
                "docs/sources/{page}.md"
            );
            assert!(
                index.contains(&format!("[`{page}`]({page}.md")),
                "docs/sources/README.md lacks {page}"
            );
        };
        let mut in_families = std::collections::BTreeSet::new();
        for family in crate::lookups::FAMILIES {
            listed(family.name());
            in_families.extend(family.sources().iter().map(|s| s.id()));
        }
        for source in registry() {
            if !in_families.contains(source.id()) {
                listed(source.page());
            }
        }
    }
}
