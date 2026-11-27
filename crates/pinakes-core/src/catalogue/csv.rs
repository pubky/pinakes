//! A CSV reader for the catalogues' bulk files (RFC 4180: fields split by `,`, a field in double
//! quotes may hold commas, line breaks and `""` for one quote), streaming and bounded: each field
//! keeps at most its cap of bytes and the rest is skipped as it is read, so a record holding a
//! megabyte README takes no more memory than the caps, and a record has at most [`FIELDS_CAP`]
//! fields. A malformed file is read on as well as it can be (a stray quote is kept as text);
//! nothing here panics on any input.
use std::io::{self, BufRead};

/// The most fields one record keeps; the rest of a longer record is skipped.
pub const FIELDS_CAP: usize = 64;

/// One record: its fields, each at most the reader's cap of bytes (cut at a character boundary
/// when read as text).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    fields: Vec<Vec<u8>>,
    /// Whether a field was longer than its cap (and so cut).
    pub cut: bool,
}
impl Record {
    pub fn len(&self) -> usize {
        self.fields.len()
    }
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
    /// Field `i` as text (invalid UTF-8 replaced), or empty when the record has no such field.
    pub fn text(&self, i: usize) -> String {
        self.fields
            .get(i)
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .unwrap_or_default()
    }
    pub fn bytes(&self, i: usize) -> &[u8] {
        self.fields.get(i).map_or(&[], Vec::as_slice)
    }
}

