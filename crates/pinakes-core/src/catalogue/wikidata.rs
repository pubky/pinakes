//! The Wikidata importer: items with a Wikipedia article, from the weekly JSON dump, kept current
//! from the recent changes.
//!
//! **First import** ([`Wikidata::import`]): the newest dated dump
//! (`dumps.wikimedia.org/wikidatawiki/entities/<date>/wikidata-<date>-all.json.bz2`, about
//! 104 GB) is downloaded to `catalogues.dir/wikidata` on two connections (each half by ranges, so
//! a cut-off download resumes), checked against the dump's SHA-1 when its checksum file lists it,
//! then read from disk on several threads ([`pipeline`]): blocks decoded in parallel
//! ([`bz2`]), lines parsed in parallel ([`entity`]), cards written in dump order. With
//! `[catalogues.wikidata] limit` the file is read as a stream over HTTP instead, and only up to
//! the limit. The read resumes from its last written batch ([`pipeline::Position`]). The
//! downloaded file is deleted when the import finishes. A dump that fails its checksum, changed
//! or went upstream, reads as broken, or whose work files were lost, starts the import over
//! from the newest dump; one that fails so [`RESTARTS`] times is refused (`run::REFUSED`), as
//! is one that makes no card (the runner's `empty`), and the import waits for the next dump.
//!
//! **Labels:** a card's labels are the English labels of its genres and classes. The dump holds
//! those items too, but often after the cards that name them, so their labels are collected as
//! the read goes (`labels.tsv`), the cards whose label items were not read yet are listed
//! (`pending.tsv`), and at the end the labels still unknown are asked of the API (at most
//! [`LABEL_LOOKUPS`] items) and those cards are written again with all their labels.
//!
//! **Updates** ([`Wikidata::update`], every [`UPDATE_EVERY`], at once while behind): the recent
//! changes of items after the stored position (a time; after an import, the dump's date), at most
//! [`UPDATE_WINDOW`] of them per run, then the changed items, 50 per `wbgetentities` request, one
//! request at a time. An item deleted, merged into another, without a Wikipedia article now, or
//! become a Wikimedia page leaves the catalogue. A position an update left more than
//! [`LONGEST_GAP`] ago (the daemon was down) is not caught up change by change: the next weekly
//! dump is imported instead, once one newer than the imported one is out.
use super::failure::Failure;
use super::run::{Importer, Job, Plan, Run, UpdateEnd};
use super::store::State;
use super::{Card, Catalogue};
use crate::config::Config;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod bz2;
pub mod entity;
pub mod net;
pub mod pipeline;

use entity::Labels;
use pipeline::{Counts, Out, Position};

/// How often the recent changes are read when the cards are current.
pub const UPDATE_EVERY: Duration = Duration::from_secs(15 * 60);
/// The most time of changes one update reads, so a catch-up moves `current_as_of` run by run.
pub const UPDATE_WINDOW: chrono::Duration = chrono::Duration::hours(6);
/// The longest gap an update's position may have; longer waits for the next weekly dump.
pub const LONGEST_GAP: chrono::Duration = chrono::Duration::days(5);
/// How far back the recent changes reach (Wikidata keeps 30 days): a position an import left
/// older than this asks for a newer dump.
pub const RECENT_CHANGES_REACH: chrono::Duration = chrono::Duration::days(25);
/// How often one dump is read again from the start after a failure that needs it.
pub const RESTARTS: u32 = 3;
/// The most items whose labels the end of an import asks the API for.
pub const LABEL_LOOKUPS: usize = 50_000;
/// The most labels of classes and genres kept (an entry is a few dozen bytes).
pub const LABELS_CAP: usize = 16_000_000;
/// The most cards listed as waiting for labels.
pub const PENDING_CAP: u64 = 10_000_000;
/// Items per `wbgetentities` request (the API's limit for a client that is not a bot).
pub const ENTITIES_PER_REQUEST: usize = 50;
/// Free disk a full import needs beyond the dump: the label files and room to spare.
const SPARE: u64 = 4_000_000_000;
/// The dump's size before its HEAD says it (the 2026-11-23 dump: 104.1 GB, growing).
const DUMP_SIZE: u64 = 125_000_000_000;
/// A dump the HEAD says is larger than this is refused.
const DUMP_SIZE_MAX: u64 = 4 * DUMP_SIZE;
/// What an update or an import read as a stream needs: the label files.
const SMALL_NEED: u64 = 1_000_000_000;
/// Progress is logged at most this often.
const PROGRESS_EVERY: Duration = Duration::from_secs(60);

/// Where the importer reads.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// The dumps directory (`net::DUMPS`).
    pub dumps: String,
    /// The API (`net::API`).
    pub api: String,
}
impl Default for Endpoints {
    fn default() -> Self {
        Self {
            dumps: net::DUMPS.into(),
            api: net::API.into(),
        }
    }
}

/// What `pinakesd catalogue import wikidata --from` names instead of the newest dump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum From {
    /// A local `.json.bz2` file (read from disk, as is), by its absolute path.
    File(PathBuf),
    /// A `.json.bz2` file on the web (`https://`), read as a stream.
    Url(String),
}
impl From {
    /// What `--from` names: an `https://` URL without credentials or a query, else a local
    /// file.
    pub fn parse(text: &str) -> Result<Self> {
        if text.starts_with("http://") {
            anyhow::bail!("--from takes an https:// URL (or a local file)");
        }
        if text.starts_with("https://") {
            let url = url::Url::parse(text).context("--from is not a URL")?;
            // The URL is stored in the import's cursor until it ends: no credential in it, nor
            // a query (a signed link's token).
            anyhow::ensure!(
                url.username().is_empty() && url.password().is_none() && url.query().is_none(),
                "--from takes a URL without credentials or a query"
            );
            return Ok(From::Url(url.to_string()));
        }
        let path = std::path::absolute(text).context("--from names no file")?;
        anyhow::ensure!(path.is_file(), "--from names no file: {}", path.display());
        Ok(From::File(path))
    }
}

/// The Wikidata importer.
pub struct Wikidata {
    pub endpoints: Endpoints,
    pub from: Option<From>,
    /// `[catalogues.wikidata] limit` (or `catalogue import --limit`): a limited import reads
    /// the dump as a stream.
    pub limit: Option<u64>,
    /// The read's threads (decoders, parsers): `pipeline::threads` by default.
    pub threads: (usize, usize),
    /// Plain HTTP allowed (the tests' local server); HTTPS only otherwise.
    pub http: bool,
    /// Bytes one round of the download reads per connection before the cursor is saved.
    pub download_round: u64,
    /// Cards written per cursor.
    pub write_cards: usize,
    /// The largest `wbgetentities` answer read (`net::API_CAP`).
    pub api_cap: usize,
    /// How long a dump read may wait for 8 MB before it fails `download` (`net::SLOW_WINDOW`).
    pub slow_window: std::time::Duration,
    /// A limit given on the command line (`catalogue import --limit`): kept for the updates and
    /// the imports they ask for. A configured limit is not kept: taking it out of the
    /// configuration lifts it.
    pub cli_limit: Option<u64>,
    /// The labels the last update ended with, and the length of `labels.tsv` then.
    labels: tokio::sync::Mutex<Option<(u64, Labels)>>,
}
impl Wikidata {
    pub fn new() -> Self {
        Self {
            endpoints: Endpoints::default(),
            from: None,
            limit: None,
            threads: pipeline::threads(),
            http: false,
            download_round: 512 << 20,
            write_cards: super::store::BATCH_ROWS,
            api_cap: net::API_CAP,
            slow_window: net::SLOW_WINDOW,
            cli_limit: None,
            labels: tokio::sync::Mutex::new(None),
        }
    }
    /// The daemon's importer for `config`.
    pub fn for_config(config: &Config) -> Self {
        Self {
            limit: config.catalogues.limit(Catalogue::Wikidata),
            ..Self::new()
        }
    }
}
impl Default for Wikidata {
    fn default() -> Self {
        Self::new()
    }
}

