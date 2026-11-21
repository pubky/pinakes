//! The `podcasts` also-on source against its recorded answers (`common::media`).
mod common;

use common::media::*;
use pinakes_core::{
    also_on::{
        media::{type_card, TypeHints},
        podcasts::{derived_guid, enclosure_key, item_key, Podcasts},
        Fact, Found, Source, Subject,
    },
    config::Config,
    model::{ResourceMeta, Stream},
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
            ("Overcast", "https://overcast.fm/itunes1200361736", "itunes"),
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
    let enclosure = enclosure_key("https://www.podtrac.com/pts/redirect.mp3/dovetail.prxu.org/7057/141e0c43-f935-41de-be94-708448052c4d/darknet-diaries-ep180-conti.mp3").unwrap();
    assert_eq!(keys(&found), [item.as_str(), enclosure.as_str()]);
    assert_eq!(
        links(&found),
        [
            (
                "Podcast feed",
                "https://podcast.darknetdiaries.com/?guid=prx_7057_141e0c43-f935-41de-be94-708448052c4d",
                "feed"
            ),
            (
                "Podcast Index",
                "https://podcastindex.org/podcast/577105?episode=61326138294",
                "podcastindex"
            ),
        ]
    );
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
    // Without Podcast Index nothing vouches for the show: no key. The page is still the item
    // its feed links back to, so it plays the item's audio, which joins nothing.
    let found = Podcasts
        .lookup(&crate::cx(recorded.clone(), Config::default()), &card)
        .await
        .unwrap();
    assert!(found.keys.is_empty() && found.links.is_empty(), "{found:?}");
    assert_eq!(
        heard(&found),
        [
            ("stream_url", "https://www.podtrac.com/pts/redirect.mp3/dovetail.prxu.org/7057/141e0c43-f935-41de-be94-708448052c4d/darknet-diaries-ep180-conti.mp3"),
            ("stream_type", "audio/mpeg"),
            ("stream_duration", "3557"),
        ]
    );
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
        (
            "https://api.podcastindex.org/api/1.0/episodes/byguid?guid=prx_7057_141e0c43-f935-41de-be94-708448052c4d&feedid=888888",
            "application/json",
            r#"{"status":"true","episode":[],"description":"Episode not found."}"#.into(),
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
    let file = enclosure_key("https://copy.example/a.mp3").unwrap();
    assert_eq!(keys(&found), [theirs.as_str(), file.as_str()]);
    assert_eq!(sites(&found), ["Podcast feed"], "its own RSS item only");
    // Darknet Diaries' own GUID, which Podcast Index says the real feed owns: nothing of the
    // show at all. Its own audio is still its stream: a stream joins nothing.
    let found = Podcasts
        .lookup(&cx(copy_site(GUID_DARKNET, false), indexed()), &copy_page())
        .await
        .unwrap();
    assert!(found.keys.is_empty() && found.links.is_empty(), "{found:?}");
    assert_eq!(
        heard(&found),
        [
            ("stream_url", "https://copy.example/a.mp3"),
            ("stream_type", "audio/mpeg")
        ]
    );
}

#[tokio::test]
async fn a_refused_podcast_index_key_fails_nothing() {
    let _turn = common::REFUSALS.lock().await;
    // Podcast Index refuses the key (401): everything else still counts.
    let recorded = Recorded::with_recorded(&[("podcasts", "pi-byitunesid-401")]);
    let cx = cx(recorded.clone(), indexed());
    let before = common::token_refusals("podcast_index");
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
    // A refused key lasts: the run does not come back early for it, and what the source
    // recorded with Podcast Index's answers before stays beside what the others answered now.
    assert_eq!(found.retry_after, None);
    // Part of the answer is missing: nothing the source recorded before is deleted.
    assert_eq!(found.keep_stored, pinakes_core::also_on::Keep::All);
    // The refusal is counted where an alert reads it; a rate limit (429) is not.
    assert!(common::token_refusals("podcast_index") > before);
    let limited = Recorded::with(&[(
        "https://api.podcastindex.org/api/1.0/podcasts/byitunesid?id=1200361736",
        429,
        "{}",
    )]);
    let before = common::token_refusals("podcast_index");
    let _ = Podcasts
        .lookup(
            &crate::cx(limited.clone(), indexed()),
            &subject(DAILY, None, None),
        )
        .await;
    assert!(limited
        .seen()
        .iter()
        .any(|u| u.starts_with("https://api.podcastindex.org/")));
    assert_eq!(common::token_refusals("podcast_index"), before);
    // When nothing at all could be read (iTunes answering 503), the retry is passed on.
    let show = "https://itunes.apple.com/lookup?id=1200361736&entity=podcast&country=us";
    let busy = Recorded::with(&[(show, 503, "")]);
    let error = Podcasts
        .lookup(&crate::cx(busy, indexed()), &subject(DAILY, None, None))
        .await
        .unwrap_err();
    assert!(error.is::<RetryLater>(), "{error:#}");
}

/// A podcastindex.org card asks iTunes and Wikidata only with Podcast Index's answer: while
/// Podcast Index refuses the key it asks nobody, and the `podcast:apple:` key and the links an
/// earlier run recorded stay, with no failed run and no early one.
#[sqlx::test(migrations = "../../migrations")]
async fn a_refused_podcast_index_key_keeps_an_index_cards_apple_key(pool: sqlx::PgPool) {
    use pinakes_core::{also_on, model::ResourceMeta, store};
    let _turn = common::REFUSALS.lock().await;
    let uri = "https://podcastindex.org/podcast/920666";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "media", 86400);
    doc.set_type("PodcastSeries");
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let config = indexed();
    // What an earlier run, with the key accepted, recorded for the card.
    also_on::record(
        &pool,
        &id,
        "podcasts",
        Found {
            keys: vec![also_on::Key {
                key: "podcast:apple:1584274529".into(),
                via: "itunes".into(),
            }],
            links: vec![also_on::Link::new(
                "https://podcasts.apple.com/podcast/id1584274529",
                "Apple Podcasts",
                "itunes",
            )],
            ..Default::default()
        },
        std::time::Duration::from_secs(86_400),
        &config.also_on,
    )
    .await
    .unwrap();
    let refused = Recorded::with(&[(
        "https://api.podcastindex.org/api/1.0/podcasts/byfeedid?id=920666",
        401,
        "{}",
    )]);
    let sources: Vec<_> = also_on::registry()
        .into_iter()
        .filter(|s| s.id() == "podcasts")
        .collect();
    sqlx::query("INSERT INTO also_on_dirty(resource_id) VALUES($1) ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx(refused, config), &sources, 100)
        .await
        .unwrap();
    let rows: Vec<String> = sqlx::query_scalar("SELECT key FROM resource_key WHERE resource_id=$1 UNION ALL SELECT site FROM derived_link WHERE resource_id=$1 ORDER BY 1")
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows, ["Apple Podcasts", "podcast:apple:1584274529"]);
    let (error, due_in): (Option<String>, f64) = sqlx::query_as("SELECT last_error,extract(epoch FROM next_attempt_at-now())::float8 FROM also_on_dirty WHERE resource_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(error, None);
    assert!(due_in > 3600.0, "{due_in}");
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
    assert!(sites(&found).contains(&"Overcast"));
    assert!(!sites(&found).contains(&"Pocket Casts"), "the card itself");
    // Overcast's link, with or without the show's slug, names its show the same way.
    for overcast in [
        "https://overcast.fm/itunes1200361736/the-daily",
        "https://overcast.fm/itunes1200361736",
    ] {
        let found = Podcasts
            .lookup(&cx, &subject(overcast, None, None))
            .await
            .unwrap();
        assert_eq!(found.keys[0].key, "podcast:apple:1200361736", "{overcast}");
        assert!(sites(&found).contains(&"Pocket Casts"));
        assert!(!sites(&found).contains(&"Overcast"), "the card itself");
    }
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
    // The file it plays (the feed's, Podcast Index's and iTunes' alike) is a key too.
    let enclosure =
        enclosure_key("https://op3.dev/e/mp3s.nashownotes.com/PC20-275-2026-10-16-Final.mp3")
            .unwrap();
    assert_eq!(
        keys(&apple),
        [
            "podcast:apple_episode:1000796718231",
            enclosure.as_str(),
            item.as_str()
        ]
    );
    // Everything keyed by the item, never its own Apple page: the item's own page (its
    // `<link>`), the RSS item and Podcast Index's page of the episode.
    assert_eq!(
        links(&apple),
        [
            (
                "www.podpage.com",
                "https://www.podpage.com/podcasting-20/off-the-rails/",
                "feed"
            ),
            (
                "Podcast feed",
                "https://feeds.podcastindex.org/pc20.xml?guid=PC20-275",
                "feed"
            ),
            (
                "Podcast Index",
                "https://podcastindex.org/podcast/920666?episode=61842097688",
                "podcastindex"
            ),
        ]
    );
    assert!(apple
        .links
        .iter()
        .all(|l| l.key.as_deref() == Some(item.as_str())));
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
        [
            "podcast:apple_episode:1000796718231",
            enclosure.as_str(),
            item.as_str()
        ]
    );
    assert!(
        !sites(&index).contains(&"Podcast Index"),
        "its own Podcast Index page is the card"
    );
    // The Daily's episode: Apple's track and the file it names, no item key (iTunes and Podcast
    // Index name different feeds).
    let daily = Podcasts
        .lookup(
            &cx,
            &subject(&format!("{DAILY}?i=1000797051580"), None, None),
        )
        .await
        .unwrap();
    let daily_file = enclosure_key("https://dts.podtrac.com/redirect.mp3/pdst.fm/e/pfx.vpixl.com/6qj4J/pscrb.fm/rss/p/tracking.swap.fm/track/4jdbJYYRKqK96EGqOzQb/nyt.simplecastaudio.com/03d8b493-87fc-4bd1-931f-8a8e9b945d8a/episodes/5d4af67f-8924-4298-8fd7-19a04e793c81/audio/128/default.mp3?aid=rss_feed&awCollectionId=03d8b493-87fc-4bd1-931f-8a8e9b945d8a&awEpisodeId=5d4af67f-8924-4298-8fd7-19a04e793c81&feed=Sl5CSM3S").unwrap();
    assert_eq!(
        keys(&daily),
        ["podcast:apple_episode:1000797051580", daily_file.as_str()]
    );
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

/// A Castro episode page plays the show's own file and names its Apple ID (`sources::castro`):
/// the episode is the one iTunes lists with that very file, so it gets Apple's track and the
/// file's key. Darknet Diaries' "Conti" has another GUID at iTunes than in the show's feed, so
/// the file is what joins Apple's episode, Castro's and the show's own page (which plays the
/// same file). A file iTunes doesn't list joins nothing.
#[tokio::test]
async fn a_castro_episode_joins_by_the_file_apple_lists() {
    let conti = "https://www.podtrac.com/pts/redirect.mp3/dovetail.prxu.org/7057/141e0c43-f935-41de-be94-708448052c4d/darknet-diaries-ep180-conti.mp3";
    let file = enclosure_key(conti).unwrap();
    let cx = cx(Recorded::new(), indexed());
    let castro = |played: &str| {
        let uri = "https://castro.fm/episode/A1tnxb";
        let mut doc = ResourceMeta::new(uri, "castro", 3600);
        doc.set_type("PodcastEpisode");
        doc.attributes
            .insert("podcast_apple".into(), "1296350485".into());
        doc.stream = Stream::public(played, "audio/mpeg", None, false);
        subject(uri, Some("PodcastEpisode"), Some(doc))
    };
    let found = Podcasts.lookup(&cx, &castro(conti)).await.unwrap();
    // The show's GUID is vouched, so iTunes' GUID for the item is a key under it too; the
    // show's feed has no such item, so there is no RSS item to link.
    let itunes_item = item_key(GUID_DARKNET, "92ba9167-f21b-45a9-af08-ec88fe449873").unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:apple_episode:1000791501104",
            file.as_str(),
            itunes_item.as_str()
        ]
    );
    assert_eq!(
        links(&found),
        [(
            "Apple Podcasts",
            "https://podcasts.apple.com/us/podcast/id1296350485?i=1000791501104",
            "itunes"
        )]
    );
    assert!(!found.reread, "its card plays already");
    // Apple's own episode: the same file, as iTunes lists it.
    let apple = Podcasts
        .lookup(
            &cx,
            &subject(&format!("{DARKNET}?i=1000791501104"), None, None),
        )
        .await
        .unwrap();
    assert_eq!(
        keys(&apple),
        [
            "podcast:apple_episode:1000791501104",
            file.as_str(),
            itunes_item.as_str()
        ]
    );
    // A file iTunes lists for no episode of the show: nothing.
    let found = Podcasts
        .lookup(&cx, &castro("https://cdn.example.org/other.mp3"))
        .await
        .unwrap();
    assert!(found.keys.is_empty() && found.links.is_empty(), "{found:?}");
}

