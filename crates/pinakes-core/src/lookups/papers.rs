//! Papers (DOI cards): open copies from Unpaywall, PubMed and PubMed Central from the NCBI ID
//! Converter, the arXiv ↔ journal version pair from DataCite, alphaXiv, and Hugging Face Papers.
use super::{body, encode, encode_path, plain, web_url, Found, Key, Link, Request};
use crate::resolver::cards::doi;
use anyhow::{Context, Result};
use serde_json::Value;

/// The contact for APIs that ask for one (`email`, `mailto`), or `None` when it is not set.
fn contact(contact: &str) -> Option<&str> {
    let contact = contact.trim();
    (!contact.is_empty() && contact.contains('@') && !contact.contains(char::is_whitespace))
        .then_some(contact)
}

/// The operator's contact, for APIs that ask for an email address.
pub(crate) fn operator_contact(cx: &crate::also_on::Context) -> Option<&str> {
    contact(&cx.config.http.contact)
}

/// The most ids one NCBI ID Converter request may carry.
pub const NCBI_MAX_IDS: usize = 200;
/// The `tool` parameter NCBI asks for.
const NCBI_TOOL: &str = "pinakes";
/// The DataCite prefix of arXiv's own DOIs.
const ARXIV_PREFIX: &str = "10.48550/arxiv.";

pub fn doi_key(doi: &str) -> Key {
    Key::new("doi", doi)
}
pub fn arxiv_key(id: &str) -> Key {
    Key::new("arxiv", id)
}

/// An arXiv id without its version: `2101.00001` (new style) or `hep-th/9901001` (old style).
pub fn arxiv_id(value: &str) -> Option<String> {
    let value = value.strip_suffix(".pdf").unwrap_or(value);
    let base = match value.rfind('v') {
        Some(at) if at > 0 && plain(&value[at + 1..], 3, |c| c.is_ascii_digit()) => &value[..at],
        _ => value,
    };
    let new_style = base.split_once('.').is_some_and(|(ym, n)| {
        plain(ym, 4, |c| c.is_ascii_digit())
            && ym.len() == 4
            && (4..=5).contains(&n.len())
            && n.bytes().all(|b| b.is_ascii_digit())
    });
    let old_style = base.split_once('/').is_some_and(|(archive, n)| {
        plain(archive, 20, |c| {
            c.is_ascii_lowercase() || c == '-' || c == '.'
        }) && n.len() == 7
            && n.bytes().all(|b| b.is_ascii_digit())
    });
    (new_style || old_style).then(|| base.to_owned())
}

/// The arXiv id of an arXiv page (`arxiv.org/abs|pdf/{id}`), an alphaXiv page, or arXiv's DOI
/// (`doi.org/10.48550/arXiv.{id}`, `doi:10.48550/arXiv.{id}`).
pub fn arxiv_from_uri(uri: &str) -> Option<String> {
    if let Some(d) = doi(uri) {
        return d.strip_prefix(ARXIV_PREFIX).and_then(arxiv_id);
    }
    let url = web_url(uri)?;
    let host = url.host_str()?;
    let path = url.path().trim_start_matches('/');
    let rest = match host {
        "arxiv.org" | "www.arxiv.org" | "export.arxiv.org" => path
            .strip_prefix("abs/")
            .or_else(|| path.strip_prefix("pdf/")),
        "alphaxiv.org" | "www.alphaxiv.org" => path.strip_prefix("abs/"),
        _ => None,
    }?;
    arxiv_id(rest.trim_end_matches('/'))
}

/// The arXiv page and its alphaXiv page, derived from an exact arXiv id.
pub fn arxiv_links(id: &str, via: &'static str) -> Vec<Link> {
    vec![
        Link::new("arXiv", format!("https://arxiv.org/abs/{id}"), via),
        Link::new(
            "alphaXiv",
            format!("https://www.alphaxiv.org/abs/{id}"),
            via,
        ),
    ]
}

/// Whether a DOI can be a request's path as it is: none of its segments is `.` or `..`, which a
/// URL resolves, so the request would be for another path of the service.
fn path_safe(doi: &str) -> bool {
    !doi.split('/')
        .any(|segment| segment == "." || segment == "..")
}

/// Unpaywall's record for a DOI. Needs the contact setting. No request for a DOI with a `.` or
/// `..` segment ([`path_safe`]).
pub fn unpaywall_request(doi: &str, contact_email: &str) -> Option<Request> {
    let email = contact(contact_email)?;
    if !path_safe(doi) {
        return None;
    }
    Request::get(&format!(
        "https://api.unpaywall.org/v2/{}?email={}",
        encode_path(doi),
        encode(email)
    ))
}

/// A PubMed Central id (`PMC123`) from a number or an id.
fn pmcid(value: &str) -> Option<String> {
    let n = value.strip_prefix("PMC").unwrap_or(value);
    (plain(n, 12, |c| c.is_ascii_digit()) && !n.starts_with('0')).then(|| format!("PMC{n}"))
}
fn pmc_link(pmcid: &str, via: &'static str) -> Link {
    Link::new(
        "PubMed Central",
        format!("https://pmc.ncbi.nlm.nih.gov/articles/{pmcid}/"),
        via,
    )
}

/// The open copies Unpaywall lists for `doi`, other than the publisher's own page.
///
/// Unpaywall finds repository copies with its own matching, so its answers add links and never
/// keys. arXiv and PubMed Central copies get their canonical addresses.
pub fn parse_unpaywall(status: u16, raw: &[u8], doi: &str) -> Result<Vec<Link>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(Vec::new());
    };
    let record: Value = serde_json::from_slice(raw)?;
    let got = record["doi"].as_str().context("no doi")?;
    anyhow::ensure!(
        got.eq_ignore_ascii_case(doi),
        "unpaywall answered for another DOI"
    );
    let mut found = Found::default();
    for location in record["oa_locations"].as_array().into_iter().flatten() {
        if location["host_type"] != "repository" {
            continue;
        }
        let kind = match location["version"].as_str() {
            Some("submittedVersion") => Some("submitted"),
            Some("acceptedVersion") => Some("accepted"),
            Some("publishedVersion") => Some("published"),
            _ => None,
        };
        let Some(uri) = location["url_for_landing_page"]
            .as_str()
            .or(location["url"].as_str())
        else {
            continue;
        };
        let pmh = location["pmh_id"].as_str().unwrap_or_default();
        let mut link = if let Some(id) = arxiv_from_uri(uri) {
            arxiv_links(&id, "unpaywall").remove(0)
        } else if let Some(id) = pmh
            .strip_prefix("oai:pubmedcentral.nih.gov:")
            .and_then(pmcid)
        {
            pmc_link(&id, "unpaywall")
        } else {
            let Some(url) = web_url(uri) else { continue };
            let site = location["repository_institution"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::trim)
                .or(url.host_str())
                .unwrap_or_default()
                .to_owned();
            Link::new(site, url.to_string(), "unpaywall")
        };
        link.kind = kind;
        // The first entry wins, so a versioned location beats an unversioned duplicate.
        found.push_link(link);
    }
    Ok(found.links)
}

