//! X (Twitter) posts and profiles. A post comes from X's oEmbed endpoint (`publish.x.com`): its
//! text, author and date. A profile comes from its page's Open Graph tags: name, bio and
//! avatar. No key, no account. Every spelling (twitter.com, mobile., a photo's address) is one
//! resource at `https://x.com/{handle}[/status/{id}]`.
use super::{
    social::{self, Oembed, Page, Shape},
    text, web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct X;

const SITE: &str = "x.com";
const OEMBED: &str = "https://publish.x.com/oembed?omit_script=true&dnt=true";
/// First path segments that are X's own pages, never a handle.
const RESERVED: &[&str] = &[
    "about",
    "compose",
    "explore",
    "hashtag",
    "home",
    "i",
    "intent",
    "login",
    "messages",
    "notifications",
    "privacy",
    "search",
    "settings",
    "share",
    "signup",
    "tos",
];

/// A handle: 1–15 letters, digits and underscores.
fn handle(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 15
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// What an X link names: a profile, or a post (with its author when the link spells one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Profile(String),
    Post { author: Option<String>, id: String },
}

impl Target {
    /// `https://x.com/{handle}`, `/{handle}/status/{id}` (with anything after, a photo's
    /// number), `/i/web/status/{id}` or `/i/status/{id}`, on x.com, twitter.com and their www.
    /// and mobile. hosts. `i` is X's own path, never a handle: a post under it has no author.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = super::kit::web_url(uri)?;
        if !matches!(
            url.host_str()?,
            "x.com"
                | "www.x.com"
                | "mobile.x.com"
                | "twitter.com"
                | "www.twitter.com"
                | "mobile.twitter.com"
        ) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            ["i", "web", "status", id, ..] | ["i", "status", id, ..] if social::numeric(id) => {
                Some(Self::Post {
                    author: None,
                    id: (*id).to_owned(),
                })
            }
            [user, "status", id, ..]
                if handle(user) && !user.eq_ignore_ascii_case("i") && social::numeric(id) =>
            {
                Some(Self::Post {
                    author: Some(user.to_ascii_lowercase()),
                    id: (*id).to_owned(),
                })
            }
            [user] if handle(user) && !RESERVED.contains(&user.to_ascii_lowercase().as_str()) => {
                Some(Self::Profile(user.to_ascii_lowercase()))
            }
            _ => None,
        }
    }
    /// The one address of the object, when the link spells enough for it.
    pub fn root(&self) -> Option<String> {
        match self {
            Self::Profile(handle) => Some(format!("https://x.com/{handle}")),
            Self::Post {
                author: Some(author),
                id,
            } => Some(format!("https://x.com/{author}/status/{id}")),
            Self::Post { author: None, .. } => None,
        }
    }
}

/// The handle an `author_url` (`https://x.com/jack`) names.
fn author_of(v: &Value) -> Option<String> {
    let url = web_url(v["author_url"].as_str()?)?;
    let handle = url.path_segments()?.next()?.to_ascii_lowercase();
    super::social::handle(&handle, 15).then_some(handle)
}

/// The card of a post from its oEmbed answer: the text of the quoted post and the date it
/// names. Run under a parse slot (`html::blocking`): the answer's `html` is parsed.
pub fn post_card(mut doc: ResourceMeta, id: &str, v: &Value) -> Result<ResourceMeta> {
    let url = text(&v["url"]).context("the oEmbed answer names no post")?;
    anyhow::ensure!(
        url.ends_with(&format!("/status/{id}")),
        "X answered for another post"
    );
    let author = author_of(v);
    doc = Shape::Post.card(doc, SITE);
    let html = crate::resolver::html::fragment(v["html"].as_str().unwrap_or_default())?;
    let select = |s: &str| scraper::Selector::parse(s).expect("static selector");
    let body = post_body(html.select(&select("blockquote > p")));
    let by = text(&v["author_name"]).or_else(|| author.clone());
    social::post_text(&mut doc, &body, by.as_deref());
    let published = html
        .select(&select("blockquote > a"))
        .next_back()
        .map(|a| a.text().collect::<String>())
        .and_then(|date| chrono::NaiveDate::parse_from_str(date.trim(), "%B %d, %Y").ok())
        .map(|date| date.to_string());
    social::fact(&mut doc, "author", author.clone());
    social::fact(&mut doc, "author_name", text(&v["author_name"]));
    social::fact(&mut doc, "published", published);
    if let Some(author) = author {
        doc.source_root = Some(format!("https://x.com/{author}/status/{id}"));
    }
    Ok(doc)
}

