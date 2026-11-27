use super::{detect, manifest, options, state, Definition};
use crate::{
    adapter::Mode,
    config::EventConfig,
    event::{calendar, page},
    model::ResourceMeta,
    resolver::{
        egress::{Egress, FetchOptions, Fetched},
        web::WebResolver,
        Resolver,
    },
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;
use url::Url;

// Only explicitly classified parser failures may fall back to a plain web card.
// Transport, policy, database and task errors preserve the accepted document.
#[derive(Debug)]
struct ParseFailure(String);
impl std::fmt::Display for ParseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ParseFailure {}
fn parse_error(e: impl std::fmt::Display) -> anyhow::Error {
    ParseFailure(e.to_string()).into()
}
pub(super) fn upstream_status(f: &Fetched) -> Result<()> {
    if f.status >= 500 || matches!(f.status, 408 | 429) {
        return Err(anyhow::Error::new(crate::resolver::RetryLater {
            class: if f.status >= 500 {
                "http_5xx"
            } else {
                "http_4xx"
            },
            seconds: f.retry_after_secs.max(60),
        })
        .context(crate::resolver::Upstream(f.status)));
    }
    Ok(())
}

pub(crate) fn enrichment_status(f: &Fetched) -> Result<()> {
    upstream_status(f)?;
    if matches!(f.status, 401 | 403) {
        return Err(crate::resolver::RetryLater {
            class: "upstream_denied",
            seconds: 3600,
        }
        .into());
    }
    Ok(())
}

pub(crate) async fn discover_enrichment(
    egress: &Egress,
    uri: &str,
    options: &FetchOptions,
    ttl: u64,
    definition: &Definition,
) -> Result<Fetched> {
    discover(egress, uri, options, ttl, definition, true).await
}
/// [`discover_enrichment`] for a platform only guessed from a page's markup
/// (`detected_html`): a failed guess says nothing of an adapter, so it is not counted as drift.
pub(crate) async fn discover_hint(
    egress: &Egress,
    uri: &str,
    options: &FetchOptions,
    ttl: u64,
    definition: &Definition,
) -> Result<Fetched> {
    discover(egress, uri, options, ttl, definition, false).await
}
async fn discover(
    egress: &Egress,
    uri: &str,
    options: &FetchOptions,
    ttl: u64,
    definition: &Definition,
    drift: bool,
) -> Result<Fetched> {
    let fetched = egress.discover(uri, options, ttl).await?;
    enrichment_status(&fetched)?;
    if definition.parser == "mobilizon" && fetched.status == 200 {
        let body = fetched.body.clone();
        let failed = crate::resolver::html::blocking(move || {
            serde_json::from_slice::<Value>(&body)
                .ok()
                .is_some_and(|v| v["errors"].as_array().is_some_and(|a| !a.is_empty()))
        })
        .await?;
        if failed {
            egress.shorten_discovery(uri, options, 300).await?;
            if drift {
                crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
            }
            return Err(crate::resolver::RetryLater {
                class: "graphql",
                seconds: 300,
            }
            .into());
        }
    }
    Ok(fetched)
}

/// Meetup writes its descriptions in Markdown (`*local*`, `**casual**`), on its pages and in its
/// calendars: on meetup.com a card's description and its dates' are plain text.
fn meetup_plain(uri: &str, doc: &mut ResourceMeta) {
    let meetup = Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| h == "meetup.com" || h.ends_with(".meetup.com"));
    if !meetup {
        return;
    }
    let plain = |text: &mut Option<String>| {
        *text = text
            .as_deref()
            .map(crate::sources::markdown_plain)
            .filter(|t| !t.is_empty());
    };
    plain(&mut doc.description);
    if let Some(c) = doc.container.as_mut() {
        for o in &mut c.occurrences {
            plain(&mut o.doc.description);
        }
    }
}

