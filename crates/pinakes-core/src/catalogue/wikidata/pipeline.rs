//! The dump read on several threads: one reads the compressed bytes and finds the blocks
//! ([`bz2::Splitter`]), several decode blocks, one puts them back in order and cuts the text into
//! lines, several parse batches of lines into cards ([`entity::parse_line`]). The batches come
//! out numbered ([`Batch::seq`]), each with the position the read resumes at after it
//! ([`Position`]); the caller writes them in order.
//!
//! Everything in flight is bounded (channels of a few items), so memory stays a few hundred
//! megabytes whatever the dump's size. The threads stop when [`Pipeline`] is dropped or
//! [`Pipeline::close`]d: each checks a stop flag and ends when the next stage is gone.
use super::bz2::{self, Bz2Error, Decoded, Orderer, Segment, Splitter};
use super::entity::{self, Kind, Labels, Parsed};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Where a read stands: the first bit of a block, and how many bytes of that block's text come
/// before the next line to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub bit: u64,
    pub skip: u64,
}

/// The lines one batch holds at most, and its bytes at most (a line larger than this is a batch
/// of its own).
const BATCH_LINES: usize = 256;
const BATCH_BYTES: usize = 8 << 20;
/// Compressed bytes read at a time.
const READ_CHUNK: usize = 1 << 20;
/// The most decoded text waiting to be cut into lines: decoders pause past it.
const DECODED_IN_FLIGHT: usize = 512 << 20;

/// What a batch counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub lines: u64,
    pub kept: u64,
    pub no_article: u64,
    pub wikimedia_pages: u64,
    pub unusable: u64,
    pub other: u64,
    pub bad: u64,
    pub oversized: u64,
}
impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, o: Self) {
        self.lines += o.lines;
        self.kept += o.kept;
        self.no_article += o.no_article;
        self.wikimedia_pages += o.wikimedia_pages;
        self.unusable += o.unusable;
        self.other += o.other;
        self.bad += o.bad;
        self.oversized += o.oversized;
    }
}

/// One parsed batch.
#[derive(Debug)]
pub struct Batch {
    pub seq: u64,
    /// The kept items in dump order, each with the items its labels come from.
    pub items: Vec<Parsed>,
    /// Labels of classes and genres the batch's lines hold.
    pub labels: Vec<(u32, Option<Box<str>>)>,
    pub counts: Counts,
    /// Where the read resumes after this batch.
    pub end: Position,
    /// The bytes of text the batch's lines held.
    pub bytes: u64,
}

/// Why a read stopped early.
#[derive(Debug)]
pub enum ReadError {
    Bz2(Bz2Error),
    Io(std::io::Error),
}
impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Bz2(e) => write!(f, "{e}"),
            ReadError::Io(_) => f.write_str("read"),
        }
    }
}
impl std::error::Error for ReadError {}

/// What comes out of the pipeline.
#[derive(Debug)]
pub enum Out {
    Batch(Batch),
    /// The read ended: this many batches in all (numbered from 0), or why it stopped.
    End(Result<u64, ReadError>),
}

/// A read under way.
pub struct Pipeline {
    pub rx: tokio::sync::mpsc::Receiver<Out>,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}
