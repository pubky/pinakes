//! Proteins in UniProtKB, read from the UniProt REST API by accession.
//!
//! Every UniProt link of one entry (its page and the page's tabs, the legacy `/uniprot/` page,
//! the REST record in its full formats and the PURL) is an alias of `uniprot:{ACC}`. A link by
//! entry name (`HBA_HUMAN`) names the same entry once `name_request` finds exactly one current
//! entry with that name. A FASTA or GFF file is the sequence or features, not the entry.
//!
//! An accession is read through UniProt's search by accession, which answers with the entry's
//! fields and never redirects: the entry endpoint redirects a merged accession to the full record
//! of the entry it was merged into, which can be larger than a card may read. A merged accession
//! answers with the entry it was merged into, which is then read: UniProt's own record is the
//! proof, so the link becomes an alias of that entry. A demerged accession gets its own card
//! naming the accessions that replace it, and a deleted one is missing. The data is CC BY 4.0,
//! credited to UniProt.
use super::{
    kit::{encode, fetch, host_and_segments, int, json, text, Card, Link, Request},
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

pub const SITE: &str = "UniProt";
const VIA: &str = "uniprot";
pub const ATTRIBUTION: &str = "UniProt (CC BY 4.0)";
/// How many PDB structures the card lists.
const PDB_IDS: usize = 5;
/// The fields of an entry the card reads (the full record is up to about 1 MB).
const FIELDS: &str = "accession,id,protein_name,gene_primary,organism_name,organism_id,length,\
                      cc_function,xref_alphafolddb,xref_pdb,date_created,date_modified,\
                      sequence_version";

/// What a link names: an entry by accession, or by entry name (which needs `name_request`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Accession(String),
    Name(String),
}

/// A UniProtKB accession in upper case:
/// `[OPQ][0-9][A-Z0-9]{3}[0-9]|[A-NR-Z][0-9]([A-Z][A-Z0-9]{2}[0-9]){1,2}`.
pub fn accession(value: &str) -> Option<String> {
    let v = value.to_ascii_uppercase();
    let b = v.as_bytes();
    let (digit, alnum, upper) = (
        |c: u8| c.is_ascii_digit(),
        |c: u8| c.is_ascii_uppercase() || c.is_ascii_digit(),
        |c: u8| c.is_ascii_uppercase(),
    );
    let ok = match b {
        [b'O' | b'P' | b'Q', rest @ ..] if rest.len() == 5 => {
            digit(rest[0]) && rest[1..4].iter().all(|c| alnum(*c)) && digit(rest[4])
        }
        [first, d, rest @ ..] if upper(*first) && digit(*d) && matches!(rest.len(), 4 | 8) => {
            !matches!(first, b'O' | b'P' | b'Q')
                && rest
                    .chunks(4)
                    .all(|g| upper(g[0]) && alnum(g[1]) && alnum(g[2]) && digit(g[3]))
        }
        _ => false,
    };
    ok.then_some(v)
}

/// A UniProtKB entry name (`HBA_HUMAN`, `Q9TQ74_HUMAN`) in upper case.
pub fn entry_name(value: &str) -> Option<String> {
    let v = value.to_ascii_uppercase();
    let (code, species) = v.split_once('_')?;
    let ok = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|c| c.is_ascii_alphanumeric())
    };
    (ok(code, 10) && ok(species, 5)).then_some(v)
}

fn by_id(id: &str) -> Option<Target> {
    accession(id)
        .map(Target::Accession)
        .or_else(|| entry_name(id).map(Target::Name))
}

/// An id with one of the given format suffixes, or none.
fn strip<'a>(segment: &'a str, formats: &[&str]) -> Option<&'a str> {
    match segment.rsplit_once('.') {
        Some((id, ext)) => formats.contains(&ext).then_some(id),
        None => Some(segment),
    }
}

/// The tabs of an entry's page on uniprot.org.
const TABS: [&str; 7] = [
    "entry",
    "feature-viewer",
    "variant-viewer",
    "publications",
    "external-links",
    "history",
    "genomic-coordinates",
];

