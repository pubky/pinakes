//! Figshare articles, read from the Figshare API by the article id in the link.
//!
//! Every link of one article on figshare.com or an institutional portal (`{name}.figshare.com`,
//! which shares figshare.com's article ids) and its API URL are aliases of `figshare:{id}`. A link
//! to one version (`…/{id}/{version}`) is a dated version, not the article, and is not read
//! here. The card is the article as a whole, read from its latest version, whose number is in
//! `version`. Portals on their own domains are not recognised from the link; the card's page is
//! the one the answer names.
//!
//! Figshare gives each version a DOI ending in `.v{n}`; the same DOI without the suffix names the
//! article and resolves to its latest version. Both are keys. DOI links
//! (`doi.org/10.6084/m9.figshare.{id}`) belong to the DOI source.
use super::{
    kit::{
        credit, fetch, host_and_segments, html_text, int, json, plain, text, texts, time, Card,
        Key, Request,
    },
    Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context as _, Result};
use async_trait::async_trait;

pub const SITE: &str = "Figshare";

/// The type segment of an article page, e.g. `journal_contribution`.
const TYPES: &[&str] = &[
    "book",
    "chapter",
    "composition",
    "conference_contribution",
    "data_management_plan",
    "dataset",
    "educational_resource",
    "event",
    "figure",
    "funding",
    "journal_contribution",
    "media",
    "model",
    "monograph",
    "online_resource",
    "peer_review",
    "performance",
    "physical_object",
    "poster",
    "preprint",
    "presentation",
    "report",
    "service",
    "software",
    "standard",
    "thesis",
    "workflow",
];

/// A positive decimal number without a leading zero.
fn number(s: &str) -> Option<u64> {
    (plain(s, 10, |c| c.is_ascii_digit()) && !s.starts_with('0'))
        .then(|| s.parse().ok())
        .flatten()
}

/// The article an article page names: `/articles/{type}/{slug}/{id}`, the older
/// `/articles/{slug}/{id}`, and `/articles/{id}`. A page of one version (`…/{id}/{version}`) is
/// not the article.
fn article_path(s: &[&str]) -> Option<u64> {
    match s {
        [id] => number(id),
        // Two numbers are an article and one of its versions, not a slug and an article.
        [slug, id] if !slug.bytes().all(|b| b.is_ascii_digit()) => number(id),
        [kind, _, id] if TYPES.contains(kind) => number(id),
        _ => None,
    }
}

/// The article a Figshare link names.
pub fn from_uri(uri: &str) -> Option<u64> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    if host == "api.figshare.com" {
        return match s.as_slice() {
            ["v2", "articles", id] => number(id),
            _ => None,
        };
    }
    let portal = host.strip_suffix(".figshare.com").is_some_and(|name| {
        !matches!(name, "ndownloader" | "s3")
            && plain(name, 63, |c| c.is_ascii_alphanumeric() || c == '-')
    });
    if host != "figshare.com" && !portal {
        return None;
    }
    match s.as_slice() {
        ["articles", rest @ ..] => article_path(rest),
        _ => None,
    }
}

pub fn identifier(id: u64) -> String {
    format!("figshare:{id}")
}

/// The article of a root (`figshare:{id}`).
pub fn from_root(uri: &str) -> Option<u64> {
    number(uri.strip_prefix("figshare:")?)
}

/// The article of a root or a Figshare link.
pub fn target(uri: &str) -> Option<u64> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// A page of the article on figshare.com, for use before the API names the canonical one (the
/// type and slug of a page are not part of its address).
pub fn page(id: u64) -> String {
    format!("https://figshare.com/articles/_/{id}")
}

/// The article's API record (its latest version).
pub fn request(id: u64) -> Option<Request> {
    Request::get(&format!("https://api.figshare.com/v2/articles/{id}"))
}

/// The schema.org type of a Figshare item type.
fn type_name(kind: &str) -> &'static str {
    match kind {
        "dataset" => "Dataset",
        "software" => "SoftwareSourceCode",
        "figure" => "ImageObject",
        "media" => "MediaObject",
        "journal contribution" | "preprint" | "conference contribution" => "ScholarlyArticle",
        "thesis" => "Thesis",
        "book" | "monograph" => "Book",
        "chapter" => "Chapter",
        "report" => "Report",
        _ => "CreativeWork",
    }
}

