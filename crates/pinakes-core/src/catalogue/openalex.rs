//! The OpenAlex catalogue: one card per work cited more than `openalex.min_citations` times (50
//! by default), from OpenAlex's snapshot in its public S3 bucket (`data/jsonl/`, works as gzipped
//! JSON lines, one directory per `updated_date`).
//!
//! A full import reads the snapshot's manifest and every works file it lists, [`STREAMS`] files
//! at a time; nothing is written to disk. A work can sit in more than one `updated_date`
//! directory: the newest copy is the right one, so a file's cards are written only once every
//! file of an earlier date has been written (files of one date are written side by side).
//!
//! The card's link is the work's DOI link (`https://doi.org/…`, stored as its root `doi:…`),
//! else its openalex.org page. Its keys are the DOI, the OpenAlex ID, the PubMed and PubMed
//! Central IDs and the arXiv ID (from an arXiv landing page or an arXiv DOI), so a shared DOI,
//! arXiv, PubMed or OpenAlex link joins the card. Labels are its topics, their subfields and fields, and its keywords; fame is
//! ln(1 + citations) / ln(1 + 100,000).
//!
//! An update reads a new snapshot's files of dates after the last one read (and that last date's
//! files again when their list changed): it writes the works whose card changed and removes the
//! works in those files that no longer qualify. A changed `min_citations` asks for a full import.
//!
//! The snapshot lists no merged or deleted works (its `merged_ids` lists stopped in 2025 and
//! are not in `data/` any more), so an update can't see that a work went. A full import, which
//! removes every card it did not read, is run instead of the first update of a new snapshot
//! [`FULL_EVERY`] after the last one.
//!
//! Memory: up to [`BUFFERED`] files wait with their cards held ([`HELD_CARDS`] each at most,
//! a few hundred bytes a card, the IDs of works that don't qualify counting toward it: about 1-2
//! GB at worst, a few hundred MB as a rule), and the IDs of
//! the works with a card are held as 64-bit numbers (about 15M today: 250-500 MB).
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
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashSet},
    io::BufReader,
    path::PathBuf,
    sync::Arc,
};

/// The public bucket.
pub const BUCKET: &str = "https://openalex.s3.amazonaws.com/";
/// The manifest's path in the bucket.
pub const MANIFEST: &str = "data/jsonl/manifest.json";
/// Files read at once.
pub const STREAMS: usize = 8;
/// The longest record kept (bytes of one JSON line; the longest seen is under 1 MB). A longer
/// one is skipped and counted.
pub const LINE_CAP: usize = 8 << 20;
/// The longest text a card takes from a work (a title, a venue, an author, a label), in bytes:
/// cut as it is read, so a part of cards stays small whatever a line holds.
const TEXT_CAP: usize = 512;
/// The most bytes read of the manifest (0.7 MB today) and of one works file (under 1 GB).
const MANIFEST_MAX: u64 = 64 << 20;
const PART_MAX: u64 = 16 << 30;
/// The most bytes one works file may unpack to (a few GB today): past it the file fails (`size`).
const UNPACKED_MAX: u64 = 64 << 30;
/// Lines that may qualify but don't parse that a file may hold before it counts as broken
/// (and only when they are more than one in a thousand).
const BROKEN_FLOOR: u64 = 100;
/// The most works files a manifest may list (1,898 today).
const FILES_CAP: usize = 100_000;
/// How long after a full import a new snapshot is read by a full import again, not an update:
/// the works merged or deleted upstream since then lose their cards.
pub const FULL_EVERY: chrono::Duration = chrono::Duration::days(180);
/// The update's reason for handing over to that full import.
const FULL_AGAIN: &str =
    "a full import every 180 days removes the works merged or deleted upstream";

/// One works file of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Part {
    /// Its `updated_date` (`YYYY-MM-DD`).
    pub date: String,
    /// Its path in the bucket (`data/jsonl/works/updated_date=…/part_0000.gz`).
    pub path: String,
    pub bytes: u64,
}
/// A snapshot's manifest: its date and its works files, in (date, path) order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub date: String,
    pub works: Vec<Part>,
}
fn date(text: &str) -> bool {
    text.len() == 10 && chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok()
}
/// The manifest, its works files checked to be the bucket's own works paths
/// (`s3://openalex/data/jsonl/works/updated_date=YYYY-MM-DD/part_NNNN.gz`); a file elsewhere,
/// or a manifest without a date, is refused.
pub fn manifest(body: &[u8]) -> Option<Manifest> {
    #[derive(Deserialize)]
    struct Raw {
        date: String,
        entities: Vec<Entity>,
    }
    #[derive(Deserialize)]
    struct Entity {
        entity: String,
        #[serde(default)]
        files: Vec<File>,
    }
    #[derive(Deserialize)]
    struct File {
        url: String,
        #[serde(default)]
        meta: Meta,
    }
    #[derive(Default, Deserialize)]
    struct Meta {
        #[serde(default)]
        content_length: u64,
    }
    let raw: Raw = serde_json::from_slice(body).ok()?;
    if !date(&raw.date) {
        return None;
    }
    let mut works = Vec::new();
    for entity in raw.entities.into_iter().filter(|e| e.entity == "works") {
        for file in entity.files {
            let path = file.url.strip_prefix("s3://openalex/")?;
            let rest = path.strip_prefix("data/jsonl/works/updated_date=")?;
            let (day, name) = rest.split_once('/')?;
            let part = name.strip_prefix("part_")?.strip_suffix(".gz")?;
            if !date(day)
                || part.is_empty()
                || part.len() > 8
                || !part.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            works.push(Part {
                date: day.to_owned(),
                path: path.to_owned(),
                bytes: file.meta.content_length,
            });
            if works.len() > FILES_CAP {
                return None;
            }
        }
    }
    works.sort();
    works.dedup();
    Some(Manifest {
        date: raw.date,
        works,
    })
}
/// A digest of one date's file list (paths and sizes), to see whether that date changed.
fn files_digest(works: &[Part], day: &str) -> String {
    let mut hash = blake3::Hasher::new();
    for part in works.iter().filter(|p| p.date == day) {
        hash.update(part.path.as_bytes());
        hash.update(&part.bytes.to_le_bytes());
    }
    hash.finalize().to_hex()[..32].to_owned()
}

