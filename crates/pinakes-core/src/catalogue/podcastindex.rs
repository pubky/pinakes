//! The Podcast Index catalogue: one card per live podcast feed, from Podcast Index's weekly
//! database (`podcastindex_feeds.db.tgz`, an SQLite file in a gzipped tar).
//!
//! A run downloads the archive and unpacks its database into the catalogue's directory (5 GB
//! today), reads the feeds that are not dead in ID order, and deletes the file when it is done.
//! The card's link is the feed's URL; its keys are the podcast's GUID (`podcast:guid:`), its
//! Apple Podcasts ID (`podcast:apple:`), and the `url:` keys of its Podcast Index page, of the
//! feed's original URL and of the podcast's own site (only a site exactly one feed names: a
//! hosting platform's page that many feeds name is no one podcast's). Labels are its categories,
//! fame its Podcast Index popularity (0-9). A Podcast Index card backs no link: the feed is
//! fetched for its episodes, and every link of the podcast joins the card's group (`join`).
//!
//! The database is new each week: an update checks the archive's version, and when it is new,
//! reads it whole and writes only the changed cards ([`diff::Sink`]). A database that can't be
//! read as it is (a broken archive, no Podcast Index table), an update that would remove more
//! than a tenth of the cards, and a full import that reads no live feed (the runner's `empty`)
//! are refused ([`Job::refuse`]): the unpacked database is deleted, nothing is removed for it,
//! and that version is not downloaded again; the importer waits for the next week's.
use super::{
    bulk,
    diff::{self, Sink},
    failure::Failure,
    fetch::{self, Input},
    run::{Importer, Job, Run, UpdateEnd},
    store::State,
    Card, Catalogue,
};
use crate::config::Config;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

/// The official database.
pub const DATABASE: &str = "https://public.podcastindex.org/podcastindex_feeds.db.tgz";
/// The database's name in the archive and in the catalogue's directory.
const FILE: &str = "podcastindex_feeds.db";
/// The most bytes read of the archive (1.85 GB today) and of the database in it (5.1 GB).
const ARCHIVE_MAX: u64 = 8 << 30;
const DATABASE_MAX: u64 = 12 << 30;
/// What a run needs free in the catalogue's directory: the unpacked database (5.1 GB today) and
/// room for it to grow. The cards' share of the database is sized apart (it may be on another
/// disk).
const DISK_NEED: u64 = 8 << 30;
/// The most feeds read (4.8M today).
pub const FEEDS_CAP: u64 = 50_000_000;
/// The most distinct site links counted while reading (4.7M feeds name one today); past it,
/// further sites are taken as named once (and logged).
const SHARED_SITES_CAP: usize = 16_000_000;

/// Set when the run that started a blocking download or read is over (finished, failed or cut
/// off): the thread stops at its next megabyte or its next SQLite progress check.
#[derive(Clone, Default)]
struct Stop(Arc<AtomicBool>);
impl Stop {
    fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
/// Sets its [`Stop`] when dropped (the run's future ends or is dropped).
struct StopOnDrop(Stop);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0 .0.store(true, Ordering::Relaxed);
    }
}

