//! Shows and episodes on Fountain, from their own pages: a page hook on the web route. Fountain
//! has no public API; its pages name the show's RSS feed and the episode's GUID in the app
//! data they embed. With them the card is the show's or episode's feed card, so the podcasts
//! also-on source finds it on Apple Podcasts, Podcast Index and the rest. The page's own
//! metadata names the card, as for any page.
use super::{kit::plain, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::Fetched};
use anyhow::Result;
use async_trait::async_trait;

/// What a Fountain link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Show(String),
    Episode(String),
}

/// A Fountain ID: letters and digits.
fn id(value: &str) -> Option<String> {
    plain(value, 40, |c| c.is_ascii_alphanumeric()).then(|| value.to_owned())
}

/// The show or episode a Fountain link names: `fountain.fm/show/{id}`, `/episode/{id}`.
pub fn page_of(uri: &str) -> Option<Page> {
    let url = web_url(uri)?;
    if !matches!(url.host_str()?, "fountain.fm" | "www.fountain.fm") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["show", show] => id(show).map(Page::Show),
        ["episode", episode] => id(episode).map(Page::Episode),
        _ => None,
    }
}

/// What a Fountain page's embedded app data says about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Embedded {
    /// The show's RSS feed: the one `rss` URL the data names.
    pub feed: Option<String>,
    /// An episode's item GUID: the `_guid` of the record whose `_id` is the page's own and whose
    /// `_type` is `EPISODE`.
    pub item: Option<String>,
}

/// The `"name":"value"` strings of the page's data, in order, with JSON escapes in script
/// strings (`\"`) undone.
fn values<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let key = format!("\"{name}\":\"");
    text.match_indices(&key)
        .filter_map(|(at, _)| {
            let rest = &text[at + key.len()..];
            let end = rest.find('"')?;
            Some(&rest[..end])
        })
        .collect()
}

/// The data of a Fountain page (its body), for the page `page`.
pub fn embedded(body: &str, page: &Page) -> Embedded {
    let text = body.replace("\\\"", "\"");
    let mut feeds: Vec<&str> = values(&text, "rss")
        .into_iter()
        .filter(|u| web_url(u).is_some())
        .collect();
    feeds.sort_unstable();
    feeds.dedup();
    let feed = match feeds.as_slice() {
        [one] => Some((*one).to_owned()),
        _ => None,
    };
    let item = match page {
        Page::Show(_) => None,
        Page::Episode(own) => {
            let record = format!("\"_id\":\"{own}\",\"_guid\":\"");
            text.match_indices(&record).find_map(|(at, _)| {
                let rest = &text[at + record.len()..];
                let (guid, after) = rest.split_once('"')?;
                (after.starts_with(",\"_type\":\"EPISODE\"")
                    && plain(guid, 200, |c| c.is_ascii_graphic() && c != '\\'))
                .then(|| guid.to_owned())
            })
        }
    };
    Embedded { feed, item }
}

/// The page's card as the show's or episode's feed card: typed by the page, with its feed (and
/// an episode's item GUID); `None` when the data names no feed (or an episode no GUID).
pub fn card(doc: &ResourceMeta, page: &Page, data: Embedded) -> Option<ResourceMeta> {
    let feed = data.feed?;
    let mut doc = doc.clone();
    match page {
        Page::Show(_) => doc.set_type("PodcastSeries"),
        Page::Episode(_) => {
            doc.set_type("PodcastEpisode");
            doc.attributes.insert("podcast_guid".into(), data.item?);
            // Its show's card: the feed, as `POST /v1/resolve` reads it; never a feed whose
            // address carries a credential.
            if !crate::capability::is_capability(&feed) {
                doc.related
                    .push(crate::model::Related::new("show", Some(feed.clone())));
            }
        }
    }
    doc.attributes.insert("podcast_feed".into(), feed);
    doc.source = "fountain".into();
    Some(doc)
}

