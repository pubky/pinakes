//! The software catalogue: every crate on crates.io, every app on F-Droid and every application
//! on Flathub, one card each, from each registry's own catalogue file:
//!
//! - crates.io's daily database dump (`db-dump.tar.gz`), read as a stream up to its
//!   `crates_keywords.csv` (the version tables after it are never read): crates, their
//!   categories, keywords and downloads;
//! - F-Droid's `index-v2.json`, as its `entry.json` names it (its SHA-256 and size are checked
//!   against the entry before a card is written);
//! - Flathub's AppStream catalogue (`appstream.xml.gz`), desktop and console applications only.
//!
//! A card's link is its package page (`crates.io/crates/{name}`, `f-droid.org/packages/{id}`,
//! `flathub.org/apps/{id}`), its keys the ones `join::link_keys` reads from that page, so a shared
//! package page joins its card; the repository and the homepage are related links. Labels come
//! from categories and keywords. Fame: a crate's downloads; the app catalogues name no install
//! count, so their fame is 0.
//!
//! Each registry is read on its own: an update reads only a registry whose file changed, writes
//! the changed cards and removes that registry's cards no longer listed ([`diff::Sink`]).
use super::{
    bulk,
    csv::{self, Record},
    diff::{self, Sink},
    failure::Failure,
    fetch::{self, Input},
    run::{self, Importer, Job, Run, UpdateEnd},
    store::State,
    Card, Catalogue,
};
use crate::config::Config;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Read},
    sync::Arc,
};

/// One registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Registry {
    Crates,
    FDroid,
    Flathub,
}
impl Registry {
    pub const ALL: [Registry; 3] = [Registry::Crates, Registry::FDroid, Registry::Flathub];
    /// Its `catalogue_card.source`.
    pub fn source(self) -> &'static str {
        match self {
            Registry::Crates => "crates.io",
            Registry::FDroid => "f-droid",
            Registry::Flathub => "flathub",
        }
    }
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.source() == name)
    }
}
/// The official files.
pub const CRATES_DUMP: &str = "https://static.crates.io/db-dump.tar.gz";
pub const FDROID_ENTRY: &str = "https://f-droid.org/repo/entry.json";
pub const FLATHUB_APPSTREAM: &str = "https://dl.flathub.org/repo/appstream/x86_64/appstream.xml.gz";
/// The hosts a registry's file may redirect to (crates.io's dump is served by its CDN).
const REDIRECT_HOSTS: &[&str] = &["static.crates.io", "cloudfront-static.crates.io"];

/// The most bytes read of each file: the crates.io dump (2.3 GB today, of which the part read
/// is under a gigabyte), F-Droid's index (75 MB) and its entry, Flathub's catalogue (11 MB).
const CRATES_MAX: u64 = 16 << 30;
const FDROID_INDEX_MAX: u64 = 512 << 20;
const FDROID_ENTRY_MAX: u64 = 1 << 20;
const FLATHUB_MAX: u64 = 256 << 20;
/// The most decompressed bytes read of Flathub's catalogue (52 MB today).
const FLATHUB_XML_MAX: u64 = 512 << 20;
/// The most packages one registry may list (crates.io: 0.4M today).
pub const PACKAGES_CAP: usize = 2_000_000;
/// The most bytes the crates.io dump's rows and maps (downloads, categories, keywords and the
/// crates' categories and keywords, each entry counted with its text) may hold while it is read
/// (about 150 MB today); past it the run fails (`size`).
const CRATES_BYTES_CAP: usize = 1 << 30;
/// The most decompressed bytes of the crates.io dump read (about 1.6 GB are read today).
const CRATES_TAR_MAX: u64 = 16 << 30;

/// Where each registry's file comes from.
#[derive(Clone, Debug)]
pub struct Sources {
    pub crates: Input,
    pub fdroid: Input,
    pub flathub: Input,
}
impl Sources {
    pub fn official() -> Self {
        let url = |u: &str| Input::Url(url::Url::parse(u).expect("an official URL"));
        Self {
            crates: url(CRATES_DUMP),
            fdroid: url(FDROID_ENTRY),
            flathub: url(FLATHUB_APPSTREAM),
        }
    }
    fn of(&self, registry: Registry) -> &Input {
        match registry {
            Registry::Crates => &self.crates,
            Registry::FDroid => &self.fdroid,
            Registry::Flathub => &self.flathub,
        }
    }
}

