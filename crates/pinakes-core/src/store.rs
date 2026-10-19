use crate::{
    model::{identity, ResourceMeta},
    resolver::classify,
};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

pub async fn ensure_resource(pool: &PgPool, uri: &str) -> Result<String> {
    let (id, uri, _) = identity(uri)?;
    let mut tx = pool.begin().await?;
    // An alias is assigned under the lock every alias assignment takes (`alias::assign`).
    if crate::alias::root(&uri).is_some() {
        lock_search_revision(&mut tx).await?;
    }
    insert_resource(&mut tx, &id, &uri).await?;
    tx.commit().await?;
    Ok(id)
}
/// Insert a resource row once; a URI with a static alias root (`alias::root`) becomes an alias
/// of that root, which is inserted and queued in its place.
pub async fn insert_resource(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    uri: &str,
) -> Result<()> {
    insert_plain(tx, id, uri).await?;
    if let Some(alias) = crate::alias::root(uri) {
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if !known {
            crate::alias::assign(tx, id, &alias).await?;
        }
    }
    Ok(())
}
/// `insert_resource` without the alias check, for roots.
pub(crate) async fn insert_plain(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    uri: &str,
) -> Result<()> {
    let host = url::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    let osm = crate::resolver::osm::parse_id(uri);
    sqlx::query("INSERT INTO resource(id, uri, host, resolver,osm_type,osm_id,doc,type_path) VALUES($1,$2,$3,$4,$5,$6,CASE WHEN $6::bigint IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('type','Place','url',$2::text,'osm_id',$5::text||'/'||$6::text,'source','osm','status','pending') END,CASE WHEN $6::bigint IS NULL THEN '{}'::text[] ELSE NULL END) ON CONFLICT(id) DO NOTHING")
        .bind(id).bind(uri).bind(host).bind(classify(uri)).bind(osm.as_ref().map(|o|&o.osm_type)).bind(osm.as_ref().map(|o|o.id)).execute(&mut **tx).await?;
    Ok(())
}
pub async fn recompute_scores(tx: &mut Transaction<'_, Postgres>, ids: &[String]) -> Result<()> {
    // An alias's references count for its root.
    let roots: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT root_id FROM resource_alias WHERE alias_id=ANY($1) AND NOT root_id=ANY($1)",
    )
    .bind(ids)
    .fetch_all(&mut **tx)
    .await?;
    let all = [ids, &roots[..]].concat();
    let ids = &all[..];
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
        FROM effective_reference WHERE resource_id = ANY($1) GROUP BY resource_id
    "#,
    )
    .bind(ids)
    .execute(&mut **tx)
    .await?;
    sqlx::query(r#"UPDATE resource r SET orphaned_at = CASE WHEN EXISTS
        (SELECT 1 FROM reference f WHERE f.resource_id=r.id) OR EXISTS
        (SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id) THEN NULL ELSE coalesce(orphaned_at, now()) END
        WHERE r.id = ANY($1)"#).bind(ids).execute(&mut **tx).await?;
    Ok(())
}
pub async fn cleanup(pool: &PgPool, grace_days: u32) -> Result<u64> {
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
pub(crate) async fn write_document(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    generation: i64,
    doc: &ResourceMeta,
) -> Result<()> {
    if doc.source == "osm" {
        let osm = crate::resolver::osm::parse_id(&doc.url)
            .ok_or_else(|| anyhow::anyhow!("invalid OSM document identity"))?;
        let minimal = serde_json::json!({"type":"Place","url":doc.url,"osm_id":osm.key(),"source":"osm","status":doc.status});
        sqlx::query("UPDATE resource SET osm_type=$3,osm_id=$4,type=NULL,type_path=NULL,name=NULL,category=NULL,point=NULL,doc=$5,status=$6,ever_existed=ever_existed OR $6='ok',resolved_at=now(),refresh_at=to_timestamp($7::float8/1000)+$8*interval '1 second',next_attempt_at=to_timestamp($7::float8/1000)+$8*interval '1 second',attempts=0,timeout_streak=0,dead_letter=false,last_error=NULL,lease_until=NULL,priority_at=NULL,osm_version=md5($9::jsonb::text) WHERE id=$1 AND generation=$2")
            .bind(id).bind(generation).bind(&osm.osm_type).bind(osm.id).bind(minimal).bind(doc.status.as_str()).bind(doc.fetched_at as f64).bind(doc.ttl_secs as f64).bind(crate::changes::normalize(doc)).execute(&mut **tx).await?;
        sqlx::query("UPDATE osm_place SET point=CASE WHEN $3::float8 IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($4,$3),4326) END,bbox=$5,name=$6,category=$7,address=$8,tags=$9,type=$10,type_path=$11,doc=$12,status=$13,display_name=CASE WHEN $13='ok' THEN fetch_display_name ELSE display_name END,versions=versions || jsonb_build_object(md5($14::jsonb::text),$14::jsonb) WHERE osm_type=$1 AND osm_id=$2")
            .bind(&osm.osm_type).bind(osm.id).bind(doc.location.as_ref().map(|p|p.lat)).bind(doc.location.as_ref().map(|p|p.lon)).bind(doc.bbox.map(|b|b.to_vec())).bind(&doc.name).bind(&doc.category).bind(serde_json::json!(doc.address)).bind(serde_json::json!(doc.attributes)).bind(&doc.type_name).bind(&doc.type_path).bind(serde_json::json!(doc)).bind(doc.status.as_str()).bind(crate::changes::normalize(doc)).execute(&mut **tx).await?;
        return Ok(());
    }
    sqlx::query(r#"UPDATE resource SET type=$3, type_path=$4, name=$5, category=$6,
        point=CASE WHEN $7::double precision IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($8,$7),4326) END,
        start_at=$9, end_at=$10, venue_id=$14, doc=$11, status=$12, ever_existed=ever_existed OR $12='ok', resolved_at=now(),
        refresh_at=now()+NULLIF($13,0)*interval '1 second', next_attempt_at=coalesce(now()+NULLIF($13,0)*interval '1 second','infinity'::timestamptz), final=($13=0),
        attempts=0, timeout_streak=0, dead_letter=false, last_error=NULL, lease_until=NULL, priority_at=NULL
        WHERE id=$1 AND generation=$2"#)
        .bind(id).bind(generation).bind(&doc.type_name).bind(&doc.type_path).bind(&doc.name).bind(&doc.category)
        .bind(doc.location.as_ref().map(|p| p.lat)).bind(doc.location.as_ref().map(|p| p.lon))
        .bind(doc.start).bind(doc.end).bind(serde_json::to_value(doc)?).bind(doc.status.as_str()).bind(doc.ttl_secs as f64).bind(doc.event.venue.as_ref().and_then(|v|v.resource_id.as_deref()))
        .execute(&mut **tx).await?;
    if matches!(doc.source.as_str(), "media" | "activitypub")
        && doc.status == crate::model::Status::Ok
    {
        // A promoted card is that route's card from now on.
        sqlx::query("UPDATE resource SET resolver=$2 WHERE id=$1 AND generation=$3")
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
