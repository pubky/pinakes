//! The MusicBrainz importer: every artist, release group and recording of MusicBrainz's JSON
//! dumps as a card.
//!
//! MusicBrainz publishes its JSON dumps twice a week (`json-dumps/<YYYYMMDD-HHMMSS>/`, the
//! newest named in `LATEST`). The importer reads four of their files, each a `tar.xz` holding
//! one JSON line per entity (`mbdump/<entity>`), as streams (nothing is downloaded to disk):
//!
//! 1. `release`: for each release, a record for each artist it credits, one for its release
//!    group, and one for each recording on it (with the release, its date and its release group);
//! 2. `release-group` and `artist`: one record each;
//! 3. `recording`: the recordings on no release.
//!
//! The records are sorted by the item they belong to (`artist/<mbid>`, `recording/<mbid>`,
//! `release-group/<mbid>`) in runs under `catalogues.dir/musicbrainz` ([`super::spill`]), so all
//! that the dump says about one item comes together without holding the dump in memory: a
//! recording's releases give its fame and its earliest release, its album and its album's genres;
//! a release group's releases give its fame and its releases' album pages; ISRCs (`isrc/<ISRC>`,
//! sorted before the recordings) that name several recordings are left out of the keys.
//!
//! A first import writes every card ([`super::run::Run::Import`]). A new dump is an update: the
//! stored cards' content hashes are sorted with the dump's records, and the merge writes only
//! the cards that are new or changed and removes those the dump no longer has; an unchanged card
//! is not written at all. An update that would remove more than [`MAX_REMOVED_SHARE`] of the
//! cards stops (`removals`) before it removes any. Each file's SHA-256 is checked against the
//! dump's `SHA256SUMS` before anything is written.
//!
//! `[catalogues.musicbrainz] limit` imports at most that many cards: the first releases whose
//! recordings, release groups and credited artists fit, then only those release groups and
//! artists.
pub mod records;

use super::{
    bulk::{self, Lines, Tar},
    failure::Failure,
    run::{Importer, Job, Run, UpdateEnd},
    spill::{self, Spill},
    store::State,
    Card, Catalogue,
};
use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use records::{Item, Rec, Shared};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// The official mirror of the JSON dumps.
pub const MIRROR: &str = "https://data.metabrainz.org/pub/musicbrainz/data/json-dumps/";
/// The files read, in order.
const FILES: [&str; 4] = ["release", "release-group", "artist", "recording"];
/// The longest dump line read (bytes): a longer one is skipped and counted.
pub const LINE_CAP: usize = 128 << 20;
/// The largest compressed file read (bytes).
const FILE_CAP: u64 = 128 << 30;
/// The free disk a full read needs under `catalogues.dir/musicbrainz`: the sorted runs (measured
/// on a sample: see `docs/deployment.md`) with room to spare.
pub const DISK_NEED: u64 = 40_000_000_000;
/// The disk a read needs per card of `limit`, and at least.
const DISK_PER_LIMITED_CARD: u64 = 16_384;
const DISK_LEAST: u64 = 256 << 20;
/// What the file system keeps free while runs are written.
const DISK_FLOOR: u64 = 1 << 30;
/// Memory for the records sorted at once, all readers together.
const SORT_MEMORY: usize = 512 << 20;
/// The xz decoder's threads and memory.
const XZ_THREADS: u32 = 2;
const XZ_MEMORY: u64 = 512 << 20;
/// An update stops when it would remove more than this share of the cards.
pub const MAX_REMOVED_SHARE: f64 = 0.10;
/// Bad lines a file may hold: more than this many, and more than one in a thousand, fail the run.
const BAD_LINES: u64 = 100;
/// How often a new dump is looked for.
const UPDATE_EVERY: Duration = Duration::from_secs(12 * 3600);

/// Where the dumps are read from: a mirror's `json-dumps/` directory (with `LATEST`), or a local
/// directory holding one dump's files (`<entity>.tar.xz`, and `SHA256SUMS` when it is checked).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Mirror(url::Url),
    Dir(PathBuf),
}
impl Source {
    /// The official mirror ([`MIRROR`]).
    pub fn official() -> Self {
        Self::Mirror(url::Url::parse(MIRROR).expect("a URL"))
    }
    /// A mirror URL (`https://`, or `http://` on the loopback address) or a local directory.
    pub fn parse(from: &str) -> Result<Self> {
        if from.starts_with("http://") || from.starts_with("https://") {
            let mut url = url::Url::parse(from).context("not a URL")?;
            let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
            if url.scheme() == "http" && !loopback {
                bail!("a dump mirror is read over https");
            }
            if url.query().is_some() || !url.username().is_empty() || url.password().is_some() {
                bail!("a dump mirror URL takes no query or credentials");
            }
            if !url.path().ends_with('/') {
                url.set_path(&format!("{}/", url.path()));
            }
            Ok(Self::Mirror(url))
        } else {
            let dir = PathBuf::from(from);
            anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());
            Ok(Self::Dir(dir))
        }
    }
    fn at(&self) -> String {
        match self {
            Self::Mirror(url) => url.host_str().unwrap_or_default().to_owned(),
            Self::Dir(dir) => dir.display().to_string(),
        }
    }
}
/// A dump's name: `YYYYMMDD-HHMMSS`.
pub fn valid_version(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 15
        && b[8] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 8 || c.is_ascii_digit())
}

