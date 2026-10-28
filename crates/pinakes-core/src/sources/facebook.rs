//! Facebook pages, posts and videos, from what their public pages carry. A page's Open Graph
//! tags name it (name, about, picture). Posts and videos are shown to accounts only: the
//! sign-in redirect gets the plain card, marked `blocked`. Event links (`/events/{id}`) are not
//! this source's: the event module's Facebook adapter reads them. No key, no account. Every
//! spelling (m., web., fb.com, a capitalised name) is one resource at
//! `https://www.facebook.com/…`.
use super::{
    social::{self, Body, OpenGraph, Shape},
    web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct Facebook;

const SITE: &str = "facebook.com";
/// First path segments that are Facebook's own pages, never a page's name.
const RESERVED: &[&str] = &[
    "ads",
    "business",
    "dialog",
    "directory",
    "events",
    "friends",
    "fundraisers",
    "gaming",
    "groups",
    "hashtag",
    "help",
    "home.php",
    "jobs",
    "l.php",
    "live",
    "login",
    "marketplace",
    "messages",
    "notifications",
    "pages",
    "people",
    "permalink.php",
    "pg",
    "photo",
    "photo.php",
    "places",
    "plugins",
    "policies",
    "privacy",
    "profile.php",
    "public",
    "reel",
    "search",
    "settings",
    "share",
    "sharer",
    "sharer.php",
    "stories",
    "story.php",
    "video.php",
    "watch",
];

/// A page's name: 2–64 letters, digits, dots and underscores (a numeric id too).
fn name(value: &str) -> bool {
    (2..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
        && !RESERVED.contains(&value.to_ascii_lowercase().as_str())
}

/// What a Facebook link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Page(String),
    Post { page: String, id: String },
    Video(String),
}

impl Target {
    /// `https://www.facebook.com/{name}`, `/{name}/posts/{id}`, `/reel/{id}` and
    /// `/watch/?v={id}`, on facebook.com (also m., web., mbasic.) and fb.com.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if !matches!(
            url.host_str()?,
            "facebook.com"
                | "www.facebook.com"
                | "m.facebook.com"
                | "web.facebook.com"
                | "mbasic.facebook.com"
                | "fb.com"
                | "www.fb.com"
        ) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            ["reel", id] if social::numeric(id) => Some(Self::Video((*id).to_owned())),
            ["watch"] => url
                .query_pairs()
                .find(|(k, _)| k == "v")
                .filter(|(_, v)| social::numeric(v))
                .map(|(_, v)| Self::Video(v.into_owned())),
            [page, "posts", id] if name(page) && social::numeric(id) => Some(Self::Post {
                page: page.to_ascii_lowercase(),
                id: (*id).to_owned(),
            }),
            [page] if name(page) => Some(Self::Page(page.to_ascii_lowercase())),
            _ => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Page(name) => format!("https://www.facebook.com/{name}"),
            Self::Post { page, id } => format!("https://www.facebook.com/{page}/posts/{id}"),
            Self::Video(id) => format!("https://www.facebook.com/watch/?v={id}"),
        }
    }
}

/// The card of a page from its tags: its name, its about text (the description after
/// `Name. N followers · N talking about this.`, in the page's language) and its picture.
pub fn page_card(doc: ResourceMeta, name: &str, tags: &OpenGraph) -> Option<ResourceMeta> {
    let title = tags.title.as_deref()?.trim();
    if title.is_empty() {
        return None;
    }
    let mut doc = Shape::Profile.card(doc, SITE);
    doc.set_type("Organization");
    doc.name = Some(title.to_owned());
    doc.description = tags.description.as_deref().and_then(|d| {
        let rest = d
            .strip_prefix(title)
            .and_then(|d| d.strip_prefix(". "))
            .unwrap_or(d);
        // The counts sentence (`N followers · N talking about this.`) comes first.
        let about = match rest.split_once(". ") {
            Some((counts, about)) if counts.contains(" · ") => about,
            _ => rest,
        };
        let about = about.trim();
        (!about.is_empty()).then(|| social::excerpt(about, 200))
    });
    doc.image = tags.image.clone();
    social::fact(&mut doc, "handle", Some(name.to_owned()));
    Some(doc)
}

