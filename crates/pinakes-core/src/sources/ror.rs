//! Research organizations from the Research Organization Registry (ROR), read from its v2 API by
//! the ROR id in the link.
//!
//! Every ror.org and API link of one organization is an alias of `ror:{id}`. A ROR id is `0`, six
//! Crockford base32 characters and two check digits (ISO 7064 mod 97-10 over the base32 value),
//! in lower case; ids with a wrong check are rejected. The data is CC0.
use super::{
    kit::{
        fetch, float, host_and_segments, int, json, text, texts, Card, Key, Link, Point, Request,
    },
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

pub const ATTRIBUTION: &str = "Research Organization Registry (CC0)";
pub const SITE: &str = "ROR";
/// Crockford base32 without `i`, `l`, `o` and `u`.
const BASE32: &str = "0123456789abcdefghjkmnpqrstvwxyz";

/// A ROR id with a valid check, in lower case, e.g. `03yrm5c26`.
pub fn ror_id(value: &str) -> Option<String> {
    let value = value.to_ascii_lowercase();
    if value.len() != 9 || !value.is_ascii() || !value.starts_with('0') {
        return None;
    }
    let (body, check) = value.split_at(7);
    let mut n: u64 = 0;
    for c in body.chars() {
        n = n * 32 + BASE32.find(c)? as u64;
    }
    let check: u64 = check
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| check.parse().ok())
        .flatten()?;
    (check == 98 - (n * 100) % 97).then_some(value)
}

/// The ROR id a link names: `ror.org/{id}` and the API's v2 and v1 organization URLs.
pub fn from_uri(uri: &str) -> Option<String> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (host.as_str(), s.as_slice()) {
        ("ror.org", [id]) => ror_id(id),
        ("api.ror.org", ["v2" | "v1", "organizations", id] | ["organizations", id]) => ror_id(id),
        _ => None,
    }
}

pub fn identifier(id: &str) -> String {
    format!("ror:{id}")
}

/// The ROR id of a root (`ror:{id}`, the id already in lower case).
pub fn from_root(uri: &str) -> Option<String> {
    let id = uri.strip_prefix("ror:")?;
    ror_id(id).filter(|i| i == id)
}

/// The ROR id of a root or a ROR link.
pub fn target(uri: &str) -> Option<String> {
    from_root(uri).or_else(|| from_uri(uri))
}

pub fn page(id: &str) -> String {
    format!("https://ror.org/{id}")
}

pub fn request(id: &str) -> Option<Request> {
    Request::get(&format!(
        "https://api.ror.org/v2/organizations/{}",
        ror_id(id)?
    ))
}

/// The schema.org type of one ROR type; `funder` and `other` say nothing about the kind.
fn schema_type(ror_type: &str) -> Option<&'static str> {
    Some(match ror_type {
        "education" => "EducationalOrganization",
        "healthcare" => "MedicalOrganization",
        "government" => "GovernmentOrganization",
        "company" => "Corporation",
        "nonprofit" => "NGO",
        "archive" => "ArchiveOrganization",
        "facility" => "ResearchOrganization",
        _ => return None,
    })
}

/// The one schema.org type the ROR types map to, else `Organization`.
fn card_type(types: &[String]) -> &'static str {
    let mut mapped: Vec<&str> = types.iter().filter_map(|t| schema_type(t)).collect();
    mapped.sort_unstable();
    mapped.dedup();
    match mapped.as_slice() {
        [one] => one,
        _ => "Organization",
    }
}

