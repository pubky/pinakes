//! The OpenStreetMap catalogue: named places and useful points from a planet file, or from a
//! region's extract (`catalogues.osm.region`, Geofabrik's region files), kept current with the
//! replication change files (the planet's daily ones, or the region's).
//!
//! A full import downloads the file into `catalogues.dir/osm`, then reads it twice:
//!
//! 1. every node's location goes into a node index on disk ([`nodes`]); tagged nodes and ways
//!    the filter keeps ([`place`]) become cards, a way's geometry read from the index; kept
//!    relations are set aside with their members;
//! 2. the ways that are members of a kept relation are read again for their bounding boxes,
//!    and the relations become cards.
//!
//! The file and the index are removed when the import has finished. An update reads the change
//! files after the replication sequence the last run left ([`Job::position`]) and writes the
//! changed cards: a way whose nodes moved without the way itself changing keeps its geometry
//! until the next full import.
//!
//! Besides cards, an update records what the place rules need: a deleted object's successor on
//! evidence (`osm_successor`: the same Wikidata item, or a delete and a create in one changeset
//! with the same name within 50 m), and a kept object's change to an unrelated name
//! (`osm_rename`).
pub mod nodes;
pub mod osc;
pub mod pbf;
pub mod place;
pub mod rules;

use super::{
    failure::Failure,
    run::{Importer, Job, Plan, Run, UpdateEnd},
    store::State,
    Card, Catalogue,
};
use crate::model::Point;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// Cards per write.
const BATCH: usize = 1000;
/// The dense node index's size on disk at the planet's node IDs (about 14 billion in 2026): what
/// the disk checks ask for.
const DENSE_INDEX: u64 = 115_000_000_000;
/// What the dense index may take before the import stops (`disk`): half again its size today,
/// room for node IDs up to about 21 billion.
const DENSE_INDEX_CAP: u64 = DENSE_INDEX / 2 * 3;
/// The most bytes one change file's changes take in memory while it is read (every version, the
/// changed nodes, the untagged IDs; a planet day takes a few hundred MB).
const MAX_CHANGES_HELD: usize = 1 << 30;
/// A file this large gets the dense node index (the planet, a continent).
const DENSE_FROM: u64 = 40_000_000_000;
/// The most members of one relation its geometry is read from.
const GEOMETRY_MEMBERS: usize = 10_000;
/// The most member ways all kept relations' geometry is read from (8 bytes each while gathered,
/// 40 with their boxes: about 2.5 GB at the cap).
const MEMBER_WAYS: usize = 64_000_000;
/// How often a full import reads the file again.
const REIMPORT_EVERY: Duration = Duration::from_secs(30 * 86400);
/// The planet's: a full read is a 96 GB download and hours of work.
const REIMPORT_EVERY_PLANET: Duration = Duration::from_secs(90 * 86400);
/// A change file missing this many sequences behind the server's state is gone for good.
const GONE_AFTER: u64 = 3;
/// The most change files one update applies; the next update goes on.
const FILES_PER_UPDATE: u64 = 30;
/// Within how many metres a successor or a same-named place is looked for.
pub const NEAR_METRES: f64 = 50.0;
/// Within how many metres an object tagged with a deleted one's Wikidata item succeeds it.
pub const WIKIDATA_METRES: f64 = 1000.0;
/// The hosts bulk files are read from: Geofabrik, and the planet's site with the two mirrors it
/// redirects files and change files to (OpenStreetMap's buckets in Frankfurt and Oregon). Exact
/// names, https only.
const HOSTS: &[&str] = &[
    "planet.openstreetmap.org",
    "download.geofabrik.de",
    "osm-planet-eu-central-1.s3.dualstack.eu-central-1.amazonaws.com",
    "osm-planet-us-west-2.s3.dualstack.us-west-2.amazonaws.com",
];

/// Where the import reads its file and its changes from.
#[derive(Clone, Debug, Default)]
pub enum Input {
    /// The configured region's file and change files, downloaded.
    #[default]
    Region,
    /// A local file (`pinakesd catalogue import osm --from`): a PBF file for an import, change
    /// files for an update.
    Files(Vec<PathBuf>),
}

/// The file and change-file addresses of a region (`planet` or a Geofabrik region path).
pub fn sources(region: &str) -> (String, String) {
    if region == "planet" {
        return (
            "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf".into(),
            "https://planet.openstreetmap.org/replication/day".into(),
        );
    }
    let last = region.rsplit('/').next().unwrap_or(region);
    (
        format!("https://download.geofabrik.de/{region}/{last}-latest.osm.pbf"),
        format!("https://download.geofabrik.de/{region}/updates"),
    )
}
/// A change file's path below its replication base: sequence 4979 is `000/004/979`.
pub fn sequence_path(sequence: u64) -> String {
    format!(
        "{:03}/{:03}/{:03}",
        sequence / 1_000_000,
        sequence / 1000 % 1000,
        sequence % 1000
    )
}
/// A replication `state.txt`: its sequence and timestamp.
pub fn parse_state(text: &str) -> Option<(u64, chrono::DateTime<chrono::Utc>)> {
    let mut sequence = None;
    let mut timestamp = None;
    for line in text.lines().take(64) {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("sequenceNumber=") {
            sequence = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("timestamp=") {
            timestamp = chrono::DateTime::parse_from_rfc3339(&v.trim().replace("\\:", ":"))
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc));
        }
    }
    Some((sequence?, timestamp?))
}

/// The importer `pinakesd` runs: the configured region's files; none without the operator's
/// contact (`http.contact`), which every request to the bulk-file hosts names.
pub fn importer(config: &crate::config::Config) -> Option<std::sync::Arc<dyn Importer>> {
    if config.egress.contact.trim().is_empty() {
        if config.catalogues.enabled(Catalogue::Osm) {
            tracing::warn!(
                catalogue = "osm",
                "the osm importer starts only with http.contact set (the bulk-file hosts are asked with the operator's contact)"
            );
        }
        return None;
    }
    Some(std::sync::Arc::new(OsmImporter {
        region: config.catalogues.osm.region.clone(),
        ..Default::default()
    }))
}

/// The OSM importer.
pub struct OsmImporter {
    pub input: Input,
    /// `catalogues.osm.region`.
    pub region: String,
    /// How many threads decode the file (at least one).
    pub threads: usize,
    /// The run to make whatever is due (`pinakesd catalogue import|update`).
    pub force: Option<Run>,
    /// The node index's layout; none: by the region and the file's size.
    pub layout: Option<nodes::Layout>,
    /// The file and change-file addresses instead of the region's ([`sources`]), for tests.
    pub upstream: Option<(String, String)>,
    /// The most disk the node index may take; none: by its layout and the file (benchmarks set
    /// it lower).
    pub index_cap: Option<u64>,
}
impl Default for OsmImporter {
    fn default() -> Self {
        Self {
            input: Input::Region,
            region: "planet".into(),
            force: None,
            layout: None,
            upstream: None,
            index_cap: None,
            threads: std::thread::available_parallelism()
                .map(|n| n.get().clamp(1, 8))
                .unwrap_or(2),
        }
    }
}

/// Whether two replication bases name one stream (scheme and a trailing slash aside).
fn same_stream(a: &str, b: &str) -> bool {
    let norm = |u: &str| {
        u.trim_end_matches('/')
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_owned()
    };
    norm(a) == norm(b)
}
/// The file's header: its replication sequence and time, where the updates start.
async fn read_header(file: &Path) -> Result<pbf::Header> {
    let path = file.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut reader = pbf::Reader::new(std::fs::File::open(&path)?, 0);
        match reader.next_blob()? {
            Some(blob) if blob.kind == "OSMHeader" => pbf::header(&pbf::inflate(&blob.data)?),
            _ => anyhow::bail!("pbf: the file starts with no header"),
        }
    })
    .await
    .map_err(|_| Failure::new("panic"))?
    .with_context(|| Failure::new("parse").at(file.display().to_string()))
}
/// The catalogues' client, outside `Egress`: it reads only the fixed bulk-file hosts
/// ([`HOSTS`]), following redirects only to them.
fn http(config: &crate::config::Config) -> Result<super::fetch::Client> {
    super::fetch::client(&config.egress.contact, HOSTS)
}
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
}
/// A small text file (a replication state): at most 64 KiB, or none when it is not there.
async fn get_text(client: &super::fetch::Client, url: &str) -> Result<Option<String>> {
    let body = client.small(url, 64 * 1024).await?;
    Ok(body.map(|b| String::from_utf8_lossy(&b).into_owned()))
}
/// The MD5 OpenStreetMap's file servers publish beside a bulk file (`<file>.md5`: an `md5sum`
/// line naming it), the planet's and Geofabrik's; none when none is published (a 404). One that
/// names no MD5 of this file is a failure (`checksum`): the file is not the one it lists.
async fn published_md5(client: &super::fetch::Client, file: &url::Url) -> Result<Option<String>> {
    let Some(name) = file.path_segments().and_then(|mut s| s.next_back()) else {
        return Ok(None);
    };
    let mut sums = file.clone();
    sums.set_path(&format!("{}.md5", file.path()));
    let Some(text) = client.small(sums.as_str(), 4096).await? else {
        return Ok(None);
    };
    let listed = super::bulk::sums(&String::from_utf8_lossy(&text), 32)
        .into_iter()
        .find(|(listed, _)| listed == name)
        .map(|(_, hex)| hex);
    match listed {
        Some(hex) => Ok(Some(hex)),
        None => Err(Failure::new("checksum").at(host_of(file.as_str())).into()),
    }
}
/// The largest bulk file read (the planet is about 96 GB).
const MAX_FILE: u64 = 250_000_000_000;
/// The largest change file read, compressed (a planet day is about 130 MB).
const MAX_CHANGES: u64 = 4_000_000_000;
/// A downloaded file is used again for this long by a later attempt.
const KEEP_DOWNLOAD: Duration = Duration::from_secs(7 * 86400);

