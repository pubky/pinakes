//! Bounded grouping evidence; it never changes URI identity.
use crate::model::{identity, ResourceMeta};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::LazyLock};
use utoipa::ToSchema;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TenantRule {
    Subdomain,
    FirstPathSegment,
    /// One-based path segment; constant prefixes remain part of the publisher scope.
    PathSegment(usize),
}
pub type TenantRules = BTreeMap<String, TenantRule>;
pub static TENANT_RULES: LazyLock<TenantRules> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../data/multi-tenant-hosts.json"))
        .expect("tenant rules")
});

pub fn valid_stored(key: &GroupKey) -> bool {
    key.key.split_once(':').is_some_and(|(kind, value)| {
        !value.is_empty()
            && matches!(
                kind,
                "url"
                    | "isrc"
                    | "iswc"
                    | "musicbrainz"
                    | "podcast_guid"
                    | "podcast_series_guid"
                    | "wikidata"
                    | "imdb"
                    | "tmdb"
                    | "ics_uid"
                    | "isbn"
                    | "doi"
                    | "osm"
            )
    }) && key.key.len() <= 2100
        && key.basis.len() <= 32
        && key.target.as_ref().is_none_or(|s| s.len() <= 2048)
        && key.confidence.is_finite()
        && (0.0..=1.0).contains(&key.confidence)
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, PartialEq)]
pub struct GroupKey {
    pub key: String,
    pub basis: String,
    pub confidence: f64,
    /// Destination URL (older documents contain a publisher origin). Never a fetch instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}
pub fn add(doc: &mut ResourceMeta, basis: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
        return;
    }
    let (kind, confidence) = match basis {
        "isrc" => ("isrc", 1.0),
        "iswc" => ("iswc", 1.0),
        "musicbrainz" => ("musicbrainz", 1.0),
        "podcast_guid" => ("podcast_guid", 1.0),
        "podcast_series_guid" => ("podcast_series_guid", 1.0),
        "wikidata" => ("wikidata", 0.95),
        "imdb" => ("imdb", 0.95),
        "tmdb" => ("tmdb", 0.95),
        "ics_uid" => ("ics_uid", 0.9),
        "isbn" => ("isbn", 1.0),
        "doi" => ("doi", 1.0),
        "same_as" | "canonical" => ("url", 0.8),
        // Landing pages named by DOI/ISBN registries; only those resolvers emit this basis.
        "registry" => ("url", 0.9),
        _ => return,
    };
    let target = (kind == "url").then(|| value.to_owned());
    let value = if kind == "url" {
        let Ok((id, _, scheme)) = identity(value) else {
            return;
        };
        if !matches!(scheme.as_str(), "http" | "https") {
            return;
        }
        id
    } else if kind == "isbn" {
        let Some(number) = crate::resolver::cards::isbn(&format!("isbn:{value}")) else {
            return;
        };
        if number.len() == 10 {
            // ISBN-10 and its ISBN-13 edition identify the same book independently of
            // any provider URL. Keep this evidence separate from redirect aliases.
            let body = format!("978{}", &number[..9]);
            let sum: u32 = body
                .bytes()
                .enumerate()
                .map(|(i, n)| u32::from(n - b'0') * if i % 2 == 0 { 1 } else { 3 })
                .sum();
            format!("{body}{}", (10 - sum % 10) % 10)
        } else {
            number
        }
    } else if kind == "doi" {
        let Some(number) = crate::resolver::cards::doi(&format!("doi:{value}")) else {
            return;
        };
        number
    } else if matches!(kind, "isrc" | "iswc") {
        value
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_uppercase)
            .collect()
    } else if kind == "musicbrainz" {
        value
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    } else {
        value.to_owned()
    };
    if kind == "musicbrainz"
        && (value.len() != 36
            || !value.bytes().enumerate().all(|(i, c)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            }))
    {
        return;
    }
    if kind == "iswc"
        && (value.len() != 11
            || !value.starts_with('T')
            || !value[1..].bytes().all(|c| c.is_ascii_digit()))
    {
        return;
    }
    if kind == "isrc"
        && (value.len() != 12
            || !value[..2].bytes().all(|b| b.is_ascii_alphabetic())
            || !value[5..].bytes().all(|b| b.is_ascii_digit()))
    {
        return;
    }
    if kind == "wikidata"
        && (!value.starts_with('Q')
            || value.len() < 2
            || !value[1..].bytes().all(|b| b.is_ascii_digit()))
    {
        return;
    }
    if kind == "imdb"
        && (!value.starts_with("tt")
            || value.len() < 3
            || !value[2..].bytes().all(|b| b.is_ascii_digit()))
    {
        return;
    }
    let key = format!("{kind}:{value}");
    if let Some(existing) = doc.group_keys.iter_mut().find(|k| k.key == key) {
        if existing.target.is_none() {
            existing.target = target;
        }
    } else if doc.group_keys.len() < 16 {
        doc.group_keys.push(GroupKey {
            key,
            basis: basis.into(),
            confidence,
            target,
        });
    }
}
pub fn url_keys(doc: &mut ResourceMeta, url: &str) {
    if let Some(number) = url.strip_prefix("isbn:") {
        add(doc, "isbn", number);
        return;
    }
    if let Some(number) = crate::resolver::cards::doi(url) {
        add(doc, "doi", &number);
    }
    if let Ok(u) = url::Url::parse(url) {
        let last = u
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        match u.host_str().unwrap_or("") {
            "wikidata.org" | "www.wikidata.org" => add(doc, "wikidata", last),
            "imdb.com" | "www.imdb.com" => add(doc, "imdb", last),
            "musicbrainz.org" => add(doc, "musicbrainz", u.path()),
            "themoviedb.org" | "www.themoviedb.org" => {
                let mut parts = u.path().trim_matches('/').split('/');
                if let (Some(kind), Some(id)) = (parts.next(), parts.next()) {
                    let id = id.split('-').next().unwrap_or("");
                    if matches!(kind, "movie" | "tv")
                        && !id.is_empty()
                        && id.bytes().all(|c| c.is_ascii_digit())
                    {
                        add(doc, "tmdb", &format!("{kind}/{id}"));
                    }
                }
            }
            _ => {}
        }
    }
}
pub fn jsonld(doc: &mut ResourceMeta, v: &Value, base: &url::Url) {
    for (field, basis) in [
        ("isrcCode", "isrc"),
        ("iswcCode", "iswc"),
        ("identifier", "identifier"),
        ("sameAs", "same_as"),
    ] {
        let Some(value) = v.get(field) else {
            continue;
        };
        let values = value
            .as_array()
            .map(|a| a.iter().take(16).collect::<Vec<_>>())
            .unwrap_or_else(|| vec![value]);
        for value in values {
            if basis == "identifier" {
                let name = value["propertyID"].as_str().unwrap_or("").to_lowercase();
                if let Some(s) = value["value"].as_str() {
                    let basis = match name.as_str() {
                        "isrc" => "isrc",
                        "iswc" => "iswc",
                        "musicbrainz" | "musicbrainz id" => "musicbrainz",
                        "wikidata" => "wikidata",
                        "imdb" => "imdb",
                        "tmdb" => "tmdb",
                        "guid" | "podcast guid" => "podcast_guid",
                        "uid" | "ics uid" => "ics_uid",
                        _ => continue,
                    };
                    add(doc, basis, s);
                } else if let Some(s) = value.as_str() {
                    url_keys(doc, s);
                }
            } else if let Some(s) = value.as_str() {
                if basis == "same_as" {
                    if let Some(url) = crate::resolver::web::origin_url(base, s) {
                        url_keys(doc, &url);
                        add(doc, basis, &url);
                    }
                } else {
                    add(doc, basis, s);
                }
            }
        }
    }
}

