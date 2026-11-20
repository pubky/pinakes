//! Instagram posts, reels and profiles, from their pages' Open Graph tags when Instagram
//! writes them for a request without an account; its oEmbed endpoint needs an app token and is
//! not used. A profile answers the sign-in redirect: the plain card, marked `blocked`. A post
//! page names the post only to a browser: the plain card, "Post on instagram.com". No key, no
//! account. Every spelling (instagr.am, a capitalised name, no trailing slash) is one resource
//! at `https://www.instagram.com/{name}/`, `/p/{code}/` or `/reel/{code}/`.
use super::{
    social::{self, Body, OpenGraph, Shape},
    web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct Instagram;

const SITE: &str = "instagram.com";
/// First path segments that are Instagram's own pages, never a name.
const RESERVED: &[&str] = &[
    "about",
    "accounts",
    "api",
    "challenge",
    "developer",
    "direct",
    "emails",
    "explore",
    "legal",
    "login",
    "nametag",
    "oauth",
    "p",
    "press",
    "privacy",
    "reel",
    "reels",
    "s",
    "static",
    "stories",
    "tv",
];

/// A post code: letters, digits, `_` and `-`.
fn code(value: &str) -> bool {
    (6..=40).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn name(value: &str) -> bool {
    social::handle(value, 30) && !RESERVED.contains(&value.to_ascii_lowercase().as_str())
}

/// What an Instagram link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Profile(String),
    Post(String),
    Reel(String),
}

impl Target {
    /// `https://www.instagram.com/{name}/`, `/p/{code}/`, `/reel/{code}/` (also `/tv/`), on
    /// instagram.com with or without `www.`, and instagr.am.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = super::kit::web_url(uri)?;
        if !matches!(
            url.host_str()?,
            "instagram.com" | "www.instagram.com" | "instagr.am" | "www.instagr.am"
        ) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            ["p", post] if code(post) => Some(Self::Post((*post).to_owned())),
            ["reel" | "tv", post] if code(post) => Some(Self::Reel((*post).to_owned())),
            [user] if name(user) => Some(Self::Profile(user.to_ascii_lowercase())),
            _ => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Profile(name) => format!("https://www.instagram.com/{name}/"),
            Self::Post(code) => format!("https://www.instagram.com/p/{code}/"),
            Self::Reel(code) => format!("https://www.instagram.com/reel/{code}/"),
        }
    }
    fn shape(&self) -> Shape {
        match self {
            Self::Profile(_) => Shape::Profile,
            Self::Post(_) => Shape::Post,
            Self::Reel(_) => Shape::Video,
        }
    }
}

/// The card of a profile from its page's tags (`Name (@name) • Instagram photos and videos`,
/// a description of counts and bio), when Instagram wrote them; else the plain card.
pub fn profile_card(doc: ResourceMeta, name: &str, tags: &OpenGraph) -> ResourceMeta {
    let mut doc = Shape::Profile.card(doc, SITE);
    let (display, titled) = tags
        .title
        .as_deref()
        .map(social::name_and_handle)
        .unwrap_or_default();
    if titled.is_some_and(|t| t.eq_ignore_ascii_case(name)) {
        doc.name = display.or(doc.name);
        doc.description = tags.description.as_deref().and_then(|d| {
            let bio = d
                .split(" - ")
                .nth(1)
                .unwrap_or(d)
                .split(" • ")
                .skip_while(|part| {
                    part.ends_with(" Followers")
                        || part.ends_with(" Following")
                        || part.ends_with(" Posts")
                })
                .collect::<Vec<_>>()
                .join(" • ");
            let bio = bio.trim();
            (!bio.is_empty()).then(|| social::excerpt(bio, 200))
        });
        doc.image = tags.image.clone();
    }
    social::fact(&mut doc, "handle", Some(name.to_owned()));
    doc
}

/// The card of a post or reel from its page's tags, when Instagram wrote them (a title naming
/// the author, `Name on Instagram: "caption"`); else the plain card.
pub fn post_card(doc: ResourceMeta, target: &Target, tags: &OpenGraph) -> ResourceMeta {
    let mut doc = target.shape().card(doc, SITE);
    let Some(title) = tags
        .title
        .as_deref()
        .filter(|t| !t.eq_ignore_ascii_case("instagram"))
    else {
        return doc;
    };
    let (by, caption) = match title.split_once(" on Instagram: ") {
        Some((by, caption)) => (Some(by.trim()), caption.trim().trim_matches('"')),
        None => (None, title),
    };
    social::post_text(&mut doc, caption, by);
    if doc.description.is_none() {
        doc.description = tags.description.as_deref().map(|d| social::excerpt(d, 200));
    }
    doc.image = tags.image.clone();
    social::fact(&mut doc, "author_name", by.map(str::to_owned));
    doc
}

