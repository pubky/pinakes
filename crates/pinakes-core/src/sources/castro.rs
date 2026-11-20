//! Episodes on Castro, from their own pages: a page hook on the web route. Castro has no public
//! API; an episode's page plays the show's own enclosure in its player and names the show's
//! Apple Podcasts ID in its "Listen on" links. With them the card plays that file (its
//! `stream`), and the podcasts also-on source finds the episode on Apple Podcasts by the file
//! iTunes lists for it. The page's own metadata names the card, as for any page.
use super::{
    kit::{decimal, plain},
    web_url, Context, Example, Source,
};
use crate::{
    model::{ResourceMeta, Stream},
    resolver::egress::Fetched,
};
use anyhow::Result;
use async_trait::async_trait;

/// The episode a Castro link names: `castro.fm/episode/{id}`.
pub fn episode_of(uri: &str) -> Option<String> {
    let url = super::kit::web_url(uri)?;
    if !matches!(url.host_str()?, "castro.fm" | "www.castro.fm") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["episode", id] if plain(id, 32, |c| c.is_ascii_alphanumeric()) => Some((*id).to_owned()),
        _ => None,
    }
}

/// What a Castro episode page says about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Played {
    /// The file its player plays (`<audio id="co-supertop-castro-episode">`'s one `<source>`),
    /// with its type.
    pub file: Option<(String, String)>,
    /// The show's Apple Podcasts ID: the one its links to Apple Podcasts and the players that
    /// open a show by it name.
    pub apple: Option<u64>,
}

/// The value of the attribute `name` in the tag `tag` (`<source src="…">`), with `&amp;` undone.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let value = &tag[at..at + tag[at..].find('"')?];
    Some(value.replace("&amp;", "&"))
}

/// The Apple ID a link opens a show by: Apple Podcasts' `…/id{n}`, Pocket Casts' or Castro's
/// `/itunes/{n}`.
fn apple_id(href: &str) -> Option<u64> {
    let url = web_url(href)?;
    let host = url.host_str()?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let id = match (host, segments.as_slice()) {
        ("pca.st" | "castro.fm" | "www.castro.fm", ["itunes", id]) => *id,
        (h, [.., last]) if h == "podcasts.apple.com" || h.ends_with("itunes.apple.com") => {
            last.strip_prefix("id")?
        }
        _ => return None,
    };
    decimal(id, 20)
}

/// What a Castro episode page (its body) plays and which show it names.
pub fn played(body: &str) -> Played {
    let file = body
        .split_once("<audio id=\"co-supertop-castro-episode\"")
        .and_then(|(_, rest)| rest.split_once("</audio>"))
        .and_then(|(player, _)| {
            let mut sources = player.match_indices("<source ").map(|(at, _)| {
                let tag = &player[at..at + player[at..].find('>').unwrap_or(player.len() - at)];
                (attribute(tag, "src"), attribute(tag, "type"))
            });
            match (sources.next(), sources.next()) {
                (Some((Some(src), kind)), None) => {
                    let kind = media_type(&src, kind.as_deref())?;
                    Some((src, kind))
                }
                _ => None,
            }
        });
    let mut apples: Vec<u64> = body
        .match_indices(" href=\"")
        .filter_map(|(at, _)| {
            let rest = &body[at + 7..];
            apple_id(&rest[..rest.find('"')?].replace("&amp;", "&"))
        })
        .collect();
    apples.sort_unstable();
    apples.dedup();
    let apple = match apples.as_slice() {
        [one] => Some(*one),
        _ => None,
    };
    Played { file, apple }
}

/// The file's MIME type: the one its `<source>` names (Castro writes `audio/mp3` for
/// `audio/mpeg`), else the one its name says.
fn media_type(src: &str, named: Option<&str>) -> Option<String> {
    let named = named.map(|t| t.trim().to_ascii_lowercase());
    if let Some(t) = named.filter(|t| !t.is_empty()) {
        return Some(if t == "audio/mp3" {
            "audio/mpeg".into()
        } else {
            t
        });
    }
    let path = url::Url::parse(src).ok()?.path().to_ascii_lowercase();
    [
        (".mp3", "audio/mpeg"),
        (".m4a", "audio/mp4"),
        (".mp4", "video/mp4"),
    ]
    .iter()
    .find(|(end, _)| path.ends_with(end))
    .map(|(_, t)| (*t).to_owned())
}

/// The page's card as the episode's: typed `PodcastEpisode`, playing its file, with the show's
/// Apple ID as `podcast_apple`; `None` when the page plays no public file.
pub fn card(doc: &ResourceMeta, played: Played) -> Option<ResourceMeta> {
    let (src, kind) = played.file?;
    let stream = Stream::public(&src, &kind, None, false)?;
    let mut doc = doc.clone();
    doc.set_type("PodcastEpisode");
    doc.stream = Some(stream);
    if let Some(apple) = played.apple {
        doc.attributes
            .insert("podcast_apple".into(), apple.to_string());
    }
    doc.source = "castro".into();
    Some(doc)
}

/// The source: a page hook for Castro's episode pages.
pub struct Castro;