/// The stream facts a lookup recorded.
fn heard(found: &Found) -> Vec<(&str, &str)> {
    found
        .facts
        .iter()
        .filter(|f| f.name.starts_with("stream_"))
        .map(|f| (f.name.as_str(), f.value.as_str()))
        .collect()
}

/// The hints a card would read from these stream facts (`media::type_hints`, without a database).
fn hints_of(found: &Found, episode: bool) -> TypeHints {
    let fact = |name: &str| {
        heard(found)
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v)
    };
    let stream = Stream::public(
        fact("stream_url").unwrap(),
        fact("stream_type").unwrap(),
        fact("stream_duration").map(|d| d.parse().unwrap()),
        false,
    );
    TypeHints {
        stream: stream.map(|s| ("podcasts".to_owned(), s)),
        episode,
        ..Default::default()
    }
}

/// An episode's lookup hears of its enclosure and records it: Apple's episode from iTunes'
/// record of it, a Podcast Index episode from Podcast Index's, a feed card's item (a Fountain
/// episode page) from the feed. A card without a stream of its own is read again to take it;
/// one that plays already is not.
#[tokio::test]
async fn an_episode_hears_of_its_stream() {
    let cx = cx(Recorded::new(), indexed());
    let enclosure = "https://op3.dev/e/mp3s.nashownotes.com/PC20-275-2026-10-16-Final.mp3";
    let pc20_275 = [
        ("stream_url", enclosure),
        ("stream_type", "audio/mpeg"),
        ("stream_duration", "5995"),
    ];
    let apple = format!("{PC20}?i=1000796718231");
    let page = ResourceMeta::new(&apple, "media", 3600);
    let found = Podcasts
        .lookup(&cx, &subject(&apple, Some("PodcastEpisode"), Some(page)))
        .await
        .unwrap();
    assert_eq!(heard(&found), pc20_275);
    assert!(found.reread, "the Apple page has no stream yet");
    let index = "https://podcastindex.org/podcast/920666?episode=61842097688";
    let mut card = ResourceMeta::new(index, "podcast_index", 3600);
    card.stream = Stream::public(enclosure, "audio/mpeg", Some(5995), false);
    let found = Podcasts
        .lookup(&cx, &subject(index, Some("PodcastEpisode"), Some(card)))
        .await
        .unwrap();
    assert_eq!(heard(&found), pc20_275);
    assert!(!found.reread, "its card plays it already");
    let fountain = feed_card(
        "https://fountain.fm/episode/off-the-rails",
        "https://feeds.podcastindex.org/pc20.xml",
        Some("PC20-275"),
    );
    let found = Podcasts.lookup(&cx, &fountain).await.unwrap();
    assert_eq!(heard(&found), pc20_275);
    assert!(found.reread);
    // Without Podcast Index nothing vouches for the show, but the feed Fountain names is still
    // read for the stream its card lacks.
    let unvouched = crate::cx(Recorded::new(), Config::default());
    let found = Podcasts.lookup(&unvouched, &fountain).await.unwrap();
    assert!(found.keys.is_empty(), "{found:?}");
    assert_eq!(heard(&found), pc20_275);
    // Once its card plays that stream, the next lookup still hears of it, so the card keeps it at
    // its next refresh (else the facts go from 3 to 0 and the card loses its audio).
    // Whether or not the source also typed the card an episode (its fingerprint says which).
    for episode in [false, true] {
        let mut card = fountain.doc.clone().unwrap();
        type_card(&fountain.uri, &hints_of(&found, episode), &mut card);
        assert!(card.stream.is_some());
        let again = subject(&fountain.uri, Some("PodcastEpisode"), Some(card.clone()));
        let found = Podcasts.lookup(&unvouched, &again).await.unwrap();
        assert_eq!(heard(&found), pc20_275, "{episode}");
        assert!(!found.reread, "its card plays it already");
        let mut refreshed = fountain.doc.clone().unwrap();
        type_card(&fountain.uri, &hints_of(&found, episode), &mut refreshed);
        assert_eq!(refreshed.stream, card.stream);
        assert_eq!(
            refreshed.hinted_by, card.hinted_by,
            "the stored card is not outdated"
        );
    }
    // A card with a stream of its own is not read for one.
    let mut own = fountain.doc.clone().unwrap();
    own.stream = Stream::public("https://cdn.example.org/own.mp3", "audio/mpeg", None, false);
    let found = Podcasts
        .lookup(
            &unvouched,
            &subject(&fountain.uri, Some("PodcastEpisode"), Some(own)),
        )
        .await
        .unwrap();
    assert!(heard(&found).is_empty(), "{found:?}");
    // The Daily's episode on Apple: the enclosure in the feed iTunes reads (`feed=Sl5CSM3S`).
    let daily = format!("{DAILY}?i=1000797051580");
    let found = Podcasts
        .lookup(&cx, &subject(&daily, Some("PodcastEpisode"), None))
        .await
        .unwrap();
    assert!(heard(&found)[0].1.ends_with("&feed=Sl5CSM3S"), "{found:?}");
    // A show hears of none (`a_feed_names_its_guid_as_a_fact`).
}

