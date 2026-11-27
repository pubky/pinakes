//! OpenStreetMap change files (`*.osc.gz`): the objects created, modified and deleted in a
//! replication interval, read as a stream. Every size is capped (an element's tags, a way's
//! nodes, a relation's members, the bytes of one XML tag), so a damaged file fails with an error
//! and never exhausts memory.
use super::pbf::Kind;
use anyhow::{bail, ensure, Context, Result};
use quick_xml::events::{BytesStart, Event};
use std::io::{BufRead, Read};

/// The most bytes between two `>` (one XML tag): a longer run is a damaged file.
pub const MAX_TAG_BYTES: usize = 1 << 20;
/// The most elements one change file holds.
pub const MAX_ELEMENTS: usize = 20_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Create,
    Modify,
    Delete,
}
/// One changed object as the file states it.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub action: Action,
    pub kind: Kind,
    pub id: i64,
    pub version: u32,
    /// Its changeset, when the file names it (Geofabrik's public files do not).
    pub changeset: Option<i64>,
    /// When the change was made, as written (`2026-11-24T16:06:03Z`).
    pub timestamp: Option<String>,
    /// A node's location in 1e-7 degrees.
    pub location: Option<(i32, i32)>,
    pub tags: Vec<(String, String)>,
    pub refs: Vec<i64>,
    pub members: Vec<(Kind, i64, String)>,
}

/// Counts the bytes pulled from the stream since the reader's last finished event (the read
/// loop resets it after each one), and fails a read past [`MAX_TAG_BYTES`]: one XML event of any
/// kind (a tag, however its attributes quote `<` or `>`, text, a comment) never grows the event
/// buffer past that and the one read buffer pulled before the count started.
struct Bounded<R> {
    inner: R,
    pulled: std::rc::Rc<std::cell::Cell<usize>>,
}
/// The read buffer between the stream and the XML reader.
const READ_BUFFER: usize = 64 * 1024;
impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let pulled = self.pulled.get() + n;
        self.pulled.set(pulled);
        if pulled > MAX_TAG_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "osc: an XML event over 1 MiB",
            ));
        }
        Ok(n)
    }
}

fn coordinate(value: &str, max: i64) -> Option<i32> {
    let v: f64 = value.parse().ok()?;
    let e7 = (v * 1e7).round();
    (e7.is_finite() && e7.abs() <= max as f64).then_some(e7 as i32)
}
fn attributes(e: &BytesStart<'_>) -> Result<Vec<(String, String)>> {
    let mut out = vec![];
    for a in e.attributes() {
        let a = a.context("osc: damaged attribute")?;
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let value = a
            .unescape_value()
            .context("osc: damaged attribute value")?
            .into_owned();
        ensure!(out.len() < 64, "osc: too many attributes");
        out.push((key, value));
    }
    Ok(out)
}
fn get<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}
fn positive(value: Option<&str>) -> Option<i64> {
    value?.parse::<i64>().ok().filter(|v| *v > 0)
}

/// Read a gzip-compressed change file, passing each change to `each` in file order.
pub fn read_gz(input: impl Read, each: impl FnMut(Change) -> Result<()>) -> Result<()> {
    read_gz_to(input, MAX_DECOMPRESSED, each)
}
/// [`read_gz`] reading at most `cap` decompressed bytes: past them the file ends inside an
/// element, an error.
pub fn read_gz_to(
    input: impl Read,
    cap: u64,
    each: impl FnMut(Change) -> Result<()>,
) -> Result<()> {
    read(
        std::io::BufReader::new(flate2::read::MultiGzDecoder::new(input).take(cap)),
        each,
    )
}
/// The most bytes a change file decompresses to (a planet day is about 1.5 GB): past it the file
/// ends inside an element, an error.
pub const MAX_DECOMPRESSED: u64 = 16 << 30;

