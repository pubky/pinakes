//! Swiss companies in the commercial register (Zefix), read as linked data from LINDAS, the
//! Confederation's linked-data service, by the company's register id (EHRA id). No key.
use super::{list, sparql_rows, sparql_url, web_url, Context, Example, Row, Source, SPARQL_JSON};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::Result;
use async_trait::async_trait;

pub struct Zefix;

pub const ENDPOINT: &str = "https://ld.admin.ch/query";
pub const ATTRIBUTION: &str = "Source: Zefix, Federal Office of Justice (EHRA), via LINDAS";

fn ehraid(value: &str) -> Option<u64> {
    super::kit::number(value, 9)
}

/// The EHRA id of a LINDAS company IRI (or one of its sub-IRIs) or a zefix.ch company page.
pub fn of_uri(uri: &str) -> Option<u64> {
    let url = web_url(uri)?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (url.host_str()?, segments.as_slice()) {
        ("register.ld.admin.ch", ["zefix", "company", id, ..]) => ehraid(id),
        ("www.zefix.ch" | "zefix.ch", [lang, "search", "entity", "list", "firm", id])
            if matches!(*lang, "de" | "fr" | "it" | "en") =>
        {
            ehraid(id)
        }
        _ => None,
    }
}

pub fn root(id: u64) -> String {
    format!("https://register.ld.admin.ch/zefix/company/{id}")
}

/// Every triple of the company, its address and identifiers, and the names of its legal form
/// and seat.
pub fn query(id: u64) -> String {
    format!(
        "PREFIX schema: <http://schema.org/> SELECT ?s ?p ?o WHERE {{ VALUES ?c {{ <{}> }} \
         {{ ?c ?p ?o BIND(?c AS ?s) }} UNION {{ ?c schema:address ?s . ?s ?p ?o }} UNION \
         {{ ?c schema:identifier ?s . ?s ?p ?o }} UNION {{ ?c schema:additionalType ?s . \
         ?s schema:name ?o BIND(schema:name AS ?p) }} UNION {{ ?c \
         <https://schema.ld.admin.ch/municipality> ?s . ?s schema:name ?o \
         BIND(schema:name AS ?p) }} }}",
        root(id)
    )
}

const SCHEMA: &str = "http://schema.org/";

/// `CHE105909036` as `CHE-105.909.036`.
fn uid(value: &str) -> Option<String> {
    let digits = value.strip_prefix("CHE")?;
    (digits.len() == 9 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("CHE-{}.{}.{}", &digits[..3], &digits[3..6], &digits[6..]))
}

