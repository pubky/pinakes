use anyhow::{bail, Result};
use async_trait::async_trait;
use pinakes_core::{
    config::Config,
    model::ResourceMeta,
    resolver::{
        cards::{self, CardResolver},
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        web::WebResolver,
        Resolver,
    },
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use url::Url;
mod common;

struct PublicDns;
#[async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}
#[derive(Default)]
struct TransportMock {
    seen: Mutex<Vec<(String, Option<String>)>>,
}
#[async_trait]
impl Transport for TransportMock {
    async fn get(&self, u: &Url, addrs: &[SocketAddr]) -> Result<Fetched> {
        self.get_with(u, addrs, &Default::default()).await
    }
    async fn get_with(&self, u: &Url, _: &[SocketAddr], options: &FetchOptions) -> Result<Fetched> {
        self.seen
            .lock()
            .unwrap()
            .push((u.to_string(), options.accept.clone()));
        let (ct,body)=match u.host_str().unwrap() {
            "openlibrary.org" => ("application/json",json!({"ISBN:9780140328721":{"title":"Matilda","authors":[{"name":"Roald Dahl"}],"cover":{"medium":"https://images.example/book.jpg"}}}).to_string()),
            "doi.org" => {assert_eq!(options.accept.as_deref(),Some("application/vnd.citationstyles.csl+json"));("application/json",json!({"type":"article-journal","title":"A paper","author":[{"given":"A","family":"Writer"}]}).to_string())},
            "www.youtube.com" => ("application/json",json!({"type":"video","title":"Video","thumbnail_url":"https://images.example/video.jpg","html":"<script>never store this</script>"}).to_string()),
            "ap.example" => {
                if u.path()=="/page" {("text/html","<title>Fallback</title><link rel='alternate' type='application/activity+json' href='/actor'>".into())}
                else {assert_eq!(options.accept.as_deref(),Some("application/activity+json"));("application/activity+json",json!({"id":"https://ap.example/actor","type":"Person","preferredUsername":"alice","summary":"<p>About Alice</p>","icon":{"url":"/avatar.png"}}).to_string())}
            },
            "blog.example" => {
                if u.path()=="/post" {("text/html",r#"<title>Blog</title><script type="application/ld+json">{"@type":"BlogPosting","name":"Original blog"}</script><link rel="alternate" type="application/rss+xml" href="/feed">"#.into())}
                else {("application/rss+xml",r#"<rss><channel><title>Blog feed</title><item><guid>blog-guid</guid><title>Post</title><link>https://blog.example/post</link></item></channel></rss>"#.into())}
            },
            // A plain feed: a title with markup-like text, which is plain, and an HTML description.
            "news.example" => ("application/rss+xml",r#"<rss><channel><title><![CDATA[  Using <title>   in RSS ]]></title><description><![CDATA[<p>Daily <b>news</b></p>]]></description><item><title>One</title></item></channel></rss>"#.into()),
            "comments.example" => ("text/html","<title>Episode discussion</title><p>Comment</p><link rel='alternate' type='application/rss+xml' href='https://podcast.example/feed'><link rel='alternate' type='application/activity+json' href='https://ap.example/actor'>".into()),
            "copier.example" => ("text/html","<title>Copied episode</title><link rel='alternate' type='application/rss+xml' href='https://podcast.example/feed'>".into()),
            "podcast.example" => {
                if u.path()=="/episode" {("text/html","<title>Episode page</title><link rel='alternate' type='application/rss+xml' href='/feed'>".into())}
                else {("application/rss+xml",RSS.into())}
            },
            _=>bail!("unexpected request"),
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: u.clone(),
            status: 200,
            body: body.into_bytes(),
            content_type: ct.into(),
            retry_after_secs: 60,
            location: None,
            validators: Default::default(),
        })
    }
}
const RSS: &str = r#"<rss xmlns:podcast="https://podcastindex.org/namespace/1.0"><channel><title>Podcast</title><podcast:guid>channel-id</podcast:guid><item><guid>episode-1</guid><title>Episode One</title><link>https://podcast.example/episode</link><enclosure type="audio/mpeg" url="https://audio.example/one.mp3"/></item><item><guid>episode-2</guid><title>Episode Two</title></item></channel></rss>"#;

/// A feed read through a redirect that lands on a link carrying a credential keeps the link it was
/// asked for as its `podcast_feed`, never where the fetch ended.
#[test]
fn a_podcast_feed_that_lands_on_a_credential_keeps_the_link_asked_for() {
    let f = Fetched {
        permanent_redirect: false,
        truncated: false,
        url: "https://feeds.example/rss?auth=SECRET".parse().unwrap(),
        status: 200,
        body: RSS.as_bytes().to_vec(),
        content_type: "application/rss+xml".into(),
        retry_after_secs: 60,
        location: None,
        validators: Default::default(),
    };
    let doc = cards::podcast("https://podcast.example/feed", &f, 3600, 10)
        .unwrap()
        .unwrap();
    assert_eq!(
        doc.attributes["podcast_feed"],
        "https://podcast.example/feed"
    );
    assert!(!serde_json::to_string(&doc).unwrap().contains("SECRET"));
}

/// An episode card read from a feed that landed on a credential never takes an item's relative
/// link or image: they resolve to that address.
#[test]
fn an_episode_card_never_takes_a_link_that_lands_on_a_credential_feed() {
    let f = Fetched {
        permanent_redirect: false,
        truncated: false,
        url: "https://feeds.example/rss?auth=SECRET".parse().unwrap(),
        body: r#"<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Podcast</title><item><guid>e1</guid><title>Episode One</title><link>#e1</link><itunes:image href=""/><enclosure type="audio/mpeg" url="https://audio.example/one.mp3"/></item></channel></rss>"#.as_bytes().to_vec(),
        content_type: "application/rss+xml".into(),
        status: 200,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    };
    let d = cards::podcast("https://podcast.example/feed?guid=e1", &f, 3600, 10)
        .unwrap()
        .unwrap();
    assert_eq!(d.name.as_deref(), Some("Episode One"));
    assert!(
        !serde_json::to_string(&d).unwrap().contains("SECRET"),
        "{d:?}"
    );
}

#[test]
fn namespace_alone_does_not_make_a_podcast_series() {
    for items in ["", "<item><guid>news</guid><title>News</title></item>"] {
        let f=Fetched { permanent_redirect: false, truncated:false,url:"https://feed.example/rss".parse().unwrap(),status:200,
            body:format!(r#"<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>News feed</title>{items}</channel></rss>"#).into_bytes(),
            content_type:"application/rss+xml".into(),retry_after_secs:60,location:None,validators:Default::default() };
        assert_eq!(
            cards::podcast(f.url.as_str(), &f, 3600, 10)
                .unwrap()
                .unwrap()
                .type_name,
            "DataFeed"
        );
    }
}

#[tokio::test]
async fn optional_fetch_policy_and_metrics() {
    struct Cost {
        seen: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Transport for Cost {
        async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            self.seen.lock().unwrap().push(u.to_string());
            let embed = u.path().contains("oembed");
            if u.path() == "/failed" {
                bail!("upstream failed");
            }
            let body = if embed {
                json!({"type":"video","title":"Embed fallback","thumbnail_url":"https://images.example/embed.jpg"}).to_string()
            } else if u.path() == "/incomplete" {
                "<title>Incomplete</title>".into()
            } else if u.path() == "/no-player" {
                r#"<script type="application/ld+json">{"@type":"Article","name":"Complete","image":"https://images.example/page.jpg"}</script>"#.into()
            } else {
                r#"<head><meta name="twitter:player" content="https://player.vimeo.com/video/1"></head><script type="application/ld+json">{"@type":"Article","name":"Complete","image":"https://images.example/page.jpg"}</script>"#.into()
            };
            Ok(Fetched {
                permanent_redirect: false,
                truncated: u.path() == "/truncated",
                url: u.clone(),
                status: 200,
                body: body.into_bytes(),
                content_type: if embed {
                    "application/json"
                } else {
                    "text/html"
                }
                .into(),
                retry_after_secs: 60,
                location: None,
                validators: Default::default(),
            })
        }
    }
    for (policy, uri, count, first_embed) in [
        (
            cards::OembedPolicy::WhenNeeded,
            "https://vimeo.com/complete",
            1,
            false,
        ),
        (
            cards::OembedPolicy::Always,
            "https://vimeo.com/complete",
            2,
            true,
        ),
        // A complete page that names no player of its own: the provider is asked for its frame.
        (
            cards::OembedPolicy::WhenNeeded,
            "https://vimeo.com/no-player",
            2,
            false,
        ),
        (
            cards::OembedPolicy::WhenNeeded,
            "https://vimeo.com/incomplete",
            2,
            false,
        ),
        (
            cards::OembedPolicy::WhenNeeded,
            "https://vimeo.com/failed",
            2,
            false,
        ),
        (
            cards::OembedPolicy::WhenNeeded,
            "https://vimeo.com/truncated",
            2,
            false,
        ),
        (
            cards::OembedPolicy::WhenNeeded,
            "https://www.youtube.com/watch?v=abc",
            2,
            true,
        ),
        (
            cards::OembedPolicy::WhenNeeded,
            "https://vimeo.com/channels/music/123",
            2,
            true,
        ),
    ] {
        let seen = Arc::new(Cost {
            seen: Mutex::default(),
        });
        let mut cfg = Config::default();
        cfg.media.oembed = policy;
        let r = CardResolver {
            kind: "media",
            config: Arc::new(cfg),
            web: WebResolver {
                oembed_policy: Default::default(),
                cards: None,
                events: None,
                config: Default::default(),
                egress: Egress::with_backends(
                    Default::default(),
                    Arc::new(PublicDns),
                    seen.clone(),
                ),
            },
        };
        let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
        let (doc, _) =
            pinakes_core::telemetry::observe(&metrics, &r.manifest(), r.resolve(uri)).await;
        assert!(doc.unwrap().name.is_some());
        let requests = seen.seen.lock().unwrap();
        assert_eq!(requests.len(), count, "{uri}");
        assert_eq!(requests[0].contains("oembed"), first_embed, "{uri}");
        assert_eq!(
            metrics
                .discovery
                .with_label_values(&["oembed", if count == 1 { "skipped" } else { "requested" }])
                .get(),
            1
        );
        assert!(metrics
            .encode()
            .unwrap()
            .contains("pinakes_discovery_decisions_total"));
    }
    for discover in [false, true] {
        for typed in [false, true] {
            let seen = Arc::new(TransportMock::default());
            let mut cfg = Config::default();
            cfg.cards.media.discover_feeds = discover;
            let r = resolver("media", cfg, seen.clone());
            let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
            let uri = if typed {
                "https://blog.example/post"
            } else {
                "https://podcast.example/episode"
            };
            let (doc, _) =
                pinakes_core::telemetry::observe(&metrics, &r.manifest(), r.web.resolve(uri)).await;
            let doc = doc.unwrap();
            assert_eq!(
                seen.seen.lock().unwrap().len(),
                if discover && !typed { 2 } else { 1 }
            );
            assert_eq!(
                doc.type_name,
                if typed {
                    "BlogPosting"
                } else if discover {
                    "PodcastEpisode"
                } else {
                    "WebPage"
                }
            );
            let decision = if !discover {
                "disabled"
            } else if typed {
                "skipped"
            } else {
                "requested"
            };
            assert_eq!(
                metrics
                    .discovery
                    .with_label_values(&["feed", decision])
                    .get(),
                1
            );
        }
    }
}
/// The media route never sends a link that carries a credential to an oEmbed provider: not
/// first (a provider that names the link exactly, or `oembed = "always"`), not as the fallback for
/// a page without a full card. The page itself is still read from its own site.
#[tokio::test]
async fn the_media_route_never_sends_a_link_that_carries_a_credential_to_oembed() {
    struct Seen(Mutex<Vec<String>>);
    #[async_trait]
    impl Transport for Seen {
        async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            self.0.lock().unwrap().push(u.to_string());
            let embed = u.path().contains("oembed");
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                url: u.clone(),
                status: 200,
                body: if embed {
                    json!({"type":"video","title":"Embed"}).to_string()
                } else {
                    "<title>Page</title>".into()
                }
                .into_bytes(),
                content_type: if embed {
                    "application/json"
                } else {
                    "text/html"
                }
                .into(),
                retry_after_secs: 60,
                location: None,
                validators: Default::default(),
            })
        }
    }
    for (policy, uri) in [
        (
            cards::OembedPolicy::WhenNeeded,
            "https://www.youtube.com/watch?v=abc&token=x",
        ),
        (cards::OembedPolicy::Always, "https://vimeo.com/123?sig=x"),
        (
            cards::OembedPolicy::WhenNeeded,
            "https://vimeo.com/123?key=x",
        ),
    ] {
        let seen = Arc::new(Seen(Mutex::default()));
        let mut cfg = Config::default();
        cfg.media.oembed = policy;
        let r = CardResolver {
            kind: "media",
            config: Arc::new(cfg),
            web: WebResolver {
                oembed_policy: Default::default(),
                cards: None,
                events: None,
                config: Default::default(),
                egress: Egress::with_backends(
                    Default::default(),
                    Arc::new(PublicDns),
                    seen.clone(),
                ),
            },
        };
        assert!(r.resolve(uri).await.is_ok(), "{uri}");
        let requests = seen.0.lock().unwrap();
        assert!(!requests.is_empty(), "{uri}");
        assert!(
            requests.iter().all(|r| !r.contains("oembed")),
            "{uri}: {requests:?}"
        );
    }
}
/// A page that redirects to a link carrying a credential is read there, but the oEmbed endpoint it
/// advertises (which names that link) is never asked, whatever the policy.
#[tokio::test]
async fn a_redirect_to_a_link_that_carries_a_credential_gets_no_oembed() {
    struct Seen(Mutex<Vec<String>>);
    #[async_trait]
    impl Transport for Seen {
        async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            self.0.lock().unwrap().push(u.to_string());
            let landed = u.query().is_some_and(|q| q.contains("token="));
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                url: u.clone(),
                status: if u.path() == "/page" && !landed { 302 } else { 200 },
                body: if u.path().contains("oembed") {
                    json!({"type":"video","title":"Embed"}).to_string()
                } else {
                    "<head><link rel='alternate' type='application/json+oembed' href='/oembed?url=https%3A%2F%2Fmedia.example%2Fpage%3Ftoken%3Dx'></head><title>Page</title>".into()
                }
                .into_bytes(),
                content_type: if u.path().contains("oembed") {
                    "application/json"
                } else {
                    "text/html"
                }
                .into(),
                retry_after_secs: 60,
                location: (u.path() == "/page" && !landed).then(|| "/page?token=x".into()),
                validators: Default::default(),
            })
        }
    }
    let seen = Arc::new(Seen(Mutex::default()));
    let web = WebResolver {
        oembed_policy: cards::OembedPolicy::Always,
        cards: None,
        events: None,
        config: Default::default(),
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            seen.clone(),
        ),
    };
    assert!(web.resolve("https://media.example/page").await.is_ok());
    let requests = seen.0.lock().unwrap();
    assert!(
        requests.iter().any(|r| r.contains("token=x")),
        "{requests:?}"
    );
    assert!(
        requests.iter().all(|r| !r.contains("/oembed")),
        "{requests:?}"
    );
}
fn resolver(kind: &'static str, cfg: Config, transport: Arc<TransportMock>) -> CardResolver {
    CardResolver::new(
        kind,
        Arc::new(cfg),
        Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            transport,
        ),
    )
}
#[tokio::test]
async fn registry_identifiers_negotiation_discovery_and_switches() {
    let seen = Arc::new(TransportMock::default());
    for (kind, uri, ty) in [
        ("isbn", "isbn:9780140328721", "Book"),
        ("doi", "doi:10.1234/Example", "ScholarlyArticle"),
        (
            "media",
            "https://www.youtube.com/watch?v=abc",
            "VideoObject",
        ),
        ("media", "https://ap.example/page", "Person"),
        ("media", "https://podcast.example/episode", "PodcastEpisode"),
    ] {
        let mut cfg = Config::default();
        cfg.cards.media.discover_feeds = true;
        let d = resolver(kind, cfg, seen.clone())
            .resolve(uri)
            .await
            .unwrap();
        assert_eq!(d.type_name, ty, "{uri}");
        assert!(d.name.is_some());
        assert!(!serde_json::to_string(&d).unwrap().contains("<script>"));
    }
    let mut cfg = Config::default();
    cfg.cards.activitypub.enabled = false;
    cfg.cards.media.enabled = false;
    let r = resolver("media", cfg.clone(), seen.clone());
    let before = seen.seen.lock().unwrap().len();
    assert_eq!(
        r.web
            .resolve("https://ap.example/page")
            .await
            .unwrap()
            .type_name,
        "WebPage"
    );
    assert_eq!(seen.seen.lock().unwrap().len(), before + 1);
    assert!(r.web.queue_sources().contains(&"activitypub"));
    cfg.cards.isbn.enabled = false;
    assert_eq!(
        pinakes_core::resolver::classify_config("isbn:9780140328721", &cfg),
        None
    );
    assert_eq!(
        pinakes_core::resolver::classify_config("https://youtube.com/watch?v=abc", &cfg),
        Some("web")
    );
    assert!(cards::isbn("isbn:0-14-032872-6").is_some());
    assert!(cards::isbn("isbn:9780140328722").is_none());
    assert_eq!(
        cards::doi("https://doi.org/10.1234/AbC").as_deref(),
        Some("10.1234/abc")
    );
    assert!(cards::doi("doi:10.1/no").is_none());
}
/// A feed item's card carries its enclosure as its `stream`, with its `itunes:duration`. The
/// show's card has none.
#[test]
fn a_feed_item_carries_its_enclosure() {
    let body = r#"<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Show</title><itunes:author>Host</itunes:author><item><title>Latest</title><guid isPermaLink="false">ep-2</guid><enclosure url="https://cdn.example/pts/redirect.mp3/media.example/ep-2.mp3" length="57000000" type="audio/mpeg"/><itunes:duration>59:17</itunes:duration></item><item><title>First</title><guid isPermaLink="false">ep-1</guid><enclosure url="https://cdn.example/ep-1.mp3" length="1000" type="audio/mpeg"/><itunes:duration>61</itunes:duration></item></channel></rss>"#;
    let f = Fetched {
        permanent_redirect: false,
        truncated: false,
        url: "https://show.example/".parse().unwrap(),
        body: body.as_bytes().to_vec(),
        content_type: "application/rss+xml".into(),
        status: 200,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    };
    let d = cards::podcast("https://show.example/?guid=ep-2", &f, 3600, 1000)
        .unwrap()
        .unwrap();
    assert_eq!(d.type_name, "PodcastEpisode");
    assert_eq!(
        d.stream,
        Some(pinakes_core::model::Stream {
            url: "https://cdn.example/pts/redirect.mp3/media.example/ep-2.mp3".into(),
            media_type: "audio/mpeg".into(),
            duration_s: Some(59 * 60 + 17),
            preview: false,
        })
    );
    let show = cards::podcast("https://show.example/", &f, 3600, 1000)
        .unwrap()
        .unwrap();
    assert_eq!(show.type_name, "PodcastSeries");
    assert_eq!(show.stream, None);
    for (v, secs) in [
        ("3557", Some(3557)),
        ("3557.6", Some(3557)),
        (" 59:17 ", Some(3557)),
        ("1:02:03", Some(3723)),
        ("1:75", None),
        ("1:2:3:4", None),
        ("", None),
        ("-5", None),
        ("1e3", None),
    ] {
        assert_eq!(cards::feed_duration(v), secs, "{v}");
    }
}
#[test]
fn podcast_selection_and_schema_media_keys() {
    let f = Fetched {
        permanent_redirect: false,
        truncated: false,
        url: "https://podcast.example/feed".parse().unwrap(),
        body: RSS.as_bytes().to_vec(),
        content_type: "application/rss+xml".into(),
        status: 200,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    };
    let d = cards::podcast("https://podcast.example/feed?guid=episode-2", &f, 3600, 10)
        .unwrap()
        .unwrap();
    assert_eq!(d.name.as_deref(), Some("Episode Two"));
    assert_eq!(d.attributes["podcast_guid"], "episode-2");
    assert_eq!(
        cards::podcast("https://podcast.example/feed?guid=absent", &f, 3600, 10)
            .unwrap()
            .unwrap()
            .status,
        pinakes_core::model::Status::Missing
    );
    let series = cards::podcast("https://podcast.example/feed", &f, 3600, 10)
        .unwrap()
        .unwrap();
    assert_eq!(series.type_name, "PodcastSeries");
    assert!(cards::podcast("https://podcast.example/feed", &f, 3600, 1).is_err());
    for ty in [
        "VideoObject",
        "AudioObject",
        "MusicRecording",
        "PodcastEpisode",
        "ImageObject",
    ] {
        let mut f = f.clone();
        f.content_type = "text/html".into();
        f.body=format!("<script type='application/ld+json'>{{\"@type\":\"{ty}\",\"name\":\"Media\",\"isrcCode\":\"US-S1Z-99-00001\"}}</script>").into_bytes();
        let d = pinakes_core::resolver::web::extract("https://example.org/a", &f, 3600)
            .unwrap()
            .doc;
        assert_eq!(d.type_name, ty);
        assert_eq!(d.attributes["isrc"], "US-S1Z-99-00001");
    }
}
/// A feed cut off at the page cap (a long show's feed is 2–5 MB) still gets its card: the read
/// stops after the last whole item, and the elements still open are closed.
#[test]
fn a_feed_cut_off_at_the_cap_reads_to_its_last_whole_item() {
    let head = r#"<?xml version="1.0"?><rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Long Show</title>"#;
    let item = |n: usize| {
        format!("<item><title>Episode {n}</title><guid>e{n}</guid><description><![CDATA[<p>Notes {n}</p>]]></description><enclosure url=\"https://cdn.example/{n}.mp3\" type=\"audio/mpeg\"/></item>")
    };
    let whole: String = (1..=3).map(item).collect();
    // Cut inside the fourth item's CDATA section.
    let cut = format!("{head}{whole}{}", &item(4)[..60]);
    let completed = cards::feed_prefix(&cut, 1000).unwrap();
    assert_eq!(completed, format!("{head}{whole}</channel></rss>"));
    assert_eq!(
        cards::feed_prefix(&cut, 2).unwrap(),
        format!("{head}{}{}</channel></rss>", item(1), item(2))
    );
    assert!(cards::feed_prefix(&format!("{head}{}", &item(1)[..30]), 1000).is_none());
    // Kimi's shape on ee9a221: hundreds of thousands of open elements, then `</item>` after
    // `</item>` (1 MiB). Each `</item>` copied the whole open stack, minutes of work at the item
    // cap. With end tags that close what is open (300,000 `<item>`), the reader takes them all;
    // with `<a>` it stops at the first mismatch. The prefix ends at once with the last item whole
    // before the depth no feed reaches.
    for open in ["<item>", "<a>"] {
        let hostile = format!(
            "{head}{}{}{}",
            item(1),
            open.repeat(300_000),
            "</item>".repeat(1_000)
        );
        assert!(hostile.len() > 900_000);
        let started = common::thread_cpu_time();
        let completed = cards::feed_prefix(&hostile, 10_000).unwrap();
        // Of this thread's CPU time: a bound load never inflates.
        let used = common::thread_cpu_time() - started;
        assert!(used < std::time::Duration::from_secs(5), "{open}: {used:?}");
        assert_eq!(
            completed,
            format!("{head}{}</channel></rss>", item(1)),
            "{open}"
        );
    }
    // So does an element name longer than any feed's.
    let long = format!("{head}{}<{}>x</item>", item(1), "n".repeat(10_000));
    assert_eq!(
        cards::feed_prefix(&long, 1000).unwrap(),
        format!("{head}{}</channel></rss>", item(1))
    );
    // Atom entries too.
    let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>A</title><entry><id>1</id></entry><entry><id>2"#;
    assert_eq!(
        cards::feed_prefix(atom, 1000).unwrap(),
        r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>A</title><entry><id>1</id></entry></feed>"#
    );
    let f = Fetched {
        permanent_redirect: false,
        truncated: true,
        url: "https://podcast.example/feed".parse().unwrap(),
        body: cut.into_bytes(),
        content_type: "application/rss+xml".into(),
        status: 200,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    };
    let series = cards::podcast("https://podcast.example/feed", &f, 3600, 1000)
        .unwrap()
        .unwrap();
    assert_eq!(
        (series.type_name.as_str(), series.name.as_deref()),
        ("PodcastSeries", Some("Long Show"))
    );
    let episode = cards::podcast("https://podcast.example/feed?guid=e2", &f, 3600, 1000)
        .unwrap()
        .unwrap();
    assert_eq!(episode.name.as_deref(), Some("Episode 2"));
    // An episode past the part that was read is not missing.
    assert!(
        cards::podcast("https://podcast.example/feed?guid=e900", &f, 3600, 1000)
            .unwrap()
            .is_none()
    );
    // The cut can fall inside a character (a Japanese title's three bytes).
    let mut body = format!("{head}{whole}<item><title>第四話").into_bytes();
    body.pop();
    let f = Fetched { body, ..f };
    let series = cards::podcast("https://podcast.example/feed", &f, 3600, 1000)
        .unwrap()
        .unwrap();
    assert_eq!(series.name.as_deref(), Some("Long Show"));
    // Invalid UTF-8 before the cut is still an error.
    let f = Fetched {
        body: [&b"<rss>\xff"[..], b"</rss>"].concat(),
        ..f
    };
    assert!(cards::podcast("https://podcast.example/feed", &f, 3600, 1000).is_err());
}
#[test]
fn generic_nostr_signed_profile_note_and_nip52_routing() {
    use nostr::prelude::*;
    let keys = Keys::parse(&"11".repeat(32)).unwrap();
    for (kind, content, ty) in [
        (
            0,
            r#"{"name":"Alice","picture":"https://example.org/a.png"}"#,
            "Person",
        ),
        (1, "Hello nostr", "SocialMediaPosting"),
        (6, "Reposted", "CreativeWork"),
        (7, "+", "CreativeWork"),
    ] {
        let unsigned = EventBuilder::new(Kind::from(kind), content)
            .custom_created_at(Timestamp::from_secs(1700000000))
            .finalize_unsigned(keys.public_key());
        let sig = keys.sign_schnorr_with_aux_rand(
            &secp256k1::Secp256k1::new(),
            unsigned.compute_id().as_bytes(),
            &[0; 32],
        );
        let event = unsigned.add_signature(sig).unwrap();
        let uri = if kind == 0 {
            format!("nostr:{}", keys.public_key().to_bech32().unwrap())
        } else {
            format!("nostr:{}", event.id.to_bech32().unwrap())
        };
        assert_eq!(cards::classify(&uri), Some("nostr"));
        assert_eq!(
            cards::parse_nostr(&uri, &event, 3600).unwrap().type_name,
            ty
        );
        let mut v = serde_json::to_value(event).unwrap();
        v["content"] = Value::String("tampered".into());
        assert!(cards::parse_nostr(&uri, &serde_json::from_value(v).unwrap(), 3600).is_err());
    }
    let t = pinakes_core::event::adapters::nostr::Target::coordinate(&format!(
        "31923:{}:event",
        keys.public_key().to_hex()
    ))
    .unwrap();
    assert_eq!(
        pinakes_core::resolver::classify(&t.uri().unwrap()),
        Some("event_platform")
    );
    let _: ResourceMeta = ResourceMeta::new("nostr:x", "nostr", 3600);
}

