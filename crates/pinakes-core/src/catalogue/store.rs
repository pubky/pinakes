//! The catalogue tables: the batch writer, the removal of rows a full import did not read, the
//! drop, and the state rows.
//!
//! Every write is bounded: at most [`BATCH_ROWS`] cards per transaction, and the sweeps read
//! [`SWEEP_ROWS`] rows of the catalogue per transaction, in `source_id` order over the
//! `(catalogue, source_id)` index. The state row's `cards` moves with each transaction that adds
//! or removes cards, so it is never counted over the table.
use super::{Card, Catalogue, CataloguesConfig, Row};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

/// The most cards one transaction writes or removes. Fixed: a transaction of this size takes
/// milliseconds and a few hundred kilobytes, and no deployment needs another.
pub const BATCH_ROWS: usize = 1000;
/// The rows of one catalogue a sweep (the removal after a full import, the drop) reads per
/// transaction.
pub const SWEEP_ROWS: i64 = 10_000;

/// What a write did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WriteReport {
    /// New cards.
    pub inserted: u64,
    /// Cards whose content hash changed: rewritten.
    pub changed: u64,
    /// Cards whose content hash is the stored one: only stamped with the generation when a full
    /// import reads them, never rewritten.
    pub unchanged: u64,
    /// Cards [`Card::checked`] refused.
    pub rejected: u64,
    /// Cards removed (`remove`, and the rows a full import did not read).
    pub removed: u64,
}
impl std::ops::AddAssign for WriteReport {
    fn add_assign(&mut self, o: Self) {
        self.inserted += o.inserted;
        self.changed += o.changed;
        self.unchanged += o.unchanged;
        self.rejected += o.rejected;
        self.removed += o.removed;
    }
}

/// A catalogue's state row (`catalogue_state`).
#[derive(Clone, Debug, PartialEq, Serialize, sqlx::FromRow)]
pub struct State {
    pub catalogue: String,
    /// `idle`, `import` (a full import, the first or a re-import), `update`, `failed` or
    /// `disabled`.
    pub phase: String,
    /// The run `cursor` belongs to (`import`, `update`); none when no run is under way.
    pub run: Option<String>,
    /// The generation of the last full import begun.
    pub generation: i64,
    /// Where the run under way resumes, as its importer wrote it.
    pub cursor: Option<serde_json::Value>,
    /// Where the next update starts (a replication sequence, a time), as the last finished run
    /// left it; kept from run to run.
    pub position: Option<serde_json::Value>,
    pub cards: i64,
    pub upstream_version: Option<String>,
    pub last_import_at: Option<DateTime<Utc>>,
    pub last_update_at: Option<DateTime<Utc>>,
    /// The scope of the last finished full import (`CataloguesConfig::scope`): a full import of a
    /// narrower scope may remove more cards than `run::SHRINK_FLOOR` lets one of the same scope.
    pub scope: Option<serde_json::Value>,
    /// The cards the full import of `generation` has read so far (written, changed or stamped):
    /// what `run::SHRINK_FLOOR` compares with `cards`.
    pub generation_cards: i64,
    /// The operator let the next full import remove the cards it did not read, whatever their
    /// share (`pinakesd catalogue allow-shrink`); cleared when that import ends.
    pub shrink_allowed: bool,
    /// Why an update asked for a full import (its importer's fixed words), until that import
    /// begins; none when none is asked for.
    pub import_requested: Option<String>,
    /// How current the cards are: the upstream time of the data the last finished run left (a
    /// dump's date, the time of the last change applied). The update lag counts from it.
    pub current_as_of: Option<DateTime<Utc>>,
    /// Why the importer waits (free disk, another large import, its lock), while it does.
    pub waiting: Option<String>,
    /// The last failure, kept after a later success; `error_count` counts them all.
    pub last_error: Option<String>,
    pub error_count: i64,
    pub updated_at: DateTime<Utc>,
}
impl State {
    /// When the catalogue was last current: its last finished full import or update.
    pub fn current_at(&self) -> Option<DateTime<Utc>> {
        self.last_import_at.max(self.last_update_at)
    }
}

