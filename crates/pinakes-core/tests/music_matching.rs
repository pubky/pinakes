//! Captured MusicBrainz/Deezer wire shapes for the owner's unmatched ISRC card.
mod common;
use pinakes_core::{
    also_on::{self, Subject},
    config::Config,
    hubs::{musicbrainz, MbKind},
    music_enrichment,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
};
use serde_json::Value;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
const RECORDING: &str = "3276d651-6f19-4e3b-bfb2-d136f823f620";
const RELEASE: &str = "2b4cd0d4-fe21-46ac-a4fc-ece7e990a9d4";
fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/shared/music_matching/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
struct Public;
#[async_trait::async_trait]
impl Dns for Public {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}
#[derive(Default)]
struct Recorded {
    seen: Mutex<Vec<String>>,
    mismatch: bool,
    no_cover: bool,
    many_releases: bool,
    release_status: Option<u16>,
}
#[async_trait::async_trait]
impl Transport for Recorded {
    async fn get(&self, u: &url::Url, a: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(u, a, &FetchOptions::default()).await
    }
    async fn get_with(
        &self,
        u: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen.lock().unwrap().push(u.to_string());
        let mut status = 200;
        let mut location = None;
        let body = match (u.host_str().unwrap(), u.path()) {
            ("api.deezer.com", "/track/isrc:NLH2L2500268") => {
                let mut v: Value = serde_json::from_slice(&fixture("deezer.json")).unwrap();
                if self.mismatch {
                    v["isrc"] = "NLH2L2500269".into();
                }
                serde_json::to_vec(&v).unwrap()
            }
            ("musicbrainz.org", "/ws/2/release") => {
                status = self.release_status.unwrap_or(200);
                let mut v: Value = serde_json::from_slice(&fixture("releases.json")).unwrap();
                if self.many_releases {
                    v["release-count"] = 1000.into();
                }
                serde_json::to_vec(&v).unwrap()
            }
            ("musicbrainz.org", p) if p == format!("/ws/2/recording/{RECORDING}") => {
                fixture("works.json")
            }
            ("coverartarchive.org", _) => {
                if self.no_cover {
                    status = 404;
                } else {
                    status = 307;
                    location=Some(format!("https://archive.org/download/mbid-{RELEASE}/mbid-{RELEASE}-42800579313_thumb500.jpg"));
                }
                vec![]
            }
            // No Wikidata item: an explicit negative fixture, unrelated to matching.
            ("www.wikidata.org", _) => {
                br#"{"query":{"search":[],"searchinfo":{"totalhits":0}}}"#.to_vec()
            }
            _ => panic!("unexpected request {u}"),
        };
        Ok(Fetched {
            url: u.clone(),
            status,
            location,
            content_type: "application/json".into(),
            body,
            retry_after_secs: 60,
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
        })
    }
}
fn cx(t: Arc<Recorded>) -> also_on::Context {
    also_on::Context {
        egress: Egress::with_backends(common::unpaced_egress_config(), Arc::new(Public), t),
        config: Arc::new(Config::default()),
    }
}
fn subject(uri: &str) -> Subject {
    Subject {
        id: String::new(),
        uri: uri.into(),
        status: "ok".into(),
        type_name: Some("MusicRecording".into()),
        doc: None,
        keys: vec![("isrc:NLH2L2500268".into(), "musicbrainz_entity".into())],
        first_seen_at: None,
        candidates: vec![],
    }
}
#[tokio::test]
async fn musicbrainz_isrc_and_recording_use_exact_deezer_lookup() {
    for uri in [
        "https://musicbrainz.org/isrc/NLH2L2500268".to_owned(),
        format!("https://musicbrainz.org/recording/{RECORDING}"),
    ] {
        let s = subject(&uri);
        let Some(source) = also_on::registered()
            .into_iter()
            .find(|s| s.id() == "deezer")
        else {
            return;
        };
        assert!(
            source.applies(&s),
            "must use the already verified ISRC of {uri}"
        );
        let found = source
            .lookup(&cx(Arc::new(Recorded::default())), &s)
            .await
            .unwrap();
        assert_eq!(found.links.len(), 1);
        assert_eq!(
            found.links[0].uri,
            "https://www.deezer.com/track/3507590481"
        );
        assert_eq!(found.links[0].key.as_deref(), Some("isrc:NLH2L2500268"));
        let found = source
            .lookup(
                &cx(Arc::new(Recorded {
                    mismatch: true,
                    ..Default::default()
                })),
                &s,
            )
            .await
            .unwrap();
        assert!(
            found.links.is_empty(),
            "a mismatched answer is never an edge"
        );
    }
}
#[test]
fn exact_reverse_sources_accept_known_mb_levels_not_arbitrary_pages() {
    let sources = also_on::registered()
        .into_iter()
        .filter(|s| matches!(s.id(), "deezer" | "tidal_isrc" | "spotify_isrc"));
    for source in sources {
        let mut track = subject(&format!("https://musicbrainz.org/recording/{RECORDING}"));
        assert!(source.applies(&track), "{} MB recording", source.id());
        track.keys.clear();
        assert!(!source.applies(&track));
        let mut album = subject(&format!("https://musicbrainz.org/release/{RELEASE}"));
        album.type_name = Some("MusicAlbum".into());
        album.keys = vec![("upc:8719244868629".into(), "musicbrainz_entity".into())];
        assert!(source.applies(&album), "{} MB release", source.id());
        album.keys = vec![("isrc:NLH2L2500268".into(), "musicbrainz_entity".into())];
        assert!(!source.applies(&album));
        assert!(!source.applies(&subject("https://example.com/track/1")));
    }
}
#[tokio::test]
async fn recording_containment_is_a_fact_never_an_identity_key() {
    let mut s = subject(&format!("https://musicbrainz.org/recording/{RECORDING}"));
    s.keys.push((
        format!("mb:recording:{RECORDING}"),
        "musicbrainz_entity".into(),
    ));
    let Some(source) = also_on::registered()
        .into_iter()
        .find(|s| s.id() == "musicbrainz_containment")
    else {
        return;
    };
    let found = source
        .lookup(&cx(Arc::new(Recorded::default())), &s)
        .await
        .unwrap();
    assert!(found.keys.is_empty() && found.links.is_empty());
    assert!(found.facts.iter().any(|f| f.name == "track_of_album"
        && f.value == format!("https://musicbrainz.org/release/{RELEASE}")));
    assert_eq!(
        source.ttl(),
        std::time::Duration::from_secs(30 * 86_400),
        "containment must not reschedule every provider daily"
    );
    let capped = source
        .lookup(
            &cx(Arc::new(Recorded {
                many_releases: true,
                ..Default::default()
            })),
            &s,
        )
        .await
        .unwrap();
    assert!(
        !capped.facts.is_empty(),
        "a deliberately bounded page is usable"
    );
    let missing = source
        .lookup(
            &cx(Arc::new(Recorded {
                release_status: Some(404),
                ..Default::default()
            })),
            &s,
        )
        .await
        .unwrap();
    assert!(missing.facts.is_empty());
    assert!(source
        .lookup(
            &cx(Arc::new(Recorded {
                release_status: Some(503),
                ..Default::default()
            })),
            &s
        )
        .await
        .is_err());
}
#[tokio::test]
async fn recording_uses_its_release_cover_not_an_album_identity_key() {
    let entity = musicbrainz::parse_entity(
        MbKind::Recording,
        &serde_json::from_slice(&fixture("recording.json")).unwrap(),
    )
    .unwrap();
    let t = Arc::new(Recorded::default());
    let context = cx(t.clone());
    let found =
        music_enrichment::enrich_within(&context.egress, musicbrainz::BASE, &entity, 500, None)
            .await;
    let mut doc = pinakes_core::model::ResourceMeta::new(
        &format!("https://musicbrainz.org/recording/{RECORDING}"),
        "musicbrainz",
        3600,
    );
    doc.set_type("MusicRecording");
    found.apply(&mut doc, 300);
    assert_eq!(
        doc.image.as_deref(),
        Some(format!("https://coverartarchive.org/release/{RELEASE}/front-500").as_str())
    );
    assert!(doc
        .credits
        .iter()
        .any(|c| c.source_url == format!("https://musicbrainz.org/release/{RELEASE}/cover-art")));
    let cx = cx(Arc::new(Recorded {
        no_cover: true,
        many_releases: true,
        ..Default::default()
    }));
    let found =
        music_enrichment::enrich_within(&cx.egress, musicbrainz::BASE, &entity, 500, None).await;
    assert_eq!(
        found.image,
        music_enrichment::Found::Missing,
        "a deliberate page cap is not a transient image read"
    );
    let mut missing = pinakes_core::model::ResourceMeta::new(
        &format!("https://musicbrainz.org/recording/{RECORDING}"),
        "musicbrainz",
        3600,
    );
    missing.set_type("MusicRecording");
    found.apply(&mut missing, 300);
    assert!(
        missing.image.is_none(),
        "a release front flag is not proof that CAA serves an image"
    );
}
