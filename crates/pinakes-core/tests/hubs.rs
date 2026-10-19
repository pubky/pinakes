//! The hub clients through Egress, against recorded MusicBrainz and Wikidata answers.
use pinakes_core::{
    config::EgressConfig,
    hubs::{musicbrainz, wikidata, MbKind},
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        RetryLater,
    },
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

/// Answers by the request's path and query, from `tests/fixtures/hubs`; records each request.
struct Recorded {
    seen: Mutex<Vec<String>>,
}
fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/hubs/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
#[async_trait::async_trait]
impl Transport for Recorded {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &FetchOptions::default())
            .await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        options: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        assert_eq!(options.accept.as_deref(), Some("application/json"));
        let target = format!("{}?{}", url.path(), url.query().unwrap_or(""));
        self.seen.lock().unwrap().push(target.clone());
        let query = |k: &str| {
            url.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        let (status, body) = match url.path() {
            "/ws/2/url" => match query("resource").as_str() {
                "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq" => {
                    (200, fixture("mb-url-spotify-track.json"))
                }
                "https://busy.example/x" => (503, fixture("mb-503.json")),
                _ => (404, fixture("mb-url-unknown.json")),
            },
            "/ws/2/recording/9f9cf187-d6f9-437f-9d98-d59cdbd52757" => {
                (200, fixture("mb-recording.json"))
            }
            "/w/api.php" if query("list") == "search" => (200, fixture("wd-search-p436.json")),
            "/w/api.php" if query("action") == "wbgetentities" => {
                (200, fixture("wd-entities.json"))
            }
            other => panic!("unexpected request {other}"),
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status,
            retry_after_secs: 120,
            location: None,
            content_type: "application/json".into(),
            body,
        })
    }
}
fn egress(transport: Arc<Recorded>) -> Egress {
    Egress::with_backends(EgressConfig::default(), Arc::new(PublicDns), transport)
}

#[tokio::test]
async fn musicbrainz_answers_known_unknown_and_busy() {
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let e = egress(t.clone());
    let track = musicbrainz::url(&e, "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        track.targets,
        [(
            MbKind::Recording,
            "9f9cf187-d6f9-437f-9d98-d59cdbd52757".into()
        )]
    );
    let recording = musicbrainz::entity(&e, MbKind::Recording, &track.targets[0].1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recording.title.as_deref(), Some("Paranoid Android"));
    // A URL MusicBrainz does not know: nothing, not an error.
    assert!(
        musicbrainz::url(&e, "https://open.spotify.com/track/unknown")
            .await
            .unwrap()
            .is_none()
    );
    // A busy hub: retry later, with the hub's own wait.
    let busy = musicbrainz::url(&e, "https://busy.example/x")
        .await
        .unwrap_err();
    let retry = busy.downcast_ref::<RetryLater>().expect("retry later");
    assert_eq!((retry.class, retry.seconds), ("musicbrainz_busy", 120));
    // A malformed MBID asks nothing.
    let before = t.seen.lock().unwrap().len();
    assert!(musicbrainz::entity(&e, MbKind::Release, "not-an-mbid")
        .await
        .unwrap()
        .is_none());
    assert_eq!(t.seen.lock().unwrap().len(), before);
}

#[tokio::test]
async fn wikidata_reads_back_every_hit_before_choosing_one() {
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let e = egress(t.clone());
    // Two items carry this release group: ambiguous, no item.
    let found = wikidata::item_with(&e, "P436", "b1392450-e666-3926-a536-22c65f834433")
        .await
        .unwrap();
    assert_eq!(found, wikidata::Found::Ambiguous(2));
    let seen = t.seen.lock().unwrap().clone();
    assert!(
        seen[0].contains("srsearch=haswbstatement%3AP436%3Db1392450-e666-3926-a536-22c65f834433"),
        "{seen:?}"
    );
    assert!(seen[1].contains("ids=Q202996%7CQ33328630"), "{seen:?}");
    // Spotify album ID of one of them: exactly that item, with its sitelinks.
    let found = wikidata::item_with(&e, "P2205", "2fGCAYUMssLKiUAoNdxGLx")
        .await
        .unwrap();
    let wikidata::Found::One(item) = found else {
        panic!("{found:?}")
    };
    assert_eq!(item.qid, "Q202996");
    assert!(item.sitelinks.contains_key("dewiki"));
    // A value the search syntax cannot carry exactly asks nothing.
    let before = t.seen.lock().unwrap().len();
    assert_eq!(
        wikidata::item_with(&e, "P436", "a value with spaces")
            .await
            .unwrap(),
        wikidata::Found::None
    );
    assert_eq!(t.seen.lock().unwrap().len(), before);
}
