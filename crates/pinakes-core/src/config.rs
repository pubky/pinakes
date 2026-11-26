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
    /// Host names the admin listener answers besides `localhost` and IP addresses, such as the
    /// name a Prometheus in another container scrapes it by. A request for any other `Host` gets
    /// 421, so a page that rebinds its own name to this machine reaches nothing.
    pub admin_hosts: Vec<String>,
    pub nexus_url: String,
    /// The `Authorization` header sent, whole, on every request to the Nexus at `nexus_url`
    /// (`PINAKES_NEXUS_AUTHORIZATION`), for a Nexus behind credentials; `nexus_url` itself never
    /// carries any. Empty is none. `Debug` and serialization show `[REDACTED]`.
    pub nexus_authorization: Option<SecretKey>,
    /// Whether the Nexus at `nexus_url` serves the reach-set route, so search takes
    /// `reach=wot_2` and `boost=wot_2` (`nexus::Nexus::reach`); off, those answer 501.
    pub nexus_reach_set: bool,
    /// `discover_feeds` is accepted here and in `[cards.media]`, `music_enrichment` here alone;
    /// the other resolver sections take only `enabled`.
    #[serde(with = "web_section")]
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
    /// `[catalogues]`: the catalogue imports' switches, work directory and per-catalogue keys
    /// (`catalogue::CataloguesConfig`).
    pub catalogues: crate::catalogue::CataloguesConfig,
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
    /// Hours a resource nothing ever referenced (one only previewed) is kept, at most `grace_days`.
    #[serde(skip)]
    pub preview_grace_hours: u32,
    /// New previews (`POST /v1/resolve` of a link the instance did not know) per hour, for the
    /// whole instance.
    #[serde(skip)]
    pub preview_new_per_hour: usize,
    /// The share of those one client address may take in an hour; 0 is no share. The client
    /// address is the connecting one, or behind a proxy in `api.trusted_proxies` the one it
    /// forwards (`api::client_address`).
    #[serde(skip)]
    pub preview_new_per_client_per_hour: usize,
    pub search: SearchConfig,
    #[serde(skip)]
    pub queue: QueueConfig,
    #[serde(skip)]
    pub preview: PreviewConfig,
    /// Seconds a stopping daemon gives open requests, running jobs and background passes to
    /// finish.
    #[serde(skip)]
    pub shutdown_grace_secs: u64,
    /// The [`RETIRED`] keys the file or a `PINAKES__…` override still set, which this build
    /// ignores; the daemon warns once per key at startup.
    #[serde(skip)]
    pub ignored: Vec<&'static str>,
}
/// Keys this build no longer reads. A configuration that still sets one loads, without it, and
/// names it in `Config::ignored`.
pub const RETIRED: &[&str] = &["groups.cross_container_occurrences"];
impl Default for Config {
    fn default() -> Self {
        let mut config = Self::base();
        config.limits = Limits::of(&config);
        config.queue.catalogues = crate::catalogue::join::joinable(&config.catalogues);
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
                // A value of the wrong type is named by its section and what was expected,
                // never by the value: the deserializer's message quotes it.
                let shown = (section.shown)(value).map_err(|error| {
                    match error.downcast_ref::<serde_json::Error>() {
                        Some(shape) => {
                            anyhow::anyhow!("[{name}]: {}", without_value(&shape.to_string()))
                        }
                        None => error.context(format!("[{name}]")),
                    }
                })?;
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
        // The variable, never its value: it may be a secret.
        apply_override(value, &key, val)
            .with_context(|| format!("environment variable PINAKES__{}", key.to_uppercase()))?;
    }
    Ok(())
}
/// One `PINAKES__SECTION__KEY` override (`key` is `section__key`, in lower case).
fn apply_override(value: &mut serde_json::Value, key: &str, val: String) -> Result<()> {
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
        return Ok(());
    }
    if node.get(last).is_none() && RETIRED.contains(&parts.join(".").as_str()) {
        // Set as written, so `Config::from_value` takes it out and names it as ignored.
        node.as_object_mut()
            .context("unknown config section")?
            .insert(last.to_string(), serde_json::Value::String(val));
        return Ok(());
    }
    let target = node.get_mut(last).context("unknown config key")?;
    *target = if target.is_string() || (target.is_null() && val != "null") {
        serde_json::Value::String(val)
    } else {
        serde_json::from_str(&val).context("env override requires JSON value")?
    };
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
            admin_hosts: Vec::new(),
            refresh: RefreshConfig::default(),
            canary: CanaryConfig::default(),
            also_on: Default::default(),
            podcast_index: Default::default(),
            catalogues: Default::default(),
            sections: Sections::defaults(),
            nexus_url: "http://127.0.0.1:8080".into(),
            nexus_authorization: None,
            nexus_reach_set: false,
            poll_secs: 5,
            ingest: IngestConfig::default(),
            grace_days: 7,
            preview_grace_hours: 24,
            preview_new_per_hour: 1000,
            preview_new_per_client_per_hour: 100,
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
            shutdown_grace_secs: 30,
            ignored: Vec::new(),
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
    pub max_properties: usize,
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
            feed_body_bytes: 8 * 1024 * 1024,
            max_occurrences_per_series: 50,
            max_occurrences_per_feed: 10000,
            max_components: 100000,
            max_properties: 1_000_000,
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
#[derive(Clone, Debug)]
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
/// `[cards.media]` in the file: `enabled` and `discover_feeds` onto the shared resolver
/// settings.
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
/// `[web]` in the file: `enabled`, `discover_feeds` and `music_enrichment`.
pub(crate) mod web_section {
    use super::ResolverConfig;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    #[derive(Deserialize, Serialize)]
    #[serde(default, deny_unknown_fields)]
    struct Web {
        enabled: bool,
        discover_feeds: bool,
        music_enrichment: bool,
    }
    impl Default for Web {
        fn default() -> Self {
            let d = ResolverConfig::default();
            Self {
                enabled: d.enabled,
                discover_feeds: d.discover_feeds,
                music_enrichment: d.music_enrichment,
            }
        }
    }
    pub fn serialize<S: Serializer>(c: &ResolverConfig, s: S) -> Result<S::Ok, S::Error> {
        Web {
            enabled: c.enabled,
            discover_feeds: c.discover_feeds,
            music_enrichment: c.music_enrichment,
        }
        .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ResolverConfig, D::Error> {
        let w = Web::deserialize(d)?;
        Ok(ResolverConfig {
            enabled: w.enabled,
            discover_feeds: w.discover_feeds,
            music_enrichment: w.music_enrichment,
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
    /// `pubky` only: how long one PKARR relay may take to answer
    /// (`limits.pkarr_relay_timeout_ms`).
    #[serde(skip)]
    pub pkarr_relay_timeout_ms: u64,
    /// `web.music_enrichment` only: a MusicBrainz page's card takes an image and a description,
    /// with their credits, from its Wikidata item (`music_enrichment`). Off, the card is
    /// MusicBrainz's alone.
    #[serde(skip)]
    pub music_enrichment: bool,
    /// `web` only: how long a card's enrichment may take (`limits.music_enrichment_timeout_ms`).
    #[serde(skip)]
    pub music_enrichment_timeout_ms: u64,
    /// `web` only: when a card whose enrichment failed for a while is read again
    /// (`limits.music_enrichment_retry_secs`).
    #[serde(skip)]
    pub music_enrichment_retry_secs: u64,
    /// `web` only: the width of an enriched image (`limits.music_image_width`).
    #[serde(skip)]
    pub music_image_width: u32,
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
            pkarr_relay_timeout_ms: 4000,
            music_enrichment: true,
            music_enrichment_timeout_ms: 8000,
            music_enrichment_retry_secs: 3600,
            music_image_width: 640,
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
    /// The API hosts of the also-on sources that are off (`also_on::off_hosts`), refused as
    /// `deny_hosts` are: switching a source off stops every call to its API.
    #[serde(skip)]
    pub off_hosts: Vec<String>,
    /// The body cap of a MusicBrainz or Wikidata answer (`hubs`): a much-edited item or an
    /// artist with very many relations is megabytes, above the default cap.
    #[serde(skip)]
    pub hub_body_bytes: usize,
    /// How long an answer the also-on sources, lookups and hubs ask for is shared
    /// (`Egress::discover`), in seconds: two sources asking the same thing ask once.
    #[serde(skip)]
    pub discovery_secs: u64,
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
            off_hosts: vec![],
            hub_body_bytes: crate::hubs::DEFAULT_BODY_CAP,
            discovery_secs: 3600,
        }
    }
}
#[derive(Clone, Debug)]
pub struct QueueConfig {
    pub groups: crate::groups::GroupConfig,
    /// The catalogues a link is joined with before it is fetched (`catalogue::join::joinable`);
    /// none when every catalogue is off.
    pub catalogues: Vec<&'static str>,
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
            catalogues: vec![],
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
#[derive(Clone, Debug)]
pub struct PreviewConfig {
    pub requests_per_minute: usize,
    pub uri_requests_per_minute: usize,
    /// The same table's size when a preview adds its client address and link: past it the
    /// oldest entry (an API client's too) is forgotten, with what it had used.
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
    /// The Nominatim base. A hosted Nominatim's may carry its key, so `Debug` and serialization
    /// show `[REDACTED]`; set such a base in the environment file (`PINAKES__OSM__NOMINATIM_URL`).
    pub nominatim_url: SecretKey,
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
    /// The text index's directory (`search_index`): derived data, rebuilt from the database
    /// when it is missing or unreadable.
    pub index_dir: String,
    /// The best units a text search reads from the index before Postgres loads and filters them.
    #[serde(skip)]
    pub text_candidates: usize,
    /// Seconds between two passes that commit queued changes to the text index.
    #[serde(skip)]
    pub index_commit_secs: u64,
    /// Text searches the index runs at once (0: one per CPU core); one more waits for a turn.
    #[serde(skip)]
    pub index_searches: usize,
    /// The most a text search's index stage takes, its wait for a turn included (milliseconds):
    /// past it the search answers 503 and its work stops.
    #[serde(skip)]
    pub index_budget_ms: u64,
    #[serde(skip)]
    pub reach_timeout_secs: u64,
    /// The taggers a related page samples: the authors of a resource's most recent tags, at most
    /// this many.
    #[serde(skip)]
    pub related_taggers: usize,
    /// The days a related page looks back, for those taggers and their tags.
    #[serde(skip)]
    pub related_window_days: u32,
    /// The references a related page reads per sampled tagger (their most recent).
    #[serde(skip)]
    pub related_refs_per_tagger: usize,
    /// The shared taggers a related row needs (its group's, when collapsed).
    #[serde(skip)]
    pub related_min_shared: usize,
    /// The tags a related page reads per seed link (its most recent), from which the taggers
    /// are sampled.
    #[serde(skip)]
    pub related_tag_sample: usize,
    /// The seed links a related page samples: of the resource, its group's members and their
    /// aliases, the ones with the newest tags, at most this many.
    #[serde(skip)]
    pub related_seed_links: usize,
    /// The references counted per group member when `collapse=group` picks a group's row.
    #[serde(skip)]
    pub collapse_refs_per_member: usize,
    #[serde(skip)]
    pub max_radius_m: f64,
}
impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            index_dir: "search-index".into(),
            text_candidates: 500,
            index_commit_secs: 2,
            index_searches: 0,
            index_budget_ms: 2000,
            reach_timeout_secs: 5,
            related_taggers: 200,
            related_window_days: 365,
            related_refs_per_tagger: 1000,
            related_min_shared: 2,
            related_tag_sample: 2000,
            related_seed_links: 32,
            collapse_refs_per_member: 10_000,
            max_radius_m: 200_000.0,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    pub requests_per_minute: usize,
    /// The entries the rate limiter's table holds when an API request adds a client address
    /// (the preview limiter shares the table), and the addresses the dump download limiter
    /// remembers per hour: past it the oldest is forgotten, with what it had used.
    #[serde(skip)]
    pub cache_capacity: usize,
    /// Seconds a request may take before it is answered 504, on either listener. At least 8:
    /// an MCP tool call has two seconds less (`tool_timeout`), and `card` with `resolve: true`
    /// waits three seconds for a new link's card between its other reads.
    #[serde(skip)]
    pub request_timeout_secs: u64,
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
    /// The reverse proxies in front of the public listener: addresses (`10.0.0.2`) or networks
    /// (`10.0.0.0/8`, `fd00::/8`). A request from one is counted under the right-most
    /// `X-Forwarded-For` entry that is not itself one of them (`api::client_address`). Empty,
    /// the default: the connecting address is the client, and the header is never read.
    pub trusted_proxies: Vec<String>,
}
/// The seconds an MCP tool call's deadline lies inside the request's own.
const TOOL_MARGIN_SECS: u64 = 2;
impl ApiConfig {
    /// `trusted_proxies`, parsed: an address is a network of that one address.
    pub fn trusted_proxy_nets(&self) -> Result<Vec<ipnet::IpNet>> {
        self.trusted_proxies
            .iter()
            .map(|entry| {
                entry
                    .parse::<ipnet::IpNet>()
                    .map(|net| net.trunc())
                    .or_else(|_| entry.parse::<std::net::IpAddr>().map(ipnet::IpNet::from))
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "api.trusted_proxies: {entry:?} is not an IP address or network (10.0.0.2, 10.0.0.0/8)"
                        )
                    })
            })
            .collect()
    }
    /// How long one MCP tool call may take, all its requests together, from the start of the
    /// request that carries it: `request_timeout_secs` less two seconds, so the tool's own
    /// failure is written before the request's deadline answers 504 in its place.
    pub fn tool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.request_timeout_secs
                .saturating_sub(TOOL_MARGIN_SECS)
                .max(1),
        )
    }
}
impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 120,
            cache_capacity: 10000,
            request_timeout_secs: 30,
            reach_cost: 6,
            related_cost: 6,
            cors_allow_origins: vec!["*".into()],
            swagger_ui: true,
            mcp: true,
            trusted_proxies: vec![],
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
#[derive(Clone, Debug)]
pub struct RefreshConfig {
    pub resource_secs: [u64; 4],
    pub container_secs: [u64; 4],
    pub cursor_secs: u64,
    pub retention_days: u32,
    /// The longest wait between two reads of an undated card that keeps answering the same, and
    /// the wait of a link found gone (`scheduler::Backoff`); a first `resource_secs` tier above it
    /// is the longest instead.
    pub max_secs: u64,
}
impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            resource_secs: [7 * 86400, 2 * 86400, 6 * 3600, 3600],
            container_secs: [86400, 12 * 3600, 6 * 3600, 3600],
            cursor_secs: 300,
            retention_days: 90,
            max_secs: 90 * 86400,
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
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let file: toml::Value =
                toml::from_str(&text).map_err(|e| syntax_error(path, &text, &e))?;
            let file = serde_json::to_value(file)?;
            merge(&mut value, file);
        }
        apply_env(&mut value, std::env::vars())?;
        apply_named_env(&mut value, std::env::vars())?;
        Self::from_value(value)
    }
    /// The defaults in their file form. Secrets serialize as `[REDACTED]`, so their real defaults
    /// are put back here.
    pub(crate) fn defaults() -> Result<serde_json::Value> {
        let config = Self::default();
        let mut value = serde_json::to_value(&config)?;
        value["database_url"] = (*config.database_url).into();
        value["admin_token"] = config.admin_token.as_deref().into();
        value["osm"]["nominatim_url"] = (*config.osm.nominatim_url).into();
        Ok(value)
    }
    /// A configuration from its file form (sections plus `[limits]`), validated.
    pub fn from_value(mut value: serde_json::Value) -> Result<Self> {
        let mut ignored = Vec::new();
        for key in RETIRED {
            let (section, name) = key
                .split_once('.')
                .expect("a retired key names its section");
            if let Some(section) = value.get_mut(section).and_then(|v| v.as_object_mut()) {
                if section.remove(name).is_some() {
                    ignored.push(*key);
                }
            }
        }
        let mut cfg: Self = match serde_json::from_value(value.clone()) {
            Ok(cfg) => cfg,
            Err(error) => return Err(shape_error(&value, &error)),
        };
        cfg.ignored = ignored;
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
        // A rate is looked up by the host as the egress names it: lower case, an IDN in
        // punycode. A key that names no single host (a pattern, a URL) would pace nothing.
        let mut rates = BTreeMap::new();
        for (key, rate) in &cfg.limits.host_rates {
            // A key that is a URL is not repeated: it may carry credentials.
            let shown = if key.contains("://") || key.contains('@') {
                "a key that is a URL".to_owned()
            } else {
                format!("{key:?}")
            };
            let host = host_rule(key).filter(|host| !host.starts_with("*.")).with_context(|| {
                format!("limits.host_rates: {shown} is not a host name (one host, no `*.`, no scheme or path)")
            })?;
            anyhow::ensure!(
                rates.insert(host, *rate).is_none(),
                "limits.host_rates: {shown} names a host another key names too"
            );
        }
        cfg.limits.host_rates = rates;
        let limits = cfg.limits.clone();
        limits.apply(&mut cfg);
        cfg.queue.groups = cfg.groups.clone();
        cfg.queue.catalogues = crate::catalogue::join::joinable(&cfg.catalogues);
        cfg.share_contact();
        cfg.validate()?;
        Ok(cfg)
    }
    /// Whether `nexus_authorization` would cross a network in clear: it is set, and `nexus_url`
    /// is `http://` on a host that is neither this machine nor a private address, by its name
    /// or address as written (a name that resolves to a private address is not looked up).
    pub fn nexus_authorization_in_clear(&self) -> bool {
        let Ok(url) = url::Url::parse(&self.nexus_url) else {
            return false;
        };
        let local = match url.host() {
            Some(url::Host::Domain(name)) => name == "localhost" || name.ends_with(".localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            Some(url::Host::Ipv6(ip)) => {
                ip.is_loopback() || ip.is_unique_local() || (ip.segments()[0] & 0xffc0) == 0xfe80
            }
            None => false,
        };
        self.nexus_authorization.is_some() && url.scheme() == "http" && !local
    }
    /// Every HTTP client sends `http.contact` (`resolver::egress::user_agent`).
    fn share_contact(&mut self) {
        self.egress.contact = self.http.contact.trim().to_owned();
        self.osm.contact = self.egress.contact.clone();
        self.web.musicbrainz_base = self.also_on.musicbrainz_base().to_owned();
        // A source's published limits, for hosts the operator does not rate.
        self.egress.source_rates = crate::sources::budgets(self)
            .chain(crate::also_on::budgets())
            .map(|(host, rate)| (host.to_owned(), rate))
            .collect();
        self.egress.off_hosts = crate::also_on::off_hosts(self);
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
        self.catalogues.validate()?;
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
        self.api.trusted_proxy_nets()?;
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
        if let Some(host) = self.admin_hosts.iter().find(|h| {
            h.is_empty()
                || h.len() > 253
                || !h.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                })
        }) {
            bail!("admin_hosts: {host:?} is not a lowercase host name (no scheme, port, path or wildcard)");
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
            || !(120..=86400).contains(&a.refusal_hold_secs)
            || !(1024 * 1024..=16 * 1024 * 1024).contains(&a.watch_page_bytes)
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
        if !(1024 * 1024..=64 * 1024 * 1024).contains(&self.egress.hub_body_bytes) {
            bail!("limits.hub_body_bytes must be 1–64 MiB");
        }
        // The cache keeps an answer at least an hour: a shorter lifetime would not be kept.
        if !(crate::resolver::egress::MIN_ANSWER_SECS..=86400).contains(&self.egress.discovery_secs)
        {
            bail!(
                "limits.discovery_cache_secs must be {}–86400",
                crate::resolver::egress::MIN_ANSWER_SECS
            );
        }
        if !(100..=7000).contains(&self.pubky.pkarr_relay_timeout_ms) {
            bail!("limits.pkarr_relay_timeout_ms must be 100–7000");
        }
        let w = &self.web;
        if !(100..=13000).contains(&w.music_enrichment_timeout_ms)
            || !(60..=604800).contains(&w.music_enrichment_retry_secs)
            || !(20..=4096).contains(&w.music_image_width)
        {
            bail!("invalid music enrichment limits");
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
            || !(1..=10_000_000).contains(&e.max_properties)
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
            || !(3600..=31536000).contains(&self.refresh.max_secs)
            || !(1..=3650).contains(&self.refresh.retention_days)
            || !(3600..=604800).contains(&self.canary.interval_secs)
        {
            bail!("invalid refresh, canary or admin configuration");
        }
        // Change lines hold a card's values for `limits.change_retention_days`: no longer than a
        // source whose values they may hold allows them kept (TMDB: six months, from a read up to
        // 60 days old), while it is ready.
        for source in crate::also_on::registered() {
            if let Some(cap) = source.retention_days_cap() {
                if source.ready(self) && self.refresh.retention_days > cap {
                    let named: Vec<String> = source
                        .section()
                        .map(|s| s.variables.iter().map(|(v, _)| (*v).to_owned()).collect())
                        .unwrap_or_default();
                    bail!(
                        "limits.change_retention_days is {}, over the {cap} days change lines may keep the `{}` source's values: lower it, or switch `{}` off ({}[{}] enabled = false)",
                        self.refresh.retention_days,
                        source.id(),
                        source.id(),
                        named.iter().map(|v| format!("unset {v}, or ")).collect::<String>(),
                        source.id()
                    );
                }
            }
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
            || self.search.index_dir.trim().is_empty()
            || !(1..=168).contains(&self.osm.dump_every_hours)
            || !(1..=1000).contains(&self.osm.dump_downloads_per_hour)
            || !(1..=10000).contains(&self.api.requests_per_minute)
            || !(100..=100000).contains(&self.api.cache_capacity)
            || !(8..=600).contains(&self.api.request_timeout_secs)
            || !(1..=600).contains(&self.shutdown_grace_secs)
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
            || !(50..=10_000).contains(&self.search.text_candidates)
            || !(1..=60).contains(&self.search.index_commit_secs)
        {
            bail!("invalid OSM or spatial budget (public Nominatim maximum 1 request/s)");
        }
        if self.search.index_searches > 256 {
            bail!("limits.search_index_searches must be 0 (one per CPU core) to 256");
        }
        if !(100..=30_000).contains(&self.search.index_budget_ms) {
            bail!("limits.search_index_budget_ms must be 100 to 30000");
        }
        // A search's reach reads share this one deadline (`Nexus::reach_by`). Reads that
        // outlast the request are a 504 where they would be the reach's own answer, and
        // through MCP the tool call's deadline comes first.
        if self.search.reach_timeout_secs >= self.api.tool_timeout().as_secs() {
            bail!(
                "limits.reach_timeout_secs must be more than {TOOL_MARGIN_SECS} seconds below limits.request_timeout_secs"
            );
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
            || !(100..=100000).contains(&self.preview.cache_capacity)
            || !(1..=87600).contains(&self.preview_grace_hours)
            || !(1..=1_000_000).contains(&self.preview_new_per_hour)
            || self.preview_new_per_client_per_hour > 1_000_000
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
/// A config file that is not TOML: where, and what the parser expected. Never the parser's own
/// text, which quotes the line, and the line may hold a secret.
fn syntax_error(path: &Path, text: &str, error: &toml::de::Error) -> anyhow::Error {
    let place = error.span().map(|span| {
        // By bytes: a span's start is no promise of a character boundary.
        let before = &text.as_bytes()[..span.start.min(text.len())];
        let line = before.iter().filter(|b| **b == b'\n').count() + 1;
        let column = before.rsplit(|b| *b == b'\n').next().map_or(0, <[u8]>::len) + 1;
        format!(" at line {line}, byte {column}")
    });
    anyhow::anyhow!(
        "{} is not valid TOML{}: {}",
        path.display(),
        place.unwrap_or_default(),
        error.message()
    )
}
/// A deserializer's message without the value it found: "invalid type: integer `5`, expected a
/// string" keeps "invalid type, expected a string". The value may be a secret; what was
/// expected (the last `, expected`, since a quoted value may hold those words) is not.
fn without_value(message: &str) -> String {
    let kinds = ["invalid type: ", "invalid value: ", "unknown variant "];
    let Some(kind) = kinds.iter().find(|kind| message.starts_with(**kind)) else {
        return message.to_owned();
    };
    let kind = kind.trim_end_matches([':', ' ']);
    match message.rfind(", expected ") {
        Some(expected) => format!("{kind}{}", &message[expected..]),
        None => kind.to_owned(),
    }
}
/// A configuration of the wrong shape (a number where a string belongs, an unknown key): the
/// section it is in and what was expected. Never the value found, which the deserializer's
/// message quotes and which may be a secret.
fn shape_error(value: &serde_json::Value, error: &serde_json::Error) -> anyhow::Error {
    let message = without_value(&error.to_string());
    // The section: the one that, put back to its default, lets the rest load.
    let defaults = Config::defaults().ok();
    let section = value.as_object().and_then(|sections| {
        sections.keys().find(|key| {
            let mut rest = value.clone();
            match defaults.as_ref().and_then(|d| d.get(key.as_str())) {
                Some(default) => rest[key.as_str()] = default.clone(),
                None => {
                    rest.as_object_mut().map(|o| o.remove(key.as_str()));
                }
            }
            serde_json::from_value::<Config>(rest).is_ok()
        })
    });
    match section {
        Some(section) => anyhow::anyhow!("invalid configuration in `{section}`: {message}"),
        None => anyhow::anyhow!("invalid configuration: {message}"),
    }
}
/// An upstream service's URL is a web URL with a host, and holds no secret: no user info, query
/// or fragment, which logs and errors would otherwise carry.
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
    /// The hub body cap, the discovery cache lifetime, the refusal hold and the watch-page cap
    /// are limits: their defaults, applied where they are read, and held to their ranges.
    #[test]
    fn the_also_on_caps_and_waits_are_limits() {
        let limits = Limits::default();
        assert_eq!(limits.hub_body_bytes, 8 * 1024 * 1024);
        assert_eq!(limits.discovery_cache_secs, 3600);
        assert_eq!(limits.also_on_refusal_hold_secs, 300);
        assert_eq!(limits.also_on_watch_page_bytes, 3 * 1024 * 1024);
        let set = with(serde_json::json!({"limits": {
            "hub_body_bytes": 16 * 1024 * 1024,
            "discovery_cache_secs": 7200,
            "also_on_refusal_hold_secs": 900,
            "also_on_watch_page_bytes": 4 * 1024 * 1024,
        }}))
        .unwrap();
        assert_eq!(set.egress.hub_body_bytes, 16 * 1024 * 1024);
        assert_eq!(set.egress.discovery_secs, 7200);
        assert_eq!(set.also_on.refusal_hold_secs, 900);
        assert_eq!(set.also_on.watch_page_bytes, 4 * 1024 * 1024);
        let egress = policy(&set);
        assert_eq!(crate::hubs::body_cap(&egress), 16 * 1024 * 1024);
        for (key, value) in [
            ("hub_body_bytes", 1024),
            ("hub_body_bytes", 65 * 1024 * 1024),
            ("discovery_cache_secs", 3599),
            ("discovery_cache_secs", 86401),
            // A refusal's wait must stay above a rejected token's minute (`client_credentials`).
            ("also_on_refusal_hold_secs", 60),
            ("also_on_refusal_hold_secs", 86401),
            ("also_on_watch_page_bytes", 1024),
            ("also_on_watch_page_bytes", 17 * 1024 * 1024),
        ] {
            assert!(
                with(serde_json::json!({"limits":{key:value}})).is_err(),
                "{key}={value}"
            );
        }
    }
    /// Every `limits.discovery_cache_secs` the config accepts is how long the cache keeps a
    /// successful answer: replayed until then, gone after.
    #[tokio::test(start_paused = true)]
    async fn an_answer_is_cached_for_the_discovery_cache_secs() {
        use crate::resolver::egress::{FetchOptions, Fetched};
        let url = "https://example.org/answer";
        for secs in [3600, 7200, 86400] {
            let set = with(serde_json::json!({"limits": {"discovery_cache_secs": secs}})).unwrap();
            let egress = policy(&set);
            let options = FetchOptions::default();
            let answer = Fetched::answer(
                url::Url::parse(url).unwrap(),
                200,
                "application/json".into(),
                b"{}".to_vec(),
            );
            egress
                .seed(url, &options, egress.config().discovery_secs, answer)
                .await
                .unwrap();
            tokio::time::advance(std::time::Duration::from_secs(secs - 1)).await;
            assert!(egress.discovered(url, &options).await.unwrap(), "{secs}");
            tokio::time::advance(std::time::Duration::from_secs(2)).await;
            assert!(!egress.discovered(url, &options).await.unwrap(), "{secs}");
        }
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
    /// carries.
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
        let cfg = with(serde_json::json!({"groups":{"enabled":false},"media":{"oembed":"always"},"web":{"discover_feeds":true}})).unwrap();
        assert!(!cfg.groups.enabled && !cfg.queue.groups.enabled);
        assert_eq!(
            cfg.media.oembed,
            crate::resolver::cards::OembedPolicy::Always
        );
        assert!(cfg.web.discover_feeds);
        assert!(Config::default().groups.enabled);
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
    /// `osm.nominatim_url` may carry a hosted Nominatim's key: it is read like any setting (the
    /// file, or `PINAKES__OSM__NOMINATIM_URL` from the environment file), and `Debug` and
    /// serialization never show it.
    #[test]
    fn the_nominatim_url_is_never_shown() {
        let base = "https://nominatim.example.org/KEY-CANARY";
        let mut value = Config::defaults().unwrap();
        assert_eq!(
            value["osm"]["nominatim_url"],
            "https://nominatim.openstreetmap.org"
        );
        apply_env(
            &mut value,
            [("PINAKES__OSM__NOMINATIM_URL".to_owned(), base.to_owned())],
        )
        .unwrap();
        let config = Config::from_value(value).unwrap();
        assert_eq!(&*config.osm.nominatim_url, base);
        let shown = format!("{config:?} {}", serde_json::to_string(&config).unwrap());
        assert!(!shown.contains("KEY-CANARY"), "{shown}");
    }
    /// `[catalogues]`: every catalogue is on by default, with OSM's region `planet`, OpenAlex's
    /// `min_citations` 50 and no limit; a file without the section loads the same. Its keys
    /// load from the file and from `PINAKES__CATALOGUES__…`; an unknown key or catalogue, a bad
    /// region and a zero limit are refused.
    #[test]
    fn catalogue_switches_and_keys_load_and_unknown_ones_are_refused() {
        use crate::catalogue::Catalogue;
        let config = Config::default();
        let c = &config.catalogues;
        assert!(Catalogue::ALL.iter().all(|cat| c.enabled(*cat)));
        assert_eq!(c.dir, "catalogues");
        assert_eq!(c.osm.region, "planet");
        assert_eq!(c.openalex.min_citations, 50);
        assert_eq!((c.wikidata.limit, c.musicbrainz.limit), (None, None));
        let file: Config = toml::from_str(
            "[catalogues.osm]\nenabled = false\nregion = \"europe/switzerland\"\n[catalogues.wikidata]\nlimit = 1000\n[catalogues.openalex]\nmin_citations = 100\n",
        )
        .unwrap();
        file.validate().unwrap();
        let c = &file.catalogues;
        assert!(!c.enabled(Catalogue::Osm) && c.enabled(Catalogue::Tranco));
        assert_eq!(c.osm.region, "europe/switzerland");
        assert_eq!(c.limit(Catalogue::Wikidata), Some(1000));
        assert_eq!(c.limit(Catalogue::Musicbrainz), None);
        assert_eq!(c.openalex.min_citations, 100);
        let all_off: Config = toml::from_str("[catalogues]\nenabled = false\n").unwrap();
        assert!(all_off.catalogues.enabled_set().is_empty());
        for refused in [
            serde_json::json!({"catalogues": {"wikipedia": {"enabled": true}}}),
            serde_json::json!({"catalogues": {"osm": {"limit": 10}}}),
            serde_json::json!({"catalogues": {"tranco": {"region": "planet"}}}),
            serde_json::json!({"catalogues": {"osm": {"region": "../planet"}}}),
            serde_json::json!({"catalogues": {"wikidata": {"limit": 0}}}),
            serde_json::json!({"catalogues": {"musicbrainz": {"limit": "many"}}}),
            serde_json::json!({"catalogues": {"dir": " "}}),
        ] {
            assert!(with(refused.clone()).is_err(), "{refused}");
        }
        let mut value = Config::defaults().unwrap();
        apply_env(
            &mut value,
            [
                ("PINAKES__CATALOGUES__OSM__REGION", "europe/germany"),
                ("PINAKES__CATALOGUES__OSM__ENABLED", "false"),
                ("PINAKES__CATALOGUES__WIKIDATA__LIMIT", "5000"),
                ("PINAKES__CATALOGUES__OPENALEX__MIN_CITATIONS", "10"),
                ("PINAKES__CATALOGUES__DIR", "/var/lib/pinakes/catalogues"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned())),
        )
        .unwrap();
        let env = Config::from_value(value).unwrap();
        let c = &env.catalogues;
        assert_eq!(c.osm.region, "europe/germany");
        assert!(!c.enabled(Catalogue::Osm));
        assert_eq!(c.wikidata.limit, Some(5000));
        assert_eq!(c.openalex.min_citations, 10);
        assert_eq!(c.dir, "/var/lib/pinakes/catalogues");
        let mut value = Config::defaults().unwrap();
        let unknown = apply_env(
            &mut value,
            [(
                "PINAKES__CATALOGUES__WIKIPEDIA__ENABLED".to_owned(),
                "false".to_owned(),
            )],
        )
        .and_then(|()| Config::from_value(value));
        assert!(unknown.is_err());
    }
    /// Nominatim at `osm.nominatim_url` is the one OSM provider: a provider setting, in the file
    /// or as a `PINAKES__…` override, is refused at load, never ignored.
    #[test]
    fn an_osm_provider_setting_is_refused() {
        let refused = with(serde_json::json!({"osm": {"provider": "nominatim"}}))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("unknown field `provider`"), "{refused}");
        let mut value = Config::defaults().unwrap();
        let overridden = apply_env(
            &mut value,
            [("PINAKES__OSM__PROVIDER".to_owned(), "nominatim".to_owned())],
        )
        .and_then(|()| Config::from_value(value));
        assert!(overridden.is_err(), "the override was accepted");
    }
    /// `groups.cross_container_occurrences` is retired: a file or override that still sets it
    /// loads with the one fixed UID rule and names the key as ignored; any other unknown key in
    /// `[groups]` is still refused.
    #[test]
    fn the_retired_uid_join_setting_loads_and_is_named_as_ignored() {
        let path =
            std::env::temp_dir().join(format!("pinakes-retired-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "[groups]\nenabled = true\ncross_container_occurrences = \"corroborated\"\n",
        )
        .unwrap();
        let loaded = Config::load(Some(&path));
        std::fs::remove_file(&path).unwrap();
        let cfg = loaded.unwrap();
        assert_eq!(cfg.ignored, ["groups.cross_container_occurrences"]);
        assert!(cfg.groups.enabled);
        assert_eq!(cfg.groups.policy(), Config::default().groups.policy());
        for mode in ["off", "corroborated"] {
            let cfg = with(serde_json::json!({"groups": {"cross_container_occurrences": mode}}));
            assert_eq!(
                cfg.unwrap().ignored,
                ["groups.cross_container_occurrences"],
                "{mode}"
            );
        }
        let mut value = Config::defaults().unwrap();
        apply_env(
            &mut value,
            [(
                "PINAKES__GROUPS__CROSS_CONTAINER_OCCURRENCES".to_owned(),
                "corroborated".to_owned(),
            )],
        )
        .unwrap();
        let cfg = Config::from_value(value).unwrap();
        assert_eq!(cfg.ignored, ["groups.cross_container_occurrences"]);
        // Not set, nothing is ignored; the policy no longer names the key.
        assert!(
            Config::default().ignored.is_empty()
                && with(serde_json::json!({})).unwrap().ignored.is_empty()
        );
        assert!(Config::default()
            .groups
            .policy()
            .get("cross_container_occurrences")
            .is_none());
        let refused = with(serde_json::json!({"groups": {"cross_feed": "on"}}))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("unknown field `cross_feed`"), "{refused}");
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
    /// A switched-off also-on source's API hosts are refused at the egress, whatever path asks,
    /// and only those: Deezer's API while it is off, never deezer.com's pages.
    #[test]
    fn a_switched_off_source_s_api_is_refused_at_the_egress() {
        if !crate::also_on::registered()
            .iter()
            .any(|s| s.id() == "deezer")
        {
            return;
        }
        let api = "https://api.deezer.com/track/3135556";
        let pages = [
            "https://www.deezer.com/track/3135556",
            "https://deezer.com/album/14879699",
            "https://deezer.page.link/xyz",
        ];
        let deezer = |also_on: serde_json::Value| with(serde_json::json!({ "also_on": also_on }));
        // On by default: its API is asked.
        let default = Config::load(None).unwrap();
        assert!(!default
            .egress
            .off_hosts
            .contains(&"api.deezer.com".to_owned()));
        assert!(policy(&default).allows(api));
        let off = deezer(serde_json::json!({"sources": {"deezer": {"enabled": false}}})).unwrap();
        assert!(off.egress.off_hosts.contains(&"api.deezer.com".to_owned()));
        assert!(
            off.egress.deny_hosts.is_empty(),
            "no rule of the operator's"
        );
        assert!(!policy(&off).allows(api));
        for page in pages {
            assert!(policy(&off).allows(page), "{page}");
        }
        let on = deezer(serde_json::json!({"sources": {"deezer": {"enabled": true}}})).unwrap();
        assert!(!on.egress.off_hosts.contains(&"api.deezer.com".to_owned()));
        assert!(policy(&on).allows(api));
        // With the worker off no source runs, so none calls its API.
        let idle =
            deezer(serde_json::json!({"enabled": false, "sources": {"deezer": {"enabled": true}}}))
                .unwrap();
        assert!(!policy(&idle).allows(api));
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
    /// The music enrichment limits stay where the card's read and the clock hold them.
    #[test]
    fn music_enrichment_limits_have_bounds() {
        let ok = Config::default();
        assert!(ok.validate().is_ok());
        for set in [
            |c: &mut Config| c.web.music_enrichment_timeout_ms = 13001,
            |c: &mut Config| c.web.music_enrichment_timeout_ms = u64::MAX,
            |c: &mut Config| c.web.music_enrichment_retry_secs = 59,
            |c: &mut Config| c.web.music_enrichment_retry_secs = u64::MAX,
            |c: &mut Config| c.web.music_image_width = 0,
            |c: &mut Config| c.web.music_image_width = 4097,
        ] {
            let mut c = Config::default();
            set(&mut c);
            assert!(c.validate().is_err());
        }
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
        let budgets: Vec<_> = crate::sources::budgets(&Config::default()).collect();
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
    /// A config file that does not parse, or holds a value of the wrong type, is reported by
    /// place and expectation: the line's text and the value, which may be a secret, stay out.
    #[test]
    fn a_config_error_names_the_place_and_never_the_text() {
        let path = std::env::temp_dir().join(format!("pinakes-bad-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "listen = \"127.0.0.1:8090\"\nadmin_token = \"s3cret-admin-token\n",
        )
        .unwrap();
        let error = format!("{:#}", Config::load(Some(&path)).unwrap_err());
        std::fs::remove_file(&path).unwrap();
        assert!(error.contains("is not valid TOML at line 2"), "{error}");
        assert!(!error.contains("s3cret"), "{error}");
        let error = format!(
            "{:#}",
            with(serde_json::json!({"admin_token": 8675309})).unwrap_err()
        );
        assert!(error.contains("`admin_token`"), "{error}");
        assert!(error.contains("expected"), "{error}");
        assert!(!error.contains("8675309"), "{error}");
        let error = format!(
            "{:#}",
            with(serde_json::json!({"groups": {"enabled": "s3cret"}})).unwrap_err()
        );
        assert!(error.contains("`groups`"), "{error}");
        assert!(!error.contains("s3cret"), "{error}");
        // A quoted value that holds the deserializer's own words gives none of itself away.
        assert_eq!(
            without_value("invalid type: string \"s3cret, expected more\", expected u64"),
            "invalid type, expected u64"
        );
        // A source's own section is typed by its source: the same holds there.
        for section in crate::also_on::sections() {
            let default = (section.default)();
            let Some(key) = default
                .as_object()
                .and_then(|o| o.iter().find(|(_, v)| v.is_string() || v.is_null()))
                .map(|(k, _)| k.clone())
            else {
                continue;
            };
            let Err(error) = with(serde_json::json!({section.name: {key: 8675309}})) else {
                continue;
            };
            let error = format!("{error:#}");
            assert!(error.contains(&format!("[{}]", section.name)), "{error}");
            assert!(!error.contains("8675309"), "{}: {error}", section.name);
        }
    }
    /// A refused `PINAKES__…` override names its variable, and never its value.
    #[test]
    fn a_refused_override_names_its_variable() {
        for (name, value) in [
            ("PINAKES__NO_SUCH__KEY", "s3cret"),
            ("PINAKES__API__NO_SUCH_KEY", "s3cret"),
            ("PINAKES__API__MCP", "s3cret"),
        ] {
            let mut config = Config::defaults().unwrap();
            let error = apply_env(&mut config, [(name.to_owned(), value.to_owned())]).unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains(name), "{error}");
            assert!(!error.contains("s3cret"), "{error}");
        }
    }
    /// A `limits.host_rates` key is a host as the egress names it; one that names no single
    /// host stops the load instead of pacing nothing.
    #[test]
    fn host_rate_keys_are_canonical_hosts() {
        let cfg = with(serde_json::json!({"limits":{"host_rates":{"Example.ORG": 0.5}}})).unwrap();
        assert_eq!(cfg.egress.host_rates.get("example.org"), Some(&0.5));
        assert_eq!(cfg.limits.host_rates.get("example.org"), Some(&0.5));
        let twice =
            serde_json::json!({"limits":{"host_rates":{"Example.org": 0.5, "example.org": 2.0}}});
        assert!(format!("{:#}", with(twice).unwrap_err()).contains("limits.host_rates"));
        let secret = with(
            serde_json::json!({"limits":{"host_rates":{"https://user:s3cret@example.org/": 1.0}}}),
        );
        assert!(!format!("{:#}", secret.unwrap_err()).contains("s3cret"));
        for bad in [
            "*.example.org",
            "https://example.org/",
            "example.org/path",
            "",
        ] {
            let error = with(serde_json::json!({"limits":{"host_rates":{bad: 0.5}}})).unwrap_err();
            assert!(
                format!("{error:#}").contains("limits.host_rates"),
                "{bad}: {error:#}"
            );
        }
    }
    /// The Nexus credential is flagged only when it would cross a network in clear.
    #[test]
    fn a_nexus_credential_over_plain_http_is_flagged_off_the_local_network() {
        let clear = |url: &str, authorization: Option<&str>| {
            with(serde_json::json!({"nexus_url": url, "nexus_authorization": authorization}))
                .unwrap()
                .nexus_authorization_in_clear()
        };
        assert!(clear("http://nexus.example.org", Some("Bearer x")));
        assert!(clear("http://203.0.113.7:8080", Some("Bearer x")));
        for (url, authorization) in [
            ("http://nexus.example.org", None),
            ("https://nexus.example.org", Some("Bearer x")),
            ("http://127.0.0.1:8080", Some("Bearer x")),
            ("http://localhost:8080", Some("Bearer x")),
            ("http://10.0.0.5:8080", Some("Bearer x")),
            ("http://[::1]:8080", Some("Bearer x")),
            ("http://[fe80::1]:8080", Some("Bearer x")),
        ] {
            assert!(!clear(url, authorization), "{url}");
        }
    }
    /// The limiter sizes and the two deadlines are range-checked.
    #[test]
    fn limiter_sizes_and_deadlines_are_bounded() {
        let cfg = Config::default();
        assert_eq!(cfg.api.request_timeout_secs, 30);
        assert_eq!(cfg.shutdown_grace_secs, 30);
        for (key, value) in [
            ("api_cache_capacity", 99),
            ("api_cache_capacity", 100_001),
            ("preview_cache_capacity", 99),
            ("request_timeout_secs", 7),
            ("request_timeout_secs", 601),
            ("shutdown_grace_secs", 0),
            ("shutdown_grace_secs", 601),
        ] {
            assert!(
                with(serde_json::json!({"limits":{key:value}})).is_err(),
                "{key}={value}"
            );
        }
        let cfg =
            with(serde_json::json!({"limits":{"request_timeout_secs":8,"shutdown_grace_secs":7}}))
                .unwrap();
        assert_eq!(
            (cfg.api.request_timeout_secs, cfg.shutdown_grace_secs),
            (8, 7)
        );
        // At the lowest request deadline an MCP tool call still has the three seconds `card`
        // with `resolve: true` waits for a new link, and time for its other reads.
        assert_eq!(cfg.api.tool_timeout(), std::time::Duration::from_secs(6));
        assert_eq!(
            Config::default().api.tool_timeout(),
            std::time::Duration::from_secs(28)
        );
    }
    /// A search's reach read ends before the request's deadline, and before an MCP tool
    /// call's, two seconds inside it: a slow Nexus is the reach's own answer, never a 504.
    #[test]
    fn the_reach_deadline_is_inside_the_request_s() {
        for (reach, request, accepted) in [
            (5, 8, true),
            (6, 8, false),
            (8, 8, false),
            (27, 30, true),
            (28, 30, false),
            (30, 30, false),
            (60, 30, false),
            (60, 63, true),
        ] {
            let loaded = with(serde_json::json!({"limits":{
                "reach_timeout_secs": reach, "request_timeout_secs": request}}));
            assert_eq!(loaded.is_ok(), accepted, "reach {reach}, request {request}");
            if let Err(error) = loaded {
                let text = format!("{error:#}");
                assert!(
                    text.contains("limits.reach_timeout_secs")
                        && text.contains("limits.request_timeout_secs"),
                    "{text}"
                );
            }
        }
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
            ("preview_grace_hours", 0),
            ("preview_grace_hours", 87601),
            ("preview_new_per_hour", 0),
            ("preview_new_per_hour", 1_000_001),
            ("preview_new_per_client_per_hour", 1_000_001),
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
    /// Hours a resource nothing ever referenced (one only previewed) is kept, at most `grace_days`.
    preview_grace_hours: u32 = |c| c.preview_grace_hours => [|c, v| c.preview_grace_hours = v],
    /// Resource refresh intervals by tier (seconds).
    refresh_resource_secs: [u64; 4] = |c| c.refresh.resource_secs => [|c, v| c.refresh.resource_secs = v],
    /// Container (calendar, feed) refresh intervals by tier (seconds).
    refresh_container_secs: [u64; 4] = |c| c.refresh.container_secs => [|c, v| c.refresh.container_secs = v],
    /// Cursor feed refresh interval (seconds).
    refresh_cursor_secs: u64 = |c| c.refresh.cursor_secs => [|c, v| c.refresh.cursor_secs = v],
    /// Longest wait between reads of an undated card that keeps answering the same, and the wait of a link found gone (seconds).
    refresh_max_secs: u64 = |c| c.refresh.max_secs => [|c, v| c.refresh.max_secs = v],
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
    /// How long one PKARR relay may take to answer (milliseconds, 100–7000); a relay slower than
    /// this is a failed relay, and the others' packets are used.
    pkarr_relay_timeout_ms: u64 = |c| c.pubky.pkarr_relay_timeout_ms => [|c, v| c.pubky.pkarr_relay_timeout_ms = v],
    /// How long a MusicBrainz card's enrichment may take, within the card's own read
    /// (milliseconds, 100–13000); past it, the card keeps its stored image and description.
    music_enrichment_timeout_ms: u64 = |c| c.web.music_enrichment_timeout_ms => [|c, v| c.web.music_enrichment_timeout_ms = v],
    /// The earliest a MusicBrainz card whose enrichment failed for a while (a timeout, a 429, a
    /// 5xx) is read again (seconds, 60–604800): it waits at least twice as long as since its last
    /// reading, up to its own refresh time, so a failure on a routine refresh waits for the next.
    music_enrichment_retry_secs: u64 = |c| c.web.music_enrichment_retry_secs => [|c, v| c.web.music_enrichment_retry_secs = v],
    /// The width of the Commons thumbnail an enriched card asks for (pixels, 20–4096); Commons
    /// serves the next of its standard sizes (960 for 640).
    music_image_width: u32 = |c| c.web.music_image_width => [|c, v| c.web.music_image_width = v],
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
    /// The best units a text search reads from the text index (50–10000).
    search_text_candidates: usize = |c| c.search.text_candidates => [|c, v| c.search.text_candidates = v],
    /// Seconds between two passes that commit queued changes to the text index (1–60).
    search_index_commit_secs: u64 = |c| c.search.index_commit_secs => [|c, v| c.search.index_commit_secs = v],
    /// Text searches the index runs at once (0–256; 0: one per CPU core).
    search_index_searches: usize = |c| c.search.index_searches => [|c, v| c.search.index_searches = v],
    /// The most a text search's index stage takes, its wait for a turn included (100–30000 ms).
    search_index_budget_ms: u64 = |c| c.search.index_budget_ms => [|c, v| c.search.index_budget_ms = v],
    /// Entries the rate limiter remembers when an API request adds a client address (100–100000). It shares one table with the preview limiter; past it the oldest entry is forgotten with its used budget. Also the client addresses the dump download limiter remembers per hour.
    api_cache_capacity: usize = |c| c.api.cache_capacity => [|c, v| c.api.cache_capacity = v],
    /// Seconds a request may take on either listener before it is answered 504 (8–600).
    request_timeout_secs: u64 = |c| c.api.request_timeout_secs => [|c, v| c.api.request_timeout_secs = v],
    /// Seconds a stopping daemon gives open requests, running jobs and background passes to finish.
    shutdown_grace_secs: u64 = |c| c.shutdown_grace_secs => [|c, v| c.shutdown_grace_secs = v],
    /// Budget units a search costs for each of `reach` and `boost`.
    reach_cost: usize = |c| c.api.reach_cost => [|c, v| c.api.reach_cost = v],
    /// Budget units a related search costs.
    related_cost: usize = |c| c.api.related_cost => [|c, v| c.api.related_cost = v],
    /// Preview requests per minute per client.
    preview_requests_per_minute: usize = |c| c.preview.requests_per_minute => [|c, v| c.preview.requests_per_minute = v],
    /// Preview requests per minute per URI.
    preview_uri_requests_per_minute: usize = |c| c.preview.uri_requests_per_minute => [|c, v| c.preview.uri_requests_per_minute = v],
    /// The same table's size when a preview adds its client address and link (100–100000); past it the oldest entry (an API client's too) is forgotten with its used budget.
    preview_cache_capacity: usize = |c| c.preview.cache_capacity => [|c, v| c.preview.cache_capacity = v],
    /// New previews per hour for the whole instance; over it `POST /v1/resolve` answers 503.
    preview_new_per_hour: usize = |c| c.preview_new_per_hour => [|c, v| c.preview_new_per_hour = v],
    /// New previews per hour one client address may take (0: no share); over it `POST /v1/resolve` answers 429.
    preview_new_per_client_per_hour: usize = |c| c.preview_new_per_client_per_hour => [|c, v| c.preview_new_per_client_per_hour = v],
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
    /// How long a service's refusal of the operator's credentials (TIDAL, Spotify, Podcast Index)
    /// is held before it is asked again (seconds, 120–86400).
    also_on_refusal_hold_secs: u64 = |c| c.also_on.refusal_hold_secs => [|c, v| c.also_on.refusal_hold_secs = v],
    /// The largest YouTube watch page `youtube_match` reads (bytes, 1–16 MiB); a larger one is
    /// refused.
    also_on_watch_page_bytes: usize = |c| c.also_on.watch_page_bytes => [|c, v| c.also_on.watch_page_bytes = v],
    /// The body cap of a MusicBrainz or Wikidata answer (bytes, 1–64 MiB).
    hub_body_bytes: usize = |c| c.egress.hub_body_bytes => [|c, v| c.egress.hub_body_bytes = v],
    /// How long an answer the also-on sources, lookups and hubs ask for is shared (seconds,
    /// 3600–86400: the cache keeps a successful answer at least an hour).
    discovery_cache_secs: u64 = |c| c.egress.discovery_secs => [|c, v| c.egress.discovery_secs = v],
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
    /// Most properties one calendar may hold, over all its components.
    event_max_properties: usize = |c| c.event.max_properties => [|c, v| c.event.max_properties = v],
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
