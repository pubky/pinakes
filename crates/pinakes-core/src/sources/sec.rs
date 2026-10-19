//! SEC EDGAR filings and companies. A filing's card comes from its header (`{accession}.hdr.sgml`,
//! about 1 KB), never from its documents (often several MB); a company's from
//! `data.sec.gov/submissions`. EDGAR asks for at most ten requests per second and a User-Agent
//! naming the operator and an email address (`http.contact`).
use super::{list, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;
use url::Url;

/// What an EDGAR link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A filing, by the CIK in its address and its accession number (`0001104659-26-000959`).
    Filing {
        cik: u64,
        accession: String,
    },
    /// One document of a filing (the main form or an exhibit).
    Document {
        cik: u64,
        accession: String,
        file: String,
    },
    Company {
        cik: u64,
    },
    /// A self-regulatory organization's rule-change release as the SEC publishes it
    /// (`/files/rules/sro/{org}/{year}/{release}[-{exhibit}].pdf`), e.g. `34-106268`.
    Release {
        release: String,
        file: String,
    },
}

fn cik(value: &str) -> Option<u64> {
    let digits = value.strip_prefix("CIK").unwrap_or(value);
    (!digits.is_empty() && digits.len() <= 10 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
        .filter(|n| *n > 0)
}
/// `0001104659-26-000959` from itself or from its 18-digit folder form.
fn accession(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(|c| *c != '-').collect();
    let dashed = value.contains('-');
    let ok = digits.len() == 18
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (!dashed || value.len() == 20 && &value[10..11] == "-" && &value[13..14] == "-");
    ok.then(|| format!("{}-{}-{}", &digits[..10], &digits[10..12], &digits[12..]))
}
fn folder(accession: &str) -> String {
    accession.replace('-', "")
}
/// A file in a filing folder that is the filing itself: its index pages, header or full text.
fn filing_file(file: &str, accession: &str) -> bool {
    [
        format!("{accession}-index.htm"),
        format!("{accession}-index.html"),
        format!("{accession}-index-headers.html"),
        format!("{accession}.txt"),
        format!("{accession}.hdr.sgml"),
    ]
    .iter()
    .any(|f| f == file)
}
fn document_name(file: &str) -> bool {
    !file.is_empty()
        && file.len() <= 200
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !file.starts_with('.')
}

/// The target of an `/Archives/edgar/data/…` path.
/// An SEC release number, e.g. `34-106268`.
fn release_number(value: &str) -> Option<&str> {
    let (act, n) = value.split_once('-')?;
    (act.len() == 2
        && act.bytes().all(|b| b.is_ascii_digit())
        && (4..=7).contains(&n.len())
        && n.bytes().all(|b| b.is_ascii_digit()))
    .then_some(value)
}
/// `/files/rules/sro/{org}/{year}/{release}.pdf` or `{release}-{exhibit}.pdf`.
fn release_path(path: &str) -> Option<Target> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let ["files", "rules", "sro", org, year, file] = segments.as_slice() else {
        return None;
    };
    let ok_org = !org.is_empty() && org.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let ok_year = year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit());
    let stem = file.strip_suffix(".pdf")?;
    let release = match stem.match_indices('-').nth(1) {
        Some((at, _)) => {
            let exhibit = &stem[at + 1..];
            let ok = !exhibit.is_empty() && exhibit.bytes().all(|b| b.is_ascii_alphanumeric());
            ok.then(|| &stem[..at])?
        }
        None => stem,
    };
    (ok_org && ok_year).then_some(())?;
    Some(Target::Release {
        release: release_number(release)?.to_owned(),
        file: (*file).to_owned(),
    })
}