#[async_trait]
impl Source for Castro {
    fn name(&self) -> &'static str {
        "castro"
    }
    fn reads(&self, _uri: &str) -> bool {
        false
    }
    fn hooks_pages(&self) -> bool {
        true
    }
    fn facts(&self) -> &'static [&'static str] {
        &["podcast_apple"]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://castro.fm/episode/A1tnxb",
            card_type: "PodcastEpisode",
        }]
    }
    async fn resolve(&self, _cx: &Context<'_>, _uri: &str) -> Result<ResourceMeta> {
        anyhow::bail!("Castro pages are read by the web route")
    }
    async fn page(
        &self,
        _cx: &Context<'_>,
        uri: &str,
        page: &Fetched,
        doc: &ResourceMeta,
    ) -> Result<Option<ResourceMeta>> {
        if episode_of(uri)
            .or_else(|| episode_of(page.url.as_str()))
            .is_none()
        {
            return Ok(None);
        }
        Ok(card(doc, played(&String::from_utf8_lossy(&page.body))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    const EPISODE: &str = "https://castro.fm/episode/A1tnxb";
    const CONTI: &str = "https://www.podtrac.com/pts/redirect.mp3/dovetail.prxu.org/7057/141e0c43-f935-41de-be94-708448052c4d/darknet-diaries-ep180-conti.mp3";

    fn read(file: &str) -> String {
        String::from_utf8(fixture::read("castro", file)).unwrap()
    }

    #[test]
    fn links() {
        assert_eq!(episode_of(EPISODE).as_deref(), Some("A1tnxb"));
        assert_eq!(
            episode_of("https://www.castro.fm/episode/A1tnxb?ref=x").as_deref(),
            Some("A1tnxb")
        );
        for uri in [
            "https://castro.fm/",
            "https://castro.fm/episode/",
            "https://castro.fm/itunes/1296350485",
            "https://castro.fm/podcast/a976b506-122b-4964-b38f-e599e1687b4a",
            "https://example.org/episode/A1tnxb",
        ] {
            assert_eq!(episode_of(uri), None, "{uri}");
        }
        for example in Castro.examples() {
            assert!(episode_of(example.uri).is_some(), "{}", example.uri);
            assert!(!Castro.reads(example.uri));
        }
    }

    /// The recorded page plays the show's own enclosure for "Conti" and names Darknet Diaries'
    /// Apple ID; the card plays that file.
    #[test]
    fn the_recorded_page_plays_the_show_s_file() {
        let data = played(&read("episode-A1tnxb.html.gz"));
        assert_eq!(
            data,
            Played {
                file: Some((CONTI.into(), "audio/mpeg".into())),
                apple: Some(1296350485),
            }
        );
        let doc = card(&ResourceMeta::new(EPISODE, "web", 3600), data).unwrap();
        assert_eq!(doc.type_name, "PodcastEpisode");
        assert_eq!(doc.source, "castro");
        assert_eq!(doc.attributes["podcast_apple"], "1296350485");
        assert_eq!(doc.stream, Stream::public(CONTI, "audio/mpeg", None, false));
        for key in doc.attributes.keys().filter(|k| k.starts_with("podcast_")) {
            assert!(Castro.facts().contains(&key.as_str()), "{key}");
        }
    }

    /// The whole recorded page through the web route's extraction and this hook.
    #[tokio::test]
    async fn the_whole_page_through_the_web_route_and_the_hook() {
        let page = fixture::page(
            EPISODE,
            "text/html",
            fixture::read("castro", "episode-A1tnxb.html.gz"),
            None,
        );
        let parsed = page.clone();
        let doc = crate::resolver::html::blocking(move || {
            crate::resolver::web::extract(EPISODE, &parsed, 3600).map(|x| x.doc)
        })
        .await
        .unwrap()
        .unwrap();
        let (egress, _) = fixture::egress(vec![]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Castro
            .page(&cx, EPISODE, &page, &doc)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "PodcastEpisode");
        assert_eq!(doc.stream.map(|s| s.url).as_deref(), Some(CONTI));
        assert!(doc.name.is_some_and(|n| n.contains("Conti")));
    }

    /// No player, two files, a private link or two shows: no episode card, or no show.
    #[test]
    fn unclear_pages_give_nothing() {
        let doc = ResourceMeta::new(EPISODE, "web", 60);
        assert!(card(&doc, played("<html></html>")).is_none());
        let two = r#"<audio id="co-supertop-castro-episode"><source src="https://a.example/1.mp3" type="audio/mp3"><source src="https://a.example/2.mp3" type="audio/mp3"></audio>"#;
        assert_eq!(played(two).file, None);
        let signed = r#"<audio id="co-supertop-castro-episode"><source src="https://a.example/1.mp3?token=x" type="audio/mp3"></audio>"#;
        assert!(card(&doc, played(signed)).is_none());
        let untyped = r#"<audio id="co-supertop-castro-episode"><source src="https://a.example/1.m4a?a=1&amp;b=2"></audio>"#;
        assert_eq!(
            played(untyped).file,
            Some(("https://a.example/1.m4a?a=1&b=2".into(), "audio/mp4".into()))
        );
        let shows = r#"<a href="http://pca.st/itunes/1"></a><a href="https://podcasts.apple.com/us/podcast/x/id2"></a>"#;
        assert_eq!(played(shows).apple, None);
        let one = r#"<a href="http://pca.st/itunes/7"></a><a href="https://geo.itunes.apple.com/ca/podcast/feed/id7?at=1"></a>"#;
        assert_eq!(played(one).apple, Some(7));
    }
}
