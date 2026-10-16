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
        if let Some(k) = target.kind {
            q.append_pair("kind", &k.to_string());
        }
        if let Some(a) = &target.author {
            q.append_pair("pubkey", a);
        }
    }
    let mut successful = 0;
    for url in [wine, archive] {
        if let Ok(f) = egress
            .discover(url.as_str(), &Default::default(), 3600)
            .await
        {
            if f.status == 200 {
                let target = target.clone();
                if let Ok(Ok(event)) =
                    tokio::task::spawn_blocking(move || -> Result<Option<Event>> {
                        let v: Value = serde_json::from_slice(&f.body)?;
                        ensure!(
                            v.is_array()
                                || v["data"].is_array()
                                || v["events"].is_array()
                                || v.get("id").is_some(),
                            "invalid indexer envelope"
                        );
                        Ok(newest(&target, &rows(&v)))
                    })
                    .await
                {
                    successful += 1;
                    if event.is_some() {
                        return Ok(event);
                    }
                }
            } else if f.status == 404 {
                successful += 1;
            }
        }
    }
    if cfg.adapters.websocket {
        if let Some(relay) = target.relays.first() {
            if let Ok(values) = egress
                .nostr_request(relay, target.filter(), cfg.adapters.websocket_deadline_ms)
                .await
            {
                let target = target.clone();
                return Ok(tokio::task::spawn_blocking(move || newest(&target, &values)).await?);
            }
        }
    }
    ensure!(
        successful == 2,
        "Nostr indexers unavailable; refusing partial disappearance"
    );
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
        doc.start = chrono::DateTime::from_timestamp(start.parse()?, 0);
        doc.end = tag(event, "end")
            .map(str::parse::<i64>)
            .transpose()?
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .or(doc.start);
        if tag(event, "D").is_none() {
            doc.event.warnings.push("nostr_legacy_missing_D".into());
        }
    } else {
        let start = chrono::NaiveDate::parse_from_str(start, "%Y-%m-%d")?;
        let end = tag(event, "end")
            .map(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d"))
            .transpose()?
            .unwrap_or(start.succ_opt().context("date overflow")?);
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
    if uri.starts_with("https://api.nostr.wine/search?") {
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
    if probe {
        return Ok(doc);
    }
    let mut pending = false;
    let mut retained_bytes = 0usize;
    for target in refs {
        let resource = target.uri()?;
        let id = crate::store::ensure_resource(pool, &resource).await?;
        let value: Value = sqlx::query_scalar("SELECT doc FROM resource_document WHERE id=$1")
            .bind(&id)
            .fetch_one(pool)
            .await?;
        if value.get("type").is_none() {
            pending = true;
            continue;
        }
        let mut event_doc: ResourceMeta = serde_json::from_value(value)?;
        if event_doc.status == Status::Missing {
            continue;
        }
        if event_doc.status != Status::Ok || event_doc.start.is_none() {
            pending = true;
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
