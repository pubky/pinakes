//! Podcast Index pages (`podcastindex.org/podcast/{feed}`, with `?episode={id}` for an episode),
//! read from Podcast Index's API: `podcasts/byfeedid` for a show, `episodes/byid` for an
//! episode. The website is never fetched (it answers 403 to some hosts while the API works).
//!
//! The API is signed with the operator's Podcast Index key and secret (`[podcast_index]`,
//! `podcast_index_api`). Without them the source declines and the page gets
//! an ordinary web card. The pair goes only to `api.podcastindex.org` and never appears in logs,
//! errors or cards.
use super::{
    kit::{html_text, int, text, Card},
    Example, Source,
};
use crate::{
    event::adapters::SecretKey,
    model::ResourceMeta,
    podcast_index_api::{
        index_page, pi_episode_request, pi_options, pi_request, pi_stream, Ask, PodcastIndexConfig,
    },
};
use anyhow::{ensure, Result};
use async_trait::async_trait;
use serde_json::Value;

pub const SITE: &str = "Podcast Index";

/// The operator's key and secret (`configure`); none until the daemon sets them.
static CREDENTIALS: std::sync::RwLock<Option<(SecretKey, SecretKey)>> =
    std::sync::RwLock::new(None);

/// Record the operator's Podcast Index pair, when both are set.
fn configure(config: &PodcastIndexConfig) {
    *CREDENTIALS.write().unwrap_or_else(|e| e.into_inner()) = config
        .pair()
        .map(|(key, secret)| (SecretKey::from(key), SecretKey::from(secret)));
}

