//! Where every node is, on disk, for the import's way and relation geometry: node locations are
//! written once as the file's nodes stream by (in id order) and read back by id.
//!
//! Two layouts, by how the ids fill their range:
//! - [`Layout::Dense`], for the planet: 8 bytes per id at `id × 8` (a sparse file; about 100 GB
//!   at the planet's ids), one read per lookup.
//! - [`Layout::Sparse`], for a region: 16 bytes per node (id, location) in id order, with every
//!   [`STRIDE`]th id kept in memory, so a lookup reads one stride from the file.
//!
//! A location is stored as two `u32`: latitude and longitude in 1e-7 degrees, shifted so that 0
//! means no node.
use anyhow::{ensure, Context, Result};
use std::{
    fs::File,
    io::{BufWriter, Write},
    os::unix::fs::FileExt,
    path::Path,
};

/// Nodes per in-memory entry of a sparse index.
pub const STRIDE: usize = 256;
const LAT_SHIFT: i64 = 900_000_001;
const LON_SHIFT: i64 = 1_800_000_001;
/// The largest node id a dense index takes (OSM's ids are near 14 billion in 2026).
pub const DENSE_MAX_ID: i64 = 64_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Dense,
    Sparse,
}
fn pack(lat: i32, lon: i32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&((i64::from(lat) + LAT_SHIFT) as u32).to_le_bytes());
    b[4..].copy_from_slice(&((i64::from(lon) + LON_SHIFT) as u32).to_le_bytes());
    b
}
fn unpack(b: &[u8]) -> Option<(i32, i32)> {
    let lat = u32::from_le_bytes(b[..4].try_into().ok()?);
    let lon = u32::from_le_bytes(b[4..8].try_into().ok()?);
    (lat != 0 && lon != 0).then(|| {
        (
            (i64::from(lat) - LAT_SHIFT) as i32,
            (i64::from(lon) - LON_SHIFT) as i32,
        )
    })
}

/// Writes an index, nodes in increasing id order.
pub struct Writer {
    layout: Layout,
    file: File,
    /// Dense: the run of consecutive ids being gathered, from `run_start`.
    run: Vec<u8>,
    run_start: i64,
    /// Sparse: the buffered writer's bytes so far (the file's length).
    sparse: Option<BufWriter<File>>,
    written: u64,
    /// Bytes written since [`Writer::take_flushed`] last read them (padding included).
    flushed: u64,
    last: i64,
    /// Nodes left out because their ID did not follow the last one (a file not sorted by ID).
    pub out_of_order: u64,
}
/// Dense: a gap of at most this many IDs is written as empty entries, so a run of nodes is one
/// write.
const PAD_IDS: i64 = 65_536;
impl Writer {
    /// A new index at `path`, or the one there continued from `length` bytes (a resumed import).
    pub fn open(path: &Path, layout: Layout, length: u64) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("{}", path.display()))?;
        file.set_len(length)?;
        let mut last = 0;
        let sparse = match layout {
            // Every ID below the written length was taken (or passed over).
            Layout::Dense => {
                last = (length / 8) as i64 - 1;
                None
            }
            Layout::Sparse => {
                ensure!(length.is_multiple_of(16), "node index: torn length");
                if length > 0 {
                    let mut b = [0u8; 8];
                    file.read_exact_at(&mut b, length - 16)?;
                    last = i64::from_le_bytes(b);
                }
                let mut f = file.try_clone()?;
                std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(length))?;
                Some(BufWriter::with_capacity(1 << 20, f))
            }
        };
        Ok(Self {
            layout,
            file,
            run: Vec::new(),
            run_start: 0,
            sparse,
            written: length,
            flushed: 0,
            last,
            out_of_order: 0,
        })
    }
    pub fn add(&mut self, id: i64, lat: i32, lon: i32) -> Result<()> {
        // Out-of-order or repeated ids (a file not sorted by id) are left out.
        if id <= self.last {
            self.out_of_order += 1;
            return Ok(());
        }
        self.last = id;
        match self.layout {
            Layout::Dense => {
                ensure!(
                    id <= DENSE_MAX_ID,
                    "node index: node id over the dense range"
                );
                let next = self.run_start + (self.run.len() / 8) as i64;
                if !self.run.is_empty()
                    && id > next
                    && id - next <= PAD_IDS
                    && self.run.len() < 1 << 22
                {
                    // Empty entries (zero) mean "no node".
                    self.run
                        .resize(self.run.len() + (id - next) as usize * 8, 0);
                } else if self.run.is_empty() || id != next || self.run.len() >= 1 << 22 {
                    self.flush_run()?;
                    self.run_start = id;
                }
                self.run.extend_from_slice(&pack(lat, lon));
            }
            Layout::Sparse => {
                let w = self.sparse.as_mut().expect("a sparse writer");
                w.write_all(&id.to_le_bytes())?;
                w.write_all(&pack(lat, lon))?;
                self.written += 16;
            }
        }
        Ok(())
    }
    fn flush_run(&mut self) -> Result<()> {
        if !self.run.is_empty() {
            let at = self.run_start as u64 * 8;
            self.file.write_all_at(&self.run, at)?;
            self.written = self.written.max(at + self.run.len() as u64);
            self.flushed += self.run.len() as u64;
            self.run.clear();
        }
        Ok(())
    }
    /// The bytes written to the file since the last call (a dense index's empty entries count):
    /// a caller checks its room after each gigabyte or so, however few nodes wrote it.
    pub fn take_flushed(&mut self) -> u64 {
        std::mem::take(&mut self.flushed)
    }
    /// Write out what is buffered and sync it: the length returned is where a resumed import
    /// continues the index.
    pub fn sync(&mut self) -> Result<u64> {
        self.flush_run()?;
        if let Some(w) = self.sparse.as_mut() {
            w.flush()?;
        }
        self.file.sync_data()?;
        Ok(self.written)
    }
}

