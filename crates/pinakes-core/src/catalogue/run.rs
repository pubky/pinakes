//! One catalogue's importer and the task that runs it.
//!
//! An importer ([`Importer`]) reads its catalogue's bulk files and writes cards through its
//! [`Job`]. The task ([`run`]) is started by `pinakesd` only for a catalogue that is on, and runs
//! until the daemon stops. It:
//!
//! - holds the catalogue's advisory lock (`locks::CATALOGUE`) for as long as it runs, so two
//!   daemons never import one catalogue and `pinakesd catalogue drop` waits for no importer;
//! - asks the importer what is due ([`Importer::plan`]): a full import when none has finished,
//!   then an update once a day;
//! - runs a full import of a large catalogue only while it holds the large-import lock
//!   (`locks::CATALOGUE_LARGE_IMPORT`), so one runs at a time;
//! - checks the free disk under `catalogues.dir` against what the importer says the run needs
//!   ([`Importer::disk_need`]) before it starts or resumes one, and waits (saying why in the state
//!   row and the log) while it is short: nothing is started half;
//! - records the run in `catalogue_state` (`run`, `cursor`, `generation`), so a run cut off (a
//!   restart, a failure) resumes from its importer's last cursor;
//! - after a full import, removes the cards it did not read ([`store::finish_import`]).
//!
//! A failed run is recorded (`phase` failed, `last_error`, `error_count`) and tried again after
//! [`Pace::retry`], from its cursor. Importers do not use the fetch queue, the refresh times or
//! the change journal.
use super::{
    store::{self, State, WriteReport},
    Card, Catalogue,
};
use crate::config::Config;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures_util::FutureExt;
use sqlx::{Connection, PgConnection, PgPool};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::watch;

/// A run of an importer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Run {
    /// A full import: the first, or a re-import. Every card it reads is stamped with a new
    /// generation, and the cards it did not read are removed when it finishes.
    Import,
    /// An update: the changes since the last import or update.
    Update,
}
impl Run {
    pub fn name(self) -> &'static str {
        match self {
            Run::Import => "import",
            Run::Update => "update",
        }
    }
    fn parse(name: &str) -> Option<Self> {
        match name {
            "import" => Some(Run::Import),
            "update" => Some(Run::Update),
            _ => None,
        }
    }
}
/// What an importer does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    Run(Run),
    /// Nothing is due: ask again after this long.
    Wait(Duration),
}

