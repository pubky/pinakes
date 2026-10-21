//! The lookups as "also on" sources (`also_on::Source`): each fetches through the egress (its
//! policy, budgets and shared discovery cache), hands the answer to the pure parsers of its
//! module, and reports keys and derived links.
use super::{places, wikidata as items};
use crate::{
    also_on::{self, Context, Source, Subject},
    hubs,
    resolver::{egress::FetchOptions, RetryLater},
    sources::discogs,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

impl super::Found {
    /// As the also-on machinery records it: every key vouched for by `via`.
    pub fn into_also_on(self, via: &str) -> also_on::Found {
        also_on::Found {
            keys: self
                .keys
                .into_iter()
                .map(|k| also_on::Key {
                    key: k.0,
                    via: via.into(),
                })
                .collect(),
            links: self
                .links
                .into_iter()
                .map(|l| {
                    let mut link = also_on::Link::new(l.uri, l.site, l.via);
                    link.kind = l.kind.map(str::to_owned);
                    link.at = l.at.and_then(|at| at.parse().ok());
                    link
                })
                .collect(),
            ..Default::default()
        }
    }
}

/// A JSON GET through the shared discovery cache (an hour). A busy service (429, 5xx) answers
/// `RetryLater` with `class`; every other status is the caller's. A Wikidata read takes the
/// hubs' larger cap (a much-edited item), as the hubs' own reads of the same URL do.
async fn get(cx: &Context, url: &str, class: &'static str) -> Result<(u16, Vec<u8>)> {
    let options = FetchOptions {
        accept: Some("application/json".into()),
        body_cap: url
            .starts_with("https://www.wikidata.org/")
            .then_some(hubs::WIKIDATA_CAP),
        ..Default::default()
    };
    let f = cx.egress.discover(url, &options, 3600).await?;
    if f.status == 429 || f.status >= 500 {
        return Err(RetryLater {
            class,
            seconds: f.retry_after_secs.clamp(60, 3600),
        }
        .into());
    }
    Ok((f.status, f.body))
}

/// One Wikidata item, as the raw entity (for every claim type, URLs included) and as the hub
/// reads it (external IDs and sitelinks), or `None` when it has none or its answer is over the
/// cap. The request is the hub's own, so both share its cache.
async fn item(cx: &Context, qid: &str) -> Result<Option<(Value, hubs::wikidata::Item)>> {
    let url = hubs::wikidata::entities_lookup(&[qid.to_owned()]);
    // An item over its read's cap is no answer: asking again would read the same megabytes.
    let (status, body) = match get(cx, &url, "wikidata_busy").await {
        Err(error) if crate::resolver::egress::over_cap(&error) => return Ok(None),
        answer => answer?,
    };
    if status == 404 {
        return Ok(None);
    }
    let mut entities = items::parse_entities(status, &body)?;
    let Some(raw) = entities.remove(qid) else {
        return Ok(None);
    };
    let hub = hubs::wikidata::parse_entities(&serde_json::from_slice(&body)?)
        .into_iter()
        .find(|i| i.qid == qid);
    Ok(hub.map(|hub| (raw, hub)))
}

/// The item's Wikipedia editions in `wikipedia.rs`'s order (preferred languages first), then its
/// other encyclopedias.
fn editions(hub: &hubs::wikidata::Item) -> Vec<also_on::Link> {
    let none = crate::wikipedia::Article {
        lang: String::new(),
        title: String::new(),
        canonical: String::new(),
    };
    let (mut wikipedia, others): (Vec<_>, Vec<_>) = crate::wikipedia::found(hub, &none)
        .links
        .into_iter()
        .partition(|l| l.site.starts_with("Wikipedia"));
    wikipedia.extend(others);
    wikipedia
}

/// The `wikidata=` tag of an OSM object's card, when it names one item.
fn osm_tag(subject: &Subject) -> Option<String> {
    let tag = subject.doc.as_ref()?.attributes.get("wikidata")?;
    places::wikidata_tag(tag.trim()).map(str::to_owned)
}

/// An OSM object tagged `wikidata=Q…` whose item names it back: the item's Wikidata page,
/// official website, maps and Wikipedia editions as derived links, and both keys when the item
/// names no other OSM object.
pub struct OsmWikidata;
#[async_trait]
impl Source for OsmWikidata {
    fn id(&self) -> &'static str {
        "osm_wikidata"
    }
    fn applies(&self, subject: &Subject) -> bool {
        crate::resolver::osm::parse_id(&subject.uri).is_some() && osm_tag(subject).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let (Some(osm), Some(qid)) = (
            crate::resolver::osm::parse_id(&subject.uri),
            osm_tag(subject),
        ) else {
            return Ok(also_on::Found::default());
        };
        let Some((raw, hub)) = item(cx, &qid).await? else {
            return Ok(also_on::Found::default());
        };
        if !places::names(&osm, &qid, &raw) {
            return Ok(also_on::Found::default());
        }
        let found = places::from_item(&osm, &qid, &raw).into_also_on("wikidata");
        let mut links = vec![also_on::Link::new(
            format!("https://www.wikidata.org/wiki/{qid}"),
            "Wikidata",
            "wikidata",
        )];
        // The place links first, then the Wikipedia editions in their preferred order.
        links.extend(found.links.into_iter().filter(|l| l.site != "Wikipedia"));
        links.extend(editions(&hub));
        Ok(also_on::Found {
            keys: found.keys,
            links,
            ..Default::default()
        })
    }
}

/// The item a Wikidata link names (`www.wikidata.org/wiki/Q…`, `/entity/Q…`).
pub fn wikidata_item(uri: &str) -> Option<String> {
    let url = super::web_url(uri)?;
    if !matches!(
        url.host_str()?,
        "www.wikidata.org" | "wikidata.org" | "m.wikidata.org"
    ) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.collect();
    match segments.as_slice() {
        ["wiki" | "entity", q] => items::qid(q).map(str::to_owned),
        _ => None,
    }
}