/// The state row of `catalogue`, made when it has none: one statement, so a drop that deletes
/// the row at the same time can't leave it missing between a write and a read.
pub async fn ensure_state(pool: &PgPool, catalogue: Catalogue) -> Result<State> {
    Ok(sqlx::query_as(
        "INSERT INTO catalogue_state(catalogue) VALUES($1) ON CONFLICT (catalogue) DO UPDATE SET catalogue=excluded.catalogue RETURNING *",
    )
    .bind(catalogue.name())
    .fetch_one(pool)
    .await?)
}
pub async fn state(pool: &PgPool, catalogue: Catalogue) -> Result<Option<State>> {
    Ok(
        sqlx::query_as("SELECT * FROM catalogue_state WHERE catalogue=$1")
            .bind(catalogue.name())
            .fetch_optional(pool)
            .await?,
    )
}
/// Every state row, by catalogue name.
pub async fn states(pool: &PgPool) -> Result<Vec<State>> {
    Ok(
        sqlx::query_as("SELECT * FROM catalogue_state ORDER BY catalogue")
            .fetch_all(pool)
            .await?,
    )
}
/// The `phase` of each catalogue's state row as the configuration the daemon starts with says:
/// `disabled` for a catalogue that is off, and `idle` again for one turned back on. The run under
/// way and its cursor stay, so a catalogue turned back on resumes. Rows only: a catalogue that
/// never ran has none, and gets none here.
pub async fn sync_switches(pool: &PgPool, config: &CataloguesConfig) -> Result<()> {
    let on = config.enabled_names();
    sqlx::query("UPDATE catalogue_state SET phase='disabled',waiting=NULL,updated_at=now() WHERE NOT catalogue=ANY($1) AND phase<>'disabled'")
        .bind(&on)
        .execute(pool)
        .await?;
    sqlx::query("UPDATE catalogue_state SET phase='idle',updated_at=now() WHERE catalogue=ANY($1) AND phase='disabled'")
        .bind(&on)
        .execute(pool)
        .await?;
    Ok(())
}

/// The cards checked ([`Card::checked`]), each `source_id` once (the last one given wins), with
/// the number refused.
fn rows(catalogue: Catalogue, cards: &[Card]) -> (Vec<Row>, u64) {
    let mut rejected = 0;
    let mut rows: Vec<Row> = Vec::with_capacity(cards.len());
    let mut at = std::collections::HashMap::new();
    for card in cards {
        match card.checked(catalogue) {
            Ok(row) => match at.get(&row.source_id) {
                Some(&i) => rows[i] = row,
                None => {
                    at.insert(row.source_id.clone(), rows.len());
                    rows.push(row);
                }
            },
            Err(reject) => {
                rejected += 1;
                // Its ID as far as a valid one goes: an over-long ID is one reason to refuse.
                let mut shown: String = card.source_id.chars().take(128).collect();
                shown.retain(|c| !c.is_control());
                tracing::debug!(catalogue = catalogue.name(), source_id = %shown, ?reject, "catalogue card rejected");
            }
        }
    }
    (rows, rejected)
}

