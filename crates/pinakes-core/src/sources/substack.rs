//! Substack publications, posts and profiles, from their pages' Open Graph tags and JSON-LD. A
//! publication's card names its RSS feed (`/feed`). No key, no account. Every spelling (`open.substack.com/pub/{name}/p/{slug}`, the
//! hosts with `www.`) is one resource at `https://{name}.substack.com/[p/{slug}]` or
//! `https://substack.com/@{user}`.
use super::{
    social::{self, Page, Shape},
    web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct Substack;

const SITE: &str = "substack.com";
/// Subdomains that are Substack's own, never a publication.
const RESERVED: &[&str] = &["www", "open", "support", "substack", "on-substack", "api"];

/// A publication's subdomain or a post's slug: 1–63 lowercase letters, digits and hyphens.
fn slug(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

/// What a Substack link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Publication(String),
    Post { publication: String, slug: String },
    Profile(String),
}

impl Target {
    /// `https://{name}.substack.com/`, `https://{name}.substack.com/p/{slug}`,
    /// `https://open.substack.com/pub/{name}/p/{slug}` and `https://substack.com/@{user}`.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = super::kit::web_url(uri)?;
        let host = url.host_str()?.to_ascii_lowercase();
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match host.as_str() {
            "substack.com" | "www.substack.com" => match segments.as_slice() {
                [user] => user
                    .strip_prefix('@')
                    .filter(|u| social::handle(u, 64))
                    .map(|u| Self::Profile(u.to_ascii_lowercase())),
                _ => None,
            },
            "open.substack.com" => match segments.as_slice() {
                ["pub", name, "p", post] if slug(&name.to_ascii_lowercase()) && slug(post) => {
                    Some(Self::Post {
                        publication: name.to_ascii_lowercase(),
                        slug: (*post).to_owned(),
                    })
                }
                _ => None,
            },
            _ => {
                let name = host.strip_suffix(".substack.com")?;
                if !slug(name) || RESERVED.contains(&name) {
                    return None;
                }
                match segments.as_slice() {
                    [] => Some(Self::Publication(name.to_owned())),
                    ["p", post] if slug(post) => Some(Self::Post {
                        publication: name.to_owned(),
                        slug: (*post).to_owned(),
                    }),
                    _ => None,
                }
            }
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Publication(name) => format!("https://{name}.substack.com/"),
            Self::Post { publication, slug } => {
                format!("https://{publication}.substack.com/p/{slug}")
            }
            Self::Profile(user) => format!("https://substack.com/@{user}"),
        }
    }
    fn shape(&self) -> Shape {
        match self {
            Self::Post { .. } => Shape::Post,
            _ => Shape::Profile,
        }
    }
}

/// The card of a publication from its home page: its name (the title before ` | Substack`),
/// its description (before Substack's own "Click to read …"), its image and its feed.
pub fn publication_card(doc: ResourceMeta, name: &str, page: &ResourceMeta) -> ResourceMeta {
    let mut doc = Shape::Profile.card(doc, SITE);
    doc.set_type("Blog");
    doc.name = Some(
        page.name
            .as_deref()
            .map(|t| t.strip_suffix(" | Substack").unwrap_or(t).trim())
            .filter(|t| !t.is_empty())
            .map_or_else(|| format!("{name}.substack.com"), str::to_owned),
    );
    doc.description = page
        .description
        .as_deref()
        .map(|d| d.split(" Click to read ").next().unwrap_or(d).trim())
        .filter(|d| !d.is_empty())
        .map(|d| social::excerpt(d, 200));
    doc.image = page.image.clone();
    social::fact(&mut doc, "handle", Some(name.to_owned()));
    // The feed the page advertises only on the publication's own host; else its own `/feed`.
    let own_host = format!("{name}.substack.com");
    social::fact(
        &mut doc,
        "feed",
        Some(
            page.attributes
                .get("feed")
                .filter(|f| web_url(f).is_some_and(|u| u.host_str() == Some(own_host.as_str())))
                .cloned()
                .unwrap_or_else(|| format!("https://{own_host}/feed")),
        ),
    );
    doc
}

/// The card of a post from its page: the article its JSON-LD describes (its type, title,
/// summary, image and date), with its publication.
pub fn post_card(doc: ResourceMeta, publication: &str, page: &ResourceMeta) -> ResourceMeta {
    let mut doc = Shape::Post.card(doc, SITE);
    doc.set_type(if page.type_path.iter().any(|t| t == "Article") {
        page.type_name.as_str()
    } else {
        "BlogPosting"
    });
    social::take(&mut doc, page);
    doc.description = doc.description.as_deref().map(|d| social::excerpt(d, 200));
    social::fact(&mut doc, "publication", Some(publication.to_owned()));
    social::fact(
        &mut doc,
        "published",
        page.attributes.get("published").cloned(),
    );
    doc
}

/// The card of a profile from its page: the person its JSON-LD describes.
pub fn profile_card(doc: ResourceMeta, user: &str, page: &ResourceMeta) -> ResourceMeta {
    let mut doc = Shape::Profile.card(doc, SITE);
    if page.type_name == "Person" {
        social::take(&mut doc, page);
        doc.description = doc.description.as_deref().map(|d| social::excerpt(d, 200));
    }
    social::fact(&mut doc, "handle", Some(user.to_owned()));
    doc
}