impl Pipeline {
    /// Stop every thread and wait for them.
    pub async fn close(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.rx.close();
        let threads = std::mem::take(&mut self.threads);
        let _ = tokio::task::spawn_blocking(move || {
            for t in threads {
                let _ = t.join();
            }
        })
        .await;
    }
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        // Threads still running see the flag at their next step and end.
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// The threads a read uses: half the cores (the daemon keeps answering searches meanwhile),
/// at least one decoder and one parser.
pub fn threads() -> (usize, usize) {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let half = (cores / 2).clamp(2, 32);
    let decoders = half.div_ceil(2);
    let parsers = (half - decoders).max(1);
    (decoders, parsers)
}

/// Start reading `source` (the compressed file from its first byte, or from `from`'s block on:
/// `source` then starts at byte `from.bit / 8`).
pub fn start(
    source: Box<dyn Read + Send>,
    from: Option<Position>,
    (decoders, parsers): (usize, usize),
) -> Pipeline {
    let stop = Arc::new(AtomicBool::new(false));
    let in_flight = Arc::new(AtomicUsize::new(0));
    // Room for the batches parsed while the writer waits for the database.
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Out>(128);
    let (seg_tx, seg_rx) = crossbeam_channel::bounded::<Segment>(decoders * 4);
    let (dec_tx, dec_rx) = crossbeam_channel::bounded::<FromDecoder>(decoders * 4);
    let (line_tx, line_rx) = crossbeam_channel::bounded::<Lines>(parsers * 2);
    let mut threads = Vec::new();
    let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .expect("a thread starts")
    };
    // The reader and splitter.
    {
        let (stop, dec_tx, in_flight) = (stop.clone(), dec_tx.clone(), in_flight.clone());
        threads.push(spawn(
            "wikidata read",
            Box::new(move || read(source, from, seg_tx, dec_tx, &stop, &in_flight)),
        ));
    }
    for n in 0..decoders {
        let (seg_rx, dec_tx, stop, in_flight) = (
            seg_rx.clone(),
            dec_tx.clone(),
            stop.clone(),
            in_flight.clone(),
        );
        threads.push(spawn(
            &format!("wikidata bz2 {n}"),
            Box::new(move || {
                while let Ok(segment) = seg_rx.recv() {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    // A panic in the decoder is a block that does not decode.
                    let result = std::panic::catch_unwind(|| bz2::decode(b'9', &[&segment]))
                        .unwrap_or(Err(Bz2Error::Block));
                    if let Ok(text) = &result {
                        in_flight.fetch_add(text.len(), Ordering::Relaxed);
                    }
                    if dec_tx.send(FromDecoder::Block(segment, result)).is_err() {
                        return;
                    }
                }
            }),
        ));
    }
    drop(dec_tx);
    {
        let (stop, out_tx, in_flight) = (stop.clone(), out_tx.clone(), in_flight.clone());
        threads.push(spawn(
            "wikidata lines",
            Box::new(move || {
                order(
                    dec_rx,
                    line_tx,
                    out_tx,
                    from.map_or(0, |p| p.skip),
                    &stop,
                    &in_flight,
                )
            }),
        ));
    }
    for n in 0..parsers {
        let (line_rx, out_tx, stop) = (line_rx.clone(), out_tx.clone(), stop.clone());
        threads.push(spawn(
            &format!("wikidata parse {n}"),
            Box::new(move || {
                while let Ok(lines) = line_rx.recv() {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let batch = parse(lines);
                    if out_tx.blocking_send(Out::Batch(batch)).is_err() {
                        return;
                    }
                }
            }),
        ));
    }
    Pipeline {
        rx: out_rx,
        stop,
        threads,
    }
}

enum FromDecoder {
    Block(Segment, Result<Vec<u8>, Bz2Error>),
    /// The reader is done: this many segments sent, and why it stopped early, if it did.
    Done(u64, Option<ReadError>),
}

fn read(
    mut source: Box<dyn Read + Send>,
    from: Option<Position>,
    seg_tx: crossbeam_channel::Sender<Segment>,
    dec_tx: crossbeam_channel::Sender<FromDecoder>,
    stop: &AtomicBool,
    in_flight: &AtomicUsize,
) {
    let mut splitter = match from {
        Some(p) => Splitter::resume(p.bit),
        None => Splitter::new(),
    };
    let mut buf = vec![0u8; READ_CHUNK];
    let mut segments = Vec::new();
    let mut count = 0u64;
    let result: Result<(), ReadError> = (|| loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let n = match source.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(ReadError::Io(e)),
        };
        if n == 0 {
            splitter.finish(&mut segments).map_err(ReadError::Bz2)?;
        } else {
            splitter
                .push(&buf[..n], &mut segments)
                .map_err(ReadError::Bz2)?;
        }
        for segment in segments.drain(..) {
            // Decoded text not cut yet is bounded: no more blocks go out while it is over its
            // budget. Those already out (the next one in order among them) still decode.
            while in_flight.load(Ordering::Relaxed) > DECODED_IN_FLIGHT {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            count += 1;
            if seg_tx.send(segment).is_err() {
                return Ok(());
            }
        }
        if n == 0 {
            return Ok(());
        }
    })();
    drop(seg_tx);
    let _ = dec_tx.send(FromDecoder::Done(count, result.err()));
}

