//! Zenodo records, read from the Zenodo API by the record id in the link.
//!
//! Every zenodo.org link of one record (`/records/{id}`, the legacy `/record/{id}` and the API's
//! `/api/records/{id}`) is an alias of `zenodo:{id}`. A record is one version of a work; the
//! work's concept record (all its versions) has an id of its own, which the API redirects to the
//! latest version. A concept id's card is the concept: the latest version's title and text,
//! under the concept's identifier, page and concept DOI. A version's card names its concept in
//! `concept_record` and `concept_doi`.
//!
//! The DOI comes from the answer, never from the id: records can carry a publisher's DOI.
//! Links to a record's files name the files, not the record. `sandbox.zenodo.org` is a test
//! service, and DOI links (`doi.org/10.5281/zenodo.{id}`) belong to the DOI source.
use super::{
    kit::{
        credit, fetch, host_and_segments, html_text, int, json, number, text, texts, Card, Key,
        Request,
    },
    Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;

pub const SITE: &str = "Zenodo";

/// A record id: decimal digits without a leading zero (the API has no record `07790295`).
fn record_id(s: &str) -> Option<u64> {
    number(s, 12).filter(|_| !s.starts_with('0'))
}

/// The record a zenodo.org link names.
pub fn from_uri(uri: &str) -> Option<u64> {
    let (host, segments) = host_and_segments(uri)?;
    if host != "zenodo.org" {
        return None;
    }
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match s.as_slice() {
        ["records" | "record", id] | ["api", "records", id] => record_id(id),
        _ => None,
    }
}

pub fn identifier(id: u64) -> String {
    format!("zenodo:{id}")
}

/// The record of a root (`zenodo:{id}`).
pub fn from_root(uri: &str) -> Option<u64> {
    record_id(uri.strip_prefix("zenodo:")?)
}

/// The record of a root or a zenodo.org link.
pub fn target(uri: &str) -> Option<u64> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// The record's page on zenodo.org.
pub fn page(id: u64) -> String {
    format!("https://zenodo.org/records/{id}")
}

/// The record's API record. A concept id redirects to its latest version, which the parser
/// recognises by its `conceptrecid`.
pub fn request(id: u64) -> Option<Request> {
    Some(Request::get(&format!("https://zenodo.org/api/records/{id}"))?.accept("application/json"))
}

/// The schema.org type of a Zenodo resource type and subtype.
fn type_name(kind: &str, subtype: &str) -> &'static str {
    match (kind, subtype) {
        ("dataset", _) => "Dataset",
        ("software", _) => "SoftwareSourceCode",
        ("publication", "book") => "Book",
        ("publication", "section") => "Chapter",
        ("publication", "report" | "technicalnote" | "deliverable" | "workingpaper") => "Report",
        ("publication", "thesis") => "Thesis",
        ("publication", _) => "ScholarlyArticle",
        ("image", _) => "ImageObject",
        ("video", _) => "VideoObject",
        _ => "CreativeWork",
    }
}

/// A Zenodo licence id as people write it: `cc-by-4.0` is "CC BY 4.0", `cc-zero` is "CC0 1.0";
/// other ids stay as they are.
pub fn licence_name(id: &str) -> String {
    if matches!(id, "cc-zero" | "cc0-1.0") {
        return "CC0 1.0".into();
    }
    match id
        .strip_prefix("cc-")
        .and_then(|rest| rest.rsplit_once('-'))
    {
        Some((terms, version)) if version.starts_with(|c: char| c.is_ascii_digit()) => {
            format!("CC {} {version}", terms.to_ascii_uppercase())
        }
        _ => id.into(),
    }
}

/// Whether a licence lets us show the work's image: CC0, or CC BY without further terms.
fn open_licence(id: &str) -> bool {
    matches!(id, "cc-zero" | "cc0-1.0")
        || id
            .strip_prefix("cc-by-")
            .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
}