/// An external id that ROR states as preferred, or the only one of its type, normalized.
fn external_key(e: &Value) -> Option<Key> {
    let all = texts(&e["all"]);
    let value = text(&e["preferred"]).or_else(|| match all.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    })?;
    let kind = e["type"].as_str()?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let value = match kind {
        "wikidata" => {
            let q = value.to_ascii_uppercase();
            q.strip_prefix('Q').filter(|n| digits(n))?;
            q
        }
        "isni" => {
            let v: String = value
                .split_whitespace()
                .collect::<String>()
                .to_ascii_uppercase();
            (v.len() == 16
                && v.is_ascii()
                && digits(&v[..15])
                && v[15..].bytes().all(|b| b.is_ascii_digit() || b == b'X'))
            .then_some(v)?
        }
        "fundref" => digits(&value).then_some(value)?,
        "grid" => {
            let parts: Vec<&str> = value.split('.').collect();
            (parts.len() == 3 && parts[0] == "grid" && digits(parts[1]) && !parts[2].is_empty())
                .then(|| value.to_ascii_lowercase())?
        }
        _ => return None,
    };
    Some(Key::new(kind, &value))
}

/// The values of the names that carry a type, e.g. `acronym`.
fn names(v: &Value, kind: &str) -> Vec<String> {
    v["names"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| texts(&n["types"]).iter().any(|t| t == kind))
        .filter_map(|n| text(&n["value"]))
        .collect()
}

/// The values of the links of one type.
fn links(v: &Value, kind: &str) -> Vec<String> {
    v["links"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|l| l["type"].as_str() == Some(kind))
        .filter_map(|l| text(&l["value"]))
        .collect()
}

/// An organization, or `None` when ROR has no organization with this id (404).
pub fn parse(status: u16, raw: &[u8], id: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        v["id"].as_str() == Some(page(id).as_str()),
        "the API answered for another organization"
    );
    let types = texts(&v["types"]);
    let mut card = Card::new(card_type(&types), identifier(id), page(id));
    let display = names(&v, "ror_display").into_iter().next();
    card.name = Some(display.clone().context("no display name")?);
    card.attribution = Some(ATTRIBUTION.into());
    if let Some([place]) = v["locations"].as_array().map(Vec::as_slice) {
        let g = &place["geonames_details"];
        card.point = float(&g["lat"])
            .zip(float(&g["lng"]))
            .and_then(|(lat, lon)| Point::new(lat, lon));
        card.detail("city", text(&g["name"]));
        card.detail("country", text(&g["country_name"]));
        card.detail("country_code", text(&g["country_code"]));
    }
    // A year: a detail, never `start`.
    card.detail("established", int(&v["established"]));
    card.detail("types", types);
    card.detail("status", text(&v["status"]));
    card.detail("website", links(&v, "website").into_iter().next());
    card.detail("acronyms", names(&v, "acronym"));
    card.detail("other_names", names(&v, "alias"));
    card.detail(
        "labels",
        names(&v, "label")
            .into_iter()
            .filter(|l| Some(l) != display.as_ref())
            .collect::<Vec<_>>(),
    );
    card.detail("domains", texts(&v["domains"]));
    for e in v["external_ids"].as_array().into_iter().flatten() {
        if let Some(key) = external_key(e) {
            card.key(key);
        }
    }
    if let [wikipedia] = links(&v, "wikipedia").as_slice() {
        card.link(Link::new("Wikipedia", wikipedia.as_str(), "ror"));
    }
    Ok(Some(card))
}

/// The source: the ROR v2 API.
pub struct Ror;

