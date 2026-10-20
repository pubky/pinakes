//! EU law from EUR-Lex, read through the Publications Office's SPARQL endpoint (the EUR-Lex pages
//! are behind a bot challenge). An act's CELEX number is its key: every EUR-Lex address of it is
//! an alias of its CELEX page, and an ELI, Official Journal or Cellar address becomes one after
//! its first fetch. Reuse with acknowledgement of the source (Commission Decision 2011/833/EU).
use super::{
    sparql_literal, sparql_rows, sparql_url, web_url, Context, Example, Row, Source, SPARQL_JSON,
};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct EurLex;

pub const ENDPOINT: &str = "https://publications.europa.eu/webapi/rdf/sparql";
pub const ATTRIBUTION: &str = "© European Union, https://eur-lex.europa.eu";

/// What an EUR-Lex link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// A CELEX number, e.g. `32016R0679`.
    Celex(String),
    /// An ELI, e.g. `http://data.europa.eu/eli/reg/2016/679/oj`.
    Eli(String),
    /// An Official Journal act id from October 2023 on, e.g. `L_202401689`.
    Oj(String),
    /// A Cellar work, by its UUID.
    Cellar(String),
}

/// A CELEX number: a sector digit, a year, a document type and a number, with optional suffixes
/// for corrigenda and consolidated versions. Upper case.
pub fn celex(value: &str) -> Option<String> {
    let v = value.trim().to_ascii_uppercase();
    // Bytes, not string slices: a link may carry any character.
    let b = v.as_bytes();
    let ok = (8..=40).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"()-._".contains(c))
        && b[0].is_ascii_alphanumeric()
        && b[1..5].iter().all(u8::is_ascii_digit);
    ok.then_some(v)
}
fn eli_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn uuid(value: &str) -> Option<String> {
    let v = value.to_ascii_lowercase();
    let parts: Vec<&str> = v.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    (parts.len() == 5
        && parts
            .iter()
            .zip(lens)
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit())))
    .then_some(v)
}

impl Key {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        let host = url.host_str()?;
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match host {
            "eur-lex.europa.eu" => {
                if segments.first() == Some(&"eli") {
                    return eli(&segments[1..]);
                }
                if !matches!(segments.first(), Some(&"legal-content")) {
                    return None;
                }
                let uri = url
                    .query_pairs()
                    .find(|(k, _)| k.eq_ignore_ascii_case("uri"))?
                    .1
                    .into_owned();
                let (scheme, id) = uri.split_once(':')?;
                if scheme.eq_ignore_ascii_case("celex") {
                    celex(id).map(Self::Celex)
                } else if scheme.eq_ignore_ascii_case("oj") {
                    // Only the act ids of the Official Journal since October 2023 name one act.
                    let id = id.to_ascii_uppercase();
                    let (series, number) = id.split_once('_')?;
                    (matches!(series, "L" | "C")
                        && number.len() == 9
                        && number.bytes().all(|b| b.is_ascii_digit()))
                    .then_some(Self::Oj(id))
                } else {
                    None
                }
            }
            "data.europa.eu" if segments.first() == Some(&"eli") => eli(&segments[1..]),
            "publications.europa.eu" => match segments.as_slice() {
                ["resource", "cellar", id, ..] => uuid(id.split('.').next()?).map(Self::Cellar),
                _ => None,
            },
            "op.europa.eu" => match segments.as_slice() {
                [_, "publication-detail", "-", "publication", id, ..] => uuid(id).map(Self::Cellar),
                _ => None,
            },
            _ => None,
        }
    }
    /// The SPARQL line that selects `?work`.
    fn select(&self) -> String {
        match self {
            Self::Celex(c) => format!(
                "?work cdm:resource_legal_id_celex \"{}\"^^xsd:string .",
                sparql_literal(c)
            ),
            Self::Eli(e) => format!(
                "?work cdm:resource_legal_eli \"{}\"^^xsd:anyURI .",
                sparql_literal(e)
            ),
            Self::Oj(id) => {
                format!("?work owl:sameAs <http://publications.europa.eu/resource/oj/{id}> .")
            }
            Self::Cellar(id) => format!(
                "BIND(<http://publications.europa.eu/resource/cellar/{id}> AS ?work) \
                 ?work cdm:resource_legal_id_celex [] ."
            ),
        }
    }
    pub fn query(&self) -> String {
        format!(
            "PREFIX cdm: <http://publications.europa.eu/ontology/cdm#>\n\
             PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n\
             PREFIX owl: <http://www.w3.org/2002/07/owl#>\n\
             SELECT ?work ?celex ?eli ?type ?date ?inforce ?title WHERE {{\n  {}\n  \
             OPTIONAL {{ ?work cdm:resource_legal_id_celex ?celex }}\n  \
             OPTIONAL {{ ?work cdm:resource_legal_eli ?eli }}\n  \
             OPTIONAL {{ ?work cdm:work_has_resource-type ?type }}\n  \
             OPTIONAL {{ ?work cdm:work_date_document ?date }}\n  \
             OPTIONAL {{ ?work cdm:resource_legal_in-force ?inforce }}\n  \
             OPTIONAL {{ ?expr cdm:expression_belongs_to_work ?work ;\n    \
             cdm:expression_uses_language \
             <http://publications.europa.eu/resource/authority/language/ENG> ;\n    \
             cdm:expression_title ?title }}\n}}",
            self.select()
        )
    }
}

