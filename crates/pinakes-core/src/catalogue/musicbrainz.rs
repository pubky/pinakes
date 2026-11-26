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
//! sorted before the recordings) that name several recordings are left out of the keys. Each
//! file's runs are kept with a manifest once the file is read whole and its SHA-256 checked
//! against the dump's `SHA256SUMS`, and the runs are merged down to at most
//! [`spill::FAN_IN`] before any card is written (the `done` file), so a run cut off resumes
//! without reading again what it has.
//!
//! A first import writes every card ([`super::run::Run::Import`]). A new dump is an update: the
//! stored cards' content hashes are sorted with the dump's records; a first pass over the merge
//! lists the stored cards the dump no longer has, and when they are more than
//! [`MAX_REMOVED_SHARE`] of the cards the update stops there (`removals`) with nothing written,
//! and waits for a newer dump or a full import asked for (`pinakesd catalogue import musicbrainz
//! --request`). Otherwise the second pass writes only the cards that are new or changed (an
//! unchanged card is not written at all), and the listed cards are removed.
//!
//! `[catalogues.musicbrainz] limit` imports at most that many cards: the first releases whose
//! recordings, release groups and credited artists fit, then only those release groups and
//! artists.
pub mod records;

use super::{
    bulk::{self, Lines, Tar},
    failure::Failure,
    run::{Importer, Job, Plan, Run, UpdateEnd},
    spill::{self, Room, Spill},
    store::State,
    Catalogue, Row,
};
use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use records::{tag, Item, Rec, Shared};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
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
/// The decoded bytes an archive may hold after the entry read, before its end.
const TRAILING_CAP: u64 = 16 << 20;
/// The free disk a full read needs under `catalogues.dir/musicbrainz`: the sorted runs of an
/// update (the dump's records and the stored cards' hashes, measured on a sample: see
/// `docs/deployment.md`) with room to spare.
pub const DISK_NEED: u64 = 40_000_000_000;
/// The disk a read of a local dump needs per byte of its files (the sorted runs took about half
/// the compressed files' bytes on a sample of the real dump: `docs/deployment.md`).
const DISK_PER_DUMP_BYTE: u64 = 4;
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
/// Of one item's records at most this many, holding at most this many bytes, are kept to make
/// its card (every one is counted for its fame).
const GROUP_RECORDS: usize = 8192;
const GROUP_BYTES: usize = 64 << 20;
/// An update stops when it would remove more than this share of the cards.
pub const MAX_REMOVED_SHARE: f64 = 0.10;
/// Bad lines a file may hold: more than this many, and more than one in a thousand, fail the run.
const BAD_LINES: u64 = 100;
/// How often a long read or write logs how far it is.
const PROGRESS_EVERY: Duration = Duration::from_secs(60);
/// How often `LATEST` is read for a new dump.
pub const POLL: Duration = Duration::from_secs(3600);
/// The first wait before a broken download is asked for again (it doubles each time).
const RETRY_WAIT: Duration = Duration::from_secs(2);