/// A post's text from the paragraphs of its oEmbed `blockquote`: line breaks (`<br>`) and
/// paragraphs kept (`social::html_lines`), without the links X appends to the text for the
/// post's photos and videos (`pic.twitter.com/…`, `pic.x.com/…`): the post shows its media, not
/// that link.
fn post_body<'a>(paragraphs: impl Iterator<Item = scraper::ElementRef<'a>>) -> String {
    let mut text = paragraphs
        .map(|p| social::lines(&social::html_lines(p)))
        .collect::<Vec<_>>()
        .join("\n\n");
    loop {
        let kept = text.trim_end();
        let last = kept.rfind(char::is_whitespace).map_or(0, |at| {
            at + kept[at..].chars().next().map_or(1, char::len_utf8)
        });
        let media = ["pic.twitter.com/", "pic.x.com/"]
            .iter()
            .any(|host| kept[last..].starts_with(host));
        if !media {
            break;
        }
        text.truncate(last);
    }
    text.trim_end().to_owned()
}

/// The card of a profile from its page: the name and handle its Open Graph title carries
/// (`Name (@handle) on X`), its bio and its avatar.
pub fn profile_card(doc: ResourceMeta, handle: &str, page: &ResourceMeta) -> ResourceMeta {
    let mut doc = Shape::Profile.card(doc, SITE);
    let (name, titled) = page
        .name
        .as_deref()
        .map(social::name_and_handle)
        .unwrap_or_default();
    if titled.is_some_and(|t| t.eq_ignore_ascii_case(handle)) {
        social::take(&mut doc, page);
        doc.name = name.or(doc.name);
    }
    social::fact(&mut doc, "handle", Some(handle.to_owned()));
    doc
}