pub struct PlatformResolver {
    pub web: WebResolver,
}
impl PlatformResolver {
    pub fn config(&self) -> &EventConfig {
        &self
            .web
            .events
            .as_ref()
            .expect("platform event service")
            .config
    }
    async fn inner(
        &self,
        uri: &str,
        probe: bool,
        selected: Option<&Definition>,
    ) -> Result<ResourceMeta> {
        let mut doc = self.read(uri, probe, selected).await?;
        meetup_plain(uri, &mut doc);
        Ok(doc)
    }
    async fn read(
        &self,
        uri: &str,
        probe: bool,
        selected: Option<&Definition>,
    ) -> Result<ResourceMeta> {
        let cfg = self.config();
        let Some(d) = selected.or_else(|| detect(uri)) else {
            return self.web.resolve(uri).await;
        };
        let m = manifest(d, cfg);
        if !m.enabled {
            if self.web.config.enabled
                && Url::parse(uri)
                    .is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "webcal" | "webcals"))
            {
                return self.web.resolve(uri).await;
            }
            return Err(crate::resolver::RetryLater {
                class: "disabled",
                seconds: 3600,
            }
            .into());
        }
        let local = cfg.clone();
        if d.parser == "nostr" {
            return super::nostr::resolve(
                uri,
                &self.web.egress,
                &local,
                &self.web.events.as_ref().unwrap().pool,
                probe,
            )
            .await;
        }
        if matches!(d.parser.as_str(), "portal" | "btcmap") {
            let mut doc = super::cursor::resolve(
                &self.web.events.as_ref().unwrap().pool,
                uri,
                d,
                &self.web.egress,
                &local,
                probe,
            )
            .await?;
            if !probe {
                self.enrich_cursor(&mut doc, &local, d).await?;
            }
            if doc.container.as_ref().is_some_and(|c| c.cursor.is_none()) {
                if let Some(row) = doc.container.take().and_then(|mut c| c.occurrences.pop()) {
                    doc = row.doc;
                    if doc.url != uri {
                        doc.aliases.push(doc.url.clone());
                    }
                    doc.url = uri.into();
                }
            }
            doc.source = d.id.clone();
            return Ok(doc);
        }
        let target = match endpoint(d, uri, None, cfg, crate::hubs::body_cap(&self.web.egress)) {
            Ok(target) => target,
            Err(_) if !probe => return self.web.resolve(uri).await,
            Err(error) => return Err(error),
        };
        let (target, mut opts) = target.unwrap_or((uri.to_owned(), FetchOptions::default()));
        opts.feed_hint = crate::event::service::is_feed(&target)
            || (d.mode == Mode::Container && feed_path(&target))
            || (d.id == "starticket" && target.ends_with(".xml"));
        opts.feed_cap = opts.feed_hint.then_some(local.feed_body_bytes);
        // An organiser calendar is the platform's own answer or none (`fetch_calendar`).
        let fetched = super::fetch_calendar(&self.web.egress, &target, &opts).await?;
        if d.requires_key && matches!(fetched.status, 401 | 403) {
            // The operator's key refused (Ticketmaster's `apikey`): counted under the platform's
            // ID (`pinakes_source_token_refusals_total`).
            if let Some(source) = crate::sources::KEYED.iter().find(|k| **k == d.id) {
                crate::sources::token_refused(source);
            }
        }
        if fetched.status == 304 {
            return Err(crate::resolver::NotModified.into());
        }
        if d.mode == Mode::None {
            let extracted = extract_blocking(uri, &fetched, m.cadence_floor).await?;
            let mut doc = ResourceMeta::new(uri, &d.id, m.cadence_floor);
            doc.name = extracted.doc.name.or(extracted.title);
            doc.image = extracted.doc.image;
            doc.description = extracted.doc.description.or(extracted.description);
            doc.status = extracted.doc.status;
            return doc.bounded(crate::resolver::web::ATTRIBUTES);
        }
        upstream_status(&fetched)?;
        if fetched.status != 200 {
            if let Some(doc) = self
                .web
                .events
                .as_ref()
                .unwrap()
                .disappeared(uri, fetched.status, true)
                .await?
            {
                return Ok(doc);
            }
            return Ok(extract_blocking(uri, &fetched, 3600).await?.doc);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        let parsed: Result<ResourceMeta> = async {
            let mut doc = if d.parser == "wikidata" {
                let item = wiki(uri, &fetched, &self.web.egress).await?;
                let mut doc = item.doc;
                if !probe {
                    credit_image(&mut doc, item.image, &self.web, deadline).await;
                }
                doc
            } else if let Some(doc) = calendar_blocking(uri, d, &fetched, &local).await? {
                doc
            } else {
                if let Some(doc) = self
                    .web
                    .events
                    .as_ref()
                    .unwrap()
                    .disappeared(
                        uri,
                        fetched.status,
                        crate::event::service::feed_like(&fetched),
                    )
                    .await?
                {
                    return Ok(doc);
                }
                let card = extract_blocking(uri, &fetched, 3600).await?.doc;
                let events = self.web.events.as_ref().unwrap();
                let mut doc = events
                    .page(uri, &fetched, card, &self.web.egress, deadline)
                    .await?;
                // APIs and calendar links fill only absent page fields. Never query an unrelated recommendation.
                let missing = doc.start.is_none()
                    || doc.event.tz.is_none()
                    || doc.location.is_none()
                        && doc.event.attendance != Some(crate::event::Attendance::Online);
                // A platform with an organiser feed reads the page's own calendar file anyway,
                // for its UID alone: the UID joins the page to its date in that feed. Read so,
                // the file is optional and adds nothing else.
                let uid_only = !probe && !missing && d.container_feed.is_some();
                // On a platform with an organiser feed, the file is optional on every read: a
                // refusal, an outage or a 451 there leaves the page's own facts as they are.
                let optional = d.container_feed.is_some();
                // Whether the file was asked for, and whether it answered with a calendar: then
                // its UID, or none, is the page's now.
                let (mut asked, mut answered) = (false, false);
                if !d.requires_key && (missing || uid_only) {
                    // Calendar-link discovery reads the page: in a parse slot.
                    let found = {
                        let (d, uri, page, cfg, cap) = (
                            d.clone(),
                            uri.to_owned(),
                            crate::resolver::web::as_read(&fetched).into_owned(),
                            cfg.clone(),
                            crate::hubs::body_cap(&self.web.egress),
                        );
                        crate::resolver::html::blocking(move || {
                            endpoint(&d, &uri, Some(&page), &cfg, cap)
                        })
                        .await?
                    };
                    if let Some((target, mut opts)) = found.map_err(parse_error)? {
                        if target != fetched.url.as_str() {
                            opts.feed_hint = crate::event::service::is_feed(&target)
                                || (d.mode == Mode::Container && feed_path(&target))
                                || (d.id == "starticket" && target.ends_with(".xml"));
                            opts.feed_cap = opts.feed_hint.then_some(local.feed_body_bytes);
                            asked = true;
                            let read = discover_enrichment(
                                &self.web.egress,
                                &target,
                                &opts,
                                m.cadence_floor,
                                d,
                            )
                            .await;
                            let read = match read {
                                Ok(f) => Some(f),
                                Err(_) if optional => None,
                                Err(error) => return Err(error),
                            };
                            if let Some(f) = read {
                                if f.status == 451 && !optional {
                                    return self.enrichment_gone(uri, &d.id).await;
                                }
                                if f.status == 200 && uid_only {
                                    if let Ok(Some(calendar)) =
                                        calendar_blocking(uri, d, &f, &local).await
                                    {
                                        answered = true;
                                        own_uid(&mut doc, &calendar);
                                    }
                                } else if f.status == 200 {
                                    if d.id == "luma" {
                                        // An event's API response names its calendar too: only
                                        // a calendar response turns the page into the calendar.
                                        if let Some(id) = serde_json::from_slice::<Value>(&f.body)
                                            .ok()
                                            .filter(|v| v["kind"] == "calendar")
                                            .and_then(|v| {
                                                v.pointer("/data/calendar/api_id")
                                                    .and_then(Value::as_str)
                                                    .map(str::to_owned)
                                            })
                                            .filter(|s| {
                                                s.len() <= 128
                                                    && s.bytes().all(|c| {
                                                        c.is_ascii_alphanumeric() || c == b'-'
                                                    })
                                            })
                                        {
                                            let mut url = Url::parse("https://api.lu.ma/ics/get")?;
                                            url.query_pairs_mut()
                                                .append_pair("entity", "calendar")
                                                .append_pair("id", &id);
                                            let opts = FetchOptions {
                                                feed_hint: true,
                                                feed_cap: Some(local.feed_body_bytes),
                                                ..Default::default()
                                            };
                                            {
                                                let feed = self
                                                    .web
                                                    .egress
                                                    .discover(url.as_str(), &opts, m.cadence_floor)
                                                    .await?;
                                                enrichment_status(&feed)?;
                                                if feed.status == 451 {
                                                    return self.enrichment_gone(uri, &d.id).await;
                                                }
                                                if feed.status == 200 {
                                                    if let Some(calendar) =
                                                        calendar_blocking(uri, d, &feed, &local)
                                                            .await?
                                                    {
                                                        doc = calendar;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // On a platform with an organiser feed, a calendar answer is
                                    // told apart from a page in its place (a sign-in wall).
                                    let calendar = if optional {
                                        calendar_blocking(uri, d, &f, &local).await.ok().flatten()
                                    } else {
                                        None
                                    };
                                    answered |= calendar.is_some();
                                    let extra = match calendar {
                                        Some(calendar) => Ok(Some(calendar)),
                                        None => enrichment_blocking(uri, d, &f, &local).await,
                                    };
                                    let extra = match extra {
                                        Ok(extra) => extra,
                                        Err(_) if optional => None,
                                        Err(error) => return Err(error),
                                    };
                                    if let Some(extra) = extra {
                                        merge(&mut doc, &extra).map_err(parse_error)?;
                                    }
                                }
                            }
                        }
                    }
                }
                // An optional read the file did not answer with a calendar (refused, failed,
                // gone, a page in its place) keeps the UID the page was stored with, on either
                // path, so a passing failure never moves the page out of its group. A calendar
                // that answers without one (two events, another origin) ends it.
                if optional
                    && asked
                    && !answered
                    && !probe
                    && !doc.group_keys.iter().any(|k| k.basis == "ics_uid")
                {
                    self.stored_uid(uri, &mut doc).await?;
                }
                doc
            };
            if !probe {
                if let Some(container) = &mut doc.container {
                    self.web
                        .events
                        .as_ref()
                        .unwrap()
                        .enrich_container(container, deadline)
                        .await?;
                }
            }
            if doc.type_path.contains(&"Event".into()) {
                // API zones must replace coordinate-derived enrichment, not the other way around.
                page::finalize(&mut doc, true);
                if !probe && !doc.event.warnings.iter().any(|w| w == "venue_unavailable") {
                    let events = self.web.events.as_ref().unwrap();
                    if !matches!(
                        tokio::time::timeout(Duration::from_secs(2), events.venue(&mut doc)).await,
                        Ok(Ok(()))
                    ) {
                        doc.event.warnings.push("venue_unavailable".into());
                    }
                }
                page::finalize(&mut doc, false);
            }
            doc.source = d.id.clone();
            doc.bounded(crate::resolver::web::ATTRIBUTES)
                .map_err(parse_error)
        }
        .await;
        match parsed {
            Ok(doc) => {
                if !probe {
                    self.derive_container_feed(uri, d, &doc, &local).await?;
                }
                Ok(doc)
            }
            Err(error) if probe || !error.is::<ParseFailure>() => Err(error),
            Err(_) => {
                crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
                let mut card = extract_blocking(uri, &fetched, m.cadence_floor).await?.doc;
                card.source = d.id.clone();
                Ok(card)
            }
        }
    }
    /// The `ics_uid` keys the stored document of `uri` holds, added to `doc`.
    async fn stored_uid(&self, uri: &str, doc: &mut ResourceMeta) -> Result<()> {
        let keys: Option<Value> = sqlx::query_scalar(
            "SELECT doc->'group_keys' FROM resource WHERE id=$1 AND doc ? 'group_keys'",
        )
        .bind(crate::model::identity(uri)?.0)
        .fetch_optional(&self.web.events.as_ref().unwrap().pool)
        .await?;
        let keys: Vec<crate::group_keys::GroupKey> = keys
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let mut stored = ResourceMeta::new(uri, "", 0);
        stored.group_keys = keys
            .into_iter()
            .filter(|k| k.basis == "ics_uid" && crate::group_keys::valid_stored(k))
            .collect();
        own_uid(doc, &stored);
        Ok(())
    }
    /// Queue the organiser feed of an event page (`adapters::container_feed`) as a resource
    /// derived from the page, like a calendar's members: charged to the page's referrers, within
    /// `limits.max_linked_resources` and `limits.max_derived_resources`. The link lives until the
    /// page's event ends, at least a day from this read, capped at the horizon; each read of the
    /// page renews it.
    async fn derive_container_feed(
        &self,
        uri: &str,
        d: &Definition,
        doc: &ResourceMeta,
        cfg: &EventConfig,
    ) -> Result<()> {
        if !cfg.adapters.derive_resources
            || !doc.type_path.contains(&"Event".into())
            || doc.container.is_some()
            || doc.status != crate::model::Status::Ok
        {
            return Ok(());
        }
        let Some(feed) = super::container_feed(d, uri) else {
            return Ok(());
        };
        let floor = chrono::Utc::now() + chrono::Duration::days(1);
        let until = doc
            .end
            .or(doc.start)
            .and_then(crate::event::time::bounded)
            .map_or(floor, |at| at.max(floor));
        crate::store::derive_resource_within(
            &self.web.events.as_ref().unwrap().pool,
            uri,
            &feed,
            &d.id,
            cfg.adapters.max_linked_resources,
            cfg.adapters.max_derived_resources,
            Some(until),
            Some(crate::store::derivation_horizon(cfg)),
        )
        .await?;
        Ok(())
    }
    async fn enrichment_gone(&self, uri: &str, source: &str) -> Result<ResourceMeta> {
        let mut doc = self
            .web
            .events
            .as_ref()
            .unwrap()
            .disappeared(uri, 451, true)
            .await?
            .unwrap_or_else(|| ResourceMeta::new(uri, source, 3600));
        doc.source = source.into();
        doc.status = crate::model::Status::Missing;
        Ok(doc)
    }
    async fn enrich_cursor(
        &self,
        doc: &mut ResourceMeta,
        cfg: &EventConfig,
        d: &Definition,
    ) -> Result<()> {
        let events = self.web.events.as_ref().unwrap();
        let Some(c) = doc.container.as_mut() else {
            return Ok(());
        };
        let mut links = std::collections::BTreeMap::new();
        // Which row names which link: a row without a future link, and a tombstoned row, stop
        // naming the link they named before (`store::link_sources`).
        let mut named = vec![];
        let mut gone: Vec<String> = c
            .deleted_uids
            .iter()
            .filter(|_| d.id == "btcmap")
            .cloned()
            .collect();
        for o in c.occurrences.iter().filter(|_| d.id == "btcmap") {
            let Some(until) = o
                .doc
                .end
                .and_then(crate::event::time::bounded)
                .or(o.doc.start.and_then(crate::event::time::bounded))
                .filter(|at| *at > chrono::Utc::now())
            else {
                gone.push(o.uid.clone());
                continue;
            };
            let Ok(url) = Url::parse(&o.doc.url) else {
                gone.push(o.uid.clone());
                continue;
            };
            if o.doc.url != doc.url
                && matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
            {
                links
                    .entry(url.to_string())
                    .and_modify(|at: &mut chrono::DateTime<chrono::Utc>| *at = (*at).max(until))
                    .or_insert(until);
                named.push((o.uid.clone(), url.to_string(), until));
            } else {
                gone.push(o.uid.clone());
            }
        }
        if cfg.adapters.derive_resources {
            for (link, until) in links.into_iter().take(cfg.adapters.max_linked_resources) {
                // Delta rows skip the horizon: a distant event never holds a slot past it, and
                // the expiry follows the horizon until the event is inside it
                // (`store::EXTEND_PARENT_DERIVATIONS`, on each feed apply).
                crate::store::derive_resource_within(
                    &events.pool,
                    &doc.url,
                    &link,
                    &d.id,
                    cfg.adapters.max_linked_resources,
                    cfg.adapters.max_derived_resources,
                    Some(until),
                    Some(crate::store::derivation_horizon(cfg)),
                )
                .await?;
            }
            crate::store::link_sources(&events.pool, &doc.url, &named, &gone).await?;
        }
        Ok(())
    }
}
async fn extract_blocking(
    uri: &str,
    f: &Fetched,
    ttl: u64,
) -> Result<crate::resolver::web::Extracted> {
    ensure!(
        crate::resolver::egress::page_fits(f),
        "non-feed page exceeds cap"
    );
    let (uri, f) = (uri.to_owned(), f.clone());
    crate::resolver::html::blocking(move || crate::resolver::web::extract(&uri, &f, ttl)).await?
}
async fn calendar_blocking(
    uri: &str,
    d: &Definition,
    f: &Fetched,
    cfg: &EventConfig,
) -> Result<Option<ResourceMeta>> {
    let (uri, id, f, cfg) = (uri.to_owned(), d.id.clone(), f.clone(), cfg.clone());
    crate::resolver::html::blocking(move || {
        let d = super::definition(&id).expect("registered adapter");
        if let Some(doc) = calendar_document(&uri, d, &f, &cfg)? {
            return Ok(Some(doc));
        }
        if cfg.calendars
            && (crate::event::service::is_feed(f.url.as_str())
                || d.id == "starticket"
                || d.mode == Mode::Container)
        {
            return calendar::structured_feed(&uri, &f.body, &cfg, chrono::Utc::now());
        }
        Ok(None)
    })
    .await?
    .map_err(parse_error)
}
pub(crate) async fn enrichment_blocking(
    uri: &str,
    d: &Definition,
    f: &Fetched,
    cfg: &EventConfig,
) -> Result<Option<ResourceMeta>> {
    if let Some(doc) = calendar_blocking(uri, d, f, cfg).await? {
        return Ok(Some(doc));
    }
    ensure!(
        crate::resolver::egress::page_fits(f),
        "non-feed enrichment exceeds cap"
    );
    let (uri, id, body) = (uri.to_owned(), d.id.clone(), f.body.clone());
    let mut doc = crate::resolver::html::blocking(move || state::parse(&id, &uri, &body)).await?;
    // The performers came from this fetch, not from the address the parser was handed.
    if let Some(doc) = &mut doc {
        doc.event.read_from(f);
    }
    Ok(doc)
}

fn merge(doc: &mut ResourceMeta, extra: &ResourceMeta) -> Result<()> {
    if extra.type_path.contains(&"Event".into()) && !doc.type_path.contains(&"Event".into()) {
        doc.set_type("Event");
    }
    if doc.event.timezone_attribution.is_some() && extra.event.tz.is_some() {
        doc.event.tz = None;
        doc.event.timezone_attribution = None;
    }
    own_uid(doc, extra);
    page::merge_missing(doc, extra)
}
/// The UID of the page's own calendar file (`calendar_document`), which joins the page to its
/// date in a feed (`groups`).
fn own_uid(doc: &mut ResourceMeta, calendar: &ResourceMeta) {
    for k in calendar.group_keys.iter().filter(|k| k.basis == "ics_uid") {
        if let Some(uid) = k.key.strip_prefix("ics_uid:") {
            crate::group_keys::add(doc, "ics_uid", uid);
        }
    }
}
#[async_trait]
impl Resolver for PlatformResolver {
    fn name(&self) -> &'static str {
        "event_platform"
    }
    fn matches(&self, uri: &str) -> bool {
        crate::resolver::classify(uri) == Some(self.name())
    }
    fn manifest_for(&self, uri: &str) -> crate::adapter::Manifest {
        let mut m = detect(uri)
            .map(|d| manifest(d, self.config()))
            .unwrap_or_else(|| crate::adapter::manifest("web"));
        if super::cursor::canonical_root(uri).is_some_and(|root| root != uri) {
            m.expected_fields = vec!["aliases".into()];
        } else if m.mode == Mode::Cursor && super::cursor::canonical_root(uri).is_none() {
            m.mode = Mode::Resource;
            m.expected_fields = vec!["name".into(), "start".into()];
        }
        m
    }
    fn manifests(&self) -> Vec<crate::adapter::Manifest> {
        super::manifests(self.config())
    }
    fn canary_allowed_for(&self, m: &crate::adapter::Manifest) -> bool {
        m.enabled
            && m.canary
                .as_ref()
                .is_some_and(|u| self.web.egress.allows(u.as_str()))
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
        tokio::time::timeout(
            crate::queue::RESOLVE_TIMEOUT - Duration::from_secs(1),
            self.inner(uri, false, None),
        )
        .await
        .map_err(|_| crate::resolver::RetryLater {
            class: "timeout",
            seconds: 60,
        })?
    }
    /// A platform whose canary is a listing (`Definition::canary_link`) reads the listing and takes
    /// its first link of that form; any other reads its sample.
    async fn canary_target(&self, m: &crate::adapter::Manifest) -> Result<String> {
        let listing = m.canary.clone().context("no canary")?;
        let Some(pattern) = super::definition(&m.source).and_then(|d| d.canary_link.clone()) else {
            return Ok(listing.to_string());
        };
        let fetched = self
            .web
            .egress
            .fetch_with(listing.as_str(), &FetchOptions::default())
            .await?;
        upstream_status(&fetched)?;
        if fetched.status != 200 {
            return Err(anyhow::Error::new(crate::resolver::ErrorClass("http_4xx"))
                .context(crate::resolver::Upstream(fetched.status)));
        }
        let base = fetched.url.clone();
        crate::resolver::html::blocking(move || {
            canary_link(&base, &String::from_utf8_lossy(&fetched.body), &pattern)
        })
        .await?
        .ok_or_else(|| parse_error("the canary listing links no event of its form"))
    }
    async fn probe_manifest(
        &self,
        uri: &str,
        m: &crate::adapter::Manifest,
    ) -> Result<ResourceMeta> {
        self.inner(uri, true, super::definition(&m.source)).await
    }
    async fn probe(&self, uri: &str) -> Result<ResourceMeta> {
        self.inner(uri, true, None).await
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        Duration::from_secs(doc.ttl_secs)
    }
}
pub fn calendar_document(
    uri: &str,
    d: &Definition,
    f: &Fetched,
    cfg: &EventConfig,
) -> Result<Option<ResourceMeta>> {
    if !String::from_utf8_lossy(&f.body[..f.body.len().min(256)])
        .trim_start_matches(['\u{feff}', ' ', '\n', '\r'])
        .starts_with("BEGIN:VCALENDAR")
    {
        return Ok(None);
    }
    if !cfg.calendars {
        return Ok(None);
    }
    let mut local = cfg.clone();
    let single = d.mode == Mode::Resource
        && (d.parser == "ics_link"
            || d.id == "partiful"
            || d.id == "meetup"
                && uri.contains("/events/")
                && !uri.trim_end_matches('/').ends_with("/ical"));
    if single {
        local.months_back = 120;
    }
    let mut doc = calendar::parse(uri, &f.body, &local, chrono::Utc::now())?;
    if single {
        if let Some(c) = doc.container.take() {
            let selected = c
                .occurrences
                .iter()
                .find(|o| o.doc.start.is_some_and(|at| at >= chrono::Utc::now()))
                .or_else(|| c.occurrences.last());
            if let Some(o) = selected {
                doc = o.doc.clone();
                doc.url = uri.into();
                // A file of one event answered from the page's own URL origin (scheme, host and
                // port, after redirects) names the page's date: its UID joins the page to that
                // date in its organiser calendar (`groups`).
                if c.occurrences.len() == 1
                    && crate::group_keys::same_url_origin(uri, f.url.as_str())
                {
                    crate::group_keys::add(&mut doc, "ics_uid", &o.uid);
                }
            }
        }
    }
    Ok(Some(doc))
}
fn last(u: &Url) -> &str {
    u.path_segments()
        .and_then(|mut x| x.rfind(|s| !s.is_empty()))
        .unwrap_or("")
}
fn query(u: &str, key: &str, val: &str) -> Result<String> {
    let mut u = Url::parse(u)?;
    u.query_pairs_mut().append_pair(key, val);
    Ok(u.into())
}
/// Returns only known endpoints or links extracted from the current page. All URLs go through Egress.
/// `hub_body_bytes` is the body cap of a Wikidata item's answer (`limits.hub_body_bytes`).
pub fn endpoint(
    d: &Definition,
    uri: &str,
    page: Option<&Fetched>,
    cfg: &EventConfig,
    hub_body_bytes: usize,
) -> Result<Option<(String, FetchOptions)>> {
    let u = Url::parse(uri)?;
    let id = last(&u);
    let opts = FetchOptions::default();
    let key = || {
        options(cfg, &d.id)
            .and_then(|o| o.key.as_deref())
            .context("missing API key")
    };
    // The link's path, without its trailing slashes, query and fragment, with `suffix` after it.
    // A site's root names no event: it gets no file suffix.
    let suffixed = |suffix: &str| -> Option<String> {
        let path = u.path().trim_end_matches('/');
        if path.is_empty() && !suffix.starts_with('/') {
            return None;
        }
        let mut file = u.clone();
        file.set_query(None);
        file.set_fragment(None);
        file.set_path(&format!("{path}{suffix}"));
        Some(file.into())
    };
    let result = match d.parser.as_str() {
        // A share link names its calendar right after `/p/` or `/embed/` (a view and a date
        // may follow: `/p/{token}/dayGridMonth/now`), below wherever the install sits.
        "calendar" if d.id == "nextcloud" && !u.path().contains("/remote.php/dav/") => {
            let path = u.path();
            let share = ["/apps/calendar/p/", "/apps/calendar/embed/"]
                .iter()
                .find_map(|form| path.split_once(form));
            match share {
                Some((install, rest)) => {
                    let token = rest.split('/').next().unwrap_or_default();
                    let install = install.trim_end_matches("/index.php");
                    let mut export = u.clone();
                    export.set_fragment(None);
                    export.set_path(&format!("{install}/remote.php/dav/public-calendars/{token}"));
                    export.set_query(Some("export"));
                    (!token.is_empty()).then(|| (export.into(), opts))
                }
                None => Some((
                    u.join(&format!("/remote.php/dav/public-calendars/{id}?export"))?
                        .into(),
                    opts,
                )),
            }
        }
        "airmeet" if !u.path().ends_with("/info") => Some((
            format!("https://api.airmeet.com/api/v1/airmeet/{id}/info"),
            opts,
        )),
        "gancio" if !u.path().starts_with("/api/") => {
            Some((u.join(&format!("/api/event/detail/{id}"))?.into(), opts))
        }
        "luma" if page.is_some() => Some((
            if id.starts_with("evt-") {
                query("https://api.lu.ma/event/get", "event_api_id", id)?
            } else {
                query("https://api.lu.ma/url", "url", u.path().trim_matches('/'))?
            },
            opts,
        )),
        "mobilizon" if page.is_some() => Some((
            u.join("/api")?.into(),
            FetchOptions {
                json_body: Some(
                    json!({"query":"query($uuid: UUID!) { event(uuid: $uuid) { uuid title description beginsOn endsOn status options { timezone } onlineAddress physicalAddress { description street locality postalCode country geom } } }","variables":{"uuid":id}}),
                ),
                ..opts
            },
        )),
        // The key is the wire's alone (`query_secret`): the address this returns, and every
        // one derived from it, names none.
        "ticketmaster" => Some((
            format!("https://app.ticketmaster.com/discovery/v2/events/{}.json", id.trim_end_matches(".json")),
            FetchOptions {
                sensitive: true,
                query_secret: Some(("apikey", key()?.to_owned())),
                ..opts
            },
        )),
        "resident_advisor" => Some((
            "https://ra.co/graphql".into(),
            FetchOptions {
                json_body: Some(
                    json!({"query":"query($id: ID!) { event(id: $id) { id title content startTime endTime venue { name address location { latitude longitude } area { ianaTimeZone } } } }","variables":{"id":id}}),
                ),
                ..opts
            },
        )),
        "wikidata"
            if u.host_str().is_some_and(|h| h.ends_with("wikidata.org"))
                && !u.path().contains("Special:EntityData") =>
        {
            Some((
                format!("https://www.wikidata.org/wiki/Special:EntityData/{id}.json"),
                FetchOptions {
                    // A much-edited item carries megabytes of claims.
                    body_cap: Some(hub_body_bytes),
                    ..opts
                },
            ))
        }
        "ics_link" if !crate::event::service::is_feed(uri) => {
            let link = page.and_then(|f| ics_link(&f.url, &String::from_utf8_lossy(&f.body)));
            let fallback = match d.id.as_str() {
                "tito" | "petzi" => suffixed(".ics"),
                "pretix" | "eventyay" => suffixed("/ical/"),
                "gathio" => Some(u.join(&format!("/export/event/{id}"))?.into()),
                "allevents" if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) => Some(query(
                    "https://allevents.in/api/addtocal.php?type=ical", "event_id", id)?),
                "weezevent" if id.starts_with('E') && id[1..].chars().all(|c| c.is_ascii_digit()) => Some(format!("https://api.weezevent.com/sites/view/live/{id}.ics")),
                "eventfrog" => id
                    .trim_end_matches(".html")
                    .rsplit('-')
                    .next()
                    .filter(|v| v.chars().all(|c| c.is_ascii_digit()))
                    .map(|id| format!("https://eventfrog.ch/stream/de/event/{id}.ics")),
                _ => None,
            };
            // Prefer initial HTML for providers whose calendar ID differs from the public slug.
            if page.is_some()
                || matches!(
                    d.id.as_str(),
                    "tito" | "petzi" | "pretix" | "eventyay" | "gathio" | "eventfrog"
                ) && !(d.id == "eventyay" && (u.path().starts_with("/ev/") || u.path().starts_with("/e/")))
            {
                link.or(fallback).map(|u| (u, opts))
            } else {
                None
            }
        }
        "partiful" if page.is_some() => Some((format!("https://storage.googleapis.com/getpartiful.appspot.com/public/events/{id}/event.ics"), opts)),
        // A date's page by its number or its token (`/events/frxnbvyjcqbmb/`): both files carry
        // the date's current UID.
        "meetup" if page.is_some() && super::container_feed(d, uri).is_some() && !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()) => {
            let mut file = u.clone();
            file.set_query(None);
            file.set_fragment(None);
            file.set_path(&format!("{}/ical/{id}.ics", u.path().trim_end_matches('/')));
            Some((file.into(), opts))
        }
        _ => None,
    };
    Ok(result)
}
/// The first link of a canary listing (`base`, its HTML `text`) on the listing's host whose path
/// has the form `pattern` (`canary_link_matches`).
pub fn canary_link(base: &Url, text: &str, pattern: &str) -> Option<String> {
    let html = crate::resolver::html::document(text).ok()?;
    html.select(&scraper::Selector::parse("a[href]").unwrap())
        .filter_map(|e| e.value().attr("href"))
        .filter_map(|s| base.join(s).ok())
        .find(|u| {
            u.host_str() == base.host_str()
                && matches!(u.scheme(), "http" | "https")
                && super::canary_link_matches(pattern, u.path())
        })
        .map(|mut u| {
            u.set_fragment(None);
            u.to_string()
        })
}
pub fn ics_link(base: &Url, text: &str) -> Option<String> {
    let html = crate::resolver::html::document(text).ok()?;
    html.select(&scraper::Selector::parse("a[href],link[href]").unwrap())
        .filter_map(|e| e.value().attr("href"))
        .filter(|s| {
            s.contains(".ics")
                || s.contains("/ical/")
                || s.contains("/iCalendar/")
                || s.contains("/export/event/")
        })
        .filter_map(|s| base.join(s).ok())
        .find(|u| crate::event::service::own_calendar_link(base, u.as_str()))
        .map(|u| u.to_string())
}

/// A Wikidata item's card ([`wiki_value`]) and its image file ([`image_file`]), not yet checked
/// on Commons ([`credit_image`]).
struct Item {
    doc: ResourceMeta,
    /// The item's ID and its `P18` file name.
    image: Option<(String, String)>,
}
/// The card of the Wikidata item `f` is (an entity's JSON) or names (a Wikipedia page).
async fn wiki(uri: &str, f: &Fetched, egress: &Egress) -> Result<Item> {
    let f = f.clone();
    let (mut value, qid) = crate::resolver::html::blocking(move || {
        let value: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
        let text = String::from_utf8_lossy(&f.body);
        let qid = wikipedia_item(&text);
        (value, qid)
    })
    .await?;
    if value.get("entities").is_none() {
        let qid = qid.ok_or_else(|| parse_error("Wikipedia page has no Wikidata entity"))?;
        let f = egress
            .discover(
                &format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json"),
                &FetchOptions {
                    body_cap: Some(crate::hubs::body_cap(egress)),
                    ..Default::default()
                },
                86400,
            )
            .await?;
        enrichment_status(&f)?;
        if matches!(f.status, 404 | 410) {
            let mut doc = ResourceMeta::new(uri, "wikidata", 3600);
            doc.status = crate::model::Status::Missing;
            return Ok(Item { doc, image: None });
        }
        if f.status != 200 {
            return Err(crate::resolver::RetryLater {
                class: "wikidata_unavailable",
                seconds: 3600,
            }
            .into());
        }
        value = crate::resolver::html::blocking(move || serde_json::from_slice(&f.body))
            .await?
            .map_err(parse_error)?;
    }
    let uri = uri.to_owned();
    crate::resolver::html::blocking(move || {
        wiki_value(&uri, &value).map(|doc| Item {
            doc,
            image: image_file(&value),
        })
    })
    .await?
    .map_err(parse_error)
}
/// The longest a card waits for Commons' answer about its image, as for its other enrichments
/// (a venue, an oEmbed answer), within its own deadline.
const COMMONS_WAIT: Duration = Duration::from_secs(2);
/// What a card keeps of its deadline for the steps after its image: with less left, Commons is
/// not asked and the image is unread.
const COMMONS_RESERVE: Duration = Duration::from_secs(1);
/// A Wikidata item's image on its card, only when Commons says the file is free to show, with
/// its credit (`wikimedia::credited_image`): a thumbnail `limits.music_image_width` pixels
/// wide. Commons is asked once, within [`COMMONS_WAIT`] and [`COMMONS_RESERVE`] before
/// `deadline`. A transient
/// failure (or no answer in time) leaves the image unread: the card keeps the credited image it
/// has stored (`ResourceMeta::keep_enriched`) and is read again early
/// (`limits.music_enrichment_retry_secs`), as an enriched MusicBrainz card is
/// (`music_enrichment::Enrichment::apply`). Never fails the card.
async fn credit_image(
    doc: &mut ResourceMeta,
    image: Option<(String, String)>,
    web: &WebResolver,
    deadline: tokio::time::Instant,
) {
    use crate::wikimedia::Found;
    let Some((qid, name)) = image else {
        return;
    };
    let now = tokio::time::Instant::now();
    let latest = deadline.checked_sub(COMMONS_RESERVE).unwrap_or(now);
    let found = if latest <= now {
        tracing::info!(subject = %qid, "no time left to ask Commons");
        Found::Unread
    } else {
        match tokio::time::timeout_at(
            latest.min(now + COMMONS_WAIT),
            crate::wikimedia::credited_image(
                &web.egress,
                &name,
                web.config.music_image_width,
                &qid,
            ),
        )
        .await
        {
            Ok(found) => found,
            Err(_) => {
                tracing::info!(subject = %qid, "Commons timed out");
                Found::Unread
            }
        }
    };
    crate::music_enrichment::Enrichment {
        image: found,
        description: Found::Missing,
        cover_if_none: None,
    }
    .apply(doc, web.config.music_enrichment_retry_secs);
}
pub fn wikipedia_item(text: &str) -> Option<String> {
    let valid = |s: &str| {
        let s = s.trim_start_matches([' ', ':', '\"', '\'']);
        let id = s.split(|c: char| !c.is_ascii_alphanumeric()).next()?;
        (id.starts_with('Q') && id.len() > 1 && id[1..].bytes().all(|b| b.is_ascii_digit()))
            .then(|| id.to_owned())
    };
    text.split_once("wgWikibaseItemId")
        .and_then(|(_, s)| valid(s))
        .or_else(|| {
            text.split_once("wikidata.org/wiki/Special:EntityPage/")
                .and_then(|(_, s)| valid(s))
        })
        .or_else(|| {
            let html = crate::resolver::html::document(text).ok()?;
            html.select(&scraper::Selector::parse("script[type='application/ld+json']").unwrap())
                .find_map(|node| {
                    let v: Value = serde_json::from_str(&node.text().collect::<String>()).ok()?;
                    let main = v.get("mainEntity")?;
                    let id = main.as_str().or_else(|| main["@id"].as_str())?;
                    let url = Url::parse(id).ok()?;
                    (url.host_str() == Some("www.wikidata.org")
                        || url.host_str() == Some("wikidata.org"))
                    .then(|| valid(url.path().rsplit('/').next()?))
                    .flatten()
                })
        })
}
fn wikipedia_candidate(f: &Fetched) -> bool {
    let text = String::from_utf8_lossy(&f.body);
    let Ok(html) = crate::resolver::html::document(&text) else {
        return false;
    };
    let selector=scraper::Selector::parse(".infobox.vevent, .infobox.geography, .infobox .geo, meta[property='place:location:latitude']").unwrap();
    if html.select(&selector).next().is_some() {
        return true;
    }
    let Ok(card) = crate::resolver::web::extract(f.url.as_str(), f, 86400).map(|e| e.doc) else {
        return false;
    };
    card.type_path
        .iter()
        .any(|t| matches!(t.as_str(), "Event" | "Place"))
}
/// The card of a Wikipedia page about an event or a place, from the page's Wikidata item
/// ([`wiki`]), with the item's image when Commons says it is free to show ([`credit_image`],
/// before `deadline`).
pub async fn wikipedia(
    uri: &str,
    f: &Fetched,
    web: &WebResolver,
    deadline: tokio::time::Instant,
) -> Result<Option<ResourceMeta>> {
    let Some(host) = Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    else {
        return Ok(None);
    };
    if host != "wikipedia.org" && !host.ends_with(".wikipedia.org") {
        return Ok(None);
    }
    let input = f.clone();
    if !crate::resolver::html::blocking(move || wikipedia_candidate(&input)).await? {
        return Ok(None);
    }
    let item = match wiki(uri, f, &web.egress).await {
        Ok(item) => item,
        Err(e) if e.is::<ParseFailure>() => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut doc = item.doc;
    if doc.status == crate::model::Status::Missing
        || !doc.type_path.iter().any(|t| t == "Event" || t == "Place")
    {
        return Ok(None);
    }
    credit_image(&mut doc, item.image, web, deadline).await;
    Ok(Some(doc))
}
pub fn wiki_value(uri: &str, v: &Value) -> Result<ResourceMeta> {
    let entity = v["entities"]
        .as_object()
        .and_then(|m| m.values().next())
        .context("missing Wikidata entity")?;
    let mut doc = ResourceMeta::new(uri, "wikidata", 86400);
    // English, else the item's language-independent (`mul`) value, which Wikidata holds in
    // place of an English one for many names (The Matrix, Q83495), else any language.
    let label = |key: &str| {
        entity[key]["en"]["value"]
            .as_str()
            .or_else(|| entity[key]["mul"]["value"].as_str())
            .or_else(|| {
                entity[key]
                    .as_object()?
                    .values()
                    .find_map(|v| v["value"].as_str())
            })
            .map(str::to_owned)
    };
    doc.name = label("labels");
    doc.description = label("descriptions");
    // A creative work (a series, a film, a book, an album, a game, a podcast: by what the item
    // is an instance of, P31, else by what only a work carries: a publication date, an author,
    // a director, a composer, a publisher, an episode count; not a broadcaster, which a
    // broadcast event has too) is never an event, whatever dates it carries: a series' run
    // (P580, P582) is not an occurrence.
    let instances: Vec<&str> = crate::hubs::wikidata::best(&entity["claims"]["P31"])
        .into_iter()
        .filter_map(|s| s.pointer("/datavalue/value/id").and_then(Value::as_str))
        .collect();
    let work = instances.iter().find_map(|q| work_class(q)).or_else(|| {
        ["P577", "P50", "P57", "P86", "P123", "P1113"]
            .iter()
            .any(|p| !crate::hubs::wikidata::best(&entity["claims"][p]).is_empty())
            .then_some("CreativeWork")
    });
    if let Some(kind) = work {
        doc.set_type(kind);
        doc.event.not_event = Some("creative_work".into());
    }
    for (p, end) in [("P580", false), ("P585", false), ("P582", true)] {
        if work.is_some() || (!end && doc.start.is_some()) {
            continue;
        }
        // The first date that reads, of the best-rank ones (a tie keeps its first).
        let date = shown(entity, p, |v| {
            let s = v["time"].as_str()?.trim_start_matches('+');
            let fixed = s.replace("-00-00T", "-01-01T").replace("-00T", "-01T");
            Some((
                crate::event::time::parse(&fixed, None)?,
                v["precision"].as_i64().unwrap_or(0),
            ))
        });
        if let Some((time, precision)) = date {
            if end {
                doc.end = Some(time);
            } else {
                doc.start = Some(time);
            }
            doc.event.all_day = Some(true);
            doc.event
                .warnings
                .push(format!("wikidata_date_precision_{precision}"));
        }
    }
    if doc.start.is_some() {
        doc.set_type("Event");
    } else if instances.contains(&"Q5") {
        doc.set_type("Person");
    }
    if let Some(p) = shown(entity, "P625", |v| {
        v["globe"]
            .as_str()
            .is_none_or(|s| s.ends_with("/Q2"))
            .then(|| page::point(v))
            .flatten()
    }) {
        doc.location = Some(p);
        doc.location_source = Some(crate::model::LocationSource::Page);
        if doc.start.is_none() && work.is_none() && !doc.type_path.contains(&"Person".into()) {
            doc.set_type("Place");
        }
    }
    if let Some(qid) = entity["id"].as_str() {
        doc.aliases
            .push(format!("https://www.wikidata.org/entity/{qid}"));
    }
    if let Some(site) = claim(entity, "P856").and_then(Value::as_str) {
        doc.aliases.push(site.into());
    }
    // The image (`P18`) is shown only once Commons says the file is free ([`credit_image`]).
    Ok(doc)
}
/// The value of `entity`'s claim for property `p` that names something (the official website,
/// an alias of the card): its one best-rank value (`hubs::wikidata::single`); none when it has
/// several, which would be a guess.
fn claim<'a>(entity: &'a Value, p: &str) -> Option<&'a Value> {
    crate::hubs::wikidata::single(
        crate::hubs::wikidata::best(&entity["claims"][p])
            .into_iter()
            .filter_map(|s| s.pointer("/datavalue/value")),
        |_| true,
    )
}
/// What a claim the card shows or is typed by (an image, a place, a date) reads as: the first of
/// its best-rank values that `read` reads (`hubs::wikidata::first`), in statement order; a tie
/// keeps its first value rather than dropping them all.
fn shown<'a, T>(entity: &'a Value, p: &str, read: impl FnMut(&'a Value) -> Option<T>) -> Option<T> {
    crate::hubs::wikidata::first(
        crate::hubs::wikidata::best(&entity["claims"][p])
            .into_iter()
            .filter_map(|s| s.pointer("/datavalue/value")),
        read,
    )
}
/// The ID and image file (`P18`, the claim [`shown`] picks, in normal form,
/// `wikimedia::file_name`) of the Wikidata item an entity's JSON holds.
fn image_file(v: &Value) -> Option<(String, String)> {
    let entity = v["entities"].as_object()?.values().next()?;
    let qid = entity["id"].as_str()?;
    let file = shown(entity, "P18", |v| crate::wikimedia::file_name(v.as_str()?))?;
    Some((qid.to_owned(), file))
}