/// A feed that is no podcast keeps the card the page read made of it (`web::extract`) on the media
/// route too: its title as plain text, its description without markup, its content type.
#[tokio::test]
async fn a_plain_feed_keeps_its_card_on_the_media_route() {
    let r = resolver(
        "media",
        Config::default(),
        Arc::new(TransportMock::default()),
    );
    let d = r.web.resolve("https://news.example/rss").await.unwrap();
    assert_eq!(
        (
            d.type_name.as_str(),
            d.name.as_deref(),
            d.description.as_deref()
        ),
        ("DataFeed", Some("Using <title> in RSS"), Some("Daily news"))
    );
    assert_eq!(
        d.attributes.get("content_type").map(String::as_str),
        Some("application/rss+xml")
    );
}

#[tokio::test]
async fn blog_rss_preserves_jsonld_and_single_episode_feed_is_series() {
    let r = resolver(
        "media",
        Config::default(),
        Arc::new(TransportMock::default()),
    );
    let d = r.web.resolve("https://blog.example/post").await.unwrap();
    assert_eq!(d.type_name, "BlogPosting");
    assert_eq!(d.name.as_deref(), Some("Original blog"));
    assert!(!d.group_keys.iter().any(|k| k.basis == "podcast_guid"));
    assert_eq!(
        r.web
            .resolve("https://blog.example/feed")
            .await
            .unwrap()
            .type_name,
        "DataFeed"
    );
    for enclosure in ["audio/mpeg", "video/mp4"] {
        let f = Fetched {permanent_redirect: false,truncated: false,url:"https://podcast.example/feed".parse().unwrap(),status:200,body:format!("<rss><channel><title>Podcast</title><item><guid>one</guid><title>Episode</title><link>https://podcast.example/one</link><enclosure type='{enclosure}'/></item></channel></rss>").into_bytes(),content_type:"application/rss+xml".into(),location:None,retry_after_secs:60,validators:Default::default()};
        assert_eq!(
            cards::podcast(f.url.as_str(), &f, 3600, 10)
                .unwrap()
                .unwrap()
                .type_name,
            "PodcastSeries"
        );
        assert_eq!(
            cards::podcast("https://podcast.example/one", &f, 3600, 10)
                .unwrap()
                .unwrap()
                .type_name,
            "PodcastEpisode"
        );
    }
}