/// The dump an import reads, and how.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum Source {
    /// Downloaded to the work directory first.
    Download { dump: net::Dump },
    /// Read as a stream over HTTP.
    Stream {
        dump: net::Dump,
        /// When the changes are read from after it (its date, else a week before its
        /// `Last-Modified`).
        since: chrono::DateTime<chrono::Utc>,
    },
    /// A local file.
    File {
        path: PathBuf,
        since: chrono::DateTime<chrono::Utc>,
    },
}
impl Source {
    fn dump(&self) -> Option<&net::Dump> {
        match self {
            Source::Download { dump } | Source::Stream { dump, .. } => Some(dump),
            Source::File { .. } => None,
        }
    }
    /// What a refusal of what it reads names (`run::REFUSED`): the dump's date, or the local
    /// file's path.
    fn version(&self) -> serde_json::Value {
        match self {
            Source::Download { dump } | Source::Stream { dump, .. } => dump.date.clone().into(),
            Source::File { path, .. } => path.display().to_string().into(),
        }
    }
    fn since(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        match self {
            Source::Download { dump } => dump.time(),
            Source::Stream { since, .. } | Source::File { since, .. } => Some(*since),
        }
    }
}
/// The step an import is at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "lowercase")]
enum Step {
    /// Each connection's next byte.
    Download {
        parts: [u64; 2],
    },
    Verify,
    Read,
    /// The cards waiting for labels written again: up to byte `done` of `pending.tsv`, after
    /// the labels still unknown were asked (`asked`).
    Labels {
        done: u64,
        asked: bool,
    },
    /// Every card written; what is left is the end of the import (the catalogue's shrink floor,
    /// the sweep). A try that resumes here only ends it again.
    Done,
}
/// Where an import resumes (`catalogue_state.cursor`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ImportCursor {
    source: Source,
    step: Step,
    /// Where the read resumes; none before the first batch.
    at: Option<Position>,
    /// Cards kept so far (the limit counts these).
    kept: u64,
    counts: Counts,
    /// The label files' lengths, in step with the cards written.
    labels_len: u64,
    pending_len: u64,
    /// The read stopped at the limit.
    limited: bool,
    /// The limit the import started with (`catalogue import --limit` too), kept when it resumes.
    #[serde(default)]
    limit: Option<u64>,
    /// The command line's limit this import keeps for the updates after it.
    #[serde(default)]
    sticky: Option<u64>,
    /// Labels and waiting cards left out at their caps.
    #[serde(default)]
    labels_dropped: u64,
    #[serde(default)]
    pending_dropped: u64,
    /// The restarts of the dump before this attempt (after a restart).
    #[serde(default)]
    restarts: Option<Restarted>,
}
/// The cursor an import that has to start over leaves: the next run reads the newest dump from
/// its start, and gives up on a dump that failed [`RESTARTS`] times.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Restart {
    restart: Restarted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Restarted {
    date: String,
    count: u32,
    after: String,
    /// The command line's limit the import that starts over kept.
    #[serde(default)]
    limit: Option<u64>,
}
/// Where an update resumes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct UpdateCursor {
    /// The changes read are those up to this time.
    end: chrono::DateTime<chrono::Utc>,
    /// Where the next page of changes starts.
    next: Option<String>,
}
/// Where the next update starts (`catalogue_state.position`): the changes after this time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct UpdatePosition {
    time: chrono::DateTime<chrono::Utc>,
    /// Left by an import (its dump's date), not by an update.
    #[serde(default)]
    import: bool,
    /// The limit the import ran with (`catalogue import --limit`), which the updates keep.
    #[serde(default)]
    limit: Option<u64>,
}
/// A time at whole seconds (as the API and the position print it).
fn seconds(at: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(at.timestamp(), 0).unwrap_or(at)
}

/// Whether the catalogue is on but its importer does not start for want of `http.contact`.
pub fn needs_contact(config: &Config) -> bool {
    config.catalogues.enabled(Catalogue::Wikidata) && config.egress.contact.is_empty()
}
/// What `catalogue status` shows as the catalogue's `waiting` while [`needs_contact`].
pub const NEEDS_CONTACT: &str =
    "needs http.contact: the importer asks Wikimedia with the operator's contact";
/// The importer `pinakesd` runs; none without the operator's contact (`http.contact`), which
/// every request to Wikimedia names: the state row says why ([`NEEDS_CONTACT`]).
pub fn importer(config: &Config) -> Option<std::sync::Arc<dyn Importer>> {
    if config.egress.contact.is_empty() {
        if config.catalogues.enabled(Catalogue::Wikidata) {
            tracing::warn!(
                catalogue = "wikidata",
                "the wikidata importer starts only with http.contact set (Wikimedia is asked with the operator's contact)"
            );
        }
        return None;
    }
    Some(std::sync::Arc::new(Wikidata::for_config(config)))
}

fn contact(job: &Job<'_>) -> Result<String> {
    let contact = job.config().http.contact.trim().to_owned();
    if contact.is_empty() {
        // Wikimedia asks every client for a contact; without one nothing is asked.
        return Err(Failure::new("no_contact").into());
    }
    Ok(contact)
}
fn disk(path: &Path) -> Failure {
    Failure::new("disk").at(path.display().to_string())
}
/// The class of a run's failure, when it is a [`Failure`].
fn class(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<Failure>()
        .or_else(|| error.chain().find_map(|c| c.downcast_ref::<Failure>()))
        .map(|f| f.class)
}

/// The label files in the work directory: `labels.tsv` (`<item number>\t<label>` lines, an
/// empty label for an item that has none; a later line wins) and `pending.tsv`
/// (`<card's Q-id>\t<item numbers>`).
struct LabelFiles {
    labels: PathBuf,
    pending: PathBuf,
}
impl LabelFiles {
    fn new(dir: &Path) -> Self {
        Self {
            labels: dir.join("labels.tsv"),
            pending: dir.join("pending.tsv"),
        }
    }
    /// Cut a file after its last line break (a line left without one by a hard stop), if the
    /// file is there.
    fn end_on_a_line(path: &Path) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(disk(path).into()),
        };
        let len = file.metadata().with_context(|| disk(path))?.len();
        // Back from the end, a block at a time, to the last line break.
        let mut end = len;
        let mut block = vec![0u8; 64 << 10];
        while end > 0 {
            let start = end.saturating_sub(block.len() as u64);
            let part = &mut block[..(end - start) as usize];
            file.seek(SeekFrom::Start(start))
                .with_context(|| disk(path))?;
            file.read_exact(part).with_context(|| disk(path))?;
            if let Some(at) = part.iter().rposition(|b| *b == b'\n') {
                end = start + at as u64 + 1;
                break;
            }
            end = start;
        }
        if end < len {
            file.set_len(end).with_context(|| disk(path))?;
            file.sync_data().with_context(|| disk(path))?;
        }
        Ok(())
    }
    /// Cut a file to `len` (making it when missing), so it matches the cursor. A file shorter
    /// than the cursor says was lost: the import starts over (`label_files`).
    fn cut(path: &Path, len: u64) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| disk(path))?;
        if file.metadata().with_context(|| disk(path))?.len() < len {
            return Err(Failure::new("label_files")
                .at(path.display().to_string())
                .into());
        }
        file.set_len(len).with_context(|| disk(path))?;
        Ok(())
    }
    fn load_labels(&self) -> Result<Labels> {
        let mut labels = Labels::new();
        let file = match open_work(&self.labels) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(labels),
            Err(_) => return Err(disk(&self.labels).into()),
        };
        for line in std::io::BufReader::new(file).split(b'\n') {
            let line = line.with_context(|| disk(&self.labels))?;
            let Ok(line) = std::str::from_utf8(&line) else {
                continue;
            };
            let Some((n, label)) = line.split_once('\t') else {
                continue;
            };
            let Ok(n) = n.parse::<u32>() else { continue };
            if room_for_label(&labels, n) {
                labels.insert(
                    n,
                    crate::catalogue::label(label).map(String::into_boxed_str),
                );
            }
        }
        Ok(labels)
    }
}
/// `labels.tsv` read on a blocking thread (millions of lines at full size).
async fn load_labels(path: &Path) -> Result<Labels> {
    let files = LabelFiles {
        labels: path.to_owned(),
        pending: PathBuf::new(),
    };
    tokio::task::spawn_blocking(move || files.load_labels()).await?
}
/// One line of `pending.tsv`.
fn pending_entry(line: &[u8]) -> Option<(String, Vec<u32>)> {
    let line = std::str::from_utf8(line).ok()?;
    let (qid, items) = line.split_once('\t')?;
    let qid = crate::hubs::qid(qid)?;
    let items = items
        .split(',')
        .filter_map(|n| n.parse().ok())
        .take(64)
        .collect();
    Some((qid, items))
}
/// Lines appended to a file, flushed and synced before the cursor that counts them is saved.
struct Appender {
    path: PathBuf,
    file: std::io::BufWriter<std::fs::File>,
    len: u64,
}
impl Appender {
    fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| disk(path))?;
        let len = file.metadata().with_context(|| disk(path))?.len();
        Ok(Self {
            path: path.to_owned(),
            file: std::io::BufWriter::new(file),
            len,
        })
    }
    fn line(&mut self, line: &str) -> Result<()> {
        use std::io::Write;
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.write_all(b"\n"))
            .with_context(|| disk(&self.path))?;
        self.len += line.len() as u64 + 1;
        Ok(())
    }
    fn sync(&mut self) -> Result<u64> {
        use std::io::Write;
        self.file.flush().with_context(|| disk(&self.path))?;
        self.file
            .get_ref()
            .sync_data()
            .with_context(|| disk(&self.path))?;
        Ok(self.len)
    }
}
/// A label as a line of `labels.tsv`.
fn label_line(n: u32, label: Option<&str>) -> String {
    format!("{n}\t{}", label.unwrap_or(""))
}
fn pending_line(qid: &str, items: &[u32]) -> String {
    let items: Vec<String> = items.iter().map(u32::to_string).collect();
    format!("{qid}\t{}", items.join(","))
}

