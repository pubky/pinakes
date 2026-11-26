//! Reading a catalogue's bulk files as streams: a tar archive's entries ([`Tar`]), a file's lines
//! within a size cap ([`Lines`]), an `.xz` stream ([`xz`]), a download that picks up again where a
//! broken connection left it ([`Download`]), and the SHA-256 of what was read ([`Hashed`]).
//!
//! Nothing here holds more than one line (at most [`Lines::cap`] bytes) or one tar header in
//! memory, and nothing trusts a size the file states beyond what it reads: a malformed archive is
//! an error, never a panic.
use super::failure::Failure;
use anyhow::{bail, Result};
use sha2::Digest;
use std::io::{self, BufRead, Read};

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

/// An `.xz` stream decoded on `threads` threads (liblzma's multi-threaded decoder, which decodes
/// a stream of several blocks in parallel and one of a single block on one thread), within
/// `memory` bytes of decoder memory. One stream: what follows its end is left in `inner`
/// (`into_inner`).
pub fn xz<R: BufRead>(
    inner: R,
    threads: u32,
    memory: u64,
) -> Result<liblzma::bufread::XzDecoder<R>> {
    let stream = liblzma::stream::MtStreamBuilder::new()
        .threads(threads.max(1))
        .memlimit_threading(memory)
        .memlimit_stop(memory)
        .decoder()
        .map_err(|_| Failure::new("xz"))?;
    Ok(liblzma::bufread::XzDecoder::new_stream(inner, stream))
}