/// Podcast Index's record of the episode is its stream when the feed does not answer and
/// iTunes names no file (its `episodes/byguid` answer is read for the stream too, not its ID
/// alone): Apple's PC20-275, with the feed answering 404 and iTunes' list without
/// `episodeUrl`.
#[tokio::test]
async fn podcast_index_s_file_when_the_feed_does_not_answer() {
    let itunes =
        "https://itunes.apple.com/lookup?id=1584274529&entity=podcastEpisode&limit=200&country=us";
    let mut list: serde_json::Value =
        serde_json::from_slice(&read("podcasts", "itunes-episodes-1584274529.json.gz")).unwrap();
    for e in list["results"].as_array_mut().unwrap() {
        e.as_object_mut().unwrap().remove("episodeUrl");
    }
    let recorded = Recorded::with_answers(&[
        (
            "https://feeds.podcastindex.org/pc20.xml",
            404,
            "text/html",
            Vec::new(),
        ),
        (
            itunes,
            200,
            "application/json",
            serde_json::to_vec(&list).unwrap(),
        ),
    ]);
    let cx = cx(recorded.clone(), indexed());
    let apple = format!("{PC20}?i=1000796718231");
    let page = ResourceMeta::new(&apple, "media", 3600);
    let found = Podcasts
        .lookup(&cx, &subject(&apple, Some("PodcastEpisode"), Some(page)))
        .await
        .unwrap();
    let enclosure = "https://op3.dev/e/mp3s.nashownotes.com/PC20-275-2026-10-16-Final.mp3";
    assert_eq!(
        heard(&found),
        [
            ("stream_url", enclosure),
            ("stream_type", "audio/mpeg"),
            ("stream_duration", "5995"),
        ]
    );
    assert!(found.reread);
    // The file Podcast Index records is a key, by its word.
    let file = enclosure_key(enclosure).unwrap();
    let k = found.keys.iter().find(|k| k.key == file).unwrap();
    assert_eq!(k.via, "podcastindex");
    assert!(recorded
        .seen()
        .iter()
        .any(|u| u.starts_with("https://feeds.podcastindex.org/pc20.xml")));
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
        [
            "Apple Podcasts",
            "Pocket Casts",
            "Castro",
            "Overcast",
            "Podcast Index"
        ]
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
    let enclosure =
        enclosure_key("https://op3.dev/e/mp3s.nashownotes.com/PC20-275-2026-10-16-Final.mp3")
            .unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:apple_episode:1000796718231",
            enclosure.as_str(),
            item.as_str()
        ]
    );
    // The page is the item's own `<link>`: not listed again.
    assert_eq!(
        sites(&found),
        ["Apple Podcasts", "Podcast feed", "Podcast Index"]
    );
    // Another page claiming the item gets its Apple track (by the item GUID) but not the key.
    let other = Podcasts
        .lookup(
            &cx,
            &feed_card("https://example.org/copy/", feed, Some("PC20-275")),
        )
        .await
        .unwrap();
    assert_eq!(
        keys(&other),
        ["podcast:apple_episode:1000796718231", enclosure.as_str()],
        "Apple's track, and the file Apple's record names"
    );
}

