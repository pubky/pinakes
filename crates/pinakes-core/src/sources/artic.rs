//! Artworks of the Art Institute of Chicago, read from its public API by the id in the link.
//!
//! Every artic.edu artwork link (with or without the title slug) and the API's own URL is an
//! alias of one identifier, `artic:{id}`. The API asks every client to name itself in an
//! `AIC-User-Agent` header. Its IIIF image server answers 403 to any request without that
//! header, so a browser can't load the images: cards carry no image, only the `image_id` fact.
//!
//! The API's data is CC0 except the long `description`, which is CC BY 4.0, so a card prefers the
//! CC0 `short_description`.
use super::kit::html_text;
use super::kit::{host_and_segments, int, json, plain, text, Card, Request};
use anyhow::{ensure, Result};

const API: &str = "https://api.artic.edu/api/v1/artworks";
const CREDIT: &str = "Art Institute of Chicago";
/// The fields a card reads (the API answers with every field unless asked for fewer).
pub const FIELDS: &str = "id,title,artist_display,date_display,date_start,date_end,\
medium_display,dimensions,credit_line,place_of_origin,department_title,artwork_type_title,\
is_public_domain,image_id,description,short_description,main_reference_number,copyright_notice";
/// The value of the `AIC-User-Agent` header the API asks for: who we are and how to reach us.
pub const AIC_USER_AGENT: &str = concat!(
    "pinakes/",
    env!("CARGO_PKG_VERSION"),
    " (+https://pubky.app; info@synonym.to)"
);

/// An artwork id: decimal digits, read as a number.
pub fn artwork_id(value: &str) -> Option<u64> {
    plain(value, 10, |c| c.is_ascii_digit())
        .then(|| value.parse().ok())
        .flatten()
        .filter(|id| *id > 0)
}

/// The artwork an artic.edu page or an API URL names.
pub fn from_uri(uri: &str) -> Option<u64> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (host.as_str(), s.as_slice()) {
        ("artic.edu", ["artworks", id] | ["artworks", id, _]) => artwork_id(id),
        ("api.artic.edu", ["api", "v1", "artworks", id]) => artwork_id(id),
        _ => None,
    }
}

pub fn identifier(id: u64) -> String {
    format!("artic:{id}")
}

pub fn page(id: u64) -> String {
    format!("https://www.artic.edu/artworks/{id}")
}

/// The API record, limited to `FIELDS`, with the `AIC-User-Agent` header.
pub fn request(id: u64) -> Option<Request> {
    Some(
        Request::get(&format!("{API}/{id}?fields={FIELDS}"))?
            .header("AIC-User-Agent", AIC_USER_AGENT),
    )
}

