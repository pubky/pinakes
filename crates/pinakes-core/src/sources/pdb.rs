//! Structures in the Protein Data Bank, read from the RCSB PDB Data API by the entry's id.
//!
//! Every RCSB link of one entry (its structure page and the other tabs of it, the legacy
//! `explore.do` page and the Data API's entry URL) is an alias of `pdb:{ID}`: the four-character
//! id in upper case. An extended id (`pdb_00004hhb`) names the same entry while it starts with
//! `pdb_0000`. Coordinate files (`files.rcsb.org`) are files of an entry, not the entry.
//!
//! An obsolete entry is gone from the Data API (404); `status_request` says which entry replaced
//! it. The data is CC0.
use super::kit::{
    float, host_and_segments, int, json, text, texts, time, web_url, Card, Key, Link, Request,
};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

const VIA: &str = "pdb";
pub const ATTRIBUTION: &str = "RCSB PDB (CC0 1.0)";

/// A four-character PDB id (a digit 1–9, then three letters or digits) in upper case, also from
/// its extended form `pdb_0000xxxx`.
pub fn pdb_id(value: &str) -> Option<String> {
    let value = value.to_ascii_uppercase();
    let value = value.strip_prefix("PDB_0000").unwrap_or(&value);
    let b = value.as_bytes();
    (b.len() == 4 && (b'1'..=b'9').contains(&b[0]) && b.iter().all(u8::is_ascii_alphanumeric))
        .then(|| value.to_owned())
}

/// The tabs of an entry's page on rcsb.org.
const TABS: [&str; 6] = [
    "structure",
    "3d-view",
    "sequence",
    "annotations",
    "experimental",
    "versions",
];

/// The entry a link names: rcsb.org entry pages, the legacy `explore.do` page and the Data API.
pub fn from_uri(uri: &str) -> Option<String> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (host.as_str(), s.as_slice()) {
        ("rcsb.org", [tab, id]) if TABS.contains(tab) => pdb_id(id),
        ("rcsb.org", ["pdb", "explore.do"] | ["pdb", "explore", "explore.do"]) => web_url(uri)?
            .query_pairs()
            .find(|(k, _)| k == "structureId")
            .and_then(|(_, v)| pdb_id(&v)),
        ("data.rcsb.org", ["rest", "v1", "core", "entry", id]) => pdb_id(id),
        _ => None,
    }
}

pub fn identifier(id: &str) -> String {
    format!("pdb:{id}")
}

pub fn page(id: &str) -> String {
    format!("https://www.rcsb.org/structure/{id}")
}

/// The entry's record in the Data API.
pub fn request(id: &str) -> Option<Request> {
    Request::get(&format!(
        "https://data.rcsb.org/rest/v1/core/entry/{}",
        pdb_id(id)?
    ))
}

/// Whether an id is current, obsolete or unreleased, and what replaced an obsolete entry.
pub fn status_request(id: &str) -> Option<Request> {
    Request::get(&format!(
        "https://data.rcsb.org/rest/v1/holdings/status/{}",
        pdb_id(id)?
    ))
}

/// The entry that replaced an obsolete one, or `None` when it is not obsolete (or unknown).
pub fn parse_replaced_by(status: u16, raw: &[u8], id: &str) -> Result<Option<String>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        v["rcsb_id"].as_str().and_then(pdb_id).as_deref() == Some(id),
        "the API answered for another entry"
    );
    let holdings = &v["rcsb_repository_holdings_combined"];
    if holdings["status_code"].as_str() != Some("OBS") {
        return Ok(None);
    }
    Ok(holdings["id_code_replaced_by_latest"]
        .as_str()
        .and_then(pdb_id))
}

/// The date part of a Data API timestamp.
fn date(v: &Value) -> Option<String> {
    time(v).map(|t| t.format("%Y-%m-%d").to_string())
}