/// The importer.
pub struct OpenAlex {
    client: fetch::Client,
    /// The bucket's root, or a local directory holding a snapshot's `data/jsonl/` tree.
    base: Input,
}
impl OpenAlex {
    pub fn new(base: Input, contact: &str) -> Result<Self> {
        Ok(Self {
            client: fetch::client(contact, &[])?,
            base,
        })
    }
    /// A path of the bucket, under this importer's root.
    fn at(&self, path: &str) -> Result<Input> {
        // Paths are the manifest's checked works paths or the manifest's own: no `..`, relative.
        if path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        {
            anyhow::bail!(Failure::new("manifest"));
        }
        match &self.base {
            Input::Url(url) => Ok(Input::Url(
                url.join(path).map_err(|_| Failure::new("manifest"))?,
            )),
            Input::File(dir) => Ok(Input::File(dir.join(path))),
        }
    }
    async fn manifest(&self) -> Result<Manifest> {
        let input = self.at(MANIFEST)?;
        let body = input.read_small(&self.client, MANIFEST_MAX).await?;
        manifest(&body).ok_or_else(|| Failure::new("manifest").at(input.at()).into())
    }
    /// Read `parts` (indices into `manifest.works`), [`STREAMS`] at a time, writing cards in date
    /// order and removing the stored cards of works whose newest copy no longer qualifies. `done`
    /// are the files this run has already written (its cursor; a full import's only).
    ///
    /// A file is read whole while earlier dates are still being written, its cards and the IDs
    /// of its works that don't qualify held in memory (a file's kept cards are a few megabytes),
    /// and handed over once every earlier date is written: up to [`BUFFERED`] files wait so.
    /// Which IDs have a card is known in memory (the IDs stored when the run began, in an
    /// update, and those written since), so a work that doesn't qualify costs no query.
    async fn read(
        &self,
        job: &mut Job<'_>,
        manifest: &Manifest,
        parts: &[usize],
        mut done: BTreeSet<usize>,
    ) -> Result<Option<Vec<String>>> {
        let min = u64::from(job.config().catalogues.openalex.min_citations);
        let update = job.run() == Run::Update;
        let todo: Vec<usize> = parts
            .iter()
            .copied()
            .filter(|i| !done.contains(i))
            .collect();
        // The distinct dates of the files to read, in order; a file may write once every file
        // of an earlier date is written.
        let dates: Vec<String> = todo
            .iter()
            .map(|i| manifest.works[*i].date.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let rank = |i: usize| dates.binary_search(&manifest.works[i].date).unwrap_or(0);
        let mut left_per_date = vec![0usize; dates.len()];
        for i in &todo {
            left_per_date[rank(*i)] += 1;
        }
        // The works with a card: in an update every stored one; in a full import those this
        // generation wrote (an older one goes with the import's sweep at its end).
        // Sized at once for the cards stored (it grows past them without a doubling of the set).
        let stored = usize::try_from(job.state().cards.max(0)).unwrap_or(0);
        let mut known: HashSet<u64> = HashSet::with_capacity(stored.min(64_000_000) + stored / 8);
        {
            let generation = (!update).then_some(job.generation());
            let mut rows = sqlx::query_scalar::<_, String>("SELECT source_id FROM catalogue_card WHERE catalogue='openalex' AND ($1::bigint IS NULL OR seen_generation=$1)")
                .bind(generation)
                .fetch(job.pool());
            use futures_util::TryStreamExt;
            while let Some(id) = rows.try_next().await? {
                if let Some(n) = work_number(&id) {
                    known.insert(n);
                }
            }
        }
        let (open_tx, open_rx) = tokio::sync::watch::channel(first_open(&left_per_date));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(4 * STREAMS);
        let streams = Arc::new(tokio::sync::Semaphore::new(STREAMS));
        let buffered = Arc::new(tokio::sync::Semaphore::new(BUFFERED));
        let handle = tokio::runtime::Handle::current();
        let mut inputs = Vec::with_capacity(todo.len());
        for &i in &todo {
            inputs.push((i, rank(i), self.at(&manifest.works[i].path)?));
        }
        // One dispatcher takes the permits in file order: a file waiting to hand over its cards
        // holds a buffer place, never a stream, and every earlier file already has its places.
        let client = self.client.clone();
        let dispatch_tx = tx.clone();
        let dispatcher = AbortOnDrop(tokio::spawn(async move {
            for (i, my_rank, input) in inputs {
                let Ok(place) = buffered.clone().acquire_owned().await else {
                    return;
                };
                let Ok(stream) = streams.clone().acquire_owned().await else {
                    return;
                };
                // The run has stopped (its receiver is gone): no more files are opened.
                if dispatch_tx.is_closed() {
                    return;
                }
                let client = client.clone();
                let tx = dispatch_tx.clone();
                let handle = handle.clone();
                let open = open_rx.clone();
                tokio::spawn(async move {
                    let done_tx = tx.clone();
                    let read = tokio::task::spawn_blocking(move || -> Result<()> {
                        if tx.is_closed() {
                            return Ok(());
                        }
                        let mut stream = Some(stream);
                        let mut gate = Gate {
                            open,
                            rank: my_rank,
                            handle: &handle,
                        };
                        read_part(
                            &input,
                            &client,
                            &handle,
                            min,
                            HELD_CARDS * HELD_IDS_PER_CARD,
                            &mut gate,
                            &mut || drop(stream.take()),
                            &mut |m| tx.blocking_send(m).is_ok(),
                        )
                    })
                    .await;
                    let ended = match read {
                        Ok(Ok(())) => Message::End(i),
                        Ok(Err(error)) => Message::Failed(error),
                        Err(_) => Message::Failed(Failure::new("panic").into()),
                    };
                    let _ = done_tx.send(ended).await;
                    drop(place);
                });
            }
        }));
        drop(tx);
        let snapshot = manifest.date.clone();
        let mut sink = Sink::new(job, None);
        // An update taken up after its removals began keeps saying so in every cursor it saves,
        // so it ends with the pass over every DOI however often it is cut off ([`REMOVING`]).
        let removals_began = update && removed_before(job);
        let cursor = |done: &BTreeSet<usize>| {
            let mut c = json!({"snapshot": snapshot, "done": done});
            if removals_began {
                c["phase"] = json!(REMOVING);
            }
            c
        };
        // An update's removals wait for its end, so it can be refused before removing anything
        // when they are more than a tenth of the cards (a work written again later leaves them).
        let held = job.state().cards.max(0) as u64;
        let mut pending: HashSet<u64> = HashSet::new();
        let mut ended = 0;
        let result: Result<Option<Vec<String>>> = async {
            // The DOIs an update writes (whose holders `share_dois` looks at); a full import looks
            // at every one.
            let mut dois: Option<Vec<String>> = update.then(Vec::new);
            while let Some(message) = rx.recv().await {
                match message {
                    Message::Cards(cards) => {
                        for card in &cards {
                            if let Some(n) = work_number(&card.source_id) {
                                known.insert(n);
                                pending.remove(&n);
                            }
                        }
                        if let Some(list) = &mut dois {
                            list.extend(
                                cards
                                    .iter()
                                    .flat_map(|c| c.keys.iter())
                                    .filter(|k| k.starts_with("doi:"))
                                    .cloned(),
                            );
                            // Past a million, every DOI is looked at (as in a full import).
                            if list.len() > 1_000_000 {
                                dois = None;
                            }
                        }
                        sink.write(job, cards, cursor(&done)).await?;
                    }
                    Message::Gone(ids) => {
                        let hits: Vec<u64> = ids.into_iter().filter(|n| known.remove(n)).collect();
                        if update {
                            pending.extend(hits);
                        } else if !hits.is_empty() {
                            let ids: Vec<String> = hits.iter().map(|n| format!("W{n}")).collect();
                            job.remove(&ids, cursor(&done)).await?;
                        }
                    }
                    Message::End(i) => {
                        done.insert(i);
                        ended += 1;
                        if !update {
                            job.remove(&[], cursor(&done)).await?;
                        }
                        left_per_date[rank(i)] -= 1;
                        let _ = open_tx.send(first_open(&left_per_date));
                    }
                    Message::Failed(error) => return Err(error),
                }
            }
            if ended != todo.len() {
                anyhow::bail!(Failure::new("download"));
            }
            if diff::too_many(pending.len() as u64, held) {
                tracing::warn!(
                    catalogue = "openalex",
                    gone = pending.len(),
                    held,
                    "catalogue update refused: it would remove more than a tenth of the cards"
                );
                let note = format!("snapshot {snapshot}: {} cards gone, over a tenth", pending.len());
                return Err(job.refuse(json!(snapshot), "refused", note));
            }
            if !pending.is_empty() {
                let ids: Vec<String> = pending.drain().map(|n| format!("W{n}")).collect();
                // The DOIs the removed works held: another work that gave one up may take it back.
                if let Some(list) = &mut dois {
                    let held: Vec<String> = sqlx::query_scalar(
                        "SELECT k FROM catalogue_card c CROSS JOIN LATERAL unnest(c.keys) AS k WHERE c.catalogue='openalex' AND c.source_id=ANY($1) AND k LIKE 'doi:%'",
                    )
                    .bind(&ids)
                    .fetch_all(job.pool())
                    .await?;
                    list.extend(held);
                }
                // The cursor that says the removals began goes with the first one, so an update
                // cut off anywhere after it ends with a pass over every DOI ([`REMOVING`]).
                let removing = json!({"snapshot": snapshot, "phase": REMOVING});
                let (first, rest) = ids.split_at(1);
                job.remove(first, removing.clone()).await?;
                job.remove(rest, removing).await?;
            }
            Ok(dois)
        }
        .await;
        // Stop every reader still going (a failure): they see the channel and the gate closed at
        // their next part. A run cut off (its future dropped) drops these too.
        drop(dispatcher);
        drop(rx);
        drop(open_tx);
        result
    }
}
/// The cursor's `phase` once an update's removals began: an update taken up from it may have
/// removed holders of DOIs whose other holders `share_dois` has not looked at yet, so it ends
/// with a pass over every card's DOIs.
const REMOVING: &str = "removing";
/// Whether the run was taken up from a cursor that says its removals began ([`REMOVING`]).
fn removed_before(job: &Job<'_>) -> bool {
    job.cursor()
        .is_some_and(|c| c.get("phase").and_then(Value::as_str) == Some(REMOVING))
}
/// Which DOIs [`share_dois`] looks at.
pub enum DoiScope<'a> {
    /// The cards a full import wrote (its generation): the others go with its end.
    Generation(i64),
    /// The DOIs an update wrote or removed a holder of.
    Dois(&'a [String]),
}
/// One card per DOI: two works that share a DOI (a duplicate OpenAlex keeps) would both hold its
/// link and keys, and a shared DOI link would join neither. Over every holder of each DOI in
/// `scope`, the most-cited (then the lowest ID) keeps them; each other one takes its openalex.org
/// page as its link, drops the keys its DOI link gives and remembers them (`details.shared_doi`),
/// and gets them back once it is the best holder left. A moved card's content hash changes, so
/// its links are checked again and the text index reads it again; the next run that reads its
/// work writes it whole and the pass decides again. A full import looks at its own cards (a
/// whole pass); an update at the DOIs it wrote or removed a holder of, through the `keys` index
/// and the index on the DOIs given up.
pub async fn share_dois(pool: &sqlx::PgPool, scope: DoiScope<'_>) -> Result<u64> {
    let (generation, dois): (Option<i64>, Option<&[String]>) = match scope {
        DoiScope::Generation(g) => (Some(g), None),
        DoiScope::Dois([]) => return Ok(0),
        DoiScope::Dois(d) => (None, Some(d)),
    };
    // Each holder of a DOI (a card with its key, or one that gave it up), ranked; the rows to
    // move: a holder past the first that still holds it, a first that gave it up.
    type Move = (
        i64,
        String,
        String,
        String,
        Vec<String>,
        bool,
        Option<Value>,
    );
    let moves: Vec<Move> = sqlx::query_as(
        "WITH holders AS (\
           SELECT k AS doi, c.id, c.source_id, c.fame, false AS gave_up FROM catalogue_card c CROSS JOIN LATERAL unnest(c.keys) AS k \
            WHERE c.catalogue='openalex' AND k LIKE 'doi:%' \
              AND ($1::bigint IS NULL OR c.seen_generation=$1) AND ($2::text[] IS NULL OR (c.keys && $2 AND k = ANY($2))) \
           UNION \
           SELECT c.details->'shared_doi'->>'key', c.id, c.source_id, c.fame, true FROM catalogue_card c \
            WHERE c.catalogue='openalex' AND c.details ? 'shared_doi' \
              AND ($1::bigint IS NULL OR c.seen_generation=$1) AND ($2::text[] IS NULL OR c.details->'shared_doi'->>'key' = ANY($2))), \
         ranked AS (SELECT doi, id, gave_up, row_number() OVER (PARTITION BY doi ORDER BY fame DESC, source_id) AS n FROM holders) \
         SELECT c.id, c.source_id, r.doi, c.uri, c.keys, r.gave_up, c.details->'shared_doi' FROM ranked r JOIN catalogue_card c ON c.id=r.id \
          WHERE (r.n>1 AND NOT r.gave_up AND NOT c.details ? 'shared_doi') OR (r.n=1 AND r.gave_up)",
    )
    .bind(generation)
    .bind(dois)
    .fetch_all(pool)
    .await?;
    let mut moved = 0u64;
    for (id, source_id, doi, uri, keys, gave_up, shared) in moves {
        if gave_up {
            // The best holder left: its link and the keys it gave up back.
            let Some(shared) = shared else { continue };
            let back_uri = shared
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or(&uri)
                .to_owned();
            let mut back: Vec<String> = keys.clone();
            for k in shared
                .get("keys")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !back.iter().any(|b| b == k) {
                    back.push(k.to_owned());
                }
            }
            sqlx::query("UPDATE catalogue_card SET uri=$2, keys=$3, details=details-'shared_doi', content_hash=sha256(content_hash||'\\x01'::bytea), updated_at=now() WHERE id=$1")
                .bind(id)
                .bind(&back_uri)
                .bind(&back)
                .execute(pool)
                .await?;
        } else {
            // Every key its DOI link gives (the DOI, the page's link key), as the link is
            // written and as it is stored.
            let page = format!("https://doi.org/{}", doi.trim_start_matches("doi:"));
            let mut of_link: Vec<String> = vec![doi.clone()];
            for link in [page.as_str(), uri.as_str()] {
                of_link.extend(super::join::link_keys(link).into_iter().map(|(k, _)| k));
                of_link.extend(super::link_key(link));
            }
            let (given, kept): (Vec<String>, Vec<String>) =
                keys.into_iter().partition(|k| of_link.contains(k));
            let shared = json!({"key": doi, "uri": uri, "keys": given});
            sqlx::query("UPDATE catalogue_card SET uri=$2, keys=$3, details=details||jsonb_build_object('shared_doi',$4::jsonb), content_hash=sha256(content_hash||'\\x02'::bytea), updated_at=now() WHERE id=$1")
                .bind(id)
                .bind(format!("https://openalex.org/{source_id}"))
                .bind(&kept)
                .bind(&shared)
                .execute(pool)
                .await?;
        }
        moved += 1;
    }
    if moved > 0 {
        tracing::info!(
            catalogue = "openalex",
            moved,
            "works that share a DOI: the most cited holds it"
        );
    }
    Ok(moved)
}
/// A reader that fails once it has given `left` bytes (and says so in `over`).
struct Capped<R> {
    inner: R,
    left: u64,
    over: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl<R: std::io::Read> std::io::Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        match self.left.checked_sub(n as u64) {
            Some(left) => {
                self.left = left;
                Ok(n)
            }
            None => {
                self.over.store(true, std::sync::atomic::Ordering::Relaxed);
                Err(std::io::Error::other("over the unpacked cap"))
            }
        }
    }
}
/// The first date that still has files to write (every date before it is written).
fn first_open(left_per_date: &[usize]) -> usize {
    left_per_date
        .iter()
        .position(|n| *n > 0)
        .unwrap_or(left_per_date.len())
}
/// What a file's reader sends.
enum Message {
    Cards(Vec<Card>),
    /// The numbers of works in the file that do not qualify (those with a card lose it).
    Gone(Vec<u64>),
    End(usize),
    Failed(anyhow::Error),
}
/// Files read and waiting to hand over their cards, at most (beside the [`STREAMS`] being read).
pub const BUFFERED: usize = 2 * STREAMS;
/// Cards a file holds back while an earlier date is written before it waits instead. The IDs
/// of works that don't qualify it holds count toward it, [`HELD_IDS_PER_CARD`] to a card.
pub const HELD_CARDS: usize = 50_000;
/// Held IDs of works that don't qualify that count as one held card: an ID is held as an 8-byte
/// number, a card takes a few hundred bytes.
pub const HELD_IDS_PER_CARD: usize = 32;
/// A work ID's number (`W123` is 123).
fn work_number(id: &str) -> Option<u64> {
    work_id(id)?.strip_prefix('W')?.parse().ok()
}
/// Whether a file of date `rank` may hand over its cards: every earlier date is written.
struct Gate<'a> {
    open: tokio::sync::watch::Receiver<usize>,
    rank: usize,
    handle: &'a tokio::runtime::Handle,
}
impl Gate<'_> {
    fn is_open(&self) -> bool {
        *self.open.borrow() >= self.rank
    }
    /// Whether the run is over (its gate's sender gone): a reader stops at once.
    fn closed(&self) -> bool {
        self.open.has_changed().is_err()
    }
    /// Wait until it opens; false when the run has stopped.
    fn wait(&mut self) -> bool {
        let rank = self.rank;
        self.handle
            .block_on(self.open.wait_for(|open| *open >= rank))
            .is_ok()
    }
}