/// A record, or `None` when Zenodo has no such record (404) or it was deleted (410).
pub fn parse(status: u16, raw: &[u8], id: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404, 410])? else {
        return Ok(None);
    };
    let answered = int(&v["id"]);
    let concept = int(&v["conceptrecid"]);
    let wanted = i64::try_from(id).ok();
    let is_concept = if answered.is_some() && answered == wanted {
        false
    } else if concept.is_some() && concept == wanted {
        true
    } else {
        bail!("the API answered for another record");
    };
    let m = &v["metadata"];
    let kind = text(&m["resource_type"]["type"]).unwrap_or_default();
    let subtype = text(&m["resource_type"]["subtype"]).unwrap_or_default();
    let mut card = Card::new(type_name(&kind, &subtype), identifier(id), page(id));
    card.name = text(&m["title"]);
    card.description = m["description"].as_str().and_then(html_text);
    let licence = text(&m["license"]["id"]);
    let open = text(&m["access_right"]).as_deref() == Some("open");
    if open && licence.as_deref().is_some_and(open_licence) {
        card.image = text(&v["links"]["thumbnails"]["750"]);
    }
    let creators: Vec<String> = m["creators"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| text(&c["name"]))
        .collect();
    card.attribution = credit(&creators, licence.as_deref().map(licence_name).as_deref());
    card.detail("creators", creators);
    card.detail("license", licence);
    card.detail("resource_type", text(&m["resource_type"]["title"]));
    card.detail("access_right", text(&m["access_right"]));
    let communities: Vec<String> = m["communities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| text(&c["id"]))
        .collect();
    card.detail("communities", communities);
    card.detail("keywords", texts(&m["keywords"]));
    let concept_doi = text(&v["conceptdoi"]).map(|d| d.to_ascii_lowercase());
    if is_concept {
        card.detail("latest_version", answered);
        if let Some(doi) = concept_doi {
            card.key(Key::new("doi", &doi));
        }
        return Ok(Some(card));
    }
    card.detail("publication_date", text(&m["publication_date"]));
    card.detail("version", text(&m["version"]));
    card.detail("concept_record", concept);
    card.detail("concept_doi", concept_doi);
    if let Some(doi) = text(&v["doi"]).or_else(|| text(&m["doi"])) {
        card.key(Key::new("doi", &doi.to_ascii_lowercase()));
    }
    Ok(Some(card))
}

/// The source: the Zenodo REST API.
pub struct Zenodo;