/// A show Podcast Index vouches for (feed 777777), whose feed gives items one and two the same
/// `<link>` (a listening page), item three a page of its own, and item four the channel's own
/// `<link>`.
const SHARED_FEED: &str = "https://shared.example/feed.xml";
const GUID_SHARED: &str = "22222222-2222-5222-8222-222222222222";
fn shared_link_site() -> Arc<Recorded> {
    let item = |n: u8, link: &str| {
        format!(
            r#"<item><title>Episode {n}</title><link>{link}</link><guid>ep-{n}</guid><enclosure url="https://shared.example/{n}.mp3" type="audio/mpeg" length="1"/></item>"#
        )
    };
    let feed = format!(
        r#"<?xml version="1.0"?><rss version="2.0" xmlns:podcast="https://podcastindex.org/namespace/1.0"><channel><title>Shared</title><link>https://shared.example/show</link><podcast:guid>{GUID_SHARED}</podcast:guid>{}{}{}{}</channel></rss>"#,
        item(1, "https://shared.example/listen"),
        item(2, "https://shared.example/listen"),
        item(3, "https://shared.example/episode/3"),
        item(4, "https://shared.example/show"),
    );
    let index = format!(
        r#"{{"status":"true","feed":{{"id":777777,"podcastGuid":"{GUID_SHARED}","url":"{SHARED_FEED}","originalUrl":"{SHARED_FEED}"}}}}"#
    );
    let episode = |id: u64, n: u8| {
        (
            format!("https://api.podcastindex.org/api/1.0/episodes/byid?id={id}"),
            format!(
                r#"{{"status":"true","episode":{{"id":{id},"guid":"ep-{n}","feedId":777777,"enclosureUrl":"https://shared.example/{n}.mp3","enclosureType":"audio/mpeg"}}}}"#
            ),
        )
    };
    let by_guid = |n: u8| {
        (
            format!(
                "https://api.podcastindex.org/api/1.0/episodes/byguid?guid=ep-{n}&feedid=777777"
            ),
            r#"{"status":"true","episode":[],"description":"Episode not found."}"#.to_owned(),
        )
    };
    let mut bodies: Vec<(String, &str, String)> = vec![
        (SHARED_FEED.into(), "application/rss+xml", feed),
        (
            "https://api.podcastindex.org/api/1.0/podcasts/byfeedid?id=777777".into(),
            "application/json",
            index.clone(),
        ),
        (
            "https://api.podcastindex.org/api/1.0/podcasts/byfeedurl?url=https%3A%2F%2Fshared.example%2Ffeed.xml".into(),
            "application/json",
            index.clone(),
        ),
        (
            format!("https://api.podcastindex.org/api/1.0/podcasts/byguid?guid={GUID_SHARED}"),
            "application/json",
            index,
        ),
    ];
    for (url, body) in [episode(9001, 1), episode(9003, 3), episode(9004, 4)]
        .into_iter()
        .chain([by_guid(1), by_guid(3), by_guid(4)])
    {
        bodies.push((url, "application/json", body));
    }
    let bodies: Vec<(&str, &str, String)> = bodies
        .iter()
        .map(|(url, content_type, body)| (url.as_str(), *content_type, body.clone()))
        .collect();
    Recorded::with_bodies(&bodies)
}

