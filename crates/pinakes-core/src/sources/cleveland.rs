//! Artworks of the Cleveland Museum of Art, read from its Open Access API by the accession number
//! or the numeric id in the link.
//!
//! The identifier is `cleveland:{accession number}` (in lower case, e.g. `cleveland:1958.39`):
//! the museum's own pages are named by it, so it comes from most links alone. clevelandart.org
//! and the API also accept the numeric id (`/art/135382`); such a link is identified by the
//! accession number in the API's answer, and becomes an alias of that root once its card is
//! stored.
//!
//! The API's data is CC0; images are shown only for works the API marks CC0.
use super::kit::wikidata_id;
use super::kit::{
    fetch, host_and_segments, html_text, image_licence, image_url, int, json, plain, text, texts,
    Card, ImageLicence, Key, Request,
};
use super::{Context, Example, Source};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context as _, Result};
use async_trait::async_trait;

pub const SITE: &str = "Cleveland Museum of Art";

const API: &str = "https://openaccess-api.clevelandart.org/api/artworks";
const CREDIT: &str = "Cleveland Museum of Art";

/// What a link names: an artwork by accession number or by numeric id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Artwork {
    /// Digits and lower-case letters in dot-separated parts, e.g. `1958.39` or `1916.1511.a`.
    Accession(String),
    Id(u64),
}

/// An accession number in lower case (the API answers 404 for upper-case letters).
pub fn accession(value: &str) -> Option<String> {
    let value = value.to_ascii_lowercase();
    let parts: Vec<&str> = value.split('.').collect();
    (parts.len() >= 2
        && parts
            .iter()
            .all(|p| plain(p, 12, |c| c.is_ascii_digit() || c.is_ascii_lowercase()))
        && parts[0].bytes().all(|b| b.is_ascii_digit())
        && value.len() <= 32)
        .then_some(value)
}

fn artwork(value: &str) -> Option<Artwork> {
    if plain(value, 10, |c| c.is_ascii_digit()) {
        return value.parse().ok().filter(|id| *id > 0).map(Artwork::Id);
    }
    accession(value).map(Artwork::Accession)
}

/// The artwork a clevelandart.org page or an API URL names.
pub fn from_uri(uri: &str) -> Option<Artwork> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (host.as_str(), s.as_slice()) {
        ("clevelandart.org", ["art", id]) => artwork(id),
        ("openaccess-api.clevelandart.org", ["api", "artworks", id]) => artwork(id),
        _ => None,
    }
}

/// The identifier of an accession number.
fn root(accession: &str) -> String {
    format!("cleveland:{accession}")
}

/// The identifier, or `None` for a numeric id (its accession number comes from the answer).
pub fn identifier(artwork: &Artwork) -> Option<String> {
    match artwork {
        Artwork::Accession(a) => Some(root(a)),
        Artwork::Id(_) => None,
    }
}

/// The artwork's page on clevelandart.org, by its accession number.
pub fn page(accession: &str) -> String {
    format!("https://www.clevelandart.org/art/{accession}")
}

/// The API record; the API takes either form.
pub fn request(artwork: &Artwork) -> Option<Request> {
    Request::get(&match artwork {
        Artwork::Accession(a) => format!("{API}/{a}"),
        Artwork::Id(id) => format!("{API}/{id}"),
    })
}

/// The artwork of a root, `cleveland:{accession number}` (already in lower case).
pub fn from_root(uri: &str) -> Option<Artwork> {
    let a = uri.strip_prefix("cleveland:")?;
    accession(a).filter(|n| n == a).map(Artwork::Accession)
}