/// The MusicBrainz importer.
pub struct MusicBrainz {
    source: Source,
    client: reqwest::Client,
    /// Readers that parse lines in parallel.
    threads: usize,
    /// `[catalogues.musicbrainz] limit`, for the disk a run needs.
    limit: Option<u64>,
}
impl MusicBrainz {
    /// The importer of `source`, asking with the operator's contact in its User-Agent.
    pub fn new(source: Source, contact: &str) -> Result<Self> {
        // A client of its own, outside `Egress`: it reads only the dump mirror the operator or
        // the build names, which no reference steers. It follows no redirect.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(crate::resolver::egress::user_agent(contact))
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get() / 2)
            .clamp(1, 4);
        Ok(Self {
            source,
            client,
            threads,
            limit: None,
        })
    }
    /// The importer of the official mirror.
    pub fn official(config: &crate::config::Config) -> Result<Self> {
        Ok(Self::new(Source::official(), &config.egress.contact)?
            .limit(config.catalogues.limit(Catalogue::Musicbrainz)))
    }
    /// The `limit` the runs read with (`[catalogues.musicbrainz] limit`), for their disk need.
    pub fn limit(mut self, limit: Option<u64>) -> Self {
        self.limit = limit;
        self
    }
    /// Parse lines on this many threads (at least one).
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    async fn get_text(&self, url: url::Url, cap: usize) -> Result<Option<String>> {
        let failure = || Failure::new("download").at(self.source.at());
        let response = tokio::time::timeout(Duration::from_secs(60), self.client.get(url).send())
            .await
            .map_err(|_| failure())?
            .map_err(|_| failure())?;
        match response.status().as_u16() {
            200 => {}
            404 => return Ok(None),
            _ => bail!(failure()),
        }
        let mut body = vec![];
        let mut response = response;
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), response.chunk())
            .await
            .map_err(|_| failure())?
            .map_err(|_| failure())?
        {
            body.extend_from_slice(&chunk);
            if body.len() > cap {
                bail!(Failure::new("size").at(self.source.at()));
            }
        }
        Ok(Some(String::from_utf8_lossy(&body).into_owned()))
    }
    /// The newest dump's name.
    async fn latest(&self) -> Result<String> {
        let version = match &self.source {
            Source::Mirror(base) => self
                .get_text(base.join("LATEST")?, 256)
                .await?
                .ok_or_else(|| Failure::new("not_found").at(self.source.at()))?
                .trim()
                .to_owned(),
            Source::Dir(dir) => dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| valid_version(n))
                .unwrap_or_else(|| "00000000-000000".into()),
        };
        if !valid_version(&version) {
            bail!(Failure::new("version").at(self.source.at()));
        }
        Ok(version)
    }
    /// The dump's `SHA256SUMS`, file name to digest; empty when a local dump has none.
    async fn sums(&self, version: &str) -> Result<BTreeMap<String, String>> {
        let text = match &self.source {
            Source::Mirror(base) => self
                .get_text(base.join(&format!("{version}/SHA256SUMS"))?, 64 * 1024)
                .await?
                .ok_or_else(|| Failure::new("checksum").at(self.source.at()))?,
            Source::Dir(dir) => match tokio::fs::read_to_string(dir.join("SHA256SUMS")).await {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(_) => bail!(Failure::new("checksum").at(self.source.at())),
            },
        };
        Ok(bulk::sha256_sums(&text).into_iter().collect())
    }

    /// Read the dump, sort its records (with the stored hashes for an update) and write: the
    /// whole of a run of `mode`, or the rest of one cut off.
    async fn read_and_write(&self, job: &mut Job<'_>, mode: Run) -> Result<()> {
        let resumed: Option<Cursor> = job
            .cursor()
            .and_then(|c| serde_json::from_value(c.clone()).ok());
        let version = match &resumed {
            Some(c) => c.version.clone(),
            None => self.latest().await?,
        };
        if !valid_version(&version) {
            bail!(Failure::new("version"));
        }
        job.upstream_version(&version).await?;
        let dir = job.dir().join(format!("spill-{version}"));
        let removals = job.dir().join(format!("removals-{version}"));
        clear_others(job.dir(), &[&dir, &removals])?;
        if resumed.is_none() && removals.exists() {
            std::fs::remove_file(&removals)
                .with_context(|| Failure::new("disk").at(removals.display().to_string()))?;
        }
        let mut as_of = resumed.as_ref().and_then(|c| c.as_of);
        if resumed.as_ref().is_none_or(|c| c.phase == Phase::Write) {
            let spilled = self
                .read(job, mode, &version, &dir, resumed.is_some())
                .await?;
            as_of = as_of.or(spilled.as_of);
            let after = resumed.as_ref().and_then(|c| c.after.clone());
            self.write(job, mode, &version, &dir, &removals, &spilled, after)
                .await?;
        }
        if mode == Run::Update {
            remove_gone(job, &version, &removals, as_of).await?;
        }
        if let Some(at) = as_of {
            job.set_current_as_of(at);
        }
        job.set_position(serde_json::json!({ "version": version }));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&removals);
        Ok(())
    }

    /// The dump read into sorted runs under `dir`, or the runs a cut-off run left there (its
    /// `done` file) when it resumes.
    async fn read(
        &self,
        job: &Job<'_>,
        mode: Run,
        version: &str,
        dir: &Path,
        resumed: bool,
    ) -> Result<Done> {
        if resumed {
            if let Some(done) = read_done(dir) {
                return Ok(done);
            }
        }
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .with_context(|| Failure::new("disk").at(dir.display().to_string()))?;
        }
        let started = Instant::now();
        let mut stored = vec![];
        if mode == Run::Update {
            stored = snapshot(job, &dir.join("stored")).await?;
        }
        let read = ReadPlan {
            source: self.source.clone(),
            client: self.client.clone(),
            handle: tokio::runtime::Handle::current(),
            version: version.to_owned(),
            sums: self.sums(version).await?,
            limit: job.limit(),
            threads: if job.limit().is_some() {
                1
            } else {
                self.threads
            },
            dir: dir.to_owned(),
            max_bytes: self.disk_need(mode, job.state()),
        };
        let mut done = tokio::task::spawn_blocking(move || read_dump(read))
            .await
            .map_err(|_| Failure::new("panic"))??;
        done.runs.extend(stored);
        done.seconds = started.elapsed().as_secs_f64();
        write_done(dir, &done)?;
        tracing::info!(
            catalogue = "musicbrainz",
            version,
            records = done.records,
            lines = done.lines,
            bad_lines = done.bad_lines,
            too_long = done.too_long,
            downloaded = done.downloaded,
            spilled = done.spilled,
            seconds = done.seconds,
            "musicbrainz dump read and sorted"
        );
        Ok(done)
    }

    /// The merge: each item's card, written when new or changed (every card for an import),
    /// and for an update the stored cards the dump no longer has listed in `removals`.
    #[allow(clippy::too_many_arguments)]
    async fn write(
        &self,
        job: &mut Job<'_>,
        mode: Run,
        version: &str,
        dir: &Path,
        removals: &Path,
        spilled: &Done,
        after: Option<String>,
    ) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Part>(4);
        let runs = spilled.runs.clone();
        let merge_dir = dir.to_owned();
        let merging = tokio::task::spawn_blocking(move || merge(&merge_dir, runs, mode, after, tx));
        let started = Instant::now();
        while let Some(part) = rx.recv().await {
            // Listed before the cursor moves past them: a run cut off between the two lists
            // some twice, which removes nothing twice.
            if !part.gone.is_empty() {
                append_removals(removals, &part.gone)?;
            }
            let cursor = serde_json::to_value(Cursor {
                version: version.to_owned(),
                phase: Phase::Write,
                after: Some(part.last.clone()),
                removed: 0,
                as_of: spilled.as_of,
            })?;
            job.write(&part.cards, cursor).await?;
        }
        merging.await.map_err(|_| Failure::new("panic"))??;
        let report = job.report();
        tracing::info!(
            catalogue = "musicbrainz",
            version,
            inserted = report.inserted,
            changed = report.changed,
            unchanged = report.unchanged,
            rejected = report.rejected,
            seconds = started.elapsed().as_secs_f64(),
            "musicbrainz cards written"
        );
        Ok(())
    }
}