#[tokio::test]
async fn recorded_youtube_head_and_oembed_survive_oversize_or_failed_page() {
    use axum::{routing::get, Router};
    use std::{
        io::Read,
        sync::atomic::{AtomicBool, Ordering},
    };
    let mut body = Vec::new();
    flate2::read::GzDecoder::new(include_bytes!("fixtures/media/youtube-watch.html.gz").as_slice())
        .read_to_end(&mut body)
        .unwrap();
    assert!(body.len() > pinakes_core::resolver::egress::BODY_CAP);
    let fail = Arc::new(AtomicBool::new(false));
    let flag = fail.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/watch",
            get(move || {
                let body = body.clone();
                let fail = flag.clone();
                async move {
                    (
                        if fail.load(Ordering::SeqCst) {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            axum::http::StatusCode::OK
                        },
                        [("content-type", "text/html")],
                        body,
                    )
                }
            }),
        )
        .route(
            "/oembed",
            get(|| async {
                (
                    [("content-type", "application/json")],
                    include_str!("fixtures/media/youtube-oembed.json"),
                )
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    struct Replay {
        base: String,
        seen: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Transport for Replay {
        async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            self.seen.lock().unwrap().push(u.path().into());
            let path = if u.path().contains("oembed") {
                "/oembed"
            } else {
                "/watch"
            };
            pinakes_core::resolver::egress::read_response(
                u.clone(),
                reqwest::get(format!("{}{path}", self.base)).await?,
            )
            .await
        }
    }
    let transport = Arc::new(Replay {
        base,
        seen: Default::default(),
    });
    let make = || CardResolver {
        kind: "media",
        config: Arc::new(Config::default()),
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            events: None,
            egress: Egress::with_backends(
                Default::default(),
                Arc::new(PublicDns),
                transport.clone(),
            ),
            config: Default::default(),
        },
    };
    let uri = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
    let (doc, observations) = pinakes_core::telemetry::observe(
        &pinakes_core::telemetry::Metrics::new().unwrap(),
        &pinakes_core::adapter::manifest("web"),
        make().web.resolve(uri),
    )
    .await;
    let doc = doc.unwrap();
    assert_eq!(doc.status, pinakes_core::model::Status::Ok);
    assert!(doc.name.as_ref().unwrap().contains("Rick Astley"));
    assert!(doc.event.warnings.iter().any(|w| w == "truncated"));
    assert!(observations.iter().any(|o| o.outcome == "drift"));
    for failed in [false, true] {
        fail.store(failed, Ordering::SeqCst);
        transport.seen.lock().unwrap().clear();
        let doc = make().resolve(uri).await.unwrap();
        assert_eq!(doc.type_name, "VideoObject");
        assert_eq!(doc.source, "media");
        assert_eq!(doc.status, pinakes_core::model::Status::Ok);
        assert_eq!(transport.seen.lock().unwrap()[0], "/oembed");
    }
    server.abort();
}
#[test]
fn identifier_dates_domains_media_attributes_and_social_excerpt() {
    assert_eq!(cards::classify("https://doi.org/"), None);
    assert_eq!(
        cards::classify("https://doi.org/the-identifier-resolver.html"),
        None
    );
    assert_eq!(
        cards::classify("https://dx.doi.org/10.1234/work"),
        Some("doi")
    );
    for uri in [
        "https://evil.com/?x=.spotify.com/track/abc",
        "https://evil.com/path/youtube.com/watch?v=x",
    ] {
        assert!(cards::provider_endpoint(uri).is_none());
    }
    assert!(
        cards::provider_endpoint("https://open.spotify.com/track/abc")
            .unwrap()
            .scheme()
            == "https"
    );
    // The keyless providers that answer (Twitter's and MixCloud's endpoints redirect to their
    // current ones).
    for (uri, endpoint) in [
        ("https://x.com/jack/status/20", "publish.x.com"),
        ("https://twitter.com/jack/status/20", "publish.twitter.com"),
        ("https://vimeo.com/22439234", "vimeo.com"),
        (
            "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC",
            "open.spotify.com",
        ),
        (
            "https://www.flickr.com/photos/bees/2341623661/",
            "www.flickr.com",
        ),
        (
            "https://www.mixcloud.com/spartacus/party-time/",
            "www.mixcloud.com",
        ),
        (
            "https://www.dailymotion.com/video/x7tgad0",
            "www.dailymotion.com",
        ),
        (
            "https://www.ted.com/talks/ken_robinson_do_schools_kill_creativity",
            "www.ted.com",
        ),
        (
            "https://www.reddit.com/r/Bitcoin/comments/7v8yqb/the_bitcoin_whitepaper/",
            "www.reddit.com",
        ),
    ] {
        let found = cards::provider_endpoint(uri).unwrap_or_else(|| panic!("{uri}"));
        assert_eq!(found.host_str(), Some(endpoint), "{uri}");
    }
    // Skipped: a bot challenge, empty answers, or an app token.
    for uri in [
        "https://soundcloud.com/forss/flickermood",
        "https://www.tiktok.com/@someone/video/123",
        "https://www.facebook.com/someone/posts/123",
        "https://www.instagram.com/p/abc/",
    ] {
        assert!(cards::provider_endpoint(uri).is_none(), "{uri}");
    }
    let f=Fetched{permanent_redirect: false,truncated:false,url:"https://doi.org/10.1234/work".parse().unwrap(),body:serde_json::to_vec(&json!({"type":"article-journal","title":"Dated","issued":{"date-parts":[[2024,5,12]]},"container-title":"Journal"})).unwrap(),content_type:"application/json".into(),status:200,location:None,retry_after_secs:60,validators:Default::default()};
    let d = cards::parse_doi("doi:10.1234/work", &f, 3600).unwrap();
    assert_eq!(d.attributes["published"], "2024-05-12");
    assert_eq!(d.attributes["container"], "Journal");
    let base = "https://social.example/@alice/1".parse().unwrap();
    assert!(cards::activitypub(
        "https://attacker.example/page",
        &json!({"type":"Person","id":"https://social.example/@alice"}),
        &base,
        3600
    )
    .is_none());
    let body = "🦀".repeat(1000);
    let d=cards::activitypub(base.as_str(),&json!({"type":"Note","id":base.as_str(),"content":body,"to":["https://www.w3.org/ns/activitystreams#Public"]}),&base,86400).unwrap();
    assert_eq!(d.description.unwrap().chars().count(), 200);
    assert_eq!(d.ttl_secs, 3600);
    let d=cards::activitypub(base.as_str(),&json!({"type":"Note","id":base.as_str(),"content":"unlisted text","to":["https://social.example/followers"],"cc":["https://www.w3.org/ns/activitystreams#Public"]}),&base,86400).unwrap();
    assert!(!serde_json::to_string(&d).unwrap().contains("unlisted text"));
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn bad_identifiers_are_final_missing_and_ap_promotes_to_worker(pool: sqlx::PgPool) {
    use pinakes_core::{queue, store};
    for uri in ["isbn:9780140328722", "doi:invalid"] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let r = resolver(
            if uri.starts_with("isbn:") {
                "isbn"
            } else {
                "doi"
            },
            Config::default(),
            Arc::new(TransportMock::default()),
        );
        let job = queue::Job {
            id: id.clone(),
            uri: uri.into(),
            attempts: 0,
            generation: 0,
        };
        queue::run_job(&pool, &r, &Default::default(), &job)
            .await
            .unwrap();
        let state: (String, bool, i32, bool) =
            sqlx::query_as("SELECT status,final,attempts,dead_letter FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, ("missing".into(), true, 0, false));
    }
    let uri = "https://ap.example/page";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let r = resolver(
        "activitypub",
        Config::default(),
        Arc::new(TransportMock::default()),
    );
    let d = r.web.resolve(uri).await.unwrap();
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT resolver FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "activitypub"
    );
}

#[tokio::test]
async fn oembed_first_keeps_rich_page_types_and_ap_denial_falls_back() {
    struct Rich;
    #[async_trait]
    impl Transport for Rich {
        async fn get(&self, u: &Url, a: &[SocketAddr]) -> Result<Fetched> {
            self.get_with(u, a, &Default::default()).await
        }
        async fn get_with(&self, u: &Url, _: &[SocketAddr], o: &FetchOptions) -> Result<Fetched> {
            let mut status = 200;
            let (ct, body) = if u.path().contains("oembed") {
                (
                    "application/json",
                    json!({"type":"rich","title":"Embed fallback","author_name":"embed author"})
                        .to_string(),
                )
            } else if u.host_str() == Some("ap.example") {
                if o.accept
                    .as_deref()
                    .is_some_and(|s| s.contains("activity+json"))
                {
                    status = 403;
                }
                ("text/html", "<title>Public web card</title>".into())
            } else if u.host_str() == Some("soundcloud.com") {
                ("text/html", "<title>Sound</title>".into())
            } else {
                ("text/html",r#"<script type="application/ld+json">{"@type":"Article","name":"JSON first","author":"page author","duration":"PT3M","isrcCode":"USABC2600001"}</script>"#.into())
            };
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                url: u.clone(),
                status,
                body: body.into_bytes(),
                content_type: ct.into(),
                location: None,
                retry_after_secs: 60,
                validators: Default::default(),
            })
        }
    }
    let make = |kind| CardResolver {
        kind,
        config: Arc::new(Config::default()),
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            events: None,
            config: Default::default(),
            egress: Egress::with_backends(
                common::unpaced_egress_config(),
                Arc::new(PublicDns),
                Arc::new(Rich),
            ),
        },
    };
    let d = make("media")
        .resolve("https://test.wordpress.com/post")
        .await
        .unwrap();
    assert_eq!(d.type_name, "Article");
    assert_eq!(d.name.as_deref(), Some("JSON first"));
    assert_eq!(d.attributes["author"], "page author");
    assert_eq!(d.attributes["duration"], "PT3M");
    assert_eq!(d.attributes["isrc"], "USABC2600001");
    assert_eq!(
        make("media")
            .resolve("https://soundcloud.com/artist/song")
            .await
            .unwrap()
            .type_name,
        "AudioObject"
    );
    assert_eq!(
        make("activitypub")
            .resolve("https://ap.example/page")
            .await
            .unwrap()
            .name
            .as_deref(),
        Some("Public web card")
    );
}
/// Each first link here redirects to an address that carries a credential; whatever is read
/// there names relative links (`""`, `#x`).
struct Landing;
#[async_trait]
impl Transport for Landing {
    async fn get(&self, u: &Url, a: &[SocketAddr]) -> Result<Fetched> {
        self.get_with(u, a, &Default::default()).await
    }
    async fn get_with(&self, u: &Url, _: &[SocketAddr], o: &FetchOptions) -> Result<Fetched> {
        let ap = o
            .accept
            .as_deref()
            .is_some_and(|s| s.contains("activity+json"));
        let landed = u.query().is_some_and(|q| q.contains("token="));
        let note = |icon: &str| {
            json!({"type":"Note","id":u.as_str(),"content":"Note","icon":{"url":icon}})
                .to_string()
                .into_bytes()
        };
        let mut f = Fetched {
            permanent_redirect: false,
            truncated: false,
            url: u.clone(),
            status: 200,
            body: b"<title>Fallback</title>".to_vec(),
            content_type: "text/html".into(),
            location: None,
            retry_after_secs: 60,
            validators: Default::default(),
        };
        match u.path() {
            "/note" | "/alternate" | "/header" | "/oembed" if !landed => {
                f.status = 302;
                f.location = Some(format!("{}?token=SECRET", u.path()));
            }
            _ if ap => {
                f.content_type = "application/activity+json".into();
                f.body = note("#x");
            }
            "/note" => {
                f.content_type = "application/activity+json".into();
                f.body = note("");
            }
            "/alternate" => {
                f.body = br#"<title>Alternate</title><link rel="alternate" type="application/activity+json" href="">"#.to_vec();
            }
            "/header" => {
                f.body = b"<title>Header</title>".to_vec();
                f.validators.link =
                    Some("<>; rel=alternate; type=application/activity+json".into());
            }
            "/oembed" => {
                f.content_type = "application/json".into();
                f.body = json!({"type":"photo","title":"Embedded","thumbnail_url":"#x"})
                    .to_string()
                    .into_bytes();
            }
            "/video" => {
                f.body = br#"<title>Video</title><link rel="alternate" type="application/json+oembed" href="/oembed">"#.to_vec();
            }
            _ => {}
        }
        Ok(f)
    }
}

