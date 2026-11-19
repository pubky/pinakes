//! Species and occurrences from the GBIF API, read by the key in the link.
//!
//! Every gbif.org link of one object, with or without a language prefix or a tab, and its API
//! URL are aliases of one identifier: `gbif:species/{key}` or `gbif:occurrence/{key}`. An
//! occurrence published from iNaturalist names that observation (`inat_observation`).
//!
//! The licence is per record: an occurrence card credits its own licence and rights holder.
use super::kit::{
    float, host_and_segments, int, json, number, text, texts, time, Card, Point, Request,
};
use super::{Example, Source};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

pub const SITE: &str = "GBIF";
const API: &str = "https://api.gbif.org/v1";
/// The GBIF Backbone Taxonomy, whose species keys most links use.
const BACKBONE: &str = "d7dddbf4-2cf0-4f39-9b2a-bb099caae36c";

/// The object a link names, by its GBIF key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Species(u64),
    Occurrence(u64),
}

/// A positive decimal key, without leading zeros.
pub fn key(value: &str) -> Option<u64> {
    number(value, 19)
}

use super::biodiversity::gbif_language as language;

/// What a gbif.org or GBIF API link names. The species tabs (`metrics`, `verbatim`,
/// `literature`, `treatments`) and the occurrence's `verbatim` tab name the same object.
pub fn from_uri(uri: &str) -> Option<Object> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    let s = match host.as_str() {
        "gbif.org" => match s.as_slice() {
            [lang, rest @ ..] if language(lang) => rest,
            _ => s.as_slice(),
        },
        "api.gbif.org" => match s.as_slice() {
            ["v1", kind @ ("species" | "occurrence"), id] => {
                return object(kind, id);
            }
            _ => return None,
        },
        _ => return None,
    };
    match s {
        ["species", id] | ["species", id, "metrics" | "verbatim" | "literature" | "treatments"] => {
            object("species", id)
        }
        ["occurrence", id] | ["occurrence", id, "verbatim"] => object("occurrence", id),
        _ => None,
    }
}

fn object(kind: &str, id: &str) -> Option<Object> {
    let id = key(id)?;
    Some(match kind {
        "species" => Object::Species(id),
        _ => Object::Occurrence(id),
    })
}

/// The identifier every link form of the object is an alias of.
pub fn identifier(object: Object) -> String {
    match object {
        Object::Species(k) => format!("gbif:species/{k}"),
        Object::Occurrence(k) => format!("gbif:occurrence/{k}"),
    }
}

/// The object's page on gbif.org.
pub fn page(object: Object) -> String {
    match object {
        Object::Species(k) => format!("https://www.gbif.org/species/{k}"),
        Object::Occurrence(k) => format!("https://www.gbif.org/occurrence/{k}"),
    }
}

/// The object's API record.
pub fn request(object: Object) -> Option<Request> {
    Request::get(&match object {
        Object::Species(k) => format!("{API}/species/{k}"),
        Object::Occurrence(k) => format!("{API}/occurrence/{k}"),
    })
}

/// The object of a root (`gbif:species/{key}`, `gbif:occurrence/{key}`).
pub fn from_root(uri: &str) -> Option<Object> {
    let (kind, id) = uri.strip_prefix("gbif:")?.split_once('/')?;
    let object = match kind {
        "species" | "occurrence" => object(kind, id)?,
        _ => return None,
    };
    (identifier(object) == uri).then_some(object)
}

/// The object of a root or a gbif.org link.
pub fn target(uri: &str) -> Option<Object> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// The species or occurrence card, or `None` when GBIF has no such record.
pub fn parse(status: u16, raw: &[u8], object: Object) -> Result<Option<Card>> {
    match object {
        Object::Species(k) => parse_species(status, raw, k),
        Object::Occurrence(k) => parse_occurrence(status, raw, k),
    }
}

fn checked(v: &Value, key: u64, what: &str) -> Result<()> {
    ensure!(
        int(&v["key"]).and_then(|k| u64::try_from(k).ok()) == Some(key),
        "GBIF answered for another {what}"
    );
    Ok(())
}