/// Drops (aborts) its task with it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Read one works file: its lines (each within [`LINE_CAP`]), the works that qualify as cards,
/// and the numbers of those that don't. While `gate` is open the parts go out as they are read;
/// before, they are held (up to `hold` IDs, a card counting [`HELD_IDS_PER_CARD`]; then the
/// reader waits), and the file's stream is given back (`read_done`) once it is read whole.
#[allow(clippy::too_many_arguments)]
fn read_part(
    input: &Input,
    client: &fetch::Client,
    handle: &tokio::runtime::Handle,
    min: u64,
    hold: usize,
    gate: &mut Gate<'_>,
    read_done: &mut dyn FnMut(),
    send: &mut dyn FnMut(Message) -> bool,
) -> Result<()> {
    let body = input.open(client, handle, PART_MAX)?;
    let over = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gz = Capped {
        inner: flate2::read::MultiGzDecoder::new(body),
        left: UNPACKED_MAX,
        over: over.clone(),
    };
    let mut lines = bulk::Lines::new(BufReader::with_capacity(1 << 20, gz), LINE_CAP);
    let mut line = Vec::new();
    let mut held: Vec<Message> = Vec::new();
    // What is held, in IDs: a card counts `HELD_IDS_PER_CARD`.
    let mut held_ids = 0usize;
    let mut cards = Vec::new();
    let mut gone = Vec::new();
    let (mut lines_read, mut broken) = (0u64, 0u64);
    // Hand over what is held and the current parts, or hold them.
    let mut out =
        |held: &mut Vec<Message>, held_ids: &mut usize, part: Vec<Message>, last: bool| -> bool {
            let n: usize = part
                .iter()
                .map(|m| match m {
                    Message::Cards(c) => c.len().saturating_mul(HELD_IDS_PER_CARD),
                    Message::Gone(g) => g.len(),
                    _ => 0,
                })
                .fold(0, usize::saturating_add);
            held.extend(part.into_iter().filter(|m| match m {
                Message::Cards(c) => !c.is_empty(),
                Message::Gone(g) => !g.is_empty(),
                _ => true,
            }));
            *held_ids = held_ids.saturating_add(n);
            if gate.closed() {
                return false;
            }
            if !gate.is_open() && !last && *held_ids < hold {
                return true;
            }
            if !gate.is_open() && !gate.wait() {
                return false;
            }
            *held_ids = 0;
            held.drain(..).all(&mut *send)
        };
    loop {
        let more = lines.next_line(&mut line).map_err(|_| {
            let class = if over.load(std::sync::atomic::Ordering::Relaxed) {
                "size"
            } else {
                "download"
            };
            Failure::new(class).at(input.at())
        })?;
        if !more {
            break;
        }
        lines_read += 1;
        match work(&line, min) {
            Read::Card(card) => cards.push(*card),
            Read::Gone(id) => gone.extend(work_number(&id)),
            Read::Broken => {
                broken += 1;
                // More than 100 lines that don't parse, and more than one in a thousand: a
                // broken file, not a few odd records.
                if broken > BROKEN_FLOOR && broken * 1000 > lines_read {
                    anyhow::bail!(Failure::new("parse").at(input.at()));
                }
            }
            Read::Unread => {}
        }
        if cards.len() >= fetch::PART || gone.len() >= 10 * fetch::PART {
            let part = vec![
                Message::Cards(std::mem::take(&mut cards)),
                Message::Gone(std::mem::take(&mut gone)),
            ];
            if !out(&mut held, &mut held_ids, part, false) {
                return Ok(());
            }
        }
    }
    read_done();
    let part = vec![
        Message::Cards(std::mem::take(&mut cards)),
        Message::Gone(std::mem::take(&mut gone)),
    ];
    out(&mut held, &mut held_ids, part, true);
    crate::telemetry::catalogue_items_skipped("openalex", "bad_line", broken);
    crate::telemetry::catalogue_items_skipped("openalex", "oversized_line", lines.too_long());
    if broken > 0 {
        tracing::warn!(catalogue = "openalex", file = %input.at(), broken, "works that do not parse were passed over (their cards stay)");
    }
    if lines.too_long() > 0 {
        tracing::warn!(catalogue = "openalex", file = %input.at(), skipped = lines.too_long(), "works longer than the record cap were skipped");
    }
    Ok(())
}

