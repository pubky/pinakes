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
            apply_jsonld(&mut out.doc, &v, &fetched.url, true);
        }
        return out;
    }
    let html = Html::parse_document(&text);
    // JSON-LD stays document-wide (plugins put it in the body), but only the chosen entity
    // may carry claims (url/@id, sameAs, identifiers): from a head script, as with head meta
    // and links (its main-entity url is the publisher's canonical declaration), or from a
    // body script that names this page or its head canonical.
    let scripts: Vec<(Value, bool)> = html
        .select(&selector("script[type='application/ld+json']"))
        .filter_map(|script| {
            let head = script
                .ancestors()
                .any(|a| a.value().as_element().is_some_and(|e| e.name() == "head"));
            crate::event::page::lenient_json(&script.inner_html()).map(|v| (v, head))
        })
        .collect();
    if let Some(best) = (1..scripts.len()).fold((!scripts.is_empty()).then_some(0), |best, i| {
        best.filter(|b| entity_priority(&scripts[i].0) <= entity_priority(&scripts[*b].0))
            .or(Some(i))
    }) {
        let page: Vec<String> = [uri.to_owned(), fetched.url.to_string()]
            .into_iter()
            .chain(
                html.select(&selector("head link[rel~=canonical][href]"))
                    .filter_map(|l| origin_url(&fetched.url, l.value().attr("href")?)),
            )
            .filter_map(|u| entity_identity(&u))
            .collect();
        let (entity, head) = &scripts[best];
        let named = entity_urls(entity)
            .filter_map(|u| origin_url(&fetched.url, u).and_then(|u| entity_identity(&u)))
            .collect::<Vec<_>>();
        let claims = *head || named.iter().any(|id| page.contains(id));
        apply_jsonld(&mut out.doc, entity, &fetched.url, claims);
        // A plugin's body Article can outrank the head's WebPage for the card; the head's
        // identity claims still apply.
        if !head {
            let head_best = scripts
                .iter()
                .filter(|(_, head)| *head)
                .fold(None, |best, (v, _)| match best {
                    Some(b) if entity_priority(v) <= entity_priority(b) => Some(b),
                    _ => Some(v),
                });
            if let Some(v) = head_best {
                jsonld_claims(&mut out.doc, v, &fetched.url);
            }
        }
    }
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
    // canonical, alternate and oEmbed links belong in the head (they are not body-ok); a body
    // `<link>` may come from user content and must not claim identity or advertise endpoints.
    for link in html.select(&selector("head link")) {
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
    type_priority(v).max(nested)
}
/// A node's own priority, ignoring nested entities: 100 for content, 80 for WebPage, 20 for
/// publisher and navigation helpers.
fn type_priority(v: &Value) -> u8 {
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
    own
}
/// The `url`/`@id` values of a JSON-LD node (or array of nodes), its main entity and its
/// graph nodes.
fn entity_urls(v: &Value) -> impl Iterator<Item = &str> {
    v.as_array()
        .map_or_else(|| vec![v], |a| a.iter().take(16).collect())
        .into_iter()
        .flat_map(|v| {
            let graph = v.get("@graph").and_then(Value::as_array);
            std::iter::once(v)
                .chain(v.get("mainEntity").filter(|m| m.is_object()))
                .chain(graph.into_iter().flatten().take(16))
        })
        .flat_map(|n| {
            // Google's Article shape names its page through mainEntityOfPage (a URL, a
            // WebPage node, or an array of either).
            let pages = n.get("mainEntityOfPage").map_or_else(Vec::new, |p| {
                p.as_array()
                    .map_or_else(|| vec![p], |a| a.iter().take(16).collect())
            });
            [n.get("url"), n.get("@id")]
                .into_iter()
                .chain(pages.into_iter().flat_map(|p| {
                    [
                        Some(p).filter(|p| p.is_string()),
                        p.get("@id"),
                        p.get("url"),
                    ]
                }))
                .collect::<Vec<_>>()
        })
        .flatten()
        .filter_map(Value::as_str)
}
fn entity_identity(url: &str) -> Option<String> {
    let mut u = Url::parse(url).ok()?;
    u.set_fragment(None);
    crate::model::identity(u.as_str()).ok().map(|v| v.0)
}
/// Identity claims (url/@id aliases, typed identifiers, sameAs) that a head script donates
/// when a body entity is chosen for the card: those of the entity `apply_jsonld` would choose
/// in `v`, without its card fields, and only from a WebPage-or-better node. A head
/// Organization/WebSite/Person node would otherwise name the homepage and a site-wide
/// Wikidata item for every page. Returns whether claims were donated.
fn jsonld_claims(doc: &mut ResourceMeta, v: &Value, base: &Url) -> bool {
    if let Some(values) = v.as_array() {
        let best = values.iter().fold(None, |best, v| match best {
            Some(b) if entity_priority(v) <= entity_priority(b) => Some(b),
            _ => Some(v),
        });
        return best.is_some_and(|b| jsonld_claims(doc, b, base));
    }
    for nested in ["mainEntity", "@graph"] {
        if v.get(nested).is_some_and(|n| jsonld_claims(doc, n, base)) {
            return true;
        }
    }
    if schema_type(v).is_none() || type_priority(v) < 80 {
        return false;
    }
    for field in ["url", "@id", "sameAs"] {
        if let Some(url) = v
            .get(field)
            .and_then(string)
            .and_then(|s| origin_url(base, &s))
        {
            add_alias(doc, url);
        }
    }
    crate::group_keys::jsonld(doc, v, base);
    true
}
/// `claims` gates identity-bearing fields (aliases from url/@id/sameAs, typed identifiers):
/// they come only from an entity that is the page's own.
fn apply_jsonld(doc: &mut ResourceMeta, v: &Value, base: &Url, claims: bool) {
    if let Some(values) = v.as_array() {
        // A page's article/event outranks its publisher or breadcrumb helper nodes.
        // Extract one entity so unrelated graph nodes cannot donate misleading fields.
        if let Some(mut best) = values.first() {
            for value in values.iter().skip(1) {
                if entity_priority(value) > entity_priority(best) {
                    best = value;
                }
            }
            apply_jsonld(doc, best, base, claims);
        }
        return;
    }
    if let Some(v) = v.get("mainEntity") {
        apply_jsonld(doc, v, base, claims);
        if doc.source != "web" {
            return;
        }
    }
    if let Some(v) = v.get("@graph") {
        apply_jsonld(doc, v, base, claims);
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
        for field in ["url", "@id"].into_iter().filter(|_| claims) {
            if let Some(url) = v
                .get(field)
                .and_then(Value::as_str)
                .and_then(|s| origin_url(base, s))
            {
                add_alias(doc, url);
            }
        }
    }
    if claims {
        crate::group_keys::jsonld(doc, v, base);
    }
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
        .filter(|_| claims)
    {
        add_alias(doc, alias);
    }
    if let Some(alias) = v
        .get("sameAs")
        .and_then(string)
        .and_then(|s| origin_url(base, &s))
        .filter(|_| claims)
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
    fn body_links_and_foreign_json_ld_nodes_claim_nothing() {
        let page = |html: &str| {
            let fetched = Fetched {
                permanent_redirect: false,
                truncated: false,
                url: Url::parse("https://victim.example/post").unwrap(),
                body: html.as_bytes().to_vec(),
                content_type: "text/html".into(),
                status: 200,
                location: None,
                retry_after_secs: 60,
                validators: Default::default(),
            };
            extract("https://victim.example/post", &fetched, 100)
        };
        let claims = |out: &Extracted| {
            out.doc
                .group_keys
                .iter()
                .filter_map(|k| k.target.clone())
                .filter(|t| !t.starts_with("https://victim.example/post"))
                .collect::<Vec<_>>()
        };
        // V31: a comment's canonical/oEmbed links sit in the body and are ignored.
        let out = page(
            r#"<title>Post</title><body><p>Comment</p><link rel="canonical" href="https://attacker.example/a"><link rel="alternate" type="application/json+oembed" href="https://attacker.example/embed"></body>"#,
        );
        assert!(claims(&out).is_empty(), "{:?}", claims(&out));
        assert!(out.oembed.is_none());
        let out = page(
            r#"<head><title>Post</title><link rel="canonical" href="/canonical"><link rel="alternate" type="application/json+oembed" href="/embed"></head><body><p>x</p></body>"#,
        );
        assert_eq!(claims(&out), ["https://victim.example/canonical"]);
        assert!(out.oembed.is_some());

        // K51: a second ld+json node in the body outranks the page's WebPage node, but names
        // nothing (or another page), so it mints no claims; its card fields still apply.
        for injected in [
            r#"{"@type":"NewsArticle","headline":"Injected","sameAs":"https://attacker.example/a"}"#,
            r#"{"@type":"NewsArticle","url":"https://attacker.example/a","sameAs":"https://attacker.example/b","identifier":"https://doi.org/10.5555/evil"}"#,
        ] {
            let out = page(&format!(
                r#"<head><title>Post</title><script type="application/ld+json">{{"@type":"WebPage","name":"Post"}}</script></head><body><p>Comment</p><script type="application/ld+json">{injected}</script></body>"#
            ));
            assert!(claims(&out).is_empty(), "{injected}: {:?}", claims(&out));
            assert!(out
                .doc
                .group_keys
                .iter()
                .all(|k| !k.key.starts_with("doi:")));
        }
        // The page's own entity keeps its claims: in the head without url/@id, or anywhere
        // when it names this page or its head canonical.
        for (head, body) in [
            (
                r#"<script type="application/ld+json">{"@type":"NewsArticle","sameAs":"https://doi.org/10.1234/abc"}</script>"#,
                "",
            ),
            (
                "",
                r#"<script type="application/ld+json">{"@type":"NewsArticle","url":"https://victim.example/post","sameAs":"https://doi.org/10.1234/abc"}</script>"#,
            ),
            (
                r#"<link rel="canonical" href="https://victim.example/canonical">"#,
                r#"<script type="application/ld+json">{"@graph":[{"@type":"WebPage","@id":"https://victim.example/canonical#page"},{"@type":"NewsArticle","sameAs":"https://doi.org/10.1234/abc"}]}</script>"#,
            ),
        ] {
            let out = page(&format!(
                "<head><title>Post</title>{head}</head><body><p>Text</p>{body}</body>"
            ));
            assert!(
                out.doc
                    .group_keys
                    .iter()
                    .any(|k| k.key == "doi:10.1234/abc"),
                "{head}{body}"
            );
        }
    }
    #[test]
    fn body_entities_name_their_page_through_main_entity_of_page_and_head_claims_stay() {
        let extract_keys = |html: &str| {
            let fetched = Fetched {
                permanent_redirect: false,
                truncated: false,
                url: Url::parse("https://news.example/story").unwrap(),
                body: html.as_bytes().to_vec(),
                content_type: "text/html".into(),
                status: 200,
                location: None,
                retry_after_secs: 60,
                validators: Default::default(),
            };
            let d = extract("https://news.example/story", &fetched, 100).doc;
            (
                d.type_name.clone(),
                d.group_keys.into_iter().map(|k| k.key).collect::<Vec<_>>(),
            )
        };
        // V36: Google's Article shape names the page via mainEntityOfPage (URL or WebPage).
        for page in [
            r#""https://news.example/story""#,
            r#"{"@type":"WebPage","@id":"https://news.example/story"}"#,
            r#"{"@type":"WebPage","url":"https://news.example/story#main"}"#,
            r#"["https://news.example/other",{"@type":"WebPage","@id":"https://news.example/story"}]"#,
        ] {
            let (_, keys) = extract_keys(&format!(
                r#"<head><title>Story</title></head><body><p>Text</p><script type="application/ld+json">{{"@type":"NewsArticle","mainEntityOfPage":{page},"sameAs":"https://doi.org/10.1234/abc"}}</script></body>"#
            ));
            assert!(
                keys.contains(&"doi:10.1234/abc".to_owned()),
                "{page}: {keys:?}"
            );
        }
        let (_, keys) = extract_keys(
            r#"<head><title>Story</title></head><body><p>Text</p><script type="application/ld+json">{"@type":"NewsArticle","mainEntityOfPage":"https://elsewhere.example/x","sameAs":"https://doi.org/10.1234/abc"}</script></body>"#,
        );
        assert!(!keys.contains(&"doi:10.1234/abc".to_owned()));
        // V37: a body Article outranks the head WebPage for the card, and the head's claims
        // still apply; the body node, naming nothing, adds none.
        let (kind, keys) = extract_keys(
            r#"<head><title>Story</title><script type="application/ld+json">{"@type":"WebPage","sameAs":"https://doi.org/10.1234/head"}</script></head><body><p>Text</p><script type="application/ld+json">{"@type":"NewsArticle","headline":"Story","sameAs":"https://doi.org/10.5555/body"}</script></body>"#,
        );
        assert_eq!(kind, "NewsArticle");
        assert!(keys.contains(&"doi:10.1234/head".to_owned()), "{keys:?}");
        assert!(!keys.contains(&"doi:10.5555/body".to_owned()), "{keys:?}");
        // V41: a head Organization (below WebPage) donates nothing under a body Article: no
        // homepage canonical and no site-wide Wikidata key.
        let (kind, keys) = extract_keys(
            r#"<head><title>Story</title><script type="application/ld+json">{"@type":"Organization","url":"https://news.example/","sameAs":"https://www.wikidata.org/wiki/Q42"}</script></head><body><p>Text</p><script type="application/ld+json">{"@type":"NewsArticle","headline":"Story"}</script></body>"#,
        );
        assert_eq!(kind, "NewsArticle");
        let homepage = format!(
            "url:{}",
            crate::model::identity("https://news.example/").unwrap().0
        );
        assert!(!keys.contains(&homepage), "{keys:?}");
        assert!(keys.iter().all(|k| !k.starts_with("wikidata:")), "{keys:?}");
        // As the chosen entity (no body script), the same head node keeps its claims.
        let (_, keys) = extract_keys(
            r#"<head><title>Story</title><script type="application/ld+json">{"@type":"Organization","url":"https://news.example/","sameAs":"https://www.wikidata.org/wiki/Q42"}</script></head><body><p>Text</p></body>"#,
        );
        assert!(keys.contains(&"wikidata:Q42".to_owned()), "{keys:?}");
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
            true,
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
