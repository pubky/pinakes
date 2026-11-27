//! Reading a catalogue's bulk files as streams: a tar archive's entries ([`Tar`]), a file's lines
//! within a size cap ([`Lines`]), an `.xz` stream ([`xz`]), a download that picks up again where a
//! broken connection left it ([`Download`]), and the SHA-256 of what was read ([`Hashed`]).
//!
//! Nothing here holds more than one line (at most [`Lines::cap`] bytes) or one tar header in
//! memory, and nothing trusts a size the file states beyond what it reads: a malformed archive is
//! an error, never a panic.
use super::{failure::Failure, fetch};
use anyhow::{bail, Result};
use sha2::Digest;
use std::io::{self, BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// One entry of a tar archive as [`Tar::next_entry`] gives it: its path and its size in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
}

/// A tar archive read from a stream, one entry after the other (ustar, GNU and pax: a GNU long
/// name, a pax `path` or `size`, a size over 8 GiB in GNU's base-256 form). An entry's data is
/// read through [`Tar::data`] before the next entry is asked for; what is left of it is skipped.
pub struct Tar<R> {
    inner: R,
    /// The bytes of the current entry not read yet, then its padding to the next 512.
    left: u64,
    padding: u64,
}
/// The longest path or pax header kept (bytes).
const HEADER_TEXT_CAP: u64 = 64 * 1024;
/// The largest entry an archive may hold (bytes): 4 TiB, ten times the largest dump file.
pub const ENTRY_CAP: u64 = 4 << 40;
impl<R: Read> Tar<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            left: 0,
            padding: 0,
        }
    }
    /// The next file entry (directories, links and the like are skipped), or `None` at the
    /// archive's end (two zero blocks, or the stream's end at an entry boundary).
    pub fn next_entry(&mut self) -> Result<Option<Entry>> {
        self.skip_rest()?;
        let mut long_name: Option<String> = None;
        let mut pax_path: Option<String> = None;
        let mut pax_size: Option<u64> = None;
        loop {
            let mut header = [0u8; 512];
            if !read_block(&mut self.inner, &mut header)? {
                return Ok(None);
            }
            if header.iter().all(|b| *b == 0) {
                return Ok(None);
            }
            if !checksum_ok(&header) {
                bail!(Failure::new("archive"));
            }
            let size = header_size(&header[124..136]).ok_or_else(|| Failure::new("archive"))?;
            let kind = header[156];
            match kind {
                // GNU long name, pax extended header (this entry's), pax global header.
                b'L' | b'x' | b'g' => {
                    if size > HEADER_TEXT_CAP {
                        bail!(Failure::new("archive"));
                    }
                    // `size` is small here: no sum below overflows.
                    let mut text = vec![0u8; size as usize];
                    self.inner.read_exact(&mut text)?;
                    skip(&mut self.inner, pad(size))?;
                    match kind {
                        b'L' => {
                            let end = text.iter().position(|b| *b == 0).unwrap_or(text.len());
                            long_name = Some(String::from_utf8_lossy(&text[..end]).into_owned());
                        }
                        b'x' => {
                            for (key, value) in pax_records(&text) {
                                match key.as_str() {
                                    "path" => pax_path = Some(value),
                                    "size" => pax_size = value.parse().ok(),
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {
                    let size = pax_size.take().unwrap_or(size);
                    if size > ENTRY_CAP {
                        bail!(Failure::new("size"));
                    }
                    let path = pax_path
                        .take()
                        .or_else(|| long_name.take())
                        .unwrap_or_else(|| ustar_path(&header));
                    // A regular file ('0', or NUL in old archives); everything else is skipped
                    // with its data.
                    if matches!(kind, b'0' | 0 | b'7') {
                        self.left = size;
                        self.padding = pad(size);
                        return Ok(Some(Entry { path, size }));
                    }
                    skip(&mut self.inner, size + pad(size))?;
                }
            }
        }
    }
    /// The stream after what has been read of the archive.
    pub fn into_inner(self) -> R {
        self.inner
    }
    /// The current entry's data.
    pub fn data(&mut self) -> TarData<'_, R> {
        TarData { tar: self }
    }
    fn skip_rest(&mut self) -> Result<()> {
        skip(&mut self.inner, self.left + self.padding)?;
        self.left = 0;
        self.padding = 0;
        Ok(())
    }
}
/// The data of a tar entry: reads end at the entry's end.
pub struct TarData<'a, R> {
    tar: &'a mut Tar<R>,
}
impl<R: Read> Read for TarData<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.tar.left == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.tar.left.min(usize::MAX as u64) as usize);
        let n = self.tar.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the archive ends inside an entry",
            ));
        }
        self.tar.left -= n as u64;
        Ok(n)
    }
}
/// The padding after `size` bytes of data to the next 512-byte block. Sizes are at most
/// [`ENTRY_CAP`], so `size + pad(size)` never overflows.
fn pad(size: u64) -> u64 {
    (512 - size % 512) % 512
}
/// A whole 512-byte block, `false` at the stream's end before its first byte; an end inside it
/// is an error.
fn read_block(r: &mut impl Read, block: &mut [u8; 512]) -> Result<bool> {
    let mut got = 0;
    while got < 512 {
        let n = match r.read(&mut block[got..]) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            if got == 0 {
                return Ok(false);
            }
            bail!(Failure::new("archive"));
        }
        got += n;
    }
    Ok(true)
}
fn skip(r: &mut impl Read, n: u64) -> Result<()> {
    let copied = io::copy(&mut r.take(n), &mut io::sink())?;
    if copied != n {
        bail!(Failure::new("archive"));
    }
    Ok(())
}
/// The header's checksum: the sum of its bytes with the checksum field as spaces, written in
/// octal.
fn checksum_ok(header: &[u8; 512]) -> bool {
    let Some(stated) = octal(&header[148..156]) else {
        return false;
    };
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                u64::from(b' ')
            } else {
                u64::from(*b)
            }
        })
        .sum();
    sum == stated
}
fn octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}
/// A size field: octal, or GNU's base-256 (first byte's high bit set, the rest big-endian).
fn header_size(field: &[u8]) -> Option<u64> {
    if field[0] & 0x80 != 0 {
        if field[0] != 0x80 || field[1..4].iter().any(|b| *b != 0) {
            return None; // negative, or over 2^64
        }
        Some(
            field[4..]
                .iter()
                .fold(0u64, |n, b| (n << 8) | u64::from(*b)),
        )
    } else {
        octal(field)
    }
}
fn ustar_path(header: &[u8; 512]) -> String {
    let field = |range: std::ops::Range<usize>| {
        let bytes = &header[range];
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    let name = field(0..100);
    // A ustar prefix (POSIX `ustar\0`); GNU's `ustar  ` header keeps other fields there.
    if &header[257..263] == b"ustar\0" {
        let prefix = field(345..500);
        if !prefix.is_empty() {
            return format!("{prefix}/{name}");
        }
    }
    name
}
/// The records of a pax extended header: `<length> <key>=<value>\n` each.
fn pax_records(mut text: &[u8]) -> Vec<(String, String)> {
    let mut out = vec![];
    while !text.is_empty() {
        let Some(space) = text.iter().position(|b| *b == b' ') else {
            break;
        };
        let Some(len) = std::str::from_utf8(&text[..space])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        else {
            break;
        };
        if len <= space + 1 || len > text.len() {
            break;
        }
        let record = &text[space + 1..len];
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        if let Some(eq) = record.iter().position(|b| *b == b'=') {
            out.push((
                String::from_utf8_lossy(&record[..eq]).into_owned(),
                String::from_utf8_lossy(&record[eq + 1..]).into_owned(),
            ));
        }
        text = &text[len..];
    }
    out
}

/// A stream's lines, each at most `cap` bytes (without its line break): a longer line is skipped
/// whole and counted ([`Lines::too_long`]), so one line never takes more memory than the cap.
pub struct Lines<R> {
    inner: R,
    cap: usize,
    too_long: u64,
}
impl<R: BufRead> Lines<R> {
    pub fn new(inner: R, cap: usize) -> Self {
        Self {
            inner,
            cap,
            too_long: 0,
        }
    }
    /// The longest line kept.
    pub fn cap(&self) -> usize {
        self.cap
    }
    /// The lines skipped for being longer than the cap.
    pub fn too_long(&self) -> u64 {
        self.too_long
    }
    /// The next line into `line` (cleared first, without its `\n` or `\r\n`); `false` at the end.
    /// A last line without a line break counts.
    pub fn next_line(&mut self, line: &mut Vec<u8>) -> io::Result<bool> {
        line.clear();
        let mut skipping = false;
        loop {
            let buf = match self.inner.fill_buf() {
                Ok(buf) => buf,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if buf.is_empty() {
                if skipping {
                    self.too_long += 1;
                    line.clear();
                    return Ok(false);
                }
                return Ok(!line.is_empty());
            }
            let (part, done) = match memchr::memchr(b'\n', buf) {
                Some(i) => (&buf[..i], Some(i + 1)),
                None => (buf, None),
            };
            if !skipping {
                if line.len() + part.len() > self.cap {
                    skipping = true;
                    line.clear();
                } else {
                    line.extend_from_slice(part);
                }
            }
            match done {
                Some(used) => {
                    self.inner.consume(used);
                    if skipping {
                        self.too_long += 1;
                        skipping = false;
                        continue;
                    }
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(true);
                }
                None => {
                    let used = buf.len();
                    self.inner.consume(used);
                }
            }
        }
    }
}

/// An `.xz` file decoded on `threads` threads (liblzma's multi-threaded decoder, which decodes a
/// stream of several blocks in parallel and one of a single block on one thread), within
/// `memory` bytes of decoder memory: every stream of it, as `xz` reads concatenated ones (the
/// multi-threaded decoder stops at a stream's end), with the stream padding between them. A file
/// of more than [`XZ_EMPTY_STREAMS`] streams that decode to nothing, or more than
/// [`XZ_PADDING_CAP`] bytes of padding in all, is broken (`InvalidData`: the caller's `archive`);
/// a stop ([`Xz::stop`]) is heard between streams and in their padding.
pub fn xz<R: BufRead>(inner: R, threads: u32, memory: u64) -> Result<Xz<R>> {
    let decoder = liblzma::bufread::XzDecoder::new_stream(inner, xz_stream(threads, memory)?);
    Ok(Xz {
        decoder: Some(decoder),
        rest: None,
        threads,
        memory,
        gave: false,
        empty: 0,
        padding: 0,
        stop: None,
    })
}
/// The streams that decode to no byte one `.xz` file may hold.
pub const XZ_EMPTY_STREAMS: u32 = 64;
/// The stream padding (NUL bytes between streams) one `.xz` file may hold, in all (as much as
/// the decoded bytes MusicBrainz lets an archive hold after the entry it reads).
pub const XZ_PADDING_CAP: u64 = 16 << 20;
fn xz_stream(threads: u32, memory: u64) -> Result<liblzma::stream::Stream> {
    liblzma::stream::MtStreamBuilder::new()
        .threads(threads.max(1))
        .memlimit_threading(memory)
        .memlimit_stop(memory)
        .decoder()
        .map_err(|_| Failure::new("xz").into())
}
/// [`xz`]'s reader: the streams one after another; at the end, what is left of the input in
/// [`Xz::into_inner`].
pub struct Xz<R: BufRead> {
    decoder: Option<liblzma::bufread::XzDecoder<R>>,
    rest: Option<R>,
    threads: u32,
    memory: u64,
    /// Whether the stream being read gave a byte yet; the streams that decoded to nothing so
    /// far, and the padding read between streams.
    gave: bool,
    empty: u32,
    padding: u64,
    /// Set when the run stops: checked between streams (a `stopped` failure).
    stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}
impl<R: BufRead> Xz<R> {
    /// The input, past what was decoded.
    pub fn into_inner(self) -> R {
        match self.decoder {
            Some(decoder) => decoder.into_inner(),
            None => self.rest.expect("the input"),
        }
    }
    /// Stop between two streams once `flag` is set (`stopped`).
    pub fn stop(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.stop = Some(flag);
        self
    }
    fn stopped(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
    }
}
fn broken_xz(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}
impl<R: BufRead> Read for Xz<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(decoder) = self.decoder.as_mut() else {
                return Ok(0);
            };
            let n = decoder.read(buf)?;
            if n > 0 || buf.is_empty() {
                self.gave |= n > 0;
                return Ok(n);
            }
            // A stream ended: after its padding (NUL bytes), the input ends or another begins.
            let mut inner = self.decoder.take().expect("the decoder").into_inner();
            if !std::mem::take(&mut self.gave) {
                self.empty += 1;
            }
            loop {
                let ahead = match inner.fill_buf() {
                    Ok(ahead) => ahead,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        self.rest = Some(inner);
                        return Err(e);
                    }
                };
                if ahead.is_empty() {
                    self.rest = Some(inner);
                    return Ok(0);
                }
                let padding = ahead.iter().take_while(|b| **b == 0).count();
                let more = padding < ahead.len();
                inner.consume(padding);
                self.padding += padding as u64;
                if self.padding > XZ_PADDING_CAP {
                    self.rest = Some(inner);
                    return Err(broken_xz("xz: stream padding over its cap"));
                }
                if self.stopped() {
                    self.rest = Some(inner);
                    return Err(io::Error::other(Failure::new("stopped")));
                }
                if more {
                    break;
                }
            }
            // The first stream of a file may be empty; many are a file built to spin the reader.
            if self.empty > XZ_EMPTY_STREAMS {
                self.rest = Some(inner);
                return Err(broken_xz("xz: too many empty streams"));
            }
            if self.stopped() {
                self.rest = Some(inner);
                return Err(io::Error::other(Failure::new("stopped")));
            }
            let stream = match xz_stream(self.threads, self.memory) {
                Ok(stream) => stream,
                Err(e) => {
                    self.rest = Some(inner);
                    return Err(io::Error::other(e));
                }
            };
            self.decoder = Some(liblzma::bufread::XzDecoder::new_stream(inner, stream));
        }
    }
}

