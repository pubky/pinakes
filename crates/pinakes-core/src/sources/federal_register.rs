//! Federal Register documents, read from the official API by document number, never from the
//! HTML page (behind a bot wall). Every federalregister.gov address of a document is an alias of
//! `fr:{number}`. Public domain; no key.
use super::{list, text, texts, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct FederalRegister;

/// A document number, e.g. `2026-18291`, `E9-12345` or `R1-2026-00123`, in upper case.
pub fn document_number(value: &str) -> Option<String> {
    let value = value.to_ascii_uppercase();
    let parts: Vec<&str> = value.split('-').collect();
    let ok = (2..=3).contains(&parts.len())
        && !parts[0].is_empty()
        && parts[0].len() <= 4
        && parts[0].bytes().all(|b| b.is_ascii_alphanumeric())
        && parts[1..]
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then_some(value)
}

/// The identifier every address of the document is an alias of.
pub fn identifier(number: &str) -> String {
    format!("fr:{number}")
}

/// The document's short address on federalregister.gov, from its number alone. A card's page is
/// the address the API names (with its date and title slug), else this one; both are aliases.
pub fn page(number: &str) -> String {
    format!("https://www.federalregister.gov/d/{number}")
}

fn date(y: &str, m: &str, d: &str) -> bool {
    y.len() == 4
        && m.len() == 2
        && d.len() == 2
        && [y, m, d]
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

/// The document number of a federalregister.gov address: `/documents/{y}/{m}/{d}/{number}/…`,
/// `/d/{number}`, `/documents/{number}` and `/api/v1/documents/{number}[.json]`. The slug never
/// matters.
pub fn from_uri(uri: &str) -> Option<String> {
    let url = super::kit::web_url(uri)?;
    if !matches!(
        url.host_str()?,
        "www.federalregister.gov" | "federalregister.gov"
    ) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let number = match segments.as_slice() {
        ["documents", y, m, d, number, ..] if date(y, m, d) => number,
        ["d", number] | ["documents", number] => number,
        ["api", "v1", "documents", number] => number.strip_suffix(".json").unwrap_or(number),
        _ => return None,
    };
    document_number(number)
}

/// The document number of `fr:{number}` or of any federalregister.gov address.
pub fn number(uri: &str) -> Option<String> {
    match uri.strip_prefix("fr:") {
        Some(rest) => document_number(rest),
        None => from_uri(uri),
    }
}

/// GovInfo's day package name, `FR-YYYY-MM-DD`.
fn package(value: &str) -> bool {
    value
        .strip_prefix("FR-")
        .and_then(|d| {
            let p: Vec<&str> = d.split('-').collect();
            (p.len() == 3).then(|| date(p[0], p[1], p[2]))
        })
        .unwrap_or(false)
}

/// The document number of a GovInfo copy: `/content/pkg/FR-{date}/pdf/{number}.pdf` or
/// `/app/details/FR-{date}/{number}`. The same document, at another publisher.
pub fn from_govinfo_uri(uri: &str) -> Option<String> {
    Some(govinfo_copy(uri)?.0)
}

/// The document number of a GovInfo copy and the day of its package (`YYYY-MM-DD`): the copy is
/// the document's only when that day is the document's publication date.
pub fn govinfo_copy(uri: &str) -> Option<(String, String)> {
    let url = super::kit::web_url(uri)?;
    if !matches!(url.host_str()?, "www.govinfo.gov" | "govinfo.gov") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.collect();
    let (pkg, number) = match segments.as_slice() {
        ["content", "pkg", pkg, "pdf", file] if package(pkg) => {
            (*pkg, document_number(file.strip_suffix(".pdf")?)?)
        }
        ["app", "details", pkg, number] if package(pkg) => (*pkg, document_number(number)?),
        _ => return None,
    };
    Some((number, pkg.strip_prefix("FR-")?.to_owned()))
}

/// The SEC's PDF of a self-regulatory organization's rule-change release,
/// `https://www.sec.gov/files/rules/sro/{org}/{year}/{release}.pdf`: the organization and the
/// filing's year as the address names them (lower case, as the SEC spells its folders), and the
/// release number, e.g. `nasdaqtx`, `2026`, `34-106268`. The Federal Register publishes the
/// release as a document whose docket ids name `Release No. {release}` and the filing
/// `File No. SR-{org}-{year}-{n}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecRelease {
    pub org: String,
    pub year: String,
    pub release: String,
}

/// The SEC release an address names ([`SecRelease`]). An exhibit (`{release}-{exhibit}.pdf`) is
/// part of the filing, not the release.
pub fn sec_release(uri: &str) -> Option<SecRelease> {
    let url = super::kit::web_url(uri)?;
    if !matches!(url.host_str()?, "www.sec.gov" | "sec.gov") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.collect();
    let ["files", "rules", "sro", org, year, file] = segments.as_slice() else {
        return None;
    };
    let ok_org = !org.is_empty() && org.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let ok_year = year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit());
    let release = file.strip_suffix(".pdf")?;
    let (act, n) = release.split_once('-')?;
    let ok_release = act.len() == 2
        && act.bytes().all(|b| b.is_ascii_digit())
        && (4..=7).contains(&n.len())
        && n.bytes().all(|b| b.is_ascii_digit());
    (ok_org && ok_year && ok_release).then(|| SecRelease {
        org: org.to_ascii_lowercase(),
        year: (*year).to_owned(),
        release: release.to_owned(),
    })
}

