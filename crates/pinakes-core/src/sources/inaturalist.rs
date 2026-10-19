//! Observations and taxa from the iNaturalist API (v1), read by the id in the link.
//!
//! Every site of the iNaturalist Network shares one database, so a link on any of them, and the
//! API URL, is an alias of one identifier: `inat:observation/{id}` or `inat:taxon/{id}`.
//!
//! The API asks for at most 60 requests a minute and under 10,000 a day: `REQUEST_INTERVAL`
//! apart. Licences are per observation and per photo; a photo without a licence is never shown.
//! An observation whose location is obscured or private has public coordinates that are
//! randomised, so its card has no point.
use super::kit::html_text;
use super::kit::{
    float, host_and_segments, int, json, plain, text, time, Card, Key, Link, Point, Request,
};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::time::Duration;

pub const SITE: &str = "iNaturalist";
const VIA: &str = "inaturalist";
const API: &str = "https://api.inaturalist.org/v1";
/// The least time between two API requests.
pub const REQUEST_INTERVAL: Duration = Duration::from_secs(2);

/// The sites of the iNaturalist Network (`/v1/sites`), without `www.`, plus naturalista.mx,
/// which redirects to the Mexican site.
pub const SITES: [&str; 23] = [
    "inaturalist.org",
    "mexico.inaturalist.org",
    "naturalista.mx",
    "inaturalist.nz",
    "inaturalist.ca",
    "colombia.inaturalist.org",
    "biodiversity4all.org",
    "inaturalist.ala.org.au",
    "panama.inaturalist.org",
    "ecuador.inaturalist.org",
    "israel.inaturalist.org",
    "argentinat.org",
    "costarica.inaturalist.org",
    "inaturalist.mma.gob.cl",
    "inaturalist.laji.fi",
    "inaturalist.se",
    "spain.inaturalist.org",
    "greece.inaturalist.org",
    "guatemala.inaturalist.org",
    "uk.inaturalist.org",
    "inaturalist.lu",
    "taiwan.inaturalist.org",
    "naturalista.uy",
];

/// The object a link names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Observation(u64),
    Taxon(u64),
}

/// A positive decimal id, without leading zeros.
fn number(value: &str) -> Option<u64> {
    plain(value, 12, |c| c.is_ascii_digit())
        .then(|| value.parse().ok())
        .flatten()
        .filter(|id| *id > 0)
}

/// A taxon id, alone or followed by the name slug (`47219-Apis-mellifera`).
fn taxon_id(segment: &str) -> Option<u64> {
    match segment.split_once('-') {
        Some((id, slug)) if !slug.is_empty() => number(id),
        Some(_) => None,
        None => number(segment),
    }
}

/// What an iNaturalist Network or API link names.
pub fn from_uri(uri: &str) -> Option<Object> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (host.as_str(), s.as_slice()) {
        ("api.inaturalist.org", ["v1", "observations", id]) => number(id).map(Object::Observation),
        ("api.inaturalist.org", ["v1", "taxa", id]) => number(id).map(Object::Taxon),
        (h, ["observations", id]) if SITES.contains(&h) => number(id).map(Object::Observation),
        (h, ["taxa", id]) if SITES.contains(&h) => taxon_id(id).map(Object::Taxon),
        _ => None,
    }
}

/// The identifier every link form of the object is an alias of.
pub fn identifier(object: Object) -> String {
    match object {
        Object::Observation(id) => format!("inat:observation/{id}"),
        Object::Taxon(id) => format!("inat:taxon/{id}"),
    }
}

/// The object's page on inaturalist.org.
pub fn page(object: Object) -> String {
    match object {
        Object::Observation(id) => format!("https://www.inaturalist.org/observations/{id}"),
        Object::Taxon(id) => format!("https://www.inaturalist.org/taxa/{id}"),
    }
}

/// The object's API record.
pub fn request(object: Object) -> Option<Request> {
    Request::get(&match object {
        Object::Observation(id) => format!("{API}/observations/{id}"),
        Object::Taxon(id) => format!("{API}/taxa/{id}"),
    })
}

/// The observation or taxon card, or `None` when iNaturalist has no such record.
pub fn parse(status: u16, raw: &[u8], object: Object) -> Result<Option<Card>> {
    match object {
        Object::Observation(id) => parse_observation(status, raw, id),
        Object::Taxon(id) => parse_taxon(status, raw, id),
    }
}