/// Download `url` into `dest` through [`bulk::Download`] (a broken body is asked for again from
/// where it broke, from the same file only): a file an earlier attempt finished is used again
/// (for [`KEEP_DOWNLOAD`]). `room` is asked with the file's length before any byte is written
/// (the free-disk check). True when the file was downloaded now, false when an earlier
/// attempt's file was used.
async fn download(
    client: &super::fetch::Client,
    url: &str,
    dest: &Path,
    room: impl Fn(u64) -> Result<()>,
) -> Result<bool> {
    let done = dest.with_extension("done");
    // The file an earlier attempt read whole, of this same URL (a region changed reads anew).
    let fresh = tokio::fs::metadata(&done)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < KEEP_DOWNLOAD)
        && tokio::fs::read_to_string(&done)
            .await
            .is_ok_and(|from| from == url);
    if fresh && tokio::fs::metadata(dest).await.is_ok() {
        tracing::info!(catalogue = "osm", file = %dest.display(), "catalogue download: using the file an earlier attempt read");
        return Ok(false);
    }
    let _ = tokio::fs::remove_file(&done).await;
    let (http, handle, parsed) = (
        client.clone(),
        tokio::runtime::Handle::current(),
        url::Url::parse(url)?,
    );
    // A run cut off (this future dropped) stops the download's thread within a moment.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _stop = super::bulk::StopOnDrop(stop.clone());
    let opened = tokio::task::spawn_blocking(move || {
        super::bulk::Download::new(http, handle, parsed, MAX_FILE)
            .retry_wait(Duration::from_secs(5))
            .stop(stop)
            .start()
    })
    .await
    .map_err(|_| Failure::new("panic"))??;
    let length = opened.length();
    room(length)?;
    // The checksum published for the file the answer came from (a `-latest` name redirects to
    // the dated file), asked before its bytes are read.
    let answered = opened
        .final_url()
        .cloned()
        .unwrap_or_else(|| url::Url::parse(url).expect("parsed above"));
    let want = published_md5(client, &answered).await?;
    if want.is_none() {
        tracing::warn!(catalogue = "osm", host = %host_of(answered.as_str()), "no checksum is published beside the file (no .md5); the download goes on, checked by TLS and its length only");
    }
    tracing::info!(
        catalogue = "osm",
        bytes = length,
        "catalogue download starts"
    );
    let part = dest.with_extension("part");
    let (to, failed_at) = (part.clone(), host_of(url));
    let (read, got) = tokio::task::spawn_blocking(move || -> Result<(u64, String)> {
        let mut body = super::bulk::Hashed::<_, md5::Md5>::with(opened);
        let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&to)?);
        let read = std::io::copy(&mut body, &mut out)
            .with_context(|| Failure::new("download").at(failed_at))?;
        std::io::Write::flush(&mut out)?;
        out.get_ref().sync_all()?;
        Ok((read, body.hex()))
    })
    .await
    .map_err(|_| Failure::new("panic"))??;
    anyhow::ensure!(read == length, Failure::new("download").at(host_of(url)));
    if want.is_some_and(|want| want != got) {
        // Not the file its publisher listed: never used, and downloaded again next time.
        let _ = tokio::fs::remove_file(&part).await;
        anyhow::bail!(Failure::new("checksum").at(host_of(answered.as_str())));
    }
    tokio::fs::rename(&part, dest).await?;
    tokio::fs::write(&done, url).await?;
    tracing::info!(
        catalogue = "osm",
        bytes = read,
        "catalogue download finished"
    );
    Ok(true)
}

/// A local file's version as a refusal names it: its path, size and modification time.
fn local_version(file: &Path) -> Option<Value> {
    let meta = std::fs::metadata(file).ok()?;
    let modified = meta
        .modified()
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from)
        .map_or(0, |t| t.timestamp());
    Some(json!(format!(
        "{}:{}:{modified}",
        file.display(),
        meta.len()
    )))
}
/// A bulk file's version as its `HEAD` answer names it (a strong ETag, else Last-Modified), or
/// none when the answer names none (or does not come): the file is then read as any.
async fn remote_version(client: &super::fetch::Client, url: &str) -> Option<Value> {
    let input = super::fetch::Input::Url(url::Url::parse(url).ok()?);
    let (version, _) = input.version(client).await.ok()?;
    Some(json!(version))
}
/// A version as a message names it.
fn shown(version: &Value) -> String {
    version
        .as_str()
        .map_or_else(|| version.to_string(), str::to_owned)
}
/// What one pass sends to be written: cards, removals, and the cursor saved with them.
struct Batch {
    cards: Vec<Card>,
    cursor: Value,
}
/// A relation set aside in the first pass, for its geometry in the second.
#[derive(serde::Serialize, serde::Deserialize)]
struct Pending {
    card: Card,
    /// Members: `n`/`w` and id, and whether the member is the relation's label or centre.
    members: Vec<(char, i64, bool)>,
}

/// Geometry of a way from its nodes' locations: the mean of its distinct nodes, and the box. A way
/// across the antimeridian (spanning over 180 degrees of longitude) is measured in longitudes
/// shifted by 360: its point lies on it, and its box runs west to east across ±180 (`[min_lon,
/// min_lat, max_lon, max_lat]` with `min_lon` > `max_lon`, as search's `bbox` reads it).
fn way_geometry(points: &[(i32, i32)]) -> place::Geometry {
    if points.is_empty() {
        return place::Geometry::default();
    }
    let closed = points.len() > 1 && points.first() == points.last();
    let distinct = if closed {
        &points[..points.len() - 1]
    } else {
        points
    };
    let lons = points.iter().map(|p| f64::from(p.1) / 1e7);
    let (west, east) = lons.fold((f64::MAX, f64::MIN), |(w, e), x| (w.min(x), e.max(x)));
    let across = east - west > 180.0;
    let shift = |lon: f64| {
        if across && lon < 0.0 {
            lon + 360.0
        } else {
            lon
        }
    };
    let wrap = |lon: f64| if lon > 180.0 { lon - 360.0 } else { lon };
    let n = distinct.len() as f64;
    let lat = distinct.iter().map(|p| f64::from(p.0)).sum::<f64>() / n / 1e7;
    let lon = distinct
        .iter()
        .map(|p| shift(f64::from(p.1) / 1e7))
        .sum::<f64>()
        / n;
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for (la, lo) in points {
        let (la, lo) = (f64::from(*la) / 1e7, shift(f64::from(*lo) / 1e7));
        b = [b[0].min(lo), b[1].min(la), b[2].max(lo), b[3].max(la)];
    }
    place::Geometry {
        point: Some(Point {
            lat,
            lon: wrap(lon),
        }),
        bbox: Some([wrap(b[0]), b[1], wrap(b[2]), b[3]]),
    }
}
/// The box holding both boxes, the narrower way round the globe: boxes either side of the
/// antimeridian give one across it (west edge east of the east edge), not one spanning the world.
fn union(a: Option<[f64; 4]>, b: [f64; 4]) -> [f64; 4] {
    let Some(a) = a else { return b };
    // Each box's west and east, the east past 180 for a box across the antimeridian.
    let span = |x: [f64; 4]| (x[0], if x[0] > x[2] { x[2] + 360.0 } else { x[2] });
    let ((aw, ae), (bw, be)) = (span(a), span(b));
    let (mut width, mut w, mut e) = (f64::MAX, 0.0, 0.0);
    for shift in [0.0, -360.0, 360.0] {
        let (west, east) = (aw.min(bw + shift), ae.max(be + shift));
        if east - west < width {
            (width, w, e) = (east - west, west, east);
        }
    }
    let (south, north) = (a[1].min(b[1]), a[3].max(b[3]));
    if width >= 360.0 {
        return [-180.0, south, 180.0, north];
    }
    if w < -180.0 {
        (w, e) = (w + 360.0, e + 360.0);
    } else if w > 180.0 {
        (w, e) = (w - 360.0, e - 360.0);
    }
    [w, south, if e > 180.0 { e - 360.0 } else { e }, north]
}
fn centre(b: [f64; 4]) -> Point {
    // A box across the antimeridian (west of it to east of it) has its centre between.
    let east = if b[0] > b[2] { b[2] + 360.0 } else { b[2] };
    let lon = (b[0] + east) / 2.0;
    Point {
        lat: (b[1] + b[3]) / 2.0,
        lon: if lon > 180.0 { lon - 360.0 } else { lon },
    }
}