/// Where the dumps are read from: a mirror's `json-dumps/` directory (with `LATEST`), or a local
/// directory holding one dump's files (`<entity>.tar.xz`, and `SHA256SUMS` when they are
/// checked), named after its dump (`YYYYMMDD-HHMMSS`).
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
    /// A mirror URL (`https://`, or `http://` on the loopback address) or a local directory
    /// named after its dump.
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
            let named = dir
                .canonicalize()
                .ok()
                .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
                .is_some_and(|n| valid_version(&n));
            anyhow::ensure!(
                named,
                "{} is not named after its dump (YYYYMMDD-HHMMSS, as the mirror names it)",
                dir.display()
            );
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
    /// `catalogues.dir/musicbrainz`: what a run cut off left there counts towards its disk need.
    work_dir: Option<PathBuf>,
    sort_memory: usize,
    fan_in: usize,
    retry_wait: Duration,
    /// When `LATEST` was last read by `plan`.
    polled: Mutex<Option<Instant>>,
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
            work_dir: None,
            sort_memory: SORT_MEMORY,
            fan_in: spill::FAN_IN,
            retry_wait: RETRY_WAIT,
            polled: Mutex::new(None),
        })
    }
    /// The importer of the official mirror, as the configuration says (`limit`, the directory).
    pub fn official(config: &crate::config::Config) -> Result<Self> {
        Ok(Self::new(Source::official(), &config.egress.contact)?.configured(config))
    }
    /// The configuration's `limit` and directory.
    pub fn configured(self, config: &crate::config::Config) -> Self {
        self.limit(config.catalogues.limit(Catalogue::Musicbrainz))
            .work_dir(config.catalogues.work_dir(Catalogue::Musicbrainz))
    }
    /// The `limit` the runs read with (`[catalogues.musicbrainz] limit`), for their disk need.
    pub fn limit(mut self, limit: Option<u64>) -> Self {
        self.limit = limit;
        self
    }
    /// The catalogue's directory, for the disk a resumed run still needs.
    pub fn work_dir(mut self, dir: PathBuf) -> Self {
        self.work_dir = Some(dir);
        self
    }
    /// Parse lines on this many threads (at least one).
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }
    /// Sort with this much memory, merging this many runs at once (tests make passes happen).
    pub fn sorting(mut self, memory: usize, fan_in: usize) -> Self {
        self.sort_memory = memory.max(1);
        self.fan_in = fan_in.max(2);
        self
    }
    /// The first wait before a broken download is asked for again.
    pub fn retry_wait(mut self, wait: Duration) -> Self {
        self.retry_wait = wait;
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
                .canonicalize()
                .ok()
                .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default(),
        };
        if !valid_version(&version) {
            bail!(Failure::new("version").at(self.source.at()));
        }
        Ok(version)
    }
    /// The dump's `SHA256SUMS`, file name to digest; `None` when the mirror no longer has the
    /// dump; empty for a local dump without one.
    async fn sums(&self, version: &str) -> Result<Option<BTreeMap<String, String>>> {
        let text = match &self.source {
            Source::Mirror(base) => {
                match self
                    .get_text(base.join(&format!("{version}/SHA256SUMS"))?, 64 * 1024)
                    .await?
                {
                    Some(text) => text,
                    None => return Ok(None),
                }
            }
            Source::Dir(dir) => match tokio::fs::read_to_string(dir.join("SHA256SUMS")).await {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(_) => bail!(Failure::new("checksum").at(self.source.at())),
            },
        };
        Ok(Some(bulk::sha256_sums(&text).into_iter().collect()))
    }
    /// The bytes the catalogue's directory holds (what earlier runs left).
    fn held(&self) -> u64 {
        fn size(path: &Path) -> u64 {
            match std::fs::symlink_metadata(path) {
                Ok(m) if m.is_dir() => std::fs::read_dir(path)
                    .map(|entries| entries.flatten().map(|e| size(&e.path())).sum())
                    .unwrap_or(0),
                Ok(m) => m.len(),
                Err(_) => 0,
            }
        }
        self.work_dir.as_deref().map_or(0, size)
    }
    fn room(&self) -> Room {
        Room::new(self.need(), DISK_FLOOR)
    }
    /// What a read takes at most: [`DISK_NEED`]; for a local dump [`DISK_PER_DUMP_BYTE`] times
    /// its files' size; for a `limit` [`DISK_PER_LIMITED_CARD`] per card; at least
    /// [`DISK_LEAST`].
    fn need(&self) -> u64 {
        let mut need = DISK_NEED;
        if let Source::Dir(dir) = &self.source {
            let files: u64 = FILES
                .iter()
                .filter_map(|f| std::fs::metadata(dir.join(format!("{f}.tar.xz"))).ok())
                .map(|m| m.len())
                .sum();
            need = need.min(files.saturating_mul(DISK_PER_DUMP_BYTE));
        }
        if let Some(limit) = self.limit {
            need = need.min(limit.saturating_mul(DISK_PER_LIMITED_CARD));
        }
        need.max(DISK_LEAST)
    }

    /// The source as cursors name it: a cursor of another source is not resumed.
    fn source_id(&self) -> String {
        match &self.source {
            Source::Mirror(url) => url.to_string(),
            Source::Dir(dir) => dir.display().to_string(),
        }
    }

    /// A run of `mode`: read the dump, sort its records (with the stored hashes for an update),
    /// and write; or the rest of one cut off. A dump that can't be applied as it is (its files
    /// don't match their checksums or don't parse, or it would remove or refuse too many cards)
    /// is refused: an update records the failure once and remembers the dump in its position,
    /// so it is not read again until `LATEST` names a newer one (or a full import is asked
    /// for); an import keeps a `refused` cursor and fails at once on each try until then.
    async fn read_and_write(&self, job: &mut Job<'_>, mode: Run) -> Result<()> {
        let stop = Stop::default();
        let mut resumed: Option<Cursor> = job
            .cursor()
            .and_then(|c| serde_json::from_value(c.clone()).ok());
        // A cursor of another source or limit is another run's: this one starts over.
        if resumed
            .as_ref()
            .is_some_and(|c| c.source != self.source_id() || c.limit != job.limit())
        {
            resumed = None;
        }
        if let Some(c) = resumed.clone().filter(|c| c.phase == Phase::Refused) {
            if self.latest().await? <= c.version {
                bail!(
                    Failure::new(refused_class(c.class.as_deref())).at(c.note.unwrap_or_default())
                );
            }
            resumed = None;
        }
        let mut version = match &resumed {
            Some(c) => c.version.clone(),
            None => self.latest().await?,
        };
        if !valid_version(&version) {
            bail!(Failure::new("version"));
        }
        // A run cut off whose dump the mirror no longer has, and that must read it again, starts
        // over on the newest dump.
        let spill_of = |v: &str| job.dir().join(format!("spill-{v}"));
        if let Some(c) = &resumed {
            if c.phase == Phase::Write
                && read_done(&spill_of(&c.version)).is_none()
                && self.sums(&c.version).await?.is_none()
            {
                tracing::warn!(catalogue = "musicbrainz", version = c.version, "the dump a cut-off run read is gone from the mirror; the run starts over on the newest dump");
                resumed = None;
                version = self.latest().await?;
            }
        }
        let spill_dir = spill_of(&version);
        let removals = job.dir().join(format!("removals-{version}"));
        clear_others(job.dir(), &[&spill_dir, &removals])?;
        if resumed.is_none() && removals.exists() {
            std::fs::remove_file(&removals)
                .with_context(|| Failure::new("disk").at(removals.display().to_string()))?;
        }
        let mut as_of = resumed.as_ref().and_then(|c| c.as_of);
        let (source, limit) = (self.source_id(), job.limit());
        let cursor = |phase, after: Option<String>, removed, as_of| {
            serde_json::to_value(Cursor {
                version: version.clone(),
                phase,
                after,
                removed,
                as_of,
                note: None,
                class: None,
                source: source.clone(),
                limit,
            })
        };
        let phase = resumed.as_ref().map(|c| c.phase);
        if phase.is_none_or(|p| p == Phase::Write) {
            let done = match self
                .read(job, mode, &version, &spill_dir, resumed.is_some(), &stop)
                .await
            {
                Ok(done) => done,
                Err(error) if dump_failure(&error).is_some() => {
                    let class = dump_failure(&error).unwrap_or("refused");
                    return self
                        .refuse(
                            job,
                            mode,
                            &version,
                            class,
                            &failure_note(&error),
                            &spill_dir,
                        )
                        .await;
                }
                Err(error) => return Err(error),
            };
            as_of = as_of.or(done.as_of);
            if resumed.is_none() {
                // From here a cut-off run resumes from the runs, without reading them again.
                job.write_rows(&[], 0, cursor(Phase::Write, None, 0, as_of)?)
                    .await?;
            }
            let cards = job.state().cards.max(0) as u64;
            if mode == Run::Update && !removals.exists() {
                let gone = self.count_gone(&removals, &done, &stop).await?;
                if too_many(gone, cards, job.limit()) {
                    let note = format!("{gone} of {cards} cards gone in dump {version}");
                    return self
                        .refuse(job, mode, &version, "removals", &note, &spill_dir)
                        .await;
                }
            }
            let after = resumed.as_ref().and_then(|c| c.after.clone());
            let read = self
                .write(job, mode, &version, &done, &removals, after, as_of, &stop)
                .await;
            match read {
                Ok(()) => {}
                Err(error) if dump_failure(&error).is_some() => {
                    let class = dump_failure(&error).unwrap_or("refused");
                    return self
                        .refuse(
                            job,
                            mode,
                            &version,
                            class,
                            &failure_note(&error),
                            &spill_dir,
                        )
                        .await;
                }
                Err(error) => return Err(error),
            }
            // A full import that read nothing, or whose cards were mostly refused, removes no
            // card at its end: an empty or broken dump never empties the catalogue.
            let report = job.report();
            if mode == Run::Import
                && cards > 0
                && job.limit().is_none()
                && (done.records == 0 || too_many(report.rejected, cards, None))
            {
                let note = format!(
                    "{} of {cards} cards refused, {} records read in dump {version}",
                    report.rejected, done.records
                );
                return self
                    .refuse(job, mode, &version, "rejected", &note, &spill_dir)
                    .await;
            }
        }
        if mode == Run::Update && phase != Some(Phase::Finished) {
            // Everything an update removes counts: the cards the dump no longer has and those it
            // makes but that are refused. The new and changed cards are written by now.
            if phase != Some(Phase::Remove) {
                let listed = count_lines(&removals)?;
                let cards = job.state().cards.max(0) as u64;
                if too_many(listed, cards, job.limit()) {
                    let note =
                        format!("{listed} of {cards} cards gone or refused in dump {version}");
                    return self
                        .refuse(job, mode, &version, "removals", &note, &spill_dir)
                        .await;
                }
            }
            let done_already = resumed
                .as_ref()
                .filter(|c| c.phase == Phase::Remove)
                .map_or(0, |c| c.removed);
            remove_gone(job, &cursor, &removals, as_of, done_already).await?;
        }
        if phase != Some(Phase::Finished) {
            // Finished: a run cut off from here on (an import's sweep of the cards it did not
            // read) neither reads nor writes anything again.
            job.write_rows(&[], 0, cursor(Phase::Finished, None, 0, as_of)?)
                .await?;
        }
        if let Some(at) = as_of {
            job.set_current_as_of(at);
        }
        job.set_position(serde_json::json!({ "version": version, "limit": job.limit() }));
        job.upstream_version(&version).await?;
        let _ = std::fs::remove_dir_all(&spill_dir);
        let _ = std::fs::remove_file(&removals);
        Ok(())
    }

    /// Refuse the dump `version` (`class`, with our own `note`): its runs are removed. An update
    /// records the failure and ends, remembering the dump in its position (`refused`), so the
    /// cards stay as they are and the dump is not read again; an import keeps a `refused`
    /// cursor and fails.
    async fn refuse(
        &self,
        job: &mut Job<'_>,
        mode: Run,
        version: &str,
        class: &'static str,
        note: &str,
        dir: &Path,
    ) -> Result<()> {
        tracing::warn!(
            catalogue = "musicbrainz",
            version,
            class,
            note,
            "musicbrainz dump refused: it waits for a newer dump, or `pinakesd catalogue import musicbrainz --request` asks for a full import"
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_file(job.dir().join(format!("removals-{version}")));
        let failure = Failure::new(class).at(note.to_owned());
        if mode == Run::Update {
            super::run::record_failure(job.pool(), Catalogue::Musicbrainz, &failure.to_string())
                .await?;
            let mut position = job
                .state()
                .position
                .clone()
                .unwrap_or_else(|| serde_json::json!({}));
            if let Some(p) = position.as_object_mut() {
                p.insert("refused".into(), version.into());
                p.insert("refused_for".into(), failure.to_string().into());
            }
            job.set_position(position);
            return Ok(());
        }
        let cursor = serde_json::to_value(Cursor {
            version: version.to_owned(),
            phase: Phase::Refused,
            after: None,
            removed: 0,
            as_of: None,
            note: Some(note.to_owned()),
            class: Some(class.to_owned()),
            source: self.source_id(),
            limit: job.limit(),
        })?;
        job.write_rows(&[], 0, cursor).await?;
        bail!(failure);
    }

    /// The dump read into sorted runs under `dir` and merged down to at most the fan-in: the
    /// `done` file a cut-off run left when it resumes, else a new read, which keeps the files
    /// an earlier read of this dump finished (their manifests) and reads the others.
    async fn read(
        &self,
        job: &Job<'_>,
        mode: Run,
        version: &str,
        dir: &Path,
        resumed: bool,
        stop: &Stop,
    ) -> Result<Done> {
        if resumed {
            if let Some(done) = read_done(dir) {
                return Ok(done);
            }
        }
        let disk = || Failure::new("disk").at(dir.display().to_string());
        std::fs::create_dir_all(dir).with_context(disk)?;
        let limit = job.limit();
        // What is kept: the files whose manifests are whole (not under a limit, whose subset
        // spans the files); everything else goes.
        let mut kept: BTreeMap<String, FileDone> = BTreeMap::new();
        if limit.is_none() {
            for file in FILES {
                if let Some(done) = read_file_done(dir, file) {
                    kept.insert(file.to_owned(), done);
                }
            }
        }
        clear_spill(dir, &kept)?;
        let started = Instant::now();
        let room = self.room();
        let mut stored = vec![];
        if mode == Run::Update {
            stored = snapshot(job, &dir.join(format!("stored-{}", nonce())), room).await?;
        }
        let Some(sums) = self.sums(version).await? else {
            bail!(Failure::new("not_found").at(self.source.at()));
        };
        let plan = ReadPlan {
            source: self.source.clone(),
            client: self.client.clone(),
            handle: tokio::runtime::Handle::current(),
            version: version.to_owned(),
            sums,
            limit,
            threads: if limit.is_some() { 1 } else { self.threads },
            dir: dir.to_owned(),
            room,
            sort_memory: self.sort_memory,
            retry_wait: self.retry_wait,
            stop: stop.flag(),
            kept,
        };
        let fan_in = self.fan_in;
        let final_dir = dir.join(format!("final-{}", nonce()));
        let flag = stop.flag();
        let mut done = tokio::task::spawn_blocking(move || -> Result<Done> {
            let mut done = read_dump(plan)?;
            let mut runs = std::mem::take(&mut done.runs);
            runs.extend(stored);
            done.runs = spill::reduce(&final_dir, runs, fan_in, &room, &|| {
                flag.load(Ordering::Relaxed)
            })?;
            Ok(done)
        })
        .await
        .map_err(|_| Failure::new("panic"))??;
        done.seconds = started.elapsed().as_secs_f64();
        done.spilled = done
            .runs
            .iter()
            .filter_map(|r| std::fs::metadata(r).ok())
            .map(|m| m.len())
            .sum();
        write_done(dir, &done)?;
        tracing::info!(
            catalogue = "musicbrainz",
            version,
            records = done.records,
            lines = done.lines,
            bad_lines = done.bad_lines,
            too_long = done.too_long,
            downloaded = done.downloaded,
            checked = done.checked,
            runs = done.runs.len(),
            spilled = done.spilled,
            seconds = done.seconds,
            "musicbrainz dump read and sorted"
        );
        Ok(done)
    }

    /// An update's first pass: the stored cards the dump no longer has, listed in `removals`
    /// (written whole, then put in place). Their number.
    async fn count_gone(&self, removals: &Path, done: &Done, stop: &Stop) -> Result<u64> {
        let runs = done.runs.clone();
        let path = removals.to_owned();
        let flag = stop.flag();
        let gone = tokio::task::spawn_blocking(move || list_gone(&runs, &path, &flag))
            .await
            .map_err(|_| Failure::new("panic"))??;
        tracing::info!(
            catalogue = "musicbrainz",
            gone,
            "musicbrainz update: stored cards the dump no longer has"
        );
        Ok(gone)
    }

    /// The merge: each item's card, written when new or changed (every card for an import); for
    /// an update, a stored card whose item no longer makes a valid card is added to `removals`.
    #[allow(clippy::too_many_arguments)]
    async fn write(
        &self,
        job: &mut Job<'_>,
        mode: Run,
        version: &str,
        done: &Done,
        removals: &Path,
        after: Option<String>,
        as_of: Option<chrono::DateTime<chrono::Utc>>,
        stop: &Stop,
    ) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Part>(4);
        let runs = done.runs.clone();
        let flag = stop.flag();
        let merging = tokio::task::spawn_blocking(move || merge(runs, mode, after, tx, &flag));
        let started = Instant::now();
        let mut said = Instant::now();
        let mut rejected = 0;
        while let Some(part) = rx.recv().await {
            if said.elapsed() >= PROGRESS_EVERY {
                said = Instant::now();
                let report = job.report();
                tracing::info!(
                    catalogue = "musicbrainz",
                    version,
                    at = %part.last,
                    inserted = report.inserted,
                    changed = report.changed,
                    unchanged = report.unchanged,
                    "musicbrainz cards being written"
                );
            }
            // Listed before the cursor moves past them: a run cut off between the two lists
            // some twice, which removes nothing twice.
            if !part.gone.is_empty() {
                append_removals(removals, &part.gone)?;
            }
            rejected += part.rejected;
            let cursor = serde_json::to_value(Cursor {
                version: version.to_owned(),
                phase: Phase::Write,
                after: Some(part.last.clone()),
                removed: 0,
                as_of,
                note: None,
                class: None,
                source: self.source_id(),
                limit: job.limit(),
            })?;
            job.write_rows(&part.rows, part.rejected, cursor).await?;
        }
        merging.await.map_err(|_| Failure::new("panic"))??;
        let report = job.report();
        tracing::info!(
            catalogue = "musicbrainz",
            version,
            inserted = report.inserted,
            changed = report.changed,
            unchanged = report.unchanged,
            rejected,
            seconds = started.elapsed().as_secs_f64(),
            "musicbrainz cards written"
        );
        Ok(())
    }
}