#[tokio::test]
async fn a_page_several_items_link_takes_no_episodes_key() {
    let cx = cx(shared_link_site(), indexed());
    let episode = |id: u64| format!("https://podcastindex.org/podcast/777777?episode={id}");
    let rss = |n: u8| format!("{SHARED_FEED}?guid=ep-{n}");
    // Episode one's `<link>` is item two's as well: the show's page, not the episode's. The
    // episode keeps its item key and names its RSS item, never that page.
    let one = Podcasts
        .lookup(&cx, &subject(&episode(9001), None, None))
        .await
        .unwrap();
    let key = item_key(GUID_SHARED, "ep-1").unwrap();
    assert!(keys(&one).contains(&key.as_str()), "{one:?}");
    assert_eq!(
        links(&one),
        [("Podcast feed", rss(1).as_str(), "feed")],
        "a page two items link is keyed by neither"
    );
    // Episode four's `<link>` is the channel's own: the show's site.
    let four = Podcasts
        .lookup(&cx, &subject(&episode(9004), None, None))
        .await
        .unwrap();
    assert_eq!(
        links(&four),
        [("Podcast feed", rss(4).as_str(), "feed")],
        "the channel's link is no episode's page"
    );
    // Episode three's `<link>` is its own page: keyed by the item, as its RSS item is.
    let three = Podcasts
        .lookup(&cx, &subject(&episode(9003), None, None))
        .await
        .unwrap();
    let key = item_key(GUID_SHARED, "ep-3").unwrap();
    assert_eq!(
        links(&three),
        [
            ("shared.example", "https://shared.example/episode/3", "feed"),
            ("Podcast feed", rss(3).as_str(), "feed"),
        ]
    );
    assert_eq!(
        key_of(&three, "https://shared.example/episode/3"),
        Some(key.as_str())
    );
}

