use crate::model::ResourceMeta;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Cursor,
    Container,
    Resource,
    None,
}
impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::Container => "container",
            Self::Resource => "resource",
            Self::None => "none",
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Manifest {
    pub source: String,
    pub mode: Mode,
    pub cadence_floor: u64,
    pub identity: String,
    pub cancellation: String,
    pub expected_fields: Vec<String>,
    pub canary: Option<Url>,
    pub canary_note: Option<String>,
    pub ua_profile: String,
    pub requires_key: bool,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    pub fragile: bool,
}
pub fn manifest(source: &str) -> Manifest {
    let (mode, floor, identity, cancellation, fields, canary) = match source {
        "geo" => (
            Mode::None,
            0,
            "coordinate_uri",
            "none",
            vec!["location"],
            None,
        ),
        "osm" => (
            Mode::Resource,
            30 * 86400,
            "osm_type/osm_id",
            "missing_twice",
            vec!["name", "location"],
            Some("https://www.openstreetmap.org/node/240109189"),
        ),
        "event" => (
            Mode::Container,
            3600,
            "feed/uid/recurrence_id",
            "explicit_or_missing_twice",
            vec![],
            None,
        ),
        "web" => (
            Mode::Resource,
            3600,
            "normalized_uri",
            "missing_twice",
            vec![],
            Some("https://example.org/"),
        ),
        "pubky" => (
            Mode::Resource,
            3600,
            "normalized_uri",
            "missing_twice",
            vec!["attributes.content_type"],
            None,
        ),
        _ => (
            Mode::Resource,
            3600,
            "normalized_uri",
            "missing_twice",
            vec![],
            None,
        ),
    };
    Manifest {
        source: source.into(),
        mode,
        cadence_floor: floor,
        identity: identity.into(),
        cancellation: cancellation.into(),
        expected_fields: fields.into_iter().map(str::to_owned).collect(),
        canary: canary.map(|s| s.parse().expect("manifest URL")),
        canary_note: None,
        ua_profile: "pinakes".into(),
        requires_key: false,
        enabled: true,
        disabled_reason: None,
        fragile: false,
    }
}
pub fn manifests() -> Vec<Manifest> {
    ["web", "pubky", "geo", "osm", "event"]
        .into_iter()
        .map(manifest)
        .collect()
}
impl Manifest {
    pub fn missing_fields(&self, doc: &ResourceMeta) -> Vec<String> {
        let value = serde_json::to_value(doc).expect("document serialization");
        self.expected_fields
            .iter()
            .filter(|field| {
                let value = field.split('.').try_fold(&value, |v, k| v.get(k));
                value.is_none_or(|v| {
                    v.is_null()
                        || v.as_str().is_some_and(str::is_empty)
                        || v.as_array().is_some_and(Vec::is_empty)
                })
            })
            .cloned()
            .collect()
    }
}

pub fn configured(config: &crate::config::Config) -> Vec<Manifest> {
    let mut all = manifests();
    for m in &mut all {
        m.enabled = match m.source.as_str() {
            "web" => config.web.enabled,
            "event" => config.event.enabled && config.event.calendars,
            "pubky" => config.pubky.enabled,
            "geo" => config.geo_enabled,
            "osm" => config.osm.enabled,
            _ => true,
        };
        if m.source == "osm" {
            m.cadence_floor = config.osm.ttl_secs.max(3600);
        }
    }
    all.extend(crate::event::adapters::manifests(&config.event));
    all
}
