//! Pinterest pins and profiles. A pin comes from Pinterest's oEmbed endpoint (title, pinner,
//! thumbnail), a profile from its page's Open Graph tags (name, bio, avatar). No key, no
//! account. Every spelling (a country site, a pin's slug, a capitalised name) is one resource at
//! `https://www.pinterest.com/pin/{id}/` or `https://www.pinterest.com/{name}/`.
use super::{
    social::{self, Oembed, Page, Shape},
    text, web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Pinterest;

const SITE: &str = "pinterest.com";
const OEMBED: &str = "https://www.pinterest.com/oembed.json";
/// Pinterest's sites, `www.` or not.
const HOSTS: &[&str] = &[
    "pinterest.com",
    "pinterest.at",
    "pinterest.ca",
    "pinterest.ch",
    "pinterest.cl",
    "pinterest.co.kr",
    "pinterest.co.uk",
    "pinterest.com.au",
    "pinterest.com.mx",
    "pinterest.de",
    "pinterest.dk",
    "pinterest.es",
    "pinterest.fr",
    "pinterest.ie",
    "pinterest.it",
    "pinterest.jp",
    "pinterest.nl",
    "pinterest.nz",
    "pinterest.ph",
    "pinterest.pt",
    "pinterest.ru",
    "pinterest.se",
];
/// First path segments that are Pinterest's own pages, never a name.
const RESERVED: &[&str] = &[
    "about",
    "business",
    "categories",
    "explore",
    "ideas",
    "login",
    "news_hub",
    "oembed.json",
    "pin",
    "policy",
    "resource",
    "search",
    "settings",
    "today",
    "videos",
];

/// A name: 3–30 letters, digits and underscores.
fn name(value: &str) -> bool {
    (3..=30).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !RESERVED.contains(&value.to_ascii_lowercase().as_str())
}

/// What a Pinterest link names: a pin by its id, or a profile by its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Pin(String),
    Profile(String),
}

impl Target {
    /// `https://www.pinterest.com/pin/{id}/` (also `/pin/{slug}--{id}/`) or
    /// `https://www.pinterest.com/{name}/`, on any Pinterest site.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = super::kit::web_url(uri)?;
        let host = url.host_str()?;
        let host = host.strip_prefix("www.").unwrap_or(host);
        if !HOSTS.contains(&host) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            ["pin", pin] => {
                let id = pin.rsplit_once("--").map_or(*pin, |(_, id)| id);
                (social::numeric(id) && id.len() >= 8).then(|| Self::Pin(id.to_owned()))
            }
            [user] if name(user) => Some(Self::Profile(user.to_ascii_lowercase())),
            _ => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Pin(id) => format!("https://www.pinterest.com/pin/{id}/"),
            Self::Profile(name) => format!("https://www.pinterest.com/{name}/"),
        }
    }
}

/// The name an `author_url` (`https://www.pinterest.com/kentbrew/`) names.
fn author_of(v: &Value) -> Option<String> {
    match Target::of_uri(v["author_url"].as_str()?)? {
        Target::Profile(name) => Some(name),
        Target::Pin(_) => None,
    }
}

/// The card of a pin from its oEmbed answer: its title, its pinner and its thumbnail.
pub fn pin_card(doc: ResourceMeta, v: &Value) -> ResourceMeta {
    let mut doc = Shape::Post.card(doc, SITE);
    let author = author_of(v);
    let by = text(&v["author_name"]).or_else(|| author.clone());
    social::post_text(
        &mut doc,
        &text(&v["title"]).unwrap_or_default(),
        by.as_deref(),
    );
    doc.description = None;
    doc.image = text(&v["thumbnail_url"]).filter(|u| web_url(u).is_some());
    social::fact(&mut doc, "author", author);
    social::fact(&mut doc, "author_name", text(&v["author_name"]));
    doc
}

/// The card of a profile by its link alone, "Profile on pinterest.com" with the name the link
/// spells: what a page that is not a profile's gives. Pinterest answers its generic page, with
/// status 200, for a name it does not know, so that page does not say whether the profile
/// exists.
fn plain_profile(doc: ResourceMeta, name: &str) -> ResourceMeta {
    let mut doc = Shape::Profile.card(doc, SITE);
    social::fact(&mut doc, "handle", Some(name.to_owned()));
    doc
}

/// The card of a profile from its page, or `None` when the page is not a profile's (Pinterest's
/// generic page): its name, bio and avatar as the extractor read them from its Open Graph tags
/// and JSON-LD.
pub fn profile_card(doc: ResourceMeta, name: &str, page: &ResourceMeta) -> Option<ResourceMeta> {
    if page.type_name != "Person" || page.name.is_none() {
        return None;
    }
    let mut doc = plain_profile(doc, name);
    social::take(&mut doc, page);
    doc.description = doc
        .description
        .as_deref()
        .map(|bio| social::excerpt(bio, 200));
    Some(doc)
}

