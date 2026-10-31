use crate::{
    model::{identity, ResourceMeta},
    resolver::classify,
};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

pub async fn ensure_resource(pool: &PgPool, uri: &str) -> Result<String> {
    let (id, uri, _) = identity(uri)?;
    let mut tx = pool.begin().await?;
    if crate::alias::root(&uri).is_some() {
        lock_writers(&mut tx, WriteOrder::Shared).await?;
        crate::alias::lock(&mut tx).await?;
    }
    let line = insert_resource(&mut tx, &id, &uri).await?;
    crate::alias::record(&mut tx, line.into_iter().collect()).await?;
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
/// Extend one parent's derived links whose event has entered the horizon to the event's own
/// instant (`until`), inside a delta-feed apply (`storage::apply`): a link capped at the horizon
/// when it was derived then lives until its event while the horizon moves on. A link that lapsed
/// at the cap meanwhile (an event more than one horizon away) comes back while its instant is
/// still ahead, nearest event first, only within the per-parent (`$3`) and per-adapter (`$4`)
/// caps, as a new link would; if its child was cleaned up, the row went with it and the next
/// feed row naming the link derives it again.
pub(crate) const EXTEND_PARENT_DERIVATIONS: &str = "WITH live AS (SELECT (SELECT count(*) FROM resource_derivation WHERE parent_id=$1 AND (expires_at IS NULL OR expires_at>now())) AS parent,
 (SELECT count(*) FROM resource_derivation WHERE adapter IN (SELECT adapter FROM resource_derivation WHERE parent_id=$1) AND (expires_at IS NULL OR expires_at>now())) AS total),
 revived AS (SELECT c.child_id FROM (SELECT d.child_id,row_number() OVER (ORDER BY d.until,d.child_id) AS n FROM resource_derivation d
   WHERE d.parent_id=$1 AND d.until IS NOT NULL AND d.until<=$2 AND d.until>now() AND d.expires_at<=now()) c,live
   WHERE c.n<=least($3-live.parent,$4-live.total))
 UPDATE resource_derivation SET expires_at=until WHERE parent_id=$1 AND until IS NOT NULL AND until<=$2 AND until>now() AND (expires_at IS NULL OR expires_at<until)
   AND (expires_at IS NULL OR expires_at>now() OR child_id IN (SELECT child_id FROM revived))";