/// What a UniProt link names: entry pages (current and legacy), the REST record and the PURL.
pub fn from_uri(uri: &str) -> Option<Target> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    let formats = ["txt", "xml", "json"];
    match (host.as_str(), s.as_slice()) {
        ("uniprot.org", ["uniprotkb", id]) | ("uniprot.org", ["uniprot", id]) => {
            by_id(strip(id, &formats)?)
        }
        ("uniprot.org", ["uniprotkb", id, tab]) if TABS.contains(tab) => by_id(id),
        ("rest.uniprot.org", ["uniprotkb", id]) => by_id(strip(id, &formats)?),
        ("purl.uniprot.org", ["uniprot", id]) => accession(id).map(Target::Accession),
        _ => None,
    }
}

/// The identifier, or `None` for an entry name (its accession comes from `name_request`).
pub fn identifier(target: &Target) -> Option<String> {
    match target {
        Target::Accession(a) => Some(format!("uniprot:{a}")),
        Target::Name(_) => None,
    }
}

pub fn page(acc: &str) -> String {
    format!("https://www.uniprot.org/uniprotkb/{acc}/entry")
}

/// The entry of an accession, through the search by accession: the entry's fields, or for an
/// inactive accession its own record (never a redirect).
pub fn request(acc: &str) -> Option<Request> {
    Request::get(&format!(
        "https://rest.uniprot.org/uniprotkb/search?query={}&fields={}&format=json&size=2",
        encode(&format!("accession:{}", accession(acc)?)),
        encode(FIELDS)
    ))
}

/// The current entries with an entry name (a search, since former names still match).
pub fn name_request(name: &str) -> Option<Request> {
    let query = format!("id:{} AND active:true", entry_name(name)?);
    Request::get(&format!(
        "https://rest.uniprot.org/uniprotkb/search?query={}&fields=accession%2Cid&format=json&size=2",
        encode(&query)
    ))
}

/// The accession of the one current entry with this name, or `None` when there is none, or the
/// name is a former name of an entry.
pub fn parse_name(status: u16, raw: &[u8], name: &str) -> Result<Option<String>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let results = v["results"].as_array().context("no results")?;
    let [entry] = results.as_slice() else {
        return Ok(None);
    };
    let current = entry["uniProtkbId"].as_str().and_then(entry_name);
    if current.as_deref() != entry_name(name).as_deref() || entry["entryType"] == "Inactive" {
        return Ok(None);
    }
    Ok(entry["primaryAccession"].as_str().and_then(accession))
}

/// What UniProt holds for an accession.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// The entry's card; a demerged accession's card names the accessions that replace it.
    Card(Box<Card>),
    /// The accession was merged into this entry.
    MergedInto(String),
}

/// The entry of `acc` from the search by accession, or `None` when UniProt has no such
/// accession or deleted it. The answer must hold exactly that accession (or nothing).
pub fn parse(status: u16, raw: &[u8], acc: &str) -> Result<Option<Entry>> {
    let Some(v) = json(status, raw, &[])? else {
        return Ok(None);
    };
    let acc = accession(acc).context("not an accession")?;
    let results = v["results"].as_array().context("no results")?;
    let found: Vec<&Value> = results
        .iter()
        .filter(|r| {
            r["primaryAccession"]
                .as_str()
                .and_then(accession)
                .as_deref()
                == Some(&acc)
        })
        .collect();
    match found.as_slice() {
        [] if results.is_empty() => Ok(None),
        [entry] if results.len() == 1 => self::entry(entry, &acc),
        _ => bail!("the search answered for other entries"),
    }
}