/// The first pass over the file from `cursor` (a blob boundary): nodes into the index, node and
/// way cards sent to be written, kept relations set aside. Blocking work.
#[allow(clippy::too_many_arguments)]
fn first_pass(
    file: &Path,
    dir: &Path,
    layout: nodes::Layout,
    cursor: &Value,
    threads: usize,
    room: Option<u64>,
    free: crate::catalogue::run::FreeDisk,
    out: tokio::sync::mpsc::Sender<Batch>,
) -> Result<()> {
    let offset = cursor["offset"].as_u64().unwrap_or(0);
    let index_len = cursor["index"].as_u64().unwrap_or(0);
    let pending_len = cursor["pending"].as_u64().unwrap_or(0);
    let mut ways_at = cursor["ways_at"].as_u64();
    let first = cursor["blob"].as_u64().unwrap_or(0);
    let mut index = nodes::Writer::open(&dir.join("nodes.idx"), layout, index_len)?;
    // What the index may take on disk: the dense one's IDs, a sparse one's nodes (16 bytes for
    // the 1 or 2 bytes a node takes in the file, and some).
    let size = std::fs::metadata(file)?.len();
    let room = room.unwrap_or(match layout {
        nodes::Layout::Dense => DENSE_INDEX_CAP,
        nodes::Layout::Sparse => size.saturating_mul(16).max(1 << 30),
    });
    let mut since_check = 0u32;
    let mut written = 0u64;
    let pending_path = dir.join("relations.jsonl");
    let pending_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&pending_path)?;
    pending_file.set_len(pending_len)?;
    let mut pending = std::io::BufWriter::new(pending_file);
    std::io::Seek::seek(&mut pending, std::io::SeekFrom::Start(pending_len))?;
    let mut pending_len = pending_len;
    let mut reader: Option<nodes::Reader> = None;
    let mut cards = vec![];
    let parse = || Failure::new("parse").at(file.display().to_string());
    pbf::read_blocks(file, offset, first, threads, |d| {
        let b = &d.block;
        // The index's disk, every 64 blocks of nodes too (a stretch without cards writes it).
        since_check += 1;
        if since_check >= 64 && !b.nodes.is_empty() {
            since_check = 0;
            index.sync()?;
            nodes::check_room(&dir.join("nodes.idx"), room, &free)?;
        }
        for n in &b.nodes {
            index.add(n.id, n.lat, n.lon)?;
            // Room checked again after every gigabyte written: a dense index pads the IDs a file
            // skips, so few nodes can write much.
            written += index.take_flushed();
            if written >= 1 << 30 {
                written = 0;
                nodes::check_room(&dir.join("nodes.idx"), room, &free)?;
            }
            if n.tags.is_empty() {
                continue;
            }
            let tags = place::tags(b.tags(&n.tags));
            let geometry = place::Geometry {
                point: Some(Point {
                    lat: f64::from(n.lat) / 1e7,
                    lon: f64::from(n.lon) / 1e7,
                }),
                bbox: None,
            };
            cards.extend(place::read("node", n.id, &tags, geometry));
        }
        if !b.ways.is_empty() {
            if ways_at.is_none() {
                ways_at = Some(d.offset);
            }
            if reader.is_none() {
                index.sync()?;
                reader = Some(nodes::Reader::open(&dir.join("nodes.idx"), layout)?);
            }
            let r = reader.as_ref().expect("opened");
            for w in &b.ways {
                let tags = place::tags(b.tags(&w.tags));
                let Some(kept) = place::kept(&tags) else { continue };
                let mut points = Vec::with_capacity(w.refs.len());
                for id in &w.refs {
                    points.extend(r.get(*id)?);
                }
                cards.push(place::card("way", w.id, &tags, &kept, way_geometry(&points)));
            }
        }
        for rel in &b.relations {
            let tags = place::tags(b.tags(&rel.tags));
            if matches!(tags.get("type").copied(), Some("route" | "route_master" | "network")) {
                continue;
            }
            let Some(kept) = place::kept(&tags) else { continue };
            let card = place::card("relation", rel.id, &tags, &kept, place::Geometry::default());
            let members = rel
                .members
                .iter()
                .filter(|m| m.kind != pbf::Kind::Relation)
                .take(GEOMETRY_MEMBERS)
                .map(|m| {
                    let role = b.string(m.role);
                    (
                        if m.kind == pbf::Kind::Node { 'n' } else { 'w' },
                        m.id,
                        matches!(role, "label" | "admin_centre"),
                    )
                })
                .collect();
            let mut line = serde_json::to_vec(&Pending { card, members })?;
            line.push(b'\n');
            std::io::Write::write_all(&mut pending, &line)?;
            pending_len += line.len() as u64;
        }
        if cards.len() >= BATCH {
            let length = index.sync()?;
            nodes::check_room(&dir.join("nodes.idx"), room, &free)?;
            std::io::Write::flush(&mut pending)?;
            pending.get_ref().sync_data()?;
            let cursor = json!({"stage":"pass1","blob":d.index + 1,"offset":d.next,"index":length,"pending":pending_len,"ways_at":ways_at});
            if out
                .blocking_send(Batch {
                    cards: std::mem::take(&mut cards),
                    cursor,
                })
                .is_err()
            {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .with_context(parse)?;
    index.sync()?;
    nodes::check_room(&dir.join("nodes.idx"), room, &free)?;
    if let Some(r) = &reader {
        tracing::info!(
            catalogue = "osm",
            lookups = r.reads(),
            "catalogue import: node index reads of the first pass"
        );
    }
    if index.out_of_order > 0 {
        tracing::warn!(catalogue = "osm", nodes = index.out_of_order, "catalogue import: nodes out of ID order were left out of the node index (the file is not sorted); ways that use them lack geometry");
    }
    std::io::Write::flush(&mut pending)?;
    pending.get_ref().sync_data()?;
    let _ = out.blocking_send(Batch {
        cards,
        cursor: json!({"stage":"pass2","line":0,"ways_at":ways_at.unwrap_or(0)}),
    });
    Ok(())
}

/// The second pass: the bounding boxes of the kept relations' member ways, then the relations'
/// cards from line `cursor.line` of the set-aside relations. Blocking work.
fn second_pass(
    file: &Path,
    dir: &Path,
    layout: nodes::Layout,
    cursor: &Value,
    threads: usize,
    out: tokio::sync::mpsc::Sender<Batch>,
) -> Result<()> {
    use std::io::BufRead;
    let pending_path = dir.join("relations.jsonl");
    let skip = cursor["line"].as_u64().unwrap_or(0);
    let ways_at = cursor["ways_at"].as_u64().unwrap_or(0);
    let mut wanted: Vec<i64> = vec![];
    let mut skipped = 0u64;
    for line in std::io::BufReader::new(std::fs::File::open(&pending_path)?).lines() {
        let p: Pending = serde_json::from_str(&line?)?;
        let ways = p.members.iter().filter(|m| m.0 == 'w').map(|m| m.1);
        if wanted.len() >= MEMBER_WAYS {
            wanted.sort_unstable();
            wanted.dedup();
        }
        if wanted.len() >= MEMBER_WAYS {
            // Past the cap, a relation's ways are not read: its box comes from its nodes.
            skipped += 1;
            continue;
        }
        wanted.extend(ways);
    }
    if skipped > 0 {
        tracing::warn!(catalogue = "osm", relations = skipped, "catalogue import: member ways over the cap; these relations' boxes come from their nodes alone");
    }
    wanted.sort_unstable();
    wanted.dedup();
    let index = nodes::Reader::open(&dir.join("nodes.idx"), layout)?;
    let mut boxes: Vec<(i64, [f64; 4])> = Vec::with_capacity(wanted.len());
    if !wanted.is_empty() {
        let parse = || Failure::new("parse").at(file.display().to_string());
        pbf::read_blocks(file, ways_at, 0, threads, |d| {
            let b = &d.block;
            for w in &b.ways {
                if wanted.binary_search(&w.id).is_err() {
                    continue;
                }
                let mut points = Vec::with_capacity(w.refs.len());
                for id in &w.refs {
                    points.extend(index.get(*id)?);
                }
                if let Some(bb) = way_geometry(&points).bbox {
                    boxes.push((w.id, bb));
                }
            }
            // Relations follow the ways in a sorted file: the ways are all read.
            Ok(b.relations.is_empty() || !b.ways.is_empty())
        })
        .with_context(parse)?;
        boxes.sort_unstable_by_key(|b| b.0);
    }
    let mut cards = vec![];
    let mut n = 0u64;
    for line in std::io::BufReader::new(std::fs::File::open(&pending_path)?).lines() {
        n += 1;
        let line = line?;
        if n <= skip {
            continue;
        }
        let mut p: Pending = serde_json::from_str(&line)?;
        let mut bbox = None;
        let mut label = None;
        for (kind, id, is_label) in &p.members {
            if *kind == 'n' {
                if let Some((la, lo)) = index.get(*id)? {
                    let (la, lo) = (f64::from(la) / 1e7, f64::from(lo) / 1e7);
                    bbox = Some(union(bbox, [lo, la, lo, la]));
                    if *is_label {
                        label = Some(Point { lat: la, lon: lo });
                    }
                }
            } else if let Ok(i) = boxes.binary_search_by_key(id, |b| b.0) {
                bbox = Some(union(bbox, boxes[i].1));
            }
        }
        p.card.point = label.or(bbox.map(centre));
        if let Some(b) = bbox {
            p.card.details.insert("bbox".into(), json!(b));
        }
        cards.push(p.card);
        if cards.len() >= BATCH {
            let batch = Batch {
                cards: std::mem::take(&mut cards),
                cursor: json!({"stage":"pass2","line":n,"ways_at":ways_at}),
            };
            if out.blocking_send(batch).is_err() {
                return Ok(());
            }
        }
    }
    let _ = out.blocking_send(Batch {
        cards,
        cursor: json!({"stage":"done"}),
    });
    Ok(())
}

/// A card's fame raised to its Wikidata item's, when the item is in the Wikidata catalogue.
async fn with_wikidata_fame(pool: &PgPool, on: bool, cards: &mut [Card]) -> Result<()> {
    if !on {
        return Ok(());
    }
    let keys: Vec<String> = cards
        .iter()
        .filter_map(|c| c.details.get("tags")?.get("wikidata")?.as_str())
        .filter_map(crate::lookups::places::wikidata_tag)
        .map(|q| format!("wikidata:{q}"))
        .collect();
    if keys.is_empty() {
        return Ok(());
    }
    let found: Vec<(Vec<String>, f32)> = sqlx::query_as(
        "SELECT keys,fame FROM catalogue_card WHERE catalogue='wikidata' AND keys && $1::text[]",
    )
    .bind(&keys)
    .fetch_all(pool)
    .await?;
    let mut fame: HashMap<String, f32> = HashMap::new();
    for (k, f) in found {
        for key in k.into_iter().filter(|k| keys.contains(k)) {
            let e = fame.entry(key).or_insert(0.0);
            *e = e.max(f);
        }
    }
    for card in cards {
        let q = card
            .details
            .get("tags")
            .and_then(|t| t.get("wikidata"))
            .and_then(Value::as_str)
            .and_then(crate::lookups::places::wikidata_tag)
            .map(|q| format!("wikidata:{q}"));
        if let Some(f) = q.and_then(|q| fame.get(&q)) {
            card.fame = card.fame.max(*f);
        }
    }
    Ok(())
}

/// The stored cards' content hashes by source ID.
async fn hashes(pool: &PgPool, source_ids: &[String]) -> Result<HashMap<String, Vec<u8>>> {
    Ok(sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT source_id,content_hash FROM catalogue_card WHERE catalogue='osm' AND source_id=ANY($1)",
    )
    .bind(source_ids)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect())
}

/// Shared links of these objects read their place again: the OSM route builds them from the
/// catalogue card (`resolver::osm`), so a changed or removed card shows at once.
pub async fn touch_shared(pool: &PgPool, source_ids: &[String]) -> Result<()> {
    for part in source_ids.chunks(BATCH) {
        let (kinds, ids): (Vec<String>, Vec<i64>) = part
            .iter()
            .filter_map(|s| {
                let (k, i) = s.split_once('/')?;
                Some((k.to_owned(), i.parse::<i64>().ok()?))
            })
            .unzip();
        sqlx::query("UPDATE osm_place o SET refresh_at=now() FROM unnest($1::text[],$2::bigint[]) v(t,i) WHERE o.osm_type=v.t AND o.osm_id=v.i AND o.refresh_at>now()")
            .bind(&kinds)
            .bind(&ids)
            .execute(pool)
            .await?;
        sqlx::query("UPDATE resource r SET next_attempt_at=now(),priority_at=coalesce(priority_at,now()) FROM unnest($1::text[],$2::bigint[]) v(t,i) WHERE r.osm_type=v.t AND r.osm_id=v.i AND r.resolver='osm' AND NOT r.dead_letter")
            .bind(&kinds)
            .bind(&ids)
            .execute(pool)
            .await?;
    }
    Ok(())
}

impl OsmImporter {
    /// The end of a full import whose file was read whole (`cursor` at stage `done`, holding
    /// what it needs): keep where the updates start, remove the temporary files, and after the
    /// runner's checks ([`Job::check_shrink`]: a file that held no place is refused as `empty`,
    /// and not downloaded again) remove the cards the file no longer holds and mark their shared
    /// links due.
    async fn finish(&self, job: &mut Job<'_>, cursor: &Value) -> Result<()> {
        let pool = job.pool();
        let generation = job.generation();
        let region = job.config().catalogues.osm.region.clone();
        let base = self.sources(&region).1;
        let header = &cursor["header"];
        let at_time = header["timestamp"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&chrono::Utc));
        job.set_position(json!({
            "region": region,
            "base": base,
            "sequence": header["sequence"],
            "timestamp": at_time,
        }));
        if let Some(t) = at_time {
            job.set_current_as_of(t);
        }
        let name = header["name"].as_str().unwrap_or_default();
        job.upstream_version(&match at_time {
            Some(t) => format!("{name} {}", t.format("%Y-%m-%dT%H:%MZ")),
            None => name.to_owned(),
        })
        .await?;
        // The cards the file no longer holds go now, a part at a time (rather than in the
        // runner's sweep), each part's shared links due once its cards are gone: read again,
        // they find the object without its card. A run cut off here does the rest at its next
        // attempt (it resumes at `done`).
        // The file and the index are done with: the end needs neither, so an import that waits
        // for the operator's allowance below holds no disk.
        let dir = job.dir().to_owned();
        for temp in ["nodes.idx", "relations.jsonl"] {
            let _ = tokio::fs::remove_file(dir.join(temp)).await;
        }
        if cursor["downloaded"] == true {
            if let Some(file) = cursor["file"].as_str() {
                let _ = tokio::fs::remove_file(file).await;
                let _ = tokio::fs::remove_file(Path::new(file).with_extension("done")).await;
            }
        }
        // A file that held no place is refused; removing more than the shrink floor allows (a
        // cut-off file, another region) waits for the operator's allowance, as any catalogue's
        // full import does: nothing is removed.
        job.check_shrink().await?;
        let mut after = String::new();
        loop {
            let gone: Vec<String> = sqlx::query_scalar(
                "SELECT source_id FROM catalogue_card WHERE catalogue='osm' AND source_id>$2 AND seen_generation<$1 ORDER BY source_id LIMIT $3",
            )
            .bind(generation)
            .bind(&after)
            .bind(BATCH as i64)
            .fetch_all(pool)
            .await?;
            let Some(last) = gone.last() else { break };
            after = last.clone();
            // A shared link of a named place swept here reads missing next: the deleted-place
            // rule compares its card as it last was.
            sqlx::query("INSERT INTO osm_removed(osm_type,osm_id,name,category,point) \
                 SELECT split_part(c.source_id,'/',1),split_part(c.source_id,'/',2)::bigint,c.name,c.details->>'category',c.point FROM catalogue_card c \
                 WHERE c.catalogue='osm' AND c.source_id=ANY($1) AND c.point IS NOT NULL AND c.details->>'category' IS NOT NULL AND NOT coalesce((c.details->>'unnamed')::boolean,false) \
                 AND EXISTS (SELECT 1 FROM resource r WHERE r.osm_type=split_part(c.source_id,'/',1) AND r.osm_id=split_part(c.source_id,'/',2)::bigint) \
                 ON CONFLICT (osm_type,osm_id) DO UPDATE SET name=excluded.name,category=excluded.category,point=excluded.point,removed_at=now()")
                .bind(&gone)
                .execute(pool)
                .await?;
            job.remove(&gone, cursor.clone()).await?;
            touch_shared(pool, &gone).await?;
        }
        tracing::info!(catalogue = "osm", sequence = %header["sequence"], "catalogue import done");
        Ok(())
    }
    fn sources(&self, region: &str) -> (String, String) {
        self.upstream.clone().unwrap_or_else(|| sources(region))
    }
    fn input_file(&self, job: &Job<'_>) -> Option<PathBuf> {
        match &self.input {
            Input::Files(files) if job.run() == Run::Import => files.first().cloned(),
            _ => None,
        }
    }
    async fn write(&self, job: &mut Job<'_>, mut batch: Batch) -> Result<()> {
        let wikidata = job.config().catalogues.enabled(Catalogue::Wikidata);
        with_wikidata_fame(job.pool(), wikidata, &mut batch.cards).await?;
        // A re-import: the shared links of the cards it changes read their place again (an
        // unchanged card would read the same).
        let reimport = job.state().last_import_at.is_some() && !batch.cards.is_empty();
        let ids: Vec<String> = batch.cards.iter().map(|c| c.source_id.clone()).collect();
        let before = if reimport {
            hashes(job.pool(), &ids).await?
        } else {
            HashMap::new()
        };
        job.write(&batch.cards, batch.cursor).await?;
        // An object the file holds again is no removed object (it came back, or was swept by a
        // run of a cut-off file or another region).
        if !ids.is_empty() {
            let (kinds, numbers): (Vec<String>, Vec<i64>) = ids
                .iter()
                .filter_map(|s| split(s).map(|(k, i)| (k.to_owned(), i)))
                .unzip();
            sqlx::query("DELETE FROM osm_removed o USING unnest($1::text[],$2::bigint[]) v(t,i) WHERE o.osm_type=v.t AND o.osm_id=v.i")
                .bind(&kinds)
                .bind(&numbers)
                .execute(job.pool())
                .await?;
        }
        if reimport {
            let after = hashes(job.pool(), &ids).await?;
            let changed: Vec<String> = ids
                .into_iter()
                .filter(|id| after.get(id) != before.get(id))
                .collect();
            touch_shared(job.pool(), &changed).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Importer for OsmImporter {
    fn catalogue(&self) -> Catalogue {
        Catalogue::Osm
    }
    /// The file `--from` names.
    fn source(&self) -> Option<String> {
        match &self.input {
            Input::Files(files) => files.first().map(|f| f.display().to_string()),
            Input::Region => None,
        }
    }
    fn disk_need(&self, run: Run, state: &State) -> u64 {
        match run {
            Run::Update => 1_000_000_000,
            // The planet: its file (about 96 GB) and its node index (about 115 GB); a resumed
            // import holds the file already. A region's needs are checked again once its file
            // is downloaded (three times the file).
            Run::Import if self.region == "planet" && matches!(self.input, Input::Region) => {
                if state
                    .cursor
                    .as_ref()
                    .and_then(|c| c["stage"].as_str())
                    .is_some()
                {
                    120_000_000_000
                } else {
                    215_000_000_000
                }
            }
            // A local file needs room for its index, checked once its size is known.
            Run::Import if matches!(self.input, Input::Files(_)) => 0,
            Run::Import => 2_000_000_000,
        }
    }
    /// Updates are looked for hourly: a change file is published daily, and an update that finds
    /// none reads one small file.
    fn update_every(&self) -> Duration {
        Duration::from_secs(3600)
    }
    async fn plan(&self, state: &State) -> Result<Plan> {
        if let Some(run) = self.force {
            return Ok(Plan::Run(run));
        }
        let Some(at) = state.current_at() else {
            return Ok(Plan::Run(Run::Import));
        };
        // A full import now and then gives back the geometry change files can't move (nodes
        // that moved under an unchanged way, ways newly kept without their nodes in the file).
        let every = if self.region == "planet" {
            REIMPORT_EVERY_PLANET
        } else {
            REIMPORT_EVERY
        };
        if state.last_import_at.is_some_and(|last| {
            chrono::Utc::now() - last > chrono::Duration::from_std(every).unwrap_or_default()
        }) {
            return Ok(Plan::Run(Run::Import));
        }
        let due = at + chrono::Duration::from_std(self.update_every())?;
        let left = (due - chrono::Utc::now()).to_std().unwrap_or_default();
        Ok(if left.is_zero() {
            Plan::Run(Run::Update)
        } else {
            Plan::Wait(left)
        })
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        let region = job.config().catalogues.osm.region.clone();
        let dir = job.dir().to_owned();
        let mut cursor = job.cursor().cloned().unwrap_or(Value::Null);
        let given = self.input_file(job);
        let (url, _) = self.sources(&region);
        // A cut-off import goes on only on its own file: `--from` another file is refused (it
        // would keep the first file's cards past the sweep).
        if let (Some(file), Some(from)) = (&given, cursor["file"].as_str()) {
            if cursor["stage"].is_string() && from != file.display().to_string() {
                return Err(Failure::new("other_file").at(from.to_owned()).into());
            }
        }
        // A run of another source's file (a region changed) starts over, in a generation of its
        // own: the cards the first file's run wrote go at this run's end. A `--from` run's
        // (no source) goes on.
        if given.is_none()
            && cursor["stage"].is_string()
            && cursor["url"].as_str().is_some_and(|u| u != url)
        {
            tracing::info!(
                catalogue = "osm",
                "catalogue import: the region changed; starting over"
            );
            job.new_generation().await?;
            cursor = Value::Null;
        }
        // The file was read whole and its last part written (a run cut off after it, in the
        // runner's sweep or before): only the end is left, without the file.
        if cursor["stage"] == "done" {
            if let Some(version) = cursor.get("version").filter(|v| !v.is_null()) {
                job.reading(version.clone());
            }
            return self.finish(job, &cursor).await;
        }
        // The file's version, before it is downloaded: one refused before (it held no place) is
        // not downloaded again until the file changes; a full import asked for reads it.
        let newest = match &given {
            Some(file) => local_version(file),
            None if job.config().egress.contact.trim().is_empty() => None,
            None => remote_version(&http(job.config())?, &url).await,
        };
        if let Some(version) = &newest {
            if job.refused_before(version) {
                return Err(job.still_refused(format!("the file {}", shown(version))));
            }
        }
        let (file, downloaded, url) = match given {
            Some(file) => (file, false, None),
            None => {
                if job.config().egress.contact.trim().is_empty() {
                    return Err(super::run::Waits(super::run::NEEDS_CONTACT.into()).into());
                }
                let file = dir.join("import.osm.pbf");
                let client = http(job.config())?;
                // The file, and the node index the planet's needs beside it.
                let index = if self.region == "planet" {
                    DENSE_INDEX
                } else {
                    0
                };
                let lost = !file.exists();
                let fetched = download(&client, &url, &file, |length| {
                    job.need_disk(if index > 0 {
                        length + index
                    } else {
                        length.saturating_mul(3)
                    })
                })
                .await?;
                // A cut-off run goes on only on the file it read: a file gone, or downloaded anew
                // (the earlier one expired, or was another's), starts over in a generation of
                // its own.
                if cursor["stage"].is_string() && (lost || fetched || cursor["url"].is_null()) {
                    tracing::info!(
                        catalogue = "osm",
                        "catalogue import: a new file; starting over"
                    );
                    job.new_generation().await?;
                    cursor = Value::Null;
                }
                (file, true, Some(url))
            }
        };
        let header = read_header(&file).await?;
        let base = self.sources(&region).1;
        let at_time = header
            .replication_timestamp
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0));
        // The header's sequence counts in its own replication stream: used only when it is the
        // configured one (a planet file names none, or the minutely stream's).
        let sequence = header.replication_sequence.filter(|_| {
            header
                .replication_base_url
                .as_deref()
                .map(|u| u.trim_end_matches('/').replace("-updates", "/updates"))
                .is_some_and(|u| same_stream(&u, &base))
        });
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The same file's header, or another file under the same name (replaced): a `--from`
        // file is refused, a download starts over.
        let read_as = json!({"sequence": sequence, "timestamp": at_time, "name": name});
        if cursor["stage"].is_string()
            && cursor["header"].is_object()
            && cursor["header"] != read_as
        {
            if !downloaded {
                return Err(Failure::new("other_file")
                    .at(file.display().to_string())
                    .into());
            }
            job.new_generation().await?;
            cursor = Value::Null;
        }
        let size = std::fs::metadata(&file)?.len();
        let fresh = cursor["stage"].as_str().is_none();
        // The version of the file read: a run taken up on its file keeps the one it began with.
        let version = if fresh {
            newest
        } else {
            cursor.get("version").filter(|v| !v.is_null()).cloned()
        };
        if let Some(version) = &version {
            job.reading(version.clone());
        }
        // The planet's index is dense (8 bytes per node ID); a region's is sparse (16 bytes per node).
        let layout = match cursor["layout"].as_str() {
            Some("dense") => nodes::Layout::Dense,
            Some(_) => nodes::Layout::Sparse,
            None => self
                .layout
                .unwrap_or(if self.region == "planet" || size > DENSE_FROM {
                    nodes::Layout::Dense
                } else {
                    nodes::Layout::Sparse
                }),
        };
        if fresh {
            job.need_disk(match layout {
                nodes::Layout::Dense => DENSE_INDEX,
                nodes::Layout::Sparse => size.saturating_mul(2),
            })?;
            cursor =
                json!({"stage":"pass1","blob":0,"offset":0,"index":0,"pending":0,"ways_at":null});
        }
        // What the end needs, kept in every cursor of the run (so a run at `done` needs no file).
        let fixed = json!({
            "version": version,
            "file": file.display().to_string(),
            "downloaded": downloaded,
            "url": url,
            "layout": if layout == nodes::Layout::Dense { "dense" } else { "sparse" },
            "header": read_as,
        });
        let carry = |mut c: Value| {
            for (k, v) in fixed.as_object().expect("an object") {
                c[k] = v.clone();
            }
            c
        };
        cursor = carry(cursor);
        if header
            .optional_features
            .iter()
            .all(|f| f != "Sort.Type_then_ID")
        {
            tracing::warn!(catalogue = "osm", file = %file.display(), "the file does not say it is sorted by type and ID: geometry of ways whose nodes come later is left out");
        }
        // Saved before the first card: an attempt cut off now reads the same file again.
        job.write(&[], cursor.clone()).await?;
        tracing::info!(
            catalogue = "osm",
            file = %file.display(),
            bytes = size,
            sequence = header.replication_sequence,
            stage = cursor["stage"].as_str().unwrap_or_default(),
            "catalogue import reads its file"
        );
        let started = job.report();
        let mut stage = cursor["stage"].as_str().unwrap_or("pass1").to_owned();
        let mut at = cursor.clone();
        let mut done = cursor.clone();
        if stage == "pass1" {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Batch>(4);
            let (file2, dir2, at2, threads) = (file.clone(), dir.clone(), at.clone(), self.threads);
            let (index_cap, free) = (self.index_cap, job.free_disk());
            let pass = tokio::task::spawn_blocking(move || {
                first_pass(&file2, &dir2, layout, &at2, threads, index_cap, free, tx)
            });
            let mut last = None;
            while let Some(mut batch) = rx.recv().await {
                batch.cursor = carry(batch.cursor);
                last = Some(batch.cursor.clone());
                if let Err(e) = self.write(job, batch).await {
                    drop(rx);
                    let _ = pass.await;
                    return Err(e);
                }
            }
            pass.await.map_err(|_| Failure::new("panic"))??;
            at = last.ok_or_else(|| Failure::new("parse").at(file.display().to_string()))?;
            stage = "pass2".into();
            tracing::info!(
                catalogue = "osm",
                cards = (job.report().inserted + job.report().changed + job.report().unchanged)
                    - (started.inserted + started.changed + started.unchanged),
                "catalogue import: first pass done"
            );
        }
        if stage == "pass2" {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Batch>(4);
            let (file2, dir2, at2, threads) = (file.clone(), dir.clone(), at.clone(), self.threads);
            let pass = tokio::task::spawn_blocking(move || {
                second_pass(&file2, &dir2, layout, &at2, threads, tx)
            });
            while let Some(mut batch) = rx.recv().await {
                batch.cursor = carry(batch.cursor);
                done = batch.cursor.clone();
                if let Err(e) = self.write(job, batch).await {
                    drop(rx);
                    let _ = pass.await;
                    return Err(e);
                }
            }
            pass.await.map_err(|_| Failure::new("panic"))??;
        }
        self.finish(job, &done).await
    }
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        if let Input::Files(files) = &self.input {
            let mut latest = None;
            for file in files.clone() {
                let name = json!(file.display().to_string());
                if job.refused_before(&name) {
                    return Err(job.still_refused(format!("change file {}", file.display())));
                }
                let changes = read_changes(
                    std::fs::File::open(&file)?,
                    &file.display().to_string(),
                    MAX_CHANGES_HELD,
                )
                .await?;
                latest = latest.max(changes.latest.clone());
                apply(job, changes, json!({"files": true}), name).await?;
            }
            if let Some(t) = latest.and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()) {
                job.set_current_as_of(t.with_timezone(&chrono::Utc));
            }
            return Ok(UpdateEnd::Done);
        }
        let region = job.config().catalogues.osm.region.clone();
        if job.config().egress.contact.trim().is_empty() {
            return Err(super::run::Waits(super::run::NEEDS_CONTACT.into()).into());
        }
        let position = job.position().cloned().unwrap_or(Value::Null);
        if position["region"].as_str() != Some(region.as_str()) {
            return Ok(UpdateEnd::NeedsImport("the region changed"));
        }
        let (_, base) = self.sources(&region);
        let client = http(job.config())?;
        let state = get_text(&client, &format!("{base}/state.txt"))
            .await?
            .and_then(|t| parse_state(&t))
            .ok_or_else(|| Failure::new("replication_state").at(host_of(&base)))?;
        let mut applied = match position["sequence"].as_u64() {
            Some(s) => s,
            None => {
                // A file whose header names no sequence (the planet's): the last change file
                // published before the file's time.
                let Some(at) = position["timestamp"]
                    .as_str()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                else {
                    return Ok(UpdateEnd::NeedsImport("the file named no replication time"));
                };
                let days = (state.1 - at.with_timezone(&chrono::Utc))
                    .num_hours()
                    .max(0) as u64
                    / 24;
                state.0.saturating_sub(days + 1)
            }
        };
        if let Some(done) = job.cursor().and_then(|c| c["applied"].as_u64()) {
            applied = applied.max(done);
        }
        if state.0 < applied {
            return Ok(UpdateEnd::NeedsImport("the replication sequence went back"));
        }
        if state.0 > applied + 60 {
            return Ok(UpdateEnd::NeedsImport(
                "the change files are more than 60 behind",
            ));
        }
        let mut last_time = None;
        let end = state.0.min(applied + FILES_PER_UPDATE);
        let stored_time = position["timestamp"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&chrono::Utc));
        while applied < end {
            let sequence = applied + 1;
            // A change file refused before is not read again: the updates wait there until a
            // full import (asked for, or once the files are 60 behind) or `clear-refusal`.
            if job.refused_before(&json!(sequence)) {
                return Err(job.still_refused(format!("change file {sequence}")));
            }
            let path = sequence_path(sequence);
            let url = format!("{base}/{path}.osc.gz");
            let failed = || Failure::new("download").at(host_of(&base));
            // Kept on disk while it is read: a planet day is about 130 MB compressed.
            let local = job.dir().join("change.osc.gz");
            let (c, handle, at, to) = (
                client.clone(),
                tokio::runtime::Handle::current(),
                url::Url::parse(&url)?,
                local.clone(),
            );
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let _stop = super::bulk::StopOnDrop(stop.clone());
            let opened = tokio::task::spawn_blocking(move || {
                super::bulk::Download::new(c, handle, at, MAX_CHANGES)
                    .retry_wait(Duration::from_secs(5))
                    .stop(stop)
                    .start()
            })
            .await
            .map_err(|_| Failure::new("panic"))?;
            let body = match opened {
                Ok(body) => body,
                Err(error) => {
                    // A file missing well behind the server's state is gone: a full import is
                    // asked for. One missing near it (a mirror not caught up) is tried again.
                    if error
                        .downcast_ref::<Failure>()
                        .is_some_and(|f| f.class == "not_found")
                    {
                        if state.0 >= sequence + GONE_AFTER {
                            return Ok(UpdateEnd::NeedsImport("a change file is gone"));
                        }
                        return Err(Failure::new("not_found").at(host_of(&base)).into());
                    }
                    return Err(error.context(failed()));
                }
            };
            job.need_disk(body.length().saturating_mul(2))?;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut body = body;
                let mut out = std::fs::File::create(&to)?;
                std::io::copy(&mut body, &mut out)?;
                Ok(())
            })
            .await
            .map_err(|_| Failure::new("panic"))?
            .with_context(failed)?;
            let file_state = get_text(&client, &format!("{base}/{path}.state.txt"))
                .await?
                .and_then(|t| parse_state(&t));
            let changes = read_changes(
                std::fs::File::open(&local)?,
                &host_of(&base),
                MAX_CHANGES_HELD,
            )
            .await?;
            let _ = tokio::fs::remove_file(&local).await;
            let counts = apply(job, changes, json!({"applied": applied}), json!(sequence)).await?;
            tracing::info!(
                catalogue = "osm",
                sequence,
                written = counts.0,
                removed = counts.1,
                successors = counts.2,
                renames = counts.3,
                "catalogue update: change file applied"
            );
            applied = sequence;
            last_time = file_state.map(|s| s.1).or(last_time);
            // Where a refusal of a later file leaves the next update: after this one.
            job.set_position(json!({"region": region, "base": base, "sequence": applied, "timestamp": last_time.or(stored_time)}));
            // Saved with an empty write, so a run cut off after this file starts at the next.
            job.write(&[], json!({"applied": applied})).await?;
        }
        let timestamp = last_time.or_else(|| {
            position["timestamp"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
        });
        job.set_position(
            json!({"region": region, "base": base, "sequence": applied, "timestamp": timestamp}),
        );
        if let Some(t) = last_time {
            job.set_current_as_of(t);
        }
        Ok(UpdateEnd::Done)
    }
}