/// Lines cut from the text, numbered, with the position after the last one.
struct Lines {
    seq: u64,
    lines: Vec<Vec<u8>>,
    oversized: u64,
    blank: u64,
    end: Position,
}

fn order(
    dec_rx: crossbeam_channel::Receiver<FromDecoder>,
    line_tx: crossbeam_channel::Sender<Lines>,
    out_tx: tokio::sync::mpsc::Sender<Out>,
    first_skip: u64,
    stop: &AtomicBool,
    in_flight: &AtomicUsize,
) {
    let mut orderer = Orderer::new(b'9');
    let mut cutter = Cutter::new(first_skip);
    let mut total: Option<u64> = None;
    // Why the reader stopped early: the blocks it sent before still come out.
    let mut failed: Option<ReadError> = None;
    let mut taken = 0u64;
    let mut decoded = Vec::new();
    let mut seq = 0u64;
    let end = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if total == Some(taken) {
            if let Some(e) = failed.take() {
                // The whole lines before the failure; the line it cut is not one.
                if let Some(lines) = cutter.flush(seq) {
                    if line_tx.send(lines).is_err() {
                        return;
                    }
                }
                break Err(e);
            }
            if let Err(e) = orderer.finish() {
                break Err(ReadError::Bz2(e));
            }
            if let Some(lines) = cutter.finish(seq) {
                seq += 1;
                if line_tx.send(lines).is_err() {
                    return;
                }
            }
            break Ok(seq);
        }
        match dec_rx.recv() {
            Ok(FromDecoder::Block(segment, result)) => {
                taken += 1;
                if let Err(e) = orderer.take(segment, result, &mut decoded) {
                    break Err(ReadError::Bz2(e));
                }
                for block in decoded.drain(..) {
                    let len = block.counted;
                    let sent =
                        cutter.push(block, &mut seq, &mut |lines| line_tx.send(lines).is_ok());
                    in_flight.fetch_sub(
                        len.min(in_flight.load(Ordering::Relaxed)),
                        Ordering::Relaxed,
                    );
                    if !sent {
                        return;
                    }
                }
            }
            Ok(FromDecoder::Done(n, error)) => {
                total = Some(n);
                failed = error;
            }
            Err(_) => return,
        }
    };
    drop(line_tx);
    let _ = out_tx.blocking_send(Out::End(end));
}

