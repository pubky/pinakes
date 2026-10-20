//! What the book, podcast and film sources share: Wikidata property tables (how one external ID
//! becomes a derived link), the answer statuses of the services they read, and small checks.
use super::{Found, Link};
use crate::{hubs::wikidata::Item, resolver::RetryLater};
use anyhow::{bail, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::Value;

/// One Wikidata external-ID property and the page its value names.
#[derive(Clone, Copy, Debug)]
pub struct Property {
    pub pid: &'static str,
    /// The service's name as a card shows it.
    pub site: &'static str,
    /// The page, with `$1` for the value (percent-encoded; `/` kept where the validator allows
    /// it).
    pub template: &'static str,
    valid: fn(&str) -> bool,
}
pub(crate) const fn property(
    pid: &'static str,
    site: &'static str,
    template: &'static str,
    valid: fn(&str) -> bool,
) -> Property {
    Property {
        pid,
        site,
        template,
        valid,
    }
}

/// Characters a value keeps as they are in a link (unreserved, and `/` for path-like IDs).
const VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// The links an item has for these properties: one per property whose best-rank values hold
/// exactly one valid value (several are ambiguous and add nothing), and one per service (the
/// first property in the table wins).
pub fn links(item: &Item, table: &[Property]) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    for p in table {
        if out.iter().any(|l| l.site == p.site) {
            continue;
        }
        let mut values: Vec<&String> = item.id(p.pid).iter().filter(|v| (p.valid)(v)).collect();
        values.dedup();
        if let [value] = values.as_slice() {
            let uri = p
                .template
                .replace("$1", &utf8_percent_encode(value, VALUE).to_string());
            out.push(Link::new(uri, p.site, "wikidata"));
        }
    }
    out
}

/// A positive decimal number without leading zeros.
pub(crate) fn digits(v: &str) -> bool {
    (1..=20).contains(&v.len()) && !v.starts_with('0') && v.bytes().all(|b| b.is_ascii_digit())
}
/// A slug: letters, digits and `-_.`.
pub(crate) fn slug(v: &str) -> bool {
    (1..=200).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
/// A lower-case UUID.
pub(crate) fn uuid(v: &str) -> bool {
    crate::hubs::mbid(v).is_some_and(|m| m == v)
}

/// A service's JSON answer: `None` for "no such thing" (one of `missing`), a retry for 429 and
/// 5xx (a minute later), an error for anything else.
pub(crate) fn json(status: u16, raw: &[u8], missing: &[u16]) -> Result<Option<Value>> {
    match status {
        200 => Ok(Some(serde_json::from_slice(raw)?)),
        s if missing.contains(&s) => Ok(None),
        429 | 500..=599 => Err(RetryLater {
            class: "upstream",
            seconds: 60,
        }
        .into()),
        s => bail!("unexpected status {s}"),
    }
}

/// What a failed secondary request leaves: a retry to pass on when nothing else was found.
#[derive(Default)]
pub(crate) struct Deferred(Option<anyhow::Error>);
impl Deferred {
    /// The answer, or `None` after counting the failure; a retry is kept for later.
    pub(crate) fn take<T>(
        &mut self,
        service: &'static str,
        result: Result<Option<T>>,
    ) -> Option<T> {
        match result {
            Ok(v) => v,
            Err(error) => {
                let class = crate::resolver::error_class(&error);
                crate::telemetry::also_on_service_error(service, class);
                tracing::debug!(service, class, "also-on service failed; the lookup goes on");
                if error.is::<crate::resolver::RetryLater>() && self.0.is_none() {
                    self.0 = Some(error);
                }
                None
            }
        }
    }
    /// The lookup's answer: what was found, or the retry when nothing was.
    pub(crate) fn finish(self, found: Found) -> Result<Found> {
        match self.0 {
            Some(retry) if found == Found::default() => Err(retry),
            _ => Ok(found),
        }
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Recorded answers in `tests/fixtures/media/<family>/` (see each `provenance.json`).
    use std::io::Read;

    pub fn read(family: &str, name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/media/{family}/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        if name.ends_with(".gz") {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw.as_slice())
                .read_to_end(&mut out)
                .expect("gzip fixture");
            out
        } else {
            raw
        }
    }
    /// The recorded Wikidata items (`wikidata/entities-media.json.gz`) by Q-id.
    pub fn items() -> Vec<crate::hubs::wikidata::Item> {
        let raw = read("wikidata", "entities-media.json.gz");
        crate::hubs::wikidata::parse_entities(&serde_json::from_slice(&raw).unwrap())
    }
    pub fn item(qid: &str) -> crate::hubs::wikidata::Item {
        items()
            .into_iter()
            .find(|i| i.qid == qid)
            .unwrap_or_else(|| panic!("{qid}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hubs::wikidata::Item;
    use std::collections::BTreeMap;

    fn item(ids: &[(&str, &[&str])]) -> Item {
        Item {
            qid: "Q1".into(),
            ids: ids
                .iter()
                .map(|(p, v)| (p.to_string(), v.iter().map(|s| s.to_string()).collect()))
                .collect::<BTreeMap<_, _>>(),
            sitelinks: BTreeMap::new(),
        }
    }

    #[test]
    fn one_value_one_link_one_per_site() {
        const TABLE: &[Property] = &[
            property("P1", "A", "https://a.example/$1", digits),
            property("P2", "A", "https://a.example/other/$1", digits),
            property("P3", "B", "https://b.example/$1", slug),
            property("P4", "C", "https://c.example/$1", digits),
            property("P5", "D", "https://d.example/$1/", |v| v.contains('/')),
        ];
        let links = links(
            &item(&[
                ("P1", &["12"]),
                ("P2", &["34"]),
                ("P3", &["x y"]),
                ("P4", &["1", "2"]),
                ("P5", &["movies/the film"]),
            ]),
            TABLE,
        );
        let got: Vec<(&str, &str)> = links
            .iter()
            .map(|l| (l.site.as_str(), l.uri.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("A", "https://a.example/12"),
                ("D", "https://d.example/movies/the%20film/")
            ]
        );
        assert!(links.iter().all(|l| l.via == "wikidata"));
    }

    #[test]
    fn checks() {
        assert!(digits("278") && !digits("0278") && !digits("") && !digits("2a"));
        assert!(slug("the-shawshank-redemption") && !slug("a b"));
        assert!(uuid("304b84f0-07b0-5265-b6b7-da5cf5aeb56e"));
        assert!(!uuid("304B84F0-07B0-5265-B6B7-DA5CF5AEB56E"));
    }
}
