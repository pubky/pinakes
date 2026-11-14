use super::{calendar, page, Attendance};
use crate::{
    config::EventConfig,
    model::ResourceMeta,
    resolver::{
        egress::{Egress, FetchOptions, Fetched, Validators},
        osm::OsmResolver,
    },
};
use anyhow::{Context, Result};
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};

type StoredHorizon = (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>);

pub struct EventService {
    pub pool: PgPool,
    pub config: EventConfig,
    pub osm: Arc<OsmResolver>,
}
impl EventService {
    pub async fn fetch(&self, egress: &Egress, uri: &str) -> Result<Fetched> {
        self.fetch_at(egress, uri, chrono::Utc::now()).await
    }
    /// Explicit observation clock also lets recurrence refreshes be exercised without sleeping.
    pub async fn fetch_at(
        &self,
        egress: &Egress,
        uri: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Fetched> {
        let mapped = calendar::canonical_feed(uri);
        let feed_hint = is_feed(uri) && self.config.calendars;
        let validators = if feed_hint {
            let end = now
                .checked_add_months(chrono::Months::new(self.config.months_forward))
                .context("calendar horizon overflow")?;
            let refresh_before = now + (end - now) / 2;
            sqlx::query_scalar::<_, serde_json::Value>(
                "SELECT validators FROM event_fetch WHERE resource_id=$1 AND last_snapshot_at IS NOT NULL AND held_keys IS NULL AND missing_keys='{}'::jsonb AND horizon_end>$2 AND EXISTS(SELECT 1 FROM resource r WHERE r.id=event_fetch.resource_id AND r.status='ok' AND r.missing_streak=0)",
            )
            .bind(crate::model::identity(uri)?.0)
            .bind(refresh_before)
            .fetch_optional(&self.pool)
            .await?
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
        } else {
            Validators::default()
        };
        let options = FetchOptions {
            feed_cap: feed_hint.then_some(self.config.feed_body_bytes),
            feed_hint,
            validators,
            ..Default::default()
        };
        let fetched = egress.fetch_with(&mapped, &options).await?;
        // Conditional requests resume only after a snapshot is accepted and key-loss/hold
        // confirmation is complete. Otherwise a new ETag followed by perpetual 304s
        // would leave the first disappearance observation pending forever.
        Ok(fetched)
    }
    /// An established feed that is gone (404/410/451) or answers with an empty *feed* is an
    /// empty snapshot, subject to the same two-observation confirmation as missing keys.
    /// A 200 that is no longer a feed (HTML: a wall, a sign-in page or a move) never confirms
    /// disappearance: the feed is unreachable (normal backoff) and keeps its occurrences.
    /// Transport/parse errors do not qualify.
    pub async fn disappeared(
        &self,
        uri: &str,
        status: u16,
        feed_body: bool,
    ) -> Result<Option<ResourceMeta>> {
        if !self.config.calendars || !matches!(status, 200 | 404 | 410 | 451) {
            return Ok(None);
        }
        // A stored snapshot always stores its horizon with it.
        let horizon: Option<StoredHorizon> =
            sqlx::query_as("SELECT horizon_start,horizon_end FROM event_fetch WHERE resource_id=$1 AND last_snapshot_at IS NOT NULL AND horizon_start IS NOT NULL AND horizon_end IS NOT NULL")
                .bind(crate::model::identity(uri)?.0).fetch_optional(&self.pool).await?;
        let Some((start, end)) = horizon else {
            return Ok(None);
        };
        if status == 200 && !feed_body {
            crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
            return Err(crate::resolver::interstitial::Blocked.into());
        }
        let mut doc = ResourceMeta::new(uri, "event", 86400);
        doc.status = crate::model::Status::Missing;
        doc.container = Some(Box::new(super::Container {
            horizon_start: start,
            horizon_end: end,
            limits: self.config.container.clone(),
            held_retry_secs: self.config.held_retry_secs,
            ..Default::default()
        }));
        Ok(Some(doc))
    }
    pub async fn container(
        &self,
        uri: &str,
        fetched: &Fetched,
        deadline: tokio::time::Instant,
    ) -> Result<Option<ResourceMeta>> {
        // A body cut off at the page cap is never a snapshot: its missing events are unread.
        if !self.config.calendars || !(200..300).contains(&fetched.status) || fetched.truncated {
            return Ok(None);
        }
        let bytes = &fetched.body;
        let ics = ics_body(bytes);
        if !feed_like(fetched) {
            return Ok(None);
        }
        // An organiser calendar answered from another origin is not that calendar: no snapshot,
        // so its stored dates stay (`event/adapters/service.rs` answers the same).
        if super::adapters::off_origin_calendar(uri, fetched.url.as_str()) {
            return Err(crate::resolver::RetryLater {
                class: "off_origin",
                seconds: 3600,
            }
            .into());
        }
        let uri = uri.to_owned();
        let config = self.config.clone();
        let body = bytes.clone();
        let mut result = crate::resolver::html::blocking(move || {
            if ics {
                calendar::parse(&uri, &body, &config, chrono::Utc::now()).map(Some)
            } else {
                calendar::structured_feed(&uri, &body, &config, chrono::Utc::now())
            }
        })
        .await??;
        if let Some(doc) = &mut result {
            if let Some(c) = &mut doc.container {
                c.validators = Some(fetched.validators.clone());
                self.enrich_container(c, deadline).await?;
            }
        }
        Ok(result)
    }
    pub async fn enrich_container(
        &self,
        c: &mut super::Container,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        // One request per distinct structured address, bounded by the total resolver deadline.
        let mut venues: std::collections::BTreeMap<String, (super::Venue, super::EventFields)> =
            std::collections::BTreeMap::new();
        for row in &mut c.occurrences {
            let key = serde_json::to_string(&row.doc.event.venue)?;
            if let Some((v, fields)) = venues.get(&key) {
                row.doc.event.venue = Some(v.clone());
                if row.doc.event.tz.is_none() {
                    row.doc.event.tz = fields.tz.clone();
                    row.doc.event.timezone_attribution = fields.timezone_attribution.clone();
                }
            } else {
                let remaining = deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .saturating_sub(Duration::from_millis(100));
                if !matches!(
                    tokio::time::timeout(remaining, self.venue(&mut row.doc)).await,
                    Ok(Ok(()))
                ) {
                    row.doc.event.warnings.push("venue_unavailable".into());
                }
                if let Some(v) = &row.doc.event.venue {
                    venues.insert(key, (v.clone(), row.doc.event.clone()));
                }
            }
            page::finalize(&mut row.doc, false);
        }
        Ok(())
    }
    pub async fn page(
        &self,
        uri: &str,
        fetched: &Fetched,
        mut card: ResourceMeta,
        egress: &Egress,
        deadline: tokio::time::Instant,
    ) -> Result<ResourceMeta> {
        if !(200..300).contains(&fetched.status) {
            return Ok(card);
        }
        // Whether the fetch ended at the page asked for (a trailing slash aside): a redirect to
        // another page never makes the two spellings of one event.
        let landed = url::Url::parse(uri).is_ok_and(|u| {
            u.origin() == fetched.url.origin()
                && u.path().trim_end_matches('/') == fetched.url.path().trim_end_matches('/')
        });
        // Past the page cap: a page cut where its long head closes, or a whole page within the
        // head budget (`egress::page_fits`).
        anyhow::ensure!(
            crate::resolver::egress::page_fits(fetched),
            "non-calendar page exceeds 1 MB"
        );
        let input_uri = uri.to_owned();
        let fetched = crate::resolver::web::as_read(fetched).into_owned();
        let config = self.config.clone();
        let ttl = card.ttl_secs;
        let (mut event, calendar_link, advertised, event_candidate, platform_extra) =
            crate::resolver::html::blocking(move || -> anyhow::Result<_> {
                let uri = input_uri.as_str();
                let fetched = &fetched;
                let mut event = ResourceMeta::new(uri, "web", ttl);
                let mut calendar_link = None;
                let mut advertised =
                    header_alternate(fetched.validators.link.as_deref(), &fetched.url);
                let event_candidate;
                {
                    let text = String::from_utf8_lossy(&fetched.body);
                    // The page's structured data, read together: a body that is JSON, and
                    // its scripts (a page listing several events is a list, whichever script
                    // holds each).
                    let mut values = vec![];
                    let body = page::lenient_json(&text);
                    let body_json = body.is_some();
                    values.extend(body);
                    let html = crate::resolver::html::document(&text)?;
                    event_candidate = text.contains("startDate")
                        || text.contains("dtstart")
                        || text.contains("dt-start");
                    if advertised.is_none() {
                        advertised = html
                            .select(
                                &scraper::Selector::parse("head link[rel~=alternate][href]")
                                    .unwrap(),
                            )
                            .find_map(|e| {
                                let mime = e.value().attr("type")?;
                                if !matches!(
                                    mime,
                                    "application/activity+json" | "application/ld+json"
                                ) {
                                    return None;
                                }
                                Some((
                                    fetched.url.join(e.value().attr("href")?).ok()?.to_string(),
                                    mime.to_owned(),
                                ))
                            });
                    }
                    for node in html.select(
                        &scraper::Selector::parse("script[type='application/ld+json']").unwrap(),
                    ) {
                        if let Some(mut v) = page::lenient_json(&node.text().collect::<String>()) {
                            crate::resolver::web::decode_json_strings(&mut v);
                            values.push(v);
                        }
                    }
                    // The page's addresses (the one asked for, the one fetched, its canonical,
                    // its og:url): an event naming any of them is the page's own.
                    let pages = page::page_addresses(uri, &fetched.url, &html);
                    page::jsonld_all(
                        &mut event,
                        &values.iter().collect::<Vec<_>>(),
                        &fetched.url,
                        &pages,
                    );
                    if let Some(v) = values.first().filter(|_| body_json) {
                        page::activity(&mut event, v, &fetched.url);
                    }
                    if config.calendars {
                        calendar_link = html
                            .select(
                                &scraper::Selector::parse(
                                    "link[type='text/calendar'][href],a[href]",
                                )
                                .unwrap(),
                            )
                            .filter_map(|el| el.value().attr("href"))
                            .filter(|href| {
                                href.contains("/export/event/")
                                    || href.ends_with(".ics")
                                    || href.ends_with("/ical/")
                            })
                            .filter_map(|href| crate::resolver::web::origin_url(&fetched.url, href))
                            .find(|link| own_calendar_link(&fetched.url, link));
                    }
                    page::microformats(&mut event, &html, &fetched.url);
                }
                let mut platform_extra = None;
                if let Some(d) =
                    super::adapters::detected_html(uri, &String::from_utf8_lossy(&fetched.body))
                        .filter(|d| super::adapters::manifest(d, &config).enabled)
                {
                    // Self-hosted software cannot be classified from a shared vendor domain.
                    if super::adapters::detect(uri).is_none() {
                        platform_extra =
                            super::adapters::service::endpoint(d, uri, Some(fetched), &config)
                                .ok()
                                .flatten()
                                .map(|(target, opts)| (d.id.clone(), target, opts));
                    }
                    if let Some(extra) = super::adapters::state::parse(&d.id, uri, &fetched.body) {
                        if !event.type_path.contains(&"Event".into()) {
                            event.set_type("Event");
                        }
                        // Both inputs are bounded by the page cap; merge preserves JSON-LD precedence.
                        let _ = page::merge_missing(&mut event, &extra);
                    }
                }
                // Whatever base each parser was handed, the performers came from this fetch.
                event.event.read_from(fetched);
                Ok((
                    event,
                    calendar_link,
                    advertised,
                    event_candidate,
                    platform_extra,
                ))
            })
            .await??;
        let is_event = event.type_path.contains(&"Event".into());
        if advertised.is_some()
            || (!is_event && event_candidate)
            || (is_event && event.event.tz.is_none())
        {
            let (alternate, accept) =
                advertised.unwrap_or_else(|| (uri.to_owned(), "application/activity+json".into()));
            let remaining = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .saturating_sub(Duration::from_millis(100));
            if remaining >= Duration::from_millis(100) {
                let options = FetchOptions {
                    accept: Some(accept),
                    ..Default::default()
                };
                if let Ok(Ok(response)) = tokio::time::timeout(
                    remaining.min(Duration::from_secs(2)),
                    egress.discover(&alternate, &options, card.ttl_secs),
                )
                .await
                {
                    if response.status == 200 {
                        event = crate::resolver::html::blocking(move || {
                            if let Ok(v) = serde_json::from_slice(&response.body) {
                                let base = crate::resolver::web::read_at(&response.url);
                                page::activity(&mut event, &v, &base);
                            }
                            event
                        })
                        .await?;
                    }
                }
            }
        }
        if let Some(link) = calendar_link
            .filter(|_| event.type_path.contains(&"Event".into()) && event.event.tz.is_none())
        {
            let own_origin = crate::group_keys::same_origin(uri, &link);
            let remaining = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .saturating_sub(Duration::from_millis(100));
            if let Ok(Ok(response)) = tokio::time::timeout(
                remaining.min(Duration::from_secs(2)),
                egress.discover(
                    &link,
                    &FetchOptions {
                        feed_cap: Some(self.config.feed_body_bytes),
                        feed_hint: true,
                        ..Default::default()
                    },
                    card.ttl_secs,
                ),
            )
            .await
            {
                if response.status == 200 {
                    let answered_here =
                        crate::group_keys::same_url_origin(uri, response.url.as_str());
                    let config = self.config.clone();
                    if let Ok(Ok(mut calendar)) = crate::resolver::html::blocking(move || {
                        calendar::parse(&link, &response.body, &config, chrono::Utc::now())
                    })
                    .await
                    {
                        if let Some(c) = calendar
                            .container
                            .take()
                            .filter(|c| c.occurrences.len() == 1)
                        {
                            let fallback = &c.occurrences[0].doc;
                            if !event.type_path.contains(&"Event".into()) {
                                event.set_type("Event");
                                event.source = "ics".into();
                            }
                            super::page::merge_missing(&mut event, fallback)?;
                            // The UID joins (in the corroborated mode) only when the link is on
                            // the page's own origin, not where a redirect took the page, and the
                            // file was answered from the page's own URL origin (scheme, host and
                            // port, after redirects).
                            if let Some(uid) = c
                                .occurrences
                                .first()
                                .map(|o| o.uid.as_str())
                                .filter(|_| own_origin && answered_here)
                            {
                                crate::group_keys::add(&mut event, "ics_uid", uid);
                            }
                        }
                    }
                }
            }
        }
        if let Some((id, target, mut opts)) = platform_extra {
            let d = super::adapters::definition(&id).expect("detected platform");
            let manifest = super::adapters::manifest(d, &self.config);
            opts.feed_hint = is_feed(&target) || d.parser == "ics_link";
            opts.feed_cap = opts.feed_hint.then_some(self.config.feed_body_bytes);
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let enrichment = tokio::time::timeout(remaining.min(Duration::from_secs(2)), async {
                let response = super::adapters::service::discover_enrichment(
                    egress,
                    &target,
                    &opts,
                    manifest.cadence_floor,
                    d,
                )
                .await?;
                anyhow::ensure!(response.status == 200, "optional platform unavailable");
                let config = self.config.clone();
                super::adapters::service::enrichment_blocking(uri, d, &response, &config).await
            })
            .await;
            // HTML detection on an unregistered host is only a hint. An unrelated
            // API, policy denial or parse error must not veto the page's own facts.
            match enrichment {
                Ok(Ok(Some(extra))) => {
                    if extra.type_path.contains(&"Event".into()) {
                        event.set_type("Event");
                        page::merge_missing(&mut event, &extra)?;
                    }
                }
                Ok(Ok(None)) => {}
                _ => {
                    crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
                    card.event.warnings.push("enrichment_unavailable".into());
                    event.event.warnings.push("enrichment_unavailable".into());
                }
            }
        }
        if !event.type_path.contains(&"Event".into()) {
            let mut card = card;
            if card.type_path.contains(&"Event".into()) {
                card.set_type("WebPage");
                card.start = None;
                card.end = None;
            }
            return Ok(card);
        }
        if event.name.is_none() {
            event.name = card.name;
        }
        if event.description.is_none() {
            event.description = card.description;
        }
        if event.image.is_none() {
            event.image = card.image;
        }
        event.aliases = card.aliases;
        // An event page's canonical aliases it only as the other spelling of its event on a
        // platform that gives each event two (`alias::event_spelling`), and only when the fetch
        // ended at the page itself.
        event.event_spellings = card
            .alias_candidates
            .into_iter()
            .filter(|c| landed && crate::alias::event_spelling(uri, c).is_some())
            .collect();
        event.attributes = card.attributes;
        event.site = card.site;
        // The card's identity claims (its same-origin canonical and og:url, typed identifiers,
        // sameAs) describe this page too, so they stay with the event. Redirect aliases are not
        // among them (they never become group keys), and the grouping rules decide what
        // each claim may join.
        for key in card.group_keys {
            if event.group_keys.len() >= 16 {
                break;
            }
            if !event.group_keys.iter().any(|k| k.key == key.key) {
                event.group_keys.push(key);
            }
        }
        page::finalize(&mut event, true);
        // Geocoding is enrichment. A slow/unavailable service must not discard page facts.
        let remaining = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .saturating_sub(Duration::from_millis(25));
        if !matches!(
            tokio::time::timeout(remaining, self.venue(&mut event)).await,
            Ok(Ok(()))
        ) {
            event.event.warnings.push("venue_unavailable".into());
        }
        page::finalize(&mut event, false);
        event.bounded(crate::resolver::web::ATTRIBUTES)
    }
    pub async fn venue(&self, doc: &mut ResourceMeta) -> Result<()> {
        if !self.config.geocoding
            || doc.event.attendance == Some(Attendance::Online)
            || doc.location.is_some()
        {
            return Ok(());
        }
        doc.event.bound();
        let Some(venue) = &mut doc.event.venue else {
            return Ok(());
        };
        if venue.location.is_some()
            || venue.osm_id.is_some()
            || !venue.address.contains_key("street")
            || !(venue.address.contains_key("city") || venue.address.contains_key("postalcode"))
        {
            return Ok(());
        }
        let mut url: url::Url = format!(
            "{}/search",
            self.osm.config.nominatim_url.trim_end_matches('/')
        )
        .parse()?;
        url.query_pairs_mut()
            .append_pair("format", "jsonv2")
            .append_pair("addressdetails", "1")
            .append_pair("namedetails", "1")
            .append_pair("limit", "2")
            .append_pair("accept-language", "en");
        for k in ["street", "city", "postalcode", "country"] {
            if let Some(v) = venue.address.get(k) {
                url.query_pairs_mut().append_pair(k, v);
            }
        }
        let Some(value) = self.osm.request(url).await? else {
            return Ok(());
        };
        let Some(hits) = value.as_array() else {
            return Ok(());
        };
        if hits.len() != 1 {
            return Ok(());
        }
        let hit = &hits[0];
        let normalize = |s: &str| {
            s.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        };
        let address = &hit["address"];
        let mut matching = 0usize;
        let mut total = 0usize;
        for (k, keys) in [
            ("city", &["city", "town", "village", "municipality"][..]),
            ("postalcode", &["postcode"][..]),
            ("country", &["country_code", "country"][..]),
            ("street", &["road", "pedestrian", "footway"][..]),
        ] {
            if let Some(expected) = venue.address.get(k) {
                total += 1;
                let expected = normalize(expected);
                let matched = keys
                    .iter()
                    .filter_map(|key| address[key].as_str())
                    .any(|v| {
                        let v = normalize(v);
                        if k == "street" {
                            let number = address["house_number"]
                                .as_str()
                                .map(normalize)
                                .unwrap_or_default();
                            expected == format!("{v}{number}") || expected == format!("{number}{v}")
                        } else {
                            expected == v
                        }
                    });
                if matched {
                    matching += 1;
                }
            }
        }
        let score = matching as f64 / total.max(1) as f64;
        if score < self.config.geocode_score {
            return Ok(());
        }
        let place = self.geocoded(venue, hit).await?;
        if matching == total
            && venue
                .name
                .as_deref()
                .zip(place.name.as_deref())
                .is_some_and(|(a, b)| !a.is_empty() && normalize(a) == normalize(b))
        {
            let id = crate::store::ensure_resource(&self.pool, &place.url).await?;
            venue.resource_id = Some(id);
            venue.location_source = Some(crate::model::LocationSource::Osm);
        }
        if doc.event.tz.is_none() {
            doc.event.tz = place
                .location
                .as_ref()
                .and_then(super::time::coordinate_zone);
            if doc.event.tz.is_some() {
                doc.event.timezone_attribution = Some(super::time::ATTRIBUTION.into());
            }
        }
        Ok(())
    }
    /// Place `venue` at Nominatim's `hit`: the place is saved to `osm_place` and the venue names
    /// it (its point is read from there, never stored with the card).
    async fn geocoded(
        &self,
        venue: &mut super::Venue,
        hit: &serde_json::Value,
    ) -> Result<ResourceMeta> {
        let mut place = crate::resolver::osm::map_place(hit, self.osm.config.ttl_secs)?;
        // Nominatim name can be separate from namedetails on search responses.
        if place.name.is_none() {
            place.name = hit["name"].as_str().map(str::to_owned);
        }
        let key = crate::resolver::osm::parse_id(&place.url).expect("mapped OSM id");
        let version = self
            .osm
            .save(&key, &place, hit["display_name"].as_str())
            .await?;
        venue.osm_id = Some(key.key());
        venue.osm_version = Some(version);
        venue.location = None;
        venue.location_source = Some(crate::model::LocationSource::Geocoded);
        Ok(place)
    }
    /// A job posting's workplace (`kinds::jobs`), placed as an event's venue is when it names a
    /// street, else at its town: the one settlement Nominatim finds for it whose name (and
    /// country, when the posting names one) is the posting's, at `geocode_score` or above, and
    /// whose region and postal region contradict none the posting names, in its address or in
    /// its one-line place ("Springfield, IL", "Springfield, 90210").
    pub async fn workplace(&self, doc: &mut ResourceMeta) -> Result<()> {
        if !self.config.geocoding || doc.location.is_some() {
            return Ok(());
        }
        doc.event.bound();
        let Some(venue) = &doc.event.venue else {
            return Ok(());
        };
        if venue.location.is_some() || venue.osm_id.is_some() {
            return Ok(());
        }
        let address = venue.address.clone();
        if address.contains_key("street")
            && (address.contains_key("city") || address.contains_key("postalcode"))
        {
            return self.venue(doc).await;
        }
        let Some(line) = address.get("city").cloned() else {
            return Ok(());
        };
        let mut url: url::Url = format!(
            "{}/search",
            self.osm.config.nominatim_url.trim_end_matches('/')
        )
        .parse()?;
        url.query_pairs_mut()
            .append_pair("format", "jsonv2")
            .append_pair("addressdetails", "1")
            .append_pair("namedetails", "1")
            .append_pair("limit", "2")
            .append_pair("accept-language", "en")
            .append_pair("featureType", "settlement");
        // "Singapore, Singapore", "San Francisco, CA": a town and what holds it, in one line.
        let (town, country, held) = match line.split_once(',') {
            Some((town, held)) if !address.contains_key("country") => {
                url.query_pairs_mut().append_pair("q", &line);
                (town.trim().to_owned(), None, held)
            }
            _ => {
                for (k, param) in [
                    ("city", "city"),
                    ("region", "state"),
                    ("postalcode", "postalcode"),
                    ("country", "country"),
                ] {
                    if let Some(v) = address.get(k) {
                        url.query_pairs_mut().append_pair(param, v);
                    }
                }
                (line.clone(), address.get("country").cloned(), "")
            }
        };
        // What holds the town in one line: in each part, a first word that is a state's,
        // province's or territory's code is a region ("IL", "ON N6A 3K7"), and a word shaped
        // like a numeric postcode is the postcode ("CA 94105", "90210"); "District 1" has none.
        let mut held_codes = Vec::new();
        let mut held_postcode = None;
        for part in held.split(',') {
            let words: Vec<&str> = part.split_whitespace().collect();
            held_codes.extend(
                words
                    .first()
                    .copied()
                    .filter(|w| REGION_CODES.iter().any(|(c, _)| c == w)),
            );
            held_postcode = held_postcode.or_else(|| numeric_postcode(&words));
        }
        let Some(value) = self.osm.request(url).await? else {
            return Ok(());
        };
        let [hit] = value.as_array().map(Vec::as_slice).unwrap_or_default() else {
            return Ok(());
        };
        let normalize = |s: &str| {
            s.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        };
        let found = &hit["address"];
        let named = ["city", "town", "village", "municipality", "hamlet"]
            .iter()
            .filter_map(|k| found[k].as_str())
            .chain(hit["name"].as_str())
            .any(|name| normalize(name) == normalize(&town));
        let countries: Vec<String> = ["country_code", "country"]
            .iter()
            .filter_map(|k| found[k].as_str())
            .map(normalize)
            .collect();
        let same_country = country
            .as_deref()
            .map(|country| countries.contains(&normalize(country)));
        // The region the posting names is one of the settlement's regions, by name or by its
        // ISO 3166-2 code ("Illinois", "IL", "US-IL"); a settlement whose address names no
        // region cannot contradict it.
        let codes: Vec<&str> = found
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| k.starts_with("ISO3166-2"))
            .filter_map(|(_, v)| v.as_str())
            .collect();
        let regions: Vec<String> = ["state", "province", "region", "state_district", "county"]
            .iter()
            .filter_map(|k| found[k].as_str())
            .chain(
                codes
                    .iter()
                    .flat_map(|code| [*code, code.rsplit('-').next().unwrap_or(code)]),
            )
            .map(normalize)
            .collect();
        let other_region = address
            .get("region")
            .is_some_and(|r| !regions.is_empty() && !regions.contains(&normalize(r)));
        // In one line, a region code that is the settlement's own ISO 3166-2 code agrees,
        // whatever country the table gives it ("Chennai, TN" in IN-TN). Another contradicts a
        // settlement in none of its countries ("London, ON" in England), or one in its
        // country with another code ("Springfield, IL" in US-MO). A code that is the
        // settlement's country is its country ("Toronto, CA", "Berlin, DE"); other words after
        // a town ("UK", "EU", "HQ", a region in full) are left to Nominatim, which reads the
        // whole line.
        let country_code = found["country_code"].as_str().map(str::to_ascii_lowercase);
        let own_code = |code: &str| {
            codes
                .iter()
                .any(|c| normalize(c.rsplit('-').next().unwrap_or(c)) == normalize(code))
        };
        let other_code = held_codes.iter().any(|code| match &country_code {
            _ if own_code(code) || countries.contains(&normalize(code)) => false,
            None => false,
            Some(cc) if !REGION_CODES.iter().any(|(c, land)| c == code && land == cc) => true,
            Some(_) => !codes.is_empty() && !regions.contains(&normalize(code)),
        });
        // A numeric postcode's first digit is its postal region (US, DE, FR, CH, …); more
        // digits are not compared, as a city's one postcode in Nominatim stands for many
        // (Berlin's run from 10 to 14).
        let other_postcode = address
            .get("postalcode")
            .map(String::as_str)
            .or(held_postcode)
            .zip(found["postcode"].as_str())
            .is_some_and(|(ours, theirs)| {
                let first = |s: &str| s.trim().chars().next().filter(char::is_ascii_digit);
                matches!((first(ours), first(theirs)), (Some(a), Some(b)) if a != b)
            });
        let total = 1 + usize::from(same_country.is_some());
        let matching = usize::from(named) + usize::from(same_country == Some(true));
        // A settlement of another name, or in a country, region or postal region the posting
        // contradicts, is never placed, whatever `geocode_score` allows.
        if !named
            || same_country == Some(false)
            || other_region
            || other_code
            || other_postcode
            || (matching as f64 / total as f64) < self.config.geocode_score
        {
            return Ok(());
        }
        let venue = doc.event.venue.as_mut().expect("a venue");
        self.geocoded(venue, hit).await?;
        Ok(())
    }
}
/// The state, province and territory codes a one-line place writes after its town ("Austin,
/// TX"), with their countries: the United States', Canada's and Australia's.
#[rustfmt::skip]
const REGION_CODES: &[(&str, &str)] = &[
    ("AL", "us"), ("AK", "us"), ("AZ", "us"), ("AR", "us"), ("CA", "us"), ("CO", "us"),
    ("CT", "us"), ("DE", "us"), ("FL", "us"), ("GA", "us"), ("HI", "us"), ("ID", "us"),
    ("IL", "us"), ("IN", "us"), ("IA", "us"), ("KS", "us"), ("KY", "us"), ("LA", "us"),
    ("ME", "us"), ("MD", "us"), ("MA", "us"), ("MI", "us"), ("MN", "us"), ("MS", "us"),
    ("MO", "us"), ("MT", "us"), ("NE", "us"), ("NV", "us"), ("NH", "us"), ("NJ", "us"),
    ("NM", "us"), ("NY", "us"), ("NC", "us"), ("ND", "us"), ("OH", "us"), ("OK", "us"),
    ("OR", "us"), ("PA", "us"), ("RI", "us"), ("SC", "us"), ("SD", "us"), ("TN", "us"),
    ("TX", "us"), ("UT", "us"), ("VT", "us"), ("VA", "us"), ("WA", "us"), ("WV", "us"),
    ("WI", "us"), ("WY", "us"), ("DC", "us"),
    ("AB", "ca"), ("BC", "ca"), ("MB", "ca"), ("NB", "ca"), ("NL", "ca"), ("NS", "ca"),
    ("NT", "ca"), ("NU", "ca"), ("ON", "ca"), ("PE", "ca"), ("QC", "ca"), ("SK", "ca"),
    ("YT", "ca"),
    ("ACT", "au"), ("NSW", "au"), ("NT", "au"), ("QLD", "au"), ("SA", "au"), ("TAS", "au"),
    ("VIC", "au"), ("WA", "au"),
];
/// The numeric postcode among a place's words: four digits or more ("90210", "10115",
/// "94105-1234", "1012" of "1012 AB"), or three and two ("114 55").
fn numeric_postcode<'a>(words: &[&'a str]) -> Option<&'a str> {
    let digits = |w: &str| {
        let (a, b) = w.split_once('-').unwrap_or((w, "1"));
        [a, b]
            .iter()
            .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
            .then_some(a.len() + if w.contains('-') { b.len() } else { 0 })
    };
    words
        .iter()
        .enumerate()
        .find_map(|(i, w)| match digits(w)? {
            n if n >= 4 => Some(*w),
            3 if words
                .get(i + 1)
                .is_some_and(|next| digits(next) == Some(2) && !next.contains('-')) =>
            {
                Some(*w)
            }
            _ => None,
        })
}
pub fn is_feed(uri: &str) -> bool {
    uri.starts_with("webcal:")
        || uri.starts_with("webcals:")
        || url::Url::parse(uri).is_ok_and(|u| {
            u.path().ends_with(".ics")
                || u.path().ends_with("/ical/")
                || u.path().ends_with("/feed/ics")
                || u.path().ends_with("/feed/rss")
                || u.path().ends_with("/feed/json")
                || u.path().contains("/public-calendars/")
        })
}

