//! The `podcasts` also-on source against its recorded answers (`common::media`).
mod common;

use common::media::*;
use pinakes_core::{
    also_on::{
        podcasts::{derived_guid, item_key, Podcasts},
        Fact, Found, Source, Subject,
    },
    config::Config,
    model::ResourceMeta,
    resolver::RetryLater,
};
use std::sync::Arc;

/// A config with a Podcast Index key and secret (fake: the answers are recorded).
fn indexed() -> Config {
    let mut config = Config::default();
    config.podcast_index.key = Some("KEY".into());
    config.podcast_index.secret = Some("SECRET".into());
    config
}
fn feed_card(uri: &str, feed: &str, item: Option<&str>) -> Subject {
    let mut doc = ResourceMeta::new(uri, "media", 3600);
    doc.attributes.insert("podcast_feed".into(), feed.into());
    if let Some(item) = item {
        doc.attributes.insert("podcast_guid".into(), item.into());
    }
    let kind = if item.is_some() {
        "PodcastEpisode"
    } else {
        "PodcastSeries"
    };
    subject(uri, Some(kind), Some(doc))
}
const DAILY: &str = "https://podcasts.apple.com/us/podcast/the-daily/id1200361736";
const GUID_DAILY: &str = "304b84f0-07b0-5265-b6b7-da5cf5aeb56e";
const DARKNET: &str = "https://podcasts.apple.com/us/podcast/darknet-diaries/id1296350485";
const GUID_DARKNET: &str = "ac631a54-a629-5367-a1a5-c2e2f7c25054";
const PC20: &str = "https://podcasts.apple.com/us/podcast/podcasting-2-0/id1584274529";
const GUID_PC20: &str = "917393e3-1b1e-5cef-ace4-edaa54e1f810";

#[tokio::test]
async fn an_apple_show_without_podcast_index() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Podcasts
        .lookup(&cx, &subject(DAILY, None, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:apple:1200361736", "wikidata:Q48741148"]
    );
    assert_eq!(
        links(&found),
        [
            ("Pocket Casts", "https://pca.st/itunes/1200361736", "itunes"),
            ("Castro", "https://castro.fm/itunes/1200361736", "itunes"),
            (
                "Spotify",
                "https://open.spotify.com/show/3IM0lmZxpFAY7CwMuv9H4g",
                "wikidata"
            ),
            (
                "Podcast Index",
                "https://podcastindex.org/podcast/743229",
                "wikidata"
            ),
            (
                "Podchaser",
                "https://www.podchaser.com/podcasts/243479",
                "wikidata"
            ),
            ("Castbox", "https://castbox.fm/channel/id463650", "wikidata"),
        ],
        "never the card's own service"
    );
    assert!(
        found
            .links
            .iter()
            .all(|l| l.key.as_deref() == Some("podcast:apple:1200361736")),
        "the players open the Apple ID; the item was found by it"
    );
    assert!(found.facts.is_empty(), "no GUID without Podcast Index");
    assert!(!recorded.seen().iter().any(|u| u.contains("podcastindex")));
}

#[tokio::test]
async fn an_apple_show_with_podcast_index() {
    // Darknet Diaries: iTunes names podcast.darknetdiaries.com as the show's feed, and Podcast
    // Index's feed for the Apple ID is that feed, owning its GUID.
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(&cx, &subject(DARKNET, None, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:apple:1296350485",
            &format!("podcast:guid:{GUID_DARKNET}")
        ]
    );
    assert_eq!(found.keys[1].via, "podcastindex");
    assert!(links(&found).contains(&(
        "Podcast Index",
        "https://podcastindex.org/podcast/577105",
        "podcastindex"
    )));
    let guid = format!("podcast:guid:{GUID_DARKNET}");
    assert_eq!(
        key_of(&found, "https://podcastindex.org/podcast/577105"),
        Some(guid.as_str()),
        "Podcast Index's page is named by the GUID it vouches for"
    );
    assert_eq!(found.facts, [Fact::new("podcast_guid", GUID_DARKNET)]);
    // Asked by Apple ID, then who owns the GUID.
    assert_eq!(
        *recorded.signed_with.lock().unwrap(),
        [Some("KEY".to_string()), Some("KEY".to_string())]
    );
}

