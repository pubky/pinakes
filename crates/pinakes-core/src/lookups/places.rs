//! Places: an OSM object and its Wikidata item when both point at each other, and the BTC Map
//! merchant page for an OSM object, joined on `osm_id` from the BTC Map places sync.
//!
//! Coordinates never join anything.
use super::{body, encode, plain, wikidata, Found, Key, Link, Request};
use crate::model::OsmId;
use crate::resolver::osm::parse_id;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

pub fn key(osm: &OsmId) -> Key {
    Key::new("osm", &osm.key())
}

/// The Q-id of an OSM `wikidata=*` tag, only when it names exactly one item.
pub fn wikidata_tag(tag: &str) -> Option<&str> {
    wikidata::qid(tag)
}

/// The Wikidata property that names OSM objects of this type.
fn osm_property(osm: &OsmId) -> Option<&'static str> {
    match osm.osm_type.as_str() {
        "node" => Some("P11693"),
        "way" => Some("P10689"),
        "relation" => Some("P402"),
        _ => None,
    }
}

/// Whether an OSM object tagged `wikidata=<tag>` is named back by the item fetched for that
/// tag: the item is the tagged one (not a redirect target) and one of its best-rank OSM
/// statements names this object.
pub fn names(osm: &OsmId, tag: &str, item: &Value) -> bool {
    let Some(qid) = wikidata_tag(tag) else {
        return false;
    };
    let id = osm.id.to_string();
    item["id"] == qid
        && osm_property(osm)
            .is_some_and(|pid| wikidata::best_values(item, pid).contains(&id.as_str()))
}

/// The OSM objects an item's best-rank statements name, of every type (`node/1`, `way/2`).
pub fn objects(item: &Value) -> std::collections::BTreeSet<String> {
    [("node", "P11693"), ("way", "P10689"), ("relation", "P402")]
        .into_iter()
        .flat_map(|(kind, pid)| {
            wikidata::best_values(item, pid)
                .into_iter()
                .map(move |id| format!("{kind}/{id}"))
        })
        .collect()
}

/// Whether an OSM object and its tagged item point at each other one to one: the item names
/// this object back and no other OSM object (Berlin's item names a node and a relation, so
/// neither is the item's one object).
pub fn mutual(osm: &OsmId, tag: &str, item: &Value) -> bool {
    names(osm, tag, item) && objects(item).into_iter().eq([osm.key()])
}

/// What a place card takes from its item: the item's place links when the item names the
/// object back, and both keys only when the two are one to one.
pub fn from_item(osm: &OsmId, tag: &str, item: &Value) -> Found {
    let mut found = Found::default();
    if !names(osm, tag, item) {
        return found;
    }
    if mutual(osm, tag, item) {
        found.push_key(key(osm));
        found.push_key(wikidata::key(tag));
    }
    for link in wikidata::links(item, wikidata::PLACE) {
        found.push_link(link);
    }
    found
}

/// Tripadvisor's country sites, after `tripadvisor.`.
const TRIPADVISOR: &[&str] = &[
    "com", "co.uk", "ca", "com.au", "co.nz", "ie", "in", "de", "at", "ch", "fr", "be", "it", "es",
    "pt", "nl", "dk", "se", "fi", "jp", "com.br", "com.mx", "com.ar", "cl", "co", "com.sg",
    "com.my", "com.ph", "co.id", "co.za", "com.tr", "gr", "ru", "com.hk", "com.tw", "co.kr",
];

/// The Wikidata property a place's page on a map or review site names, its value, and the
/// site: Google Maps (`?cid=`), Apple Maps (`?auid=`) and Yelp by the place table's links
/// (`wikidata::PLACE`), and Tripadvisor by the location number in any of its pages' names
/// (`…-d188151-…`, on any of its country sites), which is all its property holds.
pub fn place_page(uri: &str) -> Option<(&'static str, String, &'static str)> {
    let url = super::web_url(uri)?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.trim_start_matches("www.");
    let tripadvisor = host
        .strip_prefix("tripadvisor.")
        .is_some_and(|site| TRIPADVISOR.contains(&site));
    if tripadvisor {
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let [page] = segments.as_slice() else {
            return None;
        };
        let (kind, rest) = page.strip_suffix(".html")?.split_once("-g")?;
        if !matches!(
            kind,
            "Attraction_Review" | "Restaurant_Review" | "Hotel_Review" | "ShowUserReviews"
        ) {
            return None;
        }
        let number = rest.split('-').find_map(|part| part.strip_prefix('d'))?;
        return (plain(number, 12, |c| c.is_ascii_digit()) && !number.starts_with('0'))
            .then(|| ("P3134", number.to_owned(), "Tripadvisor"));
    }
    wikidata::PLACE
        .iter()
        .filter(|p| p.site != "Tripadvisor")
        .find_map(|p| p.value_in(uri).map(|value| (p.pid, value, p.site)))
}