/// Where a run is: the dump, the phase, the last item written.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Cursor {
    version: String,
    phase: Phase,
    after: Option<String>,
    /// Removals applied so far (the `remove` phase).
    removed: u64,
    /// The dump's time.
    as_of: Option<chrono::DateTime<chrono::Utc>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Write,
    Remove,
}

/// A finished read: the sorted runs and what the read found (its `done` file).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Done {
    pub runs: Vec<PathBuf>,
    /// The dump's time (the earliest `TIMESTAMP` of its files).
    pub as_of: Option<chrono::DateTime<chrono::Utc>>,
    pub lines: u64,
    pub bad_lines: u64,
    pub too_long: u64,
    pub records: u64,
    pub downloaded: u64,
    pub spilled: u64,
    pub seconds: f64,
    /// Whether every file was read to its end (and its checksum checked when the dump has one).
    pub whole: bool,
}
fn read_done(dir: &Path) -> Option<Done> {
    let text = std::fs::read_to_string(dir.join("done")).ok()?;
    let done: Done = serde_json::from_str(&text).ok()?;
    done.runs.iter().all(|r| r.exists()).then_some(done)
}
fn write_done(dir: &Path, done: &Done) -> Result<()> {
    let disk = || Failure::new("disk").at(dir.display().to_string());
    std::fs::create_dir_all(dir).with_context(disk)?;
    let tmp = dir.join("done.tmp");
    std::fs::write(&tmp, serde_json::to_vec(done)?).with_context(disk)?;
    std::fs::rename(&tmp, dir.join("done")).with_context(disk)?;
    Ok(())
}
/// Remove what runs over other dumps left (a run that went on to a newer dump leaves nothing
/// behind): every `spill-*` directory and `removals-*` file but `keep`.
fn clear_others(work: &Path, keep: &[&Path]) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(work) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&path.as_path()) {
            continue;
        }
        let gone = if name.starts_with("spill-") && path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else if name.starts_with("removals-") && path.is_file() {
            std::fs::remove_file(&path)
        } else {
            continue;
        };
        gone.with_context(|| Failure::new("disk").at(path.display().to_string()))?;
    }
    Ok(())
}