/// Logs a long step's progress at most every [`PROGRESS_EVERY`].
struct Progress(std::time::Instant);
impl Progress {
    fn new() -> Self {
        Self(std::time::Instant::now())
    }
    fn due(&mut self) -> bool {
        if self.0.elapsed() >= PROGRESS_EVERY {
            self.0 = std::time::Instant::now();
            true
        } else {
            false
        }
    }
}

#[async_trait::async_trait]
impl Importer for Wikidata {
    fn catalogue(&self) -> Catalogue {
        Catalogue::Wikidata
    }
    /// The dump `--from` names.
    fn source(&self) -> Option<String> {
        self.from.as_ref().map(|from| match from {
            From::File(path) => path.display().to_string(),
            From::Url(url) => url.clone(),
        })
    }
    fn disk_need(&self, run: Run, state: &State) -> u64 {
        disk_need(run, state, self.from.is_some() || self.limit.is_some())
    }
    fn update_every(&self) -> Duration {
        UPDATE_EVERY
    }
    /// A full import when none has finished; an update [`UPDATE_EVERY`] after the last run, or
    /// at once while the position is more than an hour behind (a catch-up after an import).
    async fn plan(&self, state: &State) -> Result<Plan> {
        let Some(at) = state.current_at() else {
            return Ok(Plan::Run(Run::Import));
        };
        let behind = state
            .position
            .clone()
            .and_then(|p| serde_json::from_value::<UpdatePosition>(p).ok())
            .is_some_and(|p| chrono::Utc::now() - p.time > chrono::Duration::hours(1));
        let due = at + chrono::Duration::from_std(UPDATE_EVERY)?;
        let left = (due - chrono::Utc::now()).to_std().unwrap_or_default();
        // A position the gap rule holds back is not caught up at once: it waits for a dump.
        let held = state
            .position
            .clone()
            .and_then(|p| serde_json::from_value::<UpdatePosition>(p).ok())
            .is_some_and(|p| {
                let gap = chrono::Utc::now() - p.time;
                (!p.import && gap > LONGEST_GAP) || gap > RECENT_CHANGES_REACH
            });
        Ok(if left.is_zero() || (behind && !held) {
            Plan::Run(Run::Update)
        } else {
            Plan::Wait(left)
        })
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        // A new import has its own labels: the updates' cached ones are not this import's.
        *self.labels.lock().await = None;
        let stored = job.cursor().cloned();
        let resumed: Option<ImportCursor> =
            stored.clone().and_then(|c| serde_json::from_value(c).ok());
        // The restarts so far: from the marker a restart left, or carried in the cursor of the
        // attempt that followed it.
        let restarted: Option<Restarted> = stored
            .and_then(|c| serde_json::from_value::<Restart>(c).ok())
            .map(|r| r.restart)
            .or_else(|| resumed.as_ref().and_then(|c| c.restarts.clone()));
        let mut cursor = match resumed {
            // `--from` names another dump than the import under way: that one finishes first.
            Some(cursor)
                if self.from.is_some() && !same_source(self.from.as_ref(), &cursor.source) =>
            {
                return Err(Failure::new("other_import_under_way").into());
            }
            Some(cursor) => cursor,
            None => {
                // A limit this import takes over is written down before anything is asked, so a
                // try that fails first keeps it (the request for the import is cleared now).
                let inherited = self.cli_limit.or_else(|| inherited_limit(job.state()));
                if let (Some(limit), None) = (inherited, &restarted) {
                    let keep = Restart {
                        restart: Restarted {
                            date: String::new(),
                            count: 0,
                            after: String::new(),
                            limit: Some(limit),
                        },
                    };
                    job.write(&[], serde_json::to_value(keep)?).await?;
                }
                // An import that starts over after one that failed has a generation of its own:
                // the cards the abandoned attempt wrote and this one doesn't read leave at its end,
                // and the shrink floor counts only this one's. The marker goes back with it.
                if let Some(r) = restarted.as_ref().filter(|r| r.count > 0) {
                    let marker = Restart { restart: r.clone() };
                    job.new_generation_keeping(serde_json::to_value(marker)?)
                        .await?;
                }
                let client = net::client(&contact(job)?, false, self.http)?;
                self.start(job, &client, restarted.as_ref()).await?
            }
        };
        job.reading(cursor.source.version());
        let client = net::client(&contact(job)?, false, self.http)?;
        match self.run_import(job, &client, &mut cursor).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let Some(class) = class(&error).filter(|c| starts_over(c, &cursor.source)) else {
                    return Err(error);
                };
                // The files are no good: remove them, and read the newest dump from its start
                // next time (the run is recorded as failed, and tried again).
                let date = cursor
                    .source
                    .dump()
                    .map(|d| d.date.clone())
                    .unwrap_or_default();
                let count = match &restarted {
                    Some(r) if r.date == date => r.count + 1,
                    _ => 1,
                };
                remove_files(job.dir(), &cursor.source);
                if count >= RESTARTS {
                    // The dump failed as often as one may: refused, it waits for the next one,
                    // and the large-import turn is free meanwhile.
                    let note = format!("dump {date} failed {count} times ({class})");
                    return Err(job.refuse(cursor.source.version(), "refused", note));
                }
                tracing::warn!(
                    catalogue = "wikidata",
                    class,
                    dump = %date,
                    count,
                    "the wikidata import starts over from the newest dump"
                );
                let restart = Restart {
                    restart: Restarted {
                        date,
                        count,
                        after: class.to_owned(),
                        limit: cursor.sticky,
                    },
                };
                job.write(&[], serde_json::to_value(restart)?).await?;
                Err(error)
            }
        }
    }
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        let contact = contact(job)?;
        let client = net::client(&contact, true, self.http)?;
        let Some(position) = job
            .position()
            .cloned()
            .and_then(|p| serde_json::from_value::<UpdatePosition>(p).ok())
        else {
            return Ok(UpdateEnd::NeedsImport(
                "no position to read the changes from",
            ));
        };
        let now = chrono::Utc::now();
        // An update's position too old (the daemon was down, or an update kept failing and was
        // resumed run after run), or an import's beyond what the recent changes still hold: the
        // next weekly dump instead, one newer than the imported one and than the position (an
        // older one's cards would be older than those it replaced). An update under way is
        // dropped with it.
        let gap = now - position.time;
        if (!position.import && gap > LONGEST_GAP) || gap > RECENT_CHANGES_REACH {
            let latest = net::latest_dump(&client, &self.endpoints.dumps).await?;
            let imported = job.state().upstream_version.clone().unwrap_or_default();
            // A dump refused before is no newer dump to import.
            if latest.date.as_str() > imported.as_str()
                && latest.time().is_some_and(|t| t > position.time)
                && job.refused() != Some(&serde_json::Value::from(latest.date.clone()))
            {
                return Ok(UpdateEnd::NeedsImport(
                    "the changes are more than five days behind; a newer dump is imported",
                ));
            }
            tracing::info!(
                catalogue = "wikidata",
                "the changes are more than five days behind; waiting for the next weekly dump"
            );
            return Ok(UpdateEnd::Done);
        }
        let mut cursor: UpdateCursor = match job
            .cursor()
            .cloned()
            .and_then(|c| serde_json::from_value(c).ok())
        {
            Some(c) => c,
            None => {
                // Changes of the last minute may still be coming in: read up to a minute ago,
                // and at most a window of them.
                UpdateCursor {
                    end: seconds(
                        (now - chrono::Duration::minutes(1)).min(position.time + UPDATE_WINDOW),
                    ),
                    next: None,
                }
            }
        };
        if cursor.end <= position.time {
            return Ok(UpdateEnd::Done);
        }
        // The changes after a position refused before (they would remove too much) are not read
        // again: the updates wait there, and the gap rule above imports a newer dump in time.
        let window = serde_json::to_value(position.time)?;
        if job.refused_before(&window) {
            return Err(
                job.still_refused(format!("the changes after {}", position.time.to_rfc3339()))
            );
        }
        let limit = job.limit().or(position.limit);
        let files = LabelFiles::new(job.dir());
        // A line a hard stop cut off ends no label: the file goes back to its last whole line
        // before it is read and appended to.
        LabelFiles::end_on_a_line(&files.labels)?;
        let mut labels = self.cached_labels(&files).await?;
        let mut appender = Appender::open(&files.labels)?;
        // Items fetched in this run: an item edited again later in the window is fetched once.
        let mut fetched: BTreeSet<String> = BTreeSet::new();
        let mut progress = Progress::new();
        let mut pages = 0u64;
        // Changed items whose answer alone is over the cap (logged by name as they come).
        let mut skipped = 0u64;
        loop {
            let page = net::recent_changes(
                &client,
                &self.endpoints.api,
                position.time,
                cursor.end,
                cursor.next.as_deref(),
            )
            .await?;
            pages += 1;
            if page.next.is_some() && page.next == cursor.next {
                // An answer that names its own page as the next one would never end.
                return Err(Failure::new("api")
                    .at(net::host(&self.endpoints.api))
                    .into());
            }
            let mut qids: Vec<String> = page
                .changes
                .iter()
                .filter(|c| fetched.insert(c.qid.clone()))
                .map(|c| c.qid.clone())
                .collect();
            // A limited catalogue that is full takes no new item: only its own items are asked.
            if let Some(limit) = limit {
                if self.held(job) >= limit {
                    let stored = self.stored_ids(job, &qids).await?;
                    qids.retain(|q| stored.contains(q));
                }
            }
            let mut cards = Vec::new();
            let mut gone = Vec::new();
            // First the sitelinks of every changed item (a short answer): an item deleted,
            // merged or without an article is known from them. Only items with an article, and
            // the classes and genres whose labels the catalogue keeps, are read in full: most
            // changed items are neither.
            let mut full: Vec<String> = Vec::new();
            for chunk in qids.chunks(ENTITIES_PER_REQUEST) {
                skipped += self
                    .fetch(&client, chunk, net::SITELINKS, |got| {
                        gone.extend(got.gone);
                        for (qid, entity) in got.found {
                            let label_item =
                                entity::qid_number(&qid).is_some_and(|n| labels.contains_key(&n));
                            if label_item || !entity::wikipedia_articles(&entity).is_empty() {
                                full.push(qid);
                            } else {
                                gone.push(qid);
                            }
                        }
                        Ok(())
                    })
                    .await?;
            }
            full.sort();
            full.dedup();
            for chunk in full.chunks(ENTITIES_PER_REQUEST) {
                // Each answer is parsed as it comes, so only one answer's JSON is held at a time.
                skipped += self
                    .fetch(&client, chunk, net::FULL, |got| {
                        gone.extend(got.gone);
                        for (qid, entity) in got.found {
                            let parsed = entity::parse_entity(entity, &labels);
                            // A class or a genre renamed: its new label, for the cards read later.
                            if let Some((n, label)) = parsed.label {
                                if labels.get(&n) != Some(&label) && room_for_label(&labels, n) {
                                    appender.line(&label_line(n, label.as_deref()))?;
                                    labels.insert(n, label);
                                }
                            }
                            match parsed.card {
                                Some(card) => cards.push((card, parsed.label_items)),
                                None => gone.push(qid),
                            }
                        }
                        Ok(())
                    })
                    .await?;
            }
            // The labels the changed cards name that are not known yet.
            let unknown: BTreeSet<u32> = cards
                .iter()
                .flat_map(|(_, items)| items.iter().copied())
                .filter(|n| !labels.contains_key(n))
                .collect();
            look_up_labels(
                &client,
                &self.endpoints.api,
                unknown,
                &mut labels,
                &mut appender,
            )
            .await?;
            appender.sync()?;
            let mut keep = Vec::new();
            for (card, items) in cards {
                let card = entity::resolve(card, &items, &labels).0;
                // A card the table would refuse: the item leaves rather than keep a stale card.
                if card.checked(Catalogue::Wikidata).is_ok() {
                    keep.push(card);
                } else {
                    gone.push(card.source_id);
                }
            }
            gone.sort();
            gone.dedup();
            // Only the items the catalogue holds are removed (most items gone or without an
            // article were never in it): a removal of nothing changes nothing.
            let held = self.stored_ids(job, &gone).await?;
            gone.retain(|q| held.contains(q));
            if let Some(limit) = limit {
                // The page's removals make room: a held item merged into one not held yet is
                // replaced by it (the catalogue ends within its limit, both written in turn).
                keep = self
                    .within_limit(job, keep, limit, gone.len() as u64)
                    .await?;
            }
            // A broken answer never empties the catalogue: the update's removals stay within a
            // tenth of the cards (and past ten, `diff::too_many`); past it the update is refused
            // before this page is written.
            let held = job.state().cards.max(0) as u64;
            let removing = job.report().removed + gone.len() as u64;
            if super::diff::too_many(removing, held) {
                let note = format!(
                    "the changes after {} would remove {removing} of {held} cards",
                    position.time.to_rfc3339()
                );
                return Err(job.refuse(window, "refused", note));
            }
            // The page's cards first, with the cursor of this page; then its removals, with the
            // next page's: a run cut off between the two reads this page again.
            let this = serde_json::to_value(&cursor)?;
            job.write(&keep, this).await?;
            cursor.next = page.next.clone();
            job.remove(&gone, serde_json::to_value(&cursor)?).await?;
            if progress.due() {
                let at = page.changes.last().map(|c| c.at.to_rfc3339());
                tracing::info!(catalogue = "wikidata", pages, items = fetched.len(), skipped, at, end = %cursor.end, "wikidata changes read so far");
            }
            if page.next.is_none() {
                break;
            }
        }
        *self.labels.lock().await = Some((appender.sync()?, labels));
        // The position moves to the window's end even when it held no change: Wikidata changes
        // many times a minute, so a long window without one says the source may not answer as
        // it should (the lag would not show it).
        if fetched.is_empty() && cursor.end - position.time >= chrono::Duration::minutes(10) {
            tracing::warn!(catalogue = "wikidata", from = %position.time, to = %cursor.end, "wikidata recent changes named no item in the window");
        }
        if skipped > 0 {
            tracing::warn!(
                catalogue = "wikidata",
                skipped,
                "wikidata changed items left as they were (answers over the cap)"
            );
        }
        // An import's catch-up stays one until it is less than an hour behind.
        job.set_position(serde_json::to_value(UpdatePosition {
            time: cursor.end,
            import: position.import && now - cursor.end > chrono::Duration::hours(1),
            limit: position.limit,
        })?);
        job.set_current_as_of(cursor.end);
        Ok(UpdateEnd::Done)
    }
}