/// The one OSM object an item names: its OSM relation (P402) when it has exactly one, else its
/// only OSM link of any type (P402, P11693 node, P10689 way).
pub fn osm_object(item: &Value) -> Option<(&'static str, String)> {
    let ids = |pid: &str| -> Vec<String> {
        items::best_values(item, pid)
            .into_iter()
            .filter(|v| super::plain(v, 20, |c| c.is_ascii_digit()) && !v.starts_with('0'))
            .map(str::to_owned)
            .collect()
    };
    let relations = ids("P402");
    if relations.len() == 1 {
        return Some(("relation", relations[0].clone()));
    }
    let all: Vec<(&'static str, String)> = relations
        .into_iter()
        .map(|id| ("relation", id))
        .chain(ids("P11693").into_iter().map(|id| ("node", id)))
        .chain(ids("P10689").into_iter().map(|id| ("way", id)))
        .collect();
    match all.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// A Wikidata item, or a Wikipedia article about one, that names one OSM object: the object on
/// OpenStreetMap as a derived link.
pub struct WikidataOsm;
impl WikidataOsm {
    /// The item: from a Wikidata link, a key another source found, or the article's own
    /// Wikipedia (the request `wikipedia.rs` makes, so the two share its cache).
    async fn qid(cx: &Context, subject: &Subject) -> Result<Option<String>> {
        if let Some(qid) = wikidata_item(&subject.uri) {
            return Ok(Some(qid));
        }
        if let Some(qid) = subject.keys_in("wikidata").find_map(items::qid) {
            return Ok(Some(qid.to_owned()));
        }
        let Some(article) = crate::wikipedia::parse(&subject.uri) else {
            return Ok(None);
        };
        let mut url =
            url::Url::parse(&format!("https://{}.wikipedia.org/w/api.php", article.lang))?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("prop", "pageprops")
            .append_pair("ppprop", "wikibase_item")
            .append_pair("titles", &article.title)
            .append_pair("redirects", "1")
            .append_pair("format", "json");
        let (status, body) = get(cx, url.as_str(), "wikipedia_busy").await?;
        if status != 200 {
            return Ok(None);
        }
        Ok(crate::wikipedia::parse_item(&serde_json::from_slice(
            &body,
        )?))
    }
}
#[async_trait]
impl Source for WikidataOsm {
    fn id(&self) -> &'static str {
        "wikidata_osm"
    }
    fn applies(&self, subject: &Subject) -> bool {
        crate::resolver::osm::parse_id(&subject.uri).is_none()
            && (wikidata_item(&subject.uri).is_some()
                || crate::wikipedia::parse(&subject.uri).is_some()
                || subject.keys_in("wikidata").next().is_some())
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let Some(qid) = Self::qid(cx, subject).await? else {
            return Ok(also_on::Found::default());
        };
        let Some((raw, _)) = item(cx, &qid).await? else {
            return Ok(also_on::Found::default());
        };
        let links = osm_object(&raw)
            .map(|(kind, id)| {
                also_on::Link::new(
                    format!("https://www.openstreetmap.org/{kind}/{id}"),
                    "OpenStreetMap",
                    "wikidata",
                )
            })
            .into_iter()
            .collect();
        Ok(also_on::Found {
            links,
            ..Default::default()
        })
    }
}

/// A request a builder made, through the shared discovery cache (an hour), with its `Accept`.
/// A busy service (429, 5xx) answers `RetryLater` with `class`.
async fn send(
    cx: &Context,
    request: &super::Request,
    class: &'static str,
) -> Result<crate::resolver::egress::Fetched> {
    let options = FetchOptions {
        accept: Some(request.accept.unwrap_or("application/json").into()),
        // JSON under its own media types: PyPI's provenance, JSON:API.
        extra_types: &[
            "application/vnd.pypi.integrity.v1+json",
            "application/vnd.api+json",
        ],
        keep_redirect: !request.follow_redirects,
        ..Default::default()
    };
    let f = cx
        .egress
        .discover(request.url.as_str(), &options, 3600)
        .await?;
    if f.status == 429 || f.status >= 500 {
        return Err(RetryLater {
            class,
            seconds: f.retry_after_secs.clamp(60, 3600),
        }
        .into());
    }
    Ok(f)
}

/// The operator's contact, for APIs that ask for an email address.
fn contact(cx: &Context) -> Option<&str> {
    super::contact(&cx.config.http.contact)
}

/// A Federal Register document: its GovInfo copies, regulations.gov document and
/// public-inspection PDF from the API record, and `fr:{number}`, which a GovInfo copy someone
/// tags shares (read from its address alone).
pub struct FederalRegister;
#[async_trait]
impl Source for FederalRegister {
    fn id(&self) -> &'static str {
        "federal_register"
    }
    fn applies(&self, subject: &Subject) -> bool {
        use crate::sources::federal_register as fr;
        fr::number(&subject.uri).is_some() || fr::from_govinfo_uri(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        use crate::sources::federal_register as fr;
        let key = |number: &str| also_on::Key {
            key: format!("fr:{number}"),
            via: "federalregister".into(),
        };
        if let Some(number) = fr::from_govinfo_uri(&subject.uri) {
            return Ok(also_on::Found {
                keys: vec![key(&number)],
                ..Default::default()
            });
        }
        let Some(number) = fr::number(&subject.uri) else {
            return Ok(also_on::Found::default());
        };
        let Some(url) = fr::api_url(&number) else {
            return Ok(also_on::Found::default());
        };
        let (status, body) = get(cx, &url, "federalregister_busy").await?;
        let Some(document) = fr::parse(status, &body, &number)? else {
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

/// BTC Map's places by OSM object, from its keyless sync (`updated_since`, deleted places
/// included), kept in memory and brought up to date at most hourly.
#[derive(Default)]
struct BtcmapIndex {
    places: std::collections::HashSet<String>,
    since: Option<String>,
    synced: Option<std::time::Instant>,
}
static BTCMAP: std::sync::LazyLock<tokio::sync::Mutex<BtcmapIndex>> =
    std::sync::LazyLock::new(Default::default);
/// Rows per sync page (each about 200 bytes, so a page stays under the body limit).
const BTCMAP_PAGE: u32 = 2000;
/// Rows per page when a full page's places all changed at one instant.
const BTCMAP_PAGE_MAX: u32 = 4000;
/// Pages per lookup; a longer first sync continues on the next lookup.
const BTCMAP_PAGES: usize = 25;
const BTCMAP_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);

impl BtcmapIndex {
    async fn sync(&mut self, cx: &Context) -> Result<()> {
        if self.synced.is_some_and(|at| at.elapsed() < BTCMAP_EVERY) {
            return Ok(());
        }
        let mut limit = BTCMAP_PAGE;
        for _ in 0..BTCMAP_PAGES {
            let since = self
                .since
                .clone()
                .unwrap_or_else(|| "2010-01-01T00:00:00Z".to_owned());
            let request = places::btcmap_sync_request(&since, limit)
                .ok_or_else(|| anyhow::anyhow!("invalid sync time {since}"))?;
            let f = send(cx, &request, "btcmap_busy").await?;
            let page = places::parse_btcmap_places(f.status, &f.body)?;
            for place in &page {
                if let Some(osm) = &place.osm {
                    if place.deleted {
                        self.places.remove(&osm.key());
                    } else {
                        self.places.insert(osm.key());
                    }
                }
            }
            let next = places::btcmap_next_since(&page).map(str::to_owned);
            // A short page is the last; a full one never is, more may follow.
            if page.len() < limit as usize {
                self.since = next.or(self.since.take());
                self.synced = Some(std::time::Instant::now());
                return Ok(());
            }
            if next.is_some() && next == self.since {
                // Every place on the page changed at that instant: a larger page, then past it.
                if limit < BTCMAP_PAGE_MAX {
                    limit = BTCMAP_PAGE_MAX;
                    continue;
                }
                let past = places::btcmap_after(&since)
                    .ok_or_else(|| anyhow::anyhow!("invalid sync time {since}"))?;
                tracing::warn!(%since, "more BTC Map places changed at one instant than a page holds; some are skipped");
                self.since = Some(past);
                continue;
            }
            self.since = next;
        }
        Ok(())
    }
}

/// An OSM object that BTC Map lists: its merchant page, which names that one object
/// (`btcmap.org/merchant/{type}:{id}`). Both carry the object's own `osm:` key, so a tagged
/// merchant link joins the object.
pub struct Btcmap;
#[async_trait]
impl Source for Btcmap {
    fn id(&self) -> &'static str {
        "btcmap"
    }
    fn applies(&self, subject: &Subject) -> bool {
        crate::resolver::osm::parse_id(&subject.uri).is_some()
    }
    fn ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(7 * 86_400)
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let Some(osm) = crate::resolver::osm::parse_id(&subject.uri) else {
            return Ok(also_on::Found::default());
        };
        let mut index = BTCMAP.lock().await;
        index.sync(cx).await?;
        if index.synced.is_none() {
            // The first sync is still under way: come back for the rest.
            return Err(RetryLater {
                class: "btcmap_sync",
                seconds: 60,
            }
            .into());
        }
        let place = places::BtcmapPlace {
            id: 0,
            osm: Some(osm.clone()),
            updated_at: String::new(),
            deleted: false,
        };
        let Some(link) = index
            .places
            .contains(&osm.key())
            .then(|| place.link())
            .flatten()
        else {
            return Ok(also_on::Found::default());
        };
        let key = places::key(&osm);
        let mut found = super::Found {
            keys: vec![key.clone()],
            links: vec![link],
        }
        .into_also_on("btcmap");
        found.links = found
            .links
            .into_iter()
            .map(|l| l.keyed(key.as_str()))
            .collect();
        Ok(found)
    }
}

/// The DOI of a card, unless it is arXiv's own (`10.48550/arXiv.…`, read as an arXiv paper).
fn journal_doi(subject: &Subject) -> Option<String> {
    let doi = crate::resolver::cards::doi(&subject.uri)?;
    (!doi.starts_with("10.48550/")).then_some(doi)
}
/// The arXiv paper of a card: its arXiv, alphaXiv or arXiv DOI link, or a key another source
/// found for it.
fn arxiv(subject: &Subject) -> Option<String> {
    super::papers::arxiv_from_uri(&subject.uri)
        .or_else(|| subject.keys_in("arxiv").find_map(super::papers::arxiv_id))
}

/// Open copies of a paper from Unpaywall (links, never keys). Needs the contact setting.
pub struct Unpaywall;
#[async_trait]
impl Source for Unpaywall {
    fn id(&self) -> &'static str {
        "unpaywall"
    }
    fn applies(&self, subject: &Subject) -> bool {
        journal_doi(subject).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let (Some(doi), Some(contact)) = (journal_doi(subject), contact(cx)) else {
            return Ok(also_on::Found::default());
        };
        let Some(request) = super::papers::unpaywall_request(&doi, contact) else {
            return Ok(also_on::Found::default());
        };
        let f = send(cx, &request, "unpaywall_busy").await?;
        let links = super::papers::parse_unpaywall(f.status, &f.body, &doi)?;
        Ok(super::Found {
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
    fn id(&self) -> &'static str {
        "ncbi"
    }
    fn applies(&self, subject: &Subject) -> bool {
        journal_doi(subject).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let (Some(doi), Some(contact)) = (journal_doi(subject), contact(cx)) else {
            return Ok(also_on::Found::default());
        };
        let Some(request) = super::papers::ncbi_request(&[&doi], contact) else {
            return Ok(also_on::Found::default());
        };
        let f = send(cx, &request, "ncbi_busy").await?;
        Ok(super::papers::parse_ncbi(f.status, &f.body)?
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
        let Some(request) = super::papers::crossref_request(doi) else {
            return Ok(false);
        };
        let f = send(cx, &request, "crossref_busy").await?;
        super::papers::parse_crossref_preprint(f.status, &f.body, doi, arxiv)
    }
}
#[async_trait]
impl Source for Datacite {
    fn id(&self) -> &'static str {
        "datacite"
    }
    fn applies(&self, subject: &Subject) -> bool {
        journal_doi(subject).is_some() || super::papers::arxiv_from_uri(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        use super::papers;
        if let Some(id) = papers::arxiv_from_uri(&subject.uri) {
            let Some(request) = papers::datacite_arxiv_request(&id) else {
                return Ok(also_on::Found::default());
            };
            let f = send(cx, &request, "datacite_busy").await?;
            let (mut found, journal) = papers::parse_datacite_arxiv(f.status, &f.body, &id)?;
            if let Some(journal) = journal {
                if Self::publisher_names(cx, &journal, &id).await? {
                    found.push_key(papers::doi_key(&journal));
                }
            }
            for link in papers::arxiv_links(&id, "datacite") {
                found.push_link(link);
            }
            return Ok(found.into_also_on("datacite"));
        }
        let Some(doi) = journal_doi(subject) else {
            return Ok(also_on::Found::default());
        };
        let Some(request) = papers::datacite_versions_request(&doi) else {
            return Ok(also_on::Found::default());
        };
        let f = send(cx, &request, "datacite_busy").await?;
        let (mut found, preprint) = papers::parse_datacite_versions(f.status, &f.body, &doi)?;
        if let Some(id) = preprint {
            if Self::publisher_names(cx, &doi, &id).await? {
                found.push_key(papers::arxiv_key(&id));
            }
        }
        Ok(found.into_also_on("datacite"))
    }
}

/// The Hugging Face Papers page of an arXiv paper, only when its API says the page exists.
pub struct HuggingFacePapers;
#[async_trait]
impl Source for HuggingFacePapers {
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
        let Some(request) = super::papers::hugging_face_request(&id) else {
            return Ok(also_on::Found::default());
        };
        let f = send(cx, &request, "huggingface_busy").await?;
        let links = super::papers::parse_hugging_face(f.status, &f.body, &id)?
            .into_iter()
            .collect();
        Ok(super::Found {
            keys: vec![],
            links,
        }
        .into_also_on("huggingface"))
    }
}

/// A package on crates.io, npm or PyPI: its pages on the registry and documentation sites, its
/// name as key, and its source repository only when signed build provenance names it.
pub struct Packages(pub super::packages::Registry);
impl Packages {
    fn package(&self, subject: &Subject) -> Option<super::packages::Package> {
        super::packages::Package::from_uri(&subject.uri).filter(|p| p.registry == self.0)
    }
    /// deps.dev's `SLSA_ATTESTATION` source repository, when the registry's own record names
    /// none.
    async fn depsdev(
        cx: &Context,
        package: &super::packages::Package,
        version: &str,
    ) -> Result<Option<super::packages::Repo>> {
        let Some(request) = super::packages::depsdev_request(package, version) else {
            return Ok(None);
        };
        let f = send(cx, &request, "depsdev_busy").await?;
        super::packages::parse_depsdev(f.status, &f.body, package, version)
    }
}
#[async_trait]
impl Source for Packages {
    fn id(&self) -> &'static str {
        match self.0 {
            super::packages::Registry::Crates => "crates",
            super::packages::Registry::Npm => "npm",
            super::packages::Registry::PyPI => "pypi",
        }
    }
    fn applies(&self, subject: &Subject) -> bool {
        self.package(subject).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        use super::packages::{self as p, Registry};
        let Some(package) = self.package(subject) else {
            return Ok(also_on::Found::default());
        };
        let release = match self.0 {
            Registry::Crates => {
                let Some(request) = p::crates_request(&package) else {
                    return Ok(also_on::Found::default());
                };
                let f = send(cx, &request, "crates_busy").await?;
                p::parse_crates(f.status, &f.body, &package)?
            }
            Registry::Npm => {
                let Some(request) = p::npm_latest_request(&package) else {
                    return Ok(also_on::Found::default());
                };
                let f = send(cx, &request, "npm_busy").await?;
                match p::parse_npm_latest(f.status, &f.body, &package)? {
                    None => None,
                    Some(latest) => {
                        let repository = match &latest.attestations {
                            Some(request) => {
                                let f = send(cx, request, "npm_busy").await?;
                                p::parse_npm_attestations(
                                    f.status,
                                    &f.body,
                                    &package,
                                    &latest.version,
                                )?
                            }
                            None => None,
                        };
                        Some(p::Release {
                            package: package.clone(),
                            version: latest.version,
                            repository,
                        })
                    }
                }
            }
            Registry::PyPI => {
                let Some(request) = p::pypi_request(&package) else {
                    return Ok(also_on::Found::default());
                };
                let f = send(cx, &request, "pypi_busy").await?;
                match p::parse_pypi(f.status, &f.body, &package)? {
                    None => None,
                    Some(latest) => {
                        // The files' provenance names the publisher; files that name two
                        // repositories name none.
                        let mut named = vec![];
                        for file in latest.files.iter().take(2) {
                            let Some(request) = p::pypi_provenance_request(&latest, file) else {
                                continue;
                            };
                            let f = send(cx, &request, "pypi_busy").await?;
                            named.extend(p::parse_pypi_provenance(f.status, &f.body)?);
                        }
                        let repository = p::one_repository(named);
                        Some(p::Release {
                            package: latest.package.clone(),
                            version: latest.version,
                            repository,
                        })
                    }
                }
            }
        };
        let Some(mut release) = release else {
            return Ok(also_on::Found::default());
        };
        if release.repository.is_none() {
            release.repository = Self::depsdev(cx, &release.package, &release.version).await?;
        }
        Ok(release.found().into_also_on(self.id()))
    }
}

/// A repository on GitHub or GitLab that Software Heritage archives: its archive page.
pub struct SoftwareHeritage;
#[async_trait]
impl Source for SoftwareHeritage {
    fn id(&self) -> &'static str {
        "software_heritage"
    }
    fn applies(&self, subject: &Subject) -> bool {
        super::packages::Repo::from_url(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let Some(repo) = super::packages::Repo::from_url(&subject.uri) else {
            return Ok(also_on::Found::default());
        };
        let Some(request) = super::packages::software_heritage_request(&repo) else {
            return Ok(also_on::Found::default());
        };
        let f = send(cx, &request, "software_heritage_busy").await?;
        let links = super::packages::parse_software_heritage(f.status, &f.body, &repo)?
            .into_iter()
            .collect();
        Ok(super::Found {
            keys: vec![repo.key()],
            links,
        }
        .into_also_on("software_heritage"))
    }
}

/// A Steam game: SteamDB (from the appid alone), ProtonDB (when its summary exists),
/// PCGamingWiki (where its appid lookup leads) and GOG (through GOG GamesDB).
pub struct Steam;
#[async_trait]
impl Source for Steam {
    fn id(&self) -> &'static str {
        "steam"
    }
    fn applies(&self, subject: &Subject) -> bool {
        super::games::appid_from_uri(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        use super::games;
        let Some(appid) = games::appid_from_uri(&subject.uri) else {
            return Ok(also_on::Found::default());
        };
        let mut found = super::Found::default();
        found.push_key(games::steam_key(appid));
        for link in games::templates(appid) {
            found.push_link(link);
        }
        if let Some(request) = games::protondb_request(appid) {
            let f = send(cx, &request, "protondb_busy").await?;
            if let Some(link) = games::parse_protondb(f.status, &f.body, appid)? {
                found.push_link(link);
            }
        }
        if let Some(request) = games::pcgamingwiki_request(appid) {
            // Where the lookup redirects is the page the appid names. A wiki that fails (a bot
            // wall, busy) costs only its own link.
            let link = match send(cx, &request, "pcgamingwiki_busy").await {
                Ok(f) => games::parse_pcgamingwiki(f.status, f.location.as_deref()),
                Err(error) => Err(error),
            };
            match link {
                Ok(Some(link)) => found.push_link(link),
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(appid, error = %format!("{error:#}"), "PCGamingWiki lookup failed");
                }
            }
        }
        if let Some(request) = games::gog_gamesdb_request(appid) {
            let f = send(cx, &request, "gog_busy").await?;
            if let Some(product) = games::parse_gog_gamesdb(f.status, &f.body, appid)? {
                found.push_key(games::gog_key(product));
                if let Some(request) = games::gog_product_request(product) {
                    let f = send(cx, &request, "gog_busy").await?;
                    if let Some(link) = games::parse_gog_product(f.status, &f.body, product)? {
                        found.push_link(link);
                    }
                }
            }
        }
        Ok(found.into_also_on("steam"))
    }
}

