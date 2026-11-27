//! OpenStreetMap's PBF format (`*.osm.pbf`), read as a stream of blobs: a header blob, then
//! data blobs of nodes, ways and relations (`OSMHeader`, `OSMData`). The protobuf messages are
//! decoded here by hand, with a cap on every size a file states, so a damaged or hostile file
//! fails with an error and never makes the reader allocate past the caps or panic.
//!
//! Only what the importer reads is decoded: ids, tags, node coordinates, way node references,
//! relation members and the header's replication fields. Element metadata (versions, users) is
//! skipped. Blobs are zlib-compressed or raw; other compressions are refused.
use anyhow::{bail, ensure, Context, Result};
use std::io::Read;

/// The largest `BlobHeader` read (the format's limit is 64 KiB).
pub const MAX_HEADER: usize = 64 * 1024;
/// The largest blob, compressed or not (the format's limit is 32 MiB).
pub const MAX_BLOB: usize = 32 * 1024 * 1024;
/// The most tags one element keeps; further tags are left out.
pub const MAX_TAGS: usize = 1024;
/// The most node references one way keeps (OSM's API allows 2,000).
pub const MAX_REFS: usize = 100_000;
/// The most members one relation keeps (OSM's API allows 32,000).
pub const MAX_MEMBERS: usize = 100_000;
/// The most strings one block's table holds (blocks of real files hold some thousands).
pub const MAX_STRINGS: usize = 1 << 20;
/// The most elements one block holds (real files hold 8,000).
pub const MAX_ELEMENTS: usize = 1 << 20;
/// The most way node references and relation members one block holds in all (real files: some
/// tens of thousands).
pub const MAX_BLOCK_REFS: usize = 8 << 20;
/// The most tag keys and values one block's dense nodes hold in all.
pub const MAX_DENSE_TAGS: usize = 4 << 20;
/// The most tag keys and values one block's other nodes, ways and relations hold in all.
pub const MAX_BLOCK_TAGS: usize = 4 << 20;
/// Blocks read ahead of the one passed on, per decoding thread: what waits in memory is bounded
/// even when one block takes long. With the caps above a block decodes to at most about 250 MB
/// (real ones: a few MB), so 8 threads hold at most about 4 GB.
pub const AHEAD_PER_THREAD: u64 = 2;

/// The protobuf wire types.
const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const BYTES: u8 = 2;
const FIXED32: u8 = 5;

/// A protobuf message being read.
#[derive(Clone, Copy)]
struct Message<'a> {
    data: &'a [u8],
    at: usize,
}
/// One field of a message.
#[derive(Clone, Copy)]
enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}
impl<'a> Message<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }
    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.data.get(self.at).context("pbf: varint past the end")?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("pbf: varint longer than 10 bytes")
    }
    /// The next field, or none at the end.
    fn field(&mut self) -> Result<Option<(u32, Value<'a>)>> {
        if self.at >= self.data.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let number = u32::try_from(key >> 3).context("pbf: field number too large")?;
        let value = match (key & 7) as u8 {
            VARINT => Value::Varint(self.varint()?),
            BYTES => {
                let len = usize::try_from(self.varint()?).context("pbf: length too large")?;
                let end = self
                    .at
                    .checked_add(len)
                    .filter(|end| *end <= self.data.len())
                    .context("pbf: field past the end")?;
                let bytes = &self.data[self.at..end];
                self.at = end;
                Value::Bytes(bytes)
            }
            FIXED64 => {
                ensure!(self.at + 8 <= self.data.len(), "pbf: fixed64 past the end");
                self.at += 8;
                Value::Fixed
            }
            FIXED32 => {
                ensure!(self.at + 4 <= self.data.len(), "pbf: fixed32 past the end");
                self.at += 4;
                Value::Fixed
            }
            other => bail!("pbf: wire type {other} not supported"),
        };
        Ok(Some((number, value)))
    }
}
fn zigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}
fn bytes(value: Value<'_>) -> Result<&[u8]> {
    match value {
        Value::Bytes(b) => Ok(b),
        _ => bail!("pbf: expected a length-delimited field"),
    }
}
fn int(value: Value<'_>) -> Result<u64> {
    match value {
        Value::Varint(v) => Ok(v),
        _ => bail!("pbf: expected a varint field"),
    }
}
/// The varints of a packed field (or of one unpacked varint), each passed to `each`; stops
/// with an error after `cap` values.
fn packed(value: Value<'_>, cap: usize, mut each: impl FnMut(u64) -> Result<()>) -> Result<()> {
    match value {
        Value::Varint(v) => {
            ensure!(cap > 0, "pbf: packed field over its cap");
            each(v)
        }
        Value::Bytes(b) => {
            let mut m = Message::new(b);
            let mut n = 0usize;
            while m.at < b.len() {
                n += 1;
                ensure!(n <= cap, "pbf: packed field over its cap");
                each(m.varint()?)?;
            }
            Ok(())
        }
        Value::Fixed => bail!("pbf: expected a packed field"),
    }
}

/// What the file's header block says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Header {
    pub required_features: Vec<String>,
    /// Features the file has that a reader need not read (`Sort.Type_then_ID`).
    pub optional_features: Vec<String>,
    pub replication_timestamp: Option<i64>,
    pub replication_sequence: Option<i64>,
    pub replication_base_url: Option<String>,
}
/// The features a file may require that this reader reads.
pub const FEATURES: &[&str] = &["OsmSchema-V0.6", "DenseNodes"];