/// `eli/{type}/{year}/{number}/oj[/{language}/…]` as the ELI `http://data.europa.eu/eli/…/oj`.
fn eli(segments: &[&str]) -> Option<Key> {
    let [kind, year, number, "oj", ..] = segments else {
        return None;
    };
    let ok = (2..=8).contains(&kind.len())
        && kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        && year.len() == 4
        && year.bytes().all(|b| b.is_ascii_digit())
        && eli_part(number);
    ok.then(|| {
        Key::Eli(format!(
            "http://data.europa.eu/eli/{kind}/{year}/{number}/oj"
        ))
    })
}

/// The act's page, by CELEX number: the root of every address of it.
pub fn root(celex: &str) -> String {
    format!("https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX:{celex}")
}

fn first<'a>(rows: &'a [Row], var: &str) -> Option<&'a str> {
    rows.iter()
        .filter_map(|r| r.get(var))
        .map(|(v, _)| v.trim())
        .find(|v| !v.is_empty())
}

/// The card of the act the query found, or `None` when it found nothing.
pub fn card(
    mut doc: ResourceMeta,
    key: &Key,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    anyhow::ensure!(status == 200, "the SPARQL endpoint answered {status}");
    let rows = sparql_rows(body)?;
    // One work per lookup: an exact key names one act.
    let mut works: Vec<&str> = rows
        .iter()
        .filter_map(|r| r.get("work"))
        .map(|w| w.0.as_str())
        .collect();
    works.sort_unstable();
    works.dedup();
    let [_] = works.as_slice() else {
        anyhow::ensure!(works.is_empty(), "the key names several works");
        return Ok(None);
    };
    let celex = first(&rows, "celex")
        .and_then(celex)
        .context("no CELEX number")?;
    if let Key::Celex(asked) = key {
        anyhow::ensure!(*asked == celex, "the endpoint answered for another act");
    }
    let mut types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r.get("type"))
        .filter_map(|(t, _)| t.rsplit('/').next())
        .collect();
    types.sort_unstable();
    types.dedup();
    doc.set_type("Legislation");
    doc.attribution = Some(ATTRIBUTION.into());
    doc.name = Some(first(&rows, "title").unwrap_or(&celex).to_owned());
    let about: Vec<String> = [
        Some(format!("CELEX {celex}")),
        first(&rows, "date").map(str::to_owned),
    ]
    .into_iter()
    .flatten()
    .collect();
    doc.description = Some(about.join(" · "));
    let in_force = first(&rows, "inforce").map(|v| matches!(v, "1" | "true").to_string());
    for (key, value) in [
        ("celex", Some(celex.clone())),
        ("eli", first(&rows, "eli").map(str::to_owned)),
        (
            "resource_type",
            (!types.is_empty()).then(|| types.join("; ")),
        ),
        ("document_date", first(&rows, "date").map(str::to_owned)),
        ("in_force", in_force),
        ("page_url", Some(root(&celex))),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    doc.source_root = Some(root(&celex));
    Ok(Some(doc))
}