/// The NCBI ID Converter for up to `NCBI_MAX_IDS` DOIs. Needs the contact setting. DOIs with a
/// comma (the list separator) are left out.
pub fn ncbi_request(dois: &[&str], contact_email: &str) -> Option<Request> {
    let email = contact(contact_email)?;
    let ids: Vec<String> = dois
        .iter()
        .filter(|d| !d.contains(','))
        .take(NCBI_MAX_IDS)
        .map(|d| encode_path(d))
        .collect();
    if ids.is_empty() {
        return None;
    }
    Request::get(&format!(
        "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?ids={}&format=json\
         &tool={NCBI_TOOL}&email={}",
        ids.join(","),
        encode(email)
    ))
}

/// PubMed and PubMed Central for each DOI the converter knows, keyed by the requested DOI.
pub fn parse_ncbi(status: u16, raw: &[u8]) -> Result<Vec<(String, Found)>> {
    let raw = body(status, raw, &[])?.context("no body")?;
    let response: Value = serde_json::from_slice(raw)?;
    anyhow::ensure!(
        response["status"] == "ok",
        "idconv status {}",
        response["status"]
    );
    let mut out = Vec::new();
    for record in response["records"].as_array().into_iter().flatten() {
        if record.get("status").is_some_and(|s| s == "error") {
            continue;
        }
        let (Some(requested), Some(got)) = (
            record["requested-id"]
                .as_str()
                .and_then(|d| doi(&format!("doi:{d}"))),
            record["doi"]
                .as_str()
                .and_then(|d| doi(&format!("doi:{d}"))),
        ) else {
            continue;
        };
        if requested != got {
            continue;
        }
        let mut found = Found::default();
        let pmid = match &record["pmid"] {
            Value::Number(n) => n.as_u64().map(|n| n.to_string()),
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
        .filter(|p| plain(p, 12, |c| c.is_ascii_digit()) && !p.starts_with('0'));
        if let Some(pmid) = pmid {
            found.push_key(Key::new("pmid", &pmid));
            found.push_link(Link::new(
                "PubMed",
                format!("https://pubmed.ncbi.nlm.nih.gov/{pmid}/"),
                "ncbi",
            ));
        }
        if let Some(pmc) = record["pmcid"].as_str().and_then(pmcid) {
            found.push_key(Key::new("pmcid", &pmc));
            found.push_link(pmc_link(&pmc, "ncbi"));
        }
        if !found.keys.is_empty() {
            out.push((requested, found));
        }
    }
    Ok(out)
}

/// DataCite's record for an arXiv id (arXiv registers `10.48550/arXiv.{id}`).
pub fn datacite_arxiv_request(arxiv: &str) -> Option<Request> {
    let id = arxiv_id(arxiv)?;
    Request::get(&format!(
        "https://api.datacite.org/dois/10.48550/arXiv.{id}"
    ))
}

/// The journal DOIs a record says it is a version of.
fn version_of(record: &Value) -> Vec<String> {
    let mut dois: Vec<String> = record["attributes"]["relatedIdentifiers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["relationType"] == "IsVersionOf" && r["relatedIdentifierType"] == "DOI")
        .filter_map(|r| doi(&format!("doi:{}", r["relatedIdentifier"].as_str()?)))
        .filter(|d| !d.starts_with("10.48550/"))
        .collect();
    dois.sort();
    dois.dedup();
    dois
}

/// The arXiv id as key and, when the record names exactly one journal version, that version as
/// a link and its DOI. The DOI is arXiv's submitter's claim: it becomes a key only when the
/// journal's publisher names the preprint back (`parse_crossref_preprint`).
pub fn parse_datacite_arxiv(
    status: u16,
    raw: &[u8],
    arxiv: &str,
) -> Result<(Found, Option<String>)> {
    let mut found = Found::default();
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok((found, None));
    };
    let id = arxiv_id(arxiv).context("not an arXiv id")?;
    let response: Value = serde_json::from_slice(raw)?;
    let record = &response["data"];
    let expected = format!("{ARXIV_PREFIX}{}", id.to_lowercase());
    anyhow::ensure!(
        record["id"]
            .as_str()
            .is_some_and(|got| got.eq_ignore_ascii_case(&expected)),
        "datacite answered for another DOI"
    );
    found.push_key(arxiv_key(&id));
    let journal = match version_of(record).as_slice() {
        [journal] => Some(journal.clone()),
        _ => None,
    };
    if let Some(journal) = &journal {
        found.push_link(
            Link::new("DOI", format!("https://doi.org/{journal}"), "datacite").kind("published"),
        );
    }
    Ok((found, journal))
}

/// arXiv records that say they are a version of `doi` (the reverse of the above).
pub fn datacite_versions_request(doi: &str) -> Option<Request> {
    if doi.contains(['"', '\\']) {
        return None;
    }
    let query = format!(
        "relatedIdentifiers.relatedIdentifier:\"{doi}\" AND relatedIdentifiers.relationType:IsVersionOf"
    );
    Request::get(&format!(
        "https://api.datacite.org/dois?query={}&client-id=arxiv.content&page%5Bsize%5D=25",
        encode(&query)
    ))
}

/// The arXiv preprint of `doi`, when exactly one arXiv record names it, and nothing else, as its
/// journal version: its arXiv and alphaXiv pages as links, and its id. The id becomes a key only
/// when the journal's publisher names the preprint back (`parse_crossref_preprint`).
pub fn parse_datacite_versions(
    status: u16,
    raw: &[u8],
    doi: &str,
) -> Result<(Found, Option<String>)> {
    let raw = body(status, raw, &[])?.context("no body")?;
    let response: Value = serde_json::from_slice(raw)?;
    let mut ids: Vec<String> = response["data"]
        .as_array()
        .context("no data")?
        .iter()
        .filter(|r| matches!(version_of(r).as_slice(), [only] if only.eq_ignore_ascii_case(doi)))
        .filter_map(|r| {
            let id = r["id"].as_str()?.to_lowercase();
            arxiv_id(id.strip_prefix(ARXIV_PREFIX)?)
        })
        .collect();
    ids.sort();
    ids.dedup();
    let mut found = Found::default();
    let [id] = ids.as_slice() else {
        return Ok((found, None));
    };
    for link in arxiv_links(id, "datacite") {
        found.push_link(link.kind("submitted"));
    }
    Ok((found, Some(id.clone())))
}

/// Crossref's record of a journal DOI, as its publisher deposited it. No request for a DOI with
/// a `.` or `..` segment ([`path_safe`]).
pub fn crossref_request(doi: &str) -> Option<Request> {
    if !path_safe(doi) {
        return None;
    }
    Request::get(&format!(
        "https://api.crossref.org/works/{}",
        encode_path(doi)
    ))
}

/// Whether the publisher's Crossref record of `doi` names arXiv paper `arxiv` as its preprint: a
/// `has-preprint` relation the publisher asserted itself (`asserted-by: subject`), by arXiv id (any
/// version) or arXiv DOI (`10.48550/arXiv.{id}`). A DOI Crossref does not know (404) names none.
pub fn parse_crossref_preprint(status: u16, raw: &[u8], doi: &str, arxiv: &str) -> Result<bool> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(false);
    };
    let id = arxiv_id(arxiv).context("not an arXiv id")?;
    let response: Value = serde_json::from_slice(raw)?;
    let record = &response["message"];
    anyhow::ensure!(
        record["DOI"]
            .as_str()
            .is_some_and(|got| got.eq_ignore_ascii_case(doi)),
        "crossref answered for another DOI"
    );
    let arxiv_doi = format!("{ARXIV_PREFIX}{id}");
    Ok(record["relation"]["has-preprint"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["asserted-by"] == "subject")
        .any(|r| {
            let named = r["id"].as_str().unwrap_or_default();
            match r["id-type"].as_str() {
                Some("arxiv") => arxiv_id(named).is_some_and(|n| n == id),
                Some("doi") => named.eq_ignore_ascii_case(&arxiv_doi),
                _ => false,
            }
        }))
}