/// The card of a company, or `None` when LINDAS has no such company.
pub fn card(
    mut doc: ResourceMeta,
    id: u64,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    anyhow::ensure!(status == 200, "LINDAS answered {status}");
    let rows = sparql_rows(body)?;
    let company = root(id);
    let get = |subject: &str, property: &str| -> Vec<(String, Option<String>)> {
        rows.iter()
            .filter(|r: &&Row| {
                r.get("s").is_some_and(|s| s.0 == subject)
                    && r.get("p")
                        .is_some_and(|p| p.0 == format!("{SCHEMA}{property}"))
            })
            .filter_map(|r| r.get("o").cloned())
            .collect()
    };
    let one = |subject: &str, property: &str| {
        get(subject, property)
            .into_iter()
            .map(|(v, _)| v.trim().to_owned())
            .find(|v| !v.is_empty())
    };
    let Some(name) = one(&company, "legalName") else {
        return Ok(None);
    };
    doc.set_type("Organization");
    doc.attribution = Some(ATTRIBUTION.into());
    doc.name = Some(name);
    doc.description = one(&company, "description");
    let address = format!("{company}/address");
    for (key, value) in [
        ("street", one(&address, "streetAddress")),
        ("postcode", one(&address, "postalCode")),
        ("city", one(&address, "addressLocality")),
        ("country_code", Some("ch".to_owned())),
    ] {
        if let Some(value) = value {
            doc.address.insert(key.into(), value);
        }
    }
    // The identifiers are nodes with a name and a value.
    let identifier = |kind: &str| {
        get(&company, "identifier")
            .into_iter()
            .find_map(|(node, _)| {
                (one(&node, "name").as_deref() == Some(kind))
                    .then(|| one(&node, "value"))
                    .flatten()
            })
    };
    let named = |property: &str, lang: Option<&str>| {
        let node = rows.iter().find_map(|r| {
            (r.get("s").is_some_and(|s| s.0 == company)
                && r.get("p").is_some_and(|p| p.0 == property))
            .then(|| r.get("o").map(|o| o.0.clone()))
            .flatten()
        })?;
        let names = get(&node, "name");
        names
            .iter()
            .find(|(_, l)| l.as_deref() == lang)
            .or(names.first())
            .map(|(v, _)| v.clone())
    };
    let other_names: Vec<String> = get(&company, "name")
        .into_iter()
        .map(|(v, l)| match l {
            Some(l) => format!("{v} ({l})"),
            None => v,
        })
        .collect();
    for (key, value) in [
        ("uid", identifier("CompanyUID").and_then(|u| uid(&u))),
        ("ch_id", identifier("CompanyCHID")),
        ("ehra_id", Some(id.to_string())),
        (
            "legal_form",
            named(&format!("{SCHEMA}additionalType"), Some("de")),
        ),
        (
            "seat",
            named("https://schema.ld.admin.ch/municipality", None),
        ),
        ("canton", one(&address, "addressRegion")),
        ("other_names", list(&other_names)),
        (
            "page_url",
            Some(format!(
                "https://www.zefix.ch/en/search/entity/list/firm/{id}"
            )),
        ),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for Zefix {
    fn name(&self) -> &'static str {
        "zefix"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "zefix",
                uri: "https://register.ld.admin.ch/zefix/company/126286",
                card_type: "Organization",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "ld.admin.ch",
                path: "/query",
                query: &[],
                header: None,
                content_type: "application/sparql-results+json",
                file: "sparql_company_126286_spo.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = root(of_uri(uri)?);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "uid",
            "ch_id",
            "ehra_id",
            "legal_form",
            "seat",
            "canton",
            "other_names",
            "page_url",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://register.ld.admin.ch/zefix/company/126286",
            card_type: "Organization",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = of_uri(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = cx
            .fetch(
                &sparql_url(ENDPOINT, &query(id)),
                &FetchOptions {
                    accept: Some(SPARQL_JSON.into()),
                    extra_types: &[SPARQL_JSON],
                    ..Default::default()
                },
            )
            .await?;
        Ok(card(cx.card(uri, self), id, f.status, &f.body)?
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("zefix", file)
    }

    #[test]
    fn addresses() {
        let root = root(126286);
        for uri in [
            "https://register.ld.admin.ch/zefix/company/126286/address",
            "https://register.ld.admin.ch/zefix/company/126286/UID/CHE105909036",
            "https://www.zefix.ch/en/search/entity/list/firm/126286",
            "https://www.zefix.ch/de/search/entity/list/firm/126286?name=nestle",
        ] {
            assert_eq!(
                Zefix.alias_root(uri).as_deref(),
                Some(root.as_str()),
                "{uri}"
            );
        }
        assert_eq!(Zefix.alias_root(&root), None);
        for uri in [
            "https://www.zefix.ch/en/search/entity/list?mainSearch=nestle",
            "https://register.ld.admin.ch/zefix/company/x",
        ] {
            assert!(!Zefix.reads(uri), "{uri}");
        }
    }

    #[test]
    fn the_query_matches_the_recorded_one() {
        let recorded = "PREFIX schema: <http://schema.org/> SELECT ?s ?p ?o WHERE { VALUES ?c { <https://register.ld.admin.ch/zefix/company/126286> } { ?c ?p ?o BIND(?c AS ?s) } UNION { ?c schema:address ?s . ?s ?p ?o } UNION { ?c schema:identifier ?s . ?s ?p ?o } UNION { ?c schema:additionalType ?s . ?s schema:name ?o BIND(schema:name AS ?p) } UNION { ?c <https://schema.ld.admin.ch/municipality> ?s . ?s schema:name ?o BIND(schema:name AS ?p) } }";
        assert_eq!(query(126286), recorded);
    }

    #[test]
    fn company_card() {
        let base = ResourceMeta::new(&root(126286), "zefix", 3600);
        let doc = card(
            base.clone(),
            126286,
            200,
            &read("sparql_company_126286_spo.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "Organization");
        assert_eq!(doc.name.as_deref(), Some("Nestlé AG"));
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .starts_with("Beteiligung an Industrie-"));
        assert_eq!(doc.address["street"], "Zugerstrasse 8");
        assert_eq!(doc.address["city"], "Cham");
        let a = &doc.attributes;
        assert_eq!(a["uid"], "CHE-105.909.036");
        assert_eq!(a["ch_id"], "CH17030010013");
        assert_eq!(a["legal_form"], "Aktiengesellschaft");
        assert_eq!(a["seat"], "Cham");
        assert_eq!(a["canton"], "ZG");
        assert!(a["other_names"].contains("Nestlé S.A. (fr)"));
        assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));
        let missing = read("sparql_company_999999999_spo_missing.json");
        assert!(card(base, 999999999, 200, &missing).unwrap().is_none());
    }
}
