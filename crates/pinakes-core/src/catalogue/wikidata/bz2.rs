//! A bzip2 stream decoded on several threads, in order.
//!
//! Wikidata's dump is one bzip2 stream (one header, about two million blocks of up to 900 kB of
//! text each), so it can't be cut at byte boundaries as a multi-stream file can. A block starts
//! with a 48-bit magic number at any bit offset and carries its own CRC, so [`Splitter`] finds
//! the blocks by their magic, and each block is decoded alone as a one-block stream (the
//! stream's CRC of one block is the block's own CRC). The magic can also occur inside compressed
//! data (about once in 2^48 bit offsets, a few times in a thousand full dumps): the two halves
//! of such a block fail to decode, and the block is decoded again joined to the next one
//! ([`Decoded`]).
//!
//! Every position is a block's first bit ([`Segment::start_bit`]), so a read can start again at
//! any block: [`Splitter::resume`].

/// The magic number each block starts with (the BCD digits of pi).
pub const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
/// The magic number that ends a stream (the BCD digits of the square root of pi).
pub const EOS_MAGIC: u64 = 0x1772_4538_5090;
const MASK48: u64 = (1 << 48) - 1;
/// The most compressed bytes one block may hold. A 900 kB block compresses to well under this;
/// more means the input is not a bzip2 stream.
pub const SEGMENT_CAP: usize = 4 << 20;
/// The most text one block (or a block joined to its next) may decode to. A block holds at most
/// 900 kB after bzip2's first run-length step, which can expand it up to 51 times.
pub const DECODED_CAP: usize = 64 << 20;
/// The most segments one block may be joined from (a false magic number splits it in two).
const JOIN_CAP: usize = 4;
/// The most decoded blocks waiting for an earlier one: more means a block was lost.
pub const WAITING_CAP: usize = 4096;

/// For two bytes (`hi << 8 | lo`): whether a magic number can end in `lo`, its earlier bits in
/// `hi` and before.
static ENDS: std::sync::LazyLock<Vec<bool>> = std::sync::LazyLock::new(|| {
    (0..1usize << 16)
        .map(|pair| {
            (0..8).any(|e| {
                // A candidate whose last bit is bit `e` (from the top) of `lo`: the pair's top
                // `9 + e` bits are the magic's last `9 + e` bits.
                let n = 9 + e;
                let top = (pair >> (16 - n)) as u64;
                [BLOCK_MAGIC, EOS_MAGIC]
                    .iter()
                    .any(|m| top == m & ((1 << n) - 1))
            })
        })
        .collect()
});

/// One block's bits, copied so that its first bit (the magic number) is the first bit of
/// `bits`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The block's number in this read, from 0.
    pub index: u64,
    /// The absolute bit offset of the block's first bit in the file.
    pub start_bit: u64,
    pub bits: Vec<u8>,
    pub nbits: u64,
}

/// Why a stream could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bz2Error {
    /// Not a bzip2 stream at its start.
    Header,
    /// A block longer than [`SEGMENT_CAP`]: not a bzip2 stream.
    Oversized,
    /// A block that does not decode, alone or joined to the next ones.
    Block,
    /// The input ended inside a block (a file cut short).
    Truncated,
}
impl std::fmt::Display for Bz2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Bz2Error::Header => "bz2_header",
            Bz2Error::Oversized => "bz2_oversized",
            Bz2Error::Block => "bz2_block",
            Bz2Error::Truncated => "bz2_truncated",
        })
    }
}
impl std::error::Error for Bz2Error {}

