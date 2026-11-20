//! What the social-platform sources share. Each reads its platform's public page (its Open
//! Graph tags, with any JSON-LD) or its oEmbed endpoint, never an account's view, and gives one
//! of three cards: a post, a profile or a video. A platform that refuses the request (a sign-in
//! redirect, a challenge page, a 401 or 403, or a status of its own that its source names) still
//! gets a card, of the link's shape alone, marked `blocked`: the link resolves, groups and counts
//! while the platform shows the object to accounts only.
pub use super::excerpt;
use super::Context;
use crate::{
    model::{ResourceMeta, Status},
    resolver::{
        egress::{FetchOptions, Fetched},
        html,
        interstitial::Blocked,
        web,
    },
};
use anyhow::{bail, Result};
use serde_json::Value;

/// What a link names on a platform, and so the card's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Post,
    Profile,
    Video,
}
impl Shape {
    pub fn type_name(self) -> &'static str {
        match self {
            Self::Post => "SocialMediaPosting",
            Self::Profile => "Person",
            Self::Video => "VideoObject",
        }
    }
    fn noun(self) -> &'static str {
        match self {
            Self::Post => "Post",
            Self::Profile => "Profile",
            Self::Video => "Video",
        }
    }
    /// A card of this shape named by what the link is alone ("Post on x.com"): the card of a
    /// platform that shows nothing to a request without an account, and the start of every
    /// other.
    pub fn card(self, mut doc: ResourceMeta, site: &str) -> ResourceMeta {
        doc.set_type(self.type_name());
        doc.site = site.into();
        doc.name = Some(format!("{} on {site}", self.noun()));
        doc
    }
}

/// The card of a link whose platform refused the request: its shape alone, marked `blocked`.
pub fn blocked(doc: ResourceMeta, shape: Shape, site: &str) -> ResourceMeta {
    let mut doc = shape.card(doc, site);
    doc.description = Some("Blocked by the site.".into());
    doc.attributes.insert("blocked".into(), "true".into());
    doc
}

/// What a platform answered for one of its pages.
pub enum Page {
    /// The page, read by the web resolver's extractor: its Open Graph name, description, image
    /// and site name, and what its JSON-LD gave (an event's dates), with the URL the fetch ended
    /// at. A source takes the fields it wants from it; identity (aliases) stays the link's.
    Read {
        doc: Box<ResourceMeta>,
        url: url::Url,
    },
    /// The platform says there is no such object.
    Missing,
    /// The platform refused the request.
    Blocked,
}

/// A request to a platform, or `None` when the platform refused it: an access wall, or one of
/// `refused`, the statuses the platform itself answers a request it refuses without an account.
/// Rate limits and server errors are `RetryLater`, as `Context::fetch` makes them; a refused
/// status may be above 500, and is answered before that rule would read it as a server error.
async fn fetch(
    cx: &Context<'_>,
    url: &str,
    options: &FetchOptions,
    refused: &[u16],
) -> Result<Option<Fetched>> {
    let fetched = match cx.egress.fetch_with(url, options).await {
        Ok(fetched) => fetched,
        Err(e) if e.is::<Blocked>() => return Ok(None),
        Err(e) => return Err(e),
    };
    if refused.contains(&fetched.status) {
        return Ok(None);
    }
    super::retry(&fetched)?;
    Ok(Some(fetched))
}

/// Fetch a platform's public page.
pub async fn page(cx: &Context<'_>, url: &str) -> Result<Page> {
    let Some(fetched) = fetch(cx, url, &FetchOptions::default(), &[]).await? else {
        return Ok(Page::Blocked);
    };
    match fetched.status {
        200 => {}
        404 | 410 | 451 => return Ok(Page::Missing),
        401 | 403 => return Ok(Page::Blocked),
        s => bail!("the site answered {s}"),
    }
    let (uri, ttl, ended_at) = (url.to_owned(), cx.ttl, fetched.url.clone());
    let extracted = html::blocking(move || web::extract(&uri, &fetched, ttl)).await??;
    Ok(match extracted.doc.status {
        Status::Ok => Page::Read {
            doc: Box::new(extracted.doc),
            url: ended_at,
        },
        Status::Missing => Page::Missing,
        _ => Page::Blocked,
    })
}

/// A page's name, description and image onto a card, where the page has them.
pub fn take(doc: &mut ResourceMeta, page: &ResourceMeta) {
    if page.name.is_some() {
        doc.name = page.name.clone();
    }
    doc.description = page.description.clone();
    doc.image = page.image.clone();
}

