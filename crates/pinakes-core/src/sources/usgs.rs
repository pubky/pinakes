//! Earthquakes and other seismic events from the USGS Earthquake Hazards Program, read from its
//! FDSN event service by the event id in the link.
//!
//! An event has one preferred id and the ids every contributing network gave it (`ids`). Every
//! event page and API link, by any of those ids, is an alias of `usgs:{preferred id}`. The data
//! is public domain; USGS asks to be credited.
use super::kit::{
    float, host_and_segments, int, json, plain, text, unix_ms, web_url, Card, Point, Request,
};
use anyhow::{ensure, Context, Result};
use serde_json::Value;

pub const ATTRIBUTION: &str = "U.S. Geological Survey";

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