/// Read a change file, passing each change to `each` in file order.
pub fn read(input: impl BufRead, mut each: impl FnMut(Change) -> Result<()>) -> Result<()> {
    let pulled = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::with_capacity(
        READ_BUFFER,
        Bounded {
            inner: input,
            pulled: pulled.clone(),
        },
    ));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut action = None;
    let mut current: Option<Change> = None;
    let mut elements = 0usize;
    let mut root = false;
    // The root's close tag: a file that ends without it was cut off, whatever it held.
    let mut closed = false;
    loop {
        buf.clear();
        let event = reader
            .read_event_into(&mut buf)
            .context("osc: damaged XML")?;
        pulled.set(0);
        let (start, empty) = match &event {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = start {
            let name = e.name();
            let name = name.as_ref();
            match name {
                b"osmChange" => {
                    root = true;
                    closed = empty;
                }
                b"create" | b"modify" | b"delete" if root => {
                    action = Some(match name {
                        b"create" => Action::Create,
                        b"modify" => Action::Modify,
                        _ => Action::Delete,
                    });
                }
                b"node" | b"way" | b"relation" if action.is_some() && current.is_none() => {
                    elements += 1;
                    ensure!(elements <= MAX_ELEMENTS, "osc: too many elements");
                    let attrs = attributes(&e)?;
                    let kind = match name {
                        b"node" => Kind::Node,
                        b"way" => Kind::Way,
                        _ => Kind::Relation,
                    };
                    let id =
                        positive(get(&attrs, "id")).context("osc: element without a valid id")?;
                    let location = match (get(&attrs, "lat"), get(&attrs, "lon")) {
                        (Some(la), Some(lo)) => {
                            coordinate(la, 900_000_000).zip(coordinate(lo, 1_800_000_000))
                        }
                        _ => None,
                    };
                    let change = Change {
                        action: action.expect("inside an action"),
                        kind,
                        id,
                        version: get(&attrs, "version")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0),
                        changeset: positive(get(&attrs, "changeset")),
                        timestamp: get(&attrs, "timestamp")
                            .filter(|t| t.len() <= 32)
                            .map(str::to_owned),
                        location,
                        tags: vec![],
                        refs: vec![],
                        members: vec![],
                    };
                    if empty {
                        each(change)?;
                    } else {
                        current = Some(change);
                    }
                }
                b"tag" if current.is_some() => {
                    let c = current.as_mut().expect("an element");
                    let attrs = attributes(&e)?;
                    if let (Some(k), Some(v)) = (get(&attrs, "k"), get(&attrs, "v")) {
                        if c.tags.len() < super::pbf::MAX_TAGS && !k.is_empty() {
                            // Cut as the cards keep them: a value is at most `VALUE_CAP`.
                            let (mut k, mut v) = (
                                super::place::clean(k).into_owned(),
                                super::place::clean(v).into_owned(),
                            );
                            crate::model::truncate(&mut k, super::place::VALUE_CAP);
                            crate::model::truncate(&mut v, super::place::VALUE_CAP);
                            c.tags.push((k, v));
                        }
                    }
                }
                b"nd" if current.is_some() => {
                    let c = current.as_mut().expect("an element");
                    if let Some(r) = positive(get(&attributes(&e)?, "ref")) {
                        if c.refs.len() < super::pbf::MAX_REFS {
                            c.refs.push(r);
                        }
                    }
                }
                b"member" if current.is_some() => {
                    let c = current.as_mut().expect("an element");
                    let attrs = attributes(&e)?;
                    let kind = get(&attrs, "type").and_then(Kind::parse);
                    if let (Some(kind), Some(r)) = (kind, positive(get(&attrs, "ref"))) {
                        if c.members.len() < super::pbf::MAX_MEMBERS {
                            let mut role = get(&attrs, "role").unwrap_or_default().to_owned();
                            crate::model::truncate(&mut role, 255);
                            c.members.push((kind, r, role));
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        match event {
            Event::End(e) => match e.name().as_ref() {
                b"node" | b"way" | b"relation" => {
                    if let Some(c) = current.take() {
                        each(c)?;
                    }
                }
                b"create" | b"modify" | b"delete" => action = None,
                b"osmChange" => closed = true,
                _ => {}
            },
            Event::Eof => {
                ensure!(
                    current.is_none() && action.is_none(),
                    "osc: file ends inside an element"
                );
                if !root {
                    bail!("osc: not a change file");
                }
                ensure!(closed, "osc: file ends before its </osmChange>: cut off");
                return Ok(());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<osmChange version="0.6" generator="osmium/1.16.0">
  <modify>
    <node id="1887823668" version="17" timestamp="2026-11-24T16:06:03Z" lat="47.1813835" lon="9.5211187">
      <tag k="name" v="Forst &amp; Hilti"/>
      <tag k="railway" v="halt"/>
    </node>
    <way id="33683275" version="16" changeset="123" timestamp="2026-11-24T12:33:47Z">
      <nd ref="385113127"/>
      <nd ref="385113225"/>
      <tag k="building" v="yes"/>
    </way>
  </modify>
  <create>
    <node id="5" version="1" lat="-1.5" lon="-2.25"/>
    <relation id="9" version="1">
      <member type="way" ref="33683275" role="outer"/>
      <member type="bogus" ref="1" role=""/>
      <tag k="type" v="multipolygon"/>
    </relation>
  </create>
  <delete>
    <node id="7" version="3"/>
  </delete>
</osmChange>"#;

    /// A file that decompresses past its cap ends inside an element: an error.
    #[test]
    fn a_file_past_its_decompressed_cap_fails() {
        let mut z = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
        std::io::Write::write_all(&mut z, SAMPLE.as_bytes()).unwrap();
        let gz = z.finish().unwrap();
        assert!(read_gz_to(&gz[..], SAMPLE.len() as u64, |_| Ok(())).is_ok());
        assert!(read_gz_to(&gz[..], SAMPLE.len() as u64 / 2, |_| Ok(())).is_err());
    }

    /// A tag value's hidden characters are taken out as it is read.
    #[test]
    fn hidden_characters_are_taken_out_of_values() {
        let text = "<osmChange><create><node id=\"1\" version=\"1\" lat=\"1\" lon=\"1\"><tag k=\"name\" v=\"קפה&#x200F;\"/></node></create></osmChange>";
        let mut got = vec![];
        read(text.as_bytes(), |c| {
            got.push(c);
            Ok(())
        })
        .unwrap();
        assert_eq!(got[0].tags, [("name".to_owned(), "קפה".to_owned())]);
    }

    /// The per-event cap counts one event at a time: a file of many small events, far over the
    /// cap in all, reads whole.
    #[test]
    fn a_large_file_of_small_events_reads() {
        let mut text = String::from("<osmChange version=\"0.6\"><modify>");
        for i in 1..=40_000 {
            text.push_str(&format!("<node id=\"{i}\" version=\"1\" lat=\"47.1\" lon=\"9.5\"><tag k=\"amenity\" v=\"cafe\"/></node>"));
        }
        text.push_str("</modify></osmChange>");
        assert!(text.len() > 3 * (MAX_TAG_BYTES + READ_BUFFER));
        let mut n = 0;
        read(text.as_bytes(), |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 40_000);
    }

    #[test]
    fn a_change_file_reads_in_order() {
        let mut got = vec![];
        read(SAMPLE.as_bytes(), |c| {
            got.push(c);
            Ok(())
        })
        .unwrap();
        assert_eq!(got.len(), 5);
        assert_eq!(
            (got[0].action, got[0].kind, got[0].id, got[0].version),
            (Action::Modify, Kind::Node, 1887823668, 17)
        );
        assert_eq!(got[0].location, Some((471_813_835, 95_211_187)));
        assert_eq!(got[0].tags[0], ("name".into(), "Forst & Hilti".into()));
        assert_eq!(got[0].changeset, None);
        assert_eq!(got[1].refs, [385113127, 385113225]);
        assert_eq!(got[1].changeset, Some(123));
        assert_eq!(
            (got[2].action, got[2].location),
            (Action::Create, Some((-15_000_000, -22_500_000)))
        );
        assert_eq!(got[3].members, [(Kind::Way, 33683275, "outer".to_owned())]);
        assert_eq!(
            (got[4].action, got[4].id, got[4].tags.len()),
            (Action::Delete, 7, 0)
        );
        // Compressed, as the replication server serves it.
        let mut z = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
        std::io::Write::write_all(&mut z, SAMPLE.as_bytes()).unwrap();
        let mut n = 0;
        read_gz(&z.finish().unwrap()[..], |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 5);
    }

    proptest::proptest! {
        /// Arbitrary text and bytes as a change file: an error or changes, never a panic.
        #[test]
        fn arbitrary_change_files_never_panic(text in "(<|>|/|\"|=| |[a-z]{1,8}|&[a-z]{2,4};|[0-9.]{1,6}){0,200}", bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..512)) {
            let _ = read(format!("<osmChange><create>{text}</create></osmChange>").as_bytes(), |_| Ok(()));
            let _ = read(&bytes[..], |_| Ok(()));
            let _ = read_gz(&bytes[..], |_| Ok(()));
        }
    }

    /// A file cut off between two actions (each element whole) is no change file: the update
    /// must not move past changes it never read.
    #[test]
    fn a_file_cut_off_between_actions_fails() {
        let end = SAMPLE.find("</create>").unwrap() + "</create>".len();
        let mut read_any = 0;
        let cut = read(&SAMPLE.as_bytes()[..end], |_| {
            read_any += 1;
            Ok(())
        });
        assert!(cut.unwrap_err().to_string().contains("cut off"));
        assert!(read_any > 0);
        // Whole, it reads; an empty change file is whole too.
        assert!(read(SAMPLE.as_bytes(), |_| Ok(())).is_ok());
        assert!(read(&b"<osmChange version=\"0.6\"/>"[..], |_| Ok(())).is_ok());
    }

    #[test]
    fn damaged_change_files_fail_without_panicking() {
        assert!(read(&b"<html></html>"[..], |_| Ok(())).is_err());
        assert!(read(&SAMPLE.as_bytes()[..SAMPLE.len() / 2], |_| Ok(())).is_err());
        let huge = format!(
            "<osmChange><create><node id=\"1\" v=\"{}\"/></create></osmChange>",
            "x".repeat(MAX_TAG_BYTES + READ_BUFFER + 10)
        );
        assert!(read(huge.as_bytes(), |_| Ok(())).is_err());
        // Text with `>` in it is one event too, and so is a tag whose quoted value holds `<` and
        // `>`, and a comment.
        let text = format!("<osmChange>{}</osmChange>", "a>".repeat(MAX_TAG_BYTES));
        assert!(read(text.as_bytes(), |_| Ok(())).is_err());
        let quoted = format!(
            "<osmChange><create><node id=\"1\" v=\"{}\"/></create></osmChange>",
            "><".repeat(MAX_TAG_BYTES)
        );
        assert!(read(quoted.as_bytes(), |_| Ok(())).is_err());
        let comment = format!(
            "<osmChange><!--{}--></osmChange>",
            "<>".repeat(MAX_TAG_BYTES)
        );
        assert!(read(comment.as_bytes(), |_| Ok(())).is_err());
        // A long value is cut.
        let long = format!("<osmChange><create><node id=\"1\" lat=\"1\" lon=\"1\"><tag k=\"name\" v=\"{}\"/></node></create></osmChange>", "x".repeat(10_000));
        let mut got = vec![];
        read(long.as_bytes(), |c| {
            got.push(c);
            Ok(())
        })
        .unwrap();
        assert_eq!(got[0].tags[0].1.len(), super::super::place::VALUE_CAP);
        let mut seed = 11u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..5000 {
            let mut data = SAMPLE.as_bytes().to_vec();
            for _ in 0..(1 + next() % 6) {
                let at = (next() as usize) % data.len();
                data[at] = b"<>\"&/= x0"[(next() % 9) as usize];
            }
            let _ = read(&data[..], |_| Ok(()));
        }
    }
}