#[async_trait]
impl Source for Zenodo {
    fn name(&self) -> &'static str {
        "zenodo"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "zenodo",
                uri: "https://zenodo.org/records/1234567",
                card_type: "ScholarlyArticle",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "zenodo.org",
                path: "/api/records/1234567",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "record-1234567.json",
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
        uri.starts_with("zenodo:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "doi",
            "creators",
            "license",
            "resource_type",
            "access_right",
            "communities",
            "keywords",
            "latest_version",
            "publication_date",
            "version",
            "concept_record",
            "concept_doi",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("zenodo.org", 2.2)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://zenodo.org/records/1234567",
            card_type: "ScholarlyArticle",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(id).context("record id")?).await?;
        Ok(
            match crate::resolver::html::blocking(move || parse(f.status, &f.body, id)).await?? {
                Some(card) => card.into_doc(cx.card(uri, self), SITE),
                None => cx.missing(uri, self),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("zenodo", name)
    }

    #[test]
    fn every_link_form_of_one_record() {
        for uri in [
            "https://zenodo.org/records/7790295",
            "https://zenodo.org/records/7790295?preview=1",
            "https://zenodo.org/records/7790295#files",
            "https://www.zenodo.org/records/7790295",
            "https://zenodo.org/record/7790295",
            "http://zenodo.org/record/7790295/",
            "https://zenodo.org/api/records/7790295",
        ] {
            assert_eq!(
                from_uri(uri).map(identifier).as_deref(),
                Some("zenodo:7790295"),
                "{uri}"
            );
        }
        assert_eq!(page(7790295), "https://zenodo.org/records/7790295");
        let r = request(7790295).unwrap();
        assert_eq!(r.url.as_str(), "https://zenodo.org/api/records/7790295");
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://zenodo.org/records/07790295",
            "https://zenodo.org/records/abc",
            "https://zenodo.org/records/7790295/files/Flashfunds.MOV",
            "https://zenodo.org/records/7790295/files/Flashfunds.MOV?download=1",
            "https://zenodo.org/api/records/7790295/files",
            "https://zenodo.org/records/7790295/latest",
            "https://zenodo.org/api/records/7790294/versions/latest",
            "https://zenodo.org/communities/zenodo",
            "https://zenodo.org/doi/10.5281/zenodo.7790294",
            "https://sandbox.zenodo.org/records/1234567",
            "https://doi.org/10.5281/zenodo.7790295",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn article_with_a_publishers_doi() {
        let card = parse(200, &read("record-1234567.json"), 1234567)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "ScholarlyArticle");
        assert_eq!(card.id, "zenodo:1234567");
        assert_eq!(card.url, "https://zenodo.org/records/1234567");
        assert_eq!(
            card.name.as_deref(),
            Some("Environment and Early Evolution of the 8 May 2009 Derecho-Producing Convective System")
        );
        assert!(card
            .description
            .unwrap()
            .starts_with("This study documents the complex environment"));
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["publication_date"], "2011-04-01");
        assert_eq!(card.keys, [Key::new("doi", "10.1175/2010mwr3413.1")]);
        assert_eq!(
            card.attribution.as_deref(),
            Some("Coniglio, Michael C.; Corfidi, Stephen F.; Kain, John S. / CC0 1.0")
        );
        assert_eq!(
            card.image.as_deref(),
            Some(
                "https://zenodo.org/api/iiif/record:1234567:article.pdf/full/%5E750,/0/default.jpg"
            )
        );
        assert_eq!(
            card.details["creators"],
            serde_json::json!([
                "Coniglio, Michael C.",
                "Corfidi, Stephen F.",
                "Kain, John S."
            ])
        );
        assert_eq!(card.details["resource_type"], "Journal article");
        assert_eq!(card.details["concept_record"], 1234566);
        assert!(!card.details.contains_key("concept_doi"));
        assert!(parse(200, &read("record-1234567.json"), 7790295).is_err());
        assert!(parse(429, b"", 1234567).is_err());
        assert!(parse(503, b"", 1234567).is_err());
    }