/// Asks the threads of a run to stop when the run's future is dropped (a run cut off): a
/// blocking thread can't be cancelled otherwise, and would go on reading into the directory the
/// next attempt uses.
#[derive(Default)]
struct Stop(Arc<AtomicBool>);
impl Stop {
    fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
fn stopped(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Relaxed) {
        bail!(Failure::new("stopped"));
    }
    Ok(())
}
/// A name no other attempt has: an attempt's own directory.
fn nonce() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{}", now.as_nanos(), std::process::id())
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
    /// Why a dump was refused (our own counts), and the failure's class.
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    class: Option<String>,
    /// The source and the limit the run reads with: a cursor of others is not resumed.
    #[serde(default)]
    source: String,
    #[serde(default)]
    limit: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Write,
    Remove,
    /// An import refused for its dump: it waits for a newer one.
    Refused,
    /// Written and removed: what is left is the runner's (an import's sweep).
    Finished,
}

/// A finished read: the sorted runs (at most the fan-in) and what the read found (its `done`
/// file).
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
    /// The files whose SHA-256 was checked.
    pub checked: u32,
    /// The bytes of the merged runs.
    #[serde(default)]
    pub spilled: u64,
    pub seconds: f64,
}
/// One file read whole: its runs and what the read found (its `<file>.done` manifest).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileDone {
    runs: Vec<PathBuf>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    lines: u64,
    bad_lines: u64,
    too_long: u64,
    records: u64,
    downloaded: u64,
    checked: bool,
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let disk = || Failure::new("disk").at(path.display().to_string());
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(value)?).with_context(disk)?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .with_context(disk)?;
    std::fs::rename(&tmp, path).with_context(disk)?;
    Ok(())
}
/// Run paths as a manifest keeps them: relative to the dump's directory.
fn relative(dir: &Path, runs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    runs.iter()
        .map(|r| {
            r.strip_prefix(dir)
                .map(Path::to_owned)
                .map_err(|_| Failure::new("spill").at(r.display().to_string()).into())
        })
        .collect()
}
/// A manifest's run paths under `dir`, each of plain parts and a regular file there; none when
/// one is not (the manifest is then not used).
fn resolved(dir: &Path, runs: &[PathBuf]) -> Option<Vec<PathBuf>> {
    runs.iter()
        .map(|r| {
            let plain = !r.as_os_str().is_empty()
                && r.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)));
            let path = dir.join(r);
            (plain && std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()))
                .then_some(path)
        })
        .collect()
}
fn read_done(dir: &Path) -> Option<Done> {
    let mut done: Done = read_json(&dir.join("done"))?;
    done.runs = resolved(dir, &done.runs)?;
    Some(done)
}
fn write_done(dir: &Path, done: &Done) -> Result<()> {
    let mut kept = done.clone();
    kept.runs = relative(dir, &done.runs)?;
    write_json(&dir.join("done"), &kept)
}
fn read_file_done(dir: &Path, file: &str) -> Option<FileDone> {
    let mut done: FileDone = read_json(&dir.join(format!("{file}.done")))?;
    done.runs = resolved(dir, &done.runs)?;
    Some(done)
}
fn write_file_done(dir: &Path, file: &str, done: &FileDone) -> Result<()> {
    let mut kept = done.clone();
    kept.runs = relative(dir, &done.runs)?;
    write_json(&dir.join(format!("{file}.done")), &kept)
}
/// Remove from a dump's directory every part no kept manifest names: earlier attempts' runs (an
/// attempt cut off may still be writing into its own directory: it fails there), the snapshot,
/// merge passes, the `done` file and manifests not kept.
fn clear_spill(dir: &Path, kept: &BTreeMap<String, FileDone>) -> Result<()> {
    let keep: HashSet<PathBuf> = kept
        .values()
        .flat_map(|d| d.runs.iter())
        .filter_map(|r| r.parent().and_then(Path::parent).map(Path::to_owned))
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let manifest = name
            .strip_suffix(".done")
            .is_some_and(|f| kept.contains_key(f));
        if keep.contains(&path) || manifest {
            continue;
        }
        let gone = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        gone.with_context(|| Failure::new("disk").at(path.display().to_string()))?;
    }
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
async fn snapshot(job: &Job<'_>, dir: &Path, room: Room) -> Result<Vec<PathBuf>> {
    const PAGE: i64 = 20_000;
    let pool = job.pool();
    // It runs alone, before the read: the read's memory.
    let mut spill = Some(Spill::new(dir, SORT_MEMORY, room)?);
    let (mut rows, mut said) = (0u64, Instant::now());
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
        rows += page.len() as u64;
        if said.elapsed() >= PROGRESS_EVERY {
            said = Instant::now();
            tracing::info!(
                catalogue = "musicbrainz",
                rows,
                "musicbrainz update reads the stored cards' hashes"
            );
        }
        let full = page.len() < PAGE as usize;
        let mut s = spill.take().expect("the spill is put back");
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

/// The kind of record that makes a card under `key` (`artist/…`, `release-group/…`,
/// `recording/…`).
fn entity_tag(key: &str) -> Option<usize> {
    Some(match key.split_once('/')?.0 {
        "artist" => tag::ARTIST,
        "release-group" => tag::GROUP,
        "recording" => tag::TRACK,
        _ => return None,
    })
}
/// An update's first pass over the merge: each stored card under a key with no record that
/// makes a card, written to `path` (in key order). Their number.
fn list_gone(runs: &[PathBuf], path: &Path, stop: &AtomicBool) -> Result<u64> {
    let disk = || Failure::new("disk").at(path.display().to_string());
    // Written whole, then put in place: a list there is always a whole one.
    let tmp = path.with_extension("tmp");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp).with_context(disk)?);
    let mut merged = spill::Merge::open(runs)?;
    let (mut gone, mut seen) = (0u64, 0u64);
    let mut said = Instant::now();
    while let Some((key, counts)) = merged.next_counts()? {
        seen += 1;
        if seen.is_multiple_of(65_536) {
            stopped(stop)?;
            if said.elapsed() >= PROGRESS_EVERY {
                said = Instant::now();
                tracing::info!(
                    catalogue = "musicbrainz",
                    items = seen,
                    gone,
                    "musicbrainz update lists what is gone"
                );
            }
        }
        let key = String::from_utf8(key).map_err(|_| Failure::new("spill"))?;
        let Some(entity) = entity_tag(&key) else {
            continue;
        };
        if counts[tag::STORED] > 0 && counts[entity] == 0 {
            writeln!(out, "{key}").with_context(disk)?;
            gone += 1;
        }
    }
    out.into_inner()
        .map_err(|e| e.into_error())
        .with_context(disk)?
        .sync_all()
        .with_context(disk)?;
    std::fs::rename(&tmp, path).with_context(disk)?;
    Ok(gone)
}