/// Write `cards` of `catalogue`, stamped with `generation`, in transactions of at most
/// [`BATCH_ROWS`]: a new card is inserted, a card whose content hash changed is rewritten, and a
/// card whose hash is the stored one is not rewritten (only its `seen_generation` is raised to
/// `generation` when lower). `cursor`, when given, is saved in the state row with the last part,
/// so a run that stops resumes after the last part it wrote.
pub async fn upsert(
    pool: &PgPool,
    catalogue: Catalogue,
    generation: i64,
    cards: &[Card],
    cursor: Option<&serde_json::Value>,
) -> Result<WriteReport> {
    let (rows, rejected) = rows(catalogue, cards);
    upsert_rows(pool, catalogue, generation, &rows, rejected, cursor).await
}
/// [`upsert`] of rows an importer already checked ([`Card::checked`]), each `source_id` once;
/// `rejected` is the number of cards it refused, for the report.
pub async fn upsert_rows(
    pool: &PgPool,
    catalogue: Catalogue,
    generation: i64,
    rows: &[Row],
    rejected: u64,
    cursor: Option<&serde_json::Value>,
) -> Result<WriteReport> {
    write_rows(pool, catalogue, generation, rows, rejected, cursor, true).await
}
/// [`upsert`] of cards a full import already read in this generation, written again with more
/// of their data (Wikidata's labels read after the cards): they don't count again toward the
/// cards the generation read (`generation_cards`, which the shrink floor compares).
pub async fn rewrite(
    pool: &PgPool,
    catalogue: Catalogue,
    generation: i64,
    cards: &[Card],
    cursor: Option<&serde_json::Value>,
) -> Result<WriteReport> {
    let (rows, rejected) = rows(catalogue, cards);
    write_rows(pool, catalogue, generation, &rows, rejected, cursor, false).await
}
async fn write_rows(
    pool: &PgPool,
    catalogue: Catalogue,
    generation: i64,
    rows: &[Row],
    rejected: u64,
    cursor: Option<&serde_json::Value>,
    read: bool,
) -> Result<WriteReport> {
    ensure_state(pool, catalogue).await?;
    let mut report = WriteReport {
        rejected,
        ..WriteReport::default()
    };
    crate::telemetry::catalogue_cards(
        catalogue.name(),
        WriteReport {
            rejected,
            ..WriteReport::default()
        },
    );
    let parts = rows.chunks(BATCH_ROWS).count();
    if parts == 0 {
        if let Some(cursor) = cursor {
            save_cursor(pool, catalogue, cursor).await?;
        }
        return Ok(report);
    }
    for (n, part) in rows.chunks(BATCH_ROWS).enumerate() {
        let mut tx = pool.begin().await?;
        let written = write_part_counted(&mut tx, catalogue, generation, part, read).await?;
        report += written;
        if n + 1 == parts {
            if let Some(cursor) = cursor {
                sqlx::query(
                    "UPDATE catalogue_state SET cursor=$2,updated_at=now() WHERE catalogue=$1",
                )
                .bind(catalogue.name())
                .bind(cursor)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        crate::telemetry::catalogue_cards(catalogue.name(), written);
    }
    Ok(report)
}
async fn save_cursor(
    pool: &PgPool,
    catalogue: Catalogue,
    cursor: &serde_json::Value,
) -> Result<()> {
    sqlx::query("UPDATE catalogue_state SET cursor=$2,updated_at=now() WHERE catalogue=$1")
        .bind(catalogue.name())
        .bind(cursor)
        .execute(pool)
        .await?;
    Ok(())
}
/// One part: the upsert that writes only changed content, then the stamp of the unchanged rows,
/// then the state row's count.
#[cfg(test)]
async fn write_part(
    tx: &mut Transaction<'_, Postgres>,
    catalogue: Catalogue,
    generation: i64,
    part: &[Row],
) -> Result<WriteReport> {
    write_part_counted(tx, catalogue, generation, part, true).await
}
/// [`write_part`]; `read` false for a rewrite of cards the generation has read already (they
/// don't count again toward `generation_cards`, which the shrink floor compares).
async fn write_part_counted(
    tx: &mut Transaction<'_, Postgres>,
    catalogue: Catalogue,
    generation: i64,
    part: &[Row],
    read: bool,
) -> Result<WriteReport> {
    let input: Vec<serde_json::Value> = part
        .iter()
        .map(|r| {
            serde_json::json!({
                "source": r.source, "source_id": r.source_id, "uri": r.uri, "type": r.r#type,
                "name": r.name, "other_names": r.other_names, "description": r.description,
                "labels": r.labels, "keys": r.keys, "related_uris": r.related_uris,
                "lon": r.lon, "lat": r.lat, "fame": r.fame, "details": r.details,
                "content_hash": hex(&r.content_hash),
            })
        })
        .collect();
    // `fit` keeps the rows the table's checks take, as PostgreSQL measures them: `Card::checked`
    // already holds a card to them, and a row it misjudged is refused here alone, never with
    // its whole part (which would stop the run at the same part on every retry).
    let (inserted, changed, refused): (i64, i64, Vec<String>) = sqlx::query_as(
        "WITH input AS (SELECT * FROM jsonb_to_recordset($3::jsonb) AS x(source text,source_id text,uri text,type text,name text,other_names text[],description text,labels text[],keys text[],related_uris text[],lon float8,lat float8,fame real,details jsonb,content_hash text)),\
         fit AS (SELECT * FROM input WHERE octet_length(uri) BETWEEN 1 AND 2048 AND octet_length(source_id) BETWEEN 1 AND 512 AND jsonb_typeof(details)='object' AND octet_length(details::text)<=8192),\
         written AS (INSERT INTO catalogue_card AS c(catalogue,source,source_id,uri,type,name,other_names,description,labels,keys,related_uris,point,fame,details,content_hash,seen_generation) \
          SELECT $1,source,source_id,uri,type,name,other_names,description,labels,keys,related_uris,\
           CASE WHEN lon IS NULL OR lat IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint(lon,lat),4326) END,fame,details,decode(content_hash,'hex'),$2 FROM fit \
          ON CONFLICT (catalogue,source_id) DO UPDATE SET source=excluded.source,uri=excluded.uri,type=excluded.type,name=excluded.name,other_names=excluded.other_names,\
           description=excluded.description,labels=excluded.labels,keys=excluded.keys,related_uris=excluded.related_uris,point=excluded.point,fame=excluded.fame,\
           details=excluded.details,content_hash=excluded.content_hash,seen_generation=greatest(c.seen_generation,excluded.seen_generation),updated_at=now() \
          WHERE c.content_hash<>excluded.content_hash \
          RETURNING (c.xmax::text='0') AS inserted) \
         SELECT count(*) FILTER (WHERE inserted),count(*) FILTER (WHERE NOT inserted),\
          array(SELECT source_id FROM input EXCEPT SELECT source_id FROM fit) FROM written",
    )
    .bind(catalogue.name())
    .bind(generation)
    .bind(serde_json::Value::Array(input))
    .fetch_one(&mut **tx)
    .await?;
    for id in &refused {
        tracing::debug!(catalogue = catalogue.name(), source_id = %id, "catalogue card rejected by the table's checks");
    }
    let ids: Vec<&str> = part
        .iter()
        .map(|r| r.source_id.as_str())
        .filter(|id| !refused.iter().any(|r| r == id))
        .collect();
    let stamped = sqlx::query("UPDATE catalogue_card SET seen_generation=$2 WHERE catalogue=$1 AND source_id=ANY($3) AND seen_generation<$2")
        .bind(catalogue.name())
        .bind(generation)
        .bind(&ids)
        .execute(&mut **tx)
        .await?
        .rows_affected() as i64;
    // The cards this generation has read: new ones, changed ones and stamped ones (a resumed run
    // writing a part again counts its changed cards twice, never its unchanged ones). A rewrite
    // of cards already read counts only a card it adds.
    sqlx::query("UPDATE catalogue_state SET cards=cards+$2,generation_cards=generation_cards+$3,updated_at=now() WHERE catalogue=$1")
        .bind(catalogue.name())
        .bind(inserted)
        .bind(if read { inserted + changed + stamped } else { inserted })
        .execute(&mut **tx)
        .await?;
    Ok(WriteReport {
        inserted: inserted as u64,
        changed: changed as u64,
        unchanged: (ids.len() as i64 - inserted - changed) as u64,
        rejected: refused.len() as u64,
        ..WriteReport::default()
    })
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove the cards of `catalogue` with these source IDs (items an update found removed
/// upstream), in transactions of at most [`BATCH_ROWS`]; `cursor`, when given, is saved with the
/// last one. The number removed.
pub async fn remove(
    pool: &PgPool,
    catalogue: Catalogue,
    source_ids: &[String],
    cursor: Option<&serde_json::Value>,
) -> Result<u64> {
    ensure_state(pool, catalogue).await?;
    let mut removed = 0;
    let parts = source_ids.chunks(BATCH_ROWS).count();
    for (n, part) in source_ids.chunks(BATCH_ROWS).enumerate() {
        let mut tx = pool.begin().await?;
        let gone =
            sqlx::query("DELETE FROM catalogue_card WHERE catalogue=$1 AND source_id=ANY($2)")
                .bind(catalogue.name())
                .bind(part)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        sqlx::query("UPDATE catalogue_state SET cards=greatest(cards-$2,0),cursor=coalesce($3,cursor),updated_at=now() WHERE catalogue=$1")
            .bind(catalogue.name())
            .bind(gone as i64)
            .bind(cursor.filter(|_| n + 1 == parts))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        crate::telemetry::catalogue_cards(
            catalogue.name(),
            WriteReport {
                removed: gone,
                ..WriteReport::default()
            },
        );
        removed += gone;
    }
    if parts == 0 {
        if let Some(cursor) = cursor {
            save_cursor(pool, catalogue, cursor).await?;
        }
    }
    Ok(removed)
}

/// Let the next full import of `catalogue` remove the cards it does not read, whatever their share
/// of the catalogue (`pinakesd catalogue allow-shrink`), while the full import under way last
/// failed as `shrunk`: it then goes through at its next try, unless a run fails for another
/// reason first (`run::record_failure` takes the allowance back). Refused in any other state.
pub async fn allow_shrink(pool: &PgPool, catalogue: Catalogue) -> Result<Allowance> {
    let state: Option<bool> = sqlx::query_scalar(
        "UPDATE catalogue_state SET shrink_allowed=shrink_allowed OR coalesce(run='import' AND phase='failed' AND (last_error='shrunk' OR last_error LIKE 'shrunk (%'),false),updated_at=now() WHERE catalogue=$1 RETURNING shrink_allowed",
    )
    .bind(catalogue.name())
    .fetch_optional(pool)
    .await?;
    Ok(match state {
        Some(true) => Allowance::Allowed,
        Some(false) => Allowance::NotShrunk,
        None => Allowance::NoState,
    })
}
/// Clear the refusal `catalogue`'s position holds (`run::REFUSED`, `pinakesd catalogue
/// clear-refusal`): its importer reads the refused version again at its next run, an update as
/// any. Needs no lock (a run under way that refuses again sets it again). What was refused, if
/// anything.
pub async fn clear_refusal(
    pool: &PgPool,
    catalogue: Catalogue,
) -> Result<Option<serde_json::Value>> {
    let mut tx = pool.begin().await?;
    let refused: Option<Option<serde_json::Value>> = sqlx::query_scalar(
        "SELECT position->'refused' FROM catalogue_state WHERE catalogue=$1 FOR UPDATE",
    )
    .bind(catalogue.name())
    .fetch_optional(&mut *tx)
    .await?;
    let refused = refused.flatten().filter(|v| !v.is_null());
    if refused.is_some() {
        sqlx::query("UPDATE catalogue_state SET position=position-$2::text[],updated_at=now() WHERE catalogue=$1")
            .bind(catalogue.name())
            .bind(&super::run::REFUSAL_KEYS[..])
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(refused)
}
/// What [`allow_shrink`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allowance {
    /// The next try of the run that failed as shrunk removes what it does not read.
    Allowed,
    /// No full import under way last failed as shrunk: nothing is allowed.
    NotShrunk,
    /// The catalogue has no state row: it never ran.
    NoState,
}
/// The stored cards of `catalogue` with these source IDs, as cards again (an importer that
/// rewrites part of a stored card, such as its labels, reads it back with this). At most
/// [`BATCH_ROWS`] per call; IDs with no card are left out.
pub async fn cards(
    pool: &PgPool,
    catalogue: Catalogue,
    source_ids: &[String],
) -> Result<Vec<Card>> {
    anyhow::ensure!(
        source_ids.len() <= BATCH_ROWS,
        "at most {BATCH_ROWS} cards per read"
    );
    type Stored = (
        String,
        String,
        String,
        String,
        String,
        Vec<String>,
        Option<String>,
        Vec<String>,
        Vec<String>,
        Vec<String>,
        Option<f64>,
        Option<f64>,
        f32,
        serde_json::Value,
    );
    let rows: Vec<Stored> = sqlx::query_as(
        "SELECT source,source_id,uri,type,name,other_names,description,labels,keys,related_uris,ST_X(point),ST_Y(point),fame,details \
         FROM catalogue_card WHERE catalogue=$1 AND source_id=ANY($2) ORDER BY source_id",
    )
    .bind(catalogue.name())
    .bind(source_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Card {
            source: r.0,
            source_id: r.1,
            uri: r.2,
            r#type: r.3,
            name: r.4,
            other_names: r.5,
            description: r.6,
            labels: r.7,
            keys: r.8,
            related_uris: r.9,
            point: r
                .10
                .zip(r.11)
                .map(|(lon, lat)| crate::model::Point { lat, lon }),
            fame: r.12,
            details: match r.13 {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            },
        })
        .collect())
}

/// The end of a full import of `generation`: remove the cards of `catalogue` it did not read
/// (`seen_generation` below it), so an item removed upstream leaves with the import that no
/// longer finds it. Reads the catalogue's rows in `source_id` order, [`SWEEP_ROWS`] per
/// transaction. The number removed.
pub async fn finish_import(pool: &PgPool, catalogue: Catalogue, generation: i64) -> Result<u64> {
    // `sweep` tells the total so far: each page's own count is counted.
    let mut counted = 0;
    sweep(pool, catalogue, Some(generation), |total| {
        crate::telemetry::catalogue_cards(
            catalogue.name(),
            WriteReport {
                removed: total - counted,
                ..WriteReport::default()
            },
        );
        counted = total;
    })
    .await
}

/// Delete every card of `catalogue` and its state row (`pinakesd catalogue drop`), in
/// transactions of [`SWEEP_ROWS`] rows; `progress` is told the count after each. The number
/// deleted. The caller holds the catalogue's lock, so no importer writes meanwhile.
pub async fn drop_catalogue(
    pool: &PgPool,
    catalogue: Catalogue,
    progress: impl FnMut(u64),
) -> Result<u64> {
    let dropped = sweep(pool, catalogue, None, progress).await?;
    if catalogue == Catalogue::Osm {
        // What the OSM catalogue's updates recorded goes with it, the aliases its successors
        // made released first (their links answer for themselves again).
        let followed: Vec<String> =
            sqlx::query_scalar("SELECT old_type||'/'||old_id FROM osm_successor")
                .fetch_all(pool)
                .await?;
        super::osm::unfollow(pool, &followed).await?;
        sqlx::query("DELETE FROM osm_successor")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM osm_rename").execute(pool).await?;
        sqlx::query("DELETE FROM osm_removed").execute(pool).await?;
    }
    // Tranco's list goes with its cards.
    if catalogue == Catalogue::Tranco {
        sqlx::query("DELETE FROM catalogue_site")
            .execute(pool)
            .await?;
    }
    sqlx::query("DELETE FROM catalogue_state WHERE catalogue=$1")
        .bind(catalogue.name())
        .execute(pool)
        .await?;
    Ok(dropped)
}

/// Walk the catalogue's rows in `source_id` order, a page per transaction, deleting those of the
/// page seen before `generation` (all of them without one).
async fn sweep(
    pool: &PgPool,
    catalogue: Catalogue,
    generation: Option<i64>,
    mut progress: impl FnMut(u64),
) -> Result<u64> {
    let mut after = String::new();
    let mut removed = 0u64;
    loop {
        let mut tx = pool.begin().await?;
        let (last, gone): (Option<String>, i64) = sqlx::query_as(
            "WITH page AS (SELECT source_id FROM catalogue_card WHERE catalogue=$1 AND source_id>$2 ORDER BY source_id LIMIT $3),\
             gone AS (DELETE FROM catalogue_card c USING page p WHERE c.catalogue=$1 AND c.source_id=p.source_id AND ($4::bigint IS NULL OR c.seen_generation<$4) RETURNING 1) \
             SELECT (SELECT max(source_id) FROM page),(SELECT count(*) FROM gone)",
        )
        .bind(catalogue.name())
        .bind(&after)
        .bind(SWEEP_ROWS)
        .bind(generation)
        .fetch_one(&mut *tx)
        .await?;
        if gone > 0 {
            sqlx::query("UPDATE catalogue_state SET cards=greatest(cards-$2,0),updated_at=now() WHERE catalogue=$1")
                .bind(catalogue.name())
                .bind(gone)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        removed += gone as u64;
        progress(removed);
        match last {
            Some(last) => after = last,
            None => return Ok(removed),
        }
    }
}

/// The table's `waiting` cell: a full import an update asked for, with its reason, what its
/// importer refused (`refused` in its position: a version, or each registry's), and why the
/// importer waits.
fn waiting_cell(state: Option<&State>) -> String {
    let position = state.and_then(|s| s.position.as_ref());
    let refused = match position.and_then(|p| p.get(super::run::REFUSED)) {
        Some(serde_json::Value::String(v)) => Some(format!("refused: {v}")),
        Some(serde_json::Value::Object(m)) => Some(format!(
            "refused: {}",
            m.iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(v) => format!("{k} {v}"),
                    v => format!("{k} {v}"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Some(serde_json::Value::Null) | None => None,
        Some(other) => Some(format!("refused: {other}")),
    };
    let refused = refused
        // Tranco's homepage reads that wait for a large catalogue's full import (its last run).
        .or_else(|| {
            let p = position?;
            (p.get("next")?.as_str()? == "large_import").then(|| {
                let waits = p
                    .get("waits_for")
                    .and_then(|w| w.as_str())
                    .unwrap_or("a large catalogue");
                format!("homepage reads wait for {waits}'s full import")
            })
        });
    let waiting = state.and_then(|s| s.waiting.as_deref());
    let waiting = match (refused, waiting) {
        (Some(r), Some(w)) if !w.trim().is_empty() => Some(format!("{r}; {w}")),
        (Some(r), _) => Some(r),
        (None, w) => w.map(str::to_owned),
    };
    let waiting = waiting.as_deref();
    match state.and_then(|s| s.import_requested.as_deref()) {
        Some(reason) => {
            let requested = format!("full import requested: {reason}");
            match waiting {
                Some(why) if !why.trim().is_empty() => {
                    one_line(Some(&format!("{requested}; {why}")))
                }
                _ => one_line(Some(&requested)),
            }
        }
        None => one_line(waiting),
    }
}
/// A text on one line of the table: its control characters and line breaks as spaces.
fn one_line(text: Option<&str>) -> String {
    match text {
        Some(text) if !text.trim().is_empty() => text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        _ => "-".into(),
    }
}
/// `pinakesd catalogue status`: one line per catalogue with its switch, phase, cards, the upstream
/// version it read (a dump's date, a list's ID), the last full import and update, how current its
/// cards are, why it waits and its last error.
pub fn status_table(config: &CataloguesConfig, states: &[State]) -> String {
    let at = |t: Option<DateTime<Utc>>| {
        t.map_or("-".to_owned(), |t| {
            t.format("%Y-%m-%d %H:%M:%SZ").to_string()
        })
    };
    let mut lines: Vec<[String; 10]> = vec![[
        "catalogue".to_owned(),
        "enabled".into(),
        "phase".into(),
        "cards".into(),
        "upstream version".into(),
        "last import".into(),
        "last update".into(),
        "current as of".into(),
        "waiting".into(),
        "last error".into(),
    ]];
    for catalogue in Catalogue::ALL {
        let state = states.iter().find(|s| s.catalogue == catalogue.name());
        lines.push([
            catalogue.name().to_owned(),
            if config.enabled(catalogue) {
                "yes"
            } else {
                "no"
            }
            .into(),
            state.map_or("idle", |s| s.phase.as_str()).to_owned(),
            state.map_or(0, |s| s.cards).to_string(),
            one_line(state.and_then(|s| s.upstream_version.as_deref())),
            at(state.and_then(|s| s.last_import_at)),
            at(state.and_then(|s| s.last_update_at)),
            at(state.and_then(|s| s.current_as_of)),
            waiting_cell(state),
            one_line(state.and_then(|s| s.last_error.as_deref())),
        ]);
    }
    let widths: Vec<usize> = (0..10)
        .map(|i| {
            lines
                .iter()
                .map(|l| l[i].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    lines
        .iter()
        .map(|l| {
            l.iter()
                .zip(&widths)
                .map(|(cell, w)| format!("{cell:<w$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_status_table_lists_every_catalogue() {
        let mut config = CataloguesConfig::default();
        config.osm.enabled = false;
        let state = State {
            catalogue: "osm".into(),
            phase: "disabled".into(),
            run: None,
            generation: 2,
            cursor: None,
            position: None,
            cards: 1234,
            upstream_version: Some("2026-11-24".into()),
            last_import_at: Some("2026-11-24T10:00:00Z".parse().unwrap()),
            last_update_at: None,
            current_as_of: Some("2026-11-23T00:00:00Z".parse().unwrap()),
            scope: None,
            generation_cards: 0,
            shrink_allowed: false,
            import_requested: None,
            waiting: None,
            last_error: Some("timed\nout".into()),
            error_count: 1,
            updated_at: Utc::now(),
        };
        let table = status_table(&config, &[state]);
        let lines: Vec<_> = table.lines().collect();
        assert_eq!(lines.len(), 8);
        assert!(lines[0].starts_with("catalogue"));
        let osm: Vec<_> = lines[3].split_whitespace().collect();
        assert_eq!(
            osm,
            [
                "osm",
                "no",
                "disabled",
                "1234",
                "2026-11-24",
                "2026-11-24",
                "10:00:00Z",
                "-",
                "2026-11-23",
                "00:00:00Z",
                "-",
                "timed",
                "out"
            ]
        );
        let wikidata: Vec<_> = lines[1].split_whitespace().collect();
        assert_eq!(
            wikidata,
            ["wikidata", "yes", "idle", "0", "-", "-", "-", "-", "-", "-"]
        );
        // A full import an update asked for shows with its reason, and why it waits.
        let mut requested = State {
            catalogue: "wikidata".into(),
            phase: "idle".into(),
            run: None,
            generation: 1,
            cursor: None,
            position: None,
            cards: 5,
            upstream_version: None,
            last_import_at: None,
            last_update_at: None,
            current_as_of: None,
            scope: None,
            generation_cards: 0,
            shrink_allowed: false,
            import_requested: Some("the change stream is gone".into()),
            waiting: Some("osm's full import is under way".into()),
            last_error: None,
            error_count: 0,
            updated_at: Utc::now(),
        };
        let table = status_table(
            &CataloguesConfig::default(),
            std::slice::from_ref(&requested),
        );
        assert!(
            table.lines().nth(1).unwrap().contains(
                "full import requested: the change stream is gone; osm's full import is under way"
            ),
            "{table}"
        );
        requested.waiting = None;
        let table = status_table(&CataloguesConfig::default(), &[requested]);
        assert!(
            table
                .lines()
                .nth(1)
                .unwrap()
                .trim_end()
                .ends_with("full import requested: the change stream is gone  -"),
            "{table}"
        );
    }

    /// A dump the importer refused shows in `waiting` (from its position), before why it waits,
    /// and alone when it does not.
    #[test]
    fn the_status_table_shows_a_refused_dump() {
        let mut refused = State {
            catalogue: "musicbrainz".into(),
            phase: "failed".into(),
            run: None,
            generation: 1,
            cursor: None,
            position: Some(
                serde_json::json!({"version": "20261121-001001", "refused": "20261125-001001"}),
            ),
            cards: 5,
            upstream_version: None,
            last_import_at: None,
            last_update_at: None,
            current_as_of: None,
            import_requested: None,
            waiting: Some("free disk 1.0 GB".into()),
            last_error: Some("refused (8 of 17 cards gone)".into()),
            error_count: 1,
            updated_at: Utc::now(),
            scope: None,
            generation_cards: 0,
            shrink_allowed: false,
        };
        let row = |state: &State| {
            status_table(&CataloguesConfig::default(), std::slice::from_ref(state))
                .lines()
                .nth(2)
                .unwrap()
                .to_owned()
        };
        assert!(
            row(&refused).contains("refused: 20261125-001001; free disk 1.0 GB"),
            "{}",
            row(&refused)
        );
        refused.waiting = None;
        assert!(
            row(&refused).contains("refused: 20261125-001001  refused"),
            "{}",
            row(&refused)
        );
        // The software catalogue's: each registry's file.
        refused.position =
            Some(serde_json::json!({"refused": {"crates.io": "c2", "flathub": "f9"}}));
        assert!(
            row(&refused).contains("refused: crates.io c2, flathub f9"),
            "{}",
            row(&refused)
        );
        refused.position = Some(serde_json::json!({"version": "20261121-001001"}));
        assert!(!row(&refused).contains("refused:"), "{}", row(&refused));
    }

    /// The table's own checks refuse a row `Card::checked` let through (here a link over the
    /// column's 2048 bytes): that row alone is left out, unstamped and uncounted, and the rest of
    /// its part is written.
    #[sqlx::test(migrations = "../../migrations")]
    async fn the_table_s_checks_refuse_one_row_and_keep_its_part(pool: PgPool) {
        ensure_state(&pool, Catalogue::Wikidata).await.unwrap();
        let card = |n: usize| Card {
            source_id: format!("Q{n}"),
            uri: format!("https://en.wikipedia.org/wiki/Item_{n}"),
            r#type: "Thing".into(),
            name: format!("Item {n}"),
            ..Default::default()
        };
        let mut rows: Vec<Row> = (0..4)
            .map(|n| card(n).checked(Catalogue::Wikidata).unwrap())
            .collect();
        rows[2].uri = format!("https://example.org/{}", "a".repeat(3000));
        let mut tx = pool.begin().await.unwrap();
        let report = write_part(&mut tx, Catalogue::Wikidata, 1, &rows)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            (report.inserted, report.rejected, report.unchanged),
            (3, 1, 0)
        );
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT source_id FROM catalogue_card ORDER BY source_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(ids, ["Q0", "Q1", "Q3"]);
        assert_eq!(
            state(&pool, Catalogue::Wikidata)
                .await
                .unwrap()
                .unwrap()
                .cards,
            3
        );
        // Again, with Q2 stored as it was before: the refused version neither replaces nor
        // stamps it, so the import that ends removes it.
        let stored = card(2).checked(Catalogue::Wikidata).unwrap();
        let mut tx = pool.begin().await.unwrap();
        write_part(&mut tx, Catalogue::Wikidata, 1, &[stored])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        let report = write_part(&mut tx, Catalogue::Wikidata, 2, &rows)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!((report.unchanged, report.rejected), (3, 1));
        assert_eq!(
            finish_import(&pool, Catalogue::Wikidata, 2).await.unwrap(),
            1
        );
    }
}
