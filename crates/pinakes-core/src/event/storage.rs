use super::{Container, EventStatus};
use crate::{changes, model::ResourceMeta};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};

pub struct Prepared {
    pub(crate) source: String,
    validators: Option<Value>,
    last: Option<i64>,
    fetched_at: i64,
    horizon: chrono::DateTime<chrono::Utc>,
    horizon_end: chrono::DateTime<chrono::Utc>,
    link_caps: (usize, usize),
    held: Option<Vec<String>>,
    missing: BTreeMap<String, u32>,
    holds: u32,
    rows: Vec<Value>,
    purge: Vec<Value>,
    changes: Vec<PendingChange>,
    change_count: usize,
    change_limit: usize,
    pub retry_secs: u64,
    pub dead_letter: bool,
    pub applied: bool,
    retention_days: u32,
    cursor: Option<super::adapters::cursor::Checkpoint>,
    /// Feed warnings found while building rows, added to the stored document.
    pub(crate) warnings: Vec<String>,
    delta: bool,
}
pub struct PendingChange {
    before: Option<Value>,
    after: Option<Value>,
    diff: changes::Diff,
    key: Option<String>,
}
fn patch(value: Option<&Value>, fields: &[String]) -> Option<Value> {
    value.map(|v| {
        let mut m = serde_json::Map::new();
        for k in fields.iter().map(String::as_str).chain(["type_path"]) {
            if let Some(x) = v.get(k) {
                m.insert(k.into(), x.clone());
            }
        }
        Value::Object(m)
    })
}
fn pending(before: Option<&Value>, after: &Value, key: String) -> Option<PendingChange> {
    let diff = changes::diff(before, Some(after))?;
    Some(PendingChange {
        before: patch(before, &diff.fields),
        after: patch(Some(after), &diff.fields),
        diff,
        key: Some(key),
    })
}
/// Carry a stable accepted venue over a failed enrichment, without overriding new page coordinates.
pub fn preserve_venue(doc: &mut ResourceMeta, old: &ResourceMeta) {
    if doc.event.attendance == Some(super::Attendance::Online) || doc.location.is_some() {
        return;
    }
    if let (Some(new), Some(previous)) = (&mut doc.event.venue, &old.event.venue) {
        if new.text == previous.text
            && new.address == previous.address
            && new.location.is_none()
            && new.osm_version.is_none()
            && previous.osm_version.is_some()
        {
            *new = previous.clone();
            if doc.event.tz.is_none() {
                doc.event.tz = old.event.tz.clone();
                doc.event.timezone_attribution = old.event.timezone_attribution.clone();
                super::page::finalize(doc, false);
            }
        }
    }
}
/// An occurrence's own page, when its URL names another resource than the feed.
fn page_url(doc: &ResourceMeta, id: &str, canonical_id: &str) -> Option<String> {
    crate::model::identity(&doc.url)
        .ok()
        .filter(|(target, _, _)| target != id && target != canonical_id)
        .map(|(_, url, _)| url)
}
/// Read, bound, hash and diff outside the global write/sequence locks. Apply rechecks the snapshot fence.
pub async fn prepare(
    pool: &PgPool,
    id: &str,
    container: &Container,
    fetched_at: i64,
    normal_ttl: u64,
) -> Result<Prepared> {
    let state=sqlx::query("SELECT last_snapshot_at,held_keys,missing_keys,hold_count FROM event_fetch WHERE resource_id=$1").bind(id).fetch_optional(pool).await?;
    let last = state
        .as_ref()
        .and_then(|r| r.get::<Option<i64>, _>("last_snapshot_at"));
    let held: Option<Vec<String>> = state.as_ref().and_then(|r| r.get("held_keys"));
    let mut missing: BTreeMap<String, u32> = state
        .as_ref()
        .map(|r| serde_json::from_value(r.get("missing_keys")))
        .transpose()?
        .unwrap_or_default();
    let old_holds = state
        .as_ref()
        .map_or(0, |r| r.get::<i32, _>("hold_count") as u32);
    let source_uri: String = sqlx::query_scalar("SELECT uri FROM resource WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    let canonical_id = crate::model::identity(&super::calendar::canonical_feed(&source_uri))?.0;
    let rows=sqlx::query("SELECT uid,recurrence_id,doc,status,content_hash,sequence,modified FROM occurrence WHERE resource_id=$1 ORDER BY uid,recurrence_id").bind(id).fetch_all(pool).await?;
    let now = chrono::Utc::now();
    let mutable = |d: &ResourceMeta| {
        d.start
            .is_some_and(|at| at >= container.horizon_start && at <= container.horizon_end)
            && d.end.or(d.start).is_none_or(|end| {
                end.checked_add_signed(chrono::Duration::hours(24))
                    .is_none_or(|at| now < at)
            })
    };
    let mut old = BTreeMap::new();
    let mut purge = vec![];
    for row in rows {
        let uid: String = row.get("uid");
        let rid: String = row.get("recurrence_id");
        let doc: ResourceMeta = serde_json::from_value(row.get("doc"))?;
        let status: String = row.get("status");
        if (matches!(status.as_str(), "cancelled" | "superseded")
            || doc.start.is_some_and(|at| at < container.horizon_start))
            && doc.end.or(doc.start).is_some_and(|at| {
                at.checked_add_signed(chrono::Duration::days(i64::from(
                    container.limits.retention_days,
                )))
                .is_some_and(|at| at < now)
            })
        {
            purge.push(json!({"uid":uid,"rid":rid}));
            continue;
        }
        old.insert(
            serde_json::to_string(&(&uid, &rid))?,
            (
                uid,
                rid,
                doc,
                status,
                row.get::<String, _>("content_hash"),
                row.get::<i64, _>("sequence"),
                row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("modified"),
            ),
        );
    }
    let keys: BTreeSet<_> = old
        .iter()
        .filter(|(_, (_, _, d, s, _, _, _))| {
            mutable(d) && !matches!(s.as_str(), "cancelled" | "superseded")
        })
        .map(|(k, _)| k.clone())
        .collect();
    let mut incoming: BTreeSet<_> = container
        .occurrences
        .iter()
        .filter(|o| {
            (mutable(&o.doc) || !old.contains_key(&o.key()))
                && (old.contains_key(&o.key())
                    || o.doc.start.is_some_and(|at| {
                        at >= container.horizon_start && at <= container.horizon_end
                    }))
        })
        .map(|o| o.key())
        .collect();
    let fetched_keys = incoming.clone();
    let deleted: BTreeSet<_> = container.deleted_uids.iter().collect();
    if container.delta {
        incoming.extend(keys.iter().cloned());
    }
    incoming.retain(|key| {
        !old.get(key)
            .is_some_and(|(uid, _, _, _, _, _, _)| deleted.contains(uid))
    });
    ensure!(
        container.occurrences.len() <= container.limits.max_rows,
        "incoming occurrence cap"
    );
    let fresh: Vec<_> = incoming.difference(&keys).cloned().collect();
    let losses: Vec<_> = keys.difference(&incoming).cloned().collect();
    let repeated = held
        .as_ref()
        .is_some_and(|h| h.iter().cloned().collect::<BTreeSet<_>>() == incoming);
    let row_growth = incoming.iter().filter(|k| !old.contains_key(*k)).count();
    let excessive = old.len() + row_growth > container.limits.max_rows
        || (!repeated && !old.is_empty() && fresh.len() > container.limits.max_new_keys);
    let hold = excessive || (losses.len() * 2 > keys.len() && !repeated);
    let holds = if hold { old_holds.saturating_add(1) } else { 0 };
    let floor = normal_ttl
        .max(container.limits.cadence_floor)
        .max(container.held_retry_secs)
        .max(3600);
    let retry = floor
        .saturating_mul(
            1u64.checked_shl(holds.saturating_sub(1))
                .unwrap_or(u64::MAX),
        )
        .min(7 * 86400)
        .max(floor);
    let mut plan = Prepared {
        source: container
            .cursor
            .as_ref()
            .map_or_else(|| "event".to_owned(), |c| c.adapter.clone()),
        validators: container
            .validators
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?,
        retention_days: container.limits.retention_days,
        cursor: container.cursor.clone(),
        warnings: vec![],
        delta: container.delta,
        last,
        fetched_at,
        horizon: container.horizon_start,
        horizon_end: container.horizon_end,
        link_caps: container.link_caps,
        held: None,
        missing: BTreeMap::new(),
        holds,
        rows: vec![],
        purge,
        changes: vec![],
        change_count: 0,
        change_limit: container.limits.max_changes,
        retry_secs: retry,
        dead_letter: holds >= container.limits.max_holds,
        applied: !hold && last.is_none_or(|at| fetched_at > at),
    };
    if !plan.applied {
        plan.held = if hold {
            Some(incoming.into_iter().collect())
        } else {
            held
        };
        plan.missing = missing;
        return Ok(plan);
    }
    missing.retain(|k, _| keys.contains(k) && !incoming.contains(k));
    let mut cancelled: BTreeSet<_> = old
        .iter()
        .filter(|(key, (uid, _, doc, _, _, _, _))| {
            deleted.contains(uid) && mutable(doc) && !fetched_keys.contains(*key)
        })
        .map(|(k, _)| k.clone())
        .collect();
    for k in losses.into_iter().filter(|_| !container.delta) {
        let count = missing.entry(k.clone()).or_default();
        *count += if repeated { 2 } else { 1 };
        if *count >= 2 {
            cancelled.insert(k);
        }
    }
    let mut applied = incoming.clone();
    for k in missing.keys() {
        if !cancelled.contains(k) {
            applied.insert(k.clone());
        }
    }
    missing.retain(|k, _| !cancelled.contains(k));
    plan.missing = missing;
    let mut hashes: BTreeMap<_, Vec<String>> = BTreeMap::new();
    for (key, (_, _, d, status, hash, _, _)) in &old {
        if !incoming.contains(key) && status != "superseded" && mutable(d) {
            hashes
                .entry((hash.clone(), d.start, d.name.clone()))
                .or_default()
                .push(key.clone());
        }
    }
    let mut superseded = BTreeSet::new();
    for item in &container.occurrences {
        let key = item.key();
        let previous = old.get(&key);
        if previous.is_none()
            && item
                .doc
                .start
                .is_some_and(|at| at < container.horizon_start || at > container.horizon_end)
        {
            continue;
        }
        if previous.is_some_and(|(_, _, d, _, _, seq, modified)| {
            (!container.delta && !mutable(d))
                || *seq > item.sequence
                || *seq == item.sequence && modified.zip(item.modified).is_some_and(|(a, b)| a > b)
        }) {
            continue;
        }
        // Nothing stored names a credential but the feed's own link (`capability::strip`).
        let mut value = serde_json::to_value(&item.doc)?;
        let mut doc = if crate::capability::strip::<ResourceMeta>(&mut value, &source_uri) {
            serde_json::from_value(value)?
        } else {
            item.doc.clone()
        };
        let mut modified = item.modified;
        if let Some((_, _, old, _, _, _, _)) = previous {
            preserve_venue(&mut doc, old);
        }
        // The last step before `apply` casts these instants from text: `preserve_venue` may have
        // re-derived the times under the stored zone, so bound them again whatever ran before
        // (`bound_instants` is the early pass). An out-of-range start keeps the stored row (in a
        // non-delta feed it is then cancelled after two snapshots), with the feed's warning.
        if !super::bound_item(&mut doc, &mut modified) {
            if !plan
                .warnings
                .iter()
                .any(|w| w == "occurrence_start_out_of_range")
            {
                plan.warnings.push("occurrence_start_out_of_range".into());
            }
            continue;
        }
        let mut after = changes::normalize(&doc);
        let before = previous.map(|(_, _, d, _, _, _, _)| changes::normalize(d));
        let supersedes = if previous.is_none() {
            hashes
                .get_mut(&(item.content_hash.clone(), doc.start, doc.name.clone()))
                .and_then(Vec::pop)
        } else {
            None
        };
        if let Some(key) = &supersedes {
            superseded.insert(key.clone());
            cancelled.remove(key);
            after["supersedes"] = json!(key);
        }
        if let Some(change) = pending(before.as_ref(), &after, key.clone()) {
            plan.push_change(change, container.limits.max_changes);
        }
        let url = page_url(&doc, id, &canonical_id);
        plan.rows.push(json!({"uid":item.uid,"rid":item.recurrence_id,"start":doc.start,"end":doc.end,"name":doc.name,"venue_id":doc.event.venue.as_ref().and_then(|v|v.resource_id.as_deref()),"status":doc.event.event_status.as_ref().unwrap_or(&EventStatus::Scheduled).as_str(),"url":url,"doc":doc,"lat":doc.location.as_ref().map(|p|p.lat),"lon":doc.location.as_ref().map(|p|p.lon),"sequence":item.sequence,"modified":modified,"hash":item.content_hash}));
    }
    for key in cancelled.iter().chain(superseded.iter()) {
        let Some((uid, rid, old, _, hash, seq, modified)) = old.get(key) else {
            continue;
        };
        if !mutable(old) {
            continue;
        }
        let mut doc = old.clone();
        let status = if superseded.contains(key) {
            "superseded"
        } else {
            doc.event.event_status = Some(EventStatus::Cancelled);
            "cancelled"
        };
        if status == "cancelled" {
            if let Some(c) = pending(
                Some(&changes::normalize(old)),
                &changes::normalize(&doc),
                key.clone(),
            ) {
                plan.push_change(c, container.limits.max_changes);
            }
        }
        plan.missing.remove(key);
        let url = page_url(&doc, id, &canonical_id);
        plan.rows.push(json!({"uid":uid,"rid":rid,"start":doc.start,"end":doc.end,"name":doc.name,"venue_id":doc.event.venue.as_ref().and_then(|v|v.resource_id.as_deref()),"status":status,"url":url,"doc":doc,"lat":doc.location.as_ref().map(|p|p.lat),"lon":doc.location.as_ref().map(|p|p.lon),"sequence":seq,"modified":modified,"hash":hash}));
    }
    Ok(plan)
}
pub async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    plan: &Prepared,
) -> Result<Option<bool>> {
    sqlx::query("INSERT INTO event_fetch(resource_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    let current: Option<i64> = sqlx::query_scalar(
        "SELECT last_snapshot_at FROM event_fetch WHERE resource_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    if current != plan.last || current.is_some_and(|at| plan.fetched_at <= at) {
        return Ok(None);
    }
    if let Some(cursor) = &plan.cursor {
        if !cursor.check(tx, id).await? {
            return Ok(None);
        }
    }
    sqlx::query("UPDATE event_fetch SET last_snapshot_at=$2,held_keys=$3,missing_keys=$4,hold_count=$5,horizon_start=$6,retention_days=$7,validators=coalesce($8,validators),horizon_end=$9 WHERE resource_id=$1").bind(id).bind(plan.fetched_at).bind(&plan.held).bind(json!(plan.missing)).bind(plan.holds as i32).bind(plan.horizon).bind(plan.retention_days as i32).bind(&plan.validators).bind(plan.horizon_end).execute(&mut **tx).await?;

    if !plan.applied {
        sqlx::query("INSERT INTO source_state(source,feed_held_total) VALUES($1,1) ON CONFLICT(source) DO UPDATE SET feed_held_total=source_state.feed_held_total+1").bind(&plan.source).execute(&mut **tx).await?;
        return Ok(Some(false));
    }
    for chunk in plan.purge.chunks(256) {
        sqlx::query("DELETE FROM occurrence WHERE resource_id=$1 AND (uid,recurrence_id) IN(SELECT j->>'uid',j->>'rid' FROM unnest($2::jsonb[]) j)").bind(id).bind(chunk).execute(&mut **tx).await?;
    }
    // Bound the encoded jsonb[] buffer independently of the retained plan.
    for chunk in plan.rows.chunks(256) {
        sqlx::query("INSERT INTO occurrence(resource_id,uid,recurrence_id,start_at,end_at,name,venue_id,status,url,doc,point,sequence,modified,content_hash) SELECT $1,j->>'uid',j->>'rid',(j->>'start')::timestamptz,(j->>'end')::timestamptz,j->>'name',j->>'venue_id',j->>'status',j->>'url',j->'doc',CASE WHEN j->>'lat' IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint((j->>'lon')::float8,(j->>'lat')::float8),4326) END,(j->>'sequence')::bigint,(j->>'modified')::timestamptz,j->>'hash' FROM unnest($2::jsonb[]) j ON CONFLICT(resource_id,uid,recurrence_id) DO UPDATE SET start_at=excluded.start_at,end_at=excluded.end_at,name=excluded.name,venue_id=excluded.venue_id,status=excluded.status,url=excluded.url,doc=excluded.doc,point=excluded.point,sequence=excluded.sequence,modified=excluded.modified,content_hash=excluded.content_hash").bind(id).bind(chunk).execute(&mut **tx).await?;
    }
    if let Some(cursor) = &plan.cursor {
        cursor.commit(tx, id).await?;
    }
    if plan.delta {
        // A delta feed may never resend a far event, so its links follow the horizon here.
        sqlx::query(crate::store::LOCK_PARENT_ADAPTER)
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query(crate::store::EXTEND_PARENT_DERIVATIONS)
            .bind(id)
            .bind(plan.horizon_end)
            .bind(plan.link_caps.0 as i64)
            .bind(plan.link_caps.1 as i64)
            .execute(&mut **tx)
            .await?;
    }
    Ok(Some(true))
}
impl Prepared {
    pub fn exceeds_limit(&self, additional: usize) -> bool {
        self.change_count.saturating_add(additional) > self.change_limit
    }
    pub async fn append_summary(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: &str,
        parent: Option<&changes::Diff>,
    ) -> Result<()> {
        changes::append(
            tx,
            id,
            &self.source,
            None,
            Some(json!({"occurrence_changes":self.change_count,"resource_changes":usize::from(parent.is_some()),"change_count":self.change_count+usize::from(parent.is_some()),"resource_change":parent.map(|d|json!({"class":d.class,"fields":d.fields})),"type_path":["Thing","Event"]})),
            changes::Diff {
                class: "details",
                fields: if parent.is_some() { vec!["occurrence_changes".into(), "resource_change".into()] } else { vec!["occurrence_changes".into()] },
            },
            None,
        )
        .await
    }
    fn push_change(&mut self, change: PendingChange, cap: usize) {
        self.change_count += 1;
        if self.changes.len() < cap {
            self.changes.push(change);
        }
    }
    pub async fn append_changes(&self, tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<()> {
        for change in &self.changes {
            changes::append(
                tx,
                id,
                &self.source,
                change.before.clone(),
                change.after.clone(),
                changes::Diff {
                    class: change.diff.class,
                    fields: change.diff.fields.clone(),
                },
                change.key.as_deref(),
            )
            .await?;
        }
        Ok(())
    }
}
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceQuery {
    /// Inclusive start time; defaults to this feed's last accepted horizon start.
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    #[param(minimum = 1, maximum = 500)]
    pub limit: Option<usize>,
    /// Signed keyset cursor, scoped to this resource and window; invalid after restart.
    pub cursor: Option<String>,
}
#[derive(serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct OccurrenceRef {
    pub uid: String,
    pub recurrence_id: Option<String>,
}
#[derive(serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ReferenceEvent {
    pub author: String,
    pub object_path: String,
    /// The instance of a recurring event; `null` for a one-off one.
    pub recurrence_id: Option<String>,
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: Option<chrono::DateTime<chrono::Utc>>,
    pub status: String,
}
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct OccurrencePage {
    /// One card per occurrence, with `occurrence` naming the feed instance and its lifecycle.
    pub items: Vec<crate::api::Card>,
    pub cursor: Option<String>,
}
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceLookup {
    /// Feed resource IDs; with `uris`, 1–500 together. An alias reads its root, an unknown ID
    /// adds nothing.
    pub resource_ids: Option<Vec<String>>,
    /// Feed links, read as `POST /v1/resources/lookup` reads them; an unknown link adds nothing.
    pub uris: Option<Vec<String>>,
    /// Inclusive start time (RFC 3339); defaults to the earliest accepted horizon start of these
    /// feeds.
    #[schema(format = DateTime)]
    pub from: Option<String>,
    #[schema(format = DateTime)]
    pub to: Option<String>,
    /// 1–500, default 100.
    #[schema(minimum = 1, maximum = 500)]
    pub limit: Option<usize>,
    /// Signed keyset cursor, scoped to these feeds and window; invalid after restart.
    pub cursor: Option<String>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct OccurrenceCursor {
    /// The resource ID, or a hash of the looked-up IDs.
    scope: String,
    from: chrono::DateTime<chrono::Utc>,
    to: Option<chrono::DateTime<chrono::Utc>>,
    at: chrono::DateTime<chrono::Utc>,
    feed: String,
    uid: String,
    rid: String,
}
impl OccurrenceCursor {
    fn encode(&self, key: &hmac::SimpleHmac<blake3::Hasher>) -> anyhow::Result<String> {
        use base64::Engine;
        use hmac::Mac;
        let payload = serde_json::to_vec(self)?;
        let mut mac = key.clone();
        mac.update(&payload);
        let mut signed = mac.finalize().into_bytes().to_vec();
        signed.extend(payload);
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signed))
    }
    fn decode(
        s: &str,
        key: &hmac::SimpleHmac<blake3::Hasher>,
    ) -> Result<Self, crate::api::ApiError> {
        use base64::Engine;
        use hmac::Mac;
        let invalid = || crate::api::ApiError::bad("invalid occurrence cursor");
        if s.len() > 8192 {
            return Err(invalid());
        }
        let signed = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| invalid())?;
        if signed.len() <= 32 {
            return Err(invalid());
        }
        let (tag, payload) = signed.split_at(32);
        let mut mac = key.clone();
        mac.update(payload);
        mac.verify_slice(tag).map_err(|_| invalid())?;
        serde_json::from_slice(payload).map_err(|_| invalid())
    }
}
#[utoipa::path(get,path="/v1/resources/{id}/occurrences",operation_id="list_occurrences",tag="Events",params(("id"=String,Path),OccurrenceQuery),responses((status=200,body=OccurrencePage),(status=400,description="Invalid window, limit or cursor"),(status=404,description="Resource not found")),description="Calendar instances ordered by start, UID and recurrence ID. Inclusive window, default limit 100, maximum 500. Superseded keys are omitted.")]
pub async fn list(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<OccurrenceQuery>,
) -> Result<axum::Json<OccurrencePage>, crate::api::ApiError> {
    if !crate::model::valid_id(&id) {
        return Err(crate::api::ApiError::bad("invalid occurrence id or limit"));
    }
    let page = page(&state, std::slice::from_ref(&id), Some(id.clone()), q).await?;
    page.map(axum::Json).ok_or_else(|| {
        crate::api::ApiError(
            axum::http::StatusCode::NOT_FOUND,
            "resource not found".into(),
        )
    })
}
#[utoipa::path(post,path="/v1/occurrences/lookup",operation_id="lookup_occurrences",tag="Events",request_body=OccurrenceLookup,responses((status=200,body=OccurrencePage),(status=400,description="1–500 valid IDs, a valid window, limit or cursor")),description="The calendar instances of several feeds in one time-ordered page: by start, then feed, UID and recurrence ID. Each card's `id` is its feed. The window and limits are those of `GET /v1/resources/{id}/occurrences`; unknown IDs add nothing.")]
pub async fn lookup(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
    axum::Json(body): axum::Json<OccurrenceLookup>,
) -> Result<axum::Json<OccurrencePage>, crate::api::ApiError> {
    use crate::api::ApiError;
    let ids = crate::api::lookup_ids(body.resource_ids, body.uris)?;
    let time = |t: Option<String>| {
        t.map(|t| chrono::DateTime::parse_from_rfc3339(&t).map(|t| t.to_utc()))
            .transpose()
            .map_err(|_| ApiError::bad("from and to must be RFC 3339"))
    };
    let query = OccurrenceQuery {
        from: time(body.from)?,
        to: time(body.to)?,
        limit: body.limit,
        cursor: body.cursor,
    };
    let page = page(&state, &ids, None, query).await?;
    Ok(axum::Json(page.unwrap_or(OccurrencePage {
        items: vec![],
        cursor: None,
    })))
}
/// One page of the occurrences of `ids` (aliases read as their roots), ordered by start, feed,
/// UID and recurrence ID; `None` when no ID names a resource. `scope` binds the cursor; without
/// one, a hash of the resolved feeds does, so any spelling of the same feeds continues it.
async fn page(
    state: &crate::api::AppState,
    ids: &[String],
    scope: Option<String>,
    q: OccurrenceQuery,
) -> Result<Option<OccurrencePage>, crate::api::ApiError> {
    use crate::api::ApiError;
    let limit = q.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(ApiError::bad("invalid occurrence id or limit"));
    }
    if [q.from, q.to]
        .iter()
        .flatten()
        .any(|t| super::time::bounded(*t).is_none())
    {
        return Err(ApiError::bad("timestamp out of range"));
    }
    let cursor = q
        .cursor
        .as_deref()
        .map(|c| OccurrenceCursor::decode(c, &state.cursor_mac))
        .transpose()?;
    let (mut roots, horizon): (Vec<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as("SELECT coalesce(array_agg(r.id),'{}'),min(e.horizon_start) FROM resource r LEFT JOIN event_fetch e ON e.resource_id=r.id WHERE r.id IN (SELECT DISTINCT coalesce(a.root_id,i) FROM unnest($1::text[]) i LEFT JOIN resource_alias a ON a.alias_id=i)").bind(ids).fetch_one(&state.pool).await?;
    if roots.is_empty() {
        return Ok(None);
    }
    roots.sort();
    let scope =
        scope.unwrap_or_else(|| blake3::hash(roots.concat().as_bytes()).to_hex()[..32].to_owned());
    if cursor.as_ref().is_some_and(|c| {
        c.scope != scope
            || q.from.is_some_and(|v| v != c.from)
            || q.to.is_some_and(|v| Some(v) != c.to)
    }) {
        return Err(ApiError::bad(
            "occurrence cursor belongs to another resource or window",
        ));
    }
    let from = cursor
        .as_ref()
        .map(|c| c.from)
        .or(q.from)
        .or(horizon)
        .unwrap_or_else(|| {
            chrono::Utc::now()
                .checked_sub_months(chrono::Months::new(state.config.event.months_back))
                .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
        });
    let to = cursor.as_ref().and_then(|c| c.to).or(q.to);
    if to.is_some_and(|to| from > to) {
        return Err(ApiError::bad("invalid occurrence window"));
    }
    let values:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',o.resource_id,'uri',r.uri,'host',r.host,'status',r.status,'type',o.document->>'type','name',o.name,'doc',o.document,'occurrence',jsonb_build_object('uid',o.uid,'recurrence_id',nullif(o.recurrence_id,''),'status',o.status),'start_at',o.start_at,'uid',o.uid,'rid',o.recurrence_id,'group',CASE WHEN $9 THEN group_hint(md5(jsonb_build_array(o.resource_id,o.uid,o.recurrence_id)::text)) END,'also_on',also_on(md5(jsonb_build_array(o.resource_id,o.uid,o.recurrence_id)::text),md5(jsonb_build_array(o.resource_id,o.uid,o.recurrence_id)::text),8,$9),'maybe_same',CASE WHEN $10 THEN same_event_hint(md5(jsonb_build_array(o.resource_id,o.uid,o.recurrence_id)::text),ARRAY(SELECT jsonb_array_elements_text(o.document->'type_path')),3) END) FROM event_occurrence o JOIN resource r ON r.id=o.resource_id WHERE o.resource_id=ANY($1) AND o.start_at>=$2 AND ($3::timestamptz IS NULL OR o.start_at<=$3) AND ($4::timestamptz IS NULL OR (o.start_at,o.resource_id,o.uid,o.recurrence_id)>($4,$5,$6,$7)) ORDER BY o.start_at,o.resource_id,o.uid,o.recurrence_id LIMIT $8").bind(&roots).bind(from).bind(to).bind(cursor.as_ref().map(|c|c.at)).bind(cursor.as_ref().map(|c|&c.feed)).bind(cursor.as_ref().map(|c|&c.uid)).bind(cursor.as_ref().map(|c|&c.rid)).bind((limit+1) as i64).bind(state.config.groups.enabled).bind(crate::api::same_event(&state.config)).fetch_all(&state.pool).await?;
    let more = values.len() > limit;
    let values: Vec<Value> = values.into_iter().take(limit).collect();
    let cursor = if more {
        let last = values.last().expect("nonzero limit");
        Some(
            OccurrenceCursor {
                scope,
                from,
                to,
                at: serde_json::from_value(last["start_at"].clone())?,
                feed: last["id"].as_str().unwrap_or_default().into(),
                uid: last["uid"].as_str().unwrap_or_default().into(),
                rid: last["rid"].as_str().unwrap_or_default().into(),
            }
            .encode(&state.cursor_mac)?,
        )
    } else {
        None
    };
    let mut items: Vec<crate::api::Card> = values
        .iter()
        .map(crate::api::card)
        .collect::<Result<_, _>>()?;
    crate::api::with_refs(&state.pool, items.iter_mut()).await?;
    Ok(Some(OccurrencePage { items, cursor }))
}
