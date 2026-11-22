//! Instagram posts, reels and profiles, from their pages' Open Graph tags when Instagram
//! writes them for a request without an account; its oEmbed endpoint needs an app token and is
//! not used. A profile answers the sign-in redirect: the plain card, marked `blocked`. A post or
//! reel page names the object (`Name on Instagram: "caption"`, its image, and its own address in
//! `og:url`) when the post is public; a page without them (a removed or private post) gives the
//! plain card, "Post on instagram.com". No key, no account. Every spelling (instagr.am, a
//! capitalised name, no trailing slash, `/{name}/p/{code}/`) is one resource at
//! `https://www.instagram.com/{name}/`, `/p/{code}/` or `/reel/{code}/`.
use super::{
    social::{self, Body, OpenGraph, Shape},
    Context, Example, Source,
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
    /// instagram.com with or without `www.`, and instagr.am. A post or reel address may carry its
    /// author's name first (`/{name}/p/{code}/`, the form a post page gives itself in `og:url`):
    /// the code alone names the object.
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
            [user, "p", post] if name(user) && code(post) => Some(Self::Post((*post).to_owned())),
            [user, "reel", post] if name(user) && code(post) => {
                Some(Self::Reel((*post).to_owned()))
            }
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
    /// The code of a post or reel (one code space: Instagram serves a reel under `/p/` too).
    fn code(&self) -> Option<&str> {
        match self {
            Self::Profile(_) => None,
            Self::Post(code) | Self::Reel(code) => Some(code),
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

/// The card of a post or reel from its page's tags, when Instagram wrote them on the object's own
/// page: a title naming the author (`Name on Instagram: "caption"`), and the address the page gives
/// itself (`og:url`, else the one the fetch ended at) with the link's code. Else the plain card,
/// marked `shape_only`: a removed post and a refused one answer the same tagless page, so a card
/// read before stays until a confirming read.
pub fn post_card(
    doc: ResourceMeta,
    target: &Target,
    tags: &OpenGraph,
    ended_at: &url::Url,
) -> ResourceMeta {
    let mut doc = target.shape().card(doc, SITE);
    let own =
        Target::of_uri(tags.address(ended_at)).is_some_and(|page| page.code() == target.code());
    let title = tags
        .title
        .as_deref()
        .filter(|t| !t.eq_ignore_ascii_case("instagram"));
    let (true, Some(title)) = (own, title) else {
        return social::shape_only(doc);
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
            // A post page and a reel page that carry no tags of the object: the plain cards.
            samples: &[
                E2eSample {
                    kind: "instagram",
                    uri: "https://www.instagram.com/p/C2vXx0yPvYq/",
                    card_type: "SocialMediaPosting",
                    group_with: None,
                },
                E2eSample {
                    kind: "instagram_reel",
                    uri: "https://www.instagram.com/reel/AAAAAAAAAAA/",
                    card_type: "VideoObject",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "www.instagram.com",
                    path: "/p/C2vXx0yPvYq/",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "post-C2vXx0yPvYq.html.gz",
                },
                E2eAnswer {
                    host: "www.instagram.com",
                    path: "/reel/AAAAAAAAAAA/",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "reel-missing.html.gz",
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
        &["author_name", "handle", "blocked"]
    }
    /// A post's card and a reel's (both quote the author's caption), and a profile's (its bio is
    /// its author's own text).
    fn social(&self, doc: &ResourceMeta) -> bool {
        matches!(
            doc.type_name.as_str(),
            "SocialMediaPosting" | "VideoObject" | "Person"
        )
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
            Body::Read { body, url } => {
                let tags =
                    crate::resolver::html::blocking(move || social::open_graph(&body)).await??;
                match &target {
                    Target::Profile(name) => profile_card(doc, name, &tags),
                    _ => post_card(doc, &target, &tags, &url),
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
            (
                "https://www.instagram.com/motorsportcom/p/Df_H64Tul-0/",
                Target::Post("Df_H64Tul-0".into()),
            ),
            (
                "https://www.instagram.com/motorsportcom/reel/Df_H64Tul-0/",
                Target::Reel("Df_H64Tul-0".into()),
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
            "https://www.instagram.com/p/p/Df_H64Tul-0/",
            "https://www.instagram.com/motorsportcom/tv/Df_H64Tul-0/",
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
        assert_eq!(
            Instagram
                .alias_root("https://www.instagram.com/motorsportcom/p/Df_H64Tul-0/")
                .as_deref(),
            Some("https://www.instagram.com/p/Df_H64Tul-0/")
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

    fn recorded_tags(file: &str) -> OpenGraph {
        let body = crate::sources::fixture::read("instagram", file);
        social::open_graph(&String::from_utf8_lossy(&body)).unwrap()
    }

    /// A public post's page, as Instagram answers this host's request (recorded 2026-11-22), names
    /// the post: the caption, its author and its image, and its own address with the code.
    #[test]
    fn a_public_post_page_names_the_post() {
        let target = Target::Post("Df_H64Tul-0".into());
        let tags = recorded_tags("post-Df_H64Tul-0.html.gz");
        let ended_at = url::Url::parse(&target.root()).unwrap();
        let base = ResourceMeta::new(&target.root(), "instagram", 3600);
        let doc = post_card(base, &target, &tags, &ended_at);
        assert!(!doc.shape_only);
        assert_eq!(doc.type_name, "SocialMediaPosting");
        let name = doc.name.as_deref().unwrap();
        assert!(
            name.starts_with("MAX VERSTAPPEN GAVE GEORGE RUSSELL THE FINGER!"),
            "{name}"
        );
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .starts_with("MAX VERSTAPPEN"));
        assert_eq!(doc.attributes["author_name"], "Motorsport");
        assert!(doc
            .image
            .as_deref()
            .unwrap()
            .starts_with("https://scontent-hel3-1.cdninstagram.com/"));
        assert!(Instagram.social(&doc));
        // The same page read for a reel link of that code is the reel's own page too.
        let reel = Target::Reel("Df_H64Tul-0".into());
        let doc = post_card(
            ResourceMeta::new(&reel.root(), "instagram", 3600),
            &reel,
            &tags,
            &ended_at,
        );
        assert_eq!(doc.type_name, "VideoObject");
        assert_eq!(doc.attributes["author_name"], "Motorsport");
    }

    /// A page without the post's tags (a removed or private post answers so, as does a reel that
    /// does not exist) and a page that names another post give the plain card, marked
    /// `shape_only`, so a card read before is not replaced by one read.
    #[test]
    fn a_page_without_the_posts_tags_is_the_plain_card() {
        let target = Target::Post("C2vXx0yPvYq".into());
        let ended_at = url::Url::parse(&target.root()).unwrap();
        let base = ResourceMeta::new(&target.root(), "instagram", 3600);
        for file in ["post-C2vXx0yPvYq.html.gz", "reel-missing.html.gz"] {
            let doc = post_card(base.clone(), &target, &recorded_tags(file), &ended_at);
            assert!(doc.shape_only, "{file}");
            assert_eq!(doc.name.as_deref(), Some("Post on instagram.com"), "{file}");
            assert_eq!((&doc.description, &doc.image), (&None, &None), "{file}");
        }
        // Another post's page (its og:url names another code): not this post's card.
        let other = recorded_tags("post-Df_H64Tul-0.html.gz");
        let doc = post_card(base.clone(), &target, &other, &ended_at);
        assert!(doc.shape_only);
        assert_eq!(doc.name.as_deref(), Some("Post on instagram.com"));
        assert_eq!(doc.image, None);
        // Without og:url, the address the fetch ended at decides: the sign-in page is no post's.
        let no_url = OpenGraph {
            url: None,
            ..recorded_tags("post-Df_H64Tul-0.html.gz")
        };
        let own = Target::Post("Df_H64Tul-0".into());
        let sign_in = url::Url::parse("https://www.instagram.com/accounts/login/").unwrap();
        let doc = post_card(base.clone(), &own, &no_url, &sign_in);
        assert!(doc.shape_only);
        let at_root = url::Url::parse(&own.root()).unwrap();
        let doc = post_card(base, &own, &no_url, &at_root);
        assert!(!doc.shape_only);
        assert_eq!(doc.attributes["author_name"], "Motorsport");
    }

    /// A post link is read: its page is asked, and the card comes from it.
    #[tokio::test(start_paused = true)]
    async fn a_post_link_reads_its_page() {
        let post = "https://www.instagram.com/p/Df_H64Tul-0/";
        let body = crate::sources::fixture::read("instagram", "post-Df_H64Tul-0.html.gz");
        let (egress, recorded) =
            crate::sources::fixture::egress(vec![(post, 200, "text/html", body)]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Instagram
            .resolve(&cx, "https://instagram.com/p/Df_H64Tul-0?igsh=x")
            .await
            .unwrap();
        assert_eq!(recorded.asked(), vec![post.to_owned()]);
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.source_root.as_deref(), Some(post));
        assert_eq!(doc.attributes["author_name"], "Motorsport");
        assert!(doc.image.is_some());
    }

    /// A reel withheld where the request came from (451) is not a reel that is gone: the blocked
    /// card, not `missing`.
    #[tokio::test(start_paused = true)]
    async fn a_withheld_reel_is_blocked_not_missing() {
        let reel = "https://www.instagram.com/reel/C2vXx0yPvYq/";
        let (egress, _) = crate::sources::fixture::egress(vec![(
            reel,
            451,
            "text/html",
            b"<html><title>Unavailable</title></html>".to_vec(),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Instagram.resolve(&cx, reel).await.unwrap();
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.type_name, "VideoObject");
        assert_eq!(doc.attributes["blocked"], "true");
        assert_eq!(doc.source_root.as_deref(), Some(reel));
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
            ..OpenGraph::default()
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