/// Rows to write, stored cards to remove, the cards refused, and the last item they reach.
struct Part {
    rows: Vec<Row>,
    gone: Vec<String>,
    rejected: u64,
    last: String,
}
/// The merge thread: items in key order, those after `after`; a part per
/// [`super::store::BATCH_ROWS`] rows.
fn merge(
    runs: Vec<PathBuf>,
    mode: Run,
    after: Option<String>,
    tx: tokio::sync::mpsc::Sender<Part>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut merged = spill::Merge::open(&runs)?;
    let mut shared = Shared::default();
    let empty = |last: String| Part {
        rows: vec![],
        gone: vec![],
        rejected: 0,
        last,
    };
    let mut part = empty(String::new());
    let mut said = Instant::now();
    let mut items = 0u64;
    while let Some(group) = merged.next_group(GROUP_RECORDS, GROUP_BYTES)? {
        items += 1;
        if items.is_multiple_of(4096) {
            stopped(stop)?;
            if said.elapsed() >= PROGRESS_EVERY {
                said = Instant::now();
                tracing::info!(
                    catalogue = "musicbrainz",
                    items,
                    "musicbrainz merge goes on"
                );
            }
        }
        let key = String::from_utf8(group.key).map_err(|_| Failure::new("spill"))?;
        let isrc = key.starts_with("isrc/");
        // Written before the cursor: skipped, but for the ISRCs, which the recordings after it
        // need.
        if !isrc && after.as_ref().is_some_and(|a| key <= *a) {
            continue;
        }
        let mut recs = Vec::with_capacity(group.payloads.len());
        for p in &group.payloads {
            recs.push(postcard::from_bytes::<Rec>(p).map_err(|_| Failure::new("spill"))?);
        }
        let item = Item::of(&key, recs, &group.counts, &mut shared);
        if shared.full() {
            bail!(Failure::new("isrc_shared"));
        }
        if isrc {
            continue;
        }
        let made = item.card.map(|card| card.checked(Catalogue::Musicbrainz));
        match (made, item.stored) {
            (Some(Ok(row)), stored) => {
                if mode == Run::Import || stored.as_deref() != Some(&row.content_hash[..]) {
                    part.rows.push(row);
                }
            }
            (Some(Err(_)), stored) => {
                part.rejected += 1;
                // The item no longer makes a valid card: its stored card goes (the first pass
                // listed only items with no record left).
                if mode == Run::Update && stored.is_some() {
                    part.gone.push(key.clone());
                }
            }
            (None, _) => {}
        }
        part.last = key;
        if part.rows.len() >= super::store::BATCH_ROWS || part.gone.len() >= 10_000 {
            let last = part.last.clone();
            let full = std::mem::replace(&mut part, empty(last));
            tx.blocking_send(full)
                .map_err(|_| Failure::new("stopped"))?;
        }
    }
    if !part.rows.is_empty() || !part.gone.is_empty() || !part.last.is_empty() {
        tx.blocking_send(part)
            .map_err(|_| Failure::new("stopped"))?;
    }
    tracing::debug!(
        catalogue = "musicbrainz",
        shared_isrcs = shared.len(),
        "musicbrainz merge finished"
    );
    Ok(())
}
fn append_removals(path: &Path, ids: &[String]) -> Result<()> {
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
/// An update's removals: the cards `removals` lists, read in parts, those after the first
/// `done_already` (a resumed removal), the count in the cursor `cursor` makes.
async fn remove_gone(
    job: &mut Job<'_>,
    cursor: &(dyn Fn(
        Phase,
        Option<String>,
        u64,
        Option<chrono::DateTime<chrono::Utc>>,
    ) -> serde_json::Result<serde_json::Value>
          + Sync),
    path: &Path,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    done_already: u64,
) -> Result<()> {
    let disk = || Failure::new("disk").at(path.display().to_string());
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => bail!(disk()),
    };
    let mut lines = BufReader::new(file).lines();
    let mut at = 0u64;
    let mut end = false;
    while !end {
        let mut ids = Vec::with_capacity(super::store::BATCH_ROWS);
        while ids.len() < super::store::BATCH_ROWS {
            match lines.next() {
                Some(line) => {
                    let line = line.with_context(disk)?;
                    at += 1;
                    if at > done_already && !line.is_empty() {
                        ids.push(line);
                    }
                }
                None => {
                    end = true;
                    break;
                }
            }
        }
        if ids.is_empty() {
            continue;
        }
        job.remove(&ids, cursor(Phase::Remove, None, at, as_of)?)
            .await?;
    }
    Ok(())
}
/// The lines of a file; none when there is no file.
fn count_lines(path: &Path) -> Result<u64> {
    let disk = || Failure::new("disk").at(path.display().to_string());
    match std::fs::File::open(path) {
        Ok(file) => {
            let mut n = 0;
            for line in BufReader::new(file).lines() {
                line.with_context(disk)?;
                n += 1;
            }
            Ok(n)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(_) => bail!(disk()),
    }
}
/// More than [`MAX_REMOVED_SHARE`] of `cards`; never under a limit (its subset may move).
fn too_many(n: u64, cards: u64, limit: Option<u64>) -> bool {
    limit.is_none() && n as f64 > cards as f64 * MAX_REMOVED_SHARE
}
/// The class of a failure that depends on the dump alone (its files don't match their
/// checksums, don't parse, hold oversized parts or too many shared ISRCs): reading it again
/// changes nothing.
fn dump_failure(error: &anyhow::Error) -> Option<&'static str> {
    let class = error.downcast_ref::<Failure>()?.class;
    matches!(
        class,
        "checksum" | "archive" | "parse" | "size" | "record_size" | "isrc_shared"
    )
    .then_some(class)
}
fn failure_note(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<Failure>()
        .and_then(|f| f.at.clone())
        .unwrap_or_default()
}
/// A stored class name as the fixed text a failure carries.
fn refused_class(class: Option<&str>) -> &'static str {
    [
        "checksum",
        "archive",
        "parse",
        "size",
        "record_size",
        "isrc_shared",
        "removals",
        "rejected",
    ]
    .into_iter()
    .find(|c| Some(*c) == class)
    .unwrap_or("refused")
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
    room: Room,
    sort_memory: usize,
    retry_wait: Duration,
    stop: Arc<AtomicBool>,
    /// Files an earlier attempt read whole.
    kept: BTreeMap<String, FileDone>,
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
                    self.retry_wait,
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