/// The BTC Map fields the places sync asks for.
const BTCMAP_FIELDS: &str = "id,osm_id,osm_url,name,updated_at,deleted_at";

/// One page of the BTC Map places sync: places changed at or after `updated_since` (RFC 3339),
/// deleted ones included so the caller can drop them.
pub fn btcmap_sync_request(updated_since: &str, limit: u32) -> Option<Request> {
    chrono::DateTime::parse_from_rfc3339(updated_since).ok()?;
    Request::get(&format!(
        "https://api.btcmap.org/v4/places?fields={BTCMAP_FIELDS}&updated_since={}\
         &limit={}&include_deleted=true",
        encode(updated_since),
        limit.clamp(1, 5000)
    ))
}

/// A BTC Map place from the sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BtcmapPlace {
    /// The OSM object, when the place has one.
    pub osm: Option<OsmId>,
    pub updated_at: String,
    pub deleted: bool,
}
impl BtcmapPlace {
    /// The merchant page, `btcmap.org/merchant/{type}:{id}`.
    pub fn link(&self) -> Option<Link> {
        let osm = self.osm.as_ref()?;
        (!self.deleted).then(|| {
            Link::new(
                "BTC Map",
                format!("https://btcmap.org/merchant/{}:{}", osm.osm_type, osm.id),
                "btcmap",
            )
        })
    }
}

#[derive(Deserialize)]
struct RawPlace {
    osm_id: Option<String>,
    updated_at: String,
    deleted_at: Option<String>,
}

/// `node:123` (BTC Map's `osm_id`) as an OSM object.
fn btcmap_osm(value: &str) -> Option<OsmId> {
    let (kind, id) = value.split_once(':')?;
    if !matches!(kind, "node" | "way" | "relation") || !plain(id, 20, |c| c.is_ascii_digit()) {
        return None;
    }
    parse_id(&format!("https://www.openstreetmap.org/{kind}/{id}"))
}

/// A page of the places sync, in the order served.
pub fn parse_btcmap_places(status: u16, raw: &[u8]) -> Result<Vec<BtcmapPlace>> {
    let raw = body(status, raw, &[])?.context("no body")?;
    let places: Vec<RawPlace> = serde_json::from_slice(raw)?;
    Ok(places
        .into_iter()
        .map(|p| BtcmapPlace {
            osm: p.osm_id.as_deref().and_then(btcmap_osm),
            updated_at: p.updated_at,
            deleted: p.deleted_at.is_some_and(|d| !d.is_empty()),
        })
        .collect())
}

/// Where the next sync page starts: the latest `updated_at` on this page. Rows that share it
/// come again on the next page, so the caller keys places by their OSM object.
pub fn btcmap_next_since(places: &[BtcmapPlace]) -> Option<&str> {
    places
        .iter()
        .filter_map(|p| Some((chrono::DateTime::parse_from_rfc3339(&p.updated_at).ok()?, p)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, p)| p.updated_at.as_str())
}