fn archive_path(path: &str) -> Option<Target> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        ["Archives", "edgar", "data", c, a] | ["Archives", "edgar", "data", c, a, ""] => {
            let cik = cik(c)?;
            // `{cik}/{accession}.txt` and `{cik}/{accession}-index.htm` sit beside the folder.
            if let Some(acc) = accession(a) {
                return (a.len() == 18).then_some(Target::Filing {
                    cik,
                    accession: acc,
                });
            }
            let stem = a
                .strip_suffix(".txt")
                .or_else(|| a.strip_suffix("-index.htm"))
                .or_else(|| a.strip_suffix("-index.html"))?;
            let acc = accession(stem).filter(|_| stem.len() == 20)?;
            Some(Target::Filing {
                cik,
                accession: acc,
            })
        }
        ["Archives", "edgar", "data", c, a, file] => {
            let (cik, acc) = (cik(c)?, accession(a).filter(|_| a.len() == 18)?);
            if filing_file(file, &acc) {
                Some(Target::Filing {
                    cik,
                    accession: acc,
                })
            } else if document_name(file) {
                Some(Target::Document {
                    cik,
                    accession: acc,
                    file: (*file).to_owned(),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

impl Target {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = Url::parse(uri).ok()?;
        if !matches!(url.scheme(), "https" | "http") {
            return None;
        }
        let query = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.into_owned())
        };
        match url.host_str()? {
            "www.sec.gov" | "sec.gov" => match url.path() {
                "/ix" | "/ix.xhtml" | "/cgi-bin/viewer" => archive_path(&query("doc")?),
                "/cgi-bin/browse-edgar" | "/edgar/browse/" | "/edgar/browse" => {
                    Some(Self::Company {
                        cik: cik(&query("CIK")?)?,
                    })
                }
                path => archive_path(path).or_else(|| release_path(path)),
            },
            "data.sec.gov" => {
                let file = url.path().strip_prefix("/submissions/")?;
                let digits = file.strip_prefix("CIK")?.strip_suffix(".json")?;
                (digits.len() == 10).then_some(Self::Company { cik: cik(digits)? })
            }
            _ => None,
        }
    }

    /// The one address every spelling of this target is an alias of.
    pub fn root(&self) -> String {
        match self {
            Self::Filing { cik, accession } => format!(
                "https://www.sec.gov/Archives/edgar/data/{cik}/{}/",
                folder(accession)
            ),
            Self::Document {
                cik,
                accession,
                file,
            } => format!(
                "https://www.sec.gov/Archives/edgar/data/{cik}/{}/{file}",
                folder(accession)
            ),
            Self::Company { cik } => {
                format!("https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK={cik:010}")
            }
            // A release is its own address.
            Self::Release { .. } => String::new(),
        }
    }
}

pub fn alias_root(uri: &str) -> Option<String> {
    let target = Target::of_uri(uri)?;
    if matches!(target, Target::Release { .. }) {
        return None;
    }
    let root = target.root();
    let (_, normalized, _) = crate::model::identity(uri).ok()?;
    (root != normalized).then_some(root)
}

/// A filing's header (`.hdr.sgml`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub accession: String,
    pub form: String,
    pub filing_date: Option<String>,
    pub period: Option<String>,
    pub items: Vec<String>,
    /// The filers (or subject company) in order: (CIK, conformed name, file number).
    pub filers: Vec<(u64, String, Option<String>)>,
}

/// `20260106` as `2026-01-06`.
fn date(value: &str) -> Option<String> {
    let d = chrono::NaiveDate::parse_from_str(value.trim(), "%Y%m%d").ok()?;
    Some(d.format("%Y-%m-%d").to_string())
}