#[async_trait]
impl Source for X {
    fn name(&self) -> &'static str {
        "x"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "x",
                    uri: "https://x.com/jack/status/20",
                    card_type: "SocialMediaPosting",
                    group_with: None,
                },
                E2eSample {
                    kind: "x_profile",
                    uri: "https://x.com/X",
                    card_type: "Person",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "publish.x.com",
                    path: "/oembed",
                    query: &[("url", "https://x.com/jack/status/20")],
                    header: None,
                    content_type: "application/json",
                    file: "oembed-jack-20.json",
                },
                E2eAnswer {
                    host: "x.com",
                    path: "/x",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "profile-x.html.gz",
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
        &["author", "author_name", "published", "handle", "blocked"]
    }
    fn social(&self, doc: &ResourceMeta) -> bool {
        // A post, and a profile (its bio is its author's own text).
        matches!(doc.type_name.as_str(), "SocialMediaPosting" | "Person")
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://x.com/jack/status/20",
                card_type: "SocialMediaPosting",
            },
            Example {
                uri: "https://x.com/X",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not an X link")?;
        let doc = cx.card(uri, self);
        let mut doc = match &target {
            Target::Post { author, id } => {
                let link = match author {
                    Some(author) => format!("https://x.com/{author}/status/{id}"),
                    None => format!("https://x.com/i/web/status/{id}"),
                };
                match social::oembed(cx, OEMBED, &link).await? {
                    Oembed::Found(v) => {
                        let id = id.clone();
                        crate::resolver::html::blocking(move || post_card(doc, &id, &v)).await??
                    }
                    Oembed::Missing => cx.missing(uri, self),
                    Oembed::Blocked => social::blocked(doc, Shape::Post, SITE),
                }
            }
            Target::Profile(handle) => {
                match social::page(cx, &format!("https://x.com/{handle}")).await? {
                    // Only the profile's own page names it: a fetch that ended at another
                    // address read another page.
                    Page::Read { doc: page, url }
                        if Target::of_uri(url.as_str()).as_ref() == Some(&target) =>
                    {
                        profile_card(doc, handle, &page)
                    }
                    Page::Read { .. } => {
                        let mut doc = Shape::Profile.card(doc, SITE);
                        social::fact(&mut doc, "handle", Some(handle.clone()));
                        social::shape_only(doc)
                    }
                    Page::Missing => cx.missing(uri, self),
                    Page::Blocked => social::blocked(doc, Shape::Profile, SITE),
                }
            }
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

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("x", file)
    }

    /// A profile takes its name only from its own page: the same recorded page, whose title names
    /// the handle the link does, gives the plain card when the fetch ended at another address.
    #[tokio::test(start_paused = true)]
    async fn a_profile_is_named_from_its_own_page_only() {
        let own = "https://x.com/jack";
        let elsewhere = "https://x.com/explore";
        let page = read("own-page-profile-x.html.gz");
        let read_at = |ended_at: &'static str| {
            let page = page.clone();
            async move {
                let mut routes = vec![(ended_at, 200, "text/html", page)];
                if ended_at != own {
                    routes.push((own, 302, "text/html", ended_at.as_bytes().to_vec()));
                }
                let (egress, _) = crate::sources::fixture::egress(routes);
                let cx = Context {
                    egress: &egress,
                    ttl: 3600,
                };
                X.resolve(&cx, own).await.unwrap()
            }
        };
        let doc = read_at(own).await;
        assert_eq!(doc.name.as_deref(), Some("jack"));
        assert!(doc.description.is_some());
        let doc = read_at(elsewhere).await;
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.name.as_deref(), Some("Profile on x.com"));
        assert_eq!((doc.description, doc.image), (None, None));
        assert_eq!(doc.attributes["handle"], "jack");
    }

    #[test]
    fn addresses() {
        for (uri, target) in [
            (
                "https://x.com/jack/status/20",
                Target::Post {
                    author: Some("jack".into()),
                    id: "20".into(),
                },
            ),
            (
                "https://twitter.com/Jack/status/20/photo/1",
                Target::Post {
                    author: Some("jack".into()),
                    id: "20".into(),
                },
            ),
            (
                "https://mobile.twitter.com/i/web/status/20",
                Target::Post {
                    author: None,
                    id: "20".into(),
                },
            ),
            ("https://x.com/X", Target::Profile("x".into())),
            ("https://www.x.com/jack/", Target::Profile("jack".into())),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://x.com/home",
            "https://x.com/i/lists/1",
            "https://x.com/search?q=x",
            "https://x.com/jack/status/abc",
            "https://x.com/jack/likes",
            "https://x.com/a_handle_that_is_too_long",
            "https://example.com/jack/status/20",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            X.alias_root("https://twitter.com/Jack/status/20/photo/1")
                .as_deref(),
            Some("https://x.com/jack/status/20")
        );
        assert_eq!(X.alias_root("https://x.com/jack/status/20"), None);
        assert_eq!(X.alias_root("https://x.com/i/web/status/20"), None);
    }

    /// `i` is X's own path, never a handle: `/i/status/{id}` names a post without its author,
    /// and no root is spelled from it.
    #[test]
    fn a_post_under_i_has_no_author() {
        assert_eq!(
            Target::of_uri("https://x.com/i/status/20"),
            Some(Target::Post {
                author: None,
                id: "20".into(),
            })
        );
        assert_eq!(
            X.alias_root("https://twitter.com/i/status/20/photo/1"),
            None
        );
        assert_eq!(Target::of_uri("https://x.com/I/status/20"), None);
    }

    #[test]
    fn a_post_from_its_oembed_answer() {
        let v: Value = serde_json::from_slice(&read("oembed-jack-20.json")).unwrap();
        let base = ResourceMeta::new("https://twitter.com/jack/status/20", "x", 3600);
        let doc = post_card(base.clone(), "20", &v).unwrap();
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert_eq!(doc.name.as_deref(), Some("just setting up my twttr"));
        assert_eq!(doc.attributes["author"], "jack");
        assert_eq!(doc.attributes["author_name"], "jack");
        assert_eq!(doc.attributes["published"], "2006-03-21");
        assert_eq!(doc.site, "x.com");
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://x.com/jack/status/20")
        );
        assert!(post_card(base, "21", &v).is_err());
    }

    /// A post's line breaks stay in its description (`<br>` a line, `<br><br>` a blank line), its
    /// name is one line, and the `pic.twitter.com` link X appends for its photo is left out.
    #[test]
    fn a_post_keeps_its_line_breaks_without_the_photo_link() {
        let v: Value =
            serde_json::from_slice(&read("oembed-joenakamoto-2119830751713829276.json")).unwrap();
        let base = ResourceMeta::new(
            "https://x.com/joenakamoto/status/2119830751713829276",
            "x",
            3600,
        );
        let doc = post_card(base, "2119830751713829276", &v).unwrap();
        let description = doc.description.as_deref().unwrap();
        assert_eq!(
            description,
            "this guy has been on a rampage trying to dunk on me for saying bitcoin is dying in el \
             salvador\n\nwhen his own viral post from just 3 months ago concludes with the exact \
             same thing"
        );
        let name = doc.name.as_deref().unwrap();
        assert!(name.starts_with("this guy has been on a rampage"));
        assert!(!name.contains('\n') && !name.contains("pic.twitter.com"));
        assert_eq!(doc.attributes["published"], "2026-11-09");
    }

    /// Only the media links at the end go: one in the text stays, and a post of a photo alone is
    /// named by its author.
    #[test]
    fn only_trailing_media_links_are_left_out() {
        let body = |html: &str| {
            let html = crate::resolver::html::fragment(html).unwrap();
            post_body(html.select(&scraper::Selector::parse("blockquote > p").unwrap()))
        };
        assert_eq!(
            body("<blockquote><p>a  b<br>c<br><br><br>see pic.x.com/1 here</p><p>second <a href=\"https://t.co/2\">pic.twitter.com/2</a> <a href=\"https://t.co/3\">pic.x.com/3</a></p></blockquote>"),
            "a b\nc\n\nsee pic.x.com/1 here\n\nsecond"
        );
        assert_eq!(
            body("<blockquote><p>caption <a>pic.twitter.com/2</a></p></blockquote>"),
            "caption"
        );
        let v = serde_json::json!({
            "url": "https://x.com/jack/status/20",
            "author_name": "jack",
            "author_url": "https://x.com/jack",
            "html": "<blockquote><p><a href=\"https://t.co/1\">pic.twitter.com/1</a></p>&mdash; jack</blockquote>",
        });
        let base = ResourceMeta::new("https://x.com/jack/status/20", "x", 3600);
        let doc = post_card(base, "20", &v).unwrap();
        assert_eq!(doc.name.as_deref(), Some("Post by jack"));
        assert_eq!(doc.description, None);
    }

    #[test]
    fn a_profile_from_its_page() {
        let fetched = crate::sources::fixture::page(
            "https://x.com/X",
            "text/html",
            read("profile-x.html.gz"),
            None,
        );
        let page = crate::resolver::web::extract("https://x.com/X", &fetched, 3600)
            .unwrap()
            .doc;
        let base = ResourceMeta::new("https://x.com/X", "x", 3600);
        let doc = profile_card(base.clone(), "x", &page);
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("X"));
        assert_eq!(doc.description.as_deref(), Some("what's happening?!"));
        assert!(doc
            .image
            .as_deref()
            .unwrap()
            .starts_with("https://pbs.twimg.com/"));
        assert_eq!(doc.attributes["handle"], "x");
        // A page naming another handle (a redirect to someone else) gives the plain card.
        let doc = profile_card(base, "jack", &page);
        assert_eq!(doc.name.as_deref(), Some("Profile on x.com"));
        assert_eq!(doc.description, None);
    }
}