/// What one line is.
#[derive(Debug)]
pub enum Read {
    Card(Box<Card>),
    /// A work that does not qualify (cited too little, or front matter), by its ID.
    Gone(String),
    /// Not a work this reader can read.
    Unread,
    /// A line that may qualify but does not parse.
    Broken,
}
/// The highest `"cited_by_count"` in a line: at least the work's own (its yearly counts are
/// lower), so a line whose highest is too low is passed over without parsing it.
pub fn highest_count(line: &[u8]) -> Option<u64> {
    const NEEDLE: &[u8] = b"\"cited_by_count\":";
    let mut best = None;
    for at in memchr::memmem::find_iter(line, NEEDLE) {
        let rest = &line[at + NEEDLE.len()..];
        let rest = &rest[rest.iter().take_while(|b| b.is_ascii_whitespace()).count()..];
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if let Some(n) = std::str::from_utf8(&rest[..digits.min(20)])
            .ok()
            .and_then(|d| d.parse::<u64>().ok())
        {
            best = Some(best.map_or(n, |b: u64| b.max(n)));
        }
    }
    best
}
/// The work ID a line starts with (`{"id":"https://openalex.org/W…"`), without parsing it.
fn leading_id(line: &[u8]) -> Option<String> {
    // A work's own ID is the only `"id"` naming an openalex.org `W…` (authors are `A…`, sources
    // `S…`, concepts `C…`).
    const NEEDLE: &[u8] = b"\"id\":\"https://openalex.org/W";
    let at = memchr::memmem::find(line, NEEDLE)?;
    let rest = &line[at + NEEDLE.len() - 1..];
    let end = rest.iter().position(|b| *b == b'"')?;
    let id = std::str::from_utf8(&rest[..end]).ok()?;
    work_id(id)
}
fn work_id(id: &str) -> Option<String> {
    let digits = id.strip_prefix('W')?;
    ((1..=12).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.to_owned())
}
/// One line of a works file.
pub fn work(line: &[u8], min: u64) -> Read {
    match highest_count(line) {
        // No count at all: not a work this reader can judge; its card (if any) stays.
        None => return Read::Unread,
        Some(n) if n <= min => return leading_id(line).map_or(Read::Unread, Read::Gone),
        Some(_) => {}
    }
    // A line that may qualify but does not parse removes nothing: it is counted and its card
    // (if any) stays.
    let Ok(work) = serde_json::from_slice::<Work>(line) else {
        return Read::Broken;
    };
    let Some(id) = work
        .id
        .strip_prefix("https://openalex.org/")
        .and_then(work_id)
    else {
        return Read::Unread;
    };
    if work.cited_by_count <= min || work.is_paratext {
        return Read::Gone(id);
    }
    match card(&id, &work) {
        Some(card) => Read::Card(Box::new(card)),
        None => Read::Gone(id),
    }
}