/// The artwork of a root or a clevelandart.org link.
pub fn target(uri: &str) -> Option<Artwork> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// An artwork, or `None` when the API does not know it (404 "Artwork not found").
pub fn parse(status: u16, raw: &[u8], artwork: &Artwork) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let d = &v["data"];
    let acc = d["accession_number"]
        .as_str()
        .and_then(accession)
        .context("no accession number")?;
    let id = int(&d["id"]).filter(|id| *id > 0).context("no id")?;
    ensure!(
        match artwork {
            Artwork::Accession(a) => *a == acc,
            Artwork::Id(n) => i64::try_from(*n).ok() == Some(id),
        },
        "the API answered for another artwork"
    );
    let mut card = Card::new("VisualArtwork", root(&acc), page(&acc));
    card.name = text(&d["title"]);
    card.description = text(&d["description"])
        .or_else(|| text(&d["wall_description"]))
        .and_then(|s| html_text(&s));
    let license = text(&d["share_license_status"]);
    if license.as_deref().and_then(image_licence) == Some(ImageLicence::Cc0) {
        card.image = text(&d["images"]["web"]["url"]).and_then(|u| image_url(&u));
        card.attribution = Some(format!("{CREDIT}, CC0"));
    } else {
        card.attribution = Some(CREDIT.into());
    }
    let creators: Vec<String> = d["creators"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| text(&c["description"]))
        .collect();
    card.detail("creators", creators);
    for (name, field) in [
        ("creation_date", "creation_date"),
        ("original_title", "title_in_original_language"),
        ("technique", "technique"),
        ("measurements", "measurements"),
        ("department", "department"),
        ("type", "type"),
        ("collection", "collection"),
        ("current_location", "current_location"),
        ("credit_line", "creditline"),
        ("copyright", "copyright"),
    ] {
        card.detail(name, text(&d[field]));
    }
    card.detail("culture", texts(&d["culture"]));
    card.detail("accession_number", acc.as_str());
    card.detail(
        "former_accession_numbers",
        texts(&d["former_accession_numbers"]),
    );
    card.detail("object_id", id);
    card.detail("begin_year", int(&d["creation_date_earliest"]));
    card.detail("end_year", int(&d["creation_date_latest"]));
    card.detail("share_license_status", license);
    let external = &d["external_resources"];
    if let [q] = external["wikidata"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if let Some(q) = wikidata_id(q) {
            card.key(Key::new("wikidata", &q));
        }
    }
    Ok(Some(card))
}

/// The source: the Cleveland Museum of Art's Open Access API.
pub struct Cleveland;