/// A card read at an address that carries a credential, by a redirect, resolves nothing back to
/// it: an ActivityPub object's empty icon, an empty alternate link or `Link: <>` (whose object is
/// then fetched), and an oEmbed answer that landed on one, on the page route and the media route.
#[tokio::test]
async fn a_card_read_at_a_credential_resolves_nothing_back_to_it() {
    let mut cfg = Config::default();
    cfg.cards.activitypub.enabled = true;
    let egress = || {
        Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            Arc::new(Landing),
        )
    };
    let web = WebResolver {
        oembed_policy: cards::OembedPolicy::Always,
        cards: Some(Arc::new(cfg.cards.clone())),
        events: None,
        config: Default::default(),
        egress: egress(),
    };
    for uri in [
        "https://social.example/note",
        "https://social.example/alternate",
        "https://social.example/header",
        "https://social.example/video",
    ] {
        let d = web.resolve(uri).await.unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("SECRET"), "{uri}: {json}");
    }
    let d = CardResolver::new("media", Arc::new(cfg), egress())
        .resolve("https://www.youtube.com/watch?v=abc")
        .await
        .unwrap();
    let json = serde_json::to_string(&d).unwrap();
    assert!(!json.contains("SECRET"), "{json}");
}

/// Nothing stored names a credential but the resource's own link, whichever field it came in by:
/// the image, an attribute, an alias, a client link, the stream, a performer's link.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn nothing_stored_names_a_credential_but_its_own_link(pool: sqlx::PgPool) {
    use pinakes_core::{
        event::Performer,
        model::{OpenIn, Stream},
        store,
    };
    let uri = "https://cal.example/feed.ics?token=OWN";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let leaked = "https://cdn.example/x.png?sig=SECRET";
    let mut d = ResourceMeta::new(uri, "web", 3600);
    d.name = Some("Calendar".into());
    d.image = Some(leaked.into());
    d.attributes.insert("feed".into(), uri.into());
    d.attributes.insert("author".into(), leaked.into());
    d.aliases
        .push("https://cal.example/feed.ics?key=SECRET".into());
    d.open_in.push(OpenIn {
        client: "App".into(),
        url: "https://app.example/#/open?token=SECRET".into(),
    });
    d.stream = Some(Stream {
        url: "https://audio.example/a.mp3?X-Amz-Signature=SECRET".into(),
        media_type: "audio/mpeg".into(),
        duration_s: None,
        preview: false,
    });
    d.event.performers.push(Performer {
        name: "Band".into(),
        links: vec![leaked.into(), "https://band.example/".into()],
    });
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let stored: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let json = stored.to_string();
    assert!(!json.contains("SECRET"), "{json}");
    assert_eq!(stored["url"], uri);
    assert_eq!(stored["attributes"]["feed"], uri);
    assert_eq!(stored["name"], "Calendar");
    assert_eq!(
        stored["performers"][0]["links"],
        json!(["https://band.example/"])
    );
}
/// A card kept as it was stored (an answer found missing keeps it) is written without a
/// credential too, and so is its change line: the guard runs at the last write.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_card_kept_as_stored_is_written_without_a_credential(pool: sqlx::PgPool) {
    use pinakes_core::{model::Status, store};
    let uri = "https://page.example/kept";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 3600);
    d.name = Some("Kept".into());
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    // As an earlier build stored it.
    sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{image}',to_jsonb('https://cdn.example/x.png?sig=SECRET'::text)) WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    d.status = Status::Missing;
    for _ in 0..2 {
        d.fetched_at += 1;
        store::save_document(&pool, &id, 0, &d).await.unwrap();
    }
    let stored: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored["status"], "missing");
    assert_eq!(stored["name"], "Kept");
    assert!(!stored.to_string().contains("SECRET"), "{stored}");
    let history: String = sqlx::query_scalar("SELECT coalesce(string_agg(concat_ws(' ',before::text,after::text),' '),'') FROM resource_change WHERE resource_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!history.contains("SECRET"), "{history}");
}