/// What an oEmbed endpoint answered for a link.
pub enum Oembed {
    Found(Value),
    /// The provider says there is no such object (404, or 400 naming the link).
    Missing,
    Blocked,
}

/// The oEmbed answer of `endpoint` (which may carry fixed parameters) for `url`.
pub async fn oembed(cx: &Context<'_>, endpoint: &str, url: &str) -> Result<Oembed> {
    anyhow::ensure!(
        !crate::capability::is_capability(url),
        "a link that carries a credential is not sent to oEmbed"
    );
    let mut request = url::Url::parse(endpoint)?;
    request.query_pairs_mut().append_pair("url", url);
    let options = FetchOptions::json();
    let Some(fetched) = fetch(cx, request.as_str(), &options, &[]).await? else {
        return Ok(Oembed::Blocked);
    };
    match fetched.status {
        200 => match serde_json::from_slice::<Value>(&fetched.body) {
            Ok(v) if v.is_object() => Ok(Oembed::Found(v)),
            _ => bail!("the oEmbed answer is not JSON"),
        },
        400 | 404 => Ok(Oembed::Missing),
        401 | 403 => Ok(Oembed::Blocked),
        s => bail!("the oEmbed endpoint answered {s}"),
    }
}

/// What a platform answered for one of its pages, as text, for a source that reads the page's
/// tags itself.
pub enum Body {
    Read { body: String, url: url::Url },
    Missing,
    Blocked,
}

/// Fetch a platform's public page as text. `refused` holds the statuses the platform itself
/// answers a request it refuses without an account (LinkedIn's 999); most platforms have none.
pub async fn page_body(cx: &Context<'_>, url: &str, refused: &[u16]) -> Result<Body> {
    let Some(fetched) = fetch(cx, url, &FetchOptions::default(), refused).await? else {
        return Ok(Body::Blocked);
    };
    Ok(match fetched.status {
        200 => Body::Read {
            body: String::from_utf8_lossy(&fetched.body).into_owned(),
            url: fetched.url,
        },
        404 | 410 | 451 => Body::Missing,
        401 | 403 => Body::Blocked,
        s => bail!("the site answered {s}"),
    })
}

/// A page's Open Graph tags as written, for a platform whose JSON-LD describes other things
/// than the page (LinkedIn's lists recent posts).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OpenGraph {
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub kind: Option<String>,
}

/// The Open Graph tags of a page. Run under a parse slot (`html::blocking`).
pub fn open_graph(body: &str) -> Result<OpenGraph> {
    let html = html::document(body)?;
    let selector = scraper::Selector::parse("meta[property]").expect("static selector");
    let mut tags = OpenGraph::default();
    for meta in html.select(&selector) {
        let (Some(property), Some(content)) =
            (meta.value().attr("property"), meta.value().attr("content"))
        else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let slot = match property {
            "og:title" => &mut tags.title,
            "og:description" => &mut tags.description,
            "og:image" => &mut tags.image,
            "og:type" => &mut tags.kind,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(content.to_owned());
        }
    }
    tags.image = tags.image.filter(|i| crate::sources::web_url(i).is_some());
    Ok(tags)
}

/// A post's text as its card's name (its first 120 characters) and description (200); a post
/// without text is named by its author.
pub fn post_text(doc: &mut ResourceMeta, text: &str, by: Option<&str>) {
    let flat = excerpt(text, usize::MAX);
    if flat.is_empty() {
        if let Some(by) = by {
            doc.name = Some(format!("Post by {by}"));
        }
        doc.description = None;
    } else {
        doc.name = Some(excerpt(&flat, 120));
        doc.description = Some(excerpt(&flat, 200));
    }
}

/// The name and the handle an Open Graph title carries: `Name (@handle) on X`,
/// `Name (@handle) • Threads`. The handle is the last `(@…)` of the title: the name before it is
/// the account's own text and may hold one too.
pub fn name_and_handle(title: &str) -> (Option<String>, Option<String>) {
    let Some((name, rest)) = title.rsplit_once(" (@") else {
        return (trimmed(title), None);
    };
    let handle = rest.split_once(')').map(|(handle, _)| handle);
    (trimmed(name), handle.and_then(trimmed))
}
fn trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// A handle of letters, digits, `_`, `.` and `-`, up to `max` bytes.
pub fn handle(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// A decimal id of up to 20 digits.
pub fn numeric(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit())
}