/// A reader that keeps the SHA-256 of every byte read through it.
pub struct Hashed<R> {
    inner: R,
    hash: sha2::Sha256,
    bytes: u64,
}
impl<R> Hashed<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hash: sha2::Sha256::new(),
            bytes: 0,
        }
    }
    /// The bytes read so far.
    pub fn count(&self) -> u64 {
        self.bytes
    }
    /// The lower-case hex SHA-256 of what was read.
    pub fn hex(&self) -> String {
        self.hash
            .clone()
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
impl<R: Read> Read for Hashed<R> {
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
    text.lines()
        .filter_map(|line| {
            let (hex, name) = line.trim().split_once(char::is_whitespace)?;
            let name = name.trim_start().trim_start_matches('*');
            (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty())
                .then(|| (name.to_owned(), hex.to_ascii_lowercase()))
        })
        .collect()
}

/// A download read as a blocking stream on a `spawn_blocking` thread: its body in chunks, and on
/// a broken connection the rest asked for again from the byte where it broke (`Range`, with
/// `If-Range` naming the first answer's validator, so a file replaced meanwhile is an error, not
/// a spliced stream; without a validator it is not asked for again). Each break may be retried
/// [`Download::RETRIES`] times (a failed request again too), the count renewed after every
/// [`Download::RENEW_BYTES`] read; every wait for a chunk is bounded, and so is the whole
/// download ([`Download::DEADLINE`]).
pub struct Download {
    client: reqwest::Client,
    url: url::Url,
    handle: tokio::runtime::Handle,
    response: Option<reqwest::Response>,
    validator: Option<String>,
    chunk: bytes::Bytes,
    offset: u64,
    retries: u32,
    /// The offset at the last break: past it by [`Download::RENEW_BYTES`], the retries count anew.
    broke_at: u64,
    host: String,
    /// The most bytes read; a longer body is an error (`size`).
    max_bytes: u64,
    /// The first wait before a retry; it doubles with each one.
    retry_wait: std::time::Duration,
    started: std::time::Instant,
}
impl Download {
    /// How many times in a row a broken body is asked for again.
    pub const RETRIES: u32 = 5;
    /// The bytes read after a break that renew the retries.
    pub const RENEW_BYTES: u64 = 64 << 20;
    /// The longest wait for an answer or the next chunk of a body.
    pub const CHUNK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
    /// The longest one download may take.
    pub const DEADLINE: std::time::Duration = std::time::Duration::from_secs(12 * 3600);
    /// Starts `url` (blocking: call it on a `spawn_blocking` thread). A status other than 200 is
    /// a failure at its host: `not_found` for a 404, else `download`.
    pub fn open(
        client: reqwest::Client,
        handle: tokio::runtime::Handle,
        url: url::Url,
        max_bytes: u64,
        retry_wait: std::time::Duration,
    ) -> Result<Self> {
        let host = url.host_str().unwrap_or_default().to_owned();
        let mut download = Self {
            client,
            url,
            handle,
            response: None,
            validator: None,
            chunk: bytes::Bytes::new(),
            offset: 0,
            retries: 0,
            broke_at: 0,
            host,
            max_bytes,
            retry_wait,
            started: std::time::Instant::now(),
        };
        download.connect()?;
        Ok(download)
    }
    fn failure(&self, class: &'static str) -> Failure {
        Failure::new(class).at(self.host.clone())
    }
    fn connect(&mut self) -> Result<()> {
        let mut request = self.client.get(self.url.clone());
        if self.offset > 0 {
            let Some(validator) = &self.validator else {
                bail!(self.failure("download"));
            };
            request = request
                .header(reqwest::header::RANGE, format!("bytes={}-", self.offset))
                .header(reqwest::header::IF_RANGE, validator.clone());
        }
        let sent = self
            .handle
            .block_on(async { tokio::time::timeout(Self::CHUNK_WAIT, request.send()).await });
        let response = match sent {
            Ok(Ok(response)) => response,
            _ => bail!(self.failure("download")),
        };
        let status = response.status().as_u16();
        let resumed = self.offset > 0;
        match (resumed, status) {
            (false, 200) => {
                let header = |name| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned)
                };
                // A strong ETag, else Last-Modified, makes `If-Range` safe.
                self.validator = header(reqwest::header::ETAG)
                    .filter(|e| !e.starts_with("W/"))
                    .or_else(|| header(reqwest::header::LAST_MODIFIED));
                if response
                    .content_length()
                    .is_some_and(|n| n > self.max_bytes)
                {
                    bail!(self.failure("size"));
                }
            }
            (true, 206) => {
                let starts = response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes "))
                    .and_then(|v| v.split('-').next())
                    .and_then(|v| v.parse::<u64>().ok());
                if starts != Some(self.offset) {
                    bail!(self.failure("download"));
                }
            }
            // The file changed since the first answer (a 200 to `If-Range`), or anything else.
            (_, 404) => bail!(self.failure("not_found")),
            _ => bail!(self.failure("download")),
        }
        self.response = Some(response);
        Ok(())
    }
    /// The bytes read so far.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    /// After a break: wait, and ask for the rest, as many times as the retries allow.
    fn resume(&mut self) -> io::Result<()> {
        if self.offset >= self.broke_at.saturating_add(Self::RENEW_BYTES) {
            self.retries = 0;
        }
        self.broke_at = self.offset;
        loop {
            if self.retries >= Self::RETRIES
                || self.validator.is_none()
                || self.started.elapsed() > Self::DEADLINE
            {
                return Err(io::Error::other(self.failure("download")));
            }
            self.retries += 1;
            std::thread::sleep(self.retry_wait * 2u32.pow(self.retries.min(6) - 1));
            match self.connect() {
                Ok(()) => return Ok(()),
                Err(e) => {
                    // A file gone or replaced is not retried.
                    let class = e.downcast_ref::<Failure>().map(|f| f.class);
                    if class == Some("not_found") {
                        return Err(io::Error::other(self.failure("not_found")));
                    }
                }
            }
        }
    }
}
impl Read for Download {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
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
            if self.started.elapsed() > Self::DEADLINE {
                return Err(io::Error::other(self.failure("timeout")));
            }
            let Some(response) = self.response.as_mut() else {
                return Ok(0);
            };
            let next = self
                .handle
                .block_on(async { tokio::time::timeout(Self::CHUNK_WAIT, response.chunk()).await });
            match next {
                Ok(Ok(Some(chunk))) => self.chunk = chunk,
                Ok(Ok(None)) => {
                    self.response = None;
                    return Ok(0);
                }
                // A broken connection or a stall: ask for the rest.
                _ => {
                    self.response = None;
                    self.resume()?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