#[async_trait]
impl Source for Cleveland {
    fn name(&self) -> &'static str {
        "cleveland"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "cleveland",
                uri: "https://www.clevelandart.org/art/1958.39",
                card_type: "VisualArtwork",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "openaccess-api.clevelandart.org",
                path: "/api/artworks/1958.39",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "artwork-1958.39.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    /// The root of an accession-number link; a numeric-id link is named by the fetch.
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?)?;
        (root != uri).then_some(root)
    }
    fn root_page(&self, root: &str) -> Option<String> {
        match from_root(root)? {
            Artwork::Accession(a) => Some(page(&a)),
            Artwork::Id(_) => None,
        }
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("cleveland:") && from_root(uri).is_none()
    }
    /// The work's Wikidata item, as the museum's record names it.
    fn own_ids(&self) -> &'static [super::kit::OwnId] {
        &[super::kit::OwnId::Wikidata]
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "creators",
            "creation_date",
            "begin_year",
            "end_year",
            "original_title",
            "technique",
            "measurements",
            "department",
            "type",
            "collection",
            "current_location",
            "credit_line",
            "copyright",
            "culture",
            "accession_number",
            "former_accession_numbers",
            "object_id",
            "share_license_status",
            "wikidata",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("openaccess-api.clevelandart.org", 1.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.clevelandart.org/art/1958.39",
            card_type: "VisualArtwork",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(artwork) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&artwork).context("artwork")?).await?;
        let read = artwork.clone();
        Ok(
            match crate::resolver::html::blocking(move || parse(f.status, &f.body, &read)).await?? {
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
    use serde_json::{json, Value};

    fn read(name: &str) -> Vec<u8> {
        fixture::read("cleveland", name)
    }
    fn acc(a: &str) -> Artwork {
        Artwork::Accession(a.into())
    }

    #[test]
    fn every_link_form_of_one_artwork() {
        for uri in [
            "https://www.clevelandart.org/art/1958.39",
            "https://clevelandart.org/art/1958.39",
            "https://www.clevelandart.org/art/1958.39/?utm_source=x",
            "https://openaccess-api.clevelandart.org/api/artworks/1958.39",
            "https://openaccess-api.clevelandart.org/api/artworks/1958.39?indent=1",
        ] {
            let a = from_uri(uri);
            assert_eq!(a, Some(acc("1958.39")), "{uri}");
            assert_eq!(
                identifier(&a.unwrap()).as_deref(),
                Some("cleveland:1958.39")
            );
        }
        assert_eq!(
            from_uri("https://www.clevelandart.org/art/1916.1511.A"),
            Some(acc("1916.1511.a"))
        );
        for uri in [
            "https://www.clevelandart.org/art/135382",
            "https://openaccess-api.clevelandart.org/api/artworks/135382",
        ] {
            assert_eq!(from_uri(uri), Some(Artwork::Id(135382)), "{uri}");
        }
        assert_eq!(identifier(&Artwork::Id(135382)), None);
        assert_eq!(
            request(&acc("1958.39")).unwrap().url.as_str(),
            "https://openaccess-api.clevelandart.org/api/artworks/1958.39"
        );
        assert_eq!(
            request(&Artwork::Id(135382)).unwrap().url.as_str(),
            "https://openaccess-api.clevelandart.org/api/artworks/135382"
        );
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://www.clevelandart.org/art/1958.39.",
            "https://www.clevelandart.org/art/0",
            "https://www.clevelandart.org/art/1958.39-the-red-kerchief",
            "https://www.clevelandart.org/art/1958..39",
            "https://www.clevelandart.org/art/.39",
            "https://www.clevelandart.org/art/a.39",
            "https://www.clevelandart.org/art/1958.39/extra",
            "https://www.clevelandart.org/art/collection/search",
            "https://openaccess-api.clevelandart.org/api/artworks/",
            "https://openaccess-api.clevelandart.org/api/creators/1844",
            "https://example.org/art/1958.39",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn cc0_painting() {
        let card = parse(200, &read("artwork-1958.39.json"), &acc("1958.39"))
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "VisualArtwork");
        assert_eq!(card.id, "cleveland:1958.39");
        assert_eq!(card.url, "https://www.clevelandart.org/art/1958.39");
        assert_eq!(card.name.as_deref(), Some("The Red Kerchief"));
        assert!(card
            .description
            .as_deref()
            .unwrap()
            .starts_with("This painting depicts Monet's first wife, Camille"));
        assert_eq!(
            card.image.as_deref(),
            Some("https://openaccess-cdn.clevelandart.org/1958.39/1958.39_web.jpg")
        );
        assert_eq!(card.start, None);
        assert_eq!(
            card.attribution.as_deref(),
            Some("Cleveland Museum of Art, CC0")
        );
        assert_eq!(card.keys, [Key::new("wikidata", "Q50772913")]);
        assert_eq!(card.details["object_id"], 135382);
        let d = &card.details;
        assert_eq!(d["creators"][0], "Claude Monet (French, 1840–1926)");
        assert_eq!(d["creation_date"], "c. 1868–73");
        assert_eq!(d["begin_year"], 1868);
        assert_eq!(d["end_year"], 1873);
        assert_eq!(d["original_title"], "Femme à la capeline rouge");
        assert_eq!(d["culture"][0], "France, 19th century");
        assert_eq!(d["technique"], "oil on fabric");
        assert_eq!(d["type"], "Painting");
        assert_eq!(d["accession_number"], "1958.39");
        assert_eq!(d["object_id"], 135382);
        assert_eq!(d["credit_line"], "Bequest of Leonard C. Hanna Jr.");
        assert_eq!(d["share_license_status"], "CC0");
    }

    #[test]
    fn numeric_id_gives_the_same_card() {
        let by_id = parse(200, &read("artwork-135382.json"), &Artwork::Id(135382))
            .unwrap()
            .unwrap();
        let by_acc = parse(200, &read("artwork-1958.39.json"), &acc("1958.39"))
            .unwrap()
            .unwrap();
        assert_eq!(by_id, by_acc);

        let card = parse(200, &read("artwork-1958.31.json"), &acc("1958.31"))
            .unwrap()
            .unwrap();
        assert_eq!(card.name.as_deref(), Some("Adeline Ravoux"));
        assert_eq!(card.details["former_accession_numbers"][0], "1642.43");
        assert_eq!(card.keys, [Key::new("wikidata", "Q26221230")]);
    }

    /// The work's Wikidata item joins groups.
    #[test]
    fn the_works_wikidata_item_joins() {
        use crate::sources::kit::own_keys;
        for (file, artwork, key) in [
            ("artwork-1958.39.json", acc("1958.39"), "wikidata:Q50772913"),
            ("artwork-1958.31.json", acc("1958.31"), "wikidata:Q26221230"),
        ] {
            let card = parse(200, &read(file), &artwork).unwrap().unwrap();
            assert_eq!(own_keys(&Cleveland, &card), [key], "{file}");
        }
    }

    #[test]
    fn no_image_unless_cc0() {
        let card = parse(200, &read("artwork-1997.246.json"), &acc("1997.246"))
            .unwrap()
            .unwrap();
        assert_eq!(card.name.as_deref(), Some("Marilyn x 100"));
        assert_eq!(card.image, None);
        assert_eq!(card.attribution.as_deref(), Some("Cleveland Museum of Art"));
        assert_eq!(card.details["share_license_status"], "Copyrighted");
        assert!(card.details["copyright"]
            .as_str()
            .unwrap()
            .starts_with("© The Andy Warhol Foundation"));
        let text = card.description.unwrap();
        assert!(text.starts_with("The image of Marilyn Monroe in Marilyn x 100, the largest"));
        assert!(!text.contains('<'));

        let mut v: Value = serde_json::from_slice(&read("artwork-1997.246.json")).unwrap();
        v["data"]["images"] =
            json!({"web": {"url": "https://openaccess-cdn.clevelandart.org/x.jpg"}});
        let raw = serde_json::to_vec(&v).unwrap();
        assert_eq!(
            parse(200, &raw, &acc("1997.246")).unwrap().unwrap().image,
            None
        );
    }

    #[test]
    fn an_image_address_that_is_not_an_https_url_is_no_image() {
        let images = [
            "http://openaccess-cdn.clevelandart.org/1958.39/1958.39_web.jpg",
            "data:image/png;base64,iVBORw0KGgo=",
            "1958.39/1958.39_web.jpg",
        ]
        .map(|address| {
            let mut v: Value = serde_json::from_slice(&read("artwork-1958.39.json")).unwrap();
            v["data"]["images"]["web"]["url"] = address.into();
            let raw = serde_json::to_vec(&v).unwrap();
            parse(200, &raw, &acc("1958.39")).unwrap().unwrap().image
        });
        assert!(images.iter().all(Option::is_none), "{images:?}");
    }

    #[test]
    fn roots_and_aliases() {
        let c = Cleveland;
        for uri in ["cleveland:1958.39", "cleveland:1916.1511.a"] {
            assert!(c.reads(uri), "{uri}");
            assert_eq!(c.alias_root(uri), None, "{uri}");
            assert!(!c.invalid(uri), "{uri}");
        }
        for uri in [
            "https://www.clevelandart.org/art/1958.39",
            "https://openaccess-api.clevelandart.org/api/artworks/1958.39",
        ] {
            assert_eq!(
                c.alias_root(uri).as_deref(),
                Some("cleveland:1958.39"),
                "{uri}"
            );
        }
        // A numeric id is named by the fetch: the card's root becomes the link's root.
        let by_id = "https://www.clevelandart.org/art/135382";
        assert!(c.reads(by_id));
        assert_eq!(c.alias_root(by_id), None);
        for uri in [
            "cleveland:1958.39A",
            "cleveland:1916.1511.A",
            "cleveland:135382",
            "cleveland:1958..39",
            "cleveland:",
        ] {
            assert!(c.invalid(uri), "{uri}");
            assert!(!c.reads(uri), "{uri}");
        }
        for example in c.examples() {
            assert!(c.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, artwork) in [
            ("artwork-1958.39.json", acc("1958.39")),
            ("artwork-1958.31.json", acc("1958.31")),
            ("artwork-1997.246.json", acc("1997.246")),
            ("artwork-135382.json", Artwork::Id(135382)),
        ] {
            let card = parse(200, &read(file), &artwork).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Cleveland, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let card = parse(200, &read("artwork-135382.json"), &Artwork::Id(135382))
            .unwrap()
            .unwrap();
        let doc = crate::sources::kit::checked(&Cleveland, &card);
        assert_eq!(doc.source_root.as_deref(), Some("cleveland:1958.39"));
        assert_eq!(doc.attributes["wikidata"], "Q50772913");
        assert_eq!(
            doc.attributes["creators"],
            "Claude Monet (French, 1840–1926)"
        );
    }

    #[test]
    fn missing_and_failures() {
        assert_eq!(
            parse(404, &read("artwork-missing.json"), &acc("1800.99999")).unwrap(),
            None
        );
        assert!(parse(200, &read("artwork-1958.39.json"), &acc("1958.31")).is_err());
        assert!(parse(200, &read("artwork-1958.39.json"), &Artwork::Id(135299)).is_err());
        assert!(parse(429, b"", &acc("1958.39")).is_err());
        assert!(parse(502, b"", &acc("1958.39")).is_err());
        assert!(parse(200, b"{\"data\": {}}", &acc("1958.39")).is_err());
    }

    #[test]
    fn ambiguous_wikidata_adds_no_key() {
        let mut v: Value = serde_json::from_slice(&read("artwork-1958.39.json")).unwrap();
        v["data"]["external_resources"]["wikidata"] = json!([
            "https://www.wikidata.org/wiki/Q1",
            "https://www.wikidata.org/wiki/Q2"
        ]);
        let raw = serde_json::to_vec(&v).unwrap();
        assert!(parse(200, &raw, &acc("1958.39"))
            .unwrap()
            .unwrap()
            .keys
            .is_empty());
    }
}