/// A species (or any other taxon) of a GBIF checklist. A synonym is its own record and names
/// its accepted taxon.
pub fn parse_species(status: u16, raw: &[u8], key: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    checked(&v, key, "taxon")?;
    let object = Object::Species(key);
    let mut card = Card::new("Taxon", identifier(object), page(object));
    let canonical = text(&v["canonicalName"]);
    let authorship = text(&v["authorship"]);
    card.name = text(&v["scientificName"]).or_else(|| match (&canonical, &authorship) {
        (Some(c), Some(a)) => Some(format!("{c} {a}")),
        _ => canonical.clone(),
    });
    card.name.as_ref().context("no name")?;
    card.detail("canonical_name", canonical);
    card.detail("authorship", authorship);
    card.detail("rank", text(&v["rank"]));
    card.detail("taxonomic_status", text(&v["taxonomicStatus"]));
    for rank in ["kingdom", "phylum", "class", "order", "family", "genus"] {
        card.detail(rank, text(&v[rank]));
    }
    card.detail("vernacular_name", text(&v["vernacularName"]));
    card.detail("accepted_key", int(&v["acceptedKey"]).filter(|k| *k > 0));
    card.detail("accepted_name", text(&v["accepted"]));
    card.detail("parent_key", int(&v["parentKey"]).filter(|k| *k > 0));
    card.detail("published_in", text(&v["publishedIn"]));
    let dataset = text(&v["datasetKey"]);
    card.attribution = Some(match dataset.as_deref() {
        Some(BACKBONE) => "GBIF Backbone Taxonomy (CC BY 4.0), via GBIF.org".into(),
        _ => "via GBIF.org".into(),
    });
    card.detail("dataset_key", dataset);
    Ok(Some(card))
}

/// A Creative Commons licence URL as its short name, e.g. "CC BY-NC 4.0"; anything else as
/// given.
pub fn licence(value: &str) -> String {
    let path = value
        .split_once("creativecommons.org/")
        .map(|(_, p)| p.split('/').collect::<Vec<_>>());
    match path.as_deref() {
        Some(["licenses", code, version, ..]) => {
            format!("CC {} {version}", code.to_ascii_uppercase())
        }
        Some(["publicdomain", "zero", version, ..]) => format!("CC0 {version}"),
        _ => value.to_owned(),
    }
}

/// An `eventDate` as a time: an RFC 3339 time, a full date (midnight UTC), or a local date-time
/// made exact by the offset in `eventTime`. A local date-time without an offset counts as its
/// date. Year-only and month-only values give nothing.
fn event_time(value: &str, event_time: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(t) = time(&Value::from(value)) {
        return Some(t);
    }
    let (date, clock) = value.split_once('T')?;
    if let Some(offset) = event_time.and_then(|t| t.strip_prefix(clock)) {
        if let Some(t) = time(&Value::from(format!("{value}{offset}"))) {
            return Some(t);
        }
    }
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M"))
        .ok()?;
    time(&Value::from(date))
}

/// Issues that mean the coordinates are not a real place.
const BAD_COORDINATES: [&str; 4] = [
    "ZERO_COORDINATE",
    "COORDINATE_INVALID",
    "COORDINATE_OUT_OF_RANGE",
    "COUNTRY_COORDINATE_MISMATCH",
];

/// Coordinates less certain than this are an area, not a point.
pub const MAX_UNCERTAINTY_M: f64 = 1000.0;

/// The occurrence's point, unless the publisher withheld or generalised information (often the
/// location, e.g. "Coordinate uncertainty increased to 27816m"), the coordinates are uncertain by
/// more than `MAX_UNCERTAINTY_M`, or GBIF flags them.
fn occurrence_point(v: &Value) -> Option<Point> {
    let issues = texts(&v["issues"]);
    let withheld = text(&v["informationWithheld"]).is_some()
        || float(&v["coordinateUncertaintyInMeters"]).is_some_and(|m| m > MAX_UNCERTAINTY_M)
        || text(&v["dataGeneralizations"]).is_some()
        || v["hasGeospatialIssues"].as_bool() == Some(true)
        || issues.iter().any(|i| BAD_COORDINATES.contains(&i.as_str()));
    if withheld {
        return None;
    }
    Point::new(
        float(&v["decimalLatitude"])?,
        float(&v["decimalLongitude"])?,
    )
}