/// Finds the blocks of a bzip2 stream in its bytes as they come ([`Splitter::push`]).
#[derive(Debug)]
pub struct Splitter {
    /// Bytes not yet given out in a segment, from the byte holding `buf_bit`.
    buf: Vec<u8>,
    /// The absolute bit offset of `buf[0]`'s first bit (a multiple of 8).
    buf_bit: u64,
    /// The absolute offset of the next bit to look at for a magic number (its last bit).
    scan_bit: u64,
    /// The first bit of the block under way.
    block: Option<u64>,
    /// Whether the stream's header still has to be read (a read from the file's start).
    header: bool,
    /// The level digit of the stream's header (`9` when the read starts mid-stream).
    pub level: u8,
    next_index: u64,
    /// A stream end seen at this bit: decided once the bytes after it are here.
    end: Option<u64>,
}
impl Splitter {
    /// A read from the stream's first byte.
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            buf_bit: 0,
            scan_bit: 0,
            block: None,
            header: true,
            level: b'9',
            next_index: 0,
            end: None,
        }
    }
    /// A read from the block that starts at `start_bit`: the bytes given to [`Splitter::push`]
    /// start at byte `start_bit / 8`.
    pub fn resume(start_bit: u64) -> Self {
        Self {
            buf: Vec::new(),
            buf_bit: start_bit / 8 * 8,
            // The next candidate starts after the block's own magic number.
            scan_bit: start_bit + 96,
            block: Some(start_bit),
            header: false,
            level: b'9',
            next_index: 0,
            end: None,
        }
    }
    /// The byte to start reading the file at.
    pub fn first_byte(&self) -> u64 {
        self.buf_bit / 8
    }
    fn bit_at(&self, abs: u64) -> u8 {
        let rel = (abs - self.buf_bit) as usize;
        (self.buf[rel / 8] >> (7 - rel % 8)) & 1
    }
    /// The 48 bits that start at absolute bit `abs`; the caller checks they are in the buffer.
    fn bits48(&self, abs: u64) -> u64 {
        let rel = abs - self.buf_bit;
        let byte = (rel / 8) as usize;
        let shift = rel % 8;
        let mut v: u64 = 0;
        for i in 0..7 {
            v = (v << 8) | u64::from(*self.buf.get(byte + i).unwrap_or(&0));
        }
        // 56 bits read from `byte`; the 48 wanted start `shift` bits in.
        (v >> (8 - shift)) & MASK48
    }
    fn end_bit(&self) -> u64 {
        self.buf_bit + self.buf.len() as u64 * 8
    }
    /// Take in more bytes; the segments of the blocks they complete are pushed to `out`.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<Segment>) -> Result<(), Bz2Error> {
        self.buf.extend_from_slice(bytes);
        self.scan(false, out)
    }
    /// The input has ended: the last block is complete only when a stream end follows it.
    pub fn finish(&mut self, out: &mut Vec<Segment>) -> Result<(), Bz2Error> {
        self.scan(true, out)?;
        match self.block {
            Some(_) => Err(Bz2Error::Truncated),
            None => Ok(()),
        }
    }
    fn scan(&mut self, last: bool, out: &mut Vec<Segment>) -> Result<(), Bz2Error> {
        if self.header {
            // A stream starts with its header and then a block, or the stream end of an empty
            // stream; anything else (nothing at all included) is not a bzip2 stream.
            if self.buf.len() < 10 {
                return if last { Err(Bz2Error::Header) } else { Ok(()) };
            }
            if &self.buf[..3] != b"BZh" || !(b'1'..=b'9').contains(&self.buf[3]) {
                return Err(Bz2Error::Header);
            }
            let first = self.bits48(32);
            if first != BLOCK_MAGIC && first != EOS_MAGIC {
                return Err(Bz2Error::Header);
            }
            self.level = self.buf[3];
            self.header = false;
            self.scan_bit = 32 + 48;
        }
        loop {
            if let Some(end) = self.end {
                // A stream end is real when its CRC and padding are followed by the input's end
                // or a new stream's header and first block (a multi-stream file).
                let after = (end + 48 + 32).div_ceil(8) * 8;
                let need = after + 32 + 48;
                if self.end_bit() < need && !last {
                    return Ok(());
                }
                let real = if self.end_bit() <= after {
                    true
                } else if self.end_bit() >= need {
                    let at = ((after - self.buf_bit) / 8) as usize;
                    &self.buf[at..at + 3] == b"BZh"
                        && (b'1'..=b'9').contains(&self.buf[at + 3])
                        && self.bits48(after + 32) == BLOCK_MAGIC
                } else {
                    false
                };
                self.end = None;
                if real {
                    if let Some(start) = self.block.take() {
                        self.emit(start, end, out)?;
                    }
                    if self.end_bit() > after {
                        self.block = Some(after + 32);
                        self.scan_bit = after + 32 + 48 + 48;
                    } else {
                        self.scan_bit = after;
                    }
                    self.trim();
                    continue;
                }
                // A false stream end inside compressed data: look on after it.
                self.scan_bit = end + 49;
            }
            // `scan_bit` is the end (exclusive) of the 48-bit candidate under test.
            while self.scan_bit <= self.end_bit() {
                if let Some(block) = self.block {
                    if self.scan_bit - block > SEGMENT_CAP as u64 * 8 + 96 {
                        return Err(Bz2Error::Oversized);
                    }
                }
                // The eight candidates that end in one byte are skipped together when the byte
                // and the one before it can't hold the end of a magic number.
                if (self.scan_bit - 1).is_multiple_of(8) && self.scan_bit + 7 <= self.end_bit() {
                    let byte = ((self.scan_bit - 1 - self.buf_bit) / 8) as usize;
                    if byte >= 1 {
                        let pair =
                            (usize::from(self.buf[byte - 1]) << 8) | usize::from(self.buf[byte]);
                        if !ENDS[pair] {
                            self.scan_bit += 8;
                            continue;
                        }
                    }
                }
                let start = self.scan_bit - 48;
                let v = self.bits48(start);
                if v == BLOCK_MAGIC {
                    if let Some(block) = self.block.replace(start) {
                        self.emit(block, start, out)?;
                    }
                    self.scan_bit += 48;
                } else if v == EOS_MAGIC && self.block.is_some() {
                    self.end = Some(start);
                    break;
                } else {
                    self.scan_bit += 1;
                }
            }
            if self.end.is_none() {
                self.trim();
                return Ok(());
            }
        }
    }
    /// Drop the bytes before the block under way (or before the scan, between blocks).
    fn trim(&mut self) {
        let keep_from = self.block.unwrap_or(self.scan_bit.saturating_sub(48));
        let drop = ((keep_from.saturating_sub(self.buf_bit)) / 8) as usize;
        let drop = drop.min(self.buf.len());
        if drop > 0 {
            self.buf.drain(..drop);
            self.buf_bit += drop as u64 * 8;
        }
    }
    fn emit(&mut self, start: u64, end: u64, out: &mut Vec<Segment>) -> Result<(), Bz2Error> {
        let nbits = end - start;
        if nbits > SEGMENT_CAP as u64 * 8 {
            return Err(Bz2Error::Oversized);
        }
        let mut w = BitWriter::default();
        let rel = start - self.buf_bit;
        let first = (rel / 8) as usize;
        let shift = (rel % 8) as u32;
        let whole = nbits / 8;
        // Byte by byte, then the last bits.
        for i in 0..whole as usize {
            let hi = u16::from(self.buf[first + i]);
            let lo = u16::from(*self.buf.get(first + i + 1).unwrap_or(&0));
            w.push_byte((((hi << 8) | lo) >> (8 - shift)) as u8);
        }
        for b in start + whole * 8..end {
            w.push_bit(self.bit_at(b));
        }
        out.push(Segment {
            index: self.next_index,
            start_bit: start,
            bits: w.bytes,
            nbits,
        });
        self.next_index += 1;
        Ok(())
    }
}
impl Default for Splitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Bits written most significant first.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte (0 = a new byte starts).
    used: u32,
}
impl BitWriter {
    fn push_bit(&mut self, bit: u8) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        let last = self.bytes.last_mut().expect("a byte");
        *last |= (bit & 1) << (7 - self.used);
        self.used = (self.used + 1) % 8;
    }
    fn push_byte(&mut self, byte: u8) {
        if self.used == 0 {
            self.bytes.push(byte);
        } else {
            for i in (0..8).rev() {
                self.push_bit((byte >> i) & 1);
            }
        }
    }
    fn push_bits(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            self.push_bit(((value >> i) & 1) as u8);
        }
    }
    fn push_segment(&mut self, s: &Segment) {
        let whole = (s.nbits / 8) as usize;
        for b in &s.bits[..whole] {
            self.push_byte(*b);
        }
        for i in 0..(s.nbits % 8) as u32 {
            self.push_bit((s.bits[whole] >> (7 - i)) & 1);
        }
    }
}

