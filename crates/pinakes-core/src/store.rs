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
    sqlx::query("INSERT INTO resource(id, uri, scheme, host, resolver,osm_type,osm_id,type_path) VALUES($1,$2,$3,$4,$5,$6,$7,CASE WHEN $7::bigint IS NULL THEN '{}'::text[] ELSE NULL END) ON CONFLICT(id) DO NOTHING")
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
    Ok(())
}
pub async fn cleanup(pool: &PgPool, grace_days: u32) -> Result<u64> {
    crate::changes::delete_orphans(pool, grace_days).await
}
pub async fn expedite(pool: &PgPool, ids: &[String]) -> Result<()> {
    sqlx::query("UPDATE resource SET priority_at=coalesce(priority_at,now()) WHERE id=ANY($1) AND refresh_at<=now() AND NOT final AND NOT dead_letter AND (lease_until IS NULL OR lease_until<now())").bind(ids).execute(pool).await?;
    Ok(())
}
pub async fn save_document(
    pool: &PgPool,
    id: &str,
    generation: i64,
    doc: &ResourceMeta,
) -> Result<()> {
    crate::changes::save(pool, id, generation, doc, 3600).await
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
        sqlx::query("UPDATE resource SET osm_type=$3,osm_id=$4,type=NULL,type_path=NULL,name=NULL,category=NULL,point=NULL,doc=$5,status=$6,resolved_at=now(),refresh_at=to_timestamp($7::float8/1000)+$8*interval '1 second',next_attempt_at=to_timestamp($7::float8/1000)+$8*interval '1 second',attempts=0,dead_letter=false,last_error=NULL,lease_until=NULL,priority_at=NULL,osm_version=md5($9::jsonb::text) WHERE id=$1 AND generation=$2")
            .bind(id).bind(generation).bind(&osm.osm_type).bind(osm.id).bind(minimal).bind(doc.status.as_str()).bind(doc.fetched_at as f64).bind(doc.ttl_secs as f64).bind(crate::changes::normalize(doc)).execute(&mut **tx).await?;
        sqlx::query("UPDATE osm_place SET point=CASE WHEN $3::float8 IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($4,$3),4326) END,bbox=$5,name=$6,category=$7,address=$8,tags=$9,type=$10,type_path=$11,doc=$12,status=$13,display_name=CASE WHEN $13='ok' THEN fetch_display_name ELSE display_name END,versions=versions || jsonb_build_object(md5($14::jsonb::text),$14::jsonb) WHERE osm_type=$1 AND osm_id=$2")
            .bind(&osm.osm_type).bind(osm.id).bind(doc.location.as_ref().map(|p|p.lat)).bind(doc.location.as_ref().map(|p|p.lon)).bind(doc.bbox.map(|b|b.to_vec())).bind(&doc.name).bind(&doc.category).bind(serde_json::json!(doc.address)).bind(serde_json::json!(doc.attributes)).bind(&doc.type_name).bind(&doc.type_path).bind(serde_json::json!(doc)).bind(doc.status.as_str()).bind(crate::changes::normalize(doc)).execute(&mut **tx).await?;
        return Ok(());
    }
    sqlx::query(r#"UPDATE resource SET type=$3, type_path=$4, name=$5, category=$6,
        point=CASE WHEN $7::double precision IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($8,$7),4326) END,
        start_at=$9, end_at=$10, doc=$11, status=$12, resolved_at=now(),
        refresh_at=now()+NULLIF($13,0)*interval '1 second', next_attempt_at=coalesce(now()+NULLIF($13,0)*interval '1 second','infinity'::timestamptz), final=($13=0),
        attempts=0, dead_letter=false, last_error=NULL, lease_until=NULL, priority_at=NULL
        WHERE id=$1 AND generation=$2"#)
        .bind(id).bind(generation).bind(&doc.type_name).bind(&doc.type_path).bind(&doc.name).bind(&doc.category)
        .bind(doc.location.as_ref().map(|p| p.lat)).bind(doc.location.as_ref().map(|p| p.lon))
        .bind(doc.start).bind(doc.end).bind(serde_json::to_value(doc)?).bind(doc.status.as_str()).bind(doc.ttl_secs as f64)
        .execute(&mut **tx).await?;
    Ok(())
}