/// The Wikidata property a store link names, its value, and the stores to list for the item.
pub fn store_property(uri: &str) -> Option<(&'static str, String, &'static [items::Property])> {
    let url = super::web_url(uri)?;
    let host = url.host_str()?.trim_start_matches("www.");
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let package =
        |v: &str| super::plain(v, 200, |c| c.is_ascii_alphanumeric() || "._-".contains(c));
    let (pid, value, set) = match (host, segments.as_slice()) {
        // The game's own store page only (`games::appid_from_uri`), never a page under it.
        ("store.steampowered.com", ["app", ..]) => (
            "P1733",
            super::games::appid_from_uri(uri)?.to_string(),
            items::GAME,
        ),
        ("gog.com", [lang, "game", slug]) if lang.len() == 2 => {
            ("P2725", format!("game/{slug}"), items::GAME)
        }
        ("gog.com", ["game", slug]) => ("P2725", format!("game/{slug}"), items::GAME),
        ("apps.apple.com", rest) => {
            let id = rest.last()?.strip_prefix("id")?;
            (super::plain(id, 20, |c| c.is_ascii_digit()) && rest.contains(&"app")).then_some(())?;
            ("P3861", id.to_owned(), items::APP)
        }
        ("play.google.com", ["store", "apps", "details"]) => ("P3418", query("id")?, items::APP),
        ("f-droid.org", [.., "packages", id]) => ("P3597", (*id).to_owned(), items::APP),
        ("flathub.org", ["apps", "details", id]) | ("flathub.org", ["apps", id]) => {
            ("P4655", (*id).to_owned(), items::APP)
        }
        ("snapcraft.io", [id]) => ("P4435", (*id).to_owned(), items::APP),
        _ => return None,
    };
    (pid == "P2725" || pid == "P3861" || pid == "P1733" || package(&value))
        .then_some((pid, value, set))
}