    #[test]
    fn software_version() {
        let card = parse(200, &read("record-7790295.json"), 7790295)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "SoftwareSourceCode");
        assert_eq!(card.name.as_deref(), Some("Flash Funds"));
        assert_eq!(
            card.description.as_deref(),
            Some("Flash funds | SQR400 Flash | Bitcoin Flash | Bank Account Flash\nVisit: https://flashfunds.app")
        );
        assert_eq!(card.keys, [Key::new("doi", "10.5281/zenodo.7790295")]);
        assert_eq!(card.details["version"], "5.8");
        assert_eq!(card.details["concept_doi"], "10.5281/zenodo.7790294");
        assert_eq!(card.details["license"], "cc-by-4.0");
        assert_eq!(card.attribution.as_deref(), Some("Flash Funds / CC BY 4.0"));
        assert_eq!(card.image, None, "no thumbnail for a video file");
    }

    #[test]
    fn concept_record() {
        // The API redirects the concept id to the latest version.
        assert!(parse(302, &read("concept-7790294.html"), 7790294).is_err());
        let card = parse(200, &read("concept-7790294-followed.json"), 7790294)
            .unwrap()
            .unwrap();
        assert_eq!(card.id, "zenodo:7790294");
        assert_eq!(card.url, "https://zenodo.org/records/7790294");
        assert_eq!(card.name.as_deref(), Some("Flash Funds"));
        assert_eq!(card.keys, [Key::new("doi", "10.5281/zenodo.7790294")]);
        assert_eq!(card.details["latest_version"], 7790295);
        assert_eq!(
            card.start, None,
            "the latest version's date is not the concept's"
        );
        assert!(!card.details.contains_key("version"));
    }

    #[test]
    fn images_in_a_community() {
        let card = parse(200, &read("record-13882307.json"), 13882307)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "ImageObject");
        assert_eq!(
            card.details["communities"],
            serde_json::json!(["the-turing-way"])
        );
        assert_eq!(card.details["concept_doi"], "10.5281/zenodo.3332807");
        assert_eq!(
            card.image.as_deref(),
            Some("https://zenodo.org/api/iiif/record:13882307:open-development.jpg/full/%5E750,/0/default.jpg")
        );
        let description = card.description.unwrap();
        assert!(
            description.starts_with(
                "Illustrations created by Scriberia as part of the Turing Way book dashes"
            ),
            "{description}"
        );
    }

    #[test]
    fn missing_record() {
        assert_eq!(
            parse(404, &read("record-missing.json"), 99999999999).unwrap(),
            None
        );
        assert_eq!(parse(410, b"", 1).unwrap(), None);
    }

    #[test]
    fn licences_and_text() {
        assert_eq!(licence_name("cc-by-sa-4.0"), "CC BY-SA 4.0");
        assert_eq!(licence_name("cc-by-nc-nd-3.0"), "CC BY-NC-ND 3.0");
        assert_eq!(licence_name("mit"), "mit");
        assert!(open_licence("cc-by-4.0"));
        assert!(open_licence("cc-zero"));
        assert!(!open_licence("cc-by-sa-4.0"));
        assert!(!open_licence("cc-by-nc-4.0"));
        assert!(!open_licence("mit"));
        for (kind, subtype, name) in [
            ("publication", "book", "Book"),
            ("publication", "report", "Report"),
            ("publication", "preprint", "ScholarlyArticle"),
            ("poster", "", "CreativeWork"),
            ("lesson", "", "CreativeWork"),
            ("video", "", "VideoObject"),
            ("dataset", "", "Dataset"),
        ] {
            assert_eq!(type_name(kind, subtype), name);
        }
    }

    #[test]
    fn roots_and_aliases() {
        let z = Zenodo;
        assert!(z.reads("zenodo:1234567"));
        assert_eq!(z.alias_root("zenodo:1234567"), None);
        assert!(!z.invalid("zenodo:1234567"));
        assert_eq!(
            target("zenodo:1234567").map(identifier).as_deref(),
            Some("zenodo:1234567")
        );
        assert_eq!(
            z.alias_root("https://zenodo.org/record/1234567").as_deref(),
            Some("zenodo:1234567")
        );
        for uri in [
            "zenodo:01234567",
            "zenodo:abc",
            "zenodo:",
            "zenodo:1234567/files",
        ] {
            assert!(z.invalid(uri), "{uri}");
            assert!(!z.reads(uri), "{uri}");
        }
        assert!(!z.invalid("https://zenodo.org/records/zenodo:1"));
        for example in z.examples() {
            assert!(z.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, id) in [
            ("record-1234567.json", 1234567),
            ("record-7790295.json", 7790295),
            ("record-13882307.json", 13882307),
            ("concept-7790294-followed.json", 7790294),
        ] {
            let card = parse(200, &read(file), id).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Zenodo, &card);
            assert_eq!(doc.attributes["page_url"], card.url, "{file}");
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let card = parse(200, &read("record-1234567.json"), 1234567)
            .unwrap()
            .unwrap();
        let doc = crate::sources::kit::checked(&Zenodo, &card);
        assert_eq!(doc.attributes["doi"], "10.1175/2010mwr3413.1");
        assert_eq!(
            doc.attributes["creators"],
            "Coniglio, Michael C.; Corfidi, Stephen F.; Kain, John S."
        );
    }
}
