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
            } else {
                r#"<script type="application/ld+json">{"@type":"Article","name":"Complete","image":"https://images.example/page.jpg"}</script>"#.into()
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
fn resolver(kind: &'static str, cfg: Config, transport: Arc<TransportMock>) -> CardResolver {
    CardResolver::new(
        kind,
        Arc::new(cfg),
        Egress::with_backends(Default::default(), Arc::new(PublicDns), transport),
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
        let d = pinakes_core::resolver::web::extract("https://example.org/a", &f, 3600).doc;
        assert_eq!(d.type_name, ty);
        assert_eq!(d.attributes["isrc"], "US-S1Z-99-00001");
    }
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
#[sqlx::test(migrations = "../../migrations")]
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
            egress: Egress::with_backends(Default::default(), Arc::new(PublicDns), Arc::new(Rich)),
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
#[sqlx::test(migrations = "../../migrations")]
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
            egress: Egress::with_backends(Default::default(), Arc::new(PublicDns), t.clone()),
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
