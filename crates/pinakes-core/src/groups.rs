use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GroupConfig {
    pub enabled: bool,
    pub cross_container_occurrences: CrossContainerOccurrences,
    /// Recording cards name other recordings that are possibly the same one (`maybe_same`,
    /// `same_recording`): `on` or `off`. Not a grouping setting (`policy`).
    pub same_recording_hint: crate::config::SameEventHint,
    #[serde(skip)]
    pub max_members: usize,
    #[serde(skip)]
    pub max_distance_m: f64,
    #[serde(skip)]
    pub max_graph_units: usize,
    #[serde(skip)]
    pub max_graph_bytes: usize,
    /// Compatibility checks one key may make; `None` derives it from `max_members` (`join`).
    /// Not a config key: tests set it.
    #[serde(skip)]
    pub check_budget: Option<usize>,
    /// Stopped runs on unchanged input after which a component settles (`sync`). Not a config
    /// key: tests set it.
    #[serde(skip)]
    pub settle_after_stops: u32,
}
impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cross_container_occurrences: CrossContainerOccurrences::Off,
            same_recording_hint: crate::config::SameEventHint::On,
            max_members: 256,
            max_distance_m: 500.0,
            max_graph_units: 20000,
            max_graph_bytes: 32 * 1024 * 1024,
            check_budget: None,
            settle_after_stops: 24,
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrossContainerOccurrences {
    Off,
    Corroborated,
}
impl GroupConfig {
    /// Every setting that changes grouping results, for change detection: the file form leaves
    /// the budgets to `[limits]`, so it is not serialized for this.
    pub fn policy(&self) -> Value {
        serde_json::json!({
            "enabled": self.enabled,
            "cross_container_occurrences": self.cross_container_occurrences,
            "max_members": self.max_members,
            "max_distance_m": self.max_distance_m,
            "max_graph_units": self.max_graph_units,
            "max_graph_bytes": self.max_graph_bytes,
        })
    }
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
}
/// The grouping keys of one unit. Groups come only from evidence that cannot be spoofed:
/// - `cite:{page}:{doi:…|isbn:…}`: an identifier card names `page` as its landing page (its
///   registry record does) and the page cites that identifier in its head. Open Library's own
///   edition pages need no citation: the registry builds them from its own record.
/// - `ics_feed:{feed}:{uid hash}`: one ICS UID within one feed's occurrences. The UID is hashed,
///   so no UID text can spell another feed's key. `ics_uid:{uid hash}` is recorded too, also by
///   an event page for its own calendar file's UID (only a file on the page's own origin gives
///   one); it joins across feeds only in the corroborated mode (`join`).
///
/// - `hub:{key}`: an exact key a source vouches for (`also_on`: a MusicBrainz MBID, a Wikidata
///   Q-id), or that named this resource in another's exact lookup (a keyed derived link). Two
///   links with the same key are one thing on two services. A group never holds two
///   values of a single-object namespace ([`single_object`], and those the database remembers
///   from sources since removed, `single_object_namespace`): a join that would is refused.
///
/// A point (`geo:` or a map link) never groups.
fn evidence(row: &Value) -> Vec<String> {
    let id = row["resource_id"].as_str().unwrap_or("");
    let uri = row["uri"].as_str().unwrap_or("");
    let stored: Vec<crate::group_keys::GroupKey> =
        serde_json::from_value::<Vec<crate::group_keys::GroupKey>>(row["keys"].clone())
            .unwrap_or_default()
            .into_iter()
            .filter(crate::group_keys::valid_stored)
            .collect();
    let mut keys = BTreeSet::new();
    if let Some(uid) = row["occurrence"]["uid"].as_str() {
        keys.insert(format!("ics_feed:{id}:{}", uid_hash(uid)));
        keys.insert(format!("ics_uid:{}", uid_hash(uid)));
        return keys.into_iter().collect();
    }
    if row["source"] == "geo" {
        return vec![];
    }
    // Events group only with events: an event page's hub keys name what it is about (a concert's
    // performer, `mb:artist:…`), never another copy of the event, so they are no evidence.
    let event = row["type_path"]
        .as_array()
        .is_some_and(|path| path.iter().any(|t| t == "Event"));
    for key in row["hub_keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|_| !event)
    {
        if let Some(key) = key.as_str().filter(|k| crate::also_on::valid_key(k)) {
            keys.insert(format!("hub:{key}"));
        }
    }
    for k in stored.iter().filter(|k| k.basis == "ics_uid") {
        if let Some(uid) = k.key.strip_prefix("ics_uid:") {
            keys.insert(format!("ics_uid:{}", uid_hash(uid)));
        }
    }
    if matches!(row["source"].as_str(), Some("doi" | "isbn")) {
        // The card's own identifier comes from its URI (an alias root: `doi:…` or `isbn:…`).
        let own = if let Some(doi) = crate::resolver::cards::doi(uri) {
            format!("doi:{doi}")
        } else if let Some(isbn) = crate::resolver::cards::isbn(uri) {
            format!("isbn:{}", crate::resolver::cards::isbn13(&isbn))
        } else {
            return keys.into_iter().collect();
        };
        for k in stored.iter().filter(|k| k.basis == "registry") {
            // The DOI proxy is the identifier itself, never a landing page.
            let Some(target) = k
                .target
                .as_deref()
                .filter(|t| !crate::resolver::cards::registry_proxy(t))
            else {
                continue;
            };
            let page = k.key.trim_start_matches("url:");
            keys.insert(format!("cite:{page}:{own}"));
            // Open Library's own edition pages need no citation, so this key carries no
            // identifier: only an ISBN card writes it, and it joins only the page (`plan`).
            if open_library_edition(target) && own.starts_with("isbn:") {
                keys.insert(format!("cite:{page}:openlibrary"));
            }
        }
    } else {
        for k in stored
            .iter()
            .filter(|k| matches!(k.basis.as_str(), "cites_doi" | "cites_isbn"))
        {
            keys.insert(format!("cite:{id}:{}", k.key));
        }
        if open_library_edition(uri) {
            keys.insert(format!("cite:{id}:openlibrary"));
        }
    }
    keys.into_iter().collect()
}
/// The hub key namespaces that name one object, so a group holds at most one of their values: a
/// MusicBrainz recording, release, release group or artist, a Wikidata item, an IMDb title, an
/// Open Library edition or work, a podcast's GUID or Apple Podcasts ID, an OSM object, a DOI; and
/// those each also-on source declares (`also_on::Source::single_namespaces`: a Discogs object, an
/// arXiv paper, a package, a game…). ISRCs, UPCs and ISBNs may be several (one recording has many
/// ISRCs, one book several ISBNs). `configure` writes them all to `single_object_namespace`, so
/// a source's rule outlives the source: its keys stay up to their lifetime after it is removed,
/// and `sync` reads the table beside this list.
const SINGLE_OBJECT: &[&str] = &[
    "mb:recording",
    "mb:release",
    "mb:release-group",
    "mb:artist",
    "wikidata",
    "imdb",
    "openlibrary:edition",
    "openlibrary:work",
    "podcast:guid",
    "podcast:apple",
    "osm",
    "doi",
];
/// [`SINGLE_OBJECT`] and every also-on source's own.
fn single_object_namespaces() -> &'static [&'static str] {
    static ALL: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut all = SINGLE_OBJECT.to_vec();
        for source in crate::also_on::registered() {
            all.extend(source.single_namespaces());
        }
        all
    })
}
/// A hub key's namespace and value when the namespace names one object
/// ([`single_object_namespaces`]). The value may itself hold `:` (a DOI).
fn single_object(key: &str) -> Option<(&str, &str)> {
    let key = key.strip_prefix("hub:")?;
    single_object_namespaces().iter().find_map(|namespace| {
        let value = key.strip_prefix(namespace)?.strip_prefix(':')?;
        Some((*namespace, value))
    })
}
/// [`single_object`], and over `remembered` too: the namespaces the database holds
/// (`single_object_namespace`), those of sources since removed among them.
fn single_object_with<'k>(remembered: &'k [String], key: &'k str) -> Option<(&'k str, &'k str)> {
    single_object(key).or_else(|| {
        let key = key.strip_prefix("hub:")?;
        remembered.iter().find_map(|namespace| {
            let value = key.strip_prefix(namespace.as_str())?.strip_prefix(':')?;
            Some((namespace.as_str(), value))
        })
    })
}
/// The single-object namespaces the database remembers (`configure` writes them).
async fn remembered_namespaces(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT namespace FROM single_object_namespace ORDER BY namespace")
            .fetch_all(&mut **tx)
            .await?,
    )
}
/// The resource ID of the organiser calendar an event page's platform names for it
/// (`event::adapters::container_feed`: Meetup's group calendar), on the page's own origin.
fn organiser_calendar(uri: &str) -> Option<String> {
    let feed = crate::event::adapters::container_feed(crate::event::adapters::detect(uri)?, uri)?;
    Some(crate::model::identity(&feed).ok()?.0)
}
/// An ICS UID as a key segment: free text (NIP-52 coordinates, URNs, URLs) never decides the
/// key's kind or names another feed.
fn uid_hash(uid: &str) -> String {
    blake3::hash(uid.as_bytes()).to_hex()[..32].to_owned()
}
fn open_library_edition(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        matches!(
            u.host_str(),
            Some("openlibrary.org" | "www.openlibrary.org")
        ) && u.path().starts_with("/books/")
    })
}
/// A unit of a resource (itself, or one occurrence): its ID, occurrence key, occurrence
/// reference, and grouping keys.
type Unit = (String, String, Option<Value>, Vec<String>);
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
    let rows: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('unit_id',unit_id,'resource_id',id,'uri',uri,'keys',coalesce(doc->'group_keys','[]'),'type_path',type_path,'hub_keys',coalesce((SELECT jsonb_agg(h.key ORDER BY h.key) FROM (SELECT k.key FROM resource_key k WHERE k.resource_id=search_unit.id AND k.expires_at>now() AND k.source NOT IN (SELECT source FROM also_on_source_off) UNION SELECT l.key FROM derived_link l WHERE l.link_id=search_unit.id AND l.key IS NOT NULL AND l.expires_at>now() AND l.source NOT IN (SELECT source FROM also_on_source_off)) h),'[]'),'occurrence',occurrence,'source',doc->>'source','lat',ST_Y(point),'lon',ST_X(point),'start',start_at,'trigrams',show_trgm(array_to_string(tsvector_to_array(to_tsvector('simple',coalesce(name,''))),' '))) FROM search_unit WHERE id=$1 LIMIT $2")
        .bind(id)
        .bind((cfg.max_graph_units + 1) as i64)
        .fetch_all(&mut **tx)
        .await?;
    ensure!(
        rows.len() <= cfg.max_graph_units,
        "group input budget exceeded"
    );
    let units: Vec<Unit> = rows
        .iter()
        .map(|r| {
            (
                r["unit_id"].as_str().unwrap_or_default().to_owned(),
                if r["occurrence"].is_null() {
                    String::new()
                } else {
                    serde_json::to_string(&(
                        r["occurrence"]["uid"].as_str().unwrap_or(""),
                        r["occurrence"]["recurrence_id"].as_str().unwrap_or(""),
                    ))
                    .unwrap()
                },
                r.get("occurrence").filter(|v| !v.is_null()).cloned(),
                evidence(r),
            )
        })
        .collect();
    let ids: Vec<String> = units.iter().map(|u| u.0.clone()).collect();
    let keys: Vec<String> = units.iter().flat_map(|u| u.3.clone()).collect();
    let facts: Vec<_> = rows
        .iter()
        .map(|r| (&r["lat"], &r["lon"], &r["start"], &r["trigrams"]))
        .collect();
    let fingerprint = blake3::hash(&serde_json::to_vec(&(&units, facts, cfg.policy()))?)
        .to_hex()
        .to_string();
    let old_fingerprint: Option<String> =
        sqlx::query_scalar("SELECT fingerprint FROM group_input WHERE resource_id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if old_fingerprint.as_deref() == Some(&fingerprint) {
        fence(tx, &revisions).await?;
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
    check_revisions(tx, &revisions).await?;
    // This resource's units take their new keys and facts, keeping each key's admission time.
    let mut incoming = Vec::new();
    for ((unit, _, occurrence, unit_keys), row) in units.iter().zip(&rows) {
        let mut trigrams: Vec<String> =
            serde_json::from_value(row["trigrams"].clone()).unwrap_or_default();
        trigrams.sort();
        for key in unit_keys.iter().cloned().chain(
            // A unit without keys still takes part, so its group (if any) is recomputed.
            unit_keys.is_empty().then(|| format!("unit:{unit}")),
        ) {
            let old = edges.iter().find(|e| &e.unit_id == unit);
            incoming.push(Edge {
                unit_id: unit.clone(),
                resource_id: id.into(),
                revision: revision.unwrap_or(0),
                admitted_at: edges
                    .iter()
                    .find(|e| &e.unit_id == unit && e.key == key)
                    .map_or_else(chrono::Utc::now, |e| e.admitted_at),
                key,
                group_id: old.and_then(|e| e.group_id.clone()),
                override_id: old.and_then(|e| e.override_id.clone()),
                trigrams: trigrams.clone(),
                start_at: row["start"].as_str().and_then(|v| v.parse().ok()),
                lat: row["lat"].as_f64(),
                lon: row["lon"].as_f64(),
                occurrence: occurrence.is_some(),
                uri: if occurrence.is_some() {
                    String::new()
                } else {
                    row["uri"].as_str().unwrap_or_default().to_owned()
                },
            });
        }
    }
    edges.retain(|e| e.resource_id != id);
    edges.extend(incoming);
    // A run that stopped on the budget left the key to start from.
    let (resume, stopped_fingerprint, stopped_runs): (Option<String>, Option<String>, i32) =
        sqlx::query_as("SELECT resume_key,stopped_fingerprint,stopped_runs FROM group_dirty WHERE resource_id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or((None, None, 0));
    let plan = plan(tx, edges, &previous, cfg, resume.as_deref()).await?;
    fence(tx, &revisions).await?;
    sqlx::query("DELETE FROM group_unit WHERE resource_id=$1 AND NOT(unit_id=ANY($2))")
        .bind(id)
        .bind(&ids)
        .execute(&mut **tx)
        .await?;
    for chunk in units.chunks(256) {
        let data: Vec<_> = chunk
            .iter()
            .map(|u| json!({"unit":u.0,"resource":id,"occurrence":u.1,"occurrence_ref":u.2,"keys":u.3}))
            .collect();
        sqlx::query("INSERT INTO group_unit(unit_id,resource_id,occurrence_key,occurrence) SELECT v->>'unit',v->>'resource',v->>'occurrence',nullif(v->'occurrence_ref','null'::jsonb) FROM jsonb_array_elements($1) v ON CONFLICT(unit_id) DO UPDATE SET occurrence_key=excluded.occurrence_key,occurrence=excluded.occurrence").bind(json!(data)).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO resource_group(unit_id,resource_id,key) SELECT v->>'unit',v->>'resource',k FROM jsonb_array_elements($1) v CROSS JOIN LATERAL jsonb_array_elements_text(v->'keys') k ON CONFLICT(unit_id,key) DO NOTHING").bind(json!(data)).execute(&mut **tx).await?;
    }
    let retained: Vec<_> = units
        .iter()
        .flat_map(|u| u.3.iter().map(|k| json!([u.0, k])))
        .collect();
    sqlx::query("DELETE FROM resource_group k WHERE resource_id=$1 AND NOT EXISTS(SELECT 1 FROM jsonb_array_elements($2) v WHERE v->>0=k.unit_id AND v->>1=k.key)").bind(id).bind(json!(retained)).execute(&mut **tx).await?;
    let (resume, parked) = (plan.resume.clone(), plan.parked as i32);
    let conflicts = i32::try_from(plan.conflicts).unwrap_or(i32::MAX);
    apply(tx, plan).await?;
    if let Some(resume) = resume {
        // Stopped runs on unchanged input in a row.
        let streak = if stopped_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            stopped_runs.saturating_add(1)
        } else {
            1
        };
        if streak < cfg.settle_after_stops as i32 {
            // Not settled: no fingerprint, so the next run plans again, from the key this one
            // stopped at; the reason is recorded.
            sqlx::query("INSERT INTO group_dirty(resource_id,attempts,last_error,next_attempt_at,resume_key,stopped_fingerprint,stopped_runs) VALUES($1,1,'work_budget',now()+interval '1 hour',$2,$3,$4) ON CONFLICT(resource_id) DO UPDATE SET attempts=group_dirty.attempts+1,last_error='work_budget',next_attempt_at=now()+interval '1 hour',resume_key=$2,stopped_fingerprint=$3,stopped_runs=$4")
                .bind(id)
                .bind(resume)
                .bind(&fingerprint)
                .bind(streak)
                .execute(&mut **tx)
                .await?;
            return Ok(());
        }
        // Too large for any run, and its input does not change: settle until it does, with the
        // groups the last run left (`settled_stopped`, counted in metrics).
        sqlx::query("INSERT INTO group_input(resource_id,fingerprint,parked_keys,settled_stopped,key_conflicts) VALUES($1,$2,$3,true,$4) ON CONFLICT(resource_id) DO UPDATE SET fingerprint=excluded.fingerprint,parked_keys=excluded.parked_keys,settled_stopped=true,key_conflicts=excluded.key_conflicts").bind(id).bind(&fingerprint).bind(parked).bind(conflicts).execute(&mut **tx).await?;
        sqlx::query("DELETE FROM group_dirty WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }
    // Parked keys alone settle the run: planning the same input again parks them again.
    sqlx::query("INSERT INTO group_input(resource_id,fingerprint,parked_keys,settled_stopped,key_conflicts) VALUES($1,$2,$3,false,$4) ON CONFLICT(resource_id) DO UPDATE SET fingerprint=excluded.fingerprint,parked_keys=excluded.parked_keys,settled_stopped=false,key_conflicts=excluded.key_conflicts").bind(id).bind(fingerprint).bind(parked).bind(conflicts).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM group_dirty WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
#[derive(sqlx::FromRow)]
struct Edge {
    trigrams: Vec<String>,
    revision: i64,
    unit_id: String,
    resource_id: String,
    key: String,
    occurrence: bool,
    start_at: Option<chrono::DateTime<chrono::Utc>>,
    admitted_at: chrono::DateTime<chrono::Utc>,
    group_id: Option<String>,
    override_id: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    /// A page unit's resource URI, which names its organiser calendar (`Facts::calendars`);
    /// empty for an occurrence.
    uri: String,
}
impl Edge {
    fn bytes(&self) -> usize {
        self.key.len() + self.uri.len() + self.trigrams.iter().map(String::len).sum::<usize>() + 256
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
// pg_trgm show_trgm supplies sorted unique trigrams after the same 'simple' name
// normalization as search_text. Its similarity is intersection / union.
fn name_similarity(a: &[String], b: &[String]) -> f64 {
    let common = a.iter().filter(|v| b.binary_search(v).is_ok()).count();
    let union = a.len() + b.len() - common;
    if union == 0 {
        0.0
    } else {
        common as f64 / union as f64
    }
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
      (SELECT k.unit_id,k.group_id FROM resource_group k WHERE k.key=wanted.key ORDER BY k.admitted_at,k.unit_id LIMIT $4) picked
    ), affected AS (SELECT unnest($1::text[]) AS unit_id UNION SELECT unit_id FROM group_unit WHERE group_id=ANY($3)
      UNION SELECT u.unit_id FROM group_unit u WHERE u.group_id IN(SELECT group_id FROM wanted) UNION SELECT unit_id FROM wanted
      UNION SELECT unit_id FROM group_override WHERE group_id=ANY($3))
    SELECT show_trgm(array_to_string(tsvector_to_array(to_tsvector('simple',coalesce(s.name,''))),' ')) AS trigrams,r.group_revision AS revision,u.unit_id,u.resource_id,coalesce(k.key,'unit:'||u.unit_id) AS key,coalesce(k.admitted_at,u.admitted_at) AS admitted_at,
      u.occurrence IS NOT NULL AS occurrence,s.start_at,u.group_id,o.group_id AS override_id,ST_Y(s.point) AS lat,ST_X(s.point) AS lon,
      CASE WHEN u.occurrence IS NULL THEN r.uri ELSE '' END AS uri
    FROM affected a JOIN group_unit u USING(unit_id) LEFT JOIN resource_group k USING(unit_id) JOIN resource r ON r.id=u.resource_id
    LEFT JOIN group_override o USING(unit_id) LEFT JOIN group_member_document s USING(unit_id)
    ORDER BY u.unit_id,key LIMIT $5"#)
        .bind(units).bind(keys).bind(previous).bind(cfg.max_members as i64).bind((max_edges+1) as i64).fetch(&mut **tx);
    let mut edges = Vec::new();
    let mut bytes = 0usize;
    while let Some(mut edge) = stream.try_next().await? {
        edge.trigrams.sort();
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
/// The facts a compatibility check reads, one entry per unit.
struct Facts<'a> {
    names: &'a [String],
    points: &'a [Option<(f64, f64)>],
    starts: &'a [Option<chrono::DateTime<chrono::Utc>>],
    occurrences: &'a [bool],
    /// Per unit, a calendar's resource ID: an occurrence's feed, or the organiser calendar an
    /// event page's platform names for it (`organiser_calendar`).
    calendars: &'a [Option<String>],
    trigrams: &'a [Vec<String>],
    overrides: &'a [Option<String>],
    /// Per unit, its single-object hub keys as (namespace, value) ([`single_object`]).
    singles: &'a [Vec<(String, String)>],
}
/// The unions one run made, and the keys it did not finish.
struct Joined {
    parents: Vec<usize>,
    sets: Vec<Vec<usize>>,
    /// Parked keys and the keys after the stop: their units are unreached (`plan`).
    unfinished: BTreeSet<String>,
    parked: usize,
    /// The key where the run stopped, where the next run starts.
    resume: Option<String>,
    /// Per unit, whether `max_members` kept it apart from another: which side a unit lands on
    /// depends on key order, which a resumed run rotates (`plan` freezes their sets in a stopped
    /// run). One flag per unit, however many pairs were refused.
    capped: Vec<bool>,
    /// Joins refused because the two sets hold different values of one single-object namespace.
    conflicts: usize,
}
/// One step of a key's unions, undone in reverse when the key is parked.
enum Undo {
    Parent(usize, usize),
    Union {
        into: usize,
        from: usize,
        moved: usize,
    },
}
/// `root` that records the parent links its path compression rewrites.
fn logged_root(parents: &mut [usize], mut i: usize, log: &mut Vec<Undo>) -> usize {
    while parents[i] != i {
        log.push(Undo::Parent(i, parents[i]));
        parents[i] = parents[parents[i]];
        i = parents[i];
    }
    i
}
/// The budget per key: two passes over a full admitted component, with a floor for small groups.
fn check_budget(cfg: &GroupConfig) -> usize {
    cfg.check_budget.unwrap_or_else(|| {
        cfg.max_members
            .saturating_mul(cfg.max_members)
            .saturating_mul(2)
            .clamp(100_000, 2_000_000)
    })
}
/// Join units key by key. A key may make at most the budget's compatibility checks: one that
/// alone exceeds it is parked, its unions undone (its units keep their groups, `plan`), and the
/// later keys still run, so one oversized key never starves the rest. The run stops at four
/// budgets and records the key it stopped at; the next run starts there and wraps around, so
/// every key of a component too large for one run is reached in turn.
fn join(
    facts: &Facts,
    bykey: BTreeMap<String, Vec<(usize, chrono::DateTime<chrono::Utc>)>>,
    cfg: &GroupConfig,
    resume: Option<&str>,
) -> Joined {
    let budget = check_budget(cfg);
    let mut parents: Vec<_> = (0..facts.names.len()).collect();
    let mut sets: Vec<Vec<usize>> = (0..facts.names.len()).map(|i| vec![i]).collect();
    let mut keys: Vec<_> = bykey.into_iter().collect();
    let start = resume.map_or(0, |r| keys.partition_point(|(k, _)| k.as_str() < r));
    keys.rotate_left(start);
    let mut unfinished = BTreeSet::new();
    let mut parked = 0;
    let mut stopped = None;
    let mut evaluations = 0usize;
    let mut log = Vec::new();
    let mut capped = vec![false; facts.names.len()];
    let mut conflicts = 0;
    'keys: for (n, (key, entries)) in keys.iter_mut().enumerate() {
        let curated = key.starts_with("curated:");
        entries.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        entries.dedup_by_key(|e| e.0);
        // Curated members follow their curation only; automatic keys never pull them away.
        entries.retain(|(i, _)| curated || facts.overrides[*i].is_none());
        let mut cap_reported = entries.len() > cfg.max_members;
        if cap_reported {
            tracing::warn!(
                members = entries.len(),
                cap = cfg.max_members,
                "group size cap reached"
            );
        }
        entries.truncate(cfg.max_members);
        let cross_feed = key.starts_with("ics_uid:");
        // Two cards naming one edition page never join each other, only the page itself.
        let edition_page = key
            .strip_prefix("cite:")
            .and_then(|k| k.strip_suffix(":openlibrary"));
        let ics = cross_feed || key.starts_with("ics_feed:");
        if cross_feed && cfg.cross_container_occurrences != CrossContainerOccurrences::Corroborated
        {
            continue;
        }
        // Each member joins the earliest admitted member of the key it is compatible with.
        let mut spent = 0usize;
        log.clear();
        for (i, j) in (1..entries.len()).flat_map(|pos| (0..pos).map(move |prev| (pos, prev))) {
            let (i, j) = (entries[i].0, entries[j].0);
            if edition_page.is_some_and(|page| facts.names[i] != page && facts.names[j] != page) {
                continue;
            }
            let mut a = logged_root(&mut parents, i, &mut log);
            let mut b = logged_root(&mut parents, j, &mut log);
            if a == b {
                continue;
            }
            if sets[a].len() + sets[b].len() > cfg.max_members {
                if !cap_reported {
                    tracing::warn!(
                        members = sets[a].len() + sets[b].len(),
                        cap = cfg.max_members,
                        "group size cap reached"
                    );
                    cap_reported = true;
                }
                (capped[i], capped[j]) = (true, true);
                continue;
            }
            let cost = sets[a].len() * sets[b].len();
            spent = spent.saturating_add(cost);
            if spent > budget {
                tracing::warn!(
                    budget,
                    key,
                    "a grouping key exceeds the compatibility budget; its units keep their groups"
                );
                // Its partial unions go too: they could crowd a set past `max_members` for a
                // later key, or name a set, with joins this run never finished.
                for step in log.drain(..).rev() {
                    match step {
                        Undo::Parent(i, old) => parents[i] = old,
                        Undo::Union { into, from, moved } => {
                            let at = sets[into].len() - moved;
                            sets[from] = sets[into].split_off(at);
                            parents[from] = from;
                        }
                    }
                }
                parked += 1;
                unfinished.insert(key.clone());
                // Its checks count against the run, the refused pair included.
                evaluations = evaluations.saturating_add(cost);
                continue 'keys;
            }
            // Checks the run made: a parked key's last pair is never checked.
            evaluations = evaluations.saturating_add(cost);
            if evaluations > budget.saturating_mul(4) {
                tracing::warn!(
                    budget,
                    key,
                    "grouping compatibility budget exhausted; unreached units keep their groups"
                );
                stopped = Some(n);
                break 'keys;
            }
            let (points, starts, occurrences, calendars, trigrams, singles) = (
                facts.points,
                facts.starts,
                facts.occurrences,
                facts.calendars,
                facts.trigrams,
                facts.singles,
            );
            // One recording, item or artist per group: a wrong ID on a hub never chains two.
            let clash = !curated
                && sets[a].iter().any(|x| {
                    sets[b].iter().any(|y| {
                        singles[*x].iter().any(|(namespace, value)| {
                            singles[*y]
                                .iter()
                                .any(|(n, v)| n == namespace && v != value)
                        })
                    })
                });
            if clash {
                conflicts += 1;
                continue;
            }
            let compatible = curated
                || sets[a].iter().all(|x| {
                    sets[b].iter().all(|y| {
                        let near = |m: f64| {
                            points[*x]
                                .zip(points[*y])
                                .is_none_or(|(p, q)| distance(p, q) <= m)
                        };
                        let close_start = || {
                            starts[*x]
                                .zip(starts[*y])
                                .is_none_or(|(p, q)| (p - q).num_seconds().abs() <= 300)
                        };
                        // A UID joins two instances only when both are dated, within 300 s.
                        let dated = || starts[*x].is_some() && starts[*y].is_some();
                        // An event page and an occurrence of the organiser calendar its
                        // platform names for it, on the page's own origin: the page's UID came
                        // from its own calendar file (`group_keys`), so the organiser names the
                        // page's date in both. Two feeds never pair so.
                        let own_calendar = || {
                            occurrences[*x] != occurrences[*y]
                                && calendars[*x].is_some()
                                && calendars[*x] == calendars[*y]
                        };
                        near(cfg.max_distance_m)
                            && (!(occurrences[*x] || occurrences[*y]) || close_start())
                            && (!ics || (dated() && close_start()))
                            && (!cross_feed
                                || own_calendar()
                                || (points[*x].is_some()
                                    && points[*y].is_some()
                                    && near(500.0)
                                    && starts[*x].is_some()
                                    && starts[*y].is_some()
                                    && close_start()
                                    && name_similarity(&trigrams[*x], &trigrams[*y]) >= 0.5))
                    })
                });
            if !compatible {
                continue;
            }
            if sets[a].len() < sets[b].len() {
                std::mem::swap(&mut a, &mut b);
            }
            log.push(Undo::Parent(b, parents[b]));
            parents[b] = a;
            let moved = std::mem::take(&mut sets[b]);
            log.push(Undo::Union {
                into: a,
                from: b,
                moved: moved.len(),
            });
            sets[a].extend(moved);
        }
    }
    let resume = stopped.map(|n| {
        unfinished.extend(keys[n..].iter().map(|(k, _)| k.clone()));
        keys[n].0.clone()
    });
    Joined {
        parents,
        sets,
        unfinished,
        parked,
        resume,
        capped,
        conflicts,
    }
}
struct Plan {
    /// Keys that alone exceeded the budget and were skipped (`join`).
    parked: usize,
    /// The key where the run's budget ran out (`join`): the run is applied but not settled, and
    /// the next one starts there.
    resume: Option<String>,
    /// Joins refused for conflicting single-object keys (`join`).
    conflicts: usize,
    assignments: Vec<Value>,
    groups: Vec<String>,
    old: Vec<String>,
}
async fn plan(
    tx: &mut Transaction<'_, Postgres>,
    edges: Vec<Edge>,
    previous: &[String],
    cfg: &GroupConfig,
    resume: Option<&str>,
) -> Result<Plan> {
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
    let mut points = vec![None; names.len()];
    let mut starts = vec![None; names.len()];
    let mut occurrences = vec![false; names.len()];
    let mut calendars: Vec<Option<String>> = vec![None; names.len()];
    let mut read_calendar = vec![false; names.len()];
    let mut trigrams = vec![Vec::new(); names.len()];
    let mut overrides = vec![None; names.len()];
    let mut singles: Vec<Vec<(String, String)>> = vec![Vec::new(); names.len()];
    let remembered = remembered_namespaces(tx).await?;
    // The automatic group each unit is in now, so a group keeps its ID as it grows or merges.
    let mut current: Vec<Option<String>> = vec![None; names.len()];
    let mut bykey: BTreeMap<String, Vec<(usize, chrono::DateTime<chrono::Utc>)>> = BTreeMap::new();
    let mut old: BTreeSet<String> = previous.iter().cloned().collect();
    for e in &edges {
        let i = indices[&e.unit_id];
        points[i] = e.lat.zip(e.lon).or(points[i]);
        starts[i] = e.start_at.or(starts[i]);
        occurrences[i] |= e.occurrence;
        if !read_calendar[i] {
            read_calendar[i] = true;
            calendars[i] = if e.occurrence {
                Some(e.resource_id.clone())
            } else {
                organiser_calendar(&e.uri)
            };
        }
        if !e.trigrams.is_empty() {
            trigrams[i] = e.trigrams.clone();
        }
        overrides[i] = e.override_id.clone().or(overrides[i].take());
        if e.override_id.is_none() {
            current[i] = e.group_id.clone().or(current[i].take());
        }
        old.extend(e.group_id.clone());
        if let Some((namespace, value)) = single_object_with(&remembered, &e.key) {
            singles[i].push((namespace.to_owned(), value.to_owned()));
        }
        if !e.key.starts_with("unit:") {
            bykey
                .entry(e.key.clone())
                .or_default()
                .push((i, e.admitted_at));
        }
    }
    for (i, g) in overrides.iter().enumerate() {
        if let Some(g) = g {
            bykey
                .entry(format!("curated:{g}"))
                .or_default()
                .push((i, chrono::DateTime::UNIX_EPOCH));
        }
    }
    // The joins are CPU work: off the async workers, while this transaction holds the lock.
    let Joined {
        mut parents,
        sets,
        unfinished,
        parked,
        resume,
        capped,
        conflicts,
    } = {
        let owned = (
            names.clone(),
            points.clone(),
            starts.clone(),
            occurrences.clone(),
            calendars,
            trigrams.clone(),
            overrides.clone(),
            singles,
            cfg.clone(),
            resume.map(str::to_owned),
        );
        tokio::task::spawn_blocking(move || {
            let (
                names,
                points,
                starts,
                occurrences,
                calendars,
                trigrams,
                overrides,
                singles,
                cfg,
                resume,
            ) = owned;
            let facts = Facts {
                names: &names,
                points: &points,
                starts: &starts,
                occurrences: &occurrences,
                calendars: &calendars,
                trigrams: &trigrams,
                overrides: &overrides,
                singles: &singles,
            };
            join(&facts, bykey, &cfg, resume.as_deref())
        })
        .await?
    };
    let roots: Vec<_> = (0..names.len()).map(|i| root(&mut parents, i)).collect();
    // Past the budget, a unit with a key the run did not finish (parked, or after the stop) is
    // unreached, and every set holding one keeps each member's current group: the run cannot
    // tell what the unfinished joins would have made, so an established group neither splits
    // nor takes members unchecked against the units those joins would have brought.
    let unreached: BTreeSet<&String> = edges
        .iter()
        .filter(|e| unfinished.contains(&e.key))
        .map(|e| &e.unit_id)
        .collect();
    // A stopped run rotates its keys, so where `max_members` split a pair depends on the run:
    // the sets on both sides of such a pair keep their groups too, so membership never flips
    // from one run to the next.
    let stopped = resume.is_some();
    let frozen: BTreeSet<usize> = (0..names.len())
        .filter(|i| unreached.contains(&names[*i]) || (stopped && capped[*i]))
        .map(|i| roots[i])
        .collect();
    // An automatic ID is stable once formed: a set keeps the existing ID most of its members
    // carry (larger sets choose first, so a split leaves it with the larger part). A new group is
    // named after its smallest member unit.
    let mut automatic = BTreeMap::new();
    // IDs a frozen set keeps are not for the others: a set the run did form never adopts a group
    // whose other members it was not checked against.
    let mut taken: BTreeSet<String> = (0..names.len())
        .filter(|i| frozen.contains(&roots[*i]) && overrides[*i].is_none())
        .filter_map(|i| current[i].clone())
        .collect();
    // Sets named afresh, not after an ID their members carry.
    let mut fresh = BTreeSet::new();
    let mut formed: Vec<usize> = roots
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|r| sets[*r].len() > 1 && !frozen.contains(r))
        .collect();
    formed.sort_by_key(|r| {
        (
            std::cmp::Reverse(sets[*r].len()),
            sets[*r].iter().map(|i| names[*i].clone()).min(),
        )
    });
    for r in formed {
        let mut held: BTreeMap<&String, usize> = BTreeMap::new();
        for i in &sets[r] {
            if let Some(g) = &current[*i] {
                *held.entry(g).or_default() += 1;
            }
        }
        let kept = held
            .into_iter()
            .filter(|(g, _)| !taken.contains(*g))
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
            .map(|(g, _)| g.clone());
        let first = sets[r].iter().map(|i| &names[*i]).min().unwrap();
        if kept.is_none() {
            fresh.insert(r);
        }
        let mut gid =
            kept.unwrap_or_else(|| crate::identity::resource_id(&format!("group:auto:{first}")));
        let mut salt = 0;
        while taken.contains(&gid) {
            gid = crate::identity::resource_id(&format!("group:auto:{first}:kept{salt}"));
            salt += 1;
        }
        taken.insert(gid.clone());
        automatic.insert(r, gid);
    }
    // A fresh name never takes an existing group's ID (a member that left a group named after it
    // and pairs again would otherwise merge into that group with no evidence), and operators may
    // curate the ID they just saw: never reuse that ID for the unpinned remainder.
    let fresh_ids: Vec<String> = fresh.iter().map(|r| automatic[r].clone()).collect();
    let reserved: Vec<String> =
        sqlx::query_scalar("SELECT id FROM \"group\" WHERE id=ANY($1) AND (curated OR id=ANY($2))")
            .bind(automatic.values().cloned().collect::<Vec<_>>())
            .bind(&fresh_ids)
            .fetch_all(&mut **tx)
            .await?;
    for (r, gid) in &mut automatic {
        if !reserved.contains(gid) {
            continue;
        }
        // Names salted with the members, then numbered: the first one no group holds.
        let first = sets[*r].iter().map(|i| &names[*i]).min().unwrap();
        let mut members: Vec<&String> = sets[*r].iter().map(|i| &names[*i]).collect();
        members.sort();
        let salt = blake3::hash(&serde_json::to_vec(&members)?).to_hex()[..16].to_owned();
        let mut start = 0;
        *gid = loop {
            ensure!(start < 4096, "automatic group namespace exhausted");
            let candidates: Vec<String> = (start..start + 64)
                .map(|n| crate::identity::resource_id(&format!("group:auto:{first}:{salt}:{n}")))
                .filter(|c| !taken.contains(c))
                .collect();
            let held: Vec<String> = sqlx::query_scalar("SELECT id FROM \"group\" WHERE id=ANY($1)")
                .bind(&candidates)
                .fetch_all(&mut **tx)
                .await?;
            if let Some(free) = candidates.into_iter().find(|c| !held.contains(c)) {
                break free;
            }
            start += 64;
        };
        taken.insert(gid.clone());
        fresh.insert(*r);
    }
    let assignments: Vec<_> = (0..names.len())
        .map(|i| {
            let gid = overrides[i].clone().or_else(|| {
                if frozen.contains(&roots[i]) {
                    current[i].clone()
                } else {
                    automatic.get(&roots[i]).cloned()
                }
            });
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
        parked,
        resume,
        conflicts,
        assignments,
        groups,
        old: old.into_iter().collect(),
    })
}
#[derive(Debug)]
struct SnapshotChanged;
impl std::fmt::Display for SnapshotChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("group input changed; retry with backoff")
    }
}
impl std::error::Error for SnapshotChanged {}
async fn group_lock(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    // This lock precedes the write-order lock and the inputs' row locks, including in curation.
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(crate::locks::GROUP)
        .fetch_one(&mut **tx)
        .await?;
    if !acquired {
        return Err(LockBusy.into());
    }
    Ok(())
}
/// Another writer holds the group lock. Not a conflict: retry shortly without escalating.
#[derive(Debug)]
pub struct LockBusy;
impl std::fmt::Display for LockBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("group writer busy; retry shortly")
    }
}
impl std::error::Error for LockBusy {}
/// Hold the inputs' rows as checked until this run commits: a writer that changes one waits.
/// A row a writer holds now means its change is coming, so the run retries shortly instead of
/// waiting while it holds rows (an ingested batch locks its rows in feed order, never by ID).
/// Everything before the fence only reads.
async fn fence(
    tx: &mut Transaction<'_, Postgres>,
    revisions: &BTreeMap<String, Option<i64>>,
) -> Result<()> {
    // An exclusive writer (the hourly orphan cleanup, a re-extraction) may hold the write order
    // for a whole batch, past the run's statement timeout: that is contention too.
    crate::store::lock_writers(tx, crate::store::WriteOrder::Shared)
        .await
        .map_err(|e| {
            let timed_out = e
                .downcast_ref::<sqlx::Error>()
                .and_then(|e| e.as_database_error())
                .is_some_and(|d| d.code().as_deref() == Some("57014"));
            if timed_out {
                anyhow::Error::new(LockBusy)
            } else {
                e
            }
        })?;
    sqlx::query("SELECT 1 FROM resource WHERE id=ANY($1) ORDER BY id FOR SHARE NOWAIT")
        .bind(revisions.keys().collect::<Vec<_>>())
        .execute(&mut **tx)
        .await
        .map_err(|e| {
            if e.as_database_error()
                .is_some_and(|d| d.code().as_deref() == Some("55P03"))
            {
                anyhow::Error::new(LockBusy)
            } else {
                e.into()
            }
        })?;
    check_revisions(tx, revisions).await
}
async fn check_revisions(
    tx: &mut Transaction<'_, Postgres>,
    revisions: &BTreeMap<String, Option<i64>>,
) -> Result<()> {
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
        ..
    } = plan;
    for chunk in groups.chunks(256) {
        sqlx::query("INSERT INTO \"group\"(id) SELECT unnest($1::text[]) ON CONFLICT DO NOTHING")
            .bind(chunk)
            .execute(&mut **tx)
            .await?;
    }
    for chunk in assignments.chunks(256) {
        // A unit that moves changes the links its card and the members of both groups may show.
        let moved: Vec<String> = sqlx::query_scalar("SELECT DISTINCT m.resource_id FROM jsonb_array_elements($1) v JOIN group_unit u ON u.unit_id=v->>'unit' AND u.group_id IS DISTINCT FROM v->>'group' JOIN group_unit m ON m.unit_id=u.unit_id OR m.group_id=u.group_id OR m.group_id=v->>'group'").bind(json!(chunk)).fetch_all(&mut **tx).await?;
        crate::also_on::requeue_placed(tx, &moved).await?;
        sqlx::query("UPDATE group_unit u SET group_id=v->>'group' FROM jsonb_array_elements($1) v WHERE u.unit_id=v->>'unit'").bind(json!(chunk)).execute(&mut **tx).await?;
        sqlx::query("UPDATE resource_group k SET group_id=v->>'group' FROM jsonb_array_elements($1) v WHERE k.unit_id=v->>'unit'").bind(json!(chunk)).execute(&mut **tx).await?;
    }
    sqlx::query("DELETE FROM \"group\" g WHERE g.id=ANY($1) AND NOT g.curated AND NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.group_id=g.id)").bind(old).execute(&mut **tx).await?;
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
    let plan = plan(tx, edges, previous, cfg, None).await?;
    apply(tx, plan).await
}
/// Failures are parked independently of resource resolution. The dirty row survives for retry.
pub async fn reconcile(pool: &PgPool, id: &str, cfg: &GroupConfig) -> bool {
    if !cfg.enabled {
        return false;
    }
    // Saves and the drain respect the same durable retry window.
    let due = sqlx::query_scalar::<_,bool>("SELECT coalesce((SELECT next_attempt_at<=now() FROM group_dirty WHERE resource_id=$1),true)").bind(id).fetch_one(pool).await.unwrap_or(false);
    if !due {
        return false;
    }
    let result = async {
        let mut tx = pool.begin().await?;
        sqlx::query("SET LOCAL statement_timeout='2s'")
            .execute(&mut *tx)
            .await?;
        sync(&mut tx, id, cfg).await?;
        tx.commit().await?;
        // Only report successful transactions: failed/retried plans are not merged groups.
        tracing::info!(resource_id = id, "group reconciliation committed");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        if error.is::<LockBusy>() {
            // A busy writer is contention, not a revision conflict: a short jittered retry that
            // neither charges nor advances the conflict backoff.
            sqlx::query("INSERT INTO group_dirty(resource_id,last_error,next_attempt_at) SELECT id,'lock_busy',now()+(1+random()*2)*interval '1 second' FROM resource WHERE id=$1 ON CONFLICT(resource_id) DO UPDATE SET last_error='lock_busy',next_attempt_at=greatest(group_dirty.next_attempt_at,now()+(1+random()*2)*interval '1 second')")
                .bind(id).execute(pool).await.ok();
            return false;
        }
        if error.is::<SnapshotChanged>() {
            sqlx::query("INSERT INTO group_dirty(resource_id,fence_attempts,last_error,next_attempt_at) SELECT id,1,'snapshot_changed',now()+(5+random()*5)*interval '1 second' FROM resource WHERE id=$1 ON CONFLICT(resource_id) DO UPDATE SET fence_attempts=least(group_dirty.fence_attempts+1,32),last_error='snapshot_changed',next_attempt_at=now()+least(3600,(5+random()*5)*power(2,least(group_dirty.fence_attempts,10)))*interval '1 second'")
                .bind(id).execute(pool).await.ok();
            return false;
        }
        tracing::warn!(resource_id=id,error=%crate::resolver::log_error(&error),"group_failed; reconciliation deferred");
        let parked=sqlx::query("INSERT INTO group_dirty(resource_id,attempts,last_error,next_attempt_at) SELECT id,1,'group_failed',now()+interval '1 minute' FROM resource WHERE id=$1 ON CONFLICT(resource_id) DO UPDATE SET attempts=group_dirty.attempts+1,last_error='group_failed',next_attempt_at=now()+least(3600,60*power(2,least(group_dirty.attempts,6))) * interval '1 second'").bind(id).execute(pool).await;
        if let Err(error) = parked {
            tracing::warn!(error=%crate::resolver::log_error(&error),"failed to park grouping retry");
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
#[derive(Debug)]
pub struct GroupBusy;
impl std::fmt::Display for GroupBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("group writer busy; retry later")
    }
}
impl std::error::Error for GroupBusy {}
#[derive(Debug)]
pub struct CurationError(pub &'static str);
impl std::fmt::Display for CurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for CurationError {}
/// Set a curated group's members (a member in another group leaves it).
pub async fn curate(pool: &PgPool, id: &str, body: Curation, cfg: &GroupConfig) -> Result<()> {
    ensure!(
        cfg.enabled && crate::model::valid_id(id),
        CurationError("invalid or disabled group")
    );
    ensure!(
        !body.members.is_empty() && body.members.len() <= cfg.max_members,
        CurationError("invalid group member count")
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    // Curation waits for the active writer within the transaction's two-second limit.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::GROUP)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if e.as_database_error()
                .is_some_and(|d| d.code().as_deref() == Some("57014"))
            {
                anyhow::Error::new(GroupBusy)
            } else {
                e.into()
            }
        })?;
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
    let mut units = vec![];
    let mut previous: Vec<String> =
        sqlx::query_scalar("SELECT group_id FROM group_override WHERE group_id=$1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    for m in &body.members {
        ensure!(
            crate::model::valid_id(&m.resource_id),
            CurationError("invalid member ID")
        );
        let key = m
            .occurrence
            .as_ref()
            .map(|o| {
                serde_json::to_string(&(&o.uid, o.recurrence_id.as_deref().unwrap_or(""))).unwrap()
            })
            .unwrap_or_default();
        let row:Option<(String,Option<String>)>=sqlx::query_as("SELECT unit_id,group_id FROM group_unit WHERE resource_id=$1 AND occurrence_key=$2 AND ($2<>'' OR unit_id=resource_id)").bind(&m.resource_id).bind(key).fetch_optional(&mut *tx).await?;
        let (unit, old) = row.ok_or(CurationError("unknown group member"))?;
        units.push(unit);
        previous.extend(old);
    }
    previous.push(id.into());
    sqlx::query("INSERT INTO \"group\"(id,curated) VALUES($1,true) ON CONFLICT(id) DO UPDATE SET curated=true").bind(id).execute(&mut *tx).await?;
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
    tx.commit().await?;
    Ok(())
}

/// Invalidate derived memberships when an operator changes grouping policy.
pub async fn configure(pool: &PgPool, cfg: &GroupConfig) -> Result<bool> {
    remember_namespaces(pool).await?;
    let settings = cfg.policy();
    let mut tx = pool.begin().await?;
    let old: Option<Value> =
        sqlx::query_scalar("SELECT settings FROM group_runtime WHERE id FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    if old.as_ref() == Some(&settings) {
        return Ok(false);
    }
    sqlx::query("UPDATE group_runtime SET settings=$1 WHERE id")
        .bind(settings)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM group_input")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO group_dirty(resource_id) SELECT DISTINCT resource_id FROM group_unit ON CONFLICT DO NOTHING").execute(&mut *tx).await?;
    tx.commit().await?;
    tracing::info!("grouping policy changed; derived memberships queued for reconciliation");
    Ok(true)
}
/// Write every single-object namespace this build knows to `single_object_namespace`, with the
/// source that records it (`groups` for [`SINGLE_OBJECT`]): the rule for a source's keys outlives
/// the source.
async fn remember_namespaces(pool: &PgPool) -> Result<()> {
    let mut namespaces: Vec<(&str, &str)> = SINGLE_OBJECT.iter().map(|n| (*n, "groups")).collect();
    for source in crate::also_on::registered() {
        namespaces.extend(source.single_namespaces().iter().map(|n| (*n, source.id())));
    }
    let (names, sources): (Vec<&str>, Vec<&str>) = namespaces.into_iter().unzip();
    sqlx::query("INSERT INTO single_object_namespace(namespace,source) SELECT * FROM unnest($1::text[],$2::text[]) ON CONFLICT(namespace) DO UPDATE SET source=excluded.source,seen_at=now()")
        .bind(&names)
        .bind(&sources)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(
        units: usize,
        keys: &[(&str, &[usize])],
        cfg: &GroupConfig,
        resume: Option<&str>,
    ) -> Joined {
        let names: Vec<String> = (0..units).map(|i| format!("u{i}")).collect();
        let (points, starts, occurrences, calendars, trigrams, overrides) = (
            vec![None; units],
            vec![None; units],
            vec![false; units],
            vec![None; units],
            vec![Vec::new(); units],
            vec![None; units],
        );
        // Each unit's single-object keys, from the keys it is listed under.
        let mut singles = vec![Vec::new(); units];
        for (key, members) in keys {
            if let Some((namespace, value)) = single_object(key) {
                for i in *members {
                    singles[*i].push((namespace.to_owned(), value.to_owned()));
                }
            }
        }
        let facts = Facts {
            names: &names,
            points: &points,
            starts: &starts,
            occurrences: &occurrences,
            calendars: &calendars,
            trigrams: &trigrams,
            overrides: &overrides,
            singles: &singles,
        };
        let bykey = keys
            .iter()
            .map(|(k, members)| {
                let at = chrono::DateTime::UNIX_EPOCH;
                (k.to_string(), members.iter().map(|i| (*i, at)).collect())
            })
            .collect();
        join(&facts, bykey, cfg, resume)
    }
    fn same(joined: &mut Joined, a: usize, b: usize) -> bool {
        root(&mut joined.parents, a) == root(&mut joined.parents, b)
    }

    /// A parked key's partial unions are undone: they never crowd a later key's join past
    /// `max_members`.
    #[test]
    fn a_parked_keys_partial_unions_are_undone() {
        let cfg = GroupConfig {
            max_members: 4,
            check_budget: Some(3),
            ..Default::default()
        };
        // "a" joins 0, 1 and 2 (three checks), then 3 would cost three more: parked.
        let keys: &[(&str, &[usize])] = &[("0", &[4, 5]), ("a", &[0, 1, 2, 3]), ("b", &[0, 4])];
        let mut joined = run(6, keys, &cfg, None);
        assert_eq!(joined.parked, 1);
        assert!(joined.unfinished.contains("a"));
        assert!(!same(&mut joined, 0, 1) && !same(&mut joined, 1, 2));
        // Left in place, {0, 1, 2} and {4, 5} would exceed four members and "b" could not join.
        assert!(same(&mut joined, 0, 4) && same(&mut joined, 4, 5));
        assert_eq!(joined.resume, None);
        let mut sizes: Vec<usize> = joined.sets.iter().map(Vec::len).collect();
        sizes.sort();
        assert_eq!(sizes.iter().sum::<usize>(), 6);
    }

    /// A wrong Q-id never chains two recordings: a group holds one value of each single-object
    /// namespace, while ISRCs may be several.
    #[test]
    fn a_shared_key_never_joins_two_recordings() {
        let mut joined = run(
            4,
            &[
                ("hub:isrc:GBUM71029604", &[0, 3]),
                ("hub:isrc:USIR20400274", &[0]),
                ("hub:mb:recording:a", &[0]),
                ("hub:mb:recording:b", &[1]),
                ("hub:wikidata:Q1", &[0, 1, 2]),
            ],
            &GroupConfig::default(),
            None,
        );
        assert!(!same(&mut joined, 0, 1));
        assert!(same(&mut joined, 0, 2) && same(&mut joined, 0, 3));
        // Q1 refused 1 with 0, then 1 with {0, 2}.
        assert_eq!(joined.conflicts, 2);
        assert_eq!(single_object("hub:upc:0602557656527"), None);
        assert_eq!(
            single_object("hub:imdb:tt0133093"),
            Some(("imdb", "tt0133093"))
        );
        assert_eq!(
            single_object("hub:podcast:guid:ead4c236-bf58-58c6-a2c6-a6b28d128cb6"),
            Some(("podcast:guid", "ead4c236-bf58-58c6-a2c6-a6b28d128cb6"))
        );
        assert_eq!(single_object("mb:recording:a"), None);
        for (key, namespace, value) in [
            (
                "hub:mb:release-group:b1392450",
                "mb:release-group",
                "b1392450",
            ),
            ("hub:mb:release:425d8db9", "mb:release", "425d8db9"),
            ("hub:osm:node/240109189", "osm", "node/240109189"),
            (
                "hub:doi:10.1002/(sici)1097:4<x>",
                "doi",
                "10.1002/(sici)1097:4<x>",
            ),
        ] {
            assert_eq!(single_object(key), Some((namespace, value)), "{key}");
        }
        // Each also-on source's own single-object namespaces (a Discogs master, a crate…).
        for source in crate::also_on::registered() {
            for namespace in source.single_namespaces() {
                let key = format!("hub:{namespace}:1/a:b");
                assert_eq!(single_object(&key), Some((*namespace, "1/a:b")), "{key}");
            }
        }
        assert_eq!(single_object("hub:isrc:GBAYE9701376"), None);
    }
    /// A run that stops on the budget resumes at its stop key and wraps around, so every key of a
    /// component too large for one run is reached in turn.
    #[test]
    fn a_stopped_run_resumes_at_its_stop_key() {
        let cfg = GroupConfig {
            check_budget: Some(1),
            ..Default::default()
        };
        let keys: &[(&str, &[usize])] = &[
            ("k0", &[0, 1]),
            ("k1", &[2, 3]),
            ("k2", &[4, 5]),
            ("k3", &[6, 7]),
            ("k4", &[8, 9]),
            ("k5", &[10, 11]),
        ];
        // Four checks a run: k0..k3 join, the run stops at k4.
        let mut first = run(12, keys, &cfg, None);
        assert_eq!(first.resume.as_deref(), Some("k4"));
        assert_eq!(
            first.unfinished,
            ["k4", "k5"].map(String::from).into_iter().collect()
        );
        assert!(same(&mut first, 6, 7) && !same(&mut first, 8, 9));
        // The next run starts at k4: k4, k5, k0 and k1 join, it stops at k2.
        let mut second = run(12, keys, &cfg, first.resume.as_deref());
        assert_eq!(second.resume.as_deref(), Some("k2"));
        assert!(same(&mut second, 8, 9) && same(&mut second, 10, 11) && same(&mut second, 2, 3));
        assert!(!same(&mut second, 4, 5));
        // A resume key that no longer exists starts at the next key in order.
        let third = run(12, keys, &cfg, Some("k35"));
        assert_eq!(third.resume.as_deref(), Some("k2"));
    }

    fn fillers(from: usize, count: usize) -> Vec<(String, [usize; 2])> {
        (0..count)
            .map(|n| (format!("k{n}"), [from + 2 * n, from + 2 * n + 1]))
            .collect()
    }

    /// In a stopped run the units `max_members` kept apart are reported, so `plan` freezes their
    /// sets: with the keys rotated, the cap would otherwise split them differently each run.
    #[test]
    fn a_stopped_run_reports_the_pairs_the_cap_kept_apart() {
        let cfg = GroupConfig {
            max_members: 4,
            check_budget: Some(3),
            ..Default::default()
        };
        // {1, 2, 3} and then 4 join; 5 would make five members.
        let fill = fillers(6, 8);
        let mut keys: Vec<(&str, &[usize])> = vec![("a", &[1, 2, 3]), ("b", &[3, 4, 5])];
        keys.extend(fill.iter().map(|(k, m)| (k.as_str(), &m[..])));
        let mut joined = run(22, &keys, &cfg, None);
        assert!(joined.resume.is_some());
        assert!(same(&mut joined, 1, 4) && !same(&mut joined, 4, 5));
        assert!(joined.capped[5], "{:?}", joined.capped);
    }

    /// A parked key's checks count against the run, the refused pair included.
    #[test]
    fn a_parked_keys_checks_count_against_the_run() {
        let cfg = GroupConfig {
            check_budget: Some(2),
            ..Default::default()
        };
        // "a" checks one pair (1), then a pair costing 2 parks it: three checks charged. The
        // run's cap is 8, so the fillers (one check each) stop it at the sixth, k5.
        let fill = fillers(3, 8);
        let mut keys: Vec<(&str, &[usize])> = vec![("a", &[0, 1, 2])];
        keys.extend(fill.iter().map(|(k, m)| (k.as_str(), &m[..])));
        let joined = run(19, &keys, &cfg, None);
        assert_eq!(joined.parked, 1);
        assert_eq!(joined.resume.as_deref(), Some("k5"));
    }

    /// A unit for the UID rule: its resource URI, whether it is an occurrence, its point.
    type UidUnit<'a> = (&'a str, bool, Option<(f64, f64)>);
    /// Units for the UID rule, all dated at one instant and named alike, joined by one
    /// `ics_uid:` key.
    fn uid_join(units: &[UidUnit], mode: CrossContainerOccurrences) -> Joined {
        let at = chrono::DateTime::from_timestamp(1_800_000_000, 0);
        let n = units.len();
        let names: Vec<String> = (0..n).map(|i| format!("u{i}")).collect();
        let points: Vec<_> = units.iter().map(|u| u.2).collect();
        let starts = vec![at; n];
        let occurrences: Vec<bool> = units.iter().map(|u| u.1).collect();
        let calendars: Vec<_> = units
            .iter()
            .map(|u| {
                if u.1 {
                    crate::model::identity(u.0).ok().map(|i| i.0)
                } else {
                    organiser_calendar(u.0)
                }
            })
            .collect();
        let (trigrams, overrides, singles) = (
            vec![vec!["abc".to_owned()]; n],
            vec![None; n],
            vec![Vec::new(); n],
        );
        let facts = Facts {
            names: &names,
            points: &points,
            starts: &starts,
            occurrences: &occurrences,
            calendars: &calendars,
            trigrams: &trigrams,
            overrides: &overrides,
            singles: &singles,
        };
        let epoch = chrono::DateTime::UNIX_EPOCH;
        let bykey =
            BTreeMap::from([("ics_uid:x".to_owned(), (0..n).map(|i| (i, epoch)).collect())]);
        let cfg = GroupConfig {
            cross_container_occurrences: mode,
            ..Default::default()
        };
        join(&facts, bykey, &cfg, None)
    }

    /// In the corroborated mode, an event page joins the occurrence with its UID in the
    /// organiser calendar its platform names for it without the points and names a join across
    /// feeds needs; another calendar, even on the same site, two feeds, or a page and a feed of
    /// two publishers, still need them. Off joins none.
    #[test]
    fn a_page_joins_its_date_in_its_organisers_calendar() {
        let page = "https://www.meetup.com/g/events/316908442/";
        let feed = "https://www.meetup.com/g/events/ical/";
        let corroborated = CrossContainerOccurrences::Corroborated;
        let mut joined = uid_join(&[(page, false, None), (feed, true, None)], corroborated);
        assert!(same(&mut joined, 0, 1));
        let mut off = uid_join(
            &[(page, false, None), (feed, true, None)],
            CrossContainerOccurrences::Off,
        );
        assert!(!same(&mut off, 0, 1));
        // Another publisher's page with the same UID: no points, no join.
        let other = "https://events.example.com/meetup";
        let mut joined = uid_join(&[(other, false, None), (feed, true, None)], corroborated);
        assert!(!same(&mut joined, 0, 1));
        // Two feeds of one publisher: still the cross-feed rule.
        let second = "https://www.meetup.com/h/events/ical/";
        let mut joined = uid_join(&[(feed, true, None), (second, true, None)], corroborated);
        assert!(!same(&mut joined, 0, 1));
        // Another calendar on the page's site (another group's): the cross-feed rule too.
        let mut joined = uid_join(&[(page, false, None), (second, true, None)], corroborated);
        assert!(!same(&mut joined, 0, 1));
        let mut joined = uid_join(
            &[
                (page, false, None),
                ("https://www.meetup.com/g/x.ics", true, None),
            ],
            corroborated,
        );
        assert!(!same(&mut joined, 0, 1));
        // A third publisher's located feed joins neither: its pair with the page's unlocated
        // own feed fails the cross-feed rule.
        let located = "https://calendar.example.com/a.ics";
        let mut joined = uid_join(
            &[
                (page, false, Some((47.37, 8.53))),
                (feed, true, None),
                (located, true, Some((47.37, 8.53))),
            ],
            corroborated,
        );
        assert!(same(&mut joined, 0, 1) && !same(&mut joined, 1, 2));
        // The cross-publisher rule is unchanged: located within 500 m, same start and name.
        let mut joined = uid_join(
            &[
                (other, false, Some((47.37, 8.53))),
                (located, true, Some((47.37, 8.531))),
            ],
            corroborated,
        );
        assert!(same(&mut joined, 0, 1));
        // Too far apart for a page and its organiser's calendar too (`max_distance_m`).
        let mut joined = uid_join(
            &[
                (page, false, Some((47.37, 8.53))),
                (feed, true, Some((46.0, 7.0))),
            ],
            corroborated,
        );
        assert!(!same(&mut joined, 0, 1));
    }
}