/// One change file read whole: the changes that matter to the catalogue, each object at its
/// last version in the file (a day's file may hold several edits of one object).
#[derive(Default)]
pub struct Changes {
    /// Locations of the nodes created or modified, at their last version.
    nodes: HashMap<i64, (i32, i32)>,
    /// Deleted objects, and created or modified ones with tags.
    changed: Vec<osc::Change>,
    /// Created or modified objects without tags at their last version (they may have lost them).
    untagged: Vec<String>,
    latest: Option<String>,
}
/// Read one change file, holding at most `cap` bytes of its changes in memory (every object's
/// version, the changed nodes' locations and the kept changes; [`MAX_CHANGES_HELD`]): past it
/// the file is `size`.
async fn read_changes(
    input: impl std::io::Read + Send + 'static,
    at: &str,
    cap: usize,
) -> Result<Changes> {
    let at = at.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut latest_time: Option<String> = None;
        let mut versions: HashMap<(pbf::Kind, i64), u32> = HashMap::new();
        let mut kept: HashMap<(pbf::Kind, i64), osc::Change> = HashMap::new();
        let mut nodes: HashMap<i64, (u32, (i32, i32))> = HashMap::new();
        let mut held = 0usize;
        osc::read_gz(input, |c| {
            if c.timestamp > latest_time {
                latest_time = c.timestamp.clone();
            }
            let key = (c.kind, c.id);
            // An earlier version than one already read changes nothing (a delete wins a tie).
            if let Some(v) = versions.get(&key) {
                if *v > c.version || (*v == c.version && c.action != osc::Action::Delete) {
                    return Ok(());
                }
            }
            // Each object's version (and, untagged, its ID as text later), each node's location.
            if versions.insert(key, c.version).is_none() {
                held += 80;
            }
            if c.kind == pbf::Kind::Node && c.action != osc::Action::Delete {
                if let Some(l) = c.location {
                    if nodes.insert(c.id, (c.version, l)).is_none() {
                        held += 40;
                    }
                }
            }
            let size = |c: &osc::Change| {
                c.tags
                    .iter()
                    .map(|(k, v)| k.len() + v.len() + 48)
                    .sum::<usize>()
                    + c.refs.len() * 8
                    + c.members.iter().map(|m| m.2.len() + 32).sum::<usize>()
                    + 160
            };
            if let Some(old) = kept.remove(&key) {
                held = held.saturating_sub(size(&old));
            }
            if !(c.tags.is_empty() && c.action != osc::Action::Delete) {
                held += size(&c);
                kept.insert(key, c);
            }
            // What one file's changes may hold in memory, whatever it states.
            anyhow::ensure!(held <= cap, Failure::new("size").at(at.clone()));
            Ok(())
        })
        // A failure of its own (a cap) keeps its class; anything else is the file's `parse`.
        .map_err(|e| {
            if e.downcast_ref::<Failure>().is_some() {
                e
            } else {
                e.context(Failure::new("parse").at(at))
            }
        })?;
        let untagged = versions
            .keys()
            .filter(|k| !kept.contains_key(k))
            .map(|(k, i)| format!("{}/{i}", k.name()))
            .collect();
        let mut changed: Vec<osc::Change> = kept.into_values().collect();
        changed.sort_by_key(|c| (c.kind, c.id));
        anyhow::Ok(Changes {
            nodes: nodes.into_iter().map(|(id, (_, l))| (id, l)).collect(),
            changed,
            untagged,
            latest: latest_time,
        })
    })
    .await
    .map_err(|_| Failure::new("panic"))?
}

