//! Threads posts and profiles, from their pages' Open Graph tags: a post's text, author and
//! image, a profile's name, bio and avatar. No key, no account. Every spelling (threads.net,
//! the www. hosts, a capitalised handle) is one resource at
//! `https://www.threads.com/@{handle}[/post/{code}]`.
use super::{
    social::{self, Page, Shape},
    web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct Threads;

const SITE: &str = "threads.com";

/// A post code: letters, digits, `_` and `-`.
fn code(value: &str) -> bool {
    (6..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What a Threads link names: a profile, or a post (with its author when the link spells one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Profile(String),
    Post {
        author: Option<String>,
        code: String,
    },
}

impl Target {
    /// `https://www.threads.com/@{handle}`, `/@{handle}/post/{code}` or `/t/{code}`, on
    /// threads.com and threads.net with or without `www.`.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if !matches!(
            url.host_str()?,
            "threads.com" | "www.threads.com" | "threads.net" | "www.threads.net"
        ) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let handle = |s: &str| {
            s.strip_prefix('@')
                .filter(|h| social::handle(h, 30))
                .map(str::to_ascii_lowercase)
        };
        match segments.as_slice() {
            [user] => handle(user).map(Self::Profile),
            [user, "post", post] if code(post) => Some(Self::Post {
                author: Some(handle(user)?),
                code: (*post).to_owned(),
            }),
            ["t", post] if code(post) => Some(Self::Post {
                author: None,
                code: (*post).to_owned(),
            }),
            _ => None,
        }
    }
    /// The one address of the object, when the link spells enough for it.
    pub fn root(&self) -> Option<String> {
        match self {
            Self::Profile(handle) => Some(format!("https://www.threads.com/@{handle}")),
            Self::Post {
                author: Some(author),
                code,
            } => Some(format!("https://www.threads.com/@{author}/post/{code}")),
            Self::Post { author: None, .. } => None,
        }
    }
    fn page(&self) -> String {
        match self {
            Self::Post { author: None, code } => format!("https://www.threads.com/t/{code}"),
            _ => self
                .root()
                .expect("a profile or an authored post has its root"),
        }
    }
}

/// Whether a page fetch ended on Threads's home page saying the post is invalid
/// (`/?error=invalid_post`), which is how it answers a post that does not exist.
fn invalid_post(url: &url::Url) -> bool {
    url.path() == "/"
        && url
            .query_pairs()
            .any(|(k, v)| k == "error" && v == "invalid_post")
}

/// The post's own address with its author (`/@{handle}/post/{code}`), as its page gives it: its
/// head's canonical link or the address a redirect ended at, when that names the post asked
/// for. The handle and the root come from there, never from the title's text or from what the
/// page's body says of itself.
fn own_address(code: &str, ended_at: &url::Url, page: &ResourceMeta) -> Option<(String, String)> {
    page.alias_candidates
        .iter()
        .map(String::as_str)
        .chain((ended_at.as_str() != page.url).then_some(ended_at.as_str()))
        .filter_map(Target::of_uri)
        .find_map(|target| match &target {
            Target::Post {
                author: Some(author),
                code: named,
            } if named == code => Some((author.clone(), target.root()?)),
            _ => None,
        })
}

/// The card of a post from its page: the author its title names (`Name (@handle) on Threads`),
/// its text and its image. A handle that is no Threads handle is not kept.
pub fn post_card(doc: ResourceMeta, page: &ResourceMeta) -> ResourceMeta {
    let mut doc = Shape::Post.card(doc, SITE);
    let (name, handle) = page
        .name
        .as_deref()
        .map(social::name_and_handle)
        .unwrap_or_default();
    let handle = handle.filter(|h| social::handle(h, 30));
    let by = name.clone().or_else(|| handle.clone());
    social::post_text(
        &mut doc,
        page.description.as_deref().unwrap_or_default(),
        by.as_deref(),
    );
    doc.image = page.image.clone();
    social::fact(&mut doc, "author", handle.map(|h| h.to_ascii_lowercase()));
    social::fact(&mut doc, "author_name", name);
    doc
}

/// The card of a profile from its page: the name and handle its title carries, its bio (the
/// description after its counts, `7.0M Followers • 1.4K Threads • …`) and its avatar.
pub fn profile_card(doc: ResourceMeta, handle: &str, page: &ResourceMeta) -> ResourceMeta {
    let mut doc = Shape::Profile.card(doc, SITE);
    let (name, titled) = page
        .name
        .as_deref()
        .map(social::name_and_handle)
        .unwrap_or_default();
    if titled.is_some_and(|t| t.eq_ignore_ascii_case(handle)) {
        doc.name = name.or(doc.name);
        doc.image = page.image.clone();
        doc.description = page.description.as_deref().and_then(|d| {
            let bio = d
                .split(" • ")
                .skip_while(|part| part.ends_with(" Followers") || part.ends_with(" Threads"))
                .collect::<Vec<_>>()
                .join(" • ");
            let bio = bio.trim();
            (!bio.is_empty()).then(|| social::excerpt(bio, 200))
        });
    }
    social::fact(&mut doc, "handle", Some(handle.to_owned()));
    doc
}

#[async_trait]
impl Source for Threads {
    fn name(&self) -> &'static str {
        "threads"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "threads",
                    uri: "https://www.threads.com/@threads/post/Der1HO7miy_",
                    card_type: "SocialMediaPosting",
                    group_with: None,
                },
                E2eSample {
                    kind: "threads_profile",
                    uri: "https://www.threads.net/@threads",
                    card_type: "Person",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "www.threads.com",
                    path: "/@threads/post/Der1HO7miy_",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "post-Der1HO7miy_.html.gz",
                },
                E2eAnswer {
                    host: "www.threads.com",
                    path: "/@threads",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "profile-threads.html.gz",
                },
            ],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = Target::of_uri(uri)?.root()?;
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
                uri: "https://www.threads.com/@threads/post/Der1HO7miy_",
                card_type: "SocialMediaPosting",
            },
            Example {
                uri: "https://www.threads.com/@threads",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Threads link")?;
        let doc = cx.card(uri, self);
        let shape = match target {
            Target::Profile(_) => Shape::Profile,
            Target::Post { .. } => Shape::Post,
        };
        let mut doc = match social::page(cx, &target.page()).await? {
            Page::Read { url, .. } if invalid_post(&url) => cx.missing(uri, self),
            Page::Read { doc: page, url } => match &target {
                Target::Profile(handle) => profile_card(doc, handle, &page),
                Target::Post { code, .. } => {
                    let mut doc = post_card(doc, &page);
                    // A post reached by its code alone, or under another handle: the address
                    // its own page gives it is its root, and names its author.
                    if let Some((author, root)) = own_address(code, &url, &page) {
                        social::fact(&mut doc, "author", Some(author));
                        doc.source_root = Some(root);
                    }
                    doc
                }
            },
            Page::Missing => cx.missing(uri, self),
            Page::Blocked => social::blocked(doc, shape, SITE),
        };
        if doc.source_root.is_none() {
            doc.source_root = target.root();
        }
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
            crate::sources::fixture::read("threads", file),
            None,
        );
        crate::resolver::web::extract(uri, &fetched, 3600)
            .unwrap()
            .doc
    }

    #[test]
    fn addresses() {
        for (uri, target) in [
            (
                "https://www.threads.com/@threads/post/Der1HO7miy_",
                Target::Post {
                    author: Some("threads".into()),
                    code: "Der1HO7miy_".into(),
                },
            ),
            (
                "https://threads.net/@Threads/post/Der1HO7miy_/",
                Target::Post {
                    author: Some("threads".into()),
                    code: "Der1HO7miy_".into(),
                },
            ),
            (
                "https://www.threads.net/t/Der1HO7miy_",
                Target::Post {
                    author: None,
                    code: "Der1HO7miy_".into(),
                },
            ),
            (
                "https://www.threads.com/@threads",
                Target::Profile("threads".into()),
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://www.threads.com/threads",
            "https://www.threads.com/@threads/followers",
            "https://www.threads.com/@threads/post/x",
            "https://www.threads.com/search?q=x",
            "https://example.com/@threads",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            Threads
                .alias_root("https://threads.net/@Threads/post/Der1HO7miy_/")
                .as_deref(),
            Some("https://www.threads.com/@threads/post/Der1HO7miy_")
        );
        assert_eq!(
            Threads.alias_root("https://www.threads.com/@threads/post/Der1HO7miy_"),
            None
        );
        assert!(invalid_post(
            &url::Url::parse("https://www.threads.com/?error=invalid_post").unwrap()
        ));
        assert!(!invalid_post(
            &url::Url::parse("https://www.threads.com/@threads").unwrap()
        ));
    }

    #[test]
    fn post_and_profile_cards() {
        let post = page(
            "https://www.threads.com/@threads/post/Der1HO7miy_",
            "post-Der1HO7miy_.html.gz",
        );
        let base = ResourceMeta::new(
            "https://www.threads.com/@threads/post/Der1HO7miy_",
            "threads",
            3600,
        );
        let doc = post_card(base, &post);
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert!(doc
            .name
            .as_deref()
            .unwrap()
            .contains("Football season is heating up"));
        assert_eq!(doc.attributes["author"], "threads");
        assert_eq!(doc.attributes["author_name"], "Threads");
        assert!(doc.image.as_deref().unwrap().starts_with("https://"));

        let profile = page(
            "https://www.threads.com/@threads",
            "profile-threads.html.gz",
        );
        let base = ResourceMeta::new("https://www.threads.com/@threads", "threads", 3600);
        let doc = profile_card(base.clone(), "threads", &profile);
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Threads"));
        let bio = doc.description.as_deref().unwrap();
        assert!(
            !bio.contains("Followers") && bio.contains("Come see what"),
            "{bio}"
        );
        assert_eq!(doc.attributes["handle"], "threads");
        let doc = profile_card(base, "someone", &profile);
        assert_eq!(doc.name.as_deref(), Some("Profile on threads.com"));
    }

    /// A display name is the account's own text: a handle or a path written into it names
    /// neither the author nor the post's root. Both come from the address the page gives itself.
    #[tokio::test(start_paused = true)]
    async fn a_display_name_names_neither_the_author_nor_the_root() {
        let html = |title: &str, address: &str| {
            format!(
                r#"<html><head><title>{title}</title><meta property="og:title" content="{title}">
                <meta property="og:description" content="A post."><meta property="og:url" content="{address}">
                <link rel="canonical" href="{address}"></head><body></body></html>"#
            )
            .into_bytes()
        };
        let own = "https://www.threads.com/@mallory/post/Cspoofed01";
        let (egress, _) = crate::sources::fixture::egress(vec![
            (
                "https://www.threads.com/t/Cspoofed01",
                200,
                "text/html",
                html("Zuck (@zuck) fan (@mallory) on Threads", own),
            ),
            // A name with a path in the handle's place, and a page that names another post.
            (
                "https://www.threads.com/t/Cspoofed02",
                200,
                "text/html",
                html(
                    "M (@zuck/post/Creal00001#) on Threads",
                    "https://www.threads.com/@zuck/post/Creal00001",
                ),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Threads
            .resolve(&cx, "https://www.threads.com/t/Cspoofed01")
            .await
            .unwrap();
        assert_eq!(doc.attributes["author"], "mallory");
        assert_eq!(doc.source_root.as_deref(), Some(own));
        let doc = Threads
            .resolve(&cx, "https://www.threads.com/t/Cspoofed02")
            .await
            .unwrap();
        assert!(!doc.attributes.contains_key("author"), "{:?}", doc.attributes);
        assert_eq!(doc.source_root, None);
    }
}