#[tokio::test]
async fn podcast_index_counts_for_an_apple_id_only_on_itunes_own_feed() {
    // iTunes names feeds.simplecast.com/Sl5CSM3S as The Daily's feed; Podcast Index's feed for the
    // Apple ID is 54nAGcIl. Apple is the authority for its IDs: no Podcast Index page, no GUID.
    let cx = cx(Recorded::new(), indexed());
    let found = Podcasts
        .lookup(&cx, &subject(DAILY, None, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:apple:1200361736", "wikidata:Q48741148"]
    );
    assert!(!links(&found).iter().any(|l| l.2 == "podcastindex"));
    assert!(found.facts.is_empty());
    // The same the other way: The Daily's feed card gets no Apple ID from Podcast Index.
    let feed = "https://feeds.simplecast.com/54nAGcIl";
    let card = feed_card(feed, feed, None);
    let found = Podcasts
        .lookup(
            &crate::cx(Recorded::with(&[(feed, 404, "")]), indexed()),
            &card,
        )
        .await
        .unwrap();
    assert_eq!(keys(&found), [&format!("podcast:guid:{GUID_DAILY}")]);
    assert!(!keys(&found).iter().any(|k| k.starts_with("podcast:apple")));
}

#[tokio::test]
async fn a_copy_of_a_shows_feed_is_not_vouched_for() {
    // Podcast Index's feed for Darknet Diaries' GUID is another one (a copy it indexed under that
    // GUID owns it here): the show's lookup keeps the GUID as a fact, never as a key.
    let recorded = Recorded::with_recorded(&[("podcasts", "pi-byguid-ac631a54-copy")]);
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(&cx, &subject(DARKNET, None, None))
        .await
        .unwrap();
    assert!(!keys(&found).iter().any(|k| k.starts_with("podcast:guid:")));
    assert_eq!(found.facts, [Fact::new("podcast_guid", GUID_DARKNET)]);
}

#[tokio::test]
async fn an_episode_page_that_names_its_feed() {
    // darknetdiaries.com/episode/180/ names its feed in its head; that URL redirects to the
    // show's own feed, whose item "180: Conti" has this page as its `<link>`.
    let page = "https://darknetdiaries.com/episode/180/";
    let recorded = Recorded::with(&[(
        "https://feeds.megaphone.fm/darknetdiaries",
        301,
        "https://podcast.darknetdiaries.com/",
    )]);
    let cx = cx(recorded.clone(), indexed());
    let mut doc = ResourceMeta::new(page, "web", 3600);
    doc.set_type("Article");
    doc.attributes.insert(
        "feed".into(),
        "https://feeds.megaphone.fm/darknetdiaries".into(),
    );
    let card = subject(page, Some("Article"), Some(doc.clone()));
    assert!(Podcasts.applies(&card));
    let found = Podcasts.lookup(&cx, &card).await.unwrap();
    let item = item_key(
        "ac631a54-a629-5367-a1a5-c2e2f7c25054",
        "prx_7057_141e0c43-f935-41de-be94-708448052c4d",
    )
    .unwrap();
    assert_eq!(keys(&found), [item.as_str()]);
    // Another page of the site names the same feed but is no item of it: nothing, and no
    // Podcast Index request.
    let before = recorded.signed_with.lock().unwrap().len();
    let other = "https://darknetdiaries.com/about/";
    let mut about = doc;
    about.url = other.into();
    let found = Podcasts
        .lookup(&cx, &subject(other, Some("Article"), Some(about)))
        .await
        .unwrap();
    assert_eq!(found, Found::default());
    assert_eq!(recorded.signed_with.lock().unwrap().len(), before);
    // Without Podcast Index nothing vouches for the show: nothing.
    let found = Podcasts
        .lookup(&crate::cx(recorded.clone(), Config::default()), &card)
        .await
        .unwrap();
    assert_eq!(found, Found::default());
}

/// A copy feed on another host that lists `https://copy.example/ep/` as an item carrying a real
/// Darknet Diaries item GUID, claims Darknet Diaries' Apple ID at Podcast Index, and has the GUID
/// `copy_guid` (which Podcast Index says it owns when `owns`).
fn copy_site(copy_guid: &str, owns: bool) -> Arc<Recorded> {
    let feed = format!(
        r#"<?xml version="1.0"?><rss version="2.0" xmlns:podcast="https://podcastindex.org/namespace/1.0"><channel><title>Copy</title><podcast:guid>{copy_guid}</podcast:guid><item><title>Conti</title><link>https://copy.example/ep/</link><guid>prx_7057_141e0c43-f935-41de-be94-708448052c4d</guid><enclosure url="https://copy.example/a.mp3" type="audio/mpeg" length="1"/></item></channel></rss>"#
    );
    let index = format!(
        r#"{{"status":"true","feed":{{"id":888888,"podcastGuid":"{copy_guid}","url":"https://copy.example/feed.xml","originalUrl":"https://copy.example/feed.xml","itunesId":1296350485}}}}"#
    );
    let owner = if owns {
        index.clone()
    } else {
        String::from_utf8(read("podcasts", "pi-byguid-ac631a54.json")).unwrap()
    };
    Recorded::with_bodies(&[
        ("https://copy.example/feed.xml", "application/rss+xml", feed),
        (
            "https://api.podcastindex.org/api/1.0/podcasts/byfeedurl?url=https%3A%2F%2Fcopy.example%2Ffeed.xml",
            "application/json",
            index,
        ),
        (
            &format!("https://api.podcastindex.org/api/1.0/podcasts/byguid?guid={copy_guid}"),
            "application/json",
            owner,
        ),
    ])
}
fn copy_page() -> Subject {
    let page = "https://copy.example/ep/";
    let mut doc = ResourceMeta::new(page, "web", 3600);
    doc.attributes
        .insert("feed".into(), "https://copy.example/feed.xml".into());
    subject(page, Some("Article"), Some(doc))
}

#[tokio::test]
async fn a_copy_that_claims_a_shows_apple_id_joins_nothing_of_it() {
    // Its own GUID, which it owns: its episode key is its own show's, and the Apple ID it claims
    // is not iTunes' feed, so no Apple episode.
    let own = "11111111-1111-5111-8111-111111111111";
    let found = Podcasts
        .lookup(&cx(copy_site(own, true), indexed()), &copy_page())
        .await
        .unwrap();
    let theirs = item_key(own, "prx_7057_141e0c43-f935-41de-be94-708448052c4d").unwrap();
    assert_eq!(keys(&found), [theirs.as_str()]);
    assert!(found.links.is_empty());
    // Darknet Diaries' own GUID, which Podcast Index says the real feed owns: nothing at all.
    let found = Podcasts
        .lookup(&cx(copy_site(GUID_DARKNET, false), indexed()), &copy_page())
        .await
        .unwrap();
    assert_eq!(found, Found::default());
}

#[tokio::test]
async fn a_refused_podcast_index_key_fails_nothing() {
    // Podcast Index refuses the key (401): everything else still counts.
    let recorded = Recorded::with_recorded(&[("podcasts", "pi-byitunesid-401")]);
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(&cx, &subject(DAILY, None, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:apple:1200361736", "wikidata:Q48741148"]
    );
    assert!(sites(&found).contains(&"Pocket Casts"));
    assert!(found.facts.is_empty());
    // When nothing at all could be read (iTunes answering 503), the retry is passed on.
    let show = "https://itunes.apple.com/lookup?id=1200361736&entity=podcast&country=us";
    let busy = Recorded::with(&[(show, 503, "")]);
    let error = Podcasts
        .lookup(&crate::cx(busy, indexed()), &subject(DAILY, None, None))
        .await
        .unwrap_err();
    assert!(error.is::<RetryLater>(), "{error:#}");
}

#[tokio::test]
async fn an_unknown_apple_id_asks_nobody_else() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(
            &cx,
            &subject("https://podcasts.apple.com/us/podcast/id1", None, None),
        )
        .await
        .unwrap();
    assert_eq!(found, Found::default());
    assert_eq!(recorded.seen().len(), 1, "Podcast Index would name a show");
}

#[tokio::test]
async fn players_name_a_show_by_its_apple_id() {
    let cx = cx(Recorded::new(), Config::default());
    let found = Podcasts
        .lookup(
            &cx,
            &subject("https://pca.st/itunes/1200361736", None, None),
        )
        .await
        .unwrap();
    assert_eq!(found.keys[0].key, "podcast:apple:1200361736");
    assert!(sites(&found).contains(&"Castro"));
    assert!(!sites(&found).contains(&"Pocket Casts"), "the card itself");
    // An ID iTunes doesn't know still names itself: the key, from the URL alone.
    let found = Podcasts
        .lookup(&cx, &subject("https://castro.fm/itunes/1", None, None))
        .await
        .unwrap();
    assert_eq!(keys(&found), ["podcast:apple:1"]);
    assert_eq!(found.keys[0].via, "url");
    assert!(found.links.is_empty());
}

#[tokio::test]
async fn a_podcast_index_page() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(
            &cx,
            &subject("https://podcastindex.org/podcast/920666", None, None),
        )
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:apple:1584274529",
            &format!("podcast:guid:{GUID_PC20}")
        ]
    );
    assert!(links(&found).contains(&(
        "Podcast feed",
        "https://feeds.podcastindex.org/pc20.xml",
        "podcastindex"
    )));
    assert!(sites(&found).contains(&"Apple Podcasts"));
    assert!(!sites(&found).contains(&"Podcast Index"), "the card itself");
    assert_eq!(found.facts, [Fact::new("podcast_guid", GUID_PC20)]);
    // Podcast Index's page for The Daily names an Apple ID whose iTunes feed is another one:
    // its GUID only.
    let daily = Podcasts
        .lookup(
            &cx,
            &subject("https://podcastindex.org/podcast/743229", None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&daily), [&format!("podcast:guid:{GUID_DAILY}")]);
    // Without a key a Podcast Index page has nothing to read.
    let found = Podcasts
        .lookup(
            &crate::cx(Recorded::new(), Config::default()),
            &subject("https://podcastindex.org/podcast/920666", None, None),
        )
        .await
        .unwrap();
    assert_eq!(found, Found::default());
}

