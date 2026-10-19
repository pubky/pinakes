//! Earthquakes and other seismic events from the USGS Earthquake Hazards Program, read from its
//! FDSN event service by the event id in the link.
//!
//! An event has one preferred id and the ids every contributing network gave it (`ids`). Every
//! event page and API link, by any of those ids, is an alias of `usgs:{preferred id}`. The data
//! is public domain; USGS asks to be credited.
use super::{
    kit::{
        fetch, float, host_and_segments, int, json, plain, text, unix_ms, web_url, Card, Point,
        Request,
    },
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

pub const ATTRIBUTION: &str = "U.S. Geological Survey";
pub const SITE: &str = "USGS";

/// An event id: a network code and the network's own code, in lower case, e.g. `ci38457511`.
pub fn event_id(value: &str) -> Option<String> {
    let value = value.to_ascii_lowercase();
    plain(&value, 64, |c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
    })
    .then_some(value)
}

/// The event id a USGS link names: the event page (any tab), the GeoJSON detail feed, and the
/// FDSN query by `eventid`.
pub fn from_uri(uri: &str) -> Option<String> {
    let (host, segments) = host_and_segments(uri)?;
    if host != "earthquake.usgs.gov" {
        return None;
    }
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match s.as_slice() {
        ["earthquakes", "eventpage", id, ..] => event_id(id),
        ["earthquakes", "feed", "v1.0", "detail", file] => event_id(file.strip_suffix(".geojson")?),
        ["fdsnws", "event", "1", "query"] => {
            let url = web_url(uri)?;
            let mut ids = url.query_pairs().filter(|(k, _)| k == "eventid");
            match (ids.next(), ids.next()) {
                (Some((_, id)), None) => event_id(&id),
                _ => None,
            }
        }
        _ => None,
    }
}

pub fn identifier(id: &str) -> String {
    format!("usgs:{id}")
}

/// The event id of a root (`usgs:{id}`, the id already in lower case).
pub fn from_root(uri: &str) -> Option<String> {
    let id = uri.strip_prefix("usgs:")?;
    event_id(id).filter(|i| i == id)
}

/// The event id of a root or a USGS link.
pub fn target(uri: &str) -> Option<String> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// The event page.
pub fn page(id: &str) -> String {
    format!("https://earthquake.usgs.gov/earthquakes/eventpage/{id}")
}

/// The event as GeoJSON, with every product (the answer can be over 100 KB).
pub fn request(id: &str) -> Option<Request> {
    Request::get(&format!(
        "https://earthquake.usgs.gov/fdsnws/event/1/query?eventid={}&format=geojson",
        event_id(id)?
    ))
}

/// The ids of an event, from the comma-separated `ids` (which starts and ends with a comma).
fn ids(v: &Value) -> Vec<String> {
    v.as_str()
        .into_iter()
        .flat_map(|s| s.split(','))
        .filter_map(event_id)
        .collect()
}

/// An event, or `None` when there is no event with this id (404; 409 for a deleted event, 204
/// when asked for no-data answers). `id` is the requested id, which may be any of the event's ids.
pub fn parse(status: u16, raw: &[u8], id: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[204, 404, 409])? else {
        return Ok(None);
    };
    let p = &v["properties"];
    let preferred = v["id"].as_str().and_then(event_id).context("no event id")?;
    let all = ids(&p["ids"]);
    ensure!(
        preferred == id || all.iter().any(|i| i == id),
        "the API answered for another event"
    );
    let mut card = Card::new("Event", identifier(&preferred), page(&preferred));
    card.name = text(&p["title"]);
    card.start = unix_ms(&p["time"]);
    card.attribution = Some(ATTRIBUTION.into());
    let c = &v["geometry"]["coordinates"];
    card.point = float(&c[1])
        .zip(float(&c[0]))
        .and_then(|(lat, lon)| Point::new(lat, lon));
    card.detail("magnitude", float(&p["mag"]));
    card.detail("magnitude_type", text(&p["magType"]));
    card.detail("depth_km", float(&c[2]));
    card.detail("place", text(&p["place"]));
    card.detail("event_type", text(&p["type"]));
    card.detail("tsunami", int(&p["tsunami"]).map(|t| t == 1));
    card.detail("alert", text(&p["alert"]));
    card.detail("status", text(&p["status"]));
    card.detail("felt", int(&p["felt"]));
    card.detail("significance", int(&p["sig"]));
    card.detail("ids", all.clone());
    for other in &all {
        card.alias(page(other));
    }
    Ok(Some(card))
}