/// One catalogue's importer. Steps 4–7 add one per catalogue to [`importers`].
#[async_trait]
pub trait Importer: Send + Sync + 'static {
    fn catalogue(&self) -> Catalogue;
    /// Whether its full import is a large one, run one at a time (`Catalogue::large`).
    fn large(&self) -> bool {
        self.catalogue().large()
    }
    /// The free bytes under `catalogues.dir` a run needs (its downloads and temporary files),
    /// checked before the run starts or resumes.
    fn disk_need(&self, run: Run) -> u64;
    /// How long after the last finished import or update the next update is due (the default
    /// [`Importer::plan`]'s pace). A day: the update lag alert fires after two.
    fn update_every(&self) -> Duration {
        Duration::from_secs(86400)
    }
    /// What is due, given the state row with no run under way (one under way is resumed without
    /// asking). By default: a full import when none has finished, an update [`Self::update_every`]
    /// after the last finished import or update, else a wait until then. An importer whose
    /// update needs a new full read (a new dump) answers [`Run::Import`].
    async fn plan(&self, state: &State) -> Result<Plan> {
        let Some(at) = state.current_at() else {
            return Ok(Plan::Run(Run::Import));
        };
        let due = at + chrono::Duration::from_std(self.update_every())?;
        let left = (due - chrono::Utc::now()).to_std().unwrap_or_default();
        Ok(if left.is_zero() {
            Plan::Run(Run::Update)
        } else {
            Plan::Wait(left)
        })
    }
    /// A full import: read the whole catalogue and write every card through `job`
    /// ([`Job::write`]), from [`Job::cursor`] when it resumes. The cards it does not write are
    /// removed when it returns `Ok`.
    async fn import(&self, job: &mut Job<'_>) -> Result<()>;
    /// An update: write the cards that changed and remove the items removed upstream
    /// ([`Job::remove`]), from [`Job::cursor`] when it resumes.
    async fn update(&self, job: &mut Job<'_>) -> Result<()>;
}

/// The registered importers. Each is started only when its catalogue is on.
pub fn importers(_config: &Config) -> Vec<Arc<dyn Importer>> {
    Vec::new()
}

/// What a run writes through: its catalogue, generation, resume point, work directory and the
/// configuration.
pub struct Job<'a> {
    pool: &'a PgPool,
    catalogue: Catalogue,
    run: Run,
    generation: i64,
    cursor: Option<serde_json::Value>,
    dir: &'a Path,
    config: &'a Config,
    report: WriteReport,
}
impl Job<'_> {
    pub fn catalogue(&self) -> Catalogue {
        self.catalogue
    }
    pub fn run(&self) -> Run {
        self.run
    }
    /// The generation this run stamps on the cards it writes.
    pub fn generation(&self) -> i64 {
        self.generation
    }
    /// Where to resume: the cursor of the last [`Job::write`] or [`Job::remove`] of this run, or
    /// none at its start.
    pub fn cursor(&self) -> Option<&serde_json::Value> {
        self.cursor.as_ref()
    }
    /// `catalogues.dir`, which exists: downloads and temporary files go here.
    pub fn dir(&self) -> &Path {
        self.dir
    }
    pub fn config(&self) -> &Config {
        self.config
    }
    /// `limit` of Wikidata and MusicBrainz (`[catalogues.<name>] limit`).
    pub fn limit(&self) -> Option<u64> {
        self.config.catalogues.limit(self.catalogue)
    }
    /// What the run has written so far.
    pub fn report(&self) -> WriteReport {
        self.report
    }
    /// Write `cards` ([`store::upsert`]) and save `cursor` with them: a run cut off after this
    /// resumes from `cursor`.
    pub async fn write(
        &mut self,
        cards: &[Card],
        cursor: serde_json::Value,
    ) -> Result<WriteReport> {
        let written = store::upsert(
            self.pool,
            self.catalogue,
            self.generation,
            cards,
            Some(&cursor),
        )
        .await?;
        self.cursor = Some(cursor);
        self.report += written;
        Ok(written)
    }
    /// Remove the cards with these source IDs ([`store::remove`]) and save `cursor` with them.
    pub async fn remove(
        &mut self,
        source_ids: &[String],
        cursor: serde_json::Value,
    ) -> Result<u64> {
        let removed = store::remove(self.pool, self.catalogue, source_ids, Some(&cursor)).await?;
        self.cursor = Some(cursor);
        self.report.removed += removed;
        Ok(removed)
    }
    /// The upstream version read (a dump's date), shown in `pinakesd catalogue status`.
    pub async fn upstream_version(&mut self, version: &str) -> Result<()> {
        let mut version = version.to_owned();
        crate::model::truncate(&mut version, 256);
        sqlx::query(
            "UPDATE catalogue_state SET upstream_version=$2,updated_at=now() WHERE catalogue=$1",
        )
        .bind(self.catalogue.name())
        .bind(version)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

/// How long the task waits. Fixed in the daemon ([`Pace::default`]); tests shorten it.
#[derive(Clone, Copy, Debug)]
pub struct Pace {
    /// After a failed run, before it is tried again.
    pub retry: Duration,
    /// While the free disk is short.
    pub disk: Duration,
    /// While another large full import runs, or another process holds the catalogue's lock.
    pub turn: Duration,
}
impl Default for Pace {
    fn default() -> Self {
        Self {
            retry: Duration::from_secs(600),
            disk: Duration::from_secs(600),
            turn: Duration::from_secs(60),
        }
    }
}
/// The free bytes of the file system at a path.
pub type FreeDisk = Arc<dyn Fn(&Path) -> std::io::Result<u64> + Send + Sync>;
/// What every importer's task shares.
#[derive(Clone)]
pub struct Context {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub free_disk: FreeDisk,
    pub pace: Pace,
}
impl Context {
    pub fn new(pool: PgPool, config: Arc<Config>) -> Self {
        Self {
            pool,
            config,
            free_disk: Arc::new(free_bytes),
            pace: Pace::default(),
        }
    }
}
/// The bytes an unprivileged process may still write on the file system at `path`.
pub fn free_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("a path with a NUL byte"))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a NUL-terminated string and `stat` a statvfs the call fills.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// A session advisory lock, held on a connection of its own (detached from the pool) until
/// [`SessionLock::release`] or until it is dropped, which closes the connection and so ends the
/// session and its lock.
pub struct SessionLock {
    connection: PgConnection,
    key: i64,
}
impl SessionLock {
    /// The lock `key`, or none when another session holds it.
    pub async fn try_take(pool: &PgPool, key: i64) -> Result<Option<Self>> {
        let mut connection = pool.acquire().await?.detach();
        let taken: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut connection)
            .await?;
        if taken {
            Ok(Some(Self { connection, key }))
        } else {
            let _ = connection.close().await;
            Ok(None)
        }
    }
    /// Whether the lock's session still answers (a database restart ends it, and the lock).
    pub async fn held(&mut self) -> bool {
        let answer = sqlx::query("SELECT 1").execute(&mut self.connection);
        matches!(
            tokio::time::timeout(crate::locks::LOCK_CHECK, answer).await,
            Ok(Ok(_))
        )
    }
    pub async fn release(mut self) {
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.key)
            .execute(&mut self.connection)
            .await;
        let _ = self.connection.close().await;
    }
}