#[tokio::test]
async fn an_episode_has_one_key_on_apple_and_podcast_index() {
    // Podcasting 2.0's PC20-275: Apple's track 1000796718231 and Podcast Index's episode
    // 61842097688 share the item key under the show's vouched GUID.
    let cx = cx(Recorded::new(), indexed());
    let apple = Podcasts
        .lookup(
            &cx,
            &subject(&format!("{PC20}?i=1000796718231"), None, None),
        )
        .await
        .unwrap();
    let item = item_key(GUID_PC20, "PC20-275").unwrap();
    assert_eq!(
        keys(&apple),
        ["podcast:apple_episode:1000796718231", item.as_str()]
    );
    assert!(apple.links.is_empty(), "its own Apple page is the card");
    let index = Podcasts
        .lookup(
            &cx,
            &subject(
                "https://podcastindex.org/podcast/920666?episode=61842097688",
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        keys(&index),
        ["podcast:apple_episode:1000796718231", item.as_str()]
    );
    // The Daily's episode: Apple's track only (iTunes and Podcast Index name different feeds).
    let daily = Podcasts
        .lookup(
            &cx,
            &subject(&format!("{DAILY}?i=1000797051580"), None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&daily), ["podcast:apple_episode:1000797051580"]);
    assert_eq!(
        index.links[0].key.as_deref(),
        Some("podcast:apple_episode:1000796718231"),
        "the Apple page of Podcast Index's episode is its track's"
    );
    // A gone episode has nothing.
    let gone = Podcasts
        .lookup(
            &crate::cx(Recorded::new(), Config::default()),
            &subject(&format!("{DAILY}?i=1"), None, None),
        )
        .await
        .unwrap();
    assert_eq!(gone, Found::default());
}

#[tokio::test]
async fn a_feed_names_its_guid_as_a_fact() {
    // Without Podcast Index's word, the feed's own GUID is a fact, never a key.
    let cx = cx(Recorded::new(), Config::default());
    let feed = "https://feeds.podcastindex.org/pc20.xml";
    let found = Podcasts
        .lookup(&cx, &feed_card(feed, feed, None))
        .await
        .unwrap();
    assert!(found.keys.is_empty());
    assert_eq!(
        found.facts,
        [Fact::new(
            "podcast_guid",
            "917393e3-1b1e-5cef-ace4-edaa54e1f810"
        )]
    );
    assert!(found.links.is_empty(), "no Apple ID without Podcast Index");
}

#[tokio::test]
async fn a_moved_feed_through_podcast_index() {
    // The show's first URL now redirects: the feed it ends on may be another publisher's, so its
    // GUID is Podcast Index's, which names the first URL as the feed's own.
    let old = "https://mp3s.nashownotes.com/pc20rss.xml";
    let recorded = Recorded::with(&[(old, 301, "https://feeds.podcastindex.org/pc20.xml")]);
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(&cx, &feed_card(old, old, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:apple:1584274529",
            "podcast:guid:917393e3-1b1e-5cef-ace4-edaa54e1f810"
        ]
    );
    assert_eq!(found.keys[1].via, "podcastindex");
    assert_eq!(
        found.facts,
        [Fact::new(
            "podcast_guid",
            "917393e3-1b1e-5cef-ace4-edaa54e1f810"
        )]
    );
    assert_eq!(
        sites(&found),
        ["Apple Podcasts", "Pocket Casts", "Castro", "Podcast Index"]
    );
}

#[tokio::test]
async fn a_feed_item_page_joins_its_episode() {
    // Podcasting 2.0's item PC20-275 names its page as `<link>`; Podcast Index vouches for the
    // feed's GUID, and iTunes lists the item as track 1000796718231.
    let feed = "https://feeds.podcastindex.org/pc20.xml";
    let page = "https://www.podpage.com/podcasting-20/off-the-rails/";
    let cx = cx(Recorded::new(), indexed());
    let found = Podcasts
        .lookup(&cx, &feed_card(page, feed, Some("PC20-275")))
        .await
        .unwrap();
    let item = item_key("917393e3-1b1e-5cef-ace4-edaa54e1f810", "PC20-275").unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:apple_episode:1000796718231", item.as_str()]
    );
    // Another page claiming the item gets its Apple track (by the item GUID) but not the key.
    let other = Podcasts
        .lookup(
            &cx,
            &feed_card("https://example.org/copy/", feed, Some("PC20-275")),
        )
        .await
        .unwrap();
    assert_eq!(keys(&other), ["podcast:apple_episode:1000796718231"]);
}