/// An occurrence: one record of an organism at a place and time.
pub fn parse_occurrence(status: u16, raw: &[u8], key: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    checked(&v, key, "occurrence")?;
    let object = Object::Occurrence(key);
    let mut card = Card::new("Observation", identifier(object), page(object));
    card.name = text(&v["scientificName"]);
    card.point = occurrence_point(&v);
    let date = text(&v["eventDate"]);
    if let Some(date) = &date {
        match date.split_once('/') {
            Some((from, to)) => {
                card.start = event_time(from, None);
                card.end = event_time(to, None);
            }
            None => card.start = event_time(date, v["eventTime"].as_str()),
        }
    }
    card.detail("event_date", date);
    card.detail("basis_of_record", text(&v["basisOfRecord"]));
    card.detail("occurrence_status", text(&v["occurrenceStatus"]));
    card.detail("dataset_key", text(&v["datasetKey"]));
    card.detail("dataset_name", text(&v["datasetName"]));
    card.detail("publisher", text(&v["publishingOrgKey"]));
    card.detail("recorded_by", text(&v["recordedBy"]));
    card.detail("country_code", text(&v["countryCode"]));
    card.detail("taxon_key", int(&v["taxonKey"]));
    card.detail("taxon_rank", text(&v["taxonRank"]));
    card.detail("uncertainty_m", float(&v["coordinateUncertaintyInMeters"]));
    card.detail("information_withheld", text(&v["informationWithheld"]));
    card.detail("data_generalizations", text(&v["dataGeneralizations"]));
    let licence = text(&v["license"]).map(|l| licence(&l));
    card.detail("license", licence.clone());
    let holder = text(&v["rightsHolder"]).or_else(|| text(&v["recordedBy"]));
    let credit = [holder, text(&v["datasetName"]), licence]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    card.attribution = Some(if credit.is_empty() {
        "via GBIF.org".into()
    } else {
        format!("{credit}, via GBIF.org")
    });
    let occurrence_id = text(&v["occurrenceID"]);
    if let Some(id) = occurrence_id
        .as_deref()
        .and_then(super::biodiversity::inaturalist_observation)
    {
        card.detail("inat_observation", id);
    }
    card.detail("occurrence_id", occurrence_id);
    Ok(Some(card))
}

/// The source: GBIF species and occurrences, one request per card.
pub struct Gbif;

