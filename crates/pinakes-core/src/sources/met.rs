//! Works in The Metropolitan Museum of Art, read from its collection API by the object id in the
//! link.
//!
//! Every metmuseum.org link of one object (with or without a language prefix, the older
//! "collection online" form, and the API's own URL) is an alias of one identifier,
//! `met:{objectID}`. The API's data is CC0; images are shown only for public-domain works.
use super::kit::{
    fetch, host_and_segments, int, json, plain, text, web_url, wikidata_id, Card, Key, Request,
};
use super::{Context, Example, Source};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context as _, Result};
use async_trait::async_trait;

pub const SITE: &str = "The Met";

const API: &str = "https://collectionapi.metmuseum.org/public/collection/v1/objects";
const CREDIT: &str = "The Metropolitan Museum of Art";

/// An object id: decimal digits, read as a number (so leading zeros name the same object).
pub fn object_id(value: &str) -> Option<u64> {
    plain(value, 10, |c| c.is_ascii_digit())
        .then(|| value.parse().ok())
        .flatten()
        .filter(|id| *id > 0)
}

/// A language prefix of metmuseum.org, e.g. `fr` or `zh`.
fn language(segment: &str) -> bool {
    segment.len() == 2 && segment.bytes().all(|b| b.is_ascii_lowercase())
}

/// The object a metmuseum.org page or a collection API URL names.
pub fn from_uri(uri: &str) -> Option<u64> {
    let (host, segments) = host_and_segments(uri)?;
    let mut s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match host.as_str() {
        "metmuseum.org" => {
            if s.first().is_some_and(|l| language(l)) {
                s.remove(0);
            }
            match s.as_slice() {
                ["art", "collection", "search", id]
                | ["collection", "the-collection-online", "search", id] => object_id(id),
                _ => None,
            }
        }
        "collectionapi.metmuseum.org" => match s.as_slice() {
            ["public", "collection", "v1", "objects", id] => object_id(id),
            _ => None,
        },
        _ => None,
    }
}

pub fn identifier(id: u64) -> String {
    format!("met:{id}")
}

pub fn page(id: u64) -> String {
    format!("https://www.metmuseum.org/art/collection/search/{id}")
}

pub fn request(id: u64) -> Option<Request> {
    Request::get(&format!("{API}/{id}"))
}

/// The object of a root, `met:{objectID}` (the id without leading zeros).
pub fn from_root(uri: &str) -> Option<u64> {
    let id = uri.strip_prefix("met:")?;
    object_id(id).filter(|n| n.to_string() == id)
}

/// The object of a root or a metmuseum.org link.
pub fn target(uri: &str) -> Option<u64> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// An object, or `None` when the API does not know the id (404 "ObjectID not found").
pub fn parse(status: u16, raw: &[u8], id: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        int(&v["objectID"]).and_then(|n| u64::try_from(n).ok()) == Some(id),
        "the API answered for another object"
    );
    let mut card = Card::new("VisualArtwork", identifier(id), page(id));
    card.name = text(&v["title"]);
    let public_domain = v["isPublicDomain"].as_bool();
    if public_domain == Some(true) {
        // The web-sized image; `primaryImage` is the full-resolution original.
        card.image = text(&v["primaryImageSmall"])
            .or_else(|| text(&v["primaryImage"]))
            .filter(|u| web_url(u).is_some());
        card.attribution = Some(format!("{CREDIT}, CC0"));
    } else {
        card.attribution = Some(CREDIT.into());
    }
    for (name, field) in [
        ("artist", "artistDisplayName"),
        ("artist_bio", "artistDisplayBio"),
        ("date", "objectDate"),
        ("object_name", "objectName"),
        ("medium", "medium"),
        ("dimensions", "dimensions"),
        ("department", "department"),
        ("culture", "culture"),
        ("classification", "classification"),
        ("accession_number", "accessionNumber"),
        ("credit_line", "creditLine"),
        ("gallery_number", "GalleryNumber"),
    ] {
        card.detail(name, text(&v[field]));
    }
    card.detail("artist_wikidata", wikidata_id(&v["artistWikidata_URL"]));
    card.detail("begin_year", int(&v["objectBeginDate"]));
    card.detail("end_year", int(&v["objectEndDate"]));
    card.detail("public_domain", public_domain);
    if let Some(q) = wikidata_id(&v["objectWikidata_URL"]) {
        card.key(Key::new("wikidata", &q));
    }
    Ok(Some(card))
}

/// The source: The Met's collection API.
pub struct Met;

