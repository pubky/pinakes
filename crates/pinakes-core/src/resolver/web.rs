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
    // What MusicBrainz says about its entity (`music_sources::ABOUT_FACTS`).
    "disambiguation",
    "artist_type",
    "country_code",
    "area",
    "begin_date",
    "end_date",
    "release_type",
    "first_release_date",
    "genre",
    // A job posting's facts (`kinds::jobs::FACTS`).
    "hiring_organization",
    "hiring_organization_url",
    "employment_type",
    "remote",
    "remote_regions",
    "salary_min",
    "salary_max",
    "salary_currency",
    "salary_unit",
    "date_posted",
    "valid_through",
    "apply_url",
    // A product's facts (`kinds::products::FACTS`).
    "gtin",
    "brand",
    "sku",
    "mpn",
    "price",
    "price_currency",
    "availability",
    "price_at",
    "seller",
    "asin",
    // A question's facts (`kinds::questions::FACTS`).
    "answer_count",
    "accepted_answer",
    "score",
    "closed_reason",
    "duplicate_of",
    // A recipe's facts (`kinds::cooking::FACTS`).
    "total_minutes",
    "recipe_yield",
    "ingredients",
    "cuisine",
    "recipe_category",
    "diet",
    "rating",
    "rating_count",
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
    /// A job posting's workplace on the map (`EventService::workplace`), before `deadline`; a
    /// slow or unavailable geocoder leaves the posting's own facts.
    pub(crate) async fn place_workplace(
        &self,
        doc: &mut ResourceMeta,
        deadline: tokio::time::Instant,
    ) {
        let Some(events) = self
            .events
            .as_ref()
            .filter(|_| crate::kinds::jobs::is_posting(doc))
        else {
            return;
        };
        let remaining = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .saturating_sub(Duration::from_millis(25));
        if !matches!(
            tokio::time::timeout(remaining, events.workplace(doc)).await,
            Ok(Ok(()))
        ) {
            doc.event.warnings.push("venue_unavailable".into());
        }
    }
    /// A Wikipedia article's card from its REST summary; `None` falls back to the page (a
    /// missing article, a page outside the article namespace, a failed request).
    async fn wikipedia_card(
        &self,
        uri: &str,
        article: &crate::wikipedia::Article,
    ) -> Option<ResourceMeta> {
        let options = super::egress::FetchOptions::json();
        let fetched = match self
            .egress
            .fetch_with(&crate::wikipedia::summary_url(article), &options)
            .await
        {
            Ok(fetched) if fetched.status == 200 => fetched,
            Ok(_) => return None,
            Err(error) => {
                tracing::debug!(
                    kind = crate::resolver::error_class(&error),
                    "Wikipedia summary failed; reading the page"
                );
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
        let started = tokio::time::Instant::now();
        let base = Some(self.config.musicbrainz_base.as_str())
            .filter(|b| !b.is_empty())
            .unwrap_or(musicbrainz::BASE);
        let ttl = self.config.ttl_secs;
        let card = if let Some((kind, mbid, _)) = musicbrainz::entity_link(uri) {
            match musicbrainz::entity(&self.egress, base, kind, &mbid).await? {
                Some(entity) => {
                    let mut card =
                        crate::music_sources::musicbrainz_card(uri, Some(&entity), None, ttl);
                    if self.config.music_enrichment {
                        self.enrich_music(&mut card, &entity, base, started).await;
                    }
                    card
                }
                None => return Ok(None),
            }
        } else if let Some((isrc, _)) = musicbrainz::isrc_link(uri) {
            let recordings = musicbrainz::isrc(&self.egress, base, &isrc)
                .await?
                .unwrap_or_default();
            // One recording's card; duplicates that agree as one recording take the first's.
            let entity = match recordings.as_slice() {
                [one] => {
                    musicbrainz::entity(
                        &self.egress,
                        base,
                        crate::hubs::MbKind::Recording,
                        &one.mbid,
                    )
                    .await?
                }
                [] => None,
                several => crate::music_sources::agreeing(&self.egress, base, several)
                    .await?
                    .and_then(|entities| {
                        entities
                            .into_iter()
                            .min_by_key(|e| e.title.as_ref().map_or(usize::MAX, String::len))
                    }),
            };
            let mut card =
                crate::music_sources::musicbrainz_card(uri, entity.as_ref(), Some(&isrc), ttl);
            // Of agreeing duplicates, the card states no length of its own: the
            // `musicbrainz_entity` source states the one recording's that stands for them, and
            // the plainest title's may round to another second (two `duration` values).
            if recordings.len() > 1 {
                card.attributes.remove("duration");
            }
            card
        } else {
            return Ok(None);
        };
        Ok(Some(card.bounded(ATTRIBUTES)?))
    }
    /// A MusicBrainz entity's image and description with their credits
    /// (`music_enrichment`), within `limits.music_enrichment_timeout_ms` and the card's own
    /// read (`started`, within `queue::RESOLVE_TIMEOUT`): a part still unread past either is
    /// unread, and the card keeps what it has stored of it; what the other part found stays
    /// (`music_enrichment::enrich_within`). Never fails the card.
    async fn enrich_music(
        &self,
        card: &mut ResourceMeta,
        entity: &crate::hubs::musicbrainz::Entity,
        base: &str,
        started: tokio::time::Instant,
    ) {
        let deadline = (tokio::time::Instant::now()
            + Duration::from_millis(self.config.music_enrichment_timeout_ms))
        .min(started + crate::queue::RESOLVE_TIMEOUT.saturating_sub(Duration::from_secs(2)));
        crate::music_enrichment::enrich_within(
            &self.egress,
            base,
            entity,
            self.config.music_image_width,
            Some(deadline),
        )
        .await
        .apply(card, self.config.music_enrichment_retry_secs);
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
        // Amazon is never fetched: its links' cards come from the link alone.
        if let Some(doc) = crate::kinds::products::amazon(uri, self.config.ttl_secs) {
            return Ok(doc);
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
                super::html::blocking(move || extract(&input_uri, &input, ttl)).await??;
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
            self.place_workplace(&mut extracted.doc, deadline).await;

            if self
                .events
                .as_ref()
                .is_some_and(|e| e.config.adapters.enabled)
            {
                if let Some(doc) =
                    crate::event::adapters::service::wikipedia(uri, &fetched, self, deadline)
                        .await?
                {
                    extracted.doc = doc;
                }
            }

            if let Some(cards) = &self.cards {
                // Boxed: keeps the enrichment state machine off callers' debug stacks.
                extracted.doc = Box::pin(super::cards::enrich(
                    uri,
                    &fetched,
                    extracted.doc,
                    extracted.alternates.take(),
                    &self.egress,
                    cards,
                    self.config.discover_feeds,
                ))
                .await?;
            }
            // A service the page names about itself (WordPress, Ghost) may say more.
            extracted.doc = crate::sources::page_hooks(
                &self.egress,
                uri,
                &as_read(&fetched),
                extracted.doc,
                deadline,
            )
            .await;
            // Account for the title fallback when deciding whether another request is
            // needed, while retaining oEmbed's established precedence if fetched.
            let mut candidate = extracted.doc.clone();
            fill(&mut candidate.name, extracted.title.clone());
            // A link that carries a credential is never sent to an oEmbed provider.
            if let Some(endpoint) = extracted.oembed.take().filter(|endpoint| {
                if crate::capability::is_capability(uri)
                    || crate::capability::is_capability_url(&fetched.url)
                    || crate::capability::names_capability(endpoint)
                {
                    return false;
                }
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
                            apply_oembed(&mut extracted.doc, &value, &read_at(&response.url));
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
    /// The metadata endpoints the page's head advertises (`cards::alternates`), when its body
    /// was read as HTML here.
    pub alternates: Option<super::cards::Alternates>,
}
/// `Err` when the page is too complex to parse (`html::TooComplex`).
pub fn extract(uri: &str, fetched: &Fetched, ttl: u64) -> anyhow::Result<Extracted> {
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
        alternates: None,
    };
    // The wall check's parse of the body, when it needed one: the card is read from the same
    // tree below.
    let mut walled = None;
    if let Ok(url) = Url::parse(uri) {
        let page = match super::interstitial::unparsed(&url, fetched) {
            Some(page) => page,
            None => {
                let text = String::from_utf8_lossy(&fetched.body);
                let html = super::html::document(&text)?;
                let page = super::interstitial::parsed(fetched, &text, &html);
                walled = Some((text, html));
                page
            }
        };
        if page != super::interstitial::Page::Content {
            out.doc.status = Status::Unreachable;
            out.doc.aliases.clear();
            return Ok(out);
        }
    }
    // A redirect to a link that carries a credential is followed, never kept as an alias.
    if fetched.permanent_redirect
        && fetched.url.as_str() != uri
        && !crate::capability::is_capability_url(&fetched.url)
    {
        add_alias(&mut out.doc, fetched.url.to_string());
        out.doc.alias_candidates.push(fetched.url.to_string());
    }
    match fetched.status {
        404 | 410 | 451 => {
            out.doc.status = Status::Missing;
            return Ok(out);
        }
        401 | 403 => {
            out.doc.status = Status::Unreachable;
            return Ok(out);
        }
        200..=299 => {}
        _ => {
            out.doc.status = Status::Error;
            out.doc.http_status = Some(fetched.status);
            return Ok(out);
        }
    }
    if fetched.content_type.starts_with("image/") {
        out.doc.set_type("ImageObject");
        out.doc.image = Some(landed(uri, fetched).to_string());
        return Ok(out);
    }
    if fetched.content_type == "application/pdf" {
        out.doc.name = landed(uri, fetched)
            .path_segments()
            .and_then(|mut s| s.next_back())
            .map(str::to_owned);
        return Ok(out);
    }
    let read = as_read(fetched);
    let fetched = read.as_ref();
    // `as_read` changes the address alone: the body is the one the wall check parsed.
    let (text, walled) = match walled {
        Some((text, html)) => (text, Some(html)),
        None => (String::from_utf8_lossy(&fetched.body), None),
    };
    // A feed, on whichever route fetched it: its own title and description.
    if let Some((name, description)) = feed(&fetched.content_type, &text) {
        out.doc.set_type("DataFeed");
        out.doc.name = name;
        out.doc.description = description;
        return Ok(out);
    }
    if fetched.content_type.ends_with("json") {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            apply_jsonld(&mut out.doc, &v, &fetched.url, true, true);
            // The document is the page's own, as a head script is: the same guard holds.
            guard_identity(&mut out.doc, &[(v, true)], &fetched.url, |_, _| true);
        }
        out.doc.event.read_from(fetched);
        return Ok(out);
    }
    let html = match walled {
        Some(html) => html,
        None => crate::resolver::html::document(&text)?,
    };
    out.alternates = Some(super::cards::alternates(&html, &fetched.url));
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
    // The page's addresses, for what names it: the one asked for, the one fetched, its head
    // canonical and og:url (an AMP page's article names the canonical).
    let pages = crate::event::page::page_addresses(uri, &fetched.url, &html);
    let (scripts, lists_events) = without_parts(scripts, &fetched.url, &pages);
    if lists_events {
        out.doc.event.not_event = Some("lists_events".into());
    }
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
        if v.is_object()
            && type_priority(v, &fetched.url) == 20
            && fetched.url.path() != "/"
            && named()
        {
            100
        } else {
            entity_priority(v, &fetched.url)
        }
    };
    // A site's own node (its WebSite, its publisher) ranks below any other: it never names the
    // page. Among equals, a script whose card would be the page's own (the node `apply_jsonld`
    // takes names the page, or names none in the head) over one whose card names another page,
    // however the script wraps it and whatever page metadata (a `WebPage`, breadcrumbs) sits
    // beside it; then the script holding a node naming the page (an event whose url is the
    // page over one naming none).
    let rank = |(v, head): &(Value, bool)| {
        (
            !site_script(v, &fetched.url),
            subject(v),
            selected(v, &fetched.url, *head).is_some_and(|(_, own)| own),
            has_own_node(v, &fetched.url),
        )
    };
    let claims = |entity: &Value, head: bool| {
        head || entity_urls(entity)
            .filter_map(|u| origin_url(&fetched.url, u).and_then(|u| entity_identity(&u)))
            .any(|id| page.contains(&id))
    };
    if let Some(best) = (1..scripts.len()).fold((!scripts.is_empty()).then_some(0), |best, i| {
        best.filter(|b| rank(&scripts[i]) <= rank(&scripts[*b]))
            .or(Some(i))
    }) {
        let (entity, head) = &scripts[best];
        apply_jsonld(
            &mut out.doc,
            entity,
            &fetched.url,
            claims(entity, *head),
            *head,
        );
        // A plugin's body Article can outrank the head's WebPage for the card; the head's
        // identity claims still apply.
        if !head {
            let head_best = scripts
                .iter()
                .filter(|(_, head)| *head)
                .fold(None, |best, (v, _)| match best {
                    Some(b)
                        if entity_priority(v, &fetched.url) <= entity_priority(b, &fetched.url) =>
                    {
                        Some(b)
                    }
                    _ => Some(v),
                });
            if let Some(v) = head_best {
                jsonld_claims(&mut out.doc, v, &fetched.url);
            }
        }
    }
    guard_identity(&mut out.doc, &scripts, &fetched.url, claims);
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
    // What names the page may also come from a `<meta>` that is a child of `<body>` itself:
    // frameworks that stream a page's metadata (React 19, Next.js) put it there, while user
    // content always sits inside the body's elements. It fills what the head left out, and never
    // claims an identity (an alias, a cited DOI): those stay the head's.
    let mut meta_card = meta.clone();
    for el in html.select(&selector("body > meta")) {
        if let (Some(k), Some(v)) = (
            el.value()
                .attr("property")
                .or_else(|| el.value().attr("name")),
            el.value().attr("content"),
        ) {
            let k = k.to_ascii_lowercase();
            if BODY_META.contains(&k.as_str()) {
                meta_card.entry(k).or_insert_with(|| v.to_string());
            }
        }
    }
    let meta_head = meta;
    let meta = meta_card;
    // The site's names, for the site suffix of the page's titles.
    let site_names: Vec<String> = [meta.get("og:site_name"), meta.get("application-name")]
        .into_iter()
        .flatten()
        .cloned()
        .chain(std::iter::once(out.doc.site.clone()))
        .collect();
    let root = matches!(fetched.url.path(), "" | "/");
    let titled = |t: &String| without_site_suffix(t, &site_names, &fetched.url, root);
    for key in ["og:title", "twitter:title"] {
        fill(&mut out.doc.name, meta.get(key).map(titled));
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
    if let Some(alias) = meta_head
        .get("og:url")
        .and_then(|s| origin_url(&fetched.url, s))
    {
        add_alias(&mut out.doc, alias);
    }
    // The page's own player, from its head alone, unless its link names one (`embed::link`).
    if crate::embed::link(uri).is_none() {
        out.doc.embed = crate::embed::from_meta(&meta_head, &fetched.url);
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
                    if !canonical_seen && !crate::capability::is_capability(&alias) {
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
        .map(|el| el.text().collect::<String>().trim().to_owned())
        .map(|t| titled(&t));
    out.description = meta.get("description").cloned();
    // The performers' provenance is this fetch's, whatever base the parsers were handed.
    out.doc.event.read_from(fetched);
    Ok(out)
}
/// The `<meta>` names a child of `<body>` may give (they name and describe the page; none
/// claims an identity).
const BODY_META: &[&str] = &[
    "og:title",
    "og:description",
    "og:image",
    "og:type",
    "og:site_name",
    "twitter:title",
    "twitter:description",
    "twitter:image",
    "twitter:image:src",
    "description",
    "application-name",
];

/// Title separators before a site suffix.
const TITLE_SEPARATORS: &[&str] = &[" • ", " | ", " - ", " – ", " — ", " · "];

/// A page title without its site suffix: the part after its last separator, when that part
/// names the site as a whole word (one of `sites`, such as its `og:site_name` or its own JSON-LD
/// name, or the head of one before a `:`, `-` or `|`; or its host's registrable label), e.g. "TFTC: A
/// Bitcoin Podcast • Listen on Fountain". A site's root page keeps its title, and so does a
/// title that would be left empty.
/// The content types only a feed is served as.
const FEED_TYPES: &[&str] = &[
    "application/rss+xml",
    "application/atom+xml",
    "application/feed+json",
];
/// Whether `body` is a feed, and its own title and description: RSS (`<rss>`), RSS 1.0 or 0.90
/// (`<rdf:RDF>` with its channel), Atom (`<feed>` in the Atom namespace) or JSON Feed (a `version`
/// on jsonfeed.org), whatever the content type says; or a body served as a feed type
/// (`FEED_TYPES`) that does not read as one, with neither.
pub(crate) fn feed(content_type: &str, body: &str) -> Option<(Option<String>, Option<String>)> {
    let body = body.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    let read = if body.starts_with('<') {
        xml_feed(body)
    } else if body.starts_with('{') && body.contains("https://jsonfeed.org/version/") {
        serde_json::from_str::<Value>(body).ok().and_then(|v| {
            v["version"]
                .as_str()
                .is_some_and(|s| s.starts_with("https://jsonfeed.org/version/"))
                .then(|| (feed_text(&v["title"]), feed_text(&v["description"])))
        })
    } else {
        None
    };
    read.or_else(|| FEED_TYPES.contains(&content_type).then_some((None, None)))
}
/// A JSON Feed's `title` or `description`: plain text, as its specification has them.
fn feed_text(v: &Value) -> Option<String> {
    v.as_str()
        .map(|s| feed_words(s, false))
        .filter(|s| !s.is_empty())
}
/// A feed field's words with its spaces collapsed. Only a field the feed's format makes HTML
/// (`html`: an Atom `type="html"` text, an RSS description) has its markup taken out; any other
/// keeps what looks like markup as the literal text it is (`Using <title> in HTML`).
fn feed_words(s: &str, html: bool) -> String {
    let text = if html {
        super::cards::plain(s)
    } else {
        s.to_owned()
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
/// An XML feed's root, then its own title and description (before its first item or entry), read
/// as a stream: a body cut at the page cap reads as well as a whole one, and the read stops early.
fn xml_feed(body: &str) -> Option<(Option<String>, Option<String>)> {
    use quick_xml::{events::Event, name::ResolveResult, NsReader};
    const ATOM: &[u8] = b"http://www.w3.org/2005/Atom";
    const RDF: &[u8] = b"http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    const RSS1: &[&[u8]] = &[
        b"http://purl.org/rss/1.0/",
        b"http://my.netscape.com/rdf/simple/0.9/",
    ];
    let mut reader = NsReader::from_str(body);
    // The element depth at which the feed's own fields sit, once the root is known.
    let mut fields = None;
    let mut path: Vec<Vec<u8>> = vec![];
    let (mut title, mut description) = (None::<String>, None::<String>);
    // The field being read: whether it is the title, whether its text is HTML, and its text.
    let mut capture: Option<(bool, bool, String)> = None;
    // An RDF document is a feed only with an RSS 1.0 (or 0.90) channel.
    let (mut rdf, mut rdf_channel) = (false, false);
    for _ in 0..4096 {
        let Ok((ns, event)) = reader.read_resolved_event() else {
            break;
        };
        let bound = |ns: &ResolveResult<'_>, want: &[u8]| matches!(ns, ResolveResult::Bound(n) if n.into_inner() == want);
        match event {
            Event::Start(e) => {
                let local = e.local_name().into_inner().to_vec();
                if path.is_empty() {
                    fields = match local.as_slice() {
                        b"rss" if matches!(ns, ResolveResult::Unbound) => Some(3),
                        b"feed" if bound(&ns, ATOM) => Some(2),
                        b"RDF" if bound(&ns, RDF) => {
                            rdf = true;
                            Some(3)
                        }
                        _ => return None,
                    };
                } else if rdf && path.len() == 1 && local == b"channel" {
                    if !RSS1.iter().any(|n| bound(&ns, n)) {
                        return None;
                    }
                    rdf_channel = true;
                }
                if matches!(local.as_slice(), b"item" | b"entry") {
                    break;
                }
                path.push(local);
                if Some(path.len()) == fields && capture.is_none() {
                    let atom = fields == Some(2);
                    let parent_ok = atom || path[1] == b"channel";
                    // An Atom text is HTML only as `type="html"`: `text` (the default) is plain,
                    // and `xhtml` is elements, whose text is all that is read. An RSS description
                    // is HTML by convention; its title is plain.
                    let html = if atom {
                        e.try_get_attribute("type")
                            .ok()
                            .flatten()
                            .is_some_and(|a| a.value.as_ref() == b"html")
                    } else {
                        path.last().is_some_and(|l| l == b"description")
                    };
                    capture = match path.last().map(Vec::as_slice) {
                        Some(b"title") if parent_ok && title.is_none() => {
                            Some((true, html, String::new()))
                        }
                        Some(b"description" | b"subtitle")
                            if parent_ok && description.is_none() =>
                        {
                            Some((false, html, String::new()))
                        }
                        _ => None,
                    };
                }
                if path.len() > 64 {
                    break;
                }
            }
            Event::Empty(e) if path.is_empty() => {
                let local = e.local_name().into_inner().to_vec();
                return match local.as_slice() {
                    b"rss" if matches!(ns, ResolveResult::Unbound) => Some((None, None)),
                    b"feed" if bound(&ns, ATOM) => Some((None, None)),
                    _ => None,
                };
            }
            Event::Text(t) => {
                if let (Some((_, _, text)), Ok(s)) = (&mut capture, t.unescape()) {
                    text.push_str(&s);
                }
            }
            Event::CData(t) => {
                if let (Some((_, _, text)), Ok(s)) = (&mut capture, t.decode()) {
                    text.push_str(&s);
                }
            }
            Event::End(_) => {
                if Some(path.len()) == fields {
                    if let Some((is_title, html, text)) = capture.take() {
                        let text = Some(feed_words(&text, html)).filter(|s| !s.is_empty());
                        if is_title {
                            title = text;
                        } else {
                            description = text;
                        }
                    }
                }
                path.pop();
                if path.is_empty() {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    fields
        .filter(|_| !rdf || rdf_channel)
        .map(|_| (title, description))
}
pub(crate) fn without_site_suffix(title: &str, sites: &[String], url: &Url, root: bool) -> String {
    let title = title.trim();
    if root {
        return title.to_owned();
    }
    let Some((head, suffix)) = TITLE_SEPARATORS
        .iter()
        .filter_map(|sep| title.rfind(sep).map(|at| (at, sep.len())))
        .max_by_key(|(at, _)| *at)
        .map(|(at, len)| (title[..at].trim_end(), title[at + len..].trim()))
    else {
        return title.to_owned();
    };
    let mut names: Vec<String> = vec![];
    for site in sites {
        let site = site.trim().to_lowercase();
        let short = site
            .split([':', '|'])
            .next()
            .and_then(|s| s.split(" - ").next())
            .unwrap_or_default()
            .trim()
            .to_owned();
        names.extend([site, short]);
    }
    // The host's registrable label (`fountain` in fountain.fm, `google` in play.google.com),
    // never a subdomain's (`open`, `news`, `blog` are words).
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if let Some(label) = psl::domain_str(&host).and_then(|d| d.split('.').next()) {
        names.push(label.to_owned());
    }
    let suffix_lower = suffix.to_lowercase();
    let word = |name: &str| {
        name.chars().count() >= 3
            && suffix_lower.match_indices(name).any(|(at, m)| {
                let before = suffix_lower[..at].chars().next_back();
                let after = suffix_lower[at + m.len()..].chars().next();
                !before.is_some_and(char::is_alphanumeric)
                    && !after.is_some_and(char::is_alphanumeric)
            })
    };
    if head.is_empty() || !names.iter().any(|n| word(n)) {
        return title.to_owned();
    }
    head.to_owned()
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
/// Where an answer fetched at `url` is read, so that its relative links (`""`, `#x`, `/og.png`)
/// resolve: `url`, unless that carries a credential; then the directory of `url` without its user
/// info, query and fragment (a segment may be a secret too, as in `/files/<id>?token=…` or
/// `/files/<id>/?token=…`), or its
/// origin when the path is a known secret. Either names no secret and is still the host that
/// served the answer, so ownership and trust checks stay on that host.
pub(crate) fn read_at(url: &Url) -> Url {
    if !crate::capability::is_capability_url(url) {
        return url.clone();
    }
    let mut base = url.clone();
    let _ = base.set_username("");
    let _ = base.set_password(None);
    base.set_query(None);
    base.set_fragment(None);
    if crate::capability::is_capability_url(&base) {
        base.set_path("/");
    } else {
        let directory = base
            .path()
            .trim_end_matches('/')
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory)
            .to_owned();
        base.set_path(&format!("{directory}/"));
    }
    base
}
/// `fetched` as a page is read: at [`read_at`] the address its fetch ended on.
pub(crate) fn as_read(fetched: &Fetched) -> std::borrow::Cow<'_, Fetched> {
    if crate::capability::is_capability_url(&fetched.url) {
        std::borrow::Cow::Owned(Fetched {
            url: read_at(&fetched.url),
            ..fetched.clone()
        })
    } else {
        std::borrow::Cow::Borrowed(fetched)
    }
}
/// The address a fetch ended on, unless it carries a credential: then the link itself stands in
/// for it, so nothing stored names it.
fn landed(uri: &str, fetched: &Fetched) -> Url {
    if !crate::capability::is_capability_url(&fetched.url) {
        return fetched.url.clone();
    }
    Url::parse(uri)
        .or_else(|_| Url::parse(&fetched.url.origin().ascii_serialization()))
        .unwrap_or_else(|_| fetched.url.clone())
}
/// Another URL the page names for itself (canonical, og:url, its JSON-LD `url`, a permanent
/// redirect), listed in `details.aliases`; never grouping evidence. Only the head canonical and
/// a permanent redirect are alias candidates (`alias_candidates`).
/// A link that carries a credential is never published as another's alias.
fn add_alias(doc: &mut ResourceMeta, alias: String) {
    if doc.aliases.len() < 4
        && alias != doc.url
        && !doc.aliases.contains(&alias)
        && !crate::capability::is_capability(&alias)
    {
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
fn entity_priority(v: &Value, base: &Url) -> u8 {
    if let Some(values) = v.as_array() {
        return values
            .iter()
            .map(|v| entity_priority(v, base))
            .max()
            .unwrap_or(0);
    }
    let nested = main_entity(v)
        .or_else(|| v.get("@graph"))
        .map(|v| entity_priority(v, base))
        .unwrap_or(0);
    type_priority(v, base).max(nested)
}
/// A node's own priority, ignoring nested entities: 110 for the page's own recipe, 100 for other
/// content, 90 for a recipe of another page, 80 for WebPage, 20 for publisher and navigation
/// helpers.
fn type_priority(v: &Value, base: &Url) -> u8 {
    let path = schema_type(v)
        .map(crate::model::type_path)
        .unwrap_or_default();
    // A recipe page's article or video wraps its recipe, never the other way round; a recipe
    // naming another page (one an article recommends) ranks below the page's content, so it
    // never displaces the page's own node, named or not. The page's own: it names the page
    // (url, else @id), or it names none and wraps nothing else.
    let recipe = path.iter().any(|s| s == "Recipe");
    let names = recipe
        .then(|| crate::event::page::names_page(v, base))
        .flatten();
    let own = if recipe
        && names.unwrap_or_else(|| v.get("mainEntity").is_none() && v.get("@graph").is_none())
    {
        110
    } else if names == Some(false) {
        90
    } else if recipe
        || path.iter().any(|s| {
            matches!(
                s.as_str(),
                "Article"
                    | "Event"
                    | "Product"
                    | "Place"
                    | "MediaObject"
                    | "JobPosting"
                    | "Question"
            )
        })
    {
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
/// Whether a JSON-LD node is the site's own rather than the page's: its breadcrumbs, its
/// `WebSite`, or an organization or person whose `url` (else `@id`) is the site's root while the
/// page is not (the publisher, the same on every page). The root must be this site's: the page's
/// host (`www.` aside) or a parent domain of it; a node naming another site's root (a directory's
/// restaurant, an author's own homepage) is about the page. Such a node never names, describes,
/// illustrates or types the page; its name may name the site.
fn site_node(v: &Value, base: &Url) -> bool {
    let path = schema_type(v)
        .map(crate::model::type_path)
        .unwrap_or_default();
    if path.iter().any(|t| t == "BreadcrumbList") {
        return true;
    }
    let website = path.iter().any(|t| t == "WebSite");
    if !website
        && !path
            .iter()
            .any(|t| matches!(t.as_str(), "Organization" | "Person"))
    {
        return false;
    }
    let named = ["url", "@id"]
        .into_iter()
        .filter_map(|f| v.get(f).and_then(Value::as_str))
        .find_map(|u| origin_url(base, u))
        .and_then(|u| Url::parse(&u).ok());
    let ours = |u: &Url| {
        let bare = |h: &str| h.trim_start_matches("www.").to_ascii_lowercase();
        let (node, page) = (
            bare(u.host_str().unwrap_or_default()),
            bare(base.host_str().unwrap_or_default()),
        );
        !node.is_empty() && (page == node || page.ends_with(&format!(".{node}")))
    };
    match named {
        // A WebSite that names no URL is the site's own.
        None => website,
        Some(u) => {
            ours(&u)
                && (website
                    || (matches!(u.path(), "" | "/")
                        && u.query().is_none()
                        && crate::event::page::names_page(v, base) != Some(true)))
        }
    }
}
/// Whether a script's JSON-LD holds only site nodes (`site_node`): one node, or every typed
/// node of its array or `@graph`, and no main entity of the page.
fn site_script(v: &Value, base: &Url) -> bool {
    // Every node of the list decides, however many there are. A node with a main entity counts,
    // typed or not.
    let all_site = |nodes: &[Value]| {
        let counted: Vec<&Value> = nodes
            .iter()
            .filter(|n| {
                schema_type(n).is_some()
                    || n.get("@graph").is_some()
                    || n.get("mainEntity").is_some()
                    || n.is_array()
            })
            .collect();
        !counted.is_empty() && counted.iter().all(|n| site_script(n, base))
    };
    if let Some(all) = v.as_array() {
        return all_site(all);
    }
    if v.get("mainEntity").is_some() {
        return false;
    }
    match graph_nodes(v) {
        Some(graph) => (schema_type(v).is_none() || site_node(v, base)) && all_site(graph),
        None => site_node(v, base),
    }
}
/// Whether a node is of a schema.org type, or a subtype of it.
fn typed(v: &Value, kind: &str) -> bool {
    schema_type(v).is_some_and(|k| crate::model::type_path(k).iter().any(|t| t == kind))
}
/// The node's main entity, where it is the page's subject: a `FAQPage`'s (its questions, one
/// or many) are parts of the page, never the page itself; a question is the page's subject as
/// another page's single main entity (a `QAPage`'s, as Stack Exchange's).
fn main_entity(v: &Value) -> Option<&Value> {
    let main = v.get("mainEntity")?;
    (!typed(v, "FAQPage")).then_some(main)
}
/// Every object node of a script's value, through its arrays, `@graph` and `mainEntity`, each
/// with whether it sits in a list of several main entities.
fn all_nodes<'a>(v: &'a Value, out: &mut Vec<(&'a Value, bool)>, depth: usize, in_list: bool) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Array(a) => a
            .iter()
            .take(256)
            .for_each(|v| all_nodes(v, out, depth + 1, in_list)),
        Value::Object(_) => {
            out.push((v, in_list));
            for key in ["@graph", "mainEntity"] {
                if let Some(nested) = v.get(key) {
                    let list =
                        in_list || (key == "mainEntity" && crate::event::page::main_list(nested));
                    all_nodes(nested, out, depth + 1, list);
                }
            }
        }
        _ => {}
    }
}
/// The page's scripts without the parts of the page: the entries of a list it holds (several
/// entries of one kind naming other pages, or listed as a page's main entity: an artist's
/// concerts, a FAQ's questions, a category's products, `event::page::listed_kinds`, read
/// across every script), and on a music object's page (a track, an album, an artist:
/// `music::parse`) its video. Left out before any script is ranked or read, so the card is the
/// page's own entity (the artist, the page) and never its first entry; a script with nothing
/// else goes. `pages`: the page's addresses (`page_identity`), which an entity naming one of
/// them is the page's own by. Pruned in place: nothing is copied. Also whether the page lists
/// events (the extractor's decision, recorded on the card as `not_event`).
fn without_parts(
    mut scripts: Vec<(Value, bool)>,
    base: &Url,
    pages: &[String],
) -> (Vec<(Value, bool)>, bool) {
    let listed = {
        let mut nodes = vec![];
        for (v, _) in &scripts {
            all_nodes(v, &mut nodes, 0, false);
        }
        let entries: Vec<&Value> = nodes
            .iter()
            .filter(|(v, in_list)| crate::event::page::entry(v, base, pages, *in_list))
            .map(|(v, _)| *v)
            .collect();
        crate::event::page::listed_kinds(&entries, base, pages)
    };
    let lists_events = listed.contains("Event");
    let music = crate::music::parse(base.as_str())
        .is_some_and(|link| link.level != crate::music::Level::Video);
    if listed.is_empty() && !music {
        return (scripts, lists_events);
    }
    let part = |v: &Value, in_list: bool| {
        (crate::event::page::entry(v, base, pages, in_list)
            && crate::event::page::listed(v, base, pages, &listed))
            || (music && typed(v, "VideoObject"))
    };
    scripts.retain_mut(|(v, _)| without(v, &part, false));
    (scripts, lists_events)
}
/// Removes the nodes `part` names from `v`, at any depth and in place (`in_list`: within a list
/// of several main entities); false when nothing is left of `v`.
fn without(v: &mut Value, part: &dyn Fn(&Value, bool) -> bool, in_list: bool) -> bool {
    match v {
        Value::Array(a) => {
            a.retain_mut(|v| without(v, part, in_list));
            !a.is_empty()
        }
        Value::Object(_) => {
            if part(v, in_list) {
                return false;
            }
            let Value::Object(o) = v else { unreachable!() };
            for key in ["@graph", "mainEntity"] {
                let list = in_list
                    || (key == "mainEntity"
                        && o.get(key).is_some_and(crate::event::page::main_list));
                if let Some(nested) = o.get_mut(key) {
                    if !without(nested, part, list) {
                        o.remove(key);
                    }
                }
            }
            true
        }
        _ => true,
    }
}
/// A node's `@graph`: an array of nodes, or one node object, which JSON-LD allows in its place.
fn graph_nodes(v: &Value) -> Option<&[Value]> {
    match v.get("@graph")? {
        Value::Array(nodes) => Some(nodes),
        node @ Value::Object(_) => Some(std::slice::from_ref(node)),
        _ => None,
    }
}
/// The `url`/`@id` values of a JSON-LD node (or array of nodes), its main entity and its
/// graph nodes.
fn entity_urls(v: &Value) -> impl Iterator<Item = &str> {
    v.as_array()
        .map_or_else(|| vec![v], |a| a.iter().take(16).collect())
        .into_iter()
        .flat_map(|v| {
            let graph = graph_nodes(v);
            std::iter::once(v)
                .chain(main_entity(v).filter(|m| m.is_object()))
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
            Some(b) if entity_priority(v, base) <= entity_priority(b, base) => Some(b),
            _ => Some(v),
        });
        return best.is_some_and(|b| jsonld_claims(doc, b, base));
    }
    for nested in [main_entity(v), v.get("@graph")].into_iter().flatten() {
        if jsonld_claims(doc, nested, base) {
            return true;
        }
    }
    if schema_type(v).is_none() || type_priority(v, base) < 80 {
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
/// The `Product` nodes in `v` that could give the page their identity, as `apply_jsonld` reads
/// them (with `claims`, and the page's own: naming it, or `vouched`), added to `out` by their
/// key (`node_key`; `None` for one naming none) and whether they name the page only by a
/// fragment (`by_fragment`). A repeated description of one product is added once, by a
/// fragment only if every description is; two naming no page are two. Stops at two.
fn own_products(
    v: &Value,
    base: &Url,
    claims: bool,
    vouched: bool,
    out: &mut Vec<(Option<String>, bool)>,
) {
    if !claims || out.len() > 1 {
        return;
    }
    if let Some(values) = v.as_array() {
        for v in values {
            if out.len() > 1 {
                break;
            }
            own_products(v, base, claims, vouched, out);
        }
        return;
    }
    let own = crate::event::page::names_page(v, base).unwrap_or(vouched);
    let product =
        schema_type(v).is_some_and(|k| crate::model::type_path(k).iter().any(|t| t == "Product"));
    if product && own {
        let (key, fragment) = (node_key(v, base), by_fragment(v, base));
        match out.iter_mut().find(|(k, _)| key.is_some() && *k == key) {
            Some((_, only)) => *only &= fragment,
            None => out.push((key, fragment)),
        }
    }
    if let Some(m) = main_entity(v) {
        own_products(m, base, claims, own, out);
    }
    if let Some(g) = v.get("@graph") {
        own_products(g, base, claims, vouched, out);
    }
}
/// The page's product identity guard, for an HTML page's scripts (each with whether it is in the
/// head) and a JSON document alike (one script, the head's). A page with several products of its
/// own (a category page's head list) states the barcode, SKU and part number of none: any one
/// would join the page to that product. Nor does a product naming the page only by a fragment
/// ("#nutella") when the head declares the page to be another entity (a `CollectionPage`, or a
/// main entity of another `@id`): it is one the page lists.
fn guard_identity(
    doc: &mut ResourceMeta,
    scripts: &[(Value, bool)],
    base: &Url,
    claims: impl Fn(&Value, bool) -> bool,
) {
    let mut own = Vec::new();
    for (v, head) in scripts {
        own_products(v, base, claims(v, *head), *head, &mut own);
    }
    let main = declared_main(scripts, base);
    let listed = |(key, by_fragment): &(Option<String>, bool)| {
        *by_fragment
            && key
                .as_ref()
                .is_some_and(|key| main.as_ref().is_some_and(|main| !main.contains(key)))
    };
    if own.len() > 1 || own.first().is_some_and(listed) {
        crate::kinds::products::drop_identity(doc);
    }
}
/// A node's identity: its `@id`, else its `url`, with its fragment kept. `#nutella` and `#pen`
/// on one page are two nodes, whatever `url` they share; `#product` with a `url` and without is
/// one.
fn node_key(v: &Value, base: &Url) -> Option<String> {
    let named = ["@id", "url"]
        .into_iter()
        .filter_map(|f| v.get(f).and_then(Value::as_str))
        .find_map(|u| origin_url(base, u))?;
    let fragment = Url::parse(&named)
        .ok()?
        .fragment()
        .filter(|f| !f.is_empty())
        .map(str::to_owned);
    let id = entity_identity(&named)?;
    Some(fragment.map_or_else(|| id.clone(), |f| format!("{id}#{f}")))
}
/// Whether a node names the page only by a fragment of it: the page it names (its `url`, else
/// its `@id`, as `names_page` reads it) has a fragment (`#nutella`, or the page's own URL with
/// one). `{"url": page, "@id": "#product"}` names the page itself.
fn by_fragment(v: &Value, base: &Url) -> bool {
    ["url", "@id"]
        .into_iter()
        .filter_map(|f| v.get(f).and_then(Value::as_str))
        .find_map(|u| origin_url(base, u))
        .and_then(|u| Url::parse(&u).ok())
        .is_some_and(|u| u.fragment().is_some_and(|f| !f.is_empty()))
}
/// The node `apply_jsonld` takes the card from in `v`, and whether it is the page's own (it
/// names the page, or names none and is `vouched` for); `None` when no node would type the
/// card.
fn selected<'a>(v: &'a Value, base: &Url, vouched: bool) -> Option<(&'a Value, bool)> {
    if let Some(values) = v.as_array() {
        return selected(best_node(values, base, vouched)?, base, vouched);
    }
    let own = crate::event::page::names_page(v, base).unwrap_or(vouched);
    main_entity(v)
        .and_then(|m| selected(m, base, own))
        .or_else(|| v.get("@graph").and_then(|g| selected(g, base, vouched)))
        .or_else(|| (schema_type(v).is_some() && !site_node(v, base)).then_some((v, own)))
}
/// The node of an array `apply_jsonld` reads. A page's article/event outranks its publisher or
/// breadcrumb helper nodes, and among equals the page's own node (its url is the page)
/// outranks another event it lists, and one naming no page, when vouched for, outranks one
/// naming another page; the first of equals.
fn best_node<'a>(values: &'a [Value], base: &Url, vouched: bool) -> Option<&'a Value> {
    let rank = |v: &Value| {
        (
            !site_node(v, base),
            entity_priority(v, base),
            crate::event::page::owns_page(v, base),
            crate::event::page::names_page(v, base).unwrap_or(vouched),
        )
    };
    let mut best = values.first()?;
    for value in values.iter().skip(1) {
        if rank(value) > rank(best) {
            best = value;
        }
    }
    Some(best)
}
/// What the head says the page is about, from its page nodes (a `WebPage` naming the page or
/// none): the nodes their main entity names (`node_key`: `{"@id": "#product"}`), or none for a
/// collection (`CollectionPage`, `SearchResultsPage`) naming no main entity. `None` when the
/// head says neither.
fn declared_main(scripts: &[(Value, bool)], base: &Url) -> Option<Vec<String>> {
    fn visit(v: &Value, base: &Url, out: &mut Option<Vec<String>>) {
        if let Some(values) = v.as_array() {
            return values.iter().take(16).for_each(|v| visit(v, base, out));
        }
        for node in graph_nodes(v).into_iter().flatten().take(16) {
            visit(node, base, out);
        }
        let path = schema_type(v)
            .map(crate::model::type_path)
            .unwrap_or_default();
        if !path.iter().any(|t| t == "WebPage")
            || crate::event::page::names_page(v, base) == Some(false)
        {
            return;
        }
        let main: Vec<String> = main_entity(v)
            .map_or_else(Vec::new, |m| {
                m.as_array()
                    .map_or_else(|| vec![m], |a| a.iter().take(16).collect())
            })
            .into_iter()
            .filter_map(|m| node_key(m, base))
            .collect();
        if !main.is_empty() {
            out.get_or_insert_with(Vec::new).extend(main);
        } else if path
            .iter()
            .any(|t| matches!(t.as_str(), "CollectionPage" | "SearchResultsPage"))
        {
            out.get_or_insert_with(Vec::new);
        }
    }
    let mut out = None;
    for (v, _) in scripts.iter().filter(|(_, head)| *head) {
        visit(v, base, &mut out);
    }
    out
}
/// Whether `v` holds a node whose url (else @id) is the page (`event::page::owns_page`).
fn has_own_node(v: &Value, base: &Url) -> bool {
    if let Some(values) = v.as_array() {
        return values.iter().take(16).any(|v| has_own_node(v, base));
    }
    crate::event::page::owns_page(v, base)
        || [main_entity(v), v.get("@graph")]
            .into_iter()
            .flatten()
            .any(|v| has_own_node(v, base))
}
/// `claims` gates identity-bearing fields (aliases from url/@id/sameAs, typed identifiers):
/// they come only from an entity that is the page's own. `vouched`: a node naming no page is
/// the page's own (a head script's, or the main entity of the page's own node); performers
/// come from such a node or one naming the page itself, never through a sibling's claims.
fn apply_jsonld(doc: &mut ResourceMeta, v: &Value, base: &Url, claims: bool, vouched: bool) {
    if let Some(values) = v.as_array() {
        // Extract one entity (`best_node`) so unrelated graph nodes cannot donate misleading
        // fields.
        if let Some(best) = best_node(values, base, vouched) {
            apply_jsonld(doc, best, base, claims, vouched);
        }
        return;
    }
    let own = match crate::event::page::names_page(v, base) {
        Some(own) => own,
        None => vouched,
    };
    if let Some(v) = main_entity(v) {
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
    // The site's own node names at most the site, never the page (its og: tags or title do).
    if site_node(v, base) {
        let host = base.host_str().unwrap_or_default();
        if doc.site == host {
            if let Some(name) = v.get("name").and_then(string) {
                doc.site = name.chars().take(128).collect();
            }
        }
        return;
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
    // A job posting's own facts, title and plain-text description.
    if schema_type(v).is_some_and(|k| crate::model::type_path(k).iter().any(|t| t == "JobPosting"))
    {
        crate::kinds::jobs::from_jsonld(v, base).apply(doc);
    }
    // A question's answers, votes and accepted answer.
    if schema_type(v).is_some_and(|k| crate::model::type_path(k).iter().any(|t| t == "Question")) {
        crate::kinds::questions::from_jsonld(v, base).apply(doc);
    }
    // A recipe's time, yield, ingredients and rating.
    if schema_type(v).is_some_and(|k| crate::model::type_path(k).iter().any(|t| t == "Recipe")) {
        crate::kinds::cooking::apply_jsonld(v, doc);
    }
    // A product's barcode, brand and offer. Its identity (barcode, article and part number)
    // only from the page's own product, as other claims: another product a page lists (a
    // category page's) would join the page to that product.
    if schema_type(v).is_some_and(|k| crate::model::type_path(k).iter().any(|t| t == "Product")) {
        let product = crate::kinds::products::from_jsonld(v, base);
        if claims && own {
            product.apply(doc);
        } else {
            product.without_identity().apply(doc);
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
    // A FAQ's node names its block ("FAQ Page"), never the page: the page's own title does.
    if !typed(v, "FAQPage") {
        fill(
            &mut doc.name,
            v.get("name").or_else(|| v.get("headline")).and_then(string),
        );
    }
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
/// A provider's own oEmbed answer for a link it names exactly (its scheme has a path, as
/// YouTube's `watch` and `shorts` do): its title, author and thumbnail are the object's, ahead
/// of what the page says (a regional or consent page names the site, not the video). The page
/// keeps what the answer lacks or leaves blank.
pub fn apply_oembed_first(doc: &mut ResourceMeta, v: &Value, base: &Url) {
    if let Some(title) = oembed_field(v, "title") {
        doc.name = Some(title);
    }
    if let Some(image) = oembed_field(v, "thumbnail_url").and_then(|s| origin_url(base, &s)) {
        doc.image = Some(image);
    }
    if let Some(author) = oembed_field(v, "author_name") {
        doc.attributes.insert("author".into(), author);
    }
    apply_oembed(doc, v, base);
}
pub fn apply_oembed(doc: &mut ResourceMeta, v: &Value, base: &Url) {
    fill(&mut doc.name, oembed_field(v, "title"));
    fill(
        &mut doc.image,
        oembed_field(v, "thumbnail_url").and_then(|s| origin_url(base, &s)),
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
        if let Some(v) = oembed_field(v, field) {
            doc.attributes.entry(key.into()).or_insert(v);
        }
    }
    // The provider's frame comes before the page's own player, unless the link names one
    // (`embed::link`); only its address is kept, never the answer's HTML.
    if crate::embed::link(&doc.url).is_none() {
        if let Some(embed) = crate::embed::from_oembed(v, base) {
            doc.embed = Some(embed);
        }
    }
}
/// An oEmbed answer's text field, unless blank: a blank thumbnail would resolve to the page
/// itself, and a blank title or author names nothing.
fn oembed_field(v: &Value, field: &str) -> Option<String> {
    v.get(field)
        .and_then(string)
        .filter(|s| !s.trim().is_empty())
}
#[cfg(test)]
mod tests {
    use super::*;
    /// A feed is a DataFeed named by its own title on every route: Atom (bitcoinops.org's,
    /// served as `application/xml`; GitHub's releases), JSON Feed (jsonfeed.org's own), RSS and
    /// RSS 1.0, also cut off at the page cap. Recorded answers are in `tests/fixtures/web/feeds`.
    #[test]
    fn feeds_are_data_feeds_named_by_their_own_title() {
        let read = |file: &str| {
            std::fs::read(format!(
                "{}/tests/fixtures/web/feeds/{file}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap()
        };
        let card = |url: &str, content_type: &str, body: Vec<u8>, truncated: bool| {
            let f = Fetched {
                truncated,
                ..Fetched::answer(Url::parse(url).unwrap(), 200, content_type.into(), body)
            };
            let d = extract(url, &f, 3600).unwrap().doc;
            (d.type_name, d.name, d.description)
        };
        let feed = |name: &str, description: Option<&str>| {
            (
                "DataFeed".to_owned(),
                Some(name.to_owned()),
                description.map(str::to_owned),
            )
        };
        let optech = read("bitcoinops-feed.xml");
        assert_eq!(
            card(
                "https://bitcoinops.org/feed.xml",
                "application/xml",
                optech.clone(),
                false
            ),
            feed(
                "Bitcoin Optech",
                Some("Helping Bitcoin-based businesses integrate scaling technology.")
            )
        );
        // Cut inside its first entry: the feed's own fields came before it.
        let cut = optech.len() - 200;
        assert_eq!(
            card(
                "https://bitcoinops.org/feed.xml",
                "application/xml",
                optech[..cut].to_vec(),
                true
            )
            .1
            .as_deref(),
            Some("Bitcoin Optech")
        );
        assert_eq!(
            card(
                "https://github.com/pubky/pubky-homeserver/releases.atom",
                "application/atom+xml",
                read("github-pubky-homeserver-releases.atom"),
                false
            ),
            feed("Release notes from pubky-homeserver", None)
        );
        assert_eq!(
            card(
                "https://www.jsonfeed.org/feed.json",
                "application/json",
                read("jsonfeed-feed.json"),
                false
            ),
            feed("JSON Feed", None)
        );
        // RSS, whether or not the media route reads it, and RSS 1.0, as XML or as text.
        assert_eq!(
            card("https://news.example/rss", "text/xml", br#"<?xml version="1.0"?><rss version="2.0"><channel><title>News &amp; notes</title><description><![CDATA[<p>Daily <b>news</b></p>]]></description><item><title>One</title></item></channel></rss>"#.to_vec(), false),
            feed("News & notes", Some("Daily news"))
        );
        assert_eq!(
            card("https://news.example/index.rdf", "text/plain", br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns="http://purl.org/rss/1.0/"><channel rdf:about="https://news.example/"><title>Old news</title><description>Since 2001</description></channel><item rdf:about="https://news.example/1"><title>One</title></item></rdf:RDF>"#.to_vec(), false),
            feed("Old news", Some("Since 2001"))
        );
        // Text that looks like markup is markup only where the format says the field is HTML: an
        // Atom `type="html"` text and an RSS description. Atom `text` (the default) and `xhtml`
        // texts, RSS titles and JSON Feed's fields keep it as the literal text it is.
        for (title, want) in [
            (
                r#"<title type="text">Using &lt;title&gt; in HTML</title>"#,
                "Using <title> in HTML",
            ),
            (
                r#"<title>Using &lt;title&gt; in HTML</title>"#,
                "Using <title> in HTML",
            ),
            (
                r#"<title type="html">Using &amp;lt;title&amp;gt; in &lt;b&gt;HTML&lt;/b&gt;</title>"#,
                "Using <title> in HTML",
            ),
            (
                r#"<title type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml">Using &lt;title&gt; in <b>HTML</b></div></title>"#,
                "Using <title> in HTML",
            ),
        ] {
            let atom = format!(r#"<feed xmlns="http://www.w3.org/2005/Atom">{title}</feed>"#);
            assert_eq!(
                card(
                    "https://x.example/atom",
                    "application/atom+xml",
                    atom.into_bytes(),
                    false
                )
                .1
                .as_deref(),
                Some(want),
                "{title}"
            );
        }
        assert_eq!(
            card("https://x.example/feed.json", "application/feed+json", br#"{"version":"https://jsonfeed.org/version/1.1","title":"Using <title> in HTML","description":"Why a <b> tag   bolds"}"#.to_vec(), false),
            feed("Using <title> in HTML", Some("Why a <b> tag bolds"))
        );
        assert_eq!(
            card("https://x.example/rss", "application/rss+xml", br#"<rss><channel><title>Using &lt;title&gt; in HTML</title><description>&lt;p&gt;Why a &lt;b&gt;tag&lt;/b&gt; bolds&lt;/p&gt;</description></channel></rss>"#.to_vec(), false),
            feed("Using <title> in HTML", Some("Why a tag bolds"))
        );
        // Served as a feed but unreadable: a feed with no name.
        assert_eq!(
            card(
                "https://news.example/feed",
                "application/rss+xml",
                b"not xml".to_vec(),
                false
            ),
            ("DataFeed".to_owned(), None, None)
        );
        // Not feeds: an RDF document without a channel, a feed-shaped element in another
        // namespace, ordinary JSON (a version that is not JSON Feed's), and a page.
        for (content_type, body) in [
            (
                "application/xml",
                r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="https://x.example/"/></rdf:RDF>"#,
            ),
            (
                "application/xml",
                r#"<feed xmlns="https://other.example/ns"><title>Not Atom</title></feed>"#,
            ),
            (
                "application/json",
                r#"{"version":"1.2.3","title":"A package"}"#,
            ),
            (
                "text/html",
                "<!doctype html><html><head><title>A page</title></head></html>",
            ),
        ] {
            assert_ne!(
                card(
                    "https://x.example/doc",
                    content_type,
                    body.as_bytes().to_vec(),
                    false
                )
                .0,
                "DataFeed",
                "{body}"
            );
        }
    }

    #[test]
    fn a_page_names_its_feed_in_its_head() {
        let page = |body: &str| {
            Fetched::answer(
                Url::parse("https://darknetdiaries.com/episode/180/").unwrap(),
                200,
                "text/html".into(),
                body.as_bytes().to_vec(),
            )
        };
        let feed = |body: &str| {
            extract("https://darknetdiaries.com/episode/180/", &page(body), 3600)
                .unwrap()
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

    /// A link that carries a credential is never an alias: not a permanent redirect's target, not a
    /// head canonical.
    #[test]
    fn a_link_that_carries_a_credential_is_never_an_alias() {
        let page = |url: &str, body: &str| Fetched {
            permanent_redirect: true,
            ..Fetched::answer(
                Url::parse(url).unwrap(),
                200,
                "text/html".into(),
                body.as_bytes().to_vec(),
            )
        };
        let d = extract(
            "https://example.org/a",
            &page(
                "https://example.org/a?token=x",
                r#"<head><link rel="canonical" href="/a?sig=x"></head><title>T</title>"#,
            ),
            3600,
        )
        .unwrap()
        .doc;
        assert!(d.aliases.is_empty(), "{:?}", d.aliases);
        assert!(d.alias_candidates.is_empty(), "{:?}", d.alias_candidates);
    }
    /// A fetch that ends on a link carrying a credential names nothing after it: not an image's
    /// address, not a PDF's name.
    #[test]
    fn nothing_read_names_a_redirect_target_that_carries_a_credential() {
        let landed = |content_type: &str| {
            Fetched::answer(
                Url::parse("https://cdn.example/files/SECRET-NAME.png?token=SECRET").unwrap(),
                200,
                content_type.into(),
                vec![],
            )
        };
        for content_type in ["image/png", "application/pdf"] {
            let d = extract(
                "https://example.org/files/a.png",
                &landed(content_type),
                3600,
            )
            .unwrap()
            .doc;
            let json = serde_json::to_string(&d).unwrap();
            assert!(!json.contains("SECRET"), "{content_type}: {json}");
        }
    }
    /// A page whose fetch ended on an address that carries a credential names nothing after that
    /// address: an empty og:image, an empty feed link, relative JSON-LD `image` and `url` all
    /// resolve against it.
    #[test]
    fn a_page_reached_at_a_credential_resolves_nothing_back_to_it() {
        let fetched = Fetched::answer(
            Url::parse("https://app.example/share?token=SECRET").unwrap(),
            200,
            "text/html".into(),
            br##"<head><title>Shared</title><meta property="og:image" content=""><link rel="alternate" type="application/rss+xml" href=""><script type="application/ld+json">{"@type":"Article","name":"Shared","image":"#img","url":""}</script></head>"##.to_vec(),
        );
        let d = extract("https://example.org/s/abc", &fetched, 3600)
            .unwrap()
            .doc;
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("SECRET"), "{json}");
    }
    /// Such a page is read at the origin that served it: its relative links stay on that host,
    /// never on the host of the link asked for.
    #[test]
    fn a_page_reached_at_a_credential_on_another_host_is_read_at_that_host() {
        let fetched = Fetched::answer(
            Url::parse("https://b.example/share?token=SECRET").unwrap(),
            200,
            "text/html".into(),
            br#"<head><title>Shared</title><meta property="og:image" content="/og.png"></head>"#
                .to_vec(),
        );
        let d = extract("https://a.example/s/abc", &fetched, 3600)
            .unwrap()
            .doc;
        assert_eq!(d.image.as_deref(), Some("https://b.example/og.png"));
    }
    /// A path segment of such an address may be a secret too (`/files/<id>?token=…`): the page is
    /// read in its directory, so a link to the page itself names neither.
    #[test]
    fn a_page_reached_at_a_credential_is_read_in_its_directory() {
        let fetched = Fetched::answer(
            Url::parse("https://b.example/files/SECRETID?token=SECRET").unwrap(),
            200,
            "text/html".into(),
            br#"<head><title>Shared</title><meta property="og:image" content=""><meta property="og:url" content=""></head>"#
                .to_vec(),
        );
        let d = extract("https://a.example/s/abc", &fetched, 3600)
            .unwrap()
            .doc;
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("SECRET"), "{json}");
        assert_eq!(d.image.as_deref(), Some("https://b.example/files/"));
    }
    /// A path that ends in `/` is read in its parent: its last segment may be the secret too.
    #[test]
    fn a_directory_reached_at_a_credential_is_read_in_its_parent() {
        let at = |u: &str| read_at(&Url::parse(u).unwrap()).to_string();
        assert_eq!(
            at("https://b.example/files/SECRETID/?token=SECRET"),
            "https://b.example/files/"
        );
        assert_eq!(
            at("https://b.example/files/SECRETID?token=SECRET"),
            "https://b.example/files/"
        );
        assert_eq!(at("https://b.example/?token=SECRET"), "https://b.example/");
    }
    #[test]
    fn only_head_canonicals_and_permanent_redirects_are_alias_candidates() {
        let page = |url: &str, permanent: bool, body: &str| Fetched {
            permanent_redirect: permanent,
            ..Fetched::answer(
                Url::parse(url).unwrap(),
                200,
                "text/html".into(),
                body.as_bytes().to_vec(),
            )
        };
        let claims = r#"<title>T</title><meta property="og:url" content="https://example.org/a?utm_source=og"><script type="application/ld+json">{"@type":"Article","name":"A","url":"https://example.org/a/","sameAs":"https://example.org/a?fbclid=1"}</script>"#;
        // A temporary redirect, og:url and JSON-LD claims are listed but never candidates.
        let d = extract(
            "https://example.org/a?x=1",
            &page("https://example.org/a", false, claims),
            3600,
        )
        .unwrap()
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
        .unwrap()
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
        .unwrap()
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
            let fetched = Fetched::answer(
                Url::parse("https://example.org/").unwrap(),
                200,
                "text/html".into(),
                head.as_bytes().into(),
            );
            extract("https://example.org/", &fetched, 100).unwrap()
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
        let fetched = Fetched::answer(
            Url::parse("https://example.org/data.json").unwrap(),
            200,
            "application/ld+json".into(),
            br#"{"@type":"WebPage","name":"Tom &amp; Jerry","description":"5&currency=EUR"}"#
                .as_slice()
                .into(),
        );
        let body = extract("https://example.org/data.json", &fetched, 100).unwrap();
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
        )
        .unwrap();
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
            let out = extract(asked, &fetched, 100).unwrap();
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
        let out = extract("https://venue.example/tonight", &fetched, 100).unwrap();
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
        let out = extract("https://venue.example/tonight", &fetched, 100).unwrap();
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
            extract("https://venue.example/tonight", &fetched, 100)
                .unwrap()
                .doc
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
        let doc = extract("https://venue.example/tonight", &fetched, 100)
            .unwrap()
            .doc;
        assert_eq!(names(&doc), ["Radiohead"]);
    }
    /// A site's own JSON-LD (its publisher at the site's root, its WebSite, its breadcrumbs) never
    /// names, describes, illustrates or types a page: the page's og: tags (in the head, or
    /// streamed as children of `<body>`) do, less the site's suffix, and the publisher may name
    /// the site. A node about the page still wins over og:.
    #[test]
    fn a_sites_own_json_ld_never_names_the_page() {
        let read = |url: &str, html: &str| {
            let fetched = crate::sources::fixture::page(url, "text/html", html.into(), None);
            extract(url, &fetched, 100).unwrap()
        };
        let site = r#"<script type="application/ld+json">{"@type":"Organization","name":"Pods","url":"https://pods.example","logo":"https://pods.example/logo.png","description":"The podcast app."}</script><script type="application/ld+json">{"@type":"WebSite","name":"Pods","url":"https://pods.example"}</script>"#;
        let og = r#"<meta property="og:title" content="A Show • Listen on Pods"><meta property="og:description" content="About the show."><meta property="og:image" content="https://cdn.example/cover.jpg">"#;
        let card = |out: &Extracted| {
            (
                out.doc.name.clone(),
                out.doc.description.clone(),
                out.doc.image.clone(),
            )
        };
        let expected = (
            Some("A Show".to_owned()),
            Some("About the show.".to_owned()),
            Some("https://cdn.example/cover.jpg".to_owned()),
        );
        // og: in the head, and og: a framework streamed into the body itself.
        for html in [
            format!("<html><head>{site}{og}</head><body></body></html>"),
            format!("<html><head>{site}</head><body><div hidden></div>{og}</body></html>"),
        ] {
            let out = read("https://pods.example/show/1", &html);
            assert_eq!(card(&out), expected, "{html}");
            assert_ne!(out.doc.type_name, "Organization");
            assert_eq!(out.doc.site, "Pods");
        }
        // A `<meta>` inside the body's content is not the page's.
        let nested = format!(
            "<html><head>{site}</head><body><div><meta property=\"og:title\" content=\"Injected\"></div></body></html>"
        );
        assert_eq!(read("https://pods.example/show/1", &nested).doc.name, None);
        // Only the head claims an identity: a body og:url is no alias.
        let claimed = format!(
            "<html><head></head><body>{og}<meta property=\"og:url\" content=\"https://other.example/\"></body></html>"
        );
        assert!(read("https://pods.example/show/1", &claimed)
            .doc
            .aliases
            .iter()
            .all(|a| !a.contains("other.example")));
        // The site's root page is the organization's own: it names it.
        let home = read(
            "https://pods.example/",
            &format!("<html><head>{site}</head><body></body></html>"),
        );
        assert_eq!(home.doc.name.as_deref(), Some("Pods"));
        // A node naming another site's root is about the page: a directory's restaurant keeps
        // its name, type and place, and an author page its author; neither names the site.
        let directory = r#"<html><head><script type="application/ld+json">{"@type":"Restaurant","name":"The Olive Tree","url":"https://olivetree.example/","geo":{"latitude":46.0,"longitude":8.95}}</script><meta property="og:title" content="Restaurants | Directory"></head></html>"#;
        let out = read("https://directory.example/restaurants/42", directory);
        assert_eq!(out.doc.name.as_deref(), Some("The Olive Tree"));
        assert_eq!(out.doc.type_name, "Restaurant");
        assert!(out.doc.location.is_some());
        assert_ne!(out.doc.site, "The Olive Tree");
        let author = r#"<html><head><script type="application/ld+json">{"@type":"Person","name":"Ada Writer","url":"https://ada.example/"}</script></head></html>"#;
        let out = read("https://magazine.example/authors/ada", author);
        assert_eq!(out.doc.name.as_deref(), Some("Ada Writer"));
        // The publisher on a parent domain is still the site's own.
        let parent = r#"<html><head><script type="application/ld+json">{"@type":"Organization","name":"Pods","url":"https://pods.example/"}</script><meta property="og:title" content="A Show"></head></html>"#;
        let out = read("https://play.pods.example/show/1", parent);
        assert_eq!(out.doc.name.as_deref(), Some("A Show"));
        // A script whose @graph holds only site nodes ranks below a WebPage naming the page.
        let graph = r#"<html><head><script type="application/ld+json">{"@context":"https://schema.org","@graph":[{"@type":"LocalBusiness","name":"Publisher","url":"https://example.org/"},{"@type":"WebSite","name":"Publisher","url":"https://example.org/"}]}</script><script type="application/ld+json">{"@type":"WebPage","url":"https://example.org/notes/1","name":"Note one"}</script></head></html>"#;
        let out = read("https://example.org/notes/1", graph);
        assert_eq!(out.doc.name.as_deref(), Some("Note one"));
        assert_eq!(out.doc.type_name, "WebPage");
        // A graph holding the page's Article, through an untyped wrapper's mainEntity or after
        // 16 or 40 site nodes, is not site-only: the Article names the page over a generic
        // WebPage.
        let site = r#"{"@type":"WebSite","name":"Publisher","url":"https://example.org/"}"#;
        let story = r#"{"@type":"Article","name":"Actual story","description":"What happened."}"#;
        for graph in [
            format!(r#"[{site},{{"mainEntity":{story}}}]"#),
            format!("[{}{story}]", format!("{site},").repeat(16)),
            format!("[{}{story}]", format!("{site},").repeat(40)),
        ] {
            let html = format!(
                r#"<html><head><script type="application/ld+json">{{"@graph":{graph}}}</script><script type="application/ld+json">{{"@type":"WebPage","url":"https://example.org/story","name":"Generic page"}}</script></head></html>"#
            );
            let out = read("https://example.org/story", &html);
            assert_eq!(out.doc.name.as_deref(), Some("Actual story"), "{graph}");
            assert_eq!(out.doc.description.as_deref(), Some("What happened."));
            assert_eq!(out.doc.type_name, "Article");
        }
        // A graph of site nodes alone stays site-only however long it is: a separate WebPage
        // names the page.
        let publisher =
            r#"{"@type":"LocalBusiness","name":"Publisher","url":"https://example.org/"}"#;
        for n in [16, 17, 40] {
            let graph = vec![publisher; n].join(",");
            let html = format!(
                r#"<html><head><script type="application/ld+json">{{"@graph":[{graph}]}}</script><script type="application/ld+json">{{"@type":"WebPage","url":"https://example.org/story","name":"Actual story","description":"What happened."}}</script></head></html>"#
            );
            let out = read("https://example.org/story", &html);
            assert_eq!(out.doc.name.as_deref(), Some("Actual story"), "{n}");
            assert_eq!(out.doc.description.as_deref(), Some("What happened."));
            assert_eq!(out.doc.type_name, "WebPage");
        }
        // A `@graph` may be one node object. The page's Article there is page content, in a head
        // or a body script, and its url names the page: its @id is an alias.
        let generic = r#"<script type="application/ld+json">{"@type":"WebPage","url":"https://example.org/story","name":"Generic page"}</script>"#;
        let single = r#"<script type="application/ld+json">{"@type":"WebSite","url":"https://example.org/","@graph":{"@type":"Article","name":"Actual story","description":"What happened.","url":"https://example.org/story","@id":"https://example.org/s/42"}}</script>"#;
        for html in [
            format!("<html><head>{single}{generic}</head></html>"),
            format!("<html><head>{generic}</head><body>{single}</body></html>"),
        ] {
            let out = read("https://example.org/story", &html);
            assert_eq!(out.doc.name.as_deref(), Some("Actual story"), "{html}");
            assert_eq!(out.doc.description.as_deref(), Some("What happened."));
            assert_eq!(out.doc.type_name, "Article");
            assert!(out
                .doc
                .aliases
                .iter()
                .any(|a| a == "https://example.org/s/42"));
        }
        // A node about the page (a product whose url is the page, an event with no url) wins.
        for (url, node, name) in [
            (
                "https://shop.example/p/42",
                r#"{"@type":"Product","url":"https://shop.example/p/42","name":"Lamp","image":"https://shop.example/lamp.jpg"}"#,
                "Lamp",
            ),
            (
                "https://pods.example/live/1",
                r#"{"@type":"Event","name":"Live show","startDate":"2026-11-01T19:00:00Z"}"#,
                "Live show",
            ),
        ] {
            let html = format!(
                "<html><head>{site}<script type=\"application/ld+json\">{node}</script>{og}</head></html>"
            );
            assert_eq!(read(url, &html).doc.name.as_deref(), Some(name), "{url}");
        }
    }

    /// A title loses its site suffix only when the suffix names the site.
    #[test]
    fn site_suffixes() {
        let url = Url::parse("https://fountain.fm/show/1").unwrap();
        let sites = ["Fountain: Podcasts & Music".to_owned()];
        for (title, clean) in [
            (
                "TFTC: A Bitcoin Podcast • Listen on Fountain",
                "TFTC: A Bitcoin Podcast",
            ),
            (
                "TFTC: A Bitcoin Podcast • Ten31 Timestamp • Watch on Fountain",
                "TFTC: A Bitcoin Podcast • Ten31 Timestamp",
            ),
            ("A Show | Fountain", "A Show"),
            ("A Show - fountain.fm", "A Show"),
            // The suffix names something else, or a word that only contains the site's name.
            (
                "Water - The Fountains of Rome",
                "Water - The Fountains of Rome",
            ),
            ("A Show - Season 2", "A Show - Season 2"),
            ("• Listen on Fountain", "• Listen on Fountain"),
            ("No separator Fountain", "No separator Fountain"),
        ] {
            assert_eq!(
                without_site_suffix(title, &sites, &url, false),
                clean,
                "{title}"
            );
        }
        // A host label names the site without any site name.
        let play = Url::parse("https://play.google.com/store/apps/details?id=a.b").unwrap();
        assert_eq!(
            without_site_suffix("Signal - Apps on Google Play", &[], &play, false),
            "Signal"
        );
        // A subdomain's word is no site name: an open letter on open.example.com keeps it.
        let open = Url::parse("https://open.example.com/letters/1").unwrap();
        assert_eq!(
            without_site_suffix("Foo - Open Letter", &[], &open, false),
            "Foo - Open Letter"
        );
        assert_eq!(
            without_site_suffix("Foo - Example", &[], &open, false),
            "Foo"
        );
        // A site's root page keeps its title.
        assert_eq!(
            without_site_suffix("Podcasts • Fountain", &sites, &url, true),
            "Podcasts • Fountain"
        );
    }

    /// Fountain's show and episode pages (recorded, trimmed to their metadata): the show is
    /// named by its og: tags, which Next.js streams into the body, not by Fountain's own
    /// Organization; the episode by its VideoObject. Both are typed by their address.
    #[test]
    fn fountain_pages_are_named_by_their_own_metadata() {
        let read = |url: &str, file: &str| {
            let path = format!("{}/tests/fixtures/web/{file}", env!("CARGO_MANIFEST_DIR"));
            let fetched =
                crate::sources::fixture::page(url, "text/html", std::fs::read(path).unwrap(), None);
            let mut doc = extract(url, &fetched, 100).unwrap().doc;
            doc.status = crate::model::Status::Ok;
            crate::also_on::media::type_card(url, &Default::default(), &mut doc);
            doc
        };
        let show = read(
            "https://fountain.fm/show/V71wG3DhMtF9fDqaJRWX",
            "fountain-show-V71wG3DhMtF9fDqaJRWX.html",
        );
        assert_eq!(show.type_name, "PodcastSeries");
        assert_eq!(show.name.as_deref(), Some("TFTC: A Bitcoin Podcast"));
        assert!(show
            .description
            .as_deref()
            .is_some_and(|d| d.starts_with("TFTC is a podcast hosted by Marty Bent")));
        assert!(show.image.as_deref().is_some_and(
            |i| i.starts_with("https://feeds.fountain.fm/ZwwaDULvAj0yZvJ5kdB9/files/COVER_ART")
        ));
        assert_eq!(show.site, "Fountain: Podcasts & Music");
        let episode = read(
            "https://fountain.fm/episode/Yj4aqLaHXcVmxgXbMLHb",
            "fountain-episode-Yj4aqLaHXcVmxgXbMLHb.html",
        );
        assert_eq!(episode.type_name, "PodcastEpisode");
        assert_eq!(
            episode.name.as_deref(),
            Some("Ten31 Timestamp: Spirit of '63")
        );
        assert!(episode
            .image
            .as_deref()
            .is_some_and(|i| i.contains("/items/r1808W019H3baERCQAWo/")));
    }

    #[test]
    fn extraction_precedence_and_promoted_facets() {
        let body = r#"<title>Title fallback</title><meta property="og:title" content="OG"><meta property="og:image" content="/pic.png"><link rel="alternate" type="application/json+oembed" href="/embed"><link rel="canonical" href="/canonical"><div class="h-event"><span class="p-name">MF</span><span class="p-summary">MF summary</span></div><script type="application/ld+json">{"@type":"MusicEvent","name":"JSON first","startDate":"2026-10-01T12:00:00Z","location":{"geo":{"latitude":46.0,"longitude":8.0}}}</script>"#;
        let fetched = Fetched::answer(
            Url::parse("https://example.org/").unwrap(),
            200,
            "text/html".into(),
            body.as_bytes().into(),
        );
        let out = extract("https://example.org/", &fetched, 100).unwrap();
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
        let fetch = |url: &str, body: Vec<u8>| {
            Fetched::answer(Url::parse(url).unwrap(), 200, "text/html".into(), body)
        };
        // TIDAL's artist page: its own MusicGroup, and a graph with the site's Organization,
        // WebSite and WebApplication (all named "TIDAL").
        let path = format!(
            "{}/tests/fixtures/web/tidal-artist-64518.html",
            env!("CARGO_MANIFEST_DIR")
        );
        let url = "https://tidal.com/artist/64518";
        let out = extract(url, &fetch(url, std::fs::read(path).unwrap()), 100).unwrap();
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
            let mut doc = extract(url, &fetch(url, std::fs::read(path).unwrap()), 100)
                .unwrap()
                .doc;
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
        )
        .unwrap();
        assert_eq!(out.doc.type_name, "WebPage");
        // An organization naming another page stays a helper.
        let other = r#"<head><script type="application/ld+json">{"@type":"Organization","url":"https://shop.example/","name":"Shop Ltd"}</script><script type="application/ld+json">{"@type":"WebApplication","name":"App"}</script></head>"#;
        let out = extract(
            "https://shop.example/about",
            &fetch("https://shop.example/about", other.as_bytes().to_vec()),
            100,
        )
        .unwrap();
        assert_eq!(out.doc.type_name, "WebApplication");
    }

    #[test]
    fn pages_cite_their_own_identifiers_from_meta() {
        let keys = |head: &str| {
            let fetched = Fetched::answer(
                Url::parse("https://journal.example/a").unwrap(),
                200,
                "text/html".into(),
                format!("<title>Paper</title>{head}").into_bytes(),
            );
            extract("https://journal.example/a", &fetched, 100)
                .unwrap()
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
            let fetched = Fetched::answer(
                Url::parse("https://victim.example/post").unwrap(),
                200,
                "text/html".into(),
                html.as_bytes().to_vec(),
            );
            extract("https://victim.example/post", &fetched, 100).unwrap()
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
            let fetched = Fetched::answer(
                Url::parse("https://news.example/story").unwrap(),
                200,
                "text/html".into(),
                html.as_bytes().to_vec(),
            );
            let d = extract("https://news.example/story", &fetched, 100)
                .unwrap()
                .doc;
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
        // Alone, the same head node is still the site's publisher (its url is the site's root,
        // the page is a story): it claims nothing for the page, neither the homepage nor the
        // publisher's Wikidata item.
        let (_, keys) = extract_keys(
            r#"<head><title>Story</title><script type="application/ld+json">{"@type":"Organization","url":"https://news.example/","sameAs":"https://www.wikidata.org/wiki/Q42"}</script></head><body><p>Text</p></body>"#,
        );
        assert!(keys.iter().all(|k| !k.contains("wikidata")), "{keys:?}");
        assert!(
            !keys.contains(&"https://news.example/".to_owned()),
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
    /// A page's card keeps the frame its oEmbed answer names (its `<iframe>` alone), before the
    /// player its head `<meta>` names; a link whose player follows from the link stores none.
    #[test]
    fn a_page_card_keeps_its_frame_never_the_html() {
        let page = |uri: &str, body: &str| {
            let url = Url::parse(uri).unwrap();
            extract(
                uri,
                &Fetched::answer(url, 200, "text/html".into(), body.as_bytes().to_vec()),
                100,
            )
            .unwrap()
            .doc
        };
        let head = r#"<head><title>Clip</title><meta name="twitter:player" content="/embed/clip-1"><meta name="twitter:player:width" content="640"><meta name="twitter:player:height" content="360"></head><body><meta name="twitter:player" content="https://attacker.example/x"></body>"#;
        let mut doc = page("https://video.example.com/clip-1", head);
        let embed = doc.embed.clone().unwrap();
        assert_eq!(embed.src, "https://video.example.com/embed/clip-1");
        assert_eq!(embed.aspect, Some(1.7778));
        apply_oembed(
            &mut doc,
            &serde_json::json!({"type": "rich", "provider_name": "Video Example",
                "html": "<div><iframe src=\"https://player.video.example.com/1\" height=\"300\"></iframe></div><script>bad</script>"}),
            &Url::parse("https://video.example.com/oembed").unwrap(),
        );
        let embed = doc.embed.clone().unwrap();
        assert_eq!(
            (embed.provider.as_str(), embed.src.as_str(), embed.height),
            (
                "video-example",
                "https://player.video.example.com/1",
                Some(300)
            )
        );
        assert!(!serde_json::to_string(&doc).unwrap().contains("<script>"));
        let yt = page("https://www.youtube.com/watch?v=dQw4w9WgXcQ", head);
        assert_eq!(yt.embed, None, "the link's own player is added when served");
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
                Ok(Fetched::answer(
                    url.clone(),
                    200,
                    "text/html".into(),
                    br#"<title>Page survives</title><link rel="alternate" type="application/json+oembed" href="/embed">"#.to_vec(),
                ))
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

    fn read_page(url: &str, html: &str) -> ResourceMeta {
        let fetched = crate::sources::fixture::page(url, "text/html", html.into(), None);
        extract(url, &fetched, 100).unwrap().doc
    }
    #[test]
    fn a_faqs_questions_are_parts_of_the_page_and_a_single_question_is_a_pages_subject() {
        let question = |name: &str| {
            format!(
                r#"{{"@type":"Question","name":"{name}","acceptedAnswer":{{"@type":"Answer","text":"Yes."}}}}"#
            )
        };
        let page = |scripts: &str| {
            read_page(
                "https://bank.example/",
                &format!(
                    r#"<html><head><title>Love your bank | N26</title><meta property="og:title" content="Love your bank">{scripts}</head><body></body></html>"#
                ),
            )
        };
        // A bank's homepage (n26.com): its organization, its site, and a FAQ block of questions,
        // each in its own head script. The card is the page's, named by its og:title, not the
        // first question's.
        let site = r#"<script type="application/ld+json">{"@type":"Organization","url":"https://bank.example"}</script><script type="application/ld+json">{"@type":"WebSite","name":"N26","url":"https://bank.example/en-eu"}</script>"#;
        let faq = |questions: &[String]| {
            format!(
                r#"<script type="application/ld+json">{{"@type":"FAQPage","name":"FAQ Page","url":"https://bank.example/","mainEntity":[{}]}}</script>"#,
                questions.join(",")
            )
        };
        let doc = page(&format!(
            "{site}{}",
            faq(&[
                question("Is N26 a bank?"),
                question("Where is N26 available?")
            ])
        ));
        assert_eq!(doc.type_name, "FAQPage");
        assert!(doc.type_path.iter().any(|t| t == "WebPage"));
        assert_eq!(doc.name.as_deref(), Some("Love your bank"));
        // A FAQ of one question: the same.
        let doc = page(&format!("{site}{}", faq(&[question("Is N26 a bank?")])));
        assert_eq!(doc.type_name, "FAQPage");
        assert_eq!(doc.name.as_deref(), Some("Love your bank"));
        // Several questions as a page's main entity, without the FAQ type: a list, the page.
        let doc = page(&format!(
            r#"<script type="application/ld+json">{{"@type":"WebPage","url":"https://bank.example/","mainEntity":[{},{}]}}</script>"#,
            question("Is N26 a bank?"),
            question("Is N26 secure?")
        ));
        assert_eq!(doc.type_name, "WebPage");
        assert_eq!(doc.name.as_deref(), Some("Love your bank"));
        // A Q&A page's single question is the page (Stack Exchange), as a page's single main
        // entity is.
        for kind in ["QAPage", "WebPage"] {
            let doc = page(&format!(
                r#"<script type="application/ld+json">{{"@type":"{kind}","mainEntity":{}}}</script>"#,
                question("How do I open an account?")
            ));
            assert_eq!(doc.type_name, "Question", "{kind}");
            assert_eq!(doc.name.as_deref(), Some("How do I open an account?"));
        }
    }
    #[test]
    fn a_page_listing_several_entities_of_one_kind_is_not_its_first_entry() {
        let concert = |n: u32, url: &str| {
            format!(
                r#"<script type="application/ld+json">{{"@type":"MusicEvent","name":"Radiohead @ Arena {n}","url":"{url}","startDate":"2027-05-0{n}T19:30:00","location":{{"@type":"Place","name":"Arena {n}"}}}}</script>"#
            )
        };
        let listed = |n: u32| concert(n, &format!("https://gigs.example/concerts/{n}-radiohead"));
        let page = |url: &str, scripts: &str| {
            read_page(
                url,
                &format!(
                    r#"<html><head><title>Radiohead Tour Dates</title><meta property="og:title" content="Radiohead Tour Dates">{scripts}</head><body></body></html>"#
                ),
            )
        };
        let artist = "https://gigs.example/artists/253846-radiohead";
        let own = r#"<script type="application/ld+json">{"@type":"MusicGroup","name":"Radiohead","url":"https://gigs.example/artists/253846-radiohead"}</script>"#;
        // An artist's page listing concerts (songkick.com/artists/…), each a MusicEvent in its
        // own script naming its own page: the card is the artist's own node.
        let doc = page(
            artist,
            &format!("{}{}{}{own}", listed(1), listed(2), listed(3)),
        );
        assert_eq!(doc.type_name, "MusicGroup");
        assert_eq!(doc.name.as_deref(), Some("Radiohead"));
        assert!(doc.start.is_none());
        assert_eq!(doc.event.not_event.as_deref(), Some("lists_events"));
        // Without the artist's node: the page, named by its title.
        let doc = page(artist, &format!("{}{}", listed(1), listed(2)));
        assert_eq!(doc.type_name, "WebPage");
        assert_eq!(doc.name.as_deref(), Some("Radiohead Tour Dates"));
        assert!(doc.start.is_none());
        // One concert, in two scripts (a theme's and a plugin's node for one event): the concert.
        let doc = page(artist, &format!("{}{}", listed(1), listed(1)));
        assert_eq!(doc.type_name, "MusicEvent");
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 1"));
        assert_eq!(doc.event.not_event, None);
        // A concert's own page listing two others: its own.
        let doc = page(
            "https://gigs.example/concerts/9-radiohead",
            &format!(
                "{}{}{}",
                listed(1),
                concert(9, "https://gigs.example/concerts/9-radiohead"),
                listed(2)
            ),
        );
        assert_eq!(doc.type_name, "MusicEvent");
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 9"));
        // Two listings naming no page (a theme's and a plugin's node, two nights at a venue):
        // the page's own, the first, as before.
        let unnamed = |n: u32| {
            format!(
                r#"<script type="application/ld+json">{{"@type":"MusicEvent","name":"Night {n}","startDate":"2027-05-0{n}T19:30:00"}}</script>"#
            )
        };
        let doc = page(artist, &format!("{}{}", unnamed(1), unnamed(2)));
        assert_eq!(doc.type_name, "MusicEvent");
        assert_eq!(doc.name.as_deref(), Some("Night 1"));
        // An AMP page whose head canonical is the story, its article naming the canonical, and a
        // related article in the body: the article (the canonical is the page's address too).
        let doc = read_page(
            "https://news.example/story/amp",
            r#"<html><head><title>Story</title><link rel="canonical" href="https://news.example/story"><script type="application/ld+json">{"@type":"NewsArticle","headline":"Actual story","url":"https://news.example/story","description":"What happened."}</script></head><body><script type="application/ld+json">{"@type":"Article","headline":"Related 1","url":"https://news.example/r1"}</script></body></html>"#,
        );
        assert_eq!(doc.type_name, "NewsArticle");
        assert_eq!(doc.name.as_deref(), Some("Actual story"));
        assert_eq!(doc.description.as_deref(), Some("What happened."));
        // An article naming its page through mainEntityOfPage, with related articles in the
        // body: the article.
        let doc = read_page(
            "https://news.example/story",
            r#"<html><head><title>Story</title><script type="application/ld+json">{"@type":"NewsArticle","headline":"Actual story","mainEntityOfPage":"https://news.example/story"}</script></head><body><script type="application/ld+json">[{"@type":"Article","headline":"Related 1","url":"https://news.example/r1"},{"@type":"Article","headline":"Related 2","url":"https://news.example/r2"}]</script></body></html>"#,
        );
        assert_eq!(doc.type_name, "NewsArticle");
        assert_eq!(doc.name.as_deref(), Some("Actual story"));
    }
    #[test]
    fn a_music_objects_video_is_a_part_of_its_page() {
        // A Spotify track page: og:title names the track; its JSON-LD holds a VideoObject for
        // the track's video, named "… | Music Video". The card is the track.
        let url = "https://open.spotify.com/track/63OQupATfueTdZMWTxW03A";
        let video = r#"<script type="application/ld+json">[{"@type":"VideoObject","name":"Karma Police - Radiohead | Music Video","description":"Watch the video.","thumbnailUrl":"https://i.example/x.jpg","uploadDate":"2025-01-01"}]</script>"#;
        let mut doc = read_page(
            url,
            &format!(
                r#"<html><head><title>Karma Police - Song, Lyrics, and Music Video by Radiohead | Spotify</title><meta property="og:title" content="Karma Police"><meta property="og:site_name" content="Spotify">{video}</head><body></body></html>"#
            ),
        );
        assert_eq!(doc.name.as_deref(), Some("Karma Police"));
        crate::music::type_card(url, None, &mut doc);
        assert_eq!(doc.type_name, "MusicRecording");
        assert_eq!(doc.name.as_deref(), Some("Karma Police"));
        // A track's own audio (a SoundCloud track's AudioObject, naming and describing it) is
        // the page's: kept.
        let doc = read_page(
            "https://soundcloud.com/radiohead/karma-police",
            r#"<html><head><title>SoundCloud</title><script type="application/ld+json">{"@type":"AudioObject","name":"Karma Police","description":"From OK Computer.","url":"https://soundcloud.com/radiohead/karma-police"}</script></head><body></body></html>"#,
        );
        assert_eq!(doc.name.as_deref(), Some("Karma Police"));
        assert_eq!(doc.description.as_deref(), Some("From OK Computer."));
        // Any other page keeps its video.
        let doc = read_page(
            "https://videos.example/watch/1",
            &format!("<html><head><title>Videos</title>{video}</head><body></body></html>"),
        );
        assert_eq!(doc.type_name, "VideoObject");
        assert_eq!(
            doc.name.as_deref(),
            Some("Karma Police - Radiohead | Music Video")
        );
    }

    /// A page's wall check and its card are read from one parse of its body. An answer the wall
    /// check settles without a parse is parsed once, for the card, when it is read as HTML, and
    /// never otherwise. The head's alternates come from the same tree.
    #[test]
    fn a_page_is_parsed_once_for_its_wall_check_and_its_card() {
        let read = |content_type: &str, status: u16, body: &str| {
            let mut fetched = crate::sources::fixture::page(
                "https://example.org/page",
                content_type,
                body.into(),
                None,
            );
            fetched.status = status;
            let before = crate::resolver::html::counted::documents();
            let out = extract("https://example.org/page", &fetched, 100).unwrap();
            (crate::resolver::html::counted::documents() - before, out)
        };
        let page = r#"<html><head><title>Page</title><link rel="alternate" type="application/rss+xml" href="/feed.xml"><link rel="alternate" type="application/activity+json" href="/ap"></head><body><p>Text</p></body></html>"#;
        let alternates = Some(crate::resolver::cards::Alternates {
            activitypub: Some("https://example.org/ap".into()),
            rss: Some("https://example.org/feed.xml".into()),
        });
        // HTML: the wall check parses it, and the card is read from that tree.
        let (parses, out) = read("text/html; charset=utf-8", 200, page);
        assert_eq!(parses, 1);
        assert_eq!(out.doc.status, Status::Ok);
        assert_eq!(out.title.as_deref(), Some("Page"));
        assert_eq!(
            out.doc.attributes.get("feed").map(String::as_str),
            Some("https://example.org/feed.xml")
        );
        assert_eq!(out.alternates, alternates);
        // No type: the wall check settles it unparsed, and the card's read parses it.
        let (parses, out) = read("", 200, page);
        assert_eq!(parses, 1);
        assert_eq!(out.title.as_deref(), Some("Page"));
        assert_eq!(out.alternates, alternates);
        // A wall is parsed by its check alone.
        let (parses, out) = read("text/html", 503, "<title>Just a moment</title>");
        assert_eq!(parses, 1);
        assert_eq!(out.doc.status, Status::Unreachable);
        assert_eq!(out.alternates, None);
        // An error page that is no wall, too: its status ends the read.
        let (parses, out) = read("text/html", 500, page);
        assert_eq!(parses, 1);
        assert_eq!(out.doc.status, Status::Error);
        assert_eq!(out.alternates, None);
        // What is not read as HTML is never parsed: JSON, an image, a feed.
        for (content_type, body) in [
            ("application/json", r#"{"@type":"Article","name":"Named"}"#),
            ("image/png", page),
            (
                "application/rss+xml",
                "<rss><channel><title>Feed</title></channel></rss>",
            ),
        ] {
            let (parses, out) = read(content_type, 200, body);
            assert_eq!(parses, 0, "{content_type}");
            assert_eq!(out.doc.status, Status::Ok, "{content_type}");
            assert_eq!(out.alternates, None, "{content_type}");
        }
    }
}
