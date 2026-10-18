use crate::{
    config::EventConfig,
    event::{page, time, Attendance, Occurrence, Venue},
    model::{LocationSource, Point, ResourceMeta, Status},
    resolver::egress::Egress,
};
use anyhow::{bail, ensure, Context, Result};
use nostr::{
    event::Event,
    nips::{
        nip01::Coordinate,
        nip19::{FromBech32, Nip19, Nip19Coordinate, ToBech32},
    },
};
use serde_json::{json, Value};
use url::Url;

#[derive(Clone, Debug)]
pub struct Target {
    pub author: Option<String>,
    pub kind: Option<u16>,
    pub identifier: Option<String>,
    pub id: Option<String>,
    pub relays: Vec<String>,
}
impl Target {
    pub fn decode(uri: &str) -> Result<Self> {
        ensure!(uri.len() <= 8192, "Nostr URI exceeds cap");
        let encoded = if let Some(s) = uri.strip_prefix("nostr:") {
            s.trim_start_matches('/').to_owned()
        } else {
            let u = Url::parse(uri)?;
            u.path_segments()
                .and_then(|mut s| {
                    s.find(|s| {
                        s.starts_with("naddr1")
                            || s.starts_with("nevent1")
                            || s.starts_with("npub1")
                            || s.starts_with("nprofile1")
                            || s.starts_with("note1")
                    })
                })
                .context("no NIP-19 entity in client URL")?
                .into()
        };
        ensure!(encoded.len() <= 5000, "NIP-19 identifier exceeds cap");
        let mut t = Self {
            author: None,
            kind: None,
            identifier: None,
            id: None,
            relays: vec![],
        };
        match Nip19::from_bech32(&encoded)? {
            Nip19::Coordinate(c) => {
                t.author = Some(c.public_key.to_hex());
                t.kind = Some(c.kind.as_u16());
                t.identifier = Some(c.identifier.clone());
                t.relays = c.relays.iter().map(ToString::to_string).collect();
            }
            Nip19::Event(e) => {
                t.id = Some(e.event_id.to_hex());
                t.author = e.author.map(|p| p.to_hex());
                t.kind = e.kind.map(|k| k.as_u16());
                t.relays = e.relays.iter().map(ToString::to_string).collect();
            }
            Nip19::EventId(id) => t.id = Some(id.to_hex()),
            Nip19::Pubkey(p) => {
                t.author = Some(p.to_hex());
                t.kind = Some(0);
            }
            Nip19::Profile(p) => {
                t.author = Some(p.public_key.to_hex());
                t.kind = Some(0);
                t.relays = p.relays.iter().map(ToString::to_string).collect();
            }
            _ => bail!("secret keys are not resource identifiers"),
        }
        Ok(t)
    }
    pub fn coordinate(value: &str) -> Result<Self> {
        let c = Coordinate::parse(value)?;
        Ok(Self {
            author: Some(c.public_key.to_hex()),
            kind: Some(c.kind.as_u16()),
            identifier: Some(c.identifier),
            id: None,
            relays: vec![],
        })
    }
    pub fn uri(&self) -> Result<String> {
        if let Some(id) = &self.id {
            return Ok(format!(
                "nostr:{}",
                nostr::event::EventId::from_hex(id)?.to_bech32()?
            ));
        }
        let value = format!(
            "{}:{}:{}",
            self.kind.context("missing kind")?,
            self.author.as_deref().context("missing author")?,
            self.identifier.as_deref().unwrap_or("")
        );
        Ok(format!(
            "nostr:{}",
            Nip19Coordinate::new(
                Coordinate::parse(&value)?,
                self.relays.iter().filter_map(|r| r.parse().ok())
            )
            .to_bech32()?
        ))
    }
    pub fn filter(&self) -> Value {
        if let Some(id) = &self.id {
            return json!({"ids":[id],"limit":100});
        }
        let mut filter = json!({"authors":[self.author],"kinds":[self.kind],"limit":100});
        if let Some(d) = &self.identifier {
            filter["#d"] = json!([d]);
        }
        filter
    }
    pub fn matches(&self, event: &Event) -> bool {
        self.id.as_ref().is_none_or(|s| *s == event.id.to_hex())
            && self
                .author
                .as_ref()
                .is_none_or(|s| *s == event.pubkey.to_hex())
            && self.kind.is_none_or(|k| k == event.kind.as_u16())
            && self
                .identifier
                .as_ref()
                .is_none_or(|d| tag(event, "d") == Some(d.as_str()))
    }
}
fn tag<'a>(event: &'a Event, key: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|t| {
        let a = t.as_slice();
        (a.first().is_some_and(|s| s == key))
            .then(|| a.get(1).map(String::as_str))
            .flatten()
    })
}
pub fn newest(target: &Target, values: &[Value]) -> Option<Event> {
    values
        .iter()
        .take(256)
        .filter_map(|v| serde_json::from_value::<Event>(v.clone()).ok())
        .filter(|e| target.matches(e) && e.verify().is_ok())
        // NIP-01: newest created_at, then lexicographically smallest event ID on a tie.
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        })
}
fn rows(v: &Value) -> Vec<Value> {
    v.as_array()
        .cloned()
        .or_else(|| v["data"].as_array().cloned())
        .or_else(|| v["events"].as_array().cloned())
        .unwrap_or_else(|| {
            if v.get("id").is_some() {
                vec![v.clone()]
            } else {
                vec![]
            }
        })
}
pub async fn lookup(target: &Target, egress: &Egress, cfg: &EventConfig) -> Result<Option<Event>> {
    // Leave one second for the platform wrapper and one for completion bookkeeping.
    let budget = crate::queue::RESOLVE_TIMEOUT - std::time::Duration::from_secs(2);
    let relay = async {
        if cfg.adapters.websocket {
            if let Some(relay) = target.relays.first() {
                return egress
                    .nostr_request(relay, target.filter(), cfg.adapters.websocket_deadline_ms)
                    .await;
            }
        }
        Ok(Vec::new())
    };
    // Keep the paginated indexer/relay state machine off callers' debug stacks.
    tokio::time::timeout(budget, Box::pin(lookup_inner(target, egress, cfg, relay)))
        .await
        .map_err(|_| crate::resolver::RetryLater {
            class: "inconclusive",
            seconds: 300,
        })?
}
fn indexer_budget(cfg: &EventConfig) -> std::time::Duration {
    let relay = if cfg.adapters.websocket {
        cfg.adapters.websocket_deadline_ms
    } else {
        0
    };
    (crate::queue::RESOLVE_TIMEOUT - std::time::Duration::from_secs(2))
        .saturating_sub(std::time::Duration::from_millis(relay))
        .min(std::time::Duration::from_secs(10))
}
async fn lookup_inner(
    target: &Target,
    egress: &Egress,
    cfg: &EventConfig,
    relay: impl std::future::Future<Output = Result<Vec<Value>>>,
) -> Result<Option<Event>> {
    let mut wine = Url::parse("https://api.nostr.wine/search")?;
    {
        let mut q = wine.query_pairs_mut();
        q.append_pair("limit", "100")
            .append_pair("sort", "time")
            .append_pair("order", "descending");
        if let Some(k) = target.kind {
            q.append_pair("kind", &k.to_string());
        }
        if let Some(a) = &target.author {
            q.append_pair("pubkey", a);
        }
        if let Some(id) = &target.id {
            q.append_pair("query", id);
        }
    }
    let mut archive = Url::parse("https://api.nostrarchives.com/v1/events")?;
    if let Some(id) = &target.id {
        archive.set_path(&format!("/v1/events/{id}"));
    } else {
        let mut q = archive.query_pairs_mut();
        q.append_pair("limit", "100");
        if let Some(k) = target.kind {
            q.append_pair("kind", &k.to_string());
        }
        if let Some(a) = &target.author {
            q.append_pair("pubkey", a);
        }
    }
    let mut successful = 0;
    let mut inconclusive = false;
    // Inclusive timestamp windows do not skip tied timestamps. A full tied page
    // or an indexer that ignores `until` is inconclusive, never a disappearance.
    let deadline = tokio::time::Instant::now() + indexer_budget(cfg);
    for (index, base) in [wine, archive].into_iter().enumerate() {
        let mut until = None;
        let mut answered = false;
        let mut exhausted = false;
        for _ in 0..cfg.adapters.max_indexer_pages {
            let mut url = base.clone();
            if let Some(until) = until {
                url.query_pairs_mut()
                    .append_pair("until", &format!("{until}"));
            }
            let response = tokio::time::timeout_at(
                deadline,
                egress.discover(url.as_str(), &Default::default(), 3600),
            )
            .await;
            let Ok(Ok(f)) = response else {
                if tokio::time::Instant::now() >= deadline {
                    return Err(crate::resolver::RetryLater {
                        class: "inconclusive",
                        seconds: 300,
                    }
                    .into());
                }
                answered = false;
                break;
            };
            // Search endpoint 404 is an outage; only an archive ID lookup can
            // authoritatively say that its particular event is absent.
            if f.status == 404 && index == 1 && target.id.is_some() {
                answered = true;
                exhausted = true;
                break;
            }
            if f.status != 200 {
                answered = false;
                break;
            }
            let target = target.clone();
            let parsed = tokio::task::spawn_blocking(move || -> Result<_> {
                let v: Value = serde_json::from_slice(&f.body)?;
                ensure!(
                    v.is_array()
                        || v["data"].is_array()
                        || v["events"].is_array()
                        || v.get("id").is_some(),
                    "invalid indexer envelope"
                );
                let values = rows(&v);
                let oldest = values.iter().filter_map(|v| v["created_at"].as_u64()).min();
                Ok((newest(&target, &values), values.len() >= 100, oldest))
            })
            .await?;
            let Ok((event, full, oldest)) = parsed else {
                answered = false;
                break;
            };
            answered = true;
            if event.is_some() {
                return Ok(event);
            }
            if !full {
                exhausted = true;
                break;
            }
            // An HTTP success with a capped page is incomplete evidence. Its
            // cache must expire by the next inconclusive retry, not an hour later.
            egress
                .shorten_discovery(url.as_str(), &Default::default(), 300)
                .await?;
            let Some(oldest) = oldest.filter(|v| until.is_none_or(|previous| *v < previous)) else {
                break;
            };
            until = Some(oldest);
        }
        successful += usize::from(answered);
        inconclusive |= !exhausted;
    }
    if successful != 2 {
        return Err(crate::resolver::RetryLater {
            class: "indexer_unavailable",
            seconds: 300,
        }
        .into());
    }
    // A hint can supply a signed match, but cannot certify the absence of an
    // event outside the bounded indexer scan. Poll the relay only after both answer.
    if let Ok(values) = relay.await {
        let target = target.clone();
        if let Some(event) = tokio::task::spawn_blocking(move || newest(&target, &values)).await? {
            return Ok(Some(event));
        }
    }
    if inconclusive {
        return Err(crate::resolver::RetryLater {
            class: "inconclusive",
            seconds: 300,
        }
        .into());
    }
    Ok(None)
}
pub fn parse(uri: &str, event: &Event, cfg: &EventConfig) -> Result<ResourceMeta> {
    event.verify()?;
    let kind = event.kind.as_u16();
    ensure!(matches!(kind, 31922 | 31923), "not a NIP-52 event");
    ensure!(tag(event, "d").is_some(), "calendar event omitted d tag");
    let mut doc = ResourceMeta::new(uri, "nip52", 3600);
    doc.set_type("Event");
    doc.name = tag(event, "title")
        .or_else(|| tag(event, "name"))
        .map(str::to_owned);
    ensure!(doc.name.is_some(), "calendar event omitted title");
    doc.description = Some(event.content.clone());
    doc.image = tag(event, "image")
        .and_then(|s| Url::parse(s).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .map(|u| u.to_string());
    let start = tag(event, "start").context("calendar event omitted start")?;
    doc.event.tz = tag(event, "start_tzid")
        .and_then(time::zone)
        .map(|z| z.to_string());
    if kind == 31923 {
        doc.start = Some(time::from_timestamp(start.parse()?).context("invalid start")?);
        doc.end = tag(event, "end")
            .map(|t| time::from_timestamp(t.parse()?).context("invalid end"))
            .transpose()?
            .or(doc.start);
        if tag(event, "D").is_none() {
            doc.event.warnings.push("nostr_legacy_missing_D".into());
        }
    } else {
        // Dates whose midnight is a storable instant (`time::bounded`), so 0000-01-01 is refused
        // here rather than saved and dropped at the store, and 9999-12-31's default end (the
        // next day) is refused too.
        let storable =
            |d: chrono::NaiveDate| time::bounded(d.and_hms_opt(0, 0, 0)?.and_utc()).map(|_| d);
        let date = |s: &str| time::naive(s).and_then(|t| storable(t.date()));
        let start = date(start).context("invalid all-day start")?;
        let end = match tag(event, "end") {
            Some(s) => date(s).context("invalid all-day end")?,
            None => start
                .succ_opt()
                .and_then(storable)
                .context("invalid all-day end")?,
        };
        ensure!(end > start, "invalid all-day end");
        doc.start = Some(start.and_hms_opt(0, 0, 0).unwrap().and_utc());
        doc.end = Some(end.and_hms_opt(0, 0, 0).unwrap().and_utc());
        doc.event.all_day = Some(true);
        doc.event.tz = None;
    }
    ensure!(
        doc.start.is_some() && doc.end >= doc.start,
        "invalid Nostr date range"
    );
    let locations: Vec<_> = event
        .tags
        .iter()
        .filter_map(|t| {
            let a = t.as_slice();
            (a.first().is_some_and(|s| s == "location"))
                .then(|| a.get(1))
                .flatten()
                .cloned()
        })
        .collect();
    if !locations.is_empty() {
        doc.event.venue = Some(Venue {
            text: locations.join("; "),
            ..Default::default()
        });
    }
    for t in event.tags.iter() {
        let a = t.as_slice();
        if a.first().is_some_and(|k| k == "g") {
            doc.location = if a.len() >= 3 {
                a[1].parse::<f64>()
                    .ok()
                    .zip(a[2].parse::<f64>().ok())
                    .and_then(|(lat, lon)| page::point(&json!({"latitude":lat,"longitude":lon})))
            } else {
                a.get(1).and_then(|s| geohash(s))
            };
        }
        if a.first().is_some_and(|k| k == "t") {
            if let Some(v) = a.get(1) {
                doc.event.categories.push(v.clone());
            }
        }
        if a.first().is_some_and(|k| k == "r") {
            if let Some(v) = a.get(1).filter(|s| s.starts_with("https://")) {
                doc.aliases.push(v.clone());
            }
        }
    }
    if doc.location.is_some() {
        doc.location_source = Some(LocationSource::Page);
        if let Some(v) = &mut doc.event.venue {
            v.location = doc.location.clone();
            v.location_source = Some(LocationSource::Page);
        }
    }
    if !locations.is_empty()
        && locations
            .iter()
            .all(|s| Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "https" | "http")))
    {
        doc.event.attendance = Some(Attendance::Online);
    }
    page::finalize(&mut doc, cfg.coordinate_timezone && kind != 31922);
    doc.bounded(crate::resolver::web::ATTRIBUTES)
}
fn geohash(s: &str) -> Option<Point> {
    if s.is_empty() || s.len() > 12 {
        return None;
    }
    let mut lat = (-90.0, 90.0);
    let mut lon = (-180.0, 180.0);
    let mut longitude = true;
    for c in s.bytes() {
        let value = b"0123456789bcdefghjkmnpqrstuvwxyz"
            .iter()
            .position(|x| *x == c)?;
        for bit in (0..5).rev() {
            let pair = if longitude { &mut lon } else { &mut lat };
            let mid = (pair.0 + pair.1) / 2.0;
            if value & (1 << bit) > 0 {
                pair.0 = mid
            } else {
                pair.1 = mid
            }
            longitude = !longitude;
        }
    }
    Some(Point {
        lat: (lat.0 + lat.1) / 2.0,
        lon: (lon.0 + lon.1) / 2.0,
    })
}
pub async fn resolve(
    uri: &str,
    egress: &Egress,
    cfg: &EventConfig,
    pool: &sqlx::PgPool,
    probe: bool,
) -> Result<ResourceMeta> {
    // A manifest HTTP canary checks the protocol payload, not the indexer's HTML shell.
    if Url::parse(uri).is_ok_and(|u| u.host_str() == Some("api.nostr.wine")) {
        let m = super::manifest(super::definition("nip52").unwrap(), cfg);
        ensure!(
            m.canary.as_ref().is_some_and(|u| u.as_str() == uri),
            "not the configured Nostr canary"
        );
        let f = egress.fetch(uri).await?;
        ensure!(f.status == 200, "Nostr canary HTTP failure");
        let (uri, cfg) = (uri.to_owned(), cfg.clone());
        return tokio::task::spawn_blocking(move || {
            let v: Value = serde_json::from_slice(&f.body)?;
            for value in rows(&v).into_iter().take(256) {
                if let Ok(event) = serde_json::from_value::<Event>(value) {
                    if let Ok(doc) = parse(&uri, &event, &cfg) {
                        return Ok(doc);
                    }
                }
            }
            bail!("Nostr canary contained no valid calendar events")
        })
        .await?;
    }
    let target = Target::decode(uri)?;
    let Some(event) = lookup(&target, egress, cfg).await? else {
        let mut doc = ResourceMeta::new(uri, "nip52", 3600);
        doc.status = Status::Missing;
        return Ok(doc);
    };
    if event.kind.as_u16() == 31925 {
        let mut doc = ResourceMeta::new(uri, "nip52", 0);
        doc.status = Status::Missing;
        return Ok(doc);
    }
    if event.kind.as_u16() != 31924 {
        let (uri, cfg) = (uri.to_owned(), cfg.clone());
        return tokio::task::spawn_blocking(move || parse(&uri, &event, &cfg)).await?;
    }
    let mut doc = super::cursor::container(uri, cfg);
    doc.source = "nip52".into();
    doc.name = tag(&event, "title").map(str::to_owned);
    doc.description = Some(event.content.clone());
    let refs: Vec<_> = event
        .tags
        .iter()
        .filter_map(|t| {
            let a = t.as_slice();
            if a.first().is_some_and(|s| s == "a") {
                a.get(1)
                    .and_then(|s| Target::coordinate(s).ok())
                    .map(|mut t| {
                        if let Some(r) = a.get(2) {
                            t.relays.push(r.clone());
                        }
                        t
                    })
            } else {
                None
            }
        })
        .filter(|t| matches!(t.kind, Some(31922 | 31923)))
        .collect();
    ensure!(
        refs.len() <= cfg.adapters.max_calendar_members,
        "Nostr calendar member cap exceeded"
    );
    if probe || !cfg.adapters.derive_resources {
        doc.container = None;
        return Ok(doc);
    }
    let mut pending = false;
    let mut retained_bytes = 0usize;
    // The wait for unresolved members is bounded twice: per member, from when this calendar
    // first listed it (not from when anyone first referenced it), and for the calendar as a
    // whole, from its last stored snapshot (else from when it was first seen), so members added
    // one after another cannot keep it from applying its other members' changes for good.
    let calendar_id = crate::model::identity(uri)?.0;
    let wait = cfg.adapters.calendar_member_wait_hours as f64;
    let calendar_overdue: bool = sqlx::query_scalar(
        "SELECT coalesce(to_timestamp(e.last_snapshot_at/1000.0),r.created_at)<now()-$2*interval '1 hour' FROM resource r LEFT JOIN event_fetch e ON e.resource_id=r.id WHERE r.id=$1",
    )
    .bind(&calendar_id)
    .bind(wait)
    .fetch_optional(pool)
    .await?
    .unwrap_or(false);
    for target in refs {
        let resource = target.uri()?;
        let Some(id) = crate::store::derive_resource_until(
            pool,
            uri,
            &resource,
            "nip52",
            cfg.adapters.max_calendar_members,
            cfg.adapters.max_derived_resources,
            None,
        )
        .await?
        else {
            return Err(crate::resolver::RetryLater {
                class: "derivation_cap",
                seconds: 86400,
            }
            .into());
        };
        let (value, failed, member_overdue): (Value, bool, bool) = sqlx::query_as(
            "SELECT d.doc,(r.status IN ('error','unreachable') OR r.dead_letter OR r.final) AND r.status<>'ok',coalesce((SELECT l.first_seen_at FROM resource_derivation l WHERE l.parent_id=$3 AND l.child_id=d.id),now())<now()-$2*interval '1 hour' FROM resource_document d JOIN resource r ON r.id=d.id WHERE d.id=$1",
        )
        .bind(&id)
        .bind(wait)
        .bind(&calendar_id)
        .fetch_one(pool)
        .await?;
        let overdue = member_overdue || calendar_overdue;
        // A member whose own resolution failed (refused, unreachable, dead-lettered) is skipped
        // like a missing one, or the calendar would wait for it forever: pending answers never
        // count toward dead-letter.
        let unavailable = |doc: &mut ResourceMeta| {
            if !doc
                .event
                .warnings
                .iter()
                .any(|w| w == "calendar_member_unavailable")
            {
                doc.event
                    .warnings
                    .push("calendar_member_unavailable".into());
                let c = doc.container.as_mut().unwrap();
                c.warnings.push("calendar_member_unavailable".into());
            }
        };
        if failed {
            unavailable(&mut doc);
            continue;
        }
        // A member that keeps answering "try later" (an indexer gone, an author past the
        // indexer's page limit) never reaches `error`: past the configured wait it is
        // unavailable too.
        let parsed = value
            .get("type")
            .is_some()
            .then(|| serde_json::from_value::<ResourceMeta>(value))
            .transpose()?;
        let Some(mut event_doc) = parsed.filter(|d| d.status != Status::Pending) else {
            if overdue {
                unavailable(&mut doc);
            } else {
                pending = true;
            }
            continue;
        };
        if event_doc.status == Status::Missing {
            continue;
        }
        if event_doc.status != Status::Ok {
            if overdue {
                unavailable(&mut doc);
            } else {
                pending = true;
            }
            continue;
        }
        // An accepted member without a start (its start was out of range and dropped at the
        // store) will not gain one by waiting.
        if event_doc.start.is_none() {
            unavailable(&mut doc);
            continue;
        }
        event_doc.url = resource;
        let c = doc.container.as_mut().unwrap();
        if event_doc
            .start
            .is_some_and(|at| at >= c.horizon_start && at <= c.horizon_end)
        {
            let content_hash =
                blake3::hash(&serde_json::to_vec(&crate::changes::normalize(&event_doc))?)
                    .to_hex()
                    .to_string();
            retained_bytes = retained_bytes.saturating_add(serde_json::to_vec(&event_doc)?.len());
            ensure!(
                retained_bytes <= cfg.max_event_document_bytes,
                "Nostr calendar document memory cap"
            );
            ensure!(
                c.occurrences.len() < cfg.max_occurrences_per_feed,
                "Nostr calendar occurrence cap"
            );
            c.occurrences.push(Occurrence {
                uid: format!(
                    "{}:{}:{}",
                    target.kind.unwrap(),
                    target.author.as_deref().unwrap(),
                    target.identifier.as_deref().unwrap()
                ),
                recurrence_id: String::new(),
                sequence: 0,
                modified: None,
                content_hash,
                volatile: false,
                doc: event_doc,
            });
        }
    }
    if pending {
        return Err(crate::resolver::RetryLater {
            class: "calendar_members_pending",
            seconds: 3600,
        }
        .into());
    }
    Ok(doc)
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn empty_or_unrelated_relay_cannot_confirm_an_inconclusive_indexer_scan() {
        use crate::resolver::egress::{Dns, Fetched, Transport};
        use nostr::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::{net::SocketAddr, sync::Arc};
        struct Fixture(AtomicUsize);
        #[async_trait::async_trait]
        impl Dns for Fixture {
            async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
                Ok(vec![SocketAddr::new("93.184.216.34".parse()?, port)])
            }
        }
        #[async_trait::async_trait]
        impl Transport for Fixture {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: url.clone(),
                    status: 200,
                    validators: Default::default(),
                    retry_after_secs: 60,
                    content_type: "application/json".into(),
                    location: None,
                    body: serde_json::to_vec(&if url.host_str() == Some("api.nostr.wine") {
                        vec![json!({"created_at":200}); 100]
                    } else {
                        vec![]
                    })?,
                })
            }
        }
        // A public fixture key, never used by a live identity.
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let unsigned = EventBuilder::new(Kind::from(31923), "fixture")
            .tags([Tag::parse(["d", "wanted"]).unwrap()])
            .finalize_unsigned(keys.public_key());
        let signature = keys.sign_schnorr_with_aux_rand(
            &secp256k1::Secp256k1::new(),
            unsigned.compute_id().as_bytes(),
            &[0; 32],
        );
        let event = unsigned.add_signature(signature).unwrap();
        let mut target =
            Target::coordinate(&format!("31923:{}:wanted", keys.public_key())).unwrap();
        target.relays.push("wss://hint.example/".into());
        let mut cfg = EventConfig::default();
        cfg.adapters.max_indexer_pages = 1;
        let fixture = Arc::new(Fixture(AtomicUsize::new(0)));
        let egress = Egress::with_backends(Default::default(), fixture.clone(), fixture.clone());
        for values in [vec![], vec![json!({"kind":31923,"content":"unsigned"})]] {
            let error = lookup_inner(&target, &egress, &cfg, std::future::ready(Ok(values)))
                .await
                .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<crate::resolver::RetryLater>()
                    .unwrap()
                    .class,
                "inconclusive"
            );
        }
        assert_eq!(
            lookup_inner(
                &target,
                &egress,
                &cfg,
                std::future::ready(Ok(vec![json!(event)]))
            )
            .await
            .unwrap()
            .unwrap()
            .id,
            event.id
        );
        assert_eq!(fixture.0.load(Ordering::SeqCst), 2);
        tokio::time::advance(std::time::Duration::from_secs(301)).await;
        assert!(
            lookup_inner(&target, &egress, &cfg, std::future::ready(Ok(vec![])))
                .await
                .is_err()
        );
        assert_eq!(
            fixture.0.load(Ordering::SeqCst),
            3,
            "the capped page refreshes while the complete empty indexer stays cached"
        );
    }

    #[test]
    fn relay_time_is_reserved_within_the_queue_budget() {
        let mut cfg = EventConfig::default();
        cfg.adapters.websocket_deadline_ms = 5000;
        assert_eq!(indexer_budget(&cfg), std::time::Duration::from_secs(8));
        assert!(
            indexer_budget(&cfg)
                + std::time::Duration::from_millis(cfg.adapters.websocket_deadline_ms)
                < crate::queue::RESOLVE_TIMEOUT - std::time::Duration::from_secs(1)
        );
    }
}