/// Whether Hugging Face has a page for an arXiv paper.
pub fn hugging_face_request(arxiv: &str) -> Option<Request> {
    Request::get(&format!(
        "https://huggingface.co/api/papers/{}",
        arxiv_id(arxiv)?
    ))
}

/// The Hugging Face Papers page, only when the API says it exists.
pub fn parse_hugging_face(status: u16, raw: &[u8], arxiv: &str) -> Result<Option<Link>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let id = arxiv_id(arxiv).context("not an arXiv id")?;
    let paper: Value = serde_json::from_slice(raw)?;
    Ok((paper["id"] == id.as_str()).then(|| {
        Link::new(
            "Hugging Face Papers",
            format!("https://huggingface.co/papers/{id}"),
            "huggingface",
        )
    }))
}

/// This family in the lookups' table ([`super::FAMILIES`]).
pub struct Lookups;
impl super::Family for Lookups {
    fn name(&self) -> &'static str {
        "papers"
    }
    fn sources(&self) -> Vec<std::sync::Arc<dyn crate::also_on::Source>> {
        vec![
            std::sync::Arc::new(also_on::Unpaywall),
            std::sync::Arc::new(also_on::Ncbi),
            std::sync::Arc::new(also_on::Datacite),
            std::sync::Arc::new(also_on::HuggingFacePapers),
        ]
    }
}

/// The forms of key the papers sources record (`also_on::Source::key_forms`).
pub const KEY_FORMS: &[(&str, &str)] = &[
    ("doi", "10.1038/nature12373"),
    ("arxiv", "2302.04668"),
    ("pmid", "23903748"),
    ("pmcid", "PMC3814474"),
];
/// The also-on sources that run these lookups: each fetches through the egress (its policy,
/// budgets and shared discovery cache), hands the answer to this module's parsers, and reports
/// keys and derived links.
pub mod also_on {
    use crate::{
        also_on::{self, Context, Source, Subject},
        lookups::{papers::operator_contact, secondary, send, with_retry},
    };
    use anyhow::Result;
    use async_trait::async_trait;

    /// The DOI of a card, unless it is arXiv's own (`10.48550/arXiv.…`, read as an arXiv paper).
    fn journal_doi(subject: &Subject) -> Option<String> {
        let doi = crate::resolver::cards::doi(&subject.uri)?;
        (!doi.starts_with("10.48550/")).then_some(doi)
    }

    /// The arXiv paper of a card: its arXiv, alphaXiv or arXiv DOI link, or a key another source
    /// found for it.
    fn arxiv(subject: &Subject) -> Option<String> {
        crate::lookups::papers::arxiv_from_uri(&subject.uri).or_else(|| {
            subject
                .keys_in("arxiv")
                .find_map(crate::lookups::papers::arxiv_id)
        })
    }

    /// Open copies of a paper from Unpaywall (links, never keys). Needs the contact setting.
    pub struct Unpaywall;