pub fn header(data: &[u8]) -> Result<Header> {
    let mut header = Header::default();
    let mut m = Message::new(data);
    while let Some((number, value)) = m.field()? {
        match number {
            4 => {
                ensure!(
                    header.required_features.len() < 64,
                    "pbf: too many features"
                );
                header
                    .required_features
                    .push(String::from_utf8_lossy(bytes(value)?).into_owned());
            }
            5 => {
                ensure!(
                    header.optional_features.len() < 64,
                    "pbf: too many features"
                );
                header
                    .optional_features
                    .push(String::from_utf8_lossy(bytes(value)?).into_owned());
            }
            32 => header.replication_timestamp = Some(int(value)? as i64),
            33 => header.replication_sequence = Some(int(value)? as i64),
            34 => {
                let url = bytes(value)?;
                ensure!(url.len() <= 2048, "pbf: replication URL too long");
                header.replication_base_url = Some(String::from_utf8_lossy(url).into_owned());
            }
            _ => {}
        }
    }
    for feature in &header.required_features {
        if !FEATURES.contains(&feature.as_str()) {
            bail!("pbf: required feature {feature:?} not supported");
        }
    }
    Ok(header)
}

/// A node: its id, its coordinates in 1e-7 degrees, and its tags as string-table indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: i64,
    pub lat: i32,
    pub lon: i32,
    pub tags: Vec<(u32, u32)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Way {
    pub id: i64,
    pub tags: Vec<(u32, u32)>,
    pub refs: Vec<i64>,
}
/// A relation member: its type (`node`, `way`, `relation`), id and role (a string-table index).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub kind: Kind,
    pub id: i64,
    pub role: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Node,
    Way,
    Relation,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Node => "node",
            Kind::Way => "way",
            Kind::Relation => "relation",
        }
    }
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "node" => Some(Kind::Node),
            "way" => Some(Kind::Way),
            "relation" => Some(Kind::Relation),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub id: i64,
    pub tags: Vec<(u32, u32)>,
    pub members: Vec<Member>,
}
/// One data block, decoded: its string table and its elements.
#[derive(Clone, Debug, Default)]
pub struct Block {
    /// The string table; an entry that is not UTF-8 is empty (its tags are left out).
    pub strings: Vec<Box<str>>,
    pub nodes: Vec<Node>,
    pub ways: Vec<Way>,
    pub relations: Vec<Relation>,
}
impl Block {
    pub fn string(&self, i: u32) -> &str {
        self.strings.get(i as usize).map_or("", |s| s)
    }
    /// An element's tags as text, without empty keys.
    pub fn tags<'a>(&'a self, tags: &'a [(u32, u32)]) -> impl Iterator<Item = (&'a str, &'a str)> {
        tags.iter()
            .map(|(k, v)| (self.string(*k), self.string(*v)))
            .filter(|(k, _)| !k.is_empty())
    }
}
/// Coordinates in 1e-7 degrees from a block's raw value, offset and granularity (nanodegrees):
/// none when outside the globe.
fn degrees(raw: i64, offset: i64, granularity: i64, max: i64) -> Option<i32> {
    let nano = raw.checked_mul(granularity)?.checked_add(offset)?;
    let e7 = nano / 100;
    (-max..=max).contains(&e7).then_some(e7 as i32)
}
fn tag_pairs(keys: &[u32], vals: &[u32], strings: usize) -> Vec<(u32, u32)> {
    keys.iter()
        .zip(vals)
        .filter(|(k, v)| (**k as usize) < strings && (**v as usize) < strings)
        .take(MAX_TAGS)
        .map(|(k, v)| (*k, *v))
        .collect()
}
/// The string indexes of a packed field added to `out`, which holds at most `cap` in all (a field
/// repeated in one message counts once with the others).
fn indexes(value: Value<'_>, cap: usize, out: &mut Vec<u32>) -> Result<()> {
    packed(value, cap.saturating_sub(out.len()), |v| {
        out.push(u32::try_from(v).context("pbf: string index too large")?);
        Ok(())
    })
}