/// A kept object's change to an unrelated name: its source ID, its old name and category, its
/// new name, and when the change was made.
type Rename = (String, String, Option<String>, String, Option<String>);
/// A stored card as an update compares it.
#[derive(sqlx::FromRow)]
struct Stored {
    source_id: String,
    name: String,
    category: Option<String>,
    lon: Option<f64>,
    lat: Option<f64>,
    bbox: Option<Value>,
    wikidata: Option<String>,
    unnamed: bool,
}
async fn stored(pool: &PgPool, ids: &[String]) -> Result<HashMap<String, Stored>> {
    let mut out = HashMap::new();
    for part in ids.chunks(10_000) {
        let rows: Vec<Stored> = sqlx::query_as(
            "SELECT source_id,name,details->>'category' AS category,ST_X(point) AS lon,ST_Y(point) AS lat,details->'bbox' AS bbox,details#>>'{tags,wikidata}' AS wikidata,coalesce((details->>'unnamed')::boolean,false) AS unnamed \
             FROM catalogue_card WHERE catalogue='osm' AND source_id=ANY($1)",
        )
        .bind(part)
        .fetch_all(pool)
        .await?;
        out.extend(rows.into_iter().map(|s| (s.source_id.clone(), s)));
    }
    Ok(out)
}
fn stored_geometry(s: &Stored) -> place::Geometry {
    let bbox = s
        .bbox
        .as_ref()
        .and_then(|b| serde_json::from_value::<[f64; 4]>(b.clone()).ok());
    place::Geometry {
        point: s.lat.zip(s.lon).map(|(lat, lon)| Point { lat, lon }),
        bbox,
    }
}