/// Read an EDGAR SGML header: one `<TAG>value` per line, sections opened by `<TAG>` alone.
pub fn parse_header(body: &str) -> Result<Header> {
    let mut header = Header::default();
    let mut current: Option<(Option<u64>, Option<String>, Option<String>)> = None;
    for line in body.lines() {
        let Some(rest) = line.trim().strip_prefix('<') else {
            continue;
        };
        let Some((tag, value)) = rest.split_once('>') else {
            continue;
        };
        let value = value.trim();
        match tag {
            "ACCESSION-NUMBER" => header.accession = accession(value).unwrap_or_default(),
            "TYPE" if header.form.is_empty() => header.form = value.to_owned(),
            "FILING-DATE" => header.filing_date = date(value),
            "PERIOD" => header.period = date(value),
            "ITEMS" if !value.is_empty() => header.items.push(value.to_owned()),
            "FILER" | "SUBJECT-COMPANY" | "FILED-BY" if value.is_empty() => {
                current = Some((None, None, None))
            }
            "/FILER" | "/SUBJECT-COMPANY" | "/FILED-BY" => {
                if let Some((Some(cik), Some(name), file)) = current.take() {
                    if !header.filers.iter().any(|f| f.0 == cik) {
                        header.filers.push((cik, name, file));
                    }
                }
            }
            "CONFORMED-NAME" => {
                if let Some(c) = current.as_mut().filter(|c| c.1.is_none()) {
                    c.1 = Some(value.to_owned());
                }
            }
            "CIK" => {
                if let Some(c) = current.as_mut().filter(|c| c.0.is_none()) {
                    c.0 = cik(value);
                }
            }
            "FILE-NUMBER" => {
                if let Some(c) = current.as_mut().filter(|c| c.2.is_none()) {
                    c.2 = Some(value.to_owned()).filter(|v| !v.is_empty());
                }
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        !header.accession.is_empty() && !header.form.is_empty() && !header.filers.is_empty(),
        "incomplete EDGAR header"
    );
    Ok(header)
}

/// One row of a filing's index: (document file, type, description).
pub fn index_row(html: &str, file: &str) -> Option<(String, String)> {
    let page = scraper::Html::parse_document(html);
    let rows = scraper::Selector::parse("table.tableFile tr").ok()?;
    let cells = scraper::Selector::parse("td").ok()?;
    for row in page.select(&rows) {
        let texts: Vec<String> = row
            .select(&cells)
            .map(|c| c.text().collect::<String>().trim().to_owned())
            .collect();
        let [_, description, document, kind, ..] = texts.as_slice() else {
            continue;
        };
        if document.split_whitespace().next() == Some(file) {
            return Some((kind.clone(), description.clone()));
        }
    }
    None
}

fn filing_url(cik: u64, accession: &str, file: &str) -> String {
    format!(
        "https://www.sec.gov/Archives/edgar/data/{cik}/{}/{file}",
        folder(accession)
    )
}

/// The card of a filing (or of one of its documents) from its header.
pub fn filing_card(
    mut doc: ResourceMeta,
    cik: u64,
    header: &Header,
    document: Option<(&str, Option<(String, String)>)>,
) -> ResourceMeta {
    doc.set_type("Report");
    let (primary_cik, name, file_number) = &header.filers[0];
    doc.name = Some(format!("Form {} · {name}", header.form));
    let mut about = vec![];
    if let Some(d) = &header.filing_date {
        about.push(format!("Filed {d}"));
    }
    if let Some(p) = &header.period {
        about.push(format!("period {p}"));
    }
    if !header.items.is_empty() {
        about.push(format!("items {}", header.items.join(", ")));
    }
    doc.description = (!about.is_empty()).then(|| about.join(" · "));
    let filers: Vec<String> = header.filers.iter().map(|f| f.1.clone()).collect();
    for (key, value) in [
        ("form", Some(header.form.clone())),
        ("accession", Some(header.accession.clone())),
        ("filing_date", header.filing_date.clone()),
        ("period", header.period.clone()),
        ("items", list(&header.items)),
        ("company", list(&filers)),
        ("cik", Some(primary_cik.to_string())),
        ("file_number", file_number.clone()),
        (
            "page_url",
            Some(filing_url(
                cik,
                &header.accession,
                &format!("{}-index.htm", header.accession),
            )),
        ),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    match document {
        Some((file, row)) => {
            doc.attributes.insert("document".into(), file.into());
            if let Some((kind, description)) = row {
                if !kind.is_empty() {
                    doc.attributes.insert("exhibit".into(), kind);
                }
                if !description.is_empty() {
                    doc.attributes
                        .insert("exhibit_description".into(), description);
                }
            }
        }
        // A co-registrant's spelling of the filing is an alias of the primary filer's.
        None => {
            doc.source_root = Some(
                Target::Filing {
                    cik: *primary_cik,
                    accession: header.accession.clone(),
                }
                .root(),
            )
        }
    }
    doc
}

/// A company's card from `data.sec.gov/submissions/CIK{10}.json`.
pub fn company_card(mut doc: ResourceMeta, cik: u64, body: &[u8]) -> Result<ResourceMeta> {
    let v: Value = serde_json::from_slice(body)?;
    let got = v["cik"]
        .as_str()
        .and_then(cik_of)
        .or_else(|| v["cik"].as_u64());
    anyhow::ensure!(got == Some(cik), "EDGAR answered for another company");
    let text = |key: &str| {
        v[key]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    doc.set_type("Corporation");
    doc.name = text("name");
    let tickers: Vec<String> = v["tickers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect();
    doc.description = text("sicDescription");
    for (key, value) in [
        ("cik", Some(cik.to_string())),
        ("tickers", list(&tickers)),
        ("state_of_incorporation", text("stateOfIncorporation")),
        ("lei", text("lei")),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(doc)
}
fn cik_of(value: &str) -> Option<u64> {
    cik(value)
}

/// The Federal Register's documents filed under a docket, by release number.
pub fn docket_url(release: &str) -> String {
    format!(
        "https://www.federalregister.gov/api/v1/documents.json?conditions%5Bdocket_id%5D={release}\
         &fields%5B%5D=document_number&fields%5B%5D=docket_ids&per_page=20"
    )
}
/// The one Federal Register document whose docket ids name `Release No. {release}` exactly.
/// The search matches loosely, so every result is checked; a release number is unique.
pub fn docket_document(body: &[u8], release: &str) -> Result<Option<String>> {
    let v: Value = serde_json::from_slice(body)?;
    let wanted = format!("release no. {release}");
    let named = |d: &Value| {
        d["docket_ids"].as_array().is_some_and(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .any(|id| id.trim().eq_ignore_ascii_case(&wanted))
        })
    };
    let mut numbers: Vec<String> = v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| named(d))
        .filter_map(|d| super::federal_register::document_number(d["document_number"].as_str()?))
        .collect();
    numbers.sort();
    numbers.dedup();
    Ok(match numbers.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    })
}

/// The card of an SRO release, from the Federal Register document that publishes it.
pub fn release_card(
    mut doc: ResourceMeta,
    release: &str,
    file: &str,
    document: Option<super::federal_register::Document>,
) -> ResourceMeta {
    let number = document.as_ref().map(|d| d.number.clone());
    match document {
        Some(document) => doc = super::federal_register::card(doc, document),
        None => {
            doc.set_type("Report");
            doc.name = Some(format!("Release No. {release}"));
        }
    }
    doc.site = "www.sec.gov".into();
    doc.attributes.insert("release".into(), release.into());
    doc.attributes.insert("document".into(), file.into());
    if let Some(number) = number {
        doc.attributes.insert("federal_register".into(), number);
    }
    doc
}

pub struct Sec;

impl Sec {
    async fn get(
        &self,
        cx: &Context<'_>,
        url: &str,
        accept: &str,
    ) -> Result<crate::resolver::egress::Fetched> {
        cx.fetch(
            url,
            &FetchOptions {
                accept: Some(accept.into()),
                extra_types: &["text/sgml", "text/plain"],
                ..Default::default()
            },
        )
        .await
    }
}

#[async_trait]
impl Source for Sec {
    fn name(&self) -> &'static str {
        "sec"
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        alias_root(uri)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "form",
            "accession",
            "filing_date",
            "period",
            "items",
            "company",
            "cik",
            "file_number",
            "page_url",
            "document",
            "exhibit",
            "exhibit_description",
            "tickers",
            "state_of_incorporation",
            "lei",
            "release",
            "federal_register",
            // A release's card carries its Federal Register document's facts.
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
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959/0001104659-26-000959-index.htm",
                card_type: "Report",
            },
            Example {
                uri: "https://www.sec.gov/Archives/edgar/data/2070457/000121390026079805/ea029862901ex99-1.htm",
                card_type: "Report",
            },
            Example {
                uri: "https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK=0002070457",
                card_type: "Corporation",
            },
            Example {
                uri: "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268.pdf",
                card_type: "Legislation",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        // EDGAR refuses requests whose User-Agent has no email address: wait for one.
        if cx.contact().is_none() {
            return Err(crate::resolver::RetryLater {
                class: "disabled",
                seconds: 3600,
            }
            .into());
        }
        let target = Target::of_uri(uri).context("not an EDGAR link")?;
        if let Target::Release { release, file } = &target {
            let json = FetchOptions {
                accept: Some("application/json".into()),
                ..Default::default()
            };
            let f = cx.fetch(&docket_url(release), &json).await?;
            anyhow::ensure!(f.status == 200, "Federal Register answered {}", f.status);
            let document = match docket_document(&f.body, release)? {
                Some(number) => {
                    let url = super::federal_register::api_url(&number).context("number")?;
                    let f = cx.fetch(&url, &json).await?;
                    super::federal_register::parse(f.status, &f.body, &number)?
                }
                None => None,
            };
            return Ok(release_card(cx.card(uri, self), release, file, document));
        }
        if let Target::Company { cik } = target {
            let url = format!("https://data.sec.gov/submissions/CIK{cik:010}.json");
            let f = self.get(cx, &url, "application/json").await?;
            return match f.status {
                200 => company_card(cx.card(uri, self), cik, &f.body),
                404 => Ok(cx.missing(uri, self)),
                s => anyhow::bail!("EDGAR answered {s}"),
            };
        }
        let (Target::Filing { cik, accession } | Target::Document { cik, accession, .. }) = &target
        else {
            unreachable!("companies and releases are answered above")
        };
        let header_url = filing_url(*cik, accession, &format!("{accession}.hdr.sgml"));
        let f = self
            .get(cx, &header_url, "text/sgml, text/plain;q=0.9, */*;q=0.1")
            .await?;
        match f.status {
            200 => {}
            404 => return Ok(cx.missing(uri, self)),
            s => anyhow::bail!("EDGAR answered {s}"),
        }
        let header = parse_header(&String::from_utf8_lossy(&f.body))?;
        anyhow::ensure!(
            &header.accession == accession,
            "EDGAR answered for another filing"
        );
        let document = match &target {
            Target::Document { file, .. } => {
                let index_url = filing_url(*cik, accession, &format!("{accession}-index.htm"));
                let index = self
                    .get(cx, &index_url, "text/html")
                    .await
                    .ok()
                    .filter(|f| f.status == 200);
                let row = index.and_then(|f| index_row(&String::from_utf8_lossy(&f.body), file));
                Some((file.as_str(), row))
            }
            _ => None,
        };
        Ok(filing_card(cx.card(uri, self), *cik, &header, document))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(name: &str) -> Vec<u8> {
        crate::sources::fixture::read("sec", name)
    }
    fn header(name: &str) -> Header {
        parse_header(&String::from_utf8(read(name)).unwrap()).unwrap()
    }
    fn base(uri: &str) -> ResourceMeta {
        ResourceMeta::new(uri, "sec", 3600)
    }

    #[test]
    fn every_spelling_of_a_filing() {
        let root = "https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959/";
        for uri in [
            "https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959",
            "https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959/0001104659-26-000959-index.htm",
            "https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959/0001104659-26-000959-index.html",
            "https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959/0001104659-26-000959.txt",
            "https://www.sec.gov/Archives/edgar/data/0002103612/0001104659-26-000959.txt",
            "https://www.sec.gov/Archives/edgar/data/2103612/0001104659-26-000959-index.htm",
            "https://sec.gov/Archives/edgar/data/2103612/000110465926000959/",
        ] {
            assert_eq!(alias_root(uri).as_deref(), Some(root), "{uri}");
        }
        assert_eq!(alias_root(root), None);
        let document = "https://www.sec.gov/Archives/edgar/data/2070457/000121390026079805/ea029862901ex99-1.htm";
        assert_eq!(alias_root(document), None);
        assert_eq!(
            alias_root("https://www.sec.gov/ix?doc=/Archives/edgar/data/2070457/000121390026079805/ea029862901ex99-1.htm")
                .as_deref(),
            Some(document)
        );
        assert_eq!(
            Target::of_uri(document),
            Some(Target::Document {
                cik: 2070457,
                accession: "0001213900-26-079805".into(),
                file: "ea029862901ex99-1.htm".into()
            })
        );
        for uri in [
            "https://www.sec.gov/Archives/edgar/data/2103612/",
            "https://www.sec.gov/Archives/edgar/data/x/000110465926000959/",
            "https://www.sec.gov/Archives/edgar/data/2103612/00011046592600095/",
            "https://www.sec.gov/newsroom/press-releases/2026-105",
            "https://example.org/Archives/edgar/data/2103612/000110465926000959/",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn every_spelling_of_a_company() {
        let root = "https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK=0002070457";
        for uri in [
            "https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK=2070457&type=8-K",
            "https://www.sec.gov/edgar/browse/?CIK=2070457",
            "https://data.sec.gov/submissions/CIK0002070457.json",
        ] {
            assert_eq!(alias_root(uri).as_deref(), Some(root), "{uri}");
        }
        assert_eq!(alias_root(root), None);
        assert_eq!(
            Target::of_uri("https://www.sec.gov/edgar/browse/?CIK=XXI"),
            None
        );
    }

    #[test]
    fn headers() {
        let h = header("sec-hdr-ms-s1.sgml");
        assert_eq!(h.accession, "0001104659-26-000959");
        assert_eq!(h.form, "S-1");
        assert_eq!(h.filing_date.as_deref(), Some("2026-01-06"));
        assert_eq!(
            h.filers,
            [(
                2103612,
                "Morgan Stanley Bitcoin Trust".into(),
                Some("333-292586".into())
            )]
        );
        let h = header("sec-hdr-xxi-8k.sgml");
        assert_eq!(
            (h.form.as_str(), h.period.as_deref()),
            ("8-K", Some("2026-07-15"))
        );
        assert_eq!(h.items, ["5.02", "8.01", "9.01"]);
        assert_eq!(
            header("sec-hdr-franklin-10q.sgml").period.as_deref(),
            Some("2026-06-30")
        );
        assert_eq!(header("sec-hdr-ishares-s1.sgml").form, "S-1");
        assert!(parse_header("<SEC-HEADER>\n</SEC-HEADER>").is_err());
    }

    #[test]
    fn co_registrants_alias_the_primary_filer() {
        let body = "<ACCESSION-NUMBER>0000000001-26-000001\n<TYPE>S-3\n<FILING-DATE>20260101\n\
                    <FILER>\n<COMPANY-DATA>\n<CONFORMED-NAME>Parent Inc\n<CIK>0000000010\n\
                    </COMPANY-DATA>\n</FILER>\n<FILER>\n<COMPANY-DATA>\n<CONFORMED-NAME>Sub LLC\n\
                    <CIK>0000000020\n</COMPANY-DATA>\n</FILER>\n";
        let h = parse_header(body).unwrap();
        let uri = "https://www.sec.gov/Archives/edgar/data/20/000000000126000001/";
        let doc = filing_card(base(uri), 20, &h, None);
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://www.sec.gov/Archives/edgar/data/10/000000000126000001/")
        );
        assert_eq!(doc.attributes["company"], "Parent Inc; Sub LLC");
        assert_eq!(doc.attributes["cik"], "10");
    }

    #[test]
    fn filing_and_exhibit_cards() {
        let h = header("sec-hdr-xxi-8k.sgml");
        let uri = "https://www.sec.gov/Archives/edgar/data/2070457/000121390026079805/";
        let doc = filing_card(base(uri), 2070457, &h, None);
        assert_eq!(doc.type_name, "Report");
        assert_eq!(
            doc.name.as_deref(),
            Some("Form 8-K · Twenty One Capital, Inc.")
        );
        assert_eq!(
            doc.description.as_deref(),
            Some("Filed 2026-07-21 · period 2026-07-15 · items 5.02, 8.01, 9.01")
        );
        assert_eq!(doc.attributes["items"], "5.02; 8.01; 9.01");
        assert_eq!(
            doc.attributes["page_url"],
            "https://www.sec.gov/Archives/edgar/data/2070457/000121390026079805/0001213900-26-079805-index.htm"
        );
        assert_eq!(doc.source_root.as_deref(), Some(uri));

        let index = String::from_utf8(read("sec-index-xxi-8k.html")).unwrap();
        let row = index_row(&index, "ea029862901ex99-1.htm");
        assert_eq!(
            row,
            Some((
                "EX-99.1".into(),
                "PRESS RELEASE ISSUED BY TWENTY ONE CAPITAL, INC. ON JULY 21, 2026".into()
            ))
        );
        assert_eq!(
            index_row(&index, "ea0298629-8k_twenty.htm").unwrap().0,
            "8-K"
        );
        assert_eq!(index_row(&index, "missing.htm"), None);
        let exhibit = format!("{uri}ea029862901ex99-1.htm");
        let doc = filing_card(
            base(&exhibit),
            2070457,
            &h,
            Some(("ea029862901ex99-1.htm", row)),
        );
        assert_eq!(doc.attributes["exhibit"], "EX-99.1");
        assert_eq!(doc.attributes["document"], "ea029862901ex99-1.htm");
        assert_eq!(doc.source_root, None);
    }

    #[test]
    fn sro_releases_join_their_federal_register_document() {
        let uri = "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268.pdf";
        assert_eq!(
            Target::of_uri(uri),
            Some(Target::Release {
                release: "34-106268".into(),
                file: "34-106268.pdf".into()
            })
        );
        let exhibit = "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268-ex5.pdf";
        assert!(
            matches!(Target::of_uri(exhibit), Some(Target::Release { release, .. }) if release == "34-106268")
        );
        assert_eq!(alias_root(uri), None);
        for bad in [
            "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/notice.pdf",
            "https://www.sec.gov/files/rules/sro/nasdaqtx/26/34-106268.pdf",
            "https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268-.pdf",
        ] {
            assert_eq!(Target::of_uri(bad), None, "{bad}");
        }
        let found = read("fr-docket-34-106268.json");
        assert_eq!(
            docket_document(&found, "34-106268").unwrap().as_deref(),
            Some("2026-18291")
        );
        // The search matches loosely; only an exact `Release No.` docket id counts.
        assert_eq!(docket_document(&found, "34-10626").unwrap(), None);
        let none = read("fr-docket-34-999999.json");
        assert_eq!(docket_document(&none, "34-999999").unwrap(), None);
        let fr = crate::sources::fixture::read("federal_register", "fr-2026-18291.json");
        let document = super::super::federal_register::parse(200, &fr, "2026-18291").unwrap();
        let doc = release_card(base(uri), "34-106268", "34-106268.pdf", document);
        assert_eq!(doc.type_name, "Legislation");
        assert_eq!(doc.site, "www.sec.gov");
        assert_eq!(doc.attributes["federal_register"], "2026-18291");
        assert_eq!(doc.attributes["citation"], "91 FR 57392");
        assert_eq!(doc.attributes["release"], "34-106268");
        let doc = release_card(base(uri), "34-106268", "34-106268.pdf", None);
        assert_eq!(doc.type_name, "Report");
        assert_eq!(doc.name.as_deref(), Some("Release No. 34-106268"));
    }

    #[tokio::test]
    async fn without_a_contact_edgar_links_wait() {
        let egress = crate::resolver::egress::Egress::new(Default::default());
        assert_eq!(egress.contact(), None);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let error = Sec
            .resolve(
                &cx,
                "https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK=0002070457",
            )
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
        assert_eq!(retry.class, "disabled");
    }

    #[test]
    fn company() {
        let raw = read("sec-submissions-2070457.json");
        let uri = "https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK=0002070457";
        let doc = company_card(base(uri), 2070457, &raw).unwrap();
        assert_eq!(doc.type_name, "Corporation");
        assert_eq!(doc.name.as_deref(), Some("Twenty One Capital, Inc."));
        assert_eq!(doc.attributes["tickers"], "XXI");
        assert_eq!(doc.attributes["cik"], "2070457");
        assert!(company_card(base(uri), 1, &raw).is_err());
    }
}