#[async_trait]
impl Source for Ror {
    fn name(&self) -> &'static str {
        "ror"
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?);
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("ror:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "city",
            "country",
            "country_code",
            "established",
            "types",
            "status",
            "website",
            "acronyms",
            "other_names",
            "labels",
            "domains",
            // Keys, by namespace
            "wikidata",
            "isni",
            "fundref",
            "grid",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("api.ror.org", 1.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://ror.org/03yrm5c26",
                card_type: "ArchiveOrganization",
            },
            Example {
                uri: "https://ror.org/042nb2s44",
                card_type: "EducationalOrganization",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&id).context("no request")?).await?;
        Ok(match parse(f.status, &f.body, &id)? {
            Some(card) => card.into_doc(cx.card(uri, self), SITE),
            None => cx.missing(uri, self),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;
    use serde_json::json;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("ror", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).as_deref().map(identifier)
    }
    fn keys(card: &Card) -> Vec<&str> {
        card.keys.iter().map(Key::as_str).collect()
    }

    #[test]
    fn check_digits() {
        assert_eq!(ror_id("03yrm5c26").as_deref(), Some("03yrm5c26"));
        assert_eq!(ror_id("042NB2S44").as_deref(), Some("042nb2s44"));
        assert_eq!(ror_id("0zzzzzz02").as_deref(), Some("0zzzzzz02"));
        for bad in [
            "03yrm5c27",
            "13yrm5c26",
            "03yrm5c2",
            "03yrm5c266",
            "0iyrm5c26",
            "03yrm5cx6",
        ] {
            assert_eq!(ror_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn every_link_form_of_one_organization() {
        for uri in [
            "https://ror.org/042nb2s44",
            "http://ror.org/042nb2s44",
            "https://www.ror.org/042NB2S44/",
            "https://api.ror.org/v2/organizations/042nb2s44",
            "https://api.ror.org/v1/organizations/042nb2s44",
            "https://api.ror.org/organizations/042nb2s44",
        ] {
            assert_eq!(id(uri).as_deref(), Some("ror:042nb2s44"), "{uri}");
        }
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://ror.org/042nb2s45",
            "https://ror.org/search?query=mit",
            "https://ror.org/042nb2s44/x",
            "https://api.ror.org/v2/organizations?query=mit",
            "https://example.org/042nb2s44",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn requests() {
        assert_eq!(
            request("042nb2s44").unwrap().url.as_str(),
            "https://api.ror.org/v2/organizations/042nb2s44"
        );
        assert_eq!(request("042nb2s45"), None);
    }

    #[test]
    fn other_names_are_not_link_aliases() {
        // ROR's "alias" names are other names of the organisation, published as `other_names`
        // (the card's `aliases` are other links to it).
        let mut v: Value = serde_json::from_slice(&read("org-042nb2s44.json")).unwrap();
        v["names"]
            .as_array_mut()
            .unwrap()
            .push(json!({"value": "Mass Tech", "types": ["alias"], "lang": null}));
        let raw = serde_json::to_vec(&v).unwrap();
        let card = parse(200, &raw, "042nb2s44").unwrap().unwrap();
        assert_eq!(card.details["other_names"], json!(["Mass Tech"]));
        assert!(!card.details.contains_key("aliases"));
    }

    #[test]
    fn mit() {
        let card = parse(200, &read("org-042nb2s44.json"), "042nb2s44")
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "EducationalOrganization");
        assert_eq!(card.id, "ror:042nb2s44");
        assert_eq!(card.url, "https://ror.org/042nb2s44");
        assert_eq!(
            card.name.as_deref(),
            Some("Massachusetts Institute of Technology")
        );
        assert_eq!(card.start, None, "established is a year");
        assert_eq!(card.details["established"], 1861);
        assert_eq!(card.point, Point::new(42.3751, -71.10561));
        assert_eq!(card.details["city"], "Cambridge");
        assert_eq!(card.details["country"], "United States");
        assert_eq!(card.details["website"], "https://web.mit.edu");
        assert_eq!(card.details["acronyms"], json!(["MIT"]));
        assert_eq!(
            card.details["labels"],
            json!(["Instituto Tecnológico de Massachusetts"])
        );
        assert_eq!(card.details["types"], json!(["education", "funder"]));
        assert_eq!(card.details["status"], "active");
        assert_eq!(
            keys(&card),
            [
                "fundref:100006919",
                "grid:grid.116068.8",
                "isni:0000000123412786",
                "wikidata:Q49108"
            ]
        );
        assert_eq!(
            card.also_on[0].uri,
            "http://en.wikipedia.org/wiki/Massachusetts_Institute_of_Technology"
        );
    }

    #[test]
    fn california_digital_library() {
        let card = parse(200, &read("org-03yrm5c26.json"), "03yrm5c26")
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "ArchiveOrganization");
        assert_eq!(card.name.as_deref(), Some("California Digital Library"));
        assert_eq!(card.details["established"], 1997);
        assert_eq!(card.details["city"], "Oakland");
        assert_eq!(card.attribution.as_deref(), Some(ATTRIBUTION));
        // Single values count even when ROR names none as preferred.
        assert_eq!(
            keys(&card),
            [
                "grid:grid.463323.3",
                "isni:0000000119575136",
                "wikidata:Q5020447"
            ]
        );
    }

    #[test]
    fn types_and_keys() {
        let t = |v: &[&str]| card_type(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(t(&["funder"]), "Organization");
        assert_eq!(t(&["company", "funder"]), "Corporation");
        assert_eq!(t(&["education", "healthcare"]), "Organization");
        assert_eq!(t(&[]), "Organization");
        let ambiguous = json!({"type": "wikidata", "all": ["Q1", "Q2"], "preferred": null});
        assert_eq!(external_key(&ambiguous), None);
        let isni = json!({"type": "isni", "all": ["0000 0001 2341 278X"], "preferred": null});
        assert_eq!(
            external_key(&isni).unwrap().as_str(),
            "isni:000000012341278X"
        );
        let broken = json!({"type": "isni", "all": ["0000 0001 2341 27é"], "preferred": null});
        assert_eq!(external_key(&broken), None);
        let other = json!({"type": "orgref", "all": ["1"], "preferred": "1"});
        assert_eq!(external_key(&other), None);
    }

    #[test]
    fn roots_and_aliases() {
        let r = Ror;
        for uri in ["ror:03yrm5c26", "ror:042nb2s44"] {
            assert!(r.reads(uri), "{uri}");
            assert_eq!(r.alias_root(uri), None, "{uri}");
            assert!(!r.invalid(uri), "{uri}");
        }
        assert_eq!(
            r.alias_root("https://www.ror.org/042NB2S44").as_deref(),
            Some("ror:042nb2s44")
        );
        assert_eq!(
            r.alias_root("https://api.ror.org/v2/organizations/03yrm5c26")
                .as_deref(),
            Some("ror:03yrm5c26")
        );
        for uri in [
            "ror:042nb2s45",
            "ror:042NB2S44",
            "ror:",
            "ror:https://ror.org/042nb2s44",
        ] {
            assert!(r.invalid(uri), "{uri}");
            assert!(!r.reads(uri), "{uri}");
        }
        assert!(!r.invalid("https://example.org/ror:x"));
        for example in r.examples() {
            assert!(r.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, id) in [
            ("org-03yrm5c26.json", "03yrm5c26"),
            ("org-042nb2s44.json", "042nb2s44"),
        ] {
            let card = parse(200, &read(file), id).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Ror, &card);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
            assert_eq!(doc.attributes["page_url"], card.url);
        }
        let card = parse(200, &read("org-042nb2s44.json"), "042nb2s44")
            .unwrap()
            .unwrap();
        let doc = card.into_doc(ResourceMeta::new("ror:042nb2s44", "ror", 60), SITE);
        assert_eq!(doc.attributes["wikidata"], "Q49108");
        assert_eq!(doc.attributes["isni"], "0000000123412786");
        assert_eq!(doc.attributes["fundref"], "100006919");
        assert_eq!(doc.attributes["grid"], "grid.116068.8");
        assert_eq!(doc.attributes["established"], "1861");
        assert_eq!(doc.site, "ROR");
    }

    #[test]
    fn missing_mismatched_and_failed() {
        assert_eq!(
            parse(404, &read("org-missing.json"), "0zzzzzz02").unwrap(),
            None
        );
        assert!(parse(200, &read("org-042nb2s44.json"), "03yrm5c26").is_err());
        assert!(parse(429, b"", "03yrm5c26").is_err());
        assert!(parse(502, b"", "03yrm5c26").is_err());
    }
}