    #[async_trait]
    impl Source for Unpaywall {
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::papers::KEY_FORMS
        }
        fn id(&self) -> &'static str {
            "unpaywall"
        }
        fn applies(&self, subject: &Subject) -> bool {
            journal_doi(subject).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            let (Some(doi), Some(contact)) = (journal_doi(subject), operator_contact(cx)) else {
                return Ok(also_on::Found::default());
            };
            let Some(request) = crate::lookups::papers::unpaywall_request(&doi, contact) else {
                return Ok(also_on::Found::default());
            };
            let f = send(cx, &request, "unpaywall_busy").await?;
            let links = crate::lookups::papers::parse_unpaywall(f.status, &f.body, &doi)?;
            Ok(crate::lookups::Found {
                keys: vec![],
                links,
            }
            .into_also_on("unpaywall"))
        }
    }

    /// PubMed and PubMed Central for a DOI, from the NCBI ID Converter. Needs the contact setting.
    pub struct Ncbi;

    #[async_trait]
    impl Source for Ncbi {
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::papers::KEY_FORMS
        }
        fn budgets(&self) -> &'static [(&'static str, f64)] {
            // NCBI's E-utilities and ID Converter: 3 requests/s without an API key.
            &[("pmc.ncbi.nlm.nih.gov", 3.0)]
        }
        fn single_namespaces(&self) -> &'static [&'static str] {
            &["pmid", "pmcid"]
        }
        fn search_key(&self, namespace: &str, value: &str) -> Option<String> {
            let digits = crate::lookups::digits;
            match namespace {
                "pmid" => digits(value).then(|| format!("pmid:{value}")),
                "pmcid" => digits(value.strip_prefix("PMC")?).then(|| format!("pmcid:{value}")),
                _ => None,
            }
        }
        /// An article's own page: `/{pmid}` on PubMed, `/articles/PMC{n}` on PubMed Central.
        fn service_name(&self, host: &str, url: &url::Url) -> Option<&'static str> {
            use crate::lookups::digits;
            let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
            match (host, segments.as_slice()) {
                ("pubmed.ncbi.nlm.nih.gov", [pmid]) if digits(pmid) => Some("PubMed"),
                ("pmc.ncbi.nlm.nih.gov", ["articles", id])
                    if id.strip_prefix("PMC").is_some_and(digits) =>
                {
                    Some("PubMed Central")
                }
                _ => None,
            }
        }
        fn id(&self) -> &'static str {
            "ncbi"
        }
        fn applies(&self, subject: &Subject) -> bool {
            journal_doi(subject).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            let (Some(doi), Some(contact)) = (journal_doi(subject), operator_contact(cx)) else {
                return Ok(also_on::Found::default());
            };
            let Some(request) = crate::lookups::papers::ncbi_request(&[&doi], contact) else {
                return Ok(also_on::Found::default());
            };
            let f = send(cx, &request, "ncbi_busy").await?;
            Ok(crate::lookups::papers::parse_ncbi(f.status, &f.body)?
                .into_iter()
                .find(|(requested, _)| requested.eq_ignore_ascii_case(&doi))
                .map(|(_, found)| found.into_also_on("ncbi"))
                .unwrap_or_default())
        }
    }

    /// The arXiv preprint and its journal version, which DataCite names exactly in both directions;
    /// an arXiv paper also gets its arXiv and alphaXiv pages. DataCite records what arXiv's
    /// submitter entered, so the two share keys only when the journal's publisher names the
    /// preprint back in its Crossref record.
    pub struct Datacite;

    impl Datacite {
        async fn publisher_names(cx: &Context, doi: &str, arxiv: &str) -> Result<bool> {
            let Some(request) = crate::lookups::papers::crossref_request(doi) else {
                return Ok(false);
            };
            let f = match send(cx, &request, "crossref_busy").await {
                Ok(f) => f,
                // A record past the body cap (a very long reference list) confirms nothing; the
                // run keeps its other answers.
                Err(error) if crate::resolver::egress::over_cap(&error) => return Ok(false),
                Err(error) => return Err(error),
            };
            crate::lookups::papers::parse_crossref_preprint(f.status, &f.body, doi, arxiv)
        }
    }

    #[async_trait]
    impl Source for Datacite {
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::papers::KEY_FORMS
        }
        fn budgets(&self) -> &'static [(&'static str, f64)] {
            // Crossref: 10 requests/s on the polite pool.
            &[("api.crossref.org", 10.0)]
        }
        fn single_namespaces(&self) -> &'static [&'static str] {
            &["arxiv"]
        }
        fn search_key(&self, namespace: &str, value: &str) -> Option<String> {
            use crate::lookups::papers::{arxiv_id, arxiv_key};
            (namespace == "arxiv").then(|| Some(arxiv_key(&arxiv_id(value)?).to_string()))?
        }
        /// A paper's own page (`arxiv_from_uri`).
        fn service_name(&self, host: &str, url: &url::Url) -> Option<&'static str> {
            let name = match host {
                "arxiv.org" => "arXiv",
                "alphaxiv.org" => "alphaXiv",
                _ => return None,
            };
            crate::lookups::papers::arxiv_from_uri(url.as_str()).map(|_| name)
        }
        fn id(&self) -> &'static str {
            "datacite"
        }
        fn applies(&self, subject: &Subject) -> bool {
            journal_doi(subject).is_some()
                || crate::lookups::papers::arxiv_from_uri(&subject.uri).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            use crate::lookups::papers;
            let mut retry = None;
            if let Some(id) = papers::arxiv_from_uri(&subject.uri) {
                // The paper's own key and pages first: they never wait for DataCite or Crossref.
                let mut found = crate::lookups::Found::default();
                found.push_key(papers::arxiv_key(&id));
                for link in papers::arxiv_links(&id, "datacite") {
                    found.push_link(link);
                }
                let Some(request) = papers::datacite_arxiv_request(&id) else {
                    return Ok(found.into_also_on("datacite"));
                };
                let record = secondary(
                    "datacite",
                    &mut retry,
                    async {
                        let f = send(cx, &request, "datacite_busy").await?;
                        papers::parse_datacite_arxiv(f.status, &f.body, &id)
                    }
                    .await,
                );
                if let Some((named, journal)) = record {
                    for link in named.links {
                        found.push_link(link);
                    }
                    if let Some(journal) = journal {
                        let confirmed = Self::publisher_names(cx, &journal, &id).await;
                        if secondary("crossref", &mut retry, confirmed) == Some(true) {
                            found.push_key(papers::doi_key(&journal));
                        }
                    }
                }
                return Ok(with_retry(found.into_also_on("datacite"), retry));
            }
            let Some(doi) = journal_doi(subject) else {
                return Ok(also_on::Found::default());
            };
            // The card's own DOI is its key (`key=doi:` finds it) whatever DataCite and Crossref
            // answer; only the publisher's record gives it to the preprint.
            let mut found = crate::lookups::Found::default();
            found.push_key(papers::doi_key(&doi));
            let Some(request) = papers::datacite_versions_request(&doi) else {
                return Ok(found.into_also_on("datacite"));
            };
            let record = secondary(
                "datacite",
                &mut retry,
                async {
                    let f = send(cx, &request, "datacite_busy").await?;
                    papers::parse_datacite_versions(f.status, &f.body, &doi)
                }
                .await,
            );
            if let Some((named, preprint)) = record {
                for link in named.links {
                    found.push_link(link);
                }
                if let Some(id) = preprint {
                    let confirmed = Self::publisher_names(cx, &doi, &id).await;
                    if secondary("crossref", &mut retry, confirmed) == Some(true) {
                        found.push_key(papers::arxiv_key(&id));
                    }
                }
            }
            Ok(with_retry(found.into_also_on("datacite"), retry))
        }
    }

    /// The Hugging Face Papers page of an arXiv paper, only when its API says the page exists.
    pub struct HuggingFacePapers;

    #[async_trait]
    impl Source for HuggingFacePapers {
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::papers::KEY_FORMS
        }
        /// A paper's own page: `/papers/{arXiv id}`.
        fn service_name(&self, host: &str, url: &url::Url) -> Option<&'static str> {
            let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
            (host == "huggingface.co"
                && matches!(
                    segments.as_slice(),
                    ["papers", id] if crate::lookups::papers::arxiv_id(id).is_some()
                ))
            .then_some("Hugging Face Papers")
        }
        fn id(&self) -> &'static str {
            "huggingface"
        }
        fn applies(&self, subject: &Subject) -> bool {
            arxiv(subject).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            let Some(id) = arxiv(subject) else {
                return Ok(also_on::Found::default());
            };
            let Some(request) = crate::lookups::papers::hugging_face_request(&id) else {
                return Ok(also_on::Found::default());
            };
            let f = send(cx, &request, "huggingface_busy").await?;
            let links = crate::lookups::papers::parse_hugging_face(f.status, &f.body, &id)?
                .into_iter()
                .collect();
            Ok(crate::lookups::Found {
                keys: vec![],
                links,
            }
            .into_also_on("huggingface"))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::lookups::fixture::{at, keys, read, recorded, recorded_but_busy, sites, JSON};

        #[tokio::test(start_paused = true)]
        async fn papers() {
            use crate::lookups::papers as p;
            let doi_text = "10.1038/nature12373";
            let mut cx = recorded(vec![
                (
                    p::ncbi_request(&[doi_text], "info@synonym.to")
                        .unwrap()
                        .url
                        .into(),
                    "ncbi-idconv",
                ),
                (
                    p::datacite_versions_request(doi_text).unwrap().url.into(),
                    "datacite-versions-nature12373",
                ),
            ]);
            let mut config = crate::config::Config::default();
            config.http.contact = "info@synonym.to".into();
            cx.config = std::sync::Arc::new(config);
            let doi = at("doi:10.1038/nature12373");
            for source in [&Unpaywall as &dyn Source, &Ncbi, &Datacite] {
                assert!(source.applies(&doi), "{}", source.id());
            }
            assert!(!HuggingFacePapers.applies(&doi));
            let open = Unpaywall.lookup(&cx, &doi).await.unwrap();
            assert!(!open.links.is_empty() && open.keys.is_empty());
            assert!(open.links.iter().all(|l| l.via == "unpaywall"));
            let ncbi = Ncbi.lookup(&cx, &doi).await.unwrap();
            assert_eq!(keys(&ncbi).len(), 2, "{:?}", keys(&ncbi));
            assert!(sites(&ncbi)
                .iter()
                .any(|s| s.starts_with("PubMed https://pubmed.ncbi.nlm.nih.gov/")));
            // arXiv 1304.1068's submitter named this DOI; Nature's own record names no preprint, so
            // the preprint is a link and the two never join.
            let preprint = Datacite.lookup(&cx, &doi).await.unwrap();
            assert_eq!(keys(&preprint), ["doi:10.1038/nature12373"]);
            assert!(sites(&preprint).contains(&"arXiv https://arxiv.org/abs/1304.1068".to_owned()));
            let arxiv = at("https://arxiv.org/abs/1304.1068v2");
            assert!(!Unpaywall.applies(&arxiv) && Datacite.applies(&arxiv));
            let journal = Datacite.lookup(&cx, &arxiv).await.unwrap();
            assert_eq!(keys(&journal), ["arxiv:1304.1068"]);
            let journal_sites = sites(&journal);
            assert!(journal_sites
                .contains(&"alphaXiv https://www.alphaxiv.org/abs/1304.1068".to_owned()));
            assert!(journal_sites.contains(&"DOI https://doi.org/10.1038/nature12373".to_owned()));

            // LMCS names arXiv 2302.04668 as its preprint (has-preprint): both sides share the keys.
            let lmcs = Datacite
                .lookup(&cx, &at("doi:10.46298/lmcs-20(1:8)2024"))
                .await
                .unwrap();
            assert_eq!(
                keys(&lmcs),
                ["doi:10.46298/lmcs-20(1:8)2024", "arxiv:2302.04668"]
            );
            let preprint = Datacite
                .lookup(&cx, &at("https://arxiv.org/abs/2302.04668"))
                .await
                .unwrap();
            assert_eq!(
                keys(&preprint),
                ["arxiv:2302.04668", "doi:10.46298/lmcs-20(1:8)2024"]
            );
            // Hugging Face only when its API knows the paper.
            assert!(HuggingFacePapers
                .lookup(&cx, &arxiv)
                .await
                .unwrap()
                .links
                .is_empty());
            let attention = HuggingFacePapers
                .lookup(&cx, &at("https://arxiv.org/abs/1706.03762"))
                .await
                .unwrap();
            assert_eq!(
                sites(&attention),
                ["Hugging Face Papers https://huggingface.co/papers/1706.03762"]
            );

            // Without a contact, the sources that need one find nothing (and ask nothing).
            let cx = recorded(vec![]);
            assert_eq!(
                Unpaywall.lookup(&cx, &doi).await.unwrap(),
                Default::default()
            );
            assert_eq!(Ncbi.lookup(&cx, &doi).await.unwrap(), Default::default());
        }

        /// A Crossref record past the body cap confirms nothing: the preprint keeps its own key and
        /// links, without the journal's DOI, and the source doesn't fail.
        #[tokio::test(start_paused = true)]
        async fn an_oversized_crossref_record_confirms_nothing() {
            struct TooLarge(std::sync::Arc<crate::sources::fixture::Recorded>);
            #[async_trait]
            impl crate::resolver::egress::Transport for TooLarge {
                async fn get(
                    &self,
                    url: &url::Url,
                    addresses: &[std::net::SocketAddr],
                ) -> Result<crate::resolver::egress::Fetched> {
                    if url.host_str() == Some("api.crossref.org") {
                        return Err(crate::resolver::egress::BodyTooLarge {
                            cap: crate::resolver::egress::BODY_CAP,
                        }
                        .into());
                    }
                    self.0.get(url, addresses).await
                }
            }
            let arxiv = "https://api.datacite.org/dois/10.48550/arXiv.2302.04668";
            let (_, recorded) = crate::sources::fixture::egress(vec![(
                arxiv,
                200,
                JSON,
                read("datacite-arxiv-2302.04668.json"),
            )]);
            let cx = Context {
                egress: crate::resolver::egress::Egress::with_backends(
                    Default::default(),
                    std::sync::Arc::new(crate::sources::fixture::PublicDns),
                    std::sync::Arc::new(TooLarge(recorded)),
                ),
                config: Default::default(),
            };
            let found = Datacite
                .lookup(&cx, &at("https://arxiv.org/abs/2302.04668"))
                .await
                .unwrap();
            assert_eq!(keys(&found), ["arxiv:2302.04668"]);
            assert!(
                sites(&found).contains(&"DOI https://doi.org/10.46298/lmcs-20(1:8)2024".to_owned())
            );
        }

        /// A secondary service that fails costs its own part only: the link's own key and the other
        /// services' answers are recorded, and the run asks again after the service's retry time.
        #[tokio::test(start_paused = true)]
        async fn a_failing_secondary_service_keeps_the_links_own_key() {
            use crate::lookups::papers as p;
            let nature = "10.1038/nature12373";
            let versions = p::datacite_versions_request(nature)
                .unwrap()
                .url
                .to_string();
            let arxiv = "https://api.datacite.org/dois/10.48550/arXiv.1304.1068";
            // DataCite busy: the DOI card keeps its own key, the preprint its key and pages.
            let busy = recorded_but_busy(vec![], &[versions.as_str(), arxiv]);
            let found = Datacite
                .lookup(&busy, &at("doi:10.1038/nature12373"))
                .await
                .unwrap();
            assert_eq!(keys(&found), ["doi:10.1038/nature12373"]);
            assert!(found.retry_after.is_some());
            let found = Datacite
                .lookup(&busy, &at("https://arxiv.org/abs/1304.1068"))
                .await
                .unwrap();
            assert_eq!(keys(&found), ["arxiv:1304.1068"]);
            assert!(sites(&found).contains(&"arXiv https://arxiv.org/abs/1304.1068".to_owned()));
            assert!(found.retry_after.is_some());
            // Crossref busy: LMCS keeps its own key and the preprint's pages, not its key.
            let busy = recorded_but_busy(
                vec![],
                &["https://api.crossref.org/works/10.46298/lmcs-20%281%3A8%292024"],
            );
            let found = Datacite
                .lookup(&busy, &at("doi:10.46298/lmcs-20(1:8)2024"))
                .await
                .unwrap();
            assert_eq!(keys(&found), ["doi:10.46298/lmcs-20(1:8)2024"]);
            assert!(sites(&found).contains(&"arXiv https://arxiv.org/abs/2302.04668".to_owned()));
            assert!(found.retry_after.is_some());
        }

        /// No link or ID check of this family slices a string inside a multibyte character, and no
        /// parser panics on an answer it doesn't expect.
        #[test]
        fn spellings_and_answers_never_panic() {
            use crate::lookups::{fixture, papers};
            for s in fixture::multibyte_spellings(&[
                "https://arxiv.org/abs/1304.1068v2",
                "https://arxiv.org/pdf/2101.00001v1.pdf",
                "http://arxiv.org/abs/hep-th/9901001",
                "https://doi.org/10.48550/arXiv.1304.1068",
                "https://www.alphaxiv.org/abs/1706.03762",
            ]) {
                papers::arxiv_from_uri(&s);
                papers::arxiv_id(&s);
            }
            for (status, raw) in fixture::malformed_answers() {
                let _ = papers::parse_unpaywall(status, raw, "10.1000/x");
                let _ = papers::parse_ncbi(status, raw);
                let _ = papers::parse_datacite_arxiv(status, raw, "1304.1068");
                let _ = papers::parse_datacite_versions(status, raw, "10.1000/x");
                let _ = papers::parse_crossref_preprint(status, raw, "10.1000/x", "1304.1068");
                let _ = papers::parse_hugging_face(status, raw, "1304.1068");
            }
        }

        /// Its key namespaces and services, as searches and cards name them.
        #[test]
        fn keys_and_service_names() {
            use crate::lookups::{search_key, service_name};
            assert_eq!(
                search_key("arxiv", "1304.1068v2").as_deref(),
                Some("arxiv:1304.1068")
            );
            assert_eq!(
                search_key("pmcid", "PMC3672233").as_deref(),
                Some("pmcid:PMC3672233")
            );
            assert_eq!(search_key("pmcid", "123"), None);
            assert_eq!(
                service_name("https://arxiv.org/abs/1706.03762"),
                Some("arXiv")
            );
            assert_eq!(service_name("https://huggingface.co/models"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture;

    #[test]
    fn contact_setting() {
        assert_eq!(contact(" info@synonym.to "), Some("info@synonym.to"));
        assert_eq!(contact(""), None);
        assert_eq!(contact("nobody"), None);
        assert_eq!(contact("a b@c"), None);
    }

    /// Only an object's own page on this family's services is named after the service; any
    /// other page there (a topic list, a search, a user's page, a page under an object) is not.
    #[test]
    fn only_an_objects_own_page_is_named_after_its_service() {
        for (uri, name) in [
            ("https://www.alphaxiv.org/abs/1706.03762", Some("alphaXiv")),
            ("https://pubmed.ncbi.nlm.nih.gov/23903748/", Some("PubMed")),
            (
                "https://pmc.ncbi.nlm.nih.gov/articles/PMC3779179/",
                Some("PubMed Central"),
            ),
            (
                "https://huggingface.co/papers/1706.03762",
                Some("Hugging Face Papers"),
            ),
            ("https://arxiv.org/list/cs.AI/new", None),
            ("https://pubmed.ncbi.nlm.nih.gov/?term=x", None),
            ("https://huggingface.co/papers", None),
        ] {
            assert_eq!(crate::lookups::service_name(uri), name, "{uri}");
        }
    }

    /// Every link this family derives is named, by its URL, as the link names its service: the
    /// object-page rule never unnames the family's own links.
    #[test]
    fn every_derived_link_is_named_as_its_service() {
        let links = arxiv_links("1706.03762", "datacite");
        assert!(!links.is_empty());
        for link in &links {
            assert_eq!(
                crate::lookups::service_name(&link.uri),
                Some(link.site.as_str()),
                "{}",
                link.uri
            );
        }
    }

    const NATURE: &str = "10.1038/nature12373";

    #[test]
    fn arxiv_ids() {
        for (uri, id) in [
            ("https://arxiv.org/abs/1304.1068", "1304.1068"),
            ("https://arxiv.org/abs/1304.1068v2", "1304.1068"),
            ("https://arxiv.org/pdf/2101.00001v1.pdf", "2101.00001"),
            ("http://arxiv.org/abs/hep-th/9901001", "hep-th/9901001"),
            ("https://doi.org/10.48550/arXiv.1304.1068", "1304.1068"),
            ("doi:10.48550/arxiv.1304.1068", "1304.1068"),
            ("https://www.alphaxiv.org/abs/1706.03762", "1706.03762"),
        ] {
            assert_eq!(arxiv_from_uri(uri).as_deref(), Some(id), "{uri}");
        }
        for uri in [
            "https://arxiv.org/list/hep-th",
            "https://arxiv.org/abs/13041068",
            "https://example.org/abs/1304.1068",
            "https://doi.org/10.1038/nature12373",
        ] {
            assert_eq!(arxiv_from_uri(uri), None, "{uri}");
        }
    }

    /// A DOI whose segments walk out of its path asks nothing: the URL would resolve them and
    /// name another path of the service. A DOI with dots inside a segment is asked as it is.
    #[test]
    fn a_doi_with_dot_segments_is_never_a_request_path() {
        for doi in [
            "10.1234/../../x",
            "10.1234/..",
            "10.1234/./x",
            "10.1234/a/../../../x",
        ] {
            assert!(unpaywall_request(doi, "ops@example.org").is_none(), "{doi}");
            assert!(crossref_request(doi).is_none(), "{doi}");
        }
        assert_eq!(
            unpaywall_request("10.1234/a..b/c.d", "ops@example.org")
                .unwrap()
                .url
                .as_str(),
            "https://api.unpaywall.org/v2/10.1234/a..b/c.d?email=ops%40example.org"
        );
        assert_eq!(
            crossref_request("10.1234/a..b/c.d").unwrap().url.as_str(),
            "https://api.crossref.org/works/10.1234/a..b/c.d"
        );
    }

    #[test]
    fn unpaywall() {
        assert!(unpaywall_request(NATURE, "").is_none());
        assert_eq!(
            unpaywall_request(NATURE, "info@synonym.to")
                .unwrap()
                .url
                .as_str(),
            "https://api.unpaywall.org/v2/10.1038/nature12373?email=info%40synonym.to"
        );
        let links =
            parse_unpaywall(200, &fixture::read("unpaywall-nature12373.json"), NATURE).unwrap();
        let got: Vec<_> = links
            .iter()
            .map(|l| (l.site.as_str(), l.uri.as_str(), l.kind))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "arXiv",
                    "https://arxiv.org/abs/1304.1068",
                    Some("submitted")
                ),
                (
                    "PubMed Central",
                    "https://pmc.ncbi.nlm.nih.gov/articles/PMC4221854/",
                    Some("submitted")
                ),
                (
                    "Harvard University",
                    "http://nrs.harvard.edu/urn-3:HUL.InstRepos:12285462",
                    Some("submitted")
                ),
            ]
        );
        assert!(links.iter().all(|l| l.via == "unpaywall"));
        let missing = fixture::read("unpaywall-missing.html");
        assert!(parse_unpaywall(404, &missing, "10.9999/x")
            .unwrap()
            .is_empty());
        let other = fixture::read("unpaywall-nature12373.json");
        assert!(parse_unpaywall(200, &other, "10.1/other").is_err());
        assert!(parse_unpaywall(429, b"", NATURE).is_err());
    }

    #[test]
    fn ncbi() {
        assert!(ncbi_request(&[NATURE], "").is_none());
        assert!(ncbi_request(&["10.1/a,b"], "info@synonym.to").is_none());
        let r = ncbi_request(&[NATURE, "10.1093/nar/gks1195"], "info@synonym.to").unwrap();
        assert_eq!(
            r.url.as_str(),
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/\
             ?ids=10.1038/nature12373,10.1093/nar/gks1195&format=json&tool=pinakes\
             &email=info%40synonym.to"
        );
        let many = vec![NATURE; NCBI_MAX_IDS + 5];
        let r = ncbi_request(&many, "info@synonym.to").unwrap();
        assert_eq!(r.url.as_str().matches("nature12373").count(), NCBI_MAX_IDS);

        let records = parse_ncbi(200, &fixture::read("ncbi-idconv.json")).unwrap();
        assert_eq!(records.len(), 2, "the unknown DOI is left out");
        let (doi, found) = &records[0];
        assert_eq!(doi, NATURE);
        assert_eq!(
            found.keys.iter().map(Key::as_str).collect::<Vec<_>>(),
            ["pmid:23903748", "pmcid:PMC4221854"]
        );
        assert_eq!(
            found
                .links
                .iter()
                .map(|l| l.uri.as_str())
                .collect::<Vec<_>>(),
            [
                "https://pubmed.ncbi.nlm.nih.gov/23903748/",
                "https://pmc.ncbi.nlm.nih.gov/articles/PMC4221854/"
            ]
        );
        assert!(parse_ncbi(200, br#"{"status":"error"}"#).is_err());
        assert!(parse_ncbi(429, b"").is_err());
    }

    #[test]
    fn datacite_both_ways() {
        assert_eq!(
            datacite_arxiv_request("1304.1068v3").unwrap().url.as_str(),
            "https://api.datacite.org/dois/10.48550/arXiv.1304.1068"
        );
        // The record names one journal version: a link and a candidate DOI, never a key here.
        let (found, journal) = parse_datacite_arxiv(
            200,
            &fixture::read("datacite-arxiv-1304.1068.json"),
            "1304.1068",
        )
        .unwrap();
        assert_eq!(
            found.keys.iter().map(Key::as_str).collect::<Vec<_>>(),
            ["arxiv:1304.1068"]
        );
        assert_eq!(journal.as_deref(), Some(NATURE));
        assert_eq!(found.links[0].uri, "https://doi.org/10.1038/nature12373");
        assert!(parse_datacite_arxiv(
            200,
            &fixture::read("datacite-arxiv-1304.1068.json"),
            "1304.9999"
        )
        .is_err());
        assert_eq!(
            parse_datacite_arxiv(404, b"", "1304.1068").unwrap(),
            (Found::default(), None)
        );

        let r = datacite_versions_request(NATURE).unwrap();
        assert!(r.url.as_str().starts_with(
            "https://api.datacite.org/dois?query=relatedIdentifiers.relatedIdentifier%3A%2210.1038%2Fnature12373%22%20AND"
        ));
        assert!(datacite_versions_request("10.1/\"x").is_none());
        let (found, preprint) = parse_datacite_versions(
            200,
            &fixture::read("datacite-versions-nature12373.json"),
            NATURE,
        )
        .unwrap();
        assert!(found.keys.is_empty());
        assert_eq!(preprint.as_deref(), Some("1304.1068"));
        assert_eq!(
            found
                .links
                .iter()
                .map(|l| (l.site.as_str(), l.uri.as_str()))
                .collect::<Vec<_>>(),
            [
                ("arXiv", "https://arxiv.org/abs/1304.1068"),
                ("alphaXiv", "https://www.alphaxiv.org/abs/1304.1068")
            ]
        );
        // The search may return near matches; only an exact relation counts.
        let other = parse_datacite_versions(
            200,
            &fixture::read("datacite-versions-nature12373.json"),
            "10.1038/nature1237",
        );
        assert_eq!(other.unwrap(), (Found::default(), None));
        // A record naming two journal versions names neither.
        let two = serde_json::json!({"data": [{"id": "10.48550/arxiv.1304.1068", "attributes": {
        "relatedIdentifiers": [
            {"relationType": "IsVersionOf", "relatedIdentifierType": "DOI", "relatedIdentifier": NATURE},
            {"relationType": "IsVersionOf", "relatedIdentifierType": "DOI", "relatedIdentifier": "10.1000/other"}
        ]}}]});
        let two = serde_json::to_vec(&two).unwrap();
        assert_eq!(
            parse_datacite_versions(200, &two, NATURE).unwrap(),
            (Found::default(), None)
        );
    }

    /// The pair joins only when the journal's publisher names the preprint in its own record.
    #[test]
    fn crossref_names_the_preprint() {
        const LMCS: &str = "10.46298/lmcs-20(1:8)2024";
        assert_eq!(
            crossref_request(LMCS).unwrap().url.as_str(),
            "https://api.crossref.org/works/10.46298/lmcs-20%281%3A8%292024"
        );
        let lmcs = fixture::read("crossref-lmcs-20-1-8.json");
        // By arXiv id (its versions `2302.04668v1`, `v2`), asserted by the publisher.
        assert!(parse_crossref_preprint(200, &lmcs, LMCS, "2302.04668").unwrap());
        assert!(parse_crossref_preprint(200, &lmcs, LMCS, "2302.04668v1").unwrap());
        assert!(!parse_crossref_preprint(200, &lmcs, LMCS, "2302.04669").unwrap());
        assert!(parse_crossref_preprint(200, &lmcs, NATURE, "2302.04668").is_err());
        // Nature's record names no preprint, so arXiv's claim of 10.1038/nature12373 stays a link.
        let nature = fixture::read("crossref-nature12373.json");
        assert!(!parse_crossref_preprint(200, &nature, NATURE, "1304.1068").unwrap());
        assert!(!parse_crossref_preprint(404, b"", NATURE, "1304.1068").unwrap());
        // By arXiv DOI; a relation the preprint's side asserted does not count.
        let record = |asserted: &str| {
            serde_json::to_vec(&serde_json::json!({"message": {"DOI": NATURE, "relation": {
                "has-preprint": [{"id-type": "doi", "id": "10.48550/arXiv.1304.1068", "asserted-by": asserted}]
            }}}))
            .unwrap()
        };
        assert!(parse_crossref_preprint(200, &record("subject"), NATURE, "1304.1068").unwrap());
        assert!(!parse_crossref_preprint(200, &record("object"), NATURE, "1304.1068").unwrap());
    }

    #[test]
    fn hugging_face() {
        assert_eq!(
            hugging_face_request("1706.03762").unwrap().url.as_str(),
            "https://huggingface.co/api/papers/1706.03762"
        );
        let link = parse_hugging_face(
            200,
            &fixture::read("hf-papers-1706.03762.json"),
            "1706.03762",
        )
        .unwrap()
        .unwrap();
        assert_eq!(link.uri, "https://huggingface.co/papers/1706.03762");
        let missing = fixture::read("hf-papers-1304.1068.json");
        assert_eq!(
            parse_hugging_face(404, &missing, "1304.1068").unwrap(),
            None
        );
        let wrong = fixture::read("hf-papers-1706.03762.json");
        assert_eq!(parse_hugging_face(200, &wrong, "1304.1068").unwrap(), None);
    }
}
