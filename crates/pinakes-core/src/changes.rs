use crate::model::{ResourceMeta, Status};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::BTreeSet;

pub const CLASSES: &[&str] = &[
    "created",
    "rescheduled",
    "relocated",
    "cancelled",
    "uncancelled",
    "renamed",
    "details",
    "deleted",
    "aliased",
];
/// Fetch clocks and body length are observations, not changes to the resource's facts.
pub fn normalize(doc: &ResourceMeta) -> Value {
    let mut value = serde_json::to_value(doc).expect("typed document");
    let object = value.as_object_mut().unwrap();
    object.remove("fetched_at");
    object.remove("ttl_secs");
    object.remove("warnings");
    if let Some(attrs) = object.get_mut("attributes").and_then(Value::as_object_mut) {
        attrs.remove("content_length");
    }
    if let Some(aliases) = object.get_mut("aliases").and_then(Value::as_array_mut) {
        aliases.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        aliases.dedup();
    }
    value
}
#[derive(Debug, PartialEq, Eq)]
pub struct Diff {
    pub class: &'static str,
    pub fields: Vec<String>,
}
fn cancelled(v: &Value) -> bool {
    v["status"] == "missing" || v["event_status"] == "cancelled"
}
pub fn diff(before: Option<&Value>, after: Option<&Value>) -> Option<Diff> {
    let keys = before
        .into_iter()
        .chain(after)
        .filter_map(Value::as_object)
        .flat_map(|v| v.keys().cloned())
        .collect::<BTreeSet<_>>();
    let fields = keys
        .into_iter()
        .filter(|k| before.and_then(|v| v.get(k)) != after.and_then(|v| v.get(k)))
        .collect::<Vec<_>>();
    if fields.is_empty() {
        return None;
    }
    let class = match (before, after) {
        (None, _) => "created",
        (_, None) => "deleted",
        (Some(before), Some(after)) if !cancelled(before) && cancelled(after) => "cancelled",
        (Some(before), Some(after)) if cancelled(before) && !cancelled(after) => "uncancelled",
        _ if fields.iter().any(|k| matches!(k.as_str(), "start" | "end")) => "rescheduled",
        _ if fields
            .iter()
            .any(|k| matches!(k.as_str(), "location" | "venue" | "venue_id")) =>
        {
            "relocated"
        }
        _ if fields.iter().any(|k| k == "name") => "renamed",
        _ => "details",
    };
    Some(Diff { class, fields })
}
async fn lock_sequence(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    // Commit order must equal sequence order, or a cursor could skip an uncommitted lower seq.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::CHANGE_SEQUENCE)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn snapshot(
    tx: &mut Transaction<'_, Postgres>,
    osm_type: &str,
    osm_id: i64,
    value: &Value,
) -> Result<Value> {
    let version: String = sqlx::query_scalar("SELECT md5($1::jsonb::text)")
        .bind(value)
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query("UPDATE osm_place SET versions=versions || jsonb_build_object($3::text,$4::jsonb) WHERE osm_type=$1 AND osm_id=$2")
        .bind(osm_type).bind(osm_id).bind(&version).bind(value).execute(&mut **tx).await?;
    Ok(json!({"osm_id":format!("{osm_type}/{osm_id}"),"version":version}))
}
pub(crate) async fn append(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    source: &str,
    before: Option<Value>,
    after: Option<Value>,
    diff: Diff,
    occurrence: Option<&str>,
) -> Result<()> {
    let osm = before
        .as_ref()
        .or(after.as_ref())
        .and_then(|v| v["osm_id"].as_str())
        .and_then(|v| {
            crate::resolver::osm::parse_id(&format!("https://www.openstreetmap.org/{v}"))
        });
    let (before, after) = if source == "osm" {
        let osm = osm.as_ref().context("OSM change missing identity")?;
        let before = match before {
            Some(v) => Some(snapshot(tx, &osm.osm_type, osm.id, &v).await?),
            None => None,
        };
        let after = match after {
            Some(v) => Some(snapshot(tx, &osm.osm_type, osm.id, &v).await?),
            None => None,
        };
        (before, after)
    } else {
        (before, after)
    };
    lock_sequence(tx).await?;
    sqlx::query("INSERT INTO resource_change(resource_id,class,fields,before,after,source,osm_type,osm_id,at,occurrence_key) VALUES($1,$2,$3,$4,$5,$6,$7,$8,clock_timestamp(),$9)")
        .bind(id).bind(diff.class).bind(diff.fields).bind(before).bind(after).bind(source).bind(osm.as_ref().map(|o|&o.osm_type)).bind(osm.as_ref().map(|o|o.id)).bind(occurrence).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO source_state(source,change_totals) VALUES($1,jsonb_build_object($2::text,1)) ON CONFLICT(source) DO UPDATE SET change_totals=source_state.change_totals || jsonb_build_object($2::text,coalesce((source_state.change_totals->>$2)::bigint,0)+1)")
        .bind(source).bind(diff.class).execute(&mut **tx).await?;
    Ok(())
}
pub async fn save(
    pool: &PgPool,
    id: &str,
    generation: i64,
    doc: &ResourceMeta,
    cfg: &crate::config::QueueConfig,
) -> Result<()> {
    let bounded = crate::event::bound_instants(doc);
    let doc = bounded.as_ref().unwrap_or(doc);
    let mut prepared = if let Some(container) = &doc.container {
        Some(
            crate::event::storage::prepare(pool, id, container, doc.fetched_at, doc.ttl_secs)
                .await?,
        )
    } else {
        None
    };
    let adapter_source = crate::event::adapters::definition(&doc.source).map(|d| d.id.as_str());
    if let (Some(plan), Some(source)) = (&mut prepared, adapter_source) {
        plan.source = source.into();
    }
    // A post whose author opted out of indexing keeps no text in its history: what was kept
    // before goes now, and its lines carry only their kind, fields and time.
    let opted_out = doc.attributes.get("opted_out").is_some_and(|v| v == "true");
    let mut tx = pool.begin().await?;
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
    if opted_out {
        // It also clears its aliases' rows, after its own.
        crate::alias::lock(&mut tx).await?;
    }
    let row=sqlx::query("SELECT r.*,CASE WHEN r.osm_id IS NULL THEN r.doc ELSE o.versions->r.osm_version END AS accepted FROM resource r LEFT JOIN osm_place o ON r.osm_type=o.osm_type AND r.osm_id=o.osm_id WHERE r.id=$1 AND r.generation=$2 FOR UPDATE OF r")
        .bind(id).bind(generation).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Ok(()) };
    let source = adapter_source
        .map(str::to_owned)
        .or(row.try_get::<Option<String>, _>("resolver")?)
        .unwrap_or_else(|| doc.source.clone());
    if matches!(
        doc.status,
        Status::Unreachable | Status::Error | Status::Pending
    ) {
        let attempt = row.try_get::<i32, _>("attempts")? as u32 + 1;
        sqlx::query("UPDATE resource SET status=$2,lease_until=NULL,priority_at=NULL,attempts=attempts+1,timeout_streak=0,dead_letter=$4,last_error='http_4xx',next_attempt_at=now()+$3*interval '1 second' WHERE id=$1")
            .bind(id).bind(doc.status.as_str()).bind(crate::queue::backoff(cfg,attempt) as f64).bind(attempt>=cfg.max_failures).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let accepted: Option<Value> = row.try_get("accepted")?;
    // A never-observed object can be missing without having been created or cancelled.
    let before = accepted.filter(|v| v.get("type").is_some() && row.get::<bool, _>("ever_existed"));
    // Both drift guards read the stored row's type path, so a page cannot shed `Event` in the
    // same refresh to slip past either one.
    let stored_event = row
        .try_get::<Option<Vec<String>>, _>("type_path")?
        .is_some_and(|path| path.iter().any(|t| t == "Event"));
    if doc.status == Status::Ok
        && doc.start.is_none()
        && before.as_ref().is_some_and(|old| old["start"].is_string())
        && stored_event
    {
        crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
        return Err(crate::resolver::RetryLater {
            class: "drift",
            seconds: 3600,
        }
        .into());
    }
    // The transient container is serde-skipped: copy only the small parent document,
    // without cloning thousands of already-prepared occurrence documents.
    let mut doc: ResourceMeta = serde_json::from_value(serde_json::to_value(doc)?)?;
    if let Some(plan) = prepared.as_ref().filter(|p| !p.warnings.is_empty()) {
        doc.event.warnings.extend(plan.warnings.iter().cloned());
        doc.event.warnings.sort();
        doc.event.warnings.dedup();
        doc.event.warnings.truncate(16);
    }
    let social_missing = doc.status == Status::Missing
        && before.as_ref().is_some_and(|v| {
            serde_json::from_value::<ResourceMeta>(v.clone())
                .ok()
                .as_ref()
                .is_some_and(crate::resolver::cards::social)
        });
    if social_missing || opted_out {
        sqlx::query("DELETE FROM resource_change WHERE resource_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if opted_out {
        // Spellings saved before they became aliases of this post kept its text too: their
        // lines lose their documents (the `aliased` line keeps its root) and their stored cards
        // their text.
        sqlx::query("UPDATE resource_change SET before=NULL,after=NULL WHERE class<>'aliased' AND resource_id IN (SELECT alias_id FROM resource_alias WHERE root_id=$1) AND (before IS NOT NULL OR after IS NOT NULL)")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE resource SET name=NULL,doc=doc||jsonb_build_object('name',NULL,'description',NULL,'image',NULL,'attributes','{}'::jsonb) WHERE doc IS NOT NULL AND id IN (SELECT alias_id FROM resource_alias WHERE root_id=$1)")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if doc.status == Status::Missing {
        let fetched = chrono::DateTime::from_timestamp_millis(doc.fetched_at)
            .context("invalid fetch time")?;
        let last: Option<chrono::DateTime<chrono::Utc>> = row.try_get("last_missing_at")?;
        let streak: i32 = row.try_get("missing_streak")?;
        let streak = if crate::resolver::cards::invalid_identifier(&doc.url) {
            2
        } else if last.is_some_and(|last| last >= fetched) {
            streak
        } else {
            streak + 1
        };
        sqlx::query("UPDATE resource SET missing_streak=$2,last_missing_at=greatest(last_missing_at,$3) WHERE id=$1")
            .bind(id)
            .bind(streak)
            .bind(fetched)
            .execute(&mut *tx)
            .await?;
        if streak < 2 {
            // Record the empty snapshot on this first parent observation too, so the
            // next observation confirms both the parent and its mutable occurrences.
            if let Some(plan) = &prepared {
                if crate::event::storage::apply(&mut tx, id, plan).await? == Some(true) {
                    if plan.exceeds_limit(0) {
                        plan.append_summary(&mut tx, id, None).await?;
                    } else {
                        plan.append_changes(&mut tx, id).await?;
                    }
                }
            }
            let retry = cfg.missing_ttl_secs.max(3600) as f64;
            sqlx::query("UPDATE resource SET lease_until=NULL,priority_at=NULL,attempts=0,timeout_streak=0,last_error=NULL,refresh_at=now()+$2*interval '1 second',next_attempt_at=now()+$2*interval '1 second' WHERE id=$1").bind(id).bind(retry).execute(&mut *tx).await?;
            if source == "osm" {
                sqlx::query("UPDATE osm_place SET refresh_at=least(refresh_at,now()+$3*interval '1 second') WHERE osm_type=$1 AND osm_id=$2").bind(row.try_get::<Option<String>,_>("osm_type")?).bind(row.try_get::<Option<i64>,_>("osm_id")?).bind(retry).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            return Ok(());
        }
        if let Some(before) = &before {
            let mut retained = before.clone();
            retained["fetched_at"] = json!(doc.fetched_at);
            retained["ttl_secs"] = json!(doc.ttl_secs);
            retained["status"] = json!("missing");
            if social_missing {
                for field in ["name", "description", "image"] {
                    retained.as_object_mut().unwrap().remove(field);
                }
            }
            doc = serde_json::from_value(retained)?;
        }
    } else {
        sqlx::query("UPDATE resource SET missing_streak=0,last_missing_at=NULL,timeout_streak=0 WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    doc.container = None;
    if let Some(plan) = &prepared {
        let applied = crate::event::storage::apply(&mut tx, id, plan).await?;
        if applied != Some(true) {
            sqlx::query("UPDATE resource SET lease_until=NULL,priority_at=NULL,dead_letter=coalesce($3,dead_letter),next_attempt_at=now()+$2*interval '1 second' WHERE id=$1").bind(id).bind(if applied==Some(false){plan.retry_secs as f64}else{doc.ttl_secs.max(3600) as f64}).bind((applied==Some(false)).then_some(plan.dead_letter)).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(());
        }
    }
    if let Some(old) = before
        .as_ref()
        .and_then(|v| serde_json::from_value::<ResourceMeta>(v.clone()).ok())
    {
        let had_start = doc.start.is_some();
        crate::event::storage::preserve_venue(&mut doc, &old);
        // It may re-derive the times under the stored zone, after `bound_instants`.
        crate::event::clear_instants(&mut doc);
        // Agree with the drift guard above, and with the occurrence path, which keeps its stored
        // row: a start this late pass drops never replaces a storable stored start.
        if had_start
            && doc.start.is_none()
            && old.start.and_then(crate::event::time::bounded).is_some()
            && stored_event
        {
            crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
            return Err(crate::resolver::RetryLater {
                class: "drift",
                seconds: 3600,
            }
            .into());
        }
    }
    let before = before
        .map(|v| serde_json::from_value::<ResourceMeta>(v).map(|d| normalize(&d)))
        .transpose()?;
    let after = normalize(&doc);
    crate::store::write_document(&mut tx, id, generation, &doc).await?;
    let diff = diff(before.as_ref(), Some(&after))
        .filter(|_| !social_missing && (before.is_some() || doc.status != Status::Missing));
    if let Some(plan) = prepared
        .as_ref()
        .filter(|p| p.exceeds_limit(usize::from(diff.is_some())))
    {
        plan.append_summary(&mut tx, id, diff.as_ref()).await?;
    } else {
        if let Some(plan) = prepared {
            plan.append_changes(&mut tx, id).await?;
        }
        if let Some(diff) = diff {
            let (before, after) = if opted_out {
                (None, None)
            } else {
                (before, Some(after))
            };
            append(&mut tx, id, &source, before, after, diff, None).await?;
        }
    }
    tx.commit().await?;
    // Grouping is derived work. Its failure must never roll back accepted facts or charge the resolver.
    crate::groups::reconcile(pool, id, &cfg.groups).await;
    Ok(())
}
/// Delete resources orphaned for `grace_days`. A derived child's grace counts from its link's
/// expiry, not from its creation: a lapsed link (a calendar that failed for a day, an event link
/// past its cap) keeps its child, and its row for a later revival, through the same grace.
pub async fn delete_orphans(pool: &PgPool, grace_days: u32) -> Result<u64> {
    let mut tx = pool.begin().await?;
    // Orphans are many resources' rows, and deleting them reaches their references, aliases and
    // links.
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Exclusive).await?;
    let rows=sqlx::query("SELECT r.id,r.resolver,r.ever_existed,d.doc FROM resource r JOIN resource_document d ON d.id=r.id WHERE r.orphaned_at<now()-$1*interval '1 day' AND NOT EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id) AND NOT EXISTS(SELECT 1 FROM resource_derivation d JOIN resource p ON p.id=d.parent_id WHERE (d.child_id=r.id OR d.child_id IN(SELECT alias_id FROM resource_alias WHERE root_id=r.id)) AND (d.expires_at IS NULL OR d.expires_at>now()-$1*interval '1 day') AND (p.orphaned_at IS NULL OR p.orphaned_at>=now()-$1*interval '1 day' OR EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=p.id))) AND NOT EXISTS(SELECT 1 FROM resource v WHERE v.venue_id=r.id) AND NOT EXISTS(SELECT 1 FROM occurrence o WHERE o.venue_id=r.id) AND (r.lease_until IS NULL OR r.lease_until<now()) FOR UPDATE OF r SKIP LOCKED LIMIT 1000")
        .bind(grace_days as f64).fetch_all(&mut *tx).await?;
    for row in &rows {
        let id: String = row.try_get("id")?;
        let source: Option<String> = row.try_get("resolver")?;
        let doc: Value = row.try_get("doc")?;
        if let Some(doc) = serde_json::from_value::<ResourceMeta>(doc)
            .ok()
            .filter(|_| row.get::<bool, _>("ever_existed"))
        {
            let before = normalize(&doc);
            let change = diff(Some(&before), None).unwrap();
            append(
                &mut tx,
                &id,
                source.as_deref().unwrap_or("unknown"),
                Some(before),
                None,
                change,
                None,
            )
            .await?;
        }
        sqlx::query("DELETE FROM resource WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(rows.len() as u64)
}
pub async fn retain(pool: &PgPool, days: u32) -> Result<()> {
    // The change history is kept to its days even when an occurrence step fails.
    let occurrences = retain_occurrences(pool).await;
    let mut tx = pool.begin().await?;
    lock_sequence(&mut tx).await?;
    sqlx::query("DELETE FROM resource_change WHERE at<now()-$1*interval '1 day'")
        .bind(days as f64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // Preserve any historical version still referenced by a retained change or accepted resource.
    // Lock candidate rows before inspecting references so a concurrent append cannot lose its snapshot.
    let mut tx = pool.begin().await?;
    let candidates = sqlx::query("SELECT osm_type,osm_id,doc,fetch_doc FROM osm_place WHERE (SELECT count(*) FROM jsonb_object_keys(versions))>1 FOR UPDATE SKIP LOCKED").fetch_all(&mut *tx).await?;
    for row in candidates {
        let kind: String = row.get("osm_type");
        let id: i64 = row.get("osm_id");
        let current = normalize(&serde_json::from_value(row.get("doc"))?);
        let fetched = row
            .get::<Option<Value>, _>("fetch_doc")
            .map(|value| serde_json::from_value(value).map(|doc| normalize(&doc)))
            .transpose()?;
        sqlx::query("WITH kept AS (SELECT coalesce(jsonb_object_agg(v.key,v.value),'{}'::jsonb) AS versions FROM osm_place o CROSS JOIN LATERAL jsonb_each(o.versions) v WHERE o.osm_type=$1 AND o.osm_id=$2 AND (v.key=md5($3::jsonb::text) OR v.key=md5($4::jsonb::text) OR EXISTS(SELECT 1 FROM resource r WHERE r.osm_type=o.osm_type AND r.osm_id=o.osm_id AND r.osm_version=v.key) OR EXISTS(SELECT 1 FROM resource r WHERE r.doc#>>'{venue,osm_id}'=o.osm_type||'/'||o.osm_id AND r.doc#>>'{venue,osm_version}'=v.key) OR EXISTS(SELECT 1 FROM occurrence x WHERE x.doc#>>'{venue,osm_id}'=o.osm_type||'/'||o.osm_id AND x.doc#>>'{venue,osm_version}'=v.key) OR EXISTS(SELECT 1 FROM resource_change c WHERE (c.before#>>'{venue,osm_id}'=o.osm_type||'/'||o.osm_id AND c.before#>>'{venue,osm_version}'=v.key) OR (c.after#>>'{venue,osm_id}'=o.osm_type||'/'||o.osm_id AND c.after#>>'{venue,osm_version}'=v.key)) OR EXISTS(SELECT 1 FROM resource_change c WHERE c.osm_type=o.osm_type AND c.osm_id=o.osm_id AND (c.before->>'version'=v.key OR c.after->>'version'=v.key)))) UPDATE osm_place o SET versions=k.versions FROM kept k WHERE o.osm_type=$1 AND o.osm_id=$2 AND o.versions IS DISTINCT FROM k.versions").bind(kind).bind(id).bind(current).bind(fetched).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    occurrences.map(|_| ())
}
/// Delete expired occurrences one resource at a time, in a save's order: the write order shared,
/// the resource's row, then its occurrences. Their triggers lock the owning resource's row, so
/// one statement over every resource would lock many resources' rows in an order of its own.
async fn retain_occurrences(pool: &PgPool) -> Result<u64> {
    let owners: Vec<String> = sqlx::query_scalar("SELECT DISTINCT o.resource_id FROM occurrence o JOIN event_fetch e ON e.resource_id=o.resource_id WHERE (o.status IN ('cancelled','superseded') OR o.start_at<e.horizon_start) AND coalesce(o.end_at,o.start_at)<now()-e.retention_days*interval '1 day' ORDER BY 1")
        .fetch_all(pool)
        .await?;
    let mut deleted = 0;
    for id in owners {
        let mut tx = pool.begin().await?;
        crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
        let found = sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR UPDATE")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
        if found.is_none() {
            continue;
        }
        deleted += sqlx::query("DELETE FROM occurrence o USING event_fetch e WHERE o.resource_id=$1 AND e.resource_id=$1 AND (o.status IN ('cancelled','superseded') OR o.start_at<e.horizon_start) AND coalesce(o.end_at,o.start_at)<now()-e.retention_days*interval '1 day'")
            .bind(&id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
    }
    Ok(deleted)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalized_diff_classes_and_fetch_noise() {
        let mut before = ResourceMeta::new("https://example.org", "web", 10);
        before.name = Some("One".into());
        let mut after = before.clone();
        after.fetched_at += 1;
        after.ttl_secs = 99;
        after
            .attributes
            .insert("content_length".into(), "999".into());
        assert!(diff(Some(&normalize(&before)), Some(&normalize(&after))).is_none());
        let base = normalize(&before);
        for (field, value, class) in [
            ("name", json!("Two"), "renamed"),
            ("start", json!("2026-10-15T12:00:00Z"), "rescheduled"),
            ("end", json!("2026-10-15T13:00:00Z"), "rescheduled"),
            ("location", json!({"lat":1,"lon":2}), "relocated"),
            ("status", json!("missing"), "cancelled"),
            ("description", json!("Details"), "details"),
        ] {
            let mut after = base.clone();
            after[field] = value;
            assert_eq!(diff(Some(&base), Some(&after)).unwrap().class, class);
        }
        assert_eq!(diff(None, Some(&base)).unwrap().class, "created");
        assert_eq!(diff(Some(&base), None).unwrap().class, "deleted");
        let mut missing = base.clone();
        missing["status"] = json!("missing");
        assert_eq!(
            diff(Some(&missing), Some(&base)).unwrap().class,
            "uncancelled"
        );
    }
}