/// Whether `value` is the item's only best-rank value of `pid`: an item with two appids is not
/// one Steam page's.
fn only_value(item: &Value, pid: &str, value: &str) -> bool {
    let mut values = items::best_values(item, pid);
    values.sort_unstable();
    values.dedup();
    values == [value]
}

/// An app or game on a store: the one Wikidata item carrying that store's ID (read back and
/// compared exactly; several are ambiguous and give nothing), its other stores and game
/// databases, and its Q-id as key when the item carries no other ID on that store.
pub struct WikidataStores;
#[async_trait]
impl Source for WikidataStores {
    fn id(&self) -> &'static str {
        "wikidata_stores"
    }
    fn applies(&self, subject: &Subject) -> bool {
        store_property(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let Some((pid, value, set)) = store_property(&subject.uri) else {
            return Ok(also_on::Found::default());
        };
        // An answer over its read's cap is no answer: asking again would read the same megabytes.
        let found = match hubs::wikidata::item_with(&cx.egress, pid, &value).await {
            Err(error) if crate::resolver::egress::over_cap(&error) => {
                return Ok(also_on::Found::default())
            }
            found => found?,
        };
        let hubs::wikidata::Found::One(hub) = found else {
            return Ok(also_on::Found::default());
        };
        let Some((raw, _)) = item(cx, &hub.qid).await? else {
            return Ok(also_on::Found::default());
        };
        let mut found = super::Found::default();
        if only_value(&raw, pid, &value) {
            found.push_key(items::key(&hub.qid));
        }
        for link in items::links(&raw, set) {
            found.push_link(link);
        }
        Ok(found.into_also_on("wikidata"))
    }
}

