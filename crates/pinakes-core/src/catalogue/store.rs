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
    pub cards: i64,
    pub upstream_version: Option<String>,
    pub last_import_at: Option<DateTime<Utc>>,
    pub last_update_at: Option<DateTime<Utc>>,
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

/// The state row of `catalogue`, made when it has none.
pub async fn ensure_state(pool: &PgPool, catalogue: Catalogue) -> Result<State> {
    sqlx::query("INSERT INTO catalogue_state(catalogue) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(catalogue.name())
        .execute(pool)
        .await?;
    Ok(state(pool, catalogue).await?.expect("the row just made"))
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
                tracing::debug!(catalogue = catalogue.name(), source_id = %card.source_id, ?reject, "catalogue card refused");
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
    ensure_state(pool, catalogue).await?;
    let (rows, rejected) = rows(catalogue, cards);
    let mut report = WriteReport {
        rejected,
        ..WriteReport::default()
    };
    let parts = rows.chunks(BATCH_ROWS).count();
    if parts == 0 {
        if let Some(cursor) = cursor {
            save_cursor(pool, catalogue, cursor).await?;
        }
        return Ok(report);
    }
    for (n, part) in rows.chunks(BATCH_ROWS).enumerate() {
        let mut tx = pool.begin().await?;
        report += write_part(&mut tx, catalogue, generation, part).await?;
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
async fn write_part(
    tx: &mut Transaction<'_, Postgres>,
    catalogue: Catalogue,
    generation: i64,
    part: &[Row],
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
    let (inserted, changed): (i64, i64) = sqlx::query_as(
        "WITH input AS (SELECT * FROM jsonb_to_recordset($3::jsonb) AS x(source text,source_id text,uri text,type text,name text,other_names text[],description text,labels text[],keys text[],related_uris text[],lon float8,lat float8,fame real,details jsonb,content_hash text)),\
         written AS (INSERT INTO catalogue_card AS c(catalogue,source,source_id,uri,type,name,other_names,description,labels,keys,related_uris,point,fame,details,content_hash,seen_generation) \
          SELECT $1,source,source_id,uri,type,name,other_names,description,labels,keys,related_uris,\
           CASE WHEN lon IS NULL OR lat IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint(lon,lat),4326) END,fame,details,decode(content_hash,'hex'),$2 FROM input \
          ON CONFLICT (catalogue,source_id) DO UPDATE SET source=excluded.source,uri=excluded.uri,type=excluded.type,name=excluded.name,other_names=excluded.other_names,\
           description=excluded.description,labels=excluded.labels,keys=excluded.keys,related_uris=excluded.related_uris,point=excluded.point,fame=excluded.fame,\
           details=excluded.details,content_hash=excluded.content_hash,seen_generation=greatest(c.seen_generation,excluded.seen_generation),updated_at=now() \
          WHERE c.content_hash<>excluded.content_hash \
          RETURNING (c.xmax::text='0') AS inserted) \
         SELECT count(*) FILTER (WHERE inserted),count(*) FILTER (WHERE NOT inserted) FROM written",
    )
    .bind(catalogue.name())
    .bind(generation)
    .bind(serde_json::Value::Array(input))
    .fetch_one(&mut **tx)
    .await?;
    let ids: Vec<&str> = part.iter().map(|r| r.source_id.as_str()).collect();
    sqlx::query("UPDATE catalogue_card SET seen_generation=$2 WHERE catalogue=$1 AND source_id=ANY($3) AND seen_generation<$2")
        .bind(catalogue.name())
        .bind(generation)
        .bind(&ids)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE catalogue_state SET cards=cards+$2,updated_at=now() WHERE catalogue=$1")
        .bind(catalogue.name())
        .bind(inserted)
        .execute(&mut **tx)
        .await?;
    Ok(WriteReport {
        inserted: inserted as u64,
        changed: changed as u64,
        unchanged: (part.len() as i64 - inserted - changed) as u64,
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
        removed += gone;
    }
    if parts == 0 {
        if let Some(cursor) = cursor {
            save_cursor(pool, catalogue, cursor).await?;
        }
    }
    Ok(removed)
}

/// The end of a full import of `generation`: remove the cards of `catalogue` it did not read
/// (`seen_generation` below it), so an item removed upstream leaves with the import that no
/// longer finds it. Reads the catalogue's rows in `source_id` order, [`SWEEP_ROWS`] per
/// transaction. The number removed.
pub async fn finish_import(pool: &PgPool, catalogue: Catalogue, generation: i64) -> Result<u64> {
    sweep(pool, catalogue, Some(generation), |_| {}).await
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

/// `pinakesd catalogue status`: one line per catalogue with its switch, phase, cards, the last
/// full import and update, why it waits and its last error.
pub fn status_table(config: &CataloguesConfig, states: &[State]) -> String {
    let at = |t: Option<DateTime<Utc>>| {
        t.map_or("-".to_owned(), |t| {
            t.format("%Y-%m-%d %H:%M:%SZ").to_string()
        })
    };
    let mut lines = vec![[
        "catalogue".to_owned(),
        "enabled".into(),
        "phase".into(),
        "cards".into(),
        "last import".into(),
        "last update".into(),
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
            at(state.and_then(|s| s.last_import_at)),
            at(state.and_then(|s| s.last_update_at)),
            state
                .and_then(|s| s.waiting.clone())
                .unwrap_or_else(|| "-".into()),
            state
                .and_then(|s| s.last_error.clone())
                .unwrap_or_else(|| "-".into()),
        ]);
    }
    let widths: Vec<usize> = (0..8)
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
            cards: 1234,
            upstream_version: Some("2026-11-24".into()),
            last_import_at: Some("2026-11-24T10:00:00Z".parse().unwrap()),
            last_update_at: None,
            waiting: None,
            last_error: Some("timeout".into()),
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
                "10:00:00Z",
                "-",
                "-",
                "timeout"
            ]
        );
        let wikidata: Vec<_> = lines[1].split_whitespace().collect();
        assert_eq!(
            wikidata,
            ["wikidata", "yes", "idle", "0", "-", "-", "-", "-"]
        );
    }
}
