//! Chemical compounds in PubChem, read from PUG REST by compound id (CID).
//!
//! Every PubChem link of one compound (its page, the legacy `summary.cgi` page, and the PUG REST
//! and PUG View URLs of its whole record or its properties) is an alias of `pubchem:cid/{cid}`.
//! Other PUG REST operations name other things (the structure image, related compounds,
//! substances, assays) and are not read. A page by name (`/compound/aspirin`) names
//! the same compound once `name_request` lists exactly one CID for the name. Substances
//! (`/substance/{sid}`) are depositors' records, not compounds.
//!
//! The service asks for at most 5 requests a second. Structure depictions are public domain.
use super::{
    kit::{
        encode, fetch, float, host_and_segments, json, number, text, web_url, Card, Key, Request,
    },
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use percent_encoding::percent_decode_str;

pub const SITE: &str = "PubChem";
pub const ATTRIBUTION: &str = "PubChem (NCBI)";
/// The properties the card reads. `SMILES` is the full (isomeric) SMILES; PubChem maps the old
/// names `IsomericSMILES` and `CanonicalSMILES` to `SMILES` and `ConnectivitySMILES`.
const PROPERTIES: &str =
    "Title,IUPACName,MolecularFormula,MolecularWeight,InChIKey,SMILES,ConnectivitySMILES,XLogP,ExactMass";
/// The longest compound name looked up.
const MAX_NAME: usize = 200;

/// What a link names: a compound by CID, or by name (which needs `name_request`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Cid(u64),
    Name(String),
}

/// A compound id: a positive decimal number without leading zeros.
pub fn cid(value: &str) -> Option<u64> {
    number(value, 12).filter(|_| !value.starts_with('0'))
}

/// A standard InChIKey (`XXXXXXXXXXXXXX-XXXXXXXXXX-X`, upper-case letters).
pub fn inchikey(value: &str) -> Option<String> {
    let parts: Vec<&str> = value.split('-').collect();
    let ok = matches!(parts.as_slice(), [a, b, c] if a.len() == 14 && b.len() == 10 && c.len() == 1)
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_uppercase()));
    ok.then(|| value.to_owned())
}

/// What a PubChem link names: compound pages by CID or name, and PUG REST and PUG View URLs.
pub fn from_uri(uri: &str) -> Option<Target> {
    let (host, segments) = host_and_segments(uri)?;
    if host != "pubchem.ncbi.nlm.nih.gov" {
        return None;
    }
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    Some(match s.as_slice() {
        ["compound", id] if id.bytes().all(|b| b.is_ascii_digit()) => Target::Cid(cid(id)?),
        ["compound", id] => {
            let name = percent_decode_str(id).decode_utf8().ok()?;
            let name = name.trim();
            ensure_name(name)?;
            Target::Name(name.to_owned())
        }
        ["rest", "pug", "compound", "cid", id, format]
        | ["rest", "pug", "compound", "cid", id, "property", _, format]
            if RECORD_FORMATS.contains(format) =>
        {
            Target::Cid(cid(id)?)
        }
        ["rest", "pug_view", "data", "compound", id]
        | ["rest", "pug_view", "data", "compound", id, "JSON" | "XML"] => Target::Cid(cid(id)?),
        ["summary", "summary.cgi"] => Target::Cid(
            web_url(uri)?
                .query_pairs()
                .find(|(k, _)| k == "cid")
                .and_then(|(_, v)| cid(&v))?,
        ),
        _ => return None,
    })
}

/// The PUG REST output formats of a compound's whole record or its properties. `PNG` is the
/// structure image, not the record.
const RECORD_FORMATS: &[&str] = &["JSON", "XML", "SDF", "ASNT", "ASNB", "CSV", "TXT"];

fn ensure_name(name: &str) -> Option<()> {
    (!name.is_empty()
        && name.len() <= MAX_NAME
        && !name.chars().any(char::is_control)
        && !name.chars().all(|c| c == '.'))
    .then_some(())
}

/// The identifier, or `None` for a name (its CID comes from `name_request`).
pub fn identifier(target: &Target) -> Option<String> {
    match target {
        Target::Cid(c) => Some(format!("pubchem:cid/{c}")),
        Target::Name(_) => None,
    }
}

pub fn page(cid: u64) -> String {
    format!("https://pubchem.ncbi.nlm.nih.gov/compound/{cid}")
}

