use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GroupConfig {
    pub enabled: bool,
    pub max_members: usize,
    pub max_distance_m: f64,
    pub max_graph_units: usize,
    pub max_graph_bytes: usize,
}
impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_members: 256,
            max_distance_m: 500.0,
            max_graph_units: 20000,
            max_graph_bytes: 32 * 1024 * 1024,
        }
    }
}
#[derive(Deserialize, Serialize, ToSchema)]
pub struct GroupView {
    pub id: String,
    pub kind: Option<String>,
    pub name: Option<String>,
    pub image: Option<String>,
    pub curated: bool,
    pub taggers: i64,
    pub references: i64,
    pub members: Vec<GroupMemberView>,
}
#[derive(Deserialize, Serialize, ToSchema)]
pub struct GroupMemberView {
    pub resource_id: String,
    pub uri: String,
    pub site: String,
    pub occurrence: Option<crate::event::storage::OccurrenceRef>,
}
#[derive(Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub resource_id: String,
    pub occurrence: Option<crate::event::storage::OccurrenceRef>,
}
#[derive(Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Curation {
    pub members: Vec<Member>,
    pub name: Option<String>,
    pub image: Option<String>,
}
#[derive(Deserialize)]
struct Unit {
    unit_id: String,
    resource_id: String,
    occurrence_key: String,
    keys: Vec<crate::group_keys::GroupKey>,
}