/// The importer.
pub struct PodcastIndex {
    client: reqwest::Client,
    input: Input,
    /// The catalogue's directory (`catalogues.dir/podcastindex`): what its unpacked database
    /// already holds counts toward a run's disk need.
    dir: Option<std::path::PathBuf>,
}
impl PodcastIndex {
    pub fn new(input: Input, contact: &str) -> Result<Self> {
        Ok(Self {
            client: fetch::client(contact, &[])?,
            input,
            dir: None,
        })
    }
    /// The catalogue's directory, as the configuration names it.
    pub fn configured(mut self, config: &Config) -> Self {
        self.dir = Some(config.catalogues.work_dir(Catalogue::Podcastindex));
        self
    }
    /// The database to read: a local `.db` file as it is; else the archive, downloaded and
    /// unpacked into `dir` (kept for a resumed run of the same version).
    async fn database(&self, dir: &Path, version: &str, stop: &Stop) -> Result<std::path::PathBuf> {
        if let Input::File(file) = &self.input {
            if file.extension().is_some_and(|e| e == "db") {
                return Ok(file.clone());
            }
        }
        let db = dir.join(FILE);
        let marker = dir.join(format!("{FILE}.version"));
        if tokio::fs::read_to_string(&marker).await.ok().as_deref() == Some(version) && db.exists()
        {
            return Ok(db);
        }
        let _ = tokio::fs::remove_file(&marker).await;
        // A part file of its own: a download cut off (its thread may still be writing) never
        // shares one with the next.
        let part = dir.join(format!(
            "{FILE}.part-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{FILE}.part"))
                {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
        let input = self.input.clone();
        let client = self.client.clone();
        let handle = tokio::runtime::Handle::current();
        let to = part.clone();
        let stop = stop.clone();
        let unpacked = tokio::task::spawn_blocking(move || -> Result<()> {
            let body = input.open(&client, &handle, ARCHIVE_MAX)?;
            // The decompressed stream within the database's cap and the archive's headers.
            unpack(
                flate2::read::GzDecoder::new(body).take(DATABASE_MAX + (1 << 20)),
                &to,
                &input.at(),
                &stop,
            )
        })
        .await
        .map_err(|_| anyhow::Error::from(Failure::new("panic")))
        .and_then(|r| r);
        if let Err(error) = unpacked {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(error);
        }
        tokio::fs::rename(&part, &db).await?;
        tokio::fs::write(&marker, version).await?;
        Ok(db)
    }
    async fn run(
        &self,
        job: &mut Job<'_>,
        version: String,
        at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<()> {
        let resumed_at = job
            .cursor()
            .filter(|c| c.get("version").and_then(Value::as_str) == Some(version.as_str()))
            .and_then(|c| c.get("after"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        // A database refused before is not downloaded again (a full import asked for reads it).
        job.reading(json!(version));
        if job.refused_before(&json!(version)) {
            return Err(job.still_refused(format!("database {version}")));
        }
        // The download's and the read's threads stop when this run is over, however it ends.
        let stop = Stop::default();
        let _stop = StopOnDrop(stop.clone());
        let db = match self.database(job.dir(), &version, &stop).await {
            Ok(db) => db,
            Err(error) => return Err(self.broken(job, &version, error).await),
        };
        let mut sink = Sink::new(job, None);
        // An update reads the whole file again (its removals are the cards it did not read);
        // a full import resumes after the last feed it wrote.
        let after = if job.run() == Run::Import {
            resumed_at
        } else {
            0
        };
        let at_text = self.input.at();
        let path = db.clone();
        let reading = stop.clone();
        let mut parts =
            fetch::batches(move |sink| read(&path, after, &at_text, &reading, FEEDS_CAP, sink));
        while let Some(part) = parts.recv().await {
            let part = match part {
                Ok(part) => part,
                Err(error) => return Err(self.broken(job, &version, error).await),
            };
            let last = part
                .last()
                .and_then(|c| c.source_id.parse::<i64>().ok())
                .unwrap_or(after);
            sink.write(job, part, json!({"version": version, "after": last}))
                .await?;
        }
        let finished = sink
            .finish(job, json!({"version": version, "phase": "removing"}))
            .await?;
        self.cleanup(job.dir()).await;
        if finished == diff::Finished::Refused {
            let note = format!("database {version} would remove more than a tenth of the cards");
            return Err(job.refuse(json!(version), "refused", note));
        }
        job.upstream_version(
            &at.map_or_else(|| version.clone(), |a| a.format("%Y-%m-%d").to_string()),
        )
        .await?;
        job.set_position(json!({"version": version}));
        if let Some(at) = at {
            job.set_current_as_of(at);
        }
        Ok(())
    }
    /// The end of a run whose database can't be read as it is (a broken archive, one over a cap,
    /// a database that is no Podcast Index database: `archive`, `size`, `database`, `schema`):
    /// its file is removed and the version refused ([`Job::refuse`]), so it is not downloaded
    /// again. Any other failure (a download that broke off, the disk) is the run's own, tried
    /// again.
    async fn broken(
        &self,
        job: &mut Job<'_>,
        version: &str,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let class = error
            .downcast_ref::<Failure>()
            .or_else(|| error.chain().find_map(|c| c.downcast_ref::<Failure>()))
            .map(|f| f.class);
        match class {
            Some(class @ ("archive" | "size" | "database" | "schema")) => {
                self.cleanup(job.dir()).await;
                job.refuse(json!(version), class, format!("database {version}"))
            }
            _ => error,
        }
    }
    /// The unpacked database and its marker, once a run has read it.
    async fn cleanup(&self, dir: &Path) {
        if matches!(&self.input, Input::File(f) if f.extension().is_some_and(|e| e == "db")) {
            return;
        }
        let _ = tokio::fs::remove_file(dir.join(FILE)).await;
        let _ = tokio::fs::remove_file(dir.join(format!("{FILE}.version"))).await;
    }
}

#[async_trait]
impl Importer for PodcastIndex {
    fn catalogue(&self) -> Catalogue {
        Catalogue::Podcastindex
    }
    fn source(&self) -> Option<String> {
        diff::source_of(&self.input, DATABASE)
    }
    /// [`DISK_NEED`], less what an unpacked database a cut-off run kept already holds; for a local
    /// database (`--from FILE.db`, a sample), none: nothing is unpacked.
    fn disk_need(&self, _run: Run, _state: &State) -> u64 {
        if matches!(&self.input, Input::File(f) if f.extension().is_some_and(|e| e == "db")) {
            return 0;
        }
        let kept = self
            .dir
            .as_ref()
            .and_then(|d| std::fs::metadata(d.join(FILE)).ok())
            .map_or(0, |m| m.len());
        DISK_NEED.saturating_sub(kept)
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        let (version, at) = self.input.version(&self.client).await?;
        self.run(job, version, at).await
    }
    /// An update; a full import instead when one was asked for (`--request`). The version read
    /// last is not read again: the update finishes without a change; one refused ends it at once
    /// ([`Job::still_refused`]).
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        if job.state().import_requested.is_some() {
            return Ok(UpdateEnd::NeedsImport(super::musicbrainz::REQUESTED));
        }
        let (version, at) = self.input.version(&self.client).await?;
        if job
            .position()
            .and_then(|p| p.get("version"))
            .and_then(Value::as_str)
            == Some(version.as_str())
        {
            return Ok(UpdateEnd::Done);
        }
        self.run(job, version, at).await?;
        Ok(UpdateEnd::Done)
    }
}

/// The importer `pinakesd` runs; none without the operator's contact (`http.contact`).
pub fn importer(config: &Config) -> Option<Arc<dyn Importer>> {
    if config.egress.contact.is_empty() {
        if !config.catalogues.enabled(Catalogue::Podcastindex) {
            return None;
        }
        tracing::warn!(
            catalogue = "podcastindex",
            "the Podcast Index importer starts only with http.contact set (the database is asked for with the operator's contact)"
        );
        return None;
    }
    let input = Input::Url(url::Url::parse(DATABASE).expect("the official URL"));
    match PodcastIndex::new(input, &config.egress.contact) {
        Ok(importer) => Some(Arc::new(importer.configured(config))),
        Err(error) => {
            tracing::warn!(catalogue = "podcastindex", error = %crate::resolver::error_class(&error), "the Podcast Index importer did not start");
            None
        }
    }
}

/// Unpack the database from the archive stream into `to`: its first entry named like the
/// database, within [`DATABASE_MAX`] bytes, whole (an archive that ends inside it is an error).
fn unpack<R: Read>(stream: R, to: &Path, at: &str, stop: &Stop) -> Result<()> {
    let mut tar = bulk::Tar::new(stream);
    while let Some(entry) = tar.next_entry()? {
        if entry.path.rsplit('/').next() != Some(FILE) {
            continue;
        }
        if entry.size > DATABASE_MAX {
            anyhow::bail!(Failure::new("size").at(at.to_owned()));
        }
        let mut file = std::fs::File::create(to)
            .map_err(|_| Failure::new("disk").at(to.display().to_string()))?;
        let mut data = tar.data();
        let mut buffer = vec![0u8; 1 << 20];
        let mut copied = 0u64;
        loop {
            if stop.stopped() {
                anyhow::bail!(Failure::new("stopped").at(at.to_owned()));
            }
            let n = match data.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => anyhow::bail!(Failure::new("download").at(at.to_owned())),
            };
            file.write_all(&buffer[..n])
                .map_err(|_| Failure::new("disk").at(to.display().to_string()))?;
            copied += n as u64;
        }
        if copied != entry.size {
            anyhow::bail!(Failure::new("archive").at(at.to_owned()));
        }
        file.sync_all()?;
        return Ok(());
    }
    anyhow::bail!(Failure::new("archive").at(at.to_owned()))
}

/// The columns a card reads.
const COLUMNS: &str = "id,url,title,link,itunesId,originalUrl,itunesAuthor,language,episodeCount,popularityScore,podcastGuid,description,category1,category2,category3,category4,category5,category6,category7,category8,category9,category10,newestItemPubdate,explicit";
/// What is read of each, texts cut: links to 2,048 bytes, the description to 4,096, the rest to
/// 512.
const SELECT: &str = "id,substr(url,1,2048),substr(title,1,512),substr(link,1,2048),itunesId,substr(originalUrl,1,2048),substr(itunesAuthor,1,512),substr(language,1,64),episodeCount,popularityScore,substr(podcastGuid,1,64),substr(description,1,4096),substr(category1,1,64),substr(category2,1,64),substr(category3,1,64),substr(category4,1,64),substr(category5,1,64),substr(category6,1,64),substr(category7,1,64),substr(category8,1,64),substr(category9,1,64),substr(category10,1,64),newestItemPubdate,explicit";

/// A hostile database file can do little: SQLite in defensive mode, no trust in its schema, and
/// every string or blob at most [`VALUE_CAP`] bytes; a statement stops when the run is over
/// (`stop`) or after [`READ_DEADLINE`].
fn harden(db: &rusqlite::Connection, stop: &Stop) -> rusqlite::Result<()> {
    use rusqlite::{config::DbConfig, limits::Limit};
    db.busy_timeout(Duration::from_secs(5))?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    db.set_limit(Limit::SQLITE_LIMIT_LENGTH, VALUE_CAP);
    db.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 100_000);
    db.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 100);
    db.pragma_update(None, "cell_size_check", true)?;
    let stop = stop.clone();
    let until = std::time::Instant::now() + READ_DEADLINE;
    db.progress_handler(
        PROGRESS_STEPS,
        Some(move || stop.stopped() || std::time::Instant::now() > until),
    );
    Ok(())
}
/// The longest string or blob SQLite gives (bytes): well above any real row's (each text read
/// is cut by `substr`), so no real row fails the run; it bounds what one value can hold.
const VALUE_CAP: i32 = 64 << 20;
/// The longest a database's reading may take (both passes; today's takes minutes): a crafted
/// file whose pages loop can't keep the thread for ever.
const READ_DEADLINE: Duration = Duration::from_secs(6 * 3600);
/// SQLite steps between two checks of the stop flag and the deadline.
const PROGRESS_STEPS: i32 = 100_000;
/// Read the live feeds after `after` (by ID) from the database at `path` as cards, in parts.
/// Each of its two passes reads at most `cap` rows ([`FEEDS_CAP`]).
fn read(
    path: &Path,
    after: i64,
    at: &str,
    stop: &Stop,
    cap: u64,
    sink: &mut dyn FnMut(Vec<Card>) -> bool,
) -> Result<()> {
    use rusqlite::{OpenFlags, OptionalExtension};
    let failure = || Failure::new("database").at(at.to_owned());
    let db = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| failure())?;
    harden(&db, stop).map_err(|_| failure())?;
    // The table must be there with every column read.
    let found: Option<String> = db
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='podcasts'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| failure())?;
    let found = found.ok_or_else(failure)?;
    // A plain table: no virtual table, no generated column, and no view or trigger in the file.
    let upper = found.to_ascii_uppercase();
    let others: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type IN ('view','trigger')",
            [],
            |r| r.get(0),
        )
        .map_err(|_| failure())?;
    if !COLUMNS.split(',').all(|c| found.contains(c))
        || upper.contains("VIRTUAL")
        || upper.contains("GENERATED")
        || upper.contains(" AS (")
        || others > 0
    {
        anyhow::bail!(Failure::new("schema").at(at.to_owned()));
    }
    if stop.stopped() {
        anyhow::bail!(Failure::new("stopped").at(at.to_owned()));
    }
    // Site links several feeds name, by their `url:` key (two spellings of one link are one).
    let mut shared = std::collections::HashSet::new();
    {
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut q = db
            .prepare("SELECT substr(link,1,2048) FROM podcasts WHERE dead=0 AND link<>''")
            .map_err(|_| failure())?;
        let mut rows = q.query([]).map_err(|_| failure())?;
        let mut counted = 0u64;
        while let Some(row) = rows.next().map_err(|_| failure())? {
            // The first pass's rows count against the same cap as the second's.
            counted += 1;
            if counted > cap {
                anyhow::bail!(Failure::new("size").at(at.to_owned()));
            }
            if stop.stopped() {
                anyhow::bail!(Failure::new("stopped").at(at.to_owned()));
            }
            let Some(key) = row
                .get::<_, String>(0)
                .ok()
                .as_deref()
                .and_then(web)
                .and_then(|l| super::link_key(&l))
            else {
                continue;
            };
            let hash = key_hash(&key);
            if seen.contains(&hash) {
                shared.insert(hash);
            } else if seen.len() < SHARED_SITES_CAP {
                seen.insert(hash);
            } else if seen.len() == SHARED_SITES_CAP {
                tracing::warn!(
                    catalogue = "podcastindex",
                    cap = SHARED_SITES_CAP,
                    "more site links than counted: the rest are taken as named by one feed"
                );
                seen.insert(hash);
            }
        }
    }
    let mut q = db
        .prepare(&format!(
            "SELECT {SELECT} FROM podcasts WHERE dead=0 AND id>?1 ORDER BY id"
        ))
        .map_err(|_| failure())?;
    let mut rows = q.query([after]).map_err(|_| failure())?;
    let mut part = Vec::with_capacity(fetch::PART);
    let mut read = 0u64;
    while let Some(row) = rows.next().map_err(|_| failure())? {
        read += 1;
        if read > cap {
            anyhow::bail!(Failure::new("size").at(at.to_owned()));
        }
        if stop.stopped() {
            anyhow::bail!(Failure::new("stopped").at(at.to_owned()));
        }
        let text = |i: usize| -> String {
            row.get::<_, Option<String>>(i)
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        let int = |i: usize| -> i64 { row.get::<_, Option<i64>>(i).ok().flatten().unwrap_or(0) };
        let feed = Feed {
            id: int(0),
            url: text(1),
            title: text(2),
            link: text(3),
            itunes_id: int(4),
            original_url: text(5),
            author: text(6),
            language: text(7),
            episodes: int(8),
            popularity: int(9),
            guid: text(10),
            description: text(11),
            categories: (12..22).map(text).filter(|c| !c.is_empty()).collect(),
            newest: int(22),
            explicit: int(23) != 0,
        };
        let named_by_several = |link: &str| {
            web(link)
                .and_then(|l| super::link_key(&l))
                .is_some_and(|key| shared.contains(&key_hash(&key)))
        };
        if let Some(card) = card(&feed, named_by_several) {
            part.push(card);
        }
        if part.len() >= fetch::PART && !sink(std::mem::take(&mut part)) {
            return Ok(());
        }
    }
    if !part.is_empty() {
        sink(part);
    }
    Ok(())
}

