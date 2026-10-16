use super::{detect, manifest, options, profile, state, Definition};
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
        let cfg = self.config();
        let d = selected
            .or_else(|| detect(uri))
            .context("unknown event platform")?;
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
        if !probe {
            let held: Option<f64> = sqlx::query_scalar("SELECT extract(epoch FROM held_until-now())::float8 FROM source_state WHERE source=$1 AND held_until>now()")
                .bind(&m.source).fetch_optional(&self.web.events.as_ref().unwrap().pool).await?;
            if let Some(seconds) = held {
                return Err(crate::resolver::RetryLater {
                    class: "source_held",
                    seconds: seconds.ceil() as u64,
                }
                .into());
            }
        }
        let local = super::configured_event(cfg, d);
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
        if matches!(
            d.parser.as_str(),
            "portal" | "btcmap" | "google_calendar_api"
        ) {
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
        let target = endpoint(d, uri, None, cfg)?;
        let (target, mut opts) = target.unwrap_or((uri.to_owned(), FetchOptions::default()));
        opts.user_agent = Some(profile(&m.ua_profile).into());
        opts.feed_hint = crate::event::service::is_feed(&target)
            || d.mode == Mode::Container
            || (d.id == "starticket" && target.ends_with(".xml"));
        opts.feed_cap = opts.feed_hint.then_some(local.feed_body_bytes);
        let fetched = self
            .web
            .egress
            .fetch_with(&calendar::canonical_feed(&target), &opts)
            .await?;
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
        if fetched.status != 200 {
            if let Some(doc) = self
                .web
                .events
                .as_ref()
                .unwrap()
                .disappeared(uri, fetched.status)
                .await?
            {
                return Ok(doc);
            }
            return Ok(extract_blocking(uri, &fetched, 3600).await?.doc);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        let mut doc = if d.parser == "wikidata" {
            wiki(uri, &fetched, &self.web.egress).await?
        } else if let Some(doc) = calendar_blocking(uri, d, &fetched, &local).await? {
            doc
        } else {
            if let Some(doc) = self
                .web
                .events
                .as_ref()
                .unwrap()
                .disappeared(uri, fetched.status)
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
            if !d.requires_key
                && (doc.start.is_none()
                    || doc.event.tz.is_none()
                    || doc.location.is_none()
                        && doc.event.attendance != Some(crate::event::Attendance::Online))
            {
                if let Some((target, mut opts)) = endpoint(d, uri, Some(&fetched), cfg)? {
                    if target != fetched.url.as_str() {
                        opts.feed_hint = crate::event::service::is_feed(&target)
                            || d.mode == Mode::Container
                            || (d.id == "starticket" && target.ends_with(".xml"));
                        opts.feed_cap = opts.feed_hint.then_some(local.feed_body_bytes);
                        opts.user_agent = Some(profile(&m.ua_profile).into());
                        if let Ok(f) = self
                            .web
                            .egress
                            .discover(&target, &opts, m.cadence_floor)
                            .await
                        {
                            if f.status == 200 {
                                if let Some(extra) = enrichment_blocking(uri, d, &f, &local).await?
                                {
                                    merge(&mut doc, &extra)?;
                                }
                            }
                        }
                    }
                }
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
            page::finalize(&mut doc, local.coordinate_timezone);
            if !probe {
                let events = self.web.events.as_ref().unwrap();
                let _ = tokio::time::timeout(Duration::from_secs(2), events.venue(&mut doc)).await;
            }
            page::finalize(&mut doc, false);
        }
        doc.source = d.id.clone();
        doc.bounded(crate::resolver::web::ATTRIBUTES)
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
        let links: std::collections::BTreeSet<_> = c
            .occurrences
            .iter()
            .filter(|_| d.id == "btcmap")
            .map(|o| o.doc.url.as_str())
            .filter(|u| *u != doc.url)
            .filter_map(|u| Url::parse(u).ok())
            .filter(|u| {
                matches!(u.scheme(), "http" | "https")
                    && u.username().is_empty()
                    && u.password().is_none()
            })
            .map(|u| u.to_string())
            .collect();
        ensure!(
            links.len() <= cfg.adapters.max_linked_resources,
            "linked event resource cap"
        );
        for link in links {
            crate::store::ensure_resource(&events.pool, &link).await?;
            let host = Url::parse(&link)?
                .host_str()
                .context("event link host")?
                .to_owned();
            if !c.limits.link_hosts.contains(&host) {
                c.limits.link_hosts.push(host);
            }
        }
        let indices: std::collections::BTreeMap<_, _> = c
            .occurrences
            .iter()
            .enumerate()
            .map(|(i, o)| (o.uid.clone(), i))
            .collect();
        let mut saved: std::collections::BTreeMap<String, (String, String)> =
            std::collections::BTreeMap::new();
        for (uid, place) in std::mem::take(&mut c.osm_venues) {
            let key = crate::resolver::osm::parse_id(&place.url)
                .context("invalid explicit OSM identity")?;
            let (resource_id, version) = if let Some(saved) = saved.get(&key.key()) {
                saved.clone()
            } else {
                ensure!(
                    saved.len() < cfg.adapters.max_linked_resources,
                    "explicit OSM venue cap"
                );
                let version = events.osm.save(&key, &place, None).await?;
                let resource_id = crate::store::ensure_resource(&events.pool, &place.url).await?;
                saved.insert(key.key(), (resource_id.clone(), version.clone()));
                (resource_id, version)
            };
            if let Some(i) = indices.get(&uid) {
                let row = &mut c.occurrences[*i];
                row.doc.location = None;
                row.doc.location_source = None;
                let venue = row.doc.event.venue.get_or_insert_with(Default::default);
                venue.osm_id = Some(key.key());
                venue.osm_version = Some(version);
                venue.resource_id = Some(resource_id);
                venue.location = None;
                venue.location_source = Some(crate::model::LocationSource::Osm);
                row.content_hash =
                    blake3::hash(&serde_json::to_vec(&crate::changes::normalize(&row.doc))?)
                        .to_hex()
                        .to_string();
            }
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
        f.body.len() <= crate::resolver::egress::BODY_CAP,
        "non-feed page exceeds cap"
    );
    let (uri, f) = (uri.to_owned(), f.clone());
    Ok(tokio::task::spawn_blocking(move || crate::resolver::web::extract(&uri, &f, ttl)).await?)
}
async fn calendar_blocking(
    uri: &str,
    d: &Definition,
    f: &Fetched,
    cfg: &EventConfig,
) -> Result<Option<ResourceMeta>> {
    let (uri, id, f, cfg) = (uri.to_owned(), d.id.clone(), f.clone(), cfg.clone());
    tokio::task::spawn_blocking(move || {
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
        f.body.len() <= crate::resolver::egress::BODY_CAP,
        "non-feed enrichment exceeds cap"
    );
    let (uri, id, body) = (uri.to_owned(), d.id.clone(), f.body.clone());
    Ok(tokio::task::spawn_blocking(move || state::parse(&id, &uri, &body)).await?)
}

fn merge(doc: &mut ResourceMeta, extra: &ResourceMeta) -> Result<()> {
    if extra.type_path.contains(&"Event".into()) && !doc.type_path.contains(&"Event".into()) {
        doc.set_type("Event");
    }
    if doc.event.timezone_attribution.is_some() && extra.event.tz.is_some() {
        doc.event.tz = None;
        doc.event.timezone_attribution = None;
    }
    page::merge_missing(doc, extra)
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
        detect(uri)
            .map(|d| manifest(d, self.config()))
            .unwrap_or_else(|| crate::adapter::manifest("event_platform"))
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
        tokio::time::timeout(Duration::from_secs(14), self.inner(uri, false, None)).await?
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
pub fn endpoint(
    d: &Definition,
    uri: &str,
    page: Option<&Fetched>,
    cfg: &EventConfig,
) -> Result<Option<(String, FetchOptions)>> {
    let u = Url::parse(uri)?;
    let id = last(&u);
    let opts = FetchOptions::default();
    let key = || {
        options(cfg, &d.id)
            .and_then(|o| o.key.as_deref())
            .context("missing API key")
    };
    let result = match d.parser.as_str() {
        "calendar" if d.id == "nextcloud" && !u.path().contains("/remote.php/dav/") => Some((
            u.join(&format!("/remote.php/dav/public-calendars/{id}?export"))?.into(), opts)),
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
                    json!({"query":"query($uuid: UUID!) { event(uuid: $uuid) { uuid title description beginsOn endsOn status timezone onlineAddress physicalAddress { description street locality postalCode country geom } } }","variables":{"uuid":id}}),
                ),
                ..opts
            },
        )),
        "ticketmaster" => Some((
            query(
                &format!("https://app.ticketmaster.com/discovery/v2/events/{}.json", id.trim_end_matches(".json")),
                "apikey",
                key()?,
            )?,
            FetchOptions {
                sensitive: true,
                ..opts
            },
        )),
        "skiddle" => Some((
            query(
                &format!("https://www.skiddle.com/api/v1/events/{id}/"),
                "api_key",
                key()?,
            )?,
            FetchOptions {
                sensitive: true,
                ..opts
            },
        )),
        "billetto_api" => Some((
            uri.into(),
            FetchOptions {
                headers: vec![("Api-Keypair".into(), key()?.into())],
                sensitive: true,
                ..opts
            },
        )),
        "eventfrog_api" => Some((
            uri.into(),
            FetchOptions {
                headers: vec![("authorization".into(), format!("Bearer {}", key()?))],
                sensitive: true,
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
                opts,
            ))
        }
        "ics_link" if !crate::event::service::is_feed(uri) => {
            let link =
                page.and_then(|f| ics_link(&d.id, &f.url, &String::from_utf8_lossy(&f.body)));
            let fallback = match d.id.as_str() {
                "tito" => Some(format!("{}.ics", uri.trim_end_matches('/'))),
                "petzi" => Some(format!("{}.ics", uri.trim_end_matches('/'))),
                "pretix" | "eventyay" => Some(format!("{}/ical/", uri.trim_end_matches('/'))),
                "gathio" => Some(u.join(&format!("/export/event/{id}"))?.into()),
                "allevents" if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) => Some(query(
                    "https://allevents.in/api/addtocal.php?type=ical", "event_id", id)?),
                "guidle" => id.rsplit_once('_').map(|(_, id)| query(
                    "https://www.guidle.com/iCalendar/?lang=de", "id", id)).transpose()?,
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
        "meetup" if page.is_some() && id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() => Some((format!("{}/ical/{id}.ics", uri.trim_end_matches('/')), opts)),
        _ => None,
    };
    Ok(result)
}
pub fn ics_link(_id: &str, base: &Url, text: &str) -> Option<String> {
    let html = scraper::Html::parse_document(text);
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

async fn wiki(uri: &str, f: &Fetched, egress: &Egress) -> Result<ResourceMeta> {
    let f = f.clone();
    let (mut value, qid) = tokio::task::spawn_blocking(move || {
        let value: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
        let text = String::from_utf8_lossy(&f.body);
        let qid = ["wikidata.org/wiki/", "wikidata.org/entity/"]
            .iter()
            .find_map(|prefix| {
                text.split(prefix).skip(1).find_map(|s| {
                    let id = s.split(|c: char| !c.is_ascii_alphanumeric()).next()?;
                    (id.starts_with('Q')
                        && id.len() > 1
                        && id[1..].chars().all(|c| c.is_ascii_digit()))
                    .then_some(id.to_owned())
                })
            });
        (value, qid)
    })
    .await?;
    if value.get("entities").is_none() {
        let qid = qid.context("Wikipedia page has no Wikidata entity")?;
        let f = egress
            .discover(
                &format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json"),
                &Default::default(),
                86400,
            )
            .await?;
        ensure!(f.status == 200, "Wikidata unavailable");
        value = tokio::task::spawn_blocking(move || serde_json::from_slice(&f.body)).await??;
    }
    let uri = uri.to_owned();
    tokio::task::spawn_blocking(move || wiki_value(&uri, &value)).await?
}
pub fn wiki_value(uri: &str, v: &Value) -> Result<ResourceMeta> {
    let entity = v["entities"]
        .as_object()
        .and_then(|m| m.values().next())
        .context("missing Wikidata entity")?;
    let mut doc = ResourceMeta::new(uri, "wikidata", 86400);
    let label = |key: &str| {
        entity[key]["en"]["value"]
            .as_str()
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
    let claim = |p: &str| {
        entity["claims"][p]
            .as_array()
            .and_then(|a| {
                a.iter()
                    .filter(|v| v["rank"] != "deprecated")
                    .max_by_key(|v| v["rank"] == "preferred")
            })
            .and_then(|v| v.pointer("/mainsnak/datavalue/value"))
    };
    for (p, end) in [("P580", false), ("P585", false), ("P582", true)] {
        if !end && doc.start.is_some() {
            continue;
        }
        if let Some(v) = claim(p) {
            if let Some(s) = v["time"].as_str().map(|s| s.trim_start_matches('+')) {
                let fixed = s.replace("-00-00T", "-01-01T").replace("-00T", "-01T");
                if end {
                    doc.end = crate::event::time::parse(&fixed, None);
                } else {
                    doc.start = crate::event::time::parse(&fixed, None);
                }
                doc.event.all_day = Some(true);
                doc.event.warnings.push(format!(
                    "wikidata_date_precision_{}",
                    v["precision"].as_i64().unwrap_or(0)
                ));
            }
        }
    }
    if doc.start.is_some() {
        doc.set_type("Event");
    } else if claim("P31").is_some_and(|v| v["id"] == "Q5") {
        doc.set_type("Person");
    }
    if let Some(p) = claim("P625")
        .filter(|v| v["globe"].as_str().is_none_or(|s| s.ends_with("/Q2")))
        .and_then(page::point)
    {
        doc.location = Some(p);
        doc.location_source = Some(crate::model::LocationSource::Page);
        if doc.start.is_none() && !doc.type_path.contains(&"Person".into()) {
            doc.set_type("Place");
        }
    }
    if let Some(qid) = entity["id"].as_str() {
        doc.aliases
            .push(format!("https://www.wikidata.org/entity/{qid}"));
    }
    if let Some(site) = claim("P856").and_then(Value::as_str) {
        doc.aliases.push(site.into());
    }
    if let Some(file) = claim("P18").and_then(Value::as_str) {
        doc.image = Some(format!(
            "https://commons.wikimedia.org/wiki/Special:FilePath/{}",
            percent_encoding::utf8_percent_encode(file, percent_encoding::NON_ALPHANUMERIC)
        ));
    }
    Ok(doc)
}
