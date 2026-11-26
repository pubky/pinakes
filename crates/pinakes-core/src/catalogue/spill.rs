//! An external sort: records with a text key are gathered in memory up to a budget, sorted by key
//! (bytewise) and written as runs (lz4 frames) into a directory; [`Spill::merge`] then gives
//! every record in key order, those of one key together. An importer uses it to bring together
//! what a dump says about one item in several files (a recording on many releases), within a
//! fixed memory, and to compare the result with the stored cards in the same order.
//!
//! Record order within one key is not kept: a reader of a key's records must not depend on it.
use super::failure::Failure;
use anyhow::{bail, Context as _, Result};
use std::{
    collections::BinaryHeap,
    fs::File,
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

/// The longest key (bytes).
pub const KEY_CAP: usize = 1024;
/// The largest record (bytes).
pub const RECORD_CAP: usize = 16 * 1024 * 1024;
/// The most runs merged at once; more are merged in passes first.
const FAN_IN: usize = 128;

/// A record in memory: key and payload.
type Pair = (Box<[u8]>, Box<[u8]>);
/// A key with all its records' payloads.
pub type Group = (Vec<u8>, Vec<Vec<u8>>);

/// One sorter writing its runs into `dir` (which it owns: [`Spill::new`] makes it).
pub struct Spill {
    dir: PathBuf,
    /// Records in memory: key and payload.
    buffer: Vec<Pair>,
    buffered: usize,
    budget: usize,
    runs: Vec<PathBuf>,
    /// Bytes written to runs, and the most they may take.
    written: u64,
    max_bytes: u64,
    /// Free bytes the file system must keep: below them a run is not written (`disk`).
    floor: u64,
    free_disk: fn(&Path) -> io::Result<u64>,
    next_run: usize,
}
impl Spill {
    /// A sorter holding at most `budget` bytes of records in memory and writing at most
    /// `max_bytes` of runs into `dir`, which must not exist yet or be empty.
    pub fn new(dir: &Path, budget: usize, max_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| Failure::new("disk").at(dir.display().to_string()))?;
        Ok(Self {
            dir: dir.to_owned(),
            buffer: vec![],
            buffered: 0,
            budget: budget.max(1),
            runs: vec![],
            written: 0,
            max_bytes,
            floor: 0,
            free_disk: super::run::free_bytes,
            next_run: 0,
        })
    }
    /// Keep `floor` bytes free on the file system: a run that would go below is a `disk` failure.
    pub fn keep_free(mut self, floor: u64) -> Self {
        self.floor = floor;
        self
    }
    /// The runs written so far and their bytes.
    pub fn written(&self) -> (usize, u64) {
        (self.runs.len(), self.written)
    }
    /// Add one record.
    pub fn push(&mut self, key: &[u8], payload: &[u8]) -> Result<()> {
        if key.len() > KEY_CAP || payload.len() > RECORD_CAP {
            bail!(Failure::new("record_size"));
        }
        self.buffered += key.len() + payload.len() + 48;
        self.buffer.push((key.into(), payload.into()));
        if self.buffered >= self.budget {
            self.flush()?;
        }
        Ok(())
    }
    /// Write what is in memory as a run.
    pub fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let path = self.dir.join(format!("run-{:06}.lz4", self.next_run));
        self.next_run += 1;
        let records = std::mem::take(&mut self.buffer);
        self.buffered = 0;
        let bytes = self.write_run(&path, records.iter().map(|(k, p)| (&k[..], &p[..])))?;
        self.runs.push(path);
        self.written += bytes;
        Ok(())
    }
    fn check_room(&self, written: u64) -> Result<()> {
        let disk = || Failure::new("disk").at(self.dir.display().to_string());
        if self.written + written > self.max_bytes {
            bail!(disk());
        }
        let free = (self.free_disk)(&self.dir).with_context(disk)?;
        if free < self.floor {
            bail!(disk());
        }
        Ok(())
    }
    fn write_run<'a>(
        &self,
        path: &Path,
        records: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    ) -> Result<u64> {
        let disk = || Failure::new("disk").at(path.display().to_string());
        let file = File::create(path).with_context(disk)?;
        let mut out = lz4_flex::frame::FrameEncoder::new(BufWriter::with_capacity(1 << 16, file));
        let mut since_check = 0u64;
        for (key, payload) in records {
            write_record(&mut out, key, payload).with_context(disk)?;
            since_check += (key.len() + payload.len()) as u64;
            if since_check > 64 << 20 {
                self.check_room(since_check)?;
                since_check = 0;
            }
        }
        let file = out
            .finish()
            .map_err(io::Error::other)
            .with_context(disk)?
            .into_inner()
            .map_err(|e| e.into_error())
            .with_context(disk)?;
        file.sync_all().with_context(disk)?;
        let bytes = file.metadata().with_context(disk)?.len();
        self.check_room(bytes)?;
        Ok(bytes)
    }
    /// Write what is left in memory and give the runs (to merge with other sorters' runs).
    pub fn finish(mut self) -> Result<Vec<PathBuf>> {
        self.flush()?;
        Ok(self.runs)
    }
    /// Every record in key order ([`merge`]).
    pub fn merge(self) -> Result<Merge> {
        let dir = self.dir.clone();
        merge(&dir, self.finish()?)
    }
}
/// Every record of these runs in key order, those of one key together: merged in passes of at
/// most [`FAN_IN`] runs first (each pass's run written into `dir`, its inputs removed).
pub fn merge(dir: &Path, mut runs: Vec<PathBuf>) -> Result<Merge> {
    let mut pass = 0;
    while runs.len() > FAN_IN {
        let mut next = vec![];
        for (n, group) in std::mem::take(&mut runs).chunks(FAN_IN).enumerate() {
            if group.len() == 1 {
                next.push(group[0].clone());
                continue;
            }
            let path = dir.join(format!("pass-{pass:03}-{n:06}.lz4"));
            let disk = || Failure::new("disk").at(path.display().to_string());
            let mut merged = Merge::open(group)?;
            let file = File::create(&path).with_context(disk)?;
            let mut out =
                lz4_flex::frame::FrameEncoder::new(BufWriter::with_capacity(1 << 16, file));
            while let Some((key, payload)) = merged.next_record()? {
                write_record(&mut out, &key, &payload).with_context(disk)?;
            }
            out.finish()
                .map_err(io::Error::other)
                .with_context(disk)?
                .into_inner()
                .map_err(|e| e.into_error())
                .with_context(disk)?;
            for old in group {
                let _ = std::fs::remove_file(old);
            }
            next.push(path);
        }
        runs = next;
        pass += 1;
    }
    Merge::open(&runs)
}
fn write_record(out: &mut impl Write, key: &[u8], payload: &[u8]) -> io::Result<()> {
    out.write_all(&(key.len() as u32).to_le_bytes())?;
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(key)?;
    out.write_all(payload)
}
/// One record of a run, `None` at its end; a run cut or longer than the caps says is an error.
fn read_record(input: &mut impl Read) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut lens = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        let n = input.read(&mut lens[got..])?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            bail!(Failure::new("spill"));
        }
        got += n;
    }
    let key_len = u32::from_le_bytes(lens[..4].try_into().expect("4 bytes")) as usize;
    let payload_len = u32::from_le_bytes(lens[4..].try_into().expect("4 bytes")) as usize;
    if key_len > KEY_CAP || payload_len > RECORD_CAP {
        bail!(Failure::new("spill"));
    }
    let mut key = vec![0; key_len];
    input.read_exact(&mut key).context(Failure::new("spill"))?;
    let mut payload = vec![0; payload_len];
    input
        .read_exact(&mut payload)
        .context(Failure::new("spill"))?;
    Ok(Some((key, payload)))
}