/// The API's documents filed under a docket, by an SEC release number.
pub fn docket_url(release: &str) -> String {
    format!(
        "https://www.federalregister.gov/api/v1/documents.json?conditions%5Bdocket_id%5D={release}\
         &fields%5B%5D=document_number&fields%5B%5D=docket_ids&fields%5B%5D=publication_date&per_page=20"
    )
}

/// The one document whose docket ids name `Release No. {release}` exactly, and the address's
/// filing: its docket ids name a file of the address's organization, `SR-{org}-{year}-{n}`
/// (`File No.` or one of `File Nos.`, any case), and the address's year is that file's year or
/// the document's publication year (an order can come a year after its filing). An address with
/// another organization or year names no document. The search matches loosely, so every result
/// is checked; a release number is unique.
pub fn docket_document(body: &[u8], sec: &SecRelease) -> Result<Option<String>> {
    let v: Value = serde_json::from_slice(body)?;
    let wanted = format!("release no. {}", sec.release);
    let ids = |d: &Value| -> Vec<String> {
        d["docket_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|id| id.trim().to_ascii_lowercase())
            .collect()
    };
    let named = |d: &&Value| ids(d).contains(&wanted);
    let mut documents: Vec<(String, Vec<String>, String)> = v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(named)
        .filter_map(|d| {
            let published = d["publication_date"].as_str().unwrap_or_default();
            Some((
                document_number(d["document_number"].as_str()?)?,
                ids(d),
                published.get(..4).unwrap_or_default().to_owned(),
            ))
        })
        .collect();
    documents.sort();
    documents.dedup_by(|a, b| a.0 == b.0);
    let [(number, ids, published)] = documents.as_slice() else {
        return Ok(None);
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let organization = format!("sr-{}-", sec.org);
    let filed = ids.iter().any(|id| {
        id.strip_prefix("file nos.")
            .or_else(|| id.strip_prefix("file no."))
            .is_some_and(|files| {
                files.split(',').any(|file| {
                    file.trim()
                        .strip_prefix(&organization)
                        .and_then(|rest| rest.split_once('-'))
                        .is_some_and(|(year, n)| {
                            year.len() == 4
                                && digits(year)
                                && digits(n)
                                && (year == sec.year || *published == sec.year)
                        })
                })
            })
    });
    Ok(filed.then(|| number.clone()))
}

/// The API record of one document.
pub fn api_url(number: &str) -> Option<String> {
    Some(format!(
        "https://www.federalregister.gov/api/v1/documents/{}.json",
        document_number(number)?
    ))
}

/// Another copy or page of the document that the API record names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Related {
    pub site: &'static str,
    pub uri: String,
    /// `pdf` for a PDF copy.
    pub kind: Option<&'static str>,
}

/// A Federal Register document, as the API describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub number: String,
    pub title: String,
    /// The abstract, else the action.
    pub description: Option<String>,
    /// Rule, Proposed Rule, Notice or Presidential Document.
    pub document_type: String,
    pub agencies: Vec<String>,
    /// E.g. "91 FR 57392".
    pub citation: Option<String>,
    pub publication_date: String,
    pub docket_ids: Vec<String>,
    pub regulation_id_numbers: Vec<String>,
    pub comments_close_on: Option<String>,
    pub effective_on: Option<String>,
    /// E.g. "14 CFR 39".
    pub cfr_references: Vec<String>,
    pub page_length: Option<u64>,
    /// The document's page on federalregister.gov.
    pub html_url: Option<String>,
    /// The GovInfo PDF and details page, the regulations.gov document and the public-inspection
    /// PDF (without its query), as the record names them.
    pub related: Vec<Related>,
}

