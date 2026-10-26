//! LinkedIn profiles, company pages and posts, from their pages' Open Graph tags as written
//! (their JSON-LD lists recent posts, not the page). LinkedIn shows most of it to accounts
//! only: a refused request (its 999, a sign-in page) gets the plain card, marked `blocked`. No
//! key, no account. Every spelling (the host without `www.`, a country host, a capitalised
//! slug) is one resource at `https://www.linkedin.com/in/{slug}`, `/company/{slug}` or
//! `/feed/update/urn:li:activity:{id}`.
use super::{
    social::{self, Body, OpenGraph, Shape},
    web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct LinkedIn;

const SITE: &str = "linkedin.com";

/// A profile or company slug: letters, digits, `-`, `_` and percent-encoded characters.
fn slug(value: &str) -> bool {
    (2..=120).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_%".contains(&b))
}

/// What a LinkedIn link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Person(String),
    Company(String),
    /// A post by its activity id, with the address the link spelled when it was a `/posts/`
    /// one (the address LinkedIn shows without an account).
    Post {
        id: String,
        posts: Option<String>,
    },
}

impl Target {
    /// `https://www.linkedin.com/in/{slug}`, `/company/{slug}` (also `/school/`),
    /// `/posts/{slug}-activity-{id}-{tag}` and `/feed/update/urn:li:activity:{id}`, on
    /// linkedin.com with any host prefix (`www.`, `de.`, none).
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        let host = url.host_str()?;
        if host != "linkedin.com"
            && !host.strip_suffix(".linkedin.com").is_some_and(|prefix| {
                prefix.len() <= 5 && prefix.bytes().all(|b| b.is_ascii_lowercase())
            })
        {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            ["in", s] if slug(s) => Some(Self::Person(s.to_ascii_lowercase())),
            ["company" | "school", s] if slug(s) => Some(Self::Company(s.to_ascii_lowercase())),
            // `{author}_{words}-activity-{id}-{tag}`, or `{author}_activity-{id}-{tag}` without words.
            ["posts", s] if slug(s) => {
                let (before, rest) = s.rsplit_once("activity-")?;
                if !before.ends_with('-') && !before.ends_with('_') {
                    return None;
                }
                let (id, _) = rest.split_once('-')?;
                social::numeric(id).then(|| Self::Post {
                    id: id.to_owned(),
                    posts: Some((*s).to_owned()),
                })
            }
            ["feed", "update", urn] => {
                let id = urn.strip_prefix("urn:li:activity:")?;
                social::numeric(id).then(|| Self::Post {
                    id: id.to_owned(),
                    posts: None,
                })
            }
            _ => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Person(slug) => format!("https://www.linkedin.com/in/{slug}"),
            Self::Company(slug) => format!("https://www.linkedin.com/company/{slug}"),
            Self::Post { id, .. } => {
                format!("https://www.linkedin.com/feed/update/urn:li:activity:{id}")
            }
        }
    }
    /// The page to read: a post's `/posts/` address when the link spelled one.
    fn page(&self) -> String {
        match self {
            Self::Post {
                posts: Some(posts), ..
            } => format!("https://www.linkedin.com/posts/{posts}"),
            _ => self.root(),
        }
    }
    fn shape(&self) -> Shape {
        match self {
            Self::Post { .. } => Shape::Post,
            _ => Shape::Profile,
        }
    }
}

/// Whether a page is LinkedIn's sign-in invitation in place of the object.
fn sign_in(tags: &OpenGraph) -> bool {
    tags.description
        .as_deref()
        .is_some_and(|d| d.starts_with("Sign in or join now"))
}

/// The card of a person from their page's tags: `Name - Headline | LinkedIn` and a description
/// of headline, about, experience, education and location joined by ` · `.
pub fn person_card(doc: ResourceMeta, slug: &str, tags: &OpenGraph) -> Option<ResourceMeta> {
    let title = tags.title.as_deref()?;
    let title = title.strip_suffix(" | LinkedIn").unwrap_or(title);
    let (name, headline) = match title.split_once(" - ") {
        Some((name, headline)) => (name.trim(), Some(headline.trim())),
        None => (title.trim(), None),
    };
    if name.is_empty() {
        return None;
    }
    let mut doc = Shape::Profile.card(doc, SITE);
    doc.name = Some(name.to_owned());
    let parts: Vec<&str> = tags
        .description
        .as_deref()
        .map(|d| d.split(" · ").collect())
        .unwrap_or_default();
    let about = parts
        .iter()
        .skip(1)
        .find(|part| {
            !["Experience:", "Education:", "Location:"]
                .iter()
                .any(|label| part.starts_with(label))
                && !part.contains(" connections on LinkedIn")
                && !part.contains(" followers on LinkedIn")
        })
        .map(|about| social::excerpt(about, 200));
    doc.description = about.or_else(|| headline.map(str::to_owned));
    doc.image = tags.image.clone();
    social::fact(&mut doc, "handle", Some(slug.to_owned()));
    social::fact(&mut doc, "headline", headline.map(str::to_owned));
    Some(doc)
}

