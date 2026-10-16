//! Generic media/identifier cards. Only metadata is retained; embeds and media are never served.
use super::{
    egress::{Egress, FetchOptions, Fetched},
    web, Resolver,
};
use crate::{
    adapter::Manifest,
    config::{Config, ResolverConfig},
    model::{ResourceMeta, Status},
};
use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use url::Url;

pub const KINDS: [&str; 5] = ["media", "isbn", "doi", "nostr", "activitypub"];
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CardConfig {
    pub media: ResolverConfig,
    pub isbn: ResolverConfig,
    pub doi: ResolverConfig,
    pub nostr: ResolverConfig,
    pub activitypub: ResolverConfig,
    pub openlibrary_url: String,
    pub doi_url: String,
    pub podcast_items: usize,
}
impl Default for CardConfig {
    fn default() -> Self {
        Self {
            media: Default::default(),
            isbn: Default::default(),
            doi: Default::default(),
            nostr: Default::default(),
            activitypub: Default::default(),
            openlibrary_url: "https://openlibrary.org/api/books".into(),
            doi_url: "https://doi.org/".into(),
            podcast_items: 1000,
        }
    }
}
impl CardConfig {
    pub fn resolver(&self, kind: &str) -> &ResolverConfig {
        match kind {
            "isbn" => &self.isbn,
            "doi" => &self.doi,
            "nostr" => &self.nostr,
            "activitypub" => &self.activitypub,
            _ => &self.media,
        }
    }
    pub fn validate(&self) -> Result<()> {
        for kind in KINDS {
            let c = self.resolver(kind);
            ensure!(
                c.concurrency > 0
                    && c.concurrency <= 32
                    && c.requests_per_second.is_finite()
                    && c.requests_per_second > 0.0
                    && c.requests_per_second <= 100.0
                    && (3600..=31536000).contains(&c.ttl_secs),
                "invalid card resolver configuration"
            );
        }
        for uri in [&self.openlibrary_url, &self.doi_url] {
            let u = Url::parse(uri)?;
            ensure!(
                matches!(u.scheme(), "http" | "https")
                    && u.username().is_empty()
                    && u.password().is_none(),
                "invalid identifier endpoint"
            );
        }
        ensure!(
            (1..=10000).contains(&self.podcast_items),
            "invalid podcast item cap"
        );
        Ok(())
    }
}
pub fn manifests(c: &CardConfig) -> Vec<Manifest> {
    KINDS
        .into_iter()
        .map(|kind| {
            let mut m = crate::adapter::manifest(kind);
            m.enabled = c.resolver(kind).enabled;
            m.disabled_reason = (!m.enabled).then(|| "disabled by configuration".into());
            m.expected_fields = vec!["name".into()];
            m.canary = match kind {
                "media" => Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
                "isbn" => Some("isbn:9780140328721"),
                "doi" => Some("doi:10.1038/nphys1170"),
                _ => None,
            }
            .map(|s| s.parse().unwrap());
            m
        })
        .collect()
}
#[derive(Deserialize)]
struct Provider {
    endpoints: Vec<Endpoint>,
}
#[derive(Deserialize)]
struct Endpoint {
    url: String,
    #[serde(default)]
    schemes: Vec<String>,
    formats: Option<Vec<String>>,
}
fn providers() -> &'static Vec<Provider> {
    static P: OnceLock<Vec<Provider>> = OnceLock::new();
    P.get_or_init(|| {
        serde_json::from_str(include_str!("../../../../data/oembed-providers.json"))
            .expect("pinned providers")
    })
}
fn wildcard(pattern: &str, value: &str) -> bool {
    // Registry wildcards only. Linear greedy glob, no regex or untrusted expression evaluation.
    let (p, v) = (pattern.as_bytes(), value.as_bytes());
    let (mut i, mut j, mut star, mut back) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && p[i] == v[j] {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            i += 1;
            back = j;
        } else if let Some(s) = star {
            back += 1;
            j = back;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}
fn scheme_matches(pattern: &str, target: &Url) -> bool {
    let Some((scheme, rest)) = pattern.split_once("://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    // A registry host wildcard never consumes a path or a query string.
    scheme == target.scheme()
        && wildcard(
            host,
            &target[url::Position::BeforeHost..url::Position::AfterPort],
        )
        && wildcard(pattern, target.as_str())
}
pub fn provider_endpoint(uri: &str) -> Option<Url> {
    let target = Url::parse(uri).ok()?;
    if !matches!(target.scheme(), "http" | "https")
        || !target.username().is_empty()
        || target.password().is_some()
    {
        return None;
    }
    for p in providers() {
        for e in &p.endpoints {
            if e.formats
                .as_ref()
                .is_some_and(|v| !v.iter().any(|s| s == "json"))
            {
                continue;
            }
            if e.schemes
                .iter()
                .any(|pattern| scheme_matches(pattern, &target))
            {
                let mut u = Url::parse(&e.url.replace("{format}", "json")).ok()?;
                if u.scheme() != "https" {
                    continue;
                }
                u.query_pairs_mut()
                    .append_pair("url", target.as_str())
                    .append_pair("format", "json");
                return Some(u);
            }
        }
    }
    None
}
pub fn isbn(uri: &str) -> Option<String> {
    let s = uri
        .strip_prefix("isbn:")?
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .collect::<String>()
        .to_uppercase();
    let digits: Option<Vec<u32>> = s
        .chars()
        .enumerate()
        .map(|(i, c)| {
            if c == 'X' && i == 9 && s.len() == 10 {
                Some(10)
            } else {
                c.to_digit(10)
            }
        })
        .collect();
    let d = digits?;
    let valid = match d.len() {
        10 => {
            d.iter()
                .enumerate()
                .map(|(i, d)| (10 - i as u32) * d)
                .sum::<u32>()
                % 11
                == 0
        }
        13 => {
            d.iter()
                .enumerate()
                .map(|(i, d)| d * if i % 2 == 0 { 1 } else { 3 })
                .sum::<u32>()
                % 10
                == 0
                && (s.starts_with("978") || s.starts_with("979"))
        }
        _ => false,
    };
    valid.then_some(s)
}
pub fn doi(uri: &str) -> Option<String> {
    let raw = if let Some(s) = uri.strip_prefix("doi:") {
        s.to_owned()
    } else {
        let u = Url::parse(uri).ok()?;
        if !matches!(u.host_str(), Some("doi.org" | "dx.doi.org")) {
            return None;
        }
        percent_encoding::percent_decode_str(u.path().trim_start_matches('/'))
            .decode_utf8()
            .ok()?
            .into_owned()
    };
    let (prefix, suffix) = raw.split_once('/')?;
    (prefix.starts_with("10.")
        && (4..=9).contains(&prefix[3..].len())
        && prefix[3..].bytes().all(|b| b.is_ascii_digit())
        && !suffix.is_empty()
        && raw.len() <= 512
        && !raw.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then(|| raw.to_lowercase())
}
pub fn classify(uri: &str) -> Option<&'static str> {
    if uri.starts_with("isbn:") {
        return Some("isbn");
    }
    if uri.starts_with("doi:") || doi(uri).is_some() {
        return Some("doi");
    }
    if uri.starts_with("nostr:")
        || crate::event::adapters::detect(uri).is_some_and(|d| d.id == "nip52")
    {
        let target = crate::event::adapters::nostr::Target::decode(uri).ok()?;
        return if target.kind.is_some_and(|k| (31922..=31925).contains(&k)) {
            None
        } else {
            Some("nostr")
        };
    }
    if provider_endpoint(uri).is_some()
        && !crate::event::adapters::detect(uri).is_some_and(|d| d.id != "ics")
    {
        return Some("media");
    }
    None
}
pub struct CardResolver {
    pub kind: &'static str,
    pub config: Arc<Config>,
    pub web: web::WebResolver,
}
#[async_trait]
impl Resolver for CardResolver {
    fn name(&self) -> &'static str {
        self.kind
    }
    fn manifest(&self) -> Manifest {
        manifests(&self.config.cards)
            .into_iter()
            .find(|m| m.source == self.kind)
            .unwrap()
    }
    fn matches(&self, uri: &str) -> bool {
        classify(uri) == Some(self.kind)
    }
    fn canary_allowed(&self) -> bool {
        let Some(canary) = self.manifest().canary else {
            return true;
        };
        let endpoint = match self.kind {
            "isbn" => self.config.cards.openlibrary_url.clone(),
            "doi" => self.config.cards.doi_url.clone(),
            "media" => provider_endpoint(canary.as_str())
                .map(|u| u.to_string())
                .unwrap_or_else(|| canary.to_string()),
            _ => canary.to_string(),
        };
        self.web.egress.allows(&endpoint)
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
        let ttl = self.config.cards.resolver(self.kind).ttl_secs;
        let budget = if self.kind == "nostr" {
            crate::queue::RESOLVE_TIMEOUT - Duration::from_secs(1)
        } else {
            Duration::from_secs(10)
        };
        let result = tokio::time::timeout(budget, async {
            match self.kind {
                "isbn" => {
                    let Some(number) = isbn(uri) else { return Ok(missing(uri,"isbn",0)); };
                    let mut url = Url::parse(&self.config.cards.openlibrary_url)?;
                    url.query_pairs_mut()
                        .append_pair("bibkeys", &format!("ISBN:{number}"))
                        .append_pair("format", "json")
                        .append_pair("jscmd", "data");
                    let f = self.web.egress.fetch(url.as_str()).await?;
                    let uri = uri.to_owned();
                    tokio::task::spawn_blocking(move || parse_isbn(&uri, &number, &f, ttl)).await?
                }
                "doi" => {
                    let Some(number) = doi(uri) else { return Ok(missing(uri,"doi",0)); };
                    let mut url = Url::parse(&self.config.cards.doi_url)?;
                    url.set_path(&format!(
                        "{}{}",
                        url.path().trim_end_matches('/').to_owned() + "/",
                        number
                    ));
                    let f = self
                        .web
                        .egress
                        .fetch_with(
                            url.as_str(),
                            &FetchOptions {
                                accept: Some("application/vnd.citationstyles.csl+json".into()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    let uri = uri.to_owned();
                    tokio::task::spawn_blocking(move || parse_doi(&uri, &f, ttl)).await?
                }
                "nostr" => {
                    let target = crate::event::adapters::nostr::Target::decode(uri)?;
                    let event = crate::event::adapters::nostr::lookup(
                        &target,
                        &self.web.egress,
                        &self.config.event,
                    )
                    .await?;
                    let Some(event) = event else {
                        return Ok(missing(uri, "nostr", ttl));
                    };
                    if matches!(event.kind.as_u16(), 31922 | 31923) {
                        if !self.config.event.enabled {
                            return Err(super::RetryLater { class: "disabled", seconds: 3600 }.into());
                        }
                        return crate::event::adapters::nostr::parse(
                            uri,
                            &event,
                            &self.config.event,
                        );
                    }
                    let uri=uri.to_owned();
                    tokio::task::spawn_blocking(move || parse_nostr(&uri, &event, ttl)).await?
                }
                "media" => self.media(uri,ttl).await,
                "activitypub" => {
                    let f=self.web.egress.fetch_with(uri,&FetchOptions{accept:Some("application/activity+json, application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"".into()),..Default::default()}).await?;
                    if (200..300).contains(&f.status) { if let Some(d)=parse_activitypub(uri,&f,ttl).await? {return Ok(d);} }
                    self.web.resolve(uri).await
                }
                _ => self.web.resolve(uri).await,
            }
        })
        .await.map_err(|_| super::RetryLater { class: "timeout", seconds: 60 })??;
        crate::telemetry::document(crate::telemetry::last_fetch(), &self.manifest(), &result);
        Ok(result)
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        Duration::from_secs(doc.ttl_secs)
    }
}
fn missing(uri: &str, source: &str, ttl: u64) -> ResourceMeta {
    let mut d = ResourceMeta::new(uri, source, ttl);
    d.status = Status::Missing;
    d
}
fn retry_status(f: &Fetched) -> Result<()> {
    if f.status >= 500 || matches!(f.status, 408 | 429) {
        return Err(super::RetryLater {
            class: "upstream",
            seconds: f.retry_after_secs.max(60),
        }
        .into());
    }
    Ok(())
}
fn check_status(uri: &str, source: &str, f: &Fetched, ttl: u64) -> Result<Option<ResourceMeta>> {
    retry_status(f)?;
    match f.status {
        200..=299 => Ok(None),
        404 | 410 => Ok(Some(missing(uri, source, ttl))),
        401 | 403 | 451 => {
            let mut d = ResourceMeta::new(uri, source, ttl);
            d.status = Status::Unreachable;
            Ok(Some(d))
        }
        _ => bail!("identifier upstream failed"),
    }
}
fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}
fn names(v: Option<&Value>) -> Option<String> {
    let a = v?.as_array()?;
    let out = a
        .iter()
        .take(8)
        .filter_map(|v| {
            text(v.get("name")).or_else(|| {
                let family = v.get("family")?.as_str()?;
                Some(
                    format!("{} {family}", v["given"].as_str().unwrap_or(""))
                        .trim()
                        .into(),
                )
            })
        })
        .collect::<Vec<_>>()
        .join(", ");
    (!out.is_empty()).then_some(out)
}
pub fn parse_isbn(uri: &str, number: &str, f: &Fetched, ttl: u64) -> Result<ResourceMeta> {
    if let Some(d) = check_status(uri, "isbn", f, ttl)? {
        return Ok(d);
    }
    let v: Value = serde_json::from_slice(&f.body)?;
    let Some(v) = v.get(format!("ISBN:{number}")) else {
        return Ok(missing(uri, "isbn", ttl));
    };
    let mut d = ResourceMeta::new(uri, "isbn", ttl);
    d.set_type("Book");
    d.name = text(v.get("title"));
    d.image = v
        .pointer("/cover/medium")
        .and_then(Value::as_str)
        .and_then(|s| web::origin_url(&f.url, s));
    for (k, v) in [
        ("author", names(v.get("authors"))),
        ("publisher", names(v.get("publishers"))),
        ("published", text(v.get("publish_date"))),
        ("isbn", Some(number.into())),
    ] {
        if let Some(v) = v {
            d.attributes.insert(k.into(), v);
        }
    }
    if let Some(url) = v["url"].as_str().and_then(|s| web::origin_url(&f.url, s)) {
        d.aliases.push(url);
    }
    d.bounded(web::ATTRIBUTES)
}
pub fn parse_doi(uri: &str, f: &Fetched, ttl: u64) -> Result<ResourceMeta> {
    if let Some(d) = check_status(uri, "doi", f, ttl)? {
        return Ok(d);
    }
    let v: Value = serde_json::from_slice(&f.body)?;
    let mut d = ResourceMeta::new(uri, "doi", ttl);
    d.set_type(if v["type"] == "book" {
        "Book"
    } else {
        "ScholarlyArticle"
    });
    d.name = text(v.get("title"));
    d.description = text(v.get("abstract")).map(|s| plain(&s));
    for (k, v) in [
        ("author", names(v.get("author"))),
        ("publisher", text(v.get("publisher"))),
        ("doi", doi(uri)),
        (
            "container",
            text(v.get("container-title")).or_else(|| {
                v["container-title"]
                    .as_array()
                    .and_then(|a| text(a.first()))
            }),
        ),
        ("published", csl_date(&v)),
    ] {
        if let Some(v) = v {
            d.attributes.insert(k.into(), v);
        }
    }
    if let Some(number) = doi(uri) {
        d.aliases.push(format!("https://doi.org/{number}"));
    }
    d.bounded(web::ATTRIBUTES)
}
pub fn parse_nostr(uri: &str, e: &nostr::event::Event, ttl: u64) -> Result<ResourceMeta> {
    e.verify()?;
    let mut d = ResourceMeta::new(uri, "nostr", ttl);
    d.attributes.insert("author".into(), e.pubkey.to_hex());
    match e.kind.as_u16() {
        0 => {
            let v: Value = serde_json::from_str(&e.content)?;
            d.set_type("Person");
            d.name = text(v.get("display_name")).or_else(|| text(v.get("name")));
            d.description = text(v.get("about"));
            d.image = v["picture"]
                .as_str()
                .and_then(|s| Url::parse(s).ok())
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .map(|u| u.to_string());
        }
        1 | 30023 => {
            d.set_type(if e.kind.as_u16() == 1 {
                "SocialMediaPosting"
            } else {
                "Article"
            });
            d.name = e
                .tags
                .iter()
                .find_map(|t| {
                    let a = t.as_slice();
                    (a.first().is_some_and(|s| s == "title"))
                        .then(|| a.get(1).cloned())
                        .flatten()
                })
                .or_else(|| Some(e.content.chars().take(120).collect()));
            d.description = Some(e.content.chars().take(200).collect());
            d.ttl_secs = ttl.min(3600);
        }
        _ => {
            d.set_type("CreativeWork");
            d.name = Some(format!("Nostr event (kind {})", e.kind.as_u16()));
            d.description = Some(e.content.chars().take(200).collect());
            d.ttl_secs = ttl.min(3600);
        }
    }
    d.bounded(web::ATTRIBUTES)
}
pub fn plain(s: &str) -> String {
    scraper::Html::parse_fragment(s)
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
}
pub fn activitypub(uri: &str, v: &Value, base: &Url, ttl: u64) -> Option<ResourceMeta> {
    let kind = match v["type"].as_str()? {
        "Note" => "SocialMediaPosting",
        "Article" => "Article",
        "Person" => "Person",
        _ => return None,
    };
    let id = v["id"].as_str()?;
    if !crate::group_keys::same_origin(uri, base.as_str())
        || !crate::group_keys::same_origin(uri, id)
    {
        return None;
    }
    let mut d = ResourceMeta::new(uri, "activitypub", ttl);
    d.aliases.push(id.into());
    if matches!(kind, "SocialMediaPosting" | "Article") {
        d.ttl_secs = ttl.min(3600);
        let public = "https://www.w3.org/ns/activitystreams#Public";
        if (v.get("to").is_some() || v.get("cc").is_some())
            && !(v["to"] == public
                || v["to"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x == public)))
        {
            d.name = Some(base.host_str().unwrap_or("ActivityPub object").into());
            d.description = Some(String::new());
            return Some(d);
        }
    }
    d.set_type(kind);
    d.name = text(v.get("name")).or_else(|| text(v.get("preferredUsername")));
    d.description = text(v.get("summary"))
        .or_else(|| text(v.get("content")))
        .map(|s| plain(&s));
    if matches!(kind, "SocialMediaPosting" | "Article") {
        d.description = d.description.map(|s| s.chars().take(200).collect());
    }
    if d.name.is_none() {
        d.name = d
            .description
            .as_ref()
            .map(|s| s.chars().take(120).collect());
    }
    d.image = v
        .pointer("/icon/url")
        .or_else(|| v.pointer("/image/url"))
        .and_then(Value::as_str)
        .and_then(|s| web::origin_url(base, s));
    if let Some(author) = v["attributedTo"].as_str() {
        d.attributes.insert("author".into(), author.into());
    }
    Some(d)
}
async fn parse_activitypub(uri: &str, f: &Fetched, ttl: u64) -> Result<Option<ResourceMeta>> {
    let (uri, f) = (uri.to_owned(), f.clone());
    tokio::task::spawn_blocking(move || {
        let Ok(v) = serde_json::from_slice::<Value>(&f.body) else {
            return Ok(None);
        };
        activitypub(&uri, &v, &f.url, ttl)
            .map(|d| d.bounded(web::ATTRIBUTES))
            .transpose()
    })
    .await?
}
pub async fn enrich(
    uri: &str,
    f: &Fetched,
    mut doc: ResourceMeta,
    egress: &Egress,
    cfg: &CardConfig,
) -> Result<ResourceMeta> {
    if doc.status != Status::Ok || doc.type_path.iter().any(|s| s == "Event") {
        return Ok(doc);
    }
    if cfg.activitypub.enabled {
        if let Some(d) = parse_activitypub(uri, f, cfg.activitypub.ttl_secs).await? {
            return Ok(d);
        }
    }
    // One bounded parse for advertised metadata endpoints. Never follow arbitrary links.
    let (body, base) = (f.body.clone(), f.url.clone());
    let (ap, rss) = tokio::task::spawn_blocking(move || {
        let html = scraper::Html::parse_document(&String::from_utf8_lossy(&body));
        let (mut ap, mut rss) = (None, None);
        for el in html
            .select(&scraper::Selector::parse("link[rel='alternate']").unwrap())
            .take(64)
        {
            let Some(ty) = el.value().attr("type") else {
                continue;
            };
            let Some(href) = el
                .value()
                .attr("href")
                .and_then(|s| web::origin_url(&base, s))
            else {
                continue;
            };
            if ap.is_none()
                && (ty.contains("activity+json")
                    || (ty.contains("ld+json") && ty.contains("activitystreams")))
            {
                ap = Some(href.clone());
            }
            if rss.is_none() && ty.contains("rss+xml") {
                rss = Some(href);
            }
        }
        (ap, rss)
    })
    .await?;
    let ap = ap.or_else(|| {
        crate::event::service::header_alternate(f.validators.link.as_deref(), &f.url).map(|v| v.0)
    });
    if let Some(url) =
        ap.filter(|url| cfg.activitypub.enabled && crate::group_keys::same_origin(uri, url))
    {
        if let Ok(Ok(f)) = tokio::time::timeout(
            Duration::from_secs(2),
            egress.discover(
                &url,
                &FetchOptions {
                    accept: Some("application/activity+json".into()),
                    ..Default::default()
                },
                cfg.activitypub.ttl_secs,
            ),
        )
        .await
        {
            if f.status == 200 {
                if let Some(d) = parse_activitypub(uri, &f, cfg.activitypub.ttl_secs).await? {
                    doc = d;
                }
            }
        }
    }
    if cfg.media.enabled && doc.source != "activitypub" {
        let direct = f.content_type.contains("xml") || f.content_type.contains("rss");
        let feed = if direct {
            Some(f.clone())
        } else if let Some(url) = rss {
            tokio::time::timeout(
                Duration::from_secs(2),
                egress.discover(&url, &Default::default(), cfg.media.ttl_secs),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .filter(|f| f.status == 200)
        } else {
            None
        };
        if let Some(f) = feed {
            let (uri, cap, ttl) = (uri.to_owned(), cfg.podcast_items, cfg.media.ttl_secs);
            if let Ok(Some(d)) =
                tokio::task::spawn_blocking(move || podcast_body(&uri, &f, ttl, cap, direct))
                    .await?
            {
                // A multi-episode feed must not replace an episode page with the whole series.
                if direct || d.type_name == "PodcastEpisode" {
                    doc = d;
                }
            }
        }
    }
    doc.bounded(web::ATTRIBUTES)
}
pub fn podcast(uri: &str, f: &Fetched, ttl: u64, cap: usize) -> Result<Option<ResourceMeta>> {
    let direct = crate::model::identity(uri).ok().map(|v| v.0)
        == crate::model::identity(f.url.as_str()).ok().map(|v| v.0);
    podcast_body(uri, f, ttl, cap, direct)
}
fn podcast_body(
    uri: &str,
    f: &Fetched,
    ttl: u64,
    cap: usize,
    direct: bool,
) -> Result<Option<ResourceMeta>> {
    let body = std::str::from_utf8(&f.body)?;
    let xml = roxmltree::Document::parse(body)?;
    let items: Vec<_> = xml
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "item")
        .take(cap + 1)
        .collect();
    ensure!(items.len() <= cap, "podcast item cap");
    let Some(channel) = xml.descendants().find(|n| n.has_tag_name("channel")) else {
        return Ok(None);
    };
    let podcast_namespace = channel.namespaces().any(|ns| {
        matches!(
            ns.uri().trim_end_matches('/'),
            "http://www.itunes.com/dtds/podcast-1.0.dtd"
                | "https://www.itunes.com/dtds/podcast-1.0.dtd"
                | "https://podcastindex.org/namespace/1.0"
                | "http://podcastindex.org/namespace/1.0"
        )
    });
    let enclosure = |n: roxmltree::Node<'_, '_>| {
        n.children().any(|c| {
            c.has_tag_name("enclosure")
                && c.attribute("type")
                    .is_some_and(|ty| ty.starts_with("audio/") || ty.starts_with("video/"))
        })
    };
    let field = |n: roxmltree::Node<'_, '_>, name: &str| {
        n.children()
            .find(|c| c.is_element() && c.tag_name().name() == name)
            .and_then(|c| c.text())
            .map(str::to_owned)
    };
    let guid = Url::parse(uri)?
        .query_pairs()
        .find(|(k, _)| k == "guid")
        .map(|(_, v)| v.into_owned());
    let chosen = items
        .iter()
        .find(|n| {
            guid.as_ref()
                .is_some_and(|g| field(**n, "guid").as_ref() == Some(g))
                || field(**n, "link").is_some_and(|u| {
                    crate::model::identity(&u).ok().map(|v| v.0)
                        == crate::model::identity(uri).ok().map(|v| v.0)
                })
        })
        .copied();
    let mut d = ResourceMeta::new(uri, "media", ttl);
    d.attributes
        .insert("podcast_feed".into(), f.url.to_string());
    let is_podcast = podcast_namespace
        || chosen
            .map(enclosure)
            .unwrap_or_else(|| items.iter().any(|n| enclosure(*n)));
    if !is_podcast {
        if !direct {
            return Ok(None);
        }
        d.source = "web".into();
        d.set_type("DataFeed");
        d.name = field(channel, "title");
        return Ok(Some(d.bounded(web::ATTRIBUTES)?));
    }
    if let Some(item) = chosen.filter(|_| !direct || guid.is_some()) {
        let Some(guid) = field(item, "guid") else {
            return Ok(None);
        };
        d.set_type("PodcastEpisode");
        d.name = field(item, "title");
        d.description = field(item, "description").map(|s| plain(&s));
        if let Some(duration) = field(item, "duration") {
            d.attributes.insert("duration".into(), duration);
        }
        d.attributes.insert("podcast_guid".into(), guid.clone());
        crate::group_keys::add(&mut d, "podcast_guid", &guid);
        if let Some(link) = field(item, "link").and_then(|u| web::origin_url(&f.url, &u)) {
            d.aliases.push(link);
        }
        d.image = item
            .children()
            .find(|n| n.tag_name().name() == "image")
            .and_then(|n| n.attribute("href"))
            .and_then(|s| web::origin_url(&f.url, s));
    } else {
        if guid.is_some() {
            return Ok(Some(missing(uri, "media", ttl)));
        }
        d.set_type("PodcastSeries");
        if let Some(guid) = xml
            .descendants()
            .find(|n| {
                n.is_element()
                    && n.tag_name().name() == "guid"
                    && n.tag_name()
                        .namespace()
                        .is_some_and(|s| s.contains("podcast"))
            })
            .and_then(|n| n.text())
        {
            // Distinct key namespace: a channel GUID must never merge its episodes.
            crate::group_keys::add(&mut d, "podcast_series_guid", guid);
        }
        d.name = xml
            .descendants()
            .find(|n| n.has_tag_name("channel"))
            .and_then(|n| field(n, "title"));
    }
    Ok(Some(d.bounded(web::ATTRIBUTES)?))
}

pub fn invalid_identifier(uri: &str) -> bool {
    (uri.starts_with("isbn:") && isbn(uri).is_none())
        || (uri.starts_with("doi:") && doi(uri).is_none())
}
fn csl_date(v: &Value) -> Option<String> {
    let parts = v.pointer("/issued/date-parts/0")?.as_array()?;
    let year = i32::try_from(parts.first()?.as_i64()?).ok()?;
    let month = u32::try_from(parts.get(1).and_then(Value::as_u64).unwrap_or(1)).ok()?;
    let day = u32::try_from(parts.get(2).and_then(Value::as_u64).unwrap_or(1)).ok()?;
    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    Some(
        date.format(match parts.len() {
            1 => "%Y",
            2 => "%Y-%m",
            _ => "%Y-%m-%d",
        })
        .to_string(),
    )
}
pub fn social(doc: &ResourceMeta) -> bool {
    matches!(doc.source.as_str(), "nostr" | "activitypub")
        && matches!(
            doc.type_name.as_str(),
            "SocialMediaPosting" | "Article" | "CreativeWork"
        )
}
impl CardResolver {
    async fn media(&self, uri: &str, ttl: u64) -> Result<ResourceMeta> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(9);
        let mut embed = None;
        if let Some(endpoint) = provider_endpoint(uri) {
            if let Ok(Ok(f)) = tokio::time::timeout(
                Duration::from_secs(3),
                self.web
                    .egress
                    .discover(endpoint.as_str(), &Default::default(), ttl),
            )
            .await
            {
                if f.status == 200 {
                    if let Ok(v) = serde_json::from_slice::<Value>(&f.body) {
                        let mut d = ResourceMeta::new(uri, "web", ttl);
                        web::apply_oembed(&mut d, &v, &f.url);
                        if d.name.is_some() {
                            d.source = "media".into();
                            media_type(&mut d, uri);
                            embed = Some((d, v, f.url));
                        }
                    }
                }
            }
        }
        match tokio::time::timeout_at(deadline, self.web.resolve(uri)).await {
            Ok(Ok(mut page))
                if page.status == Status::Ok
                    && !page.event.warnings.iter().any(|w| w == "truncated") =>
            {
                if let Some((_, v, base)) = embed {
                    web::apply_oembed(&mut page, &v, &base);
                }
                media_type(&mut page, uri);
                page.bounded(web::ATTRIBUTES)
            }
            result => {
                if let Some((mut d, _, _)) = embed {
                    if let Ok(Ok(page)) = &result {
                        d.event.warnings.extend(page.event.warnings.iter().cloned());
                    }
                    return d.bounded(web::ATTRIBUTES);
                }
                match result {
                    Ok(result) => result,
                    Err(_) => Err(super::RetryLater {
                        class: "timeout",
                        seconds: 60,
                    }
                    .into()),
                }
            }
        }
    }
}
fn media_type(d: &mut ResourceMeta, uri: &str) {
    if !matches!(d.type_name.as_str(), "WebPage" | "CreativeWork") {
        return;
    }
    let host = Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    let kind = match host.as_deref() {
        Some("open.spotify.com") => Some("MusicRecording"),
        Some("soundcloud.com" | "www.soundcloud.com") => Some("AudioObject"),
        Some("youtube.com" | "www.youtube.com" | "youtu.be" | "vimeo.com" | "www.vimeo.com") => {
            Some("VideoObject")
        }
        _ => None,
    };
    if let Some(kind) = kind {
        d.set_type(kind);
        d.source = "media".into();
    }
}