/// The stored cards' hashes, sorted under their keys: what an update compares the dump with.
async fn snapshot(job: &Job<'_>, dir: &Path) -> Result<Vec<PathBuf>> {
    const PAGE: i64 = 20_000;
    let pool = job.pool();
    let mut spill = Some(Spill::new(dir, 64 << 20, DISK_NEED)?.keep_free(DISK_FLOOR));
    let mut after = String::new();
    loop {
        let page: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT source_id,content_hash FROM catalogue_card WHERE catalogue=$1 AND source_id>$2 ORDER BY source_id LIMIT $3",
        )
        .bind(Catalogue::Musicbrainz.name())
        .bind(&after)
        .bind(PAGE)
        .fetch_all(pool)
        .await?;
        let Some((last, _)) = page.last() else {
            break;
        };
        after = last.clone();
        let mut s = spill.take().expect("the spill is put back");
        let full = page.len() < PAGE as usize;
        s = tokio::task::spawn_blocking(move || -> Result<Spill> {
            for (id, hash) in page {
                s.push(id.as_bytes(), &postcard::to_allocvec(&Rec::Stored(hash))?)?;
            }
            Ok(s)
        })
        .await
        .map_err(|_| Failure::new("panic"))??;
        spill = Some(s);
        if full {
            break;
        }
    }
    let s = spill.take().expect("the spill is put back");
    tokio::task::spawn_blocking(move || s.finish())
        .await
        .map_err(|_| Failure::new("panic"))?
}

/// Cards to write and stored cards gone, up to one batch, and the last item they reach.
struct Part {
    cards: Vec<Card>,
    gone: Vec<String>,
    last: String,
}
/// The merge thread: items in key order, those after `after`; a part per
/// [`super::store::BATCH_ROWS`] cards.
fn merge(
    dir: &Path,
    runs: Vec<PathBuf>,
    mode: Run,
    after: Option<String>,
    tx: tokio::sync::mpsc::Sender<Part>,
) -> Result<()> {
    let mut merged = spill::merge(dir, runs)?;
    let mut shared = Shared::default();
    let mut part = Part {
        cards: vec![],
        gone: vec![],
        last: String::new(),
    };
    let send = |part: &mut Part| -> Result<()> {
        let full = std::mem::replace(
            part,
            Part {
                cards: vec![],
                gone: vec![],
                last: part.last.clone(),
            },
        );
        tx.blocking_send(full)
            .map_err(|_| Failure::new("stopped"))?;
        Ok(())
    };
    while let Some((key, payloads)) = merged.next_group()? {
        let key = String::from_utf8(key).map_err(|_| Failure::new("spill"))?;
        let mut recs = Vec::with_capacity(payloads.len());
        for p in payloads {
            recs.push(postcard::from_bytes::<Rec>(&p).map_err(|_| Failure::new("spill"))?);
        }
        let item = Item::of(&key, recs, &mut shared);
        if key.starts_with("isrc/") || after.as_ref().is_some_and(|a| key <= *a) {
            continue;
        }
        let card = item.card.and_then(|card| {
            let row = card.checked(Catalogue::Musicbrainz).ok()?;
            Some((card, row.content_hash))
        });
        match (card, item.stored) {
            (Some((card, hash)), stored) => {
                if mode == Run::Import || stored.as_deref() != Some(&hash[..]) {
                    part.cards.push(card);
                }
            }
            (None, Some(_)) if mode == Run::Update => part.gone.push(key.clone()),
            _ => {}
        }
        part.last = key;
        if part.cards.len() >= super::store::BATCH_ROWS || part.gone.len() >= 10_000 {
            send(&mut part)?;
        }
    }
    if !part.cards.is_empty() || !part.gone.is_empty() || !part.last.is_empty() {
        send(&mut part)?;
    }
    tracing::debug!(
        catalogue = "musicbrainz",
        shared_isrcs = shared.len(),
        "musicbrainz merge finished"
    );
    Ok(())
}
fn append_removals(path: &Path, ids: &[String]) -> Result<()> {
    use std::io::Write;
    let disk = || Failure::new("disk").at(path.display().to_string());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(disk)?;
    for id in ids {
        writeln!(file, "{id}").with_context(disk)?;
    }
    file.sync_data().with_context(disk)?;
    Ok(())
}
/// An update's removals, once the merge has listed them all: refused when they are more than
/// [`MAX_REMOVED_SHARE`] of the cards (none is removed), else removed in parts, the count in the
/// cursor.
async fn remove_gone(
    job: &mut Job<'_>,
    version: &str,
    path: &Path,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => bail!(Failure::new("disk").at(path.display().to_string())),
    };
    let mut ids: Vec<String> = text.lines().map(str::to_owned).collect();
    ids.sort();
    ids.dedup();
    let cards = job.state().cards.max(1) as f64;
    if job.limit().is_none() && ids.len() as f64 > cards * MAX_REMOVED_SHARE {
        tracing::warn!(
            catalogue = "musicbrainz",
            removals = ids.len(),
            cards = job.state().cards,
            "musicbrainz update would remove too many cards; run `pinakesd catalogue import musicbrainz` for a full import"
        );
        bail!(Failure::new("removals"));
    }
    let done = job
        .cursor()
        .and_then(|c| serde_json::from_value::<Cursor>(c.clone()).ok())
        .filter(|c| c.phase == Phase::Remove)
        .map_or(0, |c| c.removed as usize);
    let mut at = done.min(ids.len());
    while at < ids.len() {
        let end = (at + super::store::BATCH_ROWS).min(ids.len());
        let cursor = serde_json::to_value(Cursor {
            version: version.to_owned(),
            phase: Phase::Remove,
            after: None,
            removed: end as u64,
            as_of,
        })?;
        job.remove(&ids[at..end], cursor).await?;
        at = end;
    }
    Ok(())
}