#[async_trait]
impl Source for Instagram {
    fn name(&self) -> &'static str {
        "instagram"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "instagram",
                uri: "https://www.instagram.com/p/C2vXx0yPvYq/",
                card_type: "SocialMediaPosting",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "www.instagram.com",
                path: "/p/C2vXx0yPvYq/",
                query: &[],
                header: None,
                content_type: "text/html",
                file: "post-C2vXx0yPvYq.html.gz",
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
        &["author_name", "handle", "blocked"]
    }
    /// A post's card and a reel's: both quote the author's caption.
    fn social(&self, doc: &ResourceMeta) -> bool {
        matches!(doc.type_name.as_str(), "SocialMediaPosting" | "VideoObject")
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.instagram.com/p/C2vXx0yPvYq/",
                card_type: "SocialMediaPosting",
            },
            Example {
                uri: "https://www.instagram.com/instagram/",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not an Instagram link")?;
        let doc = cx.card(uri, self);
        let mut doc = match social::page_body(cx, &target.root(), &[]).await? {
            Body::Read { body, .. } => {
                let tags =
                    crate::resolver::html::blocking(move || social::open_graph(&body)).await??;
                match &target {
                    Target::Profile(name) => profile_card(doc, name, &tags),
                    _ => post_card(doc, &target, &tags),
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

    #[test]
    fn addresses() {
        for (uri, target) in [
            (
                "https://www.instagram.com/Instagram",
                Target::Profile("instagram".into()),
            ),
            (
                "https://instagram.com/p/C2vXx0yPvYq/?igsh=x",
                Target::Post("C2vXx0yPvYq".into()),
            ),
            (
                "https://www.instagram.com/reel/C2vXx0yPvYq/",
                Target::Reel("C2vXx0yPvYq".into()),
            ),
            (
                "https://instagr.am/tv/C2vXx0yPvYq",
                Target::Reel("C2vXx0yPvYq".into()),
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://www.instagram.com/accounts/login/",
            "https://www.instagram.com/explore/tags/x/",
            "https://www.instagram.com/instagram/reels/",
            "https://www.instagram.com/p/x/",
            "https://www.instagram.com/stories/instagram/1/",
            "https://example.com/p/C2vXx0yPvYq/",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            Instagram
                .alias_root("https://www.instagram.com/Instagram")
                .as_deref(),
            Some("https://www.instagram.com/instagram/")
        );
        assert_eq!(
            Instagram.alias_root("https://www.instagram.com/p/C2vXx0yPvYq/"),
            None
        );
    }

    /// Instagram's sign-in page is no profile's name.
    #[test]
    fn the_sign_in_page_is_no_profile() {
        for uri in [
            "https://www.instagram.com/login",
            "https://instagram.com/Login/",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
            assert!(!Instagram.reads(uri), "{uri}");
        }
    }

    #[test]
    fn a_post_page_without_tags_is_the_plain_card() {
        let body = crate::sources::fixture::read("instagram", "post-C2vXx0yPvYq.html.gz");
        let tags = social::open_graph(&String::from_utf8_lossy(&body)).unwrap();
        let target = Target::Post("C2vXx0yPvYq".into());
        let base = ResourceMeta::new(&target.root(), "instagram", 3600);
        let doc = post_card(base.clone(), &target, &tags);
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert_eq!(doc.name.as_deref(), Some("Post on instagram.com"));
        assert_eq!((doc.description, doc.image), (None, None));
        let shown = OpenGraph {
            title: Some(r#"Instagram on Instagram: "Hello, world""#.into()),
            description: Some("1,000 likes, 10 comments".into()),
            image: Some("https://example.com/a.jpg".into()),
        };
        let doc = post_card(base, &Target::Reel("C2vXx0yPvYq".into()), &shown);
        assert_eq!(doc.type_name, "VideoObject");
        assert_eq!(doc.name.as_deref(), Some("Hello, world"));
        assert_eq!(doc.attributes["author_name"], "Instagram");
        assert_eq!(doc.image.as_deref(), Some("https://example.com/a.jpg"));
        // A reel quotes its caption: social content, as a post is.
        assert!(Instagram.social(&doc));
    }

    /// Instagram has no status of its own for a refusal: a 999 is a server error as any status
    /// above 500 is, a retry and not the blocked card.
    #[tokio::test(start_paused = true)]
    async fn a_999_is_a_server_error() {
        let profile = "https://www.instagram.com/instagram/";
        let (egress, _) = crate::sources::fixture::egress(vec![(
            profile,
            999,
            "text/html",
            b"<html></html>".to_vec(),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let error = Instagram.resolve(&cx, profile).await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<crate::resolver::RetryLater>()
                .map(|retry| retry.class),
            Some("upstream")
        );
    }

    /// A post withheld where the request came from (451) is not a post that is gone: the blocked
    /// card, not `missing`.
    #[tokio::test(start_paused = true)]
    async fn a_withheld_post_is_blocked_not_missing() {
        let post = "https://www.instagram.com/p/C2vXx0yPvYq/";
        let (egress, _) = crate::sources::fixture::egress(vec![(
            post,
            451,
            "text/html",
            b"<html><title>Unavailable</title></html>".to_vec(),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Instagram.resolve(&cx, post).await.unwrap();
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert_eq!(doc.attributes["blocked"], "true");
        assert_eq!(doc.source_root.as_deref(), Some(post));
    }

    #[test]
    fn a_profile_from_tags_or_without_them() {
        let base = ResourceMeta::new("https://www.instagram.com/instagram/", "instagram", 3600);
        let shown = OpenGraph {
            title: Some("Instagram (@instagram) • Instagram photos and videos".into()),
            description: Some(
                "700M Followers, 100 Following, 8,000 Posts - See Instagram photos and videos from Instagram (@instagram)".into(),
            ),
            image: Some("https://example.com/avatar.jpg".into()),
        };
        let doc = profile_card(base.clone(), "instagram", &shown);
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Instagram"));
        assert_eq!(doc.attributes["handle"], "instagram");
        assert_eq!(doc.image.as_deref(), Some("https://example.com/avatar.jpg"));
        let doc = profile_card(base.clone(), "someone", &shown);
        assert_eq!(doc.name.as_deref(), Some("Profile on instagram.com"));
        let doc = profile_card(base, "instagram", &OpenGraph::default());
        assert_eq!(doc.name.as_deref(), Some("Profile on instagram.com"));
        assert_eq!(doc.attributes["handle"], "instagram");
    }
}