/// Reads an index by node id.
pub struct Reader {
    layout: Layout,
    file: File,
    len: u64,
    /// Sparse: the id of every [`STRIDE`]th node.
    marks: Vec<i64>,
    /// File reads made by lookups.
    reads: std::cell::Cell<u64>,
}
impl Reader {
    /// The file reads lookups have made.
    pub fn reads(&self) -> u64 {
        self.reads.get()
    }
    pub fn open(path: &Path, layout: Layout) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("{}", path.display()))?;
        let len = file.metadata()?.len();
        let mut marks = vec![];
        if layout == Layout::Sparse {
            ensure!(len.is_multiple_of(16), "node index: torn length");
            let nodes = len / 16;
            let mut b = [0u8; 8];
            let mut i = 0u64;
            while i < nodes {
                file.read_exact_at(&mut b, i * 16)?;
                marks.push(i64::from_le_bytes(b));
                i += STRIDE as u64;
            }
        }
        Ok(Self {
            layout,
            file,
            len,
            marks,
            reads: std::cell::Cell::new(0),
        })
    }
    /// The node's location in 1e-7 degrees, when the index holds it.
    pub fn get(&self, id: i64) -> Result<Option<(i32, i32)>> {
        if id <= 0 {
            return Ok(None);
        }
        match self.layout {
            Layout::Dense => {
                let Some(at) = (id as u64)
                    .checked_mul(8)
                    .filter(|at| at.saturating_add(8) <= self.len)
                else {
                    return Ok(None);
                };
                let mut b = [0u8; 8];
                self.reads.set(self.reads.get() + 1);
                self.file.read_exact_at(&mut b, at)?;
                Ok(unpack(&b))
            }
            Layout::Sparse => {
                let stride = match self.marks.binary_search(&id) {
                    Ok(i) => i,
                    Err(0) => return Ok(None),
                    Err(i) => i - 1,
                };
                let start = stride as u64 * STRIDE as u64 * 16;
                let end = (start + STRIDE as u64 * 16).min(self.len);
                let mut b = [0u8; STRIDE * 16];
                let b = &mut b[..(end - start) as usize];
                self.reads.set(self.reads.get() + 1);
                self.file.read_exact_at(b, start)?;
                let records = b.as_chunks::<16>().0;
                let found = records.binary_search_by_key(&id, |r| {
                    i64::from_le_bytes(r[..8].try_into().expect("8 bytes"))
                });
                Ok(found.ok().and_then(|i| unpack(&records[i][8..])))
            }
        }
    }
}