#[async_trait]
impl Source for Facebook {
    fn name(&self) -> &'static str {
        "facebook"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "facebook",
                uri: "https://www.facebook.com/facebook",
                card_type: "Organization",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "www.facebook.com",
                path: "/facebook",
                query: &[],
                header: None,
                content_type: "text/html",
                file: "page-facebook.html.gz",
            }],
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
        &["author", "handle", "blocked"]
    }
    fn social(&self, doc: &ResourceMeta) -> bool {
        doc.type_name == "SocialMediaPosting"
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.facebook.com/facebook",
            card_type: "Organization",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Facebook link")?;
        let doc = cx.card(uri, self);
        let mut doc = match &target {
            Target::Page(name) => match social::page_body(cx, &target.root()).await? {
                Body::Read { body, .. } => {
                    let tags = crate::resolver::html::blocking(move || social::open_graph(&body))
                        .await??;
                    page_card(doc, name, &tags).unwrap_or_else(|| cx.missing(uri, self))
                }
                Body::Missing => cx.missing(uri, self),
                Body::Blocked => social::blocked(doc, Shape::Profile, SITE),
            },
            // Posts and videos are shown to accounts only; the page answers the sign-in
            // redirect, a plain card.
            Target::Post { page, .. } => {
                let mut doc = match social::page_body(cx, &target.root()).await? {
                    Body::Missing => cx.missing(uri, self),
                    Body::Read { .. } => Shape::Post.card(doc, SITE),
                    Body::Blocked => social::blocked(doc, Shape::Post, SITE),
                };
                social::fact(&mut doc, "author", Some(page.clone()));
                doc
            }
            Target::Video(_) => match social::page_body(cx, &target.root()).await? {
                Body::Missing => cx.missing(uri, self),
                Body::Read { .. } => Shape::Video.card(doc, SITE),
                Body::Blocked => social::blocked(doc, Shape::Video, SITE),
            },
        };
        doc.source_root = Some(target.root());
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        for (uri, target) in [
            (
                "https://www.facebook.com/Facebook/",
                Target::Page("facebook".into()),
            ),
            ("https://fb.com/zuck", Target::Page("zuck".into())),
            (
                "https://m.facebook.com/facebook/posts/10162533193146729",
                Target::Post {
                    page: "facebook".into(),
                    id: "10162533193146729".into(),
                },
            ),
            (
                "https://www.facebook.com/watch/?v=123456789",
                Target::Video("123456789".into()),
            ),
            (
                "https://www.facebook.com/reel/123456789",
                Target::Video("123456789".into()),
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://www.facebook.com/events/123456789/",
            "https://www.facebook.com/login/",
            "https://www.facebook.com/groups/123",
            "https://www.facebook.com/facebook/photos",
            "https://www.facebook.com/share/p/abc/",
            "https://www.facebook.com/profile.php?id=4",
            "https://example.com/facebook",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            Facebook
                .alias_root("https://m.facebook.com/Facebook/")
                .as_deref(),
            Some("https://www.facebook.com/facebook")
        );
        assert_eq!(
            Facebook
                .alias_root("https://www.facebook.com/reel/123456789")
                .as_deref(),
            Some("https://www.facebook.com/watch/?v=123456789")
        );
        assert_eq!(
            Facebook.alias_root("https://www.facebook.com/facebook"),
            None
        );
    }

    #[test]
    fn a_page_from_its_tags() {
        let body = crate::sources::fixture::read("facebook", "page-facebook.html.gz");
        let tags = social::open_graph(&String::from_utf8_lossy(&body)).unwrap();
        let base = ResourceMeta::new("https://www.facebook.com/facebook", "facebook", 3600);
        let doc = page_card(base.clone(), "facebook", &tags).unwrap();
        assert_eq!(doc.type_name, "Organization");
        assert_eq!(doc.name.as_deref(), Some("Facebook"));
        let about = doc.description.as_deref().unwrap();
        assert!(about.starts_with("Community Values"), "{about}");
        assert!(doc.image.as_deref().unwrap().starts_with("https://"));
        assert_eq!(doc.attributes["handle"], "facebook");
        assert!(page_card(base, "x", &OpenGraph::default()).is_none());
    }
}
