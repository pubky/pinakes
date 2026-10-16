use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub database_url: String,
    pub listen: String,
    pub admin_listen: String,
    pub refresh: RefreshConfig,
    pub canary: CanaryConfig,
    pub nexus_url: String,
    pub poll_secs: u64,
    pub grace_days: u32,
    pub web: ResolverConfig,
    pub geo_enabled: bool,
    pub osm: OsmConfig,
    pub search: SearchConfig,
    pub pubky: ResolverConfig,
    pub egress: EgressConfig,
    pub queue: QueueConfig,
    pub preview: PreviewConfig,
    pub api: ApiConfig,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: "postgresql:///pinakes?host=/var/run/postgresql".into(),
            listen: "127.0.0.1:8090".into(),
            admin_listen: "127.0.0.1:9090".into(),
            refresh: RefreshConfig::default(),
            canary: CanaryConfig::default(),
            nexus_url: "http://127.0.0.1:8080".into(),
            poll_secs: 5,
            grace_days: 7,
            web: ResolverConfig {
                concurrency: 4,
                requests_per_second: 4.0,
                ..Default::default()
            },
            pubky: ResolverConfig::default(),
            geo_enabled: true,
            osm: OsmConfig::default(),
            search: SearchConfig::default(),
            egress: EgressConfig::default(),
            queue: QueueConfig::default(),
            preview: PreviewConfig::default(),
            api: ApiConfig::default(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResolverConfig {
    pub enabled: bool,
    pub concurrency: usize,
    pub requests_per_second: f64,
    pub ttl_secs: u64,
}
impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
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
    pub host_requests_per_second: f64,
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
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
    pub max_failures: u32,
    pub backoff_secs: u64,
    pub max_backoff_secs: u64,
    pub lease_secs: u64,
    pub missing_ttl_secs: u64,
    pub author_requests_per_second: f64,
}
impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_failures: 5,
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
    pub ttl_secs: u64,
    pub fallback_enabled: bool,
    pub api_url: String,
    pub dump_requests_per_minute: usize,
    pub dump_chunk_secs: u64,
    pub dump_total_secs: u64,
}
impl Default for OsmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            nominatim_url: "https://nominatim.openstreetmap.org".into(),
            requests_per_second: 0.5,
            ttl_secs: 30 * 86400,
            fallback_enabled: false,
            api_url: "https://api.openstreetmap.org".into(),
            dump_requests_per_minute: 2,
            dump_chunk_secs: 10,
            dump_total_secs: 120,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub reach_timeout_secs: u64,
    pub max_radius_m: f64,
    pub h3_resolutions: Vec<u8>,
    pub max_cluster_cells: usize,
}
impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            reach_timeout_secs: 5,
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
    pub cache_capacity: usize,
    pub reach_cost: usize,
}
impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 120,
            cache_capacity: 10000,
            reach_cost: 6,
        }
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
            merge(&mut value, serde_json::to_value(file)?);
        }
        for (key, val) in std::env::vars()
            .filter_map(|(k, v)| k.strip_prefix("PINAKES__").map(|s| (s.to_lowercase(), v)))
        {
            let parts: Vec<_> = key.split("__").collect();
            let mut node = &mut value;
            for part in &parts[..parts.len() - 1] {
                node = node.get_mut(*part).context("unknown config section")?;
            }
            let target = node
                .get_mut(parts[parts.len() - 1])
                .context("unknown config key")?;
            *target = if target.is_string() {
                serde_json::Value::String(val)
            } else {
                serde_json::from_str(&val).context("env override requires JSON value")?
            };
        }
        let cfg: Self = serde_json::from_value(value)?;
        cfg.validate()?;
        Ok(cfg)
    }
    pub fn validate(&self) -> Result<()> {
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
        for endpoint in [&self.osm.nominatim_url, &self.osm.api_url] {
            let url = url::Url::parse(endpoint)?;
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
            || [&self.osm.nominatim_url, &self.osm.api_url]
                .iter()
                .any(|u| {
                    url::Url::parse(u).is_ok_and(|u| crate::resolver::osm::public_upstream(&u))
                })
                && self.osm.requests_per_second > 1.0
            || self.osm.ttl_secs == 0
            || self.osm.ttl_secs > 31536000
            || self.osm.dump_requests_per_minute == 0
            || !(1..=60).contains(&self.osm.dump_chunk_secs)
            || !(1..=600).contains(&self.osm.dump_total_secs)
            || !(122..=100000).contains(&self.search.max_cluster_cells)
            || !(1..=10000).contains(&self.api.requests_per_minute)
            || !(2..=100000).contains(&self.api.cache_capacity)
            || !(2..=100).contains(&self.api.reach_cost)
            || !(1..=60).contains(&self.search.reach_timeout_secs)
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
            || self.grace_days > 3650
            || !(1..=32).contains(&self.queue.max_failures)
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
    #[test]
    fn config_merges_sections_and_refuses_unknown_keys_and_unbounded_budgets() {
        let mut value = serde_json::to_value(Config::default()).unwrap();
        merge(
            &mut value,
            serde_json::json!({"web":{"concurrency":8},"egress":{"deny_hosts":["*.example.org"]}}),
        );
        let mut config: Config = serde_json::from_value(value.clone()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.web.concurrency, 8);
        assert_eq!(config.web.ttl_secs, 86400);
        value["web"]["typo"] = true.into();
        assert!(serde_json::from_value::<Config>(value).is_err());
        config.web.requests_per_second = 1e-320;
        assert!(config.validate().is_err());
    }
}