/// An artwork, or `None` when the API does not know the id (404 "Not found").
pub fn parse(status: u16, raw: &[u8], id: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let d = &v["data"];
    ensure!(
        int(&d["id"]) == Some(id as i64),
        "the API answered for another artwork"
    );
    let mut card = Card::new("VisualArtwork", identifier(id), page(id));
    card.name = text(&d["title"]);
    let short = text(&d["short_description"]).and_then(|s| html_text(&s));
    let long = || text(&d["description"]).and_then(|s| html_text(&s));
    let cc_by = short.is_none() && long().is_some();
    card.description = short.or_else(long);
    let public_domain = d["is_public_domain"].as_bool();
    let mut credit = CREDIT.to_owned();
    if public_domain == Some(true) {
        credit.push_str(", CC0 1.0");
    }
    if cc_by {
        credit.push_str("; description CC BY 4.0");
    }
    card.attribution = Some(credit);
    for (name, field) in [
        ("artist_display", "artist_display"),
        ("date_display", "date_display"),
        ("medium", "medium_display"),
        ("dimensions", "dimensions"),
        ("place_of_origin", "place_of_origin"),
        ("department", "department_title"),
        ("artwork_type", "artwork_type_title"),
        ("main_reference_number", "main_reference_number"),
        ("credit_line", "credit_line"),
        ("copyright_notice", "copyright_notice"),
        ("image_id", "image_id"),
    ] {
        card.detail(name, text(&d[field]));
    }
    card.detail("begin_year", int(&d["date_start"]));
    card.detail("end_year", int(&d["date_end"]));
    card.detail("public_domain", public_domain);
    card.detail("license_text", text(&v["info"]["license_text"]));
    Ok(Some(card))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("artic", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).map(identifier)
    }

    #[test]
    fn every_link_form_of_one_artwork() {
        for uri in [
            "https://www.artic.edu/artworks/27992",
            "https://www.artic.edu/artworks/27992/a-sunday-on-la-grande-jatte-1884",
            "https://artic.edu/artworks/27992/?q=seurat",
            "http://www.artic.edu/artworks/027992",
            "https://api.artic.edu/api/v1/artworks/27992",
            "https://api.artic.edu/api/v1/artworks/27992?fields=id,title",
        ] {
            assert_eq!(id(uri).as_deref(), Some("artic:27992"), "{uri}");
        }
        assert_eq!(page(27992), "https://www.artic.edu/artworks/27992");
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://www.artic.edu/artworks/abc",
            "https://www.artic.edu/artworks/0",
            "https://www.artic.edu/artworks",
            "https://www.artic.edu/artworks/27992/slug/extra",
            "https://www.artic.edu/artists/35577",
            "https://api.artic.edu/api/v1/artworks/27992/manifest.json",
            "https://api.artic.edu/api/v1/artworks/search?q=cats",
            "https://artic.edu.example.org/artworks/27992",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn request_names_fields_and_sends_the_header() {
        let r = request(27992).unwrap();
        assert_eq!(
            r.url.as_str(),
            format!("https://api.artic.edu/api/v1/artworks/27992?fields={FIELDS}")
        );
        assert_eq!(
            r.headers,
            [(
                "AIC-User-Agent",
                "pinakes/0.1.0 (+https://pubky.app; info@synonym.to)".to_owned()
            )]
        );
    }

    #[test]
    fn public_domain_painting() {
        let card = parse(200, &read("artwork-27992.json"), 27992)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "VisualArtwork");
        assert_eq!(card.id, "artic:27992");
        assert_eq!(card.url, "https://www.artic.edu/artworks/27992");
        assert_eq!(
            card.name.as_deref(),
            Some("A Sunday on La Grande Jatte — 1884")
        );
        assert!(card
            .description
            .as_deref()
            .unwrap()
            .starts_with("For his largest and best-known painting, Georges Seurat"));
        assert_eq!(card.image, None, "the IIIF server refuses browsers");
        assert_eq!(
            card.details["image_id"],
            "2d484387-2509-5e8e-2c43-22f9981972eb"
        );
        assert_eq!(card.start, None);
        assert_eq!(
            card.attribution.as_deref(),
            Some("Art Institute of Chicago, CC0 1.0")
        );
        let d = &card.details;
        assert_eq!(d["artist_display"], "Georges Seurat (French, 1859–1891)");
        assert_eq!(d["date_display"], "1884–86, border added 1888–89");
        assert_eq!(d["begin_year"], 1884);
        assert_eq!(d["end_year"], 1886);
        assert_eq!(d["main_reference_number"], "1926.224");
        assert_eq!(d["department"], "Painting and Sculpture of Europe");
        assert_eq!(d["public_domain"], true);
        assert!(!d.contains_key("copyright_notice"));
        assert!(d["license_text"]
            .as_str()
            .unwrap()
            .contains("Creative Commons Zero (CC0) 1.0"));

        let card = parse(200, &read("artwork-28560.json"), 28560)
            .unwrap()
            .unwrap();
        assert_eq!(card.name.as_deref(), Some("The Bedroom"));
        assert_eq!(card.details["place_of_origin"], "Saint-Rémy-de-Provence");
        assert!(card.details.contains_key("image_id"));
    }

    #[test]
    fn copyrighted_work() {
        let card = parse(200, &read("artwork-111628.json"), 111628)
            .unwrap()
            .unwrap();
        assert_eq!(card.name.as_deref(), Some("Nighthawks"));
        assert_eq!(card.image, None);
        assert_eq!(
            card.details["image_id"],
            "831a05de-d3f6-f4fa-a460-23008dd58dda"
        );
        assert_eq!(
            card.attribution.as_deref(),
            Some("Art Institute of Chicago")
        );
        assert!(card.description.as_deref().unwrap().starts_with(
            "One of the best-known images of 20th-century art, Edward Hopper's Nighthawks depicts"
        ));
    }

    #[test]
    fn long_description_is_credited() {
        let mut v: serde_json::Value = serde_json::from_slice(&read("artwork-28560.json")).unwrap();
        v["data"]["short_description"] = serde_json::Value::Null;
        let card = parse(200, &serde_json::to_vec(&v).unwrap(), 28560)
            .unwrap()
            .unwrap();
        let text = card.description.unwrap();
        assert!(text.starts_with("Perhaps the most famous depiction of a bedroom"));
        assert!(text.contains("restful image.\nThe painting in the Art Institute’s"));
        assert!(!text.contains('<'));
        assert_eq!(
            card.attribution.as_deref(),
            Some("Art Institute of Chicago, CC0 1.0; description CC BY 4.0")
        );
    }

    #[test]
    fn missing_and_failures() {
        assert_eq!(
            parse(404, &read("artwork-missing.json"), 999999999).unwrap(),
            None
        );
        assert!(parse(200, &read("artwork-27992.json"), 28560).is_err());
        assert!(parse(429, b"", 27992).is_err());
        assert!(parse(500, b"", 27992).is_err());
        assert!(parse(200, b"{}", 27992).is_err());
    }
}