/// The schema.org type of a Wikidata class a creative work is an instance of (P31).
fn work_class(q: &str) -> Option<&'static str> {
    Some(match q {
        // television series, television program, animated series, miniseries
        "Q5398426" | "Q15416" | "Q581714" | "Q1259759" => "TVSeries",
        "Q3464665" => "TVSeason",
        "Q21191270" => "TVEpisode",
        // film, short film, animated film
        "Q11424" | "Q24862" | "Q202866" => "Movie",
        // literary work, book, written work, novel, short story
        "Q7725634" | "Q571" | "Q47461344" | "Q8261" | "Q49084" => "Book",
        // album, extended play, live album, single
        "Q482994" | "Q169930" | "Q209939" | "Q134556" => "MusicAlbum",
        // song, musical work/composition
        "Q7366" | "Q2188189" => "MusicComposition",
        "Q7889" => "VideoGame",
        "Q24634210" => "PodcastSeries",
        // periodical, newspaper
        "Q1002697" | "Q11032" => "Periodical",
        _ => return None,
    })
}

fn feed_path(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|u| {
        crate::event::service::is_feed(uri)
            || u.path().contains("/published/")
            || u.path().contains("/remote.php/dav/public-calendars/")
    })
}

#[cfg(test)]
mod tests {
    /// Meetup's Markdown reads as plain text on its cards and dates; another site's asterisks stay.
    #[test]
    fn meetup_descriptions_are_plain_text() {
        let md = "*This event is local*\n\n**Our casual meetup.** 5 * 3";
        let mut doc = ResourceMeta::new(
            "https://www.meetup.com/bitcoin-meetup-switzerland/events/316908442/",
            "meetup",
            3600,
        );
        doc.description = Some(md.into());
        let mut date = doc.clone();
        date.description = Some(format!("Bitcoin Switzerland\n{md}"));
        doc.container = Some(Box::new(crate::event::Container {
            occurrences: vec![crate::event::Occurrence {
                uid: "event_1@meetup.com".into(),
                recurrence_id: String::new(),
                doc: date,
                sequence: 0,
                modified: None,
                content_hash: String::new(),
            }],
            ..Default::default()
        }));
        let mut other = doc.clone();
        super::meetup_plain(&doc.url.clone(), &mut doc);
        assert_eq!(
            doc.description.as_deref(),
            Some("This event is local\n\nOur casual meetup. 5 * 3")
        );
        assert_eq!(
            doc.container.unwrap().occurrences[0]
                .doc
                .description
                .as_deref(),
            Some("Bitcoin Switzerland\nThis event is local\n\nOur casual meetup. 5 * 3")
        );
        super::meetup_plain("https://example.org/events/1/", &mut other);
        assert_eq!(other.description.as_deref(), Some(md));
        let mut empty = ResourceMeta::new("https://meetup.com/g/events/1/", "meetup", 3600);
        empty.description = Some("**".into());
        super::meetup_plain("https://meetup.com/g/events/1/", &mut empty);
        assert_eq!(empty.description, None);
    }