/// A reader that keeps the digest (SHA-256 unless named, [`Hashed::with`]) of every byte read
/// through it.
pub struct Hashed<R, D = sha2::Sha256> {
    inner: R,
    hash: D,
    bytes: u64,
}
impl<R> Hashed<R> {
    pub fn new(inner: R) -> Self {
        Self::with(inner)
    }
}
impl<R, D: Digest + Clone> Hashed<R, D> {
    /// A reader that keeps the digest `D` (`md5::Md5`, …).
    pub fn with(inner: R) -> Self {
        Self {
            inner,
            hash: D::new(),
            bytes: 0,
        }
    }
    /// The bytes read so far.
    pub fn count(&self) -> u64 {
        self.bytes
    }
    /// The lower-case hex digest of what was read.
    pub fn hex(&self) -> String {
        self.hash
            .clone()
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
impl<R: Read, D: Digest> Read for Hashed<R, D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

/// The files of a `SHA256SUMS` list (`<hex>  <name>` or `<hex> *<name>` per line): name and
/// lower-case hex digest.
pub fn sha256_sums(text: &str) -> Vec<(String, String)> {
    sums(text, 64)
}
/// The files of a checksum list (`md5sum`'s or `sha256sum`'s lines: `<hex>  <name>` or
/// `<hex> *<name>`) whose digests have `hex_len` hex digits: name and lower-case hex digest.
pub fn sums(text: &str, hex_len: usize) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (hex, name) = line.trim().split_once(char::is_whitespace)?;
            let name = name.trim_start().trim_start_matches('*');
            (hex.len() == hex_len && hex.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty())
                .then(|| (name.to_owned(), hex.to_ascii_lowercase()))
        })
        .collect()
}

