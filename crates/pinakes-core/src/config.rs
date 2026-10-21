use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub database_url: String,
    pub listen: String,
    pub admin_listen: String,
    pub admin_token: Option<String>,
    pub nexus_url: String,
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
    /// The Podcast Index API's key and secret, for the `podcasts` also-on source.
    pub podcast_index: crate::also_on::podcasts::PodcastIndexConfig,
    /// TIDAL's developer API credentials and country, for the `tidal` also-on source.
    pub tidal: crate::also_on::tidal::TidalConfig,
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
/// Secrets read from a variable of their own (`PINAKES_NAME`, one underscore): a build that has no
/// such setting ignores the variable, where a `PINAKES__SECTION__KEY` it does not know stops it.
const NAMED_ENV: &[(&str, &[&str], Named)] = &[
    (
        "PINAKES_TIDAL_CLIENT_ID",
        &["tidal", "client_id"],
        Named::Sets,
    ),
    (
        "PINAKES_TIDAL_CLIENT_SECRET",
        &["tidal", "client_secret"],
        Named::Sets,
    ),
    (
        "PINAKES_GITHUB_TOKEN",
        &["sources", "github", "token"],
        Named::Fills,
    ),
];
/// How a named variable meets the file form.
#[derive(Clone, Copy, PartialEq)]
enum Named {
    /// It replaces what the file and the `PINAKES__…` form say.
    Sets,
    /// It fills the value only when the file and the `PINAKES__…` form leave it unset (a source's
    /// entry is created for it).
    Fills,
}
/// Apply the `NAMED_ENV` variables that are set (and not empty) onto the file form.
fn apply_named_env(
    value: &mut serde_json::Value,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Result<()> {
    for (key, val) in vars {
        let Some((_, path, named)) = NAMED_ENV.iter().find(|(name, _, _)| *name == key) else {
            continue;
        };
        if val.trim().is_empty() {
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
        node.insert(last.to_string(), serde_json::Value::String(val));
    }
    Ok(())
}
/// Apply `PINAKES__SECTION__KEY` overrides onto the file form. Strings are literal; numbers,
/// booleans and arrays are JSON. A new map entry's keys are strings except `enabled`, and a
/// new `limits.host_rates` entry is a number.
fn apply_env(
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
            tidal: Default::default(),
            nexus_url: "http://127.0.0.1:8080".into(),
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
/// The same-event hint (`maybe_same` on event cards): `on` or `off`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
    /// Event cards name other events that are possibly the same one (`maybe_same`).
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
/// The ISO 3166-1 alpha-2 codes in use (tzdata's `iso3166.tab`): a TIDAL country must be one,
/// so a typo stops startup instead of failing every request.
const COUNTRIES: &[&str] = &[
    "AD", "AE", "AF", "AG", "AI", "AL", "AM", "AO", "AQ", "AR", "AS", "AT", "AU", "AW", "AX", "AZ",
    "BA", "BB", "BD", "BE", "BF", "BG", "BH", "BI", "BJ", "BL", "BM", "BN", "BO", "BQ", "BR", "BS",
    "BT", "BV", "BW", "BY", "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI", "CK", "CL", "CM", "CN",
    "CO", "CR", "CU", "CV", "CW", "CX", "CY", "CZ", "DE", "DJ", "DK", "DM", "DO", "DZ", "EC", "EE",
    "EG", "EH", "ER", "ES", "ET", "FI", "FJ", "FK", "FM", "FO", "FR", "GA", "GB", "GD", "GE", "GF",
    "GG", "GH", "GI", "GL", "GM", "GN", "GP", "GQ", "GR", "GS", "GT", "GU", "GW", "GY", "HK", "HM",
    "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IM", "IN", "IO", "IQ", "IR", "IS", "IT", "JE", "JM",
    "JO", "JP", "KE", "KG", "KH", "KI", "KM", "KN", "KP", "KR", "KW", "KY", "KZ", "LA", "LB", "LC",
    "LI", "LK", "LR", "LS", "LT", "LU", "LV", "LY", "MA", "MC", "MD", "ME", "MF", "MG", "MH", "MK",
    "ML", "MM", "MN", "MO", "MP", "MQ", "MR", "MS", "MT", "MU", "MV", "MW", "MX", "MY", "MZ", "NA",
    "NC", "NE", "NF", "NG", "NI", "NL", "NO", "NP", "NR", "NU", "NZ", "OM", "PA", "PE", "PF", "PG",
    "PH", "PK", "PL", "PM", "PN", "PR", "PS", "PT", "PW", "PY", "QA", "RE", "RO", "RS", "RU", "RW",
    "SA", "SB", "SC", "SD", "SE", "SG", "SH", "SI", "SJ", "SK", "SL", "SM", "SN", "SO", "SR", "SS",
    "ST", "SV", "SX", "SY", "SZ", "TC", "TD", "TF", "TG", "TH", "TJ", "TK", "TL", "TM", "TN", "TO",
    "TR", "TT", "TV", "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ", "VA", "VC", "VE", "VG", "VI",
    "VN", "VU", "WF", "WS", "YE", "YT", "ZA", "ZM", "ZW",
];
impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut value = serde_json::to_value(Self::default())?;
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
    /// A configuration from its file form (sections plus `[limits]`), validated.
    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        let mut cfg: Self = serde_json::from_value(value)?;
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
        // A source's published limits, for hosts the operator does not rate.
        self.egress.source_rates = crate::sources::budgets()
            .map(|(host, rate)| (host.to_owned(), rate))
            .collect();
    }
    pub fn validate(&self) -> Result<()> {
        if self.pubky.enabled {
            if self.pubky.pkarr_relays.is_empty() {
                bail!("[pubky] pkarr_relays must list at least one relay");
            }
            for relay in &self.pubky.pkarr_relays {
                let url = url::Url::parse(relay)
                    .with_context(|| format!("[pubky] pkarr_relays: invalid URL {relay}"))?;
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    bail!("[pubky] pkarr_relays: {relay} must be an https:// base URL without credentials, query or fragment");
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
                if id != "discogs"
                    || token.len() > 256
                    || !token.bytes().all(|b| b.is_ascii_graphic())
                {
                    bail!("also_on.sources.{id}.token must be at most 256 printable characters, and only discogs takes one");
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
        let country = &self.tidal.country;
        if !COUNTRIES.contains(&country.as_str()) {
            bail!(
                "tidal.country must be an ISO 3166-1 alpha-2 country code in upper case, like US"
            );
        }
        if a.apple_storefront.len() != 2
            || !a.apple_storefront.bytes().all(|b| b.is_ascii_lowercase())
        {
            bail!("also_on.apple_storefront must be a two-letter country code in lower case");
        }
        if !(1024 * 1024..=16 * 1024 * 1024).contains(&self.egress.html_head_bytes) {
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
        let nexus = url::Url::parse(&self.nexus_url)?;
        if !matches!(nexus.scheme(), "http" | "https") || nexus.host_str().is_none() {
            bail!("invalid Nexus URL");
        }
        {
            let url = url::Url::parse(&self.osm.nominatim_url)?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                bail!("invalid OSM upstream URL");
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    fn with(change: serde_json::Value) -> Result<Config> {
        let mut value = serde_json::to_value(Config::default()).unwrap();
        merge(&mut value, change);
        Config::from_value(value)
    }
    #[test]
    fn loading_nothing_gives_the_defaults_and_limits_round_trip() {
        let loaded = Config::load(None).unwrap();
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
    #[test]
    fn the_github_token_has_a_short_name_the_file_overrides() {
        let vars = |token: &str| vec![("PINAKES_GITHUB_TOKEN".to_owned(), token.to_owned())];
        let mut value = serde_json::to_value(Config::default()).unwrap();
        apply_named_env(&mut value, vars("from-env")).unwrap();
        let cfg = Config::from_value(value).unwrap();
        assert_eq!(cfg.sources.token("github"), Some("from-env"));
        assert!(cfg.sources.enabled("github"));
        // A token in the file (or the `PINAKES__…` form) wins; an empty one is none.
        let mut value = serde_json::to_value(Config::default()).unwrap();
        value["sources"]["github"] = serde_json::json!({"token": "from-file"});
        apply_named_env(&mut value, vars("from-env")).unwrap();
        assert_eq!(
            Config::from_value(value).unwrap().sources.token("github"),
            Some("from-file")
        );
        let mut value = serde_json::to_value(Config::default()).unwrap();
        apply_named_env(&mut value, vars("")).unwrap();
        assert!(!Config::from_value(value).unwrap().sources.enabled("github"));
    }

    #[test]
    fn env_overrides_create_platform_and_host_rate_entries() {
        let load = |vars: &[(&str, &str)]| {
            let mut value = serde_json::to_value(Config::default()).unwrap();
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
    fn the_podcast_index_pair_comes_from_the_environment_and_stays_secret() {
        let mut value = serde_json::to_value(Config::default()).unwrap();
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
    #[test]
    fn the_tidal_pair_comes_from_its_own_variables_and_stays_secret() {
        let mut value = serde_json::to_value(Config::default()).unwrap();
        let vars = [
            ("PINAKES_TIDAL_CLIENT_ID", "ABCDEFID"),
            ("PINAKES_TIDAL_CLIENT_SECRET", "ABCDEFSECRET"),
            ("PINAKES_OTHER", "x"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        // `apply_env` reads only the double-underscore form.
        apply_env(&mut value, vars.clone()).unwrap();
        apply_named_env(&mut value, vars).unwrap();
        let cfg = Config::from_value(value).unwrap();
        assert_eq!(cfg.tidal.client_id.as_deref(), Some("ABCDEFID"));
        assert_eq!(cfg.tidal.client_secret.as_deref(), Some("ABCDEFSECRET"));
        assert_eq!(cfg.tidal.country, "US");
        let shown = format!("{cfg:?} {}", serde_json::to_string(&cfg).unwrap());
        assert!(!shown.contains("ABCDEF"), "{shown}");
        // An empty variable leaves the file's value.
        let mut value = serde_json::to_value(Config::default()).unwrap();
        value["tidal"]["client_id"] = "FROMFILE".into();
        apply_named_env(&mut value, [("PINAKES_TIDAL_CLIENT_ID".into(), "".into())]).unwrap();
        assert_eq!(value["tidal"]["client_id"], "FROMFILE");
        assert_eq!(COUNTRIES.len(), 249);
        for country in ["us", "USA", "", "UX", "EU"] {
            assert!(with(serde_json::json!({"tidal":{"country":country}})).is_err());
        }
        assert_eq!(
            with(serde_json::json!({"tidal":{"country":"CH"}}))
                .unwrap()
                .tidal
                .country,
            "CH"
        );
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
            let mut value = serde_json::to_value(Config::default()).unwrap();
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
    /// The facts cap has a floor: a recording's single-valued facts fit beside its ISRCs.
    #[test]
    fn the_facts_cap_is_at_least_eight() {
        let mut c = Config::default();
        c.also_on.facts_per_source = 7;
        assert!(c.validate().is_err());
        c.also_on.facts_per_source = 8;
        assert!(c.validate().is_ok());
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
    /// Budget units a reach search costs.
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