pub struct CalendarResolver {
    pub web: crate::resolver::web::WebResolver,
}
#[async_trait::async_trait]
impl crate::resolver::Resolver for CalendarResolver {
    fn name(&self) -> &'static str {
        "event"
    }
    fn matches(&self, uri: &str) -> bool {
        crate::resolver::classify(uri) == Some("event")
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
        self.web.resolve(uri).await
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        Duration::from_secs(doc.ttl_secs)
    }
}

pub(crate) fn header_alternate(header: Option<&str>, base: &url::Url) -> Option<(String, String)> {
    header?.split(',').find_map(|part| {
        if !part.contains("rel=\"alternate\"") && !part.contains("rel=alternate") {
            return None;
        }
        let mime = if part.contains("application/activity+json") {
            "application/activity+json"
        } else if part.contains("application/ld+json") {
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
        } else {
            return None;
        };
        Some((
            base.join(part.split('<').nth(1)?.split('>').next()?)
                .ok()?
                .to_string(),
            mime.into(),
        ))
    })
}
/// Whether a calendar `link` on the page at `base` is the page's own: on its host, and below its
/// path, named after its last segment, or naming the page itself in its `url` parameter (a
/// portal's export, `/iCalendar/?id=…&url=<page>`).
pub(crate) fn own_calendar_link(base: &url::Url, link: &str) -> bool {
    let Ok(link) = url::Url::parse(link) else {
        return false;
    };
    if base.host_str() != link.host_str() {
        return false;
    }
    if link
        .query_pairs()
        .any(|(k, v)| k == "url" && url::Url::parse(&v).is_ok_and(|v| v == *base))
    {
        return true;
    }
    let stem = base
        .path()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    !stem.is_empty()
        && (link
            .path()
            .starts_with(&format!("{}/", base.path().trim_end_matches('/')))
            || link.path().contains(&format!("/{stem}."))
            || link.path().ends_with(&format!("/{stem}")))
}

fn ics_body(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(&bytes[..bytes.len().min(256)])
        .trim_start_matches(['\u{feff}', ' ', '\n', '\r'])
        .starts_with("BEGIN:VCALENDAR")
}
/// Calendar and feed bodies; everything else (notably HTML) is not a feed observation.
pub fn feed_like(fetched: &Fetched) -> bool {
    ics_body(&fetched.body)
        || matches!(
            fetched.content_type.as_str(),
            "text/calendar"
                | "application/ics"
                | "application/rss+xml"
                | "application/atom+xml"
                | "text/xml"
                | "application/xml"
                | "application/feed+json"
                | "application/json"
        )
}