/// Bytes a read must bring in its slow window of waiting for them ([`SLOW_WINDOW`] by default):
/// a server slower than that (a few bytes every two minutes) would hold the import, and the
/// large-import turn, without end.
pub const SLOW_BYTES: u64 = 8 << 20;
/// The time a download may wait for [`SLOW_BYTES`].
pub const SLOW_WINDOW: Duration = Duration::from_secs(600);
/// A read's speed floor, counting only the time spent waiting for the server's bytes (not the
/// reader's own pauses: a slow batch write is no slow server).
pub struct Speed {
    window: Duration,
    waited: Duration,
    bytes: u64,
}
impl Speed {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            waited: Duration::ZERO,
            bytes: 0,
        }
    }
    /// `n` bytes more came after waiting `waited` for them; false once the window of waiting
    /// passed without [`SLOW_BYTES`].
    pub fn read(&mut self, n: u64, waited: Duration) -> bool {
        self.waited += waited;
        self.bytes += n;
        if self.bytes >= SLOW_BYTES {
            self.waited = Duration::ZERO;
            self.bytes = 0;
            return true;
        }
        self.waited <= self.window
    }
}

/// A download read as a blocking stream on a `spawn_blocking` thread: its body in chunks, and on
/// a broken connection the rest asked for again from the byte where it broke (`Range`, with
/// `If-Range` naming the first answer's validator, so a file replaced meanwhile is an error
/// (`dump_changed`), not a spliced stream; without a validator it is not asked for again). Each
/// break may be retried [`Download::RETRIES`] times (a failed request again too), the count
/// renewed after every [`Download::RENEW_BYTES`] read; every request is tried as
/// [`fetch::Client::send`] does (a `Retry-After` waited for), every wait for a chunk is bounded,
/// a server slower than [`SLOW_BYTES`] in its slow window fails it (`download`), and so does the
/// whole download past [`Download::DEADLINE`] (`timeout`). The file's length comes from its first
/// answer: one with none is refused (a body that ends early could not be told from the whole),
/// and a body that ends before it is a break too.
///
/// A download of a file already known ([`Download::known`]: its length and validator, from a HEAD
/// or an earlier run) asks for its bytes from [`Download::from`] (to [`Download::until`]) under
/// that validator from its first request, which goes out at the first read.
pub struct Download {
    client: fetch::Client,
    url: url::Url,
    handle: tokio::runtime::Handle,
    response: Option<reqwest::Response>,
    validator: Option<String>,
    /// The file's length, from the first answer (or known).
    length: u64,
    /// Whether the file was known before the first request.
    known: bool,
    /// Where the bytes asked for end (exclusive); none: the file's end.
    end: Option<u64>,
    connected: bool,
    /// The last answer was one no retry mends (a range from another byte than asked for).
    hopeless: bool,
    /// The URL of the last answer (after the redirects it followed).
    answered_by: Option<url::Url>,
    chunk: bytes::Bytes,
    offset: u64,
    retries: u32,
    /// The offset at the last break: past it by [`Download::RENEW_BYTES`], the retries count anew.
    broke_at: u64,
    host: String,
    /// The most bytes read; a longer body is an error (`size`).
    max_bytes: u64,
    /// The first wait before a retry; it doubles with each one.
    retry_wait: Duration,
    started: std::time::Instant,
    deadline: Option<Duration>,
    speed: Speed,
    /// Set when the run stops: heard before and during every wait (a `stopped` failure).
    stop: Option<Arc<AtomicBool>>,
}
/// How often a wait listens for a stop.
const STOP_SLICE: Duration = Duration::from_millis(100);
thread_local! {
    /// The stop of the downloads made on this thread ([`stopping_with`]).
    static THREAD_STOP: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
}
/// Runs `f` with `flag` as the stop of every [`Download`] made on this thread meanwhile that
/// names none of its own ([`Download::stop`]): a reader of a run's file hears the run end.
pub fn stopping_with<T>(flag: Arc<AtomicBool>, f: impl FnOnce() -> T) -> T {
    let before = THREAD_STOP.with(|s| s.replace(Some(flag)));
    let out = f();
    THREAD_STOP.with(|s| *s.borrow_mut() = before);
    out
}
/// Sets its flag when dropped: a run's future cut off stops the downloads it runs on blocking
/// threads.
pub struct StopOnDrop(pub Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
impl Download {
    /// How many times in a row a broken body is asked for again.
    pub const RETRIES: u32 = 5;
    /// The bytes read after a break that renew the retries.
    pub const RENEW_BYTES: u64 = 64 << 20;
    /// The longest wait for an answer or the next chunk of a body.
    pub const CHUNK_WAIT: Duration = Duration::from_secs(120);
    /// The longest one download may take.
    pub const DEADLINE: Duration = Duration::from_secs(12 * 3600);
    /// Starts `url` (blocking: call it on a `spawn_blocking` thread). A status other than 200 is
    /// a failure at its host: `not_found` for a 404, else `download`.
    pub fn open(
        client: fetch::Client,
        handle: tokio::runtime::Handle,
        url: url::Url,
        max_bytes: u64,
        retry_wait: Duration,
    ) -> Result<Self> {
        let mut download = Self::new(client, handle, url, max_bytes).retry_wait(retry_wait);
        download.connect()?;
        Ok(download)
    }
    /// A download of `url` of at most `max_bytes`, asked for at its first read (or
    /// [`Download::open`]).
    pub fn new(
        client: fetch::Client,
        handle: tokio::runtime::Handle,
        url: url::Url,
        max_bytes: u64,
    ) -> Self {
        let host = url.host_str().unwrap_or_default().to_owned();
        Self {
            client,
            url,
            handle,
            response: None,
            validator: None,
            length: 0,
            known: false,
            end: None,
            connected: false,
            hopeless: false,
            answered_by: None,
            chunk: bytes::Bytes::new(),
            offset: 0,
            retries: 0,
            broke_at: 0,
            host,
            max_bytes,
            retry_wait: Duration::from_secs(2),
            started: std::time::Instant::now(),
            deadline: Some(Self::DEADLINE),
            speed: Speed::new(SLOW_WINDOW),
            stop: THREAD_STOP.with(|s| s.borrow().clone()),
        }
    }
    /// Stop once `flag` is set: before and during every wait (a request, its `Retry-After`, a
    /// chunk, the pause before a resume), within [`STOP_SLICE`]: `stopped`.
    pub fn stop(mut self, flag: Arc<AtomicBool>) -> Self {
        self.stop = Some(flag);
        self
    }
    /// Asks for the file now (blocking: call it on a `spawn_blocking` thread).
    pub fn start(mut self) -> Result<Self> {
        self.connect()?;
        Ok(self)
    }
    fn stopped(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    }
    /// `future` run to its end on the runtime, or none once the stop is set.
    fn wait_for<F: std::future::Future>(&self, future: F) -> Option<F::Output> {
        let Some(flag) = self.stop.clone() else {
            return Some(self.handle.block_on(future));
        };
        if flag.load(Ordering::Relaxed) {
            return None;
        }
        self.handle.block_on(async move {
            let stopped = async {
                while !flag.load(Ordering::Relaxed) {
                    tokio::time::sleep(STOP_SLICE).await;
                }
            };
            tokio::select! {
                out = future => Some(out),
                () = stopped => None,
            }
        })
    }
    /// Sleeps `wait` in slices; false when the stop was set meanwhile.
    fn pause(&self, wait: Duration) -> bool {
        let until = std::time::Instant::now() + wait;
        loop {
            if self.stopped() {
                return false;
            }
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(STOP_SLICE));
        }
    }
    /// Read ahead of the reader on a thread of its own, at most `depth` reads of 64 KiB waiting:
    /// the network is read while the reader works. Dropping the reader stops the download.
    pub fn read_ahead(mut self, depth: usize) -> ReadAhead {
        let flag = self
            .stop
            .get_or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone();
        let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<Vec<u8>>>(depth.max(1));
        let spawned = std::thread::Builder::new()
            .name("catalogue-read-ahead".into())
            .spawn(move || {
                let mut body = self;
                loop {
                    let mut buf = vec![0u8; 64 << 10];
                    match body.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => {
                            buf.truncate(n);
                            if tx.send(Ok(buf)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    }
                }
            });
        let failed = spawned
            .err()
            .map(|_| io::Error::other(Failure::new("read")));
        ReadAhead {
            rx,
            chunk: Vec::new(),
            at: 0,
            failed,
            _stop: StopOnDrop(flag),
        }
    }
    /// The first wait before a retry (it doubles with each one).
    pub fn retry_wait(mut self, wait: Duration) -> Self {
        self.retry_wait = wait;
        self
    }
    /// The file is known: `length` bytes under `validator` (an empty one is none: such a file
    /// is not read, as a resume could splice two).
    pub fn known(mut self, length: u64, validator: &str) -> Self {
        self.known = true;
        self.length = length;
        self.validator = Some(validator.to_owned()).filter(|v| !v.is_empty());
        self
    }
    /// Read from byte `offset` of a known file.
    pub fn from(mut self, offset: u64) -> Self {
        self.offset = offset;
        self.broke_at = offset;
        self
    }
    /// Read up to byte `end` (exclusive) of a known file.
    pub fn until(mut self, end: u64) -> Self {
        self.end = Some(end);
        self
    }
    /// The slow-server window: [`SLOW_BYTES`] must come in this much waiting for them.
    pub fn slow(mut self, window: Duration) -> Self {
        self.speed = Speed::new(window);
        self
    }
    /// No deadline for the whole download (a stream its reader paces, which has the speed floor
    /// and a cursor of its own).
    pub fn no_deadline(mut self) -> Self {
        self.deadline = None;
        self
    }
    fn failure(&self, class: &'static str) -> Failure {
        Failure::new(class).at(self.host.clone())
    }
    fn past_deadline(&self) -> bool {
        self.deadline.is_some_and(|d| self.started.elapsed() > d)
    }
    /// Where the bytes asked for end.
    fn stop_at(&self) -> u64 {
        self.end.unwrap_or(self.length).min(self.length)
    }
    fn connect(&mut self) -> Result<()> {
        self.connected = true;
        let ranged = self.offset > 0 || self.known;
        let mut headers = vec![];
        if ranged {
            let Some(validator) = &self.validator else {
                bail!(self.failure("download"));
            };
            let range = match self.end {
                Some(end) => format!("bytes={}-{}", self.offset, end.saturating_sub(1)),
                None => format!("bytes={}-", self.offset),
            };
            headers.push((reqwest::header::RANGE, range));
            headers.push((reqwest::header::IF_RANGE, validator.clone()));
        }
        let sent = self.wait_for(self.client.send_with(
            reqwest::Method::GET,
            self.url.as_str(),
            &headers,
            None,
            self.retry_wait,
        ));
        let response = match sent {
            None => bail!(Failure::new("stopped")),
            Some(Ok((response, _))) => response,
            Some(Err(error)) => {
                let class = error
                    .downcast_ref::<Failure>()
                    .map_or("download", |f| f.class);
                bail!(self.failure(class))
            }
        };
        let status = response.status().as_u16();
        match (ranged, status) {
            (false, 200) => {
                // A strong ETag, else Last-Modified, makes `If-Range` safe.
                self.validator =
                    Some(fetch::validator_of(response.headers())).filter(|v| !v.is_empty());
                let Some(length) = response.content_length() else {
                    bail!(self.failure("download"));
                };
                if length > self.max_bytes {
                    bail!(self.failure("size"));
                }
                self.length = length;
            }
            // A server that ignores ranges answers a known file whole: fine from its start.
            (true, 200) if self.offset == 0 && self.end.is_none() => {
                if response.content_length() != Some(self.length) {
                    bail!(self.failure("dump_changed"));
                }
            }
            // `If-Range` did not hold: the file was replaced since its first answer.
            (true, 200) => bail!(self.failure("dump_changed")),
            (true, 206) => {
                // `bytes <first>-<last>/<length>`: from the byte asked for, of the same file.
                let range = response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes "))
                    .and_then(|v| v.split_once('/'))
                    .and_then(|(span, length)| {
                        let first = span.split('-').next()?.parse::<u64>().ok()?;
                        Some((first, length.parse::<u64>().ok()?))
                    });
                if range != Some((self.offset, self.length)) {
                    // Another byte or another length: a server that answers so is not asked
                    // again.
                    self.hopeless = true;
                    bail!(self.failure("download"));
                }
            }
            (false, 404) => bail!(self.failure("not_found")),
            // Gone since its first answer.
            (true, 404) => bail!(self.failure("dump_gone")),
            _ => bail!(self.failure("download")),
        }
        if self.length > self.max_bytes {
            bail!(self.failure("size"));
        }
        self.answered_by = Some(response.url().clone());
        self.response = Some(response);
        Ok(())
    }
    /// The file's length, from the first answer.
    pub fn length(&self) -> u64 {
        self.length
    }
    /// The bytes read so far (the offset in the file).
    pub fn offset(&self) -> u64 {
        self.offset
    }
    /// The URL the file came from at last (after the redirects followed): none before the first
    /// answer.
    pub fn final_url(&self) -> Option<&url::Url> {
        self.answered_by.as_ref()
    }
    /// After a break: wait, and ask for the rest, as many times as the retries allow.
    fn resume(&mut self) -> io::Result<()> {
        if self.offset >= self.broke_at.saturating_add(Self::RENEW_BYTES) {
            self.retries = 0;
        }
        self.broke_at = self.offset;
        loop {
            // (Without a validator `connect` refuses to resume.)
            if self.retries >= Self::RETRIES || self.past_deadline() {
                return Err(io::Error::other(self.failure("download")));
            }
            self.retries += 1;
            if !self.pause(self.retry_wait * 2u32.pow(self.retries.min(6) - 1)) {
                return Err(io::Error::other(Failure::new("stopped")));
            }
            match self.connect() {
                Ok(()) => return Ok(()),
                Err(_) if self.hopeless => return Err(io::Error::other(self.failure("download"))),
                Err(e) => {
                    // A file gone or replaced, a host that asks to be left alone, a file never
                    // there: not retried.
                    let class = e.downcast_ref::<Failure>().map(|f| f.class);
                    if let Some(
                        class @ ("not_found" | "dump_gone" | "dump_changed" | "rate_limited"),
                    ) = class
                    {
                        return Err(io::Error::other(self.failure(class)));
                    }
                }
            }
        }
    }
}
impl Read for Download {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // A known file read from its end (a range already read whole): nothing to ask.
        if !self.connected && self.known && self.offset >= self.stop_at() {
            return Ok(0);
        }
        if !self.connected {
            self.connect().map_err(|e| match e.downcast::<Failure>() {
                Ok(f) => io::Error::other(f),
                Err(_) => io::Error::other(self.failure("download")),
            })?;
        }
        loop {
            if !self.chunk.is_empty() {
                let n = buf.len().min(self.chunk.len());
                buf[..n].copy_from_slice(&self.chunk[..n]);
                self.chunk = self.chunk.slice(n..);
                self.offset += n as u64;
                if self.offset > self.max_bytes {
                    return Err(io::Error::other(self.failure("size")));
                }
                return Ok(n);
            }
            if self.past_deadline() {
                return Err(io::Error::other(self.failure("timeout")));
            }
            if self.offset >= self.stop_at() {
                self.response = None;
                return Ok(0);
            }
            let Some(response) = self.response.as_mut() else {
                return Ok(0);
            };
            let asked = std::time::Instant::now();
            let handle = self.handle.clone();
            let next = match self.stop.clone() {
                None => handle.block_on(async {
                    tokio::time::timeout(Self::CHUNK_WAIT, response.chunk()).await
                }),
                Some(flag) => {
                    let waited = handle.block_on(async {
                        let stopped = async {
                            while !flag.load(Ordering::Relaxed) {
                                tokio::time::sleep(STOP_SLICE).await;
                            }
                        };
                        tokio::select! {
                            out = tokio::time::timeout(Self::CHUNK_WAIT, response.chunk()) => Some(out),
                            () = stopped => None,
                        }
                    });
                    match waited {
                        Some(next) => next,
                        None => {
                            self.response = None;
                            return Err(io::Error::other(Failure::new("stopped")));
                        }
                    }
                }
            };
            match next {
                Ok(Ok(Some(chunk))) => {
                    if !self.speed.read(chunk.len() as u64, asked.elapsed()) {
                        // Too slow to ever end.
                        self.response = None;
                        return Err(io::Error::other(self.failure("download")));
                    }
                    // Never past the bytes asked for.
                    let room = usize::try_from(self.stop_at() - self.offset).unwrap_or(usize::MAX);
                    self.chunk = chunk.slice(..chunk.len().min(room));
                }
                // A body that ended before the bytes asked for did (an answer of part of the
                // rest, or a connection closed early), a broken connection or a stall: ask for
                // the rest.
                _ => {
                    self.response = None;
                    self.resume()?;
                }
            }
        }
    }
}