/// The one record of an answer, `None` for an empty one (iNaturalist answers 200 with no
/// results for an unknown id).
fn record(status: u16, raw: &[u8], id: u64, what: &str) -> Result<Option<Value>> {
    let Some(mut v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let total = int(&v["total_results"]);
    let results = v["results"].as_array_mut().context("no results")?;
    if results.is_empty() && total == Some(0) {
        return Ok(None);
    }
    ensure!(results.len() == 1, "{} {what} records", results.len());
    let record = results.remove(0);
    ensure!(
        record["id"].as_u64() == Some(id),
        "iNaturalist answered for another {what}"
    );
    Ok(Some(record))
}

/// A licence code as people read it, e.g. `cc-by-nc` → "CC BY-NC", `cc0` → "CC0".
pub fn licence(code: &str) -> String {
    match code.to_ascii_lowercase().strip_prefix("cc-") {
        Some(rest) => format!("CC {}", rest.to_ascii_uppercase()),
        None => code.to_ascii_uppercase(),
    }
}

/// A licensed photo's URL at the given size (`medium`, `large`) and its attribution.
fn photo(p: &Value, size: &str) -> Option<(String, Option<String>)> {
    text(&p["license_code"])?;
    let url = text(&p[format!("{size}_url").as_str()]).or_else(|| {
        let square = text(&p["url"])?;
        square
            .contains("/square.")
            .then(|| square.replacen("/square.", &format!("/{size}."), 1))
    })?;
    Some((url, text(&p["attribution"])))
}

/// Whether an observation's coordinates are its real, public location.
fn public_location(v: &Value) -> bool {
    let open = |field: &str| matches!(v[field].as_str(), None | Some("open"));
    v["obscured"].as_bool() == Some(false) && open("geoprivacy") && open("taxon_geoprivacy")
}

/// An observation: someone saw an organism at a place and time.
pub fn parse_observation(status: u16, raw: &[u8], id: u64) -> Result<Option<Card>> {
    let Some(v) = record(status, raw, id, "observation")? else {
        return Ok(None);
    };
    let object = Object::Observation(id);
    let mut card = Card::new("Observation", identifier(object), page(object));
    let taxon = &v["taxon"];
    card.name = text(&v["species_guess"]).or_else(|| text(&taxon["name"]));
    let public = public_location(&v);
    if public {
        let c = &v["geojson"]["coordinates"];
        card.point = float(&c[1])
            .zip(float(&c[0]))
            .and_then(|(la, lo)| Point::new(la, lo));
        card.detail("place_guess", text(&v["place_guess"]));
        card.detail("positional_accuracy_m", int(&v["positional_accuracy"]));
    }
    card.start = time(&v["time_observed_at"]).or_else(|| time(&v["observed_on"]));
    card.detail("observed_on", text(&v["observed_on"]));
    card.detail("quality_grade", text(&v["quality_grade"]));
    card.detail("taxon_id", taxon["id"].as_u64());
    card.detail("taxon_name", text(&taxon["name"]));
    card.detail("taxon_rank", text(&taxon["rank"]));
    card.detail("common_name", text(&taxon["preferred_common_name"]));
    let observer = text(&v["user"]["login"]);
    card.detail("user_login", observer.clone());
    card.detail("obscured", v["obscured"].as_bool());
    card.detail("geoprivacy", text(&v["geoprivacy"]));
    card.detail("taxon_geoprivacy", text(&v["taxon_geoprivacy"]));
    card.detail("uuid", text(&v["uuid"]));
    let licence = text(&v["license_code"]).map(|c| self::licence(&c));
    card.detail("license_code", text(&v["license_code"]));
    let photos = v["photos"].as_array();
    card.detail("photos", photos.map(Vec::len));
    if let Some((url, attribution)) = photos
        .and_then(|p| p.first())
        .and_then(|p| photo(p, "medium"))
    {
        card.image = Some(url);
        card.detail("image_attribution", attribution);
    }
    let name = text(&v["user"]["name"]).or(observer);
    card.attribution = Some(match (licence, name) {
        (Some(l), Some(n)) => format!("(c) {n}, some rights reserved ({l}), via iNaturalist"),
        (None, Some(n)) => format!("(c) {n}, all rights reserved, via iNaturalist"),
        (_, None) => "via iNaturalist".into(),
    });
    // iNaturalist names the GBIF occurrence it published the observation as.
    for link in v["outlinks"].as_array().into_iter().flatten() {
        let gbif = text(&link["url"]).and_then(|u| super::gbif::from_uri(&u));
        if let (Some("GBIF"), Some(o @ super::gbif::Object::Occurrence(key))) =
            (link["source"].as_str(), gbif)
        {
            card.key(Key::new("gbif", &format!("occurrence/{key}")));
            card.link(Link::new("GBIF", super::gbif::page(o), VIA));
        }
    }
    Ok(Some(card))
}

/// A taxon. Its description is the Wikipedia summary iNaturalist shows, credited to Wikipedia.
pub fn parse_taxon(status: u16, raw: &[u8], id: u64) -> Result<Option<Card>> {
    let Some(v) = record(status, raw, id, "taxon")? else {
        return Ok(None);
    };
    let object = Object::Taxon(id);
    let mut card = Card::new("Taxon", identifier(object), page(object));
    card.name = Some(text(&v["name"]).context("no name")?);
    card.detail("rank", text(&v["rank"]));
    card.detail("preferred_common_name", text(&v["preferred_common_name"]));
    card.detail("iconic_taxon_name", text(&v["iconic_taxon_name"]));
    card.detail("observations_count", v["observations_count"].as_u64());
    card.detail("is_active", v["is_active"].as_bool());
    card.detail("extinct", v["extinct"].as_bool());
    card.detail(
        "wikipedia_url",
        text(&v["wikipedia_url"]).map(|u| u.replace(' ', "_")),
    );
    let ancestors: Vec<String> = v["ancestors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| text(&a["name"]))
        .collect();
    card.detail("ancestry", ancestors);
    card.detail("parent_id", v["parent_id"].as_u64());
    if let Some((url, attribution)) = photo(&v["default_photo"], "medium") {
        card.image = Some(url);
        card.detail("image_attribution", attribution);
    }
    card.description = text(&v["wikipedia_summary"])
        .and_then(|s| html_text(&s))
        .filter(|s| !s.is_empty());
    card.attribution = Some(match card.description {
        Some(_) => "iNaturalist; description from Wikipedia (CC BY-SA 4.0)".into(),
        None => "iNaturalist".into(),
    });
    Ok(Some(card))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("inaturalist", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).map(identifier)
    }

    #[test]
    fn link_forms() {
        for uri in [
            "https://www.inaturalist.org/observations/333069440",
            "https://inaturalist.org/observations/333069440/",
            "http://www.inaturalist.org/observations/333069440?locale=fr",
            "https://www.inaturalist.ca/observations/333069440",
            "https://www.naturalista.mx/observations/333069440",
            "https://mexico.inaturalist.org/observations/333069440",
            "https://www.argentinat.org/observations/333069440",
            "https://www.biodiversity4all.org/observations/333069440",
            "https://inaturalist.mma.gob.cl/observations/333069440",
            "https://api.inaturalist.org/v1/observations/333069440",
        ] {
            assert_eq!(
                id(uri).as_deref(),
                Some("inat:observation/333069440"),
                "{uri}"
            );
        }
        for uri in [
            "https://www.inaturalist.org/taxa/47219",
            "https://www.inaturalist.org/taxa/47219-Apis-mellifera",
            "https://www.inaturalist.nz/taxa/47219-Apis-mellifera",
            "https://api.inaturalist.org/v1/taxa/47219",
        ] {
            assert_eq!(id(uri).as_deref(), Some("inat:taxon/47219"), "{uri}");
        }
        let o = Object::Observation(333069440);
        assert_eq!(
            page(o),
            "https://www.inaturalist.org/observations/333069440"
        );
        assert_eq!(
            request(o).unwrap().url.as_str(),
            "https://api.inaturalist.org/v1/observations/333069440"
        );
        assert_eq!(
            request(Object::Taxon(47219)).unwrap().url.as_str(),
            "https://api.inaturalist.org/v1/taxa/47219"
        );
    }

    #[test]
    fn every_network_site_is_known() {
        let v: Value = serde_json::from_slice(&read("sites.json")).unwrap();
        for site in v["results"].as_array().unwrap() {
            let (host, _) = host_and_segments(site["url"].as_str().unwrap()).unwrap();
            assert!(SITES.contains(&host.as_str()), "{host}");
        }
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://www.inaturalist.org/observations/0",
            "https://www.inaturalist.org/observations/abc",
            "https://www.inaturalist.org/observations/7536b3e3-e1f0-4e5f-b77a-7c3522ae3d54",
            "https://www.inaturalist.org/observations/333069440/edit",
            "https://www.inaturalist.org/taxa/47219-",
            "https://www.inaturalist.org/taxa/Apis-mellifera",
            "https://www.inaturalist.org/photos/604606400",
            "https://api.inaturalist.org/v1/taxa/47219,1",
            "https://api.inaturalist.org/v2/observations/333069440",
            "https://www.inaturalist.com/observations/333069440",
            "https://www.gbif.org/occurrence/5938027305",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn observation() {
        let card = parse_observation(200, &read("observation-333069440.json.gz"), 333069440)
            .unwrap()
            .unwrap();
        assert_eq!(
            (card.type_name, card.id.as_str(), card.name.as_deref()),
            (
                "Observation",
                "inat:observation/333069440",
                Some("Brewer's Blackbird")
            )
        );
        assert_eq!(
            card.url,
            "https://www.inaturalist.org/observations/333069440"
        );
        let p = card.point.unwrap();
        assert_eq!((p.lat, p.lon), (33.195340253, -117.3847581279));
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "2026-01-01T23:09:23+00:00"
        );
        assert_eq!(card.details["taxon_name"], "Euphagus cyanocephalus");
        assert_eq!(card.details["quality_grade"], "research");
        assert_eq!(card.details["user_login"], "ramchop");
        assert_eq!(card.details["license_code"], "cc-by-nc");
        assert_eq!(
            card.details["place_guess"],
            "The Strand N, Oceanside, CA, US"
        );
        assert_eq!(card.details["photos"], 1);
        assert_eq!(
            card.image.as_deref(),
            Some("https://inaturalist-open-data.s3.amazonaws.com/photos/604606400/medium.jpg")
        );
        assert_eq!(
            card.details["image_attribution"],
            "(c) ramchop, some rights reserved (CC BY-NC)"
        );
        assert_eq!(
            card.attribution.as_deref(),
            Some("(c) ramchop, some rights reserved (CC BY-NC), via iNaturalist")
        );
        assert_eq!(card.keys, [Key::new("gbif", "occurrence/5938027305")]);
        assert_eq!(
            card.also_on[0].uri,
            "https://www.gbif.org/occurrence/5938027305"
        );

        assert_eq!(
            parse_observation(200, &read("observation-missing.json"), 999999999999).unwrap(),
            None
        );
        assert!(parse_observation(200, &read("observation-333069440.json.gz"), 333069441).is_err());
        assert!(parse_observation(429, b"", 333069440).is_err());
        assert!(parse_observation(502, b"", 333069440).is_err());
        assert!(parse_observation(200, b"{}", 333069440).is_err());
    }

    #[test]
    fn obscured_observation_has_no_point() {
        let card = parse_observation(
            200,
            &read("observation-195404405-obscured.json.gz"),
            195404405,
        )
        .unwrap()
        .unwrap();
        assert_eq!(card.point, None);
        assert!(!card.details.contains_key("place_guess"));
        assert_eq!(card.details["obscured"], true);
        assert_eq!(card.details["geoprivacy"], "obscured");
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "2023-12-31T21:16:30+00:00"
        );
        assert_eq!(card.keys, [Key::new("gbif", "occurrence/4993788366")]);
        // Any one of the three signals hides the point.
        for (field, value) in [
            ("obscured", Value::Bool(true)),
            ("geoprivacy", "private".into()),
            ("taxon_geoprivacy", "obscured".into()),
            ("obscured", Value::Null),
        ] {
            let mut v = serde_json::json!({
                "obscured": false, "geoprivacy": null, "taxon_geoprivacy": "open"
            });
            v[field] = value;
            assert!(!public_location(&v), "{field}");
        }
    }

    #[test]
    fn taxon() {
        let card = parse_taxon(200, &read("taxon-47219.json.gz"), 47219)
            .unwrap()
            .unwrap();
        assert_eq!(
            (card.type_name, card.id.as_str(), card.name.as_deref()),
            ("Taxon", "inat:taxon/47219", Some("Apis mellifera"))
        );
        assert_eq!(card.url, "https://www.inaturalist.org/taxa/47219");
        assert_eq!(card.details["rank"], "species");
        assert_eq!(card.details["preferred_common_name"], "Western Honey Bee");
        assert_eq!(card.details["iconic_taxon_name"], "Insecta");
        assert_eq!(
            card.details["wikipedia_url"],
            "https://en.wikipedia.org/wiki/Apis_mellifera"
        );
        assert_eq!(card.details["ancestry"][0], "Animalia");
        // The default photo is "all rights reserved".
        assert_eq!(card.image, None);
        let description = card.description.unwrap();
        assert!(
            description.starts_with("The western honey bee or European honey bee (Apis mellifera)"),
            "{description}"
        );
        assert_eq!(
            card.attribution.as_deref(),
            Some("iNaturalist; description from Wikipedia (CC BY-SA 4.0)")
        );
        assert_eq!(
            parse_taxon(200, &read("taxon-missing.json"), 999999999).unwrap(),
            None
        );
        assert!(parse_taxon(200, &read("taxon-47219.json.gz"), 47220).is_err());
        assert!(parse_taxon(429, b"", 47219).is_err());
    }

    #[test]
    fn photos_and_text() {
        let licensed = serde_json::json!({
            "license_code": "cc-by", "attribution": "(c) A",
            "url": "https://static.inaturalist.org/photos/1/square.jpg"
        });
        assert_eq!(
            photo(&licensed, "medium"),
            Some((
                "https://static.inaturalist.org/photos/1/medium.jpg".into(),
                Some("(c) A".into())
            ))
        );
        let reserved = serde_json::json!({
            "license_code": null, "url": "https://static.inaturalist.org/photos/1/square.jpg"
        });
        assert_eq!(photo(&reserved, "medium"), None);
        assert_eq!(licence("cc0"), "CC0");
    }
}
