use super::{
    egress::{Egress, Fetched},
    Resolver,
};
use crate::{
    config::ResolverConfig,
    model::{Point, ResourceMeta, Status},
};
use anyhow::Result;
use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use std::{sync::OnceLock, time::Duration};
use url::Url;

pub const ATTRIBUTES: &[&str] = &[
    "content_type",
    "content_length",
    "author",
    "provider",
    "language",
    "publisher",
    "published",
    "container",
    "isbn",
    "doi",
    "podcast_guid",
    "podcast_feed",
    "duration",
    "isrc",
];
pub struct WebResolver {
    pub oembed_policy: super::cards::OembedPolicy,
    pub cards: Option<std::sync::Arc<super::cards::CardConfig>>,
    pub events: Option<std::sync::Arc<crate::event::service::EventService>>,
    pub egress: Egress,
    pub config: ResolverConfig,
}
#[async_trait]
impl Resolver for WebResolver {
    fn name(&self) -> &'static str {
        "web"
    }
    fn queue_sources(&self) -> Vec<&'static str> {
        let mut sources = vec!["web"];
        if self.events.as_ref().is_none_or(|e| !e.config.calendars) {
            sources.push("event");
        }
        if self
            .events
            .as_ref()
            .is_none_or(|e| !e.config.adapters.enabled)
        {
            sources.push("event_platform");
        }
        for kind in super::cards::KINDS {
            if self
                .cards
                .as_ref()
                .is_none_or(|c| !c.resolver(kind).enabled)
            {
                sources.push(kind);
            }
        }
        sources
    }
    fn matches(&self, uri: &str) -> bool {
        super::classify(uri).is_some_and(|source| self.queue_sources().contains(&source))
    }
    fn canary_allowed(&self) -> bool {
        self.manifest()
            .canary
            .is_none_or(|u| self.egress.allows(u.as_str()))
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
        self.resolve_with_policy(uri, self.oembed_policy).await
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        Duration::from_secs(doc.ttl_secs)
    }
}
impl WebResolver {
    pub async fn resolve_with_policy(
        &self,
        uri: &str,
        policy: super::cards::OembedPolicy,
    ) -> Result<ResourceMeta> {
        if !url::Url::parse(uri)
            .is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "webcal" | "webcals"))
        {
            return Err(super::RetryLater {
                class: "disabled",
                seconds: 3600,
            }
            .into());
        }
        if Url::parse(uri).is_ok_and(|u| u.host_str() == Some("api.nostr.wine"))
            && crate::event::adapters::detect(uri).is_none()
        {
            return Err(super::ErrorClass("policy").into());
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        tokio::time::timeout_at(deadline, async {
            let fetched = if let Some(events) = &self.events {
                events.fetch(&self.egress, uri).await?
            } else {
                let uri = if uri.starts_with("webcal:") || uri.starts_with("webcals:") {
                    crate::event::calendar::canonical_feed(uri)
                } else {
                    uri.to_owned()
                };
                self.egress.fetch(&uri).await?
            };
            if let Some(events) = &self.events {
                if let Some(doc) = events.container(uri, &fetched, deadline).await? {
                    return Ok(doc);
                }
                let feed = crate::event::service::feed_like(&fetched);
                if let Some(doc) = events.disappeared(uri, fetched.status, feed).await? {
                    return Ok(doc);
                }
            }
            if fetched.status == 304 {
                return Err(super::NotModified.into());
            }
            let observation = crate::telemetry::last_fetch();
            anyhow::ensure!(
                fetched.body.len() <= super::egress::BODY_CAP,
                "non-feed page exceeds 1 MB"
            );
            let input_uri = uri.to_owned();
            let input = fetched.clone();
            let ttl = self.config.ttl_secs;
            let mut extracted =
                tokio::task::spawn_blocking(move || extract(&input_uri, &input, ttl)).await?;
            if let Some(events) = &self.events {
                extracted.doc = events
                    .page(uri, &fetched, extracted.doc, &self.egress, deadline)
                    .await?;
            } else if extracted.doc.type_path.contains(&"Event".into()) {
                extracted.doc.set_type("WebPage");
                extracted.doc.start = None;
                extracted.doc.end = None;
                extracted.doc.event = Default::default();
            }

            if self
                .events
                .as_ref()
                .is_some_and(|e| e.config.adapters.enabled)
            {
                if let Some(doc) =
                    crate::event::adapters::service::wikipedia(uri, &fetched, &self.egress).await?
                {
                    extracted.doc = doc;
                }
            }

            if let Some(cards) = &self.cards {
                extracted.doc = super::cards::enrich(
                    uri,
                    &fetched,
                    extracted.doc,
                    &self.egress,
                    cards,
                    self.config.discover_feeds,
                )
                .await?;
            }
            // Account for the title fallback when deciding whether another request is
            // needed, while retaining oEmbed's established precedence if fetched.
            let mut candidate = extracted.doc.clone();
            fill(&mut candidate.name, extracted.title.clone());
            if let Some(endpoint) = extracted.oembed.take().filter(|endpoint| {
                let trusted = endpoint.origin() == fetched.url.origin()
                    || super::cards::provider_endpoint(uri).is_some_and(|registered| {
                        registered.origin() == endpoint.origin()
                            && registered.path() == endpoint.path()
                    });
                if !trusted {
                    crate::telemetry::discovery("oembed", "untrusted");
                }
                trusted
                    && (policy == super::cards::OembedPolicy::Always
                        || super::cards::needs_embed(&candidate))
            }) {
                if policy == super::cards::OembedPolicy::WhenNeeded
                    && extracted.doc.status == Status::Ok
                    && candidate.name.is_some()
                    && extracted.doc.image.is_none()
                {
                    crate::telemetry::discovery("oembed", "missing_image");
                }
                crate::telemetry::discovery("oembed", "requested");
                // Discovery is the only additional fetch; it follows the same policy as the page.
                let remaining = deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .saturating_sub(Duration::from_millis(25));
                if let Ok(Ok(response)) = tokio::time::timeout(
                    remaining.min(Duration::from_secs(2)),
                    self.egress.discover(
                        endpoint.as_str(),
                        &Default::default(),
                        self.config.ttl_secs,
                    ),
                )
                .await
                {
                    if response.status == 200 {
                        if let Ok(value) = serde_json::from_slice::<Value>(&response.body) {
                            apply_oembed(&mut extracted.doc, &value, &response.url);
                        }
                    }
                }
            }
            fill(&mut extracted.doc.name, extracted.title);
            fill(&mut extracted.doc.description, extracted.description);
            if fetched.truncated
                && !extracted
                    .doc
                    .event
                    .warnings
                    .iter()
                    .any(|w| w == "truncated")
            {
                extracted.doc.event.warnings.push("truncated".into());
            }
            let doc = extracted.doc.bounded(ATTRIBUTES)?;
            crate::telemetry::document(observation, &self.manifest(), &doc);
            Ok(doc)
        })
        .await?
    }
}
pub struct Extracted {
    pub doc: ResourceMeta,
    pub oembed: Option<Url>,
    pub title: Option<String>,
    pub description: Option<String>,
}
pub fn extract(uri: &str, fetched: &Fetched, ttl: u64) -> Extracted {
    let mut doc = ResourceMeta::new(uri, "web", ttl);
    if fetched.truncated {
        doc.event.warnings.push("truncated".into());
    }
    doc.attributes
        .insert("content_type".into(), fetched.content_type.clone());
    doc.attributes
        .insert("content_length".into(), fetched.body.len().to_string());
    if fetched.url.as_str() != uri {
        doc.aliases.push(fetched.url.to_string());
    }
    let mut out = Extracted {
        doc,
        oembed: None,
        title: None,
        description: None,
    };
    if Url::parse(uri).is_ok_and(|url| {
        super::interstitial::inspect(&url, fetched) != super::interstitial::Page::Content
    }) {
        out.doc.status = Status::Blocked;
        out.doc.aliases.clear();
        return out;
    }
    if fetched.permanent_redirect {
        add_alias(&mut out.doc, fetched.url.to_string());
    }
    match fetched.status {
        404 | 410 | 451 => {
            out.doc.status = Status::Missing;
            return out;
        }
        401 | 403 => {
            out.doc.status = Status::Unreachable;
            return out;
        }
        200..=299 => {}
        _ => {
            out.doc.status = Status::Error;
            return out;
        }
    }
    if fetched.content_type.starts_with("image/") {
        out.doc.set_type("ImageObject");
        out.doc.image = Some(fetched.url.to_string());
        return out;
    }
    if fetched.content_type == "application/pdf" {
        out.doc.name = fetched
            .url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .map(str::to_owned);
        return out;
    }
    let text = String::from_utf8_lossy(&fetched.body);
    if fetched.content_type.ends_with("json") {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            apply_jsonld(&mut out.doc, &v, &fetched.url);
        }
        return out;
    }
    let html = Html::parse_document(&text);
    let jsonld: Vec<Value> = html
        .select(&selector("script[type='application/ld+json']"))
        .filter_map(|script| crate::event::page::lenient_json(&script.inner_html()))
        .collect();
    apply_jsonld(&mut out.doc, &Value::Array(jsonld), &fetched.url);
    for (class, kind) in [
        ("h-event", "Event"),
        ("h-card", "Person"),
        ("h-entry", "Article"),
    ] {
        if let Some(root) = html.select(&selector(&format!(".{class}"))).next() {
            if out.doc.source == "web" {
                out.doc.set_type(kind);
                out.doc.source = "microformats".into();
            }
            fill(&mut out.doc.name, mf(&root, ".p-name"));
            fill(
                &mut out.doc.description,
                mf(&root, ".p-summary, .p-description"),
            );
            fill(
                &mut out.doc.image,
                mf(&root, ".u-photo").and_then(|s| origin_url(&fetched.url, &s)),
            );
            if out.doc.start.is_none() {
                out.doc.start = mf(&root, ".dt-start").and_then(|s| parse_time(&s));
            }
            if out.doc.end.is_none() {
                out.doc.end = mf(&root, ".dt-end").and_then(|s| parse_time(&s));
            }
            if out.doc.location.is_none() {
                out.doc.location = mf(&root, ".p-latitude")
                    .and_then(|s| s.parse().ok())
                    .zip(mf(&root, ".p-longitude").and_then(|s| s.parse().ok()))
                    .and_then(|(lat, lon)| point(lat, lon));
            }
        }
    }
    // Only the document head is the publisher's: a body `<meta>` may come from user content
    // (a comment citing an attacker's DOI).
    let mut meta = std::collections::HashMap::new();
    for el in html.select(&selector("head meta")) {
        if let (Some(k), Some(v)) = (
            el.value()
                .attr("property")
                .or_else(|| el.value().attr("name")),
            el.value().attr("content"),
        ) {
            meta.entry(k.to_ascii_lowercase())
                .or_insert_with(|| v.to_string());
        }
    }
    // Scholarly pages cite their own identifiers in Highwire, PRISM and Dublin Core meta;
    // a registry naming this page needs that citation before it may merge with it.
    for key in [
        "citation_doi",
        "prism.doi",
        "dc.identifier",
        "bepress_citation_doi",
    ] {
        if let Some(doi) = meta.get(key).and_then(|v| cited_doi(v)) {
            crate::group_keys::add(&mut out.doc, "doi", &doi);
        }
    }
    if let Some(isbn) = meta.get("citation_isbn") {
        crate::group_keys::add(&mut out.doc, "isbn", isbn);
    }
    for key in ["og:title", "twitter:title"] {
        fill(&mut out.doc.name, meta.get(key).cloned());
    }
    for key in ["og:description", "twitter:description"] {
        fill(&mut out.doc.description, meta.get(key).cloned());
    }
    for key in ["og:image", "twitter:image", "twitter:image:src"] {
        fill(
            &mut out.doc.image,
            meta.get(key).and_then(|s| origin_url(&fetched.url, s)),
        );
    }
    if out.doc.source == "web" {
        if let Some(kind) = meta
            .get("og:type")
            .and_then(|s| mapped_type("opengraph", s))
        {
            out.doc.set_type(kind);
            out.doc.source = "opengraph".into();
        }
    }
    if let Some(site) = meta.get("og:site_name") {
        out.doc.site = site.chars().take(128).collect();
    }
    if let Some(alias) = meta.get("og:url").and_then(|s| origin_url(&fetched.url, s)) {
        add_alias(&mut out.doc, alias);
    }
    for link in html.select(&selector("link")) {
        if let Some(href) = link.value().attr("href") {
            if link
                .value()
                .attr("rel")
                .is_some_and(|s| s.split_whitespace().any(|r| r == "canonical"))
            {
                if let Some(alias) = origin_url(&fetched.url, href) {
                    add_alias(&mut out.doc, alias);
                }
            }
            if out.oembed.is_none()
                && link.value().attr("type") == Some("application/json+oembed")
                && link
                    .value()
                    .attr("rel")
                    .is_some_and(|s| s.split_whitespace().any(|r| r == "alternate"))
            {
                out.oembed = origin_url(&fetched.url, href).and_then(|u| Url::parse(&u).ok());
            }
        }
    }
    out.title = html
        .select(&selector("title"))
        .next()
        .map(|el| el.text().collect::<String>().trim().to_owned());
    out.description = meta.get("description").cloned();
    out
}
fn cited_doi(value: &str) -> Option<String> {
    let value = value.trim();
    let value = match value.get(..4) {
        Some(p) if p.eq_ignore_ascii_case("doi:") => &value[4..],
        _ => value,
    };
    super::cards::doi(value).or_else(|| super::cards::doi(&format!("doi:{}", value.trim())))
}
fn selector(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}
fn mf(root: &ElementRef<'_>, s: &str) -> Option<String> {
    root.select(&selector(s)).next().map(|el| {
        let attrs: &[&str] = if s.starts_with(".u-") {
            &["href", "src", "data", "value"]
        } else if s.starts_with(".dt-") {
            &["datetime", "title", "value"]
        } else {
            &["content", "title", "value", "alt"]
        };
        attrs
            .iter()
            .find_map(|a| el.value().attr(a))
            .map(str::to_owned)
            .unwrap_or_else(|| el.text().collect::<String>().trim().to_owned())
    })
}
fn fill(dst: &mut Option<String>, src: Option<String>) {
    if dst.is_none() {
        *dst = src.filter(|s| !s.trim().is_empty());
    }
}
fn string(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.get("@value").and_then(Value::as_str).map(str::to_owned))
}
fn image(v: &Value) -> Option<String> {
    string(v)
        .or_else(|| v.get("url").and_then(string))
        .or_else(|| v.get("contentUrl").and_then(string))
        .or_else(|| v.as_array().and_then(|a| a.first()).and_then(image))
}
pub fn origin_url(base: &Url, s: &str) -> Option<String> {
    base.join(s)
        .ok()
        .filter(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.username().is_empty()
                && u.password().is_none()
        })
        .map(|u| u.to_string())
        .filter(|u| u.len() <= 2048)
}
fn parse_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.to_utc())
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
fn point(lat: f64, lon: f64) -> Option<Point> {
    (lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon))
    .then_some(Point { lat, lon })
}
fn add_alias(doc: &mut ResourceMeta, alias: String) {
    crate::group_keys::url_keys(doc, &alias);
    crate::group_keys::add(doc, "canonical", &alias);
    if doc.aliases.len() < 4 && alias != doc.url && !doc.aliases.contains(&alias) {
        doc.aliases.push(alias);
    }
}
fn schema_type(v: &Value) -> Option<&str> {
    let kind = v.get("@type")?;
    if let Some(s) = kind.as_str() {
        return (!crate::model::type_path(s).is_empty()).then_some(s);
    }
    kind.as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|s| !crate::model::type_path(s).is_empty())
}
fn entity_priority(v: &Value) -> u8 {
    if let Some(values) = v.as_array() {
        return values.iter().map(entity_priority).max().unwrap_or(0);
    }
    let nested = v
        .get("mainEntity")
        .or_else(|| v.get("@graph"))
        .map(entity_priority)
        .unwrap_or(0);
    let path = schema_type(v)
        .map(crate::model::type_path)
        .unwrap_or_default();
    let own = if path.iter().any(|s| {
        matches!(
            s.as_str(),
            "Article" | "Event" | "Product" | "Place" | "MediaObject"
        )
    }) {
        100
    } else if path.iter().any(|s| s == "WebPage") {
        80
    } else if path.iter().any(|s| {
        matches!(
            s.as_str(),
            "Organization" | "Person" | "WebSite" | "BreadcrumbList"
        )
    }) {
        20
    } else if !path.is_empty() {
        60
    } else {
        0
    };
    own.max(nested)
}
fn apply_jsonld(doc: &mut ResourceMeta, v: &Value, base: &Url) {
    if let Some(values) = v.as_array() {
        // A page's article/event outranks its publisher or breadcrumb helper nodes.
        // Extract one entity so unrelated graph nodes cannot donate misleading fields.
        if let Some(mut best) = values.first() {
            for value in values.iter().skip(1) {
                if entity_priority(value) > entity_priority(best) {
                    best = value;
                }
            }
            apply_jsonld(doc, best, base);
        }
        return;
    }
    if let Some(v) = v.get("mainEntity") {
        apply_jsonld(doc, v, base);
        if doc.source != "web" {
            return;
        }
    }
    if let Some(v) = v.get("@graph") {
        apply_jsonld(doc, v, base);
        if doc.source != "web" {
            return;
        }
    }
    let Some(kind) = schema_type(v) else {
        return;
    };
    if doc.source == "web" {
        doc.set_type(kind);
        doc.source = "jsonld".into();
        for field in ["url", "@id"] {
            if let Some(url) = v
                .get(field)
                .and_then(Value::as_str)
                .and_then(|s| origin_url(base, s))
            {
                add_alias(doc, url);
            }
        }
    }
    crate::group_keys::jsonld(doc, v, base);
    for (field, attribute) in [("duration", "duration"), ("isrcCode", "isrc")] {
        if let Some(value) = v.get(field).and_then(Value::as_str) {
            doc.attributes
                .entry(attribute.into())
                .or_insert_with(|| value.into());
        }
    }
    for field in ["author", "creator"] {
        if !doc.attributes.contains_key("author") {
            if let Some(value) = v.get(field) {
                if let Some(name) = value.as_str().or_else(|| value["name"].as_str()) {
                    doc.attributes.insert("author".into(), name.into());
                }
            }
        }
    }
    fill(
        &mut doc.name,
        v.get("name").or_else(|| v.get("headline")).and_then(string),
    );
    fill(&mut doc.description, v.get("description").and_then(string));
    fill(
        &mut doc.image,
        v.get("image")
            .and_then(image)
            .and_then(|s| origin_url(base, &s)),
    );
    fill(&mut doc.category, v.get("category").and_then(string));
    let geo = v.get("geo").or_else(|| v.pointer("/location/geo"));
    if doc.location.is_none() {
        doc.location = geo
            .and_then(|v| {
                v.get("latitude")
                    .and_then(number)
                    .zip(v.get("longitude").and_then(number))
            })
            .and_then(|(lat, lon)| point(lat, lon));
    }
    if doc.start.is_none() {
        doc.start = v
            .get("startDate")
            .and_then(Value::as_str)
            .and_then(parse_time);
    }
    if doc.end.is_none() {
        doc.end = v
            .get("endDate")
            .and_then(Value::as_str)
            .and_then(parse_time);
    }
    if let Some(alias) = v
        .get("url")
        .and_then(string)
        .and_then(|s| origin_url(base, &s))
    {
        add_alias(doc, alias);
    }
    if let Some(alias) = v
        .get("sameAs")
        .and_then(string)
        .and_then(|s| origin_url(base, &s))
    {
        add_alias(doc, alias);
    }
}
fn mapped_type(vocab: &str, name: &str) -> Option<&'static str> {
    static MAP: OnceLock<Value> = OnceLock::new();
    MAP.get_or_init(|| {
        serde_json::from_str(include_str!("../../../../data/type-mappings.json"))
            .expect("mapping table")
    })
    .get(vocab)?
    .get(name)?
    .as_str()
}
pub fn apply_oembed(doc: &mut ResourceMeta, v: &Value, base: &Url) {
    fill(&mut doc.name, v.get("title").and_then(string));
    fill(
        &mut doc.image,
        v.get("thumbnail_url")
            .and_then(string)
            .and_then(|s| origin_url(base, &s)),
    );
    if doc.source == "web" {
        if let Some(kind) = v
            .get("type")
            .and_then(Value::as_str)
            .and_then(|s| mapped_type("oembed", s))
        {
            doc.set_type(kind);
            doc.source = "oembed".into();
        }
    }
    for (key, field) in [("author", "author_name"), ("provider", "provider_name")] {
        if let Some(v) = v.get(field).and_then(string) {
            doc.attributes.entry(key.into()).or_insert(v);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extraction_precedence_and_promoted_facets() {
        let body = r#"<title>Title fallback</title><meta property="og:title" content="OG"><meta property="og:image" content="/pic.png"><link rel="alternate" type="application/json+oembed" href="/embed"><link rel="canonical" href="/canonical"><div class="h-event"><span class="p-name">MF</span><span class="p-summary">MF summary</span></div><script type="application/ld+json">{"@type":"MusicEvent","name":"JSON first","startDate":"2026-10-01T12:00:00Z","location":{"geo":{"latitude":46.0,"longitude":8.0}}}</script>"#;
        let fetched = Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: Url::parse("https://example.org/").unwrap(),
            status: 200,
            retry_after_secs: 60,
            content_type: "text/html".into(),
            location: None,
            body: body.as_bytes().into(),
        };
        let out = extract("https://example.org/", &fetched, 100);
        assert_eq!(out.doc.name.as_deref(), Some("JSON first"));
        assert_eq!(out.doc.description.as_deref(), Some("MF summary"));
        assert_eq!(
            out.doc.image.as_deref(),
            Some("https://example.org/pic.png")
        );
        assert!(out.doc.type_path.contains(&"Event".into()));
        assert!(out.doc.start.is_some());
        assert_eq!(out.doc.location.unwrap().lat, 46.0);
        assert!(out.oembed.is_some());
        assert!(out
            .doc
            .aliases
            .contains(&"https://example.org/canonical".into()));
    }
    #[test]
    fn pages_cite_their_own_identifiers_from_meta() {
        let keys = |head: &str| {
            let fetched = Fetched {
                permanent_redirect: false,
                truncated: false,
                url: Url::parse("https://journal.example/a").unwrap(),
                body: format!("<title>Paper</title>{head}").into_bytes(),
                content_type: "text/html".into(),
                status: 200,
                location: None,
                retry_after_secs: 60,
                validators: Default::default(),
            };
            extract("https://journal.example/a", &fetched, 100)
                .doc
                .group_keys
                .into_iter()
                .map(|k| k.key)
                .filter(|k| !k.starts_with("url:"))
                .collect::<Vec<_>>()
        };
        for head in [
            r#"<meta name="citation_doi" content="10.1234/ABC">"#,
            r#"<meta name="prism.doi" content="doi:10.1234/abc">"#,
            r#"<meta name="DC.Identifier" content="DOI:10.1234/abc">"#,
            r#"<meta name="citation_doi" content="https://doi.org/10.1234/abc">"#,
        ] {
            assert_eq!(keys(head), ["doi:10.1234/abc"], "{head}");
        }
        assert_eq!(
            keys(r#"<meta name="citation_isbn" content="978-0-14-032872-1">"#),
            ["isbn:9780140328721"]
        );
        // A body meta (user content) cites nothing; the head's own citation still counts.
        assert!(keys(
            r#"<body><p>Comment</p><meta name="citation_doi" content="10.5555/evil"></body>"#
        )
        .is_empty());
        assert_eq!(
            keys(
                r#"<head><meta name="citation_doi" content="10.1234/abc"></head><body><p>x</p><meta name="citation_doi" content="10.5555/evil"></body>"#
            ),
            ["doi:10.1234/abc"]
        );
        // Other identifiers and malformed values mint nothing, multi-byte text included.
        for head in [
            r#"<meta name="dc.identifier" content="urn:issn:1234-5678">"#,
            r#"<meta name="citation_doi" content="d€i:10.1234/abc">"#,
            r#"<meta name="citation_doi" content="€">"#,
            r#"<meta name="citation_isbn" content="not an isbn">"#,
        ] {
            assert!(keys(head).is_empty(), "{head}");
        }
    }
    #[test]
    fn oembed_fills_only_missing_and_never_keeps_html() {
        let mut doc = ResourceMeta::new("https://example.org/", "web", 100);
        doc.name = Some("OG wins".into());
        apply_oembed(
            &mut doc,
            &serde_json::json!({"type":"video","title":"embed","thumbnail_url":"/thumb","html":"<script>bad</script>"}),
            &Url::parse("https://example.org/").unwrap(),
        );
        assert_eq!(doc.name.as_deref(), Some("OG wins"));
        assert_eq!(doc.type_name, "VideoObject");
        assert!(!serde_json::to_string(&doc).unwrap().contains("<script>"));
    }
    #[test]
    fn schema_graph_prefers_the_page_entity_over_its_publisher() {
        let mut doc = ResourceMeta::new("https://example.org/story", "web", 100);
        apply_jsonld(
            &mut doc,
            &serde_json::json!({"@graph":[
                {"@type":"Organization","name":"Publisher","image":"https://example.org/logo"},
                {"@type":["UnknownType","NewsArticle"],"headline":"Actual story","description":"Story details"},
                {"@type":"BreadcrumbList","name":"Home"}
            ]}),
            &Url::parse("https://example.org/story").unwrap(),
        );
        assert_eq!(doc.type_name, "NewsArticle");
        assert_eq!(doc.name.as_deref(), Some("Actual story"));
        assert!(doc.image.is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn slow_oembed_does_not_discard_the_page_card_and_page_timeout_is_ten_seconds() {
        use super::super::egress::{Dns, Transport};
        use std::{net::SocketAddr, sync::Arc};
        struct TestDns;
        #[async_trait]
        impl Dns for TestDns {
            async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
                Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
            }
        }
        struct Pages;
        #[async_trait]
        impl Transport for Pages {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                if url.path() != "/page" {
                    std::future::pending::<()>().await;
                }
                Ok(Fetched {permanent_redirect: false,truncated: false, validators:Default::default(),url:url.clone(),status:200,retry_after_secs:60,location:None,content_type:"text/html".into(),body:br#"<title>Page survives</title><link rel="alternate" type="application/json+oembed" href="/embed">"#.to_vec() })
            }
        }
        let resolver = WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            events: None,
            egress: Egress::with_backends(Default::default(), Arc::new(TestDns), Arc::new(Pages)),
            config: Default::default(),
        };
        let start = tokio::time::Instant::now();
        let doc = resolver.resolve("https://example.org/page").await.unwrap();
        assert_eq!(doc.name.as_deref(), Some("Page survives"));
        assert!(start.elapsed() < Duration::from_secs(3));
        let start = tokio::time::Instant::now();
        assert!(resolver.resolve("https://example.org/hangs").await.is_err());
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }
}
