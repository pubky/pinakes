//! Swiss federal law from Fedlex, the Confederation's publication platform, read through its
//! SPARQL endpoint (JOLux) by ELI. Every Fedlex address of an act in the classified compilation,
//! in any language, is an alias of its ELI; a dated version is its own resource. No key.
use super::{sparql_rows, sparql_url, web_url, Context, Example, Row, Source, SPARQL_JSON};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct Fedlex;

pub const ENDPOINT: &str = "https://fedlex.data.admin.ch/sparqlendpoint";
pub const ATTRIBUTION: &str = "Source: Fedlex, www.fedlex.admin.ch";

/// What a Fedlex link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// An act of the classified compilation: `eli/cc/{a}/{b}`, or one dated version of it
    /// (`eli/cc/{a}/{b}/{yyyymmdd}`).
    Act(String, Option<String>),
    /// The old classified-compilation number of an act (`admin.ch/opc/…/19995395/…`).
    Dossier(String),
}

fn eli_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'_')
}

impl Key {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match (url.host_str()?, segments.as_slice()) {
            (
                "www.fedlex.admin.ch" | "fedlex.admin.ch" | "fedlex.data.admin.ch",
                ["eli", "cc", a, b, ..],
            ) if eli_part(a) && eli_part(b) => {
                let version = segments
                    .get(4)
                    .filter(|v| chrono::NaiveDate::parse_from_str(v, "%Y%m%d").is_ok())
                    .map(|v| (*v).to_owned());
                Some(Self::Act(format!("eli/cc/{a}/{b}"), version))
            }
            ("www.admin.ch" | "admin.ch", ["opc", _, "classified-compilation", dossier, ..])
                if dossier.len() == 8 && dossier.bytes().all(|b| b.is_ascii_digit()) =>
            {
                Some(Self::Dossier((*dossier).to_owned()))
            }
            _ => None,
        }
    }
}

/// The act's ELI URI, the root of every address of it; a dated version's adds its date.
pub fn root(eli: &str, version: Option<&str>) -> String {
    match version {
        Some(v) => format!("https://fedlex.data.admin.ch/{eli}/{v}"),
        None => format!("https://fedlex.data.admin.ch/{eli}"),
    }
}

/// The act's titles, abbreviations, SR number, dates, status and type, one row per language.
pub fn act_query(eli: &str) -> String {
    let act = root(eli, None);
    format!(
        "PREFIX jolux: <http://data.legilux.public.lu/resource/ontology/jolux#>\n\
         PREFIX skos: <http://www.w3.org/2004/02/skos/core#>\n\
         PREFIX dcterms: <http://purl.org/dc/terms/>\n\
         SELECT DISTINCT ?lang ?title ?short ?sr ?dateDocument ?dateEntryInForce \
         ?latestApplicability ?status ?statusLabel ?typeLabel ?dossier WHERE {{\n  \
         BIND(<{act}> AS ?act)\n  \
         ?act jolux:isRealizedBy ?expr .\n  \
         ?expr jolux:language ?lang ; jolux:title ?title .\n  \
         OPTIONAL {{ ?expr jolux:titleShort ?short }}\n  \
         OPTIONAL {{ ?act jolux:classifiedByTaxonomyEntry/skos:notation ?sr }}\n  \
         OPTIONAL {{ ?act jolux:dateDocument ?dateDocument }}\n  \
         OPTIONAL {{ ?act jolux:dateEntryInForce ?dateEntryInForce }}\n  \
         OPTIONAL {{ ?act dcterms:identifier ?dossier }}\n  \
         OPTIONAL {{ ?act jolux:inForceStatus ?status . OPTIONAL {{ ?status skos:prefLabel \
         ?statusLabel FILTER(lang(?statusLabel) = \"en\") }} }}\n  \
         OPTIONAL {{ ?act jolux:typeDocument ?t . ?t skos:prefLabel ?typeLabel \
         FILTER(lang(?typeLabel) = \"en\") }}\n  \
         OPTIONAL {{ SELECT (MAX(?d) AS ?latestApplicability) WHERE {{\n    \
         ?cons jolux:isMemberOf <{act}> ; jolux:dateApplicability ?d .\n    \
         FILTER(STR(?d) <= SUBSTR(STR(NOW()), 1, 10)) }} }}\n}}"
    )
}

/// The act (with a status, so in the classified compilation) that carries this old number.
pub fn dossier_query(dossier: &str) -> String {
    format!(
        "PREFIX jolux: <http://data.legilux.public.lu/resource/ontology/jolux#>\n\
         PREFIX dcterms: <http://purl.org/dc/terms/>\n\
         SELECT DISTINCT ?act ?status WHERE {{\n  \
         ?act dcterms:identifier \"{dossier}\"^^<https://fedlex.data.admin.ch/datatype/dossier> \
         ; jolux:inForceStatus ?status .\n}}"
    )
}