/// A page that is gone or unreachable: its Wayback Machine capture (HTTP 200 only) closest to
/// when it was first referenced, with that time.
pub struct Wayback;
#[async_trait]
impl Source for Wayback {
    fn id(&self) -> &'static str {
        "wayback"
    }
    fn applies(&self, subject: &Subject) -> bool {
        matches!(subject.status.as_str(), "missing" | "unreachable")
            && super::web_url(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let near = subject.first_seen_at.unwrap_or_else(chrono::Utc::now);
        let Some(request) = super::archive::wayback_request(&subject.uri, near) else {
            return Ok(also_on::Found::default());
        };
        let f = send(cx, &request, "wayback_busy").await?;
        let links = super::archive::parse_wayback(f.status, &f.body, &subject.uri, near)?
            .into_iter()
            .collect();
        Ok(super::Found {
            keys: vec![],
            links,
        }
        .into_also_on("wayback"))
    }
}

/// A Discogs link's own ID as the key `discogs:{kind}:{id}`, and a release's barcodes from its
/// record (the card's request, shared) as `upc:` keys and facts.
pub struct Discogs;
#[async_trait]
impl Source for Discogs {
    fn id(&self) -> &'static str {
        "discogs"
    }
    fn applies(&self, subject: &Subject) -> bool {
        discogs::of_uri(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
        let Some((kind, id)) = discogs::of_uri(&subject.uri) else {
            return Ok(also_on::Found::default());
        };
        let key = |key: String| also_on::Key {
            key,
            via: "discogs".into(),
        };
        let mut found = also_on::Found {
            keys: vec![key(format!("discogs:{}:{id}", kind.as_str()))],
            ..Default::default()
        };
        if kind == discogs::Kind::Release {
            let token = discogs::token(&cx.config);
            if let (Some(v), _) = discogs::record(&cx.egress, kind, id, token.as_deref()).await? {
                let barcodes = discogs::barcodes(&v);
                // A box set lists each disc's barcode: only a release with one barcode keys it.
                if let [upc] = barcodes.as_slice() {
                    found.keys.push(key(format!("upc:{upc}")));
                }
                for upc in barcodes.into_iter().take(4) {
                    found.facts.push(also_on::Fact::new("upc", upc));
                }
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lookups::fixture::read, model::ResourceMeta};

    const JSON: &str = "application/json";
    fn entity(q: &str) -> String {
        hubs::wikidata::entities_lookup(&[q.to_owned()])
    }
    fn subject(uri: &str, wikidata: Option<&str>) -> Subject {
        let mut doc = ResourceMeta::new(uri, "osm", 3600);
        if let Some(q) = wikidata {
            doc.attributes.insert("wikidata".into(), q.into());
        }
        Subject {
            id: "x".into(),
            uri: uri.into(),
            status: "ok".into(),
            type_name: Some("Place".into()),
            doc: Some(doc),
            keys: vec![],
            first_seen_at: None,
        }
    }
    fn context(routes: Vec<(&str, u16, &'static str, Vec<u8>)>) -> Context {
        let (egress, _) = crate::sources::fixture::egress(routes);
        Context {
            egress,
            config: Default::default(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_tagged_osm_object_named_back_by_its_item() {
        let cx = context(vec![(
            &entity("Q64"),
            200,
            JSON,
            read("wikidata-q64.json.gz"),
        )]);
        let node = subject("https://www.openstreetmap.org/node/240109189", Some("Q64"));
        assert!(OsmWikidata.applies(&node));
        let found = OsmWikidata.lookup(&cx, &node).await.unwrap();
        // Berlin's item names this node and relation 62422 (P402): not one to one, so links
        // only, and neither object joins the item.
        assert!(found.keys.is_empty());
        let sites: Vec<&str> = found.links.iter().map(|l| l.site.as_str()).collect();
        assert_eq!(
            &sites[..6],
            [
                "Wikidata",
                "Official website",
                "Google Maps",
                "Apple Maps",
                "Tripadvisor",
                "Wikipedia (en)"
            ]
        );
        assert_eq!(found.links[0].uri, "https://www.wikidata.org/wiki/Q64");
        assert_eq!(found.links[1].uri, "https://www.berlin.de/");
        assert_eq!(sites[6], "Wikipedia (de)");
        assert!(found.links.iter().all(|l| l.via == "wikidata"));

        let relation = subject("https://www.openstreetmap.org/relation/62422", Some("Q64"));
        let found = OsmWikidata.lookup(&cx, &relation).await.unwrap();
        assert!(found.keys.is_empty());
        assert_eq!(found.links[0].uri, "https://www.wikidata.org/wiki/Q64");

        // An object tagged with an item that does not name it back: nothing.
        let other = subject("https://www.openstreetmap.org/node/1", Some("Q64"));
        assert_eq!(
            OsmWikidata.lookup(&cx, &other).await.unwrap(),
            Default::default()
        );
        // Untagged, or a tag that names no single item: the source does not apply.
        assert!(!OsmWikidata.applies(&subject("https://www.openstreetmap.org/node/1", None)));
        assert!(!OsmWikidata.applies(&subject(
            "https://www.openstreetmap.org/node/1",
            Some("Q64;Q90")
        )));
        assert!(!OsmWikidata.applies(&subject("https://example.org/", Some("Q64"))));
    }

    #[tokio::test(start_paused = true)]
    async fn an_item_or_its_article_links_its_osm_object() {
        let wiki = "https://en.wikipedia.org/w/api.php?action=query&prop=pageprops&ppprop=wikibase_item&titles=Paris&redirects=1&format=json";
        let cx = context(vec![
            (&entity("Q90"), 200, JSON, read("wikidata-q90.json.gz")),
            (&entity("Q64"), 200, JSON, read("wikidata-q64.json.gz")),
            (wiki, 200, JSON, read("enwiki-pageprops-paris.json")),
        ]);
        for (uri, osm) in [
            (
                "https://www.wikidata.org/wiki/Q90",
                "https://www.openstreetmap.org/relation/7444",
            ),
            (
                "https://en.wikipedia.org/wiki/Paris",
                "https://www.openstreetmap.org/relation/7444",
            ),
            // Berlin names a node and a relation: the relation (P402) is the object.
            (
                "https://www.wikidata.org/entity/Q64",
                "https://www.openstreetmap.org/relation/62422",
            ),
        ] {
            let item = subject(uri, None);
            assert!(WikidataOsm.applies(&item), "{uri}");
            let found = WikidataOsm.lookup(&cx, &item).await.unwrap();
            assert_eq!(found.links.len(), 1, "{uri}");
            assert_eq!(
                (found.links[0].uri.as_str(), found.links[0].site.as_str()),
                (osm, "OpenStreetMap")
            );
            assert!(found.keys.is_empty());
        }
        // A key another source found is enough.
        let mut keyed = subject("https://example.org/paris", None);
        keyed.keys.push(("wikidata:Q90".into(), "wikipedia".into()));
        assert!(WikidataOsm.applies(&keyed));
        assert_eq!(
            WikidataOsm.lookup(&cx, &keyed).await.unwrap().links.len(),
            1
        );
        // An OSM object itself is the other source's.
        assert!(!WikidataOsm.applies(&subject(
            "https://www.openstreetmap.org/node/240109189",
            Some("Q64")
        )));
        assert!(!WikidataOsm.applies(&subject("https://example.org/", None)));
    }

    /// Every recorded lookup answer, by its request URL (a redirect's body is its `Location`),
    /// plus `extra` routes for requests made differently from the recording.
    fn recorded(extra: Vec<(String, &str)>) -> Context {
        recorded_but_busy(extra, &[])
    }
    /// As `recorded`, with `busy` URLs answering 503.
    fn recorded_but_busy(extra: Vec<(String, &str)>, busy: &[&str]) -> Context {
        let provenance: Value =
            serde_json::from_slice(&read("provenance.json")).expect("provenance");
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let mut routes: Vec<(String, u16, &'static str, Vec<u8>)> = vec![];
        for entry in provenance.as_object().unwrap().values() {
            let status = entry["status"].as_u64().unwrap() as u16;
            let kind = leak(
                entry["content_type"]
                    .as_str()
                    .unwrap_or("text/html")
                    .to_owned(),
            );
            let body = match (entry["location"].as_str(), entry["file"].as_str()) {
                (Some(location), _) => location.as_bytes().to_vec(),
                (None, Some(file)) => read(file),
                (None, None) => vec![],
            };
            routes.push((
                entry["url"].as_str().unwrap().to_owned(),
                status,
                kind,
                body,
            ));
        }
        for (url, like) in extra {
            let entry = &provenance[like];
            let kind = leak(entry["content_type"].as_str().unwrap().to_owned());
            routes.push((
                url,
                entry["status"].as_u64().unwrap() as u16,
                kind,
                read(entry["file"].as_str().unwrap()),
            ));
        }
        for url in busy {
            routes.push((url.to_string(), 503, "text/html", vec![]));
        }
        context(
            routes
                .iter()
                .map(|(u, s, k, b)| (u.as_str(), *s, *k, b.clone()))
                .collect(),
        )
    }
    fn at(uri: &str) -> Subject {
        Subject {
            doc: None,
            ..subject(uri, None)
        }
    }
    fn sites(found: &also_on::Found) -> Vec<String> {
        found
            .links
            .iter()
            .map(|l| format!("{} {}", l.site, l.uri))
            .collect()
    }
    fn keys(found: &also_on::Found) -> Vec<&str> {
        found.keys.iter().map(|k| k.key.as_str()).collect()
    }

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
        assert!(keys(&preprint).is_empty());
        assert!(sites(&preprint).contains(&"arXiv https://arxiv.org/abs/1304.1068".to_owned()));
        let arxiv = at("https://arxiv.org/abs/1304.1068v2");
        assert!(!Unpaywall.applies(&arxiv) && Datacite.applies(&arxiv));
        let journal = Datacite.lookup(&cx, &arxiv).await.unwrap();
        assert_eq!(keys(&journal), ["arxiv:1304.1068"]);
        let journal_sites = sites(&journal);
        assert!(
            journal_sites.contains(&"alphaXiv https://www.alphaxiv.org/abs/1304.1068".to_owned())
        );
        assert!(journal_sites.contains(&"DOI https://doi.org/10.1038/nature12373".to_owned()));

        // LMCS names arXiv 2302.04668 as its preprint (has-preprint): both sides share the keys.
        let lmcs = Datacite
            .lookup(&cx, &at("doi:10.46298/lmcs-20(1:8)2024"))
            .await
            .unwrap();
        assert_eq!(keys(&lmcs), ["arxiv:2302.04668"]);
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

    #[tokio::test(start_paused = true)]
    async fn packages() {
        use crate::lookups::packages::Registry;
        let cx = recorded(vec![]);
        let crates = Packages(Registry::Crates);
        let rand = at("https://crates.io/crates/rand");
        assert!(crates.applies(&rand) && !Packages(Registry::Npm).applies(&rand));
        let found = crates.lookup(&cx, &rand).await.unwrap();
        assert!(keys(&found).contains(&"crate:rand"), "{:?}", keys(&found));
        assert!(sites(&found).contains(&"Docs.rs https://docs.rs/rand".to_owned()));
        // rand's provenance (trusted publishing, or deps.dev's SLSA attestation) names its
        // repository: a link, never a key (rand_core and rand_chacha come from it too).
        assert_eq!(keys(&found), ["crate:rand"]);
        assert!(
            sites(&found).contains(&"GitHub https://github.com/rust-random/rand".to_owned()),
            "{:?}",
            sites(&found)
        );
        assert!(found.links.iter().all(|l| l.via == "crates"));

        let semver = Packages(Registry::Npm)
            .lookup(&cx, &at("https://www.npmjs.com/package/semver"))
            .await
            .unwrap();
        assert_eq!(keys(&semver), ["npm:semver"]);
        assert!(
            sites(&semver).contains(&"GitHub https://github.com/npm/node-semver".to_owned()),
            "{:?}",
            sites(&semver)
        );
        // left-pad publishes no provenance: no repository.
        let left_pad = Packages(Registry::Npm)
            .lookup(&cx, &at("https://www.npmjs.com/package/left-pad"))
            .await
            .unwrap();
        assert_eq!(keys(&left_pad), ["npm:left-pad"]);

        let urllib3 = Packages(Registry::PyPI)
            .lookup(&cx, &at("https://pypi.org/project/urllib3/"))
            .await
            .unwrap();
        assert_eq!(keys(&urllib3), ["pypi:urllib3"]);
        assert!(
            sites(&urllib3).contains(&"GitHub https://github.com/urllib3/urllib3".to_owned()),
            "{:?}",
            sites(&urllib3)
        );

        // The repository itself, on Software Heritage (120 requests an hour: one every 30 s).
        let repo = at("https://github.com/rust-random/rand");
        assert!(SoftwareHeritage.applies(&repo));
        let archived = SoftwareHeritage.lookup(&cx, &repo).await.unwrap();
        assert_eq!(keys(&archived), ["repo:github.com/rust-random/rand"]);
        assert_eq!(archived.links.len(), 1);
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        let missing = SoftwareHeritage
            .lookup(&cx, &at("https://github.com/pubky/pinakes-no-such-repo"))
            .await
            .unwrap();
        assert!(missing.links.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn games_and_stores() {
        let cx = recorded(vec![]);
        let portal = at("https://store.steampowered.com/app/620/Portal_2/");
        let found = Steam.lookup(&cx, &portal).await.unwrap();
        assert!(keys(&found).contains(&"steam:620"));
        let s = sites(&found);
        assert!(s.iter().any(|s| s.starts_with("SteamDB ")), "{s:?}");
        assert!(s.iter().any(|s| s.starts_with("ProtonDB ")), "{s:?}");
        assert!(
            s.contains(&"PCGamingWiki https://www.pcgamingwiki.com/wiki/Portal_2".to_owned()),
            "{s:?}"
        );
        // A busy wiki costs only its own link.
        let busy = recorded_but_busy(
            vec![],
            &["https://www.pcgamingwiki.com/api/appid.php?appid=620"],
        );
        let without = sites(&Steam.lookup(&busy, &portal).await.unwrap());
        assert!(
            without.iter().any(|s| s.starts_with("ProtonDB ")),
            "{without:?}"
        );
        assert!(!without.iter().any(|s| s.starts_with("PCGamingWiki ")));
        // An appid nobody knows: the templates only.
        let none = Steam
            .lookup(&cx, &at("https://store.steampowered.com/app/1/"))
            .await
            .unwrap();
        assert!(
            sites(&none).iter().all(|s| s.starts_with("Steam")),
            "{:?}",
            sites(&none)
        );

        // Through Wikidata: the one item with the store's ID, its other stores and databases.
        assert!(WikidataStores.applies(&portal));
        let item = WikidataStores.lookup(&cx, &portal).await.unwrap();
        assert_eq!(keys(&item), ["wikidata:Q279446"]);
        let s = sites(&item);
        assert!(
            s.contains(&"PCGamingWiki https://www.pcgamingwiki.com/wiki/Portal_2".to_owned()),
            "{s:?}"
        );
        assert!(
            s.contains(&"IGDB https://www.igdb.com/games/portal-2".to_owned()),
            "{s:?}"
        );
        // MobyGames names two games: ambiguous, left out.
        assert!(!s.iter().any(|s| s.starts_with("MobyGames")), "{s:?}");
        let app = at("https://play.google.com/store/apps/details?id=de.danoeh.antennapod&hl=en");
        let item = WikidataStores.lookup(&cx, &app).await.unwrap();
        assert_eq!(keys(&item), ["wikidata:Q16927599"]);
        assert!(
            sites(&item)
                .contains(&"F-Droid https://f-droid.org/packages/de.danoeh.antennapod".to_owned()),
            "{:?}",
            sites(&item)
        );
    }

    #[test]
    fn store_links() {
        for (uri, pid, value) in [
            (
                "https://store.steampowered.com/app/620/Portal_2/",
                "P1733",
                "620",
            ),
            (
                "https://www.gog.com/en/game/the_witcher_3_wild_hunt",
                "P2725",
                "game/the_witcher_3_wild_hunt",
            ),
            (
                "https://apps.apple.com/us/app/signal-private-messenger/id874139669",
                "P3861",
                "874139669",
            ),
            (
                "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms",
                "P3418",
                "org.thoughtcrime.securesms",
            ),
            (
                "https://f-droid.org/en/packages/de.danoeh.antennapod/",
                "P3597",
                "de.danoeh.antennapod",
            ),
            (
                "https://flathub.org/apps/org.mozilla.firefox",
                "P4655",
                "org.mozilla.firefox",
            ),
            ("https://snapcraft.io/firefox", "P4435", "firefox"),
        ] {
            let (p, v, _) = store_property(uri).unwrap_or_else(|| panic!("{uri}"));
            assert_eq!((p, v.as_str()), (pid, value), "{uri}");
        }
        for uri in [
            "https://play.google.com/store/apps/details",
            "https://play.google.com/store/apps/details?id=a%20b",
            "https://apps.apple.com/us/app/x/idabc",
            "https://snapcraft.io/docs/x",
            "https://example.org/app/1",
        ] {
            assert!(store_property(uri).is_none(), "{uri}");
        }
    }

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
                &"GovInfo https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291".to_owned()
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
        // A GovInfo copy someone tags shares the key, from its address alone.
        let copy = at("https://www.govinfo.gov/content/pkg/FR-2026-09-09/pdf/2026-18291.pdf");
        assert!(FederalRegister.applies(&copy));
        let found = FederalRegister.lookup(&cx, &copy).await.unwrap();
        assert_eq!(
            (keys(&found), found.links.len()),
            (vec!["fr:2026-18291"], 0)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn archived_copies_and_btcmap() {
        let near: chrono::DateTime<chrono::Utc> = "2015-06-01T00:00:00Z".parse().unwrap();
        let cx = recorded(vec![
            (
                places::btcmap_sync_request("2010-01-01T00:00:00Z", BTCMAP_PAGE)
                    .unwrap()
                    .url
                    .into(),
                "btcmap-places",
            ),
            (
                crate::lookups::archive::wayback_request("https://example.com/", near)
                    .unwrap()
                    .url
                    .into(),
                "wayback-cdx-example",
            ),
        ]);
        let mut gone = at("https://example.com/");
        gone.status = "missing".into();
        gone.first_seen_at = Some(near);
        assert!(Wayback.applies(&gone));
        let found = Wayback.lookup(&cx, &gone).await.unwrap();
        assert_eq!(found.links.len(), 1);
        let link = &found.links[0];
        assert_eq!(
            (link.kind.as_deref(), link.via.as_str()),
            (Some("archived"), "wayback")
        );
        assert!(link.at.is_some() && link.uri.starts_with("https://web.archive.org/web/"));
        // A page that answers is not offered an archived copy.
        assert!(!Wayback.applies(&at("https://example.com/")));

        let places: Vec<places::BtcmapPlace> =
            places::parse_btcmap_places(200, &read("btcmap-places.json")).unwrap();
        let listed = places
            .iter()
            .find(|p| !p.deleted && p.osm.is_some())
            .unwrap();
        let osm = listed.osm.as_ref().unwrap();
        let uri = format!("https://www.openstreetmap.org/{}/{}", osm.osm_type, osm.id);
        let found = Btcmap.lookup(&cx, &at(&uri)).await.unwrap();
        assert_eq!(found.links.len(), 1, "{uri}");
        // The merchant page names this one object: it and the object carry the object's key.
        let key = format!("osm:{}/{}", osm.osm_type, osm.id);
        assert_eq!(keys(&found), [key.as_str()]);
        assert_eq!(
            (found.links[0].uri.clone(), found.links[0].key.clone()),
            (
                format!("https://btcmap.org/merchant/{}:{}", osm.osm_type, osm.id),
                Some(key.clone())
            )
        );
        let found = Btcmap
            .lookup(&cx, &at("https://www.openstreetmap.org/node/1"))
            .await
            .unwrap();
        assert!(found.links.is_empty() && found.keys.is_empty());
    }

    #[test]
    fn the_one_osm_object() {
        let item = |claims: Value| serde_json::json!({ "claims": claims });
        let claim = |v: &str| serde_json::json!({"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": v}}});
        assert_eq!(
            osm_object(&item(serde_json::json!({"P11693": [claim("5")]}))),
            Some(("node", "5".into()))
        );
        assert_eq!(
            osm_object(&item(
                serde_json::json!({"P402": [claim("7")], "P11693": [claim("5")]})
            )),
            Some(("relation", "7".into()))
        );
        // Two of a kind, or two kinds without a relation: no single object.
        assert_eq!(
            osm_object(&item(serde_json::json!({"P402": [claim("7"), claim("8")]}))),
            None
        );
        assert_eq!(
            osm_object(&item(
                serde_json::json!({"P11693": [claim("5")], "P10689": [claim("6")]})
            )),
            None
        );
        assert_eq!(
            osm_object(&item(serde_json::json!({"P11693": [claim("05")]}))),
            None
        );
        assert_eq!(
            wikidata_item("https://www.wikidata.org/wiki/Q90").as_deref(),
            Some("Q90")
        );
        assert_eq!(
            wikidata_item("https://m.wikidata.org/wiki/Q90").as_deref(),
            Some("Q90")
        );
        assert_eq!(
            wikidata_item("https://www.wikidata.org/wiki/Property:P402"),
            None
        );
        assert_eq!(wikidata_item("https://www.wikidata.org/wiki/Q090"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn discogs_keys_its_id_and_a_releases_barcodes() {
        let release = crate::sources::fixture::read("discogs", "release-83182.json");
        // The same release listing a second disc's barcode, as a box set does.
        let mut boxed: Value = serde_json::from_slice(&release).unwrap();
        boxed["id"] = 1.into();
        boxed["identifiers"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "Barcode", "value": "0724384260521"}));
        let cx = context(vec![
            ("https://api.discogs.com/releases/83182", 200, JSON, release),
            (
                "https://api.discogs.com/releases/1",
                200,
                JSON,
                serde_json::to_vec(&boxed).unwrap(),
            ),
        ]);
        let keys = |found: also_on::Found| -> Vec<String> {
            found.keys.into_iter().map(|k| k.key).collect()
        };
        let mut s = subject("https://www.discogs.com/release/83182", None);
        assert!(Discogs.applies(&s));
        assert_eq!(
            keys(Discogs.lookup(&cx, &s).await.unwrap()),
            ["discogs:release:83182", "upc:0724385522925"]
        );
        // Two barcodes: facts, no key, so a box set never joins its discs' albums.
        s.uri = "https://www.discogs.com/release/1".into();
        let found = Discogs.lookup(&cx, &s).await.unwrap();
        assert_eq!(found.facts.len(), 2);
        assert_eq!(keys(found), ["discogs:release:1"]);
        // A master's or artist's key needs no request.
        s.uri = "https://www.discogs.com/master/21491".into();
        assert_eq!(
            keys(Discogs.lookup(&cx, &s).await.unwrap()),
            ["discogs:master:21491"]
        );
        s.uri = "https://www.discogs.com/label/1".into();
        assert!(!Discogs.applies(&s));
    }

    #[test]
    fn a_page_under_a_game_names_no_store_item() {
        assert_eq!(
            store_property("https://store.steampowered.com/app/620/Portal_2/")
                .map(|(p, v, _)| (p, v)),
            Some(("P1733", "620".to_owned()))
        );
        for uri in [
            "https://store.steampowered.com/app/620/Portal_2/reviews/",
            "https://store.steampowered.com/app/620/Portal_2/news/1",
        ] {
            assert!(store_property(uri).is_none(), "{uri}");
            assert!(!Steam.applies(&at(uri)), "{uri}");
        }
        assert!(!Steam.applies(&at(
            "https://steamcommunity.com/app/620/discussions/0/123456789/"
        )));
    }

    #[test]
    fn an_item_with_two_appids_is_neither_page() {
        use serde_json::json;
        let claim = |v: &str| json!({"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": v}}});
        let one = json!({"claims": {"P1733": [claim("400"), claim("400")]}});
        let two = json!({"claims": {"P1733": [claim("400"), claim("620")]}});
        assert!(only_value(&one, "P1733", "400"));
        assert!(!only_value(&two, "P1733", "400"));
        assert!(!only_value(&two, "P1733", "620"));
        assert!(!only_value(&one, "P1733", "620"));
    }

    /// A full page is never the last: places that changed at one instant past a page come on a
    /// larger page, and the sync then goes past that instant.
    #[tokio::test(start_paused = true)]
    async fn btcmap_sync_reads_past_a_full_page_of_one_instant() {
        let at = "2026-10-10T00:00:00.000Z";
        let rows = |ids: std::ops::Range<u64>, at: &str| {
            serde_json::to_vec(
                &ids.map(|id| serde_json::json!({"id": id, "osm_id": format!("node:{id}"), "updated_at": at}))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let url = |since: &str, limit: u32| {
            places::btcmap_sync_request(since, limit)
                .unwrap()
                .url
                .to_string()
        };
        let routes = [
            (url("2010-01-01T00:00:00Z", BTCMAP_PAGE), rows(1..2001, at)),
            // From that instant: the same full page, then a larger one, still all at it.
            (url(at, BTCMAP_PAGE), rows(1..2001, at)),
            (url(at, BTCMAP_PAGE_MAX), rows(1..4001, at)),
            (
                url("2026-10-10T00:00:00.001Z", BTCMAP_PAGE_MAX),
                rows(5000..5001, "2026-10-10T00:00:01.000Z"),
            ),
        ];
        let (egress, asked) = crate::sources::fixture::egress(
            routes
                .iter()
                .map(|(u, b)| (u.as_str(), 200, JSON, b.clone()))
                .collect(),
        );
        let cx = Context {
            egress,
            config: Default::default(),
        };
        let mut index = BtcmapIndex::default();
        let synced = index.sync(&cx).await;
        assert!(synced.is_ok(), "{synced:?} {:?}", asked.asked());
        assert!(index.synced.is_some());
        assert_eq!(index.places.len(), 4001);
        assert!(index.places.contains("node/5000"));
        assert_eq!(index.since.as_deref(), Some("2026-10-10T00:00:01.000Z"));
    }
}
