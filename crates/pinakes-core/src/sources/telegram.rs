//! Telegram public channels and their posts, from t.me's public pages: a channel's preview page
//! (its Open Graph tags: name, description, photo) and a post's widget page (`?embed=1`, what
//! Telegram's post widget loads: text, channel, time, views, photo). No key, no account; a
//! private channel, a user or a group without a public page gets the plain card. Every spelling
//! (telegram.me, `/s/`, a capitalised name) is one resource at `https://t.me/{channel}[/{n}]`.
use super::{
    social::{self, Body, Page, Shape},
    web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};

pub struct Telegram;

const SITE: &str = "t.me";
/// First path segments that are t.me's own pages, never a public name.
const RESERVED: &[&str] = &[
    "addemoji",
    "addstickers",
    "addtheme",
    "bg",
    "boost",
    "c",
    "confirmphone",
    "giftcode",
    "invoice",
    "iv",
    "joinchat",
    "login",
    "m",
    "proxy",
    "setlanguage",
    "share",
    "socks",
];

/// A public name: 5–32 letters, digits and underscores (an invite link starts with `+`).
fn name(value: &str) -> bool {
    (5..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !RESERVED.contains(&value.to_ascii_lowercase().as_str())
}

/// What a Telegram link names: a channel (or any public name), or a post in a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub channel: String,
    pub post: Option<String>,
}

impl Target {
    /// `https://t.me/{name}`, `/{name}/{n}`, and the same under `/s/` (the preview), on t.me
    /// and telegram.me.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = super::kit::web_url(uri)?;
        if !matches!(
            url.host_str()?,
            "t.me" | "www.t.me" | "telegram.me" | "www.telegram.me"
        ) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let segments = match segments.as_slice() {
            ["s", rest @ ..] => rest,
            all => all,
        };
        let (channel, post) = match segments {
            [channel] if name(channel) => (channel, None),
            [channel, n] if name(channel) && social::numeric(n) => (channel, Some(*n)),
            _ => return None,
        };
        Some(Self {
            channel: channel.to_ascii_lowercase(),
            post: post.map(str::to_owned),
        })
    }
    pub fn root(&self) -> String {
        match &self.post {
            Some(n) => format!("https://t.me/{}/{n}", self.channel),
            None => format!("https://t.me/{}", self.channel),
        }
    }
}

fn select(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}
fn first<'a>(html: &'a Html, s: &str) -> Option<ElementRef<'a>> {
    html.select(&select(s)).next()
}
/// The URL in a `background-image:url('…')` style.
fn background_image(style: &str) -> Option<String> {
    let (_, rest) = style.split_once("url(")?;
    let url = rest
        .split_once(')')?
        .0
        .trim_matches(|c| c == '\'' || c == '"');
    web_url(url).map(|u| u.to_string())
}

/// The card of a post from its widget page, or `None` when the widget says there is no such
/// post. Run under a parse slot (`html::blocking`).
pub fn post_card(doc: ResourceMeta, target: &Target, body: &str) -> Result<Option<ResourceMeta>> {
    let html = crate::resolver::html::document(body)?;
    if first(&html, ".tgme_widget_message_error").is_some() {
        return Ok(None);
    }
    let message = first(&html, ".tgme_widget_message").context("no post widget")?;
    let mut doc = Shape::Post.card(doc, SITE);
    let text = |s: &str| {
        message
            .select(&select(s))
            .next()
            .map(|e| e.text().collect::<String>())
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
    };
    let by = text(".tgme_widget_message_owner_name");
    let body = message
        .select(&select(".tgme_widget_message_text"))
        .next()
        .and_then(|e| super::kit::html_text(&e.inner_html()))
        .unwrap_or_default();
    social::post_text(&mut doc, &body, by.as_deref());
    doc.image = message
        .select(&select(
            ".tgme_widget_message_photo_wrap, .tgme_widget_message_video_thumb",
        ))
        .find_map(|e| background_image(e.value().attr("style")?));
    social::fact(&mut doc, "author", Some(target.channel.clone()));
    social::fact(&mut doc, "author_name", by);
    social::fact(
        &mut doc,
        "published",
        message
            .select(&select(".tgme_widget_message_date time"))
            .next()
            .and_then(|t| t.value().attr("datetime"))
            .map(str::to_owned),
    );
    social::fact(&mut doc, "views", text(".tgme_widget_message_views"));
    Ok(Some(doc))
}