#[async_trait]
impl Source for Pinterest {
    fn name(&self) -> &'static str {
        "pinterest"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "pinterest",
                    uri: "https://www.pinterest.com/pin/99360735500167749/",
                    card_type: "SocialMediaPosting",
                    group_with: None,
                },
                E2eSample {
                    kind: "pinterest_profile",
                    uri: "https://www.pinterest.com/pinterest/",
                    card_type: "Person",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "www.pinterest.com",
                    path: "/oembed.json",
                    query: &[("url", "https://www.pinterest.com/pin/99360735500167749/")],
                    header: None,
                    content_type: "application/json",
                    file: "oembed-pin-99360735500167749.json",
                },
                E2eAnswer {
                    host: "www.pinterest.com",
                    path: "/pinterest/",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "profile-pinterest.html.gz",
                },
            ],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = Target::of_uri(uri)?.root();
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &["author", "author_name", "handle", "blocked"]
    }
    fn social(&self, doc: &ResourceMeta) -> bool {
        doc.type_name == "SocialMediaPosting"
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.pinterest.com/pin/99360735500167749/",
                card_type: "SocialMediaPosting",
            },
            Example {
                uri: "https://www.pinterest.com/pinterest/",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Pinterest link")?;
        let doc = cx.card(uri, self);
        let mut doc = match &target {
            Target::Pin(_) => match social::oembed(cx, OEMBED, &target.root()).await? {
                Oembed::Found(v) => pin_card(doc, &v),
                Oembed::Missing => cx.missing(uri, self),
                Oembed::Blocked => social::blocked(doc, Shape::Post, SITE),
            },
            Target::Profile(name) => match social::page(cx, &target.root()).await? {
                Page::Read { doc: page, .. } => profile_card(doc.clone(), name, &page)
                    .unwrap_or_else(|| plain_profile(doc, name)),
                Page::Missing => cx.missing(uri, self),
                Page::Blocked => social::blocked(doc, Shape::Profile, SITE),
            },
        };
        doc.source_root = Some(target.root());
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("pinterest", file)
    }
    fn page(uri: &str, file: &str) -> ResourceMeta {
        let fetched = crate::sources::fixture::page(uri, "text/html", read(file), None);
        crate::resolver::web::extract(uri, &fetched, 3600)
            .unwrap()
            .doc
    }

    #[test]
    fn addresses() {
        for (uri, target) in [
            (
                "https://www.pinterest.com/pin/99360735500167749/",
                Target::Pin("99360735500167749".into()),
            ),
            (
                "https://pinterest.co.uk/pin/what-a-cute-animal--485825878526052520",
                Target::Pin("485825878526052520".into()),
            ),
            (
                "https://www.pinterest.de/Pinterest/",
                Target::Profile("pinterest".into()),
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://www.pinterest.com/pinterest/boards/",
            "https://www.pinterest.com/pin/1/",
            "https://www.pinterest.com/search/pins/?q=x",
            "https://pin.it/abc123",
            "https://example.com/pin/99360735500167749/",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            Pinterest
                .alias_root("https://pinterest.co.uk/pin/what-a-cute-animal--485825878526052520")
                .as_deref(),
            Some("https://www.pinterest.com/pin/485825878526052520/")
        );
        assert_eq!(
            Pinterest.alias_root("https://www.pinterest.com/pin/485825878526052520/"),
            None
        );
    }

    #[test]
    fn a_pin_from_its_oembed_answer() {
        let v: Value = serde_json::from_slice(&read("oembed-pin-99360735500167749.json")).unwrap();
        let base = ResourceMeta::new(
            "https://www.pinterest.com/pin/99360735500167749/",
            "pinterest",
            3600,
        );
        let doc = pin_card(base, &v);
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert_eq!(doc.name.as_deref(), Some("Next stop: Pinterest!"));
        assert_eq!(doc.attributes["author"], "kentbrew");
        assert_eq!(doc.attributes["author_name"], "Kent Brewster");
        assert!(doc
            .image
            .as_deref()
            .unwrap()
            .starts_with("https://i.pinimg.com/"));
        let missing: Value = serde_json::from_slice(&read("oembed-missing.json")).unwrap();
        assert!(missing["error"].is_string());
    }

    /// The page Pinterest answers for a name it does not know is its generic page, with status
    /// 200 and no profile tags: the plain card, not `missing`.
    #[tokio::test(start_paused = true)]
    async fn a_page_that_is_no_profile_is_the_plain_card() {
        let profile = "https://www.pinterest.com/thisuserdoesnotexist12345x/";
        let (egress, _) = crate::sources::fixture::egress(vec![(
            profile,
            200,
            "text/html",
            read("profile-missing.html.gz"),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Pinterest.resolve(&cx, profile).await.unwrap();
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Profile on pinterest.com"));
        assert_eq!(doc.attributes["handle"], "thisuserdoesnotexist12345x");
        assert!(!doc.attributes.contains_key("blocked"));
        assert_eq!((doc.description, doc.image), (None, None));
        assert_eq!(doc.source_root.as_deref(), Some(profile));
    }

    #[test]
    fn a_profile_from_its_page_and_no_such_name() {
        let base = ResourceMeta::new("https://www.pinterest.com/pinterest/", "pinterest", 3600);
        let doc = profile_card(
            base.clone(),
            "pinterest",
            &page(
                "https://www.pinterest.com/pinterest/",
                "profile-pinterest.html.gz",
            ),
        )
        .unwrap();
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Pinterest"));
        assert_eq!(
            doc.description.as_deref(),
            Some("Find your reason to go offline. 📌")
        );
        assert!(doc.image.as_deref().unwrap().starts_with("https://"));
        assert_eq!(doc.attributes["handle"], "pinterest");
        assert!(profile_card(
            base,
            "thisuserdoesnotexist12345x",
            &page(
                "https://www.pinterest.com/thisuserdoesnotexist12345x/",
                "profile-missing.html.gz",
            ),
        )
        .is_none());
    }
}