fn credentials() -> Option<(SecretKey, SecretKey)> {
    CREDENTIALS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// A show's page.
pub fn show_page(feed: u64) -> String {
    format!("https://podcastindex.org/podcast/{feed}")
}
/// An episode's page, under its show's.
pub fn episode_page(feed: u64, episode: u64) -> String {
    format!("https://podcastindex.org/podcast/{feed}?episode={episode}")
}
/// The page a link names, in its one spelling.
fn root(uri: &str) -> Option<String> {
    let (feed, episode) = index_page(uri)?;
    Some(match episode {
        Some(episode) => episode_page(feed, episode),
        None => show_page(feed),
    })
}

/// A Unix time as RFC 3339, when it is one.
fn date(v: &Value) -> Option<String> {
    let seconds = int(v).filter(|s| *s > 0)?;
    chrono::DateTime::from_timestamp(seconds, 0).map(|t| t.to_rfc3339())
}

/// The show `feed`, or `None` when Podcast Index has no feed with that id (it answers an empty
/// list).
pub fn parse_show(status: u16, raw: &[u8], feed: u64) -> Result<Option<Card>> {
    let Some(v) = super::kit::json(status, raw, &[400, 404])? else {
        return Ok(None);
    };
    let f = &v["feed"];
    if !f.is_object() {
        return Ok(None);
    }
    ensure!(
        int(&f["id"]) == i64::try_from(feed).ok(),
        "the API answered for another feed"
    );
    let page = show_page(feed);
    let mut card = Card::new("PodcastSeries", page.clone(), page);
    card.name = text(&f["title"]);
    card.description = text(&f["description"]).and_then(|d| html_text(&d));
    card.image = text(&f["artwork"]).or_else(|| text(&f["image"]));
    card.detail("author", text(&f["author"]));
    card.detail("website", text(&f["link"]));
    card.detail("language", text(&f["language"]));
    card.detail("feed_url", text(&f["url"]));
    Ok(Some(card))
}

/// The episode `episode` of the show `feed`, or `None` when Podcast Index has no such episode,
/// or the episode is another show's.
pub fn parse_episode(status: u16, raw: &[u8], feed: u64, episode: u64) -> Result<Option<Card>> {
    let Some(v) = super::kit::json(status, raw, &[400, 404])? else {
        return Ok(None);
    };
    let e = &v["episode"];
    if !e.is_object() {
        return Ok(None);
    }
    ensure!(
        int(&e["id"]) == i64::try_from(episode).ok(),
        "the API answered for another episode"
    );
    if int(&e["feedId"]) != i64::try_from(feed).ok() {
        return Ok(None);
    }
    let page = episode_page(feed, episode);
    let mut card = Card::new("PodcastEpisode", page.clone(), page);
    card.name = text(&e["title"]);
    card.description = text(&e["description"]).and_then(|d| html_text(&d));
    card.image = text(&e["image"]).or_else(|| text(&e["feedImage"]));
    card.stream = pi_stream(e);
    card.detail("published", date(&e["datePublished"]));
    card.detail("duration", int(&e["duration"]).filter(|d| *d > 0));
    card.detail("website", text(&e["link"]));
    card.detail("show", show_page(feed));
    // Its show's card: the show's page, as `POST /v1/resolve` reads it, by the show's title as
    // the record names it (`feedTitle`), as a feed's item names its show.
    let mut show = crate::model::Related::new("show", Some(show_page(feed)));
    show.name = text(&e["feedTitle"]).map(|mut t| {
        crate::model::truncate(&mut t, 128);
        t
    });
    card.related.push(show);
    Ok(Some(card))
}

/// The source: Podcast Index's API.
pub struct PodcastIndex;

#[async_trait]
impl Source for PodcastIndex {
    fn name(&self) -> &'static str {
        "podcast_index"
    }
    fn configure(&self, config: &crate::config::Config) {
        configure(&config.podcast_index);
    }
    fn reads(&self, uri: &str) -> bool {
        index_page(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        root(uri).filter(|root| root != uri)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "author",
            "website",
            "language",
            "feed_url",
            "published",
            "duration",
            "show",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://podcastindex.org/podcast/48166",
                card_type: "PodcastSeries",
            },
            Example {
                uri: "https://podcastindex.org/podcast/255500?episode=61684996715",
                card_type: "PodcastEpisode",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some((feed, episode)) = index_page(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let Some((key, secret)) = credentials() else {
            return Err(super::Declined.into());
        };
        let Some(url) = (match episode {
            Some(id) => Some(pi_episode_request(id)),
            None => pi_request(Ask::FeedId(feed)),
        }) else {
            return Ok(cx.missing(uri, self));
        };
        let options = pi_options(&key, &secret);
        let f = cx.egress.fetch_with(&url, &options).await?;
        crate::podcast_index_api::note_refusal(f.status);
        super::retry(&f)?;
        let card = crate::resolver::html::blocking(move || match episode {
            Some(id) => parse_episode(f.status, &f.body, feed, id),
            None => parse_show(f.status, &f.body, feed),
        })
        .await??;
        // A show's latest episodes, from the feed Podcast Index names for it (`episodes`).
        let feed_url = card
            .as_ref()
            .filter(|_| episode.is_none())
            .and_then(|c| c.details.get("feed_url"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut doc = match card {
            Some(card) => card.into_doc(cx.card(uri, self), SITE),
            None => cx.missing(uri, self),
        };
        if let Some(feed) = feed_url {
            doc.episodes = crate::episodes::fetch(
                cx.egress,
                &feed,
                cx.ttl,
                crate::episodes::feed_deadline(cx.deadline()),
            )
            .await;
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("podcast_index", name)
    }

    #[test]
    fn a_show_page_and_an_episode_page() {
        assert_eq!(
            root("https://www.podcastindex.org/podcast/577105").as_deref(),
            Some("https://podcastindex.org/podcast/577105")
        );
        assert_eq!(
            root("https://podcastindex.org/podcast/255500?episode=61684996715").as_deref(),
            Some("https://podcastindex.org/podcast/255500?episode=61684996715")
        );
        for uri in [
            "https://podcastindex.org/podcast/x",
            "https://podcastindex.org/",
            "https://podcastindex.org/search?q=x",
        ] {
            assert!(!PodcastIndex.reads(uri), "{uri}");
        }
    }

    #[test]
    fn a_show_s_card() {
        let card = parse_show(200, &read("feed-577105.json"), 577105)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "PodcastSeries");
        assert_eq!(card.name.as_deref(), Some("Darknet Diaries"));
        assert_eq!(card.details["author"], "Jack Rhysider");
        assert_eq!(card.details["website"], "https://darknetdiaries.com/");
        assert_eq!(
            card.details["feed_url"],
            "https://podcast.darknetdiaries.com"
        );
        assert!(card.image.is_some());
        assert!(card
            .description
            .as_deref()
            .is_some_and(|d| d.starts_with("Explore true stories")));
        let acquired = parse_show(200, &read("feed-48166.json"), 48166)
            .unwrap()
            .unwrap();
        assert_eq!(acquired.name.as_deref(), Some("Acquired"));
        // An id that names no feed: an empty list.
        assert!(parse_show(200, &read("feed-missing.json"), 999_999_999)
            .unwrap()
            .is_none());
        // An answer for another feed is an error, never another show's card.
        assert!(parse_show(200, &read("feed-48166.json"), 577105).is_err());
    }

    #[test]
    fn an_episode_s_card() {
        let card = parse_episode(200, &read("episode-61684996715.json"), 255500, 61684996715)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "PodcastEpisode");
        assert_eq!(
            card.name.as_deref(),
            Some("Why AI Needs Bitcoin to Survive | Dhruv Bansal")
        );
        assert_eq!(card.details["duration"], 5213);
        // Its enclosure, as Podcast Index records it: the audio a client plays.
        let stream = card.stream.as_ref().unwrap();
        assert_eq!(stream.url, "https://feeds.fountain.fm/UZSKQcrOnhqYS1JopxGg/items/GArmh0J4JEN85eVHqLQh/files/VIDEO---DEFAULT---84e6ddd8-fe98-4fa9-bb94-8f80543d1848.mp4/ORIGINAL.mp3");
        assert_eq!(stream.media_type, "audio/mpeg");
        assert_eq!(stream.duration_s, Some(5213));
        assert!(!stream.preview);
        assert_eq!(
            card.details["show"],
            "https://podcastindex.org/podcast/255500"
        );
        // Its show, by the title the record names.
        assert_eq!(card.related.len(), 1);
        assert_eq!(card.related[0].rel, "show");
        assert_eq!(
            card.related[0].uri.as_deref(),
            Some("https://podcastindex.org/podcast/255500")
        );
        assert_eq!(card.related[0].name.as_deref(), Some("What Bitcoin Did"));
        assert!(card.details["published"]
            .as_str()
            .is_some_and(|p| p.starts_with("2026-")));
        // The description is plain text.
        assert!(!card.description.as_deref().unwrap_or("").contains("<p>"));
        // The episode is another show's: not this page's.
        assert!(
            parse_episode(200, &read("episode-61684996715.json"), 1, 61684996715)
                .unwrap()
                .is_none()
        );
        assert!(parse_episode(200, &read("episode-missing.json"), 1, 1)
            .unwrap()
            .is_none());
    }

    /// The operator's pair is global (`CREDENTIALS`): the tests that set it hold this while
    /// they run, one at a time, so none sees another's pair.
    static PAIR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Without the operator's pair the source declines (the page gets a web card) and asks
    /// nothing; with it, it asks the API, never the website.
    #[tokio::test(start_paused = true)]
    async fn the_api_is_asked_with_the_pair_and_nothing_without() {
        let _pair = PAIR.lock().await;
        let api = "https://api.podcastindex.org/api/1.0/podcasts/byfeedid?id=577105";
        let (egress, recorded) = fixture::egress(vec![(
            api,
            200,
            "application/json",
            read("feed-577105.json"),
        )]);
        let cx = super::super::Context {
            egress: &egress,
            ttl: 3600,
        };
        let uri = "https://podcastindex.org/podcast/577105";
        *CREDENTIALS.write().unwrap() = None;
        let error = PodcastIndex.resolve(&cx, uri).await.unwrap_err();
        assert!(error.is::<super::super::Declined>(), "{error:#}");
        assert!(recorded.asked().is_empty());
        configure(&PodcastIndexConfig {
            key: Some("TESTKEY".into()),
            secret: Some("TESTSECRET".into()),
        });
        let doc = PodcastIndex.resolve(&cx, uri).await.unwrap();
        assert_eq!(doc.name.as_deref(), Some("Darknet Diaries"));
        assert_eq!(doc.type_name, "PodcastSeries");
        // The show's feed is asked for its latest episodes; unanswered (404 here), the show's
        // stored list stays (`None`), and the card is the show's all the same.
        assert_eq!(
            recorded.asked(),
            [api, "https://podcast.darknetdiaries.com/"]
        );
        assert_eq!(doc.episodes, None);
        assert!(!format!("{doc:?}").contains("TESTSECRET"));
        *CREDENTIALS.write().unwrap() = None;
    }

    /// A show's card carries its feed's latest episodes, read from the feed Podcast Index names
    /// (its first items, as recorded), newest first, with their public streams.
    #[tokio::test(start_paused = true)]
    async fn a_shows_card_lists_its_latest_episodes_from_its_feed() {
        let _pair = PAIR.lock().await;
        let api = "https://api.podcastindex.org/api/1.0/podcasts/byfeedid?id=577105";
        let (egress, recorded) = fixture::egress(vec![
            (api, 200, "application/json", read("feed-577105.json")),
            (
                "https://podcast.darknetdiaries.com/",
                200,
                "application/rss+xml",
                read("feed-darknetdiaries-head.xml"),
            ),
        ]);
        let cx = super::super::Context {
            egress: &egress,
            ttl: 3600,
        };
        configure(&PodcastIndexConfig {
            key: Some("TESTKEY".into()),
            secret: Some("TESTSECRET".into()),
        });
        let doc = PodcastIndex
            .resolve(&cx, "https://podcastindex.org/podcast/577105")
            .await
            .unwrap();
        *CREDENTIALS.write().unwrap() = None;
        assert_eq!(
            recorded.asked(),
            [api, "https://podcast.darknetdiaries.com/"]
        );
        let episodes = doc.episodes.expect("the feed's episodes");
        // Every item of the fixture's head (25, fewer than are kept), each by its feed item.
        assert_eq!(episodes.len(), 25);
        let first = &episodes[0];
        assert!(first.name.is_some(), "{first:?}");
        assert!(
            first
                .uri
                .starts_with("https://podcast.darknetdiaries.com/?guid="),
            "{first:?}"
        );
        assert!(first.published.is_some());
        let stream = first.stream.as_ref().expect("a public enclosure");
        assert!(stream.url.starts_with("https://"), "{stream:?}");
        assert_eq!(stream.media_type, "audio/mpeg");
        assert!(stream.duration_s.is_some_and(|d| d > 600), "{stream:?}");
        assert!(!stream.preview);
        assert!(
            episodes
                .windows(2)
                .all(|w| w[0].published >= w[1].published),
            "newest first"
        );
        // An episode page asked as such reads no feed.
        let (egress, recorded) = fixture::egress(vec![(
            "https://api.podcastindex.org/api/1.0/episodes/byid?id=61684996715",
            200,
            "application/json",
            read("episode-61684996715.json"),
        )]);
        let cx = super::super::Context {
            egress: &egress,
            ttl: 3600,
        };
        configure(&PodcastIndexConfig {
            key: Some("TESTKEY".into()),
            secret: Some("TESTSECRET".into()),
        });
        let doc = PodcastIndex
            .resolve(
                &cx,
                "https://podcastindex.org/podcast/255500?episode=61684996715",
            )
            .await
            .unwrap();
        *CREDENTIALS.write().unwrap() = None;
        assert_eq!(doc.type_name, "PodcastEpisode");
        assert_eq!(doc.episodes, None);
        assert_eq!(recorded.asked().len(), 1);
    }
}
