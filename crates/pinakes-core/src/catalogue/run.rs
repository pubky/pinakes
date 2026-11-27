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
//! - after a full import, removes the cards it did not read ([`store::finish_import`]), unless the
//!   import read fewer than half the cards the catalogue holds ([`SHRINK_FLOOR`]): then the run
//!   fails (`shrunk`) and removes nothing, until the importer says the drop is expected
//!   ([`Job::expect_shrink`]), the operator allows it (`pinakesd catalogue allow-shrink`,
//!   [`store::allow_shrink`]) or the import's scope is narrower (a lower `limit`, a higher
//!   `min_citations`; [`CataloguesConfig::scope`](super::CataloguesConfig::scope));
//! - keeps how current the cards are (`upstream_at`, the upstream data time the importer
//!   states with [`Job::set_upstream_at`]), which the update lag metric measures.
//!
//! A failed run is recorded (`phase` failed, `last_error` as [`failure::shown`] shows it,
//! `error_count`) and tried again after [`Pace::retry`], from its cursor. Importers do not use
//! the fetch queue, the refresh times or the change journal.
use super::{
    failure::{self, Failure},
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
/// How an update ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateEnd {
    /// The update is done: the cards are as current as [`Job::set_upstream_at`] says (or as
    /// before, when it says nothing).
    Done,
    /// The changes can't be applied (a Wikidata change stream more than five days behind, a
    /// replication sequence that is gone): the update ends without marking the cards current,
    /// and a full import is asked for (`import_requested`). It begins as any new full import:
    /// it waits for the large-import turn and the disk, and holds no turn while it waits. The
    /// reason is the importer's own fixed text.
    NeedsImport(&'static str),
}
/// A run its importer abandons (`Err(Abandoned(..).into())` from [`Importer::import`] or
/// [`Importer::update`]): what it read can't be applied (a dump refused for its checksums, its
/// shape or what it would remove). The run ends without a full import's removal of the cards it
/// did not read: its run and cursor are cleared, the position the importer set
/// ([`Job::set_position`]) is kept, so its plan can tell what it refused, and the failure is
/// recorded as any. Nothing else of the run is kept.
#[derive(Debug)]
pub struct Abandoned(pub Failure);
impl std::fmt::Display for Abandoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Abandoned {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// What an importer does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    Run(Run),
    /// Nothing is due: ask again after this long.
    Wait(Duration),
}

/// One catalogue's importer; [`importers`] lists them.
#[async_trait]
pub trait Importer: Send + Sync + 'static {
    /// Its catalogue; whether its full import is a large one is `Catalogue::large`.
    fn catalogue(&self) -> Catalogue;
    /// The free bytes a run still needs in its directory (`catalogues.dir/<name>`: downloads and
    /// temporary files), checked before the run starts or resumes. `state` says whether the run
    /// resumes (`run`, `cursor`), so a resumed run asks only for what its files do not hold yet.
    fn disk_need(&self, run: Run, state: &State) -> u64;
    /// How long after the last finished import or update the next update is due (the default
    /// [`Importer::plan`]'s pace). A day. The update lag is measured from `upstream_at`, the
    /// upstream data time the importer states, not from when a run finished: an update that
    /// finds nothing new finishes without moving it, and a source that stops publishing shows
    /// as lag.
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
    /// ([`Job::write`]), from [`Job::cursor`] when it resumes, and say how current it is
    /// ([`Job::set_upstream_at`]: the dump's date; the time the import finishes when it says
    /// nothing). The cards it does not write are removed when it returns `Ok`. A failure is a
    /// [`Failure`] (`failure`), so no upstream's text reaches the log.
    async fn import(&self, job: &mut Job<'_>) -> Result<()>;
    /// An update: write the cards that changed and remove the items removed upstream
    /// ([`Job::remove`]), from [`Job::cursor`] when it resumes. It starts from
    /// [`Job::position`], where the last finished run left off (a replication sequence, a time),
    /// leaves the next one with [`Job::set_position`], and says how current the cards are now
    /// ([`Job::set_upstream_at`]: the time of the last change it applied). Finding nothing new
    /// is a finished update that moves nothing. [`UpdateEnd::NeedsImport`] hands over to a full
    /// import.
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd>;
    /// Whether the full imports this importer runs are asked for by hand (`pinakesd catalogue
    /// import`'s, [`run_once`]): [`Job::asked`]. Kept with the run, never in the state row, so a
    /// by-hand run that fails or is cut off leaves no standing request behind.
    fn asked(&self) -> bool {
        false
    }
}

/// The registered importers. `pinakesd` starts those of the catalogues that are on
/// ([`to_start`]).
pub fn importers(config: &Config) -> Vec<Arc<dyn Importer>> {
    super::musicbrainz::importer(config)
        .into_iter()
        .chain(super::osm::importer(config))
        .collect()
}

/// What a run writes through: its catalogue, generation, resume point, position, work directory
/// and the configuration.
pub struct Job<'a> {
    pool: &'a PgPool,
    catalogue: Catalogue,
    run: Run,
    generation: i64,
    cursor: Option<serde_json::Value>,
    /// The position to keep when the run finishes ([`Job::set_position`]).
    position: Option<serde_json::Value>,
    /// How current the cards are when the run finishes ([`Job::set_upstream_at`]).
    upstream_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The state row as the run started.
    state: State,
    /// A full import asked for ([`Job::asked`]).
    asked: bool,
    /// Whether the importer said this full import may hold far fewer cards than the last
    /// ([`Job::expect_shrink`]).
    shrink_expected: bool,
    dir: &'a Path,
    config: &'a Config,
    report: WriteReport,
    free_disk: FreeDisk,
}
/// A run that can't go on yet (short free disk once its download's size is known, a setting it
/// needs): the runner records why in `waiting`, keeps the run and its cursor, and steps again
/// after `Pace::disk`. Not a failure. The reason is the importer's own text, naming nothing an
/// upstream sent.
#[derive(Debug)]
pub struct Waits(pub String);
impl std::fmt::Display for Waits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Waits {}
impl<'a> Job<'a> {
    /// The database the run writes to (an importer reads its stored cards through it).
    pub fn pool(&self) -> &'a PgPool {
        self.pool
    }
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
    /// A full import that starts over, dropping a cut-off attempt's cursor (of another source or
    /// dump): a generation of its own, so the cards that attempt wrote and this one does not read
    /// are removed at its end as any others.
    /// The shrink floor counts only this generation's cards ([`SHRINK_FLOOR`]), and an operator's
    /// allowance to shrink, given to the attempt dropped, is taken back.
    pub async fn new_generation(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.run == Run::Import,
            "only a full import starts a generation"
        );
        self.generation = sqlx::query_scalar("UPDATE catalogue_state SET generation=generation+1,generation_cards=0,shrink_allowed=false,cursor=NULL,updated_at=now() WHERE catalogue=$1 RETURNING generation")
            .bind(self.catalogue.name())
            .fetch_one(self.pool)
            .await?;
        self.cursor = None;
        Ok(())
    }
    /// Where to resume: the cursor of the last [`Job::write`] or [`Job::remove`] of this run, or
    /// none at its start.
    pub fn cursor(&self) -> Option<&serde_json::Value> {
        self.cursor.as_ref()
    }
    /// `catalogues.dir/<name>`, which exists: the catalogue's downloads and temporary files go
    /// here, and nowhere else (`pinakesd catalogue drop` deletes it).
    pub fn dir(&self) -> &Path {
        self.dir
    }
    /// The state row as the run started: its last finished import and update, the upstream
    /// version.
    pub fn state(&self) -> &State {
        &self.state
    }
    /// Whether this is a full import asked for: by `--request` or an update's hand-over
    /// (`import_requested` as it started), or by hand ([`Importer::asked`]).
    pub fn asked(&self) -> bool {
        self.asked
    }
    /// Where the last finished run left the next update to start; none before the first.
    pub fn position(&self) -> Option<&serde_json::Value> {
        self.state.position.as_ref()
    }
    /// Where the next update starts: kept when this run finishes, in the transaction that ends
    /// it (a run cut off keeps the old position and resumes from its cursor).
    pub fn set_position(&mut self, position: serde_json::Value) {
        self.position = Some(position);
    }
    /// The upstream time of the data the run leaves (a dump's date, the time of the last change
    /// applied): kept as `upstream_at` when the run finishes, and what the update lag measures.
    pub fn set_upstream_at(&mut self, at: chrono::DateTime<chrono::Utc>) {
        self.upstream_at = Some(at);
    }
    pub fn config(&self) -> &Config {
        self.config
    }
    /// Say that this full import reads far fewer cards than the catalogue holds and that is
    /// expected (upstream removed them, or the operator asked for it): the runner then removes the
    /// cards it did not read even when they are more than [`SHRINK_FLOOR`] allows.
    pub fn expect_shrink(&mut self) {
        self.shrink_expected = true;
    }
    /// `limit` of Wikidata and MusicBrainz (`[catalogues.<name>] limit`).
    pub fn limit(&self) -> Option<u64> {
        self.config.catalogues.limit(self.catalogue)
    }
    /// Fails the run with [`Waits`] unless `catalogues.dir/<name>` has `need` bytes free (the
    /// runner's own check, for a need known only once the run has begun).
    pub fn need_disk(&self, need: u64) -> Result<()> {
        let free = (self.free_disk)(self.dir)
            .with_context(|| Failure::new("disk").at(self.dir.display().to_string()))?;
        if free < need {
            return Err(Waits(format!(
                "free disk: the {} needs {} under {}, {} free",
                self.run.name(),
                gigabytes(need),
                self.dir.display(),
                gigabytes(free)
            ))
            .into());
        }
        Ok(())
    }
    /// The shrink floor of a full import, before it removes the cards it did not read: past a
    /// first import, one that read under [`SHRINK_FLOOR`] of the cards held fails (`shrunk`) and
    /// removes nothing, unless the drop is expected ([`Job::expect_shrink`]), allowed
    /// (`pinakesd catalogue allow-shrink`) or the scope is narrower. The runner checks it before
    /// its sweep; an importer that removes cards itself checks it first.
    pub async fn check_shrink(&self) -> Result<()> {
        let (held, read, allowed): (i64, i64, bool) = sqlx::query_as(
            "SELECT cards,generation_cards,shrink_allowed FROM catalogue_state WHERE catalogue=$1",
        )
        .bind(self.catalogue.name())
        .fetch_one(self.pool)
        .await?;
        // A state row from before scopes were kept: the limit its position records.
        let old_scope = self.state.scope.clone().or_else(|| {
            self.state
                .position
                .as_ref()
                .and_then(|p| p.get("limit"))
                .map(|limit| serde_json::json!({ "limit": limit }))
        });
        let narrower = old_scope
            .as_ref()
            .is_some_and(|old| narrower(old, &self.config.catalogues.scope(self.catalogue)));
        let expected = self.shrink_expected || allowed || narrower;
        if self.state.last_import_at.is_some() && shrunk(held, read) {
            if !expected {
                tracing::warn!(
                catalogue = self.catalogue.name(),
                held,
                read,
                "catalogue full import read far fewer cards than the catalogue holds; nothing is removed (pinakesd catalogue allow-shrink lets the next try remove them)"
            );
                return Err(Failure::new("shrunk").into());
            }
            tracing::warn!(
            catalogue = self.catalogue.name(),
            held,
            read,
            importer = self.shrink_expected,
            operator = allowed,
            narrower,
            "catalogue full import removes the cards it did not read, more than half the catalogue"
        );
        }
        Ok(())
    }
    /// How free disk is read (the daemon's: the file system; tests set their own).
    pub fn free_disk(&self) -> FreeDisk {
        self.free_disk.clone()
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
    /// Write rows the importer already checked ([`store::upsert_rows`]; each `source_id` once,
    /// `rejected` the cards it refused) and save `cursor` with them.
    pub async fn write_rows(
        &mut self,
        rows: &[super::Row],
        rejected: u64,
        cursor: serde_json::Value,
    ) -> Result<WriteReport> {
        let written = store::upsert_rows(
            self.pool,
            self.catalogue,
            self.generation,
            rows,
            rejected,
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
    /// How often a held lock's session is checked while a run goes on, and how long one check
    /// may take (`locks::LOCK_CHECK`).
    pub lock_check: Duration,
}
impl Default for Pace {
    fn default() -> Self {
        Self {
            retry: Duration::from_secs(600),
            disk: Duration::from_secs(600),
            turn: Duration::from_secs(60),
            lock_check: crate::locks::LOCK_CHECK,
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
    /// Shared with the checks of [`SessionLock::lost`], one at a time.
    connection: Arc<tokio::sync::Mutex<PgConnection>>,
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
            Ok(Some(Self {
                connection: Arc::new(tokio::sync::Mutex::new(connection)),
                key,
            }))
        } else {
            let _ = connection.close().await;
            Ok(None)
        }
    }
    /// Ends once the lock is lost, as the instance lock's watch decides (`locks`): at once when
    /// its session has ended, and after two checks in a row with no answer within `every`; one
    /// slow check alone loses nothing. A check every `every`. Never ends otherwise.
    pub async fn lost(&mut self, every: Duration) {
        let connection = self.connection.clone();
        let check = move || {
            let connection = connection.clone();
            async move { check_session(&mut *connection.lock().await, every).await }
        };
        crate::locks::watch_checks(check, every).await
    }
    /// One check of the lock's session, given `wait` to answer: held, slow (no answer in time,
    /// or an error that leaves the session open) or gone (the session has ended, and the lock
    /// with it), as `locks::failed_check` tells them apart.
    pub async fn check(&mut self, wait: Duration) -> crate::locks::Check {
        check_session(&mut *self.connection.lock().await, wait).await
    }
    pub async fn release(self) {
        let mut connection = self.connection.lock().await;
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.key)
            .execute(&mut *connection)
            .await;
        drop(connection);
        // The last holder closes the connection; a check still holding a clone drops it later,
        // which closes it too.
        if let Ok(connection) = Arc::try_unwrap(self.connection) {
            let _ = connection.into_inner().close().await;
        }
    }
}
/// One check of a lock's session ([`SessionLock::check`]).
async fn check_session(connection: &mut PgConnection, wait: Duration) -> crate::locks::Check {
    use crate::locks::Check;
    let answer = sqlx::query("SELECT 1").execute(connection);
    match tokio::time::timeout(wait, answer).await {
        Ok(Ok(_)) => Check::Held,
        Ok(Err(error)) => crate::locks::failed_check(&error),
        Err(_) => Check::Slow,
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

/// The catalogues whose importer needs `http.contact`, with whether `config` leaves one that is
/// on without it and what `waiting` says then.
pub fn contact_needs(config: &Config) -> [(Catalogue, bool, &'static str); 2] {
    [
        (
            Catalogue::Musicbrainz,
            super::musicbrainz::needs_contact(config),
            super::musicbrainz::NEEDS_CONTACT,
        ),
        (
            Catalogue::Osm,
            super::osm::needs_contact(config),
            super::osm::NEEDS_CONTACT,
        ),
    ]
}
/// Say in the state rows why a catalogue that is on has no importer task: MusicBrainz or
/// OpenStreetMap without `http.contact` (their `NEEDS_CONTACT` in `waiting`). Started with its
/// contact, or with the catalogue off, the text goes (a task that starts clears any at its first
/// step).
pub async fn note_unstarted(pool: &PgPool, config: &Config) -> Result<()> {
    for (catalogue, needs, note) in contact_needs(config) {
        if needs {
            set_waiting(pool, catalogue, note).await?;
        } else {
            sqlx::query("UPDATE catalogue_state SET waiting=NULL,updated_at=now() WHERE catalogue=$1 AND waiting=$2")
                .bind(catalogue.name())
                .bind(note)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}
/// The importers `pinakesd` starts: those of the catalogues that are on. With every catalogue
/// off, none.
pub fn to_start(importers: Vec<Arc<dyn Importer>>, config: &Config) -> Vec<Arc<dyn Importer>> {
    importers
        .into_iter()
        .filter(|i| config.catalogues.enabled(i.catalogue()))
        .collect()
}

/// Run `importer` until `shutdown`: take its catalogue's lock (waiting while another process
/// holds it), then step ([`step`]) and wait as each step says. The lock is held from step to
/// step and watched while a step runs: a step whose catalogue lock is lost is cut off and only
/// logged, with nothing written to the state row (another daemon may hold the lock by then); the
/// task takes the lock again and resumes. A step cut off (a shutdown, a lost lock) leaves its run
/// and cursor in the state row, and the next step resumes it.
pub async fn run(importer: Arc<dyn Importer>, ctx: Context, mut shutdown: watch::Receiver<bool>) {
    let catalogue = importer.catalogue();
    let mut lock: Option<SessionLock> = None;
    // Whether the last try found the lock held elsewhere: logged once, when that starts.
    let mut busy = false;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let stepped = async {
            // A slow check keeps the lock: only a session that has ended lost it (the watch
            // during the step decides on slow ones).
            let gone = match lock.as_mut() {
                Some(l) => l.check(ctx.pace.lock_check).await == crate::locks::Check::Gone,
                None => true,
            };
            if gone {
                lock = SessionLock::try_take(&ctx.pool, catalogue.lock_key()).await?;
            }
            let Some(held) = lock.as_mut() else {
                // The state row is the importing process's: only the log says this one waits.
                if !busy {
                    tracing::info!(
                        catalogue = catalogue.name(),
                        "catalogue import waits: another process imports this catalogue (its lock is held)"
                    );
                }
                busy = true;
                return Ok(Next::Wait(ctx.pace.turn));
            };
            busy = false;
            let stepped = std::panic::AssertUnwindSafe(step(&*importer, &ctx)).catch_unwind();
            tokio::select! {
                stepped = stepped => stepped.unwrap_or_else(|_| Err(Failure::new("panic").into())),
                _ = held.lost(ctx.pace.lock_check) => Err(CatalogueLockLost.into()),
            }
        };
        let next = tokio::select! {
            _ = shutdown.changed() => return,
            next = stepped => next,
        };
        let wait = match next {
            Ok(Next::Again) => continue,
            Ok(Next::Wait(wait)) => wait,
            // The state row may already be another daemon's, which took the lock: only the log
            // says so. The run resumes from its cursor once this task holds the lock again.
            Err(failed) if failed.is::<CatalogueLockLost>() => {
                tracing::warn!(
                    catalogue = catalogue.name(),
                    "catalogue lock lost; the run resumes from its cursor once the lock is taken again"
                );
                lock = None;
                ctx.pace.turn
            }
            // A run that failed while this task's catalogue lock went too (a database restart
            // ends both sessions): another daemon may hold the lock and the state row by now, so
            // it is a lost lock, logged only.
            Err(_) if catalogue_lock_gone(&mut lock, ctx.pace.lock_check).await => {
                tracing::warn!(
                    catalogue = catalogue.name(),
                    "catalogue lock lost; the run resumes from its cursor once the lock is taken again"
                );
                lock = None;
                ctx.pace.turn
            }
            Err(failed) => {
                let shown = failure::shown(&failed);
                if failed.is::<Abandoned>() {
                    tracing::warn!(catalogue = catalogue.name(), error = %shown, "catalogue run abandoned: what it read can't be applied; the importer waits for something new");
                } else {
                    tracing::warn!(catalogue = catalogue.name(), error = %shown, "catalogue import failed; it resumes from its cursor");
                }
                if let Err(unrecorded) = record_failure(&ctx.pool, catalogue, &shown).await {
                    tracing::warn!(catalogue = catalogue.name(), error = %failure::shown(&unrecorded), "catalogue failure not recorded");
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

/// Whether the catalogue lock this task held has gone (its session ended).
async fn catalogue_lock_gone(lock: &mut Option<SessionLock>, wait: Duration) -> bool {
    match lock.as_mut() {
        Some(l) => l.check(wait).await == crate::locks::Check::Gone,
        None => true,
    }
}

/// The large catalogue, other than `catalogue`, whose full import is under way or was cut off
/// (a failure, a restart) and is not switched off: it keeps the large-import turn until it
/// finishes, since its temporary files stay on the disk meanwhile. A catalogue that resumes a
/// cut-off import of its own waits only for those before it in `Catalogue::ALL`: when two were
/// cut off (one turned off and on again while the other ran), the first goes, then the other,
/// and neither waits for the other forever.
async fn large_import_under_way(
    pool: &PgPool,
    catalogue: Catalogue,
    resuming: bool,
) -> Result<Option<String>> {
    let large: Vec<&str> = Catalogue::ALL
        .into_iter()
        .filter(|c| c.large() && *c != catalogue && (!resuming || *c < catalogue))
        .map(Catalogue::name)
        .collect();
    Ok(sqlx::query_scalar(
        "SELECT catalogue FROM catalogue_state WHERE catalogue=ANY($1) AND run='import' AND phase<>'disabled' ORDER BY catalogue LIMIT 1",
    )
    .bind(&large)
    .fetch_optional(pool)
    .await?)
}

/// One step of an importer's task, which holds the catalogue's lock: resume the run under way or
/// plan one, wait for the large-import turn and the free disk it needs, run it to its end and
/// record it. A failure leaves the run and its cursor for the next step.
pub async fn step(importer: &dyn Importer, ctx: &Context) -> Result<Next> {
    let catalogue = importer.catalogue();
    let pool = &ctx.pool;
    let state = store::ensure_state(pool, catalogue).await?;
    let resumed = state.run.as_deref().and_then(Run::parse);
    // A full import an update asked for starts as any new full import does (the large-import
    // turn, the disk), not as a resumed one; meanwhile nothing else is planned.
    let run = match resumed {
        Some(run) => run,
        None if state.import_requested.is_some() => Run::Import,
        None => match importer.plan(&state).await? {
            Plan::Run(run) => run,
            Plan::Wait(wait) => {
                clear_waiting(pool, catalogue).await?;
                return Ok(Next::Wait(wait));
            }
        },
    };
    // One large full import at a time: the lock while one runs, and the state rows for one that
    // was cut off and has not finished.
    let mut large = None;
    if run == Run::Import && catalogue.large() {
        let Some(lock) = SessionLock::try_take(pool, crate::locks::CATALOGUE_LARGE_IMPORT).await?
        else {
            let why = format!("another large catalogue's full import runs; {TURN}");
            set_waiting(pool, catalogue, &why).await?;
            return Ok(Next::Wait(ctx.pace.turn));
        };
        if let Some(other) = large_import_under_way(pool, catalogue, resumed.is_some()).await? {
            lock.release().await;
            let why = format!("{other}'s full import is under way and has not finished; {TURN}");
            set_waiting(pool, catalogue, &why).await?;
            return Ok(Next::Wait(ctx.pace.turn));
        }
        large = Some(lock);
    }
    let dir = ctx.config.catalogues.work_dir(catalogue);
    let disk = || Failure::new("disk").at(dir.display().to_string());
    tokio::fs::create_dir_all(&dir).await.with_context(disk)?;
    let need = importer.disk_need(run, &state);
    let free = (ctx.free_disk)(&dir).with_context(disk)?;
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
        (state.generation, state.cursor.clone())
    } else {
        let generation: i64 = sqlx::query_scalar("UPDATE catalogue_state SET run=$2,phase=$2,cursor=NULL,waiting=NULL,generation=generation+($2='import')::int,generation_cards=CASE WHEN $2='import' THEN 0 ELSE generation_cards END,updated_at=now() WHERE catalogue=$1 RETURNING generation")
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
    let asked = run == Run::Import && (importer.asked() || state.import_requested.is_some());
    let mut job = Job {
        pool,
        catalogue,
        run,
        generation,
        cursor,
        position: None,
        upstream_at: None,
        asked,
        state,
        shrink_expected: false,
        dir: &dir,
        config: &ctx.config,
        report: WriteReport::default(),
        free_disk: ctx.free_disk.clone(),
    };
    // The large-import lock is watched while the run goes on: lost while this task still holds
    // the catalogue lock, the run stops as a recorded failure (`turn_lost`) and resumes from its
    // cursor once the turn is its own again.
    let ran = async {
        match run {
            Run::Import => importer.import(&mut job).await.map(|()| UpdateEnd::Done),
            Run::Update => importer.update(&mut job).await,
        }
    };
    let every = ctx.pace.lock_check;
    let watched = async {
        match large.as_mut() {
            Some(lock) => lock.lost(every).await,
            None => std::future::pending().await,
        }
    };
    let ended = tokio::select! {
        ran = ran => ran,
        _ = watched => return Err(Failure::new("turn_lost").into()),
    };
    let ended = match ended {
        Ok(ended) => ended,
        Err(error) if error.is::<Abandoned>() => {
            // A full import abandoned answers its request (the dump it read can't be applied).
            sqlx::query("UPDATE catalogue_state SET run=NULL,cursor=NULL,waiting=NULL,position=coalesce($2,position),import_requested=CASE WHEN $3='import' THEN NULL ELSE import_requested END,updated_at=now() WHERE catalogue=$1")
                .bind(catalogue.name())
                .bind(job.position.take())
                .bind(run.name())
                .execute(pool)
                .await?;
            if let Some(lock) = large {
                lock.release().await;
            }
            tracing::info!(
                catalogue = catalogue.name(),
                run = run.name(),
                "catalogue run abandoned: what it read can't be applied"
            );
            return Err(error);
        }
        Err(error) => match error.downcast::<Waits>() {
            Ok(Waits(why)) => {
                set_waiting(pool, catalogue, &why).await?;
                if let Some(lock) = large {
                    lock.release().await;
                }
                return Ok(Next::Wait(ctx.pace.disk));
            }
            Err(error) => return Err(error),
        },
    };
    let mut report = job.report;
    let position = job.position.take();
    // The run's own generation (a full import that started over has a new one).
    let generation = job.generation;
    if let UpdateEnd::NeedsImport(reason) = ended {
        // The update ends without marking anything current, and a full import is asked for: the
        // next step starts it as a new import, through the large-import turn and the disk check.
        sqlx::query("UPDATE catalogue_state SET run=NULL,cursor=NULL,phase='idle',waiting=NULL,import_requested=$2,updated_at=now() WHERE catalogue=$1")
            .bind(catalogue.name())
            .bind(reason)
            .execute(pool)
            .await?;
        tracing::info!(
            catalogue = catalogue.name(),
            reason,
            "catalogue update needs a full import"
        );
        return Ok(Next::Again);
    }
    let scope = ctx.config.catalogues.scope(catalogue);
    if run == Run::Import {
        // A full import that read far fewer cards than the catalogue holds (a dump cut at a clean
        // boundary, an empty answer, a filter gone wrong) removes nothing: it fails (`shrunk`) with
        // its cursor kept, and is tried again; it goes through once the importer says the drop is
        // expected, the operator allows it (`pinakesd catalogue allow-shrink`, or `catalogue import
        // --force`), or the import's scope is narrower than the last finished import's (a lower
        // `limit`, a higher `min_citations`). No scope recorded: the limit the position records.
        job.check_shrink().await?;
        report.removed += store::finish_import(pool, catalogue, generation).await?;
    }
    sqlx::query(
        "UPDATE catalogue_state SET run=NULL,cursor=NULL,phase='idle',waiting=NULL,position=coalesce($3,position),\
         last_import_at=CASE WHEN $2='import' THEN now() ELSE last_import_at END,\
         import_requested=CASE WHEN $2='import' THEN NULL ELSE import_requested END,\
         last_update_at=CASE WHEN $2='update' THEN now() ELSE last_update_at END,\
         upstream_at=CASE WHEN $2='import' THEN coalesce($4,now()) ELSE coalesce($4,upstream_at) END,\
         scope=CASE WHEN $2='import' THEN $5 ELSE scope END,shrink_allowed=CASE WHEN $2='import' THEN false ELSE shrink_allowed END,\
         updated_at=now() WHERE catalogue=$1",
    )
    .bind(catalogue.name())
    .bind(run.name())
    .bind(position)
    .bind(job.upstream_at)
    .bind(&scope)
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
/// An importer whose plan is always `run` (`pinakesd catalogue import`); it keeps what its runs
/// wrote.
struct Forced {
    inner: Arc<dyn Importer>,
    run: Run,
    /// The operator expects the drop (`--force`): [`Job::expect_shrink`].
    expect_shrink: bool,
    written: std::sync::Mutex<WriteReport>,
}
#[async_trait]
impl Importer for Forced {
    fn catalogue(&self) -> Catalogue {
        self.inner.catalogue()
    }
    fn disk_need(&self, run: Run, state: &State) -> u64 {
        self.inner.disk_need(run, state)
    }
    async fn plan(&self, _state: &State) -> Result<Plan> {
        Ok(Plan::Run(self.run))
    }
    fn asked(&self) -> bool {
        self.run == Run::Import
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        if self.expect_shrink {
            job.expect_shrink();
        }
        let ran = self.inner.import(job).await;
        *self.written.lock().expect("the report") += job.report();
        ran
    }
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        let ran = self.inner.update(job).await;
        *self.written.lock().expect("the report") += job.report();
        ran
    }
}

/// One run of `importer` now, in this process (`pinakesd catalogue import`): a full import
/// ([`Run::Import`], which takes up a full import that was cut off, else begins a new one), or an
/// update ([`Run::Update`], after a finished import). It takes the catalogue's lock (refused
/// while another process holds it), waits for the large-import turn and the disk as the task
/// does (`waiting` says why, at each change), and runs through [`step`], so the daemon's code and
/// state rows are the same. A failure is recorded as the task records it, and returned. The state
/// row when the run has finished, and what it wrote (the cards a full import removed at its end
/// not counted).
pub async fn run_once(
    importer: Arc<dyn Importer>,
    ctx: &Context,
    run: Run,
    waiting: impl FnMut(&str),
) -> Result<(State, WriteReport)> {
    run_once_with(importer, ctx, run, false, waiting).await
}
/// [`run_once`]; with `expect_shrink`, a full import that reads far fewer cards than the catalogue
/// holds removes the rest ([`Job::expect_shrink`]: `catalogue import --force`).
pub async fn run_once_with(
    importer: Arc<dyn Importer>,
    ctx: &Context,
    run: Run,
    expect_shrink: bool,
    mut waiting: impl FnMut(&str),
) -> Result<(State, WriteReport)> {
    let catalogue = importer.catalogue();
    let Some(mut lock) = SessionLock::try_take(&ctx.pool, catalogue.lock_key()).await? else {
        anyhow::bail!("another process imports catalogue {catalogue} (its lock is held): stop that pinakesd or switch the catalogue off there first, or ask it for a full import with --request");
    };
    let held = &mut lock;
    let ran = async {
        let state = store::ensure_state(&ctx.pool, catalogue).await?;
        match run {
            // A new full import (an update under way is dropped); asked for through the run
            // itself ([`Importer::asked`]), never as a request left in the state row.
            Run::Import if state.run.as_deref() != Some("import") => {
                sqlx::query("UPDATE catalogue_state SET run=NULL,cursor=NULL,updated_at=now() WHERE catalogue=$1")
                    .bind(catalogue.name())
                    .execute(&ctx.pool)
                    .await?;
            }
            Run::Import => {}
            Run::Update => {
                anyhow::ensure!(
                    state.last_import_at.is_some() && state.run.as_deref() != Some("import"),
                    "catalogue {catalogue} has no finished full import to update: run the import first"
                );
            }
        }
        let forced = Forced {
            inner: importer.clone(),
            run,
            expect_shrink,
            written: Default::default(),
        };
        let mut said: Option<String> = None;
        loop {
            // As the task does: a panic is a failed run, and the lock watched while it goes on.
            let stepped = std::panic::AssertUnwindSafe(step(&forced, ctx)).catch_unwind();
            let stepped = tokio::select! {
                stepped = stepped => stepped.unwrap_or_else(|_| Err(Failure::new("panic").into())),
                _ = held.lost(ctx.pace.lock_check) => {
                    anyhow::bail!("the lock of catalogue {catalogue} was lost (its database session ended); the run resumes from its cursor at the next import");
                }
            };
            match stepped {
                Ok(Next::Again) => {
                    let state = store::ensure_state(&ctx.pool, catalogue).await?;
                    if state.run.is_none() && state.import_requested.is_none() {
                        let written = *forced.written.lock().expect("the report");
                        return Ok((state, written));
                    }
                }
                Ok(Next::Wait(wait)) => {
                    let state = store::ensure_state(&ctx.pool, catalogue).await?;
                    let why = state.waiting.clone().unwrap_or_default();
                    if said.as_deref() != Some(why.as_str()) {
                        waiting(&why);
                        said = Some(why);
                    }
                    tokio::time::sleep(wait.min(Duration::from_secs(60))).await;
                }
                Err(failed) => {
                    let shown = failure::shown(&failed);
                    record_failure(&ctx.pool, catalogue, &shown).await?;
                    anyhow::bail!("catalogue {catalogue} run failed: {shown}");
                }
            }
        }
    };
    let ended = ran.await;
    lock.release().await;
    ended
}

/// Ask for a full import of `catalogue` (`pinakesd catalogue import --request`): the importer
/// that holds the catalogue (a daemon's task) starts it as any full import asked for, after the
/// run under way, or at its next step. Needs no lock.
pub async fn request_import(pool: &PgPool, catalogue: Catalogue, reason: &str) -> Result<()> {
    let mut reason = reason.to_owned();
    crate::model::truncate(&mut reason, 1024);
    store::ensure_state(pool, catalogue).await?;
    sqlx::query(
        "UPDATE catalogue_state SET import_requested=$2,updated_at=now() WHERE catalogue=$1",
    )
    .bind(catalogue.name())
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// The share of a catalogue's cards a full import must read for the runner to remove the rest
/// without being told the drop is expected ([`Job::expect_shrink`]): half.
pub const SHRINK_FLOOR: f64 = 0.5;
/// Whether a full import that read `read` of the `held` cards reads too few to remove the rest.
fn shrunk(held: i64, read: i64) -> bool {
    (read as f64) < SHRINK_FLOOR * held as f64
}
/// Whether the scope `new` holds fewer items than `old` ([`CataloguesConfig::scope`](super::CataloguesConfig::scope)):
/// a `limit` set or lowered, a higher `min_citations`. Another `region` is not: it may be larger,
/// and a cut-off file would then empty the catalogue (the operator allows it, `allow-shrink`).
fn narrower(old: &serde_json::Value, new: &serde_json::Value) -> bool {
    let limit = |v: &serde_json::Value| v.get("limit").map(|l| l.as_u64());
    let narrower_limit = match (limit(old), limit(new)) {
        (Some(None), Some(Some(_))) => true,
        (Some(Some(was)), Some(Some(now))) => now < was,
        _ => false,
    };
    let citations = |v: &serde_json::Value| v.get("min_citations").and_then(|c| c.as_u64());
    let narrower_citations =
        matches!((citations(old), citations(new)), (Some(was), Some(now)) if now > was);
    narrower_limit || narrower_citations
}
/// The catalogue's lock was lost while a run went on (its session ended).
#[derive(Debug)]
struct CatalogueLockLost;
impl std::fmt::Display for CatalogueLockLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lock_lost")
    }
}
impl std::error::Error for CatalogueLockLost {}
/// The words every wait for the large-import turn ends with;
/// `pinakes_catalogue_waiting_for_turn` counts the state rows whose `waiting` holds them.
pub const TURN: &str = "one large import runs at a time";
fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}
/// Say why the importer waits, logged once when the reason changes.
async fn set_waiting(pool: &PgPool, catalogue: Catalogue, why: &str) -> Result<()> {
    let mut why = why.to_owned();
    crate::model::truncate(&mut why, 1024);
    let why = why.as_str();
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
/// and its cursor stay; an operator's allowance to shrink is taken back (the next `shrunk` failure
/// is checked again).
pub async fn record_failure(pool: &PgPool, catalogue: Catalogue, error: &str) -> Result<()> {
    let mut error = error.to_owned();
    crate::model::truncate(&mut error, 1024);
    store::ensure_state(pool, catalogue).await?;
    sqlx::query("UPDATE catalogue_state SET phase='failed',waiting=NULL,last_error=$2,error_count=error_count+1,shrink_allowed=false,updated_at=now() WHERE catalogue=$1")
        .bind(catalogue.name())
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A lower `limit` or a higher `min_citations` is a narrower scope; another `region` is not
    /// (a larger one would let a cut-off file empty the catalogue): the floor holds for it.
    #[test]
    fn only_a_lower_limit_or_more_citations_is_narrower() {
        assert!(narrower(&json!({"limit": null}), &json!({"limit": 10})));
        assert!(narrower(&json!({"limit": 20}), &json!({"limit": 10})));
        assert!(!narrower(&json!({"limit": 10}), &json!({"limit": 20})));
        assert!(narrower(
            &json!({"min_citations": 10}),
            &json!({"min_citations": 50})
        ));
        assert!(!narrower(
            &json!({"region": "europe/switzerland"}),
            &json!({"region": "planet"})
        ));
        assert!(!narrower(
            &json!({"region": "planet"}),
            &json!({"region": "europe/switzerland"})
        ));
    }
}