/// The limit a new import takes over when none is configured: the limit of the import that
/// starts over (its restart marker), or, for an import an update asked for, the last import's
/// (in the position). An import the operator asked for (`catalogue import --request`) takes none.
fn inherited_limit(state: &State) -> Option<u64> {
    let restarted = state
        .cursor
        .clone()
        .and_then(|c| serde_json::from_value::<Restart>(c).ok())
        .and_then(|r| r.restart.limit);
    let asked = state
        .import_requested
        .as_ref()
        .filter(|r| r.as_str() != super::musicbrainz::REQUESTED)
        .and_then(|_| state.position.clone())
        .and_then(|p| serde_json::from_value::<UpdatePosition>(p).ok())
        .and_then(|p| p.limit);
    restarted.or(asked)
}
/// The free disk a run still needs ([`Importer::disk_need`]); `small` for a new import read as a
/// stream or from a local file.
fn disk_need(run: Run, state: &State, small: bool) -> u64 {
    let small = small || inherited_limit(state).is_some();
    if run == Run::Update {
        return SMALL_NEED;
    }
    let cursor: Option<ImportCursor> = state
        .cursor
        .clone()
        .and_then(|c| serde_json::from_value(c).ok());
    match cursor {
        // The file is made at its full length without its blocks (sparse): what is still to
        // come takes disk.
        Some(ImportCursor {
            source: Source::Download { dump },
            step: Step::Download { parts },
            ..
        }) => left_to_download(dump.size, parts) + SPARE,
        Some(ImportCursor {
            step: Step::Done, ..
        }) => SMALL_NEED,
        Some(ImportCursor {
            source: Source::Download { .. },
            ..
        }) => SPARE,
        Some(_) => SMALL_NEED,
        None if small => SMALL_NEED,
        None => DUMP_SIZE + SPARE,
    }
}
/// The bytes the two connections still have to read.
fn left_to_download(size: u64, parts: [u64; 2]) -> u64 {
    let (h, _) = halves(size);
    (h[0].1 - parts[0].min(h[0].1)) + (h[1].1 - parts[1].clamp(h[1].0, h[1].1))
}
/// Whether a failure of class `class` makes the import start over: the files on disk (or the
/// dump upstream) are no good, so trying again from the cursor would fail the same way. A local
/// file is the operator's, and is never thrown away.
fn starts_over(class: &str, source: &Source) -> bool {
    let local = matches!(source, Source::File { .. });
    match class {
        "checksum" | "dump_changed" | "dump_gone" | "label_files" | "dump_missing" => !local,
        "bz2_header" | "bz2_block" | "bz2_truncated" | "bz2_oversized" | "bad_dump" => !local,
        _ => false,
    }
}
/// Whether `--from` names the source of the import under way.
fn same_source(from: Option<&From>, source: &Source) -> bool {
    match (from, source) {
        (Some(From::File(a)), Source::File { path, .. }) => a == path,
        (Some(From::Url(a)), Source::Stream { dump, .. }) => *a == dump.url,
        (None, _) => true,
        _ => false,
    }
}
/// The import's work files: the downloaded dump and the label files.
fn remove_files(dir: &Path, source: &Source) {
    let files = LabelFiles::new(dir);
    let mut paths = vec![files.labels, files.pending];
    if let Source::Download { dump } = source {
        paths.push(dir.join(dump.file_name()));
    }
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

/// The two halves of a file of `size` bytes, as `(from, to)`, and the middle.
fn halves(size: u64) -> ([(u64, u64); 2], u64) {
    let mid = size / 2;
    ([(0, mid), (mid, size)], mid)
}
/// The download's file at its full length, so both connections write into it at their offsets.
fn preallocate(path: &Path, size: u64) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        // A link put in its place is not followed.
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| disk(path))?;
    file.set_len(size).with_context(|| disk(path))?;
    Ok(())
}
/// Waits (the runner's `Waits`: the run and its cursor kept, stepped again later) unless the file
/// system under the job's directory has `need` bytes free.
fn room(job: &Job<'_>, need: u64) -> Result<()> {
    job.need_disk(need)
}
/// The downloaded dump against its listed SHA-1 (none listed: the blocks' own CRCs check it as
/// it is read).
async fn verify(client: &reqwest::Client, dump: &net::Dump, path: &Path) -> Result<()> {
    let Some(want) = net::dump_sha1(client, dump).await? else {
        tracing::info!(
            catalogue = "wikidata",
            "the dump's checksum file does not list it; its blocks' CRCs check it as it is read"
        );
        return Ok(());
    };
    let path = path.to_owned();
    let got = tokio::task::spawn_blocking(move || -> Result<String> {
        use sha1::Digest;
        use std::io::Read;
        let mut file = open_work(&path).map_err(|_| Failure::new("dump_missing"))?;
        let mut hasher = sha1::Sha1::new();
        let mut buf = vec![0u8; 8 << 20];
        loop {
            let n = file.read(&mut buf).with_context(|| disk(&path))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    })
    .await??;
    if got != want {
        return Err(Failure::new("checksum").at(net::host(&dump.url)).into());
    }
    Ok(())
}
/// Ask the API for the labels of `items` (at most [`LABEL_LOOKUPS`]), 50 per request; each is
/// added to `labels` and its file.
async fn look_up_labels(
    client: &reqwest::Client,
    api: &str,
    items: BTreeSet<u32>,
    labels: &mut Labels,
    appender: &mut Appender,
) -> Result<usize> {
    let items: Vec<String> = items
        .into_iter()
        .take(LABEL_LOOKUPS)
        .map(|n| format!("Q{n}"))
        .collect();
    for chunk in items.chunks(ENTITIES_PER_REQUEST) {
        for (qid, label) in net::labels(client, api, chunk).await? {
            let Some(n) = entity::qid_number(&qid) else {
                continue;
            };
            if !room_for_label(labels, n) {
                continue;
            }
            appender.line(&label_line(n, label.as_deref()))?;
            labels.insert(n, label.map(String::into_boxed_str));
        }
    }
    Ok(items.len())
}
/// A refused version as a message names it.
fn shown(version: &serde_json::Value) -> String {
    version
        .as_str()
        .map_or_else(|| version.to_string(), str::to_owned)
}
/// The day a dump's file name names (`wikidata-20261123-all.json.bz2`), as a time.
fn named_date(name: &str) -> Option<(String, chrono::DateTime<chrono::Utc>)> {
    let date = name
        .split(['-', '.', '_'])
        .find(|p| p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit()))?;
    let day = chrono::NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
    Some((date.to_owned(), day.and_hms_opt(0, 0, 0)?.and_utc()))
}