/// The card of a company from its page's tags: `Name | LinkedIn` and a description starting
/// `Name | N followers on LinkedIn. About…`.
pub fn company_card(doc: ResourceMeta, slug: &str, tags: &OpenGraph) -> Option<ResourceMeta> {
    let title = tags.title.as_deref()?;
    let name = title.strip_suffix(" | LinkedIn").unwrap_or(title).trim();
    if name.is_empty() {
        return None;
    }
    let mut doc = Shape::Profile.card(doc, SITE);
    doc.set_type("Organization");
    doc.name = Some(name.to_owned());
    let rest = tags.description.as_deref().map(|d| {
        d.strip_prefix(name)
            .and_then(|d| d.strip_prefix(" | "))
            .unwrap_or(d)
    });
    let followers = rest
        .and_then(|d| d.split_once(" followers on LinkedIn."))
        .map(|(count, _)| count.replace(',', ""))
        .filter(|count| social::numeric(count));
    doc.description = rest
        .map(|d| {
            d.split_once(" followers on LinkedIn.")
                .map_or(d, |(_, about)| about)
                .trim()
        })
        .filter(|about| !about.is_empty())
        .map(|about| social::excerpt(about, 200));
    doc.image = tags.image.clone();
    social::fact(&mut doc, "handle", Some(slug.to_owned()));
    social::fact(&mut doc, "followers", followers);
    Some(doc)
}

/// The card of a post from its page's tags, when LinkedIn shows the post rather than its
/// sign-in invitation.
pub fn post_card(doc: ResourceMeta, target: &Target, tags: &OpenGraph) -> ResourceMeta {
    let author = match target {
        Target::Post {
            posts: Some(posts), ..
        } => posts.split_once('_').map(|(author, _)| author.to_owned()),
        _ => None,
    };
    if sign_in(tags) || tags.title.is_none() {
        let mut doc = social::blocked(doc, Shape::Post, SITE);
        social::fact(&mut doc, "author", author);
        return doc;
    }
    let mut doc = Shape::Post.card(doc, SITE);
    social::post_text(
        &mut doc,
        tags.description.as_deref().unwrap_or_default(),
        author.as_deref(),
    );
    doc.name = tags.title.as_deref().map(|t| social::excerpt(t, 120));
    doc.image = tags.image.clone();
    social::fact(&mut doc, "author", author);
    doc
}