/// A change line exempts only the resource's own link: a credential an earlier build stored in a
/// document's `url` (another spelling of a public link) goes from the line too.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_change_line_exempts_only_the_resources_own_link(pool: sqlx::PgPool) {
    use pinakes_core::store;
    let uri = "https://page.example/legacy";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 3600);
    d.name = Some("First".into());
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    // As an earlier build stored it.
    sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{url}',to_jsonb('https://page.example/legacy#access_token=SECRET'::text)) WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    d.name = Some("Second".into());
    d.fetched_at += 1;
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let history: String = sqlx::query_scalar("SELECT coalesce(string_agg(concat_ws(' ',before::text,after::text),' '),'') FROM resource_change WHERE resource_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(history.contains("Second"), "{history}");
    assert!(!history.contains("SECRET"), "{history}");
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn social_deletion_purges_content_history(pool: sqlx::PgPool) {
    use pinakes_core::{model::Status, store};
    let uri = "https://social.example/post/one";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "activitypub", 3600);
    d.set_type("SocialMediaPosting");
    d.name = Some("Note".into());
    d.description = Some("Retained excerpt".into());
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    d.description = Some("Updated excerpt".into());
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap()
            > 0
    );
    d.status = Status::Missing;
    for _ in 0..2 {
        d.fetched_at += 1;
        store::save_document(&pool, &id, 0, &d).await.unwrap();
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let d: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(d["status"], "missing");
    assert!(d["description"].is_null());
}

