use super::{options, state, Definition};
use crate::{
    config::EventConfig,
    event::{Container, Occurrence},
    model::ResourceMeta,
    resolver::egress::{Egress, FetchOptions},
};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use url::Url;

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub source: String,
    pub adapter: String,
    pub previous: Option<String>,
    pub next: String,
    pub reset: bool,
}
impl Checkpoint {
    pub async fn check(&self, tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<bool> {
        ensure!(
            self.next.len() <= 4096 && self.source == id,
            "invalid cursor checkpoint"
        );
        let current: Option<String> = sqlx::query_scalar(
            "SELECT adapter_cursor FROM event_fetch WHERE resource_id=$1 FOR UPDATE",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        Ok(current == self.previous)
    }
    pub async fn commit(&self, tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<()> {
        sqlx::query("UPDATE event_fetch SET adapter_cursor=$2 WHERE resource_id=$1")
            .bind(id)
            .bind(&self.next)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO source_state(source,last_success_at,cursor_resets_total) VALUES($1,now(),$2) ON CONFLICT(source) DO UPDATE SET last_success_at=now(),cursor_resets_total=source_state.cursor_resets_total+excluded.cursor_resets_total").bind(&self.adapter).bind(i64::from(self.reset)).execute(&mut **tx).await?;
        Ok(())
    }
}
pub fn rows(v: &Value) -> Option<&Vec<Value>> {
    v.as_array().or_else(|| {
        ["data", "events", "meetup_events", "items"]
            .iter()
            .find_map(|k| v[*k].as_array())
    })
}
fn uid(v: &Value) -> Option<String> {
    v["id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| v["id"].as_i64().map(|x| x.to_string()))
}
fn timestamp(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()?.parse().ok()
}
pub fn container(uri: &str, cfg: &EventConfig) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, "event", 3600);
    doc.set_type("Collection");
    let now = Utc::now();
    doc.container = Some(Box::new(Container {
        limits: cfg.container.clone(),
        horizon_start: now
            .checked_sub_months(chrono::Months::new(cfg.months_back))
            .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC),
        horizon_end: now
            .checked_add_months(chrono::Months::new(cfg.months_forward))
            .unwrap_or(chrono::DateTime::<Utc>::MAX_UTC),
        held_retry_secs: cfg.held_retry_secs,
        ..Default::default()
    }));
    doc
}
pub fn append(doc: &mut ResourceMeta, adapter: &str, v: &Value, cfg: &EventConfig) -> Result<()> {
    let id = uid(v).context("event feed row has no identity")?;
    ensure!(id.len() <= 256, "event identity too long");
    let c = doc.container.as_mut().context("not a container")?;
    if adapter == "btcmap" && v["status"] != "live"
        || v["status"] == "cancelled"
        || v["status"] == "rejected"
        || v.get("deleted_at").is_some_and(|v| !v.is_null())
    {
        c.deleted_uids.push(id.clone());
        if adapter == "btcmap" || v.get("start").is_none() && v.get("starts_at").is_none() {
            return Ok(());
        }
    }
    let mut event = state::object(adapter, &doc.url, v).context("event feed row has no start")?;
    if event.name.is_none() {
        event.name = v
            .pointer("/meetup/name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some(format!("Meetup {id}")));
    }
    if let Some(url) = v["website"]
        .as_str()
        .or_else(|| v["htmlLink"].as_str())
        .or_else(|| v["url"].as_str())
        .or_else(|| v["link"].as_str())
    {
        if Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
            event.url = url.into();
        }
    }
    if let Some(a) = v["nostr_address"].as_str() {
        if let Ok(t) = super::nostr::Target::coordinate(a) {
            event.aliases.push(t.uri()?);
        }
    }
    crate::event::page::finalize(&mut event, cfg.coordinate_timezone);
    let at = event.start.context("missing start")?;
    if at < c.horizon_start || at > c.horizon_end {
        return Ok(());
    }
    let modified = timestamp(&v["updated_at"]).or_else(|| timestamp(&v["updated"]));
    let recurrence = v["recurringEventId"]
        .as_str()
        .and_then(|_| {
            v["originalStartTime"]["dateTime"]
                .as_str()
                .or_else(|| v["originalStartTime"]["date"].as_str())
        })
        .unwrap_or("")
        .to_owned();
    event = event.bounded(crate::resolver::web::ATTRIBUTES)?;
    let content_hash = blake3::hash(&serde_json::to_vec(&crate::changes::normalize(&event))?)
        .to_hex()
        .to_string();

    if adapter == "einundzwanzig" {
        if let Some(place) = portal_place(v)? {
            if event.event.tz.is_none() && cfg.coordinate_timezone {
                event.event.tz = place
                    .location
                    .as_ref()
                    .and_then(crate::event::time::coordinate_zone);
                if event.event.tz.is_some() {
                    event.event.timezone_attribution = Some(crate::event::time::ATTRIBUTION.into());
                }
            }
            c.osm_venues.push((id.clone(), place));
        }
    }
    c.occurrences.push(Occurrence {
        uid: id,
        recurrence_id: recurrence,
        doc: event,
        sequence: v["sequence"].as_i64().unwrap_or(0),
        modified,
        content_hash,
        volatile: false,
    });
    ensure!(
        c.occurrences.len() <= cfg.max_occurrences_per_feed,
        "cursor event cap exceeded"
    );
    Ok(())
}
async fn json(
    egress: &Egress,
    uri: &str,
    cfg: &EventConfig,
    sensitive: bool,
    cached: bool,
) -> Result<(u16, Value)> {
    let options = FetchOptions {
        feed_hint: true,
        feed_cap: Some(cfg.feed_body_bytes),
        sensitive,
        ..Default::default()
    };
    // A snapshot used to confirm a held resync must be a fresh upstream response.
    // Only the shared full-list lookup used for an individual portal page is cached.
    let f = if cached {
        egress.discover(uri, &options, 3600).await?
    } else {
        egress.fetch_with(uri, &options).await?
    };
    if f.status == 410 {
        return Ok((410, Value::Null));
    }
    ensure!(f.status == 200, "cursor HTTP failure");
    Ok((
        f.status,
        tokio::task::spawn_blocking(move || serde_json::from_slice(&f.body)).await??,
    ))
}
pub async fn resolve(
    pool: &PgPool,
    uri: &str,
    d: &Definition,
    egress: &Egress,
    cfg: &EventConfig,
    probe: bool,
) -> Result<ResourceMeta> {
    let original = Url::parse(uri)?;
    if d.id == "einundzwanzig" && !original.path().starts_with("/api/") {
        let id = original
            .path_segments()
            .and_then(|mut s| s.next_back())
            .context("portal event ID missing")?
            .to_owned();
        let (_, v) = json(
            egress,
            "https://portal.einundzwanzig.space/api/meetup-events",
            cfg,
            false,
            true,
        )
        .await?;
        let uri = uri.to_owned();
        let cfg = cfg.clone();
        return tokio::task::spawn_blocking(move || {
            let row = rows(&v)
                .context("invalid portal event list")?
                .iter()
                .find(|r| uid(r).as_deref() == Some(&id))
                .context("portal event absent")?;
            single(&uri, "einundzwanzig", row, &cfg)
        })
        .await?;
    }
    let resource_id = crate::model::identity(uri)?.0;
    let source = resource_id;
    let previous: Option<String> = if probe {
        None
    } else {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT adapter_cursor FROM event_fetch WHERE resource_id=$1",
        )
        .bind(&source)
        .fetch_optional(pool)
        .await?
        .flatten()
    };
    let mut doc = container(uri, cfg);
    let mut next = previous.clone().unwrap_or_default();
    let mut reset = false;
    let mut delta = previous.is_some();
    if d.id == "einundzwanzig" {
        let mut since = previous.clone().unwrap_or_else(|| "0".into());
        let mut done = false;
        for n in 0..cfg.adapters.max_cursor_pages {
            let mut url = Url::parse("https://portal.einundzwanzig.space/api/changes")?;
            url.query_pairs_mut()
                .append_pair("since_sequence", &since)
                .append_pair("limit", if probe { "1" } else { "1000" });
            let (_, v) = json(egress, url.as_str(), cfg, false, false).await?;
            next = v["next_since"]
                .as_i64()
                .map(|v| v.to_string())
                .or_else(|| v["next_since"].as_str().map(str::to_owned))
                .context("portal omitted next_since")?;
            reset = v["cursor_expired"] == true;
            if (previous.is_none() || reset) && !probe && n == 0 {
                let (_, full) = json(
                    egress,
                    "https://portal.einundzwanzig.space/api/meetup-events",
                    cfg,
                    false,
                    false,
                )
                .await?;
                doc = append_batch(doc, &d.id, full, "full", cfg, None).await?.0;
                delta = false;
                done = true;
                break;
            }
            let more = v["has_more"] == true;
            doc = append_batch(doc, &d.id, v, "portal", cfg, None).await?.0;
            if !more || probe {
                done = true;
                break;
            }
            ensure!(next != since, "non-advancing portal cursor");
            since = next.clone();
        }
        ensure!(done, "portal pagination cap exceeded");
    } else if d.id == "btcmap" {
        let original = Url::parse(uri)?;
        if original
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .is_some_and(|s| s.parse::<u64>().is_ok())
        {
            let (_, v) = json(egress, uri, cfg, false, false).await?;
            let uri = uri.to_owned();
            let cfg = cfg.clone();
            return tokio::task::spawn_blocking(move || single(&uri, "btcmap", &v, &cfg)).await?;
        }
        // Delta semantics expose updated_at and tombstones even for the initial sync.
        let since = previous.as_deref().unwrap_or("1970-01-01T00:00:00Z");
        let mut url = Url::parse("https://api.btcmap.org/v4/events")?;
        url.query_pairs_mut()
            .append_pair("updated_since", since)
            .append_pair("include_deleted", "true")
            .append_pair("status", "all");
        let (_, v) = json(egress, url.as_str(), cfg, false, false).await?;
        let (parsed, latest) =
            append_batch(doc, &d.id, v, "btcmap", cfg, Some(since.to_owned())).await?;
        doc = parsed;
        next = latest.context("BTC Map cursor missing")?;
        delta = true;
    } else {
        let base = Url::parse(uri)?;
        let key = options(cfg, &d.id)
            .and_then(|o| o.key.as_deref())
            .context("Google Calendar key missing")?;
        let mut token = previous.clone();
        let mut page_token = None::<String>;
        let mut done = false;
        for _ in 0..cfg.adapters.max_cursor_pages {
            let mut url = base.clone();
            url.set_query(None);
            url.query_pairs_mut()
                .append_pair("key", key)
                .append_pair("showDeleted", "true")
                .append_pair("singleEvents", "true")
                .append_pair("maxResults", "2500");
            if let Some(t) = &token {
                url.query_pairs_mut().append_pair("syncToken", t);
            }
            if let Some(t) = &page_token {
                url.query_pairs_mut().append_pair("pageToken", t);
            }
            let (status, v) = json(egress, url.as_str(), cfg, true, false).await?;
            if status == 410 {
                ensure!(!reset, "repeated Google cursor expiration");
                reset = true;
                delta = false;
                token = None;
                page_token = None;
                doc.container.as_mut().unwrap().occurrences.clear();
                doc.container.as_mut().unwrap().deleted_uids.clear();
                continue;
            }
            page_token = v["nextPageToken"].as_str().map(str::to_owned);
            let sync = v["nextSyncToken"].as_str().map(str::to_owned);
            doc = append_batch(doc, &d.id, v, "full", cfg, None).await?.0;
            if page_token.is_none() {
                next = sync.context("Google omitted sync token")?;
                done = true;
                break;
            }
        }
        ensure!(done, "Google pagination cap exceeded");
    }
    let c = doc.container.as_mut().unwrap();
    c.delta = delta;
    c.cursor = Some(Checkpoint {
        source,
        adapter: d.id.clone(),
        previous,
        next,
        reset,
    });
    Ok(doc)
}