/// The importer.
pub struct Software {
    client: reqwest::Client,
    sources: Sources,
}
impl Software {
    pub fn new(sources: Sources, contact: &str) -> Result<Self> {
        Ok(Self {
            client: fetch::client(contact, REDIRECT_HOSTS)?,
            sources,
        })
    }
    /// The version of a registry's file, and the time it names.
    async fn version(
        &self,
        registry: Registry,
    ) -> Result<(String, Option<chrono::DateTime<chrono::Utc>>)> {
        let input = self.sources.of(registry);
        match registry {
            // The entry names the index's own time and digest.
            Registry::FDroid => {
                let entry = fdroid_entry(&input.read_small(&self.client, FDROID_ENTRY_MAX).await?)
                    .ok_or_else(|| Failure::new("parse").at(input.at()))?;
                let at = chrono::DateTime::from_timestamp_millis(entry.timestamp)
                    .map(|at| at.min(chrono::Utc::now()));
                Ok((format!("{}:{}", entry.timestamp, entry.index.sha256), at))
            }
            _ => input.version(&self.client).await,
        }
    }
    /// Read one registry's cards into `sink`; how many it read.
    async fn read(
        &self,
        job: &mut Job<'_>,
        sink: &mut Sink,
        registry: Registry,
        cursor: &Value,
    ) -> Result<u64> {
        let input = self.sources.of(registry).clone();
        let client = self.client.clone();
        let handle = tokio::runtime::Handle::current();
        let mut parts = match registry {
            Registry::Crates => fetch::batches(move |sink| {
                let body = input.open(&client, &handle, CRATES_MAX)?;
                crates(
                    flate2::read::GzDecoder::new(body).take(CRATES_TAR_MAX),
                    &input.at(),
                    sink,
                )
            }),
            Registry::FDroid => {
                let entry = fdroid_entry(&input.read_small(&client, FDROID_ENTRY_MAX).await?)
                    .ok_or_else(|| Failure::new("parse").at(input.at()))?;
                let index = input.join(entry.index.name.trim_start_matches('/'))?;
                // Next to the entry, on its host.
                if let (Input::Url(from), Input::Url(to)) = (&input, &index) {
                    let parent =
                        |u: &url::Url| u.path().rsplit_once('/').map(|(dir, _)| dir.to_owned());
                    if from.scheme() != to.scheme()
                        || from.host_str() != to.host_str()
                        || from.port() != to.port()
                        || parent(from) != parent(to)
                    {
                        anyhow::bail!(Failure::new("manifest").at(input.at()));
                    }
                }
                fetch::batches(move |sink| {
                    let body = index.open(
                        &client,
                        &handle,
                        FDROID_INDEX_MAX.min(entry.index.size.saturating_add(1)),
                    )?;
                    fdroid(body, &entry, &index.at(), sink)
                })
            }
            Registry::Flathub => fetch::batches(move |sink| {
                let body = input.open(&client, &handle, FLATHUB_MAX)?;
                let xml = flate2::read::GzDecoder::new(body).take(FLATHUB_XML_MAX);
                flathub(BufReader::new(xml), &input.at(), sink)
            }),
        };
        let mut cards = 0u64;
        while let Some(part) = parts.recv().await {
            let part = part?;
            cards += part.len() as u64;
            sink.write(job, part, cursor.clone()).await?;
        }
        Ok(cards)
    }
    /// The registries done in this run (`cursor.done`: registry -> its version and time).
    fn done(job: &Job<'_>) -> BTreeMap<String, Value> {
        job.cursor()
            .and_then(|c| c.get("done"))
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .unwrap_or_default()
    }
    /// The run: each registry whose file is new (every one in a full import) and not read in this
    /// run already is read. A registry's file that would remove more than a tenth of its cards in
    /// an update (`refused`), or that makes no card in a full import (`empty`), is refused and
    /// the others are read; the run then names the files refused in its position
    /// ([`run::REFUSED`]: registry -> version) and is abandoned ([`Job::refuse`], recorded once).
    /// The next updates read that registry again only when it has a newer file; a full import of
    /// the importer's own waits while one of them is still the newest ([`Job::still_refused`]);
    /// one asked for reads every one.
    async fn run(&self, job: &mut Job<'_>) -> Result<()> {
        let import = job.run() == Run::Import;
        let mut done = Self::done(job);
        let mut position: BTreeMap<String, Value> = job
            .position()
            .and_then(|p| serde_json::from_value(p.clone()).ok())
            .unwrap_or_default();
        for key in run::REFUSAL_KEYS {
            position.remove(key);
        }
        // The files refused before, by registry.
        let standing: BTreeMap<String, Value> = job
            .refused()
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default();
        // A full import must read every registry: while a file refused before is still the
        // newest, one of the importer's own reads nothing.
        if import && !job.asked() {
            for (name, version) in &standing {
                let Some(registry) = Registry::parse(name) else {
                    continue;
                };
                let (newest, _) = self.version(registry).await?;
                if version.as_str() == Some(newest.as_str()) {
                    return Err(job.still_refused(format!("the {name} file {newest}")));
                }
            }
        }
        // The files this run refused, and those of them that made no card (kept in its cursor,
        // so a run taken up after another registry failed does not read them again).
        let from_cursor = |key: &str| job.cursor().and_then(|c| c.get(key)).cloned();
        let mut refused: BTreeMap<String, Value> = from_cursor("refused")
            .and_then(|d| serde_json::from_value(d).ok())
            .unwrap_or_default();
        let mut empty: Vec<String> = from_cursor("empty")
            .and_then(|d| serde_json::from_value(d).ok())
            .unwrap_or_default();
        // A registry that fails leaves the others to be read; the run then fails with the first
        // failure and is taken up again from its cursor (the registries done are not read again).
        let mut failed: Option<anyhow::Error> = None;
        let cursor = |done: &BTreeMap<String, Value>,
                      refused: &BTreeMap<String, Value>,
                      empty: &[String],
                      reading: Option<&str>| {
            let mut c = json!({"done": done, "refused": refused, "empty": empty});
            if let Some(name) = reading {
                c["reading"] = json!(name);
            }
            c
        };
        for registry in Registry::ALL {
            let name = registry.source();
            let ran: Result<()> = async {
                let (version, at) = self.version(registry).await?;
                let entry = json!({"version": version, "at": at});
                let same = |v: Option<&Value>| {
                    v.and_then(|v| v.get("version").or(Some(v)))
                        .and_then(Value::as_str)
                        == Some(version.as_str())
                };
                // Done in this run (a run taken up again): read again only when its file
                // changed since.
                if same(done.get(name)) || same(refused.get(name)) {
                    return Ok(());
                }
                if !import {
                    if same(position.get(name)) {
                        done.insert(name.to_owned(), position[name].clone());
                        return Ok(());
                    }
                    if same(standing.get(name)) {
                        tracing::debug!(catalogue = "software", registry = name, "this registry's file was refused before: it waits for a newer one, or a full import (--request)");
                        return Ok(());
                    }
                }
                let reading = cursor(&done, &refused, &empty, Some(name));
                let mut sink = Sink::new(job, Some(name));
                let cards = self.read(job, &mut sink, registry, &reading).await?;
                // A full import reading no card of a registry would remove all its cards.
                let refuse = if import && cards == 0 {
                    empty.push(name.to_owned());
                    true
                } else {
                    sink.finish(job, reading.clone()).await? == diff::Finished::Refused
                };
                if refuse {
                    refused.insert(name.to_owned(), json!(version));
                    job.remove(&[], cursor(&done, &refused, &empty, None)).await?;
                    return Ok(());
                }
                done.insert(name.to_owned(), entry);
                job.remove(&[], cursor(&done, &refused, &empty, None)).await?;
                job.upstream_version(&versions(&done)).await?;
                Ok(())
            }
            .await;
            if let Err(failure) = ran {
                let shown = super::failure::shown(&failure);
                tracing::warn!(catalogue = "software", registry = name, error = %shown, "a registry's file could not be read; the others go on");
                failed.get_or_insert(failure);
            }
        }
        if let Some(error) = failed {
            return Err(error);
        }
        // Every registry's version is kept. The cards are as current as the newest file read:
        // the registries publish at their own pace (F-Droid every few days), so the oldest would
        // show lag where every file read is the newest there is.
        let mut kept = position;
        kept.extend(done.clone());
        if let Some(at) = kept
            .values()
            .filter_map(|v| v.get("at").and_then(Value::as_str))
            .filter_map(|a| chrono::DateTime::parse_from_rfc3339(a).ok())
            .map(|a| a.with_timezone(&chrono::Utc))
            .max()
        {
            // Never ahead of now (an entry's time is the file's own word).
            job.set_current_as_of(at.min(chrono::Utc::now()));
        }
        // The refusals that stand: the earlier ones of registries not read since, and this run's.
        let mut standing: BTreeMap<String, Value> = standing
            .into_iter()
            .filter(|(name, _)| !done.contains_key(name))
            .collect();
        let refused_now = !refused.is_empty();
        // Each registry's shrink floor, before the runner's sweep (which counts the catalogue
        // whole): a short file of one registry never sweeps that registry's other cards.
        if import && !refused_now {
            let counted: Vec<(String, i64, i64)> = sqlx::query_as(
                "SELECT source,count(*) FILTER (WHERE seen_generation=$2),count(*) FROM catalogue_card WHERE catalogue=$1 GROUP BY source ORDER BY source",
            )
            .bind(Catalogue::Software.name())
            .bind(job.generation())
            .fetch_all(job.pool())
            .await?;
            for (registry, read, held) in counted {
                job.check_floor(held, read, &registry).await?;
            }
        }
        let note = refused
            .iter()
            .map(|(name, version)| {
                let why = if empty.contains(name) {
                    "no card"
                } else {
                    "over a tenth of its cards gone"
                };
                format!(
                    "{name} file {}: {why}",
                    version.as_str().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let class = if refused.keys().all(|name| empty.contains(name)) {
            "empty"
        } else {
            "refused"
        };
        standing.extend(refused);
        let kept = serde_json::to_value(kept)?;
        if refused_now {
            job.set_position(kept);
            return Err(job.refuse(serde_json::to_value(standing)?, class, note));
        }
        job.set_position(if standing.is_empty() {
            kept
        } else {
            run::refusing(
                Some(&kept),
                serde_json::to_value(standing)?,
                job.source(),
                job.limit(),
            )
        });
        Ok(())
    }
}
/// The versions read, as `catalogue status` shows them.
fn versions(done: &BTreeMap<String, Value>) -> String {
    done.iter()
        .map(|(name, v)| {
            let at = v.get("at").and_then(Value::as_str).unwrap_or("");
            format!("{name} {}", at.get(..10).unwrap_or(at))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
impl Importer for Software {
    fn catalogue(&self) -> Catalogue {
        Catalogue::Software
    }
    /// The registries read from elsewhere than their own files (`REGISTRY=FILE|URL`).
    fn source(&self) -> Option<String> {
        let official = Sources::official();
        let given: Vec<String> = Registry::ALL
            .into_iter()
            .filter(|r| self.sources.of(*r) != official.of(*r))
            .map(|r| {
                let at = match self.sources.of(r) {
                    Input::Url(url) => url.to_string(),
                    Input::File(file) => file.display().to_string(),
                };
                format!("{}={at}", r.source())
            })
            .collect();
        (!given.is_empty()).then(|| given.join(" "))
    }
    /// None: everything is read as a stream, and nothing is written under `catalogues.dir` (the
    /// cards' 0.4 GB of the database are sized apart).
    fn disk_need(&self, _run: Run, _state: &State) -> u64 {
        0
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        self.run(job).await
    }
    /// An update; a full import instead when one was asked for (`--request`).
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        if job.state().import_requested.is_some() {
            return Ok(UpdateEnd::NeedsImport(super::musicbrainz::REQUESTED));
        }
        self.run(job).await?;
        Ok(UpdateEnd::Done)
    }
}

/// The importer `pinakesd` runs: the official files; none without the operator's contact
/// (`http.contact`), which every request names.
pub fn importer(config: &Config) -> Option<Arc<dyn Importer>> {
    if config.egress.contact.is_empty() {
        if !config.catalogues.enabled(Catalogue::Software) {
            return None;
        }
        tracing::warn!(
            catalogue = "software",
            "the software importer starts only with http.contact set (the registries are asked with the operator's contact)"
        );
        return None;
    }
    match Software::new(Sources::official(), &config.egress.contact) {
        Ok(importer) => Some(Arc::new(importer)),
        Err(error) => {
            tracing::warn!(catalogue = "software", error = %crate::resolver::error_class(&error), "the software importer did not start");
            None
        }
    }
}

// --- crates.io ---------------------------------------------------------------------------------

/// The longest description, link and keyword kept from the dump (bytes).
const TEXT_CAP: usize = 2048;
const NAME_CAP: usize = 128;
/// The longest crate description and link kept from the dump (bytes; a longer link is left out).
const DESCRIPTION_CAP: usize = 1024;
const LINK_CAP: usize = 512;
/// The longest keyword and category slug kept from the dump (bytes; crates.io allows far shorter
/// ones).
const LABEL_CAP: usize = 64;
/// What one map entry costs beyond its text, as counted against [`CRATES_BYTES_CAP`].
const ENTRY_BYTES: usize = 48;

/// One crate as the dump lists it.
#[derive(Clone, Debug, Default)]
struct CrateRow {
    id: u64,
    name: String,
    description: String,
    homepage: String,
    repository: String,
    documentation: String,
    updated_at: String,
}

/// The cards of a crates.io dump (a tar stream): read up to its `crates_keywords.csv`, then the
/// crates as cards in parts. The dump's files come in the order its `import.sql` loads them:
/// categories and downloads before the crates, keywords and the crates' categories and keywords
/// after them, so the crates are held (without their READMEs) until those are read.
fn crates<R: Read>(stream: R, at: &str, sink: &mut dyn FnMut(Vec<Card>) -> bool) -> Result<()> {
    crates_within(stream, at, CRATES_BYTES_CAP, sink)
}
/// [`crates`], holding at most `cap` bytes as counted.
fn crates_within<R: Read>(
    stream: R,
    at: &str,
    cap: usize,
    sink: &mut dyn FnMut(Vec<Card>) -> bool,
) -> Result<()> {
    let mut tar = bulk::Tar::new(stream);
    let mut categories: HashMap<u64, String> = HashMap::new();
    let mut downloads: HashMap<u64, u64> = HashMap::new();
    let mut keywords: HashMap<u64, String> = HashMap::new();
    let mut crate_categories: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut crate_keywords: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut rows: Vec<CrateRow> = Vec::new();
    let mut seen_crates = false;
    // The files after the crates that a card needs; a dump that ends before them is refused.
    let mut seen_after = 0;
    // What the rows and maps hold, against [`CRATES_BYTES_CAP`].
    let mut held = 0usize;
    let failure = |class| Failure::new(class).at(at.to_owned());
    while let Some(entry) = tar.next_entry()? {
        let name = entry.path.rsplit('/').next().unwrap_or_default().to_owned();
        let in_data = entry.path.contains("/data/") || entry.path.starts_with("data/");
        if !in_data {
            continue;
        }
        let data = BufReader::with_capacity(1 << 16, tar.data());
        match name.as_str() {
            "categories.csv" => {
                each(data, &["id", "slug"], |r, c| {
                    // Past the cap: nothing more is held (the file then fails `size`).
                    if held > cap {
                        return;
                    }
                    if let (Some(id), slug) = (number(&r.text(c[0])), r.text(c[1])) {
                        if categories.len() < 100_000 {
                            let slug = shorten(&slug, LABEL_CAP);
                            held += ENTRY_BYTES + slug.len();
                            categories.insert(id, slug);
                        }
                    }
                })
                .map_err(|e| with_at(e, at))?;
            }
            "crate_downloads.csv" => {
                each(data, &["crate_id", "downloads"], |r, c| {
                    // Past the cap: nothing more is held (the file then fails `size`).
                    if held > cap {
                        return;
                    }
                    if let (Some(id), Some(n)) = (number(&r.text(c[0])), number(&r.text(c[1]))) {
                        if downloads.len() < PACKAGES_CAP {
                            held += ENTRY_BYTES;
                            downloads.insert(id, n);
                        }
                    }
                })
                .map_err(|e| with_at(e, at))?;
            }
            "crates.csv" => {
                seen_crates = true;
                let names = [
                    "id",
                    "name",
                    "description",
                    "homepage",
                    "repository",
                    "documentation",
                    "updated_at",
                ];
                let mut over = false;
                let link = |t: String| {
                    if t.len() <= LINK_CAP {
                        t
                    } else {
                        String::new()
                    }
                };
                each(data, &names, |r, c| {
                    let Some(id) = number(&r.text(c[0])) else {
                        return;
                    };
                    let name = r.text(c[1]);
                    if !crate_name(name.trim()) {
                        return;
                    }
                    let row = CrateRow {
                        id,
                        name,
                        description: shorten(&r.text(c[2]), DESCRIPTION_CAP),
                        homepage: link(r.text(c[3])),
                        repository: link(r.text(c[4])),
                        documentation: link(r.text(c[5])),
                        updated_at: shorten(&r.text(c[6]), 32),
                    };
                    held += ENTRY_BYTES
                        + row.name.len()
                        + row.description.len()
                        + row.homepage.len()
                        + row.repository.len()
                        + row.documentation.len()
                        + row.updated_at.len();
                    if rows.len() >= PACKAGES_CAP || held > cap {
                        over = true;
                        return;
                    }
                    rows.push(row);
                })
                .map_err(|e| with_at(e, at))?;
                if over {
                    anyhow::bail!(failure("size"));
                }
            }
            "keywords.csv" => {
                seen_after += 1;
                each(data, &["id", "keyword"], |r, c| {
                    // Past the cap: nothing more is held (the file then fails `size`).
                    if held > cap {
                        return;
                    }
                    if let Some(id) = number(&r.text(c[0])) {
                        if keywords.len() < PACKAGES_CAP {
                            let keyword = shorten(&r.text(c[1]), LABEL_CAP);
                            held += ENTRY_BYTES + keyword.len();
                            keywords.insert(id, keyword);
                        }
                    }
                })
                .map_err(|e| with_at(e, at))?;
            }
            "crates_categories.csv" => {
                seen_after += 1;
                each(data, &["crate_id", "category_id"], |r, c| {
                    // Past the cap: nothing more is held (the file then fails `size`).
                    if held > cap {
                        return;
                    }
                    if let (Some(k), Some(v)) = (number(&r.text(c[0])), number(&r.text(c[1]))) {
                        held += push_capped(&mut crate_categories, k, v);
                    }
                })
                .map_err(|e| with_at(e, at))?;
            }
            "crates_keywords.csv" => {
                seen_after += 1;
                each(data, &["crate_id", "keyword_id"], |r, c| {
                    // Past the cap: nothing more is held (the file then fails `size`).
                    if held > cap {
                        return;
                    }
                    if let (Some(k), Some(v)) = (number(&r.text(c[0])), number(&r.text(c[1]))) {
                        held += push_capped(&mut crate_keywords, k, v);
                    }
                })
                .map_err(|e| with_at(e, at))?;
                if held > cap {
                    anyhow::bail!(failure("size"));
                }
                // Everything a card needs has been read; the version tables are not.
                break;
            }
            _ => {}
        }
        if held > cap {
            anyhow::bail!(failure("size"));
        }
    }
    if !seen_crates || seen_after < 3 {
        anyhow::bail!(failure("archive"));
    }
    let mut part = Vec::with_capacity(fetch::PART);
    for row in rows {
        let labels = crate_keywords
            .get(&row.id)
            .into_iter()
            .flatten()
            .filter_map(|k| keywords.get(k).cloned())
            .chain(
                crate_categories
                    .get(&row.id)
                    .into_iter()
                    .flatten()
                    .filter_map(|c| categories.get(c))
                    .map(|slug| slug.rsplit("::").next().unwrap_or(slug).to_owned()),
            )
            .collect();
        if let Some(card) = crate_card(&row, downloads.get(&row.id).copied().unwrap_or(0), labels) {
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
/// A dump file's failure, with where it was read.
fn with_at(error: anyhow::Error, at: &str) -> anyhow::Error {
    match error.downcast_ref::<Failure>() {
        Some(f) => Failure::new(f.class).at(at.to_owned()).into(),
        None => error,
    }
}
/// At most 64 values per crate (labels need far fewer); the bytes it added.
fn push_capped(map: &mut HashMap<u64, Vec<u64>>, key: u64, value: u64) -> usize {
    if map.len() >= PACKAGES_CAP && !map.contains_key(&key) {
        return 0;
    }
    let mut added = 0;
    let values = map.entry(key).or_insert_with(|| {
        added += ENTRY_BYTES;
        Vec::new()
    });
    if values.len() < 64 {
        values.push(value);
        added += 8;
    }
    added
}
/// Read a CSV file with a header, calling `row` for each record with the places of `names`
/// (fields cut at [`TEXT_CAP`]). A file without one of those columns is refused (`schema`): a
/// renamed column must never read as a registry without crates or labels.
fn each<R: BufRead, const N: usize>(
    data: R,
    names: &[&str; N],
    mut row: impl FnMut(&Record, &[usize; N]),
) -> Result<()> {
    let mut reader = csv::Reader::new(data, TEXT_CAP);
    let mut record = Record::default();
    let parse = || Failure::new("parse");
    if !reader.next_record(&mut record).map_err(|_| parse())? {
        anyhow::bail!(Failure::new("schema"));
    }
    let Some(columns) = csv::columns(&record, *names) else {
        anyhow::bail!(Failure::new("schema"));
    };
    // The columns not asked for (a crate's README) are skipped as they are read.
    let width = record.len();
    let mut caps = vec![0; width];
    for c in columns {
        caps[c] = TEXT_CAP;
    }
    reader.caps(caps);
    while reader.next_record(&mut record).map_err(|_| parse())? {
        row(&record, &columns);
    }
    Ok(())
}
fn number(text: &str) -> Option<u64> {
    (!text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}
/// A crate name as crates.io allows it: 1-64 ASCII letters, digits, `-` and `_`, starting with a
/// letter.
fn crate_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
/// The keys a package page names, in their stored form (`join::link_keys`).
fn page_keys(uri: &str) -> Vec<String> {
    super::join::link_keys(uri)
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}
/// An http(s) link the card may relate (a repository, a homepage), else none.
fn web_link(link: &str) -> Option<String> {
    let link = link.trim();
    let url = url::Url::parse(link).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && link.len() <= 2048
        && !crate::capability::is_capability(link))
    .then(|| link.to_owned())
}
fn crate_card(row: &CrateRow, downloads: u64, labels: Vec<String>) -> Option<Card> {
    let name = row.name.trim();
    if !crate_name(name) {
        return None;
    }
    let uri = format!("https://crates.io/crates/{name}");
    let mut details = serde_json::Map::new();
    for (key, link) in [
        ("repository", &row.repository),
        ("homepage", &row.homepage),
        ("documentation", &row.documentation),
    ] {
        if let Some(link) = web_link(link) {
            details.insert(key.into(), json!(link));
        }
    }
    if let Some(day) = row
        .updated_at
        .get(..10)
        .filter(|d| d.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
    {
        details.insert("updated".into(), json!(day));
    }
    Some(Card {
        source: Registry::Crates.source().into(),
        source_id: format!("crates.io/{name}"),
        keys: page_keys(&uri),
        r#type: "SoftwareSourceCode".into(),
        name: shorten(name, NAME_CAP),
        description: Some(row.description.clone()).filter(|d| !d.trim().is_empty()),
        labels,
        related_uris: [&row.repository, &row.homepage]
            .into_iter()
            .filter_map(|l| web_link(l))
            .collect(),
        fame: downloads_fame(downloads),
        details,
        uri,
        ..Card::default()
    })
}
/// A crate's fame: ln(1 + downloads) / ln(1 + 10^9), at most 1, in steps of 0.01 (downloads
/// grow every day; the card changes only when its fame moves a step).
pub fn downloads_fame(downloads: u64) -> f32 {
    let fame = ((1.0 + downloads as f64).ln() / (1.0 + 1e9_f64).ln()).min(1.0);
    ((fame * 100.0).floor() / 100.0) as f32
}
fn shorten(text: &str, cap: usize) -> String {
    let mut text = text.to_owned();
    crate::model::truncate(&mut text, cap);
    text
}

// --- F-Droid -----------------------------------------------------------------------------------

/// F-Droid's `entry.json`: the index file it names, with its digest and size.
#[derive(Clone, Debug, Deserialize)]
pub struct FdroidEntry {
    pub timestamp: i64,
    pub index: FdroidFile,
}
#[derive(Clone, Debug, Deserialize)]
pub struct FdroidFile {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}
fn fdroid_entry(body: &[u8]) -> Option<FdroidEntry> {
    let entry: FdroidEntry = serde_json::from_slice(body).ok()?;
    // The index sits next to the entry: a plain file name of letters, digits, `.`, `_` and `-`
    // (nothing a URL join could read as another host, scheme or directory).
    let name = entry
        .index
        .name
        .strip_prefix('/')
        .unwrap_or(&entry.index.name);
    (index_name(name)
        && entry.index.sha256.len() == 64
        && entry.index.sha256.bytes().all(|b| b.is_ascii_hexdigit()))
    .then_some(entry)
}
/// A plain file name F-Droid's entry may name: 1-128 ASCII letters, digits, `.`, `_` and `-`,
/// not starting with a dot.
pub fn index_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
/// The fields of `index-v2.json` a card reads; the versions and everything else are skipped.
#[derive(Deserialize)]
struct FdroidIndex {
    #[serde(default)]
    packages: BTreeMap<String, FdroidPackage>,
}
#[derive(Deserialize)]
struct FdroidPackage {
    #[serde(default)]
    metadata: FdroidMetadata,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FdroidMetadata {
    #[serde(default)]
    name: Localized,
    #[serde(default)]
    summary: Localized,
    #[serde(default)]
    categories: Vec<String>,
    source_code: Option<String>,
    web_site: Option<String>,
    license: Option<String>,
    author_name: Option<String>,
    added: Option<i64>,
    last_updated: Option<i64>,
}
/// A text in several languages (`{"en-US": …}`), or a plain text.
#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Localized {
    Plain(String),
    Languages(BTreeMap<String, String>),
    #[default]
    #[serde(skip)]
    None,
}
impl Localized {
    /// The English text (`en-US`, `en`, `en-GB`, any `en-…`), else the first.
    fn english(&self) -> Option<String> {
        match self {
            Localized::Plain(text) => Some(text.clone()),
            Localized::Languages(map) => ["en-US", "en", "en-GB"]
                .iter()
                .find_map(|l| map.get(*l))
                .or_else(|| {
                    map.iter()
                        .find(|(l, _)| l.starts_with("en-"))
                        .map(|(_, t)| t)
                })
                .or_else(|| map.values().next())
                .cloned(),
            Localized::None => None,
        }
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
    }
}
/// The cards of F-Droid's index: read whole (within its size), its SHA-256 and size checked
/// against the entry, then its packages as cards.
fn fdroid<R: Read>(
    body: R,
    entry: &FdroidEntry,
    at: &str,
    sink: &mut dyn FnMut(Vec<Card>) -> bool,
) -> Result<()> {
    let mut hashed = bulk::Hashed::new(body);
    let index: Result<FdroidIndex, _> = serde_json::from_reader(BufReader::new(&mut hashed));
    // The rest, if any, counts for the digest.
    std::io::copy(&mut hashed, &mut std::io::sink())?;
    if hashed.count() != entry.index.size || !hashed.hex().eq_ignore_ascii_case(&entry.index.sha256)
    {
        anyhow::bail!(Failure::new("checksum").at(at.to_owned()));
    }
    let index = index.map_err(|_| Failure::new("parse").at(at.to_owned()))?;
    if index.packages.len() > PACKAGES_CAP {
        anyhow::bail!(Failure::new("size").at(at.to_owned()));
    }
    let mut part = Vec::new();
    for (id, package) in &index.packages {
        if let Some(card) = fdroid_card(id, &package.metadata) {
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
/// An Android application ID: two or more parts of ASCII letters, digits and `_`, each starting
/// with a letter, joined by dots; at most 255 bytes.
pub fn android_id(id: &str) -> bool {
    id.len() <= 255
        && id.split('.').count() >= 2
        && id.split('.').all(|part| {
            part.starts_with(|c: char| c.is_ascii_alphabetic())
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}
fn fdroid_card(id: &str, m: &FdroidMetadata) -> Option<Card> {
    if !android_id(id) {
        return None;
    }
    // The page as a link to it is stored (no final slash), so the card holds its own link.
    let uri = format!("https://f-droid.org/packages/{id}");
    let mut details = serde_json::Map::new();
    if let Some(license) = m.license.as_deref().filter(|l| l.len() <= 128) {
        details.insert("license".into(), json!(license));
    }
    if let Some(author) = m.author_name.as_deref().filter(|a| a.len() <= 256) {
        details.insert("author".into(), json!(author));
    }
    for (key, ms) in [("added", m.added), ("updated", m.last_updated)] {
        if let Some(day) = ms.and_then(chrono::DateTime::from_timestamp_millis) {
            details.insert(key.into(), json!(day.format("%Y-%m-%d").to_string()));
        }
    }
    for (key, link) in [("source_code", &m.source_code), ("website", &m.web_site)] {
        if let Some(link) = link.as_deref().and_then(web_link) {
            details.insert(key.into(), json!(link));
        }
    }
    Some(Card {
        source: Registry::FDroid.source().into(),
        source_id: format!("f-droid/{id}"),
        keys: page_keys(&uri),
        r#type: "MobileApplication".into(),
        name: m
            .name
            .english()
            .map_or_else(|| id.to_owned(), |n| shorten(&n, NAME_CAP * 2)),
        description: m.summary.english(),
        labels: m.categories.iter().take(32).cloned().collect(),
        related_uris: [&m.source_code, &m.web_site]
            .into_iter()
            .filter_map(|l| l.as_deref().and_then(web_link))
            .collect(),
        details,
        uri,
        ..Card::default()
    })
}

// --- Flathub -----------------------------------------------------------------------------------

/// The kinds of AppStream component that are applications.
const APP_TYPES: &[&str] = &["desktop-application", "console-application", "desktop"];
/// Toolkit and desktop categories that say nothing about what an app is for.
const TOOLKITS: &[&str] = &[
    "GTK", "Qt", "KDE", "GNOME", "XFCE", "Motif", "Java", "Building", "Debugger",
];
/// The most components and the deepest nesting read from the catalogue.
const COMPONENTS_CAP: usize = 200_000;
const DEPTH_CAP: usize = 64;
/// The most bytes of one element's text kept.
const XML_TEXT_CAP: usize = 4096;

#[derive(Default, Debug)]
struct Component {
    kind: String,
    id: String,
    name: Option<String>,
    summary: Option<String>,
    categories: Vec<String>,
    keywords: Vec<String>,
    homepage: Option<String>,
    vcs: Option<String>,
    license: Option<String>,
    developer: Option<String>,
}
/// The cards of Flathub's AppStream catalogue, read as a stream of XML events: each
/// application component's ID, name, summary, categories, keywords, homepage and source links,
/// in the catalogue's own language (elements with an `xml:lang` are passed over).
fn flathub<R: BufRead>(xml: R, at: &str, sink: &mut dyn FnMut(Vec<Card>) -> bool) -> Result<()> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let config = reader.config_mut();
    config.trim_text(true);
    config.expand_empty_elements = true;
    let mut buf = Vec::new();
    // The open elements' names (local, without a prefix), and whether one carries `xml:lang`.
    let mut path: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut component: Option<Component> = None;
    let mut url_type: Option<String> = None;
    let mut text = String::new();
    let mut components = 0usize;
    let mut part = Vec::new();
    let failure = || Failure::new("parse").at(at.to_owned());
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Err(_) => anyhow::bail!(failure()),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if path.len() >= DEPTH_CAP {
                    anyhow::bail!(failure());
                }
                let name = e.local_name().as_ref().to_vec();
                let mut lang = false;
                let mut kind = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"xml:lang" => lang = true,
                        b"type" => kind = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                        _ => {}
                    }
                }
                if name == b"component" && path.len() == 1 {
                    components += 1;
                    if components > COMPONENTS_CAP {
                        anyhow::bail!(Failure::new("size").at(at.to_owned()));
                    }
                    component = Some(Component {
                        kind: kind.clone().unwrap_or_default(),
                        ..Component::default()
                    });
                }
                if name == b"url" {
                    url_type = kind;
                }
                path.push((name, lang));
                text.clear();
            }
            Ok(Event::Text(t)) => {
                if let Ok(t) = t.unescape() {
                    if text.len() + t.len() <= XML_TEXT_CAP {
                        text.push_str(&t);
                    }
                }
            }
            Ok(Event::CData(t)) => {
                let t = String::from_utf8_lossy(&t);
                if text.len() + t.len() <= XML_TEXT_CAP {
                    text.push_str(&t);
                }
            }
            Ok(Event::End(_)) => {
                let Some((name, lang)) = path.pop() else {
                    anyhow::bail!(failure());
                };
                let parent = path.last().map(|(n, _)| n.as_slice());
                let any_lang = lang || path.iter().any(|(_, l)| *l);
                let value = text.trim().to_owned();
                text.clear();
                if let Some(c) = component.as_mut() {
                    match (name.as_slice(), parent) {
                        (b"component", _) if path.len() == 1 => {
                            if let Some(card) = component.take().and_then(|c| flathub_card(&c)) {
                                part.push(card);
                                if part.len() >= fetch::PART && !sink(std::mem::take(&mut part)) {
                                    return Ok(());
                                }
                            }
                        }
                        _ if any_lang || value.is_empty() => {}
                        (b"id", Some(b"component")) => c.id = value,
                        (b"name", Some(b"component")) => c.name = Some(value),
                        (b"summary", Some(b"component")) => c.summary = Some(value),
                        (b"project_license", Some(b"component")) => c.license = Some(value),
                        (b"developer_name", Some(b"component")) => c.developer = Some(value),
                        (b"name", Some(b"developer")) => c.developer = Some(value),
                        (b"category", Some(b"categories")) if c.categories.len() < 32 => {
                            c.categories.push(value)
                        }
                        (b"keyword", Some(b"keywords")) if c.keywords.len() < 32 => {
                            c.keywords.push(value)
                        }
                        (b"url", Some(b"component")) => match url_type.as_deref() {
                            Some("homepage") => c.homepage = Some(value),
                            Some("vcs-browser") => c.vcs = Some(value),
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
            Ok(_) => {}
        }
    }
    if !path.is_empty() {
        anyhow::bail!(failure());
    }
    if !part.is_empty() {
        sink(part);
    }
    Ok(())
}
/// A Flathub application ID: three or more parts of ASCII letters, digits, `_` and `-`, joined
/// by dots; at most 255 bytes.
pub fn flathub_id(id: &str) -> bool {
    id.len() <= 255
        && id.split('.').count() >= 3
        && id.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}
/// A category as words: `AudioVideo` is `audio video`, `2DGraphics` is `2d graphics`.
fn category_words(category: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = category.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
        if i > 0
            && c.is_uppercase()
            && (chars[i - 1].is_lowercase() || next_lower && chars[i - 1].is_uppercase())
        {
            out.push(' ');
        }
        out.push(*c);
    }
    out
}
fn flathub_card(c: &Component) -> Option<Card> {
    if !APP_TYPES.contains(&c.kind.as_str()) {
        return None;
    }
    let id = c.id.strip_suffix(".desktop").unwrap_or(&c.id);
    if !flathub_id(id) {
        return None;
    }
    let uri = format!("https://flathub.org/apps/{id}");
    let mut details = serde_json::Map::new();
    if let Some(license) = c.license.as_deref().filter(|l| l.len() <= 256) {
        details.insert("license".into(), json!(license));
    }
    if let Some(developer) = c.developer.as_deref().filter(|d| d.len() <= 256) {
        details.insert("developer".into(), json!(developer));
    }
    for (key, link) in [("homepage", &c.homepage), ("source_code", &c.vcs)] {
        if let Some(link) = link.as_deref().and_then(web_link) {
            details.insert(key.into(), json!(link));
        }
    }
    Some(Card {
        source: Registry::Flathub.source().into(),
        source_id: format!("flathub/{id}"),
        keys: page_keys(&uri),
        r#type: "SoftwareApplication".into(),
        name: c
            .name
            .clone()
            .map_or_else(|| id.to_owned(), |n| shorten(&n, NAME_CAP * 2)),
        description: c.summary.clone(),
        labels: c
            .categories
            .iter()
            .filter(|cat| !TOOLKITS.contains(&cat.as_str()))
            .map(|cat| category_words(cat))
            .chain(c.keywords.iter().cloned())
            .collect(),
        related_uris: [&c.homepage, &c.vcs]
            .into_iter()
            .filter_map(|l| l.as_deref().and_then(web_link))
            .collect(),
        details,
        uri,
        ..Card::default()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cards_of(
        read: impl FnOnce(&mut dyn FnMut(Vec<Card>) -> bool) -> Result<()>,
    ) -> Result<Vec<Card>> {
        let mut out = Vec::new();
        read(&mut |part| {
            out.extend(part);
            true
        })?;
        Ok(out)
    }
    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/catalogue/software/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }
    /// A tar archive of `files` (path, body), as the dump packs them.
    pub(crate) fn tar(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, body) in files {
            let mut h = [0u8; 512];
            h[..path.len()].copy_from_slice(path.as_bytes());
            h[100..108].copy_from_slice(b"0000644\0");
            h[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
            h[156] = b'0';
            h[257..265].copy_from_slice(b"ustar  \0");
            h[148..156].copy_from_slice(b"        ");
            let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
            h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            out.extend(h);
            out.extend(*body);
            out.extend(vec![0u8; (512 - body.len() % 512) % 512]);
        }
        out.extend([0u8; 1024]);
        out
    }

    /// What the dump's reading holds counts its maps too: keywords cut to 64 bytes count as
    /// such, and many keyword rows pass the cap.
    #[test]
    fn the_crates_dump_s_maps_count_toward_the_cap() {
        let dump = |keywords: &str| {
            tar(&[
                ("d/data/categories.csv", b"id,slug\n"),
                ("d/data/crate_downloads.csv", b"crate_id,downloads\n"),
                (
                    "d/data/crates.csv",
                    b"id,name,description,homepage,repository,documentation,updated_at\n1,serde,S,,,,2026-01-01\n",
                ),
                ("d/data/keywords.csv", keywords.as_bytes()),
                ("d/data/crates_categories.csv", b"crate_id,category_id\n"),
                ("d/data/crates_keywords.csv", b"crate_id,keyword_id\n1,1\n"),
            ])
        };
        // Ten keywords of 1,000 bytes, cut to 64 each: within a cap of 2,000 bytes.
        let long: String = (1..=10)
            .map(|n| format!("{n},{}\n", "k".repeat(1000)))
            .collect();
        let long = dump(&format!("id,keyword\n{long}"));
        assert!(cards_of(|sink| crates_within(&long[..], "t", 2000, sink)).is_ok());
        // A hundred short keywords pass it: the map's entries count.
        let many: String = (1..=100).map(|n| format!("{n},k{n}\n")).collect();
        let many = dump(&format!("id,keyword\n{many}"));
        let failed = cards_of(|sink| crates_within(&many[..], "t", 2000, sink)).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "size");
        // The crates' keywords alone pass it (the dump's last file read): it fails too.
        let pairs: String = (1..=100).map(|n| format!("{n},1\n")).collect();
        let crates: String = (1..=100)
            .map(|n| format!("{n},c{n},S,,,,2026-01-01\n"))
            .collect();
        let dump = tar(&[
            ("d/data/categories.csv", b"id,slug\n"),
            ("d/data/crate_downloads.csv", b"crate_id,downloads\n"),
            (
                "d/data/crates.csv",
                format!(
                    "id,name,description,homepage,repository,documentation,updated_at\n{crates}"
                )
                .as_bytes(),
            ),
            ("d/data/keywords.csv", b"id,keyword\n1,k\n"),
            ("d/data/crates_categories.csv", b"crate_id,category_id\n"),
            (
                "d/data/crates_keywords.csv",
                format!("crate_id,keyword_id\n{pairs}").as_bytes(),
            ),
        ]);
        let failed = cards_of(|sink| crates_within(&dump[..], "t", 9000, sink)).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "size");
        assert!(cards_of(|sink| crates_within(&dump[..], "t", 20_000, sink)).is_ok());
    }

    /// The crates of a dump become cards: the crate page as the link, the crate key, keywords and
    /// categories as labels, the repository and homepage as related links, fame from downloads.
    #[test]
    fn a_crates_dump_becomes_cards() {
        let dump = tar(&[
            ("2026-11-26-020015/metadata.json", b"{}"),
            (
                "2026-11-26-020015/data/categories.csv",
                &fixture("categories.csv"),
            ),
            (
                "2026-11-26-020015/data/crate_downloads.csv",
                &fixture("crate_downloads.csv"),
            ),
            ("2026-11-26-020015/data/crates.csv", &fixture("crates.csv")),
            (
                "2026-11-26-020015/data/keywords.csv",
                &fixture("keywords.csv"),
            ),
            (
                "2026-11-26-020015/data/crates_categories.csv",
                &fixture("crates_categories.csv"),
            ),
            (
                "2026-11-26-020015/data/crates_keywords.csv",
                &fixture("crates_keywords.csv"),
            ),
            ("2026-11-26-020015/data/versions.csv", b"never read"),
        ]);
        let cards = cards_of(|sink| crates(&dump[..], "test", sink)).unwrap();
        assert!(cards.len() >= 3, "{}", cards.len());
        let serde = cards
            .iter()
            .find(|c| c.source_id == "crates.io/serde")
            .expect("serde");
        assert_eq!(serde.uri, "https://crates.io/crates/serde");
        assert_eq!(serde.source, "crates.io");
        assert!(
            serde.keys.contains(&"crate:serde".to_owned()),
            "{:?}",
            serde.keys
        );
        assert!(
            serde.labels.iter().any(|l| l == "serde"),
            "{:?}",
            serde.labels
        );
        assert!(
            serde.labels.iter().any(|l| l == "encoding"),
            "{:?}",
            serde.labels
        );
        assert!(serde
            .related_uris
            .iter()
            .any(|u| u.contains("github.com/serde-rs/serde")));
        assert!(serde.fame > 0.5, "{}", serde.fame);
        for card in &cards {
            card.checked(Catalogue::Software).unwrap();
        }
        // A dump without its crates is not a dump.
        let empty = tar(&[("x/data/categories.csv", &fixture("categories.csv"))]);
        let failed = cards_of(|sink| crates(&empty[..], "test", sink)).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "archive");
        // A dump without the files after its crates (keywords, the crates' categories and
        // keywords) is refused: its cards would lose every label.
        let short = tar(&[
            ("d/data/categories.csv", &fixture("categories.csv")),
            (
                "d/data/crate_downloads.csv",
                &fixture("crate_downloads.csv"),
            ),
            ("d/data/crates.csv", &fixture("crates.csv")),
        ]);
        let failed = cards_of(|sink| crates(&short[..], "test", sink)).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "archive");
        // A dump cut inside its crates is an error, never a smaller catalogue.
        let cut = &dump[..dump.len() / 2];
        assert!(cards_of(|sink| crates(cut, "test", sink)).is_err());
    }

    fn entry_for(body: &[u8]) -> FdroidEntry {
        let mut hashed = bulk::Hashed::new(body);
        std::io::copy(&mut hashed, &mut std::io::sink()).unwrap();
        FdroidEntry {
            timestamp: 1_795_453_653_400,
            index: FdroidFile {
                name: "/index-v2.json".into(),
                sha256: hashed.hex(),
                size: body.len() as u64,
            },
        }
    }
    /// F-Droid's index becomes cards once its digest and size are the entry's.
    #[test]
    fn an_fdroid_index_becomes_cards_once_its_digest_holds() {
        let index = fixture("fdroid-index-v2.json");
        let entry = entry_for(&index);
        let cards = cards_of(|sink| fdroid(&index[..], &entry, "test", sink)).unwrap();
        assert!(cards.len() >= 2);
        let app = &cards[0];
        assert!(app.uri.starts_with("https://f-droid.org/packages/"));
        assert!(
            app.keys.iter().any(|k| k.starts_with("android:")),
            "{:?}",
            app.keys
        );
        assert!(!app.labels.is_empty());
        for card in &cards {
            card.checked(Catalogue::Software).unwrap();
        }
        let mut wrong = entry.clone();
        wrong.index.sha256 = "0".repeat(64);
        let failed = cards_of(|sink| fdroid(&index[..], &wrong, "test", sink)).unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "checksum");
        // An entry naming a file elsewhere is refused.
        let mut far = serde_json::json!({"timestamp": 1, "index": {"name": "/../x.json", "sha256": "0".repeat(64), "size": 1}});
        assert!(fdroid_entry(far.to_string().as_bytes()).is_none());
        far["index"]["name"] = "/index-v2.json".into();
        assert!(fdroid_entry(far.to_string().as_bytes()).is_some());
    }

    /// F-Droid's own entry (`entry.json` as recorded, its diffs and other fields beside the
    /// index) reads: the index next to it, its digest and size, and the entry's time.
    #[test]
    fn the_recorded_fdroid_entry_reads() {
        let entry = fdroid_entry(&fixture("fdroid-entry.json")).expect("the recorded entry");
        assert_eq!(entry.index.name, "/index-v2.json");
        assert!(entry.index.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(entry.index.size, 74_859_096);
        assert_eq!(entry.timestamp, 1_795_453_653_400);
    }

    /// Flathub's catalogue becomes cards for its applications only, in its own language.
    #[test]
    fn a_flathub_catalogue_becomes_cards_for_its_applications() {
        let xml = fixture("flathub-appstream.xml");
        let cards = cards_of(|sink| flathub(&xml[..], "test", sink)).unwrap();
        assert!(!cards.is_empty());
        for card in &cards {
            assert!(card.uri.starts_with("https://flathub.org/apps/"));
            assert!(
                card.keys.iter().any(|k| k.starts_with("flathub:")),
                "{:?}",
                card.keys
            );
            card.checked(Catalogue::Software).unwrap();
        }
        // No runtime or add-on.
        assert!(cards.iter().all(|c| !c.source_id.contains("Platform")));
        // A broken file is a failure.
        assert!(cards_of(|sink| flathub(&b"<components><component>"[..], "t", sink)).is_err());
    }

    #[test]
    fn categories_read_as_words() {
        for (raw, words) in [
            ("AudioVideo", "Audio Video"),
            ("2DGraphics", "2D Graphics"),
            ("Game", "Game"),
            ("IDE", "IDE"),
            ("InstantMessaging", "Instant Messaging"),
        ] {
            assert_eq!(category_words(raw), words);
        }
        assert_eq!(
            super::super::label(&category_words("ArcadeGame")).as_deref(),
            Some("arcade-game")
        );
    }

    #[test]
    fn package_ids() {
        assert!(android_id("org.fdroid.fdroid") && android_id("de.danoeh.antennapod"));
        for bad in ["fdroid", "org..x", "1org.x", "org.x/../y", "org.x y", ""] {
            assert!(!android_id(bad), "{bad}");
        }
        assert!(flathub_id("org.gnome.Maps") && flathub_id("io.github.some-one.App_1"));
        for bad in [
            "org.gnome",
            "org..gnome.Maps",
            "org.gnome.Maps/x",
            "org.gnome.Ma ps",
        ] {
            assert!(!flathub_id(bad), "{bad}");
        }
        assert!(crate_name("serde_json") && crate_name("a-b"));
        for bad in ["", "1x", "x y", "x/y", &"x".repeat(65)] {
            assert!(!crate_name(bad), "{bad}");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]
        /// Property: every package page a card names reads back the card's own key, and the
        /// link is stored as it is written (a canonical link).
        #[test]
        fn a_package_page_names_its_own_key(name in "[a-z][a-z0-9_-]{0,20}", id in "[a-z][a-z0-9_]{0,8}(\\.[a-z][a-z0-9_]{0,8}){1,3}", fid in "[a-z][a-z0-9]{0,6}\\.[a-z][a-z0-9-]{0,6}\\.[A-Za-z][A-Za-z0-9_]{0,8}") {
            let row = CrateRow { id: 1, name: name.clone(), ..CrateRow::default() };
            let card = crate_card(&row, 10, vec![]).unwrap();
            let checked = card.checked(Catalogue::Software).unwrap();
            prop_assert_eq!(&checked.uri, &card.uri);
            prop_assert!(checked.keys.iter().any(|k| k.starts_with("crate:")), "{:?}", checked.keys);
            let card = fdroid_card(&id, &FdroidMetadata::default()).unwrap();
            let checked = card.checked(Catalogue::Software).unwrap();
            prop_assert_eq!(&checked.uri, &card.uri);
            prop_assert!(checked.keys.contains(&format!("android:{id}")), "{:?}", checked.keys);
            let c = Component { kind: "desktop-application".into(), id: fid.clone(), ..Component::default() };
            let card = flathub_card(&c).unwrap();
            let checked = card.checked(Catalogue::Software).unwrap();
            prop_assert_eq!(&checked.uri, &card.uri);
            prop_assert!(checked.keys.iter().any(|k| k.starts_with("flathub:")), "{:?}", checked.keys);
        }
        /// Fuzz: any bytes as a dump, an index or a catalogue end without a panic.
        #[test]
        fn any_bytes_end_without_a_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = cards_of(|sink| crates(&bytes[..], "t", sink));
            let entry = entry_for(&bytes);
            let _ = cards_of(|sink| fdroid(&bytes[..], &entry, "t", sink));
            let _ = cards_of(|sink| flathub(&bytes[..], "t", sink));
        }
        /// Fuzz: a dump whose files hold arbitrary CSV reads without a panic.
        #[test]
        fn any_csv_in_a_dump_reads(text in "[a-z0-9,\"\n_:]{0,600}") {
            let dump = tar(&[
                ("d/data/categories.csv", format!("id,slug\n{text}").as_bytes()),
                ("d/data/crates.csv", format!("id,name,description,homepage,repository,documentation,updated_at\n{text}").as_bytes()),
                ("d/data/crates_keywords.csv", format!("crate_id,keyword_id\n{text}").as_bytes()),
            ]);
            let _ = cards_of(|sink| crates(&dump[..], "t", sink));
        }
        /// Fuzz: XML made of AppStream's own elements, nested and cut anywhere, reads without a
        /// panic, and only applications become cards.
        #[test]
        fn any_appstream_reads(parts in proptest::collection::vec(prop_oneof![
            Just("<component type=\"desktop-application\">"), Just("<component type=\"runtime\">"), Just("</component>"),
            Just("<id>org.example.App</id>"), Just("<name>A</name>"), Just("<name xml:lang=\"de\">B</name>"),
            Just("<categories><category>AudioVideo</category></categories>"), Just("<url type=\"homepage\">https://example.org/</url>"),
            Just("<![CDATA[x]]>"), Just("&amp;"), Just("&bogus;"), Just("<"), Just(">"),
        ], 0..60)) {
            let xml = format!("<components>{}</components>", parts.concat());
            if let Ok(cards) = cards_of(|sink| flathub(xml.as_bytes(), "t", sink)) {
                for card in cards {
                    prop_assert!(card.checked(Catalogue::Software).is_ok());
                }
            }
        }
    }
}