/// The source: a page hook for Fountain's show and episode pages.
pub struct Fountain;

#[async_trait]
impl Source for Fountain {
    fn name(&self) -> &'static str {
        "fountain"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "fountain",
                uri: "https://fountain.fm/show/V71wG3DhMtF9fDqaJRWX",
                card_type: "PodcastSeries",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "fountain.fm",
                path: "/show/V71wG3DhMtF9fDqaJRWX",
                query: &[],
                header: None,
                content_type: "text/html",
                file: "show-V71wG3DhMtF9fDqaJRWX.html.gz",
            }],
        })
    }
    fn reads(&self, _uri: &str) -> bool {
        false
    }
    fn hooks_pages(&self) -> bool {
        true
    }
    fn facts(&self) -> &'static [&'static str] {
        &["podcast_feed", "podcast_guid"]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://fountain.fm/show/V71wG3DhMtF9fDqaJRWX",
                card_type: "PodcastSeries",
            },
            Example {
                uri: "https://fountain.fm/episode/Yj4aqLaHXcVmxgXbMLHb",
                card_type: "PodcastEpisode",
            },
        ]
    }
    async fn resolve(&self, _cx: &Context<'_>, _uri: &str) -> Result<ResourceMeta> {
        anyhow::bail!("Fountain pages are read by the web route")
    }
    async fn page(
        &self,
        _cx: &Context<'_>,
        uri: &str,
        page: &Fetched,
        doc: &ResourceMeta,
    ) -> Result<Option<ResourceMeta>> {
        let Some(own) = page_of(uri).or_else(|| page_of(page.url.as_str())) else {
            return Ok(None);
        };
        let data = embedded(&String::from_utf8_lossy(&page.body), &own);
        Ok(card(doc, &own, data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    const SHOW: &str = "https://fountain.fm/show/V71wG3DhMtF9fDqaJRWX";
    const EPISODE: &str = "https://fountain.fm/episode/Yj4aqLaHXcVmxgXbMLHb";
    const FEED: &str = "https://feeds.fountain.fm/ZwwaDULvAj0yZvJ5kdB9";

    fn read(file: &str) -> String {
        String::from_utf8(fixture::read("fountain", file)).unwrap()
    }

    #[test]
    fn links() {
        assert_eq!(
            page_of(SHOW),
            Some(Page::Show("V71wG3DhMtF9fDqaJRWX".into()))
        );
        assert_eq!(
            page_of("https://www.fountain.fm/episode/Yj4aqLaHXcVmxgXbMLHb?ref=x"),
            Some(Page::Episode("Yj4aqLaHXcVmxgXbMLHb".into()))
        );
        for uri in [
            "https://fountain.fm/",
            "https://fountain.fm/show/",
            "https://fountain.fm/show/V71wG3DhMtF9fDqaJRWX/episodes",
            "https://fountain.fm/p/npub1abc",
            "https://example.org/show/V71wG3DhMtF9fDqaJRWX",
        ] {
            assert_eq!(page_of(uri), None, "{uri}");
        }
        for example in Fountain.examples() {
            assert!(page_of(example.uri).is_some(), "{}", example.uri);
            assert!(!Fountain.reads(example.uri));
        }
    }

    /// The recorded show page names its feed; the episode page its feed and its own item's
    /// GUID (the show's GUID, also on the page, is not the item's).
    #[test]
    fn recorded_pages() {
        let show = Page::Show("V71wG3DhMtF9fDqaJRWX".into());
        let data = embedded(&read("show-V71wG3DhMtF9fDqaJRWX.html.gz"), &show);
        assert_eq!(
            data,
            Embedded {
                feed: Some(FEED.into()),
                item: None
            }
        );
        let mut web = ResourceMeta::new(SHOW, "web", 3600);
        web.name = Some("TFTC: A Bitcoin Podcast".into());
        let doc = card(&web, &show, data).unwrap();
        assert_eq!(doc.type_name, "PodcastSeries");
        assert_eq!(doc.source, "fountain");
        assert_eq!(doc.attributes["podcast_feed"], FEED);
        assert_eq!(doc.name.as_deref(), Some("TFTC: A Bitcoin Podcast"));

        let episode = Page::Episode("Yj4aqLaHXcVmxgXbMLHb".into());
        let data = embedded(&read("episode-Yj4aqLaHXcVmxgXbMLHb.html.gz"), &episode);
        assert_eq!(
            data,
            Embedded {
                feed: Some(FEED.into()),
                item: Some("f6735b84-3677-417a-a15c-260987367634".into())
            }
        );
        let doc = card(&ResourceMeta::new(EPISODE, "web", 3600), &episode, data).unwrap();
        assert_eq!(doc.type_name, "PodcastEpisode");
        assert_eq!(
            doc.attributes["podcast_guid"],
            "f6735b84-3677-417a-a15c-260987367634"
        );
        for key in doc.attributes.keys().filter(|k| k.starts_with("podcast_")) {
            assert!(Fountain.facts().contains(&key.as_str()), "{key}");
        }
    }

    /// The whole recorded pages through the web route's extraction and this hook: the show is
    /// named by its streamed og: tags, not by Fountain's Organization, and becomes the feed's
    /// show card.
    #[tokio::test]
    async fn whole_pages_through_the_web_route_and_the_hook() {
        for (uri, file, kind, name) in [
            (
                SHOW,
                "show-V71wG3DhMtF9fDqaJRWX.html.gz",
                "PodcastSeries",
                "TFTC: A Bitcoin Podcast",
            ),
            (
                EPISODE,
                "episode-Yj4aqLaHXcVmxgXbMLHb.html.gz",
                "PodcastEpisode",
                "Ten31 Timestamp: Spirit of '63",
            ),
        ] {
            let page = fixture::page(uri, "text/html", fixture::read("fountain", file), None);
            let parsed = page.clone();
            let doc = crate::resolver::html::blocking(move || {
                crate::resolver::web::extract(uri, &parsed, 3600).map(|x| x.doc)
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(doc.name.as_deref(), Some(name), "{uri}");
            let (egress, _) = fixture::egress(vec![]);
            let cx = Context {
                egress: &egress,
                ttl: 3600,
            };
            let doc = Fountain.page(&cx, uri, &page, &doc).await.unwrap().unwrap();
            assert_eq!(doc.type_name, kind, "{uri}");
            assert_eq!(doc.attributes["podcast_feed"], FEED);
        }
    }

    /// No feed, or two different feeds, give no feed card; an episode whose own record names no
    /// GUID gives none either.
    #[test]
    fn unclear_data_gives_nothing() {
        let show = Page::Show("a".into());
        let doc = ResourceMeta::new(SHOW, "web", 60);
        assert!(card(&doc, &show, embedded("<html></html>", &show)).is_none());
        let two = r#"{"rss":"https://a.example/feed"} {"rss":"https://b.example/feed"}"#;
        assert_eq!(embedded(two, &show).feed, None);
        let same = r#"{\"rss\":\"https://a.example/feed\"} {"rss":"https://a.example/feed"}"#;
        assert_eq!(
            embedded(same, &show).feed.as_deref(),
            Some("https://a.example/feed")
        );
        let episode = Page::Episode("own".into());
        let other = r#"{"rss":"https://a.example/feed","episode":{"_id":"other","_guid":"g1","_type":"EPISODE"}}"#;
        assert_eq!(embedded(other, &episode).item, None);
        assert!(card(&doc, &episode, embedded(other, &episode)).is_none());
        let shown =
            r#"{"rss":"https://a.example/feed","show":{"_id":"own","_guid":"g2","_type":"SHOW"}}"#;
        assert_eq!(embedded(shown, &episode).item, None);
    }
}