#[tokio::test]
async fn redirected_feed_cannot_inherit_destination_container() {
    struct RedirectFeed;
    #[async_trait]
    impl Transport for RedirectFeed {
        async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Default::default(),
                url: u.clone(),
                content_type: "application/rss+xml".into(),
                status: if u.host_str() == Some("attacker.example") {
                    302
                } else {
                    200
                },
                retry_after_secs: 60,
                location: Some("https://podcast.example/feed".into()),
                body: RSS.as_bytes().to_vec(),
            })
        }
    }
    let e = Egress::with_backends(
        Default::default(),
        Arc::new(PublicDns),
        Arc::new(RedirectFeed),
    );
    let r = WebResolver {
        oembed_policy: Default::default(),
        cards: Some(Arc::new(Default::default())),
        events: None,
        config: Default::default(),
        egress: e,
    };
    let victim = r.resolve("https://podcast.example/feed").await.unwrap();
    assert_eq!(
        victim.attributes["podcast_feed"],
        "https://podcast.example/feed"
    );
    let redirect = r.resolve("https://attacker.example/feed").await.unwrap();
    assert_eq!(redirect.type_name, "PodcastSeries");
    assert_eq!(
        redirect.attributes["podcast_feed"],
        "https://attacker.example/feed"
    );
}

#[tokio::test]
async fn advertised_oembed_uses_policy_and_origin_checks() {
    struct Advertised {
        target: &'static str,
        complete: bool,
        seen: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Transport for Advertised {
        async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            self.seen.lock().unwrap().push(u.to_string());
            let body = if u.path().contains("embed") {
                json!({"title":"Embed","thumbnail_url":"https://images.example/image.jpg"})
                    .to_string()
            } else {
                format!("<title>Title</title>{}<link rel='alternate' type='application/json+oembed' href='{}'>",if self.complete {"<meta property='og:image' content='/image.jpg'>"}else{""},self.target)
            };
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Default::default(),
                url: u.clone(),
                status: 200,
                content_type: if u.path().contains("embed") {
                    "application/json"
                } else {
                    "text/html"
                }
                .into(),
                body: body.into_bytes(),
                retry_after_secs: 60,
                location: None,
            })
        }
    }
    use cards::OembedPolicy::{Always, WhenNeeded};
    for (target, complete, policy, count) in [
        ("/embed", true, WhenNeeded, 1),
        ("/embed", true, Always, 2),
        ("/embed", false, WhenNeeded, 2),
        ("https://evil.example/embed", false, Always, 1),
    ] {
        let t = Arc::new(Advertised {
            target,
            complete,
            seen: Mutex::default(),
        });
        let r = WebResolver {
            oembed_policy: policy,
            cards: None,
            events: None,
            config: Default::default(),
            egress: Egress::with_backends(
                common::unpaced_egress_config(),
                Arc::new(PublicDns),
                t.clone(),
            ),
        };
        let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
        let (doc, _) = pinakes_core::telemetry::observe(
            &metrics,
            &r.manifest(),
            r.resolve("https://page.example/item"),
        )
        .await;
        assert_eq!(
            doc.unwrap().name.as_deref(),
            Some(if count == 2 { "Embed" } else { "Title" })
        );
        assert_eq!(t.seen.lock().unwrap().len(), count);
        if !complete && count == 2 {
            assert_eq!(
                metrics
                    .discovery
                    .with_label_values(&["oembed", "missing_image"])
                    .get(),
                1
            );
        }
        if target.starts_with("https://evil") {
            assert_eq!(
                metrics
                    .discovery
                    .with_label_values(&["oembed", "untrusted"])
                    .get(),
                1
            );
        }
    }
}