/// What a read needs.
struct ReadPlan {
    source: Source,
    client: reqwest::Client,
    handle: tokio::runtime::Handle,
    version: String,
    sums: BTreeMap<String, String>,
    limit: Option<u64>,
    threads: usize,
    dir: PathBuf,
    max_bytes: u64,
}
impl ReadPlan {
    fn open(&self, file: &str) -> Result<Box<dyn Read + Send>> {
        match &self.source {
            Source::Mirror(base) => {
                let url = base.join(&format!("{}/{file}", self.version))?;
                Ok(Box::new(bulk::Download::open(
                    self.client.clone(),
                    self.handle.clone(),
                    url,
                    FILE_CAP,
                )?))
            }
            Source::Dir(dir) => {
                let path = dir.join(file);
                let opened = std::fs::File::open(&path)
                    .with_context(|| Failure::new("not_found").at(path.display().to_string()))?;
                Ok(Box::new(opened))
            }
        }
    }
}

/// The subset a `limit` keeps: the items of the first releases that fit.
#[derive(Default)]
struct Subset {
    cap: u64,
    recordings: HashSet<String>,
    groups: HashSet<String>,
    artists: HashSet<String>,
    full: bool,
}
impl Subset {
    fn count(&self) -> u64 {
        (self.recordings.len() + self.groups.len() + self.artists.len()) as u64
    }
    /// Take a release's records when its new items fit; `false` once one does not.
    fn take(&mut self, recs: &[(String, Rec)]) -> bool {
        if self.full {
            return false;
        }
        let (mut recordings, mut groups, mut artists) =
            (HashSet::new(), HashSet::new(), HashSet::new());
        for (key, rec) in recs {
            match rec {
                Rec::Track(t) => {
                    if !self.recordings.contains(key) {
                        recordings.insert(key.clone());
                    }
                    for a in &t.artists {
                        let k = records::key(crate::hubs::MbKind::Artist, a);
                        if !self.artists.contains(&k) {
                            artists.insert(k);
                        }
                    }
                }
                Rec::Release { .. } if !self.groups.contains(key) => {
                    groups.insert(key.clone());
                }
                Rec::Credited if !self.artists.contains(key) => {
                    artists.insert(key.clone());
                }
                _ => {}
            }
        }
        let new = (recordings.len() + groups.len() + artists.len()) as u64;
        if self.count() + new > self.cap {
            self.full = true;
            return false;
        }
        self.recordings.extend(recordings);
        self.groups.extend(groups);
        self.artists.extend(artists);
        true
    }
}

/// Lines of one file handed to a parsing thread.
struct Chunk {
    data: Vec<u8>,
    ends: Vec<usize>,
}
const CHUNK_BYTES: usize = 4 << 20;