/// The block CRC a segment carries (the 32 bits after its magic number).
fn block_crc(s: &Segment) -> u32 {
    let mut v: u64 = 0;
    for i in 0..11 {
        v = (v << 8) | u64::from(*s.bits.get(i).unwrap_or(&0));
    }
    // Bits 48..80 of the 88 read.
    ((v >> 8) & 0xffff_ffff) as u32
}

/// Decode `parts` (one block, or a block a false magic number split, joined again) as a
/// one-block stream of header level `level`.
pub fn decode(level: u8, parts: &[&Segment]) -> Result<Vec<u8>, Bz2Error> {
    let first = parts.first().ok_or(Bz2Error::Block)?;
    if first.nbits < 80 {
        return Err(Bz2Error::Block);
    }
    let mut w = BitWriter::default();
    w.bytes.extend_from_slice(&[b'B', b'Z', b'h', level]);
    for part in parts {
        w.push_segment(part);
    }
    w.push_bits(EOS_MAGIC, 48);
    w.push_bits(u64::from(block_crc(first)), 32);
    let input = w.bytes;
    let mut d = bzip2::Decompress::new(false);
    let mut out: Vec<u8> = Vec::with_capacity(1 << 20);
    loop {
        if out.len() == out.capacity() {
            if out.len() >= DECODED_CAP {
                return Err(Bz2Error::Block);
            }
            out.reserve((out.len()).min(DECODED_CAP - out.len()).max(1 << 20));
        }
        let read = d.total_in() as usize;
        let before = d.total_out();
        let status = d
            .decompress_vec(&input[read.min(input.len())..], &mut out)
            .map_err(|_| Bz2Error::Block)?;
        match status {
            bzip2::Status::StreamEnd => return Ok(out),
            _ if d.total_in() as usize >= input.len()
                && d.total_out() == before
                && out.len() < out.capacity() =>
            {
                // All input taken, nothing more out, and no stream end: a block cut short.
                return Err(Bz2Error::Block);
            }
            _ => {}
        }
    }
}