#[async_trait]
impl Source for EurLex {
    fn name(&self) -> &'static str {
        "eur_lex"
    }
    fn reads(&self, uri: &str) -> bool {
        Key::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let Key::Celex(celex) = Key::of_uri(uri)? else {
            return None;
        };
        let root = root(&celex);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "celex",
            "eli",
            "resource_type",
            "document_date",
            "in_force",
            "page_url",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX:32016R0679",
                card_type: "Legislation",
            },
            Example {
                uri: "https://eur-lex.europa.eu/eli/reg/2023/1114/oj",
                card_type: "Legislation",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let key = Key::of_uri(uri).context("not an EUR-Lex link")?;
        let f = cx
            .fetch(
                &sparql_url(ENDPOINT, &key.query()),
                &FetchOptions {
                    accept: Some(SPARQL_JSON.into()),
                    extra_types: &[SPARQL_JSON],
                    ..Default::default()
                },
            )
            .await?;
        Ok(card(cx.card(uri, self), &key, f.status, &f.body)?
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("eur_lex", file)
    }
    fn base() -> ResourceMeta {
        ResourceMeta::new(&root("32016R0679"), "eur_lex", 3600)
    }

    #[test]
    fn addresses() {
        let gdpr = root("32016R0679");
        for uri in [
            "https://eur-lex.europa.eu/legal-content/DE/TXT/?uri=CELEX:32016R0679",
            "https://eur-lex.europa.eu/legal-content/EN/ALL/?uri=CELEX%3A32016r0679&qid=1",
            "https://eur-lex.europa.eu/legal-content/FR/TXT/PDF/?uri=CELEX:32016R0679&from=EN",
        ] {
            assert_eq!(
                EurLex.alias_root(uri).as_deref(),
                Some(gdpr.as_str()),
                "{uri}"
            );
        }
        assert_eq!(EurLex.alias_root(&gdpr), None);
        let eli = Key::Eli("http://data.europa.eu/eli/reg/2016/679/oj".into());
        for uri in [
            "https://eur-lex.europa.eu/eli/reg/2016/679/oj",
            "https://eur-lex.europa.eu/eli/reg/2016/679/oj/eng",
            "http://data.europa.eu/eli/reg/2016/679/oj",
        ] {
            assert_eq!(Key::of_uri(uri), Some(eli.clone()), "{uri}");
            assert_eq!(EurLex.alias_root(uri), None, "{uri}");
        }
        assert_eq!(
            Key::of_uri("https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=OJ:L_202401689"),
            Some(Key::Oj("L_202401689".into()))
        );
        assert_eq!(
            Key::of_uri("https://publications.europa.eu/resource/cellar/01d55833-0660-11ee-b12e-01aa75ed71a1.0001.03"),
            Some(Key::Cellar("01d55833-0660-11ee-b12e-01aa75ed71a1".into()))
        );
        for uri in [
            "https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=OJ:L:2016:119:TOC",
            "https://eur-lex.europa.eu/homepage.html",
            "https://eur-lex.europa.eu/eli/reg/2016/679",
            "https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX:x",
            "https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3AAXYZ%C3%A9123",
        ] {
            assert!(!EurLex.reads(uri), "{uri}");
        }
    }

    #[test]
    fn queries() {
        let q = Key::Celex("32016R0679".into()).query();
        assert!(q.contains("?work cdm:resource_legal_id_celex \"32016R0679\"^^xsd:string ."));
        let recorded: serde_json::Value = serde_json::from_slice(&read("queries.json")).unwrap();
        assert_eq!(
            recorded["celex_32016R0679"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            q.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn cards() {
        let gdpr = Key::Celex("32016R0679".into());
        let doc = card(base(), &gdpr, 200, &read("celex_32016R0679.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "Legislation");
        assert!(doc
            .name
            .as_deref()
            .unwrap()
            .starts_with("Regulation (EU) 2016/679"));
        assert_eq!(
            doc.description.as_deref(),
            Some("CELEX 32016R0679 · 2016-04-27")
        );
        let a = &doc.attributes;
        assert_eq!(a["eli"], "http://data.europa.eu/eli/reg/2016/679/oj");
        assert_eq!(a["resource_type"], "REG");
        assert_eq!(a["in_force"], "true");
        assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));
        // An ELI link names the act's CELEX page as its root.
        let eli = Key::Eli("http://data.europa.eu/eli/reg/2016/679/oj".into());
        let doc = card(base(), &eli, 200, &read("eli_reg_2016_679_oj.json"))
            .unwrap()
            .unwrap();
        assert_eq!(
            doc.source_root.as_deref(),
            Some(root("32016R0679").as_str())
        );
        let oj = Key::Oj("L_202401689".into());
        let doc = card(base(), &oj, 200, &read("sameas_oj_L_202401689.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.attributes["celex"], "32024R1689");
        // A corrigendum is its own work with two resource types.
        let corr = Key::Celex("32016R0679R(02)".into());
        let doc = card(
            base(),
            &corr,
            200,
            &read("celex_32016R0679R02_corrigendum.json"),
        )
        .unwrap()
        .unwrap();
        assert!(doc.attributes["resource_type"].contains("CORRIGENDUM"));
        let none = Key::Celex("32099R9999".into());
        assert!(
            card(base(), &none, 200, &read("celex_missing_32099R9999.json"))
                .unwrap()
                .is_none()
        );
        assert!(card(
            base(),
            &Key::Celex("32023R1114".into()),
            200,
            &read("celex_32016R0679.json")
        )
        .is_err());
    }
}