/// Cuts decoded blocks into lines, keeping where each starts.
struct Cutter {
    /// Bytes of the first block to drop (a resume).
    skip: u64,
    line: Vec<u8>,
    /// The line under way is over the cap: its bytes are dropped up to its end.
    over: bool,
    oversized: u64,
    /// Empty lines dropped (counted as `other`).
    blank: u64,
    lines: Vec<Vec<u8>>,
    bytes: usize,
    /// Where the next line starts.
    next: Position,
    started: bool,
}
impl Cutter {
    fn new(skip: u64) -> Self {
        Self {
            skip,
            line: Vec::new(),
            over: false,
            oversized: 0,
            blank: 0,
            lines: Vec::new(),
            bytes: 0,
            next: Position::default(),
            started: false,
        }
    }
    /// Cut `block` into lines; each batch that fills is given to `send` at once (false: the
    /// next stage is gone, stop). Whether every batch was taken.
    fn push(&mut self, block: Decoded, seq: &mut u64, send: &mut dyn FnMut(Lines) -> bool) -> bool {
        let mut text = &block.text[..];
        let mut at = 0u64;
        if !self.started {
            self.started = true;
            let skip = (self.skip as usize).min(text.len());
            text = &text[skip..];
            at = skip as u64;
            self.next = Position {
                bit: block.start_bit,
                skip: at,
            };
        }
        while !text.is_empty() {
            match memchr::memchr(b'\n', text) {
                Some(i) => {
                    self.append(&text[..i]);
                    let line = std::mem::take(&mut self.line);
                    if self.over {
                        self.oversized += 1;
                        self.over = false;
                    } else if line.is_empty() {
                        self.blank += 1;
                    } else {
                        self.bytes += line.len();
                        self.lines.push(line);
                    }
                    at += i as u64 + 1;
                    text = &text[i + 1..];
                    self.next = Position {
                        bit: block.start_bit,
                        skip: at,
                    };
                    if self.lines.len() >= BATCH_LINES || self.bytes >= BATCH_BYTES {
                        let lines = self.take(*seq);
                        *seq += 1;
                        if !send(lines) {
                            return false;
                        }
                    }
                }
                None => {
                    // The line goes on in the next block: room for about as much again.
                    if self.line.capacity() == 0 {
                        self.line.reserve(text.len() * 2);
                    }
                    self.append(text);
                    text = &[];
                }
            }
        }
        true
    }
    fn append(&mut self, bytes: &[u8]) {
        if self.over {
            return;
        }
        if self.line.len() + bytes.len() > entity::LINE_CAP {
            self.over = true;
            self.line = Vec::new();
        } else {
            self.line.extend_from_slice(bytes);
        }
    }
    fn take(&mut self, seq: u64) -> Lines {
        self.bytes = 0;
        Lines {
            seq,
            lines: std::mem::take(&mut self.lines),
            oversized: std::mem::take(&mut self.oversized),
            blank: std::mem::take(&mut self.blank),
            end: self.next,
        }
    }
    /// The text has ended: the lines left, with a last line that has no newline.
    /// The whole lines cut so far (a read that failed): the line under way stays out.
    fn flush(&mut self, seq: u64) -> Option<Lines> {
        (!self.lines.is_empty() || self.oversized > 0 || self.blank > 0).then(|| self.take(seq))
    }
    fn finish(&mut self, seq: u64) -> Option<Lines> {
        if !self.line.is_empty() && !self.over {
            let line = std::mem::take(&mut self.line);
            self.lines.push(line);
        }
        if self.over {
            self.oversized += 1;
        }
        (!self.lines.is_empty() || self.oversized > 0 || self.blank > 0).then(|| self.take(seq))
    }
}