type RunReader = lz4_flex::frame::FrameDecoder<BufReader<File>>;
/// The merge of sorted runs ([`Spill::merge`]).
pub struct Merge {
    runs: Vec<RunReader>,
    heap: BinaryHeap<Head>,
}
struct Head {
    key: Vec<u8>,
    payload: Vec<u8>,
    run: usize,
}
impl PartialEq for Head {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.run == other.run
    }
}
impl Eq for Head {}
impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Head {
    // The smallest key first (a max-heap reversed), then the earlier run.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.run.cmp(&self.run))
    }
}
impl Merge {
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let mut runs = vec![];
        let mut heap = BinaryHeap::new();
        for (i, path) in paths.iter().enumerate() {
            let file = File::open(path)
                .with_context(|| Failure::new("spill").at(path.display().to_string()))?;
            let mut reader =
                lz4_flex::frame::FrameDecoder::new(BufReader::with_capacity(1 << 16, file));
            if let Some((key, payload)) = read_record(&mut reader)? {
                heap.push(Head {
                    key,
                    payload,
                    run: i,
                });
            }
            runs.push(reader);
        }
        Ok(Self { runs, heap })
    }
    /// The next record in key order.
    pub fn next_record(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(head) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some((key, payload)) = read_record(&mut self.runs[head.run])? {
            if key < head.key {
                bail!(Failure::new("spill"));
            }
            self.heap.push(Head {
                key,
                payload,
                run: head.run,
            });
        }
        Ok(Some((head.key, head.payload)))
    }
    /// The next key with all its records.
    pub fn next_group(&mut self) -> Result<Option<Group>> {
        let Some((key, payload)) = self.next_record()? else {
            return Ok(None);
        };
        let mut payloads = vec![payload];
        while self.heap.peek().is_some_and(|h| h.key == key) {
            let (_, payload) = self.next_record()?.expect("a record was peeked");
            payloads.push(payload);
        }
        Ok(Some((key, payloads)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys come out sorted bytewise, each with all its records, over many runs and merge passes.
    #[test]
    fn records_come_out_sorted_and_grouped_over_many_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut spill = Spill::new(&dir.path().join("s"), 200, 1 << 30).unwrap();
        let mut want = std::collections::BTreeMap::<Vec<u8>, Vec<Vec<u8>>>::new();
        let mut seed = 7u64;
        for n in 0..20_000u64 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let key = format!("k{:05}", (seed >> 33) % 3000).into_bytes();
            let payload = n.to_le_bytes().to_vec();
            spill.push(&key, &payload).unwrap();
            want.entry(key).or_default().push(payload);
        }
        assert!(spill.written().0 > FAN_IN, "{:?}", spill.written());
        let mut merge = spill.merge().unwrap();
        let mut got = std::collections::BTreeMap::new();
        let mut last: Option<Vec<u8>> = None;
        while let Some((key, mut payloads)) = merge.next_group().unwrap() {
            assert!(last.as_ref().is_none_or(|l| *l < key));
            last = Some(key.clone());
            payloads.sort();
            got.insert(key, payloads);
        }
        for v in want.values_mut() {
            v.sort();
        }
        assert_eq!(got, want);
    }

    #[test]
    fn a_spill_over_its_bytes_is_a_disk_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut spill = Spill::new(&dir.path().join("s"), 10, 100).unwrap();
        let failed = (0..100)
            .map(|n| spill.push(format!("{n}").as_bytes(), &[1; 64]))
            .find_map(Result::err)
            .expect("the cap is reached");
        assert_eq!(
            failed.downcast_ref::<Failure>().map(|f| f.class),
            Some("disk")
        );
        assert!(spill.push(&[b'k'; KEY_CAP + 1], b"").is_err());
    }

    #[test]
    fn a_damaged_run_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut spill = Spill::new(&dir.path().join("s"), 1 << 20, 1 << 30).unwrap();
        for n in 0..100u32 {
            spill.push(format!("{n:04}").as_bytes(), &[7; 100]).unwrap();
        }
        let runs = spill.finish().unwrap();
        let bytes = std::fs::read(&runs[0]).unwrap();
        std::fs::write(&runs[0], &bytes[..bytes.len() / 2]).unwrap();
        // An error when the merge opens the run or while it reads it, never a panic.
        let read = merge(&dir.path().join("s"), runs).and_then(|mut merge| {
            while merge.next_group()?.is_some() {}
            Ok(())
        });
        assert!(read.is_err());
    }
}