/// The compound's computed properties.
pub fn request(cid: u64) -> Option<Request> {
    Request::get(&format!(
        "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/{cid}/property/{PROPERTIES}/JSON"
    ))
}

/// The CIDs PubChem lists for a name.
pub fn name_request(name: &str) -> Option<Request> {
    ensure_name(name)?;
    Request::get(&format!(
        "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/name/{}/cids/JSON",
        encode(name)
    ))
}

/// What PubChem names for a compound name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    /// No compound.
    Nothing,
    /// Exactly one compound, by CID.
    One(u64),
    /// Several compounds: the name is no identifier.
    Several,
}

/// The compounds PubChem names for a name.
pub fn parse_name(status: u16, raw: &[u8]) -> Result<Named> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(Named::Nothing);
    };
    let cids = v["IdentifierList"]["CID"]
        .as_array()
        .context("no CID list")?;
    Ok(match cids.as_slice() {
        [] => Named::Nothing,
        [one] => Named::One(one.as_u64().filter(|c| *c > 0).context("not a CID")?),
        _ => Named::Several,
    })
}

/// A compound, or `None` when there is no compound with this CID (PUG REST answers an unknown
/// CID's properties with a row holding only the CID, and other unknown ids with a 404 Fault).
pub fn parse(status: u16, raw: &[u8], cid: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let rows = v["PropertyTable"]["Properties"]
        .as_array()
        .context("no property table")?;
    let [row] = rows.as_slice() else {
        bail!("{} property rows", rows.len());
    };
    ensure!(
        row["CID"].as_u64() == Some(cid),
        "the API answered for another compound"
    );
    let key = row["InChIKey"].as_str().and_then(inchikey);
    if key.is_none() && text(&row["MolecularFormula"]).is_none() {
        return Ok(None);
    }
    let mut card = Card::new("MolecularEntity", format!("pubchem:cid/{cid}"), page(cid));
    card.name = text(&row["Title"]);
    card.image = Some(format!(
        "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/{cid}/PNG"
    ));
    card.attribution = Some(ATTRIBUTION.into());
    card.detail("iupac_name", text(&row["IUPACName"]));
    card.detail("formula", text(&row["MolecularFormula"]));
    card.detail("molecular_weight", float(&row["MolecularWeight"]));
    card.detail("exact_mass", float(&row["ExactMass"]));
    card.detail("inchikey", key.clone());
    card.detail("smiles", text(&row["SMILES"]));
    card.detail("xlogp", float(&row["XLogP"]));
    if let Some(key) = key {
        card.key(Key::new("inchikey", &key));
    }
    Ok(Some(card))
}

/// The compound of a root, `pubchem:cid/{cid}`.
pub fn from_root(uri: &str) -> Option<Target> {
    let id = uri.strip_prefix("pubchem:cid/")?;
    cid(id).map(Target::Cid)
}

/// The compound of a root or a PubChem link.
pub fn target(uri: &str) -> Option<Target> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// The source: PubChem compounds from PUG REST.
pub struct Pubchem;

