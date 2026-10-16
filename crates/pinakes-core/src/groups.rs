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
    pub max_members_per_origin: usize,
}
impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_members: 256,
            max_distance_m: 500.0,
            max_graph_units: 20000,
            max_graph_bytes: 32 * 1024 * 1024,
            max_members_per_origin: 16,
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
    pub attribution: Option<String>,
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
#[derive(Deserialize, Serialize)]
struct Unit {
    unit_id: String,
    resource_id: String,
    occurrence_key: String,
    keys: Vec<crate::group_keys::GroupKey>,
    origin: String,
    authoritative: Vec<String>,
    occurrence: Option<Value>,
    event: Option<Value>,
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
                target: None,
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
            target: row["event_uri"]
                .as_str()
                .and_then(crate::group_keys::origin),
        });
    }
    // Only recurrence identities use exact start time; ordinary event aliases can differ by seconds.
    for key in &mut d.group_keys {
        if key.basis == "ics_uid" {
            key.key = format!("{}:{}", key.key, row["start"].as_str().unwrap_or("undated"));
        }
    }
    d.group_keys.push(crate::group_keys::GroupKey {
        key: format!("unit:{}", row["unit_id"].as_str().unwrap()),
        basis: "identity".into(),
        confidence: 1.0,
        target: None,
    });
    d.group_keys
}
pub async fn sync(tx: &mut Transaction<'_, Postgres>, id: &str, cfg: &GroupConfig) -> Result<()> {
    if !cfg.enabled {
        return Ok(());
    }
    // Serialize group writers, while resource ingestion continues during computation.
    group_lock(tx).await?;
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT group_revision FROM resource WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    let mut revisions = BTreeMap::from([(id.to_owned(), revision)]);
    let previous: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT group_id FROM group_unit WHERE resource_id=$1 AND group_id IS NOT NULL",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let mut stream=sqlx::query_scalar::<_,Value>("SELECT jsonb_build_object('unit_id',unit_id,'uri',uri,'keys',coalesce(doc->'group_keys','[]'),'aliases',coalesce(doc->'aliases','[]'),'occurrence',occurrence,'event',event,'source',doc->>'source','lat',ST_Y(point),'lon',ST_X(point),'event_uri',(SELECT uri FROM resource WHERE id=occurrence->>'event_resource_id'),'start',to_char(start_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')) FROM search_unit WHERE id=$1 LIMIT $2").bind(id).bind((cfg.max_graph_units+1) as i64).fetch(&mut **tx);
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
            origin: match r["source"].as_str() {
                Some("isbn") => "openlibrary.org".into(),
                Some("doi") => "doi.org".into(),
                _ => crate::group_keys::origin(r["uri"].as_str().unwrap_or(""))
                    .unwrap_or_else(|| id.into()),
            },
            authoritative: {
                let mut own =
                    crate::model::ResourceMeta::new(r["uri"].as_str().unwrap_or(""), "groups", 0);
                crate::group_keys::url_keys(&mut own, r["uri"].as_str().unwrap_or(""));
                crate::group_keys::add(&mut own, "canonical", r["uri"].as_str().unwrap_or(""));
                let mut keys: Vec<_> = own.group_keys.into_iter().map(|k| k.key).collect();
                if let Some(osm) = crate::resolver::osm::parse_id(r["uri"].as_str().unwrap_or("")) {
                    keys.push(format!("osm:{}", osm.key()));
                }
                if matches!(r["source"].as_str(), Some("isbn" | "doi")) {
                    keys.extend(evidence(r).into_iter().map(|k| k.key));
                }
                keys
            },
            occurrence: r.get("occurrence").filter(|v| !v.is_null()).cloned(),
            event: r.get("event").filter(|v| !v.is_null()).cloned(),
        })
        .collect();
    let ids: Vec<_> = units.iter().map(|u| u.unit_id.clone()).collect();
    let keys: Vec<_> = units
        .iter()
        .flat_map(|u| u.keys.iter().map(|k| k.key.clone()))
        .collect();
    let fingerprint = blake3::hash(&serde_json::to_vec(&(
        &units,
        cfg,
        rows.iter()
            .map(|r| (&r["lat"], &r["lon"], &r["start"]))
            .collect::<Vec<_>>(),
    ))?)
    .to_hex()
    .to_string();
    let old_fingerprint: Option<String> =
        sqlx::query_scalar("SELECT fingerprint FROM group_input WHERE resource_id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if old_fingerprint.as_deref() == Some(&fingerprint) {
        fence(tx, &revisions).await?;
        refresh_best(tx, &previous).await?;
        sqlx::query("DELETE FROM group_dirty WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }
    let mut edges = load_edges(tx, &ids, &keys, &previous, cfg).await?;
    for e in &edges {
        revisions
            .entry(e.resource_id.clone())
            .or_insert(Some(e.revision));
    }
    let mut incoming = Vec::new();
    for (unit, row) in units.iter().zip(&rows) {
        let old = edges.iter().find(|e| e.unit_id == unit.unit_id);
        for k in &unit.keys {
            incoming.push(Edge {
                unit_id: unit.unit_id.clone(),
                resource_id: id.into(),
                revision: revision.unwrap_or(0),
                key: k.key.clone(),
                basis: k.basis.clone(),
                confidence: k.confidence,
                origin: unit.origin.clone(),
                target: k.target.clone(),
                trusted: unit.authoritative.contains(&k.key),
                occurrence: unit.occurrence.is_some(),
                start_at: row["start"].as_str().and_then(|v| v.parse().ok()),
                admitted_at: edges
                    .iter()
                    .find(|e| e.unit_id == unit.unit_id && e.key == k.key)
                    .map_or_else(chrono::Utc::now, |e| e.admitted_at),
                group_id: old.and_then(|e| e.group_id.clone()),
                override_id: old.and_then(|e| e.override_id.clone()),
                lat: row["lat"].as_f64(),
                lon: row["lon"].as_f64(),
            });
        }
    }
    edges.retain(|e| e.resource_id != id);
    edges.extend(incoming);
    let plan = plan(tx, edges, &previous, cfg).await?;
    fence(tx, &revisions).await?;
    sqlx::query("DELETE FROM group_unit WHERE resource_id=$1 AND NOT(unit_id=ANY($2))")
        .bind(id)
        .bind(&ids)
        .execute(&mut **tx)
        .await?;

    for chunk in units.chunks(256) {
        let data:Vec<_>=chunk.iter().map(|u|json!({"unit":u.unit_id,"resource":u.resource_id,"occurrence":u.occurrence_key,"keys":u.keys,"origin":u.origin,"authoritative":u.authoritative,"occurrence_ref":u.occurrence,"event_ref":u.event})).collect();
        sqlx::query("INSERT INTO group_unit(unit_id,resource_id,occurrence_key,occurrence,event) SELECT v->>'unit',v->>'resource',v->>'occurrence',nullif(v->'occurrence_ref','null'::jsonb),nullif(v->'event_ref','null'::jsonb) FROM jsonb_array_elements($1) v ON CONFLICT(unit_id) DO UPDATE SET occurrence_key=excluded.occurrence_key,occurrence=excluded.occurrence,event=excluded.event").bind(json!(data)).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO resource_group(unit_id,resource_id,key,basis,confidence,origin,target,trusted) SELECT v->>'unit',v->>'resource',k->>'key',k->>'basis',(k->>'confidence')::float8,v->>'origin',k->>'target',(v->'authoritative') ? (k->>'key') FROM jsonb_array_elements($1) v CROSS JOIN LATERAL jsonb_array_elements(v->'keys') k ON CONFLICT(unit_id,key) DO UPDATE SET basis=excluded.basis,confidence=excluded.confidence,origin=excluded.origin,target=excluded.target,trusted=excluded.trusted").bind(json!(data)).execute(&mut **tx).await?;
    }
    let retained: Vec<_> = units
        .iter()
        .flat_map(|u| u.keys.iter().map(|k| json!([u.unit_id, k.key])))
        .collect();
    sqlx::query("DELETE FROM resource_group k WHERE resource_id=$1 AND NOT EXISTS(SELECT 1 FROM jsonb_array_elements($2) v WHERE v->>0=k.unit_id AND v->>1=k.key)").bind(id).bind(json!(retained)).execute(&mut **tx).await?;
    apply(tx, plan).await?;
    sqlx::query("INSERT INTO group_input(resource_id,fingerprint) VALUES($1,$2) ON CONFLICT(resource_id) DO UPDATE SET fingerprint=excluded.fingerprint").bind(id).bind(fingerprint).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM group_dirty WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
#[derive(sqlx::FromRow)]
struct Edge {
    revision: i64,
    unit_id: String,
    resource_id: String,
    key: String,
    basis: String,
    confidence: f64,
    origin: String,
    target: Option<String>,
    trusted: bool,
    occurrence: bool,
    start_at: Option<chrono::DateTime<chrono::Utc>>,
    admitted_at: chrono::DateTime<chrono::Utc>,
    group_id: Option<String>,
    override_id: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
}
impl Edge {
    fn bytes(&self) -> usize {
        self.key.len()
            + self.basis.len()
            + self.origin.len()
            + self.target.as_ref().map_or(0, String::len)
            + 256
    }
}
fn root(parents: &mut [usize], mut i: usize) -> usize {
    while parents[i] != i {
        parents[i] = parents[parents[i]];
        i = parents[i];
    }
    i
}
fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dlat = (b.0 - a.0).to_radians();
    let dlon = (b.1 - a.1).to_radians();
    let h = (dlat / 2.).sin().powi(2)
        + a.0.to_radians().cos() * b.0.to_radians().cos() * (dlon / 2.).sin().powi(2);
    6371000. * 2. * h.sqrt().min(1.).asin()
}
async fn load_edges(
    tx: &mut Transaction<'_, Postgres>,
    units: &[String],
    keys: &[String],
    previous: &[String],
    cfg: &GroupConfig,
) -> Result<Vec<Edge>> {
    use futures_util::TryStreamExt;
    let max_edges = cfg.max_graph_units.saturating_mul(20);
    let mut stream=sqlx::query_as::<_,Edge>(r#"WITH wanted AS (
      SELECT DISTINCT picked.unit_id,picked.group_id FROM unnest($2::text[]) wanted(key) CROSS JOIN LATERAL
      (SELECT k.unit_id,k.group_id FROM resource_group k WHERE k.key=wanted.key ORDER BY k.trusted DESC,k.admitted_at,k.unit_id LIMIT $4) picked
    ), affected AS (SELECT unnest($1::text[]) AS unit_id UNION SELECT unit_id FROM group_unit WHERE group_id=ANY($3)
      UNION SELECT u.unit_id FROM group_unit u WHERE u.group_id IN(SELECT group_id FROM wanted) UNION SELECT unit_id FROM wanted)
    SELECT r.group_revision AS revision,k.unit_id,k.resource_id,k.key,k.basis,k.confidence,k.origin,k.target,k.trusted,k.admitted_at,
      u.occurrence IS NOT NULL AS occurrence,s.start_at,u.group_id,o.group_id AS override_id,ST_Y(s.point) AS lat,ST_X(s.point) AS lon
    FROM resource_group k JOIN affected a USING(unit_id) JOIN group_unit u USING(unit_id) JOIN resource r ON r.id=k.resource_id
    LEFT JOIN group_override o USING(unit_id) LEFT JOIN group_member_document s USING(unit_id)
    ORDER BY k.unit_id,k.key LIMIT $5"#)
        .bind(units).bind(keys).bind(previous).bind(cfg.max_members as i64).bind((max_edges+1) as i64).fetch(&mut **tx);
    let mut edges = Vec::new();
    let mut bytes = 0usize;
    while let Some(edge) = stream.try_next().await? {
        bytes = bytes.saturating_add(edge.bytes());
        ensure!(
            bytes <= cfg.max_graph_bytes && edges.len() < max_edges,
            "group evidence budget exceeded"
        );
        edges.push(edge);
    }
    drop(stream);
    Ok(edges)
}
struct Plan {
    assignments: Vec<Value>,
    groups: Vec<String>,
    old: Vec<String>,
    resources: Vec<String>,
}
async fn plan(
    tx: &mut Transaction<'_, Postgres>,
    edges: Vec<Edge>,
    previous: &[String],
    cfg: &GroupConfig,
) -> Result<Plan> {
    ensure!(
        edges.len() <= cfg.max_graph_units.saturating_mul(20)
            && edges.iter().map(|e| e.bytes()).sum::<usize>() <= cfg.max_graph_bytes,
        "group evidence budget exceeded"
    );
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
    let mut starts = vec![None; names.len()];
    let mut occurrences = vec![false; names.len()];
    let mut origins = vec![String::new(); names.len()];
    let mut resource_ids = vec![String::new(); names.len()];
    let mut claims = vec![BTreeSet::new(); names.len()];
    let mut overrides = vec![None; names.len()];
    let mut bykey: BTreeMap<String, Vec<(usize, &Edge)>> = BTreeMap::new();
    let mut resources = BTreeSet::new();
    let mut old: BTreeSet<String> = previous.iter().cloned().collect();
    for e in &edges {
        let i = indices[&e.unit_id];
        points[i] = e.lat.zip(e.lon);
        starts[i] = e.start_at;
        occurrences[i] = e.occurrence;
        origins[i] = e.origin.clone();
        resource_ids[i] = e.resource_id.clone();
        claims[i].insert(e.key.clone());
        overrides[i] = e.override_id.clone();
        resources.insert(e.resource_id.clone());
        old.extend(e.group_id.clone());
        let key = if let Some(g) = &e.override_id {
            format!("curated:{g}")
        } else {
            e.key.clone()
        };
        bykey.entry(key).or_default().push((i, e));
    }
    let mut keys: Vec<_> = bykey.into_iter().collect();
    keys.sort_by(|a, b| {
        b.1.iter()
            .map(|v| v.1.confidence)
            .fold(0.0, f64::max)
            .total_cmp(&a.1.iter().map(|v| v.1.confidence).fold(0.0, f64::max))
            .then_with(|| a.0.cmp(&b.0))
    });
    for (key, mut entries) in keys {
        let curated = key.starts_with("curated:");
        entries.sort_by(|a, b| {
            b.1.trusted
                .cmp(&a.1.trusted)
                .then(a.1.admitted_at.cmp(&b.1.admitted_at))
                .then(a.0.cmp(&b.0))
        });
        let mut seen = BTreeSet::new();
        let mut per_origin = BTreeMap::<&str, usize>::new();
        entries.retain(|(i, e)| {
            if !seen.insert(*i) {
                return false;
            }
            let n = per_origin.entry(&e.origin).or_default();
            *n += 1;
            curated || *n <= cfg.max_members_per_origin
        });
        // Keep established admissions. Excess new claimants never disable the key for everyone.
        entries.truncate(cfg.max_members);
        for (pos, (i, ie)) in entries.iter().enumerate() {
            for (j, je) in &entries[..pos] {
                let reciprocal = claims[*i].contains(&format!("url:{}", resource_ids[*j]))
                    && claims[*j].contains(&format!("url:{}", resource_ids[*i]));
                let same_origin = origins[*i] == origins[*j];
                let url_owned = |e: &Edge| e.target.as_deref() == Some(e.origin.as_str());
                let allowed = curated
                    || reciprocal
                    || (ie.trusted && je.trusted)
                    || (same_origin
                        && (!key.starts_with("url:") || (url_owned(ie) && url_owned(je))));
                if !allowed {
                    continue;
                }
                let mut a = root(&mut parents, *i);
                let mut b = root(&mut parents, *j);
                if a == b || sets[a].len() + sets[b].len() > cfg.max_members {
                    continue;
                }
                if !curated
                    && sets[a].iter().any(|i| {
                        sets[b].iter().any(|j| {
                            points[*i]
                                .zip(points[*j])
                                .is_some_and(|(a, b)| distance(a, b) > cfg.max_distance_m)
                                || ((occurrences[*i] || occurrences[*j])
                                    && starts[*i]
                                        .zip(starts[*j])
                                        .is_some_and(|(a, b)| (a - b).num_seconds().abs() > 300))
                        })
                    })
                {
                    continue;
                }
                // Iterative path compression and union by size bound both stack and lookup depth.
                if sets[a].len() < sets[b].len() {
                    std::mem::swap(&mut a, &mut b);
                }
                parents[b] = a;
                let moved = std::mem::take(&mut sets[b]);
                sets[a].extend(moved);
            }
        }
    }
    let roots: Vec<_> = (0..names.len()).map(|i| root(&mut parents, i)).collect();
    let mut automatic = BTreeMap::new();
    for r in &roots {
        if sets[*r].len() > 1 {
            let first = sets[*r].iter().map(|i| &names[*i]).min().unwrap();
            automatic
                .entry(*r)
                .or_insert_with(|| pubky_resource_id::resource_id(&format!("group:auto:{first}")));
        }
    }
    // Operators may curate the ID they just saw. Never reuse that ID for the unpinned remainder.
    for salt in 0..=16 {
        let reserved: Vec<String> =
            sqlx::query_scalar("SELECT id FROM \"group\" WHERE curated AND id=ANY($1)")
                .bind(automatic.values().cloned().collect::<Vec<_>>())
                .fetch_all(&mut **tx)
                .await?;
        if reserved.is_empty() {
            break;
        }
        ensure!(salt < 16, "automatic group namespace exhausted");
        for (r, gid) in &mut automatic {
            if reserved.contains(gid) {
                let first = sets[*r].iter().map(|i| &names[*i]).min().unwrap();
                *gid = pubky_resource_id::resource_id(&format!("group:auto:{first}:{salt}"));
            }
        }
    }
    let assignments: Vec<_> = (0..names.len())
        .map(|i| {
            let gid = overrides[i]
                .clone()
                .or_else(|| automatic.get(&roots[i]).cloned());
            json!({"unit":names[i],"group":gid})
        })
        .collect();
    let groups: Vec<String> = assignments
        .iter()
        .filter_map(|v| v["group"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Plan {
        assignments,
        groups,
        old: old.into_iter().collect(),
        resources: resources.into_iter().collect(),
    })
}
#[derive(Debug)]
struct SnapshotChanged;
impl std::fmt::Display for SnapshotChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("group input changed; retry immediately")
    }
}
impl std::error::Error for SnapshotChanged {}
async fn group_lock(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    // This lock must precede the final search revision lock, including in curation.
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(73491620)")
        .fetch_one(&mut **tx)
        .await?;
    if !acquired {
        return Err(SnapshotChanged.into());
    }
    Ok(())
}
async fn fence(
    tx: &mut Transaction<'_, Postgres>,
    revisions: &BTreeMap<String, Option<i64>>,
) -> Result<()> {
    crate::store::lock_search_revision(tx).await?;
    let current: BTreeMap<String, i64> =
        sqlx::query_as("SELECT id,group_revision FROM resource WHERE id=ANY($1)")
            .bind(revisions.keys().collect::<Vec<_>>())
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .collect();
    if revisions
        .iter()
        .any(|(id, revision)| current.get(id) != revision.as_ref())
    {
        return Err(SnapshotChanged.into());
    }
    Ok(())
}
async fn apply(tx: &mut Transaction<'_, Postgres>, plan: Plan) -> Result<()> {
    let Plan {
        assignments,
        groups,
        old,
        resources,
    } = plan;
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
    refresh_best(tx, &groups).await?;
    sqlx::query("DELETE FROM \"group\" g WHERE g.id=ANY($1) AND NOT g.curated AND NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.group_id=g.id)").bind(old).execute(&mut **tx).await?;
    scores(tx, &resources).await?;
    Ok(())
}
async fn refresh_best(tx: &mut Transaction<'_, Postgres>, groups: &[String]) -> Result<()> {
    sqlx::query(r#"UPDATE "group" g SET best_unit=b.unit_id,best_resource_id=b.resource_id,best_occurrence_key=b.occurrence_key,kind=b.type,member_count=(SELECT count(*) FROM group_unit u WHERE u.group_id=g.id) FROM (SELECT DISTINCT ON(u.group_id) u.group_id,s.unit_id,u.resource_id,u.occurrence_key,s.type FROM group_unit u JOIN group_member_document s USING(unit_id) LEFT JOIN resource_score r ON r.resource_id=u.resource_id WHERE u.group_id=ANY($1) ORDER BY u.group_id,(s.point IS NOT NULL) DESC,(s.status='ok') DESC,(s.start_at IS NOT NULL) DESC,(s.name IS NOT NULL) DESC,(s.doc->>'image' IS NOT NULL) DESC,coalesce(r.score,0) DESC,s.unit_id) b WHERE g.id=b.group_id"#).bind(groups).execute(&mut **tx).await?;
    Ok(())
}
async fn rebuild(
    tx: &mut Transaction<'_, Postgres>,
    units: &[String],
    keys: &[String],
    previous: &[String],
    cfg: &GroupConfig,
) -> Result<()> {
    let edges = load_edges(tx, units, keys, previous, cfg).await?;
    let plan = plan(tx, edges, previous, cfg).await?;
    apply(tx, plan).await
}
/// Failures are parked independently of resource resolution. The dirty row survives for retry.
pub async fn reconcile(pool: &PgPool, id: &str, cfg: &GroupConfig) -> bool {
    if !cfg.enabled {
        return false;
    }
    let mut result = Ok(());
    for _ in 0..3 {
        result = async {
            let mut tx = pool.begin().await?;
            sqlx::query("SET LOCAL statement_timeout='2s'")
                .execute(&mut *tx)
                .await?;
            sync(&mut tx, id, cfg).await?;
            tx.commit().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if !result.as_ref().is_err_and(|e| e.is::<SnapshotChanged>()) {
            break;
        }
        tokio::task::yield_now().await;
    }
    if let Err(error) = result {
        if error.is::<SnapshotChanged>() {
            sqlx::query("INSERT INTO group_dirty(resource_id) SELECT id FROM resource WHERE id=$1 ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now(),last_error=NULL")
                .bind(id).execute(pool).await.ok();
            return false;
        }
        tracing::warn!(resource_id=id,error=%error,"group_failed; reconciliation deferred");
        let parked=sqlx::query("INSERT INTO group_dirty(resource_id,attempts,last_error,next_attempt_at) SELECT id,1,'group_failed',now()+interval '1 minute' FROM resource WHERE id=$1 ON CONFLICT(resource_id) DO UPDATE SET attempts=group_dirty.attempts+1,last_error='group_failed',next_attempt_at=now()+least(3600,60*power(2,least(group_dirty.attempts,6))) * interval '1 second'").bind(id).execute(pool).await;
        if let Err(error) = parked {
            tracing::warn!(error=%error,"failed to park grouping retry");
        }
        return false;
    }
    true
}
pub async fn drain(pool: &PgPool, cfg: &GroupConfig, limit: i64) -> Result<usize> {
    if !cfg.enabled {
        return Ok(0);
    }
    let ids: Vec<String> = sqlx::query_scalar("SELECT resource_id FROM group_dirty WHERE next_attempt_at<=now() ORDER BY next_attempt_at,resource_id LIMIT $1")
        .bind(limit.clamp(1,100)).fetch_all(pool).await?;
    for id in &ids {
        reconcile(pool, id, cfg).await;
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
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    group_lock(&mut tx).await?;
    crate::store::lock_search_revision(&mut tx).await?;
    let mut units = vec![];
    let mut previous: Vec<String> =
        sqlx::query_scalar("SELECT group_id FROM group_override WHERE group_id=$1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    for m in &body.members {
        ensure!(crate::model::valid_id(&m.resource_id), "invalid member ID");
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
