//! The ODbL derivative database as a static file: the licence notice, then one `osm_place` row per
//! line (`api::OSM_DUMP_LINE`), then, while the OSM catalogue is on, one imported place per line
//! (`api::OSM_DUMP_CATALOGUE_LINE`, with the renames the updates saw), the renames of objects with no imported place, and the successors found, gzip-compressed. The daemon writes it from one read-only snapshot
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
/// written. `catalogue`: the OSM catalogue is on, and its places are written too. On any failure
/// the previous file stays and the temporary one is removed. Refused while another writer holds
/// the directory's lock.
pub async fn write(pool: &PgPool, dir: &Path, catalogue: bool) -> Result<usize> {
    let _one = WRITER.lock().await;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("dump directory {}", dir.display()))?;
    let lock = lock(dir)?.context("another writer holds the dump directory's lock")?;
    remove_temps(dir)?;
    let tmp = temp_path(dir);
    match write_to(pool, &tmp, catalogue).await {
        Ok(places) => replace(dir, tmp, lock).await.map(|()| places),
        Err(error) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(error)
        }
    }
}

/// Rename `tmp` into place and sync the directory, in one blocking task that holds the
/// directory's `lock` until its last file operation, so a cancelled write never frees the lock
/// while its rename is still to come. On a failure the temporary file is removed.
async fn replace(dir: &Path, tmp: PathBuf, lock: std::fs::File) -> Result<()> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || {
        let _lock = lock;
        let replaced = (|| -> Result<()> {
            #[cfg(test)]
            before_rename(&dir);
            std::fs::rename(&tmp, path(&dir))?;
            // The rename is durable once the directory is.
            std::fs::File::open(&dir)?.sync_all()?;
            Ok(())
        })();
        if replaced.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        replaced
    })
    .await?
}

/// A test's pause before a writer's final rename, by dump directory.
#[cfg(test)]
pub(crate) type Pause = std::sync::Arc<dyn Fn() + Send + Sync>;
#[cfg(test)]
pub(crate) static BEFORE_RENAME: std::sync::Mutex<Vec<(PathBuf, Pause)>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(test)]
fn before_rename(dir: &Path) {
    let pause = BEFORE_RENAME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(at, _)| at == dir)
        .map(|(_, pause)| pause.clone());
    if let Some(pause) = pause {
        pause();
    }
}

async fn write_to(pool: &PgPool, tmp: &Path, catalogue: bool) -> Result<usize> {
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
        if catalogue {
            let mut rows =
                sqlx::query_scalar::<_, Value>(crate::api::OSM_DUMP_CATALOGUE_LINE).fetch(&mut *tx);
            while let Some(mut row) = rows.try_next().await? {
                let path = crate::model::type_path(row["type"].as_str().unwrap_or("Place"));
                row["type_path"] = json!(path);
                lines.send(row.to_string()).await?;
                places += 1;
            }
            drop(rows);
            for query in [
                crate::api::OSM_DUMP_RENAME_LINE,
                crate::api::OSM_DUMP_SUCCESSOR_LINE,
                crate::api::OSM_DUMP_REMOVED_LINE,
            ] {
                let mut rows = sqlx::query_scalar::<_, Value>(query).fetch(&mut *tx);
                while let Some(row) = rows.try_next().await? {
                    lines.send(row.to_string()).await?;
                }
            }
        }
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
pub async fn run(
    pool: PgPool,
    dir: PathBuf,
    every: Duration,
    catalogue: bool,
    mut shutdown: watch::Receiver<bool>,
) {
    let leftovers = dir.clone();
    match tokio::task::spawn_blocking(move || remove_leftovers(&leftovers)).await {
        Ok(Ok(0)) => {}
        Ok(Ok(removed)) => tracing::info!(removed, "removed leftover ODbL dump temporary files"),
        Ok(Err(error)) => {
            tracing::warn!(error = %crate::resolver::log_error(&error), "could not remove leftover ODbL dump files")
        }
        Err(error) => {
            tracing::warn!(error = %crate::resolver::log_error(&error), "could not remove leftover ODbL dump files")
        }
    }
    let mut wait = due_in(&dir, every);
    loop {
        tokio::select! { _=shutdown.changed()=>break, _=tokio::time::sleep(wait)=>{} }
        let due = due_in(&dir, every);
        if !due.is_zero() {
            wait = due;
            continue;
        }
        let started = std::time::Instant::now();
        let written =
            tokio::select! { _=shutdown.changed()=>break, r=write(&pool, &dir, catalogue)=>r };
        crate::telemetry::background_pass("osm_dump", &written);
        wait = match written {
            Ok(places) => {
                // How long its one snapshot stayed open.
                let elapsed_ms = started.elapsed().as_millis() as u64;
                tracing::info!(places, elapsed_ms, file=%path(&dir).display(), "ODbL dump written");
                every
            }
            Err(error) => {
                tracing::warn!(error=%crate::resolver::log_error(&error), "ODbL dump failed; retrying in five minutes");
                Duration::from_secs(300)
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Arc, Mutex};

    /// A write cancelled at its last step (shutdown) keeps the directory's lock until its rename
    /// is done: another writer can neither take the lock nor publish while the cancelled write's
    /// rename is still to come, and once it has gone the next writer's file stays in place.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_cancelled_write_keeps_the_lock_until_its_rename_is_done(pool: PgPool) {
        let dir = std::env::temp_dir().join(format!("pinakes-cancelled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(&pool, &dir, false).await.unwrap();
        let before = std::fs::read(path(&dir)).unwrap();
        // Writer A stops before its rename until `go`.
        let (paused, reached) = std::sync::mpsc::channel::<()>();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        BEFORE_RENAME.lock().unwrap().push((
            dir.clone(),
            Arc::new(move || {
                let _ = paused.send(());
                let _ = wait.lock().unwrap().recv();
            }),
        ));
        let a = {
            let (pool, dir) = (pool.clone(), dir.clone());
            tokio::spawn(async move { write(&pool, &dir, false).await })
        };
        tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("writer A reached its rename");
        a.abort();
        assert!(a.await.unwrap_err().is_cancelled());
        // A's rename is still to come: B neither takes the lock nor publishes.
        assert!(lock(&dir).unwrap().is_none(), "the lock is free");
        let refused = write(&pool, &dir, false).await.unwrap_err();
        assert!(format!("{refused:#}").contains("lock"), "{refused:#}");
        assert_eq!(std::fs::read(path(&dir)).unwrap(), before);
        go.send(()).unwrap();
        BEFORE_RENAME.lock().unwrap().retain(|(at, _)| at != &dir);
        // A's file work ends, its file in place, and the lock is free again.
        let mut free = false;
        for _ in 0..500 {
            if lock(&dir).unwrap().is_some() {
                free = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(free, "the lock was never released");
        let a_file = std::fs::metadata(path(&dir)).unwrap().ino();
        write(&pool, &dir, false).await.unwrap();
        let b_file = std::fs::metadata(path(&dir)).unwrap().ino();
        assert_ne!(a_file, b_file);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            std::fs::metadata(path(&dir)).unwrap().ino(),
            b_file,
            "B's file stays in place"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