/// `EXTEND_PARENT_DERIVATIONS` outside a feed apply.
pub async fn extend_parent_links(
    pool: &PgPool,
    parent: &str,
    horizon: chrono::DateTime<chrono::Utc>,
    caps: (usize, usize),
) -> Result<u64> {
    let parent = identity(parent)?.0;
    let mut tx = pool.begin().await?;
    sqlx::query(LOCK_PARENT_ADAPTER)
        .bind(&parent)
        .execute(&mut *tx)
        .await?;
    let extended = sqlx::query(EXTEND_PARENT_DERIVATIONS)
        .bind(&parent)
        .bind(horizon)
        .bind(caps.0 as i64)
        .bind(caps.1 as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(extended)
}
/// The adapter lock a new derived link takes (`derive_resource_within`), for the adapter of
/// `$1`'s links: a revival and a new link never count the caps at the same time.
pub(crate) const LOCK_PARENT_ADAPTER: &str = "SELECT pg_advisory_xact_lock(hashtextextended(adapter,17614)) FROM (SELECT DISTINCT adapter FROM resource_derivation WHERE parent_id=$1 ORDER BY adapter) a";
/// Record which feed rows name which of `parent`'s links. `live` are this batch's rows with a
/// link (row key, link URL, the row's instant); `gone` are its row keys that name no future link
/// any more (tombstoned, past, or without a website). A row key names one link at a time: a
/// re-linked row leaves its previous link. Each link's `until` becomes the farthest instant among
/// the rows still naming it, or NULL when none does, and its expiry never stays past that. A
/// link records at most 64 naming rows, those asking for the farthest instants.
pub async fn link_sources(
    pool: &PgPool,
    parent: &str,
    live: &[(String, String, chrono::DateTime<chrono::Utc>)],
    gone: &[String],
) -> Result<()> {
    let parent_id = identity(parent)?.0;
    let live: Vec<serde_json::Value> = live
        .iter()
        .filter_map(|(key, link, until)| {
            identity(link)
                .ok()
                .map(|(child, _, _)| serde_json::json!({"key":key,"child":child,"until":until}))
        })
        .collect();
    let leaving: Vec<String> = live
        .iter()
        .filter_map(|v| v["key"].as_str().map(str::to_owned))
        .chain(gone.iter().cloned())
        .collect();
    if leaving.is_empty() {
        return Ok(());
    }
    sqlx::query(
        r#"WITH merged AS (
             SELECT d.parent_id,d.child_id,
               (d.sources - $2::text[]) || coalesce((SELECT jsonb_object_agg(v->>'key',v->'until') FROM jsonb_array_elements($3) v WHERE v->>'child'=d.child_id),'{}'::jsonb) AS sources
             FROM resource_derivation d
             WHERE d.parent_id=$1 AND (d.sources ?| $2::text[] OR d.child_id IN (SELECT v->>'child' FROM jsonb_array_elements($3) v))),
           touched AS (
             SELECT m.parent_id,m.child_id,(SELECT coalesce(jsonb_object_agg(e.key,e.value),'{}'::jsonb) FROM
               (SELECT key,value FROM jsonb_each(m.sources) ORDER BY (value#>>'{}')::timestamptz DESC,key LIMIT 64) e) AS sources
             FROM merged m)
           UPDATE resource_derivation d SET sources=t.sources,
             until=(SELECT max(value::timestamptz) FROM jsonb_each_text(t.sources)),
             expires_at=CASE WHEN t.sources='{}'::jsonb THEN least(d.expires_at,now())
               ELSE least(d.expires_at,(SELECT max(value::timestamptz) FROM jsonb_each_text(t.sources))) END
           FROM touched t WHERE d.parent_id=t.parent_id AND d.child_id=t.child_id"#,
    )
    .bind(&parent_id)
    .bind(&leaving)
    .bind(serde_json::Value::Array(live))
    .execute(pool)
    .await?;
    Ok(())
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
    derive_resource_within(pool, parent, uri, adapter, cap, global_cap, expires, None).await
}
/// `derive_resource_until` for a link whose event asks for `until` but whose expiry is capped
/// at `horizon`; the uncapped instant is kept so each feed apply can extend it as the horizon moves.
#[allow(clippy::too_many_arguments)]
pub async fn derive_resource_within(
    pool: &PgPool,
    parent: &str,
    uri: &str,
    adapter: &str,
    cap: usize,
    global_cap: usize,
    until: Option<chrono::DateTime<chrono::Utc>>,
    horizon: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Option<String>> {
    if until.is_some_and(|at| at <= chrono::Utc::now()) {
        return Ok(None);
    }
    // Never past the last storable instant (`time::bounded`), whatever the caller derived.
    let latest = chrono::DateTime::from_timestamp(253402300799, 0).expect("9999-12-31T23:59:59Z");
    let until = until.map(|at| at.min(latest));
    let expires = until.map(|at| horizon.map_or(at, |h| at.min(h)));
    let (parent_id, _, _) = identity(parent)?;
    let (id, uri, _) = identity(uri)?;
    if parent_id == id {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    // A child with an alias root becomes an alias (`insert_resource`): it then locks the root's
    // row while holding the adapter lock, which a feed apply takes after its own row.
    let order = if crate::alias::root(&uri).is_some() {
        WriteOrder::Exclusive
    } else {
        WriteOrder::Shared
    };
    lock_writers(&mut tx, order).await?;
    // Lock order: the parent's row, then the adapter, then the parent, as a feed apply takes them
    // (its parent row `FOR UPDATE`, then `LOCK_PARENT_ADAPTER`). The row lock (the one the link's
    // foreign key takes anyway) also keeps the parent until the link commits. A resolver probe
    // or a removed parent must never create background work.
    let exists = sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR KEY SHARE")
        .bind(&parent_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    if !exists {
        return Ok(None);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,17614))")
        .bind(adapter)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,17613))")
        .bind(&parent_id)
        .execute(&mut *tx)
        .await?;
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
        let line = insert_resource(&mut tx, &id, &uri).await?;
        sqlx::query("INSERT INTO resource_derivation(parent_id,child_id,adapter,expires_at,until) VALUES($1,$2,$3,$4,$5) ON CONFLICT(parent_id,child_id) DO UPDATE SET expires_at=excluded.expires_at,until=excluded.until")
            .bind(&parent_id)
            .bind(&id)
            .bind(adapter)
            .bind(expires)
            .bind(until)
            .execute(&mut *tx)
            .await?;
        crate::alias::record(&mut tx, line.into_iter().collect()).await?;
    }
    if known {
        // A shared link keeps the farthest instant any row asked for: another row's nearer edit
        // never shortens it (`link_sources` lowers it when a row stops naming the link).
        sqlx::query(
            "UPDATE resource_derivation SET until=CASE WHEN $4::timestamptz IS NULL THEN NULL ELSE greatest(until,$4) END,expires_at=CASE WHEN $4::timestamptz IS NULL THEN $3 ELSE least(greatest(until,$4),coalesce($5,greatest(until,$4))) END WHERE parent_id=$1 AND child_id=$2",
        )
        .bind(&parent_id)
        .bind(&id)
        .bind(expires)
        .bind(until)
        .bind(horizon)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(id))
}
/// Insert a resource row once; a URI with a static alias root (`alias::root`) becomes an alias
/// of that root, which is inserted and queued in its place. The caller then holds the alias
/// lock and records the returned line (`alias::record`) before committing.
pub async fn insert_resource(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    uri: &str,
) -> Result<Option<crate::alias::Line>> {
    insert_plain(tx, id, uri).await?;
    if let Some(alias) = crate::alias::root(uri) {
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if !known {
            return crate::alias::assign(tx, id, &alias).await;
        }
    }
    Ok(None)
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
    // A show's episodes follow its document: kept, replaced or cleared with it.
    crate::episodes::store(tx, id, doc).await?;
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

/// How a transaction takes the write-order lock (`lock_writers`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteOrder {
    /// The transaction locks one resource's rows, then only locks every writer takes in the same
    /// order: the resource row, its event fetch and occurrences, its adapters, its derived links,
    /// the change sequence (`changes::append`, always last). One that locks several resources'
    /// rows in an order of its own also takes the alias lock (`alias::lock`), or checks without
    /// waiting (a group run's fence).
    Shared,
    /// The transaction locks several resources' rows in an order a shared writer may meet the
    /// other way round (orphans and what deleting them reaches, a reset of every reference, a
    /// re-extraction, a derived link that becomes an alias), so it runs while no other ordered
    /// writer does.
    Exclusive,
}
/// The write-order lock, taken before a transaction's first row lock. Shared holders run
/// together; an exclusive holder runs alone among them, so no lock cycle can form. A transaction
/// that locks one row and nothing else needs none. The search revision takes no lock
/// (`search_write`): writers never wait on each other for it.
pub(crate) async fn lock_writers(
    tx: &mut Transaction<'_, Postgres>,
    order: WriteOrder,
) -> Result<()> {
    sqlx::query(match order {
        WriteOrder::Shared => "SELECT pg_advisory_xact_lock_shared($1)",
        WriteOrder::Exclusive => "SELECT pg_advisory_xact_lock($1)",
    })
    .bind(crate::locks::WRITE_ORDER)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
/// Notes a write that search pages must see but no trigger counts (the hint passes' pairs): one
/// `search_write` row for this transaction, as `bump_search_revision` leaves for the indexed
/// tables. A page reads the revision and the pairs in one snapshot, so a page that has not seen
/// the pairs is never answered 304 (a sequence would move outside the snapshot).
pub async fn note_search_write(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("INSERT INTO search_write(writes) SELECT 1 WHERE current_setting('pinakes.search_write',true) IS DISTINCT FROM 'on'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT set_config('pinakes.search_write','on',true)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Fold the counted write transactions into one row, so reading the revision stays one short
/// sum. Readers see either the rows or their sum, never both, so their revisions do not change.
pub async fn compact_search_writes(pool: &PgPool) -> Result<u64> {
    Ok(sqlx::query(
        "WITH gone AS (DELETE FROM search_write WHERE (SELECT count(*) FROM search_write)>1 RETURNING writes) INSERT INTO search_write(writes) SELECT sum(writes) FROM gone HAVING count(*)>0",
    )
    .execute(pool)
    .await?
    .rows_affected())
}
