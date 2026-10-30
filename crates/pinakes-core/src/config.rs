use crate::event::adapters::SecretKey;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// The file form: a top-level section no field names is a source's own ([`Sections`]), and one
/// no source owns is refused at validation.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// May carry a password: `Debug` and serialization show `[REDACTED]`.
    pub database_url: SecretKey,
    pub listen: String,
    pub admin_listen: String,
    pub admin_token: Option<SecretKey>,
    pub nexus_url: String,
    /// The `Authorization` header sent, whole, on every request to the Nexus at `nexus_url`
    /// (`PINAKES_NEXUS_AUTHORIZATION`), for a Nexus behind credentials; `nexus_url` itself never
    /// carries any. Empty is none. `Debug` and serialization show `[REDACTED]`.
    pub nexus_authorization: Option<SecretKey>,
    /// Whether the Nexus at `nexus_url` serves the reach-set route, so search takes
    /// `reach=wot_2` and `boost=wot_2` (`nexus::Nexus::reach`); off, those answer 501.
    pub nexus_reach_set: bool,
    /// `discover_feeds` is accepted here and in `[cards.media]`; the other resolver sections take
    /// only `enabled`.
    #[serde(with = "feed_section")]
    pub web: ResolverConfig,
    /// `enabled` and `pkarr_relays` (`pubky_section`).
    #[serde(with = "pubky_section")]
    pub pubky: ResolverConfig,
    pub geo: GeoConfig,
    pub osm: OsmConfig,
    pub cards: crate::resolver::cards::CardConfig,
    pub media: crate::resolver::cards::MediaConfig,
    /// One switch per official-API source (`[sources.<name>]`, see `crate::sources`).
    pub sources: crate::sources::SourcesConfig,
    pub event: EventConfig,
    pub egress: EgressConfig,
    pub http: HttpConfig,
    pub groups: crate::groups::GroupConfig,
    /// "Also on": keys and derived links from sources (`also_on::registry`).
    pub also_on: crate::also_on::AlsoOnConfig,
    /// The Podcast Index API's key and secret, which the `podcasts` also-on source and the
    /// `podcast_index` source read (`podcast_index_api`).
    pub podcast_index: crate::podcast_index_api::PodcastIndexConfig,
    /// The sections sources own (`also_on::Source::section`, such as `[tidal]`), each read by its
    /// source alone (`Config::section`).
    #[serde(flatten)]
    pub sections: Sections,
    pub api: ApiConfig,
    pub canary: CanaryConfig,
    /// Every cap, timeout, budget, backoff and refresh tier, with documented defaults
    /// (`docs/operations.md`); applied onto the sections below at load.
    pub limits: Limits,
    #[serde(skip)]
    pub refresh: RefreshConfig,
    #[serde(skip)]
    pub poll_secs: u64,
    #[serde(skip)]
    pub ingest: IngestConfig,
    #[serde(skip)]
    pub grace_days: u32,
    #[serde(skip)]
    pub search: SearchConfig,
    #[serde(skip)]
    pub queue: QueueConfig,
    #[serde(skip)]
    pub preview: PreviewConfig,
}
impl Default for Config {
    fn default() -> Self {
        let mut config = Self::base();
        config.limits = Limits::of(&config);
        config.share_contact();
        config
    }
}
/// The top-level sections sources own ([`crate::also_on::OwnSection`]), as written: each source
/// reads its own (`Config::section`). They show (`Debug`, JSON) through their source's own type,
/// so their secrets are redacted; one no source owns is refused at validation.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Sections(BTreeMap<String, serde_json::Value>);
impl Sections {
    /// Every owned section with its defaults.
    fn defaults() -> Self {
        Self(
            crate::also_on::sections()
                .map(|s| (s.name.to_owned(), (s.default)()))
                .collect(),
        )
    }
    /// Set a section as the file would (its secrets in clear).
    pub fn set(&mut self, name: &str, value: serde_json::Value) {
        self.0.insert(name.to_owned(), value);
    }
    /// Each section checked by its source, as it may be shown.
    fn shown(&self) -> Result<BTreeMap<&str, serde_json::Value>> {
        let owned: Vec<_> = crate::also_on::sections().collect();
        self.0
            .iter()
            .map(|(name, value)| {
                let section = owned
                    .iter()
                    .find(|s| s.name == name)
                    .with_context(|| format!("unknown config section [{name}]"))?;
                let shown = (section.shown)(value).with_context(|| format!("[{name}]"))?;
                Ok((name.as_str(), shown))
            })
            .collect()
    }
    fn check(&self) -> Result<()> {
        self.shown().map(|_| ())
    }
}
impl Default for Sections {
    fn default() -> Self {
        Self::defaults()
    }
}
impl Serialize for Sections {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.shown()
            .map_err(serde::ser::Error::custom)?
            .serialize(s)
    }
}
impl std::fmt::Debug for Sections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.shown() {
            Ok(shown) => shown.fmt(f),
            Err(_) => f.write_str("[unchecked sections]"),
        }
    }
}
impl Config {
    /// A source's own section ([`crate::also_on::OwnSection`]): as written, its defaults where
    /// it says nothing or what it says does not check (validation refuses that at load).
    pub fn section<T: crate::also_on::OwnSection>(&self) -> T {
        self.sections
            .0
            .get(T::NAME)
            .and_then(|v| serde_json::from_value::<T>(v.clone()).ok())
            .filter(|s| s.check().is_ok())
            .unwrap_or_default()
    }
}
/// Map sections whose entries an operator names: a `PINAKES__…` override may create one (and
/// its keys); `deny_unknown_fields` and `adapters::validate` still refuse what is unknown.
const ENV_MAPS: &[&[&str]] = &[
    &["event", "adapters", "platforms"],
    &["sources"],
    &["also_on", "sources"],
    HOST_RATES,
];
/// The one map whose new entries are numbers.
const HOST_RATES: &[&str] = &["limits", "host_rates"];
/// The variable whose whole value is the `Authorization` header sent to the Nexus.
pub const NEXUS_AUTHORIZATION: &str = "PINAKES_NEXUS_AUTHORIZATION";
/// Secrets read from a variable of their own (`PINAKES_NAME`, one underscore), as the sources
/// name them: a source's own section's (`also_on::OwnSection::VARIABLES`) and an official
/// source's token (`sources::Source::token_variable`). A build that has no such setting ignores
/// the variable, where a `PINAKES__SECTION__KEY` it does not know stops it.
fn named_variables() -> Vec<(&'static str, Vec<&'static str>, Named)> {
    let sections = crate::also_on::sections().flat_map(|s| {
        s.variables
            .iter()
            .map(move |(name, key)| (*name, vec![s.name, *key], Named::Sets))
    });
    let tokens = crate::sources::REGISTRY.iter().filter_map(|s| {
        Some((
            s.token_variable()?,
            vec!["sources", s.name(), "token"],
            Named::Fills,
        ))
    });
    let app_keys = crate::also_on::registered().iter().filter_map(|s| {
        let (key, secret) = s.app_key_variables()?;
        Some([(key, "key"), (secret, "secret")].map(|(variable, field)| {
            (
                variable,
                vec!["also_on", "sources", s.id(), field],
                Named::Fills,
            )
        }))
    });
    let nexus = std::iter::once((
        NEXUS_AUTHORIZATION,
        vec!["nexus_authorization"],
        Named::Owns,
    ));
    sections
        .chain(tokens)
        .chain(app_keys.flatten())
        .chain(nexus)
        .collect()
}
/// How a named variable meets the file form.
#[derive(Clone, Copy, PartialEq)]
enum Named {
    /// It replaces what the file and the `PINAKES__…` form say.
    Sets,
    /// It fills the value only when the file and the `PINAKES__…` form leave it unset (a source's
    /// entry is created for it).
    Fills,
    /// As `Sets`, and set empty it clears the value: the credential is the variable's
    /// (`PINAKES_NEXUS_AUTHORIZATION`), so an operator takes it away by emptying it.
    Owns,
}
/// Apply the named variables that are set onto the file form: an empty one is skipped, unless
/// it clears its value (`Named::Owns`).
pub(crate) fn apply_named_env(
    value: &mut serde_json::Value,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Result<()> {
    let named_variables = named_variables();
    for (key, val) in vars {
        let Some((_, path, named)) = named_variables.iter().find(|(name, _, _)| *name == key)
        else {
            continue;
        };
        if val.trim().is_empty() && *named != Named::Owns {
            continue;
        }
        let (last, sections) = path.split_last().expect("a path");
        let mut node = &mut *value;
        for (depth, part) in sections.iter().enumerate() {
            node = if *named == Named::Fills && depth > 0 {
                // A source's entry under `sources` may be absent: a filling variable creates it.
                let entry = node
                    .as_object_mut()
                    .context("unknown config section")?
                    .entry(part.to_string())
                    .or_insert(serde_json::Value::Null);
                if entry.is_null() {
                    *entry = serde_json::json!({});
                }
                entry
            } else {
                node.get_mut(*part).context("unknown config section")?
            };
        }
        let node = node.as_object_mut().context("unknown config section")?;
        if *named == Named::Fills && node.get(*last).is_some_and(|v| !v.is_null()) {
            continue;
        }
        let val = if val.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(val)
        };
        node.insert(last.to_string(), val);
    }
    Ok(())
}
/// Apply `PINAKES__SECTION__KEY` overrides onto the file form. Strings are literal; numbers,
/// booleans and arrays are JSON. A new map entry's keys are strings except `enabled`, and a
/// new `limits.host_rates` entry is a number.
pub(crate) fn apply_env(
    value: &mut serde_json::Value,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Result<()> {
    for (key, val) in vars
        .into_iter()
        .filter_map(|(k, v)| k.strip_prefix("PINAKES__").map(|s| (s.to_lowercase(), v)))
    {
        let parts: Vec<_> = key.split("__").collect();
        let in_map = |depth: usize| {
            ENV_MAPS
                .iter()
                .any(|m| m.len() <= depth && m.iter().zip(&parts).all(|(a, b)| a == b))
        };
        let mut node = &mut *value;
        for (depth, part) in parts[..parts.len() - 1].iter().enumerate() {
            if in_map(depth) && node.get(*part).is_none() {
                node.as_object_mut()
                    .context("unknown config section")?
                    .insert(part.to_string(), serde_json::json!({}));
            }
            node = node.get_mut(*part).context("unknown config section")?;
        }
        let last = parts[parts.len() - 1];
        let created = node.get(last).is_none() && in_map(parts.len() - 1);
        if created {
            let new = if last == "enabled" || HOST_RATES.iter().zip(&parts).all(|(a, b)| a == b) {
                serde_json::from_str(&val).context("env override requires JSON value")?
            } else {
                serde_json::Value::String(val)
            };
            node.as_object_mut()
                .context("unknown config section")?
                .insert(last.to_string(), new);
            continue;
        }
        let target = node.get_mut(last).context("unknown config key")?;
        *target = if target.is_string() || (target.is_null() && val != "null") {
            serde_json::Value::String(val)
        } else {
            serde_json::from_str(&val).context("env override requires JSON value")?
        };
    }
    Ok(())
}
impl Config {
    /// The built-in defaults, with `limits` not yet read back from them.
    fn base() -> Self {
        Self {
            limits: Limits::zeroed(),
            database_url: "postgresql:///pinakes?host=/var/run/postgresql".into(),
            listen: "127.0.0.1:8090".into(),
            admin_listen: "127.0.0.1:9090".into(),
            admin_token: None,
            refresh: RefreshConfig::default(),
            canary: CanaryConfig::default(),
            also_on: Default::default(),
            podcast_index: Default::default(),
            sections: Sections::defaults(),
            nexus_url: "http://127.0.0.1:8080".into(),
            nexus_authorization: None,
            nexus_reach_set: false,
            poll_secs: 5,
            ingest: IngestConfig::default(),
            grace_days: 7,
            // The web route (and the event routes on it) fetches arbitrary hosts: twice the
            // other resolvers' workers and rate.
            web: ResolverConfig {
                concurrency: 4,
                requests_per_second: 4.0,
                ..ResolverConfig::default()
            },
            pubky: ResolverConfig {
                pkarr_relays: pubky_section::default_relays(),
                ..ResolverConfig::default()
            },
            cards: Default::default(),
            media: Default::default(),
            sources: Default::default(),
            event: EventConfig::default(),
            geo: GeoConfig::default(),
            osm: OsmConfig::default(),
            search: SearchConfig::default(),
            egress: EgressConfig::default(),
            http: HttpConfig::default(),
            queue: QueueConfig::default(),
            groups: Default::default(),
            preview: PreviewConfig::default(),
            api: ApiConfig::default(),
        }
    }
}
/// Which zone sources make a recurring series with a UTC (`Z`) DTSTART expand on local wall-clock
/// time instead of in UTC (RFC 5545).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UtcSeriesWallClock {
    /// An explicit `X-WR-TIMEZONE` (Google's exports) or a `TZID` on DTSTART.
    #[default]
    XWrTimezone,
    /// Any zone the series has, including `default_timezone` and coordinate-derived zones.
    Coordinates,
    /// Never: UTC series always expand in UTC.
    Never,
}
/// A "possibly the same" hint (`maybe_same` on event cards, `[event] same_event_hint`; on
/// recording cards, `[groups] same_recording_hint`): `on` or `off`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SameEventHint {
    #[default]
    On,
    Off,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventConfig {
    #[serde(skip)]
    pub container: crate::event::ContainerLimits,
    #[serde(skip)]
    pub max_line_bytes: usize,
    #[serde(skip)]
    pub max_date_values: usize,
    pub adapters: crate::event::adapters::AdapterConfig,
    pub enabled: bool,
    pub calendars: bool,
    pub geocoding: bool,
    /// Wall-clock expansion of UTC-DTSTART series (see `UtcSeriesWallClock`).
    pub utc_series_wall_clock: UtcSeriesWallClock,
    /// Event cards name other events that are possibly the same one, and app store cards other
    /// store cards that are possibly the same app (`maybe_same`).
    pub same_event_hint: SameEventHint,
    #[serde(skip)]
    pub feed_body_bytes: usize,
    #[serde(skip)]
    pub max_occurrences_per_series: usize,
    #[serde(skip)]
    pub max_occurrences_per_feed: usize,
    #[serde(skip)]
    pub max_components: usize,
    #[serde(skip)]
    pub max_event_revisions: usize,
    #[serde(skip)]
    pub max_event_document_bytes: usize,
    #[serde(skip)]
    pub max_recurrence_steps: usize,
    #[serde(skip)]
    pub max_period_dates: usize,
    #[serde(skip)]
    pub parse_timeout_ms: u64,
    #[serde(skip)]
    pub months_back: u32,
    #[serde(skip)]
    pub months_forward: u32,
    pub default_timezone: Option<String>,
    #[serde(skip)]
    pub geocode_score: f64,
    #[serde(skip)]
    pub held_retry_secs: u64,
    /// The most events that may name one event in the same-event hint
    /// (`limits.same_event_incoming_kept`).
    #[serde(skip)]
    pub hint_incoming_kept: i64,
    /// Events the same-event sweep drops per transaction (`limits.same_event_drop_batch`).
    #[serde(skip)]
    pub hint_drop_batch: i64,
}
impl Default for EventConfig {
    fn default() -> Self {
        Self {
            adapters: Default::default(),
            enabled: true,
            container: Default::default(),
            max_line_bytes: 65536,
            max_date_values: 4096,
            calendars: true,
            geocoding: true,
            utc_series_wall_clock: UtcSeriesWallClock::XWrTimezone,
            same_event_hint: SameEventHint::On,
            feed_body_bytes: 32 * 1024 * 1024,
            max_occurrences_per_series: 50,
            max_occurrences_per_feed: 10000,
            max_components: 100000,
            max_event_revisions: 10000,
            max_event_document_bytes: 8 * 1024 * 1024,
            max_recurrence_steps: 100000,
            max_period_dates: crate::event::time::MAX_PERIOD_DATES,
            parse_timeout_ms: 2000,
            months_back: 1,
            months_forward: 12,
            default_timezone: None,
            geocode_score: 0.8,
            held_retry_secs: 60,
            hint_incoming_kept: 40,
            hint_drop_batch: 500,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IngestConfig {
    pub max_label_bytes: usize,
    pub max_object_path_bytes: usize,
}
impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            max_label_bytes: 128,
            max_object_path_bytes: 2048,
        }
    }
}
/// `[web]` and `[cards.media]` in the file: `enabled` and `discover_feeds` onto the shared
/// resolver settings.
pub(crate) mod feed_section {
    use super::ResolverConfig;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    #[derive(Deserialize, Serialize)]
    #[serde(default, deny_unknown_fields)]
    struct Web {
        enabled: bool,
        discover_feeds: bool,
    }
    impl Default for Web {
        fn default() -> Self {
            let d = ResolverConfig::default();
            Self {
                enabled: d.enabled,
                discover_feeds: d.discover_feeds,
            }
        }
    }
    pub fn serialize<S: Serializer>(c: &ResolverConfig, s: S) -> Result<S::Ok, S::Error> {
        Web {
            enabled: c.enabled,
            discover_feeds: c.discover_feeds,
        }
        .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ResolverConfig, D::Error> {
        let w = Web::deserialize(d)?;
        Ok(ResolverConfig {
            enabled: w.enabled,
            discover_feeds: w.discover_feeds,
            ..Default::default()
        })
    }
}
/// `[pubky]` in the file: `enabled` and `pkarr_relays`, the relays (HTTPS base URLs) that
/// Pubky owners' PKARR records are fetched from.
pub(crate) mod pubky_section {
    use super::ResolverConfig;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    #[derive(Deserialize, Serialize)]
    #[serde(default, deny_unknown_fields)]
    struct Pubky {
        enabled: bool,
        pkarr_relays: Vec<String>,
    }
    pub fn default_relays() -> Vec<String> {
        crate::resolver::pkarr_relays::DEFAULT_RELAYS
            .map(String::from)
            .to_vec()
    }
    impl Default for Pubky {
        fn default() -> Self {
            Self {
                enabled: ResolverConfig::default().enabled,
                pkarr_relays: default_relays(),
            }
        }
    }
    pub fn serialize<S: Serializer>(c: &ResolverConfig, s: S) -> Result<S::Ok, S::Error> {
        Pubky {
            enabled: c.enabled,
            pkarr_relays: c.pkarr_relays.clone(),
        }
        .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ResolverConfig, D::Error> {
        let p = Pubky::deserialize(d)?;
        Ok(ResolverConfig {
            enabled: p.enabled,
            pkarr_relays: p.pkarr_relays,
            ..Default::default()
        })
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResolverConfig {
    /// `pubky.pkarr_relays` only (`pubky_section`).
    #[serde(skip)]
    pub pkarr_relays: Vec<String>,
    /// `web.discover_feeds` and `cards.media.discover_feeds` only: follow RSS alternates (podcast
    /// feeds) on pages without a typed card, on the web route and on the media card route.
    #[serde(skip)]
    pub discover_feeds: bool,
    pub enabled: bool,
    #[serde(skip)]
    pub concurrency: usize,
    #[serde(skip)]
    pub requests_per_second: f64,
    #[serde(skip)]
    pub ttl_secs: u64,
    /// `web` only: the MusicBrainz web service its pages' cards are read from, the also-on
    /// sources' (`[also_on.sources.musicbrainz] base_url`); empty is the public one.
    #[serde(skip)]
    pub musicbrainz_base: String,
    /// `pubky` only: the most dates one period of a recurrence rule may hold
    /// (`limits.event_max_period_dates`), for the Pubky apps' event readers.
    #[serde(skip)]
    pub max_period_dates: usize,
}
impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            discover_feeds: false,
            pkarr_relays: vec![],
            enabled: true,
            concurrency: 2,
            requests_per_second: 2.0,
            ttl_secs: 86400,
            musicbrainz_base: String::new(),
            max_period_dates: crate::event::time::MAX_PERIOD_DATES,
        }
    }
}
/// `[http]`: how Pinakes introduces itself to the services it reads.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    /// The operator's contact address, sent in the User-Agent of every request
    /// (`pinakes/<version> (+https://pubky.app; <contact>)`). Empty: none, and the links of
    /// sources that need a contact wait. An address naming github is refused: some services
    /// block such User-Agents.
    pub contact: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    #[serde(skip)]
    pub host_requests_per_second: f64,
    /// `http.contact`, copied at load.
    #[serde(skip)]
    pub contact: String,
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
    #[serde(skip)]
    pub host_rates: BTreeMap<String, f64>,
    /// The official-API sources' published rates per host (`sources::Source::budgets`), used
    /// where `host_rates` names no rate.
    #[serde(skip)]
    pub source_rates: BTreeMap<String, f64>,
    /// How far an HTML page is read while its head is still open at the 1 MB page cap.
    #[serde(skip)]
    pub html_head_bytes: usize,
}
impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            host_requests_per_second: 1.0,
            contact: String::new(),
            allow_hosts: vec![],
            deny_hosts: vec![],
            host_rates: BTreeMap::new(),
            source_rates: BTreeMap::new(),
            html_head_bytes: 4 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    #[serde(skip)]
    pub groups: crate::groups::GroupConfig,
    pub max_failures: u32,
    pub timeout_grace: u32,
    pub backoff_secs: u64,
    pub max_backoff_secs: u64,
    pub lease_secs: u64,
    pub missing_ttl_secs: u64,
    pub author_requests_per_second: f64,
}
impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            groups: Default::default(),
            max_failures: 5,
            timeout_grace: 3,
            backoff_secs: 30,
            max_backoff_secs: 3600,
            lease_secs: 60,
            missing_ttl_secs: 3600,
            author_requests_per_second: 1.0,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreviewConfig {
    pub requests_per_minute: usize,
    pub uri_requests_per_minute: usize,
    pub cache_capacity: usize,
}
impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 30,
            uri_requests_per_minute: 30,
            cache_capacity: 10000,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OsmConfig {
    pub enabled: bool,
    pub nominatim_url: String,
    pub requests_per_second: f64,
    #[serde(skip)]
    pub ttl_secs: u64,
    /// Where the daemon writes the ODbL dump file (`osm_place.ndjson.gz`).
    pub dump_dir: String,
    #[serde(skip)]
    pub dump_every_hours: u64,
    #[serde(skip)]
    pub dump_downloads_per_hour: usize,
    /// `http.contact`, copied at load.
    #[serde(skip)]
    pub contact: String,
}
impl Default for OsmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            nominatim_url: "https://nominatim.openstreetmap.org".into(),
            requests_per_second: 0.5,
            ttl_secs: 30 * 86400,
            dump_dir: "dumps".into(),
            dump_every_hours: 24,
            dump_downloads_per_hour: 6,
            contact: String::new(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub reach_timeout_secs: u64,
    /// The taggers a related page samples: the authors of a resource's most recent tags, at most
    /// this many.
    pub related_taggers: usize,
    /// The days a related page looks back, for those taggers and their tags.
    pub related_window_days: u32,
    /// The references a related page reads per sampled tagger (their most recent).
    pub related_refs_per_tagger: usize,
    /// The shared taggers a related row needs (its group's, when collapsed).
    pub related_min_shared: usize,
    /// The tags a related page reads per seed link (its most recent), from which the taggers
    /// are sampled.
    pub related_tag_sample: usize,
    /// The seed links a related page samples: of the resource, its group's members and their
    /// aliases, the ones with the newest tags, at most this many.
    pub related_seed_links: usize,
    /// The references counted per group member when `collapse=group` picks a group's row.
    pub collapse_refs_per_member: usize,
    pub max_radius_m: f64,
    pub h3_resolutions: Vec<u8>,
    pub max_cluster_cells: usize,
}
impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            reach_timeout_secs: 5,
            related_taggers: 200,
            related_window_days: 365,
            related_refs_per_tagger: 1000,
            related_min_shared: 2,
            related_tag_sample: 2000,
            related_seed_links: 32,
            collapse_refs_per_member: 10_000,
            max_radius_m: 200_000.0,
            h3_resolutions: (0u8..=22).map(|z| z.saturating_sub(4).min(11)).collect(),
            max_cluster_cells: 10000,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    pub requests_per_minute: usize,
    #[serde(skip)]
    pub cache_capacity: usize,
    #[serde(skip)]
    pub reach_cost: usize,
    #[serde(skip)]
    pub related_cost: usize,
    /// Browser origins allowed to call the public listener (CORS): `["*"]` (any origin; the
    /// public API takes no cookies or tokens), a list of `scheme://host[:port]` origins, or `[]`
    /// to send no CORS headers. Never applied to the admin listener.
    pub cors_allow_origins: Vec<String>,
    /// Serve Swagger UI for the public document at `/swagger-ui` (as pubky-nexus does).
    pub swagger_ui: bool,
    /// Serve the MCP endpoint, `POST /mcp`, on the public listener (docs/api.md, Agents).
    pub mcp: bool,
}
impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 120,
            cache_capacity: 10000,
            reach_cost: 6,
            related_cost: 6,
            cors_allow_origins: vec!["*".into()],
            swagger_ui: true,
            mcp: true,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeoConfig {
    pub enabled: bool,
}
impl Default for GeoConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefreshConfig {
    pub resource_secs: [u64; 4],
    pub container_secs: [u64; 4],
    pub cursor_secs: u64,
    pub retention_days: u32,
}
impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            resource_secs: [7 * 86400, 2 * 86400, 6 * 3600, 3600],
            container_secs: [86400, 12 * 3600, 6 * 3600, 3600],
            cursor_secs: 300,
            retention_days: 90,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CanaryConfig {
    pub enabled: bool,
    #[serde(skip)]
    pub interval_secs: u64,
}
impl Default for CanaryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 6 * 3600,
        }
    }
}
impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut value = Self::defaults()?;
        if let Some(path) = path {
            let file: toml::Value = toml::from_str(
                &std::fs::read_to_string(path)
                    .with_context(|| format!("read {}", path.display()))?,
            )?;
            let file = serde_json::to_value(file)?;
            merge(&mut value, file);
        }
        apply_env(&mut value, std::env::vars())?;
        apply_named_env(&mut value, std::env::vars())?;
        Self::from_value(value)
    }
    /// The defaults in their file form. Secrets serialize as `[REDACTED]`, so their real defaults
    /// are put back here.
    fn defaults() -> Result<serde_json::Value> {
        let config = Self::default();
        let mut value = serde_json::to_value(&config)?;
        value["database_url"] = (*config.database_url).into();
        value["admin_token"] = config.admin_token.as_deref().into();
        Ok(value)
    }
    /// A configuration from its file form (sections plus `[limits]`), validated.
    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        let mut cfg: Self = serde_json::from_value(value)?;
        cfg.nexus_authorization = cfg
            .nexus_authorization
            .take()
            .filter(|a| !a.trim().is_empty());
        for (name, rules) in [
            ("egress.allow_hosts", &mut cfg.egress.allow_hosts),
            ("egress.deny_hosts", &mut cfg.egress.deny_hosts),
        ] {
            for rule in rules.iter_mut() {
                *rule = host_rule(rule).with_context(|| {
                    format!("{name}: {rule:?} is not a host name or `*.` and one")
                })?;
            }
        }
        let limits = cfg.limits.clone();
        limits.apply(&mut cfg);
        cfg.queue.groups = cfg.groups.clone();
        cfg.share_contact();
        cfg.validate()?;
        Ok(cfg)
    }
    /// Every HTTP client sends `http.contact` (`resolver::egress::user_agent`).
    fn share_contact(&mut self) {
        self.egress.contact = self.http.contact.trim().to_owned();
        self.osm.contact = self.egress.contact.clone();
        self.web.musicbrainz_base = self.also_on.musicbrainz_base().to_owned();
        // The Pubky apps' event readers bound recurrence rules as calendars do.
        self.pubky.max_period_dates = self.event.max_period_dates;
        // A source's published limits, for hosts the operator does not rate.
        self.egress.source_rates = crate::sources::budgets()
            .chain(crate::also_on::budgets())
            .map(|(host, rate)| (host.to_owned(), rate))
            .collect();
    }
    pub fn validate(&self) -> Result<()> {
        if self.pubky.enabled {
            if self.pubky.pkarr_relays.is_empty() {
                bail!("[pubky] pkarr_relays must list at least one relay");
            }
            // An entry is named by its place in the list and its host, never in full: a relay URL
            // with credentials would put them in the startup error.
            for (n, relay) in self.pubky.pkarr_relays.iter().enumerate() {
                let Ok(url) = url::Url::parse(relay) else {
                    bail!("[pubky] pkarr_relays: entry {} is not a URL", n + 1);
                };
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    bail!(
                        "[pubky] pkarr_relays: entry {} ({}) must be an https:// base URL without credentials, query or fragment",
                        n + 1,
                        url.host_str().unwrap_or("no host")
                    );
                }
            }
        }
        self.cards.validate()?;
        self.sources.validate()?;
        let contact = self.http.contact.trim();
        if !contact.is_empty()
            && (contact.len() > 254
                || !contact.contains('@')
                || contact
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() || "()<>;".contains(c)))
        {
            bail!("http.contact must be an email address (or empty)");
        }
        // sec.gov refuses User-Agents that mention github as undeclared tools.
        if contact.to_ascii_lowercase().contains("github") {
            bail!("http.contact must not name github: some services refuse such User-Agents");
        }
        let origins = &self.api.cors_allow_origins;
        if origins.iter().any(|o| o == "*") && origins.len() > 1 {
            bail!("api.cors_allow_origins: \"*\" must be the only entry");
        }
        for origin in origins.iter().filter(|o| *o != "*") {
            // Origins are exact: the url crate accepts `*` in a host, which would never match.
            let valid = !origin.contains('*')
                && url::Url::parse(origin).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "https")
                        && u.host_str().is_some()
                        && u.origin().ascii_serialization() == *origin
                });
            if !valid {
                bail!("api.cors_allow_origins: {origin:?} is not an origin (scheme://host[:port], no path or trailing slash)");
            }
        }
        let admin: std::net::SocketAddr =
            self.admin_listen.parse().context("invalid admin_listen")?;
        if self.admin_token.as_ref().is_some_and(|t| {
            t.trim().is_empty() || t.len() > 4096 || t.chars().any(char::is_control)
        }) {
            bail!("invalid admin_token");
        }
        if !admin.ip().is_loopback() && self.admin_token.is_none() {
            bail!("non-loopback admin_listen requires admin_token (PINAKES__ADMIN_TOKEN)");
        }
        if !(10000..=100000).contains(&self.groups.max_graph_units)
            || !(1024 * 1024..=128 * 1024 * 1024).contains(&self.groups.max_graph_bytes)
            || !(2..=1000).contains(&self.groups.max_members)
            || self.groups.max_graph_units < self.groups.max_members
            || !self.groups.max_distance_m.is_finite()
            || !(1.0..=10000.0).contains(&self.groups.max_distance_m)
        {
            bail!("invalid grouping limits");
        }
        crate::event::adapters::validate(&self.event)?;
        let known = crate::also_on::source_ids();
        if let Some(unknown) = self
            .also_on
            .sources
            .keys()
            .find(|id| !known.contains(&id.as_str()))
        {
            bail!("unknown also_on source {unknown}");
        }
        for (id, options) in &self.also_on.sources {
            if let Some(base) = &options.base_url {
                let parsed = url::Url::parse(base).ok().filter(|u| {
                    matches!(u.scheme(), "http" | "https")
                        && u.host_str().is_some()
                        && u.username().is_empty()
                        && u.password().is_none()
                        && u.query().is_none()
                        && u.fragment().is_none()
                        && !base.ends_with('/')
                });
                if id != "musicbrainz" || parsed.is_none() {
                    bail!("also_on.sources.{id}.base_url must be an http(s) URL without a trailing slash, and only musicbrainz takes one");
                }
            }
            if let Some(token) = options.token.as_deref().filter(|t| !t.is_empty()) {
                let takes = crate::also_on::registered()
                    .iter()
                    .any(|s| s.id() == id && s.takes_token());
                if !takes || token.len() > 256 || !token.bytes().all(|b| b.is_ascii_graphic()) {
                    bail!("also_on.sources.{id}.token must be at most 256 printable characters, for a source that takes one");
                }
            }
            for (field, value) in [("key", &options.key), ("secret", &options.secret)] {
                if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
                    let takes = crate::also_on::registered()
                        .iter()
                        .any(|s| s.id() == id && s.app_key_variables().is_some());
                    // Printable and without a comma: they go into one header.
                    if !takes
                        || value.len() > 256
                        || !value.bytes().all(|b| b.is_ascii_graphic() && b != b',')
                    {
                        bail!("also_on.sources.{id}.{field} must be at most 256 printable characters without a comma, for a source that takes an app key");
                    }
                }
            }
        }
        let a = &self.also_on;
        if !(1..=256).contains(&a.links_per_source)
            || !(1..=64).contains(&a.keys_per_source)
            || !(8..=256).contains(&a.facts_per_source)
            || !(1..=16).contains(&a.concurrency)
            || !(1..=86400).contains(&a.retry_secs)
        {
            bail!("invalid also_on limits");
        }
        self.sections.check()?;
        if a.apple_storefront.len() != 2
            || !a.apple_storefront.bytes().all(|b| b.is_ascii_lowercase())
        {
            bail!("also_on.apple_storefront must be a two-letter country code in lower case");
        }
        if !(1024 * 1024..=crate::resolver::egress::MAX_HTML_HEAD_BYTES)
            .contains(&self.egress.html_head_bytes)
        {
            bail!("limits.html_head_bytes must be 1–16 MB");
        }
        let e = &self.event;
        let c = &e.container;
        if !(1..=1000).contains(&c.max_changes)
            || !(1..=10000).contains(&c.max_rows)
            || !(1..=10000).contains(&c.max_new_keys)
            || !(1..=3650).contains(&c.retention_days)
            || !(1..=32).contains(&c.max_holds)
            || !(3600..=604800).contains(&c.cadence_floor)
        {
            bail!("invalid container storage limits");
        }
        if !(1024..=262144).contains(&e.max_line_bytes)
            || !(1..=10000).contains(&e.max_date_values)
            || !(1024..=64 * 1024 * 1024).contains(&e.feed_body_bytes)
            || !(1..=1000).contains(&e.max_occurrences_per_series)
            || !(1..=10000).contains(&e.max_occurrences_per_feed)
            || !(1..=100000).contains(&e.max_components)
            || !(1..=10000).contains(&e.max_event_revisions)
            || !(1024..=32 * 1024 * 1024).contains(&e.max_event_document_bytes)
            || !(1..=1000000).contains(&e.max_recurrence_steps)
            || !(1..=10_000_000).contains(&e.max_period_dates)
            || !(1..=5000).contains(&e.parse_timeout_ms)
            || e.months_back > 12
            || !(1..=36).contains(&e.months_forward)
            || !e.geocode_score.is_finite()
            || !(0.0..=1.0).contains(&e.geocode_score)
            || !(1..=604800).contains(&e.held_retry_secs)
            || !(1..=10_000).contains(&e.hint_incoming_kept)
            || !(1..=100_000).contains(&e.hint_drop_batch)
            || e.default_timezone
                .as_deref()
                .is_some_and(|s| crate::event::time::zone(s).is_none())
        {
            bail!("invalid event budget or timezone");
        }
        self.listen.parse::<std::net::SocketAddr>()?;
        self.admin_listen.parse::<std::net::SocketAddr>()?;
        if self.admin_listen == self.listen
            || self
                .refresh
                .resource_secs
                .iter()
                .chain(self.refresh.container_secs.iter())
                .any(|s| *s == 0 || *s > 31536000)
            || !(1..=86400).contains(&self.refresh.cursor_secs)
            || !(1..=3650).contains(&self.refresh.retention_days)
            || !(3600..=604800).contains(&self.canary.interval_secs)
        {
            bail!("invalid refresh, canary or admin configuration");
        }
        if !plain_upstream(&self.nexus_url).unwrap_or(false) {
            bail!("invalid Nexus URL: an http(s) base without user info, query or fragment; send credentials with {NEXUS_AUTHORIZATION}");
        }
        // Never shown: the value is a credential.
        if self
            .nexus_authorization
            .as_ref()
            .is_some_and(|a| a.len() > 4096 || reqwest::header::HeaderValue::from_str(a).is_err())
        {
            bail!("invalid {NEXUS_AUTHORIZATION}: not a header value");
        }
        if !plain_upstream(&self.osm.nominatim_url)? {
            bail!("invalid OSM upstream URL");
        }
        if !self.osm.requests_per_second.is_finite()
            || !(0.001..=1000.0).contains(&self.osm.requests_per_second)
            || url::Url::parse(&self.osm.nominatim_url)
                .is_ok_and(|u| crate::resolver::osm::public_upstream(&u))
                && self.osm.requests_per_second > 1.0
            || self.osm.ttl_secs == 0
            || self.osm.ttl_secs > 31536000
            || self.osm.dump_dir.trim().is_empty()
            || !(1..=168).contains(&self.osm.dump_every_hours)
            || !(1..=1000).contains(&self.osm.dump_downloads_per_hour)
            || !(122..=100000).contains(&self.search.max_cluster_cells)
            || !(1..=10000).contains(&self.api.requests_per_minute)
            || !(2..=100000).contains(&self.api.cache_capacity)
            || !(2..=100).contains(&self.api.reach_cost)
            || !(2..=100).contains(&self.api.related_cost)
            || !(1..=60).contains(&self.search.reach_timeout_secs)
            || !(1..=10_000).contains(&self.search.related_taggers)
            || !(1..=3650).contains(&self.search.related_window_days)
            || !(1..=100_000).contains(&self.search.related_refs_per_tagger)
            || !(1..=10_000).contains(&self.search.related_min_shared)
            || !(1..=100_000).contains(&self.search.related_tag_sample)
            || !(1..=1000).contains(&self.search.related_seed_links)
            || !(1..=1_000_000).contains(&self.search.collapse_refs_per_member)
            || !self.search.max_radius_m.is_finite()
            || !(1.0..=200_000.0).contains(&self.search.max_radius_m)
            || self.search.h3_resolutions.is_empty()
            || self.search.h3_resolutions.len() > 31
            || self.search.h3_resolutions.iter().any(|r| *r > 11)
        {
            bail!("invalid OSM or spatial budget (public Nominatim maximum 1 request/s)");
        }
        for r in [&self.web, &self.pubky] {
            if r.concurrency == 0
                || r.concurrency > 128
                || !r.requests_per_second.is_finite()
                || !(0.001..=1000.0).contains(&r.requests_per_second)
                || r.ttl_secs == 0
                || r.ttl_secs > 31536000
            {
                bail!("invalid resolver budget");
            }
        }
        for rate in std::iter::once(&self.egress.host_requests_per_second)
            .chain(self.egress.host_rates.values())
            .chain(std::iter::once(&self.queue.author_requests_per_second))
        {
            if !rate.is_finite() || !(0.001..=1000.0).contains(rate) {
                bail!("host/author rates must be 0.001–1000 per second");
            }
        }
        if !(1..=3600).contains(&self.poll_secs)
            || !(1..=1024).contains(&self.ingest.max_label_bytes)
            || !(1..=2048).contains(&self.ingest.max_object_path_bytes)
            || self.grace_days > 3650
            || !(1..=32).contains(&self.queue.max_failures)
            || self.queue.timeout_grace > 32
            || !(30..=3600).contains(&self.queue.lease_secs)
            || self.queue.backoff_secs == 0
            || self.queue.max_backoff_secs > 86400
            || self.queue.max_backoff_secs < self.queue.backoff_secs
            || !(1..=86400).contains(&self.queue.missing_ttl_secs)
            || self.preview.requests_per_minute == 0
            || self.preview.uri_requests_per_minute == 0
            || !(1..=100000).contains(&self.preview.cache_capacity)
        {
            bail!("invalid queue, preview or polling budget");
        }
        Ok(())
    }
}
fn merge(dst: &mut serde_json::Value, src: serde_json::Value) {
    match (dst, src) {
        (serde_json::Value::Object(dst), serde_json::Value::Object(src)) => {
            for (k, v) in src {
                merge(dst.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (dst, src) => *dst = src,
    }
}

/// An upstream service's URL is a web URL with a host, and holds no secret: no user info, query
/// or fragment, which logs and errors would otherwise carry.
/// An egress host rule as the policy compares hosts (`egress::canonical_host`, the one reading
/// for rules and the hosts asked alike: `faß.de。` is `xn--fa-hia.de`, `8.8.8..` is `8.8.0.8`),
/// a leading `*.` kept for a name. `None` for what names no host, a name with a `*` inside it, or
/// a wildcard on an address. Idempotent: a canonical rule is its own canonical form.
fn host_rule(rule: &str) -> Option<String> {
    let rule = rule.trim();
    let (wild, name) = match rule.strip_prefix("*.") {
        Some(name) => (true, name),
        None => (false, rule),
    };
    match crate::resolver::egress::canonical_host(name)? {
        url::Host::Domain(d) => {
            (!d.contains('*') && !d.starts_with('.'))
                .then(|| if wild { format!("*.{d}") } else { d })
        }
        ip if !wild => Some(ip.to_string()),
        _ => None,
    }
}
fn plain_upstream(raw: &str) -> Result<bool> {
    let url = url::Url::parse(raw)?;
    Ok(matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn with(change: serde_json::Value) -> Result<Config> {
        let mut value = Config::defaults().unwrap();
        merge(&mut value, change);
        Config::from_value(value)
    }
    /// An egress with these rules, for asking its policy (`allows`); it resolves to a fixed
    /// address and answers from recordings, never the network.
    fn policy(cfg: &Config) -> crate::resolver::egress::Egress {
        let (_, recorded) = crate::sources::fixture::egress(vec![]);
        crate::resolver::egress::Egress::with_backends(
            cfg.egress.clone(),
            std::sync::Arc::new(crate::sources::fixture::PublicDns),
            recorded,
        )
    }
    #[test]
    fn loading_nothing_gives_the_defaults_and_limits_round_trip() {
        let loaded = Config::load(None).unwrap();
        assert_eq!(
            &*loaded.database_url,
            "postgresql:///pinakes?host=/var/run/postgresql"
        );
        assert_eq!(format!("{loaded:?}"), format!("{:?}", Config::default()));
        assert_eq!(Limits::of(&Config::default()), Limits::default());
        let mut applied = Config::default();
        Limits::default().apply(&mut applied);
        assert_eq!(format!("{applied:?}"), format!("{:?}", Config::default()));
    }
    #[test]
    fn related_limits_have_config_bounds() {
        assert_eq!(Config::default().search.related_seed_links, 32);
        assert_eq!(Config::default().api.related_cost, 6);
        for (key, value) in [
            ("related_seed_links", 0),
            ("related_seed_links", 1001),
            ("related_cost", 1),
            ("related_cost", 101),
        ] {
            assert!(
                with(serde_json::json!({"limits":{key:value}})).is_err(),
                "{key}={value}"
            );
        }
    }
    #[test]
    fn the_pubky_readers_take_the_recurrence_period_limit() {
        assert_eq!(Config::default().pubky.max_period_dates, 100_000);
        let cfg = with(serde_json::json!({"limits":{"event_max_period_dates":2}})).unwrap();
        assert_eq!(cfg.pubky.max_period_dates, 2);
    }
    #[test]
    fn the_recurrence_period_limit_has_a_default_and_bounds() {
        assert_eq!(Config::default().event.max_period_dates, 100_000);
        for (key, value) in [
            ("event_max_period_dates", 0),
            ("event_max_period_dates", 10_000_001),
        ] {
            assert!(
                with(serde_json::json!({"limits":{key:value}})).is_err(),
                "{key}={value}"
            );
        }
        assert_eq!(
            with(serde_json::json!({"limits":{"event_max_period_dates":2}}))
                .unwrap()
                .event
                .max_period_dates,
            2
        );
    }
    #[test]
    fn operations_md_documents_every_limit() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/operations.md");
        let text = std::fs::read_to_string(path).unwrap();
        let limits = serde_json::to_value(Limits::default()).unwrap();
        for key in limits.as_object().unwrap().keys() {
            assert!(text.contains(&format!("| `{key}` |")), "{key}");
        }
    }
    /// A source's own token variable fills `[sources.<name>] token` when the file leaves it
    /// unset; the file (or the `PINAKES__…` form) wins; an empty variable is none.
    #[test]
    fn a_token_variable_fills_what_the_file_leaves_unset() {
        // Whichever registered source has one: a build without any has nothing to check.
        let Some(source) = crate::sources::REGISTRY
            .iter()
            .find(|s| s.takes_token() && s.token_variable().is_some())
        else {
            return;
        };
        let (name, variable) = (source.name(), source.token_variable().unwrap());
        let vars = |token: &str| vec![(variable.to_owned(), token.to_owned())];
        let mut value = Config::defaults().unwrap();
        apply_named_env(&mut value, vars("from-env")).unwrap();
        let cfg = Config::from_value(value).unwrap();
        assert_eq!(cfg.sources.token(name), Some("from-env"));
        assert!(cfg.sources.enabled(name));
        let mut value = Config::defaults().unwrap();
        value["sources"][name] = serde_json::json!({"token": "from-file"});
        apply_named_env(&mut value, vars("from-env")).unwrap();
        assert_eq!(
            Config::from_value(value).unwrap().sources.token(name),
            Some("from-file")
        );
        let mut value = Config::defaults().unwrap();
        apply_named_env(&mut value, vars("")).unwrap();
        assert!(!Config::from_value(value).unwrap().sources.enabled(name));
    }

    #[test]
    fn env_overrides_create_platform_and_host_rate_entries() {
        let load = |vars: &[(&str, &str)]| {
            let mut value = Config::defaults().unwrap();
            apply_env(
                &mut value,
                vars.iter().map(|(k, v)| (k.to_string(), v.to_string())),
            )?;
            Config::from_value(value)
        };
        let cfg = load(&[
            (
                "PINAKES__EVENT__ADAPTERS__PLATFORMS__TICKETMASTER__KEY",
                "12345",
            ),
            (
                "PINAKES__EVENT__ADAPTERS__PLATFORMS__RESIDENT_ADVISOR__ENABLED",
                "true",
            ),
            ("PINAKES__LIMITS__HOST_RATES__EXAMPLE.ORG", "0.5"),
        ])
        .unwrap();
        let platforms = &cfg.event.adapters.platforms;
        assert_eq!(platforms["ticketmaster"].key.as_deref(), Some("12345"));
        assert_eq!(platforms["resident_advisor"].enabled, Some(true));
        assert_eq!(cfg.egress.host_rates["example.org"], 0.5);
        for unknown in [
            "PINAKES__EVENT__ADAPTERS__PLATFORMS__NO_SUCH__KEY",
            "PINAKES__EVENT__ADAPTERS__PLATFORMS__TICKETMASTER__TYPO",
            "PINAKES__EVENT__NO_SUCH__KEY",
        ] {
            assert!(load(&[(unknown, "x")]).is_err(), "{unknown}");
        }
    }
    #[test]
    fn the_database_url_and_admin_token_come_from_the_environment_and_stay_secret() {
        let mut value = Config::defaults().unwrap();
        apply_env(
            &mut value,
            [
                (
                    "PINAKES__DATABASE_URL",
                    "postgresql://pinakes:ABCDEFPASS@db/pinakes",
                ),
                ("PINAKES__ADMIN_TOKEN", "ABCDEFTOKEN"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        )
        .unwrap();
        let cfg = Config::from_value(value).unwrap();
        assert_eq!(
            &*cfg.database_url,
            "postgresql://pinakes:ABCDEFPASS@db/pinakes"
        );
        assert_eq!(cfg.admin_token.as_deref(), Some("ABCDEFTOKEN"));
        let shown = format!("{cfg:?} {}", serde_json::to_string(&cfg).unwrap());
        assert!(!shown.contains("ABCDEF"), "{shown}");
    }
    #[test]
    fn the_podcast_index_pair_comes_from_the_environment_and_stays_secret() {
        let mut value = Config::defaults().unwrap();
        apply_env(
            &mut value,
            [
                ("PINAKES__PODCAST_INDEX__KEY", "ABCDEFKEY"),
                ("PINAKES__PODCAST_INDEX__SECRET", "ABCDEFSECRET"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        )
        .unwrap();
        let cfg = Config::from_value(value).unwrap();
        assert_eq!(cfg.podcast_index.key.as_deref(), Some("ABCDEFKEY"));
        assert_eq!(cfg.podcast_index.secret.as_deref(), Some("ABCDEFSECRET"));
        let shown = format!("{cfg:?} {}", serde_json::to_string(&cfg).unwrap());
        assert!(!shown.contains("ABCDEF"), "{shown}");
    }
    /// A top-level section no field and no source names is refused; each owned section shows
    /// through its source's type.
    #[test]
    fn a_section_no_source_owns_is_refused() {
        assert!(with(serde_json::json!({"no_such_section": {"key": "x"}}))
            .unwrap_err()
            .to_string()
            .contains("unknown config section [no_such_section]"));
        let cfg = Config::default();
        let shown = serde_json::to_value(&cfg).unwrap();
        for section in crate::also_on::sections() {
            assert_eq!(shown[section.name], (section.default)(), "{}", section.name);
        }
    }
    /// `.env.example` names only variables the config accepts (its commented examples too), and
    /// a key for every platform that needs one.
    #[test]
    fn the_env_example_holds_only_keys_the_config_accepts() {
        let example = include_str!("../../../.env.example");
        let vars: Vec<(String, String)> = example
            .lines()
            .map(|l| l.trim_start_matches("# "))
            .filter(|l| l.starts_with("PINAKES__"))
            .map(|l| {
                let (k, v) = l.split_once('=').unwrap();
                (k.to_owned(), v.to_owned())
            })
            .collect();
        assert!(vars.len() >= 4, "{vars:?}");
        for var in &vars {
            let mut value = Config::defaults().unwrap();
            apply_env(&mut value, [var.clone()]).unwrap_or_else(|e| panic!("{var:?}: {e:#}"));
            Config::from_value(value).unwrap_or_else(|e| panic!("{var:?}: {e:#}"));
        }
        let registry: serde_json::Value =
            serde_json::from_str(include_str!("../../../data/event-adapters.json")).unwrap();
        let adapters = registry
            .as_array()
            .or_else(|| registry["adapters"].as_array());
        for adapter in adapters.into_iter().flatten() {
            if adapter["requires_key"] == true {
                let name = format!(
                    "PINAKES__EVENT__ADAPTERS__PLATFORMS__{}__KEY=",
                    adapter["id"].as_str().unwrap().to_uppercase()
                );
                assert!(example.contains(&name), "{name}");
            }
        }
    }
    /// A relay entry that is refused is named by its place and host, never with what its URL
    /// carries (Kimi).
    #[test]
    fn a_refused_pkarr_relay_is_named_by_its_host_alone() {
        for (relay, says) in [
            (
                "https://user:CANARY@relay.example/?token=CANARY",
                "entry 2 (relay.example)",
            ),
            ("http://relay.example/CANARY", "entry 2 (relay.example)"),
            ("not a url CANARY", "entry 2 is not a URL"),
        ] {
            let error = with(serde_json::json!({
                "pubky": {"pkarr_relays": ["https://ok.example/", relay]}
            }))
            .unwrap_err()
            .to_string();
            assert!(error.contains(says), "{error}");
            assert!(!error.contains("CANARY"), "{error}");
        }
    }
    #[test]
    fn pubky_is_on_by_default_with_the_pkarr_relays() {
        let d = Config::default();
        assert!(d.pubky.enabled);
        assert_eq!(
            d.pubky.pkarr_relays,
            ["https://pkarr.pubky.app", "https://pkarr.pubky.org"]
        );
        assert!(
            !with(serde_json::json!({"pubky":{"enabled":false}}))
                .unwrap()
                .pubky
                .enabled
        );
        // An unrelated section keeps the default relays; a listed set replaces them.
        assert_eq!(
            with(serde_json::json!({"pubky":{"enabled":true}}))
                .unwrap()
                .pubky
                .pkarr_relays,
            d.pubky.pkarr_relays
        );
        let own =
            with(serde_json::json!({"pubky":{"pkarr_relays":["https://relay.example/pkarr"]}}))
                .unwrap();
        assert_eq!(own.pubky.pkarr_relays, ["https://relay.example/pkarr"]);
        for bad in [
            serde_json::json!([]),
            serde_json::json!(["http://relay.example"]),
            serde_json::json!(["https://user:pw@relay.example"]),
            serde_json::json!(["https://relay.example/?x=1"]),
            serde_json::json!(["not a url"]),
        ] {
            assert!(
                with(serde_json::json!({"pubky":{"pkarr_relays":bad}})).is_err(),
                "{bad}"
            );
        }
        // Relays belong to [pubky] only.
        assert!(with(serde_json::json!({"web":{"pkarr_relays":[]}})).is_err());
    }
    #[test]
    fn discover_feeds_belongs_to_web_and_the_media_card_only() {
        let cfg = with(serde_json::json!({"web":{"discover_feeds":true}})).unwrap();
        assert!(cfg.web.discover_feeds && cfg.web.enabled);
        assert_eq!(cfg.web.concurrency, Config::default().web.concurrency);
        let cfg = with(serde_json::json!({"cards":{"media":{"discover_feeds":true}}})).unwrap();
        assert!(cfg.cards.media.discover_feeds && !cfg.web.discover_feeds);
        for section in [
            serde_json::json!({"pubky":{"discover_feeds":true}}),
            serde_json::json!({"cards":{"doi":{"discover_feeds":true}}}),
            serde_json::json!({"cards":{"isbn":{"discover_feeds":false}}}),
            serde_json::json!({"web":{"concurrency":8}}),
        ] {
            let error = with(section.clone()).unwrap_err().to_string();
            assert!(error.contains("unknown field"), "{section}: {error}");
        }
    }
    #[test]
    fn operator_choices_reach_workers() {
        let cfg = with(serde_json::json!({"groups":{"cross_container_occurrences":"corroborated"},"media":{"oembed":"always"},"web":{"discover_feeds":true}})).unwrap();
        assert_eq!(
            cfg.groups.cross_container_occurrences,
            crate::groups::CrossContainerOccurrences::Corroborated
        );
        assert_eq!(
            cfg.queue.groups.cross_container_occurrences,
            cfg.groups.cross_container_occurrences
        );
        assert_eq!(
            cfg.media.oembed,
            crate::resolver::cards::OembedPolicy::Always
        );
        assert!(cfg.web.discover_feeds);
        assert_eq!(
            Config::default().groups.cross_container_occurrences,
            crate::groups::CrossContainerOccurrences::Off
        );
        assert!(!Config::default().web.discover_feeds);
    }
    /// The Nexus and OSM URLs hold no secret: user info, a query or a fragment is refused.
    #[test]
    fn upstream_urls_with_user_info_query_or_fragment_are_refused() {
        let nexus = |url: &str| with(serde_json::json!({ "nexus_url": url }));
        let osm = |url: &str| with(serde_json::json!({"osm":{ "nominatim_url": url }}));
        for url in [
            "http://u:p@nexus.example.org",
            "http://u@nexus.example.org",
            "http://:p@nexus.example.org",
            "http://nexus.example.org/?token=x",
            "http://nexus.example.org/?",
            "http://nexus.example.org/#x",
            "ftp://nexus.example.org",
            "unix:/run/nexus",
        ] {
            let refused = nexus(url).unwrap_err().to_string();
            // The way to send credentials, and nothing of the URL.
            assert!(
                refused.contains("PINAKES_NEXUS_AUTHORIZATION")
                    && !refused.contains("nexus.example"),
                "{refused}"
            );
            assert!(osm(url).is_err(), "{url}");
        }
        for url in [
            "http://nexus.example.org",
            "https://nexus.example.org:8443/base/",
        ] {
            assert!(nexus(url).is_ok(), "{url}");
            assert!(osm(url).is_ok(), "{url}");
        }
    }
    /// The Nexus's `Authorization` header comes from its own variable, is never shown, and is
    /// none when empty; a value that is no header value is refused without being named.
    #[test]
    fn the_nexus_authorization_comes_from_its_variable_and_stays_secret() {
        let read = |vars: &[(&str, &str)]| {
            let mut value = Config::defaults().unwrap();
            let vars = vars.iter().map(|(k, v)| (k.to_string(), v.to_string()));
            apply_env(&mut value, vars.clone())?;
            apply_named_env(&mut value, vars)?;
            Config::from_value(value)
        };
        let cfg = read(&[("PINAKES_NEXUS_AUTHORIZATION", "Bearer ABCDEFNEXUS")]).unwrap();
        assert_eq!(
            cfg.nexus_authorization.as_deref(),
            Some("Bearer ABCDEFNEXUS")
        );
        let shown = format!("{cfg:?} {}", serde_json::to_string(&cfg).unwrap());
        assert!(!shown.contains("ABCDEF"), "{shown}");
        for empty in [
            ("PINAKES_NEXUS_AUTHORIZATION", ""),
            ("PINAKES__NEXUS_AUTHORIZATION", " "),
        ] {
            assert!(
                read(&[empty]).unwrap().nexus_authorization.is_none(),
                "{empty:?}"
            );
        }
        // Set empty over a credential the file holds, either form clears it; unset, the file's
        // stands, and a value replaces it.
        let over_file = |vars: &[(&str, &str)]| {
            let mut value = Config::defaults().unwrap();
            value["nexus_authorization"] = "Bearer ABCDEFFILE".into();
            let vars = vars.iter().map(|(k, v)| (k.to_string(), v.to_string()));
            apply_env(&mut value, vars.clone())?;
            apply_named_env(&mut value, vars)?;
            Config::from_value(value)
        };
        for empty in [
            ("PINAKES_NEXUS_AUTHORIZATION", ""),
            ("PINAKES_NEXUS_AUTHORIZATION", "  "),
            ("PINAKES__NEXUS_AUTHORIZATION", ""),
        ] {
            assert!(
                over_file(&[empty]).unwrap().nexus_authorization.is_none(),
                "{empty:?}"
            );
        }
        assert_eq!(
            over_file(&[]).unwrap().nexus_authorization.as_deref(),
            Some("Bearer ABCDEFFILE")
        );
        assert_eq!(
            over_file(&[("PINAKES_NEXUS_AUTHORIZATION", "Bearer ABCDEFNEW")])
                .unwrap()
                .nexus_authorization
                .as_deref(),
            Some("Bearer ABCDEFNEW")
        );
        let refused = read(&[("PINAKES_NEXUS_AUTHORIZATION", "Bearer ABCDEF\u{7f}")])
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("PINAKES_NEXUS_AUTHORIZATION") && !refused.contains("ABCDEF"),
            "{refused}"
        );
    }
    /// Egress host rules may be written in Unicode or punycode, any case, with a trailing dot:
    /// each is compared as URL hosts are written. One that names no host stops startup, naming
    /// the rule.
    #[test]
    fn egress_host_rules_are_canonical_and_a_bad_one_is_refused() {
        let rules = |deny: &[&str]| with(serde_json::json!({"egress": {"deny_hosts": deny}}));
        let cfg = rules(&[
            "faß.de",
            "*.Bücher.DE.",
            " Example.ORG ",
            "xn--fa-hia.de",
            "10.0.0.1",
            "[::1]",
        ])
        .unwrap();
        assert_eq!(
            cfg.egress.deny_hosts,
            [
                "xn--fa-hia.de",
                "*.xn--bcher-kva.de",
                "example.org",
                "xn--fa-hia.de",
                "10.0.0.1",
                "[::1]"
            ]
        );
        let egress = policy(&cfg);
        for denied in [
            "https://xn--fa-hia.de/",
            "https://faß.de/x",
            "https://FAß.DE/",
            "https://shop.bücher.de/",
            "https://bücher.de/",
            "https://shop.xn--bcher-kva.de/",
            "https://example.org./",
        ] {
            assert!(!egress.allows(denied), "{denied}");
        }
        for allowed in ["https://fass.de/", "https://buecher.de/", "https://de/"] {
            assert!(egress.allows(allowed), "{allowed}");
        }
        // An address written short, or with trailing dots, is read as a URL reads it.
        let cfg = rules(&[
            "8.8.8..",
            "8.8.8。",
            "127.1.",
            "0x7f.0.0.2..",
            "10.0.0.1%2e%2e",
        ])
        .unwrap();
        assert_eq!(
            cfg.egress.deny_hosts,
            ["8.8.0.8", "8.8.0.8", "127.0.0.1", "127.0.0.2", "10.0.0.1"]
        );
        let egress = policy(&cfg);
        for denied in [
            "https://8.8.8/",
            "https://8.8.0.8/",
            "http://127.1/",
            "http://10.0.0.1./",
        ] {
            assert!(!egress.allows(denied), "{denied}");
        }
        let allow = with(serde_json::json!({"egress": {"allow_hosts": ["*.bücher.de"]}})).unwrap();
        assert_eq!(allow.egress.allow_hosts, ["*.xn--bcher-kva.de"]);
        // A trailing full stop, in any of the forms IDNA maps to a dot, goes like a dot.
        for dot in [".", "。", "．", "｡", "%2e", "%2E", "..", ".。"] {
            let cfg = rules(&[&format!("*.bücher.de{dot}"), &format!("faß.de{dot}")]).unwrap();
            assert_eq!(
                cfg.egress.deny_hosts,
                ["*.xn--bcher-kva.de", "xn--fa-hia.de"],
                "{dot}"
            );
            let egress = policy(&cfg);
            for denied in ["https://shop.bücher.de/", "https://faß.de/"] {
                assert!(!egress.allows(denied), "{dot} {denied}");
            }
        }
        for bad in [
            "exa mple.com",
            "",
            "*.",
            "*",
            "a*b.example",
            "*.10.0.0.1",
            "::1",
            ".",
            "。",
            "%2e",
            "*.。",
            "..example",
            "*.8.8.8..",
            "*.8.8.8。",
            "*.[::1]",
        ] {
            let refused = rules(&[bad]).unwrap_err().to_string();
            assert!(
                refused.contains("egress.deny_hosts") && refused.contains(&format!("{bad:?}")),
                "{bad}: {refused}"
            );
        }
    }
    /// A rule written as any spelling of a host denies a URL whose host is written as any
    /// spelling of it (Unicode or punycode, any case, trailing dots and full stops, an address
    /// written short): rule and host are read by one function (`egress::canonical_host`).
    #[test]
    fn a_rule_denies_its_host_however_either_is_written() {
        let hosts: [&[&str]; 4] = [
            &[
                "faß.de",
                "FAß.DE",
                "xn--fa-hia.de",
                "XN--FA-HIA.DE",
                "faß.de.",
                "faß.de..",
                "faß.de。",
                "faß.de．",
                "faß.de｡",
                "faß.de%2e",
                "faß.de.。",
            ],
            &[
                "8.8.0.8",
                "8.8.8",
                "8.8.8.",
                "8.8.8..",
                "8.8.8。",
                "8.8.8%2e",
                "0x8.8.8",
                "8.8.0.8..",
            ],
            &[
                "127.0.0.1",
                "127.1",
                "0x7f.1",
                "0177.0.0.1",
                "127.1..",
                "127.1。",
            ],
            &[
                "Example.ORG",
                "example.org",
                "example.org.",
                "example.org..",
                "example.org。",
            ],
        ];
        for (n, spellings) in hosts.iter().enumerate() {
            for rule in *spellings {
                let cfg = with(serde_json::json!({"egress": {"deny_hosts": [rule]}}))
                    .unwrap_or_else(|e| panic!("{rule}: {e:#}"));
                let egress = policy(&cfg);
                for (m, others) in hosts.iter().enumerate() {
                    for host in *others {
                        let url = format!("http://{host}/");
                        assert!(url::Url::parse(&url).is_ok(), "{url}");
                        assert_eq!(egress.allows(&url), n != m, "rule {rule:?}, {url}");
                    }
                }
            }
        }
    }
    /// Every rule `host_rule` accepts is its own canonical form, whatever it was written as: a
    /// rule loaded twice is the same rule.
    #[test]
    fn a_canonical_host_rule_is_a_fixed_point() {
        let names = [
            "faß.de",
            "Bücher.DE",
            "example.org",
            "xn--fa-hia.de",
            "a.b.example",
            "8.8.8",
            "8.8.0.8",
            "10.0.0.1",
            "127.1",
            "0x7f.1",
            "0177.0.0.1",
            "1.2.3.4.5",
            "[::1]",
            "[2001:DB8::1]",
            "localhost",
        ];
        let suffixes = ["", ".", "..", "。", "．", "｡", "%2e", ".。", " ", ". "];
        let mut accepted = 0;
        for prefix in ["", "*.", " "] {
            for name in names {
                for suffix in suffixes {
                    let rule = format!("{prefix}{name}{suffix}");
                    let Some(canonical) = host_rule(&rule) else {
                        continue;
                    };
                    accepted += 1;
                    assert_eq!(
                        host_rule(&canonical).as_deref(),
                        Some(canonical.as_str()),
                        "{rule:?}"
                    );
                    // And it is written as a URL writes the host it names.
                    let host = canonical.strip_prefix("*.").unwrap_or(&canonical);
                    let url = url::Url::parse(&format!("https://{host}/")).unwrap();
                    assert_eq!(url.host_str(), Some(host), "{rule:?}");
                }
            }
        }
        assert!(accepted > 300, "{accepted}");
    }
    /// The facts cap has a floor: a recording's single-valued facts fit beside its ISRCs.
    #[test]
    fn the_facts_cap_is_at_least_eight() {
        let mut c = Config::default();
        c.also_on.facts_per_source = 7;
        assert!(c.validate().is_err());
        c.also_on.facts_per_source = 8;
        assert!(c.validate().is_ok());
    }
    /// An also-on source's own limits pace its hosts as an official source's do.
    #[test]
    fn also_on_source_budgets_pace_their_hosts() {
        let cfg = Config::default();
        for (host, rate) in crate::also_on::budgets() {
            assert_eq!(cfg.egress.source_rates.get(host), Some(&rate), "{host}");
        }
    }
    #[test]
    fn source_budgets_never_override_the_operator() {
        let budgets: Vec<_> = crate::sources::budgets().collect();
        let Some((host, rate)) = budgets.first().copied() else {
            return;
        };
        let cfg = Config::default();
        assert_eq!(cfg.egress.source_rates[host], rate);
        assert!(!cfg.egress.host_rates.contains_key(host));
        let set = with(serde_json::json!({"limits":{"host_rates":{host: 0.25}}})).unwrap();
        assert_eq!(set.egress.host_rates[host], 0.25);
        assert_eq!(set.egress.source_rates[host], rate);
    }
    #[test]
    fn the_contact_reaches_every_client() {
        let cfg = with(serde_json::json!({"http":{"contact":" info@synonym.to "}})).unwrap();
        assert_eq!(cfg.egress.contact, "info@synonym.to");
        assert_eq!(cfg.osm.contact, "info@synonym.to");
        assert_eq!(Config::default().egress.contact, "");
        for bad in [
            "nobody",
            "a b@example.org",
            "x@example.org)",
            "x@users.noreply.github.com",
            "GitHub@example.org",
        ] {
            assert!(
                with(serde_json::json!({"http":{"contact":bad}})).is_err(),
                "{bad}"
            );
        }
        assert!(with(serde_json::json!({"egress":{"contact":"x@example.org"}})).is_err());
    }
    #[test]
    fn limits_apply_once_to_every_resolver_and_caps_left_the_sections() {
        let cfg = with(serde_json::json!({"limits":{"resolver_concurrency":8},"egress":{"deny_hosts":["*.example.org"]}})).unwrap();
        for c in [&cfg.pubky, &cfg.cards.media, &cfg.cards.nostr] {
            assert_eq!(c.concurrency, 8);
            assert_eq!(c.ttl_secs, 86400);
        }
        // The web tier keeps its own limits.
        assert_eq!((cfg.web.concurrency, cfg.web.requests_per_second), (4, 4.0));
        let d = Config::default();
        for c in [&d.pubky, &d.cards.media, &d.cards.isbn, &d.cards.doi] {
            assert_eq!((c.concurrency, c.requests_per_second), (2, 2.0));
        }
        for moved in [
            serde_json::json!({"web":{"concurrency":8}}),
            serde_json::json!({"queue":{"backoff_secs":5}}),
            serde_json::json!({"event":{"months_forward":24}}),
            serde_json::json!({"web":{"typo":true}}),
            serde_json::json!({"limits":{"typo":1}}),
        ] {
            assert!(with(moved.clone()).is_err(), "{moved}");
        }
        assert!(
            with(serde_json::json!({"limits":{"resolver_requests_per_second":1e-320}})).is_err()
        );
    }
    #[test]
    fn the_example_config_loads_and_lists_only_operator_choices() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pinakes.example.toml");
        let config = Config::load(Some(&path)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let limits = serde_json::to_value(Limits::default()).unwrap();
        for key in limits.as_object().unwrap().keys() {
            assert!(
                !text.lines().any(|l| l.starts_with(&format!("{key} = "))),
                "{key} is a limit, not an operator choice"
            );
        }
        assert!(text.lines().any(|l| l == r#"cors_allow_origins = ["*"]"#));
        assert_eq!(config.api.cors_allow_origins, ["*"]);
        assert_eq!(
            config.event.utc_series_wall_clock,
            UtcSeriesWallClock::XWrTimezone
        );
        assert!(toml::from_str::<Config>("[event]\nutc_series_wall_clock = \"always\"\n").is_err());
        assert_eq!(config.event.same_event_hint, SameEventHint::On);
        let off: Config = toml::from_str("[event]\nsame_event_hint = \"off\"\n").unwrap();
        assert_eq!(off.event.same_event_hint, SameEventHint::Off);
        assert!(toml::from_str::<Config>("[event]\nsame_event_hint = true\n").is_err());
    }
    #[test]
    fn reach_and_ingest_limits_have_safe_defaults_and_config_bounds() {
        let config = Config::default();
        assert_eq!(config.search.reach_timeout_secs, 5);
        assert_eq!(config.api.reach_cost, 6);
        assert_eq!(config.ingest.max_label_bytes, 128);
        assert_eq!(config.ingest.max_object_path_bytes, 2048);
        for (key, value) in [
            ("reach_timeout_secs", 0),
            ("reach_timeout_secs", 61),
            ("reach_cost", 1),
            ("reach_cost", 101),
            ("max_label_bytes", 0),
            ("max_label_bytes", 1025),
            ("max_object_path_bytes", 0),
            ("max_object_path_bytes", 2049),
        ] {
            assert!(
                with(serde_json::json!({"limits":{key:value}})).is_err(),
                "{key}={value}"
            );
        }
    }
}

/// Defines `Limits`: each field, its documentation, the internal setting it reads its default from
/// and the settings it is applied to (several for the per-resolver values).
macro_rules! limits {
    ($( $(#[doc = $doc:literal])* $name:ident: $ty:ty = $from:expr => [$($to:expr),+] ),* $(,)?) => {
        /// The optional `[limits]` section: every cap, timeout, budget, backoff and refresh tier.
        /// Each value is applied to the internal setting(s) it names; defaults come from those
        /// settings' defaults, so the two cannot disagree.
        #[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
        #[serde(default, deny_unknown_fields)]
        pub struct Limits {
            $( $(#[doc = $doc])* pub $name: $ty, )*
        }
        impl Default for Limits {
            fn default() -> Self {
                Self::of(&Config::base())
            }
        }
        impl Limits {
            fn zeroed() -> Self {
                Self { $( $name: Default::default(), )* }
            }
            /// The limits `config` currently holds.
            pub fn of(config: &Config) -> Self {
                let c = config;
                Self { $( $name: { let f: fn(&Config) -> $ty = $from; f(c) }, )* }
            }
            /// Write these limits into `config`'s internal settings.
            pub fn apply(&self, config: &mut Config) {
                $( { let v = self.$name.clone(); $( { let f: fn(&mut Config, $ty) = $to; f(config, v.clone()); } )+ } )*
            }
        }
    };
}
limits! {
    /// Nexus feed poll interval in seconds.
    poll_secs: u64 = |c| c.poll_secs => [|c, v| c.poll_secs = v],
    /// Days an unreferenced resource is kept before orphan cleanup.
    grace_days: u32 = |c| c.grace_days => [|c, v| c.grace_days = v],
    /// Resource refresh intervals by tier (seconds).
    refresh_resource_secs: [u64; 4] = |c| c.refresh.resource_secs => [|c, v| c.refresh.resource_secs = v],
    /// Container (calendar, feed) refresh intervals by tier (seconds).
    refresh_container_secs: [u64; 4] = |c| c.refresh.container_secs => [|c, v| c.refresh.container_secs = v],
    /// Cursor feed refresh interval (seconds).
    refresh_cursor_secs: u64 = |c| c.refresh.cursor_secs => [|c, v| c.refresh.cursor_secs = v],
    /// Days change lines are retained.
    change_retention_days: u32 = |c| c.refresh.retention_days => [|c, v| c.refresh.retention_days = v],
    /// Adapter canary interval (seconds).
    canary_interval_secs: u64 = |c| c.canary.interval_secs => [|c, v| c.canary.interval_secs = v],
    /// Concurrent jobs of the web resolver, which the event routes share.
    web_concurrency: usize = |c| c.web.concurrency => [|c, v| c.web.concurrency = v],
    /// Jobs per second of the web resolver and the event routes.
    web_requests_per_second: f64 = |c| c.web.requests_per_second => [|c, v| c.web.requests_per_second = v],
    /// Concurrent jobs per pubky or card resolver.
    resolver_concurrency: usize = |c| c.pubky.concurrency => [
        |c, v| c.pubky.concurrency = v,
        |c, v| c.cards.media.concurrency = v, |c, v| c.cards.isbn.concurrency = v,
        |c, v| c.cards.doi.concurrency = v, |c, v| c.cards.nostr.concurrency = v,
        |c, v| c.cards.activitypub.concurrency = v, |c, v| c.cards.official.concurrency = v],
    /// Jobs per second per pubky or card resolver.
    resolver_requests_per_second: f64 = |c| c.pubky.requests_per_second => [
        |c, v| c.pubky.requests_per_second = v,
        |c, v| c.cards.media.requests_per_second = v, |c, v| c.cards.isbn.requests_per_second = v,
        |c, v| c.cards.doi.requests_per_second = v, |c, v| c.cards.nostr.requests_per_second = v,
        |c, v| c.cards.activitypub.requests_per_second = v,
        |c, v| c.cards.official.requests_per_second = v],
    /// Default document lifetime (seconds) for web, pubky and card resolvers.
    resolver_ttl_secs: u64 = |c| c.web.ttl_secs => [
        |c, v| c.web.ttl_secs = v, |c, v| c.pubky.ttl_secs = v,
        |c, v| c.cards.media.ttl_secs = v, |c, v| c.cards.isbn.ttl_secs = v,
        |c, v| c.cards.doi.ttl_secs = v, |c, v| c.cards.nostr.ttl_secs = v,
        |c, v| c.cards.activitypub.ttl_secs = v, |c, v| c.cards.official.ttl_secs = v],
    /// Requests per second to any one upstream host.
    host_requests_per_second: f64 = |c| c.egress.host_requests_per_second => [|c, v| c.egress.host_requests_per_second = v],
    /// Per-host overrides of `host_requests_per_second`.
    host_rates: BTreeMap<String, f64> = |c| c.egress.host_rates.clone() => [|c, v| c.egress.host_rates = v],
    /// How far an HTML page is read while its head is still open at the 1 MB page cap (bytes).
    html_head_bytes: usize = |c| c.egress.html_head_bytes => [|c, v| c.egress.html_head_bytes = v],
    /// Failures before a resource dead-letters.
    max_failures: u32 = |c| c.queue.max_failures => [|c, v| c.queue.max_failures = v],
    /// Timeouts tolerated before they count as failures.
    timeout_grace: u32 = |c| c.queue.timeout_grace => [|c, v| c.queue.timeout_grace = v],
    /// First retry backoff (seconds).
    backoff_secs: u64 = |c| c.queue.backoff_secs => [|c, v| c.queue.backoff_secs = v],
    /// Retry backoff ceiling (seconds).
    max_backoff_secs: u64 = |c| c.queue.max_backoff_secs => [|c, v| c.queue.max_backoff_secs = v],
    /// Job lease (seconds).
    lease_secs: u64 = |c| c.queue.lease_secs => [|c, v| c.queue.lease_secs = v],
    /// Re-check interval for missing resources (seconds).
    missing_ttl_secs: u64 = |c| c.queue.missing_ttl_secs => [|c, v| c.queue.missing_ttl_secs = v],
    /// Resolution budget per author per second.
    author_requests_per_second: f64 = |c| c.queue.author_requests_per_second => [|c, v| c.queue.author_requests_per_second = v],
    /// Longest accepted tag label (bytes).
    max_label_bytes: usize = |c| c.ingest.max_label_bytes => [|c, v| c.ingest.max_label_bytes = v],
    /// Longest accepted Pubky object path (bytes).
    max_object_path_bytes: usize = |c| c.ingest.max_object_path_bytes => [|c, v| c.ingest.max_object_path_bytes = v],
    /// OSM place cache lifetime (seconds).
    osm_ttl_secs: u64 = |c| c.osm.ttl_secs => [|c, v| c.osm.ttl_secs = v],
    /// Hours between two writes of the ODbL dump file.
    osm_dump_every_hours: u64 = |c| c.osm.dump_every_hours => [|c, v| c.osm.dump_every_hours = v],
    /// ODbL dump downloads per hour per client address.
    osm_dump_downloads_per_hour: usize = |c| c.osm.dump_downloads_per_hour => [|c, v| c.osm.dump_downloads_per_hour = v],
    /// Nexus reach deadline per search (seconds).
    reach_timeout_secs: u64 = |c| c.search.reach_timeout_secs => [|c, v| c.search.reach_timeout_secs = v],
    /// Taggers a related page samples: the authors of a resource's most recent tags, at most this
    /// many.
    related_taggers: usize = |c| c.search.related_taggers => [|c, v| c.search.related_taggers = v],
    /// Days a related page looks back, for those taggers and their tags.
    related_window_days: u32 = |c| c.search.related_window_days => [|c, v| c.search.related_window_days = v],
    /// References a related page reads per sampled tagger (their most recent).
    related_refs_per_tagger: usize = |c| c.search.related_refs_per_tagger => [|c, v| c.search.related_refs_per_tagger = v],
    /// Shared taggers a related row needs (its group's, when collapsed).
    related_min_shared: usize = |c| c.search.related_min_shared => [|c, v| c.search.related_min_shared = v],
    /// Tags a related page reads per seed link (its most recent), from which the taggers are sampled.
    related_tag_sample: usize = |c| c.search.related_tag_sample => [|c, v| c.search.related_tag_sample = v],
    /// Seed links a related page samples: of the resource, its group's members and their
    /// aliases, the ones with the newest tags, at most this many.
    related_seed_links: usize = |c| c.search.related_seed_links => [|c, v| c.search.related_seed_links = v],
    /// References counted per group member when `collapse=group` picks a group's row.
    collapse_refs_per_member: usize = |c| c.search.collapse_refs_per_member => [|c, v| c.search.collapse_refs_per_member = v],
    /// Largest `near` radius (metres).
    max_radius_m: f64 = |c| c.search.max_radius_m => [|c, v| c.search.max_radius_m = v],
    /// H3 resolution per map zoom level.
    h3_resolutions: Vec<u8> = |c| c.search.h3_resolutions.clone() => [|c, v| c.search.h3_resolutions = v],
    /// Most cells one cluster query returns.
    max_cluster_cells: usize = |c| c.search.max_cluster_cells => [|c, v| c.search.max_cluster_cells = v],
    /// API response cache entries.
    api_cache_capacity: usize = |c| c.api.cache_capacity => [|c, v| c.api.cache_capacity = v],
    /// Budget units a search costs for each of `reach` and `boost`.
    reach_cost: usize = |c| c.api.reach_cost => [|c, v| c.api.reach_cost = v],
    /// Budget units a related search costs.
    related_cost: usize = |c| c.api.related_cost => [|c, v| c.api.related_cost = v],
    /// Preview requests per minute per client.
    preview_requests_per_minute: usize = |c| c.preview.requests_per_minute => [|c, v| c.preview.requests_per_minute = v],
    /// Preview requests per minute per URI.
    preview_uri_requests_per_minute: usize = |c| c.preview.uri_requests_per_minute => [|c, v| c.preview.uri_requests_per_minute = v],
    /// Preview cache entries.
    preview_cache_capacity: usize = |c| c.preview.cache_capacity => [|c, v| c.preview.cache_capacity = v],
    /// Podcast feed items read per scan.
    podcast_items: usize = |c| c.cards.podcast_items => [|c, v| c.cards.podcast_items = v],
    /// Most members of one automatic group.
    group_max_members: usize = |c| c.groups.max_members => [|c, v| c.groups.max_members = v],
    /// Farthest two located members of one group (metres).
    group_max_distance_m: f64 = |c| c.groups.max_distance_m => [|c, v| c.groups.max_distance_m = v],
    /// Units one grouping pass may read.
    group_max_graph_units: usize = |c| c.groups.max_graph_units => [|c, v| c.groups.max_graph_units = v],
    /// Bytes one grouping pass may read.
    group_max_graph_bytes: usize = |c| c.groups.max_graph_bytes => [|c, v| c.groups.max_graph_bytes = v],
    /// Derived links one source keeps per resource.
    also_on_links_per_source: usize = |c| c.also_on.links_per_source => [|c, v| c.also_on.links_per_source = v],
    /// Join keys one source keeps per resource.
    also_on_keys_per_source: usize = |c| c.also_on.keys_per_source => [|c, v| c.also_on.keys_per_source = v],
    /// Facts one source keeps per resource.
    also_on_facts_per_source: usize = |c| c.also_on.facts_per_source => [|c, v| c.also_on.facts_per_source = v],
    /// Resources whose sources run at once.
    also_on_concurrency: usize = |c| c.also_on.concurrency => [|c, v| c.also_on.concurrency = v],
    /// First retry of a failed source run (seconds); doubles up to an hour.
    also_on_retry_secs: u64 = |c| c.also_on.retry_secs => [|c, v| c.also_on.retry_secs = v],
    /// Longest calendar content line (bytes).
    event_max_line_bytes: usize = |c| c.event.max_line_bytes => [|c, v| c.event.max_line_bytes = v],
    /// Most date values in one calendar property.
    event_max_date_values: usize = |c| c.event.max_date_values => [|c, v| c.event.max_date_values = v],
    /// Largest calendar or feed body (bytes).
    event_feed_body_bytes: usize = |c| c.event.feed_body_bytes => [|c, v| c.event.feed_body_bytes = v],
    /// Most occurrences one series expands to.
    event_max_occurrences_per_series: usize = |c| c.event.max_occurrences_per_series => [|c, v| c.event.max_occurrences_per_series = v],
    /// Most occurrences one feed yields.
    event_max_occurrences_per_feed: usize = |c| c.event.max_occurrences_per_feed => [|c, v| c.event.max_occurrences_per_feed = v],
    /// Most components one calendar may hold.
    event_max_components: usize = |c| c.event.max_components => [|c, v| c.event.max_components = v],
    /// Most revisions of one event (RECURRENCE-ID overrides).
    event_max_revisions: usize = |c| c.event.max_event_revisions => [|c, v| c.event.max_event_revisions = v],
    /// Largest retained event document set per feed (bytes).
    event_max_document_bytes: usize = |c| c.event.max_event_document_bytes => [|c, v| c.event.max_event_document_bytes = v],
    /// Most recurrence steps one series may take.
    event_max_recurrence_steps: usize = |c| c.event.max_recurrence_steps => [|c, v| c.event.max_recurrence_steps = v],
    /// Most dates one period of a recurrence rule may hold; a larger rule is not expanded.
    event_max_period_dates: usize = |c| c.event.max_period_dates => [|c, v| c.event.max_period_dates = v],
    /// Calendar parse deadline (milliseconds).
    event_parse_timeout_ms: u64 = |c| c.event.parse_timeout_ms => [|c, v| c.event.parse_timeout_ms = v],
    /// Months of past occurrences kept in the horizon.
    event_months_back: u32 = |c| c.event.months_back => [|c, v| c.event.months_back = v],
    /// Months of future occurrences kept in the horizon.
    event_months_forward: u32 = |c| c.event.months_forward => [|c, v| c.event.months_forward = v],
    /// Lowest accepted geocoding score.
    geocode_score: f64 = |c| c.event.geocode_score => [|c, v| c.event.geocode_score = v],
    /// Retry interval of a held feed snapshot (seconds).
    held_retry_secs: u64 = |c| c.event.held_retry_secs => [|c, v| c.event.held_retry_secs = v],
    /// The most events that may name one event in the same-event hint (each keeps its best twenty).
    same_event_incoming_kept: i64 = |c| c.event.hint_incoming_kept => [|c, v| c.event.hint_incoming_kept = v],
    /// Events the same-event sweep drops per transaction.
    same_event_drop_batch: i64 = |c| c.event.hint_drop_batch => [|c, v| c.event.hint_drop_batch = v],
    /// Occurrence changes per snapshot before they become one summary line.
    feed_max_changes: usize = |c| c.event.container.max_changes => [|c, v| c.event.container.max_changes = v],
    /// Most stored rows of one feed.
    feed_max_rows: usize = |c| c.event.container.max_rows => [|c, v| c.event.container.max_rows = v],
    /// New keys one snapshot may add before it is held.
    feed_max_new_keys: usize = |c| c.event.container.max_new_keys => [|c, v| c.event.container.max_new_keys = v],
    /// Days cancelled and past rows are kept.
    feed_retention_days: u32 = |c| c.event.container.retention_days => [|c, v| c.event.container.retention_days = v],
    /// Consecutive holds before a feed dead-letters.
    feed_max_holds: u32 = |c| c.event.container.max_holds => [|c, v| c.event.container.max_holds = v],
    /// Shortest feed refresh interval (seconds).
    feed_cadence_floor: u64 = |c| c.event.container.cadence_floor => [|c, v| c.event.container.cadence_floor = v],
    /// Nostr relay websocket deadline (milliseconds).
    websocket_deadline_ms: u64 = |c| c.event.adapters.websocket_deadline_ms => [|c, v| c.event.adapters.websocket_deadline_ms = v],
    /// Pages one cursor feed fetch may read.
    max_cursor_pages: usize = |c| c.event.adapters.max_cursor_pages => [|c, v| c.event.adapters.max_cursor_pages = v],
    /// Pages one Nostr indexer lookup may read.
    max_indexer_pages: usize = |c| c.event.adapters.max_indexer_pages => [|c, v| c.event.adapters.max_indexer_pages = v],
    /// Derived links one adapter may hold across all parents.
    max_derived_resources: usize = |c| c.event.adapters.max_derived_resources => [|c, v| c.event.adapters.max_derived_resources = v],
    /// Members one NIP-52 calendar may list.
    max_calendar_members: usize = |c| c.event.adapters.max_calendar_members => [|c, v| c.event.adapters.max_calendar_members = v],
    /// Hours a NIP-52 calendar waits for a member that has never resolved.
    calendar_member_wait_hours: u64 = |c| c.event.adapters.calendar_member_wait_hours => [|c, v| c.event.adapters.calendar_member_wait_hours = v],
    /// Website links one BTC Map feed may derive.
    max_linked_resources: usize = |c| c.event.adapters.max_linked_resources => [|c, v| c.event.adapters.max_linked_resources = v],
}
