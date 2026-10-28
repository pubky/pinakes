//! Twitch channels, videos and clips, from their pages' Open Graph tags. A channel's page names
//! it (name, bio, avatar); a video's page names its title, channel and category; a clip's page
//! names nothing to a request without a browser, so a clip gets the plain card. No key, no
//! account. Every spelling (m., a capitalised login) is one resource at
//! `https://www.twitch.tv/{login}`, `https://www.twitch.tv/videos/{id}` or
//! `https://clips.twitch.tv/{slug}`.
use super::{
    social::{self, Page, Shape},
    web_url, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct Twitch;

const SITE: &str = "twitch.tv";
/// First path segments that are Twitch's own pages, never a login.
const RESERVED: &[&str] = &[
    "about",
    "bits",
    "broadcast",
    "clip",
    "collections",
    "dashboard",
    "directory",
    "downloads",
    "drops",
    "embed",
    "events",
    "friends",
    "inventory",
    "jobs",
    "legal",
    "login",
    "moderator",
    "p",
    "popout",
    "prime",
    "privacy",
    "products",
    "search",
    "settings",
    "signup",
    "store",
    "subscriptions",
    "team",
    "turbo",
    "u",
    "user",
    "videos",
    "wallet",
];

/// A login: 3–25 letters, digits and underscores.
fn login(value: &str) -> bool {
    (3..=25).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !RESERVED.contains(&value.to_ascii_lowercase().as_str())
}
/// A clip slug: letters, digits, `-` and `_`.
fn slug(value: &str) -> bool {
    (8..=120).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What a Twitch link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Channel(String),
    Video(String),
    Clip {
        channel: Option<String>,
        slug: String,
    },
}

impl Target {
    /// `https://www.twitch.tv/{login}`, `/videos/{id}`, `/{login}/video/{id}` (also `/v/`),
    /// `/{login}/clip/{slug}`, and `https://clips.twitch.tv/{slug}`; also m.twitch.tv and the
    /// host without `www.`.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match url.host_str()? {
            "clips.twitch.tv" => match segments.as_slice() {
                [clip] if slug(clip) => Some(Self::Clip {
                    channel: None,
                    slug: (*clip).to_owned(),
                }),
                _ => None,
            },
            "twitch.tv" | "www.twitch.tv" | "m.twitch.tv" => match segments.as_slice() {
                ["videos", id] if social::numeric(id) => Some(Self::Video((*id).to_owned())),
                [user, "video" | "v", id] if login(user) && social::numeric(id) => {
                    Some(Self::Video((*id).to_owned()))
                }
                [user, "clip", clip] if login(user) && slug(clip) => Some(Self::Clip {
                    channel: Some(user.to_ascii_lowercase()),
                    slug: (*clip).to_owned(),
                }),
                [user] if login(user) => Some(Self::Channel(user.to_ascii_lowercase())),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Channel(login) => format!("https://www.twitch.tv/{login}"),
            Self::Video(id) => format!("https://www.twitch.tv/videos/{id}"),
            Self::Clip { slug, .. } => format!("https://clips.twitch.tv/{slug}"),
        }
    }
    fn shape(&self) -> Shape {
        match self {
            Self::Channel(_) => Shape::Profile,
            _ => Shape::Video,
        }
    }
}

/// The card of a channel from its page, or `None` when the page is not a profile (its Open
/// Graph type is Twitch's own `website`: no such channel). The extractor has read the name
/// from the title (`Name - Twitch`) and the bio from the description.
pub fn channel_card(doc: ResourceMeta, login: &str, page: &ResourceMeta) -> Option<ResourceMeta> {
    if page.type_name != "Person" {
        return None;
    }
    let mut doc = Shape::Profile.card(doc, SITE);
    social::take(&mut doc, page);
    doc.description = doc
        .description
        .as_deref()
        .map(|bio| social::excerpt(bio, 200));
    social::fact(&mut doc, "handle", Some(login.to_owned()));
    Some(doc)
}