fn iso_date(v: &Value) -> Option<String> {
    text(v).filter(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok())
}

/// The document, or `None` when the API has no document with this number (404).
pub fn parse(status: u16, raw: &[u8], number: &str) -> Result<Option<Document>> {
    let Some(raw) = super::kit::body(status, raw, &[404])? else {
        return Ok(None);
    };
    let number = document_number(number).context("not a document number")?;
    let record: Value = serde_json::from_slice(raw)?;
    anyhow::ensure!(
        record["document_number"]
            .as_str()
            .and_then(document_number)
            .as_deref()
            == Some(&number),
        "the API answered for another document"
    );
    let publication_date = iso_date(&record["publication_date"]).context("no publication date")?;
    let cfr_references = record["cfr_references"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let title = r["title"].as_u64()?;
            let part = match &r["part"] {
                Value::String(s) => s.trim().to_owned(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            (!part.is_empty()).then(|| format!("{title} CFR {part}"))
        })
        .collect();

    let mut related = Vec::new();
    if let Some(pdf) = record["pdf_url"].as_str().and_then(web_url) {
        let expected = format!("/content/pkg/FR-{publication_date}/pdf/{number}.pdf");
        if pdf.host_str() == Some("www.govinfo.gov") && pdf.path() == expected {
            related.push(Related {
                site: "GovInfo",
                uri: pdf.to_string(),
                kind: Some("pdf"),
            });
        }
    }
    related.push(Related {
        site: "GovInfo",
        uri: format!("https://www.govinfo.gov/app/details/FR-{publication_date}/{number}"),
        kind: None,
    });
    if let Some(id) = record["regulations_dot_gov_info"]["document_id"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 100
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        })
    {
        related.push(Related {
            site: "Regulations.gov",
            uri: format!("https://www.regulations.gov/document/{id}"),
            kind: None,
        });
    }
    if let Some(mut pdf) = record["public_inspection_pdf_url"]
        .as_str()
        .and_then(web_url)
    {
        pdf.set_query(None);
        if pdf.host_str() == Some("public-inspection.federalregister.gov")
            && pdf.path().ends_with(".pdf")
        {
            related.push(Related {
                site: "Federal Register public inspection",
                uri: pdf.to_string(),
                kind: Some("pdf"),
            });
        }
    }

    Ok(Some(Document {
        title: text(&record["title"]).context("no title")?,
        description: text(&record["abstract"]).or_else(|| text(&record["action"])),
        document_type: text(&record["type"]).context("no type")?,
        agencies: record["agencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| text(&a["name"]).or_else(|| text(&a["raw_name"])))
            .collect(),
        citation: text(&record["citation"]),
        docket_ids: texts(&record["docket_ids"]),
        regulation_id_numbers: texts(&record["regulation_id_numbers"]),
        comments_close_on: iso_date(&record["comments_close_on"]),
        effective_on: iso_date(&record["effective_on"]),
        cfr_references,
        page_length: record["page_length"].as_u64(),
        html_url: record["html_url"]
            .as_str()
            .and_then(web_url)
            .filter(|u| from_uri(u.as_str()).as_deref() == Some(&number))
            .map(String::from),
        publication_date,
        number,
        related,
    }))
}

/// The card of a parsed document.
pub fn card(mut doc: ResourceMeta, document: Document) -> ResourceMeta {
    doc.set_type("Legislation");
    doc.site = "Federal Register".into();
    doc.name = Some(document.title);
    doc.description = document.description;
    let page = document.html_url.unwrap_or_else(|| page(&document.number));
    for (key, value) in [
        ("document_type", Some(document.document_type)),
        ("agencies", list(&document.agencies)),
        ("citation", document.citation),
        ("publication_date", Some(document.publication_date)),
        ("docket_ids", list(&document.docket_ids)),
        (
            "regulation_id_numbers",
            list(&document.regulation_id_numbers),
        ),
        ("comments_close_on", document.comments_close_on),
        ("effective_on", document.effective_on),
        ("cfr_references", list(&document.cfr_references)),
        ("page_count", document.page_length.map(|n| n.to_string())),
        ("page_url", Some(page)),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    doc
}

#[async_trait]
impl Source for FederalRegister {
    fn name(&self) -> &'static str {
        "federal_register"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "federal_register",
                    uri: "fr:2026-18291",
                    card_type: "Legislation",
                    group_with: None,
                },
                E2eSample {
                    kind: "federal_register_page",
                    uri: "https://www.federalregister.gov/d/2026-18291",
                    card_type: "Legislation",
                    group_with: Some("federal_register"),
                },
            ],
            answers: &[E2eAnswer {
                host: "www.federalregister.gov",
                path: "/api/v1/documents/2026-18291.json",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "fr-2026-18291.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        number(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&number(uri)?);
        (root != uri).then_some(root)
    }
    fn root_page(&self, root: &str) -> Option<String> {
        let number = document_number(root.strip_prefix("fr:")?)?;
        (identifier(&number) == root).then(|| page(&number))
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.strip_prefix("fr:")
            .is_some_and(|rest| document_number(rest).is_none())
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "document_type",
            "agencies",
            "citation",
            "publication_date",
            "docket_ids",
            "regulation_id_numbers",
            "comments_close_on",
            "effective_on",
            "cfr_references",
            "page_count",
            "page_url",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "fr:2026-18291",
                card_type: "Legislation",
            },
            Example {
                uri: "https://www.federalregister.gov/documents/2026/09/09/2026-18291/self-regulatory-organizations-nasdaq-texas-llc-notice-of-filing-and-order-granting-accelerated",
                card_type: "Legislation",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(number) = number(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let url = api_url(&number).context("document number")?;
        let f = cx.fetch(&url, &FetchOptions::json()).await?;
        Ok(match parse(f.status, &f.body, &number)? {
            Some(document) => card(cx.card(uri, self), document),
            None => cx.missing(uri, self),
        })
    }
}

/// The also-on half of this source, `federal_register`.
pub mod also_on {
    use crate::{
        also_on::{self, Context, Source, Subject},
        lookups::{get, secondary, with_retry},
    };
    use anyhow::Result;
    use async_trait::async_trait;

    /// A Federal Register document: its GovInfo copies, regulations.gov document and
    /// public-inspection PDF from the API record, and `fr:{number}`, which a GovInfo copy someone
    /// tags shares when its package day is the record's publication date. The SEC's PDF of a
    /// rule-change release whose number and filing the document's docket ids name is another
    /// publisher's: possibly the same document (a `maybe` link and a fact), never its key.
    pub struct FederalRegister;

    #[async_trait]
    impl Source for FederalRegister {
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            &[("fr", "2024-12345")]
        }
        fn single_namespaces(&self) -> &'static [&'static str] {
            &["fr"]
        }
        fn search_key(&self, namespace: &str, value: &str) -> Option<String> {
            (namespace == "fr").then(|| Some(format!("fr:{}", super::document_number(value)?)))?
        }
        fn id(&self) -> &'static str {
            "federal_register"
        }
        fn applies(&self, subject: &Subject) -> bool {
            use crate::sources::federal_register as fr;
            fr::number(&subject.uri).is_some()
                || fr::govinfo_copy(&subject.uri).is_some()
                || fr::sec_release(&subject.uri).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            use crate::sources::federal_register as fr;
            let key = |number: &str| also_on::Key {
                key: format!("fr:{number}"),
                via: "federalregister".into(),
            };
            // A GovInfo copy is the document's only when its package's day is the record's
            // publication date: an address with another day names no document.
            if let Some((number, day)) = fr::govinfo_copy(&subject.uri) {
                let Some(url) = fr::api_url(&number) else {
                    return Ok(also_on::Found::default());
                };
                let mut retry = None;
                let answer = secondary(
                    "federalregister",
                    &mut retry,
                    async {
                        let (status, body) = get(cx, &url, "federalregister_busy").await?;
                        fr::parse(status, &body, &number)
                    }
                    .await,
                );
                let keys = match answer {
                    Some(Some(document)) if document.publication_date == day => {
                        vec![key(&document.number)]
                    }
                    _ => vec![],
                };
                return Ok(with_retry(
                    also_on::Found {
                        keys,
                        ..Default::default()
                    },
                    retry,
                ));
            }
            // The SEC's PDF of a release and the document whose docket ids name the release and
            // its filing (exact matches) are two publishers' copies: the record names another
            // service's ID, so the document is possibly the same (a `maybe` link, no key) and its
            // number a fact; joins never cross publishers.
            if let Some(sec) = fr::sec_release(&subject.uri) {
                let mut retry = None;
                let answer = secondary(
                    "federalregister",
                    &mut retry,
                    async {
                        let (status, body) =
                            get(cx, &fr::docket_url(&sec.release), "federalregister_busy").await?;
                        anyhow::ensure!(status == 200, "Federal Register answered {status}");
                        fr::docket_document(&body, &sec)
                    }
                    .await,
                );
                let found = match answer.flatten() {
                    Some(number) => also_on::Found {
                        links: vec![also_on::Link::new(
                            format!("https://www.federalregister.gov/d/{number}"),
                            "Federal Register",
                            "federalregister",
                        )
                        .maybe()],
                        facts: vec![also_on::Fact::new("fr_document", number)],
                        ..Default::default()
                    },
                    None => also_on::Found::default(),
                };
                return Ok(with_retry(found, retry));
            }
            let Some(number) = fr::number(&subject.uri) else {
                return Ok(also_on::Found::default());
            };
            let Some(url) = fr::api_url(&number) else {
                return Ok(also_on::Found::default());
            };
            // The document's number is its key whatever the API answers; a number the API doesn't
            // know still names nothing.
            let mut retry = None;
            let answer = secondary(
                "federalregister",
                &mut retry,
                async {
                    let (status, body) = get(cx, &url, "federalregister_busy").await?;
                    fr::parse(status, &body, &number)
                }
                .await,
            );
            let Some(document) = answer else {
                return Ok(with_retry(
                    also_on::Found {
                        keys: vec![key(&number)],
                        ..Default::default()
                    },
                    retry,
                ));
            };
            let Some(document) = document else {
                return Ok(also_on::Found::default());
            };
            let links = document
                .related
                .into_iter()
                .map(|r| {
                    let mut link = also_on::Link::new(r.uri, r.site, "federalregister");
                    link.kind = r.kind.map(str::to_owned);
                    link
                })
                .collect();
            Ok(also_on::Found {
                keys: vec![key(&document.number)],
                links,
                ..Default::default()
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::lookups::fixture::{at, keys, sites, JSON};

        #[tokio::test(start_paused = true)]
        async fn federal_register_and_its_govinfo_copies() {
            let record = "https://www.federalregister.gov/api/v1/documents/2026-18291.json";
            let (egress, _) = crate::sources::fixture::egress(vec![(
                record,
                200,
                JSON,
                crate::sources::fixture::read("federal_register", "fr-2026-18291.json"),
            )]);
            let cx = Context {
                egress,
                config: Default::default(),
            };
            let doc = at("fr:2026-18291");
            assert!(FederalRegister.applies(&doc));
            let found = FederalRegister.lookup(&cx, &doc).await.unwrap();
            assert_eq!(keys(&found), ["fr:2026-18291"]);
            let s = sites(&found);
            assert!(
                s.contains(
                    &"GovInfo https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291"
                        .to_owned()
                ),
                "{s:?}"
            );
            assert!(
                s.contains(
                    &"Regulations.gov https://www.regulations.gov/document/SEC-2026-5584-0001"
                        .to_owned()
                ),
                "{s:?}"
            );
            assert!(found.links.iter().any(|l| l.kind.as_deref() == Some("pdf")));
            // A GovInfo copy someone tags shares the key when its package's day is the record's
            // publication date.
            for copy in [
                "https://www.govinfo.gov/content/pkg/FR-2026-09-09/pdf/2026-18291.pdf",
                "https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291",
            ] {
                let copy = at(copy);
                assert!(FederalRegister.applies(&copy));
                let found = FederalRegister.lookup(&cx, &copy).await.unwrap();
                assert_eq!(
                    (keys(&found), found.links.len()),
                    (vec!["fr:2026-18291"], 0)
                );
            }
        }

        /// An address naming the document under another day is no copy of it: no key.
        #[tokio::test(start_paused = true)]
        async fn a_govinfo_address_with_another_day_gets_no_key() {
            let record = "https://www.federalregister.gov/api/v1/documents/2026-18291.json";
            let (egress, _) = crate::sources::fixture::egress(vec![(
                record,
                200,
                JSON,
                crate::sources::fixture::read("federal_register", "fr-2026-18291.json"),
            )]);
            let cx = Context {
                egress,
                config: Default::default(),
            };
            for invented in [
                "https://www.govinfo.gov/app/details/FR-1900-01-01/2026-18291",
                "https://www.govinfo.gov/content/pkg/FR-2026-09-10/pdf/2026-18291.pdf",
            ] {
                let copy = at(invented);
                assert!(FederalRegister.applies(&copy));
                let found = FederalRegister.lookup(&cx, &copy).await.unwrap();
                assert!(found.keys.is_empty(), "{invented}");
            }
        }

        /// The SEC's PDF of a rule-change release is another publisher's copy: the document
        /// whose docket ids name the release and its filing is a `maybe` link and a fact, never
        /// its key; an address whose organization or year the filing does not name, or a release
        /// no document names, gets nothing.
        #[tokio::test(start_paused = true)]
        async fn an_sec_release_pdf_is_possibly_its_document_never_its_key() {
            let docket = |release: &str| {
                url::Url::parse(&super::super::docket_url(release))
                    .unwrap()
                    .to_string()
            };
            let (found_url, none_url) = (docket("34-106268"), docket("34-999999"));
            let (egress, _) = crate::sources::fixture::egress(vec![
                (
                    found_url.as_str(),
                    200,
                    JSON,
                    crate::sources::fixture::read("federal_register", "fr-docket-34-106268.json"),
                ),
                (
                    none_url.as_str(),
                    200,
                    JSON,
                    crate::sources::fixture::read("federal_register", "fr-docket-34-999999.json"),
                ),
            ]);
            let cx = Context {
                egress,
                config: Default::default(),
            };
            let pdf = "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268.pdf";
            let copy = at(pdf);
            assert!(FederalRegister.applies(&copy), "{pdf}");
            let found = FederalRegister.lookup(&cx, &copy).await.unwrap();
            assert!(found.keys.is_empty(), "{pdf}: {:?}", found.keys);
            let links: Vec<(&str, &str, Option<&str>, Option<&str>)> = found
                .links
                .iter()
                .map(|l| {
                    (
                        l.uri.as_str(),
                        l.site.as_str(),
                        l.kind.as_deref(),
                        l.key.as_deref(),
                    )
                })
                .collect();
            assert_eq!(
                links,
                [(
                    "https://www.federalregister.gov/d/2026-18291",
                    "Federal Register",
                    Some(also_on::MAYBE),
                    None
                )]
            );
            assert_eq!(
                found.facts,
                [also_on::Fact::new("fr_document", "2026-18291")]
            );
            // An exhibit is part of the filing, not a copy of the document.
            assert!(!FederalRegister.applies(&at(
                "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268-ex5.pdf"
            )));
            for nothing in [
                "https://www.sec.gov/files/rules/sro/invented/1901/34-106268.pdf",
                "https://www.sec.gov/files/rules/sro/nasdaqtx/1901/34-106268.pdf",
                "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-999999.pdf",
            ] {
                let found = FederalRegister.lookup(&cx, &at(nothing)).await.unwrap();
                assert_eq!(found, also_on::Found::default(), "{nothing}");
            }
        }

        /// Its key namespace, as searches name it.
        #[test]
        fn search_keys() {
            assert_eq!(
                crate::lookups::search_key("fr", "2026-18291").as_deref(),
                Some("fr:2026-18291")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("federal_register", file)
    }
    fn card_of(file: &str, number: &str) -> ResourceMeta {
        let document = parse(200, &fixture(file), number).unwrap().unwrap();
        card(
            ResourceMeta::new(&identifier(number), "federal_register", 86400),
            document,
        )
    }

    #[test]
    fn sec_release_copies_and_their_docket() {
        let release = |uri: &str| sec_release(uri).map(|s| s.release);
        let nasdaqtx =
            sec_release("https://www.sec.gov/files/rules/sro/NasdaqTX/2026/34-106268.pdf").unwrap();
        assert_eq!(
            (
                nasdaqtx.org.as_str(),
                nasdaqtx.year.as_str(),
                nasdaqtx.release.as_str()
            ),
            ("nasdaqtx", "2026", "34-106268")
        );
        for bad in [
            "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/notice.pdf",
            "https://www.sec.gov/files/rules/sro/nasdaqtx/26/34-106268.pdf",
            "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268-.pdf",
            "https://example.org/files/rules/sro/nasdaqtx/2026/34-106268.pdf",
        ] {
            assert_eq!(release(bad), None, "{bad}");
        }
        let at = |org: &str, year: &str, release: &str| SecRelease {
            org: org.into(),
            year: year.into(),
            release: release.into(),
        };
        let found = fixture("fr-docket-34-106268.json");
        assert_eq!(
            docket_document(&found, &nasdaqtx).unwrap().as_deref(),
            Some("2026-18291")
        );
        // The search matches loosely; only an exact `Release No.` docket id counts.
        assert_eq!(
            docket_document(&found, &at("nasdaqtx", "2026", "34-10626")).unwrap(),
            None
        );
        // The address's organization and year are the filing's (`File No. SR-NasdaqTX-2026-039`):
        // an invented one names no document.
        for (org, year) in [
            ("invented", "2026"),
            ("nasdaqtx", "1901"),
            ("nasdaq", "2026"),
        ] {
            assert_eq!(
                docket_document(&found, &at(org, year, "34-106268")).unwrap(),
                None,
                "{org}/{year}"
            );
        }
        let filed = |ids: &str| {
            let body =
                format!(r#"{{"results":[{{"document_number":"2026-18291","docket_ids":{ids}}}]}}"#);
            docket_document(body.as_bytes(), &nasdaqtx).unwrap()
        };
        assert_eq!(
            filed(
                r#"["Release No. 34-106268", "File Nos. SR-NYSE-2026-01, SR-NasdaqTX-2026-039"]"#
            )
            .as_deref(),
            Some("2026-18291")
        );
        assert_eq!(
            filed(
                r#"["Release No. 34-106268", "File Nos. SR-NasdaqTX-2026-039, SR-NYSE-2026-01"]"#
            )
            .as_deref(),
            Some("2026-18291")
        );
        assert_eq!(filed(r#"["Release No. 34-106268"]"#), None);
        // The address's year may be the document's publication year instead of the filing's:
        // an order a year after its filing. Neither year: nothing.
        let ordered = |year: &str, published: &str| {
            let body = format!(
                r#"{{"results":[{{"document_number":"2027-00012","publication_date":"{published}","docket_ids":["Release No. 34-106268","File No. SR-NasdaqTX-2026-039"]}}]}}"#
            );
            docket_document(body.as_bytes(), &at("nasdaqtx", year, "34-106268")).unwrap()
        };
        assert_eq!(ordered("2027", "2027-01-08").as_deref(), Some("2027-00012"));
        assert_eq!(ordered("2026", "2027-01-08").as_deref(), Some("2027-00012"));
        assert_eq!(ordered("2028", "2027-01-08"), None);
        assert_eq!(ordered("2027", ""), None);
        assert_eq!(
            filed(r#"["Release No. 34-106268", "File No. SR-NasdaqTX-2026-"]"#),
            None
        );
        let none = fixture("fr-docket-34-999999.json");
        assert_eq!(
            docket_document(&none, &at("nasdaqtx", "2026", "34-999999")).unwrap(),
            None
        );
        assert_eq!(
            govinfo_copy("https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291"),
            Some(("2026-18291".into(), "2026-09-09".into()))
        );
    }

    #[test]
    fn every_address_form() {
        for uri in [
            "https://www.federalregister.gov/documents/2026/09/09/2026-18291/self-regulatory-organizations-nasdaq-texas-llc-notice-of-filing-and-order-granting-accelerated/",
            "https://www.federalregister.gov/documents/2026/09/09/2026-18291/another-slug",
            "https://www.federalregister.gov/documents/2026/09/09/2026-18291",
            "https://federalregister.gov/d/2026-18291",
            "https://www.federalregister.gov/documents/2026-18291",
            "https://www.federalregister.gov/api/v1/documents/2026-18291",
            "https://www.federalregister.gov/api/v1/documents/2026-18291.json",
            "http://www.federalregister.gov/d/2026-18291?utm_source=x",
        ] {
            assert_eq!(from_uri(uri).as_deref(), Some("2026-18291"), "{uri}");
            assert_eq!(
                FederalRegister.alias_root(uri).as_deref(),
                Some("fr:2026-18291"),
                "{uri}"
            );
        }
        assert_eq!(
            from_uri("https://www.federalregister.gov/d/e9-12345").as_deref(),
            Some("E9-12345")
        );
        for uri in [
            "https://www.federalregister.gov/documents/search?conditions=x",
            "https://www.federalregister.gov/documents/2026/09/2026-18291",
            "https://www.federalregister.gov/agencies/securities-and-exchange-commission",
            "https://www.federalregister.gov/d/2026_18291",
            "https://example.gov/d/2026-18291",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
            assert!(!FederalRegister.reads(uri), "{uri}");
        }
        assert_eq!(FederalRegister.alias_root("fr:2026-18291"), None);
        assert_eq!(
            FederalRegister.alias_root("fr:e9-12345").as_deref(),
            Some("fr:E9-12345")
        );
        assert!(FederalRegister.invalid("fr:not a number"));
        assert!(!FederalRegister.invalid("fr:2026-18291"));
    }

    #[test]
    fn govinfo_copies() {
        for uri in [
            "https://www.govinfo.gov/content/pkg/FR-2026-09-09/pdf/2026-18291.pdf",
            "https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291",
        ] {
            assert_eq!(
                from_govinfo_uri(uri).as_deref(),
                Some("2026-18291"),
                "{uri}"
            );
        }
        for uri in [
            "https://www.govinfo.gov/content/pkg/FR-2026-09-09/pdf/FR-2026-09-09.pdf",
            "https://www.govinfo.gov/app/details/FR-2026-09-09",
            "https://www.govinfo.gov/app/details/CFR-2026-title14/2026-18291",
        ] {
            assert_eq!(from_govinfo_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn notice() {
        assert_eq!(
            api_url("2026-18291").as_deref(),
            Some("https://www.federalregister.gov/api/v1/documents/2026-18291.json")
        );
        assert!(api_url("../x").is_none());
        let document = parse(200, &fixture("fr-2026-18291.json"), "2026-18291")
            .unwrap()
            .unwrap();
        assert_eq!(document.description, None, "no abstract and no action");
        assert!(document.cfr_references.is_empty());
        let related: Vec<_> = document
            .related
            .iter()
            .map(|l| (l.site, l.uri.as_str(), l.kind))
            .collect();
        assert_eq!(
            related,
            [
                (
                    "GovInfo",
                    "https://www.govinfo.gov/content/pkg/FR-2026-09-09/pdf/2026-18291.pdf",
                    Some("pdf")
                ),
                (
                    "GovInfo",
                    "https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291",
                    None
                ),
                (
                    "Regulations.gov",
                    "https://www.regulations.gov/document/SEC-2026-5584-0001",
                    None
                ),
                (
                    "Federal Register public inspection",
                    "https://public-inspection.federalregister.gov/2026-18291.pdf",
                    Some("pdf")
                ),
            ]
        );
        for link in &document.related[..2] {
            assert_eq!(from_govinfo_uri(&link.uri).as_deref(), Some("2026-18291"));
        }
        // GovInfo's own record names the same details page.
        let mods = String::from_utf8(fixture("govinfo-mods-2026-18291.xml")).unwrap();
        assert!(mods.contains(&format!(
            "<url displayLabel=\"Content Detail\" access=\"object in context\">{}</url>",
            document.related[1].uri
        )));

        let doc = card_of("fr-2026-18291.json", "2026-18291");
        assert_eq!(doc.type_name, "Legislation");
        assert_eq!(doc.site, "Federal Register");
        assert!(doc
            .name
            .as_deref()
            .unwrap()
            .starts_with("Self-Regulatory Organizations; Nasdaq Texas, LLC;"));
        let a = &doc.attributes;
        assert_eq!(a["document_type"], "Notice");
        assert_eq!(a["agencies"], "Securities and Exchange Commission");
        assert_eq!(a["citation"], "91 FR 57392");
        assert_eq!(a["publication_date"], "2026-09-09");
        assert_eq!(
            a["docket_ids"],
            "Release No. 34-106268; File No. SR-NasdaqTX-2026-039"
        );
        assert_eq!(a["page_count"], "8");
        assert!(a["page_url"].starts_with(
            "https://www.federalregister.gov/documents/2026/09/09/2026-18291/self-regulatory"
        ));
        // The page the API names and the root's own page are both addresses of the root.
        use crate::sources::Source;
        let page = super::FederalRegister.root_page("fr:2026-18291").unwrap();
        assert_eq!(page, "https://www.federalregister.gov/d/2026-18291");
        for p in [&a["page_url"], &page] {
            assert_eq!(
                super::FederalRegister.alias_root(p).as_deref(),
                Some("fr:2026-18291"),
                "{p}"
            );
        }
        let doc = doc.bounded(crate::sources::attributes()).unwrap();
        assert_eq!(doc.attributes.len(), 7);
    }

    #[test]
    fn rule() {
        let doc = card_of("fr-2026-20818.json", "2026-20818");
        let a = &doc.attributes;
        assert_eq!(a["document_type"], "Rule");
        assert_eq!(
            a["agencies"],
            "Transportation Department; Federal Aviation Administration"
        );
        assert_eq!(a["comments_close_on"], "2026-11-27");
        assert_eq!(a["effective_on"], "2026-10-14");
        assert_eq!(a["cfr_references"], "14 CFR 39");
        assert_eq!(a["regulation_id_numbers"], "2120-AA64");
        assert_eq!(a["citation"], "91 FR 64753");
        assert!(doc
            .description
            .unwrap()
            .starts_with("The FAA is adopting a new airworthiness directive"));
    }

    #[test]
    fn missing_and_mismatched() {
        assert_eq!(
            parse(404, &fixture("fr-missing.html"), "2026-99999").unwrap(),
            None
        );
        assert!(parse(200, &fixture("fr-2026-18291.json"), "2026-20818").is_err());
        assert!(parse(403, b"", "2026-18291").is_err());
    }
}