/// Decode one `OSMData` block.
pub fn block(data: &[u8]) -> Result<Block> {
    let mut out = Block::default();
    let mut groups = vec![];
    let (mut granularity, mut lat_offset, mut lon_offset) = (100i64, 0i64, 0i64);
    let mut m = Message::new(data);
    while let Some((number, value)) = m.field()? {
        match number {
            1 => {
                let mut table = Message::new(bytes(value)?);
                while let Some((n, v)) = table.field()? {
                    if n == 1 {
                        let s = bytes(v)?;
                        ensure!(
                            out.strings.len() < MAX_STRINGS,
                            "pbf: string table over its cap"
                        );
                        out.strings.push(match std::str::from_utf8(s) {
                            Ok(s) => super::place::clean(s).into(),
                            Err(_) => "".into(),
                        });
                    }
                }
            }
            2 => groups.push(bytes(value)?),
            17 => {
                granularity = int(value)? as i64;
                ensure!(
                    (1..=1_000_000_000).contains(&granularity),
                    "pbf: granularity out of range"
                );
            }
            19 => lat_offset = int(value)? as i64,
            20 => lon_offset = int(value)? as i64,
            _ => {}
        }
    }
    let lat = |raw: i64| degrees(raw, lat_offset, granularity, 900_000_000);
    let lon = |raw: i64| degrees(raw, lon_offset, granularity, 1_800_000_000);
    let strings = out.strings.len();
    let (mut refs_held, mut tags_held) = (0usize, 0usize);
    for group in groups {
        let mut g = Message::new(group);
        while let Some((number, value)) = g.field()? {
            ensure!(
                out.nodes.len() + out.ways.len() + out.relations.len() < MAX_ELEMENTS,
                "pbf: a block over its cap of elements"
            );
            match number {
                1 => {
                    let (mut id, mut la, mut lo) = (0i64, 0i64, 0i64);
                    let (mut keys, mut vals) = (vec![], vec![]);
                    let mut n = Message::new(bytes(value)?);
                    while let Some((f, v)) = n.field()? {
                        match f {
                            1 => id = zigzag(int(v)?),
                            2 => indexes(v, MAX_TAGS * 64, &mut keys)?,
                            3 => indexes(v, MAX_TAGS * 64, &mut vals)?,
                            8 => la = zigzag(int(v)?),
                            9 => lo = zigzag(int(v)?),
                            _ => {}
                        }
                    }
                    tags_held += keys.len() + vals.len();
                    ensure!(
                        tags_held <= MAX_BLOCK_TAGS,
                        "pbf: a block over its cap of tags"
                    );
                    if let (Some(la), Some(lo)) = (lat(la), lon(lo)) {
                        if id > 0 {
                            out.nodes.push(Node {
                                id,
                                lat: la,
                                lon: lo,
                                tags: tag_pairs(&keys, &vals, strings),
                            });
                        }
                    }
                }
                2 => {
                    let room =
                        MAX_ELEMENTS - (out.nodes.len() + out.ways.len() + out.relations.len());
                    dense(bytes(value)?, strings, &lat, &lon, &mut out.nodes, room)?
                }
                3 => {
                    let mut id = 0i64;
                    let (mut keys, mut vals, mut refs) = (vec![], vec![], vec![]);
                    let mut w = Message::new(bytes(value)?);
                    while let Some((f, v)) = w.field()? {
                        match f {
                            1 => id = int(v)? as i64,
                            2 => indexes(v, MAX_TAGS * 64, &mut keys)?,
                            3 => indexes(v, MAX_TAGS * 64, &mut vals)?,
                            8 => {
                                let mut at = 0i64;
                                packed(v, MAX_REFS * 64, |d| {
                                    at = at.wrapping_add(zigzag(d));
                                    if refs.len() < MAX_REFS {
                                        refs.push(at);
                                    }
                                    Ok(())
                                })?;
                            }
                            _ => {}
                        }
                    }
                    tags_held += keys.len() + vals.len();
                    ensure!(
                        tags_held <= MAX_BLOCK_TAGS,
                        "pbf: a block over its cap of tags"
                    );
                    refs_held += refs.len();
                    ensure!(
                        refs_held <= MAX_BLOCK_REFS,
                        "pbf: a block over its cap of references"
                    );
                    if id > 0 {
                        out.ways.push(Way {
                            id,
                            tags: tag_pairs(&keys, &vals, strings),
                            refs,
                        });
                    }
                }
                4 => {
                    let mut id = 0i64;
                    let (mut keys, mut vals, mut roles, mut ids, mut types) =
                        (vec![], vec![], vec![], vec![], vec![]);
                    let mut r = Message::new(bytes(value)?);
                    while let Some((f, v)) = r.field()? {
                        match f {
                            1 => id = int(v)? as i64,
                            2 => indexes(v, MAX_TAGS * 64, &mut keys)?,
                            3 => indexes(v, MAX_TAGS * 64, &mut vals)?,
                            8 => indexes(v, MAX_MEMBERS * 4, &mut roles)?,
                            9 => {
                                let mut at = 0i64;
                                packed(v, (MAX_MEMBERS * 4).saturating_sub(ids.len()), |d| {
                                    at = at.wrapping_add(zigzag(d));
                                    ids.push(at);
                                    Ok(())
                                })?;
                            }
                            10 => packed(v, (MAX_MEMBERS * 4).saturating_sub(types.len()), |t| {
                                types.push(t);
                                Ok(())
                            })?,
                            _ => {}
                        }
                    }
                    tags_held += keys.len() + vals.len();
                    ensure!(
                        tags_held <= MAX_BLOCK_TAGS,
                        "pbf: a block over its cap of tags"
                    );
                    let members = ids
                        .iter()
                        .zip(&types)
                        .zip(roles.iter().chain(std::iter::repeat(&0)))
                        .filter_map(|((id, t), role)| {
                            let kind = match t {
                                0 => Kind::Node,
                                1 => Kind::Way,
                                2 => Kind::Relation,
                                _ => return None,
                            };
                            (*id > 0).then_some(Member {
                                kind,
                                id: *id,
                                role: if (*role as usize) < strings { *role } else { 0 },
                            })
                        })
                        .take(MAX_MEMBERS)
                        .collect::<Vec<_>>();
                    refs_held += ids.len();
                    ensure!(
                        refs_held <= MAX_BLOCK_REFS,
                        "pbf: a block over its cap of references"
                    );
                    if id > 0 {
                        out.relations.push(Relation {
                            id,
                            tags: tag_pairs(&keys, &vals, strings),
                            members,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// `DenseNodes`: delta-coded ids and coordinates, and one run of key/value indexes per node,
/// each ended by a 0.
fn dense(
    data: &[u8],
    strings: usize,
    lat: &dyn Fn(i64) -> Option<i32>,
    lon: &dyn Fn(i64) -> Option<i32>,
    nodes: &mut Vec<Node>,
    room: usize,
) -> Result<()> {
    // At most the elements the block has room for, refused as they are read (not once held).
    let cap = room;
    let (mut ids, mut lats, mut lons, mut kv) = (vec![], vec![], vec![], vec![]);
    let mut m = Message::new(data);
    while let Some((f, v)) = m.field()? {
        let delta = |out: &mut Vec<i64>| {
            let mut at = 0i64;
            packed(v, cap.saturating_sub(out.len()), |d| {
                at = at.wrapping_add(zigzag(d));
                out.push(at);
                Ok(())
            })
        };
        match f {
            1 => delta(&mut ids)?,
            8 => delta(&mut lats)?,
            9 => delta(&mut lons)?,
            10 => indexes(v, MAX_DENSE_TAGS, &mut kv)?,
            _ => {}
        }
    }
    ensure!(
        ids.len() == lats.len() && ids.len() == lons.len(),
        "pbf: dense nodes of uneven lengths"
    );
    ensure!(
        nodes.len() + ids.len() <= MAX_ELEMENTS,
        "pbf: a block over its cap of elements"
    );
    let mut kv = kv.into_iter();
    for ((id, la), lo) in ids.into_iter().zip(lats).zip(lons) {
        let mut tags = vec![];
        loop {
            match kv.next() {
                None | Some(0) => break,
                Some(k) => {
                    let Some(v) = kv.next() else { break };
                    if tags.len() < MAX_TAGS && (k as usize) < strings && (v as usize) < strings {
                        tags.push((k, v));
                    }
                }
            }
        }
        if let (Some(la), Some(lo)) = (lat(la), lon(lo)) {
            if id > 0 {
                nodes.push(Node {
                    id,
                    lat: la,
                    lon: lo,
                    tags,
                });
            }
        }
    }
    Ok(())
}

/// One blob of the file as stored: its type (`OSMHeader`, `OSMData`) and its `Blob` message,
/// still compressed, with the byte offset it starts at.
#[derive(Clone, Debug)]
pub struct RawBlob {
    pub offset: u64,
    pub kind: String,
    pub data: Vec<u8>,
}
/// The blob's content, decompressed (at most [`MAX_BLOB`] bytes).
pub fn inflate(blob: &[u8]) -> Result<Vec<u8>> {
    let (mut raw, mut size, mut zlib) = (None, None, None);
    let mut m = Message::new(blob);
    while let Some((number, value)) = m.field()? {
        match number {
            1 => raw = Some(bytes(value)?),
            2 => size = Some(int(value)?),
            3 => zlib = Some(bytes(value)?),
            4..=7 => bail!("pbf: blob compression not supported"),
            _ => {}
        }
    }
    if let Some(raw) = raw {
        return Ok(raw.to_vec());
    }
    let zlib = zlib.context("pbf: blob holds no data")?;
    let size = usize::try_from(size.context("pbf: compressed blob without its size")?)
        .ok()
        .filter(|s| *s <= MAX_BLOB)
        .context("pbf: blob over 32 MiB")?;
    let mut out = Vec::with_capacity(size);
    flate2::read::ZlibDecoder::new(zlib)
        .take(size as u64 + 1)
        .read_to_end(&mut out)
        .context("pbf: blob does not inflate")?;
    ensure!(out.len() == size, "pbf: blob size differs from its header");
    Ok(out)
}

/// Reads a PBF file's blobs in order.
pub struct Reader<R> {
    inner: R,
    offset: u64,
}
impl<R: Read> Reader<R> {
    /// `offset` is where `inner` starts in the file (a blob boundary).
    pub fn new(inner: R, offset: u64) -> Self {
        Self { inner, offset }
    }
    /// The next blob, or none at the end of the file.
    pub fn next_blob(&mut self) -> Result<Option<RawBlob>> {
        let offset = self.offset;
        let mut len = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            match self.inner.read(&mut len[got..]) {
                Ok(0) if got == 0 => return Ok(None),
                Ok(0) => bail!("pbf: file ends inside a blob header length"),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let len = u32::from_be_bytes(len) as usize;
        ensure!(len <= MAX_HEADER, "pbf: blob header over 64 KiB");
        let mut head = vec![0u8; len];
        self.inner
            .read_exact(&mut head)
            .context("pbf: file ends inside a blob header")?;
        let (mut kind, mut size) = (None, None);
        let mut m = Message::new(&head);
        while let Some((number, value)) = m.field()? {
            match number {
                1 => kind = Some(String::from_utf8_lossy(bytes(value)?).into_owned()),
                3 => size = Some(int(value)?),
                _ => {}
            }
        }
        let kind = kind.context("pbf: blob header without a type")?;
        let size = usize::try_from(size.context("pbf: blob header without a size")?)
            .ok()
            .filter(|s| *s <= MAX_BLOB)
            .context("pbf: blob over 32 MiB")?;
        let mut data = vec![0u8; size];
        self.inner
            .read_exact(&mut data)
            .context("pbf: file ends inside a blob")?;
        self.offset += 4 + len as u64 + size as u64;
        Ok(Some(RawBlob { offset, kind, data }))
    }
    /// Where the next blob starts.
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

/// A data block decoded off the reading thread, with the blob it came from.
pub struct Decoded {
    pub index: u64,
    pub offset: u64,
    /// Where the blob after it starts.
    pub next: u64,
    pub block: Block,
}

/// Read the data blobs of a PBF file from `start` (a blob boundary; 0 for the whole file, whose
/// header is checked), decoding them on `threads` threads, and pass each block to `each` in file
/// order with its index (counted from `first_index`). `each` returns false to stop early.
///
/// A panic while decoding a block or in `each` ends the read with an error (a [`Failure`] of
/// class `panic`): every thread stops, none is left waiting.
///
/// [`Failure`]: crate::catalogue::failure::Failure
pub fn read_blocks(
    path: &std::path::Path,
    start: u64,
    first_index: u64,
    threads: usize,
    each: impl FnMut(Decoded) -> Result<bool>,
) -> Result<Option<Header>> {
    read_blocks_with(path, start, first_index, threads, decode, each)
}

/// A blob's bytes decoded to a block.
fn decode(blob: &[u8]) -> Result<Block> {
    block(&inflate(blob)?)
}

/// A panic caught on a reading thread, as the read's error.
fn panicked(what: &'static str) -> anyhow::Error {
    anyhow::Error::new(crate::catalogue::failure::Failure::new("panic")).context(what)
}

/// Sets `stop` under the turn's lock and wakes the feeder when dropped: the read stops on every
/// way out, a panic included.
struct StopOnDrop<'a> {
    stop: &'a std::sync::atomic::AtomicBool,
    passed: &'a (std::sync::Mutex<u64>, std::sync::Condvar),
}
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        {
            // Set under the turn's lock: a feeder about to wait sees it, one waiting is woken.
            let _held = self.passed.0.lock();
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.passed.1.notify_all();
    }
}

/// [`read_blocks`] with the decoder given (tests pass one that panics).
fn read_blocks_with(
    path: &std::path::Path,
    start: u64,
    first_index: u64,
    threads: usize,
    decode: fn(&[u8]) -> Result<Block>,
    mut each: impl FnMut(Decoded) -> Result<bool>,
) -> Result<Option<Header>> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path).with_context(|| format!("{}", path.display()))?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = Reader::new(std::io::BufReader::with_capacity(1 << 20, file), start);
    let threads = threads.clamp(1, 32);
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<(u64, RawBlob, u64)>(threads);
    let raw_rx = std::sync::Arc::new(std::sync::Mutex::new(raw_rx));
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<Result<Decoded>>(threads);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The index of the block to pass on next: the feeder reads at most `ahead` blocks past it.
    let passed = std::sync::Arc::new((
        std::sync::Mutex::new(first_index),
        std::sync::Condvar::new(),
    ));
    let ahead = threads.clamp(1, 32) as u64 * AHEAD_PER_THREAD;
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let (raw_rx, done_tx, stop) = (raw_rx.clone(), done_tx.clone(), stop.clone());
            std::thread::spawn(move || loop {
                let next = raw_rx.lock().map(|rx| rx.recv());
                let Ok(Ok((index, blob, next))) = next else {
                    return;
                };
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let decoded = std::panic::catch_unwind(|| decode(&blob.data))
                    .unwrap_or_else(|_| Err(panicked("pbf: decoding a block panicked")))
                    .map(|block| Decoded {
                        index,
                        offset: blob.offset,
                        next,
                        block,
                    });
                if done_tx.send(decoded).is_err() {
                    return;
                }
            })
        })
        .collect();
    // Only the workers hold the receiver: once they have all stopped, the feeder's send fails.
    drop(raw_rx);
    drop(done_tx);
    let result = std::thread::scope(|scope| -> Result<Option<Header>> {
        let stop_reading = stop.clone();
        let passed_feeder = passed.clone();
        let feeder = scope.spawn(move || -> Result<(u64, Option<Header>)> {
            let mut header = None;
            let mut index = first_index;
            while let Some(blob) = reader.next_blob()? {
                if stop_reading.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                match blob.kind.as_str() {
                    "OSMHeader" => {
                        let h = self::header(&inflate(&blob.data)?)?;
                        header = Some(h);
                    }
                    "OSMData" => {
                        let (lock, turn) = &*passed_feeder;
                        let mut at = lock
                            .lock()
                            .map_err(|_| anyhow::anyhow!("pbf: reader poisoned"))?;
                        while index >= *at + ahead
                            && !stop_reading.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            at = turn
                                .wait(at)
                                .map_err(|_| anyhow::anyhow!("pbf: reader poisoned"))?;
                        }
                        if stop_reading.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        #[cfg(test)]
                        tests::MOST_AHEAD.fetch_max(
                            index as i64 + 1 - (*at + ahead) as i64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        drop(at);
                        let next = reader.offset();
                        if raw_tx.send((index, blob, next)).is_err() {
                            break;
                        }
                        index += 1;
                    }
                    _ => {}
                }
            }
            drop(raw_tx);
            Ok((index, header))
        });
        let stopping = StopOnDrop {
            stop: &stop,
            passed: &passed,
        };
        // Blocks come back in any order; they are passed on in file order.
        let mut waiting = std::collections::BTreeMap::new();
        let mut want = first_index;
        let mut outcome = Ok(());
        let mut early = false;
        for decoded in done_rx.iter() {
            match decoded {
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
                Ok(d) => {
                    waiting.insert(d.index, d);
                    let mut go_on = true;
                    while let Some(d) = waiting.remove(&want) {
                        want += 1;
                        if let Ok(mut at) = passed.0.lock() {
                            *at = want;
                        }
                        passed.1.notify_all();
                        let passed_on =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| each(d)))
                                .unwrap_or_else(|_| {
                                    Err(panicked("pbf: a block's reader panicked"))
                                });
                        match passed_on {
                            Ok(true) => {}
                            Ok(false) => {
                                go_on = false;
                                break;
                            }
                            Err(e) => {
                                outcome = Err(e);
                                go_on = false;
                                break;
                            }
                        }
                    }
                    if !go_on {
                        early = true;
                        break;
                    }
                }
            }
        }
        drop(stopping);
        // Drain so the feeder and the workers can end.
        drop(done_rx);
        let fed = feeder
            .join()
            .map_err(|_| panicked("pbf: the file's reader panicked"))?;
        outcome?;
        let (fed, header) = fed?;
        ensure!(
            early || (waiting.is_empty() && want == fed),
            "pbf: blocks missing"
        );
        Ok(header)
    });
    for worker in workers {
        let _ = worker.join();
    }
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A protobuf writer for test files.
    /// How far past its bound the feeder ever read ahead of the blocks passed on, over every read
    /// in this test run (its block index + 1 minus the bound): never above 0.
    pub static MOST_AHEAD: std::sync::atomic::AtomicI64 =
        std::sync::atomic::AtomicI64::new(i64::MIN);
    #[derive(Default)]
    pub struct Writer(pub Vec<u8>);
    impl Writer {
        pub fn varint(&mut self, mut v: u64) {
            loop {
                let byte = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    self.0.push(byte);
                    return;
                }
                self.0.push(byte | 0x80);
            }
        }
        pub fn key(&mut self, number: u32, wire: u8) {
            self.varint(u64::from(number) << 3 | u64::from(wire));
        }
        pub fn int(&mut self, number: u32, v: u64) {
            self.key(number, VARINT);
            self.varint(v);
        }
        pub fn bytes(&mut self, number: u32, b: &[u8]) {
            self.key(number, BYTES);
            self.varint(b.len() as u64);
            self.0.extend_from_slice(b);
        }
        pub fn packed(&mut self, number: u32, values: &[u64]) {
            let mut inner = Writer::default();
            for v in values {
                inner.varint(*v);
            }
            self.bytes(number, &inner.0);
        }
    }
    pub fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }
    fn deltas(values: &[i64]) -> Vec<u64> {
        let mut last = 0;
        values
            .iter()
            .map(|v| {
                let d = v - last;
                last = *v;
                zz(d)
            })
            .collect()
    }

    /// An element of a test file.
    pub enum Element<'a> {
        Node(i64, f64, f64, &'a [(&'a str, &'a str)]),
        Way(i64, &'a [i64], &'a [(&'a str, &'a str)]),
        Relation(i64, &'a [(Kind, i64, &'a str)], &'a [(&'a str, &'a str)]),
    }
    /// One `OSMData` block of `elements` (dense nodes, ways, relations, each in its own group).
    pub fn data_block(elements: &[Element<'_>]) -> Vec<u8> {
        let mut strings: Vec<String> = vec![String::new()];
        let mut index = |s: &str| -> u64 {
            if let Some(i) = strings.iter().position(|x| x == s) {
                return i as u64;
            }
            strings.push(s.to_owned());
            (strings.len() - 1) as u64
        };
        let mut dense = (vec![], vec![], vec![], vec![]);
        let mut ways = vec![];
        let mut relations = vec![];
        for e in elements {
            match e {
                Element::Node(id, lat, lon, tags) => {
                    dense.0.push(*id);
                    dense.1.push((lat * 1e7).round() as i64);
                    dense.2.push((lon * 1e7).round() as i64);
                    for (k, v) in *tags {
                        dense.3.push(index(k));
                        dense.3.push(index(v));
                    }
                    dense.3.push(0);
                }
                Element::Way(id, refs, tags) => {
                    let mut w = Writer::default();
                    w.int(1, *id as u64);
                    w.packed(2, &tags.iter().map(|(k, _)| index(k)).collect::<Vec<_>>());
                    w.packed(3, &tags.iter().map(|(_, v)| index(v)).collect::<Vec<_>>());
                    w.packed(8, &deltas(refs));
                    ways.push(w.0);
                }
                Element::Relation(id, members, tags) => {
                    let mut r = Writer::default();
                    r.int(1, *id as u64);
                    r.packed(2, &tags.iter().map(|(k, _)| index(k)).collect::<Vec<_>>());
                    r.packed(3, &tags.iter().map(|(_, v)| index(v)).collect::<Vec<_>>());
                    r.packed(
                        8,
                        &members
                            .iter()
                            .map(|(_, _, role)| index(role))
                            .collect::<Vec<_>>(),
                    );
                    r.packed(
                        9,
                        &deltas(&members.iter().map(|(_, id, _)| *id).collect::<Vec<_>>()),
                    );
                    r.packed(
                        10,
                        &members
                            .iter()
                            .map(|(k, _, _)| match k {
                                Kind::Node => 0,
                                Kind::Way => 1,
                                Kind::Relation => 2,
                            })
                            .collect::<Vec<_>>(),
                    );
                    relations.push(r.0);
                }
            }
        }
        let mut block = Writer::default();
        let mut table = Writer::default();
        for s in &strings {
            table.bytes(1, s.as_bytes());
        }
        block.bytes(1, &table.0);
        if !dense.0.is_empty() {
            // Granularity 100 nanodegrees: the raw values are 1e-7 degrees.
            let mut d = Writer::default();
            d.packed(1, &deltas(&dense.0));
            d.packed(8, &deltas(&dense.1));
            d.packed(9, &deltas(&dense.2));
            d.packed(10, &dense.3);
            let mut g = Writer::default();
            g.bytes(2, &d.0);
            block.bytes(2, &g.0);
        }
        if !ways.is_empty() {
            let mut g = Writer::default();
            for w in &ways {
                g.bytes(3, w);
            }
            block.bytes(2, &g.0);
        }
        if !relations.is_empty() {
            let mut g = Writer::default();
            for r in &relations {
                g.bytes(4, r);
            }
            block.bytes(2, &g.0);
        }
        block.0
    }
    /// A whole file: a header blob, then one zlib blob per block.
    pub fn file(header: &Header, blocks: &[Vec<u8>]) -> Vec<u8> {
        use std::io::Write;
        let mut h = Writer::default();
        for f in &header.required_features {
            h.bytes(4, f.as_bytes());
        }
        for f in &header.optional_features {
            h.bytes(5, f.as_bytes());
        }
        if let Some(t) = header.replication_timestamp {
            h.int(32, t as u64);
        }
        if let Some(s) = header.replication_sequence {
            h.int(33, s as u64);
        }
        if let Some(u) = &header.replication_base_url {
            h.bytes(34, u.as_bytes());
        }
        let mut out = vec![];
        let mut put = |kind: &str, content: &[u8]| {
            let mut z = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::fast());
            z.write_all(content).unwrap();
            let mut blob = Writer::default();
            blob.int(2, content.len() as u64);
            blob.bytes(3, &z.finish().unwrap());
            let mut head = Writer::default();
            head.bytes(1, kind.as_bytes());
            head.int(3, blob.0.len() as u64);
            out.extend_from_slice(&(head.0.len() as u32).to_be_bytes());
            out.extend_from_slice(&head.0);
            out.extend_from_slice(&blob.0);
        };
        put("OSMHeader", &h.0);
        for b in blocks {
            put("OSMData", b);
        }
        out
    }

    /// A block whose string table or elements pass their caps is refused, before it is held.
    #[test]
    fn a_block_past_its_caps_is_refused() {
        let mut table = Writer::default();
        for _ in 0..=MAX_STRINGS {
            table.bytes(1, b"");
        }
        let mut b = Writer::default();
        b.bytes(1, &table.0);
        let failed = block(&b.0).unwrap_err().to_string();
        assert!(failed.contains("string table over its cap"), "{failed}");
        let mut group = Writer::default();
        for id in 1..=MAX_ELEMENTS as u64 + 1 {
            let mut way = Writer::default();
            way.int(1, id);
            group.bytes(3, &way.0);
        }
        let mut b = Writer::default();
        b.bytes(1, &Writer::default().0);
        b.bytes(2, &group.0);
        let failed = block(&b.0).unwrap_err().to_string();
        assert!(failed.contains("over its cap of elements"), "{failed}");
        // Dense nodes past the block's room: refused as their IDs are read.
        let mut d = Writer::default();
        d.packed(1, &vec![2u64; MAX_ELEMENTS + 1]);
        let mut group = Writer::default();
        group.bytes(2, &d.0);
        let mut b = Writer::default();
        b.bytes(1, &Writer::default().0);
        b.bytes(2, &group.0);
        let failed = block(&b.0).unwrap_err().to_string();
        assert!(failed.contains("packed field over its cap"), "{failed}");
        // The same field repeated, each within the room alone, past it together.
        let mut d = Writer::default();
        d.packed(1, &vec![2u64; MAX_ELEMENTS / 2 + 1]);
        d.packed(1, &vec![2u64; MAX_ELEMENTS / 2 + 1]);
        let mut group = Writer::default();
        group.bytes(2, &d.0);
        let mut b = Writer::default();
        b.bytes(1, &Writer::default().0);
        b.bytes(2, &group.0);
        let failed = block(&b.0).unwrap_err().to_string();
        assert!(failed.contains("packed field over its cap"), "{failed}");
        // Ways whose references pass the block's cap in all, each within its own.
        let mut group = Writer::default();
        let refs = vec![2u64; 50_000];
        for id in 1..=(MAX_BLOCK_REFS / 50_000) as u64 + 1 {
            let mut way = Writer::default();
            way.int(1, id);
            way.packed(8, &refs);
            group.bytes(3, &way.0);
        }
        let mut b = Writer::default();
        b.bytes(1, &Writer::default().0);
        b.bytes(2, &group.0);
        let failed = block(&b.0).unwrap_err().to_string();
        assert!(failed.contains("over its cap of references"), "{failed}");
        // Ways whose tags pass the block's cap in all, each within its own.
        let mut group = Writer::default();
        let tags = vec![0u64; 60_000];
        for id in 1..=(MAX_BLOCK_TAGS / 120_000) as u64 + 1 {
            let mut way = Writer::default();
            way.int(1, id);
            way.packed(2, &tags);
            way.packed(3, &tags);
            group.bytes(3, &way.0);
        }
        let mut b = Writer::default();
        b.bytes(1, &Writer::default().0);
        b.bytes(2, &group.0);
        let failed = block(&b.0).unwrap_err().to_string();
        assert!(failed.contains("over its cap of tags"), "{failed}");
    }

    /// The feeder reads at most its bound of blocks ahead of the slowest one passed on.
    #[test]
    fn reading_ahead_stays_within_its_bound() {
        let header = Header {
            required_features: vec!["OsmSchema-V0.6".into(), "DenseNodes".into()],
            ..Default::default()
        };
        let blocks: Vec<Vec<u8>> = (0..64)
            .map(|i| data_block(&[Element::Node(i + 1, 47.0, 9.0, &[("amenity", "atm")])]))
            .collect();
        let dir = tempfile_dir();
        let path = dir.join("many.osm.pbf");
        std::fs::write(&path, file(&header, &blocks)).unwrap();
        let mut n = 0;
        read_blocks(&path, 0, 0, 2, |_| {
            // A slow consumer: the feeder would run ahead without its bound.
            std::thread::sleep(std::time::Duration::from_millis(2));
            n += 1;
            Ok(true)
        })
        .unwrap();
        assert_eq!(n, 64);
        let most = MOST_AHEAD.load(std::sync::atomic::Ordering::Relaxed);
        assert!(most > i64::MIN && most <= 0, "{most}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A file of `n` blocks, each one node (block `i` holds node `i + 1`).
    fn many_blocks(name: &str, n: i64) -> (std::path::PathBuf, std::path::PathBuf) {
        let header = Header {
            required_features: vec!["OsmSchema-V0.6".into(), "DenseNodes".into()],
            ..Default::default()
        };
        let blocks: Vec<Vec<u8>> = (0..n)
            .map(|i| data_block(&[Element::Node(i + 1, 47.0, 9.0, &[("amenity", "atm")])]))
            .collect();
        let dir = tempfile_dir();
        let path = dir.join(name);
        std::fs::write(&path, file(&header, &blocks)).unwrap();
        (dir, path)
    }

    /// `read` on a thread of its own, its answer within 30 seconds or the test fails (a hang).
    fn within_time<T: Send + 'static>(read: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read());
        });
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .expect("the read hung or panicked")
    }

    /// A panic in the code a block is passed to ends the read with a `panic` failure: the
    /// feeder, waiting for its turn many blocks ahead, is stopped and the read returns.
    #[test]
    fn a_panic_passing_a_block_on_fails_the_read_without_hanging() {
        let (dir, path) = many_blocks("panic-each.osm.pbf", 64);
        let failed = within_time(move || {
            read_blocks(&path, 0, 0, 2, |d| {
                assert!(d.index < 1, "a reader's bug");
                Ok(true)
            })
            .map(|_| ())
        })
        .unwrap_err();
        assert_eq!(crate::catalogue::failure::shown(&failed), "panic");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A panic decoding one block on one thread ends the read with a `panic` failure, while the
    /// other threads and the feeder would wait for that block for ever.
    #[test]
    fn a_panic_decoding_a_block_fails_the_read_without_hanging() {
        fn decode_but_node_3(blob: &[u8]) -> Result<Block> {
            let b = decode(blob)?;
            assert!(b.nodes.iter().all(|n| n.id != 3), "a decoder's bug");
            Ok(b)
        }
        let (dir, path) = many_blocks("panic-decode.osm.pbf", 64);
        let (failed, passed) = within_time(move || {
            let mut passed = vec![];
            let read = read_blocks_with(&path, 0, 0, 2, decode_but_node_3, |d| {
                passed.push(d.index);
                Ok(true)
            });
            (read.map(|_| ()), passed)
        });
        let failed = failed.unwrap_err();
        assert_eq!(crate::catalogue::failure::shown(&failed), "panic");
        // Only blocks before the one that failed were passed on, in order.
        assert!(passed.len() <= 2 && passed.iter().copied().eq(0..passed.len() as u64));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_written_file_reads_back() {
        let header = Header {
            required_features: vec!["OsmSchema-V0.6".into(), "DenseNodes".into()],
            optional_features: vec!["Sort.Type_then_ID".into()],
            replication_timestamp: Some(1_764_000_000),
            replication_sequence: Some(4979),
            replication_base_url: Some(
                "https://download.geofabrik.de/europe/liechtenstein/updates".into(),
            ),
        };
        let blocks = vec![
            data_block(&[
                Element::Node(
                    1,
                    47.1,
                    9.5,
                    &[("amenity", "cafe"), ("name", "Café Zürich")],
                ),
                Element::Node(2, -33.5, -70.25, &[]),
                Element::Node(5, 0.0, 0.0, &[]),
            ]),
            data_block(&[Element::Way(10, &[1, 2, 5, 1], &[("name", "Loop")])]),
            data_block(&[Element::Relation(
                20,
                &[(Kind::Way, 10, "outer"), (Kind::Node, 1, "label")],
                &[("type", "multipolygon")],
            )]),
        ];
        let dir = tempfile_dir();
        let path = dir.join("t.osm.pbf");
        std::fs::write(&path, file(&header, &blocks)).unwrap();
        let mut seen = vec![];
        let got = read_blocks(&path, 0, 0, 3, |d| {
            seen.push(d);
            Ok(true)
        })
        .unwrap();
        assert_eq!(got, Some(header));
        assert_eq!(seen.iter().map(|d| d.index).collect::<Vec<_>>(), [0, 1, 2]);
        let b = &seen[0].block;
        assert_eq!(b.nodes.len(), 3);
        assert_eq!((b.nodes[0].lat, b.nodes[0].lon), (471_000_000, 95_000_000));
        assert_eq!(
            (b.nodes[1].lat, b.nodes[1].lon),
            (-335_000_000, -702_500_000)
        );
        assert_eq!(
            b.tags(&b.nodes[0].tags).collect::<Vec<_>>(),
            [("amenity", "cafe"), ("name", "Café Zürich")]
        );
        assert!(b.nodes[1].tags.is_empty());
        assert_eq!(seen[1].block.ways[0].refs, [1, 2, 5, 1]);
        let r = &seen[2].block.relations[0];
        assert_eq!(r.members.len(), 2);
        assert_eq!(seen[2].block.string(r.members[1].role), "label");
        // Reading again from a block's own offset gives that block and the rest.
        let mut again = vec![];
        read_blocks(&path, seen[1].offset, 1, 2, |d| {
            again.push(d.index);
            Ok(true)
        })
        .unwrap();
        assert_eq!(again, [1, 2]);
        // Stopping early stops.
        let mut first = 0;
        read_blocks(&path, 0, 0, 2, |_| {
            first += 1;
            Ok(false)
        })
        .unwrap();
        assert_eq!(first, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_header_requiring_history_is_refused() {
        let mut h = Writer::default();
        h.bytes(4, b"OsmSchema-V0.6");
        h.bytes(4, b"HistoricalInformation");
        assert!(header(&h.0).is_err());
    }

    pub fn tempfile_dir() -> std::path::PathBuf {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "pinakes-osm-{}-{}",
            std::process::id(),
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    proptest::proptest! {
        /// Arbitrary bytes as a block, a header, a blob or a file stream: an error or a value,
        /// never a panic.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(proptest::num::u8::ANY, 0..2048)) {
            let _ = block(&data);
            let _ = header(&data);
            let _ = inflate(&data);
            let mut reader = Reader::new(&data[..], 0);
            while let Ok(Some(_)) = reader.next_blob() {}
        }
    }

    /// Random and damaged inputs: decoding never panics and never allocates past the caps (a
    /// stated size over a cap is an error before anything is allocated for it).
    #[test]
    fn damaged_blocks_never_panic() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let good = data_block(&[
            Element::Node(1, 47.1, 9.5, &[("amenity", "cafe"), ("name", "x")]),
            Element::Way(10, &[1, 2], &[("name", "y")]),
            Element::Relation(20, &[(Kind::Way, 10, "outer")], &[("name", "z")]),
        ]);
        for round in 0..20_000 {
            let mut data = if round % 3 == 0 {
                (0..(next() % 300))
                    .map(|_| next() as u8)
                    .collect::<Vec<u8>>()
            } else {
                good.clone()
            };
            for _ in 0..(next() % 8) {
                if data.is_empty() {
                    break;
                }
                let at = (next() as usize) % data.len();
                match next() % 3 {
                    0 => data[at] = next() as u8,
                    1 => {
                        data.truncate(at);
                    }
                    _ => data.insert(at, 0xff),
                }
            }
            let _ = block(&data);
            let _ = header(&data);
            let _ = inflate(&data);
            let mut reader = Reader::new(&data[..], 0);
            while let Ok(Some(_)) = reader.next_blob() {}
        }
        // A blob header that states a huge size is refused before reading it.
        let mut head = Writer::default();
        head.bytes(1, b"OSMData");
        head.int(3, 1 << 40);
        let mut stream = (head.0.len() as u32).to_be_bytes().to_vec();
        stream.extend_from_slice(&head.0);
        assert!(Reader::new(&stream[..], 0).next_blob().is_err());
        // A blob that inflates past its stated size is refused.
        let mut z = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::fast());
        std::io::Write::write_all(&mut z, &vec![0u8; 4096]).unwrap();
        let mut blob = Writer::default();
        blob.int(2, 10);
        blob.bytes(3, &z.finish().unwrap());
        assert!(inflate(&blob.0).is_err());
    }
}