fn recorded(url: &str, body: &[u8]) -> Fetched {
    Fetched {
        permanent_redirect: false,
        truncated: false,
        url: url.parse().unwrap(),
        body: body.to_vec(),
        content_type: "application/json".into(),
        status: 200,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    }
}
fn url_key(uri: &str) -> String {
    format!("url:{}", pinakes_core::model::identity(uri).unwrap().0)
}
#[test]
fn identifier_registries_name_landing_pages() {
    // Recorded Crossref CSL: `URL` is the DOI proxy, the landing page is resource.primary.
    let f = recorded(
        "https://api.crossref.org/v1/works/10.1038%2Fnature12373/transform",
        include_bytes!("fixtures/registry/crossref-nature12373.csl.json"),
    );
    let d = cards::parse_doi("doi:10.1038/nature12373", &f, 3600).unwrap();
    let registry: Vec<_> = d
        .group_keys
        .iter()
        .filter(|k| k.basis == "registry")
        .map(|k| k.key.clone())
        .collect();
    assert!(registry.contains(&url_key("https://www.nature.com/articles/nature12373")));
    // The text/html link is a legacy http registration; its https form is also claimed.
    assert!(registry.contains(&url_key("http://www.nature.com/articles/nature12373")));
    // PDFs, the DOI proxy and the doi.org identity are never landing pages.
    for absent in [
        "http://www.nature.com/articles/nature12373.pdf",
        "http://dx.doi.org/10.1038/nature12373",
        "https://doi.org/10.1038/nature12373",
    ] {
        assert!(!registry.contains(&url_key(absent)), "{absent}");
    }
    assert_eq!(registry.len(), 2, "{registry:?}");
    assert!(d
        .group_keys
        .iter()
        .any(|k| k.key == "doi:10.1038/nature12373"));
    assert!(d
        .aliases
        .contains(&"https://www.nature.com/articles/nature12373".to_string()));
    // DataCite-style CSL carries the landing page in `URL`; relative or credentialed
    // values are refused by the shared URL filter.
    let f = recorded(
        "https://doi.org/10.5281/zenodo.1",
        &serde_json::to_vec(&json!({"type":"dataset","title":"Data","URL":"https://zenodo.org/records/1",
            "link":[{"URL":"https://user:pw@zenodo.org/x","content-type":"text/html"},{"URL":"javascript:alert(1)","content-type":"text/html"}]}))
        .unwrap(),
    );
    let d = cards::parse_doi("doi:10.5281/zenodo.1", &f, 3600).unwrap();
    let registry: Vec<_> = d
        .group_keys
        .iter()
        .filter(|k| k.basis == "registry")
        .collect();
    assert_eq!(registry.len(), 1);
    assert_eq!(registry[0].key, url_key("https://zenodo.org/records/1"));

    // Recorded Open Library: the edition page (with and without its slug) is the landing
    // page; Goodreads/LibraryThing ids are not URL identity.
    let f = recorded(
        "https://openlibrary.org/api/books?bibkeys=ISBN:9780140328721&format=json&jscmd=data",
        include_bytes!("fixtures/registry/openlibrary-9780140328721.json"),
    );
    let d = cards::parse_isbn("isbn:9780140328721", "9780140328721", &f, 3600).unwrap();
    let registry: Vec<_> = d
        .group_keys
        .iter()
        .filter(|k| k.basis == "registry")
        .map(|k| k.key.clone())
        .collect();
    for page in [
        "https://openlibrary.org/books/OL7353617M",
        "https://openlibrary.org/books/OL7353617M/Fantastic_Mr._Fox",
        "http://openlibrary.org/books/OL7353617M/Fantastic_Mr._Fox",
    ] {
        assert!(registry.contains(&url_key(page)), "{page}");
    }
    assert_eq!(registry.len(), 3, "{registry:?}");
    assert!(d.group_keys.iter().any(|k| k.key == "isbn:9780140328721"));
    assert!(!serde_json::to_string(&d.group_keys)
        .unwrap()
        .contains("goodreads"));
    // Malformed edition ids from the registry are ignored.
    let f = recorded(
        "https://openlibrary.org/api/books",
        &serde_json::to_vec(&json!({"ISBN:9780140328721":{"title":"T","key":"/authors/OL1A","identifiers":{"openlibrary":["OL../x","OLM","OL12W"]}}})).unwrap(),
    );
    let d = cards::parse_isbn("isbn:9780140328721", "9780140328721", &f, 3600).unwrap();
    assert!(d.group_keys.iter().all(|k| k.basis != "registry"));
    // A slugged url on another host is not Open Library's edition page.
    let f = recorded(
        "https://openlibrary.org/api/books",
        &serde_json::to_vec(&json!({"ISBN:9780140328721":{"title":"T","key":"/books/OL7353617M","url":"https://evil.example/books/OL7353617M/slug"}})).unwrap(),
    );
    let d = cards::parse_isbn("isbn:9780140328721", "9780140328721", &f, 3600).unwrap();
    let registry: Vec<_> = d
        .group_keys
        .iter()
        .filter(|k| k.basis == "registry")
        .filter_map(|k| k.target.clone())
        .collect();
    assert_eq!(registry, ["https://openlibrary.org/books/OL7353617M"]);
}

#[tokio::test]
async fn discovered_feeds_select_episodes_on_any_origin() {
    // The media route scans feeds only with its own switch: the web one does not reach it.
    let mut cfg = Config::default();
    cfg.web.discover_feeds = true;
    let off = Arc::new(TransportMock::default());
    let page = resolver("media", cfg, off.clone())
        .web
        .resolve("https://podcast.example/episode")
        .await
        .unwrap();
    assert_ne!(page.type_name, "PodcastEpisode");
    assert!(off
        .seen
        .lock()
        .unwrap()
        .iter()
        .all(|(u, _)| u != "https://podcast.example/feed"));
    let mut cfg = Config::default();
    cfg.cards.media.discover_feeds = true;
    let seen = Arc::new(TransportMock::default());
    let r = resolver("media", cfg, seen.clone());
    // The publisher's own page keeps its feed's episode and series GUIDs.
    let own = r
        .web
        .resolve("https://podcast.example/episode")
        .await
        .unwrap();
    assert_eq!(own.type_name, "PodcastEpisode");
    assert_eq!(own.attributes["podcast_guid"], "episode-1");
    // Another origin advertising that feed with ?guid= gets the episode card too: a GUID is
    // display data, never grouping evidence.
    let copy = r
        .web
        .resolve("https://copier.example/episode?guid=episode-1")
        .await
        .unwrap();
    assert_eq!(copy.type_name, "PodcastEpisode");
    assert!(seen
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|(u, _)| u == "https://podcast.example/feed"));
}

#[tokio::test]
async fn body_links_advertise_no_feeds_or_actors() {
    let mut cfg = Config::default();
    cfg.cards.media.discover_feeds = true;
    let seen = Arc::new(TransportMock::default());
    let r = resolver("media", cfg, seen.clone());
    // Alternate links rendered from user content (in the body) are not followed.
    let d = r
        .web
        .resolve("https://comments.example/thread?guid=episode-1")
        .await
        .unwrap();
    assert_ne!(d.type_name, "PodcastEpisode");
    let seen = seen.seen.lock().unwrap();
    assert!(
        seen.iter()
            .all(|(u, _)| u.starts_with("https://comments.example/")),
        "{seen:?}"
    );
}