/// The card of a channel from its preview page: its name, description and photo. A name
/// without a public page (a user, a private channel, no such name) gets the page "Telegram:
/// Contact @name": the plain card.
pub fn profile_card(doc: ResourceMeta, target: &Target, page: &ResourceMeta) -> ResourceMeta {
    let mut doc = Shape::Profile.card(doc, SITE);
    if page
        .name
        .as_deref()
        .is_some_and(|name| !name.starts_with("Telegram: Contact @"))
    {
        social::take(&mut doc, page);
        doc.description = doc.description.as_deref().map(|d| social::excerpt(d, 200));
    }
    social::fact(&mut doc, "handle", Some(target.channel.clone()));
    doc
}

#[async_trait]
impl Source for Telegram {
    fn name(&self) -> &'static str {
        "telegram"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "telegram",
                    uri: "https://t.me/telegram/300",
                    card_type: "SocialMediaPosting",
                    group_with: None,
                },
                E2eSample {
                    kind: "telegram_channel",
                    uri: "https://t.me/s/telegram",
                    card_type: "Person",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "t.me",
                    path: "/telegram/300",
                    query: &[("embed", "1")],
                    header: None,
                    content_type: "text/html",
                    file: "post-telegram-300.html",
                },
                E2eAnswer {
                    host: "t.me",
                    path: "/telegram",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "channel-telegram.html",
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
        &[
            "author",
            "author_name",
            "published",
            "views",
            "handle",
            "blocked",
        ]
    }
    fn social(&self, doc: &ResourceMeta) -> bool {
        // A post, and a profile (its bio is its author's own text).
        matches!(doc.type_name.as_str(), "SocialMediaPosting" | "Person")
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://t.me/telegram/300",
                card_type: "SocialMediaPosting",
            },
            Example {
                uri: "https://t.me/telegram",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Telegram link")?;
        let doc = cx.card(uri, self);
        let mut doc = if target.post.is_some() {
            match social::page_body(cx, &format!("{}?embed=1", target.root()), &[]).await? {
                Body::Read { body, .. } => {
                    let read = target.clone();
                    crate::resolver::html::blocking(move || post_card(doc, &read, &body))
                        .await??
                        .unwrap_or_else(|| cx.missing(uri, self))
                }
                Body::Missing => cx.missing(uri, self),
                Body::Blocked => social::blocked(doc, Shape::Post, SITE),
            }
        } else {
            match social::page(cx, &target.root()).await? {
                Page::Read { doc: page, .. } => profile_card(doc, &target, &page),
                Page::Missing => cx.missing(uri, self),
                Page::Blocked => social::blocked(doc, Shape::Profile, SITE),
            }
        };
        doc.source_root = Some(target.root());
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> String {
        String::from_utf8(crate::sources::fixture::read("telegram", file)).unwrap()
    }
    fn page(uri: &str, file: &str) -> ResourceMeta {
        let fetched = crate::sources::fixture::page(
            uri,
            "text/html",
            crate::sources::fixture::read("telegram", file),
            None,
        );
        crate::resolver::web::extract(uri, &fetched, 3600)
            .unwrap()
            .doc
    }
    fn target(post: Option<&str>) -> Target {
        Target {
            channel: "telegram".into(),
            post: post.map(str::to_owned),
        }
    }

    #[test]
    fn addresses() {
        for (uri, post) in [
            ("https://t.me/telegram", None),
            ("https://t.me/s/Telegram", None),
            ("https://telegram.me/telegram/300", Some("300")),
            ("https://t.me/s/telegram/300?single", Some("300")),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target(post)), "{uri}");
        }
        for uri in [
            "https://t.me/+AbCdEf",
            "https://t.me/joinchat/AbCdEf",
            "https://t.me/c/1234567/89",
            "https://t.me/abc",
            "https://t.me/telegram/300/comments",
            "https://t.me/share/url?url=x",
            "https://example.com/telegram/300",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            Telegram
                .alias_root("https://t.me/s/Telegram/300")
                .as_deref(),
            Some("https://t.me/telegram/300")
        );
        assert_eq!(Telegram.alias_root("https://t.me/telegram/300"), None);
        assert_eq!(
            background_image("background-image:url('https://cdn4.telesco.pe/file/a.jpg')")
                .as_deref(),
            Some("https://cdn4.telesco.pe/file/a.jpg")
        );
    }

    #[test]
    fn a_post_from_its_widget_page() {
        let base = ResourceMeta::new("https://t.me/telegram/300", "telegram", 3600);
        let doc = post_card(
            base.clone(),
            &target(Some("300")),
            &read("post-telegram-300.html"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert!(doc.name.as_deref().unwrap().starts_with("Birthdays."));
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .contains("date of birth"));
        assert_eq!(doc.attributes["author"], "telegram");
        assert_eq!(doc.attributes["author_name"], "Telegram News");
        assert_eq!(doc.attributes["published"], "2024-04-29T12:29:41+00:00");
        assert_eq!(doc.attributes["views"], "2.82M");
        assert!(doc.image.as_deref().unwrap().starts_with("https://"));
        assert!(
            post_card(base, &target(Some("999999999")), &read("post-missing.html"))
                .unwrap()
                .is_none()
        );
    }

    /// A post t.me withholds where the request came from (451) is the blocked card, as a
    /// channel's page is: not an error, not `missing`.
    #[tokio::test(start_paused = true)]
    async fn a_withheld_post_is_the_blocked_card() {
        let (egress, _) = crate::sources::fixture::egress(vec![(
            "https://t.me/telegram/300?embed=1",
            451,
            "text/html",
            b"<html><title>Unavailable</title></html>".to_vec(),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Telegram
            .resolve(&cx, "https://t.me/telegram/300")
            .await
            .unwrap();
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert_eq!(doc.attributes["blocked"], "true");
    }

    /// A post whose fetch ends at a sign-in page is the blocked card at the post's root: every
    /// spelling of the link stays one resource while t.me refuses.
    #[tokio::test(start_paused = true)]
    async fn a_walled_post_keeps_its_root() {
        let (egress, _) = crate::sources::fixture::egress(vec![(
            "https://t.me/telegram/300?embed=1",
            302,
            "text/html",
            b"https://t.me/login".to_vec(),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Telegram
            .resolve(&cx, "https://t.me/s/Telegram/300")
            .await
            .unwrap();
        assert_eq!(doc.attributes["blocked"], "true");
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://t.me/telegram/300")
        );
    }

    #[test]
    fn a_channel_from_its_page_and_a_name_without_one() {
        let base = ResourceMeta::new("https://t.me/telegram", "telegram", 3600);
        let doc = profile_card(
            base.clone(),
            &target(None),
            &page("https://t.me/telegram", "channel-telegram.html"),
        );
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Telegram News"));
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .starts_with("The official Telegram"));
        assert!(doc.image.as_deref().unwrap().starts_with("https://cdn"));
        assert_eq!(doc.attributes["handle"], "telegram");
        let doc = profile_card(
            base,
            &target(None),
            &page(
                "https://t.me/thischanneldoesnotexist12345",
                "channel-missing.html",
            ),
        );
        assert_eq!(doc.name.as_deref(), Some("Profile on t.me"));
        assert_eq!((doc.description, doc.image), (None, None));
    }
}