/// An entry, or `None` when there is no current entry with this id (never assigned, or obsolete).
pub fn parse(status: u16, raw: &[u8], id: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        v["rcsb_id"].as_str().and_then(pdb_id).as_deref() == Some(id),
        "the API answered for another entry"
    );
    let mut card = Card::new("Dataset", identifier(id), page(id));
    card.name = text(&v["struct"]["title"]);
    card.attribution = Some(ATTRIBUTION.into());
    let accession = &v["rcsb_accession_info"];
    card.start = time(&accession["initial_release_date"]);
    card.detail("deposit_date", date(&accession["deposit_date"]));
    card.detail("revision_date", date(&accession["revision_date"]));
    let methods: Vec<String> = v["exptl"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| text(&e["method"]))
        .collect();
    card.detail("methods", methods);
    let info = &v["rcsb_entry_info"];
    card.detail("resolution", float(&info["resolution_combined"][0]));
    card.detail("polymer_entity_count", int(&info["polymer_entity_count"]));
    card.detail("molecular_weight_kda", float(&info["molecular_weight"]));
    let keywords = &v["struct_keywords"];
    card.detail("classification", text(&keywords["pdbx_keywords"]));
    let mut words: Vec<String> = vec![];
    for w in keywords["text"].as_str().unwrap_or("").split(',') {
        let w = w.trim();
        if !w.is_empty() && !words.iter().any(|x| x == w) {
            words.push(w.into());
        }
    }
    card.detail("keywords", words);
    let c = &v["rcsb_primary_citation"];
    let citation: serde_json::Map<String, Value> = [
        ("title", text(&c["title"]).map(Value::from)),
        (
            "journal",
            text(&c["rcsb_journal_abbrev"])
                .or_else(|| text(&c["journal_abbrev"]))
                .map(Value::from),
        ),
        ("year", int(&c["year"]).map(Value::from)),
        (
            "doi",
            text(&c["pdbx_database_id_DOI"]).map(|d| d.to_ascii_lowercase().into()),
        ),
        (
            "pubmed_id",
            int(&c["pdbx_database_id_PubMed"]).map(Value::from),
        ),
        ("authors", Some(json!(texts(&c["rcsb_authors"])))),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.filter(|v| v != &json!([])).map(|v| (k.to_owned(), v)))
    .collect();
    card.detail("citation", Value::Object(citation));
    // The entry's own DOI, as the entry states it.
    for db in v["database_2"].as_array().into_iter().flatten() {
        let ours = db["database_id"].as_str() == Some("PDB")
            && db["database_code"].as_str().and_then(pdb_id).as_deref() == Some(id);
        if let Some(doi) = text(&db["pdbx_DOI"]).filter(|d| ours && d.starts_with("10.")) {
            card.key(Key::new("doi", &doi.to_ascii_lowercase()));
        }
    }
    let lower = id.to_ascii_lowercase();
    card.link(Link::new(
        "PDBe",
        format!("https://www.ebi.ac.uk/pdbe/entry/pdb/{lower}"),
        VIA,
    ));
    card.link(Link::new(
        "PDBj",
        format!("https://pdbj.org/mine/summary/{lower}"),
        VIA,
    ));
    Ok(Some(card))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("pdb", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).map(|i| identifier(&i))
    }

    #[test]
    fn every_link_form_of_one_entry() {
        for uri in [
            "https://www.rcsb.org/structure/4HHB",
            "https://www.rcsb.org/structure/4hhb",
            "https://rcsb.org/structure/4HHB",
            "http://www.rcsb.org/structure/4HHB?foo=1#x",
            "https://www.rcsb.org/3d-view/4HHB",
            "https://www.rcsb.org/sequence/4HHB",
            "https://www.rcsb.org/annotations/4HHB",
            "https://www.rcsb.org/experimental/4HHB",
            "https://www.rcsb.org/versions/4HHB",
            "https://www.rcsb.org/structure/pdb_00004hhb",
            "https://www.rcsb.org/structure/PDB_00004HHB",
            "https://www.rcsb.org/pdb/explore.do?structureId=4hhb",
            "https://www.rcsb.org/pdb/explore/explore.do?structureId=4HHB",
            "https://data.rcsb.org/rest/v1/core/entry/4HHB",
            "https://data.rcsb.org/rest/v1/core/entry/pdb_00004hhb",
        ] {
            assert_eq!(id(uri).as_deref(), Some("pdb:4HHB"), "{uri}");
        }
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://www.rcsb.org/structure/0HHB",
            "https://www.rcsb.org/structure/4HH",
            "https://www.rcsb.org/structure/4HHBB",
            "https://www.rcsb.org/structure/4HH-",
            "https://www.rcsb.org/structure/pdb_12344hhb",
            "https://www.rcsb.org/3d-view/4HHB/1",
            "https://www.rcsb.org/ligand/HEM",
            "https://www.rcsb.org/pdb/explore.do?pdbId=4HHB",
            "https://files.rcsb.org/view/4HHB.cif",
            "https://files.rcsb.org/download/4HHB.pdb",
            "https://www.ebi.ac.uk/pdbe/entry/pdb/4hhb",
            "https://example.org/structure/4HHB",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn requests() {
        assert_eq!(
            request("4HHB").unwrap().url.as_str(),
            "https://data.rcsb.org/rest/v1/core/entry/4HHB"
        );
        assert_eq!(
            status_request("1HHB").unwrap().url.as_str(),
            "https://data.rcsb.org/rest/v1/holdings/status/1HHB"
        );
        assert_eq!(page("4HHB"), "https://www.rcsb.org/structure/4HHB");
    }

    #[test]
    fn haemoglobin() {
        let card = parse(200, &read("entry-4hhb.json"), "4HHB")
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "Dataset");
        assert_eq!(card.id, "pdb:4HHB");
        assert_eq!(card.url, "https://www.rcsb.org/structure/4HHB");
        assert_eq!(
            card.name.as_deref(),
            Some("THE CRYSTAL STRUCTURE OF HUMAN DEOXYHAEMOGLOBIN AT 1.74 ANGSTROMS RESOLUTION")
        );
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "1984-07-17T00:00:00+00:00"
        );
        assert_eq!(card.attribution.as_deref(), Some(ATTRIBUTION));
        let d = &card.details;
        assert_eq!(d["deposit_date"], "1984-03-07");
        assert_eq!(d["methods"], json!(["X-RAY DIFFRACTION"]));
        assert_eq!(d["resolution"], 1.74);
        assert_eq!(d["polymer_entity_count"], 2);
        assert_eq!(d["molecular_weight_kda"], 64.74);
        assert_eq!(d["classification"], "OXYGEN TRANSPORT");
        assert_eq!(d["keywords"], json!(["OXYGEN TRANSPORT"]));
        assert_eq!(d["citation"]["journal"], "J Mol Biol");
        assert_eq!(d["citation"]["year"], 1984);
        assert_eq!(d["citation"]["doi"], "10.1016/0022-2836(84)90472-8");
        assert_eq!(d["citation"]["pubmed_id"], 6726807);
        assert_eq!(card.keys, [Key::new("doi", "10.2210/pdb4hhb/pdb")]);
        let links: Vec<_> = card.also_on.iter().map(|l| l.uri.as_str()).collect();
        assert_eq!(
            links,
            [
                "https://www.ebi.ac.uk/pdbe/entry/pdb/4hhb",
                "https://pdbj.org/mine/summary/4hhb"
            ]
        );
        assert!(parse(200, &read("entry-4hhb.json"), "6VXX").is_err());
    }

    #[test]
    fn spike() {
        let card = parse(200, &read("entry-6vxx.json"), "6VXX")
            .unwrap()
            .unwrap();
        assert_eq!(
            card.name.as_deref(),
            Some("Structure of the SARS-CoV-2 spike glycoprotein (closed state)")
        );
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "2020-03-11T00:00:00+00:00"
        );
        let d = &card.details;
        assert_eq!(d["methods"], json!(["ELECTRON MICROSCOPY"]));
        assert_eq!(d["resolution"], 2.8);
        assert_eq!(d["keywords"][0], "Coronavirus");
        assert_eq!(d["keywords"].as_array().unwrap().len(), 9);
        assert_eq!(d["citation"]["doi"], "10.1016/j.cell.2020.02.058");
        assert_eq!(card.keys, [Key::new("doi", "10.2210/pdb6vxx/pdb")]);
    }

    #[test]
    fn missing_and_obsolete() {
        assert_eq!(
            parse(404, &read("entry-missing.json"), "9999").unwrap(),
            None
        );
        assert_eq!(
            parse(404, &read("entry-1hhb-obsolete.json"), "1HHB").unwrap(),
            None
        );
        assert_eq!(
            parse_replaced_by(200, &read("status-1hhb.json"), "1HHB")
                .unwrap()
                .as_deref(),
            Some("4HHB")
        );
        assert!(parse_replaced_by(200, &read("status-1hhb.json"), "4HHB").is_err());
        assert!(parse(429, b"", "4HHB").is_err());
        assert!(parse(503, b"", "4HHB").is_err());
        assert!(parse(200, b"<html>", "4HHB").is_err());
    }
}