#[async_trait]
impl Source for Pubchem {
    fn name(&self) -> &'static str {
        "pubchem"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "pubchem",
                uri: "https://pubchem.ncbi.nlm.nih.gov/compound/2244",
                card_type: "MolecularEntity",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "pubchem.ncbi.nlm.nih.gov",
                path: "/rest/pug/compound/cid/2244/property/*",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "compound-2244.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?)?;
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("pubchem:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "inchikey",
            "iupac_name",
            "formula",
            "molecular_weight",
            "exact_mass",
            "smiles",
            "xlogp",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("pubchem.ncbi.nlm.nih.gov", 4.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://pubchem.ncbi.nlm.nih.gov/compound/2244",
                card_type: "MolecularEntity",
            },
            Example {
                uri: "https://pubchem.ncbi.nlm.nih.gov/compound/2519",
                card_type: "MolecularEntity",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let (cid, by_name) = match target(uri) {
            Some(Target::Cid(cid)) => (cid, false),
            Some(Target::Name(name)) => {
                let f = fetch(cx, &name_request(&name).context("no lookup")?).await?;
                match parse_name(f.status, &f.body)? {
                    Named::One(cid) => (cid, true),
                    Named::Nothing => return Ok(cx.missing(uri, self)),
                    Named::Several => return Err(super::Declined.into()),
                }
            }
            None => return Ok(cx.missing(uri, self)),
        };
        let f = fetch(cx, &request(cid).context("no reader")?).await?;
        Ok(match parse(f.status, &f.body, cid)? {
            Some(card) => {
                let mut doc = card.into_doc(cx.card(uri, self), SITE);
                // A name is no identifier: the page by name keeps its own card and never becomes
                // an alias of the compound.
                if by_name {
                    doc.source_root = None;
                }
                doc
            }
            None => cx.missing(uri, self),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("pubchem", name)
    }

    #[test]
    fn every_link_form_of_one_compound() {
        for uri in [
            "https://pubchem.ncbi.nlm.nih.gov/compound/2244",
            "https://pubchem.ncbi.nlm.nih.gov/compound/2244#section=Names-and-Identifiers",
            "http://pubchem.ncbi.nlm.nih.gov/compound/2244/",
            "https://pubchem.ncbi.nlm.nih.gov/summary/summary.cgi?cid=2244",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/JSON",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/property/Title/JSON",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/SDF",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug_view/data/compound/2244/JSON",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug_view/data/compound/2244",
        ] {
            assert_eq!(
                from_uri(uri).as_ref().and_then(identifier).as_deref(),
                Some("pubchem:cid/2244"),
                "{uri}"
            );
        }
        assert_eq!(
            from_uri("https://pubchem.ncbi.nlm.nih.gov/compound/aspirin"),
            Some(Target::Name("aspirin".into()))
        );
        assert_eq!(
            from_uri("https://pubchem.ncbi.nlm.nih.gov/compound/acetylsalicylic%20acid"),
            Some(Target::Name("acetylsalicylic acid".into()))
        );
        assert_eq!(identifier(&Target::Name("aspirin".into())), None);
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://pubchem.ncbi.nlm.nih.gov/compound/%2E%2E",
            "https://pubchem.ncbi.nlm.nih.gov/compound/%2E",
            "https://pubchem.ncbi.nlm.nih.gov/compound/0",
            "https://pubchem.ncbi.nlm.nih.gov/compound/02244",
            "https://pubchem.ncbi.nlm.nih.gov/compound/2244/extra",
            "https://pubchem.ncbi.nlm.nih.gov/substance/3243128",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/substance/sid/3243128/JSON",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/name/aspirin/JSON",
            // Other operations name other things: the image, related compounds, substances.
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/PNG",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/cids/JSON",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/sids/JSON",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/assaysummary/JSON",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug_view/data/compound/2244/JSON/extra",
            "https://pubchem.ncbi.nlm.nih.gov/summary/summary.cgi?sid=3243128",
            "https://pubchem.ncbi.nlm.nih.gov/compound/%00",
            "https://example.org/compound/2244",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn requests() {
        assert_eq!(
            request(2244).unwrap().url.as_str(),
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/property/Title,IUPACName,MolecularFormula,MolecularWeight,InChIKey,SMILES,ConnectivitySMILES,XLogP,ExactMass/JSON"
        );
        assert_eq!(
            name_request("acetylsalicylic acid").unwrap().url.as_str(),
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/name/acetylsalicylic%20acid/cids/JSON"
        );
        assert_eq!(page(2244), "https://pubchem.ncbi.nlm.nih.gov/compound/2244");
    }

    #[test]
    fn names() {
        assert_eq!(
            parse_name(200, &read("name-aspirin.json")).unwrap(),
            Named::One(2244)
        );
        assert_eq!(
            parse_name(200, &read("name-glucose.json")).unwrap(),
            Named::One(5793)
        );
        assert_eq!(
            parse_name(404, &read("name-missing.json")).unwrap(),
            Named::Nothing
        );
        let two = br#"{"IdentifierList":{"CID":[5793,79025]}}"#;
        assert_eq!(parse_name(200, two).unwrap(), Named::Several);
        assert!(parse_name(503, b"",).is_err());
    }

    #[test]
    fn aspirin() {
        let card = parse(200, &read("compound-2244.json"), 2244)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "MolecularEntity");
        assert_eq!(card.id, "pubchem:cid/2244");
        assert_eq!(card.url, "https://pubchem.ncbi.nlm.nih.gov/compound/2244");
        assert_eq!(card.name.as_deref(), Some("Aspirin"));
        assert_eq!(card.description, None);
        assert_eq!(
            card.image.as_deref(),
            Some("https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/PNG")
        );
        let d = &card.details;
        assert_eq!(d["iupac_name"], "2-acetyloxybenzoic acid");
        assert_eq!(d["formula"], "C9H8O4");
        assert_eq!(d["molecular_weight"], 180.16);
        assert_eq!(d["exact_mass"], 180.04225873);
        assert_eq!(d["inchikey"], "BSYNRYMUTXBXSQ-UHFFFAOYSA-N");
        assert_eq!(d["smiles"], "CC(=O)OC1=CC=CC=C1C(=O)O");
        assert_eq!(d["xlogp"], 1.2);
        assert_eq!(
            card.keys,
            [Key::new("inchikey", "BSYNRYMUTXBXSQ-UHFFFAOYSA-N")]
        );
        assert!(parse(200, &read("compound-2244.json"), 2519).is_err());
    }

    #[test]
    fn caffeine() {
        let card = parse(200, &read("compound-2519.json"), 2519)
            .unwrap()
            .unwrap();
        assert_eq!(card.name.as_deref(), Some("Caffeine"));
        assert_eq!(card.details["formula"], "C8H10N4O2");
        assert_eq!(card.details["xlogp"], -0.1);
        assert_eq!(
            card.keys,
            [Key::new("inchikey", "RYYVLZVUVIJVGH-UHFFFAOYSA-N")]
        );
    }

    #[test]
    fn missing() {
        assert_eq!(
            parse(200, &read("compound-missing.json"), 999999999).unwrap(),
            None
        );
        assert_eq!(
            parse(404, &read("record-missing.json"), 999999999).unwrap(),
            None
        );
        assert!(parse(503, b"", 2244).is_err());
        assert!(parse(429, b"", 2244).is_err());
        assert!(parse(400, b"{}", 2244).is_err());
        assert_eq!(inchikey("BSYNRYMUTXBXSQ-UHFFFAOYSA-n"), None);
    }

    #[test]
    fn roots_and_aliases() {
        let p = Pubchem;
        assert!(p.reads("pubchem:cid/2244"));
        assert_eq!(p.alias_root("pubchem:cid/2244"), None);
        assert!(!p.invalid("pubchem:cid/2244"));
        for uri in [
            "https://pubchem.ncbi.nlm.nih.gov/compound/2244",
            "https://pubchem.ncbi.nlm.nih.gov/summary/summary.cgi?cid=2244",
            "https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/2244/JSON",
        ] {
            assert_eq!(
                p.alias_root(uri).as_deref(),
                Some("pubchem:cid/2244"),
                "{uri}"
            );
        }
        // A name names a compound only after the lookup.
        let name = "https://pubchem.ncbi.nlm.nih.gov/compound/aspirin";
        assert!(p.reads(name));
        assert_eq!(p.alias_root(name), None);
        for uri in [
            "pubchem:cid/02244",
            "pubchem:cid/0",
            "pubchem:cid/aspirin",
            "pubchem:sid/1",
            "pubchem:",
        ] {
            assert!(p.invalid(uri), "{uri}");
            assert!(!p.reads(uri), "{uri}");
        }
        for example in p.examples() {
            assert!(p.reads(example.uri), "{}", example.uri);
        }
    }

    /// A compound's InChIKey is a fact that joins no group.
    #[test]
    fn its_inchikey_joins_nothing() {
        use crate::sources::kit::{checked, own_keys};
        let card = parse(200, &read("compound-2244.json"), 2244)
            .unwrap()
            .unwrap();
        assert!(checked(&Pubchem, &card).attributes.contains_key("inchikey"));
        assert_eq!(own_keys(&Pubchem, &card), Vec::<String>::new());
    }

    #[test]
    fn documents() {
        for (file, cid) in [("compound-2244.json", 2244), ("compound-2519.json", 2519)] {
            let card = parse(200, &read(file), cid).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Pubchem, &card);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
            assert_eq!(doc.attributes["page_url"], card.url);
        }
        let card = parse(200, &read("compound-2244.json"), 2244)
            .unwrap()
            .unwrap();
        let doc = card.into_doc(ResourceMeta::new("pubchem:cid/2244", "pubchem", 60), SITE);
        assert_eq!(doc.site, "PubChem");
        assert_eq!(doc.attributes["inchikey"], "BSYNRYMUTXBXSQ-UHFFFAOYSA-N");
        assert_eq!(doc.attributes["molecular_weight"], "180.16");
    }
}