/// A decoded block in order: its first bit and its text.
#[derive(Debug)]
pub struct Decoded {
    pub start_bit: u64,
    pub text: Vec<u8>,
    /// The decoded bytes the decoders counted for the segments it stands for (a joined block's
    /// own text was never counted; the halves that decoded alone were), for the pipeline's budget.
    pub counted: usize,
}

/// Puts decoded blocks back in order, and decodes again joined to the next ones a block that
/// failed alone (a false magic number split it).
pub struct Orderer {
    level: u8,
    next: u64,
    waiting: std::collections::BTreeMap<u64, (Segment, Result<Vec<u8>, Bz2Error>)>,
    /// The segments joined so far into the failed block at `next`, and the bytes the ones that
    /// decoded alone counted.
    joined: Vec<Segment>,
    joined_counted: usize,
}
impl Orderer {
    pub fn new(level: u8) -> Self {
        Self {
            level,
            next: 0,
            waiting: Default::default(),
            joined: Vec::new(),
            joined_counted: 0,
        }
    }
    /// Take one decoded (or failed) segment; the blocks now in order are pushed to `out`.
    pub fn take(
        &mut self,
        segment: Segment,
        result: Result<Vec<u8>, Bz2Error>,
        out: &mut Vec<Decoded>,
    ) -> Result<(), Bz2Error> {
        self.waiting.insert(segment.index, (segment, result));
        if self.waiting.len() > WAITING_CAP {
            return Err(Bz2Error::Block);
        }
        while let Some((segment, result)) = self.waiting.remove(&self.next) {
            self.next += 1;
            if self.joined.is_empty() {
                match result {
                    Ok(text) => out.push(Decoded {
                        start_bit: segment.start_bit,
                        counted: text.len(),
                        text,
                    }),
                    Err(_) => self.joined.push(segment),
                }
                continue;
            }
            self.joined_counted += result.as_ref().map_or(0, Vec::len);
            self.joined.push(segment);
            let parts: Vec<&Segment> = self.joined.iter().collect();
            let level = self.level;
            // A panic in the decoder is a block that does not decode, as in the decoder threads.
            let joined =
                std::panic::catch_unwind(|| decode(level, &parts)).unwrap_or(Err(Bz2Error::Block));
            match joined {
                Ok(text) => {
                    out.push(Decoded {
                        start_bit: self.joined[0].start_bit,
                        text,
                        counted: std::mem::take(&mut self.joined_counted),
                    });
                    self.joined.clear();
                }
                Err(_) if self.joined.len() < JOIN_CAP => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    /// The input has ended: a block still failing is an error.
    pub fn finish(&mut self) -> Result<(), Bz2Error> {
        if self.joined.is_empty() && self.waiting.is_empty() {
            Ok(())
        } else {
            Err(Bz2Error::Block)
        }
    }
    /// How many segments wait for an earlier one.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
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
    /// Text that compresses to several blocks at level 1 (100 kB blocks).
    fn text(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let word = ["alpha", "beta", "\"Q42\"", "{", "}", ",", "\n", "wiki"][(x % 8) as usize];
            out.extend_from_slice(word.as_bytes());
            out.push(b'a' + (x >> 8) as u8 % 26);
        }
        out
    }
    fn decode_all(input: &[u8], chunk: usize, resume: Option<u64>) -> Result<Vec<u8>, Bz2Error> {
        let mut splitter = match resume {
            Some(bit) => Splitter::resume(bit),
            None => Splitter::new(),
        };
        let from = splitter.first_byte() as usize;
        let mut segments = vec![];
        for piece in input[from..].chunks(chunk.max(1)) {
            splitter.push(piece, &mut segments)?;
        }
        splitter.finish(&mut segments)?;
        let mut orderer = Orderer::new(splitter.level);
        let mut out = vec![];
        // Out of order, as the threads give them back.
        segments.reverse();
        for s in segments {
            let r = decode(splitter.level, &[&s]);
            orderer.take(s, r, &mut out)?;
        }
        orderer.finish()?;
        Ok(out.into_iter().flat_map(|d| d.text).collect())
    }

    /// A stream cut into blocks and decoded block by block gives the text back, however its
    /// bytes arrive.
    #[test]
    fn blocks_decode_alone_to_the_whole_text() {
        for (n, level) in [(0usize, 9u32), (10, 9), (450_000, 1), (1_200_000, 2)] {
            let data = text(n, n as u64 + 7);
            let packed = compress(&data, level);
            for chunk in [1usize, 7, 4096, usize::MAX] {
                if n > 100_000 && chunk < 4096 {
                    continue;
                }
                assert_eq!(
                    decode_all(&packed, chunk.min(packed.len()), None).unwrap(),
                    data,
                    "{n} {level} {chunk}"
                );
            }
        }
    }

    /// A read can start at any block, and gives the text from that block on.
    #[test]
    fn a_read_resumes_at_any_block() {
        let data = text(600_000, 3);
        let packed = compress(&data, 1);
        let mut splitter = Splitter::new();
        let mut segments = vec![];
        splitter.push(&packed, &mut segments).unwrap();
        splitter.finish(&mut segments).unwrap();
        assert!(segments.len() >= 5, "{}", segments.len());
        let texts: Vec<Vec<u8>> = segments
            .iter()
            .map(|s| decode(b'1', &[s]).unwrap())
            .collect();
        for (i, s) in segments.iter().enumerate() {
            let rest: Vec<u8> = texts[i..].concat();
            assert_eq!(decode_all(&packed, 1000, Some(s.start_bit)).unwrap(), rest);
        }
    }

    /// Two streams one after the other (a multi-stream file) read as one text.
    #[test]
    fn streams_one_after_the_other_are_one_text() {
        let (a, b) = (text(150_000, 1), text(10, 2));
        let mut packed = compress(&a, 1);
        packed.extend(compress(&b, 9));
        assert_eq!(decode_all(&packed, 333, None).unwrap(), [a, b].concat());
    }

    /// A block split by a magic number inside compressed data is decoded joined again.
    #[test]
    fn a_false_magic_number_joins_the_halves() {
        let data = text(250_000, 11);
        let packed = compress(&data, 1);
        let mut splitter = Splitter::new();
        let mut segments = vec![];
        splitter.push(&packed, &mut segments).unwrap();
        splitter.finish(&mut segments).unwrap();
        // Split the second block in two at an arbitrary bit, as a false magic would.
        let s = segments.remove(1);
        let cut = s.nbits / 2 + 3;
        let mut left = BitWriter::default();
        let mut right = BitWriter::default();
        for i in 0..s.nbits {
            let bit = (s.bits[(i / 8) as usize] >> (7 - i % 8)) & 1;
            if i < cut {
                left.push_bit(bit);
            } else {
                right.push_bit(bit);
            }
        }
        let halves = [
            Segment {
                index: 1,
                start_bit: s.start_bit,
                bits: left.bytes,
                nbits: cut,
            },
            Segment {
                index: 2,
                start_bit: s.start_bit + cut,
                bits: right.bytes,
                nbits: s.nbits - cut,
            },
        ];
        let mut all = vec![segments[0].clone()];
        all.extend(halves);
        for (i, mut rest) in segments.into_iter().skip(1).enumerate() {
            rest.index = 3 + i as u64;
            all.push(rest);
        }
        let mut orderer = Orderer::new(b'1');
        let mut out = vec![];
        for s in all.into_iter().rev() {
            let r = decode(b'1', &[&s]);
            if s.index == 1 || s.index == 2 {
                assert!(r.is_err());
            }
            orderer.take(s, r, &mut out).unwrap();
        }
        orderer.finish().unwrap();
        assert_eq!(
            out.into_iter().flat_map(|d| d.text).collect::<Vec<_>>(),
            data
        );
    }

    /// A block longer than the cap (no next magic number within 4 MB) is not a bzip2 stream:
    /// `Oversized`, before its bytes pile up.
    #[test]
    fn a_block_over_the_cap_is_refused() {
        let mut input = b"BZh9".to_vec();
        input.extend_from_slice(&[0x31, 0x41, 0x59, 0x26, 0x53, 0x59]);
        input.extend(std::iter::repeat_n(0u8, SEGMENT_CAP + (1 << 20)));
        assert_eq!(decode_all(&input, 1 << 20, None), Err(Bz2Error::Oversized));
    }

    /// Input that is not bzip2, is cut short, or is damaged fails with an error, never a panic.
    #[test]
    fn bad_input_is_an_error() {
        assert_eq!(decode_all(b"PK\x03\x04", 4, None), Err(Bz2Error::Header));
        assert_eq!(decode_all(b"BZ", 4, None), Err(Bz2Error::Header));
        // Nothing, or a header with no block after it, is no stream: a read of it fails.
        assert_eq!(decode_all(b"", 4, None), Err(Bz2Error::Header));
        assert_eq!(
            decode_all(b"BZh9\0\0\0\0\0\0\0\0", 4, None),
            Err(Bz2Error::Header)
        );
        // An empty stream (header and stream end) is an empty text.
        assert_eq!(decode_all(&compress(b"", 9), 3, None), Ok(vec![]));
        let packed = compress(&text(300_000, 5), 1);
        let cut = &packed[..packed.len() * 2 / 3];
        assert_eq!(decode_all(cut, 4096, None), Err(Bz2Error::Truncated));
        let mut damaged = packed.clone();
        let at = damaged.len() / 2;
        damaged[at] ^= 0x5a;
        assert!(decode_all(&damaged, 4096, None).is_err());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 300, ..Default::default() })]