/// The sync time just past `since` (one millisecond, BTC Map's resolution).
pub fn btcmap_after(since: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(since).ok()? + chrono::Duration::milliseconds(1);
    Some(
        at.to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

/// This family in the lookups' table ([`super::FAMILIES`]).
pub struct Lookups;
impl super::Family for Lookups {
    fn name(&self) -> &'static str {
        "places"
    }
    fn sources(&self) -> Vec<std::sync::Arc<dyn crate::also_on::Source>> {
        vec![
            std::sync::Arc::new(also_on::OsmWikidata),
            std::sync::Arc::new(also_on::WikidataOsm),
            std::sync::Arc::new(also_on::WikidataPlaces),
            std::sync::Arc::new(also_on::Btcmap),
        ]
    }
}

/// The also-on sources that run these lookups: each fetches through the egress (its policy,
/// budgets and shared discovery cache), hands the answer to this module's parsers, and reports
/// keys and derived links.
/// The item's Wikipedia editions in `wikipedia.rs`'s order (preferred languages first), then its
/// other encyclopedias.
fn editions(hub: &crate::hubs::wikidata::Item) -> Vec<crate::also_on::Link> {
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

/// The forms of key the places sources record (`also_on::Source::key_forms`).
pub const KEY_FORMS: &[(&str, &str)] = &[("osm", "node/240109189"), ("wikidata", "Q42")];
pub mod also_on {
    use super::editions;
    use crate::{
        also_on::{self, Context, Source, Subject},
        lookups::{get, item, places, send, wikidata as items, wikidata::wikidata_item},
        resolver::RetryLater,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::Value;

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
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::places::KEY_FORMS
        }
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
            links.extend(found.links);
            links.extend(editions(&hub));
            Ok(also_on::Found {
                keys: found.keys,
                links,
                ..Default::default()
            })
        }
    }

    /// The one OSM object an item names: its OSM relation (P402) when it has exactly one, else its
    /// only OSM link of any type (P402, P11693 node, P10689 way).
    pub fn osm_object(item: &Value) -> Option<(&'static str, String)> {
        let ids = |pid: &str| -> Vec<String> {
            items::best_values(item, pid)
                .into_iter()
                .filter(|v| {
                    crate::lookups::plain(v, 20, |c| c.is_ascii_digit()) && !v.starts_with('0')
                })
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
            let url = crate::wikipedia::item_lookup(&article)?;
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
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::places::KEY_FORMS
        }
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

    /// A place's page on a map or review site (`places::place_page`): the one Wikidata item
    /// carrying the page's value (read back and compared exactly; several are ambiguous and give
    /// nothing), its Q-id as key, its one OSM object (when it names exactly one) as key and link,
    /// and its Wikidata page, other place links and Wikipedia editions as links.
    pub struct WikidataPlaces;

    #[async_trait]
    impl Source for WikidataPlaces {
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::places::KEY_FORMS
        }
        fn id(&self) -> &'static str {
            "wikidata_places"
        }
        fn applies(&self, subject: &Subject) -> bool {
            places::place_page(&subject.uri).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            let Some((pid, value, own)) = places::place_page(&subject.uri) else {
                return Ok(also_on::Found::default());
            };
            // An answer over its read's cap is no answer: asking again would read the same megabytes.
            let hub = match crate::hubs::wikidata::item_with(&cx.egress, pid, &value).await {
                Err(error) if crate::resolver::egress::over_cap(&error) => None,
                Ok(crate::hubs::wikidata::Found::One(hub)) => Some(hub),
                Ok(_) => None,
                Err(error) => return Err(error),
            };
            let Some(hub) = hub else {
                return Ok(also_on::Found::default());
            };
            let Some((raw, hub)) = item(cx, &hub.qid).await? else {
                return Ok(also_on::Found::default());
            };
            let mut found = crate::lookups::Found::default();
            found.push_key(items::key(&hub.qid));
            // The item's OSM objects with a valid id (a malformed statement names none): its one
            // object, when it names exactly one, is a key and a link.
            let objects: Vec<crate::model::OsmId> = places::objects(&raw)
                .iter()
                .filter_map(|object| {
                    let (kind, id) = object.split_once('/')?;
                    let valid = crate::lookups::plain(id, 20, |c| c.is_ascii_digit())
                        && !id.starts_with('0');
                    Some(crate::model::OsmId {
                        osm_type: kind.to_owned(),
                        id: id.parse().ok().filter(|_| valid)?,
                    })
                })
                .collect();
            let mut links = vec![];
            if let [osm] = objects.as_slice() {
                found.push_key(places::key(osm));
                links.push(also_on::Link::new(
                    format!("https://www.openstreetmap.org/{}", osm.key()),
                    "OpenStreetMap",
                    "wikidata",
                ));
            }
            links.push(also_on::Link::new(
                format!("https://www.wikidata.org/wiki/{}", hub.qid),
                "Wikidata",
                "wikidata",
            ));
            let found = found.into_also_on("wikidata");
            links.extend(
                items::links(&raw, items::PLACE)
                    .into_iter()
                    .map(|l| also_on::Link::new(l.uri, l.site, l.via))
                    .filter(|l| l.site != own),
            );
            links.extend(editions(&hub));
            Ok(also_on::Found {
                keys: found.keys,
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
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::places::KEY_FORMS
        }
        /// A merchant page only: `/merchant/{node|way|relation}:{id}`.
        fn service_name(&self, host: &str, url: &url::Url) -> Option<&'static str> {
            let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
            (host == "btcmap.org"
                && matches!(
                    segments.as_slice(),
                    ["merchant", object] if object.split_once(':').is_some_and(|(kind, id)| {
                        matches!(kind, "node" | "way" | "relation") && crate::lookups::digits(id)
                    })
                ))
            .then_some("BTC Map")
        }
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
            let mut found = crate::lookups::Found {
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::lookups::fixture::{at, context, keys, read, recorded, subject, JSON};

        fn entity(q: &str) -> String {
            crate::hubs::wikidata::entities_lookup(&[q.to_owned()])
        }

        /// A malformed OSM statement names no object: it costs only that, the Q-id is still a key,
        /// and a valid OSM statement beside it still names the item's one object.
        #[tokio::test(start_paused = true)]
        async fn a_malformed_osm_statement_costs_only_the_osm_key() {
            let search = "https://www.wikidata.org/w/api.php?action=query&list=search&srsearch=haswbstatement%3AP3134%3D188151&srlimit=5&format=json";
            let item = |keep_way: bool| {
                let mut v: serde_json::Value =
                    serde_json::from_slice(&read("wikidata-q243.json")).unwrap();
                let claims = &mut v["entities"]["Q243"]["claims"];
                claims["P402"] = serde_json::json!([{"mainsnak": {"snaktype": "value", "property": "P402",
                    "datavalue": {"value": "12a", "type": "string"}, "datatype": "external-id"},
                    "type": "statement", "rank": "normal"}]);
                if !keep_way {
                    claims.as_object_mut().unwrap().remove("P10689");
                }
                serde_json::to_vec(&v).unwrap()
            };
            let page = at("https://www.tripadvisor.com/Attraction_Review-g187147-d188151-Reviews-Eiffel_Tower-Paris_Ile_de_France.html");
            for (keep_way, expected) in [
                (false, vec!["wikidata:Q243"]),
                (true, vec!["wikidata:Q243", "osm:way/5013364"]),
            ] {
                let cx = context(vec![
                    (search, 200, JSON, read("wikidata-search-p3134-188151.json")),
                    (&entity("Q243"), 200, JSON, item(keep_way)),
                ]);
                let found = WikidataPlaces.lookup(&cx, &page).await.unwrap();
                assert_eq!(keys(&found), expected, "{keep_way}");
            }
        }

        /// The Eiffel Tower's Tripadvisor page on the German site finds its item by the location
        /// number alone: the item's Q-id and its one OSM way are keys, and its OSM, Wikidata,
        /// Google Maps, Yelp and Wikipedia pages are links, never Tripadvisor itself.
        #[tokio::test(start_paused = true)]
        async fn a_tripadvisor_page_reaches_its_place_through_wikidata() {
            let cx = recorded(vec![]);
            let page = at("https://www.tripadvisor.de/Attraction_Review-g187147-d188151-Reviews-Eiffelturm-Paris_Ile_de_France.html");
            assert!(WikidataPlaces.applies(&page));
            let found = WikidataPlaces.lookup(&cx, &page).await.unwrap();
            assert_eq!(keys(&found), ["wikidata:Q243", "osm:way/5013364"]);
            let sites: Vec<&str> = found.links.iter().map(|l| l.site.as_str()).collect();
            assert_eq!(
                sites[..4],
                ["OpenStreetMap", "Wikidata", "Google Maps", "Yelp"]
            );
            assert!(
                sites.iter().any(|s| s.starts_with("Wikipedia")),
                "{sites:?}"
            );
            assert!(!sites.contains(&"Tripadvisor"));
            assert_eq!(
                found.links[0].uri,
                "https://www.openstreetmap.org/way/5013364"
            );
            // A value nobody recorded finds no item and gives nothing.
            let other = at("https://www.yelp.com/biz/no-such-place-pinakes");
            assert!(WikidataPlaces.applies(&other));
            let none = WikidataPlaces.lookup(&cx, &other).await.unwrap();
            assert!(none.keys.is_empty() && none.links.is_empty());
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

        #[tokio::test(start_paused = true)]
        async fn btcmap() {
            let cx = recorded(vec![(
                places::btcmap_sync_request("2010-01-01T00:00:00Z", BTCMAP_PAGE)
                    .unwrap()
                    .url
                    .into(),
                "btcmap-places",
            )]);
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

        /// No link or ID check of this family slices a string inside a multibyte character, and no
        /// parser panics on an answer it doesn't expect.
        #[test]
        fn spellings_and_answers_never_panic() {
            use crate::lookups::{fixture, places};
            for s in fixture::multibyte_spellings(&["https://example.com/", "Q7024"]) {
                places::wikidata_tag(&s);
            }
            for (status, raw) in fixture::malformed_answers() {
                let _ = places::parse_btcmap_places(status, raw);
            }
        }

        #[test]
        fn service_names() {
            assert_eq!(
                crate::lookups::service_name("https://btcmap.org/merchant/way:259338174"),
                Some("BTC Map")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture;
    use std::collections::BTreeMap;

    /// A place's page on a map or review site names its Wikidata property and value: Tripadvisor
    /// by the location number on any of its country sites, the others by the place table's
    /// links.
    #[test]
    fn place_pages_name_their_property_and_value() {
        for (uri, found) in [
            (
                "https://www.tripadvisor.de/Attraction_Review-g187147-d188151-Reviews-Eiffelturm-Paris_Ile_de_France.html",
                Some(("P3134", "188151", "Tripadvisor")),
            ),
            (
                "https://www.tripadvisor.co.uk/Restaurant_Review-g188113-d1234567-Reviews-Name-Zurich.html",
                Some(("P3134", "1234567", "Tripadvisor")),
            ),
            (
                "https://www.google.com/maps?cid=10222232094831998944",
                Some(("P3749", "10222232094831998944", "Google Maps")),
            ),
            (
                "https://maps.apple.com/place?auid=123456789",
                Some(("P10046", "123456789", "Apple Maps")),
            ),
            (
                "https://www.yelp.com/biz/la-tour-eiffel-paris-4",
                Some(("P3108", "la-tour-eiffel-paris-4", "Yelp")),
            ),
            ("https://www.tripadvisor.de/Tourism-g187147-Paris_Ile_de_France-Vacations.html", None),
            ("https://www.tripadvisor.com/188151", None),
            ("https://www.tripadvisor.evil.example/Attraction_Review-g1-d2-Reviews-x.html", None),
            ("https://tripadvisor.xyz.com/Attraction_Review-g1-d2-Reviews-x.html", None),
            ("https://www.google.com/maps/place/Eiffel+Tower", None),
        ] {
            let got = place_page(uri);
            assert_eq!(
                got.as_ref().map(|(p, v, s)| (*p, v.as_str(), *s)),
                found,
                "{uri}"
            );
        }
    }

    /// Only an object's own page on this family's services is named after the service; any
    /// other page there (a topic list, a search, a user's page, a page under an object) is not.
    #[test]
    fn only_an_objects_own_page_is_named_after_its_service() {
        for (uri, name) in [
            ("https://btcmap.org/merchant/way:259338174", Some("BTC Map")),
            ("https://btcmap.org/merchant/shop:1", None),
            ("https://btcmap.org/map", None),
        ] {
            assert_eq!(crate::lookups::service_name(uri), name, "{uri}");
        }
    }

    fn osm(s: &str) -> OsmId {
        parse_id(&format!("https://www.openstreetmap.org/{s}")).unwrap()
    }
    /// The `wikidata` tag of each object in the recorded Nominatim lookup.
    fn tags() -> BTreeMap<String, String> {
        let v: Value = serde_json::from_slice(&fixture::read("nominatim-places.json")).unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    format!("{}/{}", p["osm_type"].as_str().unwrap(), p["osm_id"]),
                    p["extratags"]["wikidata"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn mutual_links() {
        let items =
            wikidata::parse_entities(200, &fixture::read("wikidata-places.json.gz")).unwrap();
        let tags = tags();
        assert_eq!(tags["relation/2888417"], "Q7024");
        let lugano = from_item(
            &osm("relation/2888417"),
            &tags["relation/2888417"],
            &items["Q7024"],
        );
        // Lugano's item names its relation and a node: links, but neither object is the item.
        assert!(lugano.keys.is_empty());
        assert!(!mutual(&osm("relation/2888417"), "Q7024", &items["Q7024"]));
        assert_eq!(lugano.links[0].uri, "https://www.lugano.ch/");

        let museum = from_item(
            &osm("node/9984134345"),
            &tags["node/9984134345"],
            &items["Q153306"],
        );
        // The museum's item names this one node: one to one, both keys.
        assert_eq!(
            museum.keys.iter().map(Key::as_str).collect::<Vec<_>>(),
            ["osm:node/9984134345", "wikidata:Q153306"]
        );
        assert!(museum.links.iter().any(|l| l.site == "Yelp"));
        let archives = from_item(
            &osm("way/70417858"),
            &tags["way/70417858"],
            &items["Q38945047"],
        );
        assert_eq!(archives.keys[0].as_str(), "osm:way/70417858");
        assert!(archives.links.iter().any(|l| l.site == "Apple Maps"));

        // One side only: the item does not name this object, or the tag names another item.
        assert_eq!(
            from_item(&osm("relation/1"), "Q7024", &items["Q7024"]),
            Found::default()
        );
        assert_eq!(
            from_item(&osm("node/2888417"), "Q7024", &items["Q7024"]),
            Found::default()
        );
        assert_eq!(
            from_item(&osm("relation/2888417"), "Q153306", &items["Q7024"]),
            Found::default()
        );
        assert!(!mutual(
            &osm("relation/2888417"),
            "Q7024;Q1",
            &items["Q7024"]
        ));
    }

    #[test]
    fn btcmap_sync() {
        let r = btcmap_sync_request("2026-10-10T00:00:00Z", 500).unwrap();
        assert_eq!(
            r.url.as_str(),
            "https://api.btcmap.org/v4/places?fields=id,osm_id,osm_url,name,updated_at,deleted_at\
             &updated_since=2026-10-10T00%3A00%3A00Z&limit=500&include_deleted=true"
        );
        assert!(btcmap_sync_request("yesterday", 10).is_none());

        let places = parse_btcmap_places(200, &fixture::read("btcmap-places.json")).unwrap();
        assert_eq!(places.len(), 40);
        let live: Vec<_> = places.iter().filter_map(BtcmapPlace::link).collect();
        let deleted = places.iter().filter(|p| p.deleted).count();
        assert!(deleted > 0);
        assert_eq!(live.len() + deleted, places.len());
        let first = &places.iter().find(|p| !p.deleted).unwrap();
        let link = first.link().unwrap();
        let osm = first.osm.as_ref().unwrap();
        assert_eq!(
            link.uri,
            format!("https://btcmap.org/merchant/{}:{}", osm.osm_type, osm.id)
        );
        assert_eq!((link.site.as_str(), link.via), ("BTC Map", "btcmap"));
        assert_eq!(
            key(osm).as_str(),
            format!("osm:{}/{}", osm.osm_type, osm.id)
        );
        assert!(btcmap_next_since(&places).unwrap() >= places[0].updated_at.as_str());
        assert!(parse_btcmap_places(503, b"").is_err());
    }

    #[test]
    fn btcmap_osm_ids() {
        assert_eq!(btcmap_osm("way:42").unwrap().key(), "way/42");
        for bad in ["node:", "node:x", "area:1", "node/1", "node:1:2"] {
            assert!(btcmap_osm(bad).is_none(), "{bad}");
        }
    }
}