/// A YouTube link is named by YouTube's oEmbed answer: a regional page titled only "- YouTube"
/// (with the site's own description) is the site's stand-in for the video, a wall, and a page
/// with another title yields to the answer for name, author and thumbnail. The site's root,
/// titled "YouTube", is a page like any other.
#[tokio::test]
async fn youtube_is_named_by_its_oembed_answer_not_a_regional_page() {
    struct YouTube;
    #[async_trait]
    impl Transport for YouTube {
        async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            let (content_type, body) = if u.path() == "/oembed" {
                let video = Url::parse(
                    &u.query_pairs()
                        .find(|(k, _)| k == "url")
                        .map(|(_, v)| v.into_owned())
                        .unwrap(),
                )?;
                let id = video
                    .query_pairs()
                    .find(|(k, _)| k == "v")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                (
                    "application/json",
                    json!({"type": "video", "title": format!("Queen – Bohemian Rhapsody ({id})"),
                           "author_name": "Queen Official", "provider_name": "YouTube",
                           "thumbnail_url": format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")})
                    .to_string(),
                )
            } else {
                let v = u
                    .query_pairs()
                    .find(|(k, _)| k == "v")
                    .map(|(_, v)| v.into_owned());
                let title = match v.as_deref() {
                    Some("fJ9rUzIMcZQ") => "- YouTube",
                    Some(_) => "Queen - Bohemian Rhapsody (old upload) - YouTube",
                    None => "YouTube",
                };
                (
                    "text/html",
                    format!(
                        "<html><head><title>{title}</title>\
                         <meta property=\"og:title\" content=\"{title}\">\
                         <meta property=\"og:description\" content=\"Nauti videoista ja musiikista, jota rakastat.\">\
                         </head><body>{}</body></html>",
                        "<p>YouTube</p>".repeat(400)
                    ),
                )
            };
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Default::default(),
                url: u.clone(),
                content_type: content_type.into(),
                status: 200,
                retry_after_secs: 60,
                location: None,
                body: body.into_bytes(),
            })
        }
    }
    let resolver = CardResolver {
        kind: "media",
        config: Arc::new(Config::default()),
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            events: None,
            egress: Egress::with_backends(
                Default::default(),
                Arc::new(PublicDns),
                Arc::new(YouTube),
            ),
            config: Default::default(),
        },
    };
    // The regional page: a wall, so the card is the answer's, with no stray site description.
    let doc = resolver
        .resolve("https://www.youtube.com/watch?v=fJ9rUzIMcZQ")
        .await
        .unwrap();
    assert_eq!(doc.status, pinakes_core::model::Status::Ok);
    assert_eq!(doc.type_name, "VideoObject");
    assert_eq!(
        doc.name.as_deref(),
        Some("Queen – Bohemian Rhapsody (fJ9rUzIMcZQ)")
    );
    assert_eq!(
        doc.image.as_deref(),
        Some("https://i.ytimg.com/vi/fJ9rUzIMcZQ/hqdefault.jpg")
    );
    assert_eq!(doc.attributes["author"], "Queen Official");
    assert_eq!(doc.description, None);
    // A page with a title of its own: the answer names the video, the page keeps the rest.
    let doc = resolver
        .resolve("https://www.youtube.com/watch?v=old")
        .await
        .unwrap();
    assert_eq!(doc.name.as_deref(), Some("Queen – Bohemian Rhapsody (old)"));
    assert_eq!(doc.attributes["author"], "Queen Official");
    assert_eq!(
        doc.description.as_deref(),
        Some("Nauti videoista ja musiikista, jota rakastat.")
    );
    // The site's root titled "YouTube" is the page it is, not a stand-in.
    use pinakes_core::resolver::interstitial::{inspect, Page};
    for (link, expected) in [
        ("https://www.youtube.com/", Page::Content),
        ("https://www.youtube.com/watch?v=fJ9rUzIMcZQ", Page::Blocked),
    ] {
        let link = Url::parse(link).unwrap();
        let page = YouTube.get(&link, &[]).await.unwrap();
        let seen = pinakes_core::resolver::html::blocking(move || inspect(&link, &page).unwrap())
            .await
            .unwrap();
        assert_eq!(seen, expected);
    }
    // The placeholder titles are YouTube's own, on its hosts: the regional page's very title on
    // another site is just a title.
    let regional = YouTube
        .get(
            &Url::parse("https://www.youtube.com/watch?v=fJ9rUzIMcZQ").unwrap(),
            &[],
        )
        .await
        .unwrap();
    // Where the fetch ended decides, not where it began: a short link that redirected to
    // YouTube's placeholder is a wall, and the placeholder's title on another site (after a
    // redirect away from YouTube) is a page's title.
    for (asked, landed, expected) in [
        (
            "https://m.youtube.com/watch?v=fJ9rUzIMcZQ",
            "https://m.youtube.com/watch?v=fJ9rUzIMcZQ",
            Page::Blocked,
        ),
        (
            "https://youtu.be/fJ9rUzIMcZQ",
            "https://youtu.be/fJ9rUzIMcZQ",
            Page::Blocked,
        ),
        (
            "https://t.co/abc123",
            "https://www.youtube.com/watch?v=fJ9rUzIMcZQ",
            Page::Blocked,
        ),
        (
            "https://example.org/articles/youtube",
            "https://example.org/articles/youtube",
            Page::Content,
        ),
        (
            "https://notyoutube.com/watch?v=fJ9rUzIMcZQ",
            "https://notyoutube.com/watch?v=fJ9rUzIMcZQ",
            Page::Content,
        ),
        (
            "https://www.youtube.com/redirect?q=https%3A%2F%2Fexample.org%2Farticles%2Fyoutube",
            "https://example.org/articles/youtube",
            Page::Content,
        ),
    ] {
        let (asked, mut page) = (Url::parse(asked).unwrap(), regional.clone());
        page.url = Url::parse(landed).unwrap();
        let seen = pinakes_core::resolver::html::blocking(move || inspect(&asked, &page).unwrap())
            .await
            .unwrap();
        assert_eq!(seen, expected, "{landed}");
    }
}

/// A media page's healthy JSON-LD names the object, and oEmbed only fills gaps, unless the
/// provider names the link exactly (YouTube's `watch`): then its title, author and thumbnail
/// come first, and a field it leaves blank keeps the page's.
#[tokio::test]
async fn json_ld_names_the_object_unless_its_provider_names_the_link_exactly() {
    struct Media;
    #[async_trait]
    impl Transport for Media {
        async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            let host = u.host_str().unwrap_or_default().to_owned();
            let (content_type, body) = if u.path().ends_with("/oembed") {
                // YouTube's answer leaves its thumbnail and author blank.
                let youtube = host == "www.youtube.com";
                let blank = |s: &str| if youtube { "" } else { s }.to_owned();
                (
                    "application/json",
                    json!({"type": "video", "title": "Provider title",
                           "author_name": blank("Provider author"),
                           "thumbnail_url": blank("https://cdn.example/provider.jpg")})
                    .to_string(),
                )
            } else {
                (
                    "text/html",
                    format!(
                        "<html><head><title>Page title</title>\
                         <script type=\"application/ld+json\">{}</script>\
                         </head><body>{}</body></html>",
                        json!({"@type": "VideoObject", "name": "Page title",
                               "description": "The page's own description.",
                               "image": format!("https://{host}/page.jpg"),
                               "author": {"@type": "Person", "name": "Page author"}}),
                        "<p>Video</p>".repeat(400)
                    ),
                )
            };
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Default::default(),
                url: u.clone(),
                content_type: content_type.into(),
                status: 200,
                retry_after_secs: 60,
                location: None,
                body: body.into_bytes(),
            })
        }
    }
    let resolver = CardResolver {
        kind: "media",
        config: Arc::new(Config::default()),
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            events: None,
            egress: Egress::with_backends(
                common::unpaced_egress_config(),
                Arc::new(PublicDns),
                Arc::new(Media),
            ),
            config: Default::default(),
        },
    };
    // audio.com's scheme (`https://audio.com/*`) names no path: the page's JSON-LD names it.
    let doc = resolver
        .resolve("https://audio.com/someone/audio/a-track")
        .await
        .unwrap();
    assert_eq!(doc.name.as_deref(), Some("Page title"));
    assert_eq!(doc.image.as_deref(), Some("https://audio.com/page.jpg"));
    assert_eq!(doc.attributes["author"], "Page author");
    // YouTube's scheme names the link: its title comes first, its blank fields keep the page's.
    let doc = resolver
        .resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        .await
        .unwrap();
    assert_eq!(doc.name.as_deref(), Some("Provider title"));
    assert_eq!(
        doc.image.as_deref(),
        Some("https://www.youtube.com/page.jpg")
    );
    assert_eq!(doc.attributes["author"], "Page author");
    assert_eq!(
        doc.description.as_deref(),
        Some("The page's own description.")
    );
}
