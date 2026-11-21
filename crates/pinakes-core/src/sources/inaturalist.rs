//! Observations and taxa from the iNaturalist API (v1), read by the id in the link.
//!
//! Every site of the iNaturalist Network shares one database, so a link on any of them, and the
//! API URL, is an alias of one identifier: `inat:observation/{id}` or `inat:taxon/{id}`.
//!
//! The API asks for at most 60 requests a minute and under 10,000 a day, so the budget is one
//! request every 9 seconds. Licences are per observation and per photo; a photo is shown only in
//! the public domain or under a Creative Commons licence.
//! An observation whose location is obscured or private has public coordinates that are
//! randomised, so its card has no point.
use super::kit::html_text;
use super::kit::{
    float, host_and_segments, image_licence, image_url, int, json, text, time, Card, ImageLicence,
    Point, Request,
};
use super::{Example, Source};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

pub const SITE: &str = "iNaturalist";
const API: &str = "https://api.inaturalist.org/v1";

pub use super::biodiversity::INATURALIST_SITES as SITES;

/// The object a link names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Observation(u64),
    Taxon(u64),
}

/// A positive decimal id of at most 12 digits, read as a number (`kit::number`), so leading
/// zeros name the same id. A root holds the id without them (`from_root`).
fn number(value: &str) -> Option<u64> {
    super::kit::number(value, 12)
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

/// The object of a root (`inat:observation/{id}`, `inat:taxon/{id}`).
pub fn from_root(uri: &str) -> Option<Object> {
    let (kind, id) = uri.strip_prefix("inat:")?.split_once('/')?;
    let object = match kind {
        "observation" => Object::Observation(number(id)?),
        "taxon" => Object::Taxon(number(id)?),
        _ => return None,
    };
    (identifier(object) == uri).then_some(object)
}

/// The object of a root or an iNaturalist Network link.
pub fn target(uri: &str) -> Option<Object> {
    from_root(uri).or_else(|| from_uri(uri))
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

/// The address of a photo at the given size (`medium`, `large`) and its attribution, for a photo
/// in the public domain (`pd`) or under a Creative Commons licence (CC0, CC BY and its NC, ND and
/// SA forms). A photo with no licence code, or with any other one, has none.
fn photo(p: &Value, size: &str) -> Option<(String, Option<String>)> {
    let code = text(&p["license_code"])?;
    let shown = code.eq_ignore_ascii_case("pd")
        || matches!(
            image_licence(&code),
            Some(ImageLicence::Cc0 | ImageLicence::By | ImageLicence::ByWithTerms)
        );
    if !shown {
        return None;
    }
    let url = text(&p[format!("{size}_url").as_str()]).or_else(|| {
        let square = text(&p["url"])?;
        square
            .contains("/square.")
            .then(|| square.replacen("/square.", &format!("/{size}."), 1))
    })?;
    Some((image_url(&url)?, text(&p["attribution"])))
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
    card.detail(
        "observed_at",
        time(&v["time_observed_at"]).map(|t| t.to_rfc3339()),
    );
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
        (Some(l), Some(n)) if l == "CC0" => {
            format!("{n}, no rights reserved (CC0), via iNaturalist")
        }
        (Some(l), Some(n)) => format!("(c) {n}, some rights reserved ({l}), via iNaturalist"),
        (None, Some(n)) => format!("(c) {n}, all rights reserved, via iNaturalist"),
        (_, None) => "via iNaturalist".into(),
    });
    // iNaturalist names the GBIF occurrence it published the observation as.
    for link in v["outlinks"].as_array().into_iter().flatten() {
        let gbif = text(&link["url"]).and_then(|u| super::biodiversity::gbif_occurrence(&u));
        if let (Some("GBIF"), Some(key)) = (link["source"].as_str(), gbif) {
            card.detail("gbif_occurrence", key);
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

/// The source: iNaturalist observations and taxa, one request per card.
pub struct Inaturalist;

#[async_trait]
impl Source for Inaturalist {
    fn name(&self) -> &'static str {
        "inaturalist"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "inaturalist",
                uri: "https://www.inaturalist.org/observations/333069440",
                card_type: "Observation",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "api.inaturalist.org",
                path: "/v1/observations/333069440",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "observation-333069440.json.gz",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(target(uri)?);
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("inat:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            // Observations
            "place_guess",
            "positional_accuracy_m",
            "observed_at",
            "observed_on",
            "quality_grade",
            "taxon_id",
            "taxon_name",
            "taxon_rank",
            "common_name",
            "user_login",
            "obscured",
            "geoprivacy",
            "taxon_geoprivacy",
            "uuid",
            "license_code",
            "photos",
            "image_attribution",
            // Taxa
            "rank",
            "preferred_common_name",
            "iconic_taxon_name",
            "observations_count",
            "is_active",
            "extinct",
            "wikipedia_url",
            "ancestry",
            "parent_id",
            // Keys
            "gbif_occurrence",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("api.inaturalist.org", 0.11)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.inaturalist.org/observations/333069440",
                card_type: "Observation",
            },
            Example {
                uri: "https://www.inaturalist.org/taxa/47219-Apis-mellifera",
                card_type: "Taxon",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(object) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = super::kit::fetch(cx, &request(object).context("no request")?).await?;
        Ok(
            match crate::resolver::html::blocking(move || parse(f.status, &f.body, object))
                .await??
            {
                Some(card) => card.into_doc(cx.card(uri, self), SITE),
                None => cx.missing(uri, self),
            },
        )
    }
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
        // An observation can still change: its time is a fact, not a start that makes it final.
        assert_eq!(card.start, None);
        assert_eq!(card.details["observed_at"], "2026-01-01T23:09:23+00:00");
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
        assert!(card.keys.is_empty(), "another service's object is a fact");
        assert_eq!(card.details["gbif_occurrence"], 5938027305u64);

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
        assert_eq!(card.details["observed_at"], "2023-12-31T21:16:30+00:00");
        assert_eq!(card.details["gbif_occurrence"], 4993788366u64);
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
    fn a_waived_licence_reserves_no_rights() {
        let raw = read("observation-333069440.json.gz");
        let mut v: Value = serde_json::from_slice(&raw).unwrap();
        v["results"][0]["license_code"] = "cc0".into();
        let raw = serde_json::to_vec(&v).unwrap();
        let card = parse_observation(200, &raw, 333069440).unwrap().unwrap();
        assert_eq!(
            card.attribution.as_deref(),
            Some("ramchop, no rights reserved (CC0), via iNaturalist")
        );
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

    /// The image of the recorded observation's card after `change` edits its first photo.
    fn image_with(change: impl Fn(&mut Value)) -> Option<String> {
        let mut v: Value = serde_json::from_slice(&read("observation-333069440.json.gz")).unwrap();
        change(&mut v["results"][0]["photos"][0]);
        let raw = serde_json::to_vec(&v).unwrap();
        let card = parse_observation(200, &raw, 333069440).unwrap().unwrap();
        assert_eq!(
            card.image.is_some(),
            card.details.contains_key("image_attribution"),
            "a credit only with its image"
        );
        card.image
    }

    #[test]
    fn a_photo_is_shown_only_in_the_public_domain_or_under_a_creative_commons_licence() {
        let medium = "https://inaturalist-open-data.s3.amazonaws.com/photos/604606400/medium.jpg";
        for code in [
            "pd",
            "cc0",
            "cc-by",
            "cc-by-nc",
            "cc-by-sa",
            "cc-by-nd",
            "cc-by-nc-sa",
            "cc-by-nc-nd",
        ] {
            assert_eq!(
                image_with(|p| p["license_code"] = code.into()).as_deref(),
                Some(medium),
                "{code}"
            );
        }
        // Codes that are no Creative Commons licence, and one iNaturalist does not have.
        let others = ["gfdl", "all-rights-reserved"]
            .map(|code| image_with(|p| p["license_code"] = code.into()));
        assert!(others.iter().all(Option::is_none), "{others:?}");
        // A taxon's default photo follows the same rule.
        let mut v: Value = serde_json::from_slice(&read("taxon-47219.json.gz")).unwrap();
        v["results"][0]["default_photo"]["license_code"] = "gfdl".into();
        let raw = serde_json::to_vec(&v).unwrap();
        assert_eq!(parse_taxon(200, &raw, 47219).unwrap().unwrap().image, None);
    }

    #[test]
    fn a_photo_address_that_is_not_an_https_url_is_no_image() {
        let data = "data:image/png;base64,iVBORw0KGgo=";
        // In the size's own field, and in the square one the size's address is made from.
        let images = [
            image_with(|p| p["medium_url"] = data.into()),
            image_with(|p| p["medium_url"] = "http://static.inaturalist.org/1/medium.jpg".into()),
            image_with(|p| p["url"] = format!("{data}/square.png").into()),
            image_with(|p| p["url"] = "http://static.inaturalist.org/1/square.jpg".into()),
            image_with(|p| p["url"] = "photos/1/square.jpg".into()),
        ];
        assert!(images.iter().all(Option::is_none), "{images:?}");
    }

    #[test]
    fn roots_and_aliases() {
        let i = Inaturalist;
        for uri in ["inat:observation/333069440", "inat:taxon/47219"] {
            assert!(i.reads(uri), "{uri}");
            assert_eq!(i.alias_root(uri), None, "{uri}");
            assert!(!i.invalid(uri), "{uri}");
            assert_eq!(target(uri).map(identifier).as_deref(), Some(uri));
        }
        assert_eq!(
            i.alias_root("https://www.argentinat.org/observations/333069440")
                .as_deref(),
            Some("inat:observation/333069440")
        );
        assert_eq!(
            i.alias_root("https://www.inaturalist.org/taxa/47219-Apis-mellifera")
                .as_deref(),
            Some("inat:taxon/47219")
        );
        for uri in [
            "inat:observation/0333069440",
            "inat:observation/7536b3e3-e1f0-4e5f-b77a-7c3522ae3d54",
            "inat:taxon/47219-Apis-mellifera",
            "inat:user/1",
            "inat:47219",
        ] {
            assert!(i.invalid(uri), "{uri}");
            assert!(!i.reads(uri), "{uri}");
        }
        for example in i.examples() {
            assert!(i.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        let cards = [
            parse_observation(200, &read("observation-333069440.json.gz"), 333069440),
            parse_observation(
                200,
                &read("observation-195404405-obscured.json.gz"),
                195404405,
            ),
            parse_taxon(200, &read("taxon-47219.json.gz"), 47219),
        ];
        for card in cards {
            let card = card.unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Inaturalist, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let card = parse_observation(200, &read("observation-333069440.json.gz"), 333069440)
            .unwrap()
            .unwrap();
        let doc = card.into_doc(
            ResourceMeta::new("inat:observation/333069440", "inaturalist", 60),
            SITE,
        );
        assert_eq!(doc.attributes["gbif_occurrence"], "5938027305");
        assert_eq!(doc.site, "iNaturalist");
    }
}