#[async_trait]
impl Source for Substack {
    fn name(&self) -> &'static str {
        "substack"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "substack",
                    uri: "https://on.substack.com/p/model-behavior-rick-rubin",
                    card_type: "NewsArticle",
                    group_with: None,
                },
                E2eSample {
                    kind: "substack_publication",
                    uri: "https://on.substack.com/",
                    card_type: "Blog",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "on.substack.com",
                    path: "/p/model-behavior-rick-rubin",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "post-model-behavior-rick-rubin.html.gz",
                },
                E2eAnswer {
                    host: "on.substack.com",
                    path: "/",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "publication-on.html.gz",
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
    fn social(&self, doc: &ResourceMeta) -> bool {
        // A profile (its bio is its author's own text); a publication's post is an article.
        doc.type_name == "Person" || crate::changes::gone_post_type(doc)
    }
    fn facts(&self) -> &'static [&'static str] {
        &["publication", "published", "handle", "feed", "blocked"]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://on.substack.com/p/model-behavior-rick-rubin",
                card_type: "NewsArticle",
            },
            Example {
                uri: "https://on.substack.com/",
                card_type: "Blog",
            },
            Example {
                uri: "https://substack.com/@substack",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Substack link")?;
        let doc = cx.card(uri, self);
        let mut doc = match social::page(cx, &target.root()).await? {
            // A profile that does not exist sends the fetch to Substack's search.
            Page::Read { url, .. } if url.path().starts_with("/search/") => cx.missing(uri, self),
            Page::Read { doc: page, .. } => match &target {
                Target::Publication(name) => publication_card(doc, name, &page),
                Target::Post { publication, .. } => post_card(doc, publication, &page),
                Target::Profile(user) => profile_card(doc, user, &page),
            },
            Page::Missing => cx.missing(uri, self),
            Page::Blocked => social::blocked(doc, target.shape(), SITE),
        };
        doc.source_root = Some(target.root());
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(uri: &str, file: &str) -> ResourceMeta {
        let fetched = crate::sources::fixture::page(
            uri,
            "text/html",
            crate::sources::fixture::read("substack", file),
            None,
        );
        crate::resolver::web::extract(uri, &fetched, 3600)
            .unwrap()
            .doc
    }

    #[test]
    fn addresses() {
        for (uri, target) in [
            ("https://on.substack.com/", Target::Publication("on".into())),
            ("https://On.substack.com", Target::Publication("on".into())),
            (
                "https://on.substack.com/p/model-behavior-rick-rubin",
                Target::Post {
                    publication: "on".into(),
                    slug: "model-behavior-rick-rubin".into(),
                },
            ),
            (
                "https://open.substack.com/pub/on/p/model-behavior-rick-rubin?r=abc",
                Target::Post {
                    publication: "on".into(),
                    slug: "model-behavior-rick-rubin".into(),
                },
            ),
            (
                "https://www.substack.com/@Substack",
                Target::Profile("substack".into()),
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://on.substack.com/archive",
            "https://on.substack.com/p/",
            "https://www.substack.com/",
            "https://substack.com/home",
            "https://open.substack.com/pub/on",
            "https://example.com/p/a-post",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            Substack
                .alias_root("https://open.substack.com/pub/on/p/model-behavior-rick-rubin")
                .as_deref(),
            Some("https://on.substack.com/p/model-behavior-rick-rubin")
        );
        assert_eq!(Substack.alias_root("https://on.substack.com/"), None);
    }

    #[test]
    fn publication_post_and_profile_cards() {
        let base = ResourceMeta::new("https://on.substack.com/", "substack", 3600);
        let doc = publication_card(
            base,
            "on",
            &page("https://on.substack.com/", "publication-on.html.gz"),
        );
        assert_eq!(doc.type_name, "Blog");
        assert_eq!(doc.name.as_deref(), Some("On Substack"));
        let about = doc.description.as_deref().unwrap();
        assert!(about.starts_with("Resources, stories") && !about.contains("Click to read"));
        assert_eq!(doc.attributes["feed"], "https://on.substack.com/feed");
        assert_eq!(doc.attributes["handle"], "on");
        // A feed the page advertises on another host is not the publication's.
        let mut elsewhere = page("https://on.substack.com/", "publication-on.html.gz");
        elsewhere
            .attributes
            .insert("feed".into(), "https://feeds.example.org/on.xml".into());
        let doc = publication_card(
            ResourceMeta::new("https://on.substack.com/", "substack", 3600),
            "on",
            &elsewhere,
        );
        assert_eq!(doc.attributes["feed"], "https://on.substack.com/feed");

        let uri = "https://on.substack.com/p/model-behavior-rick-rubin";
        let doc = post_card(
            ResourceMeta::new(uri, "substack", 3600),
            "on",
            &page(uri, "post-model-behavior-rick-rubin.html.gz"),
        );
        assert_eq!(doc.type_name, "NewsArticle");
        assert_eq!(doc.name.as_deref(), Some("Model Behavior: Rick Rubin"));
        assert_eq!(doc.attributes["published"], "2026-10-22T17:02:28+00:00");
        assert_eq!(doc.attributes["publication"], "on");
        assert!(doc.image.is_some());

        let doc = profile_card(
            ResourceMeta::new("https://substack.com/@substack", "substack", 3600),
            "substack",
            &page("https://substack.com/@substack", "profile-substack.html.gz"),
        );
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Substack"));
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .starts_with("The subscription network"));
        assert_eq!(doc.attributes["handle"], "substack");
    }
}