#[tokio::test]
async fn a_card_on_a_page_several_items_link_is_no_episode() {
    let recorded = shared_link_site();
    let cx = cx(recorded.clone(), indexed());
    // A card at the page items one and two both link, read as item one of the feed: the page
    // is not that item's own, so it takes no item key and names nothing keyed by it.
    let shared = Podcasts
        .lookup(
            &cx,
            &feed_card("https://shared.example/listen", SHARED_FEED, Some("ep-1")),
        )
        .await
        .unwrap();
    assert!(
        keys(&shared).is_empty() && shared.links.is_empty(),
        "{shared:?}"
    );
    // The channel's own link, which item four names and whose head names the feed: no item of
    // the feed is found by it, and Podcast Index is not asked.
    let before = recorded.signed_with.lock().unwrap().len();
    let page = "https://shared.example/show";
    let mut doc = ResourceMeta::new(page, "web", 3600);
    doc.attributes.insert("feed".into(), SHARED_FEED.into());
    let site = Podcasts
        .lookup(&cx, &subject(page, Some("WebPage"), Some(doc)))
        .await
        .unwrap();
    assert_eq!(site, Found::default());
    assert_eq!(
        recorded.signed_with.lock().unwrap().len(),
        before,
        "Podcast Index is not asked"
    );
    // Item three's own page is its episode: the item key, and the RSS item keyed by it.
    let own = Podcasts
        .lookup(
            &cx,
            &feed_card(
                "https://shared.example/episode/3",
                SHARED_FEED,
                Some("ep-3"),
            ),
        )
        .await
        .unwrap();
    let key = item_key(GUID_SHARED, "ep-3").unwrap();
    assert!(keys(&own).contains(&key.as_str()), "{own:?}");
    assert_eq!(
        key_of(&own, &format!("{SHARED_FEED}?guid=ep-3")),
        Some(key.as_str())
    );
}