/// Read the dump's files into sorted runs under `plan.dir`.
fn read_dump(plan: ReadPlan) -> Result<Done> {
    let mut done = Done {
        whole: true,
        ..Done::default()
    };
    let mut subset = plan.limit.map(|cap| Subset {
        cap,
        ..Subset::default()
    });
    let budget = SORT_MEMORY / plan.threads;
    let per_spill = plan.max_bytes / plan.threads as u64;
    let mut spills: Vec<Spill> = (0..plan.threads)
        .map(|i| {
            Ok(
                Spill::new(&plan.dir.join(format!("w{i}")), budget, per_spill)?
                    .keep_free(DISK_FLOOR),
            )
        })
        .collect::<Result<_>>()?;
    for file in FILES {
        if file == "recording" && subset.is_some() {
            continue;
        }
        let name = format!("{file}.tar.xz");
        let at = match &plan.source {
            Source::Mirror(u) => u.host_str().unwrap_or_default().to_owned(),
            Source::Dir(d) => d.join(&name).display().to_string(),
        };
        let mut hashed = bulk::Hashed::new(plan.open(&name)?);
        let stopped;
        let mut timestamp = None;
        {
            let decoded = bulk::xz(
                BufReader::with_capacity(1 << 20, &mut hashed),
                XZ_THREADS,
                XZ_MEMORY,
            )?;
            let mut tar = Tar::new(decoded);
            let wanted = format!("mbdump/{file}");
            loop {
                let Some(entry) = tar.next_entry().map_err(|e| read_failure(e, &at))? else {
                    bail!(Failure::new("archive").at(at.clone()));
                };
                if entry.path == "TIMESTAMP" {
                    let mut text = String::new();
                    tar.data()
                        .take(256)
                        .read_to_string(&mut text)
                        .map_err(|e| read_failure(e.into(), &at))?;
                    timestamp = dump_time(&text);
                    continue;
                }
                if entry.path != wanted {
                    continue;
                }
                let lines = Lines::new(BufReader::with_capacity(1 << 20, tar.data()), LINE_CAP);
                let read = match subset.as_mut() {
                    Some(subset) => read_limited(file, lines, subset, &mut spills[0], &at)?,
                    None => read_parallel(file, lines, &mut spills, &at)?,
                };
                done.lines += read.lines;
                done.bad_lines += read.bad;
                done.too_long += read.too_long;
                done.records += read.records;
                stopped = read.stopped;
                if read.bad > BAD_LINES && read.bad * 1000 > read.lines {
                    bail!(Failure::new("parse").at(at.clone()));
                }
                break;
            }
            if !stopped {
                // The rest of the archive and of the file, so the checksum covers all of it.
                let mut decoded = tar.into_inner();
                std::io::copy(&mut decoded, &mut std::io::sink())
                    .map_err(|e| read_failure(e.into(), &at))?;
                std::io::copy(&mut decoded.into_inner(), &mut std::io::sink())
                    .map_err(|e| read_failure(e.into(), &at))?;
            }
        }
        done.downloaded += hashed.count();
        if let Some(t) = timestamp {
            done.as_of = Some(
                done.as_of
                    .map_or(t, |a: chrono::DateTime<chrono::Utc>| a.min(t)),
            );
        }
        if stopped {
            done.whole = false;
        } else if let Some(want) = plan.sums.get(&name) {
            if *want != hashed.hex() {
                bail!(Failure::new("checksum").at(at));
            }
        } else if matches!(plan.source, Source::Mirror(_)) {
            // The mirror lists every file it serves.
            bail!(Failure::new("checksum").at(at));
        }
    }
    let mut runs = vec![];
    for spill in spills {
        runs.extend(spill.finish()?);
    }
    done.spilled = runs
        .iter()
        .filter_map(|r| std::fs::metadata(r).ok())
        .map(|m| m.len())
        .sum();
    done.runs = runs;
    Ok(done)
}
/// A read error as the run shows it: the failure it carries (a download's), else `read` at the
/// file.
fn read_failure(error: anyhow::Error, at: &str) -> anyhow::Error {
    let carried = error
        .downcast_ref::<std::io::Error>()
        .and_then(|e| e.get_ref())
        .and_then(|e| e.downcast_ref::<Failure>())
        .cloned();
    match carried {
        Some(f) => f.into(),
        None if error.downcast_ref::<Failure>().is_some() => error,
        None => Failure::new("read").at(at.to_owned()).into(),
    }
}
/// The `TIMESTAMP` file's time (`2026-11-25 01:17:36.116289+00`).
fn dump_time(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f%#z")
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

#[derive(Default)]
struct FileRead {
    lines: u64,
    bad: u64,
    too_long: u64,
    records: u64,
    /// The read stopped before the file's end (a `limit`).
    stopped: bool,
}
/// The records of one line of `file`; `None` for a line that is not one.
fn parse_line(file: &str, line: &[u8], out: &mut Vec<(String, Rec)>) -> Option<()> {
    match file {
        "release" => records::release(&serde_json::from_slice(line).ok()?, out),
        "release-group" => out.push(records::group(&serde_json::from_slice(line).ok()?)?),
        "artist" => out.push(records::artist(&serde_json::from_slice(line).ok()?)?),
        "recording" => records::standalone(&serde_json::from_slice(line).ok()?, out),
        _ => return None,
    }
    Some(())
}
fn push_all(spill: &mut Spill, recs: &mut Vec<(String, Rec)>) -> Result<u64> {
    let n = recs.len() as u64;
    for (key, rec) in recs.drain(..) {
        spill.push(key.as_bytes(), &postcard::to_allocvec(&rec)?)?;
    }
    Ok(n)
}
/// One file's lines parsed on `spills.len()` threads, each sorting into its own runs.
fn read_parallel<R: std::io::BufRead>(
    file: &'static str,
    mut lines: Lines<R>,
    spills: &mut [Spill],
    at: &str,
) -> Result<FileRead> {
    let mut read = FileRead::default();
    let (tx, rx) = crossbeam_channel::bounded::<Chunk>(spills.len() * 2);
    let results: Vec<Result<(u64, u64, u64)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = spills
            .iter_mut()
            .map(|spill| {
                let rx = rx.clone();
                scope.spawn(move || -> Result<(u64, u64, u64)> {
                    let (mut lines, mut bad, mut records) = (0, 0, 0);
                    let mut recs = vec![];
                    while let Ok(chunk) = rx.recv() {
                        let mut start = 0;
                        for end in chunk.ends {
                            lines += 1;
                            if parse_line(file, &chunk.data[start..end], &mut recs).is_none() {
                                bad += 1;
                            }
                            records += push_all(spill, &mut recs)?;
                            start = end;
                        }
                    }
                    Ok((lines, bad, records))
                })
            })
            .collect();
        drop(rx);
        let mut feed = || -> Result<()> {
            let mut line = vec![];
            let mut chunk = Chunk {
                data: Vec::with_capacity(CHUNK_BYTES),
                ends: vec![],
            };
            while lines
                .next_line(&mut line)
                .map_err(|e| read_failure(e.into(), at))?
            {
                chunk.data.extend_from_slice(&line);
                chunk.ends.push(chunk.data.len());
                if chunk.data.len() >= CHUNK_BYTES {
                    let full = std::mem::replace(
                        &mut chunk,
                        Chunk {
                            data: Vec::with_capacity(CHUNK_BYTES),
                            ends: vec![],
                        },
                    );
                    if tx.send(full).is_err() {
                        break;
                    }
                }
            }
            if !chunk.ends.is_empty() {
                let _ = tx.send(chunk);
            }
            Ok(())
        };
        let fed = feed();
        drop(tx);
        let mut results: Vec<Result<(u64, u64, u64)>> = workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| Err(Failure::new("panic").into()))
            })
            .collect();
        if let Err(e) = fed {
            results.push(Err(e));
        }
        results
    });
    for result in results {
        let (lines, bad, records) = result?;
        read.lines += lines;
        read.bad += bad;
        read.records += records;
    }
    read.too_long = lines.too_long();
    Ok(read)
}
/// One file's lines in order on this thread, kept to `subset`: releases while their items fit,
/// then only the release groups and artists those name (stopping once all are found).
fn read_limited<R: std::io::BufRead>(
    file: &'static str,
    mut lines: Lines<R>,
    subset: &mut Subset,
    spill: &mut Spill,
    at: &str,
) -> Result<FileRead> {
    let mut read = FileRead::default();
    let mut line = vec![];
    let mut recs = vec![];
    let mut found = 0usize;
    let wanted = match file {
        "release-group" => subset.groups.len(),
        "artist" => subset.artists.len(),
        _ => usize::MAX,
    };
    while lines
        .next_line(&mut line)
        .map_err(|e| read_failure(e.into(), at))?
    {
        read.lines += 1;
        if parse_line(file, &line, &mut recs).is_none() {
            read.bad += 1;
            continue;
        }
        match file {
            "release" => {
                if !subset.take(&recs) {
                    recs.clear();
                    read.stopped = true;
                    break;
                }
            }
            _ => {
                let set = if file == "artist" {
                    &subset.artists
                } else {
                    &subset.groups
                };
                recs.retain(|(k, _)| set.contains(k));
                found += recs.len();
            }
        }
        read.records += push_all(spill, &mut recs)?;
        if found >= wanted {
            read.stopped = true;
            break;
        }
    }
    read.too_long = lines.too_long();
    Ok(read)
}

