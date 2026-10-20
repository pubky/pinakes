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
fn objects(item: &Value) -> std::collections::BTreeSet<String> {
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

/// What a place card takes from its item: the item's place links and its Wikipedia articles
/// when the item names the object back, and both keys only when the two are one to one.
pub fn from_item(osm: &OsmId, tag: &str, item: &Value) -> Found {
    let mut found = Found::default();
    if !names(osm, tag, item) {
        return found;
    }
    if mutual(osm, tag, item) {
        found.push_key(key(osm));
        found.push_key(wikidata::key(tag));
    }
    for link in wikidata::links(item, wikidata::PLACE)
        .into_iter()
        .chain(wikidata::wikipedia(item))
    {
        found.push_link(link);
    }
    found
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
    pub id: i64,
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
    id: i64,
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
            id: p.id,
            osm: p.osm_id.as_deref().and_then(btcmap_osm),
            updated_at: p.updated_at,
            deleted: p.deleted_at.is_some_and(|d| !d.is_empty()),
        })
        .collect())
}

/// Where the next sync page starts: the latest `updated_at` on this page. Rows that share it
/// come again on the next page, so the caller keys places by `id`.
pub fn btcmap_next_since(places: &[BtcmapPlace]) -> Option<&str> {
    places
        .iter()
        .filter_map(|p| Some((chrono::DateTime::parse_from_rfc3339(&p.updated_at).ok()?, p)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, p)| p.updated_at.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture;
    use std::collections::BTreeMap;

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
        assert!(lugano
            .links
            .iter()
            .any(|l| l.uri == "https://it.wikipedia.org/wiki/Lugano"));

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
