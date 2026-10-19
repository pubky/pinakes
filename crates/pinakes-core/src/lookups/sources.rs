//! The lookups as "also on" sources (`also_on::Source`): each fetches through the egress (its
//! policy, budgets and shared discovery cache), hands the answer to the pure parsers of its
//! module, and reports keys and derived links.
use super::{places, wikidata as items};
use crate::{
    also_on::{self, Context, Source, Subject},
    hubs,
    resolver::{egress::FetchOptions, RetryLater},
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

/// An OSM object tagged `wikidata=Q…` whose item names it back: both keys, and the item's
/// Wikidata page, official website, maps and Wikipedia editions as derived links.
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
        let found = places::from_item(&osm, &qid, &raw);
        if found.keys.is_empty() {
            return Ok(also_on::Found::default());
        }
        let found = found.into_also_on("wikidata");
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
        let keys: Vec<&str> = found.keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(keys, ["osm:node/240109189", "wikidata:Q64"]);
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

        // The item names the relation too (P402).
        let relation = subject("https://www.openstreetmap.org/relation/62422", Some("Q64"));
        let found = OsmWikidata.lookup(&cx, &relation).await.unwrap();
        assert_eq!(found.keys[0].key, "osm:relation/62422");

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
}
