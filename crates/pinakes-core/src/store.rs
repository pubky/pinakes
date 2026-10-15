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
    sqlx::query("INSERT INTO resource(id, uri, scheme, host, resolver) VALUES($1,$2,$3,$4,$5) ON CONFLICT(id) DO NOTHING")
        .bind(id).bind(uri).bind(scheme).bind(host).bind(classify(uri)).execute(&mut **tx).await?;
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
    Ok(sqlx::query("DELETE FROM resource r WHERE orphaned_at < now() - $1 * interval '1 day' AND NOT EXISTS (SELECT 1 FROM reference f WHERE f.resource_id=r.id) AND (lease_until IS NULL OR lease_until < now())")
        .bind(f64::from(grace_days)).execute(pool).await?.rows_affected())
}
pub async fn save_document(
    pool: &PgPool,
    id: &str,
    generation: i64,
    doc: &ResourceMeta,
) -> Result<()> {
    sqlx::query(r#"UPDATE resource SET type=$3, type_path=$4, name=$5, category=$6,
        point=CASE WHEN $7::double precision IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($8,$7),4326) END,
        start_at=$9, end_at=$10, doc=$11, status=$12, resolved_at=now(),
        refresh_at=now()+$13*interval '1 second', next_attempt_at=now()+$13*interval '1 second',
        attempts=0, dead_letter=false, last_error=NULL, lease_until=NULL
        WHERE id=$1 AND generation=$2"#)
        .bind(id).bind(generation).bind(&doc.type_name).bind(&doc.type_path).bind(&doc.name).bind(&doc.category)
        .bind(doc.location.as_ref().map(|p| p.lat)).bind(doc.location.as_ref().map(|p| p.lon))
        .bind(doc.start).bind(doc.end).bind(serde_json::to_value(doc)?).bind(doc.status.as_str()).bind(doc.ttl_secs as f64)
        .execute(pool).await?;
    Ok(())
}
