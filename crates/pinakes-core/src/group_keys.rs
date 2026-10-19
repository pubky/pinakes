//! Bounded grouping evidence and publisher origins; they never change URI identity.
use crate::model::{identity, ResourceMeta};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::LazyLock};
use utoipa::ToSchema;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TenantRule {
    Subdomain,
    FirstPathSegment,
    /// One-based path segment; constant prefixes remain part of the publisher scope.
    PathSegment(usize),
    /// A path segment after removing noise windows (`skip`, e.g. `u/*` in
    /// `/calendar/u/0/…`); a tenant segment listed in `extend` takes the next one too
    /// (anchor.fm `/s/{id}` feeds beside `/{show}` pages); without enough segments, the
    /// first listed query parameter present scopes the tenant (`embed?src=`).
    Path(PathRule),
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub segment: usize,
    #[serde(default)]
    pub skip: Vec<String>,
    #[serde(default)]
    pub extend: Vec<String>,
    #[serde(default)]
    pub query: Vec<String>,
}
impl PathRule {
    pub fn valid(&self) -> bool {
        (1..=8).contains(&self.segment)
            && self.skip.len() <= 8
            && self.extend.len() <= 32
            && self.query.len() <= 8
            && self
                .skip
                .iter()
                .all(|p| !p.is_empty() && p.len() <= 64 && p.split('/').all(|s| !s.is_empty()))
            && self
                .extend
                .iter()
                .chain(&self.query)
                .all(|v| !v.is_empty() && v.len() <= 64 && !v.contains('/'))
    }
    fn scope(&self, host: &str, u: &url::Url, uri: &str) -> String {
        let mut path: Vec<&str> = u
            .path_segments()
            .map(|s| s.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        for pattern in &self.skip {
            let pattern: Vec<_> = pattern.split('/').collect();
            if let Some(at) = (0..path.len().saturating_sub(pattern.len() - 1)).find(|&i| {
                pattern
                    .iter()
                    .zip(&path[i..])
                    .all(|(p, s)| *p == "*" || p == s)
            }) {
                path.drain(at..at + pattern.len());
            }
        }
        let mut n = self.segment;
        if path.len() >= n && self.extend.iter().any(|e| e == path[n - 1]) {
            n += 1;
        }
        if path.len() >= n {
            return format!("{host}/{}", path[..n].join("/"));
        }
        for name in &self.query {
            if let Some((_, v)) = u.query_pairs().find(|(k, v)| k == name && !v.is_empty()) {
                return format!("{host}/?{name}={v}");
            }
        }
        format!("{host}/unscoped:{}", crate::identity::resource_id(uri))
    }
}
pub type TenantRules = BTreeMap<String, TenantRule>;
pub static TENANT_RULES: LazyLock<TenantRules> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../data/multi-tenant-hosts.json"))
        .expect("tenant rules")
});

/// Grouping evidence a document records. Only evidence that cannot be spoofed groups (`groups`):
/// an identifier card's own `doi`/`isbn` and the landing pages its registry names (`registry`),
/// a page's head citation of an identifier (`cites_doi`, `cites_isbn`), and an ICS UID
/// (`ics_uid`).
pub fn valid_stored(key: &GroupKey) -> bool {
    key.key.split_once(':').is_some_and(|(kind, value)| {
        !value.is_empty() && matches!(kind, "url" | "ics_uid" | "isbn" | "doi")
    }) && key.key.len() <= 2100
        && matches!(
            key.basis.as_str(),
            "doi" | "isbn" | "registry" | "cites_doi" | "cites_isbn" | "ics_uid"
        )
        && key.target.as_ref().is_none_or(|s| s.len() <= 2048)
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, PartialEq)]
pub struct GroupKey {
    pub key: String,
    pub basis: String,
    /// A registry key's landing page URL. Never a fetch instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}
pub fn add(doc: &mut ResourceMeta, basis: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
        return;
    }
    let kind = match basis {
        "doi" | "cites_doi" => "doi",
        "isbn" | "cites_isbn" => "isbn",
        "ics_uid" => "ics_uid",
        // Landing pages named by DOI/ISBN registries; only those resolvers emit this basis.
        "registry" => "url",
        _ => return,
    };
    let target = (kind == "url").then(|| value.to_owned());
    let value = match kind {
        "url" => {
            let Ok((id, _, scheme)) = identity(value) else {
                return;
            };
            if !matches!(scheme.as_str(), "http" | "https") {
                return;
            }
            id
        }
        "isbn" => {
            let Some(number) = crate::resolver::cards::isbn(&format!("isbn:{value}")) else {
                return;
            };
            crate::resolver::cards::isbn13(&number)
        }
        "doi" => {
            let Some(number) = crate::resolver::cards::doi(&format!("doi:{value}")) else {
                return;
            };
            number
        }
        _ => value.to_owned(),
    };
    let key = format!("{kind}:{value}");
    if !doc
        .group_keys
        .iter()
        .any(|k| k.key == key && k.basis == basis)
        && doc.group_keys.len() < 16
    {
        doc.group_keys.push(GroupKey {
            key,
            basis: basis.into(),
            target,
        });
    }
}

/// The PSL includes private suffixes (e.g. separate hosted-blog tenants).
/// The publisher of a URL: its registrable domain, or a tenant of a multi-tenant host
/// (`data/multi-tenant-hosts.json`).
pub fn origin(uri: &str) -> Option<String> {
    let u = url::Url::parse(uri).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if let Some((_, rule)) = TENANT_RULES
        .iter()
        .filter(|(base, _)| host == **base || host.ends_with(&format!(".{base}")))
        .max_by_key(|(base, _)| base.len())
    {
        return Some(match rule {
            TenantRule::Subdomain => host,
            TenantRule::Path(rule) => rule.scope(&host, &u, uri),
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
                    format!("{host}/unscoped:{}", crate::identity::resource_id(uri))
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
