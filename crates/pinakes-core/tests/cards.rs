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
                else {assert_eq!(options.accept.as_deref(),Some("application/activity+json"));("application/activity+json",json!({"type":"Person","preferredUsername":"alice","summary":"<p>About Alice</p>","icon":{"url":"/avatar.png"}}).to_string())}
            },
            "podcast.example" => {
                if u.path()=="/episode" {("text/html","<title>Episode page</title><link rel='alternate' type='application/rss+xml' href='/feed'>".into())}
                else {("application/rss+xml",RSS.into())}
            },
            _=>bail!("unexpected request"),
        };
        Ok(Fetched {
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
const RSS: &str = r#"<rss xmlns:podcast="https://podcastindex.org/namespace/1.0"><channel><title>Podcast</title><podcast:guid>channel-id</podcast:guid><item><guid>episode-1</guid><title>Episode One</title><link>https://podcast.example/episode</link></item><item><guid>episode-2</guid><title>Episode Two</title></item></channel></rss>"#;
fn resolver(kind: &'static str, cfg: Config, transport: Arc<TransportMock>) -> CardResolver {
    CardResolver {
        kind,
        web: WebResolver {
            cards: Some(Arc::new(cfg.cards.clone())),
            events: None,
            egress: Egress::with_backends(Default::default(), Arc::new(PublicDns), transport),
            config: cfg.web.clone(),
        },
        config: Arc::new(cfg),
    }
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
        let d = resolver(kind, Config::default(), seen.clone())
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
    assert!(d
        .group_keys
        .iter()
        .any(|k| k.key == "podcast_guid:episode-2"));
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
    assert_eq!(series.group_keys[0].key, "podcast_series_guid:channel-id");
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
        assert!(d.group_keys.iter().any(|k| k.basis == "isrc"));
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
