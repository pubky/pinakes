//! Artworks of the Art Institute of Chicago, read from its public API by the id in the link.
//!
//! Every artic.edu artwork link (with or without the title slug) and the API's own URL is an
//! alias of one identifier, `artic:{id}`. The API asks every client to name itself in an
//! `AIC-User-Agent` header. Its IIIF image server answers 403 to any request without that
//! header, so a browser can't load the images: cards carry no image, only the `image_id` fact.
//!
//! The API's data is CC0 except the long `description`, which is CC BY 4.0, so a card prefers the
//! CC0 `short_description`.
use super::kit::{fetch, host_and_segments, html_text, int, json, number, text, Card, Request};
use super::{Context, Example, Source};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context as _, Result};
use async_trait::async_trait;

pub const SITE: &str = "Art Institute of Chicago";

const API: &str = "https://api.artic.edu/api/v1/artworks";
const CREDIT: &str = "Art Institute of Chicago";
/// The fields a card reads (the API answers with every field unless asked for fewer).
pub const FIELDS: &str = "id,title,artist_display,date_display,date_start,date_end,\
medium_display,dimensions,credit_line,place_of_origin,department_title,artwork_type_title,\
is_public_domain,image_id,description,short_description,main_reference_number,copyright_notice";

/// An artwork id: decimal digits, read as a number.
pub fn artwork_id(value: &str) -> Option<u64> {
    number(value, 10)
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

/// The API record, limited to `FIELDS`, with the `AIC-User-Agent` header the API asks for (the
/// operator's contact User-Agent).
pub fn request(id: u64, contact: Option<&str>) -> Option<Request> {
    let request = Request::get(&format!("{API}/{id}?fields={FIELDS}"))?;
    Some(match contact {
        Some(contact) => request.header(
            "AIC-User-Agent",
            crate::resolver::egress::user_agent(contact),
        ),
        None => request,
    })
}

/// The artwork of a root, `artic:{id}` (the id without leading zeros).
pub fn from_root(uri: &str) -> Option<u64> {
    let id = uri.strip_prefix("artic:")?;
    artwork_id(id).filter(|n| n.to_string() == id)
}

/// The artwork of a root or an artic.edu link.
pub fn target(uri: &str) -> Option<u64> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// An artwork, or `None` when the API does not know the id (404 "Not found").
pub fn parse(status: u16, raw: &[u8], id: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let d = &v["data"];
    ensure!(
        int(&d["id"]).and_then(|n| u64::try_from(n).ok()) == Some(id),
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
    Ok(Some(card))
}

/// The source: the Art Institute of Chicago's public API.
pub struct Artic;

#[async_trait]
impl Source for Artic {
    fn name(&self) -> &'static str {
        "artic"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "artic",
                uri: "https://www.artic.edu/artworks/27992",
                card_type: "VisualArtwork",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "api.artic.edu",
                path: "/api/v1/artworks/27992",
                query: &[],
                header: Some("AIC-User-Agent"),
                content_type: "application/json",
                file: "artwork-27992.json",
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
        uri.starts_with("artic:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "artist_display",
            "date_display",
            "begin_year",
            "end_year",
            "medium",
            "dimensions",
            "place_of_origin",
            "department",
            "artwork_type",
            "main_reference_number",
            "credit_line",
            "copyright_notice",
            "public_domain",
            "image_id",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("api.artic.edu", 1.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.artic.edu/artworks/27992",
            card_type: "VisualArtwork",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(id, cx.contact()).context("artwork id")?).await?;
        Ok(
            match crate::resolver::html::blocking(move || parse(f.status, &f.body, id)).await?? {
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
        let r = request(27992, Some("ops@example.org")).unwrap();
        assert_eq!(
            r.url.as_str(),
            format!("https://api.artic.edu/api/v1/artworks/27992?fields={FIELDS}")
        );
        assert_eq!(
            r.headers,
            [(
                "AIC-User-Agent",
                format!(
                    "pinakes/{} (+https://pubky.app; ops@example.org)",
                    env!("CARGO_PKG_VERSION")
                )
            )]
        );
        // Without a contact there is nothing to name: no header.
        assert!(request(27992, None).unwrap().headers.is_empty());
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
    fn roots_and_aliases() {
        let a = Artic;
        assert!(a.reads("artic:27992"));
        assert_eq!(a.alias_root("artic:27992"), None);
        assert!(!a.invalid("artic:27992"));
        for uri in [
            "https://www.artic.edu/artworks/27992/a-sunday-on-la-grande-jatte-1884",
            "https://api.artic.edu/api/v1/artworks/27992",
        ] {
            assert_eq!(a.alias_root(uri).as_deref(), Some("artic:27992"), "{uri}");
        }
        for uri in ["artic:027992", "artic:0", "artic:abc", "artic:"] {
            assert!(a.invalid(uri), "{uri}");
            assert!(!a.reads(uri), "{uri}");
        }
        assert!(!a.invalid("https://www.artic.edu/artworks/abc"));
        for example in a.examples() {
            assert!(a.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, id) in [
            ("artwork-27992.json", 27992),
            ("artwork-28560.json", 28560),
            ("artwork-111628.json", 111628),
        ] {
            let card = parse(200, &read(file), id).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Artic, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
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