/// The PSL includes private suffixes (e.g. separate hosted-blog tenants).
pub fn origin(uri: &str) -> Option<String> {
    origin_with(uri, &TENANT_RULES)
}
pub fn origin_with(uri: &str, rules: &TenantRules) -> Option<String> {
    let u = url::Url::parse(uri).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if let Some((_, rule)) = rules
        .iter()
        .filter(|(base, _)| host == **base || host.ends_with(&format!(".{base}")))
        .max_by_key(|(base, _)| base.len())
    {
        return Some(match rule {
            TenantRule::Subdomain => host,
            TenantRule::FirstPathSegment | TenantRule::PathSegment(_) => {
                let n = match rule {
                    TenantRule::PathSegment(n) => *n,
                    _ => 1,
                };
                let path: Vec<_> = u
                    .path_segments()?
                    .filter(|s| !s.is_empty())
                    .take(n)
                    .collect();
                if path.len() < n {
                    format!("{host}/unscoped:{}", pubky_resource_id::resource_id(uri))
                } else {
                    format!("{host}/{}", path.join("/"))
                }
            }
        });
    }
    Some(psl::domain_str(&host).unwrap_or(&host).to_owned())
}
pub fn same_origin(a: &str, b: &str) -> bool {
    origin(a).zip(origin(b)).is_some_and(|(a, b)| a == b)
}
