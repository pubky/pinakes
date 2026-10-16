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

/// Dataset roots are shared even when an author tags an arbitrary query variant.
/// A Google calendar is its own dataset; its validated calendar ID selects the root.
pub fn canonical_root(uri: &str) -> Option<String> {
    let u = Url::parse(uri).ok()?;
    match (u.host_str()?, u.path()) {
        ("api.btcmap.org", "/v4/events") => Some("https://api.btcmap.org/v4/events".into()),
        ("portal.einundzwanzig.space", "/api/changes" | "/api/meetup-events") => {
            Some("https://portal.einundzwanzig.space/api/changes".into())
        }
        ("www.googleapis.com", _) => super::service::keyed_url("google_calendar_api", uri)
            .ok()
            .map(Into::into),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub source: String,
    pub adapter: String,
    pub previous: Option<String>,
    pub next: String,
    pub reset: bool,
    pub resync: bool,
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
        sqlx::query(
            "UPDATE event_fetch SET adapter_cursor=$2,adapter_resync=$3 WHERE resource_id=$1",
        )
        .bind(id)
        .bind(&self.next)
        .bind(self.resync)
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
    append_row(doc, adapter, v, cfg).map_err(row_error)?;
    ensure!(
        doc.container.as_ref().unwrap().occurrences.len() <= cfg.max_occurrences_per_feed,
        "cursor event cap exceeded"
    );
    Ok(())
}
#[derive(Debug)]
struct RowFailure(String);
impl std::fmt::Display for RowFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RowFailure {}
fn row_error(error: impl std::fmt::Display) -> anyhow::Error {
    RowFailure(error.to_string()).into()
}
fn append_row(doc: &mut ResourceMeta, adapter: &str, v: &Value, cfg: &EventConfig) -> Result<()> {
    let id = uid(v).context("event feed row has no identity")?;
    ensure!(id.len() <= 256, "event identity too long");
    let c = doc.container.as_mut().context("not a container")?;
    let deleted = v.get("deleted_at").is_some_and(|v| !v.is_null())
        || matches!(
            v["status"].as_str(),
            Some("cancelled" | "canceled" | "rejected" | "deleted")
        );
    if deleted {
        c.deleted_uids.push(id.clone());
        if adapter == "btcmap" || v.get("start").is_none() && v.get("starts_at").is_none() {
            return Ok(());
        }
    }
    if adapter == "btcmap" && !deleted && v["status"] != "live" {
        // An unknown status is drift, not evidence of disappearance or an update.
        crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
        return Ok(());
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
    if !c.delta && (at < c.horizon_start || at > c.horizon_end) {
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

    if adapter == "einundzwanzig" {
        // These fields are supplied by a third-party feed, not by Nominatim.
        if let Some(place) = portal_place(v)? {
            event.location = place.location.clone();
            event.location_source = Some(crate::model::LocationSource::Page);
            let venue = event.event.venue.get_or_insert_with(Default::default);
            venue.location = place.location;
            venue.location_source = Some(crate::model::LocationSource::Page);
            venue.text = place.name.clone().unwrap_or_default();
            venue.name = place.name;
            venue.address = place.address;
            venue.osm_id = None;
            venue.osm_version = None;
            venue.resource_id = None;
            crate::event::page::finalize(&mut event, cfg.coordinate_timezone);
        }
    }
    let content_hash = blake3::hash(&serde_json::to_vec(&crate::changes::normalize(&event))?)
        .to_hex()
        .to_string();
    c.occurrences.push(Occurrence {
        uid: id,
        recurrence_id: recurrence,
        doc: event,
        sequence: v["sequence"].as_i64().unwrap_or(0),
        modified,
        content_hash,
        volatile: false,
    });
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
    super::service::upstream_status(&f)?;
    if gone(f.status) {
        return Ok((f.status, Value::Null));
    }
    if f.status != 200 {
        return Err(crate::resolver::RetryLater {
            class: "upstream_denied",
            seconds: 3600,
        }
        .into());
    }
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
    if !probe {
        if let Some(root) = canonical_root(uri).filter(|r| r != uri) {
            let (alias, alias_uri, scheme) = crate::model::identity(uri)?;
            let (id, root_uri, root_scheme) = crate::model::identity(&root)?;
            let mut tx = pool.begin().await?;
            crate::store::lock_search_revision(&mut tx).await?;
            crate::store::insert_resource(&mut tx, &alias, &alias_uri, &scheme).await?;
            crate::store::insert_resource(&mut tx, &id, &root_uri, &root_scheme).await?;
            sqlx::query("INSERT INTO cursor_alias(alias_id,root_id) VALUES($1,$2) ON CONFLICT(alias_id) DO NOTHING")
                .bind(&alias).bind(&id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM occurrence WHERE resource_id=$1")
                .bind(&alias)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM event_fetch WHERE resource_id=$1")
                .bind(&alias)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            let value: Option<Value> =
                sqlx::query_scalar("SELECT doc FROM resource_document WHERE id=$1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await?;
            let Some(value) = value.filter(|v| v.get("type").is_some()) else {
                return Err(crate::resolver::RetryLater {
                    class: "cursor_root_pending",
                    seconds: 60,
                }
                .into());
            };
            let mut doc: ResourceMeta = serde_json::from_value(value)?;
            doc.container = None;
            doc.url = uri.into();
            doc.aliases.push(root);
            doc.source = d.id.clone();
            return Ok(doc);
        }
    }
    let original = Url::parse(uri)?;
    if d.id == "einundzwanzig" && !original.path().starts_with("/api/") {
        let id = original
            .path_segments()
            .and_then(|mut s| s.next_back())
            .context("portal event ID missing")?
            .to_owned();
        let (status, mut v) = json(
            egress,
            "https://portal.einundzwanzig.space/api/meetup-events",
            cfg,
            false,
            true,
        )
        .await?;
        if gone(status) {
            return Ok(missing(uri, &d.id));
        }
        if rows(&v).is_some_and(|rows| !rows.iter().any(|r| uid(r).as_deref() == Some(&id))) {
            // A shared cached miss cannot confirm a fresh disappearance.
            let (status, fresh) = json(
                egress,
                "https://portal.einundzwanzig.space/api/meetup-events",
                cfg,
                false,
                false,
            )
            .await?;
            if gone(status) {
                return Ok(missing(uri, &d.id));
            }
            v = fresh;
        }
        portal_names(&mut v, egress, cfg).await?;
        let uri = uri.to_owned();
        let cfg = cfg.clone();
        return tokio::task::spawn_blocking(move || {
            let row = rows(&v)
                .context("invalid portal event list")?
                .iter()
                .find(|r| uid(r).as_deref() == Some(&id));
            if let Some(row) = row {
                single(&uri, "einundzwanzig", row, &cfg)
            } else {
                let mut doc = ResourceMeta::new(&uri, "einundzwanzig", 3600);
                doc.status = crate::model::Status::Missing;
                Ok(doc)
            }
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
    let mut resync = false;
    let mut delta = previous.is_some();
    if d.id == "einundzwanzig" {
        resync = previous.is_none()
            || (!probe
                && sqlx::query_scalar::<_, bool>(
                    "SELECT adapter_resync FROM event_fetch WHERE resource_id=$1",
                )
                .bind(&source)
                .fetch_optional(pool)
                .await?
                .unwrap_or(true));
        let ceiling = (doc.fetched_at + 5 * 60 * 1000).to_string();
        let mut since = previous
            .clone()
            .filter(|v| valid_sequence(v) && decimal_cmp(v, &ceiling).is_le())
            .unwrap_or_else(|| {
                if previous.is_some() {
                    reset = true;
                    resync = true;
                }
                "0".into()
            });
        let mut done = false;
        let mut expired = false;
        // Partial history is a delta. Only a complete, fresh full list may imply absence.
        doc.container.as_mut().unwrap().delta = true;
        for page in 0..cfg.adapters.max_cursor_pages {
            let mut url = Url::parse("https://portal.einundzwanzig.space/api/changes")?;
            // The portal's API default (no since_sequence) returns the head.
            // Read it before the full snapshot; subsequent changes replay from that head.
            let head = page == 0 && previous.is_none() && !probe;
            if !head {
                url.query_pairs_mut().append_pair("since_sequence", &since);
            }
            url.query_pairs_mut()
                .append_pair("limit", if probe || head { "1" } else { "1000" });
            let (status, mut v) = json(egress, url.as_str(), cfg, false, false).await?;
            if gone(status) {
                return Ok(missing_container(uri, &d.id, cfg));
            }
            next = v["next_since"]
                .as_u64()
                .map(|v| v.to_string())
                .or_else(|| v["next_since"].as_str().map(str::to_owned))
                .context("portal omitted next_since")?;
            ensure!(valid_sequence(&next), "invalid portal next_since");
            // Portal cursors are sequences, not timestamps. Reject absurd future-sized
            // values rather than persisting a clock value that would skip real sequences.
            ensure!(
                decimal_cmp(&next, &ceiling).is_le(),
                "portal cursor exceeds plausibility ceiling"
            );
            let more = v["has_more"] == true;
            if v["cursor_expired"] == true {
                ensure!(!expired, "repeated portal cursor expiration");
                expired = true;
                reset = true;
                resync = true;
                since = "0".into();
                doc = container(uri, cfg);
                doc.container.as_mut().unwrap().delta = true;
            }
            ensure!(
                !more || decimal_cmp(&next, &since).is_gt(),
                "non-advancing portal cursor"
            );
            if !probe {
                portal_names(&mut v, egress, cfg).await?;
            }
            doc = append_batch(doc, &d.id, v, "portal", cfg, None).await?.0;
            if !more {
                done = true;
                break;
            }
            if probe {
                break;
            }
            since = next.clone();
        }
        delta = true;
        if done && resync && !probe {
            let (status, mut full) = json(
                egress,
                "https://portal.einundzwanzig.space/api/meetup-events",
                cfg,
                false,
                false,
            )
            .await?;
            if gone(status) {
                return Ok(missing_container(uri, &d.id, cfg));
            }
            portal_names(&mut full, egress, cfg).await?;
            // Take the snapshot after reaching the head: replaying old deltas over a
            // current snapshot can otherwise resurrect or cancel newer records.
            doc = append_batch(container(uri, cfg), &d.id, full, "full", cfg, None)
                .await?
                .0;
            delta = false;
            resync = false;
        }
    } else if d.id == "btcmap" {
        let original = Url::parse(uri)?;
        if original
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .is_some_and(|s| s.parse::<u64>().is_ok())
        {
            let (status, v) = json(egress, uri, cfg, false, false).await?;
            if gone(status) {
                return Ok(missing(uri, "btcmap"));
            }
            let uri = uri.to_owned();
            let cfg = cfg.clone();
            return tokio::task::spawn_blocking(move || single(&uri, "btcmap", &v, &cfg)).await?;
        }
        // Delta semantics expose updated_at and tombstones even for the initial sync.
        let ceiling = DateTime::<Utc>::from_timestamp_millis(doc.fetched_at + 5 * 60 * 1000)
            .context("invalid fetch time")?;
        let valid = previous
            .as_deref()
            .filter(|s| s.parse::<DateTime<Utc>>().is_ok_and(|at| at <= ceiling));
        reset = previous.is_some() && valid.is_none();
        let probe_since = original
            .query_pairs()
            .find(|(k, _)| k == "updated_since")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_else(|| (Utc::now() - chrono::Duration::days(1)).to_rfc3339());
        let since = if probe {
            probe_since.as_str()
        } else {
            valid.unwrap_or("1970-01-01T00:00:00Z")
        };
        doc.container.as_mut().unwrap().delta = true;
        let mut url = Url::parse("https://api.btcmap.org/v4/events")?;
        if probe {
            url.set_query(original.query());
        }
        for (key, value) in [
            ("updated_since", since),
            ("include_deleted", "true"),
            ("status", "all"),
        ] {
            if !url.query_pairs().any(|(k, _)| k == key) {
                url.query_pairs_mut().append_pair(key, value);
            }
        }
        let (status, v) = json(egress, url.as_str(), cfg, false, false).await?;
        if gone(status) {
            return Ok(missing_container(uri, &d.id, cfg));
        }
        let (parsed, latest) =
            append_batch(doc, &d.id, v, "btcmap", cfg, Some(since.to_owned())).await?;
        doc = parsed;
        next = latest
            .context("BTC Map cursor missing")?
            .parse::<DateTime<Utc>>()?
            .min(
                DateTime::<Utc>::from_timestamp_millis(doc.fetched_at)
                    .context("invalid fetch time")?,
            )
            .to_rfc3339();
        delta = true;
    } else {
        let base = super::service::keyed_url("google_calendar_api", uri)?;
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
            } else {
                let c = doc.container.as_ref().unwrap();
                url.query_pairs_mut()
                    .append_pair("timeMin", &c.horizon_start.to_rfc3339())
                    .append_pair("timeMax", &c.horizon_end.to_rfc3339());
            }
            doc.container.as_mut().unwrap().delta = delta;
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
            if gone(status) {
                return Ok(missing_container(uri, &d.id, cfg));
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
    // Quarantined rows cannot imply disappearance of previously accepted members.
    c.delta = delta || c.warnings.iter().any(|w| w == "invalid_cursor_row");
    c.cursor = Some(Checkpoint {
        source,
        adapter: d.id.clone(),
        previous,
        next,
        reset,
        resync,
    });
    Ok(doc)
}

fn gone(status: u16) -> bool {
    matches!(status, 401 | 403 | 404 | 410 | 451)
}
fn missing_container(uri: &str, source: &str, cfg: &EventConfig) -> ResourceMeta {
    let mut doc = container(uri, cfg);
    doc.source = source.into();
    doc.status = crate::model::Status::Missing;
    doc
}

fn missing(uri: &str, source: &str) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, source, 3600);
    doc.status = crate::model::Status::Missing;
    doc
}
fn single(uri: &str, adapter: &str, row: &Value, cfg: &EventConfig) -> Result<ResourceMeta> {
    let mut doc = container(uri, cfg);
    let c = doc.container.as_mut().unwrap();
    c.horizon_start = DateTime::<Utc>::MIN_UTC;
    c.horizon_end = DateTime::<Utc>::MAX_UTC;
    append(&mut doc, adapter, row, cfg)?;
    if doc.container.as_ref().unwrap().occurrences.is_empty() {
        if adapter == "btcmap" && doc.container.as_ref().unwrap().deleted_uids.is_empty() {
            return Err(crate::resolver::RetryLater {
                class: "drift",
                seconds: 900,
            }
            .into());
        }
        return Ok(missing(uri, adapter));
    }
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
    if kind == "btcmap"
        && rows(&v).is_some_and(|rows| {
            rows.iter().any(|r| {
                r.get("deleted_at").is_none_or(Value::is_null)
                    && !matches!(
                        r["status"].as_str(),
                        Some("live" | "cancelled" | "canceled" | "rejected" | "deleted")
                    )
            })
        })
    {
        crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
    }
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
        let mut operations = std::collections::BTreeMap::new();
        for row in entries {
            ensure!(std::time::Instant::now() < deadline, "cursor parse budget");
            let before = doc.container.as_ref().unwrap().occurrences.len();
            let before_deleted = doc.container.as_ref().unwrap().deleted_uids.len();
            let result: Result<()> = (|| {
                if kind == "portal" {
                    if row["resource"] != "meetup-event" {
                        return Ok(());
                    }
                    if row["action"] == "deleted" {
                        let id = uid(row)
                            .context("portal tombstone omitted identity")
                            .map_err(row_error)?;
                        if id.len() > 256 {
                            return Err(row_error("tombstone identity cap"));
                        }
                        operations.insert(id.clone(), false);
                        doc.container.as_mut().unwrap().deleted_uids.push(id);
                    } else {
                        let id = uid(&row["data"])
                            .context("portal row identity")
                            .map_err(row_error)?;
                        append(&mut doc, &adapter, &row["data"], &cfg)?;
                        operations.insert(id, true);
                    }
                } else {
                    if kind == "btcmap" {
                        let at = timestamp(&row["updated_at"])
                            .context("BTC Map delta omitted updated_at")
                            .map_err(row_error)?;
                        latest = Some(latest.map_or(at, |old| old.max(at)));
                    }
                    append(&mut doc, &adapter, row, &cfg)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                if !error.is::<RowFailure>() {
                    return Err(error);
                }
                let c = doc.container.as_mut().unwrap();
                c.occurrences.truncate(before);
                c.deleted_uids.truncate(before_deleted);
                if !c.warnings.iter().any(|w| w == "invalid_cursor_row") {
                    c.warnings.push("invalid_cursor_row".into());
                }
                crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
            }
            let c = doc.container.as_ref().unwrap();
            if c.occurrences.len() > before {
                document_bytes = document_bytes
                    .saturating_add(serde_json::to_vec(&c.occurrences.last().unwrap().doc)?.len());
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
    let mut place = ResourceMeta::new(&uri, "einundzwanzig", 3600);
    place.set_type("Place");
    place.location = Some(point);
    place.location_source = Some(crate::model::LocationSource::Page);
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

fn valid_sequence(s: &str) -> bool {
    !s.is_empty() && s.len() <= 4096 && s.bytes().all(|b| b.is_ascii_digit())
}
fn decimal_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}
async fn portal_names(v: &mut Value, egress: &Egress, cfg: &EventConfig) -> Result<()> {
    let entries = v
        .as_array()
        .or_else(|| rows(v))
        .or_else(|| v["changes"].as_array());
    if !entries.is_some_and(|rows| {
        rows.iter().any(|r| {
            let r = r.get("data").unwrap_or(r);
            r["title"].is_null() && r.get("meetup_id").is_some()
        })
    }) {
        return Ok(());
    }
    let Ok((_, names)) = json(
        egress,
        "https://portal.einundzwanzig.space/api/meetups",
        cfg,
        false,
        true,
    )
    .await
    else {
        return Ok(());
    };
    let Some(names) = names
        .as_array()
        .or_else(|| names["meetups"].as_array())
        .or_else(|| names["data"].as_array())
    else {
        return Ok(());
    };
    let names: std::collections::BTreeMap<_, _> = names
        .iter()
        .filter_map(|r| Some((uid(r)?, r["name"].as_str()?.to_owned())))
        .collect();
    let entries = if v.is_array() {
        v.as_array_mut()
    } else {
        ["data", "events", "meetup_events", "items", "changes"]
            .into_iter()
            .find(|k| v[*k].is_array())
            .and_then(|k| v[k].as_array_mut())
    };
    for r in entries.into_iter().flatten() {
        let r = if r.get("data").is_some() {
            &mut r["data"]
        } else {
            r
        };
        if r["title"].is_null() {
            let id = r["meetup_id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| r["meetup_id"].as_i64().map(|v| v.to_string()));
            if let Some(name) = id.and_then(|id| names.get(&id)) {
                r["title"] = Value::String(name.clone());
            }
        }
    }
    Ok(())
}