/// Apply one change file: write the changed cards, remove the gone ones, and record successors
/// and renames. Applying a file again gives the same cards.
/// What applying a change file did: cards written, removed, successors and renames found.
type Applied = (usize, usize, usize, usize);
/// Apply one change file (`refuse_as` names it: its sequence, or a `--from` file's path). One that
/// would take the update's removals past a tenth of the catalogue's cards (and past ten:
/// `diff::too_many`) is refused before anything of it is applied ([`Job::refuse`]).
async fn apply(
    job: &mut Job<'_>,
    changes: Changes,
    cursor: Value,
    refuse_as: Value,
) -> Result<Applied> {
    let pool = job.pool();
    let wikidata_on = job.config().catalogues.enabled(Catalogue::Wikidata);
    // What is stored of every object the file names with tags, and of the untagged ones that
    // are stored (they lost their tags).
    let named: Vec<String> = changes
        .changed
        .iter()
        .map(|c| format!("{}/{}", c.kind.name(), c.id))
        .collect();
    let mut before = stored(pool, &named).await?;
    let lost: Vec<String> = stored(pool, &changes.untagged).await?.into_keys().collect();
    // Ways read in this file, for relations' geometry.
    let mut way_boxes: HashMap<i64, [f64; 4]> = HashMap::new();
    let mut cards: Vec<Card> = vec![];
    let mut removed: Vec<String> = lost;
    let mut created: Vec<(Card, Option<i64>)> = vec![];
    let mut deleted: Vec<(String, Option<i64>, Stored)> = vec![];
    let mut renames: Vec<Rename> = vec![];
    let order = |k: pbf::Kind| match k {
        pbf::Kind::Node => 0,
        pbf::Kind::Way => 1,
        pbf::Kind::Relation => 2,
    };
    let mut changed = changes.changed;
    changed.sort_by_key(|c| order(c.kind));
    // Member ways stored as cards, for relations whose ways the file does not hold.
    let member_ways: Vec<String> = changed
        .iter()
        .filter(|c| c.kind == pbf::Kind::Relation)
        .flat_map(|c| {
            c.members
                .iter()
                .filter(|m| m.0 == pbf::Kind::Way)
                .take(GEOMETRY_MEMBERS)
                .map(|m| format!("way/{}", m.1))
        })
        .collect();
    let member_cards = stored(pool, &member_ways).await?;
    for c in changed {
        let source_id = format!("{}/{}", c.kind.name(), c.id);
        let old = before.remove(&source_id);
        if c.action == osc::Action::Delete {
            // Only what is stored is removed: most deleted objects were never cards.
            if let Some(old) = old {
                deleted.push((source_id.clone(), c.changeset, old));
                removed.push(source_id);
            }
            continue;
        }
        let pairs: Vec<(&str, &str)> = c
            .tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let tags = place::tags(pairs);
        let geometry = match c.kind {
            pbf::Kind::Node => place::Geometry {
                point: c.location.map(|(la, lo)| Point {
                    lat: f64::from(la) / 1e7,
                    lon: f64::from(lo) / 1e7,
                }),
                bbox: None,
            },
            pbf::Kind::Way => {
                let points: Vec<(i32, i32)> = c
                    .refs
                    .iter()
                    .filter_map(|r| changes.nodes.get(r).copied())
                    .collect();
                let g = if points.len() == c.refs.len() && !points.is_empty() {
                    way_geometry(&points)
                } else if let Some(old) = &old {
                    stored_geometry(old)
                } else {
                    // Newly kept without all its nodes in the file: no geometry rather than one
                    // from some of them, until the next full import.
                    place::Geometry::default()
                };
                if let Some(b) = g.bbox {
                    way_boxes.insert(c.id, b);
                }
                g
            }
            pbf::Kind::Relation => {
                let mut bbox = None;
                let mut label = None;
                let mut all = true;
                for (kind, id, role) in c.members.iter().take(GEOMETRY_MEMBERS) {
                    match kind {
                        pbf::Kind::Node => match changes.nodes.get(id) {
                            Some((la, lo)) => {
                                let (la, lo) = (f64::from(*la) / 1e7, f64::from(*lo) / 1e7);
                                bbox = Some(union(bbox, [lo, la, lo, la]));
                                if matches!(role.as_str(), "label" | "admin_centre") {
                                    label = Some(Point { lat: la, lon: lo });
                                }
                            }
                            None => all = false,
                        },
                        pbf::Kind::Way => {
                            let b = way_boxes.get(id).copied().or_else(|| {
                                member_cards
                                    .get(&format!("way/{id}"))
                                    .and_then(|s| stored_geometry(s).bbox)
                            });
                            match b {
                                Some(b) => bbox = Some(union(bbox, b)),
                                None => all = false,
                            }
                        }
                        pbf::Kind::Relation => {}
                    }
                }
                match (&old, all) {
                    (Some(old), false) => stored_geometry(old),
                    _ => place::Geometry {
                        point: label.or(bbox.map(centre)),
                        bbox,
                    },
                }
            }
        };
        let route = c.kind == pbf::Kind::Relation
            && matches!(
                tags.get("type").copied(),
                Some("route" | "route_master" | "network")
            );
        let Some(kept) = place::kept(&tags).filter(|_| !route) else {
            if old.is_some() {
                removed.push(source_id);
            }
            continue;
        };
        let card = place::card(c.kind.name(), c.id, &tags, &kept, geometry);
        if let Some(old) = &old {
            if !old.unnamed && tags.contains_key("name") && !place::same_name(&old.name, &card.name)
            {
                renames.push((
                    source_id.clone(),
                    old.name.clone(),
                    old.category.clone(),
                    card.name.clone(),
                    c.timestamp.clone(),
                ));
            }
        }
        if c.action == osc::Action::Create || old.is_none() {
            created.push((card.clone(), c.changeset));
        }
        cards.push(card);
    }
    // Successors of the deleted objects: the one object created in this file or stored that is
    // tagged with the same Wikidata item, within 1 km; or the one created in the same changeset
    // with the same name within 50 m.
    let removed_set: std::collections::HashSet<&String> = removed.iter().collect();
    let mut by_item: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_set: HashMap<i64, Vec<usize>> = HashMap::new();
    for (n, (c, set)) in created.iter().enumerate() {
        if let Some(q) = c
            .details
            .get("tags")
            .and_then(|t| t.get("wikidata"))
            .and_then(Value::as_str)
        {
            by_item.entry(q).or_default().push(n);
        }
        if let Some(set) = set {
            by_set.entry(*set).or_default().push(n);
        }
    }
    let near = |c: &Card, at: &Option<Point>, metres: f64| {
        at.as_ref()
            .zip(c.point.as_ref())
            .is_some_and(|(a, b)| place::metres(a, b) <= metres)
    };
    let mut successors: Vec<(String, String, &'static str)> = vec![];
    for (id, changeset, old) in &deleted {
        let at = old.lat.zip(old.lon).map(|(lat, lon)| Point { lat, lon });
        let one = |found: Vec<&Card>| match found.as_slice() {
            [one] => Some(one.source_id.clone()),
            _ => None,
        };
        let mut by_wikidata = old.wikidata.as_deref().and_then(|q| {
            one(by_item
                .get(q)
                .into_iter()
                .flatten()
                .map(|n| &created[*n].0)
                .filter(|c| near(c, &at, WIKIDATA_METRES))
                .collect())
        });
        let by_changeset = changeset.and_then(|cs| {
            let name = place::folded(&old.name);
            one(by_set
                .get(&cs)
                .into_iter()
                .flatten()
                .map(|n| &created[*n].0)
                .filter(|c| place::folded(&c.name) == name && near(c, &at, NEAR_METRES))
                .collect())
        });
        // Or the one other stored object tagged with the same item, within 1 km.
        if by_wikidata.is_none()
            && !by_item.contains_key(old.wikidata.as_deref().unwrap_or_default())
        {
            if let (Some(q), Some(p)) = (&old.wikidata, &at) {
                let others: Vec<String> = sqlx::query_scalar("SELECT source_id FROM catalogue_card WHERE catalogue='osm' AND details#>>'{tags,wikidata}'=$1 AND source_id<>$2 AND ST_DWithin(point::geography,ST_SetSRID(ST_MakePoint($4,$3),4326)::geography,$5) LIMIT 2")
                    .bind(q)
                    .bind(id)
                    .bind(p.lat)
                    .bind(p.lon)
                    .bind(WIKIDATA_METRES)
                    .fetch_all(pool)
                    .await?;
                if let [one] = others.as_slice() {
                    if !removed_set.contains(one) {
                        by_wikidata = Some(one.clone());
                    }
                }
            }
        }
        if let Some(new) = by_wikidata {
            successors.push((id.clone(), new, "wikidata"));
        } else if let Some(new) = by_changeset {
            successors.push((id.clone(), new, "changeset"));
        }
    }
    // A broken change file never empties the catalogue: the removals an update makes, this
    // file's with those of the files before it, stay within a tenth of the cards.
    let held = job.state().cards.max(0) as u64;
    let gone = job.report().removed + removed_set.len() as u64;
    if super::diff::too_many(gone, held) {
        let note = format!(
            "change file {} would take the update's removals to {gone} of {held} cards",
            shown(&refuse_as)
        );
        return Err(job.refuse(refuse_as, "refused", note));
    }
    // An object that is back (a deletion undone) is no longer followed.
    let back: Vec<String> = cards.iter().map(|c| c.source_id.clone()).collect();
    // Recorded before the cards are written: a file applied again after a cut finds the old
    // names and the deleted cards gone, and would not see them again.
    record(pool, &successors, &renames, &deleted).await?;
    unfollow(pool, &back).await?;
    with_wikidata_fame(pool, wikidata_on, &mut cards).await?;
    let mut touched: Vec<String> = cards.iter().map(|c| c.source_id.clone()).collect();
    for part in cards.chunks(BATCH) {
        job.write(part, cursor.clone()).await?;
    }
    removed.sort();
    removed.dedup();
    job.remove(&removed, cursor.clone()).await?;
    touched.extend(removed.iter().cloned());
    touch_shared(pool, &touched).await?;
    for (old, new, _) in &successors {
        follow(pool, old, new).await?;
    }
    Ok((cards.len(), removed.len(), successors.len(), renames.len()))
}

/// The successors recorded for these objects, which are written again (a deletion reverted):
/// their rows go, and the shared links that became aliases of a successor answer for themselves
/// again, read on the OSM route.
pub(crate) async fn unfollow(pool: &PgPool, source_ids: &[String]) -> Result<()> {
    for part in source_ids.chunks(BATCH) {
        let (kinds, ids): (Vec<String>, Vec<i64>) = part
            .iter()
            .filter_map(|s| split(s).map(|(k, i)| (k.to_owned(), i)))
            .unzip();
        // Back: no longer a removed object.
        sqlx::query("DELETE FROM osm_removed o USING unnest($1::text[],$2::bigint[]) v(t,i) WHERE o.osm_type=v.t AND o.osm_id=v.i")
            .bind(&kinds)
            .bind(&ids)
            .execute(pool)
            .await?;
        let gone: Vec<(String, i64, String, i64)> = sqlx::query_as(
            "DELETE FROM osm_successor s USING unnest($1::text[],$2::bigint[]) v(t,i) WHERE s.old_type=v.t AND s.old_id=v.i RETURNING s.old_type,s.old_id,s.new_type,s.new_id",
        )
        .bind(&kinds)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
        for (old_type, old_id, new_type, new_id) in gone {
            let old = crate::model::OsmId {
                osm_type: old_type,
                id: old_id,
            };
            let root = crate::model::identity(&format!(
                "https://www.openstreetmap.org/{new_type}/{new_id}"
            ))?
            .0;
            let aliases: Vec<(String, String)> = sqlx::query_as(
                "SELECT a.alias_id,x.uri FROM resource_alias a JOIN resource x ON x.id=a.alias_id WHERE a.root_id=$1 AND a.basis='source'",
            )
            .bind(&root)
            .fetch_all(pool)
            .await?;
            for (alias, uri) in aliases {
                if crate::resolver::osm::parse_id(&uri).as_ref() != Some(&old) {
                    continue;
                }
                let mut tx = pool.begin().await?;
                crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
                if let Some(line) = crate::alias::release(&mut tx, &alias).await? {
                    crate::alias::record(&mut tx, vec![line]).await?;
                }
                // Back on the OSM route, as a new OSM link's row is (`store::insert_plain`).
                sqlx::query("UPDATE resource SET resolver='osm',osm_type=$2,osm_id=$3,osm_version=NULL,name=NULL,point=NULL,category=NULL,type=NULL,type_path=NULL,\
                    doc=jsonb_build_object('type','Place','url',uri,'osm_id',$2::text||'/'||$3::text,'source','osm','status','pending'),next_attempt_at=now(),priority_at=coalesce(priority_at,now()) WHERE id=$1")
                    .bind(&alias)
                    .bind(&old.osm_type)
                    .bind(old.id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
            }
        }
    }
    Ok(())
}

fn split(source_id: &str) -> Option<(&str, i64)> {
    let (k, i) = source_id.split_once('/')?;
    Some((k, i.parse().ok()?))
}

/// Record successors, renames, and the last card of each named deleted object (`osm_removed`:
/// the deleted-place rule compares its name, category and point once the card is gone).
async fn record(
    pool: &PgPool,
    successors: &[(String, String, &'static str)],
    renames: &[Rename],
    deleted: &[(String, Option<i64>, Stored)],
) -> Result<()> {
    for (id, _, old) in deleted {
        let (Some((t, i)), Some((lat, lon))) = (split(id), old.lat.zip(old.lon)) else {
            continue;
        };
        if old.unnamed || old.name.trim().is_empty() {
            continue;
        }
        sqlx::query("INSERT INTO osm_removed(osm_type,osm_id,name,category,point) VALUES($1,$2,$3,$4,ST_SetSRID(ST_MakePoint($6,$5),4326)) \
             ON CONFLICT (osm_type,osm_id) DO UPDATE SET name=excluded.name,category=excluded.category,point=excluded.point,removed_at=now()")
            .bind(t).bind(i).bind(&old.name).bind(&old.category).bind(lat).bind(lon)
            .execute(pool)
            .await?;
    }
    for (old, new, basis) in successors {
        let (Some((ot, oi)), Some((nt, ni))) = (split(old), split(new)) else {
            continue;
        };
        sqlx::query("INSERT INTO osm_successor(old_type,old_id,new_type,new_id,basis) VALUES($1,$2,$3,$4,$5) ON CONFLICT (old_type,old_id) DO UPDATE SET new_type=excluded.new_type,new_id=excluded.new_id,basis=excluded.basis,found_at=now()")
            .bind(ot).bind(oi).bind(nt).bind(ni).bind(basis)
            .execute(pool)
            .await?;
    }
    for (id, old_name, old_category, new_name, at) in renames {
        let Some((t, i)) = split(id) else { continue };
        let at = at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let mut old_name = old_name.clone();
        crate::model::truncate(&mut old_name, 512);
        let mut new_name = new_name.clone();
        crate::model::truncate(&mut new_name, 512);
        sqlx::query("INSERT INTO osm_rename(osm_type,osm_id,changed_at,old_name,old_category,new_name) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
            .bind(t).bind(i).bind(at).bind(old_name).bind(old_category).bind(new_name)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// The shared links of a deleted object whose successor is known become aliases of the
/// successor's link (`alias::source`: the change file is the evidence). The links stay as they
/// were written; reads answer with the successor.
pub async fn follow(pool: &PgPool, old: &str, new: &str) -> Result<()> {
    let Some((t, i)) = split(old) else {
        return Ok(());
    };
    let roots: Vec<(String, String)> = sqlx::query_as(
        "SELECT r.id,r.uri FROM resource r WHERE r.osm_type=$1 AND r.osm_id=$2 AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id)",
    )
    .bind(t)
    .bind(i)
    .fetch_all(pool)
    .await?;
    let new_uri = format!("https://www.openstreetmap.org/{new}");
    for (id, uri) in roots {
        let Some(alias) = crate::alias::source(&uri, &new_uri) else {
            continue;
        };
        let mut tx = pool.begin().await?;
        crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
        crate::alias::lock(&mut tx).await?;
        let line = crate::alias::assign(&mut tx, &id, &alias).await?;
        crate::alias::record(&mut tx, line.into_iter().collect()).await?;
        tx.commit().await?;
    }
    Ok(())
}

/// The successor recorded for a deleted object, when the evidence is exact.
pub async fn successor(pool: &PgPool, osm_type: &str, osm_id: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT new_type||'/'||new_id FROM osm_successor WHERE old_type=$1 AND old_id=$2 AND basis IN ('wikidata','changeset') \
         AND EXISTS (SELECT 1 FROM catalogue_state WHERE catalogue='osm' AND phase<>'disabled')",
    )
    .bind(osm_type)
    .bind(osm_id)
    .fetch_optional(pool)
    .await?)
}

/// An imported OSM card as the OSM route and the unshared reads read it.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct OsmCard {
    pub source_id: String,
    pub uri: String,
    pub name: String,
    #[sqlx(rename = "type")]
    pub r#type: String,
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    pub details: Value,
}
const OSM_CARD_COLUMNS: &str =
    "source_id,uri,name,type,ST_X(point) AS lon,ST_Y(point) AS lat,details";

/// The imported cards of these objects (`node/1`), by source ID.
pub async fn cards(pool: &PgPool, source_ids: &[String]) -> Result<Vec<OsmCard>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {OSM_CARD_COLUMNS} FROM catalogue_card WHERE catalogue='osm' AND source_id=ANY($1)"
    ))
    .bind(source_ids)
    .fetch_all(pool)
    .await?)
}
/// The imported card whose own link has resource ID `id` (its `url:` key), if any.
pub async fn card_by_link(pool: &PgPool, id: &str) -> Result<Option<OsmCard>> {
    if !crate::model::valid_id(id) {
        return Ok(None);
    }
    let found: Vec<OsmCard> = sqlx::query_as(&format!(
        "SELECT {OSM_CARD_COLUMNS} FROM catalogue_card WHERE catalogue='osm' AND keys @> ARRAY['url:'||$1]::text[] LIMIT 2"
    ))
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(found
        .into_iter()
        .find(|c| crate::model::identity(&c.uri).is_ok_and(|(own, _, _)| own == id)))
}

