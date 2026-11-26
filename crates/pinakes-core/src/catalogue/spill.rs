//! An external sort: records with a text key are gathered in memory up to a budget, sorted by key
//! (bytewise) and written as runs (lz4 frames) into a directory; [`reduce`] merges runs in passes
//! until at most a fan-in of them is left, and [`Merge`] then gives every key in order with its
//! records. An importer uses it to bring together what a dump says about one item in several
//! files (a recording on many releases), within a fixed memory, and to compare the result with the
//! stored cards in the same order.
//!
//! Record order within one key is not kept, and [`Merge::next_group`] keeps a bounded part of a
//! key's records: which part depends only on the records, never on their order.
use super::failure::Failure;
use anyhow::{bail, Context as _, Result};
use std::{
    collections::{BTreeSet, BinaryHeap},
    fs::File,
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

/// The longest key (bytes).
pub const KEY_CAP: usize = 1024;
/// The largest record (bytes).
pub const RECORD_CAP: usize = 16 * 1024 * 1024;
/// The most runs merged at once ([`reduce`]).
pub const FAN_IN: usize = 256;
/// The distinct first bytes of payloads [`Group::counts`] counts apart (higher ones count in the
/// last).
pub const TAGS: usize = 16;

/// What the disk may take: at most `max_bytes` written in all, and `floor` bytes left free on the
/// file system.
#[derive(Clone, Copy)]
pub struct Room {
    pub max_bytes: u64,
    pub floor: u64,
    pub free_disk: fn(&Path) -> io::Result<u64>,
}
impl Room {
    pub fn new(max_bytes: u64, floor: u64) -> Self {
        Self {
            max_bytes,
            floor,
            free_disk: super::run::free_bytes,
        }
    }
    fn check(&self, dir: &Path, written: u64) -> Result<()> {
        let disk = || Failure::new("disk").at(dir.display().to_string());
        if written > self.max_bytes {
            bail!(disk());
        }
        let free = (self.free_disk)(dir).with_context(disk)?;
        if free < self.floor {
            bail!(disk());
        }
        Ok(())
    }
}

/// A record in memory: key and payload.
type Pair = (Box<[u8]>, Box<[u8]>);

/// One sorter writing its runs into `dir` (which it owns: [`Spill::new`] makes it).
pub struct Spill {
    dir: PathBuf,
    buffer: Vec<Pair>,
    buffered: usize,
    budget: usize,
    runs: Vec<PathBuf>,
    /// Bytes written to runs (as stored, compressed), and records written.
    written: u64,
    records: u64,
    room: Room,
    next_run: usize,
}
impl Spill {
    /// A sorter holding at most `budget` bytes of records in memory and writing runs into `dir`
    /// within `room`.
    pub fn new(dir: &Path, budget: usize, room: Room) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| Failure::new("disk").at(dir.display().to_string()))?;
        Ok(Self {
            dir: dir.to_owned(),
            buffer: vec![],
            buffered: 0,
            budget: budget.max(1),
            runs: vec![],
            written: 0,
            records: 0,
            room,
            next_run: 0,
        })
    }
    /// The runs written so far, their bytes and their records.
    pub fn written(&self) -> (usize, u64, u64) {
        (self.runs.len(), self.written, self.records)
    }
    /// Add one record.
    pub fn push(&mut self, key: &[u8], payload: &[u8]) -> Result<()> {
        if key.len() > KEY_CAP || payload.len() > RECORD_CAP {
            bail!(Failure::new("record_size"));
        }
        // What a pair takes in memory: its bytes, two boxes and the vector's slot.
        self.buffered += key.len() + payload.len() + 64;
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
        let records = self.buffer.len() as u64;
        let bytes = write_run(
            &path,
            self.buffer
                .drain(..)
                .map(|(k, p)| (k.into_vec(), p.into_vec())),
            &self.room,
            self.written,
        )?;
        self.buffered = 0;
        self.runs.push(path);
        self.written += bytes;
        self.records += records;
        Ok(())
    }
    /// Write what is left in memory and give the runs (to merge with other sorters' runs).
    pub fn finish(mut self) -> Result<Vec<PathBuf>> {
        self.flush()?;
        Ok(self.runs)
    }
}
/// Write `records` (in key order) as a run at `path`, checking `room` every 64 MiB with the bytes
/// `before` it counted against it. The run's size on disk.
fn write_run(
    path: &Path,
    records: impl Iterator<Item = (Vec<u8>, Vec<u8>)>,
    room: &Room,
    before: u64,
) -> Result<u64> {
    let disk = || Failure::new("disk").at(path.display().to_string());
    let dir = path.parent().unwrap_or(path);
    let file = File::create(path).with_context(disk)?;
    let mut out = lz4_flex::frame::FrameEncoder::new(BufWriter::with_capacity(1 << 16, file));
    let (mut raw, mut since_check) = (0u64, 0u64);
    for (key, payload) in records {
        write_record(&mut out, &key, &payload).with_context(disk)?;
        let n = (key.len() + payload.len() + 8) as u64;
        raw += n;
        since_check += n;
        if since_check > 64 << 20 {
            // A run takes less than its raw records: half of them bounds what it took so far.
            room.check(dir, before + raw / 2)?;
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
    room.check(dir, before + bytes)?;
    Ok(bytes)
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

/// Merge `runs` until at most `fan_in` are left: each pass merges the smallest runs, as few as
/// that takes (at most `fan_in`), into one written into `dir` within `room`, and then removes its
/// inputs. The runs left. `stop` is asked as a pass goes: when it says so, the pass's output is
/// removed, every input is left as it was, and the merge stops (`stopped`).
pub fn reduce(
    dir: &Path,
    mut runs: Vec<PathBuf>,
    fan_in: usize,
    room: &Room,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<PathBuf>> {
    let fan_in = fan_in.max(2);
    std::fs::create_dir_all(dir)
        .with_context(|| Failure::new("disk").at(dir.display().to_string()))?;
    let mut pass = 0;
    let mut written = 0u64;
    while runs.len() > fan_in {
        if stop() {
            bail!(Failure::new("stopped"));
        }
        let take = (runs.len() - fan_in + 1).min(fan_in);
        runs.sort_by_key(|r| std::fs::metadata(r).map_or(0, |m| m.len()));
        let group: Vec<PathBuf> = runs.drain(..take).collect();
        let path = dir.join(format!("pass-{pass:06}.lz4"));
        let mut merged = Merge::open(&group)?;
        let mut failed = None;
        let mut n = 0u64;
        let records = std::iter::from_fn(|| {
            n += 1;
            if n.is_multiple_of(65_536) && stop() {
                failed = Some(Failure::new("stopped").into());
                return None;
            }
            match merged.next_record() {
                Ok(next) => next,
                Err(e) => {
                    failed = Some(e);
                    None
                }
            }
        });
        let bytes = write_run(&path, records, room, written);
        if let Some(e) = failed {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        written += bytes?;
        if stop() {
            let _ = std::fs::remove_file(&path);
            bail!(Failure::new("stopped"));
        }
        for old in &group {
            let _ = std::fs::remove_file(old);
        }
        runs.push(path);
        pass += 1;
    }
    Ok(runs)
}

type RunReader = lz4_flex::frame::FrameDecoder<BufReader<File>>;
/// The merge of sorted runs.
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
/// A key with a bounded part of its records ([`Merge::next_group`]).
pub struct Group {
    pub key: Vec<u8>,
    /// The distinct payloads kept, in byte order.
    pub payloads: Vec<Vec<u8>>,
    /// Every record of the key, kept or not, by its payload's first byte (the last slot holds
    /// those of [`TAGS`] − 1 and over).
    pub counts: [u64; TAGS],
}
impl Merge {
    pub fn open(paths: &[PathBuf]) -> Result<Self> {
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
    /// The next key and its records counted by their first byte, none of them kept.
    pub fn next_counts(&mut self) -> Result<Option<(Vec<u8>, [u64; TAGS])>> {
        let Some((key, payload)) = self.next_record()? else {
            return Ok(None);
        };
        let mut counts = [0u64; TAGS];
        let mut count = |p: &[u8]| {
            counts[usize::from(p.first().copied().unwrap_or(0)).min(TAGS - 1)] += 1;
        };
        count(&payload);
        while self.heap.peek().is_some_and(|h| h.key == key) {
            let (_, payload) = self.next_record()?.expect("a record was peeked");
            count(&payload);
        }
        Ok(Some((key, counts)))
    }
    /// The next key, every one of its records counted, and of its distinct payloads at most
    /// `max_records` holding at most `max_bytes`: the smallest ones in byte order that fit, so
    /// the same records give the same group in any order.
    pub fn next_group(&mut self, max_records: usize, max_bytes: usize) -> Result<Option<Group>> {
        let Some((key, payload)) = self.next_record()? else {
            return Ok(None);
        };
        let mut counts = [0u64; TAGS];
        let mut kept: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut bytes = 0usize;
        // Every payload at or above it was left out: none of them is taken any more.
        let mut above: Option<Vec<u8>> = None;
        let mut take = |payload: Vec<u8>| {
            counts[usize::from(payload.first().copied().unwrap_or(0)).min(TAGS - 1)] += 1;
            if above.as_ref().is_some_and(|a| payload >= *a) || kept.contains(&payload) {
                return;
            }
            bytes += payload.len();
            kept.insert(payload);
            while kept.len() > max_records.max(1) || bytes > max_bytes {
                let Some(last) = kept.pop_last() else { break };
                bytes -= last.len();
                above = Some(last);
            }
        };
        take(payload);
        while self.heap.peek().is_some_and(|h| h.key == key) {
            let (_, payload) = self.next_record()?.expect("a record was peeked");
            take(payload);
        }
        Ok(Some(Group {
            key,
            payloads: kept.into_iter().collect(),
            counts,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> Room {
        Room::new(1 << 30, 0)
    }
    struct Gen(u64);
    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Property: for random keys, payloads, budgets and fan-ins, the merge after `reduce` gives
    /// every key in byte order with exactly its distinct payloads and counts, as a `BTreeMap`
    /// does.
    #[test]
    fn the_merge_is_a_sorted_map_of_the_records() {
        let mut gen = Gen(7);
        for round in 0..6 {
            let dir = tempfile::tempdir().unwrap();
            let budget = 64 + (gen.next() % 4000) as usize;
            let fan_in = 2 + (gen.next() % 6) as usize;
            let mut spill = Spill::new(&dir.path().join("s"), budget, room()).unwrap();
            let mut want = std::collections::BTreeMap::<Vec<u8>, (BTreeSet<Vec<u8>>, u64)>::new();
            for _ in 0..(500 + round * 700) {
                let key = format!("k{:04}", gen.next() % 400).into_bytes();
                let payload = vec![(gen.next() % 3) as u8, (gen.next() % 50) as u8];
                spill.push(&key, &payload).unwrap();
                let e = want.entry(key).or_default();
                e.0.insert(payload);
                e.1 += 1;
            }
            let runs = spill.finish().unwrap();
            let runs = reduce(&dir.path().join("p"), runs, fan_in, &room(), &|| false).unwrap();
            assert!(runs.len() <= fan_in);
            let mut merge = Merge::open(&runs).unwrap();
            let mut got = std::collections::BTreeMap::new();
            let mut last: Option<Vec<u8>> = None;
            while let Some(g) = merge.next_group(usize::MAX, usize::MAX).unwrap() {
                assert!(last.as_ref().is_none_or(|l| *l < g.key));
                last = Some(g.key.clone());
                let total: u64 = g.counts.iter().sum();
                got.insert(
                    g.key,
                    (g.payloads.into_iter().collect::<BTreeSet<_>>(), total),
                );
            }
            assert_eq!(got, want, "round {round}");
        }
    }

    /// Property: a group's kept payloads depend only on its records, never on their order: the
    /// smallest distinct payloads that fit the caps. Counts cover every record.
    #[test]
    fn a_bounded_group_is_the_same_in_any_order() {
        let mut gen = Gen(99);
        for _ in 0..200 {
            let n = 1 + (gen.next() % 40) as usize;
            let payloads: Vec<Vec<u8>> = (0..n)
                .map(|_| {
                    let len = 1 + (gen.next() % 6) as usize;
                    (0..len).map(|_| (gen.next() % 4) as u8).collect()
                })
                .collect();
            let max_records = 1 + (gen.next() % 10) as usize;
            let max_bytes = 1 + (gen.next() % 30) as usize;
            let group_of = |order: &[Vec<u8>]| {
                let dir = tempfile::tempdir().unwrap();
                let mut spill = Spill::new(dir.path(), 1, room()).unwrap();
                for p in order {
                    spill.push(b"k", p).unwrap();
                }
                let runs = spill.finish().unwrap();
                let mut merge = Merge::open(&runs).unwrap();
                let g = merge.next_group(max_records, max_bytes).unwrap().unwrap();
                (g.payloads, g.counts)
            };
            let (first, counts) = group_of(&payloads);
            let mut reversed = payloads.clone();
            reversed.reverse();
            let mut rotated = payloads.clone();
            rotated.rotate_left(n / 2);
            for other in [reversed, rotated] {
                assert_eq!(group_of(&other), (first.clone(), counts));
            }
            assert_eq!(counts.iter().sum::<u64>(), n as u64);
            assert!(first.len() <= max_records);
            assert!(first.iter().map(Vec::len).sum::<usize>() <= max_bytes);
            // The kept ones are the smallest distinct payloads.
            let distinct: BTreeSet<Vec<u8>> = payloads.into_iter().collect();
            assert_eq!(
                first,
                distinct.into_iter().take(first.len()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_spill_over_its_room_is_a_disk_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut spill = Spill::new(&dir.path().join("s"), 10, Room::new(100, 0)).unwrap();
        let failed = (0..100)
            .map(|n| spill.push(format!("{n}").as_bytes(), &[1; 64]))
            .find_map(Result::err)
            .expect("the cap is reached");
        assert_eq!(
            failed.downcast_ref::<Failure>().map(|f| f.class),
            Some("disk")
        );
        assert!(spill.push(&[b'k'; KEY_CAP + 1], b"").is_err());
        // Too little free disk.
        let full = Room {
            free_disk: |_| Ok(10),
            ..Room::new(1 << 30, 1 << 20)
        };
        let mut spill = Spill::new(&dir.path().join("t"), 10, full).unwrap();
        assert!(spill.push(b"k", b"v").is_err());
        // Passes stop when asked to, and check their room too.
        let mut spill = Spill::new(&dir.path().join("u"), 1, room()).unwrap();
        for n in 0..10u8 {
            spill.push(&[n], b"v").unwrap();
        }
        let runs = spill.finish().unwrap();
        assert!(reduce(&dir.path().join("p"), runs.clone(), 2, &room(), &|| true).is_err());
        assert!(reduce(&dir.path().join("q"), runs, 2, &full, &|| false).is_err());
    }

    /// A pass merges only as many runs as it takes to get down to the fan-in, the smallest
    /// first; the others are left as they are.
    #[test]
    fn a_pass_merges_only_the_excess() {
        let dir = tempfile::tempdir().unwrap();
        let mut runs = vec![];
        for n in 0..10u8 {
            let mut spill = Spill::new(&dir.path().join(format!("s{n}")), 1 << 20, room()).unwrap();
            for k in 0..(10 + u32::from(n) * 50) {
                spill.push(format!("{k:05}").as_bytes(), &[n]).unwrap();
            }
            runs.extend(spill.finish().unwrap());
        }
        let before: Vec<_> = runs
            .iter()
            .map(|r| std::fs::metadata(r).unwrap().modified().unwrap())
            .collect();
        let left = reduce(&dir.path().join("p"), runs.clone(), 8, &room(), &|| false).unwrap();
        assert_eq!(left.len(), 8);
        // The three smallest went into one pass file; the seven others are untouched.
        let kept: Vec<_> = runs[3..].to_vec();
        for (r, t) in kept.iter().zip(&before[3..]) {
            assert!(left.contains(r));
            assert_eq!(std::fs::metadata(r).unwrap().modified().unwrap(), *t);
        }
        assert!(runs[..3].iter().all(|r| !r.exists()));
        // A merge asked to stop in a pass leaves every input and no output.
        let mut spill = Spill::new(&dir.path().join("big"), 1 << 22, room()).unwrap();
        for k in 0..200_000u32 {
            spill.push(&k.to_be_bytes(), b"v").unwrap();
        }
        let runs = spill.finish().unwrap();
        assert!(runs.len() > 3);
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let stop = || asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1;
        assert!(reduce(&dir.path().join("q"), runs.clone(), 2, &room(), &stop).is_err());
        assert!(runs.iter().all(|r| r.exists()));
        assert_eq!(std::fs::read_dir(dir.path().join("q")).unwrap().count(), 0);
        // Asked to stop within a pass (from the third ask: past the pass's start and its first
        // 65,536 records), it stops there: no pass ends, every input is kept.
        let mut spill = Spill::new(&dir.path().join("long"), 8 << 20, room()).unwrap();
        for k in 0..600_000u32 {
            spill.push(&k.to_be_bytes(), b"v").unwrap();
        }
        let runs = spill.finish().unwrap();
        assert!(runs.len() > 3, "{} runs", runs.len());
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let stop = || asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2;
        assert!(reduce(&dir.path().join("r"), runs.clone(), 2, &room(), &stop).is_err());
        assert!(runs.iter().all(|r| r.exists()));
        assert_eq!(std::fs::read_dir(dir.path().join("r")).unwrap().count(), 0);
    }

    #[test]
    fn a_damaged_run_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut spill = Spill::new(&dir.path().join("s"), 1 << 20, room()).unwrap();
        for n in 0..100u32 {
            spill.push(format!("{n:04}").as_bytes(), &[7; 100]).unwrap();
        }
        let runs = spill.finish().unwrap();
        let bytes = std::fs::read(&runs[0]).unwrap();
        std::fs::write(&runs[0], &bytes[..bytes.len() / 2]).unwrap();
        // An error when the merge opens the run or while it reads it, never a panic.
        let read = Merge::open(&runs).and_then(|mut merge| {
            while merge.next_group(usize::MAX, usize::MAX)?.is_some() {}
            Ok(())
        });
        assert!(read.is_err());
    }
}