/// The lines of a batch parsed. Labels are resolved by the writer, in dump order, so a parser
/// knows none; a line whose parse panics counts as `bad`.
fn parse(lines: Lines) -> Batch {
    static NONE: std::sync::LazyLock<Labels> = std::sync::LazyLock::new(Labels::new);
    let mut counts = Counts {
        oversized: lines.oversized,
        other: lines.blank,
        lines: lines.blank,
        ..Counts::default()
    };
    let mut items = Vec::new();
    let mut found = Vec::new();
    let mut bytes = 0u64;
    for line in &lines.lines {
        counts.lines += 1;
        bytes += line.len() as u64;
        let parsed = std::panic::catch_unwind(|| entity::parse_line(line, &NONE));
        let Ok(mut parsed) = parsed else {
            counts.bad += 1;
            continue;
        };
        if let Some(label) = parsed.label.take() {
            found.push(label);
        }
        match parsed.kind {
            Kind::Kept => {
                counts.kept += 1;
                items.push(parsed);
            }
            Kind::NoArticle => counts.no_article += 1,
            Kind::WikimediaPage => counts.wikimedia_pages += 1,
            Kind::Unusable => counts.unusable += 1,
            Kind::Other => counts.other += 1,
            Kind::Bad => counts.bad += 1,
        }
    }
    Batch {
        seq: lines.seq,
        items,
        labels: found,
        counts,
        end: lines.end,
        bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn compress(data: &[u8], level: u32) -> Vec<u8> {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(level));
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }
    fn lines_text(n: usize) -> Vec<u8> {
        let mut out = b"[\n".to_vec();
        for i in 1..=n {
            out.extend_from_slice(
                format!(
                    "{{\"type\":\"item\",\"id\":\"Q{i}\",\"labels\":{{}},\"pad\":\"{}\"}},\n",
                    "x".repeat(i % 97)
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(b"]\n");
        out
    }
    async fn run(
        packed: Vec<u8>,
        from: Option<Position>,
        threads: (usize, usize),
    ) -> (Vec<Batch>, u64) {
        let start = from.map_or(0, |p| p.bit / 8) as usize;
        let source: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(packed[start..].to_vec()));
        let mut p = super::start(source, from, threads);
        let mut batches = vec![];
        let total = loop {
            match p.rx.recv().await.unwrap() {
                Out::Batch(b) => batches.push(b),
                Out::End(r) => break r.unwrap(),
            }
        };
        while (batches.len() as u64) < total {
            match p.rx.recv().await.unwrap() {
                Out::Batch(b) => batches.push(b),
                Out::End(_) => unreachable!(),
            }
        }
        p.close().await;
        batches.sort_by_key(|b| b.seq);
        (batches, total)
    }

    /// Every line is read once, in order, and a read resumed at any batch's end reads exactly
    /// the lines after it.
    #[tokio::test(flavor = "multi_thread")]
    async fn lines_come_once_in_order_and_a_read_resumes_after_any_batch() {
        let text = lines_text(20_000);
        let packed = compress(&text, 1);
        let (batches, total) = run(packed.clone(), None, (3, 2)).await;
        assert_eq!(total as usize, batches.len());
        let lines: u64 = batches.iter().map(|b| b.counts.lines).sum();
        // `[`, 20,000 items, `]`.
        assert_eq!(lines, 20_002);
        assert_eq!(batches.iter().map(|b| b.counts.other).sum::<u64>(), 2);
        assert_eq!(
            batches.iter().map(|b| b.counts.no_article).sum::<u64>(),
            20_000
        );
        for cut in [0, 1, batches.len() / 2, batches.len() - 2] {
            let from = batches[cut].end;
            let (rest, _) = run(packed.clone(), Some(from), (2, 1)).await;
            let want: u64 = batches[cut + 1..].iter().map(|b| b.counts.lines).sum();
            assert_eq!(
                rest.iter().map(|b| b.counts.lines).sum::<u64>(),
                want,
                "cut {cut}"
            );
            let want_bytes: u64 = batches[cut + 1..].iter().map(|b| b.bytes).sum();
            assert_eq!(
                rest.iter().map(|b| b.bytes).sum::<u64>(),
                want_bytes,
                "cut {cut}"
            );
        }
    }

    /// A line over the cap is skipped and counted (`oversized`); the lines around it are read.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_line_over_the_cap_is_counted_and_skipped() {
        let mut text = b"[\n".to_vec();
        text.extend_from_slice(b"{\"type\":\"item\",\"id\":\"Q1\",\"x\":\"");
        text.extend(std::iter::repeat_n(b'a', entity::LINE_CAP + 10));
        text.extend_from_slice(b"\"},\n{\"type\":\"item\",\"id\":\"Q2\"},\n]\n");
        let (batches, _) = run(compress(&text, 9), None, (2, 2)).await;
        let counts = batches.iter().fold(Counts::default(), |mut c, b| {
            c += b.counts;
            c
        });
        assert_eq!(counts.oversized, 1);
        assert_eq!(counts.lines, 3);
    }

    /// A read of a broken file ends with an error, and dropping the pipeline stops its threads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_broken_file_ends_with_an_error() {
        let packed = compress(&lines_text(5_000), 1);
        let cut = packed[..packed.len() / 2].to_vec();
        let source: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(cut));
        let mut p = super::start(source, None, (2, 2));
        let end = loop {
            match p.rx.recv().await.unwrap() {
                Out::Batch(_) => {}
                Out::End(r) => break r,
            }
        };
        assert!(
            matches!(end, Err(ReadError::Bz2(Bz2Error::Truncated))),
            "{end:?}"
        );
        p.close().await;
        let source: Box<dyn Read + Send> =
            Box::new(std::io::Cursor::new(compress(&lines_text(50_000), 1)));
        let p = super::start(source, None, (2, 2));
        p.close().await;
    }
}
