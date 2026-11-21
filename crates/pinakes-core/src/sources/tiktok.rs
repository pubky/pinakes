//! TikTok videos and profiles. A profile's name comes from TikTok's oEmbed endpoint; a video is
//! a link rule (TikTok names a video to a request without a browser by its id alone): its card
//! is the link's root and shape, with the author the link spells, and nothing is asked. No key, no account. Every spelling (a capitalised handle, the
//! host without `www.`) is one resource at `https://www.tiktok.com/@{handle}[/video/{id}]`.
use super::{
    social::{self, Oembed, Shape},
    text, Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct TikTok;

const SITE: &str = "tiktok.com";
const OEMBED: &str = "https://www.tiktok.com/oembed";

/// A handle: 2–24 letters, digits, `_` and `.`.
fn handle(value: &str) -> bool {
    (2..=24).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

/// What a TikTok link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Profile(String),
    Video { author: String, id: String },
}

impl Target {
    /// `https://www.tiktok.com/@{handle}` and `https://www.tiktok.com/@{handle}/video/{id}`, on
    /// tiktok.com with or without `www.` or `m.`. Short links (`vm.tiktok.com`, `/t/`) name
    /// nothing without a fetch and are not read.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = super::kit::web_url(uri)?;
        if !matches!(
            url.host_str()?,
            "tiktok.com" | "www.tiktok.com" | "m.tiktok.com"
        ) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let user = |s: &str| {
            s.strip_prefix('@')
                .filter(|h| handle(h))
                .map(str::to_ascii_lowercase)
        };
        match segments.as_slice() {
            [u] => user(u).map(Self::Profile),
            [u, "video", id] if social::numeric(id) => Some(Self::Video {
                author: user(u)?,
                id: (*id).to_owned(),
            }),
            _ => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Profile(handle) => format!("https://www.tiktok.com/@{handle}"),
            Self::Video { author, id } => format!("https://www.tiktok.com/@{author}/video/{id}"),
        }
    }
    fn shape(&self) -> Shape {
        match self {
            Self::Profile(_) => Shape::Profile,
            Self::Video { .. } => Shape::Video,
        }
    }
}

/// The handle an `author_url` (`https://www.tiktok.com/@tiktok`) names.
fn author_of(v: &Value) -> Option<String> {
    match Target::of_uri(v["author_url"].as_str()?)? {
        Target::Profile(handle) => Some(handle),
        Target::Video { .. } => None,
    }
}

/// The card of a profile from its oEmbed answer (`TikTok's Creator Profile`, by its name).
pub fn profile_card(doc: ResourceMeta, handle: &str, v: &Value) -> ResourceMeta {
    let mut doc = Shape::Profile.card(doc, SITE);
    if author_of(v).is_some_and(|h| h == handle) {
        doc.name = text(&v["author_name"]).or(doc.name);
    }
    social::fact(&mut doc, "handle", Some(handle.to_owned()));
    doc
}

#[async_trait]
impl Source for TikTok {
    fn name(&self) -> &'static str {
        "tiktok"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "tiktok",
                    uri: "https://www.tiktok.com/@scout2015/video/6718335390845095173",
                    card_type: "VideoObject",
                    group_with: None,
                },
                E2eSample {
                    kind: "tiktok_profile",
                    uri: "https://www.tiktok.com/@tiktok",
                    card_type: "Person",
                    group_with: None,
                },
            ],
            answers: &[E2eAnswer {
                host: "www.tiktok.com",
                path: "/oembed",
                query: &[("url", "https://www.tiktok.com/@tiktok")],
                header: None,
                content_type: "application/json",
                file: "oembed-profile.json",
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
    /// A video and a profile are social content (a video is its author's, a profile's name its
    /// owner's own).
    fn social(&self, doc: &ResourceMeta) -> bool {
        // A video, and a profile (its bio is its author's own text).
        matches!(doc.type_name.as_str(), "VideoObject" | "Person")
            || crate::changes::gone_post_type(doc)
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.tiktok.com/@scout2015/video/6718335390845095173",
                card_type: "VideoObject",
            },
            Example {
                uri: "https://www.tiktok.com/@tiktok",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a TikTok link")?;
        let doc = cx.card(uri, self);
        // A video's oEmbed answer names no more than the link does, so it is never asked: the
        // card is the link's (its root, its shape and the handle the link spells).
        if let Target::Video { author, .. } = &target {
            let mut doc = Shape::Video.card(doc, SITE);
            social::fact(&mut doc, "author", Some(author.clone()));
            doc.source_root = Some(target.root());
            return Ok(doc);
        }
        let mut doc = match social::oembed(cx, OEMBED, &target.root()).await? {
            Oembed::Found(v) => match &target {
                Target::Profile(handle) => profile_card(doc, handle, &v),
                Target::Video { .. } => unreachable!("a video is answered above"),
            },
            Oembed::Missing => cx.missing(uri, self),
            Oembed::Blocked => social::blocked(doc, target.shape(), SITE),
        };
        doc.source_root = Some(target.root());
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Value {
        serde_json::from_slice(&crate::sources::fixture::read("tiktok", file)).unwrap()
    }

    #[test]
    fn addresses() {
        for (uri, target) in [
            (
                "https://www.tiktok.com/@TikTok",
                Target::Profile("tiktok".into()),
            ),
            (
                "https://tiktok.com/@scout2015/video/6718335390845095173?lang=en",
                Target::Video {
                    author: "scout2015".into(),
                    id: "6718335390845095173".into(),
                },
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://www.tiktok.com/tiktok",
            "https://www.tiktok.com/@tiktok/live",
            "https://www.tiktok.com/@x/video/abc",
            "https://vm.tiktok.com/ZMabc/",
            "https://www.tiktok.com/t/ZTabc/",
            "https://example.com/@tiktok",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            TikTok
                .alias_root("https://www.tiktok.com/@TikTok")
                .as_deref(),
            Some("https://www.tiktok.com/@tiktok")
        );
        assert_eq!(TikTok.alias_root("https://www.tiktok.com/@tiktok"), None);
    }

    #[test]
    fn a_profile_from_its_oembed_answer() {
        let base = ResourceMeta::new("https://www.tiktok.com/@tiktok", "tiktok", 3600);
        let doc = profile_card(base.clone(), "tiktok", &read("oembed-profile.json"));
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("TikTok"));
        assert_eq!(doc.attributes["handle"], "tiktok");
        let doc = profile_card(base, "someone", &read("oembed-profile.json"));
        assert_eq!(doc.name.as_deref(), Some("Profile on tiktok.com"));
    }
}