pub async fn scores(tx: &mut Transaction<'_, Postgres>, ids: &[String]) -> Result<()> {
    sqlx::query(r#"WITH members AS (SELECT DISTINCT group_id,resource_id FROM group_unit WHERE group_id IN (SELECT group_id FROM group_unit WHERE resource_id=ANY($1))),
    facts AS (SELECT DISTINCT m.group_id,f.author,f.kind,f.object_path,f.label,f.created_at FROM members m JOIN effective_reference f USING(resource_id)),
    counts AS (SELECT m.group_id,count(DISTINCT f.author) FILTER(WHERE f.kind='tag') AS taggers,count(*) FILTER(WHERE f.kind='reply') AS replies,count(*) FILTER(WHERE f.kind IN ('embed','collection')) AS embeds,count(f.author) AS refs,max(f.created_at) AS newest FROM (SELECT DISTINCT group_id FROM members) m LEFT JOIN facts f USING(group_id) GROUP BY m.group_id)
    INSERT INTO group_score(group_id,score,updated_at,newest_at,taggers,references_count) SELECT group_id,(ln(1.0+taggers)+0.5*ln(1.0+replies)+0.3*ln(1.0+embeds))*exp(-ln(2.0)*greatest(0,coalesce(extract(epoch FROM(now()-newest)),0))/2592000.0),now(),newest,taggers,refs FROM counts
    ON CONFLICT(group_id) DO UPDATE SET score=excluded.score,updated_at=excluded.updated_at,newest_at=excluded.newest_at,taggers=excluded.taggers,references_count=excluded.references_count"#).bind(ids).execute(&mut **tx).await?;
    Ok(())
}
fn evidence(row: &Value) -> Vec<crate::group_keys::GroupKey> {
    let uri = row["uri"].as_str().unwrap_or("");
    let mut d = crate::model::ResourceMeta::new(uri, "groups", 0);
    d.group_keys = serde_json::from_value(row["keys"].clone()).unwrap_or_default();
    d.group_keys.retain(|k| {
        k.key.len() <= 2100
            && k.basis.len() <= 32
            && k.confidence.is_finite()
            && (0.0..=1.0).contains(&k.confidence)
    });
    d.group_keys.truncate(16);
    if row["occurrence"].is_null() && row["event"].is_null() {
        crate::group_keys::url_keys(&mut d, uri);
        crate::group_keys::add(&mut d, "canonical", uri);
        for alias in row["aliases"]
            .as_array()
            .into_iter()
            .flatten()
            .take(4)
            .filter_map(Value::as_str)
        {
            crate::group_keys::add(&mut d, "canonical", alias);
        }
        if let Some(osm) = crate::resolver::osm::parse_id(uri) {
            d.group_keys.push(crate::group_keys::GroupKey {
                key: format!("osm:{}", osm.key()),
                basis: "canonical".into(),
                confidence: 1.0,
            });
        }
    }
    if let Some(uid) = row["occurrence"]["uid"].as_str() {
        crate::group_keys::add(&mut d, "ics_uid", uid);
    }
    if let Some(id) = row["occurrence"]["event_resource_id"].as_str() {
        d.group_keys.push(crate::group_keys::GroupKey {
            key: format!("url:{id}"),
            basis: "canonical".into(),
            confidence: 0.8,
        });
    }
    // A recurring UID or shared event page must not merge different dates.
    // Every dated key uses the same scope, including typed IDs and page URLs.
    for key in &mut d.group_keys {
        if key.basis == "ics_uid" || row["start"].is_string() {
            key.key = format!("{}:{}", key.key, row["start"].as_str().unwrap_or("undated"));
        }
    }
    d.group_keys.push(crate::group_keys::GroupKey {
        key: format!("unit:{}", row["unit_id"].as_str().unwrap()),
        basis: "identity".into(),
        confidence: 1.0,
    });
    d.group_keys
}
pub async fn sync(tx: &mut Transaction<'_, Postgres>, id: &str, cfg: &GroupConfig) -> Result<()> {
    if !cfg.enabled {
        return Ok(());
    }
    // Caller owns search_revision before row locks. Group edits use the same ordering.
    let previous: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT group_id FROM group_unit WHERE resource_id=$1 AND group_id IS NOT NULL",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let mut stream=sqlx::query_scalar::<_,Value>("SELECT jsonb_build_object('unit_id',unit_id,'uri',uri,'keys',coalesce(doc->'group_keys','[]'),'aliases',coalesce(doc->'aliases','[]'),'occurrence',occurrence,'event',event,'start',to_char(start_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')) FROM search_unit WHERE id=$1 LIMIT $2").bind(id).bind((cfg.max_graph_units+1) as i64).fetch(&mut **tx);
    use futures_util::TryStreamExt;
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    while let Some(row) = stream.try_next().await? {
        bytes = bytes.saturating_add(row.to_string().len());
        ensure!(
            rows.len() < cfg.max_graph_units && bytes <= cfg.max_graph_bytes,
            "group input budget exceeded"
        );
        rows.push(row);
    }
    drop(stream);
    let units: Vec<Unit> = rows
        .iter()
        .map(|r| Unit {
            unit_id: r["unit_id"].as_str().unwrap().into(),
            resource_id: id.into(),
            occurrence_key: if r["occurrence"].is_null() {
                String::new()
            } else {
                serde_json::to_string(&(
                    r["occurrence"]["uid"].as_str().unwrap_or(""),
                    r["occurrence"]["recurrence_id"].as_str().unwrap_or(""),
                ))
                .unwrap()
            },
            keys: evidence(r),
        })
        .collect();
    let ids: Vec<_> = units.iter().map(|u| u.unit_id.clone()).collect();
    sqlx::query("DELETE FROM group_unit WHERE resource_id=$1 AND NOT(unit_id=ANY($2))")
        .bind(id)
        .bind(&ids)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM resource_group WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    for chunk in units.chunks(256) {
        let data:Vec<_>=chunk.iter().map(|u|json!({"unit":u.unit_id,"resource":u.resource_id,"occurrence":u.occurrence_key,"keys":u.keys})).collect();
        sqlx::query("INSERT INTO group_unit(unit_id,resource_id,occurrence_key) SELECT v->>'unit',v->>'resource',v->>'occurrence' FROM jsonb_array_elements($1) v ON CONFLICT(unit_id) DO UPDATE SET occurrence_key=excluded.occurrence_key").bind(json!(data)).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO resource_group(unit_id,resource_id,key,basis,confidence) SELECT v->>'unit',v->>'resource',k->>'key',k->>'basis',(k->>'confidence')::float8 FROM jsonb_array_elements($1) v CROSS JOIN LATERAL jsonb_array_elements(v->'keys') k ON CONFLICT(unit_id,key) DO NOTHING").bind(json!(data)).execute(&mut **tx).await?;
    }
    let keys: Vec<_> = units
        .iter()
        .flat_map(|u| u.keys.iter().map(|k| k.key.clone()))
        .collect();
    rebuild(tx, &ids, &keys, &previous, cfg).await?;
    sqlx::query("DELETE FROM group_dirty WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
#[derive(sqlx::FromRow)]
struct Edge {
    unit_id: String,
    resource_id: String,
    key: String,
    basis: String,
    confidence: f64,
    eligible: bool,
    group_id: Option<String>,
    override_id: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
}
fn root(parents: &mut [usize], i: usize) -> usize {
    if parents[i] != i {
        parents[i] = root(parents, parents[i]);
    }
    parents[i]
}
fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dlat = (b.0 - a.0).to_radians();
    let dlon = (b.1 - a.1).to_radians();
    let h = (dlat / 2.).sin().powi(2)
        + a.0.to_radians().cos() * b.0.to_radians().cos() * (dlon / 2.).sin().powi(2);
    6371000. * 2. * h.sqrt().min(1.).asin()
}
async fn rebuild(
    tx: &mut Transaction<'_, Postgres>,
    units: &[String],
    keys: &[String],
    previous: &[String],
    cfg: &GroupConfig,
) -> Result<()> {
    use futures_util::TryStreamExt;
    let max_edges = cfg.max_graph_units.saturating_mul(20);
    let mut stream=sqlx::query_as::<_,Edge>(r#"WITH relevant_keys AS (
      SELECT key FROM unnest($2::text[]) AS wanted(key) WHERE (SELECT count(*) FROM (SELECT 1 FROM resource_group k WHERE k.key=wanted.key LIMIT $4) sample)<$4
    ), affected AS (SELECT unnest($1::text[]) AS unit_id UNION SELECT unit_id FROM group_unit WHERE group_id=ANY($3) UNION SELECT u.unit_id FROM group_unit u WHERE u.group_id IN (SELECT DISTINCT k.group_id FROM resource_group k JOIN relevant_keys USING(key)) UNION SELECT k.unit_id FROM resource_group k JOIN relevant_keys USING(key)), degrees AS MATERIALIZED (
      SELECT wanted.key, (SELECT count(*) FROM (SELECT 1 FROM resource_group all_keys WHERE all_keys.key=wanted.key LIMIT $4) sample)<$4 AS eligible
      FROM (SELECT DISTINCT key FROM resource_group JOIN affected USING(unit_id)) wanted
    )
    SELECT k.unit_id,k.resource_id,k.key,k.basis,k.confidence,d.eligible,u.group_id,o.group_id AS override_id,ST_Y(s.point) AS lat,ST_X(s.point) AS lon FROM resource_group k JOIN affected a USING(unit_id) JOIN degrees d USING(key) JOIN group_unit u USING(unit_id) LEFT JOIN group_override o USING(unit_id) LEFT JOIN search_unit s USING(unit_id) ORDER BY k.unit_id,k.key LIMIT $5"#)
        .bind(units).bind(keys).bind(previous).bind((cfg.max_members+1) as i64).bind((max_edges+1) as i64).fetch(&mut **tx);
    let mut edges = Vec::new();
    let mut bytes = 0usize;
    while let Some(edge) = stream.try_next().await? {
        bytes = bytes.saturating_add(edge.key.len() + edge.basis.len() + 256);
        ensure!(
            bytes <= cfg.max_graph_bytes && edges.len() < max_edges,
            "group evidence budget exceeded"
        );
        edges.push(edge);
    }
    drop(stream);
    // At most bounded metadata per unit, no resource documents in this graph.
    let names: Vec<_> = edges
        .iter()
        .map(|e| e.unit_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ensure!(
        names.len() <= cfg.max_graph_units,
        "group unit budget exceeded"
    );
    let indices: BTreeMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(i, s)| (s.clone(), i))
        .collect();
    let mut parents: Vec<_> = (0..names.len()).collect();
    let mut sets: Vec<Vec<usize>> = (0..names.len()).map(|i| vec![i]).collect();
    let mut points = vec![None; names.len()];
    let mut overrides = vec![None; names.len()];
    let mut bykey: BTreeMap<String, Vec<(usize, f64)>> = BTreeMap::new();
    let mut resources = BTreeSet::new();
    let mut old: BTreeSet<String> = previous.iter().cloned().collect();
    for e in &edges {
        let i = indices[&e.unit_id];
        points[i] = e.lat.zip(e.lon);
        overrides[i] = e.override_id.clone();
        resources.insert(e.resource_id.clone());
        old.extend(e.group_id.clone());
        if !e.eligible && e.override_id.is_none() {
            continue;
        }
        let key = if let Some(g) = &e.override_id {
            format!("curated:{g}")
        } else {
            e.key.clone()
        };
        bykey.entry(key).or_default().push((
            i,
            if e.basis == "identity" {
                0.0
            } else {
                e.confidence
            },
        ));
    }
    let mut keys: Vec<_> = bykey.into_iter().collect();
    keys.sort_by(|a, b| {
        b.1.iter()
            .map(|v| v.1)
            .fold(0.0, f64::max)
            .total_cmp(&a.1.iter().map(|v| v.1).fold(0.0, f64::max))
            .then_with(|| a.0.cmp(&b.0))
    });
    for (key, mut entries) in keys {
        entries.sort_by_key(|e| e.0);
        entries.dedup_by_key(|e| e.0);
        if entries.len() > cfg.max_members {
            continue;
        }
        let mut first = None;
        for (i, _) in entries {
            let Some(a) = first else {
                first = Some(i);
                continue;
            };
            let a = root(&mut parents, a);
            let b = root(&mut parents, i);
            if a == b {
                continue;
            }
            let curated = key.starts_with("curated:");
            if sets[a].len() + sets[b].len() > cfg.max_members {
                continue;
            }
            if !curated
                && sets[a].iter().any(|i| {
                    sets[b].iter().any(|j| {
                        points[*i]
                            .zip(points[*j])
                            .is_some_and(|(a, b)| distance(a, b) > cfg.max_distance_m)
                    })
                })
            {
                continue;
            }
            let (a, b) = if a < b { (a, b) } else { (b, a) };
            parents[b] = a;
            let moved = std::mem::take(&mut sets[b]);
            sets[a].extend(moved);
            first = Some(a);
        }
    }
    let assignments: Vec<_> = (0..names.len())
        .map(|i| {
            let r = root(&mut parents, i);
            let gid = overrides[i]
                .clone()
                .unwrap_or_else(|| pubky_resource_id::resource_id(&format!("group:{}", names[r])));
            json!({"unit":names[i],"group":gid})
        })
        .collect();
    let groups: Vec<String> = assignments
        .iter()
        .filter_map(|v| v["group"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for chunk in groups.chunks(256) {
        sqlx::query("INSERT INTO \"group\"(id) SELECT unnest($1::text[]) ON CONFLICT DO NOTHING")
            .bind(chunk)
            .execute(&mut **tx)
            .await?;
    }
    for chunk in assignments.chunks(256) {
        sqlx::query("UPDATE group_unit u SET group_id=v->>'group' FROM jsonb_array_elements($1) v WHERE u.unit_id=v->>'unit'").bind(json!(chunk)).execute(&mut **tx).await?;
        sqlx::query("UPDATE resource_group k SET group_id=v->>'group' FROM jsonb_array_elements($1) v WHERE k.unit_id=v->>'unit'").bind(json!(chunk)).execute(&mut **tx).await?;
    }
    sqlx::query(r#"UPDATE "group" g SET best_unit=b.unit_id FROM (SELECT DISTINCT ON(u.group_id) u.group_id,s.unit_id FROM group_unit u JOIN search_unit s USING(unit_id) LEFT JOIN resource_score r ON r.resource_id=u.resource_id WHERE u.group_id=ANY($1) ORDER BY u.group_id,(s.point IS NOT NULL) DESC,(s.status='ok') DESC,(s.start_at IS NOT NULL) DESC,(s.name IS NOT NULL) DESC,(s.doc->>'image' IS NOT NULL) DESC,coalesce(r.score,0) DESC,s.unit_id) b WHERE g.id=b.group_id"#).bind(&groups).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM \"group\" g WHERE g.id=ANY($1) AND NOT g.curated AND NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.group_id=g.id)").bind(old.into_iter().collect::<Vec<_>>()).execute(&mut **tx).await?;
    scores(tx, &resources.into_iter().collect::<Vec<_>>()).await?;
    Ok(())
}
pub async fn drain(pool: &PgPool, cfg: &GroupConfig, limit: i64) -> Result<usize> {
    if !cfg.enabled {
        return Ok(0);
    }
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT resource_id FROM group_dirty ORDER BY resource_id LIMIT $1")
            .bind(limit)
            .fetch_all(pool)
            .await?;
    for id in &ids {
        let mut tx = pool.begin().await?;
        crate::store::lock_search_revision(&mut tx).await?;
        sync(&mut tx, id, cfg).await?;
        tx.commit().await?;
    }
    Ok(ids.len())
}
pub async fn curate(pool: &PgPool, id: &str, body: Curation, cfg: &GroupConfig) -> Result<Value> {
    ensure!(
        cfg.enabled && crate::model::valid_id(id),
        "invalid or disabled group"
    );
    ensure!(
        !body.members.is_empty() && body.members.len() <= cfg.max_members,
        "invalid group member count"
    );
    ensure!(
        body.name.as_ref().is_none_or(|s| s.len() <= 512)
            && body.image.as_ref().is_none_or(|s| s.len() <= 2048
                && url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https")
                    && u.username().is_empty()
                    && u.password().is_none())),
        "invalid curated fields"
    );
    let mut tx = pool.begin().await?;
    crate::store::lock_search_revision(&mut tx).await?;
    let mut units = vec![];
    let mut previous: Vec<String> =
        sqlx::query_scalar("SELECT group_id FROM group_override WHERE group_id=$1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    for m in &body.members {
        ensure!(crate::model::valid_id(&m.resource_id), "invalid member ID");
        sync(&mut tx, &m.resource_id, cfg).await?;
        let key = m
            .occurrence
            .as_ref()
            .map(|o| {
                serde_json::to_string(&(&o.uid, o.recurrence_id.as_deref().unwrap_or(""))).unwrap()
            })
            .unwrap_or_default();
        let row:Option<(String,Option<String>)>=sqlx::query_as("SELECT unit_id,group_id FROM group_unit WHERE resource_id=$1 AND occurrence_key=$2 AND ($2<>'' OR unit_id=resource_id)").bind(&m.resource_id).bind(key).fetch_optional(&mut *tx).await?;
        let (unit, old) = row.context("unknown group member")?;
        units.push(unit);
        previous.extend(old);
    }
    previous.push(id.into());
    sqlx::query("INSERT INTO \"group\"(id,name,image,curated) VALUES($1,$2,$3,true) ON CONFLICT(id) DO UPDATE SET name=excluded.name,image=excluded.image,curated=true").bind(id).bind(body.name).bind(body.image).execute(&mut *tx).await?;
    let removed: Vec<String> =
        sqlx::query_scalar("DELETE FROM group_override WHERE group_id=$1 RETURNING unit_id")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    sqlx::query("INSERT INTO group_override(unit_id,group_id) SELECT unnest($1::text[]),$2 ON CONFLICT(unit_id) DO UPDATE SET group_id=excluded.group_id").bind(&units).bind(id).execute(&mut *tx).await?;
    units.extend(removed);
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT key FROM resource_group WHERE unit_id=ANY($1)")
            .bind(&units)
            .fetch_all(&mut *tx)
            .await?;
    rebuild(&mut tx, &units, &keys, &previous, cfg).await?;
    let result = sqlx::query_scalar("SELECT group_card($1)")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}
