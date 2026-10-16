use super::{Container, EventStatus};
use crate::{changes, model::ResourceMeta};
use anyhow::Result;
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeSet;

/// Called under the resource generation fence and change sequence lock. Snapshot and rows commit together.
pub async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    container: &Container,
    fetched_at: i64,
) -> Result<bool> {
    sqlx::query("INSERT INTO event_fetch(resource_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    let last: Option<i64> = sqlx::query_scalar(
        "SELECT last_snapshot_at FROM event_fetch WHERE resource_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    if last.is_some_and(|at| fetched_at <= at) {
        return Ok(false);
    }
    sqlx::query("UPDATE event_fetch SET last_snapshot_at=$2 WHERE resource_id=$1")
        .bind(id)
        .bind(fetched_at)
        .execute(&mut **tx)
        .await?;
    let source = format!("calendar:{id}");
    let rows=sqlx::query("SELECT uid,recurrence_id,doc,content_hash,sequence,modified FROM occurrence WHERE resource_id=$1 ORDER BY uid,recurrence_id FOR UPDATE").bind(id).fetch_all(&mut **tx).await?;
    let now = chrono::Utc::now();
    let mutable = |doc: &ResourceMeta| {
        doc.start
            .is_some_and(|at| at >= container.horizon_start && at <= container.horizon_end)
            && doc
                .end
                .or(doc.start)
                .is_none_or(|e| now < e + chrono::Duration::hours(24))
    };
    let mut previous = std::collections::BTreeMap::new();
    for row in rows {
        let uid: String = row.try_get("uid")?;
        let rid: String = row.try_get("recurrence_id")?;
        let key = serde_json::to_string(&(&uid, &rid))?;
        let doc: ResourceMeta = serde_json::from_value(row.try_get("doc")?)?;
        previous.insert(
            key,
            (
                uid,
                rid,
                doc,
                row.try_get::<String, _>("content_hash")?,
                row.try_get::<i64, _>("sequence")?,
                row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("modified")?,
            ),
        );
    }
    let old: Vec<_> = previous
        .iter()
        .filter(|(_, (_, _, d, _, _, _))| {
            mutable(d) && d.event.event_status != Some(EventStatus::Cancelled)
        })
        .map(|(k, _)| k.clone())
        .collect();
    sqlx::query("INSERT INTO source_state(source,keys) VALUES($1,$2) ON CONFLICT(source) DO UPDATE SET keys=$2").bind(&source).bind(&old).execute(&mut **tx).await?;
    let incoming: Vec<_> = container
        .occurrences
        .iter()
        .filter(|o| mutable(&o.doc) || !previous.contains_key(&o.key()))
        .map(|o| o.key())
        .collect();
    let keys =
        crate::source::container_keys_tx(tx, &source, &incoming, container.held_retry_secs).await?;
    if keys.held {
        return Ok(false);
    }
    let present: BTreeSet<_> = container.occurrences.iter().map(|o| o.key()).collect();
    let mut cancelled = keys.cancelled.into_iter().collect::<BTreeSet<_>>();
    for item in &container.occurrences {
        let key = item.key();
        let old = previous.get(&key);
        if old.is_some_and(|(_, _, d, _, _, _)| !mutable(d)) {
            continue;
        }
        if old.is_some_and(|(_, _, _, _, seq, modified)| {
            *seq > item.sequence
                || (*seq == item.sequence
                    && modified.is_some()
                    && item.modified.is_some()
                    && *modified > item.modified)
        }) {
            continue;
        }
        let before = old.map(|(_, _, d, _, _, _)| changes::normalize(d));
        let after = changes::normalize(&item.doc);
        let supersedes = if old.is_none() {
            previous
                .iter()
                .find(|(k, (_, _, d, hash, _, _))| {
                    !present.contains(*k)
                        && hash == &item.content_hash
                        && d.start == item.doc.start
                        && d.name == item.doc.name
                })
                .map(|(key, _)| key.clone())
        } else {
            None
        };
        if let Some(old) = &supersedes {
            cancelled.insert(old.clone());
        }
        let status = item
            .doc
            .event
            .event_status
            .as_ref()
            .unwrap_or(&EventStatus::Scheduled)
            .as_str();
        let link = crate::model::identity(&item.doc.url).ok().map(|v| v.0);
        sqlx::query("INSERT INTO occurrence(resource_id,uid,recurrence_id,start_at,end_at,name,venue_id,status,url,event_resource_id,doc,point,sequence,modified,content_hash,volatile,supersedes) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,(SELECT id FROM resource WHERE id=$10 AND id<>$1),$11,CASE WHEN $12::float8 IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($13,$12),4326) END,$14,$15,$16,$17,$18) ON CONFLICT(resource_id,uid,recurrence_id) DO UPDATE SET start_at=excluded.start_at,end_at=excluded.end_at,name=excluded.name,venue_id=excluded.venue_id,status=excluded.status,url=excluded.url,event_resource_id=excluded.event_resource_id,doc=excluded.doc,point=excluded.point,sequence=excluded.sequence,modified=excluded.modified,content_hash=excluded.content_hash,volatile=excluded.volatile,supersedes=coalesce(excluded.supersedes,occurrence.supersedes)")
            .bind(id).bind(&item.uid).bind(&item.recurrence_id).bind(item.doc.start).bind(item.doc.end).bind(&item.doc.name).bind(item.doc.event.venue.as_ref().and_then(|v|v.resource_id.as_deref())).bind(status).bind(&item.doc.url).bind(link).bind(serde_json::to_value(&item.doc)?).bind(item.doc.location.as_ref().map(|p|p.lat)).bind(item.doc.location.as_ref().map(|p|p.lon)).bind(item.sequence).bind(item.modified).bind(&item.content_hash).bind(item.volatile).bind(supersedes).execute(&mut **tx).await?;
        if let Some(diff) = changes::diff(before.as_ref(), Some(&after)) {
            changes::append(tx, id, "event", before, Some(after), diff, Some(&key)).await?;
        }
    }
    for key in cancelled {
        let Some((uid, rid, doc, _, _, _)) = previous.get(&key) else {
            continue;
        };
        if !mutable(doc) {
            continue;
        }
        let before = changes::normalize(doc);
        let mut doc = doc.clone();
        doc.event.event_status = Some(EventStatus::Cancelled);
        let after = changes::normalize(&doc);
        sqlx::query("UPDATE occurrence SET status='cancelled',doc=$4 WHERE resource_id=$1 AND uid=$2 AND recurrence_id=$3").bind(id).bind(uid).bind(rid).bind(serde_json::to_value(doc)?).execute(&mut **tx).await?;
        if let Some(diff) = changes::diff(Some(&before), Some(&after)) {
            changes::append(tx, id, "event", Some(before), Some(after), diff, Some(&key)).await?;
        }
    }
    Ok(true)
}