#[async_trait]
impl Source for Gbif {
    fn name(&self) -> &'static str {
        "gbif"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "gbif",
                uri: "https://www.gbif.org/species/1341976",
                card_type: "Taxon",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "api.gbif.org",
                path: "/v1/species/1341976",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "species-1341976.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(target(uri)?);
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("gbif:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            // Species
            "canonical_name",
            "authorship",
            "rank",
            "taxonomic_status",
            "kingdom",
            "phylum",
            "class",
            "order",
            "family",
            "genus",
            "vernacular_name",
            "accepted_key",
            "accepted_name",
            "parent_key",
            "published_in",
            "dataset_key",
            // Occurrences
            "event_date",
            "basis_of_record",
            "occurrence_status",
            "dataset_name",
            "publisher",
            "recorded_by",
            "country_code",
            "taxon_key",
            "taxon_rank",
            "uncertainty_m",
            "information_withheld",
            "data_generalizations",
            "license",
            "occurrence_id",
            "inat_observation",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("api.gbif.org", 1.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.gbif.org/species/1341976",
                card_type: "Taxon",
            },
            Example {
                uri: "https://www.gbif.org/occurrence/5938027305",
                card_type: "Observation",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(object) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = super::kit::fetch(cx, &request(object).context("no request")?).await?;
        Ok(match parse(f.status, &f.body, object)? {
            Some(card) => card.into_doc(cx.card(uri, self), SITE),
            None => cx.missing(uri, self),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("gbif", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).map(identifier)
    }

    #[test]
    fn link_forms() {
        for uri in [
            "https://www.gbif.org/species/1341976",
            "https://gbif.org/species/1341976/",
            "http://www.gbif.org/species/1341976",
            "https://www.gbif.org/species/1341976/metrics",
            "https://www.gbif.org/species/1341976/verbatim",
            "https://www.gbif.org/es/species/1341976",
            "https://www.gbif.org/zh-tw/species/1341976?q=x#top",
            "https://www.gbif.org/species/01341976",
            "https://api.gbif.org/v1/species/1341976",
        ] {
            assert_eq!(id(uri).as_deref(), Some("gbif:species/1341976"), "{uri}");
        }
        for uri in [
            "https://www.gbif.org/occurrence/5938027305",
            "http://www.gbif.org/occurrence/5938027305",
            "https://www.gbif.org/occurrence/5938027305/verbatim",
            "https://www.gbif.org/fr/occurrence/5938027305",
            "https://api.gbif.org/v1/occurrence/5938027305",
        ] {
            assert_eq!(
                id(uri).as_deref(),
                Some("gbif:occurrence/5938027305"),
                "{uri}"
            );
        }
        let o = from_uri("https://www.gbif.org/es/species/1341976/metrics").unwrap();
        assert_eq!(page(o), "https://www.gbif.org/species/1341976");
        assert_eq!(
            request(o).unwrap().url.as_str(),
            "https://api.gbif.org/v1/species/1341976"
        );
        assert_eq!(
            request(Object::Occurrence(5938027305))
                .unwrap()
                .url
                .as_str(),
            "https://api.gbif.org/v1/occurrence/5938027305"
        );
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://www.gbif.org/species/0",
            "https://www.gbif.org/species/-1",
            "https://www.gbif.org/species/abc",
            "https://www.gbif.org/species/1341976/children",
            "https://www.gbif.org/species/search?q=apis",
            "https://www.gbif.org/dataset/50c9509d-22c7-4a22-a47d-8c48425ef4a7",
            "https://www.gbif.org/english/species/1341976",
            "https://api.gbif.org/v1/species/1341976/vernacularNames",
            "https://api.gbif.org/v2/species/1341976",
            "https://www.gbif.org/occurrence/99999999999999999999",
            "https://www.gbif.es/species/1341976",
            "https://www.inaturalist.org/observations/333069440",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn species() {
        let card = parse_species(200, &read("species-1341976.json"), 1341976)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                card.type_name,
                card.id.as_str(),
                card.url.as_str(),
                card.name.as_deref()
            ),
            (
                "Taxon",
                "gbif:species/1341976",
                "https://www.gbif.org/species/1341976",
                Some("Apis mellifera Linnaeus, 1758")
            )
        );
        assert_eq!(card.details["rank"], "SPECIES");
        assert_eq!(card.details["taxonomic_status"], "ACCEPTED");
        assert_eq!(card.details["class"], "Insecta");
        assert_eq!(card.details["genus"], "Apis");
        assert_eq!(card.details["vernacular_name"], "western honey bee");
        assert!(!card.details.contains_key("accepted_key"));
        assert_eq!((card.point, card.start), (None, None));
        assert_eq!(
            card.attribution.as_deref(),
            Some("GBIF Backbone Taxonomy (CC BY 4.0), via GBIF.org")
        );

        let synonym = parse_species(200, &read("species-10951632-synonym.json"), 10951632)
            .unwrap()
            .unwrap();
        assert_eq!(
            synonym.name.as_deref(),
            Some("Apis hirtipes Fabricius, 1793")
        );
        assert_eq!(synonym.details["taxonomic_status"], "SYNONYM");
        assert_eq!(synonym.details["accepted_key"], 5039645);
        assert_eq!(
            synonym.details["accepted_name"],
            "Epicharis rustica (Olivier, 1789)"
        );

        assert_eq!(
            parse_species(404, &read("species-missing.json"), 999999999).unwrap(),
            None
        );
        assert!(parse_species(200, &read("species-1341976.json"), 1341977).is_err());
        assert!(parse_species(429, b"", 1341976).is_err());
        assert!(parse_species(503, b"", 1341976).is_err());
    }

    #[test]
    fn occurrence() {
        let card = parse_occurrence(200, &read("occurrence-5938027305.json"), 5938027305)
            .unwrap()
            .unwrap();
        assert_eq!(
            (card.type_name, card.id.as_str(), card.name.as_deref()),
            (
                "Observation",
                "gbif:occurrence/5938027305",
                Some("Euphagus cyanocephalus (Wagler, 1829)")
            )
        );
        assert_eq!(card.url, "https://www.gbif.org/occurrence/5938027305");
        let p = card.point.unwrap();
        assert_eq!((p.lat, p.lon), (33.19534, -117.384758));
        // eventDate is local time; eventTime carries the offset (-08:00).
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "2026-01-01T23:09:23+00:00"
        );
        assert_eq!(card.details["basis_of_record"], "HUMAN_OBSERVATION");
        assert_eq!(card.details["recorded_by"], "ramchop");
        assert_eq!(card.details["country_code"], "US");
        assert_eq!(card.details["license"], "CC BY-NC 4.0");
        assert_eq!(card.details["taxon_key"], 2484183);
        assert_eq!(card.details["uncertainty_m"], 5.0);
        assert_eq!(
            card.details["publisher"],
            "28eb1a3f-1c15-4a95-931a-4af90ecb574d"
        );
        assert_eq!(
            card.attribution.as_deref(),
            Some("ramchop, iNaturalist research-grade observations, CC BY-NC 4.0, via GBIF.org")
        );
        assert!(card.keys.is_empty(), "another service's object is a fact");
        assert_eq!(card.details["inat_observation"], 333069440);

        assert_eq!(
            parse_occurrence(404, &read("occurrence-missing.txt"), 999999999999).unwrap(),
            None
        );
        assert!(parse_occurrence(200, &read("occurrence-5938027305.json"), 5938027306).is_err());
        assert!(parse_occurrence(500, b"", 5938027305).is_err());
        assert!(parse_occurrence(200, b"<html>", 5938027305).is_err());
    }

    #[test]
    fn uncertain_coordinates_give_no_point() {
        let raw = read("occurrence-5938027305.json");
        let with = |metres: f64| {
            let mut v: Value = serde_json::from_slice(&raw).unwrap();
            v["coordinateUncertaintyInMeters"] = metres.into();
            let raw = serde_json::to_vec(&v).unwrap();
            parse_occurrence(200, &raw, 5938027305).unwrap().unwrap()
        };
        assert!(with(MAX_UNCERTAINTY_M).point.is_some());
        let vague = with(1500.0);
        assert_eq!(vague.point, None, "an area, not a point");
        assert_eq!(vague.details["uncertainty_m"], 1500.0);
    }

    #[test]
    fn generalised_coordinates_give_no_point() {
        let card = parse_occurrence(
            200,
            &read("occurrence-4993788366-obscured.json"),
            4993788366,
        )
        .unwrap()
        .unwrap();
        assert_eq!(card.point, None);
        assert_eq!(card.details["uncertainty_m"], 27816.0);
        assert_eq!(
            card.details["information_withheld"],
            "Coordinate uncertainty increased to 27816m at the request of the observer"
        );
        assert_eq!(card.details["license"], "CC BY 4.0");
        assert_eq!(card.details["inat_observation"], 195404405);
    }

    #[test]
    fn event_dates() {
        let t = |d: &str, et: Option<&str>| event_time(d, et).map(|t| t.to_rfc3339());
        assert_eq!(t("2020-04", None), None);
        assert_eq!(t("2020", None), None);
        assert_eq!(
            t("2020-04-30", None).as_deref(),
            Some("2020-04-30T00:00:00+00:00")
        );
        assert_eq!(
            t("2020-04-30T10:00:00Z", None).as_deref(),
            Some("2020-04-30T10:00:00+00:00")
        );
        // A local time without an offset counts as its date.
        assert_eq!(
            t("2024-01-01T10:16:30", None).as_deref(),
            Some("2024-01-01T00:00:00+00:00")
        );
        assert_eq!(
            t("2024-01-01T10:16:30", Some("10:16:30+13:00")).as_deref(),
            Some("2023-12-31T21:16:30+00:00")
        );
        assert_eq!(
            t("2024-01-01T10:16:30", Some("11:00:00+13:00")).as_deref(),
            Some("2024-01-01T00:00:00+00:00")
        );
        assert_eq!(
            licence("http://creativecommons.org/publicdomain/zero/1.0/legalcode"),
            "CC0 1.0"
        );
    }

    #[test]
    fn roots_and_aliases() {
        let g = Gbif;
        for uri in ["gbif:species/1341976", "gbif:occurrence/5938027305"] {
            assert!(g.reads(uri), "{uri}");
            assert_eq!(g.alias_root(uri), None, "{uri}");
            assert!(!g.invalid(uri), "{uri}");
            assert_eq!(target(uri).map(identifier).as_deref(), Some(uri));
        }
        assert_eq!(
            g.alias_root("https://www.gbif.org/es/species/1341976/metrics")
                .as_deref(),
            Some("gbif:species/1341976")
        );
        assert_eq!(
            g.alias_root("https://api.gbif.org/v1/occurrence/5938027305")
                .as_deref(),
            Some("gbif:occurrence/5938027305")
        );
        for uri in [
            "gbif:species/01341976",
            "gbif:species/0",
            "gbif:species/abc",
            "gbif:dataset/1",
            "gbif:1341976",
            "gbif:occurrence/",
        ] {
            assert!(g.invalid(uri), "{uri}");
            assert!(!g.reads(uri), "{uri}");
        }
        assert!(!g.invalid("https://example.org/gbif:species/x"));
        for example in g.examples() {
            assert!(g.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        let cards = [
            parse_species(200, &read("species-1341976.json"), 1341976),
            parse_species(200, &read("species-10951632-synonym.json"), 10951632),
            parse_occurrence(200, &read("occurrence-5938027305.json"), 5938027305),
            parse_occurrence(
                200,
                &read("occurrence-4993788366-obscured.json"),
                4993788366,
            ),
        ];
        for card in cards {
            let card = card.unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Gbif, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let card = parse_occurrence(200, &read("occurrence-5938027305.json"), 5938027305)
            .unwrap()
            .unwrap();
        let doc = card.into_doc(
            ResourceMeta::new("gbif:occurrence/5938027305", "gbif", 60),
            SITE,
        );
        assert_eq!(doc.attributes["inat_observation"], "333069440");
        assert_eq!(doc.site, "GBIF");
    }
}