#[async_trait]
impl Source for Met {
    fn name(&self) -> &'static str {
        "met"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "met",
                uri: "https://www.metmuseum.org/art/collection/search/436535",
                card_type: "VisualArtwork",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "collectionapi.metmuseum.org",
                path: "/public/collection/v1/objects/436535",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "object-436535.json",
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
        uri.starts_with("met:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "artist",
            "artist_bio",
            "artist_wikidata",
            "date",
            "begin_year",
            "end_year",
            "object_name",
            "medium",
            "dimensions",
            "department",
            "culture",
            "classification",
            "accession_number",
            "credit_line",
            "gallery_number",
            "public_domain",
            "wikidata",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("collectionapi.metmuseum.org", 1.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.metmuseum.org/art/collection/search/436535",
            card_type: "VisualArtwork",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(id).context("object id")?).await?;
        Ok(match parse(f.status, &f.body, id)? {
            Some(card) => card.into_doc(cx.card(uri, self), SITE),
            None => cx.missing(uri, self),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;
    use serde_json::json;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("met", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).map(identifier)
    }

    #[test]
    fn every_link_form_of_one_object() {
        for uri in [
            "https://www.metmuseum.org/art/collection/search/436535",
            "https://metmuseum.org/art/collection/search/436535?img=0",
            "https://www.metmuseum.org/art/collection/search/436535?searchField=All&ft=van+gogh",
            "https://www.metmuseum.org/fr/art/collection/search/436535",
            "https://www.metmuseum.org/art/collection/search/0436535/",
            "http://www.metmuseum.org/collection/the-collection-online/search/436535",
            "https://collectionapi.metmuseum.org/public/collection/v1/objects/436535",
        ] {
            assert_eq!(id(uri).as_deref(), Some("met:436535"), "{uri}");
        }
        assert_eq!(
            request(436535).unwrap().url.as_str(),
            "https://collectionapi.metmuseum.org/public/collection/v1/objects/436535"
        );
        assert_eq!(
            page(436535),
            "https://www.metmuseum.org/art/collection/search/436535"
        );
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://www.metmuseum.org/art/collection/search/abc",
            "https://www.metmuseum.org/art/collection/search/0",
            "https://www.metmuseum.org/art/collection/search",
            "https://www.metmuseum.org/art/collection/search/436535/extra",
            "https://www.metmuseum.org/toah/works-of-art/1993.132",
            "https://www.metmuseum.org/french/art/collection/search/436535",
            "https://collectionapi.metmuseum.org/public/collection/v1/search?q=gogh",
            "https://example.org/art/collection/search/436535",
            "ftp://www.metmuseum.org/art/collection/search/436535",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn public_domain_painting() {
        let card = parse(200, &read("object-436535.json"), 436535)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "VisualArtwork");
        assert_eq!(card.id, "met:436535");
        assert_eq!(card.name.as_deref(), Some("Wheat Field with Cypresses"));
        assert_eq!(card.description, None);
        assert_eq!(
            card.image.as_deref(),
            Some("https://images.metmuseum.org/CRDImages/ep/web-large/DP-42549-001.jpg")
        );
        assert_eq!(card.start, None);
        assert_eq!(
            card.attribution.as_deref(),
            Some("The Metropolitan Museum of Art, CC0")
        );
        assert_eq!(card.keys, [Key::new("wikidata", "Q18689458")]);
        let d = &card.details;
        assert_eq!(d["artist"], "Vincent van Gogh");
        assert_eq!(d["artist_bio"], "Dutch, Zundert 1853–1890 Auvers-sur-Oise");
        assert_eq!(d["artist_wikidata"], "Q5582");
        assert_eq!(d["date"], "1889");
        assert_eq!(d["begin_year"], 1889);
        assert_eq!(d["medium"], "Oil on canvas");
        assert_eq!(d["accession_number"], "1993.132");
        assert_eq!(d["gallery_number"], "822");
        assert_eq!(d["public_domain"], true);
        assert!(!d.contains_key("culture"));

        let card = parse(200, &read("object-436532.json"), 436532)
            .unwrap()
            .unwrap();
        assert_eq!(card.id, "met:436532");
        assert!(card.image.is_some());
        assert_eq!(card.details["artist"], "Vincent van Gogh");
    }

    #[test]
    fn no_image_unless_public_domain() {
        let card = parse(200, &read("object-488221.json"), 488221)
            .unwrap()
            .unwrap();
        assert_eq!(card.name.as_deref(), Some("Gertrude Stein"));
        assert_eq!(card.image, None);
        assert_eq!(
            card.attribution.as_deref(),
            Some("The Metropolitan Museum of Art")
        );
        assert_eq!(card.details["public_domain"], false);

        let mut v: serde_json::Value = serde_json::from_slice(&read("object-488221.json")).unwrap();
        v["primaryImage"] = json!("https://images.metmuseum.org/x.jpg");
        let raw = serde_json::to_vec(&v).unwrap();
        assert_eq!(parse(200, &raw, 488221).unwrap().unwrap().image, None);
    }

    #[test]
    fn roots_and_aliases() {
        let m = Met;
        assert!(m.reads("met:436535"));
        assert_eq!(m.alias_root("met:436535"), None);
        assert!(!m.invalid("met:436535"));
        for uri in [
            "https://www.metmuseum.org/art/collection/search/436535",
            "https://www.metmuseum.org/fr/art/collection/search/436535?img=0",
            "https://collectionapi.metmuseum.org/public/collection/v1/objects/436535",
        ] {
            assert_eq!(m.alias_root(uri).as_deref(), Some("met:436535"), "{uri}");
        }
        for uri in ["met:0436535", "met:0", "met:x", "met:"] {
            assert!(m.invalid(uri), "{uri}");
            assert!(!m.reads(uri), "{uri}");
        }
        for example in m.examples() {
            assert!(m.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, id) in [
            ("object-436535.json", 436535),
            ("object-436532.json", 436532),
            ("object-488221.json", 488221),
        ] {
            let card = parse(200, &read(file), id).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Met, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let card = parse(200, &read("object-436535.json"), 436535)
            .unwrap()
            .unwrap();
        let doc = crate::sources::kit::checked(&Met, &card);
        assert_eq!(doc.attributes["wikidata"], "Q18689458");
        assert_eq!(doc.attributes["artist_wikidata"], "Q5582");
    }

    #[test]
    fn missing_and_failures() {
        assert_eq!(
            parse(404, &read("object-missing.json"), 999999999).unwrap(),
            None
        );
        assert!(parse(200, &read("object-436535.json"), 436532).is_err());
        assert!(parse(429, b"", 436535).is_err());
        assert!(parse(503, b"", 436535).is_err());
        assert!(parse(200, b"<html>", 436535).is_err());
    }
}
