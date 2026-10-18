use crate::{
    model::{identity, ResourceMeta},
    resolver::classify,
};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

pub async fn ensure_resource(pool: &PgPool, uri: &str) -> Result<String> {
    let (id, uri, scheme) = identity(uri)?;
    let mut tx = pool.begin().await?;
    insert_resource(&mut tx, &id, &uri, &scheme).await?;
    tx.commit().await?;
    Ok(id)
}
/// Admit derived identities once per parent, including across subsequent polls.
/// The lineage survives refreshes so author budgets apply when the child is claimed.
pub async fn derive_resource(
    pool: &PgPool,
    parent: &str,
    uri: &str,
    adapter: &str,
    cap: usize,
) -> Result<Option<String>> {
    derive_resource_until(pool, parent, uri, adapter, cap, 10000, None).await
}
/// The latest expiry a derived link may carry: the event horizon (`event.months_forward`).
pub fn derivation_horizon(cfg: &crate::config::EventConfig) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
        .checked_add_months(chrono::Months::new(cfg.months_forward))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
}
/// Cap stored derived-link expiries at the event horizon, so a link sponsored by a distant event
/// (stored before the cap, or under a longer horizon) frees its slot in time. Runs at startup in
/// batches of 1000; idempotent.
pub async fn cap_derivation_expiry(pool: &PgPool, cfg: &crate::config::EventConfig) -> Result<u64> {
    let horizon = derivation_horizon(cfg);
    let mut total = 0u64;
    loop {
        let capped = sqlx::query("UPDATE resource_derivation SET expires_at=$1 WHERE (parent_id,child_id) IN (SELECT parent_id,child_id FROM resource_derivation WHERE expires_at>$1 LIMIT 1000)")
            .bind(horizon)
            .execute(pool)
            .await?
            .rows_affected();
        total += capped;
        if capped == 0 {
            return Ok(total);
        }
    }
}
/// Time-scoped sponsorship frees expired event slots without fetching every historical link.
pub async fn derive_resource_until(
    pool: &PgPool,
    parent: &str,
    uri: &str,
    adapter: &str,
    cap: usize,
    global_cap: usize,
    expires: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Option<String>> {
    if expires.is_some_and(|at| at <= chrono::Utc::now()) {
        return Ok(None);
    }
    // Never past the last storable instant (`time::bounded`), whatever the caller derived.
    let latest = chrono::DateTime::from_timestamp(253402300799, 0).expect("9999-12-31T23:59:59Z");
    let expires = expires.map(|at| at.min(latest));
    let (parent_id, _, _) = identity(parent)?;
    let (id, uri, scheme) = identity(uri)?;
    if parent_id == id {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,17614))")
        .bind(adapter)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,17613))")
        .bind(&parent_id)
        .execute(&mut *tx)
        .await?;
    // A resolver probe or a removed parent must never create background work.
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource WHERE id=$1)")
        .bind(&parent_id)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Ok(None);
    }
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM resource_derivation WHERE parent_id=$1 AND child_id=$2 AND (expires_at IS NULL OR expires_at>now()))",
    )
    .bind(&parent_id)
    .bind(&id)
    .fetch_one(&mut *tx)
    .await?;
    if !known {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM resource_derivation WHERE parent_id=$1 AND (expires_at IS NULL OR expires_at>now())")
                .bind(&parent_id)
                .fetch_one(&mut *tx)
                .await?;
        let total: i64 =
            sqlx::query_scalar("SELECT count(*) FROM resource_derivation WHERE adapter=$1 AND (expires_at IS NULL OR expires_at>now())")
                .bind(adapter)
                .fetch_one(&mut *tx)
                .await?;
        if count >= cap as i64 || total >= global_cap as i64 {
            return Ok(None);
        }
        insert_resource(&mut tx, &id, &uri, &scheme).await?;
        sqlx::query("INSERT INTO resource_derivation(parent_id,child_id,adapter,expires_at) VALUES($1,$2,$3,$4) ON CONFLICT(parent_id,child_id) DO UPDATE SET expires_at=excluded.expires_at")
            .bind(&parent_id)
            .bind(&id)
            .bind(adapter)
            .bind(expires)
            .execute(&mut *tx)
            .await?;
    }
    if known {
        sqlx::query(
            "UPDATE resource_derivation SET expires_at=$3 WHERE parent_id=$1 AND child_id=$2",
        )
        .bind(&parent_id)
        .bind(&id)
        .bind(expires)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(id))
}
pub async fn insert_resource(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    uri: &str,
    scheme: &str,
) -> Result<()> {
    let host = url::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    let osm = crate::resolver::osm::parse_id(uri);
    sqlx::query("INSERT INTO resource(id, uri, scheme, host, resolver,osm_type,osm_id,doc,type_path) VALUES($1,$2,$3,$4,$5,$6,$7,CASE WHEN $7::bigint IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('type','Place','url',$2::text,'osm_id',$6::text||'/'||$7::text,'source','osm','status','pending') END,CASE WHEN $7::bigint IS NULL THEN '{}'::text[] ELSE NULL END) ON CONFLICT(id) DO NOTHING")
        .bind(id).bind(uri).bind(scheme).bind(host).bind(classify(uri)).bind(osm.as_ref().map(|o|&o.osm_type)).bind(osm.as_ref().map(|o|o.id)).execute(&mut **tx).await?;
    Ok(())
}
pub async fn recompute_scores(tx: &mut Transaction<'_, Postgres>, ids: &[String]) -> Result<()> {
    sqlx::query("DELETE FROM resource_score WHERE resource_id = ANY($1)")
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO resource_score(resource_id, score, updated_at, newest_at)
        SELECT resource_id,
          (ln(1.0 + count(DISTINCT author) FILTER (WHERE kind='tag'))
          + 0.5 * ln(1.0 + count(*) FILTER (WHERE kind='reply'))
          + 0.3 * ln(1.0 + count(*) FILTER (WHERE kind IN ('embed','collection'))))
          * exp(-ln(2.0) * greatest(0, extract(epoch FROM (now()-max(created_at)))) / 2592000.0),
          now(), max(created_at)
        FROM reference WHERE resource_id = ANY($1) GROUP BY resource_id
    "#,
    )
    .bind(ids)
    .execute(&mut **tx)
    .await?;
    sqlx::query(r#"UPDATE resource r SET orphaned_at = CASE WHEN EXISTS
        (SELECT 1 FROM reference f WHERE f.resource_id=r.id) THEN NULL ELSE coalesce(orphaned_at, now()) END
        WHERE r.id = ANY($1)"#).bind(ids).execute(&mut **tx).await?;
    crate::groups::scores(tx, ids).await?;
    Ok(())
}
pub async fn cleanup(pool: &PgPool, grace_days: u32) -> Result<u64> {
    sqlx::query("DELETE FROM blocked_reprobe WHERE day<(now() AT TIME ZONE 'UTC')::date-1")
        .execute(pool)
        .await?;
    crate::changes::delete_orphans(pool, grace_days).await
}
pub async fn expedite(pool: &PgPool, ids: &[String]) -> Result<()> {
    sqlx::query("UPDATE resource SET priority_at=coalesce(priority_at,now()) WHERE id=ANY($1) AND (refresh_at IS NULL OR refresh_at<=now()) AND NOT final AND NOT dead_letter AND (lease_until IS NULL OR lease_until<now())").bind(ids).execute(pool).await?;
    Ok(())
}
pub async fn save_document(
    pool: &PgPool,
    id: &str,
    generation: i64,
    doc: &ResourceMeta,
) -> Result<()> {
    crate::changes::save(
        pool,
        id,
        generation,
        doc,
        &crate::config::QueueConfig::default(),
    )
    .await
}
/// Which extractor produced a saved document, recorded as `extracted_by`/`extracted_with` for
/// `queue::reextract_outdated`.
#[derive(Clone, Copy, Debug)]
pub enum Extraction<'a> {
    /// The worker of the row's own route (direct saves).
    Route,
    /// This worker kind ran, e.g. `web` for a disabled card kind, or for an event/event_platform
    /// row while calendars/adapters are off.
    By(&'a str),
    /// A 304 re-saved the stored document: the route and extractor recorded with it stand.
    Unchanged,
}
impl Extraction<'_> {
    fn binds(self) -> (bool, Option<String>) {
        match self {
            Self::Route => (false, None),
            Self::By(kind) => (false, Some(kind.to_owned())),
            Self::Unchanged => (true, None),
        }
    }
}
pub(crate) async fn write_document(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    generation: i64,
    doc: &ResourceMeta,
    extraction: Extraction<'_>,
) -> Result<()> {
    let (unchanged, kind) = extraction.binds();
    if doc.source == "osm" {
        let osm = crate::resolver::osm::parse_id(&doc.url)
            .ok_or_else(|| anyhow::anyhow!("invalid OSM document identity"))?;
        let minimal = serde_json::json!({"type":"Place","url":doc.url,"osm_id":osm.key(),"source":"osm","status":doc.status});
        sqlx::query("UPDATE resource SET osm_type=$3,osm_id=$4,type=NULL,type_path=NULL,name=NULL,category=NULL,point=NULL,doc=$5,status=$6,ever_existed=ever_existed OR $6='ok',resolved_at=now(),resolved_by=CASE WHEN $11 THEN resolved_by ELSE resolver END,extracted_by=CASE WHEN $11 THEN extracted_by ELSE coalesce($12,resolver) END,extracted_with=CASE WHEN $11 THEN extracted_with ELSE coalesce(($10::jsonb->>coalesce($12,resolver))::int,0) END,refresh_at=to_timestamp($7::float8/1000)+$8*interval '1 second',next_attempt_at=to_timestamp($7::float8/1000)+$8*interval '1 second',attempts=0,timeout_streak=0,dead_letter=false,last_error=NULL,lease_until=NULL,priority_at=NULL,osm_version=md5($9::jsonb::text) WHERE id=$1 AND generation=$2")
            .bind(id).bind(generation).bind(&osm.osm_type).bind(osm.id).bind(minimal).bind(doc.status.as_str()).bind(doc.fetched_at as f64).bind(doc.ttl_secs as f64).bind(crate::changes::normalize(doc)).bind(crate::resolver::extractor_versions()).bind(unchanged).bind(&kind).execute(&mut **tx).await?;
        sqlx::query("UPDATE osm_place SET point=CASE WHEN $3::float8 IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($4,$3),4326) END,bbox=$5,name=$6,category=$7,address=$8,tags=$9,type=$10,type_path=$11,doc=$12,status=$13,display_name=CASE WHEN $13='ok' THEN fetch_display_name ELSE display_name END,versions=versions || jsonb_build_object(md5($14::jsonb::text),$14::jsonb) WHERE osm_type=$1 AND osm_id=$2")
            .bind(&osm.osm_type).bind(osm.id).bind(doc.location.as_ref().map(|p|p.lat)).bind(doc.location.as_ref().map(|p|p.lon)).bind(doc.bbox.map(|b|b.to_vec())).bind(&doc.name).bind(&doc.category).bind(serde_json::json!(doc.address)).bind(serde_json::json!(doc.attributes)).bind(&doc.type_name).bind(&doc.type_path).bind(serde_json::json!(doc)).bind(doc.status.as_str()).bind(crate::changes::normalize(doc)).execute(&mut **tx).await?;
        return Ok(());
    }
    sqlx::query(r#"UPDATE resource SET type=$3, type_path=$4, name=$5, category=$6,
        point=CASE WHEN $7::double precision IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($8,$7),4326) END,
        start_at=$9, end_at=$10, venue_id=$14, doc=$11, status=$12, ever_existed=ever_existed OR $12='ok', resolved_at=now(), resolved_by=CASE WHEN $16 THEN resolved_by ELSE resolver END,
        extracted_by=CASE WHEN $16 THEN extracted_by ELSE coalesce($17,resolver) END,
        extracted_with=CASE WHEN $16 THEN extracted_with ELSE coalesce(($15::jsonb->>coalesce($17,resolver))::int,0) END,
        refresh_at=now()+NULLIF($13,0)*interval '1 second', next_attempt_at=coalesce(now()+NULLIF($13,0)*interval '1 second','infinity'::timestamptz), final=($13=0),
        attempts=0, timeout_streak=0, dead_letter=false, last_error=NULL, lease_until=NULL, priority_at=NULL
        WHERE id=$1 AND generation=$2"#)
        .bind(id).bind(generation).bind(&doc.type_name).bind(&doc.type_path).bind(&doc.name).bind(&doc.category)
        .bind(doc.location.as_ref().map(|p| p.lat)).bind(doc.location.as_ref().map(|p| p.lon))
        .bind(doc.start).bind(doc.end).bind(serde_json::to_value(doc)?).bind(doc.status.as_str()).bind(doc.ttl_secs as f64).bind(doc.event.venue.as_ref().and_then(|v|v.resource_id.as_deref()))
        .bind(crate::resolver::extractor_versions()).bind(unchanged).bind(&kind)
        .execute(&mut **tx).await?;
    if matches!(doc.source.as_str(), "media" | "activitypub")
        && doc.status == crate::model::Status::Ok
    {
        // A promoted card is already that route's card, so it is not re-queued at startup; the
        // extractor that produced it stays recorded.
        sqlx::query("UPDATE resource SET resolver=$2,resolved_by=$2 WHERE id=$1 AND generation=$3")
            .bind(id)
            .bind(&doc.source)
            .bind(generation)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Acquire before explicit resource row locks, matching the write triggers' lock order.
pub(crate) async fn lock_search_revision(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SELECT revision FROM search_revision WHERE id=1 FOR UPDATE")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