/// Fails when the index at `path` takes more disk than `cap` bytes (what a file of its size can
/// need), or when under 1 GB is left beside it: a file whose IDs are spread far apart must not
/// fill the disk.
pub fn check_room(path: &Path, cap: u64, free: &crate::catalogue::run::FreeDisk) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let taken = std::fs::metadata(path)?.blocks() * 512;
    let dir = path.parent().unwrap_or(Path::new("."));
    let free = free(dir)?;
    ensure!(
        taken <= cap && free >= 1_000_000_000,
        crate::catalogue::failure::Failure::new("disk").at(path.display().to_string())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_layouts_find_what_was_written_and_nothing_else() {
        let dir = super::super::pbf::tests::tempfile_dir();
        for layout in [Layout::Dense, Layout::Sparse] {
            let path = dir.join(format!("{layout:?}"));
            let mut ids: Vec<i64> = vec![];
            let mut seed = 3u64;
            let mut id = 0i64;
            for _ in 0..5000 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                id += 1 + (seed % 50) as i64;
                ids.push(id);
            }
            let loc = |id: i64| {
                (
                    ((id * 7919) % 1_800_000_000 - 900_000_000) as i32,
                    ((id * 104_729) % 3_600_000_000 - 1_800_000_000) as i32,
                )
            };
            // Written in two sittings, as a resumed import writes it.
            let mut w = Writer::open(&path, layout, 0).unwrap();
            for id in &ids[..2500] {
                w.add(*id, loc(*id).0, loc(*id).1).unwrap();
            }
            let length = w.sync().unwrap();
            drop(w);
            let mut w = Writer::open(&path, layout, length).unwrap();
            // A node written again or out of order is left out.
            w.add(ids[10], 0, 0).unwrap();
            for id in &ids[2500..] {
                w.add(*id, loc(*id).0, loc(*id).1).unwrap();
            }
            w.add(0, 0, 0).unwrap();
            w.sync().unwrap();
            let r = Reader::open(&path, layout).unwrap();
            for id in &ids {
                assert_eq!(r.get(*id).unwrap(), Some(loc(*id)), "{layout:?} {id}");
            }
            for id in [0, -1, ids[0] - 1, ids[100] + 1, id + 1, id + 10_000] {
                if !ids.contains(&id) {
                    assert_eq!(r.get(id).unwrap(), None, "{layout:?} {id}");
                }
            }
            // The corners of the globe are locations, not "no node".
            let path = dir.join(format!("{layout:?}-corners"));
            let mut w = Writer::open(&path, layout, 0).unwrap();
            w.add(1, -900_000_000, -1_800_000_000).unwrap();
            w.add(2, 900_000_000, 1_800_000_000).unwrap();
            w.add(3, 0, 0).unwrap();
            w.sync().unwrap();
            let r = Reader::open(&path, layout).unwrap();
            assert_eq!(r.get(1).unwrap(), Some((-900_000_000, -1_800_000_000)));
            assert_eq!(r.get(2).unwrap(), Some((900_000_000, 1_800_000_000)));
            assert_eq!(r.get(3).unwrap(), Some((0, 0)));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A dense index counts what it writes, the empty entries padding its gaps included, so its
    /// room is checked by bytes written, not nodes.
    #[test]
    fn a_dense_index_counts_its_padding() {
        let dir = super::super::pbf::tests::tempfile_dir();
        let mut w = Writer::open(&dir.join("nodes.idx"), Layout::Dense, 0).unwrap();
        for i in 1..=64i64 {
            w.add(i * 1000, 1, 1).unwrap();
        }
        w.sync().unwrap();
        assert_eq!(w.take_flushed(), (64 * 1000 - 1000 + 1) * 8);
        assert_eq!(w.take_flushed(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A dense index whose IDs lie far apart takes disk past its cap: `disk`.
    #[test]
    fn an_index_past_its_cap_stops() {
        let dir = super::super::pbf::tests::tempfile_dir();
        let path = dir.join("nodes.idx");
        let mut w = Writer::open(&path, Layout::Dense, 0).unwrap();
        for i in 1..=64i64 {
            w.add(i * 1_000_000, 1, 1).unwrap();
        }
        w.sync().unwrap();
        let plenty: crate::catalogue::run::FreeDisk = std::sync::Arc::new(|_| Ok(u64::MAX));
        check_room(&path, 1 << 30, &plenty).unwrap();
        let failed = check_room(&path, 64 * 1024, &plenty).unwrap_err();
        assert!(
            crate::catalogue::failure::shown(&failed).starts_with("disk"),
            "{failed:#}"
        );
        // Room for the index, under 1 GB left beside it: `disk` too.
        let short: crate::catalogue::run::FreeDisk = std::sync::Arc::new(|_| Ok(500_000_000));
        let failed = check_room(&path, 1 << 30, &short).unwrap_err();
        assert!(
            crate::catalogue::failure::shown(&failed).starts_with("disk"),
            "{failed:#}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