/// One entry of a search answer.
fn entry(v: &Value, acc: &str) -> Result<Option<Entry>> {
    let mut card = Card::new("Protein", format!("uniprot:{acc}"), page(acc));
    card.attribution = Some(ATTRIBUTION.into());
    let entry_type = v["entryType"].as_str().context("no entry type")?;
    if entry_type == "Inactive" {
        let reason = &v["inactiveReason"];
        let kind = reason["inactiveReasonType"]
            .as_str()
            .context("no inactive reason")?
            .to_ascii_lowercase();
        if kind == "deleted" {
            return Ok(None);
        }
        let to: Vec<String> = reason["mergeDemergeTo"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().and_then(accession))
            .collect();
        if let ("merged", [into]) = (kind.as_str(), to.as_slice()) {
            return Ok(Some(Entry::MergedInto(into.clone())));
        }
        card.detail("inactive", kind);
        card.detail("replaced_by", to);
        return Ok(Some(Entry::Card(Box::new(card))));
    }
    let protein = &v["proteinDescription"];
    card.name = text(&protein["recommendedName"]["fullName"]["value"])
        .or_else(|| text(&protein["submissionNames"][0]["fullName"]["value"]));
    card.description = v["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["commentType"] == "FUNCTION" && c.get("molecule").is_none())
        .map(|c| {
            c["texts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| text(&t["value"]))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .filter(|t| !t.is_empty());
    let audit = &v["entryAudit"];
    card.detail("first_public", text(&audit["firstPublicDate"]));
    card.detail("entry_name", text(&v["uniProtkbId"]));
    let genes: Vec<String> = v["genes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| text(&g["geneName"]["value"]))
        .collect();
    match genes.as_slice() {
        [gene] => card.detail("gene", gene.as_str()),
        _ => card.detail("genes", genes),
    }
    card.detail("organism", text(&v["organism"]["scientificName"]));
    card.detail("taxon_id", int(&v["organism"]["taxonId"]));
    card.detail("length", int(&v["sequence"]["length"]));
    card.detail(
        "reviewed",
        if entry_type.contains("Swiss-Prot") {
            Some(true)
        } else if entry_type.contains("TrEMBL") {
            Some(false)
        } else {
            None
        },
    );
    card.detail("sequence_version", int(&audit["sequenceVersion"]));
    card.detail("last_updated", text(&audit["lastAnnotationUpdateDate"]));
    let xrefs = v["uniProtKBCrossReferences"].as_array();
    let ids = |db: &'static str| {
        xrefs
            .into_iter()
            .flatten()
            .filter(move |x| x["database"] == db)
            .filter_map(|x| text(&x["id"]))
    };
    let pdb: Vec<String> = ids("PDB").collect();
    card.detail(
        "pdb_ids",
        pdb.iter().take(PDB_IDS).cloned().collect::<Vec<_>>(),
    );
    card.detail("pdb_count", (!pdb.is_empty()).then_some(pdb.len()));
    if ids("AlphaFoldDB").any(|a| accession(&a).as_deref() == Some(acc)) {
        card.link(Link::new(
            "AlphaFold DB",
            format!("https://alphafold.ebi.ac.uk/entry/{acc}"),
            VIA,
        ));
    }
    Ok(Some(Entry::Card(Box::new(card))))
}

/// The entry of a root, `uniprot:{ACC}` (the accession in upper case).
pub fn from_root(uri: &str) -> Option<Target> {
    let acc = uri.strip_prefix("uniprot:")?;
    accession(acc).filter(|a| a == acc).map(Target::Accession)
}

/// The entry of a root or a UniProt link.
pub fn target(uri: &str) -> Option<Target> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// The source: UniProtKB entries from the UniProt REST API.
pub struct Uniprot;

#[async_trait]
impl Source for Uniprot {
    fn name(&self) -> &'static str {
        "uniprot"
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?)?;
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("uniprot:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "first_public",
            "entry_name",
            "gene",
            "genes",
            "organism",
            "taxon_id",
            "length",
            "reviewed",
            "sequence_version",
            "last_updated",
            "pdb_ids",
            "pdb_count",
            "inactive",
            "replaced_by",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("rest.uniprot.org", 1.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.uniprot.org/uniprotkb/P69905/entry",
                card_type: "Protein",
            },
            Example {
                uri: "https://www.uniprot.org/uniprotkb/P04637/entry",
                card_type: "Protein",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let acc = match target(uri) {
            Some(Target::Accession(acc)) => acc,
            Some(Target::Name(name)) => {
                let f = fetch(cx, &name_request(&name).context("no lookup")?).await?;
                match parse_name(f.status, &f.body, &name)? {
                    Some(acc) => acc,
                    None => return Ok(cx.missing(uri, self)),
                }
            }
            None => return Ok(cx.missing(uri, self)),
        };
        let f = fetch(cx, &request(&acc).context("no reader")?).await?;
        let card = match parse(f.status, &f.body, &acc)? {
            Some(Entry::Card(card)) => *card,
            // UniProt's record names the entry the accession now lives in: read that one; the
            // link becomes its alias.
            Some(Entry::MergedInto(into)) => {
                let f = fetch(cx, &request(&into).context("no reader")?).await?;
                match parse(f.status, &f.body, &into)? {
                    Some(Entry::Card(card)) => *card,
                    Some(Entry::MergedInto(again)) => {
                        bail!("{acc} was merged into {into}, itself merged into {again}")
                    }
                    None => return Ok(cx.missing(uri, self)),
                }
            }
            None => return Ok(cx.missing(uri, self)),
        };
        Ok(card.into_doc(cx.card(uri, self), SITE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;
    use serde_json::json;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("uniprot", name)
    }
    /// The card of an accession's recorded search answer.
    fn card(file: &str, acc: &str) -> Card {
        match parse(200, &read(file), acc).unwrap() {
            Some(Entry::Card(card)) => *card,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn accessions() {
        for ok in [
            "P69905",
            "p04637",
            "Q15086",
            "O51290",
            "A0A023GPI8",
            "A2BC19",
            "A0A0A0MRZ7",
        ] {
            assert_eq!(accession(ok), Some(ok.to_ascii_uppercase()), "{ok}");
        }
        for bad in [
            "P6990",
            "P699055",
            "PA9905",
            "P6990A",
            "O0A023GPI8",
            "A0A023GPI",
            "Q9XXXX0",
            "P69905_HUMAN",
        ] {
            assert_eq!(accession(bad), None, "{bad}");
        }
        assert_eq!(entry_name("hba_human").as_deref(), Some("HBA_HUMAN"));
        assert_eq!(entry_name("Q9TQ74_HUMAN").as_deref(), Some("Q9TQ74_HUMAN"));
        assert_eq!(entry_name("HBA_HUMANS"), None);
        assert_eq!(entry_name("_HUMAN"), None);
        assert_eq!(entry_name("HBA-HUMAN"), None);
    }

    #[test]
    fn every_link_form_of_one_entry() {
        for uri in [
            "https://www.uniprot.org/uniprotkb/P69905",
            "https://www.uniprot.org/uniprotkb/P69905/entry",
            "https://uniprot.org/uniprotkb/p69905/entry#function",
            "https://www.uniprot.org/uniprotkb/P69905/feature-viewer",
            "https://www.uniprot.org/uniprotkb/P69905/variant-viewer",
            "https://www.uniprot.org/uniprotkb/P69905/publications",
            "https://www.uniprot.org/uniprotkb/P69905/external-links",
            "https://www.uniprot.org/uniprotkb/P69905/history",
            "https://www.uniprot.org/uniprotkb/P69905.txt",
            "https://www.uniprot.org/uniprot/P69905",
            "https://www.uniprot.org/uniprot/P69905.txt",
            "https://www.uniprot.org/uniprot/P69905.xml",
            "https://rest.uniprot.org/uniprotkb/P69905",
            "https://rest.uniprot.org/uniprotkb/P69905.json",
            "https://rest.uniprot.org/uniprotkb/P69905.txt",
            "http://purl.uniprot.org/uniprot/P69905",
        ] {
            assert_eq!(
                from_uri(uri).as_ref().and_then(identifier).as_deref(),
                Some("uniprot:P69905"),
                "{uri}"
            );
        }
        for uri in [
            "https://www.uniprot.org/uniprotkb/HBA_HUMAN/entry",
            "https://www.uniprot.org/uniprotkb/hba_human",
            "https://www.uniprot.org/uniprot/HBA_HUMAN",
            "https://rest.uniprot.org/uniprotkb/HBA_HUMAN.json",
        ] {
            let t = from_uri(uri).unwrap();
            assert_eq!(t, Target::Name("HBA_HUMAN".into()), "{uri}");
            assert_eq!(identifier(&t), None);
        }
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://www.uniprot.org/uniprotkb/P69905.fasta",
            "https://rest.uniprot.org/uniprotkb/P69905.gff",
            "https://www.uniprot.org/uniprotkb/P69905/sequence-viewer",
            "https://www.uniprot.org/uniprotkb/P6990",
            "https://www.uniprot.org/uniprotkb?query=HBA",
            "https://www.uniprot.org/uniparc/UPI0000000239",
            "https://rest.uniprot.org/uniprotkb/search?query=P69905",
            "http://purl.uniprot.org/uniprot/HBA_HUMAN",
            "https://alphafold.ebi.ac.uk/entry/P69905",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn requests() {
        let r = request("p69905").unwrap();
        assert_eq!(
            r.url.as_str(),
            format!(
                "https://rest.uniprot.org/uniprotkb/search?query=accession%3AP69905&fields={}&format=json&size=2",
                encode(FIELDS)
            )
        );
        assert!(request("HBA_HUMAN").is_none());
        assert_eq!(
            name_request("hba_human").unwrap().url.as_str(),
            "https://rest.uniprot.org/uniprotkb/search?query=id%3AHBA_HUMAN%20AND%20active%3Atrue&fields=accession%2Cid&format=json&size=2"
        );
        assert_eq!(
            page("P69905"),
            "https://www.uniprot.org/uniprotkb/P69905/entry"
        );
    }

    #[test]
    fn names() {
        let parse = |file: &str, name: &str| parse_name(200, &read(file), name).unwrap();
        assert_eq!(
            parse("name-hba-human.json", "HBA_HUMAN").as_deref(),
            Some("P69905")
        );
        assert_eq!(parse("name-missing.json", "ZZZQ9_HUMAN"), None);
        // A former name finds the renamed entry: not exact.
        assert_eq!(parse("name-nope-human-former.json", "NOPE_HUMAN"), None);
        let two = json!({"results": [
            {"entryType": "UniProtKB reviewed (Swiss-Prot)", "primaryAccession": "P69905", "uniProtkbId": "HBA_HUMAN"},
            {"entryType": "UniProtKB reviewed (Swiss-Prot)", "primaryAccession": "P69906", "uniProtkbId": "HBA_HUMAN"},
        ]});
        assert_eq!(
            parse_name(200, two.to_string().as_bytes(), "HBA_HUMAN").unwrap(),
            None
        );
        assert!(parse_name(500, b"", "HBA_HUMAN").is_err());
    }

    #[test]
    fn haemoglobin_alpha() {
        let card = card("search-p69905.json", "P69905");
        assert_eq!(card.type_name, "Protein");
        assert_eq!(card.id, "uniprot:P69905");
        assert_eq!(card.url, "https://www.uniprot.org/uniprotkb/P69905/entry");
        assert_eq!(card.name.as_deref(), Some("Hemoglobin subunit alpha"));
        assert_eq!(
            card.description.as_deref(),
            Some("Involved in oxygen transport from the lung to the various peripheral tissues")
        );
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["first_public"], "1986-07-21");
        assert_eq!(card.attribution.as_deref(), Some(ATTRIBUTION));
        let d = &card.details;
        assert_eq!(d["entry_name"], "HBA_HUMAN");
        assert_eq!(d["genes"], json!(["HBA1", "HBA2"]));
        assert!(!d.contains_key("gene"));
        assert_eq!(d["organism"], "Homo sapiens");
        assert_eq!(d["taxon_id"], 9606);
        assert_eq!(d["length"], 142);
        assert_eq!(d["reviewed"], true);
        assert_eq!(d["sequence_version"], 2);
        assert_eq!(d["last_updated"], "2026-09-02");
        assert_eq!(
            d["pdb_ids"],
            json!(["1A00", "1A01", "1A0U", "1A0Z", "1A3N"])
        );
        assert_eq!(d["pdb_count"], 355);
        assert!(card.keys.is_empty());
        assert_eq!(card.also_on.len(), 1);
        assert_eq!(
            card.also_on[0].uri,
            "https://alphafold.ebi.ac.uk/entry/P69905"
        );
        assert!(parse(200, &read("search-p69905.json"), "P04637").is_err());
    }

    #[test]
    fn p53() {
        let card = card("search-p04637.json", "P04637");
        assert_eq!(card.name.as_deref(), Some("Cellular tumor antigen p53"));
        assert!(card
            .description
            .as_deref()
            .unwrap()
            .starts_with("Multifunctional transcription factor"));
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["first_public"], "1987-08-13");
        assert_eq!(card.details["gene"], "TP53");
        assert_eq!(card.details["length"], 393);
        assert_eq!(card.details["sequence_version"], 4);
    }

    #[test]
    fn inactive_entries() {
        // A merged accession answers with the entry it was merged into, which is read next.
        assert_eq!(
            parse(200, &read("search-q15086.json"), "Q15086").unwrap(),
            Some(Entry::MergedInto("P04637".into()))
        );
        let merged = card("search-p04637.json", "P04637");
        assert_eq!(merged.id, "uniprot:P04637");
        let demerged = card("search-p00001.json", "P00001");
        assert_eq!(demerged.id, "uniprot:P00001");
        assert_eq!(demerged.name, None);
        assert_eq!(demerged.details["inactive"], "demerged");
        assert_eq!(demerged.details["replaced_by"], json!(["P99999", "P99998"]));
        assert_eq!(
            parse(200, &read("search-o51290.json"), "O51290").unwrap(),
            None,
            "deleted"
        );
        assert_eq!(
            parse(200, &read("search-p0dzz9.json"), "P0DZZ9").unwrap(),
            None,
            "never assigned"
        );
        // An answer for other accessions, or for more than one entry, is not an answer.
        assert!(parse(200, &read("search-q15086.json"), "P00001").is_err());
        let two = br#"{"results":[{"primaryAccession":"P69905","entryType":"Inactive"},{"primaryAccession":"P69905","entryType":"Inactive"}]}"#;
        assert!(parse(200, two, "P69905").is_err());
        assert!(parse(200, b"{}", "P69905").is_err());
        assert!(parse(400, b"{}", "P69905").is_err());
        assert!(parse(429, b"", "P69905").is_err());
        assert!(parse(500, b"", "P69905").is_err());
    }

    #[test]
    fn roots_and_aliases() {
        let u = Uniprot;
        assert!(u.reads("uniprot:P69905"));
        assert_eq!(u.alias_root("uniprot:P69905"), None);
        assert!(!u.invalid("uniprot:P69905"));
        for uri in [
            "https://www.uniprot.org/uniprotkb/p69905/entry",
            "https://rest.uniprot.org/uniprotkb/P69905.json",
            "http://purl.uniprot.org/uniprot/P69905",
        ] {
            assert_eq!(
                u.alias_root(uri).as_deref(),
                Some("uniprot:P69905"),
                "{uri}"
            );
        }
        // An entry name names the entry only after the lookup.
        let name = "https://www.uniprot.org/uniprotkb/HBA_HUMAN/entry";
        assert!(u.reads(name));
        assert_eq!(u.alias_root(name), None);
        for uri in [
            "uniprot:p69905",
            "uniprot:HBA_HUMAN",
            "uniprot:P6990",
            "uniprot:",
        ] {
            assert!(u.invalid(uri), "{uri}");
            assert!(!u.reads(uri), "{uri}");
        }
        for example in u.examples() {
            assert!(u.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for card in [
            card("search-p69905.json", "P69905"),
            card("search-p04637.json", "P04637"),
            card("search-p00001.json", "P00001"),
        ] {
            let doc = crate::sources::kit::checked(&Uniprot, &card);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
            assert_eq!(doc.attributes["page_url"], card.url);
        }
        let card = card("search-p69905.json", "P69905");
        let doc = card.into_doc(ResourceMeta::new("uniprot:P69905", "uniprot", 60), SITE);
        assert_eq!(doc.site, "UniProt");
        assert_eq!(doc.attributes["genes"], "HBA1; HBA2");
        assert_eq!(doc.attributes["reviewed"], "true");
    }
}