impl Wikidata {
    /// A new import: its source (`--from`, else the newest dump, as a stream with a limit, else
    /// downloaded), its files made anew, its first step.
    async fn start(
        &self,
        job: &mut Job<'_>,
        client: &reqwest::Client,
        restarted: Option<&Restarted>,
    ) -> Result<ImportCursor> {
        // The command line's limit is kept; an import an update asked for, or one that starts
        // over, keeps the one kept before. The configured limit applies as it is (none: all).
        let sticky = self.cli_limit.or_else(|| inherited_limit(job.state()));
        let limit = job.limit().or(sticky);
        let source = self.source(client, limit).await?;
        // A dump refused before (it failed again and again, or read no record) waits for the
        // next one; a full import asked for reads it again.
        if job.refused_before(&source.version()) {
            return Err(job.still_refused(format!("dump {}", shown(&source.version()))));
        }
        let files = LabelFiles::new(job.dir());
        for path in [&files.labels, &files.pending] {
            LabelFiles::cut(path, 0)?;
        }
        let step = match &source {
            Source::Download { dump } => {
                if dump.size > DUMP_SIZE_MAX {
                    return Err(Failure::new("size").at(net::host(&dump.url)).into());
                }
                room(job, dump.size + dump.size / 25)?;
                preallocate(&job.dir().join(dump.file_name()), dump.size)?;
                let (h, _) = halves(dump.size);
                Step::Download {
                    parts: [h[0].0, h[1].0],
                }
            }
            _ => Step::Read,
        };
        Ok(ImportCursor {
            source,
            step,
            at: None,
            kept: 0,
            counts: Counts::default(),
            labels_len: 0,
            pending_len: 0,
            limited: false,
            limit,
            restarts: restarted.cloned(),
            sticky,
            labels_dropped: 0,
            pending_dropped: 0,
        })
    }