#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct OccurrenceQuery {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}
#[utoipa::path(get,path="/v1/resources/{id}/occurrences",params(("id"=String,Path),OccurrenceQuery),responses((status=200,body=Value)),description="Calendar instances inside an inclusive start window. Stable UID/recurrence keys; bounded by the configured feed cap.")]
pub async fn list(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<OccurrenceQuery>,
) -> Result<axum::Json<Value>, crate::api::ApiError> {
    if !crate::model::valid_id(&id) || q.from.zip(q.to).is_some_and(|(a, b)| a > b) {
        return Err(crate::api::ApiError::bad("invalid occurrence id or window"));
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource WHERE id=$1)")
        .bind(&id)
        .fetch_one(&state.pool)
        .await?;
    if !exists {
        return Err(crate::api::ApiError(
            axum::http::StatusCode::NOT_FOUND,
            "resource not found".into(),
        ));
    }
    let items:Vec<Value>=sqlx::query_scalar("SELECT (to_jsonb(o)-ARRAY['point','effective_point','doc','document','content_hash','modified','sequence']::text[])||jsonb_build_object('document',document,'occurrence',jsonb_build_object('uid',uid,'recurrence_id',nullif(recurrence_id,''))) FROM event_occurrence o WHERE resource_id=$1 AND ($2::timestamptz IS NULL OR start_at>=$2) AND ($3::timestamptz IS NULL OR start_at<=$3) ORDER BY start_at,uid,recurrence_id LIMIT $4").bind(id).bind(q.from).bind(q.to).bind(state.config.event.max_occurrences_per_feed as i64).fetch_all(&state.pool).await?;
    Ok(axum::Json(serde_json::json!({"items":items})))
}