/// Reads records from `inner`; field `i` keeps at most `caps[i]` bytes (`default_cap` for a
/// field past the list).
pub struct Reader<R> {
    inner: R,
    caps: Vec<usize>,
    default_cap: usize,
    /// Records read.
    pub records: u64,
}
impl<R: BufRead> Reader<R> {
    pub fn new(inner: R, default_cap: usize) -> Self {
        Self {
            inner,
            caps: Vec::new(),
            default_cap,
            records: 0,
        }
    }
    /// The cap of each field by its place (a field not listed takes the default).
    pub fn caps(&mut self, caps: Vec<usize>) {
        self.caps = caps;
    }
    /// The next record into `record`; `false` at the end of the stream. An empty line is a
    /// record with one empty field.
    pub fn next_record(&mut self, record: &mut Record) -> io::Result<bool> {
        let Self {
            inner,
            caps,
            default_cap,
            records,
        } = self;
        let cap_of = |i: usize| caps.get(i).copied().unwrap_or(*default_cap);
        record.fields.clear();
        record.cut = false;
        let mut field: Vec<u8> = Vec::new();
        let mut cap = cap_of(0);
        // Inside a quoted field; just after its closing quote (or a `""` pair's first one).
        let mut quoted = false;
        let mut quote_seen = false;
        let mut started = false;
        // Whether the current field opened with a quote (its `\r` before a break is its own).
        let mut field_quoted = false;
        let mut any = false;
        loop {
            let buf = match inner.fill_buf() {
                Ok(buf) => buf,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if buf.is_empty() {
                if !any {
                    return Ok(false);
                }
                push(record, &mut field);
                *records += 1;
                return Ok(true);
            }
            let mut used = 0;
            let mut done = false;
            for &b in buf {
                used += 1;
                any = true;
                if quoted {
                    if quote_seen {
                        quote_seen = false;
                        if b == b'"' {
                            // `""`: one quote.
                            keep(&mut field, b, cap, &mut record.cut);
                            continue;
                        }
                        quoted = false;
                        // Fall through: the byte after a closing quote.
                    } else if b == b'"' {
                        quote_seen = true;
                        continue;
                    } else {
                        keep(&mut field, b, cap, &mut record.cut);
                        continue;
                    }
                }
                match b {
                    b'"' if !started => {
                        quoted = true;
                        started = true;
                        field_quoted = true;
                    }
                    b',' => {
                        push(record, &mut field);
                        cap = cap_of(record.fields.len());
                        started = false;
                        field_quoted = false;
                    }
                    b'\n' => {
                        // A `\r` kept before the break of an unquoted field is the break's.
                        if !field_quoted && field.last() == Some(&b'\r') {
                            field.pop();
                        }
                        push(record, &mut field);
                        done = true;
                        break;
                    }
                    b'\r' if started && !quoted => {
                        // After a closing quote: the line break's first half.
                    }
                    _ => {
                        started = true;
                        keep(&mut field, b, cap, &mut record.cut);
                    }
                }
            }
            inner.consume(used);
            if done {
                *records += 1;
                return Ok(true);
            }
        }
    }
}
/// A field into its record, past [`FIELDS_CAP`] only marking it cut.
fn push(record: &mut Record, field: &mut Vec<u8>) {
    let field = std::mem::take(field);
    if record.fields.len() < FIELDS_CAP {
        record.fields.push(field);
    } else {
        record.cut = true;
    }
}
fn keep(field: &mut Vec<u8>, b: u8, cap: usize, cut: &mut bool) {
    if field.len() < cap {
        field.push(b);
    } else {
        *cut = true;
    }
}

/// The place of each column `names` asks for in a header record; `None` when one is missing.
pub fn columns<const N: usize>(header: &Record, names: [&str; N]) -> Option<[usize; N]> {
    let mut out = [0; N];
    for (slot, name) in out.iter_mut().zip(names) {
        *slot = (0..header.len()).find(|i| header.bytes(*i) == name.as_bytes())?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn read_all(text: &[u8], cap: usize) -> Vec<Vec<String>> {
        let mut reader = Reader::new(io::BufReader::with_capacity(7, text), cap);
        let mut record = Record::default();
        let mut out = vec![];
        while reader.next_record(&mut record).unwrap() {
            out.push((0..record.len()).map(|i| record.text(i)).collect());
        }
        out
    }

    #[test]
    fn records_with_quotes_commas_and_line_breaks() {
        let text = b"id,name,readme\n1,serde,\"A \"\"quoted\"\", multi\nline\r\nreadme\"\r\n2,\"tokio\",plain\n3,,\n\"4\"\n";
        assert_eq!(
            read_all(text, 100),
            vec![
                vec!["id", "name", "readme"],
                vec!["1", "serde", "A \"quoted\", multi\nline\r\nreadme"],
                vec!["2", "tokio", "plain"],
                vec!["3", "", ""],
                vec!["4"],
            ]
        );
        // A last record without its line break counts.
        assert_eq!(read_all(b"a,b", 10), vec![vec!["a", "b"]]);
        assert!(read_all(b"", 10).is_empty());
    }

    #[test]
    fn a_long_field_is_cut_at_its_cap_and_the_record_read_on() {
        let mut text = b"1,".to_vec();
        text.extend(std::iter::repeat_n(b'x', 10_000));
        text.extend(b",end\n2,y,z\n");
        let mut reader = Reader::new(&text[..], 4);
        reader.caps(vec![8, 3]);
        let mut record = Record::default();
        assert!(reader.next_record(&mut record).unwrap());
        assert_eq!(record.text(1), "xxx");
        assert_eq!(record.text(2), "end");
        assert!(record.cut);
        assert!(reader.next_record(&mut record).unwrap());
        assert_eq!((record.text(0), record.text(1)), ("2".into(), "y".into()));
        assert!(!record.cut);
    }

    #[test]
    fn a_record_keeps_at_most_its_fields_cap() {
        let text = vec![b','; 1000];
        let mut reader = Reader::new(&text[..], 4);
        let mut record = Record::default();
        assert!(reader.next_record(&mut record).unwrap());
        assert_eq!(record.len(), FIELDS_CAP);
        assert!(record.cut);
    }

    #[test]
    fn columns_by_name() {
        let header = Record {
            fields: vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
            cut: false,
        };
        assert_eq!(columns(&header, ["c", "a"]), Some([2, 0]));
        assert_eq!(columns(&header, ["d"]), None);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]
        /// Fuzz: any bytes read without a panic, every field within its cap, every record within
        /// the field cap, and the reader always ends.
        #[test]
        fn any_bytes_are_read_within_the_caps(bytes in proptest::collection::vec(
            prop_oneof![Just(b'"'), Just(b','), Just(b'\n'), Just(b'\r'), any::<u8>()], 0..4096),
            cap in 0usize..64) {
            let mut reader = Reader::new(io::BufReader::with_capacity(3, &bytes[..]), cap);
            let mut record = Record::default();
            let mut n = 0;
            while reader.next_record(&mut record).unwrap() {
                n += 1;
                prop_assert!(n <= bytes.len() + 1);
                prop_assert!(record.len() <= FIELDS_CAP);
                for i in 0..record.len() {
                    prop_assert!(record.bytes(i).len() <= cap);
                }
            }
        }
        /// Round trip: fields written as RFC 4180 (every one quoted) read back as written.
        #[test]
        fn quoted_fields_read_back(rows in proptest::collection::vec(
            proptest::collection::vec("[a-z,\"\n\r é]{0,12}", 1..6), 1..8)) {
            let mut text = String::new();
            for row in &rows {
                let fields: Vec<String> = row.iter().map(|f| format!("\"{}\"", f.replace('"', "\"\""))).collect();
                text.push_str(&fields.join(","));
                text.push_str("\r\n");
            }
            prop_assert_eq!(read_all(text.as_bytes(), 1000), rows);
        }
    }
}
