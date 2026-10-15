use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};
use utoipa::ToSchema;

pub const DOCUMENT_CAP: usize = 8192;
pub const ATTRIBUTE_CAP: usize = 32;

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    #[default]
    Pending,
    Missing,
    Unreachable,
    Error,
}
impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Pending => "pending",
            Self::Missing => "missing",
            Self::Unreachable => "unreachable",
            Self::Error => "error",
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Point {
    pub lat: f64,
    pub lon: f64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct ResourceMeta {
    #[serde(rename = "type")]
    pub type_name: String,
    pub type_path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub url: String,
    pub site: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub attributes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub source: String,
    pub fetched_at: i64,
    pub ttl_secs: u64,
    pub status: Status,
}
impl ResourceMeta {
    pub fn new(uri: &str, source: &str, ttl_secs: u64) -> Self {
        Self {
            type_name: "WebPage".into(),
            type_path: type_path("WebPage"),
            url: uri.into(),
            site: url::Url::parse(uri)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_default(),
            source: source.into(),
            fetched_at: Utc::now().timestamp_millis(),
            ttl_secs,
            status: Status::Ok,
            ..Default::default()
        }
    }
    pub fn set_type(&mut self, name: &str) {
        let path = type_path(name);
        if let Some(leaf) = path.last() {
            self.type_name = leaf.clone();
            self.type_path = path;
        }
    }
    pub fn validate(&self, allowed_attributes: &[&str]) -> Result<()> {
        if self.attributes.len() > ATTRIBUTE_CAP {
            bail!("document exceeds 32 attributes");
        }
        if self
            .attributes
            .keys()
            .any(|k| !allowed_attributes.contains(&k.as_str()))
        {
            bail!("attribute is not allowlisted");
        }
        if self.type_path != type_path(&self.type_name) || self.type_path.is_empty() {
            bail!("invalid schema.org type path");
        }
        if let Some(p) = &self.location {
            if !p.lat.is_finite()
                || !p.lon.is_finite()
                || !(-90.0..=90.0).contains(&p.lat)
                || !(-180.0..=180.0).contains(&p.lon)
            {
                bail!("invalid point");
            }
        }
        // PostgreSQL jsonb adds spaces; leave enough room for its canonical representation.
        if serde_json::to_vec(self)?.len() > DOCUMENT_CAP
            || serde_json::to_string_pretty(self)?.len() > DOCUMENT_CAP
        {
            bail!("document exceeds 8 KB");
        }
        Ok(())
    }
    pub fn bounded(mut self, allowed_attributes: &[&str]) -> Result<Self> {
        self.attributes
            .retain(|k, _| allowed_attributes.contains(&k.as_str()));
        while self.attributes.len() > ATTRIBUTE_CAP {
            self.attributes.pop_last();
        }
        for v in self.attributes.values_mut() {
            truncate(v, 256);
        }
        for (value, max) in [
            (&mut self.name, 512),
            (&mut self.description, 2048),
            (&mut self.category, 128),
        ] {
            if let Some(s) = value {
                truncate(s, max);
            }
        }
        if self.image.as_ref().is_some_and(|s| s.len() > 2048) {
            self.image = None;
        }
        self.aliases.truncate(4);
        while self.validate(allowed_attributes).is_err() {
            if self.attributes.pop_last().is_some() || self.aliases.pop().is_some() {
                continue;
            }
            if self.description.take().is_some() || self.image.take().is_some() {
                continue;
            }
            self.validate(allowed_attributes)?;
        }
        Ok(self)
    }
}
pub fn truncate(s: &mut String, max: usize) {
    if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
}
pub fn type_path(name: &str) -> Vec<String> {
    static TYPES: OnceLock<BTreeMap<String, Vec<String>>> = OnceLock::new();
    let types = TYPES.get_or_init(|| {
        serde_json::from_str(include_str!("../../../data/schemaorg-types.json"))
            .expect("pinned vocabulary")
    });
    let name = name
        .strip_prefix("https://schema.org/")
        .or_else(|| name.strip_prefix("http://schema.org/"))
        .or_else(|| name.strip_prefix("schema:"))
        .unwrap_or(name);
    fn visit(
        name: &str,
        types: &BTreeMap<String, Vec<String>>,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(name.into()) {
            return;
        }
        if let Some(parents) = types.get(name) {
            for p in parents {
                visit(p, types, seen, out);
            }
            out.push(name.into());
        }
    }
    let mut out = vec![];
    visit(name, types, &mut BTreeSet::new(), &mut out);
    out
}
pub fn identity(uri: &str) -> Result<(String, String, String)> {
    if uri.is_empty() || uri.len() > 2048 {
        bail!("URI must be 1–2048 bytes");
    }
    let (uri, scheme) = pubky_resource_id::normalize_uri(uri).map_err(anyhow::Error::msg)?;
    let id = pubky_resource_id::resource_id(&uri);
    Ok((id, uri, scheme))
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalization_is_the_shared_crate() {
        let (id, uri, scheme) =
            identity("HTTPS://user:pass@Example.COM:443/path?q=1#frag").unwrap();
        assert_eq!(id, "2397d03755c83364010ee03600730121");
        assert_eq!(uri, "https://example.com/path?q=1");
        assert_eq!(scheme, "https");
        assert!(identity("no-scheme").is_err());
        assert!(identity(&"x".repeat(2049)).is_err());
    }
    #[test]
    fn hierarchy_keeps_multiple_parents_and_uncommon_types() {
        for (child, parent) in [
            ("CafeOrCoffeeShop", "Place"),
            ("CafeOrCoffeeShop", "Organization"),
            ("MusicEvent", "Event"),
            ("ScholarlyArticle", "CreativeWork"),
        ] {
            let path = type_path(child);
            assert!(path.contains(&parent.into()));
            assert_eq!(path.last().unwrap(), child);
            assert_eq!(path.iter().collect::<BTreeSet<_>>().len(), path.len());
        }
        assert_eq!(
            type_path("https://schema.org/MusicEvent"),
            type_path("MusicEvent")
        );
        assert!(type_path("InventedType").is_empty());
    }
    #[test]
    fn documents_are_bounded() {
        let mut doc = ResourceMeta::new("https://example.org", "web", 100);
        doc.description = Some("é".repeat(9000));
        assert!(doc.validate(&[]).is_err());
        let doc = doc.bounded(&[]).unwrap();
        assert!(serde_json::to_vec(&doc).unwrap().len() < DOCUMENT_CAP);
        let mut doc = doc.clone();
        doc.attributes = (0..33).map(|n| (format!("x{n}"), "v".into())).collect();
        let keys: Vec<_> = doc.attributes.keys().cloned().collect();
        let allowed: Vec<_> = keys.iter().map(String::as_str).collect();
        assert!(doc.validate(&allowed).is_err());
        assert_eq!(doc.bounded(&allowed).unwrap().attributes.len(), 32);
        let mut doc = ResourceMeta::new("https://example.org", "web", 100);
        doc.attributes.insert("secret".into(), "x".into());
        assert!(doc.validate(&[]).is_err());
    }
}