/// The fields of a work a card reads; the rest is skipped.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Work {
    #[serde(deserialize_with = "nullable")]
    id: String,
    doi: Option<String>,
    display_name: Option<String>,
    title: Option<String>,
    publication_year: Option<i64>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(deserialize_with = "nullable")]
    cited_by_count: u64,
    #[serde(deserialize_with = "nullable")]
    ids: Ids,
    primary_location: Option<Location>,
    #[serde(deserialize_with = "nullable")]
    locations: Vec<Option<Location>>,
    #[serde(deserialize_with = "nullable")]
    authorships: Vec<Option<Authorship>>,
    authors_count: Option<u64>,
    #[serde(deserialize_with = "nullable")]
    topics: Vec<Option<Topic>>,
    #[serde(deserialize_with = "nullable")]
    keywords: Vec<Option<Named>>,
    #[serde(deserialize_with = "nullable")]
    is_retracted: bool,
    #[serde(deserialize_with = "nullable")]
    is_paratext: bool,
}
/// A field whose `null` reads as its default (OpenAlex writes `null` for an empty list).
fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Ids {
    pmid: Option<String>,
    pmcid: Option<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Location {
    landing_page_url: Option<String>,
    pdf_url: Option<String>,
    source: Option<Named>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Authorship {
    author: Option<Named>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Named {
    display_name: Option<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Topic {
    display_name: Option<String>,
    subfield: Option<Named>,
    field: Option<Named>,
}
/// The schema.org type of an OpenAlex work type.
pub fn schema_type(kind: &str) -> &'static str {
    match kind {
        "book" | "edited-book" | "monograph" => "Book",
        "book-chapter" | "book-section" | "book-part" => "Chapter",
        "dataset" => "Dataset",
        "dissertation" => "Thesis",
        "report" => "Report",
        "reference-entry" | "paratext" | "standard" | "other" | "libguides" | "grant" => {
            "CreativeWork"
        }
        _ => "ScholarlyArticle",
    }
}
/// A work's fame: ln(1 + citations) / ln(1 + 100,000), at most 1.
pub fn citation_fame(citations: u64) -> f32 {
    ((1.0 + citations as f64).ln() / (1.0 + 100_000_f64).ln()).min(1.0) as f32
}
/// A title as plain text: the markup OpenAlex keeps in some (`<i>`, `<sub>`, `<scp>`) taken out.
fn plain_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut chars = title.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            // A tag: `<` then up to 12 letters, digits or `/`, then `>`.
            let tag: String = chars.clone().take(14).collect();
            if let Some(end) = tag.find('>') {
                let inside = &tag[..end];
                if !inside.is_empty()
                    && inside.len() <= 12
                    && inside
                        .trim_start_matches('/')
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric())
                {
                    for _ in 0..=end {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}
/// The card of a work with ID `id`, or none when it has no title.
fn card(id: &str, work: &Work) -> Option<Card> {
    use crate::lookups::papers;
    let cut = |t: &str| {
        let mut t = t.to_owned();
        crate::model::truncate(&mut t, TEXT_CAP);
        t
    };
    let name = plain_title(&cut(work
        .display_name
        .as_deref()
        .or(work.title.as_deref())?));
    if !super::usable_name(&name) {
        return None;
    }
    let page = format!("https://openalex.org/{id}");
    let doi = work.doi.as_deref().and_then(crate::resolver::cards::doi);
    let uri = doi
        .as_deref()
        .map_or_else(|| page.clone(), crate::resolver::cards::doi_page);
    let mut keys: Vec<String> = super::join::link_keys(&uri)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    keys.push(format!("openalex:{id}"));
    if let Some(doi) = &doi {
        keys.push(format!("doi:{doi}"));
    }
    if let Some(pmid) = work
        .ids
        .pmid
        .as_deref()
        .and_then(|p| p.trim_end_matches('/').rsplit('/').next())
        .filter(|p| crate::lookups::digits(p))
    {
        keys.push(format!("pmid:{pmid}"));
    }
    if let Some(pmcid) = work
        .ids
        .pmcid
        .as_deref()
        .and_then(|p| p.trim_end_matches('/').rsplit('/').next())
        .filter(|p| p.strip_prefix("PMC").is_some_and(crate::lookups::digits))
    {
        keys.push(format!("pmcid:{pmcid}"));
    }
    let arxiv = work
        .locations
        .iter()
        .flatten()
        .take(64)
        .flat_map(|l| [l.landing_page_url.as_deref(), l.pdf_url.as_deref()])
        .flatten()
        .chain(work.doi.as_deref())
        .find_map(papers::arxiv_from_uri);
    if let Some(arxiv) = &arxiv {
        keys.push(papers::arxiv_key(arxiv).to_string());
    }
    let venue = work
        .primary_location
        .as_ref()
        .and_then(|l| l.source.as_ref())
        .and_then(|s| s.display_name.as_deref().map(cut))
        .filter(|v| !v.trim().is_empty());
    let authors: Vec<String> = work
        .authorships
        .iter()
        .flatten()
        .filter_map(|a| a.author.as_ref()?.display_name.as_deref().map(cut))
        .filter(|n| !n.trim().is_empty())
        .take(3)
        .collect();
    // Its type as OpenAlex names it (a few words); a longer text is no type.
    let kind = work
        .kind
        .as_deref()
        .filter(|k| k.len() <= 64)
        .unwrap_or_default()
        .to_owned();
    let mut description = Vec::new();
    if let Some(year) = work.publication_year.filter(|y| (1000..=3000).contains(y)) {
        description.push(year.to_string());
    }
    if !kind.is_empty() {
        let mut k = kind.replace('-', " ");
        if let Some(first) = k.get(..1) {
            k = first.to_uppercase() + &k[1..];
        }
        description.push(k);
    }
    if let Some(venue) = &venue {
        description.push(venue.clone());
    }
    if !authors.is_empty() {
        let more = work.authors_count.unwrap_or(authors.len() as u64) > authors.len() as u64;
        description.push(format!(
            "{}{}",
            authors.join(", "),
            if more { " et al." } else { "" }
        ));
    }
    let mut labels = Vec::new();
    // A label over 20 characters is dropped anyway (`catalogue::label`): 64 bytes are plenty.
    let short = |t: &Option<String>| t.as_deref().filter(|t| t.len() <= 64).map(str::to_owned);
    for topic in work.topics.iter().flatten().take(16) {
        labels.extend(short(&topic.display_name));
        labels.extend(topic.subfield.as_ref().and_then(|s| short(&s.display_name)));
        labels.extend(topic.field.as_ref().and_then(|s| short(&s.display_name)));
    }
    labels.extend(
        work.keywords
            .iter()
            .flatten()
            .take(32)
            .filter_map(|k| short(&k.display_name)),
    );
    let mut details = serde_json::Map::new();
    details.insert("cited_by".into(), json!(work.cited_by_count));
    if let Some(year) = work.publication_year.filter(|y| (1000..=3000).contains(y)) {
        details.insert("year".into(), json!(year));
    }
    if !kind.is_empty() {
        details.insert("work_type".into(), json!(kind));
    }
    if let Some(venue) = venue.filter(|v| v.len() <= 512) {
        details.insert("venue".into(), json!(venue));
    }
    if !authors.is_empty() {
        details.insert("authors".into(), json!(authors));
    }
    if uri != page {
        details.insert("openalex".into(), json!(page));
    }
    if work.is_retracted {
        details.insert("retracted".into(), json!(true));
    }
    Some(Card {
        source_id: id.to_owned(),
        uri,
        r#type: schema_type(&kind).into(),
        name,
        description: (!description.is_empty()).then(|| description.join(" · ")),
        labels,
        keys,
        fame: citation_fame(work.cited_by_count),
        details,
        ..Card::default()
    })
}

/// Where a run is: the snapshot it reads and the files it has written.
fn cursor_done(job: &Job<'_>, snapshot: &str) -> BTreeSet<usize> {
    job.cursor()
        .filter(|c| c.get("snapshot").and_then(Value::as_str) == Some(snapshot))
        .and_then(|c| c.get("done"))
        .and_then(|d| serde_json::from_value(d.clone()).ok())
        .unwrap_or_default()
}
/// Where the next update starts: the snapshot read, the last date read and that date's file
/// list, and the threshold the cards were read with.
fn position(manifest: &Manifest, min: u32) -> Value {
    let through = manifest
        .works
        .last()
        .map(|p| p.date.clone())
        .unwrap_or_default();
    json!({
        "snapshot": manifest.date,
        "through": through,
        "through_files": files_digest(&manifest.works, &through),
        "min_citations": min,
    })
}
fn snapshot_time(manifest: &Manifest) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::NaiveDate::parse_from_str(&manifest.date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
}

#[async_trait]
impl Importer for OpenAlex {
    fn catalogue(&self) -> Catalogue {
        Catalogue::Openalex
    }
    fn source(&self) -> Option<String> {
        diff::source_of(&self.base, BUCKET)
    }
    /// None: the files are read as streams, and nothing is written under `catalogues.dir` (the
    /// cards' share of the database is sized apart: it may be on another disk).
    fn disk_need(&self, _run: Run, _state: &State) -> u64 {
        0
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        let manifest = self.manifest().await?;
        // A snapshot that holds no work (a manifest with no works file) reads no record: the
        // runner refuses it (`empty`), and it is not read again.
        job.reading(json!(manifest.date));
        if job.refused_before(&json!(manifest.date)) {
            return Err(job.still_refused(format!("snapshot {}", manifest.date)));
        }
        let all: Vec<usize> = (0..manifest.works.len()).collect();
        let done = cursor_done(job, &manifest.date);
        self.read(job, &manifest, &all, done).await?;
        share_dois(job.pool(), DoiScope::Generation(job.generation())).await?;
        let min = job.config().catalogues.openalex.min_citations;
        job.set_position(position(&manifest, min));
        if let Some(at) = snapshot_time(&manifest) {
            job.set_current_as_of(at);
        }
        job.upstream_version(&manifest.date).await?;
        Ok(())
    }
    /// An update; a full import instead when one was asked for (`--request`), when
    /// `min_citations` changed, or when a new snapshot comes [`FULL_EVERY`] after the last full
    /// import. A snapshot read last, or one refused, is not read again.
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        if job.state().import_requested.is_some() {
            return Ok(UpdateEnd::NeedsImport(super::musicbrainz::REQUESTED));
        }
        let min = job.config().catalogues.openalex.min_citations;
        let last = job.position().cloned().unwrap_or_default();
        if last.get("min_citations").and_then(Value::as_u64) != Some(u64::from(min)) {
            return Ok(UpdateEnd::NeedsImport("openalex.min_citations changed"));
        }
        // Taken up after its removals began: the DOIs of the works removed are not known any
        // more, so the pass looks at every DOI.
        let whole_pass = removed_before(job);
        let manifest = self.manifest().await?;
        if last.get("snapshot").and_then(Value::as_str) == Some(manifest.date.as_str()) {
            return Ok(UpdateEnd::Done);
        }
        if job.refused_before(&json!(manifest.date)) {
            return Err(job.still_refused(format!("snapshot {}", manifest.date)));
        }
        if job
            .state()
            .last_import_at
            .is_some_and(|at| chrono::Utc::now() - at >= FULL_EVERY)
        {
            return Ok(UpdateEnd::NeedsImport(FULL_AGAIN));
        }
        let through = last.get("through").and_then(Value::as_str).unwrap_or("");
        let same_through = last.get("through_files").and_then(Value::as_str)
            == Some(files_digest(&manifest.works, through).as_str());
        let parts: Vec<usize> = (0..manifest.works.len())
            .filter(|&i| {
                let day = manifest.works[i].date.as_str();
                day > through || (day == through && !same_through)
            })
            .collect();
        // An update cut off reads its files again: its removals are decided over all of them.
        let dois = self
            .read(job, &manifest, &parts, BTreeSet::new())
            .await?
            .filter(|_| !whole_pass);
        match &dois {
            Some(dois) => share_dois(job.pool(), DoiScope::Dois(dois)).await?,
            // Past a million DOIs written, or taken up after its removals: a whole pass.
            None => share_dois(job.pool(), DoiScope::Generation(job.generation())).await?,
        };
        job.set_position(position(&manifest, min));
        if let Some(at) = snapshot_time(&manifest) {
            job.set_current_as_of(at);
        }
        job.upstream_version(&manifest.date).await?;
        Ok(UpdateEnd::Done)
    }
}

/// The importer `pinakesd` runs: the public bucket; none without the operator's contact.
pub fn importer(config: &Config) -> Option<Arc<dyn Importer>> {
    if config.egress.contact.is_empty() {
        if !config.catalogues.enabled(Catalogue::Openalex) {
            return None;
        }
        tracing::warn!(
            catalogue = "openalex",
            "the OpenAlex importer starts only with http.contact set (the bucket is asked with the operator's contact)"
        );
        return None;
    }
    let base = Input::Url(url::Url::parse(BUCKET).expect("the bucket's URL"));
    match OpenAlex::new(base, &config.egress.contact) {
        Ok(importer) => Some(Arc::new(importer)),
        Err(error) => {
            tracing::warn!(catalogue = "openalex", error = %crate::resolver::error_class(&error), "the OpenAlex importer did not start");
            None
        }
    }
}
/// A local snapshot directory as an importer's root (`--from DIR`).
pub fn local(dir: PathBuf) -> Input {
    Input::File(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/catalogue/openalex/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn the_manifest_lists_the_bucket_s_works_files_in_date_order() {
        let m = manifest(&fixture("manifest.json")).unwrap();
        assert_eq!(m.date, "2026-10-14");
        assert_eq!(m.works.len(), 6);
        assert!(m.works.windows(2).all(|w| w[0] <= w[1]));
        assert!(m
            .works
            .iter()
            .all(|p| p.path.starts_with("data/jsonl/works/updated_date=")));
        for bad in [
            r#"{"date":"2026-10-14","entities":[{"entity":"works","files":[{"url":"s3://openalex/data/jsonl/works/updated_date=2026-10-14/../../x.gz"}]}]}"#,
            r#"{"date":"2026-10-14","entities":[{"entity":"works","files":[{"url":"https://evil.example/part_0000.gz"}]}]}"#,
            r#"{"date":"yesterday","entities":[]}"#,
            "not json",
        ] {
            assert!(manifest(bad.as_bytes()).is_none(), "{bad}");
        }
    }

    /// A file waiting for an earlier date holds the IDs of its works that don't qualify toward
    /// its hold: past it the reader waits (its stream not given back) instead of reading on,
    /// and hands everything over once the gate opens.
    #[test]
    fn held_ids_that_dont_qualify_count_toward_the_hold() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part_0000.gz");
        let works = 3 * 10 * fetch::PART;
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        for n in 1..=works {
            writeln!(
                gz,
                r#"{{"id":"https://openalex.org/W{n}","cited_by_count":1}}"#
            )
            .unwrap();
        }
        gz.finish().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let handle = runtime.handle().clone();
        let (open_tx, open_rx) = tokio::sync::watch::channel(0usize);
        let read_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sent = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let reader = {
            let (read_done, sent) = (read_done.clone(), sent.clone());
            let input = Input::File(path);
            std::thread::spawn(move || {
                let mut gate = Gate {
                    open: open_rx,
                    rank: 1,
                    handle: &handle,
                };
                read_part(
                    &input,
                    &super::fetch::client("test", &[]).unwrap(),
                    &handle,
                    50,
                    // Two cards' worth: less than one part of IDs.
                    2 * HELD_IDS_PER_CARD,
                    &mut gate,
                    &mut || read_done.store(true, std::sync::atomic::Ordering::SeqCst),
                    &mut |m| {
                        match m {
                            Message::Gone(ids) => sent.lock().unwrap().extend(ids),
                            Message::Cards(cards) => assert!(cards.is_empty()),
                            _ => {}
                        }
                        true
                    },
                )
            })
        };
        // The reader stops at its first part: it neither reads the file whole nor sends.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && !read_done.load(std::sync::atomic::Ordering::SeqCst)
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!read_done.load(std::sync::atomic::Ordering::SeqCst));
        assert!(sent.lock().unwrap().is_empty());
        open_tx.send(1).unwrap();
        reader.join().unwrap().unwrap();
        assert!(read_done.load(std::sync::atomic::Ordering::SeqCst));
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), works);
        assert!(sent.iter().copied().eq(1..=works as u64));
    }

    /// Works cited more than the threshold become cards with their links' keys; the others are
    /// named for removal.
    #[test]
    fn works_become_cards_with_their_keys() {
        let text = fixture("works.jsonl");
        let mut cards = vec![];
        let mut gone = vec![];
        for line in text.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            match work(line, 50) {
                Read::Card(card) => cards.push(*card),
                Read::Gone(id) => gone.push(id),
                Read::Unread | Read::Broken => panic!("unread"),
            }
        }
        assert_eq!(cards.len(), 5);
        assert_eq!(gone, ["W2404603502", "W4230102594"]);
        for card in &cards {
            let row = card.checked(Catalogue::Openalex).unwrap();
            assert!(
                row.keys.contains(&format!("openalex:{}", card.source_id)),
                "{:?}",
                row.keys
            );
            assert!(row.fame > 0.3 && row.fame <= 1.0);
        }
        let kidney = cards.iter().find(|c| c.source_id == "W4225145323").unwrap();
        let row = kidney.checked(Catalogue::Openalex).unwrap();
        assert!(
            row.uri.contains("10.1016/j.kint.2022.02.042"),
            "{}",
            row.uri
        );
        assert!(row
            .keys
            .contains(&"doi:10.1016/j.kint.2022.02.042".to_owned()));
        assert!(
            row.keys.contains(&"pmid:35490842".to_owned()),
            "{:?}",
            row.keys
        );
        assert!(!row.labels.is_empty());
        let iccv = cards.iter().find(|c| c.source_id == "W3205276578").unwrap();
        assert!(
            iccv.keys.contains(&"arxiv:2104.04167".to_owned()),
            "{:?}",
            iccv.keys
        );
        let no_doi = cards.iter().find(|c| c.source_id == "W2146296031").unwrap();
        assert_eq!(no_doi.uri, "https://openalex.org/W2146296031");
        assert_eq!(
            cards
                .iter()
                .find(|c| c.source_id == "W2148189974")
                .unwrap()
                .r#type,
            "Chapter"
        );
        // A higher threshold names more works for removal.
        assert!(matches!(
            work(text.split(|b| *b == b'\n').next().unwrap(), 100),
            Read::Gone(_)
        ));
    }

    /// A line that may qualify but does not parse, or names no count, removes nothing; `null`
    /// lists read as empty.
    #[test]
    fn a_broken_or_countless_line_removes_nothing() {
        assert!(matches!(
            work(
                br#"{"id":"https://openalex.org/W1","cited_by_count":99,"#,
                50
            ),
            Read::Broken
        ));
        assert!(matches!(
            work(br#"{"id":"https://openalex.org/W1","title":"x"}"#, 50),
            Read::Unread
        ));
        let nulls = br#"{"id":"https://openalex.org/W1","display_name":"T","cited_by_count":99,"topics":null,"keywords":null,"locations":[null],"authorships":null,"ids":null,"is_retracted":null}"#;
        assert!(
            matches!(work(nulls, 50), Read::Card(_)),
            "{:?}",
            work(nulls, 50)
        );
    }

    /// A type over 64 bytes is no type (it is neither shown nor kept); unpacking stops past its
    /// cap with the reader saying so.
    /// A work whose title carries a credential (the table refuses such a name) makes no card.
    #[test]
    fn a_title_with_a_credential_makes_no_card() {
        let text = String::from_utf8(fixture("works.jsonl")).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        for title in ["//:a@&A", "https://user:secret@example.org/"] {
            v["display_name"] = title.into();
            v["title"] = title.into();
            assert!(
                !matches!(work(v.to_string().as_bytes(), 0), Read::Card(_)),
                "{title}"
            );
        }
        v["display_name"] = "A paper".into();
        assert!(matches!(work(v.to_string().as_bytes(), 0), Read::Card(_)));
    }

    #[test]
    fn long_types_and_big_files_are_bounded() {
        let line = with_type(&"x".repeat(100));
        let Read::Card(card) = work(line.as_bytes(), 0) else {
            panic!("a card")
        };
        assert!(
            !card.details.contains_key("work_type"),
            "{:?}",
            card.details
        );
        assert!(!card.description.unwrap_or_default().contains("xxxx"));
        let Read::Card(card) = work(with_type("article").as_bytes(), 0) else {
            panic!("a card")
        };
        assert_eq!(card.details["work_type"], "article");
        use std::io::Read as _;
        let over = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut capped = Capped {
            inner: &[7u8; 100][..],
            left: 64,
            over: over.clone(),
        };
        let mut out = Vec::new();
        assert!(capped.read_to_end(&mut out).is_err());
        assert!(over.load(std::sync::atomic::Ordering::Relaxed));
        let mut within = Capped {
            inner: &[7u8; 100][..],
            left: 100,
            over: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        assert_eq!(within.read_to_end(&mut out).unwrap(), 100);
    }
    /// The fixture's first work with another type.
    fn with_type(kind: &str) -> String {
        let text = String::from_utf8(fixture("works.jsonl")).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        v["type"] = kind.into();
        v.to_string()
    }

    #[test]
    fn the_highest_count_is_found_without_parsing() {
        assert_eq!(
            highest_count(br#"{"cited_by_count":12,"counts_by_year":[{"cited_by_count":3}]}"#),
            Some(12)
        );
        assert_eq!(highest_count(br#"{"cited_by_count": 7}"#), Some(7));
        assert_eq!(highest_count(br#"{"x":1}"#), None);
        assert_eq!(
            highest_count(br#"{"cited_by_count":99999999999999999999999}"#),
            None
        );
        assert_eq!(
            leading_id(br#"{"id":"https://openalex.org/W123","x":1}"#).as_deref(),
            Some("W123")
        );
        assert_eq!(leading_id(br#"{"id":"https://openalex.org/A123"}"#), None);
        // An author's ID before the work's (keys in another order) is passed over.
        assert_eq!(
            leading_id(br#"{"authorships":[{"author":{"id":"https://openalex.org/A1"}}],"id":"https://openalex.org/W7"}"#).as_deref(),
            Some("W7")
        );
    }

    #[test]
    fn titles_lose_their_markup() {
        assert_eq!(
            plain_title("The <i>Drosophila</i> gene H<sub>2</sub>O"),
            "The Drosophila gene H2O"
        );
        assert_eq!(plain_title("a < b and c > d"), "a < b and c > d");
        assert_eq!(plain_title("<"), "<");
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 3000, ..ProptestConfig::default() })]
        /// Fuzz: any line, or a real work cut anywhere or with bytes changed, reads without a
        /// panic, and a card it gives is one the table takes.
        #[test]
        fn any_line_reads_without_a_panic(cut in 0usize..12_000, flip in proptest::collection::vec((0usize..12_000, any::<u8>()), 0..8), noise in proptest::collection::vec(any::<u8>(), 0..300)) {
            let text = fixture("works.jsonl");
            let first = text.split(|b| *b == b'\n').next().unwrap().to_vec();
            let mut line = first[..cut.min(first.len())].to_vec();
            for (at, b) in flip {
                if at < line.len() { line[at] = b; }
            }
            for l in [line, noise] {
                if let Read::Card(card) = work(&l, 0) {
                    let checked = card.checked(Catalogue::Openalex);
                    prop_assert!(checked.is_ok(), "{:?}: {:?}", card, checked.err());
                }
            }
        }
        /// Property: a DOI's canonical link reads back the DOI key, and the OpenAlex page its
        /// work key (`join::link_keys`), whatever the DOI's case.
        #[test]
        fn canonical_links_read_back_their_keys(prefix in "10\\.[0-9]{4,5}", suffix in "[A-Za-z0-9_-][A-Za-z0-9._-]{0,19}", id in 1u64..999_999_999_999) {
            let w = format!("W{id}");
            let line = format!(r#"{{"id":"https://openalex.org/{w}","doi":"https://doi.org/{prefix}/{suffix}","display_name":"T","cited_by_count":60}}"#);
            let Read::Card(card) = work(line.as_bytes(), 50) else { panic!("{line}") };
            let row = card.checked(Catalogue::Openalex).unwrap();
            let read: Vec<String> = super::super::join::link_keys(&row.uri).into_iter().map(|(k, _)| k).collect();
            let doi = format!("doi:{}/{}", prefix, suffix.to_lowercase());
            prop_assert!(read.contains(&doi), "{} reads {:?}", row.uri, read);
            prop_assert!(row.keys.contains(&doi));
            let page: Vec<String> = super::super::join::link_keys(&format!("https://openalex.org/{w}")).into_iter().map(|(k, _)| k).collect();
            prop_assert!(page.iter().all(|k| row.keys.contains(k)), "{:?}", page);
        }
    }
}
