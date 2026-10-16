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
];
/// Fetch clocks and body length are observations, not changes to the resource's facts.
pub fn normalize(doc: &ResourceMeta) -> Value {
    let mut value = serde_json::to_value(doc).expect("typed document");
    let object = value.as_object_mut().unwrap();
    object.remove("fetched_at");
    object.remove("ttl_secs");
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
        (Some(before), Some(after))
            if before["status"] != "missing" && after["status"] == "missing" =>
        {
            "cancelled"
        }
        (Some(before), Some(after))
            if before["status"] == "missing" && after["status"] != "missing" =>
        {
            "uncancelled"
        }
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
    sqlx::query("SELECT pg_advisory_xact_lock(73822918)")
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
async fn append(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    source: &str,
    before: Option<Value>,
    after: Option<Value>,
    diff: Diff,
) -> Result<()> {
    let osm = before
        .as_ref()
        .or(after.as_ref())
        .and_then(|v| v["osm_id"].as_str())
        .and_then(|v| {
            crate::resolver::osm::parse_id(&format!("https://www.openstreetmap.org/{v}"))
        });
    let type_path = if source == "osm" {
        None
    } else {
        after
            .as_ref()
            .or(before.as_ref())
            .and_then(|v| v["type_path"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
    };
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
    sqlx::query("INSERT INTO resource_change(resource_id,class,fields,before,after,source,type_path,osm_type,osm_id,at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,clock_timestamp())")
        .bind(id).bind(diff.class).bind(diff.fields).bind(before).bind(after).bind(source).bind(type_path).bind(osm.as_ref().map(|o|&o.osm_type)).bind(osm.as_ref().map(|o|o.id)).execute(&mut **tx).await?;
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
    let mut tx = pool.begin().await?;
    lock_sequence(&mut tx).await?;
    let row=sqlx::query("SELECT r.*,CASE WHEN r.osm_id IS NULL THEN r.doc ELSE o.versions->r.osm_version END AS accepted FROM resource r LEFT JOIN osm_place o ON r.osm_type=o.osm_type AND r.osm_id=o.osm_id WHERE r.id=$1 AND r.generation=$2 FOR UPDATE OF r")
        .bind(id).bind(generation).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Ok(()) };
    let source = row
        .try_get::<Option<String>, _>("resolver")?
        .unwrap_or_else(|| doc.source.clone());
    if matches!(
        doc.status,
        Status::Unreachable | Status::Error | Status::Pending
    ) {
        let attempt = row.try_get::<i32, _>("attempts")? as u32 + 1;
        sqlx::query("UPDATE resource SET status=$2,lease_until=NULL,priority_at=NULL,attempts=attempts+1,dead_letter=$4,last_error='http_4xx',next_attempt_at=now()+$3*interval '1 second' WHERE id=$1")
            .bind(id).bind(doc.status.as_str()).bind(crate::queue::backoff(cfg,attempt) as f64).bind(attempt>=cfg.max_failures).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let accepted: Option<Value> = row.try_get("accepted")?;
    // A never-observed object can be missing without having been created or cancelled.
    let before = accepted.filter(|v| v.get("type").is_some() && row.get::<bool, _>("ever_existed"));
    let mut doc = doc.clone();
    if doc.status == Status::Missing {
        let fetched = chrono::DateTime::from_timestamp_millis(doc.fetched_at)
            .context("invalid fetch time")?;
        let last: Option<chrono::DateTime<chrono::Utc>> = row.try_get("last_missing_at")?;
        let streak: i32 = row.try_get("missing_streak")?;
        let streak = if last == Some(fetched) {
            streak
        } else {
            streak + 1
        };
        sqlx::query("UPDATE resource SET missing_streak=$2,last_missing_at=$3 WHERE id=$1")
            .bind(id)
            .bind(streak)
            .bind(fetched)
            .execute(&mut *tx)
            .await?;
        if streak < 2 {
            let retry = cfg.missing_ttl_secs.max(3600) as f64;
            sqlx::query("UPDATE resource SET lease_until=NULL,priority_at=NULL,attempts=0,last_error=NULL,refresh_at=now()+$2*interval '1 second',next_attempt_at=now()+$2*interval '1 second' WHERE id=$1").bind(id).bind(retry).execute(&mut *tx).await?;
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
            doc = serde_json::from_value(retained)?;
        }
    } else {
        sqlx::query("UPDATE resource SET missing_streak=0,last_missing_at=NULL WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let before = before
        .map(|v| serde_json::from_value::<ResourceMeta>(v).map(|d| normalize(&d)))
        .transpose()?;
    let after = normalize(&doc);
    crate::store::write_document(&mut tx, id, generation, &doc).await?;
    if let Some(diff) = diff(before.as_ref(), Some(&after))
        .filter(|_| before.is_some() || doc.status != Status::Missing)
    {
        append(&mut tx, id, &source, before, Some(after), diff).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn delete_orphans(pool: &PgPool, grace_days: u32) -> Result<u64> {
    let mut tx = pool.begin().await?;
    lock_sequence(&mut tx).await?;
    let rows=sqlx::query("SELECT r.id,r.resolver,r.ever_existed,d.doc FROM resource r JOIN resource_document d ON d.id=r.id WHERE r.orphaned_at<now()-$1*interval '1 day' AND NOT EXISTS(SELECT 1 FROM reference f WHERE f.resource_id=r.id) AND (r.lease_until IS NULL OR r.lease_until<now()) FOR UPDATE OF r SKIP LOCKED LIMIT 1000")
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
    let candidates: Vec<(String, i64)> = sqlx::query_as("SELECT osm_type,osm_id FROM osm_place WHERE (SELECT count(*) FROM jsonb_object_keys(versions))>1 FOR UPDATE SKIP LOCKED").fetch_all(&mut *tx).await?;
    for (kind, id) in candidates {
        sqlx::query("WITH kept AS (SELECT coalesce(jsonb_object_agg(v.key,v.value),'{}'::jsonb) AS versions FROM osm_place o CROSS JOIN LATERAL jsonb_each(o.versions) v WHERE o.osm_type=$1 AND o.osm_id=$2 AND (v.key=md5((o.doc-ARRAY['fetched_at','ttl_secs']::text[])::text) OR v.key=md5((o.fetch_doc-ARRAY['fetched_at','ttl_secs']::text[])::text) OR EXISTS(SELECT 1 FROM resource r WHERE r.osm_type=o.osm_type AND r.osm_id=o.osm_id AND r.osm_version=v.key) OR EXISTS(SELECT 1 FROM resource_change c WHERE c.osm_type=o.osm_type AND c.osm_id=o.osm_id AND (c.before->>'version'=v.key OR c.after->>'version'=v.key)))) UPDATE osm_place o SET versions=k.versions FROM kept k WHERE o.osm_type=$1 AND o.osm_id=$2 AND o.versions IS DISTINCT FROM k.versions").bind(kind).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
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