/// Whether a licence lets us show the article's thumbnail: CC0, or CC BY without further terms.
fn open_licence(name: &str) -> bool {
    name == "CC0"
        || name.starts_with("CC0 ")
        || name
            .strip_prefix("CC BY ")
            .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
}

/// An article, or `None` when Figshare has no public article with this id.
pub fn parse(status: u16, raw: &[u8], id: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        int(&v["id"]) == i64::try_from(id).ok(),
        "the API answered for another article"
    );
    let kind = text(&v["defined_type_name"]).unwrap_or_default();
    // The canonical page the answer names, on figshare.com or the article's portal.
    let pages: Vec<String> = [&v["figshare_url"], &v["url_public_html"]]
        .into_iter()
        .filter_map(text)
        .collect();
    let url = pages
        .iter()
        .find(|p| from_uri(p) == Some(id))
        .cloned()
        .unwrap_or_else(|| page(id));
    let mut card = Card::new(type_name(&kind), identifier(id), url);
    card.name = text(&v["title"]);
    card.description = v["description"].as_str().and_then(html_text);
    card.detail(
        "published",
        time(&v["published_date"]).map(|t| t.to_rfc3339()),
    );
    let licence = text(&v["license"]["name"]);
    if licence.as_deref().is_some_and(open_licence) {
        card.image = text(&v["thumb"]);
    }
    let authors: Vec<String> = v["authors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| text(&a["full_name"]))
        .collect();
    let categories: Vec<String> = v["categories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| text(&c["title"]))
        .collect();
    let version = v["version"].as_u64();
    card.attribution = credit(&authors, licence.as_deref());
    card.detail("authors", authors);
    card.detail("license", licence);
    card.detail("item_type", text(&v["defined_type_name"]));
    card.detail("version", version);
    card.detail("categories", categories);
    card.detail("tags", texts(&v["tags"]));
    let doi = text(&v["doi"]).map(|d| d.to_ascii_lowercase());
    if let Some(doi) = doi {
        let article = version.and_then(|n| {
            doi.strip_suffix(&format!(".v{n}"))
                .filter(|base| base.ends_with(&format!(".{id}")))
        });
        // A version's DOI names that version: a fact. The article's own DOI is the key.
        match article {
            Some(base) => {
                card.key(Key::new("doi", base));
                card.detail("version_doi", doi.as_str());
            }
            None => card.key(Key::new("doi", &doi)),
        }
    }
    Ok(Some(card))
}

/// The source: the Figshare API.
pub struct Figshare;

#[async_trait]
impl Source for Figshare {
    fn name(&self) -> &'static str {
        "figshare"
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(target(uri)?);
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("figshare:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "version_doi",
            "published",
            "doi",
            "authors",
            "license",
            "item_type",
            "version",
            "categories",
            "tags",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("api.figshare.com", 1.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://figshare.com/articles/dataset/Garden_data_set/1018700",
                card_type: "Dataset",
            },
            Example {
                uri: "https://figshare.com/articles/presentation/Using_APIs_to_customise_repositories_and_engage_audiences/5616445",
                card_type: "CreativeWork",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(id).context("article id")?).await?;
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

    fn read(name: &str) -> Vec<u8> {
        fixture::read("figshare", name)
    }

    #[test]
    fn every_link_form_of_one_article() {
        for uri in [
            "https://figshare.com/articles/presentation/Using_APIs_to_customise_repositories_and_engage_audiences/5616445",
            "https://www.figshare.com/articles/presentation/x/5616445",
            "https://figshare.com/articles/Using_APIs_to_customise_repositories_and_engage_audiences/5616445",
            "https://figshare.com/articles/5616445",
            "https://figshare.com/articles/_/5616445",
            "https://sheffield.figshare.com/articles/presentation/x/5616445",
            "https://api.figshare.com/v2/articles/5616445",
        ] {
            assert_eq!(from_uri(uri).map(identifier).as_deref(), Some("figshare:5616445"), "{uri}");
        }
        // One version of the article is a dated version, not the article.
        for uri in [
            "https://figshare.com/articles/presentation/Using_APIs_to_customise_repositories_and_engage_audiences/5616445/1",
            "https://figshare.com/articles/presentation/Using_APIs_to_customise_repositories_and_engage_audiences/5616445/2?file=9778651",
            "https://figshare.com/articles/Using_APIs_to_customise_repositories_and_engage_audiences/5616445/1",
            "https://api.figshare.com/v2/articles/5616445/versions/1",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        assert_eq!(page(5616445), "https://figshare.com/articles/_/5616445");
        assert_eq!(
            request(5616445).unwrap().url.as_str(),
            "https://api.figshare.com/v2/articles/5616445"
        );
    }

    #[test]
    fn invalid_links() {
        for uri in [
            // An article id and a version, not a slug and an id.
            "https://figshare.com/articles/5616445/2",
            "https://figshare.com/articles/presentation/x/05616445",
            "https://figshare.com/articles/presentation/x/abc",
            "https://figshare.com/articles/presentation/x/5616445/v2",
            "https://figshare.com/articles/presentation/x/5616445/1/2",
            "https://figshare.com/articles",
            "https://figshare.com/collections/x/123456",
            "https://figshare.com/projects/x/123456",
            "https://figshare.com/s/865e6b6d0d55c5a6c3a5",
            "https://figshare.com/ndownloader/files/9778651",
            "https://ndownloader.figshare.com/files/9778651",
            "https://api.figshare.com/v2/articles/5616445/files",
            "https://api.figshare.com/v2/articles/5616445/versions/x",
            "https://api.figshare.com/articles/5616445",
            "https://outputs.ncl.ac.uk/articles/journal_contribution/x/34404654",
            "https://doi.org/10.6084/m9.figshare.5616445",
            "https://figshare.com.example.org/articles/5616445",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn dataset() {
        let card = parse(200, &read("article-1018700.json"), 1018700)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "Dataset");
        assert_eq!(card.id, "figshare:1018700");
        assert_eq!(
            card.url,
            "https://figshare.com/articles/dataset/Garden_data_set/1018700"
        );
        assert_eq!(card.name.as_deref(), Some("Garden data set"));
        assert_eq!(
            card.description.as_deref(),
            Some("Temperature data from a WSN node located in a garden. 5000 data points, 1 per minute.")
        );
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["published"], "2014-05-07T12:19:41+00:00");
        assert_eq!(
            card.attribution.as_deref(),
            Some("Pehr Söderman / CC BY 4.0"),
            "the creator as well as the licence"
        );
        assert_eq!(card.image, None, "no thumbnail");
        assert_eq!(
            card.details["authors"],
            serde_json::json!(["Pehr Söderman"])
        );
        assert_eq!(card.details["version"], 1);
        assert_eq!(
            card.keys,
            [Key::new("doi", "10.6084/m9.figshare.1018700")],
            "a version's DOI names that version"
        );
        assert_eq!(
            card.details["version_doi"],
            "10.6084/m9.figshare.1018700.v1"
        );
        assert!(parse(200, &read("article-1018700.json"), 5616445).is_err());
        assert!(parse(429, b"", 1018700).is_err());
        assert!(parse(502, b"", 1018700).is_err());
    }

    #[test]
    fn presentation_in_its_second_version() {
        let card = parse(200, &read("article-5616445.json"), 5616445)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "CreativeWork");
        assert_eq!(card.details["item_type"], "presentation");
        assert_eq!(card.details["version"], 2);
        assert_eq!(
            card.description.as_deref(),
            Some("Presentation at RDMF18 in Manchester on 20-21 November 2017.\nAn accompanying report on Figshare's API is linked below.")
        );
        assert_eq!(
            card.image.as_deref(),
            Some("https://s3-eu-west-1.amazonaws.com/pfigshare-u-previews/9778651/thumb.png")
        );
        assert_eq!(
            card.details["authors"],
            serde_json::json!(["Jez Cope", "Megan Hardeman", "Team Figshare"])
        );
        assert_eq!(
            card.details["categories"][0],
            "Applied computing not elsewhere classified"
        );
        assert_eq!(card.details["tags"][0], "RDMF");
        assert_eq!(
            card.keys,
            [Key::new("doi", "10.6084/m9.figshare.5616445")],
            "a version's DOI names that version"
        );
        assert_eq!(
            card.details["version_doi"],
            "10.6084/m9.figshare.5616445.v2"
        );
        // The first version's record names the same article; its page with the version is a
        // dated version, never an alias.
        let v1 = parse(200, &read("article-5616445-v1.json"), 5616445)
            .unwrap()
            .unwrap();
        assert_eq!(v1.id, card.id);
        assert_eq!(v1.url, card.url);
        assert_eq!(v1.keys[0], card.keys[0]);
    }

    #[test]
    fn portal_article_with_a_publishers_doi() {
        let card = parse(200, &read("article-34700883.json"), 34700883)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "Dataset");
        assert_eq!(
            card.url,
            "https://frontiersin.figshare.com/articles/dataset/Supplementary_file_1_OCEAN_KARIZ_a_novel_approach_to_artificial_upwelling_docx/34700883"
        );
        assert_eq!(from_uri(&card.url), Some(34700883));
        assert_eq!(
            card.keys,
            [Key::new("doi", "10.3389/fmars.2026.1969697.s001")]
        );
        assert!(card.image.is_some());
    }

    #[test]
    fn missing_article() {
        assert_eq!(
            parse(404, &read("article-missing.json"), 999999999).unwrap(),
            None
        );
    }

    #[test]
    fn licences_and_types() {
        assert!(open_licence("CC BY 4.0"));
        assert!(open_licence("CC0"));
        assert!(!open_licence("CC BY-NC 4.0"));
        assert!(!open_licence("CC BY-SA 4.0"));
        assert!(!open_licence("MIT"));
        assert!(!open_licence("In Copyright"));
        assert_eq!(type_name("journal contribution"), "ScholarlyArticle");
        assert_eq!(type_name("thesis"), "Thesis");
        assert_eq!(type_name("poster"), "CreativeWork");
        assert_eq!(type_name("figure"), "ImageObject");
    }

    #[test]
    fn roots_and_aliases() {
        let f = Figshare;
        assert!(f.reads("figshare:5616445"));
        assert_eq!(f.alias_root("figshare:5616445"), None);
        assert!(!f.invalid("figshare:5616445"));
        assert_eq!(
            target("figshare:5616445").map(identifier).as_deref(),
            Some("figshare:5616445")
        );
        assert_eq!(
            f.alias_root("https://sheffield.figshare.com/articles/presentation/x/5616445")
                .as_deref(),
            Some("figshare:5616445")
        );
        assert_eq!(
            f.alias_root("https://sheffield.figshare.com/articles/presentation/x/5616445/1"),
            None
        );
        for uri in [
            "figshare:05616445",
            "figshare:abc",
            "figshare:",
            "figshare:5616445.v1",
        ] {
            assert!(f.invalid(uri), "{uri}");
            assert!(!f.reads(uri), "{uri}");
        }
        for example in f.examples() {
            assert!(f.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, id) in [
            ("article-1018700.json", 1018700),
            ("article-5616445.json", 5616445),
            ("article-5616445-v1.json", 5616445),
            ("article-34700883.json", 34700883),
        ] {
            let card = parse(200, &read(file), id).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&Figshare, &card);
            assert_eq!(doc.attributes["page_url"], card.url, "{file}");
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let card = parse(200, &read("article-1018700.json"), 1018700)
            .unwrap()
            .unwrap();
        let doc = crate::sources::kit::checked(&Figshare, &card);
        assert_eq!(doc.attributes["doi"], "10.6084/m9.figshare.1018700");
        assert_eq!(
            doc.attributes["version_doi"],
            "10.6084/m9.figshare.1018700.v1"
        );
        assert_eq!(doc.attributes["version"], "1");
    }
}