/// A fact on a card, when there is a value.
pub fn fact(doc: &mut ResourceMeta, key: &str, value: Option<String>) {
    if let Some(value) = value {
        doc.attributes.insert(key.into(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_name_and_handle() {
        assert_eq!(
            name_and_handle("X (@X) on X"),
            (Some("X".into()), Some("X".into()))
        );
        assert_eq!(
            name_and_handle("Threads (@threads) • Threads, Say more"),
            (Some("Threads".into()), Some("threads".into()))
        );
        assert_eq!(
            name_and_handle("Twitch - Twitch"),
            (Some("Twitch - Twitch".into()), None)
        );
        assert_eq!(name_and_handle("  "), (None, None));
        // A display name is the account's own text: a handle written into it is not the handle.
        assert_eq!(
            name_and_handle("Zuck (@zuck) fan (@mallory) on Threads"),
            (Some("Zuck (@zuck) fan".into()), Some("mallory".into()))
        );
    }

    #[test]
    fn a_post_is_named_by_its_text_or_its_author() {
        let mut doc = ResourceMeta::new("https://x.com/a/status/1", "x", 3600);
        post_text(&mut doc, "  just   setting\nup my twttr ", Some("jack"));
        assert_eq!(doc.name.as_deref(), Some("just setting up my twttr"));
        assert_eq!(doc.description.as_deref(), Some("just setting up my twttr"));
        post_text(&mut doc, " ", Some("jack"));
        assert_eq!(doc.name.as_deref(), Some("Post by jack"));
        assert_eq!(doc.description, None);
        let long = "x".repeat(300);
        post_text(&mut doc, &long, None);
        assert_eq!(
            doc.name.as_deref().map(str::chars).map(Iterator::count),
            Some(121)
        );
        assert!(doc.description.as_deref().unwrap().ends_with('…'));
    }

    #[test]
    fn a_blocked_card_is_the_shape_alone() {
        let doc = ResourceMeta::new("https://www.instagram.com/instagram/", "instagram", 3600);
        let doc = blocked(doc, Shape::Profile, "instagram.com");
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Profile on instagram.com"));
        assert_eq!(doc.description.as_deref(), Some("Blocked by the site."));
        assert_eq!(doc.attributes["blocked"], "true");
        assert_eq!(doc.site, "instagram.com");
        assert_eq!(Shape::Video.type_name(), "VideoObject");
    }

    #[test]
    fn open_graph_tags_as_written() {
        let tags = open_graph(
            r#"<title>T</title><meta property="og:title" content=" A &amp; B "><meta property="og:title" content="second"><meta property="og:description" content=""><meta property="og:image" content="/relative.png"><meta property="og:type" content="profile"><script type="application/ld+json">{"@type":"Article","name":"Other"}</script>"#,
        )
        .unwrap();
        assert_eq!(
            tags,
            OpenGraph {
                title: Some("A & B".into()),
                description: None,
                image: None,
                kind: Some("profile".into()),
            }
        );
    }

    /// A status above 500 that no source names as its platform's refusal is a server error: a
    /// retry, never the blocked card.
    #[tokio::test(start_paused = true)]
    async fn a_999_is_a_server_error() {
        let profile = "https://example.com/someone";
        let mut asked = url::Url::parse("https://example.com/oembed").unwrap();
        asked.query_pairs_mut().append_pair("url", profile);
        let (egress, _) = crate::sources::fixture::egress(vec![
            (profile, 999, "text/html", b"<html></html>".to_vec()),
            (asked.as_str(), 999, "application/json", b"{}".to_vec()),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let class = |error: Option<anyhow::Error>| {
            error
                .as_ref()
                .and_then(|e| e.downcast_ref::<crate::resolver::RetryLater>())
                .map(|retry| retry.class)
        };
        assert_eq!(class(page(&cx, profile).await.err()), Some("upstream"));
        assert_eq!(
            class(
                oembed(&cx, "https://example.com/oembed", profile)
                    .await
                    .err()
            ),
            Some("upstream")
        );
    }

    #[test]
    fn handles_and_ids() {
        assert!(handle("jack", 15) && handle("a.b-c_d", 15) && !handle("", 15));
        assert!(!handle("jack@x", 15) && !handle("abcdefghijklmnop", 15));
        assert!(numeric("20") && !numeric("") && !numeric("2a") && !numeric(&"1".repeat(21)));
    }
}
