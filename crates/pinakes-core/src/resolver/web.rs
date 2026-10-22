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
use scraper::{ElementRef, Selector};
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
    // The RSS feed a page's own head names (`rel="alternate"`), on any host.
    "feed",
    "duration",
    "isrc",
    "artist",
    "album",
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
    /// A Wikipedia article's card from its REST summary; `None` falls back to the page (a
    /// missing article, a page outside the article namespace, a failed request).
    async fn wikipedia_card(
        &self,
        uri: &str,
        article: &crate::wikipedia::Article,
    ) -> Option<ResourceMeta> {
        let options = super::egress::FetchOptions {
            accept: Some("application/json".into()),
            ..Default::default()
        };
        let fetched = match self
            .egress
            .fetch_with(&crate::wikipedia::summary_url(article), &options)
            .await
        {
            Ok(fetched) if fetched.status == 200 => fetched,
            Ok(_) => return None,
            Err(error) => {
                tracing::debug!(error = %crate::resolver::log_error(&error), "Wikipedia summary failed; reading the page");
                return None;
            }
        };
        let v: Value = serde_json::from_slice(&fetched.body).ok()?;
        crate::wikipedia::summary_card(uri, article, &v, self.config.ttl_secs)?
            .bounded(ATTRIBUTES)
            .ok()
    }
    /// A MusicBrainz page's card from the entity MusicBrainz serves (`None`: not such a page, an
    /// entity MusicBrainz doesn't know, or an answer over its cap, so the page is read). A busy
    /// MusicBrainz retries later.
    async fn musicbrainz_card(&self, uri: &str) -> Result<Option<ResourceMeta>> {
        match self.musicbrainz_entity_card(uri).await {
            Err(e) if super::egress::over_cap(&e) => Ok(None),
            card => card,
        }
    }
    async fn musicbrainz_entity_card(&self, uri: &str) -> Result<Option<ResourceMeta>> {
        use crate::hubs::musicbrainz;
        let base = Some(self.config.musicbrainz_base.as_str())
            .filter(|b| !b.is_empty())
            .unwrap_or(musicbrainz::BASE);
        let ttl = self.config.ttl_secs;
        let card = if let Some((kind, mbid, _)) = musicbrainz::entity_link(uri) {
            match musicbrainz::entity(&self.egress, base, kind, &mbid).await? {
                Some(entity) => {
                    crate::music_sources::musicbrainz_card(uri, Some(&entity), None, ttl)
                }
                None => return Ok(None),
            }
        } else if let Some((isrc, _)) = musicbrainz::isrc_link(uri) {
            let recordings = musicbrainz::isrc(&self.egress, base, &isrc)
                .await?
                .unwrap_or_default();
            let entity = match recordings.as_slice() {
                [one] => {
                    musicbrainz::entity(&self.egress, base, crate::hubs::MbKind::Recording, one)
                        .await?
                }
                _ => None,
            };
            crate::music_sources::musicbrainz_card(uri, entity.as_ref(), Some(&isrc), ttl)
        } else {
            return Ok(None);
        };
        Ok(Some(card.bounded(ATTRIBUTES)?))
    }
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
        if let Some(article) = crate::wikipedia::parse(uri) {
            if let Some(doc) = self.wikipedia_card(uri, &article).await {
                return Ok(doc);
            }
        }
        if let Some(doc) = self.musicbrainz_card(uri).await? {
            return Ok(doc);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        tokio::time::timeout_at(deadline, async {
            let fetched = if let Some(events) = &self.events {
                events.fetch(&self.egress, uri).await
            } else {
                let uri = if uri.starts_with("webcal:") || uri.starts_with("webcals:") {
                    crate::event::calendar::canonical_feed(uri)
                } else {
                    uri.to_owned()
                };
                self.egress.fetch(&uri).await
            };
            // A page behind an access wall may still be described by its site's own feed.
            let fetched = match fetched {
                Err(e) if e.is::<super::interstitial::Blocked>() => {
                    return match crate::sources::wall_hooks(
                        &self.egress,
                        uri,
                        self.config.ttl_secs,
                        deadline,
                    )
                    .await
                    {
                        Some(doc) => Ok(doc),
                        None => Err(e),
                    };
                }
                fetched => fetched?,
            };
            if let Some(events) = &self.events {
                if let Some(doc) = events.container(uri, &fetched, deadline).await? {
                    return Ok(doc);
                }
                // A cut-off body never confirms that an established feed emptied.
                let feed = crate::event::service::feed_like(&fetched) && !fetched.truncated;
                if let Some(doc) = events.disappeared(uri, fetched.status, feed).await? {
                    return Ok(doc);
                }
            }
            if fetched.status == 304 {
                return Err(super::NotModified.into());
            }
            let observation = crate::telemetry::last_fetch();
            // Past the page cap: a page cut where its long head closes, or a whole page within the
            // head budget (`egress::page_fits`).
            anyhow::ensure!(
                super::egress::page_fits(&fetched),
                "non-feed page exceeds 1 MB"
            );
            let input_uri = uri.to_owned();
            let input = fetched.clone();
            let ttl = self.config.ttl_secs;
            let mut extracted =
                super::html::blocking(move || extract(&input_uri, &input, ttl)).await?;
            if extracted.doc.status == Status::Unreachable {
                if let Some(doc) =
                    crate::sources::wall_hooks(&self.egress, uri, ttl, deadline).await
                {
                    return Ok(doc);
                }
            }
            // Later steps may replace the document; the page's own candidates stay.
            let candidates = std::mem::take(&mut extracted.doc.alias_candidates);
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
            // A service the page names about itself (WordPress, Ghost) may say more.
            extracted.doc =
                crate::sources::page_hooks(&self.egress, uri, &fetched, extracted.doc, deadline)
                    .await;
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
            extracted.doc.alias_candidates = candidates;
            // A page hook's card keeps its source's facts.
            let attributes = if crate::sources::by_name(&extracted.doc.source).is_some() {
                crate::sources::attributes()
            } else {
                ATTRIBUTES
            };
            let doc = extracted.doc.bounded(attributes)?;
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
    let mut out = Extracted {
        doc,
        oembed: None,
        title: None,
        description: None,
    };
    if Url::parse(uri).is_ok_and(|url| {
        super::interstitial::inspect(&url, fetched) != super::interstitial::Page::Content
    }) {
        out.doc.status = Status::Unreachable;
        out.doc.aliases.clear();
        return out;
    }
    if fetched.permanent_redirect && fetched.url.as_str() != uri {
        add_alias(&mut out.doc, fetched.url.to_string());
        out.doc.alias_candidates.push(fetched.url.to_string());
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
            out.doc.http_status = Some(fetched.status);
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
            apply_jsonld(&mut out.doc, &v, &fetched.url, true, true);
        }
        out.doc.event.read_from(fetched);
        return out;
    }
    let html = crate::resolver::html::document(&text);
    // JSON-LD stays document-wide (plugins put it in the body), but only the chosen entity
    // may carry claims (url/@id, sameAs, identifiers): from a head script, as with head meta
    // and links (its main-entity url is the publisher's canonical declaration), or from a
    // body script that names this page or its head canonical.
    let scripts: Vec<(Value, bool)> = html
        .select(&selector("script[type='application/ld+json']"))
        .filter_map(|script| {
            let head = script
                .ancestors()
                .take(super::html::MAX_ANCESTORS)
                .any(|a| a.value().as_element().is_some_and(|e| e.name() == "head"));
            // The script's text as served: `inner_html` would escape it as HTML (`&` → `&amp;`).
            crate::event::page::lenient_json(&script.text().collect::<String>()).map(|mut v| {
                decode_json_strings(&mut v);
                (v, head)
            })
        })
        .collect();
    let page: Vec<String> = [uri.to_owned(), fetched.url.to_string()]
        .into_iter()
        .chain(
            html.select(&selector("head link[rel~=canonical][href]"))
                .filter_map(|l| origin_url(&fetched.url, l.value().attr("href")?)),
        )
        .filter_map(|u| entity_identity(&u))
        .collect();
    // An organization or person whose own url/@id is this (non-root) page is the page's subject
    // (an artist page, a profile), not a publisher helper: it outranks a site-wide app node.
    let subject = |v: &Value| {
        let named = || {
            ["url", "@id"]
                .into_iter()
                .filter_map(|f| v.get(f).and_then(Value::as_str))
                .filter_map(|u| origin_url(&fetched.url, u).and_then(|u| entity_identity(&u)))
                .any(|id| page.contains(&id))
        };
        if v.is_object() && type_priority(v) == 20 && fetched.url.path() != "/" && named() {
            100
        } else {
            entity_priority(v)
        }
    };
    // Among equals, the script holding the page's own node (an event whose url is the page
    // over another event the page lists).
    let rank = |v: &Value| (subject(v), has_own_node(v, &fetched.url));
    if let Some(best) = (1..scripts.len()).fold((!scripts.is_empty()).then_some(0), |best, i| {
        best.filter(|b| rank(&scripts[i].0) <= rank(&scripts[*b].0))
            .or(Some(i))
    }) {
        let (entity, head) = &scripts[best];
        let named = entity_urls(entity)
            .filter_map(|u| origin_url(&fetched.url, u).and_then(|u| entity_identity(&u)))
            .collect::<Vec<_>>();
        let claims = *head || named.iter().any(|id| page.contains(id));
        apply_jsonld(&mut out.doc, entity, &fetched.url, claims, *head);
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
            crate::group_keys::add(&mut out.doc, "cites_doi", &doi);
        }
    }
    if let Some(isbn) = meta.get("citation_isbn") {
        crate::group_keys::add(&mut out.doc, "cites_isbn", isbn);
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
    // Only the first head canonical is an alias candidate, beside a permanent redirect's target.
    let mut canonical_seen = false;
    for link in html.select(&selector("head link")) {
        if let Some(href) = link.value().attr("href") {
            if link
                .value()
                .attr("rel")
                .is_some_and(|s| s.split_whitespace().any(|r| r == "canonical"))
            {
                if let Some(alias) = origin_url(&fetched.url, href) {
                    add_alias(&mut out.doc, alias.clone());
                    if !canonical_seen {
                        canonical_seen = true;
                        out.doc.alias_candidates.push(alias);
                    }
                }
            }
            let alternate = link
                .value()
                .attr("rel")
                .is_some_and(|s| s.split_whitespace().any(|r| r == "alternate"));
            if alternate
                && !out.doc.attributes.contains_key("feed")
                && link
                    .value()
                    .attr("type")
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/rss+xml"))
            {
                if let Some(feed) = origin_url(&fetched.url, href) {
                    out.doc.attributes.insert("feed".into(), feed);
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
    // The performers' provenance is this fetch's, whatever base the parsers were handed.
    out.doc.event.read_from(fetched);
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
/// Text from JSON inside an HTML page's `<script>` (JSON-LD, embedded state) with its character
/// references decoded once. The HTML parser decodes `<title>`, text and attribute values itself
/// but leaves a script alone, and publishers HTML-escape JSON-LD strings: `&amp;` becomes `&`, a
/// double-escaped `&amp;amp;` becomes `&amp;`, never `&`. Only references ended by `;` count
/// (named, decimal or hex): `?id=5&currency=EUR` stays as it is.
pub(crate) fn decoded(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match reference(tail) {
            Some((text, used)) => {
                out.push_str(&text);
                rest = &tail[used..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}
/// The character reference `tail` starts with (`&name;`, `&#n;`, `&#xh;`) and its length.
/// The characters HTML reads numeric references 128–159 as (Windows-1252); an unassigned one
/// stays a control character.
const WINDOWS_1252: [char; 32] = [
    '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}', '\u{8f}',
    '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{17e}', '\u{178}',
];
fn reference(tail: &str) -> Option<(String, usize)> {
    let end = tail[1..]
        .char_indices()
        .take(40)
        .find(|(_, c)| *c == ';')?
        .0
        + 1;
    let body = &tail[1..end];
    let text = if let Some(number) = body.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                u32::from_str_radix(hex, 16).ok()?
            }
            None if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => {
                number.parse().ok()?
            }
            _ => return None,
        };
        // As HTML reads them: 128–159 name Windows-1252 characters, and a reference to NUL, a
        // surrogate or another control character stays literal text.
        let code = WINDOWS_1252
            .get(code.wrapping_sub(0x80) as usize)
            .filter(|_| (0x80..=0x9f).contains(&code))
            .map_or(code, |c| *c as u32);
        char::from_u32(code)
            .filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))?
            .to_string()
    } else {
        if body.is_empty() || !body.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let &(first, second) = markup5ever::data::NAMED_ENTITIES.get(&format!("{body};"))?;
        [first, second]
            .into_iter()
            .filter(|c| *c != 0)
            .filter_map(char::from_u32)
            .collect()
    };
    (!text.is_empty()).then_some((text, end + 1))
}
/// Decode every string of JSON read from a page's `<script>`, once (`decoded`). JSON fetched as
/// a body or from an API is never decoded: its strings are the publisher's text as it is.
pub(crate) fn decode_json_strings(v: &mut Value) {
    fn walk(v: &mut Value, depth: usize) {
        if depth > 64 {
            return;
        }
        match v {
            Value::String(s) if s.contains('&') => *s = decoded(s),
            Value::Array(a) => a.iter_mut().for_each(|v| walk(v, depth + 1)),
            Value::Object(m) => m.values_mut().for_each(|v| walk(v, depth + 1)),
            _ => {}
        }
    }
    walk(v, 0)
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
        .and_then(|d| crate::event::time::bounded(d.to_utc()))
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
/// Another URL the page names for itself (canonical, og:url, its JSON-LD `url`, a permanent
/// redirect), listed in `details.aliases`; never grouping evidence. Only the head canonical and
/// a permanent redirect are alias candidates (`alias_candidates`).
fn add_alias(doc: &mut ResourceMeta, alias: String) {
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
    true
}
/// Whether `v` holds a node whose url (else @id) is the page (`event::page::owns_page`).
fn has_own_node(v: &Value, base: &Url) -> bool {
    if let Some(values) = v.as_array() {
        return values.iter().take(16).any(|v| has_own_node(v, base));
    }
    crate::event::page::owns_page(v, base)
        || ["mainEntity", "@graph"]
            .into_iter()
            .filter_map(|k| v.get(k))
            .any(|v| has_own_node(v, base))
}
/// `claims` gates identity-bearing fields (aliases from url/@id/sameAs, typed identifiers):
/// they come only from an entity that is the page's own. `vouched`: a node naming no page is
/// the page's own (a head script's, or the main entity of the page's own node); performers
/// come from such a node or one naming the page itself, never through a sibling's claims.
fn apply_jsonld(doc: &mut ResourceMeta, v: &Value, base: &Url, claims: bool, vouched: bool) {
    if let Some(values) = v.as_array() {
        // A page's article/event outranks its publisher or breadcrumb helper nodes, and among
        // equals the page's own node (its url is the page) outranks another event it lists.
        // Extract one entity so unrelated graph nodes cannot donate misleading fields.
        let rank = |v: &Value| (entity_priority(v), crate::event::page::owns_page(v, base));
        if let Some(mut best) = values.first() {
            for value in values.iter().skip(1) {
                if rank(value) > rank(best) {
                    best = value;
                }
            }
            apply_jsonld(doc, best, base, claims, vouched);
        }
        return;
    }
    let own = match crate::event::page::names_page(v, base) {
        Some(own) => own,
        None => vouched,
    };
    if let Some(v) = v.get("mainEntity") {
        apply_jsonld(doc, v, base, claims, own);
        if doc.source != "web" {
            return;
        }
    }
    if let Some(v) = v.get("@graph") {
        apply_jsonld(doc, v, base, claims, vouched);
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
    for (field, attribute) in [("duration", "duration"), ("isrcCode", "isrc")] {
        if let Some(value) = v.get(field).and_then(Value::as_str) {
            doc.attributes
                .entry(attribute.into())
                .or_insert_with(|| value.into());
        }
    }
    // A recording's or album's artists and album, as music pages state them.
    let names = |value: &Value| -> Option<String> {
        fn one(v: &Value) -> Option<&str> {
            v.as_str().or_else(|| v["name"].as_str()).map(str::trim)
        }
        let names: Vec<&str> = match value {
            Value::Array(all) => all.iter().filter_map(one).collect(),
            v => one(v).into_iter().collect(),
        };
        let names: Vec<&str> = names
            .into_iter()
            .filter(|n| !n.is_empty())
            .take(8)
            .collect();
        (!names.is_empty()).then(|| names.join(" & "))
    };
    for (field, attribute) in [("byArtist", "artist"), ("inAlbum", "album")] {
        if let Some(value) = v.get(field).and_then(names) {
            doc.attributes.entry(attribute.into()).or_insert(value);
        }
    }
    if let Some(date) = v.get("datePublished").and_then(Value::as_str) {
        doc.attributes
            .entry("published".into())
            .or_insert_with(|| date.trim().into());
    }
    // An event's performers, as the page names them (the event service parses them too): from
    // the page's own node only (its url, else its @id, is the page; or it names none and is
    // vouched for), never from another event the page describes, nor through a sibling.
    if doc.event.performers.is_empty() && doc.type_path.iter().any(|t| t == "Event") && own {
        doc.event.performers = crate::event::page::performers(v, base);
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
    fn a_page_names_its_feed_in_its_head() {
        let page = |body: &str| Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: Url::parse("https://darknetdiaries.com/episode/180/").unwrap(),
            status: 200,
            retry_after_secs: 60,
            location: None,
            content_type: "text/html".into(),
            body: body.as_bytes().to_vec(),
        };
        let feed = |body: &str| {
            extract("https://darknetdiaries.com/episode/180/", &page(body), 3600)
                .doc
                .attributes
                .get("feed")
                .cloned()
        };
        // On any host; the first RSS alternate only.
        assert_eq!(
            feed(r#"<head><link type="application/rss+xml" rel="alternate" title="Darknet Diaries" href="https://feeds.megaphone.fm/darknetdiaries"><link rel="alternate" type="application/rss+xml" href="/comments/feed/"></head><title>180: Conti</title>"#).as_deref(),
            Some("https://feeds.megaphone.fm/darknetdiaries")
        );
        assert_eq!(
            feed(r#"<head><link rel="alternate" type="application/rss+xml" href="/feed/"></head>"#)
                .as_deref(),
            Some("https://darknetdiaries.com/feed/")
        );
        // A body link is user content; other types and relations are not feeds.
        assert_eq!(
            feed(
                r#"<head><title>T</title></head><body><link rel="alternate" type="application/rss+xml" href="https://x.example/feed"></body>"#
            ),
            None
        );
        assert_eq!(
            feed(
                r#"<head><link rel="alternate" type="application/atom+xml" href="/atom"><link rel="stylesheet" type="application/rss+xml" href="/x"></head>"#
            ),
            None
        );
    }

    #[test]
    fn only_head_canonicals_and_permanent_redirects_are_alias_candidates() {
        let page = |url: &str, permanent: bool, body: &str| Fetched {
            permanent_redirect: permanent,
            truncated: false,
            validators: Default::default(),
            url: Url::parse(url).unwrap(),
            status: 200,
            retry_after_secs: 60,
            location: None,
            content_type: "text/html".into(),
            body: body.as_bytes().to_vec(),
        };
        let claims = r#"<title>T</title><meta property="og:url" content="https://example.org/a?utm_source=og"><script type="application/ld+json">{"@type":"Article","name":"A","url":"https://example.org/a/","sameAs":"https://example.org/a?fbclid=1"}</script>"#;
        // A temporary redirect, og:url and JSON-LD claims are listed but never candidates.
        let d = extract(
            "https://example.org/a?x=1",
            &page("https://example.org/a", false, claims),
            3600,
        )
        .doc;
        assert!(d.alias_candidates.is_empty(), "{:?}", d.alias_candidates);
        assert!(d.aliases.iter().any(|a| a.contains("utm_source=og")));
        let d = extract(
            "https://example.org/a?utm_source=x",
            &page(
                "https://example.org/a",
                true,
                r#"<head><link rel="canonical" href="/a/"></head><title>T</title>"#,
            ),
            3600,
        )
        .doc;
        assert_eq!(
            d.alias_candidates,
            ["https://example.org/a", "https://example.org/a/"]
        );
        // Further head canonicals are listed as claims only: the candidates stay two.
        let many: String = (0..20)
            .map(|n| format!(r#"<link rel="canonical" href="/a/{n}">"#))
            .collect();
        let d = extract(
            "https://example.org/a?utm_source=x",
            &page(
                "https://example.org/a",
                true,
                &format!(
                    "<head><link rel=\"canonical\" href=\"/a/\">{many}</head><title>T</title>"
                ),
            ),
            3600,
        )
        .doc;
        assert_eq!(
            d.alias_candidates,
            ["https://example.org/a", "https://example.org/a/"]
        );
    }
    /// Every text of a card is entity-decoded exactly once: the HTML parser decodes the title and
    /// meta values, JSON-LD (an article, an event as Luma publishes it) is decoded here, and a
    /// double-escaped `&amp;amp;` keeps one level.
    #[test]
    fn card_text_is_entity_decoded_exactly_once() {
        let page = |head: &str| {
            let fetched = Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Default::default(),
                url: Url::parse("https://example.org/").unwrap(),
                status: 200,
                retry_after_secs: 60,
                content_type: "text/html".into(),
                location: None,
                body: head.as_bytes().into(),
            };
            extract("https://example.org/", &fetched, 100)
        };
        let meta = page(
            r#"<title>Money transfers &amp; payments</title><meta property="og:title" content="It&#39;s &quot;quick&quot; &amp;amp; more"><meta name="description" content="Tom &amp; Jerry &lt;3"><meta property="og:description" content="Bill &amp;amp; Ted"><meta property="og:site_name" content="A &amp; B">"#,
        );
        assert_eq!(meta.doc.name.as_deref(), Some(r#"It's "quick" &amp; more"#));
        assert_eq!(meta.title.as_deref(), Some("Money transfers & payments"));
        assert_eq!(meta.doc.description.as_deref(), Some("Bill &amp; Ted"));
        assert_eq!(meta.description.as_deref(), Some("Tom & Jerry <3"));
        assert_eq!(meta.doc.site, "A & B");
        let jsonld = page(
            r#"<title>Shell</title><script type="application/ld+json">{"@type":"WebPage","name":"Digital wallet for money transfers &amp; online payments","description":"It&#39;s &quot;quick&quot; &amp;amp; 1 < 2"}</script>"#,
        );
        assert_eq!(
            jsonld.doc.name.as_deref(),
            Some("Digital wallet for money transfers & online payments")
        );
        assert_eq!(
            jsonld.doc.description.as_deref(),
            Some(r#"It's "quick" &amp; 1 < 2"#)
        );
        // The www.skrill.com case: a plain `&` in JSON-LD stays one `&` (the script is read as
        // served, not re-escaped as HTML).
        let plain = page(
            r#"<script type="application/ld+json">{"@type":"WebPage","name":"Money transfers & online payments","description":"Tom & Jerry's"}</script>"#,
        );
        assert_eq!(
            (plain.doc.name.as_deref(), plain.doc.description.as_deref()),
            (
                Some("Money transfers & online payments"),
                Some("Tom & Jerry's")
            )
        );
        // An event page as Luma serves it: the same, through the event JSON-LD reader.
        let event = page(
            r#"<script type="application/ld+json">{"@type":"Event","name":"Bitcoin &amp; Kaiserschmarrn","description":"Blake Scholl (Founder &amp; CEO), &quot;Boom&quot; &amp;amp; friends","startDate":"2026-11-01T18:00:00Z"}</script>"#,
        );
        assert_eq!(event.doc.name.as_deref(), Some("Bitcoin & Kaiserschmarrn"));
        assert_eq!(
            event.doc.description.as_deref(),
            Some(r#"Blake Scholl (Founder & CEO), "Boom" &amp; friends"#)
        );
        assert_eq!(decoded("no references"), "no references");
        assert_eq!(decoded("AT&T &amp;amp;"), "AT&T &amp;");
        // Only `;`-ended references: a legacy name without one is ordinary text.
        assert_eq!(
            decoded("pay?id=5&currency=EUR&region=eu&section=2&copy"),
            "pay?id=5&currency=EUR&region=eu&section=2&copy"
        );
        assert_eq!(
            decoded(
                "&lt;b&gt; &#39;x&#x27; &quot;q&quot; &eacute;t&eacute; &nosuch; &#0; &#xD800; & ;"
            ),
            "<b> 'x' \"q\" été &nosuch; &#0; &#xD800; & ;"
        );
        // 128–159 as HTML reads them (Windows-1252); control characters stay literal.
        assert_eq!(
            decoded("&#128;5 &#150; &#x99; &#x9F;"),
            "\u{20ac}5 \u{2013} \u{2122} \u{178}"
        );
        assert_eq!(
            decoded("&#1;&#x1F;&#127;&#129;&#x9d; &#9;&#10;"),
            "&#1;&#x1F;&#127;&#129;&#x9d; \t\n"
        );
        // A page script's JSON keeps `&currency=` in its URLs and decodes its `&amp;`.
        let urls = page(
            r#"<script type="application/ld+json">{"@type":"WebPage","name":"Pay &amp; go","url":"https://example.org/pay?id=5&currency=EUR&section=2"}</script>"#,
        );
        assert_eq!(urls.doc.name.as_deref(), Some("Pay & go"));
        // JSON fetched as a body is the publisher's text as it is.
        let fetched = Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: Url::parse("https://example.org/data.json").unwrap(),
            status: 200,
            retry_after_secs: 60,
            content_type: "application/ld+json".into(),
            location: None,
            body: br#"{"@type":"WebPage","name":"Tom &amp; Jerry","description":"5&currency=EUR"}"#
                .as_slice()
                .into(),
        };
        let body = extract("https://example.org/data.json", &fetched, 100);
        assert_eq!(body.doc.name.as_deref(), Some("Tom &amp; Jerry"));
        assert_eq!(body.doc.description.as_deref(), Some("5&currency=EUR"));
    }
    /// An event page's performers, with the links the page gives for them; a bare name is a
    /// performer without links, and a nameless entry nothing.
    #[test]
    fn performers_come_from_the_pages_structured_data() {
        let body = r#"<title>Radiohead @ RAC Arena</title><script type="application/ld+json">{"@type":"MusicEvent","name":"Radiohead @ RAC Arena","url":"https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena?utm_medium=organic&utm_source=microformat","startDate":"2027-05-03T19:30:00","performer":[{"@type":"MusicGroup","name":"Radiohead","genre":["rock"],"sameAs":"https://www.songkick.com/artists/253846-radiohead?utm_medium=organic&utm_source=microformat"},{"@type":"MusicGroup","name":"Support Act","url":"/artists/1-support","sameAs":["https://open.spotify.com/artist/abc","https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"]},"Someone Else",{"@type":"Person","sameAs":"https://example.org/nobody"}]}</script>"#;
        let fetched = crate::sources::fixture::page(
            "https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena",
            "text/html",
            body.as_bytes().to_vec(),
            None,
        );
        let out = extract(
            "https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena",
            &fetched,
            100,
        );
        assert_eq!(out.doc.type_name, "MusicEvent");
        let performers: Vec<(&str, Vec<&str>)> = out
            .doc
            .event
            .performers
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    p.links.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            performers,
            [
                (
                    "Radiohead",
                    vec!["https://www.songkick.com/artists/253846-radiohead?utm_medium=organic&utm_source=microformat"]
                ),
                (
                    "Support Act",
                    // What keys the performer first: a page MusicBrainz relates, then its
                    // MusicBrainz page, then the rest (`abc` is no Spotify ID).
                    vec![
                        "https://www.songkick.com/artists/1-support",
                        "https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711",
                        "https://open.spotify.com/artist/abc"
                    ]
                ),
                ("Someone Else", vec![]),
            ]
        );
    }
    /// Another event the page describes beside its own never donates its performers.
    /// The performers record where they were read: the fetch's final address, which judges whose
    /// service the page is (`music_sources`), not the link that was asked for.
    #[test]
    fn performers_record_the_address_the_page_came_from() {
        let body = r#"<title>Gig</title><script type="application/ld+json">{"@type":"MusicEvent","name":"Gig","startDate":"2027-05-03T19:30:00","performer":{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://www.songkick.com/artists/253846-radiohead"}}</script>"#;
        let asked = "https://www.songkick.com/concerts/8-redirected-off-site";
        for (from, after) in [
            (asked, asked),
            ("https://evil.example/gig", "https://evil.example/gig"),
        ] {
            let fetched =
                crate::sources::fixture::page(from, "text/html", body.as_bytes().to_vec(), None);
            let out = extract(asked, &fetched, 100);
            assert_eq!(out.doc.event.performers.len(), 1);
            assert_eq!(out.doc.event.performers_from.as_deref(), Some(after));
        }
    }
    #[test]
    fn another_events_performers_never_donate() {
        let body = r#"<title>Venue</title><script type="application/ld+json">{"@type":"MusicEvent","name":"Tonight","url":"https://venue.example/tonight","startDate":"2027-05-03T19:30:00"}</script><script type="application/ld+json">{"@type":"MusicEvent","name":"Next week","url":"https://venue.example/next-week","startDate":"2027-05-10T19:30:00","performer":[{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"}]}</script>"#;
        let fetched = crate::sources::fixture::page(
            "https://venue.example/tonight",
            "text/html",
            body.as_bytes().to_vec(),
            None,
        );
        let out = extract("https://venue.example/tonight", &fetched, 100);
        assert_eq!(out.doc.name.as_deref(), Some("Tonight"));
        assert!(
            out.doc.event.performers.is_empty(),
            "{:?}",
            out.doc.event.performers
        );
        // The page's only event node is another event (a body script, its url not the page):
        // the card takes it, but not its performers.
        let body = r#"<html><head><title>Venue</title></head><body><p>Shows</p><script type="application/ld+json">{"@type":"MusicEvent","name":"Next week","url":"https://venue.example/next-week","startDate":"2027-05-10T19:30:00","performer":[{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"}]}</script></body></html>"#;
        let fetched = crate::sources::fixture::page(
            "https://venue.example/tonight",
            "text/html",
            body.as_bytes().to_vec(),
            None,
        );
        let out = extract("https://venue.example/tonight", &fetched, 100);
        assert_eq!(out.doc.type_name, "MusicEvent");
        assert!(
            out.doc.event.performers.is_empty(),
            "{:?}",
            out.doc.event.performers
        );
    }
    /// Performers come from a node that is the page's own by itself, never through a sibling:
    /// in a body script's graph listing another event before the page's own, the page's own
    /// is the card and nothing of the other's performers is.
    #[test]
    fn a_sibling_never_vouches_for_another_events_performers() {
        let radiohead = r#"[{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"}]"#;
        let extract_body = |graph: String| {
            let body = format!(
                r#"<html><head><title>Venue</title></head><body><p>Shows</p><script type="application/ld+json">{{"@context":"https://schema.org","@graph":{graph}}}</script></body></html>"#
            );
            let fetched = crate::sources::fixture::page(
                "https://venue.example/tonight",
                "text/html",
                body.into_bytes(),
                None,
            );
            extract("https://venue.example/tonight", &fetched, 100).doc
        };
        let names = |doc: &ResourceMeta| -> Vec<String> {
            doc.event
                .performers
                .iter()
                .map(|p| p.name.clone())
                .collect()
        };
        let next_week = format!(
            r#"{{"@type":"MusicEvent","name":"Next week","url":"https://venue.example/next-week","startDate":"2027-05-10T19:30:00","performer":{radiohead}}}"#
        );
        // [B (another event, performers), A (the page's own, none)]: A is the card, no performers.
        let doc = extract_body(format!(
            r#"[{next_week},{{"@type":"MusicEvent","name":"Tonight","url":"https://venue.example/tonight","startDate":"2027-05-03T19:30:00"}}]"#
        ));
        assert_eq!(doc.name.as_deref(), Some("Tonight"));
        assert!(names(&doc).is_empty(), "{:?}", names(&doc));
        // A's own performers, when it names them.
        let doc = extract_body(format!(
            r#"[{next_week},{{"@type":"MusicEvent","name":"Tonight","url":"https://venue.example/tonight","startDate":"2027-05-03T19:30:00","performer":"Support"}}]"#
        ));
        assert_eq!(names(&doc), ["Support"]);
        // [B, a page node naming the page]: B is the card (an event outranks a page node), and
        // the page node's claim doesn't make B's performers the page's.
        let doc = extract_body(format!(
            r#"[{next_week},{{"@type":"WebPage","url":"https://venue.example/tonight"}}]"#
        ));
        assert_eq!(doc.name.as_deref(), Some("Next week"));
        assert!(names(&doc).is_empty(), "{:?}", names(&doc));
        // A page node naming the page beside an event naming none vouches for nothing in a
        // body script; the event as the page node's main entity is the page's.
        let anonymous = format!(
            r#"{{"@type":"MusicEvent","name":"Tonight","startDate":"2027-05-03T19:30:00","performer":{radiohead}}}"#
        );
        let doc = extract_body(format!(
            r#"[{{"@type":"WebPage","url":"https://venue.example/tonight"}},{anonymous}]"#
        ));
        assert!(names(&doc).is_empty(), "{:?}", names(&doc));
        let doc = extract_body(format!(
            r#"[{{"@type":"WebPage","url":"https://venue.example/tonight","mainEntity":{anonymous}}}]"#
        ));
        assert_eq!(names(&doc), ["Radiohead"]);
        // A head script's event naming no page is the page's.
        let body = format!(
            r#"<html><head><title>Venue</title><script type="application/ld+json">{anonymous}</script></head><body></body></html>"#
        );
        let fetched = crate::sources::fixture::page(
            "https://venue.example/tonight",
            "text/html",
            body.into_bytes(),
            None,
        );
        let doc = extract("https://venue.example/tonight", &fetched, 100).doc;
        assert_eq!(names(&doc), ["Radiohead"]);
    }
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
    fn the_pages_own_artist_outranks_a_site_wide_app_node() {
        let fetch = |url: &str, body: Vec<u8>| Fetched {
            permanent_redirect: false,
            truncated: false,
            url: Url::parse(url).unwrap(),
            body,
            content_type: "text/html".into(),
            status: 200,
            location: None,
            retry_after_secs: 60,
            validators: Default::default(),
        };
        // TIDAL's artist page: its own MusicGroup, and a graph with the site's Organization,
        // WebSite and WebApplication (all named "TIDAL").
        let path = format!(
            "{}/tests/fixtures/web/tidal-artist-64518.html",
            env!("CARGO_MANIFEST_DIR")
        );
        let url = "https://tidal.com/artist/64518";
        let out = extract(url, &fetch(url, std::fs::read(path).unwrap()), 100);
        assert_eq!(
            (out.doc.type_name.as_str(), out.doc.name.as_deref()),
            ("MusicGroup", Some("Radiohead"))
        );
        // A card says what TIDAL's own JSON-LD says, not its share text.
        let mut artist = out.doc.clone();
        artist.status = crate::model::Status::Ok;
        crate::music::type_card(url, None, &mut artist);
        assert_eq!(artist.description, None);
        for (url, file, description) in [
            (
                "https://tidal.com/track/534050211",
                "tidal-track-534050211.html",
                "Queen · A Night At The Opera (Deluxe Edition) · 5:55",
            ),
            (
                "https://tidal.com/album/495402659",
                "tidal-album-495402659.html",
                "Tame Impala & JENNIE · 2025",
            ),
        ] {
            let path = format!("{}/tests/fixtures/web/{file}", env!("CARGO_MANIFEST_DIR"));
            let mut doc = extract(url, &fetch(url, std::fs::read(path).unwrap()), 100).doc;
            doc.status = crate::model::Status::Ok;
            crate::music::type_card(url, None, &mut doc);
            assert_eq!(doc.description.as_deref(), Some(description), "{url}");
            assert_eq!(doc.site, "TIDAL");
        }
        // A homepage's Organization naming the site root is its publisher, not its subject.
        let home = r#"<head><script type="application/ld+json">{"@type":"Organization","@id":"https://shop.example/","url":"https://shop.example/","name":"Shop Ltd"}</script><script type="application/ld+json">{"@type":"WebPage","url":"https://shop.example/","name":"Welcome"}</script></head>"#;
        let out = extract(
            "https://shop.example/",
            &fetch("https://shop.example/", home.as_bytes().to_vec()),
            100,
        );
        assert_eq!(out.doc.type_name, "WebPage");
        // An organization naming another page stays a helper.
        let other = r#"<head><script type="application/ld+json">{"@type":"Organization","url":"https://shop.example/","name":"Shop Ltd"}</script><script type="application/ld+json">{"@type":"WebApplication","name":"App"}</script></head>"#;
        let out = extract(
            "https://shop.example/about",
            &fetch("https://shop.example/about", other.as_bytes().to_vec()),
            100,
        );
        assert_eq!(out.doc.type_name, "WebApplication");
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
                .aliases
                .iter()
                .filter(|t| !t.starts_with("https://victim.example/post"))
                .cloned()
                .collect::<Vec<_>>()
        };
        // A comment's canonical/oEmbed links sit in the body and are ignored.
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

        // A second ld+json node in the body outranks the page's WebPage node, but names
        // nothing (or another page), so it mints no claims; its card fields still apply.
        for injected in [
            r#"{"@type":"NewsArticle","headline":"Injected","sameAs":"https://attacker.example/a"}"#,
            r#"{"@type":"NewsArticle","url":"https://attacker.example/a","sameAs":"https://attacker.example/b","identifier":"https://doi.org/10.5555/evil"}"#,
        ] {
            let out = page(&format!(
                r#"<head><title>Post</title><script type="application/ld+json">{{"@type":"WebPage","name":"Post"}}</script></head><body><p>Comment</p><script type="application/ld+json">{injected}</script></body>"#
            ));
            assert!(claims(&out).is_empty(), "{injected}: {:?}", claims(&out));
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
                claims(&out).contains(&"https://doi.org/10.1234/abc".to_owned()),
                "{head}{body}: {:?}",
                claims(&out)
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
            (d.type_name.clone(), d.aliases)
        };
        // Google's Article shape names the page via mainEntityOfPage (URL or WebPage).
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
                keys.contains(&"https://doi.org/10.1234/abc".to_owned()),
                "{page}: {keys:?}"
            );
        }
        let (_, keys) = extract_keys(
            r#"<head><title>Story</title></head><body><p>Text</p><script type="application/ld+json">{"@type":"NewsArticle","mainEntityOfPage":"https://elsewhere.example/x","sameAs":"https://doi.org/10.1234/abc"}</script></body>"#,
        );
        assert!(!keys.contains(&"https://doi.org/10.1234/abc".to_owned()));
        // A body Article outranks the head WebPage for the card, and the head's claims
        // still apply; the body node, naming nothing, adds none.
        let (kind, keys) = extract_keys(
            r#"<head><title>Story</title><script type="application/ld+json">{"@type":"WebPage","sameAs":"https://doi.org/10.1234/head"}</script></head><body><p>Text</p><script type="application/ld+json">{"@type":"NewsArticle","headline":"Story","sameAs":"https://doi.org/10.5555/body"}</script></body>"#,
        );
        assert_eq!(kind, "NewsArticle");
        assert!(
            keys.contains(&"https://doi.org/10.1234/head".to_owned()),
            "{keys:?}"
        );
        assert!(
            !keys.contains(&"https://doi.org/10.5555/body".to_owned()),
            "{keys:?}"
        );
        // A head Organization (below WebPage) donates nothing under a body Article: no
        // homepage canonical and no site-wide Wikidata key.
        let (kind, keys) = extract_keys(
            r#"<head><title>Story</title><script type="application/ld+json">{"@type":"Organization","url":"https://news.example/","sameAs":"https://www.wikidata.org/wiki/Q42"}</script></head><body><p>Text</p><script type="application/ld+json">{"@type":"NewsArticle","headline":"Story"}</script></body>"#,
        );
        assert_eq!(kind, "NewsArticle");
        assert!(
            !keys.contains(&"https://news.example/".to_owned()),
            "{keys:?}"
        );
        assert!(keys.iter().all(|k| !k.contains("wikidata")), "{keys:?}");
        // As the chosen entity (no body script), the same head node keeps its claims.
        let (_, keys) = extract_keys(
            r#"<head><title>Story</title><script type="application/ld+json">{"@type":"Organization","url":"https://news.example/","sameAs":"https://www.wikidata.org/wiki/Q42"}</script></head><body><p>Text</p></body>"#,
        );
        assert!(
            keys.contains(&"https://www.wikidata.org/wiki/Q42".to_owned()),
            "{keys:?}"
        );
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