/// The ELI of the one act a dossier query found.
pub fn dossier_act(body: &[u8]) -> Result<Option<String>> {
    let mut acts: Vec<String> = sparql_rows(body)?
        .iter()
        .filter_map(|r| r.get("act"))
        .filter_map(|(a, _)| a.strip_prefix("https://fedlex.data.admin.ch/"))
        // The ELI goes into the next query: it must have the shape a link's would.
        .filter(|a| {
            matches!(
                a.split('/').collect::<Vec<_>>().as_slice(),
                ["eli", "cc", x, y] if eli_part(x) && eli_part(y)
            )
        })
        .map(str::to_owned)
        .collect();
    acts.sort();
    acts.dedup();
    Ok(match acts.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    })
}

fn language(row: &Row) -> &str {
    row.get("lang")
        .and_then(|(l, _)| l.rsplit('/').next())
        .unwrap_or_default()
}
fn value<'a>(rows: &'a [Row], var: &str) -> Option<&'a str> {
    rows.iter()
        .filter_map(|r| r.get(var))
        .map(|(v, _)| v.trim())
        .find(|v| !v.is_empty())
}

/// The card of an act from its rows, or `None` when the query found none.
pub fn card(
    mut doc: ResourceMeta,
    eli: &str,
    version: Option<&str>,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    anyhow::ensure!(status == 200, "the SPARQL endpoint answered {status}");
    let rows = sparql_rows(body)?;
    if rows.is_empty() {
        return Ok(None);
    }
    // German first, then French, Italian, English and Romansh.
    let by_language = |code: &str| rows.iter().find(|r| language(r) == code);
    let main = ["DEU", "FRA", "ITA", "ENG", "ROH"]
        .iter()
        .find_map(|c| by_language(c))
        .unwrap_or(&rows[0]);
    let field = |row: &Row, var: &str| {
        row.get(var)
            .map(|(v, _)| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    doc.set_type("Legislation");
    doc.attribution = Some(ATTRIBUTION.into());
    doc.name = Some(field(main, "title").context("no title")?);
    let sr = value(&rows, "sr").map(str::to_owned);
    let about: Vec<String> = [
        sr.as_ref().map(|sr| format!("SR {sr}")),
        field(main, "short"),
        value(&rows, "statusLabel").map(str::to_owned),
    ]
    .into_iter()
    .flatten()
    .collect();
    doc.description = (!about.is_empty()).then(|| about.join(" · "));
    for (key, value) in [
        ("sr_number", sr),
        ("abbreviation", field(main, "short")),
        (
            "english_title",
            by_language("ENG").and_then(|r| field(r, "title")),
        ),
        (
            "document_date",
            value(&rows, "dateDocument").map(str::to_owned),
        ),
        (
            "entry_into_force",
            value(&rows, "dateEntryInForce").map(str::to_owned),
        ),
        (
            "applicable_version",
            value(&rows, "latestApplicability").map(str::to_owned),
        ),
        ("status", value(&rows, "statusLabel").map(str::to_owned)),
        (
            "document_type",
            value(&rows, "typeLabel").map(str::to_owned),
        ),
        (
            "version_date",
            version.and_then(|v| {
                let d = chrono::NaiveDate::parse_from_str(v, "%Y%m%d").ok()?;
                Some(d.format("%Y-%m-%d").to_string())
            }),
        ),
        (
            "page_url",
            Some(match version {
                Some(v) => format!("https://www.fedlex.admin.ch/{eli}/{v}/de"),
                None => format!("https://www.fedlex.admin.ch/{eli}/de"),
            }),
        ),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    doc.source_root = Some(root(eli, version));
    Ok(Some(doc))
}

#[async_trait]
impl Source for Fedlex {
    fn name(&self) -> &'static str {
        "fedlex"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "fedlex",
                uri: "https://www.fedlex.admin.ch/eli/cc/1999/404/de",
                card_type: "Legislation",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "fedlex.data.admin.ch",
                path: "/sparqlendpoint",
                query: &[],
                header: None,
                content_type: "application/sparql-results+json",
                file: "eli_cc_1999_404.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        Key::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let Key::Act(eli, version) = Key::of_uri(uri)? else {
            return None;
        };
        let root = root(&eli, version.as_deref());
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "sr_number",
            "abbreviation",
            "english_title",
            "document_date",
            "entry_into_force",
            "applicable_version",
            "status",
            "document_type",
            "version_date",
            "page_url",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.fedlex.admin.ch/eli/cc/1999/404/de",
                card_type: "Legislation",
            },
            Example {
                uri: "https://www.fedlex.admin.ch/eli/cc/24/233_245_233/fr",
                card_type: "Legislation",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let options = FetchOptions {
            accept: Some(SPARQL_JSON.into()),
            extra_types: &[SPARQL_JSON],
            ..Default::default()
        };
        let (eli, version) = match Key::of_uri(uri).context("not a Fedlex link")? {
            Key::Act(eli, version) => (eli, version),
            Key::Dossier(dossier) => {
                let f = cx
                    .fetch(&sparql_url(ENDPOINT, &dossier_query(&dossier)), &options)
                    .await?;
                anyhow::ensure!(f.status == 200, "the SPARQL endpoint answered {}", f.status);
                match dossier_act(&f.body)? {
                    Some(eli) => (eli, None),
                    None => return Ok(cx.missing(uri, self)),
                }
            }
        };
        let f = cx
            .fetch(&sparql_url(ENDPOINT, &act_query(&eli)), &options)
            .await?;
        Ok(card(
            cx.card(uri, self),
            &eli,
            version.as_deref(),
            f.status,
            &f.body,
        )?
        .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("fedlex", file)
    }
    fn base() -> ResourceMeta {
        ResourceMeta::new(&root("eli/cc/1999/404", None), "fedlex", 3600)
    }

    #[test]
    fn addresses() {
        let constitution = root("eli/cc/1999/404", None);
        for uri in [
            "https://www.fedlex.admin.ch/eli/cc/1999/404/de",
            "https://www.fedlex.admin.ch/eli/cc/1999/404/fr#art_5",
            "https://fedlex.data.admin.ch/eli/cc/1999/404/",
        ] {
            assert_eq!(
                Fedlex.alias_root(uri).as_deref(),
                Some(constitution.as_str()),
                "{uri}"
            );
        }
        assert_eq!(Fedlex.alias_root(&constitution), None);
        // A dated version is its own resource.
        assert_eq!(
            Fedlex
                .alias_root("https://www.fedlex.admin.ch/eli/cc/1999/404/20240303/de")
                .as_deref(),
            Some("https://fedlex.data.admin.ch/eli/cc/1999/404/20240303")
        );
        assert_eq!(
            Key::of_uri("https://www.admin.ch/opc/de/classified-compilation/19995395/index.html"),
            Some(Key::Dossier("19995395".into()))
        );
        for uri in [
            "https://www.fedlex.admin.ch/eli/oc/1999/404/de",
            "https://www.fedlex.admin.ch/de/home",
            "https://www.admin.ch/opc/de/classified-compilation/1999.html",
        ] {
            assert!(!Fedlex.reads(uri), "{uri}");
        }
    }

    #[test]
    fn queries_match_the_recorded_ones() {
        let recorded: serde_json::Value = serde_json::from_slice(&read("queries.json")).unwrap();
        let words = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            words(&act_query("eli/cc/1999/404")),
            words(recorded["eli_cc_1999_404"].as_str().unwrap())
        );
        assert_eq!(
            words(&dossier_query("19995395")),
            words(recorded["dossier_19995395"].as_str().unwrap())
        );
    }

    #[test]
    fn act_cards() {
        let dated = card(
            base(),
            "eli/cc/1999/404",
            Some("20240303"),
            200,
            &read("eli_cc_1999_404.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dated.attributes["version_date"], "2024-03-03");
        assert_eq!(
            dated.source_root.as_deref(),
            Some("https://fedlex.data.admin.ch/eli/cc/1999/404/20240303")
        );
        let doc = card(
            base(),
            "eli/cc/1999/404",
            None,
            200,
            &read("eli_cc_1999_404.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "Legislation");
        assert_eq!(
            doc.name.as_deref(),
            Some("Bundesverfassung der Schweizerischen Eidgenossenschaft vom 18. April 1999")
        );
        assert_eq!(doc.description.as_deref(), Some("SR 101 · BV · In force"));
        let a = &doc.attributes;
        assert_eq!(a["sr_number"], "101");
        assert_eq!(
            a["english_title"],
            "Federal Constitution of 18 April 1999 of the Swiss Confederation"
        );
        assert_eq!(a["entry_into_force"], "2000-01-01");
        assert_eq!(a["applicable_version"], "2024-03-03");
        assert_eq!(
            a["page_url"],
            "https://www.fedlex.admin.ch/eli/cc/1999/404/de"
        );
        assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));
        let civil = card(
            base(),
            "eli/cc/24/233_245_233",
            None,
            200,
            &read("eli_cc_24_233_245_233.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(civil.attributes["sr_number"], "210");
        assert!(card(
            base(),
            "eli/cc/1999/99999",
            None,
            200,
            &read("eli_missing_cc_1999_99999.json")
        )
        .unwrap()
        .is_none());
        assert_eq!(
            dossier_act(&read("dossier_19995395.json"))
                .unwrap()
                .as_deref(),
            Some("eli/cc/1999/404")
        );
        let odd = br#"{"results":{"bindings":[{"act":{"type":"uri","value":"https://fedlex.data.admin.ch/eli/cc/1999/404> } UNION { ?s ?p ?o"}}]}}"#;
        assert_eq!(dossier_act(odd).unwrap(), None);
    }
}
