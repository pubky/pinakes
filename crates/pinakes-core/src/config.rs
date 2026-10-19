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
    pub egress: EgressConfig,
    pub groups: crate::groups::GroupConfig,
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
    pub queue: QueueConfig,
    #[serde(skip)]
    pub preview: PreviewConfig,
}
impl Default for Config {
    fn default() -> Self {
        let mut config = Self::base();
        config.limits = Limits::of(&config);
        config
    }
}
/// Map sections whose entries an operator names: a `PINAKES__…` override may create one.
const ENV_MAPS: &[&[&str]] = &[HOST_RATES];
/// The one map whose new entries are numbers.
const HOST_RATES: &[&str] = &["limits", "host_rates"];
/// Apply `PINAKES__SECTION__KEY` overrides onto the file form. Strings are literal; numbers,
/// booleans and arrays are JSON. A new `limits.host_rates` entry is a number.
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
            nexus_url: "http://127.0.0.1:8080".into(),
            poll_secs: 5,
            ingest: IngestConfig::default(),
            grace_days: 7,
            // The web route fetches arbitrary hosts: twice the other resolvers' workers and rate.
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
            geo: GeoConfig::default(),
            osm: OsmConfig::default(),
            egress: EgressConfig::default(),
            queue: QueueConfig::default(),
            groups: Default::default(),
            preview: PreviewConfig::default(),
            api: ApiConfig::default(),
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
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    #[serde(skip)]
    pub host_requests_per_second: f64,
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
    #[serde(skip)]
    pub host_rates: BTreeMap<String, f64>,
}
impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            host_requests_per_second: 1.0,
            allow_hosts: vec![],
            deny_hosts: vec![],
            host_rates: BTreeMap::new(),
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
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    pub requests_per_minute: usize,
    #[serde(skip)]
    pub cache_capacity: usize,
}
impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 120,
            cache_capacity: 10000,
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
        Self::from_value(value)
    }
    /// A configuration from its file form (sections plus `[limits]`), validated.
    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        let mut cfg: Self = serde_json::from_value(value)?;
        let limits = cfg.limits.clone();
        limits.apply(&mut cfg);
        cfg.queue.groups = cfg.groups.clone();
        cfg.validate()?;
        Ok(cfg)
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
            || !(1..=10000).contains(&self.api.requests_per_minute)
            || !(2..=100000).contains(&self.api.cache_capacity)
        {
            bail!("invalid OSM or API budget (public Nominatim maximum 1 request/s)");
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
    fn env_overrides_create_host_rate_entries() {
        let load = |vars: &[(&str, &str)]| {
            let mut value = serde_json::to_value(Config::default()).unwrap();
            apply_env(
                &mut value,
                vars.iter().map(|(k, v)| (k.to_string(), v.to_string())),
            )?;
            Config::from_value(value)
        };
        let cfg = load(&[("PINAKES__LIMITS__HOST_RATES__EXAMPLE.ORG", "0.5")]).unwrap();
        assert_eq!(cfg.egress.host_rates["example.org"], 0.5);
        for unknown in ["PINAKES__WEB__NO_SUCH_KEY", "PINAKES__NO_SUCH__KEY"] {
            assert!(load(&[(unknown, "x")]).is_err(), "{unknown}");
        }
    }
    /// `.env.example` names only variables the config accepts (its commented examples too).
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
    #[test]
    fn limits_apply_once_to_every_resolver_and_caps_left_the_sections() {
        let cfg = with(serde_json::json!({"limits":{"resolver_concurrency":8},"egress":{"deny_hosts":["*.example.org"]}})).unwrap();
        for c in [&cfg.pubky, &cfg.cards.media, &cfg.cards.activitypub] {
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
        Config::load(Some(&path)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let limits = serde_json::to_value(Limits::default()).unwrap();
        for key in limits.as_object().unwrap().keys() {
            assert!(
                !text.lines().any(|l| l.starts_with(&format!("{key} = "))),
                "{key} is a limit, not an operator choice"
            );
        }
    }
    #[test]
    fn ingest_limits_have_safe_defaults_and_config_bounds() {
        let config = Config::default();
        assert_eq!(config.ingest.max_label_bytes, 128);
        assert_eq!(config.ingest.max_object_path_bytes, 2048);
        for (key, value) in [
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
    /// Concurrent jobs of the web resolver.
    web_concurrency: usize = |c| c.web.concurrency => [|c, v| c.web.concurrency = v],
    /// Jobs per second of the web resolver.
    web_requests_per_second: f64 = |c| c.web.requests_per_second => [|c, v| c.web.requests_per_second = v],
    /// Concurrent jobs per pubky or card resolver.
    resolver_concurrency: usize = |c| c.pubky.concurrency => [
        |c, v| c.pubky.concurrency = v,
        |c, v| c.cards.media.concurrency = v, |c, v| c.cards.isbn.concurrency = v,
        |c, v| c.cards.doi.concurrency = v, |c, v| c.cards.activitypub.concurrency = v],
    /// Jobs per second per pubky or card resolver.
    resolver_requests_per_second: f64 = |c| c.pubky.requests_per_second => [
        |c, v| c.pubky.requests_per_second = v,
        |c, v| c.cards.media.requests_per_second = v, |c, v| c.cards.isbn.requests_per_second = v,
        |c, v| c.cards.doi.requests_per_second = v,
        |c, v| c.cards.activitypub.requests_per_second = v],
    /// Default document lifetime (seconds) for web, pubky and card resolvers.
    resolver_ttl_secs: u64 = |c| c.web.ttl_secs => [
        |c, v| c.web.ttl_secs = v, |c, v| c.pubky.ttl_secs = v,
        |c, v| c.cards.media.ttl_secs = v, |c, v| c.cards.isbn.ttl_secs = v,
        |c, v| c.cards.doi.ttl_secs = v, |c, v| c.cards.activitypub.ttl_secs = v],
    /// Requests per second to any one upstream host.
    host_requests_per_second: f64 = |c| c.egress.host_requests_per_second => [|c, v| c.egress.host_requests_per_second = v],
    /// Per-host overrides of `host_requests_per_second`.
    host_rates: BTreeMap<String, f64> = |c| c.egress.host_rates.clone() => [|c, v| c.egress.host_rates = v],
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
    /// API response cache entries.
    api_cache_capacity: usize = |c| c.api.cache_capacity => [|c, v| c.api.cache_capacity = v],
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
}