/// The card of a video from its page: its title and channel from the title
/// (`Title - login on Twitch`) and its category from the description (`… Catch up on their
/// Category VOD now.`). A page that is not a video's (what a request without a browser gets for
/// a clip, or a video that is gone) gives the plain card.
pub fn video_card(doc: ResourceMeta, target: &Target, page: &ResourceMeta) -> ResourceMeta {
    let mut doc = Shape::Video.card(doc, SITE);
    if let Target::Clip {
        channel: Some(channel),
        ..
    } = target
    {
        social::fact(&mut doc, "author", Some(channel.clone()));
    }
    let Some((title, login)) = page
        .name
        .as_deref()
        .filter(|_| page.type_name == "VideoObject")
        .map(|t| t.strip_suffix(" on Twitch").unwrap_or(t))
        .and_then(|t| t.rsplit_once(" - "))
    else {
        return doc;
    };
    if title.trim().is_empty() || !login_ok(login) {
        return doc;
    }
    doc.name = Some(social::excerpt(title, 120));
    doc.image = page.image.clone();
    social::fact(&mut doc, "author", Some(login.to_ascii_lowercase()));
    social::fact(
        &mut doc,
        "category",
        page.description
            .as_deref()
            .and_then(|d| {
                d.split_once("Catch up on their ")?
                    .1
                    .strip_suffix(" VOD now.")
            })
            .map(str::to_owned),
    );
    doc
}
fn login_ok(value: &str) -> bool {
    (1..=25).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[async_trait]
impl Source for Twitch {
    fn name(&self) -> &'static str {
        "twitch"
    }
    /// A video's title is its creator's own text, as a TikTok's or a reel's is.
    fn social(&self, doc: &ResourceMeta) -> bool {
        doc.type_name == "VideoObject" || crate::changes::gone_post_type(doc)
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "twitch",
                    uri: "https://www.twitch.tv/twitch",
                    card_type: "Person",
                    group_with: None,
                },
                E2eSample {
                    kind: "twitch_video",
                    uri: "https://www.twitch.tv/videos/2896433713",
                    card_type: "VideoObject",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "www.twitch.tv",
                    path: "/twitch",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "channel-twitch.html.gz",
                },
                E2eAnswer {
                    host: "www.twitch.tv",
                    path: "/videos/2896433713",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "video-2896433713.html.gz",
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
        &["author", "category", "handle", "blocked"]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.twitch.tv/twitch",
                card_type: "Person",
            },
            Example {
                uri: "https://www.twitch.tv/videos/2896433713",
                card_type: "VideoObject",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Twitch link")?;
        let doc = cx.card(uri, self);
        let mut doc = match social::page(cx, &target.root()).await? {
            Page::Read { doc: page, .. } => match &target {
                Target::Channel(login) => {
                    channel_card(doc, login, &page).unwrap_or_else(|| cx.missing(uri, self))
                }
                _ => video_card(doc, &target, &page),
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
            crate::sources::fixture::read("twitch", file),
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
                "https://www.twitch.tv/Twitch",
                Target::Channel("twitch".into()),
            ),
            (
                "https://m.twitch.tv/twitch/",
                Target::Channel("twitch".into()),
            ),
            (
                "https://twitch.tv/videos/2896433713",
                Target::Video("2896433713".into()),
            ),
            (
                "https://www.twitch.tv/twitch/v/2896433713",
                Target::Video("2896433713".into()),
            ),
            (
                "https://www.twitch.tv/twitch/clip/BoldRelievedElephantTwitchRPG-LcSvPfVEjpg_OTrK",
                Target::Clip {
                    channel: Some("twitch".into()),
                    slug: "BoldRelievedElephantTwitchRPG-LcSvPfVEjpg_OTrK".into(),
                },
            ),
            (
                "https://clips.twitch.tv/BoldRelievedElephantTwitchRPG-LcSvPfVEjpg_OTrK",
                Target::Clip {
                    channel: None,
                    slug: "BoldRelievedElephantTwitchRPG-LcSvPfVEjpg_OTrK".into(),
                },
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://www.twitch.tv/directory/category/x",
            "https://www.twitch.tv/videos/abc",
            "https://www.twitch.tv/twitch/about",
            "https://www.twitch.tv/ab",
            "https://player.twitch.tv/?video=1",
            "https://example.com/twitch",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            Twitch.alias_root("https://m.twitch.tv/Twitch/").as_deref(),
            Some("https://www.twitch.tv/twitch")
        );
        assert_eq!(Twitch.alias_root("https://www.twitch.tv/twitch"), None);
    }

    #[test]
    fn a_channel_from_its_page_and_no_such_channel() {
        let base = ResourceMeta::new("https://www.twitch.tv/twitch", "twitch", 3600);
        let doc = channel_card(
            base.clone(),
            "twitch",
            &page("https://www.twitch.tv/twitch", "channel-twitch.html.gz"),
        )
        .unwrap();
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Twitch"));
        let bio = doc.description.as_deref().unwrap();
        assert!(
            bio.starts_with("Twitch is where") && !bio.contains("streams live"),
            "{bio}"
        );
        assert!(doc
            .image
            .as_deref()
            .unwrap()
            .starts_with("https://static-cdn"));
        assert_eq!(doc.attributes["handle"], "twitch");
        assert!(channel_card(
            base,
            "thisuserdoesnotexist12345",
            &page(
                "https://www.twitch.tv/thisuserdoesnotexist12345",
                "channel-missing.html.gz"
            ),
        )
        .is_none());
    }

    /// A video's card quotes its creator (social, purged when the video goes); a channel's is
    /// the channel's.
    #[test]
    fn videos_are_social_content() {
        let mut doc = ResourceMeta::new("https://www.twitch.tv/videos/1", "twitch", 60);
        doc.set_type("VideoObject");
        assert!(Twitch.social(&doc));
        doc.set_type("Person");
        assert!(!Twitch.social(&doc));
    }

    #[test]
    fn a_video_from_its_page_and_a_clip_without_one() {
        let video = Target::Video("2896433713".into());
        let base = ResourceMeta::new("https://www.twitch.tv/videos/2896433713", "twitch", 3600);
        let doc = video_card(
            base,
            &video,
            &page(
                "https://www.twitch.tv/videos/2896433713",
                "video-2896433713.html.gz",
            ),
        );
        assert_eq!(doc.type_name, "VideoObject");
        assert!(doc
            .name
            .as_deref()
            .unwrap()
            .starts_with("Let's Chat Episode 13"));
        assert_eq!(doc.attributes["author"], "twitch");
        assert_eq!(doc.attributes["category"], "Talk Shows & Podcasts");
        let clip = Target::Clip {
            channel: Some("twitch".into()),
            slug: "BoldRelievedElephantTwitchRPG-LcSvPfVEjpg_OTrK".into(),
        };
        let base = ResourceMeta::new(&clip.root(), "twitch", 3600);
        let doc = video_card(
            base,
            &clip,
            &page(&clip.root(), "clip-BoldRelievedElephantTwitchRPG.html.gz"),
        );
        assert_eq!(doc.name.as_deref(), Some("Video on twitch.tv"));
        assert_eq!(doc.attributes["author"], "twitch");
        assert_eq!(doc.image, None);
    }
}