/// [`Download::read_ahead`]'s reader: the reads the download's thread made, in order.
pub struct ReadAhead {
    rx: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    at: usize,
    /// The thread could not start.
    failed: Option<io::Error>,
    _stop: StopOnDrop,
}
impl Read for ReadAhead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        while self.at >= self.chunk.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len() - self.at);
        buf[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read keeps going while it brings 8 MB in every ten minutes of waiting for the server;
    /// slower, it ends.
    #[test]
    fn a_read_slower_than_its_floor_ends() {
        let secs = Duration::from_secs;
        let mut speed = Speed::new(SLOW_WINDOW);
        assert!(speed.read(SLOW_BYTES - 1, secs(300)));
        // 8 MB in: the window starts again.
        assert!(speed.read(1, secs(290)));
        assert!(speed.read(1 << 20, SLOW_WINDOW));
        assert!(!speed.read(1, secs(1)));
    }

    /// A tar header for `path` of `size` bytes, of type `kind`, with its checksum.
    pub(crate) fn header(path: &str, size: u64, kind: u8, base256: bool) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..path.len()].copy_from_slice(path.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        if base256 {
            h[124] = 0x80;
            h[128..136].copy_from_slice(&size.to_be_bytes());
        } else {
            h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        }
        h[156] = kind;
        h[257..265].copy_from_slice(b"ustar  \0");
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h
    }
    pub(crate) fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = vec![];
        for (path, data) in entries {
            out.extend_from_slice(&header(path, data.len() as u64, b'0', false));
            out.extend_from_slice(data);
            out.resize(out.len() + pad(data.len() as u64) as usize, 0);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    /// Concatenated streams (with padding between them) read as one, as `xz` reads them; what
    /// follows the last is left in the input; bytes that are no stream are an error.
    #[test]
    fn concatenated_xz_streams_read_as_one() {
        let packed = |data: &[u8]| {
            let mut out = vec![];
            liblzma::read::XzEncoder::new(data, 1)
                .read_to_end(&mut out)
                .unwrap();
            out
        };
        let (first, second) = (vec![b'a'; 100_000], vec![b'b'; 50_000]);
        let mut file = packed(&first);
        file.extend_from_slice(&[0; 8]);
        file.extend(packed(&second));
        file.extend_from_slice(&[0; 4]);
        let mut decoded = xz(&file[..], 2, 64 << 20).unwrap();
        let mut out = vec![];
        decoded.read_to_end(&mut out).unwrap();
        assert_eq!(out, [first.clone(), second].concat());
        assert!(decoded.into_inner().is_empty());
        let mut bad = packed(&first);
        bad.extend_from_slice(b"no stream here");
        let mut out = vec![];
        assert!(xz(&bad[..], 2, 64 << 20)
            .unwrap()
            .read_to_end(&mut out)
            .is_err());
    }

    /// A file built to spin the reader (streams that decode to nothing, endless padding) is
    /// broken (`InvalidData`, the caller's `archive`), its input kept; a run that stops stops
    /// between streams.
    #[test]
    fn empty_xz_streams_and_padding_are_capped_and_a_stop_is_heard() {
        let packed = |data: &[u8]| {
            let mut out = vec![];
            liblzma::read::XzEncoder::new(data, 1)
                .read_to_end(&mut out)
                .unwrap();
            out
        };
        let read = |file: &[u8]| {
            let mut decoded = xz(file, 1, 64 << 20).unwrap();
            let mut out = vec![];
            let got = decoded.read_to_end(&mut out);
            // The input is there after a failure too.
            let _ = decoded.into_inner();
            got.map(|_| out)
        };
        let empty = packed(b"");
        // A few empty streams among others read.
        let mut file = [empty.clone(), empty.clone(), packed(b"abc"), empty.clone()].concat();
        assert_eq!(read(&file).unwrap(), b"abc");
        // Streams that gave bytes over several reads are no empty ones.
        let many: Vec<u8> = (0..XZ_EMPTY_STREAMS * 2)
            .flat_map(|_| packed(&[b'x'; 100]))
            .collect();
        assert_eq!(read(&many).unwrap().len(), XZ_EMPTY_STREAMS as usize * 200);
        for _ in 0..XZ_EMPTY_STREAMS {
            file.extend_from_slice(&empty);
        }
        let failed = read(&file).unwrap_err();
        assert_eq!(failed.kind(), io::ErrorKind::InvalidData, "{failed}");
        // Padding: a little is read, more than the cap is broken.
        let mut padded = [packed(b"a"), vec![0; 4096], packed(b"b")].concat();
        assert_eq!(read(&padded).unwrap(), b"ab");
        padded = [
            packed(b"a"),
            vec![0; XZ_PADDING_CAP as usize + 4],
            packed(b"b"),
        ]
        .concat();
        let failed = read(&padded).unwrap_err();
        assert_eq!(failed.kind(), io::ErrorKind::InvalidData, "{failed}");
        // A stop between two streams.
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let two = [packed(b"a"), packed(b"b")].concat();
        let mut decoded = xz(&two[..], 1, 64 << 20).unwrap().stop(flag);
        let mut out = vec![];
        let failed = decoded.read_to_end(&mut out).unwrap_err();
        let failure = failed.get_ref().and_then(|e| e.downcast_ref::<Failure>());
        assert_eq!(failure.map(|f| f.class), Some("stopped"));
        assert_eq!(out, b"a");
    }

    #[test]
    fn a_tar_archive_gives_its_files_in_order() {
        let bytes = archive(&[
            ("TIMESTAMP", b"2026-11-25 01:17:36.116289+00\n"),
            ("mbdump/artist", b"{\"a\":1}\n{\"b\":2}\n"),
        ]);
        let mut tar = Tar::new(&bytes[..]);
        let first = tar.next_entry().unwrap().unwrap();
        assert_eq!(first.path, "TIMESTAMP");
        // Not read: skipped.
        let second = tar.next_entry().unwrap().unwrap();
        assert_eq!(second.path, "mbdump/artist");
        let mut data = String::new();
        tar.data().read_to_string(&mut data).unwrap();
        assert_eq!(data, "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(tar.next_entry().unwrap(), None);
    }

    #[test]
    fn gnu_and_pax_headers_name_and_size_an_entry() {
        let data = b"abc";
        let mut bytes = vec![];
        // A size in base-256, as the dump's files over 8 GiB have.
        bytes.extend_from_slice(&header("big", 3, b'0', true));
        bytes.extend_from_slice(data);
        bytes.resize(bytes.len() + 509, 0);
        // A GNU long name.
        let long = "mbdump/".to_owned() + &"x".repeat(150);
        bytes.extend_from_slice(&header("././@LongLink", long.len() as u64 + 1, b'L', false));
        bytes.extend_from_slice(long.as_bytes());
        bytes.push(0);
        bytes.resize(bytes.len().div_ceil(512) * 512, 0);
        bytes.extend_from_slice(&header("short", 3, b'0', false));
        bytes.extend_from_slice(data);
        bytes.resize(bytes.len() + 509, 0);
        // A pax path and size.
        let pax = "21 path=pax/the-file\n";
        bytes.extend_from_slice(&header("PaxHeader", pax.len() as u64, b'x', false));
        bytes.extend_from_slice(pax.as_bytes());
        bytes.resize(bytes.len().div_ceil(512) * 512, 0);
        bytes.extend_from_slice(&header("ignored", 3, b'0', false));
        bytes.extend_from_slice(data);
        bytes.resize(bytes.len() + 509, 0);
        // A directory is skipped.
        bytes.extend_from_slice(&header("dir/", 0, b'5', false));
        bytes.resize(bytes.len() + 1024, 0);
        let mut tar = Tar::new(&bytes[..]);
        let names: Vec<(String, u64)> = std::iter::from_fn(|| tar.next_entry().unwrap())
            .map(|e| (e.path, e.size))
            .collect();
        assert_eq!(
            names,
            [
                ("big".to_owned(), 3),
                (long, 3),
                ("pax/the-file".to_owned(), 3)
            ]
        );
    }

    #[test]
    fn a_broken_archive_is_an_error_never_a_panic() {
        let good = archive(&[("a", b"0123456789")]);
        // A changed byte in the header: the checksum does not match.
        let mut bad = good.clone();
        bad[3] ^= 1;
        assert!(Tar::new(&bad[..]).next_entry().is_err());
        // Cut inside the header, inside the data.
        assert!(Tar::new(&good[..100]).next_entry().is_err());
        let mut tar = Tar::new(&good[..515]);
        tar.next_entry().unwrap().unwrap();
        let mut sink = vec![];
        assert!(tar.data().read_to_end(&mut sink).is_err());
        // A pax header that claims to be huge.
        let mut huge = header("PaxHeader", HEADER_TEXT_CAP + 1, b'x', false).to_vec();
        huge.resize(4096, 0);
        assert!(Tar::new(&huge[..]).next_entry().is_err());
        // An empty stream is an empty archive.
        assert_eq!(Tar::new(&b""[..]).next_entry().unwrap(), None);
    }

    /// Fuzz: random archives (headers with random fields, random cuts and flips) never panic.
    #[test]
    fn random_archives_never_panic() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let base = archive(&[("TIMESTAMP", b"x\n"), ("mbdump/release", &[b'{'; 700])]);
        for _ in 0..3000 {
            let mut bytes = base.clone();
            for _ in 0..(next() % 8) {
                let at = (next() % bytes.len() as u64) as usize;
                bytes[at] = next() as u8;
            }
            let cut = (next() % (bytes.len() as u64 + 1)) as usize;
            bytes.truncate(cut);
            let mut tar = Tar::new(&bytes[..]);
            for _ in 0..8 {
                match tar.next_entry() {
                    Ok(Some(_)) => {
                        let mut sink = vec![];
                        let _ = tar.data().take(10_000).read_to_end(&mut sink);
                    }
                    _ => break,
                }
            }
        }
    }

    #[test]
    fn lines_are_capped_and_a_long_one_is_skipped_whole() {
        let text = b"short\r\n0123456789abcdef\nok\n\nlast";
        let mut lines = Lines::new(io::BufReader::with_capacity(4, &text[..]), 8);
        let mut line = vec![];
        let mut got = vec![];
        while lines.next_line(&mut line).unwrap() {
            got.push(String::from_utf8(line.clone()).unwrap());
        }
        assert_eq!(got, ["short", "ok", "", "last"]);
        assert_eq!(lines.too_long(), 1);
        // A long last line without a break.
        let mut lines = Lines::new(&b"a\n0123456789"[..], 4);
        assert!(lines.next_line(&mut line).unwrap());
        assert!(!lines.next_line(&mut line).unwrap());
        assert_eq!(lines.too_long(), 1);
    }

    #[test]
    fn sha256_sums_list_their_files() {
        let text = "db5dd1eb1b3170bbb8aecad7415575df241f1b898ac5cd1222c84ddc4f2169a7 *area.tar.xz\n\
                    BFCC53DCFD8E472F596164782B5B309C04EBF544965CA9198D24D2246FDECF2A  artist.tar.xz\n\
                    nonsense\n";
        assert_eq!(
            sha256_sums(text),
            [
                (
                    "area.tar.xz".to_owned(),
                    "db5dd1eb1b3170bbb8aecad7415575df241f1b898ac5cd1222c84ddc4f2169a7".to_owned()
                ),
                (
                    "artist.tar.xz".to_owned(),
                    "bfcc53dcfd8e472f596164782b5b309c04ebf544965ca9198d24d2246fdecf2a".to_owned()
                )
            ]
        );
        let mut hashed = Hashed::new(&b"abc"[..]);
        io::copy(&mut hashed, &mut io::sink()).unwrap();
        assert_eq!(
            hashed.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hashed.count(), 3);
    }
}