#[tokio::test]
async fn a_feed_episode_on_apple_podcasts() {
    // Podcasting 2.0's feed card for item PC20-275 finds Apple's track by the item GUID: iTunes
    // names this feed for the Apple ID Podcast Index gives.
    let feed = "https://feeds.podcastindex.org/pc20.xml";
    let cx = cx(Recorded::new(), indexed());
    let card = feed_card("https://example.org/somewhere/", feed, Some("PC20-275"));
    let found = Podcasts.lookup(&cx, &card).await.unwrap();
    // And the file Apple's record names, which is a key as its track is.
    let file =
        enclosure_key("https://op3.dev/e/mp3s.nashownotes.com/PC20-275-2026-10-16-Final.mp3")
            .unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:apple_episode:1000796718231", file.as_str()]
    );
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

/// A page that names a show's feed (a player's show page) is keyed with the show's keys only when
/// the feed names it as its page (the channel's `<link>`); otherwise the show's links stay, none
/// of them a key of the page.
#[tokio::test]
async fn a_show_page_takes_the_shows_keys_only_when_its_feed_names_it() {
    let feed = "https://feeds.podcastindex.org/pc20.xml";
    let page_card = |uri: &str| {
        let mut doc = ResourceMeta::new(uri, "fountain", 3600);
        doc.attributes.insert("podcast_feed".into(), feed.into());
        subject(uri, Some("PodcastSeries"), Some(doc))
    };
    let cx = cx(Recorded::new(), indexed());
    // The feed's channel names http://podcastindex.org as its page, not this one.
    let found = Podcasts
        .lookup(&cx, &page_card("https://fountain.fm/show/pc20"))
        .await
        .unwrap();
    assert!(found.keys.is_empty(), "{:?}", keys(&found));
    assert!(!found.links.is_empty());
    assert!(found.links.iter().all(|l| l.key.is_none()));
    // The page the feed names is the show's.
    let found = Podcasts
        .lookup(&cx, &page_card("http://podcastindex.org"))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:apple:1584274529",
            "podcast:guid:917393e3-1b1e-5cef-ace4-edaa54e1f810"
        ]
    );
}