/// Read the dump's files into sorted runs under `plan.dir`: each file not kept into a directory
/// of its own (`<file>-<nonce>`), its manifest written once it is read whole and checked.
fn read_dump(plan: ReadPlan) -> Result<Done> {
    let mut done = Done::default();
    let mut subset = plan.limit.map(|cap| Subset {
        cap,
        ..Subset::default()
    });
    let budget = plan.sort_memory / plan.threads;
    for file in FILES {
        if file == "recording" && subset.is_some() {
            continue;
        }
        let read = match plan.kept.get(file) {
            Some(kept) => kept.clone(),
            None => {
                let read = read_file(&plan, file, subset.as_mut(), budget)?;
                if subset.is_none() {
                    write_file_done(&plan.dir, file, &read)?;
                }
                read
            }
        };
        done.lines += read.lines;
        done.bad_lines += read.bad_lines;
        done.too_long += read.too_long;
        done.records += read.records;
        done.downloaded += read.downloaded;
        done.checked += u32::from(read.checked);
        if let Some(t) = read.as_of {
            done.as_of = Some(done.as_of.map_or(t, |a| a.min(t)));
        }
        done.runs.extend(read.runs);
    }
    Ok(done)
}
/// One file read into sorted runs.
fn read_file(
    plan: &ReadPlan,
    file: &'static str,
    subset: Option<&mut Subset>,
    budget: usize,
) -> Result<FileDone> {
    let name = format!("{file}.tar.xz");
    let at = match &plan.source {
        Source::Mirror(u) => u.host_str().unwrap_or_default().to_owned(),
        Source::Dir(d) => d.join(&name).display().to_string(),
    };
    // A file the dump's list does not name is not read: a mirror lists every file it serves, and
    // a local dump's list, when it has one, every file it holds.
    let want = plan.sums.get(&name).cloned();
    if want.is_none() && (matches!(plan.source, Source::Mirror(_)) || !plan.sums.is_empty()) {
        bail!(Failure::new("checksum").at(at));
    }
    let dir = plan.dir.join(format!("{file}-{}", nonce()));
    let threads = if subset.is_some() { 1 } else { plan.threads };
    let mut spills: Vec<Spill> = (0..threads)
        .map(|i| Spill::new(&dir.join(format!("w{i}")), budget, plan.room))
        .collect::<Result<_>>()?;
    let mut hashed = bulk::Hashed::new(plan.open(&name)?);
    let mut out = FileDone::default();
    let stopped_early;
    {
        let decoded = bulk::xz(
            BufReader::with_capacity(1 << 20, &mut hashed),
            XZ_THREADS,
            XZ_MEMORY,
        )?;
        let mut tar = Tar::new(decoded);
        let wanted = format!("mbdump/{file}");
        loop {
            stopped(&plan.stop)?;
            let Some(entry) = tar.next_entry().map_err(|e| read_failure(e, &at))? else {
                bail!(Failure::new("archive").at(at.clone()));
            };
            if entry.path == "TIMESTAMP" {
                let mut text = String::new();
                tar.data()
                    .take(256)
                    .read_to_string(&mut text)
                    .map_err(|e| read_failure(e.into(), &at))?;
                out.as_of = dump_time(&text);
                continue;
            }
            if entry.path != wanted {
                continue;
            }
            let lines = Lines::new(BufReader::with_capacity(1 << 20, tar.data()), LINE_CAP);
            let read = match subset {
                Some(subset) => read_limited(file, lines, subset, &mut spills[0], &at, &plan.stop)?,
                None => read_parallel(file, lines, &mut spills, &at, &plan.stop)?,
            };
            out.lines = read.lines;
            out.bad_lines = read.bad;
            out.too_long = read.too_long;
            out.records = read.records;
            stopped_early = read.stopped;
            break;
        }
        if !stopped_early {
            // The rest of the archive and of the file, so the checksum covers all of it; an
            // archive holds little after its last entry.
            let mut decoded = tar.into_inner();
            let trailing = std::io::copy(
                &mut (&mut decoded).take(TRAILING_CAP + 1),
                &mut std::io::sink(),
            )
            .map_err(|e| read_failure(e.into(), &at))?;
            if trailing > TRAILING_CAP {
                bail!(Failure::new("archive").at(at.clone()));
            }
            std::io::copy(&mut decoded.into_inner(), &mut std::io::sink())
                .map_err(|e| read_failure(e.into(), &at))?;
        }
    }
    out.downloaded = hashed.count();
    if !stopped_early {
        if let Some(want) = want {
            if want != hashed.hex() {
                bail!(Failure::new("checksum").at(at));
            }
            out.checked = true;
        }
    }
    for spill in spills {
        out.runs.extend(spill.finish()?);
    }
    Ok(out)
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
/// More bad lines than a file may hold so far: more than [`BAD_LINES`], and more than one in a
/// thousand of the lines read.
fn too_bad(bad: u64, lines: u64) -> bool {
    bad > BAD_LINES && bad.saturating_mul(1000) > lines
}
/// The records of one line of `file`, each given to `out` as it is made; `None` for a line that
/// is not one.
fn parse_line(file: &str, line: &[u8], out: &mut dyn FnMut(String, Rec)) -> Option<()> {
    records::line_budget();
    match file {
        "release" => records::release(&serde_json::from_slice(line).ok()?, out),
        "release-group" => {
            let (k, r) = records::group(&serde_json::from_slice(line).ok()?)?;
            out(k, r)
        }
        "artist" => {
            let (k, r) = records::artist(&serde_json::from_slice(line).ok()?)?;
            out(k, r)
        }
        "recording" => records::standalone(&serde_json::from_slice(line).ok()?, out),
        _ => return None,
    }
    Some(())
}
/// [`parse_line`] into a vector (a limited read takes a release's records together, and the
/// tests), at most `cap` records: `None` past it, as for a line that is not one.
fn parse_into(file: &str, line: &[u8], cap: usize) -> Option<Vec<(String, Rec)>> {
    let mut out = vec![];
    let mut over = false;
    parse_line(file, line, &mut |k, r| {
        if out.len() < cap {
            out.push((k, r));
        } else {
            over = true;
        }
    })?;
    (!over).then_some(out)
}
/// The most records a limited read takes of one release.
const LIMITED_RELEASE_RECORDS: usize = 50_000;
fn push_all(spill: &mut Spill, recs: &mut Vec<(String, Rec)>) -> Result<u64> {
    let n = recs.len() as u64;
    for (key, rec) in recs.drain(..) {
        spill.push(key.as_bytes(), &postcard::to_allocvec(&rec)?)?;
    }
    Ok(n)
}
/// One file's lines parsed on `spills.len()` threads, each sorting into its own runs.
fn read_parallel<R: BufRead>(
    file: &'static str,
    mut lines: Lines<R>,
    spills: &mut [Spill],
    at: &str,
    stop: &AtomicBool,
) -> Result<FileRead> {
    let mut read = FileRead::default();
    let (tx, rx) = crossbeam_channel::bounded::<Chunk>(spills.len() * 2);
    // Lines and bad lines over all threads, checked as the read goes.
    let counted = (
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
    );
    let results: Vec<Result<u64>> = std::thread::scope(|scope| {
        let workers: Vec<_> = spills
            .iter_mut()
            .map(|spill| {
                let rx = rx.clone();
                let counted = &counted;
                scope.spawn(move || -> Result<u64> {
                    let mut records = 0;
                    while let Ok(chunk) = rx.recv() {
                        stopped(stop)?;
                        let mut start = 0;
                        let (mut lines, mut bad) = (0, 0);
                        for end in chunk.ends {
                            lines += 1;
                            // Each record goes to the sorter as it is made.
                            let mut failed = None;
                            let parsed = parse_line(file, &chunk.data[start..end], &mut |k, r| {
                                if failed.is_some() {
                                    return;
                                }
                                let pushed = postcard::to_allocvec(&r)
                                    .map_err(anyhow::Error::from)
                                    .and_then(|p| spill.push(k.as_bytes(), &p));
                                match pushed {
                                    Ok(()) => records += 1,
                                    Err(e) => failed = Some(e),
                                }
                            });
                            if let Some(e) = failed {
                                return Err(e);
                            }
                            if parsed.is_none() {
                                bad += 1;
                            }
                            start = end;
                        }
                        let lines = counted.0.fetch_add(lines, Ordering::Relaxed) + lines;
                        let bad = counted.1.fetch_add(bad, Ordering::Relaxed) + bad;
                        if too_bad(bad, lines) {
                            bail!(Failure::new("parse").at(at.to_owned()));
                        }
                    }
                    Ok(records)
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
            let mut fed = 0u64;
            let mut said = Instant::now();
            while lines
                .next_line(&mut line)
                .map_err(|e| read_failure(e.into(), at))?
            {
                fed += 1;
                if said.elapsed() >= PROGRESS_EVERY {
                    said = Instant::now();
                    stopped(stop)?;
                    tracing::info!(
                        catalogue = "musicbrainz",
                        file,
                        lines = fed,
                        "musicbrainz dump read goes on"
                    );
                }
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
        let mut results: Vec<Result<u64>> = workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| Err(Failure::new("panic").into()))
            })
            .collect();
        if let Err(e) = fed {
            results.insert(0, Err(e));
        }
        results
    });
    for result in results {
        read.records += result?;
    }
    read.lines = counted.0.load(Ordering::Relaxed);
    read.bad = counted.1.load(Ordering::Relaxed);
    read.too_long = lines.too_long();
    Ok(read)
}
/// One file's lines in order on this thread, kept to `subset`: releases while their items fit,
/// then only the release groups and artists those name (stopping once all are found).
fn read_limited<R: BufRead>(
    file: &'static str,
    mut lines: Lines<R>,
    subset: &mut Subset,
    spill: &mut Spill,
    at: &str,
    stop: &AtomicBool,
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
    if wanted == 0 {
        read.stopped = true;
        return Ok(read);
    }
    while lines
        .next_line(&mut line)
        .map_err(|e| read_failure(e.into(), at))?
    {
        read.lines += 1;
        if read.lines.is_multiple_of(4096) {
            stopped(stop)?;
        }
        match parse_into(file, &line, LIMITED_RELEASE_RECORDS) {
            Some(parsed) => recs = parsed,
            None => {
                read.bad += 1;
                if too_bad(read.bad, read.lines) {
                    bail!(Failure::new("parse").at(at.to_owned()));
                }
                continue;
            }
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

fn position_version(state: &State) -> String {
    state
        .position
        .as_ref()
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}
fn position_limit(state: &State) -> Option<u64> {
    state
        .position
        .as_ref()
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_u64())
}
fn cursor_of(state: &State) -> Option<Cursor> {
    serde_json::from_value(state.cursor.clone()?).ok()
}
/// The newest dump an update refused (`refused` in the position): not read again.
fn position_refused(state: &State) -> String {
    state
        .position
        .as_ref()
        .and_then(|p| p.get("refused"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

#[async_trait]
impl Importer for MusicBrainz {
    fn catalogue(&self) -> Catalogue {
        Catalogue::Musicbrainz
    }
    /// A read's disk ([`DISK_NEED`], or what a `limit` takes), less what the catalogue's
    /// directory already holds (a cut-off run's runs, which it reuses or removes first); a run
    /// cut off in its removals, or refused, needs none.
    fn disk_need(&self, _run: Run, state: &State) -> u64 {
        if state.run.is_some() {
            if let Some(c) = cursor_of(state) {
                let finished_read = c.phase == Phase::Write
                    && self.work_dir.as_ref().is_some_and(|d| {
                        read_done(&d.join(format!("spill-{}", c.version))).is_some()
                    });
                if matches!(c.phase, Phase::Remove | Phase::Finished) || finished_read {
                    return 0;
                }
            }
        }
        self.need().saturating_sub(self.held())
    }
    fn update_every(&self) -> Duration {
        POLL
    }
    /// A full import when none has finished; else, at most once per [`POLL`], an update when
    /// `LATEST` names a newer dump than the cards are from (and than one it refused), or the
    /// `limit` changed. A `LATEST` that can't be read is asked again at the next poll.
    async fn plan(&self, state: &State) -> Result<Plan> {
        if state.current_at().is_none() {
            return Ok(Plan::Run(Run::Import));
        }
        if position_limit(state) != self.limit {
            return Ok(Plan::Run(Run::Update));
        }
        {
            let mut polled = self.polled.lock().expect("the poll time");
            if let Some(at) = *polled {
                let since = at.elapsed();
                if since < POLL {
                    return Ok(Plan::Wait(POLL - since));
                }
            }
            *polled = Some(Instant::now());
        }
        match self.latest().await {
            Ok(latest) if latest > position_version(state) && latest > position_refused(state) => {
                Ok(Plan::Run(Run::Update))
            }
            Ok(_) => Ok(Plan::Wait(POLL)),
            Err(error) => {
                tracing::info!(catalogue = "musicbrainz", error = %super::failure::shown(&error), "the newest MusicBrainz dump could not be looked up; asked again in an hour");
                Ok(Plan::Wait(POLL))
            }
        }
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        self.read_and_write(job, Run::Import).await
    }
    /// An update; a full import instead when one was asked for, or when the `limit` changed (a
    /// different subset is a new import, not a change of the old one).
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        if job.state().import_requested.is_some() {
            return Ok(UpdateEnd::NeedsImport(REQUESTED));
        }
        if position_limit(job.state()) != job.limit() {
            return Ok(UpdateEnd::NeedsImport("the limit changed"));
        }
        if job.cursor().is_none() {
            let latest = self.latest().await?;
            if latest <= position_version(job.state()) || latest <= position_refused(job.state()) {
                return Ok(UpdateEnd::Done);
            }
        }
        self.read_and_write(job, Run::Update).await?;
        Ok(UpdateEnd::Done)
    }
}
/// The reason a full import asked for by hand is recorded under (`import_requested`).
pub const REQUESTED: &str = "asked for by pinakesd catalogue import --request";

/// The importer `pinakesd` runs: the official mirror; none without the operator's contact
/// (`http.contact`), which every request to the mirror names.
pub fn importer(config: &crate::config::Config) -> Option<Arc<dyn Importer>> {
    if config.egress.contact.is_empty() {
        if !config.catalogues.enabled(Catalogue::Musicbrainz) {
            return None;
        }
        tracing::warn!(
            catalogue = "musicbrainz",
            "the musicbrainz importer starts only with http.contact set (the mirror is asked with the operator's contact)"
        );
        return None;
    }
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

    fn parse_line_vec(file: &str, line: &[u8], out: &mut Vec<(String, Rec)>) -> Option<()> {
        parse_line(file, line, &mut |k, r| out.push((k, r)))
    }

    /// The lists of one line build at most [`records::LINE_ELEMENTS`] entries in all: nested
    /// caps don't multiply (400 tracks of 2,048 relations each would be 819,200).
    #[test]
    fn a_line_builds_a_bounded_number_of_list_entries() {
        let relations = vec![r#"{"target-type":"url"}"#; 2048].join(",");
        let track = format!(
            r#"{{"recording":{{"id":"aeeaa6df-4ab1-4eee-b72f-f21b5c7cbdfe","relations":[{relations}]}}}}"#
        );
        let line = format!(
            r#"{{"id":"a96e1d03-e685-3627-8cba-f5b96be7158f","media":[{{"tracks":[{}]}}]}}"#,
            vec![track; 400].join(",")
        );
        let mut out = vec![];
        parse_line_vec("release", line.as_bytes(), &mut out).unwrap();
        assert_eq!(records::budget_left(), 0);
        // The next line gets its own budget.
        parse_line_vec(
            "artist",
            br#"{"id":"5837d14e-4d18-49cf-a759-e5818714bce4","name":"x"}"#,
            &mut out,
        )
        .unwrap();
        assert!(records::budget_left() > records::LINE_ELEMENTS - 10);
        // A limited read refuses a release of more records than it takes.
        assert!(parse_into("release", line.as_bytes(), 0).is_none());
    }

    /// The counts by kind a group gives for these records.
    fn counts_of(recs: &[Rec]) -> [u64; spill::TAGS] {
        let mut counts = [0; spill::TAGS];
        for r in recs {
            let first = postcard::to_allocvec(r).unwrap()[0];
            counts[usize::from(first).min(spill::TAGS - 1)] += 1;
        }
        counts
    }

    /// The tags the merge counts by are the records' own first bytes.
    #[test]
    fn record_tags_are_their_first_bytes() {
        for (rec, want) in [
            (Rec::Stored(vec![1]), tag::STORED),
            (Rec::Artist(Default::default()), tag::ARTIST),
            (Rec::Group(Default::default()), tag::GROUP),
            (Rec::Credited, tag::CREDITED),
            (
                Rec::Release {
                    release: "r".into(),
                    links: vec![],
                },
                tag::RELEASE,
            ),
            (Rec::Track(Default::default()), tag::TRACK),
            (Rec::Isrc("x".into()), tag::ISRC),
        ] {
            assert_eq!(usize::from(postcard::to_allocvec(&rec).unwrap()[0]), want);
        }
    }

    /// A line's lists are read only up to their caps: a release of 10,000 tracks keeps 4,096,
    /// and a list of `null` or of the wrong type is no list.
    #[test]
    fn long_lists_are_cut_while_they_are_read() {
        let track = r#"{"title":"t","recording":{"id":"aeeaa6df-4ab1-4eee-b72f-f21b5c7cbdfe","title":"t"}}"#;
        let line = format!(
            r#"{{"id":"a96e1d03-e685-3627-8cba-f5b96be7158f","media":[{{"tracks":[{}]}}],"relations":null}}"#,
            vec![track; 10_000].join(",")
        );
        let mut out = vec![];
        parse_line_vec("release", line.as_bytes(), &mut out).unwrap();
        // The same recording on every track: one record and no more.
        assert_eq!(out.len(), 1);
        let parsed: records::ReleaseLine = serde_json::from_str(&line).unwrap();
        let _ = parsed;
        let many_media = format!(
            r#"{{"id":"a96e1d03-e685-3627-8cba-f5b96be7158f","media":[{}]}}"#,
            vec!["{}"; 2000].join(",")
        );
        assert!(parse_line_vec("release", many_media.as_bytes(), &mut out).is_some());
        assert!(parse_line_vec(
            "release",
            br#"{"id":"a96e1d03-e685-3627-8cba-f5b96be7158f","media":7}"#,
            &mut out
        )
        .is_none());
    }

    /// Property: an ISRC's number is one to one over valid ISRCs, and none for anything else.
    #[test]
    fn isrc_numbers_are_one_to_one() {
        let mut seed = 0x5eed_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let mut seen = BTreeMap::new();
        for _ in 0..20_000 {
            let code: String = (0..12)
                .map(|_| CHARS[(next() % 36) as usize] as char)
                .collect();
            let n = records::isrc_number(&code).unwrap();
            if let Some(other) = seen.insert(n, code.clone()) {
                assert_eq!(other, code);
            }
        }
        assert_eq!(
            records::isrc_number("ZZZZZZZZZZZZ"),
            Some(36u64.pow(12) - 1)
        );
        for bad in [
            "",
            "GBUM7102960",
            "GBUM710296041",
            "gbum71029604",
            "GB-M71029604",
        ] {
            assert_eq!(records::isrc_number(bad), None, "{bad}");
        }
    }

    /// Bad lines fail a file once more than 100 and one in a thousand.
    #[test]
    fn a_file_fails_on_too_many_bad_lines() {
        assert!(!too_bad(100, 100));
        assert!(too_bad(101, 101));
        assert!(too_bad(101, 100_000));
        assert!(!too_bad(101, 101_000));
        assert!(!too_bad(u64::MAX / 2, u64::MAX));
    }

    /// A run that resumes needs only the disk its directory does not already hold, a removal or a
    /// refused update none, and a limit's need is within a full read's.
    #[test]
    fn the_disk_a_run_needs() {
        let dir = tempfile::tempdir().unwrap();
        let importer = MusicBrainz::new(Source::official(), "")
            .unwrap()
            .work_dir(dir.path().to_owned());
        let state = |run: Option<&str>, cursor: Option<serde_json::Value>| State {
            catalogue: "musicbrainz".into(),
            phase: "idle".into(),
            run: run.map(str::to_owned),
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
            updated_at: chrono::Utc::now(),
        };
        assert_eq!(
            importer.disk_need(Run::Import, &state(None, None)),
            DISK_NEED
        );
        std::fs::create_dir_all(dir.path().join("spill-20261125-001001/x")).unwrap();
        std::fs::write(
            dir.path().join("spill-20261125-001001/x/run"),
            vec![0; 5000],
        )
        .unwrap();
        assert_eq!(
            importer.disk_need(Run::Update, &state(Some("update"), None)),
            DISK_NEED - 5000
        );
        let removing = serde_json::json!({"version": "20261125-001001", "phase": "remove", "after": null, "removed": 3, "as_of": null});
        assert_eq!(
            importer.disk_need(Run::Update, &state(Some("update"), Some(removing))),
            0
        );
        let finished = serde_json::json!({"version": "20261125-001001", "phase": "finished", "after": null, "removed": 0, "as_of": null});
        assert_eq!(
            importer.disk_need(Run::Import, &state(Some("import"), Some(finished))),
            0
        );
        // A refused import that reads a newer dump needs the disk as any read.
        let refused = serde_json::json!({"version": "20261125-001001", "phase": "refused", "after": null, "removed": 0, "as_of": null});
        assert_eq!(
            importer.disk_need(Run::Import, &state(Some("import"), Some(refused))),
            DISK_NEED - 5000
        );
        let small = MusicBrainz::new(Source::official(), "")
            .unwrap()
            .limit(Some(10));
        assert_eq!(small.disk_need(Run::Import, &state(None, None)), DISK_LEAST);
        let big = MusicBrainz::new(Source::official(), "")
            .unwrap()
            .limit(Some(10_000_000));
        assert_eq!(big.disk_need(Run::Import, &state(None, None)), DISK_NEED);
        // A local dump: four times its files.
        let dump = dir.path().join("20261125-001001");
        std::fs::create_dir(&dump).unwrap();
        for f in FILES {
            // Sparse: its length, no disk.
            std::fs::File::create(dump.join(format!("{f}.tar.xz")))
                .unwrap()
                .set_len(100 << 20)
                .unwrap();
        }
        let local = MusicBrainz::new(Source::Dir(dump), "").unwrap();
        assert_eq!(local.disk_need(Run::Import, &state(None, None)), 1600 << 20);
    }

    #[test]
    fn every_fixture_line_gives_its_records() {
        let mut counts = BTreeMap::new();
        for file in FILES {
            for line in fixture(file) {
                let mut out = vec![];
                parse_line_vec(file, &line, &mut out).unwrap_or_else(|| panic!("{file}"));
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
                ("artist", 4),
                ("credited", 3),
                ("group", 4),
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
            parse_line_vec("release", &line, &mut recs).unwrap();
        }
        let key = "recording/aeeaa6df-4ab1-4eee-b72f-f21b5c7cbdfe";
        let mine: Vec<Rec> = recs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, r)| r.clone())
            .collect();
        assert_eq!(mine.len(), 2);
        let hash = |recs: Vec<Rec>| {
            let counts = counts_of(&recs);
            Item::of(key, recs, &counts, &mut Shared::default())
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
                    if parse_line_vec(file, &l, &mut out).is_none() {
                        bad += 1;
                    }
                    let mut shared = Shared::default();
                    for (key, rec) in out {
                        let counts = counts_of(std::slice::from_ref(&rec));
                        let _ = Item::of(&key, vec![rec], &counts, &mut shared)
                            .card
                            .map(|c| c.checked(Catalogue::Musicbrainz));
                    }
                }
            }
        }
        assert!(bad > 0);
        let mut out = vec![];
        assert!(parse_line_vec("artist", b"not json", &mut out).is_none());
        assert!(parse_line_vec("artist", b"{\"id\":\"not an mbid\"}", &mut out).is_none());
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
        // A local dump is named after its dump.
        let root = tempfile::tempdir().unwrap();
        let named = root.path().join("20261125-001001");
        std::fs::create_dir(&named).unwrap();
        assert!(Source::parse(named.to_str().unwrap()).is_ok());
        assert!(Source::parse(root.path().to_str().unwrap()).is_err());
        assert!(valid_version("20261125-001001"));
        for bad in [
            "20261125001001",
            "2026112-0010010",
            "../../etc/x",
            "",
            "20261125-00100a",
        ] {
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
            parse_line_vec("release", &line, &mut recs).unwrap();
            releases.push(recs);
        }
        // Arovane's album: 4 recordings, its release group and its artist.
        let arovane = releases
            .iter()
            .find(|r| {
                r.iter()
                    .filter(|(k, _)| k.starts_with("recording/"))
                    .count()
                    == 4
            })
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
