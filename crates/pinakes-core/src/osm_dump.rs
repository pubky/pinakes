//! The ODbL derivative database as a static file: the licence notice, then one `osm_place` row per
//! line (`api::OSM_DUMP_LINE`), gzip-compressed. The daemon writes it from one read-only snapshot
//! every `limits.osm_dump_every_hours` into a temporary file and renames it into place, so a
//! reader always sees a whole file; the API only serves that file and never reads the database
//! for it. A lock file keeps it to one writer per directory, across daemons.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::sync::watch;

/// The file name, in `osm.dump_dir`.
pub const FILE: &str = "osm_place.ndjson.gz";
/// The lock file, in `osm.dump_dir`: whoever holds it writes, and every temporary file is then a
/// leftover.
pub const LOCK: &str = "osm_place.ndjson.gz.lock";
/// One writer per process; the lock file keeps it to one per directory.
static WRITER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The first line of the file.
pub fn notice() -> Value {
    json!({"notice":crate::resolver::osm::ATTRIBUTION,"licence":"ODbL-1.0","licence_url":"https://opendatacommons.org/licenses/odbl/1-0/","attribution_url":"https://www.openstreetmap.org/copyright","table":"osm_place"})
}

/// When the file was last written, if it exists.
pub fn written_at(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(path(dir)).and_then(|m| m.modified()).ok()
}

/// How long until the file is due: zero when it is missing or older than `every`.
pub fn due_in(dir: &Path, every: Duration) -> Duration {
    written_at(dir)
        .and_then(|at| at.elapsed().ok().or(Some(Duration::ZERO)))
        .map_or(Duration::ZERO, |age| every.saturating_sub(age))
}

/// A temporary file of this writer's own (`{FILE}.{pid}.{random}.tmp`): two writers never write
/// into one file.
pub fn temp_path(dir: &Path) -> PathBuf {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap_or_default();
    let random: String = random.iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{FILE}.{}.{random}.tmp", std::process::id()))
}
/// Take the directory's lock file without waiting: `None` while another writer holds it. The lock
/// goes with the returned file.
pub fn lock(dir: &Path) -> std::io::Result<Option<std::fs::File>> {
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}
/// Delete the temporary files an interrupted writer left; call it holding the lock. Returns how
/// many went.
pub fn remove_temps(dir: &Path) -> std::io::Result<usize> {
    let mut removed = 0;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!("{FILE}.")) && name.ends_with(".tmp") {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// At startup: remove the temporary files an interrupted writer left, under the lock (none
/// while another writer holds it, nothing when the directory does not exist yet).
pub fn remove_leftovers(dir: &Path) -> std::io::Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    match lock(dir)? {
        Some(_lock) => remove_temps(dir),
        None => Ok(0),
    }
}

/// Write the file from one read-only snapshot and rename it into place; returns the places
/// written. On any failure the previous file stays and the temporary one is removed. Refused while
/// another writer holds the directory's lock.
pub async fn write(pool: &PgPool, dir: &Path) -> Result<usize> {
    let _one = WRITER.lock().await;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("dump directory {}", dir.display()))?;
    let _lock = lock(dir)?.context("another writer holds the dump directory's lock")?;
    remove_temps(dir)?;
    let tmp = temp_path(dir);
    let result = write_to(pool, &tmp).await;
    match result {
        Ok(places) => {
            tokio::fs::rename(&tmp, path(dir)).await?;
            // The rename is durable once the directory is.
            let dir = dir.to_owned();
            tokio::task::spawn_blocking(move || std::fs::File::open(dir)?.sync_all()).await??;
            Ok(places)
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(error)
        }
    }
}

async fn write_to(pool: &PgPool, tmp: &Path) -> Result<usize> {
    use futures_util::TryStreamExt;
    let (lines, mut received) = tokio::sync::mpsc::channel::<String>(256);
    let file = tmp.to_owned();
    // Compression and file writes are blocking work, off the async workers.
    let writer = tokio::task::spawn_blocking(move || -> Result<()> {
        let out = std::fs::File::create(&file)?;
        let mut gz = flate2::write::GzEncoder::new(
            std::io::BufWriter::new(out),
            flate2::Compression::default(),
        );
        while let Some(line) = received.blocking_recv() {
            gz.write_all(line.as_bytes())?;
            gz.write_all(b"\n")?;
        }
        gz.finish()?.into_inner()?.sync_all()?;
        Ok(())
    });
    let read = async {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        // A large table takes longer than any request: no statement timeout for this read.
        sqlx::query("SET LOCAL statement_timeout = 0")
            .execute(&mut *tx)
            .await?;
        lines.send(notice().to_string()).await?;
        let mut rows = sqlx::query_scalar::<_, Value>(crate::api::OSM_DUMP_LINE).fetch(&mut *tx);
        let mut places = 0;
        while let Some(row) = rows.try_next().await? {
            lines.send(row.to_string()).await?;
            places += 1;
        }
        drop(rows);
        tx.commit().await?;
        anyhow::Ok(places)
    };
    let read = read.await;
    drop(lines);
    // A failed write closes the channel too: its own error comes first.
    writer.await??;
    read
}

/// Keep the file current: write it when it is missing or stale (another daemon sharing the
/// directory may have written it), checking again after `every`. A failed write is retried after
/// five minutes. Shutdown cancels a write in progress: the previous file stays, and the next
/// start (or the next writer) removes the temporary one.
pub async fn run(pool: PgPool, dir: PathBuf, every: Duration, mut shutdown: watch::Receiver<bool>) {
    let leftovers = dir.clone();
    match tokio::task::spawn_blocking(move || remove_leftovers(&leftovers)).await {
        Ok(Ok(0)) => {}
        Ok(Ok(removed)) => tracing::info!(removed, "removed leftover ODbL dump temporary files"),
        Ok(Err(error)) => tracing::warn!(%error, "could not remove leftover ODbL dump files"),
        Err(error) => tracing::warn!(%error, "could not remove leftover ODbL dump files"),
    }
    let mut wait = due_in(&dir, every);
    loop {
        tokio::select! { _=shutdown.changed()=>break, _=tokio::time::sleep(wait)=>{} }
        let due = due_in(&dir, every);
        if !due.is_zero() {
            wait = due;
            continue;
        }
        let written = tokio::select! { _=shutdown.changed()=>break, r=write(&pool, &dir)=>r };
        wait = match written {
            Ok(places) => {
                tracing::info!(places, file=%path(&dir).display(), "ODbL dump written");
                every
            }
            Err(error) => {
                tracing::warn!(error=%format!("{error:#}"), "ODbL dump failed; retrying in five minutes");
                Duration::from_secs(300)
            }
        };
    }
}