#[async_trait]
impl Importer for MusicBrainz {
    fn catalogue(&self) -> Catalogue {
        Catalogue::Musicbrainz
    }
    fn disk_need(&self, _run: Run, _state: &State) -> u64 {
        match self.limit {
            Some(limit) => limit.saturating_mul(DISK_PER_LIMITED_CARD).max(DISK_LEAST),
            None => DISK_NEED,
        }
    }
    fn update_every(&self) -> Duration {
        UPDATE_EVERY
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        self.read_and_write(job, Run::Import).await
    }
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        if job.cursor().is_none() {
            let latest = self.latest().await?;
            let current = job
                .position()
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            if latest.as_str() <= current.as_str() {
                return Ok(UpdateEnd::Done);
            }
        }
        self.read_and_write(job, Run::Update).await?;
        Ok(UpdateEnd::Done)
    }
}
/// The importer `pinakesd` runs: the official mirror.
pub fn importer(config: &crate::config::Config) -> Option<Arc<dyn Importer>> {
    match MusicBrainz::official(config) {
        Ok(importer) => Some(Arc::new(importer)),
        Err(error) => {
            tracing::warn!(error = %crate::resolver::error_class(&error), "the musicbrainz importer has no HTTP client");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(entity: &str) -> Vec<Vec<u8>> {
        std::fs::read(format!(
            "{}/tests/fixtures/catalogue/musicbrainz/{entity}.jsonl",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
    }

    #[test]
    fn every_fixture_line_gives_its_records() {
        let mut counts = BTreeMap::new();
        for file in FILES {
            for line in fixture(file) {
                let mut out = vec![];
                parse_line(file, &line, &mut out).unwrap_or_else(|| panic!("{file}"));
                for (key, rec) in out {
                    let kind = match rec {
                        Rec::Artist(_) => "artist",
                        Rec::Credited => "credited",
                        Rec::Group(_) => "group",
                        Rec::Release { .. } => "release",
                        Rec::Track(_) => "track",
                        Rec::Isrc(_) => "isrc",
                        Rec::Stored(_) => "stored",
                    };
                    assert!(key.len() < spill::KEY_CAP);
                    *counts.entry(kind).or_insert(0) += 1;
                }
            }
        }
        assert_eq!(
            counts,
            BTreeMap::from([
                ("artist", 2),
                ("credited", 3),
                ("group", 2),
                ("release", 3),
                ("track", 12),
                ("isrc", 10),
            ])
        );
    }

    /// An item's card is the same whatever order its records come in (the sorted runs keep no
    /// order within a key): its content hash too.
    #[test]
    fn a_card_does_not_depend_on_the_order_of_its_records() {
        let mut recs: Vec<(String, Rec)> = vec![];
        for line in fixture("release") {
            parse_line("release", &line, &mut recs).unwrap();
        }
        let key = "recording/aeeaa6df-4ab1-4eee-b72f-f21b5c7cbdfe";
        let mine: Vec<Rec> = recs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, r)| r.clone())
            .collect();
        assert_eq!(mine.len(), 2);
        let hash = |recs: Vec<Rec>| {
            Item::of(key, recs, &mut Shared::default())
                .card
                .unwrap()
                .checked(Catalogue::Musicbrainz)
                .unwrap()
                .content_hash
        };
        let mut reversed = mine.clone();
        reversed.reverse();
        assert_eq!(hash(mine), hash(reversed));
    }

    /// Fuzz: lines cut, with bytes changed, with fields of other types or `null`, and random
    /// records never panic; a line that is not an entity counts as bad.
    #[test]
    fn malformed_lines_never_panic() {
        let mut seed = 0x0123_4567_89ab_cdefu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let swaps: [(&[u8], &[u8]); 6] = [
            (b"[]", b"null"),
            (b"\"\"", b"7"),
            (b"null", b"{}"),
            (b"false", b"\"x\""),
            (b"[", b"{\"a\":"),
            (b"\"id\"", b"\"xx\""),
        ];
        let mut bad = 0;
        for file in FILES {
            for line in fixture(file) {
                for _ in 0..300 {
                    let mut l = line.clone();
                    match next() % 3 {
                        0 => l.truncate((next() % (l.len() as u64 + 1)) as usize),
                        1 => {
                            for _ in 0..(1 + next() % 6) {
                                let at = (next() % l.len() as u64) as usize;
                                l[at] = next() as u8;
                            }
                        }
                        _ => {
                            let (from, to) = swaps[(next() % swaps.len() as u64) as usize];
                            if let Some(at) = l.windows(from.len()).position(|w| w == from) {
                                l.splice(at..at + from.len(), to.iter().copied());
                            }
                        }
                    }
                    let mut out = vec![];
                    if parse_line(file, &l, &mut out).is_none() {
                        bad += 1;
                    }
                    let mut shared = Shared::default();
                    for (key, rec) in out {
                        let _ = Item::of(&key, vec![rec], &mut shared)
                            .card
                            .map(|c| c.checked(Catalogue::Musicbrainz));
                    }
                }
            }
        }
        assert!(bad > 0);
        let mut out = vec![];
        assert!(parse_line("artist", b"not json", &mut out).is_none());
        assert!(parse_line("artist", b"{\"id\":\"not an mbid\"}", &mut out).is_none());
        assert!(out.is_empty());
        // A record that does not decode is the spill's failure, never a panic.
        assert!(postcard::from_bytes::<Rec>(&[200, 1, 2]).is_err());
    }

    #[test]
    fn sources_and_versions() {
        assert!(matches!(
            Source::parse("https://data.metabrainz.org/pub/musicbrainz/data/json-dumps"),
            Ok(Source::Mirror(u)) if u.as_str() == "https://data.metabrainz.org/pub/musicbrainz/data/json-dumps/"
        ));
        assert!(Source::parse("http://127.0.0.1:8080/dumps/").is_ok());
        for bad in [
            "http://example.org/dumps/",
            "https://example.org/dumps/?x=1",
            "https://user:pw@example.org/dumps/",
            "/no/such/directory",
        ] {
            assert!(Source::parse(bad).is_err(), "{bad}");
        }
        assert!(valid_version("20261125-001001"));
        for bad in ["20261125001001", "2026112-0010010", "../../etc/x", "", "20261125-00100a"] {
            assert!(!valid_version(bad), "{bad}");
        }
        assert_eq!(
            dump_time("2026-11-25 01:17:36.116289+00\n").map(|t| t.to_rfc3339()),
            Some("2026-11-25T01:17:36.116289+00:00".to_owned())
        );
        assert_eq!(dump_time("yesterday"), None);
    }

    /// The limit's subset takes a release only when all its new items fit, and stops at the first
    /// that does not.
    #[test]
    fn a_limit_takes_whole_releases() {
        let mut releases = vec![];
        for line in fixture("release") {
            let mut recs = vec![];
            parse_line("release", &line, &mut recs).unwrap();
            releases.push(recs);
        }
        // Arovane's album: 4 recordings, its release group and its artist.
        let arovane = releases
            .iter()
            .find(|r| r.iter().filter(|(k, _)| k.starts_with("recording/")).count() == 4)
            .unwrap()
            .clone();
        let mut subset = Subset {
            cap: 5,
            ..Subset::default()
        };
        assert!(!subset.take(&arovane));
        assert_eq!(subset.count(), 0);
        let mut subset = Subset {
            cap: 6,
            ..Subset::default()
        };
        assert!(subset.take(&arovane));
        assert_eq!(subset.count(), 6);
        // Again: nothing new, it fits.
        assert!(subset.take(&arovane));
    }
}