/// What the task does after a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// A run finished: plan the next at once.
    Again,
    /// Wait this long, then step again.
    Wait(Duration),
}

/// Run `importer` until `shutdown`: take its catalogue's lock (waiting while another process
/// holds it), then step ([`step`]) and wait as each step says. A step cut off by the shutdown
/// leaves its run and cursor in the state row, and the next start resumes it.
pub async fn run(importer: Arc<dyn Importer>, ctx: Context, mut shutdown: watch::Receiver<bool>) {
    let catalogue = importer.catalogue();
    let mut lock: Option<SessionLock> = None;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let stepped = async {
            let held = match lock.as_mut() {
                Some(l) => l.held().await,
                None => false,
            };
            if !held {
                lock = SessionLock::try_take(&ctx.pool, catalogue.lock_key()).await?;
                if lock.is_none() {
                    let why = "another process imports this catalogue (its lock is held)";
                    set_waiting(&ctx.pool, catalogue, why).await?;
                    return Ok(Next::Wait(ctx.pace.turn));
                }
            }
            std::panic::AssertUnwindSafe(step(&*importer, &ctx))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the importer panicked")))
        };
        let next = tokio::select! {
            _ = shutdown.changed() => return,
            next = stepped => next,
        };
        let wait = match next {
            Ok(Next::Again) => continue,
            Ok(Next::Wait(wait)) => wait,
            Err(error) => {
                let error = crate::resolver::log_error(&error);
                tracing::warn!(catalogue = catalogue.name(), %error, "catalogue import failed; it resumes from its cursor");
                if let Err(error) = record_failure(&ctx.pool, catalogue, &error).await {
                    tracing::warn!(catalogue = catalogue.name(), error = %crate::resolver::log_error(&error), "catalogue failure not recorded");
                }
                ctx.pace.retry
            }
        };
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// One step of an importer's task, which holds the catalogue's lock: resume the run under way or
/// plan one, wait for the large-import turn and the free disk it needs, run it to its end and
/// record it. A failure leaves the run and its cursor for the next step.
pub async fn step(importer: &dyn Importer, ctx: &Context) -> Result<Next> {
    let catalogue = importer.catalogue();
    let pool = &ctx.pool;
    let state = store::ensure_state(pool, catalogue).await?;
    let resumed = state.run.as_deref().and_then(Run::parse);
    let run = match resumed {
        Some(run) => run,
        None => match importer.plan(&state).await? {
            Plan::Run(run) => run,
            Plan::Wait(wait) => {
                clear_waiting(pool, catalogue).await?;
                return Ok(Next::Wait(wait));
            }
        },
    };
    let large = if run == Run::Import && importer.large() {
        match SessionLock::try_take(pool, crate::locks::CATALOGUE_LARGE_IMPORT).await? {
            Some(lock) => Some(lock),
            None => {
                let why = "another large catalogue's full import runs; one runs at a time";
                set_waiting(pool, catalogue, why).await?;
                return Ok(Next::Wait(ctx.pace.turn));
            }
        }
    } else {
        None
    };
    let dir = Path::new(&ctx.config.catalogues.dir);
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("create catalogues.dir {}", dir.display()))?;
    let need = importer.disk_need(run);
    let free = (ctx.free_disk)(dir).with_context(|| format!("free disk at {}", dir.display()))?;
    if free < need {
        let why = format!(
            "free disk: the {} needs {} under {}, {} free",
            run.name(),
            gigabytes(need),
            dir.display(),
            gigabytes(free)
        );
        set_waiting(pool, catalogue, &why).await?;
        if let Some(lock) = large {
            lock.release().await;
        }
        return Ok(Next::Wait(ctx.pace.disk));
    }
    // Start the run, or take it up again: a new full import begins a new generation.
    let (generation, cursor) = if resumed.is_some() {
        sqlx::query(
            "UPDATE catalogue_state SET phase=$2,waiting=NULL,updated_at=now() WHERE catalogue=$1",
        )
        .bind(catalogue.name())
        .bind(run.name())
        .execute(pool)
        .await?;
        (state.generation, state.cursor)
    } else {
        let generation: i64 = sqlx::query_scalar("UPDATE catalogue_state SET run=$2,phase=$2,cursor=NULL,waiting=NULL,generation=generation+($2='import')::int,updated_at=now() WHERE catalogue=$1 RETURNING generation")
            .bind(catalogue.name())
            .bind(run.name())
            .fetch_one(pool)
            .await?;
        (generation, None)
    };
    tracing::info!(
        catalogue = catalogue.name(),
        run = run.name(),
        generation,
        resumed = resumed.is_some(),
        "catalogue run starts"
    );
    let mut job = Job {
        pool,
        catalogue,
        run,
        generation,
        cursor,
        dir,
        config: &ctx.config,
        report: WriteReport::default(),
    };
    match run {
        Run::Import => importer.import(&mut job).await?,
        Run::Update => importer.update(&mut job).await?,
    }
    let mut report = job.report;
    if run == Run::Import {
        report.removed += store::finish_import(pool, catalogue, generation).await?;
    }
    sqlx::query(
        "UPDATE catalogue_state SET run=NULL,cursor=NULL,phase='idle',waiting=NULL,\
         last_import_at=CASE WHEN $2='import' THEN now() ELSE last_import_at END,\
         last_update_at=CASE WHEN $2='update' THEN now() ELSE last_update_at END,updated_at=now() WHERE catalogue=$1",
    )
    .bind(catalogue.name())
    .bind(run.name())
    .execute(pool)
    .await?;
    if let Some(lock) = large {
        lock.release().await;
    }
    tracing::info!(
        catalogue = catalogue.name(),
        run = run.name(),
        inserted = report.inserted,
        changed = report.changed,
        unchanged = report.unchanged,
        rejected = report.rejected,
        removed = report.removed,
        "catalogue run finished"
    );
    Ok(Next::Again)
}
fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}
/// Say why the importer waits, logged once when the reason changes.
async fn set_waiting(pool: &PgPool, catalogue: Catalogue, why: &str) -> Result<()> {
    store::ensure_state(pool, catalogue).await?;
    let changed = sqlx::query("UPDATE catalogue_state SET waiting=$2,updated_at=now() WHERE catalogue=$1 AND waiting IS DISTINCT FROM $2")
        .bind(catalogue.name())
        .bind(why)
        .execute(pool)
        .await?
        .rows_affected();
    if changed > 0 {
        tracing::info!(
            catalogue = catalogue.name(),
            waiting = why,
            "catalogue import waits"
        );
    }
    Ok(())
}
async fn clear_waiting(pool: &PgPool, catalogue: Catalogue) -> Result<()> {
    sqlx::query("UPDATE catalogue_state SET waiting=NULL,updated_at=now() WHERE catalogue=$1 AND waiting IS NOT NULL")
        .bind(catalogue.name())
        .execute(pool)
        .await?;
    Ok(())
}
/// A failed run: `failed`, its error (as logged: no secret) and one more in the count. The run
/// and its cursor stay.
async fn record_failure(pool: &PgPool, catalogue: Catalogue, error: &str) -> Result<()> {
    let mut error = error.to_owned();
    crate::model::truncate(&mut error, 1024);
    store::ensure_state(pool, catalogue).await?;
    sqlx::query("UPDATE catalogue_state SET phase='failed',waiting=NULL,last_error=$2,error_count=error_count+1,updated_at=now() WHERE catalogue=$1")
        .bind(catalogue.name())
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}