#[async_trait]
impl Source for LinkedIn {
    fn name(&self) -> &'static str {
        "linkedin"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "linkedin",
                    uri: "https://www.linkedin.com/company/linkedin",
                    card_type: "Organization",
                    group_with: None,
                },
                E2eSample {
                    kind: "linkedin_person",
                    uri: "https://www.linkedin.com/in/williamhgates",
                    card_type: "Person",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "www.linkedin.com",
                    path: "/company/linkedin",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "company-linkedin.html.gz",
                },
                E2eAnswer {
                    host: "www.linkedin.com",
                    path: "/in/williamhgates",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "person-williamhgates.html.gz",
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
        &["author", "handle", "headline", "followers", "blocked"]
    }
    fn social(&self, doc: &ResourceMeta) -> bool {
        doc.type_name == "SocialMediaPosting"
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.linkedin.com/company/linkedin",
                card_type: "Organization",
            },
            Example {
                uri: "https://www.linkedin.com/in/williamhgates",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a LinkedIn link")?;
        let doc = cx.card(uri, self);
        let mut doc = match social::page_body(cx, &target.page()).await? {
            Body::Read { body, .. } => {
                let tags =
                    crate::resolver::html::blocking(move || social::open_graph(&body)).await??;
                match &target {
                    Target::Person(slug) => {
                        person_card(doc, slug, &tags).unwrap_or_else(|| cx.missing(uri, self))
                    }
                    Target::Company(slug) => {
                        company_card(doc, slug, &tags).unwrap_or_else(|| cx.missing(uri, self))
                    }
                    Target::Post { .. } => post_card(doc, &target, &tags),
                }
            }
            Body::Missing => cx.missing(uri, self),
            Body::Blocked => social::blocked(doc, target.shape(), SITE),
        };
        doc.source_root = Some(target.root());
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(file: &str) -> OpenGraph {
        let body = crate::sources::fixture::read("linkedin", file);
        social::open_graph(&String::from_utf8_lossy(&body)).unwrap()
    }

    #[test]
    fn addresses() {
        for (uri, target) in [
            (
                "https://www.linkedin.com/in/WilliamHGates/",
                Target::Person("williamhgates".into()),
            ),
            (
                "https://de.linkedin.com/company/linkedin?trk=x",
                Target::Company("linkedin".into()),
            ),
            (
                "https://www.linkedin.com/posts/linkedin_activity-7517918134654218241-7OWA",
                Target::Post {
                    id: "7517918134654218241".into(),
                    posts: Some("linkedin_activity-7517918134654218241-7OWA".into()),
                },
            ),
            (
                "https://linkedin.com/feed/update/urn:li:activity:7517918134654218241",
                Target::Post {
                    id: "7517918134654218241".into(),
                    posts: None,
                },
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://www.linkedin.com/jobs/view/1",
            "https://www.linkedin.com/in/",
            "https://www.linkedin.com/posts/no-activity-here",
            "https://www.linkedin.com/feed/update/urn:li:share:1",
            "https://example.com/in/williamhgates",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            LinkedIn
                .alias_root(
                    "https://www.linkedin.com/posts/linkedin_activity-7517918134654218241-7OWA"
                )
                .as_deref(),
            Some("https://www.linkedin.com/feed/update/urn:li:activity:7517918134654218241")
        );
        assert_eq!(
            LinkedIn.alias_root("https://www.linkedin.com/in/williamhgates"),
            None
        );
    }

    #[test]
    fn a_person_and_a_company_from_their_tags() {
        let base = ResourceMeta::new(
            "https://www.linkedin.com/in/williamhgates",
            "linkedin",
            3600,
        );
        let doc =
            person_card(base, "williamhgates", &tags("person-williamhgates.html.gz")).unwrap();
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Bill Gates"));
        assert!(doc.attributes["headline"].starts_with("Chair, Gates Foundation"));
        let about = doc.description.as_deref().unwrap();
        assert!(
            about.starts_with("Chair of the Gates Foundation."),
            "{about}"
        );
        assert!(doc
            .image
            .as_deref()
            .unwrap()
            .starts_with("https://media.licdn.com/"));
        assert_eq!(doc.attributes["handle"], "williamhgates");

        let base = ResourceMeta::new(
            "https://www.linkedin.com/company/linkedin",
            "linkedin",
            3600,
        );
        let doc =
            company_card(base.clone(), "linkedin", &tags("company-linkedin.html.gz")).unwrap();
        assert_eq!(doc.type_name, "Organization");
        assert_eq!(doc.name.as_deref(), Some("LinkedIn"));
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .starts_with("Founded in 2003"));
        assert!(social::numeric(&doc.attributes["followers"]));
        assert!(company_card(base, "x", &tags("company-missing.html.gz")).is_none());
    }

    #[test]
    fn a_post_behind_the_sign_in_invitation_is_blocked() {
        let target = Target::of_uri(
            "https://www.linkedin.com/posts/linkedin_activity-7517918134654218241-7OWA",
        )
        .unwrap();
        let base = ResourceMeta::new(&target.root(), "linkedin", 3600);
        let doc = post_card(
            base.clone(),
            &target,
            &tags("post-linkedin-7517918134654218241.html.gz"),
        );
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert_eq!(doc.name.as_deref(), Some("Post on linkedin.com"));
        assert_eq!(doc.attributes["blocked"], "true");
        assert_eq!(doc.attributes["author"], "linkedin");
        let shown = OpenGraph {
            title: Some("LinkedIn on LinkedIn: hello".into()),
            description: Some("hello world".into()),
            image: None,
            kind: Some("article".into()),
        };
        let doc = post_card(base, &target, &shown);
        assert_eq!(doc.name.as_deref(), Some("LinkedIn on LinkedIn: hello"));
        assert_eq!(doc.description.as_deref(), Some("hello world"));
        assert!(!doc.attributes.contains_key("blocked"));
    }
}