/// The source: the USGS FDSN event service.
pub struct Usgs;

#[async_trait]
impl Source for Usgs {
    fn name(&self) -> &'static str {
        "usgs"
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    /// The root of the id in the link. When that id is not the event's preferred one, the
    /// fetched card's root is the preferred id's, and this root becomes its alias.
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?);
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("usgs:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "magnitude",
            "magnitude_type",
            "depth_km",
            "place",
            "event_type",
            "tsunami",
            "alert",
            "status",
            "felt",
            "significance",
            "ids",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("earthquake.usgs.gov", 1.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri:
                    "https://earthquake.usgs.gov/earthquakes/eventpage/official20110311054624120_30",
                card_type: "Event",
            },
            Example {
                uri: "https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511/executive",
                card_type: "Event",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&id).context("no request")?).await?;
        Ok(match parse(f.status, &f.body, &id)? {
            Some(card) => card.into_doc(cx.card(uri, self), SITE),
            None => cx.missing(uri, self),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    const TOHOKU: &str = "official20110311054624120_30";

    fn read(name: &str) -> Vec<u8> {
        fixture::read("usgs", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).as_deref().map(identifier)
    }

    #[test]
    fn every_link_form_of_one_event() {
        for uri in [
            "https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511",
            "https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511/",
            "http://earthquake.usgs.gov/earthquakes/eventpage/CI38457511/executive",
            "https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511/map?shakemap-intensity=true",
            "https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511/origin/detail#x",
            "https://earthquake.usgs.gov/fdsnws/event/1/query?eventid=ci38457511&format=geojson",
            "https://earthquake.usgs.gov/fdsnws/event/1/query?format=quakeml&eventid=ci38457511",
            "https://earthquake.usgs.gov/earthquakes/feed/v1.0/detail/ci38457511.geojson",
        ] {
            assert_eq!(id(uri).as_deref(), Some("usgs:ci38457511"), "{uri}");
        }
        assert_eq!(
            id(&format!(
                "https://earthquake.usgs.gov/earthquakes/eventpage/{TOHOKU}"
            )),
            Some(format!("usgs:{TOHOKU}"))
        );
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://earthquake.usgs.gov/earthquakes/eventpage/",
            "https://earthquake.usgs.gov/earthquakes/eventpage/ci3845%2F7511",
            "https://earthquake.usgs.gov/earthquakes/map/",
            "https://earthquake.usgs.gov/fdsnws/event/1/query?format=geojson",
            "https://earthquake.usgs.gov/fdsnws/event/1/query?eventid=a&eventid=b",
            "https://earthquake.usgs.gov/earthquakes/feed/v1.0/detail/ci38457511.json",
            "https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/all_day.geojson",
            "https://www.emsc-csem.org/Earthquake/earthquake.php?id=ci38457511",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn requests() {
        assert_eq!(
            request("ci38457511").unwrap().url.as_str(),
            "https://earthquake.usgs.gov/fdsnws/event/1/query?eventid=ci38457511&format=geojson"
        );
        assert_eq!(request("a/b"), None);
    }

    #[test]
    fn tohoku() {
        let card = parse(200, &read("event-tohoku.json.gz"), TOHOKU)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "Event");
        assert_eq!(card.id, format!("usgs:{TOHOKU}"));
        assert_eq!(card.url, page(TOHOKU));
        assert_eq!(
            card.name.as_deref(),
            Some("M 9.1 - 2011 Great Tohoku Earthquake, Japan")
        );
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "2011-03-11T05:46:24.120+00:00"
        );
        assert_eq!(card.point, Point::new(38.297, 142.373));
        assert_eq!(card.attribution.as_deref(), Some(ATTRIBUTION));
        assert_eq!(card.details["magnitude"], 9.1);
        assert_eq!(card.details["magnitude_type"], "mww");
        assert_eq!(card.details["depth_km"], 29.0);
        assert_eq!(card.details["tsunami"], false);
        assert_eq!(card.details["status"], "reviewed");
        assert_eq!(card.details["felt"], 1460);
        assert_eq!(card.details["significance"], 2184);
        assert!(!card.details.contains_key("alert"));
        assert_eq!(card.details["ids"].as_array().unwrap().len(), 7);
        assert_eq!(card.aliases.len(), 6, "every id but the preferred one");
        assert!(card
            .aliases
            .contains(&"https://earthquake.usgs.gov/earthquakes/eventpage/usp000hvnu".into()));
        assert!(card.also_on.is_empty());
    }

    #[test]
    fn an_alias_id_resolves_to_the_preferred_one() {
        let t = from_uri("https://earthquake.usgs.gov/earthquakes/eventpage/usp000hvnu").unwrap();
        let card = parse(200, &read("event-tohoku-usp000hvnu.json.gz"), &t)
            .unwrap()
            .unwrap();
        assert_eq!(card.id, format!("usgs:{TOHOKU}"));
    }

    #[test]
    fn ridgecrest() {
        let card = parse(200, &read("event-ci38457511.json.gz"), "ci38457511")
            .unwrap()
            .unwrap();
        assert_eq!(card.id, "usgs:ci38457511");
        assert_eq!(card.point, Point::new(35.7695, -117.5993333));
        assert_eq!(card.details["alert"], "yellow");
        assert_eq!(card.details["magnitude"], 7.1);
        assert_eq!(card.details["place"], "Ridgecrest Earthquake Sequence");
        assert_eq!(card.details["event_type"], "earthquake");
        assert!(card
            .aliases
            .contains(&"https://earthquake.usgs.gov/earthquakes/eventpage/us70004bn0".into()));
    }

    #[test]
    fn roots_and_aliases() {
        let u = Usgs;
        for uri in ["usgs:ci38457511", "usgs:official20110311054624120_30"] {
            assert!(u.reads(uri), "{uri}");
            assert_eq!(u.alias_root(uri), None, "{uri}");
            assert!(!u.invalid(uri), "{uri}");
        }
        assert_eq!(
            u.alias_root("https://earthquake.usgs.gov/earthquakes/eventpage/CI38457511/map")
                .as_deref(),
            Some("usgs:ci38457511")
        );
        // A non-preferred id is its own root until the fetch names the preferred one.
        assert_eq!(
            u.alias_root("https://earthquake.usgs.gov/earthquakes/eventpage/usp000hvnu")
                .as_deref(),
            Some("usgs:usp000hvnu")
        );
        for uri in ["usgs:CI38457511", "usgs:", "usgs:a/b"] {
            assert!(u.invalid(uri), "{uri}");
            assert!(!u.reads(uri), "{uri}");
        }
        assert!(!u.invalid("https://example.org/usgs:x"));
        for example in u.examples() {
            assert!(u.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, id) in [
            ("event-tohoku.json.gz", TOHOKU),
            ("event-tohoku-usp000hvnu.json.gz", "usp000hvnu"),
            ("event-ci38457511.json.gz", "ci38457511"),
        ] {
            let card = parse(200, &read(file), id).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Usgs, &card);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
            assert_eq!(doc.attributes["page_url"], card.url);
        }
        let card = parse(200, &read("event-tohoku-usp000hvnu.json.gz"), "usp000hvnu")
            .unwrap()
            .unwrap();
        let doc = card.into_doc(ResourceMeta::new("usgs:usp000hvnu", "usgs", 60), SITE);
        assert_eq!(
            doc.source_root.as_deref(),
            Some("usgs:official20110311054624120_30")
        );
        assert_eq!(doc.attributes["tsunami"], "false");
        assert!(doc.attributes["ids"].contains("usp000hvnu"));
    }

    #[test]
    fn missing_mismatched_and_failed() {
        assert_eq!(
            parse(404, &read("event-missing.txt"), "us0000zzzzzz").unwrap(),
            None
        );
        assert_eq!(parse(409, b"Conflict", "us0000zzzzzz").unwrap(), None);
        assert!(parse(200, &read("event-ci38457511.json.gz"), TOHOKU).is_err());
        assert!(parse(429, b"", TOHOKU).is_err());
        assert!(parse(503, b"", TOHOKU).is_err());
        assert!(parse(200, b"<html>", TOHOKU).is_err());
    }
}