    /// The import's steps from where the cursor stands.
    async fn run_import(
        &self,
        job: &mut Job<'_>,
        client: &reqwest::Client,
        cursor: &mut ImportCursor,
    ) -> Result<()> {
        if let Some(dump) = cursor.source.dump() {
            // The date names the dump's file in the work directory: only a date. A download is
            // only ever of the dumps directory.
            let foreign = matches!(cursor.source, Source::Download { .. })
                && !dump.url.starts_with(&self.endpoints.dumps);
            if foreign || dump.date.len() != 8 || !dump.date.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Failure::new("cursor").into());
            }
            job.upstream_version(&dump.date).await?;
        } else if let Some(since) = cursor.source.since() {
            // A local file's version is the day its changes are read from.
            job.upstream_version(&since.format("%Y%m%d").to_string())
                .await?;
        }
        if let Step::Download { .. } = cursor.step {
            self.download(job, client, cursor).await?;
        }
        if cursor.step == Step::Verify {
            if let Source::Download { dump } = &cursor.source {
                verify(client, dump, &job.dir().join(dump.file_name())).await?;
            }
            cursor.step = Step::Read;
            job.write(&[], serde_json::to_value(&*cursor)?).await?;
        }
        if cursor.step == Step::Read {
            self.read(job, client, cursor).await?;
        }
        if let Step::Labels { .. } = cursor.step {
            self.fix_labels(job, client, cursor).await?;
            // Written before the work files go: the import's end may fail (the shrink floor)
            // and be tried again from here.
            cursor.step = Step::Done;
            job.write(&[], serde_json::to_value(&*cursor)?).await?;
        }
        // The cards are as current as the dump: the changes are read from its date on.
        if let Some(at) = cursor.source.since() {
            let at = seconds(at);
            job.set_current_as_of(at);
            job.set_position(serde_json::to_value(UpdatePosition {
                time: at,
                import: true,
                limit: cursor.sticky,
            })?);
        }
        // `labels.tsv` stays for the updates.
        let _ = std::fs::remove_file(LabelFiles::new(job.dir()).pending);
        if let Source::Download { dump } = &cursor.source {
            let path = job.dir().join(dump.file_name());
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(disk(&path).into()),
            }
        }
        tracing::info!(
            catalogue = "wikidata",
            lines = cursor.counts.lines,
            kept = cursor.counts.kept,
            no_article = cursor.counts.no_article,
            wikimedia_pages = cursor.counts.wikimedia_pages,
            unusable = cursor.counts.unusable,
            other = cursor.counts.other,
            bad = cursor.counts.bad,
            oversized = cursor.counts.oversized,
            labels_dropped = cursor.labels_dropped,
            pending_dropped = cursor.pending_dropped,
            compressed_bytes = cursor.at.map_or(0, |p| p.bit / 8),
            limited = cursor.limited,
            "wikidata dump read"
        );
        Ok(())
    }

    /// The labels at the start of an update: those the last update ended with when the file
    /// has not changed since, else read from `labels.tsv`.
    async fn cached_labels(&self, files: &LabelFiles) -> Result<Labels> {
        let len = std::fs::metadata(&files.labels).map_or(0, |m| m.len());
        let cached = self.labels.lock().await.take();
        match cached {
            Some((at, labels)) if at == len => Ok(labels),
            _ => load_labels(&files.labels).await,
        }
    }

    /// The dump a new import reads, and how: `--from`, else the newest dump, as a stream with a
    /// limit, else downloaded.
    async fn source(&self, client: &reqwest::Client, limit: Option<u64>) -> Result<Source> {
        // The changes since a dump whose date is unknown are read from a week before it was
        // written: the dumps are weekly.
        let week = chrono::Duration::days(7);
        match &self.from {
            Some(From::File(path)) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let since = match named_date(&name) {
                    Some((_, at)) => at,
                    None => {
                        let modified = std::fs::metadata(path)
                            .and_then(|m| m.modified())
                            .map_err(|_| disk(path))?;
                        chrono::DateTime::<chrono::Utc>::from(modified) - week
                    }
                };
                Ok(Source::File {
                    path: path.clone(),
                    since,
                })
            }
            Some(From::Url(url)) => {
                let head = net::head(client, url)
                    .await?
                    .ok_or_else(|| Failure::new("not_found").at(net::host(url)))?;
                let name = url.rsplit('/').next().unwrap_or_default();
                let (date, since) = match named_date(name) {
                    Some(named) => named,
                    None => {
                        let at = head.modified.unwrap_or_else(chrono::Utc::now) - week;
                        (at.format("%Y%m%d").to_string(), at)
                    }
                };
                Ok(Source::Stream {
                    dump: net::Dump {
                        date,
                        url: url.clone(),
                        size: head.size,
                        validator: head.validator,
                    },
                    since,
                })
            }
            None => {
                let dump = net::latest_dump(client, &self.endpoints.dumps).await?;
                Ok(if limit.is_some() {
                    let since = dump.time().unwrap_or_else(chrono::Utc::now);
                    Source::Stream { dump, since }
                } else {
                    Source::Download { dump }
                })
            }
        }
    }

    /// Download the dump on two connections, a round of `download_round` bytes each at a time,
    /// saving how far each got after every round.
    async fn download(
        &self,
        job: &mut Job<'_>,
        client: &reqwest::Client,
        cursor: &mut ImportCursor,
    ) -> Result<()> {
        let Source::Download { dump } = cursor.source.clone() else {
            return Ok(());
        };
        let path = job.dir().join(dump.file_name());
        if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(dump.size) {
            // The file went (a drop of the directory): the import starts over.
            return Err(Failure::new("dump_missing")
                .at(path.display().to_string())
                .into());
        }
        let (h, _) = halves(dump.size);
        let mut progress = Progress::new();
        loop {
            let Step::Download { parts } = cursor.step else {
                return Ok(());
            };
            if parts[0] >= h[0].1 && parts[1] >= h[1].1 {
                cursor.step = Step::Verify;
                job.write(&[], serde_json::to_value(&*cursor)?).await?;
                return Ok(());
            }
            // The disk may have filled meanwhile: check before every round.
            room(job, left_to_download(dump.size, parts) + dump.size / 50)?;
            let round = |i: usize| {
                let from = parts[i];
                let to = (from + self.download_round).min(h[i].1);
                (from, to)
            };
            let (a, b) = (round(0), round(1));
            let slow = self.slow_window;
            let read = |(from, to): (u64, u64)| {
                let (client, dump, path) = (client.clone(), dump.clone(), path.clone());
                async move {
                    if from >= to {
                        Ok(from)
                    } else {
                        net::read_range(&client, &dump, &path, from, to, slow).await
                    }
                }
            };
            let (ra, rb) = tokio::join!(read(a), read(b));
            // What one connection got is kept even when the other failed.
            let parts = [
                *ra.as_ref().unwrap_or(&parts[0]),
                *rb.as_ref().unwrap_or(&parts[1]),
            ];
            cursor.step = Step::Download { parts };
            job.write(&[], serde_json::to_value(&*cursor)?).await?;
            ra?;
            rb?;
            if progress.due() {
                let done = dump.size - left_to_download(dump.size, parts);
                tracing::info!(
                    catalogue = "wikidata",
                    done_bytes = done,
                    size_bytes = dump.size,
                    "wikidata dump download"
                );
            }
        }
    }

    /// Read the dump from the cursor's position and write its cards in order.
    async fn read(
        &self,
        job: &mut Job<'_>,
        client: &reqwest::Client,
        cursor: &mut ImportCursor,
    ) -> Result<()> {
        let files = LabelFiles::new(job.dir());
        LabelFiles::cut(&files.labels, cursor.labels_len)?;
        LabelFiles::cut(&files.pending, cursor.pending_len)?;
        let mut labels = load_labels(&files.labels).await?;
        let mut label_file = Appender::open(&files.labels)?;
        let mut pending_file = Appender::open(&files.pending)?;
        let first_byte = cursor.at.map_or(0, |p| p.bit / 8);
        let size = match &cursor.source {
            Source::Download { dump } | Source::Stream { dump, .. } => dump.size,
            Source::File { path, .. } => std::fs::metadata(path).map_or(0, |m| m.len()),
        };
        let source: Box<dyn std::io::Read + Send> = match &cursor.source {
            Source::Download { dump } => {
                let path = job.dir().join(dump.file_name());
                Box::new(
                    open_work_at(&path, first_byte)
                        .map_err(|_| Failure::new("dump_missing").at(path.display().to_string()))?,
                )
            }
            Source::File { path, .. } => Box::new(open_at(path, first_byte)?),
            Source::Stream { dump, .. } => Box::new(net::stream(
                client.clone(),
                dump.url.clone(),
                dump.validator.clone(),
                first_byte,
                dump.size,
                self.slow_window,
            )),
        };
        let mut pipeline = pipeline::start(source, cursor.at, self.threads);
        // The limit the import started with, also when it resumes after the configuration
        // changed (a new limit would stop an import read in full and sweep the rest).
        let limit = cursor.limit;
        let mut waiting: BTreeMap<u64, pipeline::Batch> = BTreeMap::new();
        let mut next = 0u64;
        let mut total: Option<u64> = None;
        let mut cards: Vec<Card> = Vec::new();
        let mut progress = Progress::new();
        // A read that failed still writes the batches parsed before the failure (until the
        // parsers are done), so the next run resumes as late as it can.
        let mut failed: Option<anyhow::Error> = None;
        let ended = loop {
            if total == Some(next) {
                break Ok(());
            }
            let Some(out) = pipeline.rx.recv().await else {
                break Err(failed
                    .take()
                    .unwrap_or_else(|| anyhow::Error::new(Failure::new("read"))));
            };
            match out {
                Out::Batch(batch) => {
                    waiting.insert(batch.seq, batch);
                    if waiting.len() > bz2::WAITING_CAP {
                        // A batch never came (a lost thread): stop rather than buffer the dump.
                        break Err(anyhow::Error::new(Failure::new("read")));
                    }
                }
                Out::End(Ok(n)) => total = Some(n),
                Out::End(Err(error)) => {
                    failed = Some(anyhow::Error::new(match error {
                        pipeline::ReadError::Bz2(e) => Failure::new(match e {
                            bz2::Bz2Error::Header => "bz2_header",
                            bz2::Bz2Error::Oversized => "bz2_oversized",
                            bz2::Bz2Error::Block => "bz2_block",
                            bz2::Bz2Error::Truncated => "bz2_truncated",
                        }),
                        pipeline::ReadError::Io(e) => {
                            net::failure_in(&e).unwrap_or_else(|| Failure::new("read"))
                        }
                    }));
                }
            }
            let mut stop = false;
            while let Some(batch) = waiting.remove(&next) {
                next += 1;
                cursor.at = Some(batch.end);
                stop = take_batch(
                    batch,
                    &mut labels,
                    &mut label_file,
                    &mut pending_file,
                    cursor,
                    limit,
                    &mut cards,
                )?;
                if cards.len() >= self.write_cards || stop {
                    cursor.labels_len = label_file.sync()?;
                    cursor.pending_len = pending_file.sync()?;
                    job.write(&std::mem::take(&mut cards), serde_json::to_value(&*cursor)?)
                        .await?;
                }
                if progress.due() {
                    tracing::info!(
                        catalogue = "wikidata",
                        lines = cursor.counts.lines,
                        kept = cursor.kept,
                        read_bytes = cursor.at.map_or(0, |p| p.bit / 8),
                        size_bytes = size,
                        "wikidata dump read so far"
                    );
                }
                if stop {
                    break;
                }
            }
            if stop {
                cursor.limited = true;
                break Ok(());
            }
        };
        pipeline.close().await;
        if ended.is_err() && !cards.is_empty() {
            // The cards parsed before the failure are written: the next run resumes after them.
            cursor.labels_len = label_file.sync()?;
            cursor.pending_len = pending_file.sync()?;
            job.write(&std::mem::take(&mut cards), serde_json::to_value(&*cursor)?)
                .await?;
        }
        ended?;
        // A dump of mostly lines that are not entities is not a dump: no card is removed for it.
        // One that gave no card at all is refused by the runner (`empty`).
        let c = cursor.counts;
        if c.bad + c.oversized > (c.lines / 100).max(100) {
            return Err(Failure::new("bad_dump").into());
        }
        cursor.labels_len = label_file.sync()?;
        cursor.pending_len = pending_file.sync()?;
        cursor.step = Step::Labels {
            done: 0,
            asked: false,
        };
        job.write(&cards, serde_json::to_value(&*cursor)?).await?;
        Ok(())
    }

    /// Write again, with every label now known, the cards whose label items were not read when
    /// they were (in the order of `pending.tsv`); the labels still unknown are asked of the API
    /// first.
    async fn fix_labels(
        &self,
        job: &mut Job<'_>,
        client: &reqwest::Client,
        cursor: &mut ImportCursor,
    ) -> Result<()> {
        let files = LabelFiles::new(job.dir());
        LabelFiles::cut(&files.labels, cursor.labels_len)?;
        LabelFiles::cut(&files.pending, cursor.pending_len)?;
        let mut labels = load_labels(&files.labels).await?;
        let Step::Labels { done, asked } = cursor.step else {
            return Ok(());
        };
        let pending = || -> Result<std::io::BufReader<std::fs::File>> {
            Ok(std::io::BufReader::new(
                open_work(&files.pending).with_context(|| disk(&files.pending))?,
            ))
        };
        if !asked {
            // The whole file is read on a blocking thread (millions of lines at full size).
            let reader = pending()?;
            let path = files.pending.clone();
            let (named, cards) =
                tokio::task::spawn_blocking(move || -> Result<(BTreeSet<u32>, u64)> {
                    let mut named = BTreeSet::new();
                    let mut cards = 0u64;
                    for line in reader.split(b'\n') {
                        let line = line.with_context(|| disk(&path))?;
                        if let Some((_, items)) = pending_entry(&line) {
                            cards += 1;
                            named.extend(items);
                        }
                    }
                    Ok((named, cards))
                })
                .await??;
            let unknown: BTreeSet<u32> = named
                .into_iter()
                .filter(|n| !labels.contains_key(n))
                .collect();
            let mut appender = Appender::open(&files.labels)?;
            let missing = unknown.len();
            let asked = look_up_labels(
                client,
                &self.endpoints.api,
                unknown,
                &mut labels,
                &mut appender,
            )
            .await?;
            cursor.labels_len = appender.sync()?;
            cursor.step = Step::Labels { done, asked: true };
            job.write(&[], serde_json::to_value(&*cursor)?).await?;
            tracing::info!(
                catalogue = "wikidata",
                pending = cards,
                labels_unknown = missing,
                labels_asked = asked,
                "wikidata cards get the labels read after them"
            );
        }
        let mut reader = pending()?;
        std::io::Seek::seek(&mut reader, std::io::SeekFrom::Start(done))
            .with_context(|| disk(&files.pending))?;
        let mut at = done;
        let mut chunk: Vec<(String, Vec<u32>)> = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader
                .read_until(b'\n', &mut line)
                .with_context(|| disk(&files.pending))?;
            at += n as u64;
            if let Some(entry) = pending_entry(line.strip_suffix(b"\n").unwrap_or(&line)) {
                chunk.push(entry);
            }
            if chunk.len() >= self.write_cards.min(super::store::BATCH_ROWS)
                || (n == 0 && !chunk.is_empty())
            {
                let items: BTreeMap<String, Vec<u32>> =
                    std::mem::take(&mut chunk).into_iter().collect();
                let ids: Vec<String> = items.keys().cloned().collect();
                let cards: Vec<Card> = job
                    .stored(&ids)
                    .await?
                    .into_iter()
                    .map(|card| {
                        let own = items
                            .get(&card.source_id)
                            .map(Vec::as_slice)
                            .unwrap_or_default();
                        entity::resolve(card, own, &labels).0
                    })
                    .collect();
                cursor.step = Step::Labels {
                    done: at,
                    asked: true,
                };
                // Cards this import read already: they don't count again for the shrink floor.
                job.rewrite(&cards, serde_json::to_value(&*cursor)?).await?;
            }
            if n == 0 {
                return Ok(());
            }
        }
    }

    /// The items of `chunk` with `props`, asked of the API, each answer handed to `take` as it comes. An answer
    /// over the cap is asked again in halves, and an item whose answer alone is over the cap is
    /// left as it is (logged; the count is returned). Items an answer leaves out are asked again
    /// one by one (two items merged into one may be listed once); one still left out fails the
    /// run (`api`), as nothing is known of it.
    async fn fetch(
        &self,
        client: &reqwest::Client,
        chunk: &[String],
        props: &str,
        mut take: impl FnMut(net::Entities) -> Result<()>,
    ) -> Result<u64> {
        let mut skipped = 0;
        let mut todo: Vec<&[String]> = vec![chunk];
        while let Some(part) = todo.pop() {
            match net::entities(client, &self.endpoints.api, part, self.api_cap, props).await {
                Ok(got) if !got.unaccounted.is_empty() && part.len() == 1 => {
                    tracing::warn!(catalogue = "wikidata", item = %part[0], "an answer leaves out the item asked for");
                    return Err(Failure::new("api")
                        .at(net::host(&self.endpoints.api))
                        .into());
                }
                Ok(mut got) => {
                    let again = std::mem::take(&mut got.unaccounted);
                    if part.len() > 1 {
                        for qid in again.iter().rev() {
                            if let Some(i) = part.iter().position(|q| q == qid) {
                                todo.push(&part[i..=i]);
                            }
                        }
                    }
                    take(got)?;
                }
                Err(e) if class(&e) == Some("size") && part.len() > 1 => {
                    let (a, b) = part.split_at(part.len() / 2);
                    todo.push(b);
                    todo.push(a);
                }
                Err(e) if class(&e) == Some("size") => {
                    skipped += 1;
                    crate::telemetry::catalogue_items_skipped("wikidata", "over_cap", 1);
                    tracing::warn!(
                        catalogue = "wikidata",
                        item = %part[0],
                        "a changed item's answer is over the cap; it keeps its card until it changes again"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok(skipped)
    }
    /// Of `ids`, those the catalogue holds.
    async fn stored_ids(&self, job: &Job<'_>, ids: &[String]) -> Result<BTreeSet<String>> {
        let mut stored = BTreeSet::new();
        for chunk in ids.chunks(super::store::BATCH_ROWS) {
            stored.extend(job.stored(chunk).await?.into_iter().map(|c| c.source_id));
        }
        Ok(stored)
    }
    /// The cards the catalogue holds now, as far as this run knows.
    fn held(&self, job: &Job<'_>) -> u64 {
        let report = job.report();
        (job.state().cards.max(0) as u64 + report.inserted).saturating_sub(report.removed)
    }
    /// With a limit, the changed cards that may be written: those already stored, and new ones
    /// only while the catalogue holds fewer cards than the limit, not counting the `leaving` held
    /// cards the page removes.
    async fn within_limit(
        &self,
        job: &Job<'_>,
        cards: Vec<Card>,
        limit: u64,
        leaving: u64,
    ) -> Result<Vec<Card>> {
        let mut stored = BTreeSet::new();
        for chunk in cards.chunks(super::store::BATCH_ROWS) {
            let ids: Vec<String> = chunk.iter().map(|c| c.source_id.clone()).collect();
            stored.extend(job.stored(&ids).await?.into_iter().map(|c| c.source_id));
        }
        let report = job.report();
        // Less the held cards this page removes after writing.
        let mut held = (job.state().cards.max(0) as u64 + report.inserted)
            .saturating_sub(report.removed)
            .saturating_sub(leaving);
        Ok(cards
            .into_iter()
            .filter(|c| {
                if stored.contains(&c.source_id) {
                    true
                } else if held < limit {
                    held += 1;
                    true
                } else {
                    false
                }
            })
            .collect())
    }
}

/// One batch read, in order: its labels into the map and its file, its cards resolved against
/// every label known now (those still waiting listed in the pending file), up to the limit.
/// Whether the label map takes item `n`'s label: one it holds already, or a new one under
/// [`LABELS_CAP`] (the import, the updates and the lookups alike).
fn room_for_label(labels: &Labels, n: u32) -> bool {
    labels.len() < LABELS_CAP || labels.contains_key(&n)
}
/// Whether the limit is reached.
fn take_batch(
    batch: pipeline::Batch,
    labels: &mut Labels,
    label_file: &mut Appender,
    pending_file: &mut Appender,
    cursor: &mut ImportCursor,
    limit: Option<u64>,
    cards: &mut Vec<Card>,
) -> Result<bool> {
    for (n, label) in batch.labels {
        if room_for_label(labels, n) {
            label_file.line(&label_line(n, label.as_deref()))?;
            labels.insert(n, label);
        } else {
            cursor.labels_dropped += 1;
            crate::telemetry::catalogue_items_skipped("wikidata", "label_dropped", 1);
        }
    }
    // Counted as each batch is taken (a resumed read counts only what it reads again).
    crate::telemetry::catalogue_items_skipped("wikidata", "bad_line", batch.counts.bad);
    crate::telemetry::catalogue_items_skipped("wikidata", "oversized_line", batch.counts.oversized);
    cursor.counts += batch.counts;
    for parsed in batch.items {
        if limit.is_some_and(|l| cursor.kept >= l) {
            return Ok(true);
        }
        let Some(card) = parsed.card else { continue };
        let (card, complete) = entity::resolve(card, &parsed.label_items, labels);
        if !complete {
            if pending_file.len < PENDING_CAP * 64 {
                pending_file.line(&pending_line(&card.source_id, &parsed.label_items))?;
            } else {
                cursor.pending_dropped += 1;
                crate::telemetry::catalogue_items_skipped("wikidata", "pending_dropped", 1);
            }
        }
        cursor.kept += 1;
        cards.push(card);
    }
    Ok(limit.is_some_and(|l| cursor.kept >= l))
}

/// A file opened at byte `at` (a local `--from` file, the operator's: a link is followed).
fn open_at(path: &Path, at: u64) -> Result<std::fs::File> {
    use std::io::Seek;
    let mut file = std::fs::File::open(path).with_context(|| disk(path))?;
    file.seek(std::io::SeekFrom::Start(at))
        .with_context(|| disk(path))?;
    Ok(file)
}
/// A work file opened for reading, never through a link put in its place (as it is written).
fn open_work(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}
/// [`open_work`] at byte `at`.
fn open_work_at(path: &Path, at: u64) -> Result<std::fs::File> {
    use std::io::Seek;
    let mut file = open_work(path).with_context(|| disk(path))?;
    file.seek(std::io::SeekFrom::Start(at))
        .with_context(|| disk(path))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No work file is opened through a link put in its place: the dump file, the label files
    /// cut to their length and appended to. The link's target is left as it was.
    #[test]
    fn work_files_are_never_opened_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.txt");
        std::fs::write(&target, b"keep me").unwrap();
        let link = |name: &str| {
            let path = dir.path().join(name);
            std::os::unix::fs::symlink(&target, &path).unwrap();
            path
        };
        assert!(preallocate(&link("wikidata-20261123-all.json.bz2"), 1 << 20).is_err());
        assert!(LabelFiles::cut(&link("labels.tsv"), 0).is_err());
        assert!(Appender::open(&link("pending.tsv")).is_err());
        // Nor read through one.
        assert!(open_work(&dir.path().join("labels.tsv")).is_err());
        assert!(open_work_at(&dir.path().join("wikidata-20261123-all.json.bz2"), 0).is_err());
        let files = LabelFiles::new(dir.path());
        assert!(files.load_labels().is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
    }

    fn state(cursor: Option<serde_json::Value>) -> State {
        State {
            catalogue: "wikidata".into(),
            phase: "idle".into(),
            run: cursor.as_ref().map(|_| "import".to_owned()),
            generation: 1,
            cursor,
            position: None,
            cards: 0,
            upstream_version: None,
            last_import_at: None,
            last_update_at: None,
            import_requested: None,
            current_as_of: None,
            waiting: None,
            last_error: None,
            error_count: 0,
            scope: None,
            generation_cards: 0,
            shrink_allowed: false,
            updated_at: chrono::Utc::now(),
        }
    }
    fn dump(size: u64) -> net::Dump {
        net::Dump {
            date: "20261123".into(),
            url: "https://dumps.wikimedia.org/x".into(),
            size,
            validator: "\"e\"".into(),
        }
    }
    fn cursor(source: Source, step: Step) -> serde_json::Value {
        serde_json::to_value(ImportCursor {
            source,
            step,
            at: None,
            kept: 0,
            counts: Counts::default(),
            labels_len: 0,
            pending_len: 0,
            limited: false,
            limit: None,
            restarts: None,
            sticky: None,
            labels_dropped: 0,
            pending_dropped: 0,
        })
        .unwrap()
    }

    /// The disk a run asks for: the whole dump for a new full import, a little for an update,
    /// a limited or `--from` import, what is left of a download, and room to spare once it is in.
    #[test]
    fn the_disk_a_run_needs() {
        let size = 100_000_000_000;
        assert_eq!(
            disk_need(Run::Import, &state(None), false),
            DUMP_SIZE + SPARE
        );
        assert_eq!(disk_need(Run::Import, &state(None), true), SMALL_NEED);
        assert_eq!(disk_need(Run::Update, &state(None), false), SMALL_NEED);
        let half = cursor(
            Source::Download { dump: dump(size) },
            Step::Download {
                parts: [size / 4, size / 2],
            },
        );
        assert_eq!(
            disk_need(Run::Import, &state(Some(half)), false),
            size / 4 + size / 2 + SPARE
        );
        let done = cursor(
            Source::Download { dump: dump(size) },
            Step::Download {
                parts: [size / 2, size],
            },
        );
        assert_eq!(disk_need(Run::Import, &state(Some(done)), false), SPARE);
        for step in [
            Step::Verify,
            Step::Read,
            Step::Labels {
                done: 0,
                asked: false,
            },
        ] {
            let c = cursor(Source::Download { dump: dump(size) }, step);
            assert_eq!(disk_need(Run::Import, &state(Some(c)), false), SPARE);
        }
        // Every card written (the work files gone): only the import's end is left.
        let ended = cursor(Source::Download { dump: dump(size) }, Step::Done);
        assert_eq!(
            disk_need(Run::Import, &state(Some(ended)), false),
            SMALL_NEED
        );
        let stream = cursor(
            Source::Stream {
                dump: dump(size),
                since: chrono::Utc::now(),
            },
            Step::Read,
        );
        assert_eq!(
            disk_need(Run::Import, &state(Some(stream)), false),
            SMALL_NEED
        );
        // A restart marker is a new import.
        let restart =
            serde_json::json!({"restart": {"date": "20261123", "count": 1, "after": "checksum"}});
        assert_eq!(
            disk_need(Run::Import, &state(Some(restart)), false),
            DUMP_SIZE + SPARE
        );
        // A marker that carries a limit streams.
        let limited =
            serde_json::json!({"restart": {"date": "", "count": 0, "after": "", "limit": 5}});
        assert_eq!(
            disk_need(Run::Import, &state(Some(limited)), false),
            SMALL_NEED
        );
        // An import an update asked for, after a limited import: the limit carries over.
        let mut asked = state(None);
        asked.import_requested = Some("a newer dump".into());
        asked.position =
            Some(serde_json::json!({"time": "2026-11-23T00:00:00Z", "import": false, "limit": 5}));
        assert_eq!(disk_need(Run::Import, &asked, false), SMALL_NEED);
        asked.import_requested = None;
        assert_eq!(disk_need(Run::Import, &asked, false), DUMP_SIZE + SPARE);
        // One the operator asked for reads the whole dump.
        asked.import_requested = Some(super::super::musicbrainz::REQUESTED.into());
        assert_eq!(disk_need(Run::Import, &asked, false), DUMP_SIZE + SPARE);
    }

    /// An update is due 15 minutes after the last run, or at once while an import's position
    /// is behind; an update's position past the gap waits (for a dump), it is not caught up.
    #[tokio::test]
    async fn updates_run_at_once_while_behind_unless_the_gap_holds_them() {
        let w = Wikidata::new();
        let now = chrono::Utc::now();
        let mut s = state(None);
        assert_eq!(w.plan(&s).await.unwrap(), Plan::Run(Run::Import));
        s.last_import_at = Some(now - chrono::Duration::minutes(5));
        let position = |hours: i64, import: bool| {
            Some(
                serde_json::json!({"time": (now - chrono::Duration::hours(hours)).to_rfc3339(), "import": import}),
            )
        };
        s.position = position(0, false);
        assert!(matches!(w.plan(&s).await.unwrap(), Plan::Wait(_)));
        s.position = position(3 * 24, true);
        assert_eq!(w.plan(&s).await.unwrap(), Plan::Run(Run::Update));
        // An import's position past the gap is still caught up at once.
        s.position = position(6 * 24, true);
        assert_eq!(w.plan(&s).await.unwrap(), Plan::Run(Run::Update));
        s.position = position(3, false);
        assert_eq!(w.plan(&s).await.unwrap(), Plan::Run(Run::Update));
        s.position = position(6 * 24, false);
        assert!(matches!(w.plan(&s).await.unwrap(), Plan::Wait(_)));
        s.last_import_at = Some(now - chrono::Duration::minutes(20));
        assert_eq!(w.plan(&s).await.unwrap(), Plan::Run(Run::Update));
    }

    #[test]
    fn which_failures_start_the_import_over() {
        let download = Source::Download { dump: dump(1) };
        let file = Source::File {
            path: "/x".into(),
            since: chrono::Utc::now(),
        };
        for class in [
            "checksum",
            "dump_changed",
            "dump_gone",
            "label_files",
            "dump_missing",
            "bz2_block",
            "bad_dump",
        ] {
            assert!(starts_over(class, &download), "{class}");
            assert!(!starts_over(class, &file), "{class}");
        }
        for class in [
            "download",
            "timeout",
            "api",
            "disk",
            "no_contact",
            "rate_limited",
            "empty",
        ] {
            assert!(!starts_over(class, &download), "{class}");
        }
    }

    #[test]
    fn dump_dates_come_from_file_names() {
        assert_eq!(
            named_date("wikidata-20261123-all.json.bz2").map(|(d, t)| (d, t.to_rfc3339())),
            Some(("20261123".into(), "2026-11-23T00:00:00+00:00".into()))
        );
        assert_eq!(named_date("latest-all.json.bz2"), None);
        assert_eq!(named_date("wikidata-20261399-all.json.bz2"), None);
    }

    #[test]
    fn from_takes_https_urls_and_files() {
        assert!(From::parse("http://dumps.wikimedia.org/x.json.bz2").is_err());
        assert!(From::parse("https://user:pw@dumps.wikimedia.org/x.json.bz2").is_err());
        assert!(From::parse("https://mirror.example/x.json.bz2?token=secret").is_err());
        assert!(matches!(
            From::parse("https://dumps.wikimedia.org/x.json.bz2"),
            Ok(From::Url(_))
        ));
        assert!(From::parse("/no/such/file.json.bz2").is_err());
        let here = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/wikidata/dump-sample.json.bz2"
        );
        assert!(matches!(From::parse(here), Ok(From::File(p)) if p.is_absolute()));
    }
}