/// A 64-bit hash of a link key (the shared-site set holds these, not the links).
fn key_hash(key: &str) -> u64 {
    u64::from_le_bytes(
        blake3::hash(key.as_bytes()).as_bytes()[..8]
            .try_into()
            .expect("eight bytes"),
    )
}
/// One feed's row.
#[derive(Clone, Debug, Default)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub link: String,
    pub itunes_id: i64,
    pub original_url: String,
    pub author: String,
    pub language: String,
    pub episodes: i64,
    pub popularity: i64,
    pub guid: String,
    pub description: String,
    pub categories: Vec<String>,
    /// Unix time of its newest episode.
    pub newest: i64,
    pub explicit: bool,
}
fn web(link: &str) -> Option<String> {
    let link = link.trim();
    let url = url::Url::parse(link).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && link.len() <= 2048
        && !crate::capability::is_capability(link))
    .then(|| link.to_owned())
}
/// Hosts (and their subdomains) that serve paid subscribers' private feeds: a feed there is
/// someone's subscription, never imported.
pub const PRIVATE_FEED_HOSTS: &[&str] = &[
    "patreon.com",
    "supercast.com",
    "supercast.tech",
    "supportingcast.fm",
    "glow.fm",
    "memberful.com",
    "steadyhq.com",
];
/// Whether a feed URL is a private feed: on a host of [`PRIVATE_FEED_HOSTS`], or with a query
/// key that carries a credential (`token`, `key`, `auth`, `access`, `secret`, `sig`,
/// `signature`, `password`, and names ending in one, such as `access_token` or `api_key`; or a
/// `uid` with a `hash`). A subscriber's token in a public index still opens their subscription.
pub fn private_feed(link: &str) -> bool {
    let Ok(url) = url::Url::parse(link.trim()) else {
        return false;
    };
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if PRIVATE_FEED_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    {
        return true;
    }
    let keys: Vec<String> = url
        .query_pairs()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    let credential = |k: &str| {
        const WORDS: [&str; 8] = [
            "token",
            "key",
            "auth",
            "access",
            "secret",
            "sig",
            "signature",
            "password",
        ];
        WORDS
            .iter()
            .any(|w| k == *w || k.ends_with(&format!("_{w}")) || k.ends_with(&format!("-{w}")))
            || k.ends_with("token")
            || k.ends_with("apikey")
            // `auth…` (`authkey`, `authorization`), not a feed's `author`.
            || (k.starts_with("auth") && !k.starts_with("author"))
            || k == "authorization"
    };
    keys.iter().any(|k| credential(k))
        || (keys.iter().any(|k| k == "uid") && keys.iter().any(|k| k == "hash"))
}
/// The card of a feed; `shared` says whether a site link is named by several feeds. A private
/// feed ([`private_feed`]) or one without a title makes none.
pub fn card(feed: &Feed, shared: impl Fn(&str) -> bool) -> Option<Card> {
    // A feed without a usable title has no name to show (the table refuses such a card).
    if feed.id <= 0 || private_feed(&feed.url) || !super::usable_name(&feed.title) {
        return None;
    }
    let uri = web(&feed.url)?;
    let mut keys = Vec::new();
    if let Some(guid) = crate::also_on::media::guid(&feed.guid) {
        keys.push(format!("podcast:guid:{guid}"));
    }
    if feed.itunes_id > 0 {
        keys.push(format!("podcast:apple:{}", feed.itunes_id));
    }
    keys.extend(super::link_key(&format!(
        "https://podcastindex.org/podcast/{}",
        feed.id
    )));
    if let Some(original) = web(&feed.original_url).filter(|o| *o != uri) {
        keys.extend(super::link_key(&original));
    }
    let site = web(&feed.link).filter(|l| *l != uri && !shared(&feed.link));
    if let Some(site) = &site {
        keys.extend(super::link_key(site));
    }
    let mut details = serde_json::Map::new();
    let short = |t: &str, cap: usize| {
        let mut t = t.trim().to_owned();
        crate::model::truncate(&mut t, cap);
        t
    };
    if !feed.author.trim().is_empty() {
        details.insert("author".into(), json!(short(&feed.author, 256)));
    }
    if !feed.language.trim().is_empty() && feed.language.len() <= 32 {
        details.insert("language".into(), json!(feed.language.trim()));
    }
    if let Some(site) = web(&feed.link).filter(|l| l.len() <= 2048) {
        details.insert("website".into(), json!(site));
    }
    if feed.explicit {
        details.insert("explicit".into(), json!(true));
    }
    Some(Card {
        source_id: feed.id.to_string(),
        uri,
        r#type: "PodcastSeries".into(),
        name: feed.title.clone(),
        description: Some(feed.description.clone()).filter(|d| !d.trim().is_empty()),
        labels: feed.categories.clone(),
        keys,
        fame: (feed.popularity.clamp(0, 9) as f32) / 9.0,
        details,
        ..Card::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn feed() -> Feed {
        Feed {
            id: 920666,
            url: "https://feeds.simplecast.com/54nAGcIl".into(),
            title: "The Daily".into(),
            link: "https://www.nytimes.com/the-daily".into(),
            itunes_id: 1200361736,
            original_url: "http://rss.art19.com/the-daily".into(),
            author: "The New York Times".into(),
            language: "en".into(),
            episodes: 2700,
            popularity: 9,
            guid: "AEAD4C23-6BF5-58C6-A2C6-A6B28D128CB6".into(),
            description: "This is what the news should sound like.".into(),
            categories: vec!["news".into(), "daily".into()],
            newest: 1_795_400_000,
            explicit: false,
        }
    }

    #[test]
    fn a_feed_becomes_a_card_with_its_podcast_keys() {
        let made = card(&feed(), |_| false).unwrap();
        let row = made.checked(Catalogue::Podcastindex).unwrap();
        assert_eq!(row.uri, "https://feeds.simplecast.com/54nAGcIl");
        assert_eq!(row.r#type, "PodcastSeries");
        assert_eq!(row.fame, 1.0);
        assert_eq!(row.labels, ["news", "daily"]);
        assert!(
            row.keys
                .contains(&"podcast:guid:aead4c23-6bf5-58c6-a2c6-a6b28d128cb6".to_owned()),
            "{:?}",
            row.keys
        );
        assert!(row.keys.contains(&"podcast:apple:1200361736".to_owned()));
        for link in [
            "https://podcastindex.org/podcast/920666",
            "http://rss.art19.com/the-daily",
            "https://www.nytimes.com/the-daily",
        ] {
            let key = super::super::link_key(link).unwrap();
            assert!(row.keys.contains(&key), "{link}");
        }
        // A site several feeds name is no key of this one.
        let shared = card(&feed(), |l| l.contains("nytimes"))
            .unwrap()
            .checked(Catalogue::Podcastindex)
            .unwrap();
        assert!(!shared
            .keys
            .contains(&super::super::link_key("https://www.nytimes.com/the-daily").unwrap()));
        // No link, no card; a feed with no GUID or Apple ID keeps its other keys.
        let mut bare = feed();
        bare.url = "not a link".into();
        assert!(card(&bare, |_| false).is_none());
        let mut bare = feed();
        bare.guid = "".into();
        bare.itunes_id = 0;
        let row = card(&bare, |_| false)
            .unwrap()
            .checked(Catalogue::Podcastindex)
            .unwrap();
        assert!(row.keys.iter().all(|k| k.starts_with("url:")));
    }

    /// The fixture database (real rows of Podcast Index's database) reads as cards in ID order,
    /// from after a given ID.
    #[test]
    fn the_database_reads_live_feeds_in_id_order() {
        let path = std::path::PathBuf::from(format!(
            "{}/tests/fixtures/catalogue/podcastindex/podcastindex_feeds.db",
            env!("CARGO_MANIFEST_DIR")
        ));
        let mut cards = Vec::new();
        read(&path, 0, "test", &Stop::default(), FEEDS_CAP, &mut |part| {
            cards.extend(part);
            true
        })
        .unwrap();
        assert!(cards.len() >= 5, "{}", cards.len());
        let ids: Vec<i64> = cards.iter().map(|c| c.source_id.parse().unwrap()).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        for card in &cards {
            card.checked(Catalogue::Podcastindex).unwrap();
        }
        let mut later = Vec::new();
        read(
            &path,
            ids[1],
            "test",
            &Stop::default(),
            FEEDS_CAP,
            &mut |part| {
                later.extend(part);
                true
            },
        )
        .unwrap();
        assert_eq!(later.len(), cards.len() - 2);
        // Not a database, or one without the table: a failure, never a panic.
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"not sqlite at all").unwrap();
        assert!(read(&junk, 0, "t", &Stop::default(), FEEDS_CAP, &mut |_| true).is_err());
    }

    /// A dead feed makes no card; a site two feeds name (in any spelling) is neither's key; a
    /// view in the file is refused.
    #[test]
    fn dead_feeds_and_shared_sites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        std::fs::copy(
            format!(
                "{}/tests/fixtures/catalogue/podcastindex/podcastindex_feeds.db",
                env!("CARGO_MANIFEST_DIR")
            ),
            &path,
        )
        .unwrap();
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute("UPDATE podcasts SET dead=1 WHERE id=1", [])
                .unwrap();
            c.execute(
                "UPDATE podcasts SET link='https://shared.example/' WHERE id=2",
                [],
            )
            .unwrap();
            c.execute(
                "UPDATE podcasts SET link='https://SHARED.example' WHERE id=4",
                [],
            )
            .unwrap();
        }
        let mut cards = Vec::new();
        read(&path, 0, "t", &Stop::default(), FEEDS_CAP, &mut |part| {
            cards.extend(part);
            true
        })
        .unwrap();
        assert!(cards.iter().all(|c| c.source_id != "1"), "a dead feed");
        let shared = super::super::link_key("https://shared.example/").unwrap();
        for id in ["2", "4"] {
            let card = cards.iter().find(|c| c.source_id == id).unwrap();
            assert!(!card.keys.contains(&shared), "{id}: {:?}", card.keys);
        }
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute("CREATE VIEW v AS SELECT 1", []).unwrap();
        }
        let failed = read(&path, 0, "t", &Stop::default(), FEEDS_CAP, &mut |_| true).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "schema");
    }

    /// Private feeds make no card: on a private-feed host, or with a credential in the query;
    /// a public feed with an ordinary query does.
    /// A feed whose title carries a credential (the table refuses such a name) makes no card.
    #[test]
    fn a_title_with_a_credential_makes_no_card() {
        let feed = |title: &str| Feed {
            id: 7,
            url: "https://feeds.example.org/show.xml".into(),
            title: title.into(),
            ..Feed::default()
        };
        assert!(card(&feed("//:a@&A"), |_| false).is_none());
        assert!(card(&feed("https://user:secret@example.org/"), |_| false).is_none());
        assert!(card(&feed("A show"), |_| false).is_some());
    }

    #[test]
    fn private_feeds_are_never_imported() {
        let feed = |url: &str| Feed {
            id: 7,
            url: url.into(),
            title: "A show".into(),
            ..Feed::default()
        };
        for private in [
            "https://www.patreon.com/rss/someone?auth=abc",
            "https://www.patreon.com/rss/someone",
            "https://show.supercast.com/feeds/Zm9vYmFy",
            "https://feeds.supportingcast.fm/a.rss",
            "https://glow.fm/rss/1/2/",
            "https://example.org/feed.xml?token=abc123",
            "https://example.org/feed.xml?access_token=abc",
            "https://example.org/feed.xml?api_key=abc",
            "https://example.org/feed.xml?apiKey=abc",
            "https://example.org/feed.xml?Signature=abc&Expires=1",
            "https://example.org/feed.xml?uid=12&hash=ab12",
            "https://example.org/feed.xml?password=x",
            "https://example.org/feed.xml?authkey=x",
            "https://example.org/feed.xml?Authorization=x",
        ] {
            assert!(private_feed(private), "{private}");
            assert!(card(&feed(private), |_| false).is_none(), "{private}");
        }
        for public in [
            "https://feeds.example.org/show.xml",
            "https://example.org/feed.xml?format=xml&page=2",
            "https://example.org/feed.xml?uid=12",
            "https://example.org/?feed=podcast&author=3",
            "https://anchor.fm/s/abc/podcast/rss",
        ] {
            assert!(!private_feed(public), "{public}");
            assert!(card(&feed(public), |_| false).is_some(), "{public}");
        }
    }

    #[test]
    fn the_archive_is_unpacked_whole_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("x.db");
        let body = b"SQLite format 3\0rest";
        let archive = super::super::software::tests::tar(&[("./podcastindex_feeds.db", body)]);
        unpack(&archive[..], &to, "t", &Stop::default()).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), body);
        // Cut inside the database's bytes (the header is 512).
        assert!(unpack(&archive[..520], &to, "t", &Stop::default()).is_err());
        let other = super::super::software::tests::tar(&[("./other.db", body)]);
        assert!(unpack(&other[..], &to, "t", &Stop::default()).is_err());
        // A run that is over stops the copy.
        let stop = Stop::default();
        drop(StopOnDrop(stop.clone()));
        let failed = unpack(&archive[..], &to, "t", &stop).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "stopped");
    }

    /// Both passes count their rows against the cap: a resumed read whose second pass reads few
    /// rows still stops on the first pass's; a run that is over stops the read.
    #[test]
    fn the_reads_are_capped_and_stop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        std::fs::copy(
            format!(
                "{}/tests/fixtures/catalogue/podcastindex/podcastindex_feeds.db",
                env!("CARGO_MANIFEST_DIR")
            ),
            &path,
        )
        .unwrap();
        let rows: i64 = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM podcasts WHERE dead=0 AND link<>''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(rows > 3, "{rows}");
        let last: i64 = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("SELECT max(id) FROM podcasts", [], |r| r.get(0))
            .unwrap();
        let cap = rows as u64 - 1;
        // From the last ID on, the second pass reads nothing: the first pass's rows decide.
        let failed = read(&path, last, "t", &Stop::default(), cap, &mut |_| true).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "size");
        assert!(
            read(&path, last, "t", &Stop::default(), rows as u64, &mut |_| {
                true
            })
            .is_ok()
        );
        let stop = Stop::default();
        drop(StopOnDrop(stop.clone()));
        let failed = read(&path, 0, "t", &stop, FEEDS_CAP, &mut |_| true).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "stopped");
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]
        /// Property: any row gives no card or a card the table takes, whose link is the feed's.
        #[test]
        fn any_row_gives_a_valid_card_or_none(url in "(https?://[a-z]{1,8}\\.[a-z]{2,3}/[ -~]{0,30})|[ -~]{0,20}",
            title in "\\PC{0,80}", guid in "[0-9a-fA-F-]{0,40}", link in "[ -~]{0,40}", cats in proptest::collection::vec("\\PC{0,25}", 0..10),
            popularity in -5i64..40, id in -2i64..1_000_000_000) {
            let feed = Feed { id, url: url.clone(), title, guid, link, categories: cats, popularity, ..Feed::default() };
            if let Some(card) = card(&feed, |_| false) {
                let checked = card.checked(Catalogue::Podcastindex);
                prop_assert!(checked.is_ok(), "{:?}: {:?}", card, checked.as_ref().err());
                let row = checked.unwrap();
                prop_assert!((0.0..=1.0).contains(&row.fame));
                prop_assert!(row.labels.iter().all(|l| super::super::label(l).as_deref() == Some(l.as_str())));
                prop_assert!(row.keys.iter().all(|k| super::super::valid_key(k)));
            }
        }
        /// Fuzz: any bytes as an archive unpack to nothing or fail, never panic.
        #[test]
        fn any_archive_bytes_end_without_a_panic(bytes in proptest::collection::vec(any::<u8>(), 0..3000)) {
            let dir = tempfile::tempdir().unwrap();
            let _ = unpack(&bytes[..], &dir.path().join("x.db"), "t", &Stop::default());
        }
    }
}