#[tokio::test]
async fn a_feed_episode_on_apple_podcasts() {
    // Podcasting 2.0's feed card for item PC20-275 finds Apple's track by the item GUID: iTunes
    // names this feed for the Apple ID Podcast Index gives.
    let feed = "https://feeds.podcastindex.org/pc20.xml";
    let cx = cx(Recorded::new(), indexed());
    let card = feed_card("https://example.org/somewhere/", feed, Some("PC20-275"));
    let found = Podcasts.lookup(&cx, &card).await.unwrap();
    assert_eq!(keys(&found), ["podcast:apple_episode:1000796718231"]);
    // The Daily's feed: iTunes names another feed for the Apple ID, so no Apple track.
    let daily = "https://feeds.simplecast.com/54nAGcIl";
    let card = feed_card(
        "https://www.nytimes.com/2026/10/19/podcasts/the-daily/putin-ukraine.html",
        daily,
        Some("1897475d-9b60-447d-ba9b-79aa09f59551"),
    );
    let found = Podcasts
        .lookup(
            &crate::cx(Recorded::with(&[(daily, 404, "")]), indexed()),
            &card,
        )
        .await
        .unwrap();
    assert_eq!(found, Found::default());
}

#[tokio::test]
async fn an_unreadable_feed_has_no_guid_and_fails_nothing() {
    let feed = "https://unreadable.example/feed.xml";
    let found = Podcasts
        .lookup(
            &cx(Recorded::new(), Config::default()),
            &feed_card(feed, feed, None),
        )
        .await
        .unwrap();
    assert!(found.keys.is_empty() && found.links.is_empty());
    assert_eq!(
        found.facts,
        [Fact::new("podcast_guid", derived_guid(feed).unwrap(),)],
        "the derived GUID, a fact only"
    );
}