    use super::*;
    fn item(qid: &str, label: &str, claims: Value) -> Value {
        json!({"entities": {qid: {"id": qid, "labels": {"en": {"value": label}}, "claims": claims}}})
    }
    fn time(t: &str) -> Value {
        json!([{"rank":"normal","mainsnak":{"datavalue":{"value":{"time":t,"precision":11}}}}])
    }
    fn id(q: &str) -> Value {
        json!([{"rank":"normal","mainsnak":{"datavalue":{"value":{"id":q}}}}])
    }
    #[test]
    fn a_name_without_english_takes_the_mul_label_before_other_languages() {
        // The Matrix (Q83495): no `en` label, `mul` "The Matrix", and 76 others before it.
        let matrix = json!({"entities": {"Q83495": {"id": "Q83495",
            "labels": {"am": {"language": "am", "value": "ማትሪክስ"}, "zh": {"language": "zh", "value": "黑客帝国"}, "mul": {"language": "mul", "value": "The Matrix"}},
            "descriptions": {"de": {"language": "de", "value": "Film"}, "mul": {"language": "mul", "value": "1999 film"}},
            "claims": {"P31": id("Q11424")}}}});
        let doc = wiki_value("https://www.wikidata.org/wiki/Q83495", &matrix).unwrap();
        assert_eq!(doc.name.as_deref(), Some("The Matrix"));
        assert_eq!(doc.description.as_deref(), Some("1999 film"));
        // English still comes first, and any language is the last resort.
        let mut both = matrix.clone();
        both["entities"]["Q83495"]["labels"]["en"] =
            json!({"language": "en", "value": "Matrix (film)"});
        let doc = wiki_value("https://www.wikidata.org/wiki/Q83495", &both).unwrap();
        assert_eq!(doc.name.as_deref(), Some("Matrix (film)"));
        let mut neither = matrix;
        neither["entities"]["Q83495"]["labels"] =
            json!({"zh": {"language": "zh", "value": "黑客帝国"}});
        let doc = wiki_value("https://www.wikidata.org/wiki/Q83495", &neither).unwrap();
        assert_eq!(doc.name.as_deref(), Some("黑客帝国"));
    }
    #[test]
    fn a_creative_work_with_a_run_is_no_event() {
        // Breaking Bad: a television series (Q5398426) first and last aired 2008 and 2013.
        let series = item(
            "Q1079",
            "Breaking Bad",
            json!({"P31": id("Q5398426"), "P580": time("+2008-01-20T00:00:00Z"), "P582": time("+2013-09-29T00:00:00Z"), "P449": id("Q294660")}),
        );
        let doc = wiki_value("https://www.wikidata.org/wiki/Q1079", &series).unwrap();
        assert_eq!(doc.type_name, "TVSeries");
        assert!(doc.type_path.iter().any(|t| t == "CreativeWork"));
        assert!(!doc.type_path.iter().any(|t| t == "Event"));
        assert!(doc.start.is_none() && doc.end.is_none());
        assert_eq!(doc.event.all_day, None);
        assert_eq!(doc.name.as_deref(), Some("Breaking Bad"));
        // A work of a class not known here, by what only a work carries (an episode count): a
        // creative work.
        let anime = item(
            "Q1",
            "Some Show",
            json!({"P31": id("Q63952888"), "P580": time("+2016-07-15T00:00:00Z"), "P449": id("Q907311"), "P1113": [{"rank":"normal","mainsnak":{"datavalue":{"value":{"amount":"+24"}}}}]}),
        );
        let doc = wiki_value("https://www.wikidata.org/wiki/Q1", &anime).unwrap();
        assert_eq!(doc.type_name, "CreativeWork");
        assert!(doc.start.is_none());
        // A work with coordinates is no place.
        let book = item(
            "Q2",
            "A Book",
            json!({"P31": id("Q7725634"), "P625": [{"rank":"normal","mainsnak":{"datavalue":{"value":{"latitude":47.37,"longitude":8.54}}}}]}),
        );
        let doc = wiki_value("https://www.wikidata.org/wiki/Q2", &book).unwrap();
        assert_eq!(doc.type_name, "Book");
        // An occurrence with a start is an event, as before, a broadcaster or not (a song
        // contest's edition is broadcast); a human a person.
        let festival = item(
            "Q3",
            "A Festival",
            json!({"P31": id("Q868557"), "P580": time("+2027-07-01T00:00:00Z"), "P582": time("+2027-07-03T00:00:00Z"), "P449": id("Q262386")}),
        );
        let doc = wiki_value("https://www.wikidata.org/wiki/Q3", &festival).unwrap();
        assert_eq!(doc.type_name, "Event");
        assert!(doc.start.is_some() && doc.end.is_some());
        let human = item("Q4", "Someone", json!({"P31": id("Q5")}));
        let doc = wiki_value("https://www.wikidata.org/wiki/Q4", &human).unwrap();
        assert_eq!(doc.type_name, "Person");
    }
    /// An item's claims are read by the one best-rank rule (`hubs::wikidata`): a class among
    /// several of P31 types it (Q5, a person, whichever comes first); a claim that links with two
    /// values gives none of them (two official websites: no alias), where a preferred one gives
    /// that one; a claim shown or typed by keeps the first of a tie (two images, two places, two
    /// dates).
    #[test]
    fn claims_follow_the_one_best_rank_rule() {
        let two = |p: &str, a: Value, b: Value| {
            json!({p: [
                {"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": a}}},
                {"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": b}}}
            ]})
        };
        let person = item(
            "Q8",
            "Someone",
            two("P31", json!({"id": "Q5"}), json!({"id": "Q15632617"})),
        );
        let doc = wiki_value("https://www.wikidata.org/wiki/Q8", &person).unwrap();
        assert_eq!(doc.type_name, "Person");
        let sites = item(
            "Q9",
            "Org",
            two(
                "P856",
                json!("https://a.example"),
                json!("https://b.example"),
            ),
        );
        let doc = wiki_value("https://www.wikidata.org/wiki/Q9", &sites).unwrap();
        assert_eq!(doc.aliases, ["https://www.wikidata.org/entity/Q9"]);
        let mut preferred = sites.clone();
        preferred["entities"]["Q9"]["claims"]["P856"][1]["rank"] = json!("preferred");
        let doc = wiki_value("https://www.wikidata.org/wiki/Q9", &preferred).unwrap();
        assert_eq!(
            doc.aliases,
            ["https://www.wikidata.org/entity/Q9", "https://b.example"]
        );
        // Ties of what is shown keep their first value: the image, the place, the date.
        let images = item(
            "Q10",
            "Bridge",
            two("P18", json!("first view.jpg"), json!("second view.jpg")),
        );
        assert_eq!(
            image_file(&images),
            Some(("Q10".into(), "First_view.jpg".into()))
        );
        let mut places = two(
            "P625",
            json!({"latitude": 46.0, "longitude": 8.95, "globe": "http://www.wikidata.org/entity/Q2"}),
            json!({"latitude": 47.0, "longitude": 9.0, "globe": "http://www.wikidata.org/entity/Q2"}),
        );
        places["P585"] = two(
            "P585",
            json!({"time": "+2026-05-01T00:00:00Z", "precision": 11}),
            json!({"time": "+2027-05-01T00:00:00Z", "precision": 11}),
        )["P585"]
            .clone();
        let event = wiki_value(
            "https://www.wikidata.org/wiki/Q11",
            &item("Q11", "Fair", places),
        )
        .unwrap();
        assert_eq!(event.type_name, "Event");
        assert_eq!(
            event.start.map(|t| t.to_rfc3339()).as_deref(),
            Some("2026-05-01T00:00:00+00:00")
        );
        let point = event.location.unwrap();
        assert_eq!((point.lat, point.lon), (46.0, 8.95));
    }
    /// The image file is the P18 statement the card's other claims are picked by (its one
    /// best-rank value: a preferred one, never a deprecated one), in MediaWiki's normal form; the
    /// card itself carries no image until Commons verifies the file.
    #[test]
    fn the_image_file_is_the_picked_claim_in_normal_form() {
        let file =
            |v: &str, rank: &str| json!({"rank": rank, "mainsnak": {"datavalue": {"value": v}}});
        let place = item(
            "Q5",
            "A Place",
            json!({"P18": [file("old view.jpg", "deprecated"), file("street view.jpg", "normal"), file("main  view.jpg", "preferred")]}),
        );
        assert_eq!(
            image_file(&place),
            Some(("Q5".into(), "Main_view.jpg".into()))
        );
        assert_eq!(
            wiki_value("https://www.wikidata.org/wiki/Q5", &place)
                .unwrap()
                .image,
            None
        );
        let only_deprecated = item("Q6", "B", json!({"P18": [file("old.jpg", "deprecated")]}));
        assert_eq!(image_file(&only_deprecated), None);
        let bad_name = item("Q7", "C", json!({"P18": [file("a|b.jpg", "normal")]}));
        assert_eq!(image_file(&bad_name), None);
    }
}