        /// Fuzz: random bytes after a header, and a real stream with bytes changed or cut,
        /// split and decode to an error or a text no larger than the caps, never a panic; an
        /// unchanged stream gives its text back whatever the chunk size.
        #[test]
        fn any_input_splits_and_decodes_without_a_panic(
            noise in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4096),
            flips in proptest::collection::vec((0.0f64..1.0, proptest::prelude::any::<u8>()), 0..6),
            cut in 0.0f64..1.0,
            chunk in 1usize..5000,
        ) {
            let mut fake = b"BZh9".to_vec();
            fake.extend_from_slice(&noise);
            let _ = decode_all(&fake, chunk, None);
            let data = text(120_000, noise.len() as u64 + 1);
            let packed = compress(&data, 1);
            proptest::prop_assert_eq!(decode_all(&packed, chunk, None).unwrap(), data);
            let mut broken = packed.clone();
            for (at, b) in flips {
                let i = ((broken.len() - 1) as f64 * at) as usize;
                broken[i] = b;
            }
            broken.truncate(((broken.len() as f64) * (0.5 + cut / 2.0)) as usize);
            if let Ok(out) = decode_all(&broken, chunk, None) {
                proptest::prop_assert!(out.len() <= DECODED_CAP * 4);
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 300, ..Default::default() })]
        /// Fuzz: a block of random bits decodes to an error, never a panic; a read resumed at a
        /// random bit (not a block's) fails or gives text, never a panic.
        #[test]
        fn random_blocks_and_resume_points_never_panic(
            bits in proptest::collection::vec(proptest::prelude::any::<u8>(), 10..2048),
            at in 0.0f64..1.0,
        ) {
            let mut w = BitWriter::default();
            w.push_bits(BLOCK_MAGIC, 48);
            for b in &bits {
                w.push_byte(*b);
            }
            let nbits = 48 + bits.len() as u64 * 8;
            let s = Segment { index: 0, start_bit: 0, bits: w.bytes, nbits };
            proptest::prop_assert!(decode(b'9', &[&s]).is_err());
            let packed = compress(&text(50_000, bits.len() as u64), 1);
            let bit = ((packed.len() * 8) as f64 * at) as u64;
            let _ = decode_all(&packed, 777, Some(bit));
        }
    }
}