/// The OSM route's card of an imported object, as `resolver::osm::map_place` builds one from
/// Nominatim's answer, and the display name its place row keeps. `None` for a card that makes no
/// valid document.
pub fn document(card: &OsmCard, ttl: u64) -> Option<(crate::model::ResourceMeta, String)> {
    use crate::resolver::osm::{parse_id, ATTRIBUTES, ATTRIBUTION};
    let id = parse_id(&card.uri)?;
    let mut doc = crate::model::ResourceMeta::new(&id.uri(), "osm", ttl);
    doc.osm_id = Some(id.key());
    doc.attribution = Some(ATTRIBUTION.into());
    doc.location = card.lat.zip(card.lon).map(|(lat, lon)| Point { lat, lon });
    doc.location_source = doc
        .location
        .as_ref()
        .map(|_| crate::model::LocationSource::Osm);
    doc.category = card.details["category"].as_str().map(str::to_owned);
    doc.set_type(&card.r#type);
    doc.name = Some(card.name.clone());
    if let Some(address) = card.details["address"].as_object() {
        for (k, v) in address {
            if let Some(v) = v.as_str() {
                doc.address.insert(k.clone(), v.to_owned());
            }
        }
    }
    if let Some(tags) = card.details["tags"].as_object() {
        for key in ATTRIBUTES {
            if let Some(v) = tags.get(*key).and_then(Value::as_str) {
                doc.attributes.insert((*key).into(), v.into());
            }
        }
    }
    doc.bbox = serde_json::from_value::<[f64; 4]>(card.details["bbox"].clone()).ok();
    let a = &doc.address;
    let street = [a.get("street"), a.get("house_number")]
        .into_iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let town = [a.get("postcode"), a.get("city")]
        .into_iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let display = [card.name.clone(), street, town]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let doc = doc.bounded(ATTRIBUTES).ok()?;
    Some((doc, display))
}

#[cfg(test)]
mod tests;