fn single(uri: &str, adapter: &str, row: &Value, cfg: &EventConfig) -> Result<ResourceMeta> {
    let mut doc = container(uri, cfg);
    let c = doc.container.as_mut().unwrap();
    c.horizon_start = DateTime::<Utc>::MIN_UTC;
    c.horizon_end = DateTime::<Utc>::MAX_UTC;
    append(&mut doc, adapter, row, cfg)?;
    ensure!(
        !doc.container.as_ref().unwrap().occurrences.is_empty(),
        "event is not public"
    );
    Ok(doc)
}

async fn append_batch(
    mut doc: ResourceMeta,
    adapter: &str,
    v: Value,
    kind: &str,
    cfg: &EventConfig,
    since: Option<String>,
) -> Result<(ResourceMeta, Option<String>)> {
    let (adapter, kind, cfg) = (adapter.to_owned(), kind.to_owned(), cfg.clone());
    tokio::task::spawn_blocking(move || {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(cfg.parse_timeout_ms);
        let entries = if kind == "portal" {
            v["changes"].as_array()
        } else {
            rows(&v)
        }
        .context("cursor response omitted entries")?;
        ensure!(entries.len() <= cfg.max_components, "cursor item cap");
        let mut latest = since
            .as_deref()
            .map(str::parse::<DateTime<Utc>>)
            .transpose()?;
        let mut retained = 0usize;
        let mut document_bytes = doc
            .container
            .as_ref()
            .unwrap()
            .occurrences
            .iter()
            .try_fold(0usize, |sum, row| -> Result<usize> {
                Ok(sum.saturating_add(serde_json::to_vec(&row.doc)?.len()))
            })?;
        document_bytes = document_bytes
            .saturating_add(serde_json::to_vec(&doc.container.as_ref().unwrap().osm_venues)?.len());
        let mut operations = std::collections::BTreeMap::new();
        for row in entries {
            ensure!(std::time::Instant::now() < deadline, "cursor parse budget");
            let before = doc.container.as_ref().unwrap().occurrences.len();
            let before_places = doc.container.as_ref().unwrap().osm_venues.len();
            if kind == "portal" {
                if row["resource"] != "meetup-event" {
                    continue;
                }
                if row["action"] == "deleted" {
                    let id = uid(row).context("portal tombstone omitted identity")?;
                    ensure!(id.len() <= 256, "tombstone identity cap");
                    operations.insert(id.clone(), false);
                    doc.container.as_mut().unwrap().deleted_uids.push(id);
                } else {
                    operations.insert(uid(&row["data"]).context("portal row identity")?, true);
                    append(&mut doc, &adapter, &row["data"], &cfg)?;
                }
            } else {
                if kind == "btcmap" {
                    let at = timestamp(&row["updated_at"])
                        .context("BTC Map delta omitted updated_at")?;
                    latest = Some(latest.map_or(at, |old| old.max(at)));
                }
                append(&mut doc, &adapter, row, &cfg)?;
            }
            let c = doc.container.as_ref().unwrap();
            if c.occurrences.len() > before {
                document_bytes = document_bytes
                    .saturating_add(serde_json::to_vec(&c.occurrences.last().unwrap().doc)?.len());
            }
            if c.osm_venues.len() > before_places {
                document_bytes = document_bytes
                    .saturating_add(serde_json::to_vec(c.osm_venues.last().unwrap())?.len());
            }
            ensure!(
                document_bytes <= cfg.max_event_document_bytes,
                "cursor document memory cap"
            );
            ensure!(
                c.deleted_uids.len() <= cfg.max_occurrences_per_feed,
                "cursor tombstone cap"
            );
            // Conservative accounting also charges tombstones and rows outside the horizon.
            retained = retained.saturating_add(serde_json::to_vec(row)?.len());
            ensure!(retained <= cfg.feed_body_bytes, "cursor row bytes cap");
        }
        // Changes are ordered by sequence. A later recreation wins over an earlier
        // deletion, including across pages; never apply two conflicting row writes.
        if kind == "portal" {
            let c = doc.container.as_mut().unwrap();
            let mut unique = std::collections::BTreeMap::new();
            for row in std::mem::take(&mut c.occurrences) {
                if operations.get(&row.uid) != Some(&false) {
                    unique.insert(row.key(), row);
                }
            }
            c.occurrences = unique.into_values().collect();
            c.deleted_uids
                .retain(|uid| operations.get(uid) != Some(&true));
            c.osm_venues
                .retain(|(uid, _)| operations.get(uid) != Some(&false));
        }
        Ok((doc, latest.map(|at| at.to_rfc3339())))
    })
    .await?
}
fn portal_place(v: &Value) -> Result<Option<ResourceMeta>> {
    let (Some(kind), Some(id)) = (
        v["osm_type"].as_str(),
        v["osm_id"]
            .as_i64()
            .or_else(|| v["osm_id"].as_str().and_then(|s| s.parse().ok())),
    ) else {
        return Ok(None);
    };
    let uri = format!("https://www.openstreetmap.org/{kind}/{id}");
    if crate::resolver::osm::parse_id(&uri).is_none() {
        return Ok(None);
    }
    let Some(point) = crate::event::page::point(
        &serde_json::json!({"latitude":v["osm_lat"],"longitude":v["osm_lon"]}),
    ) else {
        return Ok(None);
    };
    let mut place = ResourceMeta::new(&uri, "osm", 30 * 86400);
    place.set_type("Place");
    place.location = Some(point);
    place.location_source = Some(crate::model::LocationSource::Osm);
    place.name = v["osm_name"].as_str().map(str::to_owned);
    if let Some(address) = v["osm_address"].as_object() {
        for (k, v) in address {
            if let Some(s) = v.as_str() {
                place.address.insert(k.clone(), s.to_owned());
            }
        }
    }
    Ok(Some(place.bounded(crate::resolver::osm::ATTRIBUTES)?))
}
