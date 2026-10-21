//! The hub clients through Egress, against recorded MusicBrainz and Wikidata answers.
use pinakes_core::{
    also_on,
    api::{self, AppState},
    config::{Config, EgressConfig},
    groups,
    hubs::{musicbrainz, wikidata, MbKind},
    model::ResourceMeta,
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        RetryLater,
    },
    store,
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
/// MusicBrainz's answer to one URL lookup.
fn mb_url(resource: &str) -> (u16, Vec<u8>) {
    match resource {
        "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq" => {
            (200, fixture("mb-url-spotify-track.json"))
        }
        "https://busy.example/x" => (503, fixture("mb-503.json")),
        "https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE" => {
            (200, fixture("mb-url-spotify-album.json"))
        }
        "https://tidal.com/track/567396540" => (200, fixture("mb-url-tidal-567396540.json")),
        // Dracula: two releases ("Dracula", "Dracula (remix)") of one release group.
        "https://tidal.com/album/495402659" => (200, fixture("mb-url-tidal-495402659.json")),
        _ => (404, fixture("mb-url-unknown.json")),
    }
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
        // An artist whose record is over MusicBrainz's cap, and its page (the card's fallback).
        if url.host_str() == Some("musicbrainz.org") && url.path().ends_with(BIG_BAND) {
            self.seen
                .lock()
                .unwrap()
                .push(format!("musicbrainz.org{}", url.path()));
            if url.path().starts_with("/ws/2/") {
                return Err(pinakes_core::resolver::egress::BodyTooLarge {
                    cap: pinakes_core::hubs::MUSICBRAINZ_CAP,
                }
                .into());
            }
            return Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Default::default(),
                url: url.clone(),
                status: 200,
                retry_after_secs: 60,
                location: None,
                content_type: "text/html".into(),
                body: b"<html><head><title>Big Band</title></head><body></body></html>".to_vec(),
            });
        }
        assert_eq!(options.accept.as_deref(), Some("application/json"));
        // Much-edited Wikidata items and MusicBrainz entities with many links exceed the default
        // cap; their reads may be larger.
        let cap = match url.host_str() {
            Some("www.wikidata.org") => Some(pinakes_core::hubs::WIKIDATA_CAP),
            Some("musicbrainz.org" | "mb-mirror.example") => {
                Some(pinakes_core::hubs::MUSICBRAINZ_CAP)
            }
            _ => None,
        };
        assert_eq!(options.body_cap, cap, "{url}");
        let target = format!(
            "{}{}?{}",
            url.host_str().unwrap_or(""),
            url.path(),
            url.query().unwrap_or("")
        );
        self.seen.lock().unwrap().push(target.clone());
        let query = |k: &str| {
            url.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        let (status, body) = match url.path() {
            // A batch (several `resource`s): MusicBrainz's `urls` list of the links it knows,
            // each as its single lookup answers.
            "/ws/2/url" if url.query_pairs().filter(|(k, _)| k == "resource").count() > 1 => {
                let urls: Vec<serde_json::Value> = url
                    .query_pairs()
                    .filter(|(k, _)| k == "resource")
                    .filter_map(|(_, r)| match mb_url(&r) {
                        (200, body) => serde_json::from_slice(&body).ok(),
                        _ => None,
                    })
                    .collect();
                let body =
                    serde_json::json!({"url-offset": 0, "url-count": urls.len(), "urls": urls});
                (200, serde_json::to_vec(&body).unwrap())
            }
            "/ws/2/url" => mb_url(&query("resource")),
            "/ws/2/recording/9f9cf187-d6f9-437f-9d98-d59cdbd52757" => {
                (200, fixture("mb-recording.json"))
            }
            "/ws/2/release/30702389-5c67-4438-9ea0-2351c8de0f1d" => {
                (200, fixture("mb-release.json"))
            }
            "/ws/2/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69" => {
                (200, fixture("mb-recording-b1a9c0e9.json"))
            }
            // An MBID merged into b1a9c0e9: MusicBrainz answers with that recording.
            "/ws/2/recording/00000000-0000-4000-8000-00000000000d" => {
                (200, fixture("mb-recording-b1a9c0e9.json"))
            }
            // A recording with nine ISRCs, the page's own sorting last.
            "/ws/2/isrc/ZZAAA2600010" => (
                200,
                br#"{"isrcs":"ZZAAA2600010","recordings":[{"id":"00000000-0000-4000-8000-000000000009"}]}"#.to_vec(),
            ),
            "/ws/2/recording/00000000-0000-4000-8000-000000000009" => (
                200,
                serde_json::to_vec(&serde_json::json!({
                    "id": "00000000-0000-4000-8000-000000000009",
                    "title": "Many codes",
                    "isrcs": (1..=8).map(|n| format!("AAAAA260000{n}")).chain(["ZZAAA2600010".to_owned()]).collect::<Vec<_>>(),
                    "relations": [],
                }))
                .unwrap(),
            ),
            "/ws/2/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711" => {
                (200, fixture("mb-artist-a74b1b7f.json"))
            }
            // A recording's official albums (its original album).
            "/ws/2/release" if query("recording") == "b1a9c0e9-d987-4042-ae91-78d6a3267d69" => {
                (200, fixture("mb-browse-releases-b1a9c0e9.json"))
            }
            "/ws/2/release" => (200, br#"{"release-count":0,"releases":[]}"#.to_vec()),
            "/ws/2/isrc/GBUM71029604" => (200, fixture("mb-isrc-GBUM71029604.json")),
            "/ws/2/isrc/GBCEE0100112" => (200, fixture("mb-isrc-GBCEE0100112.json")),
            "/ws/2/isrc/GBUM72103841" => (404, fixture("mb-isrc-GBUM72103841.json")),
            "/ws/2/release/19fc791d-01d1-460d-960a-edb19cad0ee8" => {
                (200, fixture("mb-release-19fc791d.json"))
            }
            "/ws/2/release/132919b9-60a8-4835-884f-3a943ba587bf" => {
                (200, fixture("mb-release-132919b9.json"))
            }
            "/lookup" if query("upc") == "0634904078164" => (200, fixture("itunes-upc.json")),
            "/lookup" if query("upc") == "0602557656527" => {
                (200, fixture("itunes-upc-0602557656527.json"))
            }
            "/lookup" => (200, fixture("itunes-upc-many.json")),
            // Deezer's API: its own records, and its lookups by ISRC and barcode.
            path if url.host_str() == Some("api.deezer.com") => {
                let name = format!(
                    "deezer-{}.json",
                    path.trim_start_matches('/').replace(['/', ':'], "-")
                );
                let recorded = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
                // What Deezer answers for anything it has no data for.
                let none = br#"{"error":{"type":"DataException","message":"no data","code":800}}"#;
                (
                    200,
                    std::fs::read(recorded).unwrap_or_else(|_| none.to_vec()),
                )
            }
            // A Wikipedia's own API: the article's Wikidata item.
            "/w/api.php" if url.host_str() != Some("www.wikidata.org") => {
                match (url.host_str().unwrap_or(""), query("titles").as_str()) {
                    ("en.wikipedia.org", "Kant") => (200, fixture("enwiki-kant-redirect.json")),
                    ("en.wikipedia.org", "OK_Computer") => (200, fixture("enwiki-ok-computer.json")),
                    // An article whose item is over Wikidata's cap.
                    ("en.wikipedia.org", "Big_Country") => (
                        200,
                        br#"{"query":{"pages":{"2":{"pageid":2,"ns":0,"title":"Big Country","pageprops":{"wikibase_item":"Q999999999"}}}}}"#.to_vec(),
                    ),
                    ("de.wikipedia.org", "Immanuel_Kant") => (
                        200,
                        br#"{"query":{"pages":{"1":{"pageid":1,"ns":0,"title":"Immanuel Kant","pageprops":{"wikibase_item":"Q9312"}}}}}"#.to_vec(),
                    ),
                    _ => (200, fixture("enwiki-missing.json")),
                }
            }
            "/w/api.php" if query("list") == "search" => (200, fixture("wd-search-p436.json")),
            // An item over Wikidata's cap.
            "/w/api.php" if query("action") == "wbgetentities" && query("ids") == "Q999999999" => {
                return Err(pinakes_core::resolver::egress::BodyTooLarge {
                    cap: pinakes_core::hubs::WIKIDATA_CAP,
                }
                .into())
            }
            "/w/api.php" if query("action") == "wbgetentities" && query("ids") == "Q9312" => {
                (200, fixture("wd-kant.json"))
            }
            "/w/api.php" if query("action") == "wbgetentities" => {
                (200, fixture("wd-entities.json"))
            }
            other => panic!("unexpected request {other}"),
        };
        // As the services answer: iTunes says its JSON is JavaScript.
        let content_type = if url.path() == "/lookup" {
            "text/javascript"
        } else {
            "application/json"
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status,
            retry_after_secs: 120,
            location: None,
            content_type: content_type.into(),
            body,
        })
    }
}
/// An artist whose MusicBrainz record is over the cap.
const BIG_BAND: &str = "00000000-0000-4000-8000-00000000000b";
fn egress(transport: Arc<Recorded>) -> Egress {
    Egress::with_backends(EgressConfig::default(), Arc::new(PublicDns), transport)
}

#[tokio::test]
async fn musicbrainz_answers_known_unknown_and_busy() {
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let e = egress(t.clone());
    let track = musicbrainz::url(
        &e,
        musicbrainz::BASE,
        "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
    )
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
    let recording = musicbrainz::entity(
        &e,
        musicbrainz::BASE,
        MbKind::Recording,
        &track.targets[0].1,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(recording.title.as_deref(), Some("Paranoid Android"));
    // A URL MusicBrainz does not know: nothing, not an error.
    assert!(musicbrainz::url(
        &e,
        musicbrainz::BASE,
        "https://open.spotify.com/track/unknown"
    )
    .await
    .unwrap()
    .is_none());
    // A busy hub: retry later, with the hub's own wait.
    let busy = musicbrainz::url(&e, musicbrainz::BASE, "https://busy.example/x")
        .await
        .unwrap_err();
    let retry = busy.downcast_ref::<RetryLater>().expect("retry later");
    assert_eq!((retry.class, retry.seconds), ("musicbrainz_busy", 120));
    // A malformed MBID asks nothing.
    let before = t.seen.lock().unwrap().len();
    assert!(
        musicbrainz::entity(&e, musicbrainz::BASE, MbKind::Release, "not-an-mbid")
            .await
            .unwrap()
            .is_none()
    );
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

/// Two Spotify editions of one album: MusicBrainz names the first (a release, its links and,
/// through its barcode, the Apple Music album), Wikidata the second (its item and release
/// group). The shared release group joins them; each card lists the other first, then the
/// derived links by service.
#[sqlx::test(migrations = "../../migrations")]
async fn music_links_join_through_the_hubs_and_list_each_other(pool: sqlx::PgPool) {
    use tower::ServiceExt;
    let first = "https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE";
    let second = "https://open.spotify.com/album/2fGCAYUMssLKiUAoNdxGLx";
    let mut ids = vec![];
    for (uri, name) in [(first, "OK Computer"), (second, "OK Computer OKNOTOK")] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "media", 86400);
        d.set_type("MusicAlbum");
        d.name = Some(name.into());
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let config = Config::default();
    let cx = also_on::Context {
        egress: egress(t.clone()),
        config: Arc::new(config.clone()),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    let keys = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT key||' '||source FROM resource_key WHERE resource_id=$1 ORDER BY key",
            )
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        keys(ids[0].clone()).await,
        [
            "mb:release-group:b1392450-e666-3926-a536-22c65f834433 musicbrainz",
            "mb:release:30702389-5c67-4438-9ea0-2351c8de0f1d musicbrainz",
            "upc:0634904078164 musicbrainz"
        ]
    );
    // The first album's Spotify ID is not the one Wikidata records: no item, no key.
    assert_eq!(
        keys(ids[1].clone()).await,
        [
            "mb:release-group:b1392450-e666-3926-a536-22c65f834433 wikidata",
            "wikidata:Q202996 wikidata"
        ]
    );
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), config).unwrap());
    let mut request = axum::http::Request::builder()
        .uri(format!("/v1/resources/{}", ids[0]))
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // Every link: the details' list (the card shows one per music service).
    let also_on = card["details"]["also_on"].as_array().unwrap();
    // The other edition first, as a tagged member with its ID.
    assert_eq!(also_on[0]["id"], ids[1].as_str(), "{card}");
    let derived: Vec<(&str, &str, &str)> = also_on[1..]
        .iter()
        .map(|e| {
            assert_eq!(e["derived"], true);
            (
                e["site"].as_str().unwrap(),
                e["uri"].as_str().unwrap(),
                e["via"].as_str().unwrap(),
            )
        })
        .collect();
    // Both editions' derived links, each URL once (the same Apple album came through
    // iTunes for the first and through Wikidata for the second).
    for expected in [
        ("Apple Music", "https://music.apple.com/us/album/1097861387"),
        ("Apple Music", "https://music.apple.com/gb/album/1097861387"),
        (
            "Bandcamp",
            "https://radiohead.bandcamp.com/album/ok-computer",
        ),
        ("Deezer", "https://www.deezer.com/album/14879699"),
        ("TIDAL", "https://tidal.com/album/58990510"),
    ] {
        assert!(
            derived
                .iter()
                .any(|(site, uri, _)| (*site, *uri) == expected),
            "{expected:?} in {derived:?}"
        );
    }
    let uris: Vec<&str> = derived.iter().map(|d| d.1).collect();
    let mut once = uris.clone();
    once.dedup();
    assert_eq!(uris.len(), once.len());
    // iTunes answers the barcode MusicBrainz named: one album, keyed by the barcode.
    let itunes: String = sqlx::query_scalar(
        "SELECT uri||' '||key FROM derived_link WHERE resource_id=$1 AND via='itunes'",
    )
    .bind(&ids[0])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        itunes,
        "https://music.apple.com/us/album/1097861387 upc:0634904078164"
    );
    // Album level only: no track, no artist, and never a non-music page.
    for (_, uri, _) in &derived {
        let link = pinakes_core::music::parse(uri).unwrap();
        assert!(
            matches!(
                link.level,
                pinakes_core::music::Level::Album | pinakes_core::music::Level::Playlist
            ),
            "{uri}"
        );
    }
    // By service name, then URL.
    let sites: Vec<String> = derived.iter().map(|d| d.0.to_lowercase()).collect();
    let mut sorted = sites.clone();
    sorted.sort();
    assert_eq!(sites, sorted);
}

/// An English link (a redirect's title) and a German article are one Wikidata item: they join,
/// and each card lists the other first, then the other editions and encyclopedias.
#[sqlx::test(migrations = "../../migrations")]
async fn wikipedia_editions_join_through_their_item(pool: sqlx::PgPool) {
    use tower::ServiceExt;
    let en = "https://en.m.wikipedia.org/wiki/Kant";
    let de = "https://de.wikipedia.org/wiki/Immanuel_Kant";
    let mut ids = vec![];
    for uri in [en, de] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.set_type("Article");
        d.name = Some("Immanuel Kant".into());
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    // The mobile link is an alias of the canonical article.
    let root: String = sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
        .bind(&ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        root,
        pinakes_core::model::identity("https://en.wikipedia.org/wiki/Kant")
            .unwrap()
            .0
    );
    let config = Config::default();
    let cx = also_on::Context {
        egress: egress(Arc::new(Recorded {
            seen: Mutex::default(),
        })),
        config: Arc::new(config.clone()),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), config).unwrap());
    let mut request = axum::http::Request::builder()
        .uri(format!("/v1/resources/{}", ids[1]))
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let also_on = card["also_on"].as_array().unwrap();
    assert_eq!(also_on[0]["id"], root.as_str(), "{card}");
    let sites: Vec<&str> = also_on[1..]
        .iter()
        .map(|e| e["site"].as_str().unwrap())
        .collect();
    for site in [
        "Britannica",
        "Stanford Encyclopedia of Philosophy",
        "Wikipedia (fr)",
    ] {
        assert!(sites.contains(&site), "{site} in {sites:?}");
    }
    // Never its own edition. The tagged English link is a redirect's title; the English
    // edition the item names is the article itself, another URL, so it is listed too.
    assert!(!sites.contains(&"Wikipedia (de)"));
    assert!(also_on[1..]
        .iter()
        .any(|e| e["uri"] == "https://en.wikipedia.org/wiki/Immanuel_Kant"));
    // The two editions' runs name each other's resources (keyed links) at once, and neither
    // fails: each locks the rows it touches in order.
    let failed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM also_on_dirty WHERE last_error IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(failed, 0);
}

/// A Wikidata item link is its own hub: any spelling is the item's canonical link, and its card
/// lists the item's encyclopedia entries and Wikipedia editions from its own claims and
/// sitelinks (one request, no search).
#[sqlx::test(migrations = "../../migrations")]
async fn a_wikidata_item_lists_its_editions_and_encyclopedias(pool: sqlx::PgPool) {
    use tower::ServiceExt;
    let spelled = "https://m.wikidata.org/wiki/Q9312";
    let alias = store::ensure_resource(&pool, spelled).await.unwrap();
    let id = pinakes_core::model::identity("https://www.wikidata.org/wiki/Q9312")
        .unwrap()
        .0;
    let root: String = sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
        .bind(&alias)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(root, id);
    let mut d = ResourceMeta::new(
        "https://www.wikidata.org/wiki/Q9312",
        "event_platform",
        86400,
    );
    d.set_type("Person");
    d.name = Some("Immanuel Kant".into());
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let config = Config::default();
    let cx = also_on::Context {
        egress: egress(t.clone()),
        config: Arc::new(config.clone()),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    let seen = t.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "one request: {seen:?}");
    assert!(seen[0].contains("action=wbgetentities") && seen[0].contains("ids=Q9312"));
    let app = api::router(AppState::new(pool.clone(), config).unwrap());
    let mut request = axum::http::Request::builder()
        .uri(format!("/v1/resources/{id}"))
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let sites: Vec<&str> = card["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["site"].as_str().unwrap())
        .collect();
    for site in [
        "Britannica",
        "Stanford Encyclopedia of Philosophy",
        "Wikipedia (en)",
        "Wikipedia (de)",
    ] {
        assert!(sites.contains(&site), "{site} in {sites:?}");
    }
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM resource_key WHERE resource_id=$1")
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(keys, ["wikidata:Q9312"]);
}

/// An album link MusicBrainz relates to several releases of one release group (Dracula: TIDAL,
/// Apple and Deezer each point to "Dracula" and "Dracula (remix)") takes the release group's key
/// alone, and the links every release has. A configured mirror is asked instead of musicbrainz.org.
#[sqlx::test(migrations = "../../migrations")]
async fn editions_of_one_release_group_take_the_group(pool: sqlx::PgPool) {
    let uri = "https://tidal.com/album/495402659";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "media", 86400);
    d.set_type("MusicAlbum");
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let mut config = Config::default();
    config.also_on.sources.insert(
        "musicbrainz".into(),
        also_on::SourceOptions {
            base_url: Some("https://mb-mirror.example/ws/2".into()),
            ..Default::default()
        },
    );
    config.validate().unwrap();
    let cx = also_on::Context {
        egress: egress(t.clone()),
        config: Arc::new(config),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    let rows = |sql: &'static str| {
        let (pool, id) = (pool.clone(), id.clone());
        async move {
            sqlx::query_scalar::<_, String>(sql)
                .bind(id)
                .fetch_all(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        rows("SELECT key FROM resource_key WHERE resource_id=$1 ORDER BY key").await,
        [
            "mb:release-group:86b6aeeb-f305-45f6-a9c0-76bd6235227e",
            "upc:0196874054246"
        ]
    );
    assert_eq!(
        rows("SELECT site||' '||uri FROM derived_link WHERE resource_id=$1 AND source='musicbrainz' ORDER BY site").await,
        [
            "Apple Music https://music.apple.com/au/album/1874125260",
            "Deezer https://www.deezer.com/album/910510411",
            "Qobuz https://open.qobuz.com/album/l048fnzy555c8",
            "Spotify https://open.spotify.com/album/6KFq3hJB6JnNvDbEHRvPoK",
        ]
    );
    // Facts both releases share: the barcode, not either MBID or the differing credits.
    assert_eq!(
        rows("SELECT name||' '||value FROM resource_fact WHERE resource_id=$1 ORDER BY name").await,
        ["upc 0196874054246"]
    );
    let seen = t.seen.lock().unwrap().clone();
    let mb: Vec<_> = seen.iter().filter(|s| s.contains("/ws/2/")).collect();
    assert_eq!(mb.len(), 3, "{seen:?}");
    assert!(
        mb.iter().all(|s| s.starts_with("mb-mirror.example/ws/2/")),
        "{seen:?}"
    );
}

/// The owner's Bohemian Rhapsody on TIDAL: a TIDAL track page states its own ISRC, a key. One
/// MusicBrainz does not relate by URL joins the one recording its ISRC is on; an ISRC nobody else
/// knows keeps its key and stays alone; one MusicBrainz gives to two recordings is no key.
#[sqlx::test(migrations = "../../migrations")]
async fn a_service_pages_own_isrc_joins_its_recording(pool: sqlx::PgPool) {
    let tracks = [
        // The seed: MusicBrainz relates the URL to recording b1a9c0e9.
        (
            "https://tidal.com/track/567396540",
            "GBCEE0100112",
            "Greatest Hits - We Will Rock You Edition",
        ),
        // Not related by URL; its ISRC is on b1a9c0e9 alone.
        (
            "https://tidal.com/track/534050211",
            "GBUM71029604",
            "A Night At The Opera (Deluxe Edition)",
        ),
        // An ISRC MusicBrainz does not know.
        (
            "https://tidal.com/track/551040646",
            "GBUM72103841",
            "Greatest Hits",
        ),
    ];
    let mut ids = vec![];
    for (uri, isrc, album) in tracks {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "jsonld", 86400);
        d.set_type("MusicRecording");
        d.name = Some("Bohemian Rhapsody".into());
        d.attributes.insert("isrc".into(), isrc.into());
        // As TIDAL's JSON-LD states them.
        d.attributes.insert("artist".into(), "Queen".into());
        d.attributes.insert("album".into(), album.into());
        d.attributes.insert("duration".into(), "PT5M55S".into());
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    // A tagged Spotify track MusicBrainz names only on the recording (its own URL lookup knows
    // nothing): the derived link's key joins it.
    let spotify = "https://open.spotify.com/track/2OBofMJx94NryV2SK8p8Zf";
    let spotify_id = store::ensure_resource(&pool, spotify).await.unwrap();
    let mut d = ResourceMeta::new(spotify, "jsonld", 86400);
    d.set_type("MusicRecording");
    store::save_document(&pool, &spotify_id, 0, &d)
        .await
        .unwrap();
    // Another host's isrcCode is a fact at most, never a key.
    let other = "https://example.org/bohemian-rhapsody";
    let other_id = store::ensure_resource(&pool, other).await.unwrap();
    let mut d = ResourceMeta::new(other, "jsonld", 86400);
    d.attributes.insert("isrc".into(), "GBUM71029604".into());
    store::save_document(&pool, &other_id, 0, &d).await.unwrap();
    // So is a music service's whose uploaders type the ISRC (Bandcamp, SoundCloud).
    let uploaded = "https://someone.bandcamp.com/track/bohemian-rhapsody";
    let uploaded_id = store::ensure_resource(&pool, uploaded).await.unwrap();
    let mut d = ResourceMeta::new(uploaded, "jsonld", 86400);
    d.set_type("MusicRecording");
    d.attributes.insert("isrc".into(), "GBUM71029604".into());
    store::save_document(&pool, &uploaded_id, 0, &d)
        .await
        .unwrap();
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let cx = also_on::Context {
        egress: egress(t.clone()),
        config: Arc::new(Config::default()),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let rows = |sql: &'static str, id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(sql)
                .bind(id)
                .fetch_all(&pool)
                .await
                .unwrap()
        }
    };
    let keys = "SELECT key||' '||source||' '||via FROM resource_key WHERE resource_id=$1 ORDER BY key,source";
    let joined = rows(keys, ids[1].clone()).await;
    assert!(
        joined.contains(&"isrc:GBUM71029604 service tidal".to_owned()),
        "{joined:?}"
    );
    assert!(
        joined.contains(
            &"mb:recording:b1a9c0e9-d987-4042-ae91-78d6a3267d69 musicbrainz musicbrainz".to_owned()
        ),
        "{joined:?}"
    );
    assert_eq!(
        rows(keys, ids[2].clone()).await,
        ["isrc:GBUM72103841 service tidal"]
    );
    assert!(rows(keys, other_id).await.is_empty());
    assert!(rows(keys, uploaded_id.clone()).await.is_empty());
    assert_eq!(
        rows(
            "SELECT name||' '||value FROM resource_fact WHERE resource_id=$1",
            uploaded_id.clone()
        )
        .await,
        ["isrc GBUM71029604"]
    );
    assert_eq!(
        rows(
            "SELECT name||' '||value FROM resource_fact WHERE resource_id=$1 AND source='service' ORDER BY name",
            ids[1].clone()
        )
        .await,
        [
            "album A Night At The Opera (Deluxe Edition)",
            "artist Queen",
            "duration 355",
            "isrc GBUM71029604"
        ]
    );
    let group = "SELECT coalesce(group_id,'-') FROM group_unit WHERE resource_id=$1";
    let seed = rows(group, ids[0].clone()).await;
    assert_ne!(seed, ["-"]);
    assert_eq!(rows(group, ids[1].clone()).await, seed);
    assert_eq!(rows(group, ids[2].clone()).await, ["-"]);
    assert_eq!(rows(group, spotify_id.clone()).await, seed);
    assert_eq!(rows(group, uploaded_id).await, ["-"]);
    assert_eq!(
        rows(
            "SELECT value FROM resource_fact WHERE resource_id=$1 AND name='original_album'",
            ids[0].clone()
        )
        .await,
        ["A Night at the Opera"]
    );
    // Both TIDAL links are tagged: a card shows the one on the original album.
    let card: serde_json::Value = sqlx::query_scalar("SELECT also_on($1,$1,8,true)")
        .bind(&spotify_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let tidal: Vec<&str> = card
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["uri"].as_str().unwrap().starts_with("https://tidal.com/"))
        .map(|e| e["uri"].as_str().unwrap())
        .collect();
    assert_eq!(tidal, ["https://tidal.com/track/534050211"], "{card}");
    // The recording's other links, found through the ISRC.
    let links = rows(
        "SELECT uri FROM derived_link WHERE resource_id=$1 ORDER BY uri",
        ids[1].clone(),
    )
    .await;
    assert!(
        links.contains(&"https://www.deezer.com/track/4329888262".to_owned()),
        "{links:?}"
    );
    let seen = t.seen.lock().unwrap().clone();
    // The five music links' URL lookups: one batched request, then each from the cache (the
    // unknown links' 404s too).
    let lookups: Vec<&String> = seen.iter().filter(|s| s.contains("/ws/2/url?")).collect();
    assert_eq!(lookups.len(), 1, "{lookups:?}");
    assert_eq!(lookups[0].matches("resource=").count(), 5, "{lookups:?}");
    assert!(
        seen.iter().any(|s| s.contains("/ws/2/isrc/GBUM71029604")),
        "{seen:?}"
    );
    // The seed's ISRC is on two recordings: not its key (its own URL relation joins it).
    assert!(
        seen.iter().any(|s| s.contains("/ws/2/isrc/GBCEE0100112")),
        "{seen:?}"
    );
    assert!(!rows(keys, ids[0].clone())
        .await
        .contains(&"isrc:GBCEE0100112 service tidal".to_owned()));
}

/// With Deezer's API on: a Deezer album's own barcode is a key, iTunes names the one Apple album
/// for it, and the tagged Apple link joins (Cut to the Feeling). A TIDAL track's own ISRC names
/// the one Deezer track. Off by default, nothing asks Deezer.
#[sqlx::test(migrations = "../../migrations")]
async fn a_deezer_barcode_names_the_apple_album_that_joins(pool: sqlx::PgPool) {
    let deezer = "https://www.deezer.com/album/41839661";
    let apple = "https://music.apple.com/us/album/1445304934";
    let tidal = "https://tidal.com/track/534050211";
    let mut ids = vec![];
    for (uri, kind) in [
        (deezer, "MusicAlbum"),
        (apple, "MusicAlbum"),
        (tidal, "MusicRecording"),
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "jsonld", 86400);
        d.set_type(kind);
        if uri == tidal {
            d.attributes.insert("isrc".into(), "GBUM71029604".into());
        }
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    let run = |config: Config| {
        let pool = pool.clone();
        async move {
            let t = Arc::new(Recorded {
                seen: Mutex::default(),
            });
            let cx = also_on::Context {
                egress: egress(t.clone()),
                config: Arc::new(config),
            };
            sqlx::query("INSERT INTO also_on_dirty(resource_id) SELECT id FROM resource ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
                .execute(&pool)
                .await
                .unwrap();
            also_on::drain(&pool, &cx, &also_on::registry(), 100)
                .await
                .unwrap();
            let seen = t.seen.lock().unwrap().clone();
            seen
        }
    };
    let seen = run(Config::default()).await;
    assert!(
        !seen.iter().any(|s| s.starts_with("api.deezer.com")),
        "{seen:?}"
    );
    let mut on = Config::default();
    on.also_on.sources.insert(
        "deezer".into(),
        also_on::SourceOptions {
            enabled: Some(true),
            ..Default::default()
        },
    );
    on.validate().unwrap();
    let seen = run(on).await;
    assert!(
        seen.contains(&"api.deezer.com/album/41839661?".to_owned()),
        "{seen:?}"
    );
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let rows = |sql: &'static str, id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(sql)
                .bind(id)
                .fetch_all(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        rows(
            "SELECT key||' '||source FROM resource_key WHERE resource_id=$1 ORDER BY key",
            ids[0].clone()
        )
        .await,
        ["upc:0602557656527 deezer"]
    );
    assert_eq!(
        rows(
            "SELECT uri||' '||via||' '||key FROM derived_link WHERE resource_id=$1 AND key IS NOT NULL",
            ids[0].clone()
        )
        .await,
        ["https://music.apple.com/us/album/1445304934 itunes upc:0602557656527"]
    );
    let group = "SELECT coalesce(group_id,'-') FROM group_unit WHERE resource_id=$1";
    let album = rows(group, ids[0].clone()).await;
    assert_ne!(album, ["-"]);
    assert_eq!(rows(group, ids[1].clone()).await, album);
    assert_eq!(
        rows(
            "SELECT uri||' '||via||' '||key FROM derived_link WHERE resource_id=$1 AND source='deezer'",
            ids[2].clone()
        )
        .await,
        ["https://www.deezer.com/track/3541552051 deezer isrc:GBUM71029604"]
    );
}

/// Two Wikidata item links in one batch: one read for both, each item's own read from the cache.
#[sqlx::test(migrations = "../../migrations")]
async fn item_links_are_read_in_one_request(pool: sqlx::PgPool) {
    for q in ["Q202996", "Q33328630"] {
        let uri = format!("https://www.wikidata.org/wiki/{q}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let d = ResourceMeta::new(&uri, "web", 86400);
        store::save_document(&pool, &id, 0, &d).await.unwrap();
    }
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let cx = also_on::Context {
        egress: egress(t.clone()),
        config: Arc::new(Config::default()),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    let seen = t.seen.lock().unwrap().clone();
    let reads: Vec<&String> = seen
        .iter()
        .filter(|s| s.contains("action=wbgetentities"))
        .collect();
    assert_eq!(reads.len(), 1, "{reads:?}");
    assert!(reads[0].contains("ids=Q202996%7CQ33328630"), "{reads:?}");
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM resource_key WHERE source='wikidata_item' ORDER BY key",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(keys, ["wikidata:Q202996", "wikidata:Q33328630"]);
}

/// A MusicBrainz page is its own hub: the recording's page carries its keys and lists its links
/// (keyed by it), so it joins the TIDAL track its ISRC names; the page of an ISRC one recording
/// has joins too; an ISRC two recordings have names neither (a fact, no key); the artist's page
/// joins the TIDAL artist.
#[sqlx::test(migrations = "../../migrations")]
async fn a_musicbrainz_page_is_its_own_hub(pool: sqlx::PgPool) {
    let recording = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    let single = "https://musicbrainz.org/isrc/GBUM71029604";
    let shared = "https://musicbrainz.org/isrc/GBCEE0100112";
    let artist = "https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711";
    let many = "https://musicbrainz.org/isrc/ZZAAA2600010";
    let merged = "https://musicbrainz.org/recording/00000000-0000-4000-8000-00000000000d";
    let tidal = "https://tidal.com/track/534050211";
    let tidal_artist = "https://tidal.com/artist/64518";
    let mut ids = std::collections::BTreeMap::new();
    for (uri, ty) in [
        (recording, "MusicRecording"),
        (single, "MusicRecording"),
        (shared, "MusicRecording"),
        (many, "MusicRecording"),
        (merged, "MusicRecording"),
        (artist, "MusicGroup"),
        (tidal, "MusicRecording"),
        (tidal_artist, "MusicGroup"),
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.set_type(ty);
        if uri == tidal {
            // TIDAL's own page states its ISRC (a key).
            d.attributes.insert("isrc".into(), "GBUM71029604".into());
        }
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.insert(uri, id);
    }
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let cx = also_on::Context {
        egress: egress(t.clone()),
        config: Arc::new(Config::default()),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let rows = |sql: &'static str, id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(sql)
                .bind(id)
                .fetch_all(&pool)
                .await
                .unwrap()
        }
    };
    let keys = "SELECT key FROM resource_key WHERE resource_id=$1 AND source='musicbrainz_entity' ORDER BY key";
    let mbid = "mb:recording:b1a9c0e9-d987-4042-ae91-78d6a3267d69".to_owned();
    assert!(rows(keys, ids[recording].clone()).await.contains(&mbid));
    let isrc_keys = rows(keys, ids[single].clone()).await;
    assert!(
        isrc_keys.contains(&mbid) && isrc_keys.contains(&"isrc:GBUM71029604".to_owned()),
        "{isrc_keys:?}"
    );
    assert!(rows(keys, ids[shared].clone()).await.is_empty());
    // A page whose MBID was merged into the recording keys the recording, keeps the old MBID as a
    // fact, joins, and is found by the old MBID.
    assert!(rows(keys, ids[merged].clone()).await.contains(&mbid));
    assert_eq!(
        rows(
            "SELECT value FROM resource_fact WHERE resource_id=$1 AND name='mbid_was'",
            ids[merged].clone()
        )
        .await,
        ["00000000-0000-4000-8000-00000000000d"]
    );
    {
        use tower::ServiceExt;
        // Search lists tagged links only.
        sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) VALUES($1,repeat('a',52),'tag','/pub/test/tags/'||$1,'music',now())")
            .bind(&ids[merged])
            .execute(&pool)
            .await
            .unwrap();
        let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
        let mut request = axum::http::Request::builder()
            .uri("/v1/search?key=mbid:00000000-0000-4000-8000-00000000000d")
            .body(axum::body::Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
        ));
        let response = app.oneshot(request).await.unwrap();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let found: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let hits: Vec<&str> = found["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(hits, [ids[merged].as_str()], "{found}");
    }
    // An ISRC page keys its own ISRC even when its recording has more than the eight that fit.
    assert!(rows(keys, ids[many].clone())
        .await
        .contains(&"isrc:ZZAAA2600010".to_owned()));
    assert_eq!(
        rows(
            "SELECT name||' '||value FROM resource_fact WHERE resource_id=$1",
            ids[shared].clone()
        )
        .await,
        ["isrc GBCEE0100112"]
    );
    // The recording's links on services, keyed by it, and its facts.
    let links = rows(
        "SELECT uri||' '||coalesce(key,'-') FROM derived_link WHERE resource_id=$1 ORDER BY uri",
        ids[recording].clone(),
    )
    .await;
    assert!(
        links.contains(&format!("https://www.deezer.com/track/4329888262 {mbid}")),
        "{links:?}"
    );
    let facts = rows(
        "SELECT name FROM resource_fact WHERE resource_id=$1 AND source='musicbrainz_entity' ORDER BY name",
        ids[recording].clone(),
    )
    .await;
    for name in ["artist", "duration", "isrc", "mbid", "original_album"] {
        assert!(facts.contains(&name.to_owned()), "{name}: {facts:?}");
    }
    // One group: the recording's page, the single ISRC's page and the TIDAL track.
    let group = "SELECT coalesce(group_id,'-') FROM group_unit WHERE resource_id=$1";
    let song = rows(group, ids[recording].clone()).await;
    assert_ne!(song, ["-"]);
    assert_eq!(rows(group, ids[single].clone()).await, song);
    assert_eq!(rows(group, ids[merged].clone()).await, song);
    assert_eq!(rows(group, ids[tidal].clone()).await, song);
    assert_ne!(rows(group, ids[shared].clone()).await, song);
    // The artist's page joins the TIDAL artist MusicBrainz lists.
    let band = rows(group, ids[artist].clone()).await;
    assert_ne!(band, ["-"]);
    assert_eq!(rows(group, ids[tidal_artist].clone()).await, band);
    // One read of the recording serves its page and the ISRC's.
    let seen = t.seen.lock().unwrap().clone();
    let reads = seen
        .iter()
        .filter(|s| s.contains("/ws/2/recording/b1a9c0e9"))
        .count();
    assert_eq!(reads, 1, "{seen:?}");
}

/// A MusicBrainz page's card comes from the entity MusicBrainz serves, the same read the hub's
/// source makes: the recording's title, artist credit and length; an artist's type; an ISRC's
/// page is its one recording's card, or the ISRC's own when two recordings have it.
#[tokio::test]
async fn a_musicbrainz_page_card_comes_from_the_entity() {
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let web = pinakes_core::resolver::web::WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        events: None,
        egress: egress(t.clone()),
        config: Default::default(),
    };
    let card = |uri: &'static str| {
        let web = &web;
        async move {
            web.resolve_with_policy(uri, Default::default())
                .await
                .unwrap()
        }
    };
    let song = card("https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69").await;
    assert_eq!(
        (
            song.type_name.as_str(),
            song.name.as_deref(),
            song.site.as_str()
        ),
        ("MusicRecording", Some("Bohemian Rhapsody"), "MusicBrainz")
    );
    assert_eq!(song.description.as_deref(), Some("Queen · 5:55"));
    let band = card("https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711").await;
    assert_eq!(
        (band.type_name.as_str(), band.name.as_deref()),
        ("MusicGroup", Some("Radiohead"))
    );
    let isrc = card("https://musicbrainz.org/isrc/GBUM71029604").await;
    assert_eq!(isrc.name.as_deref(), Some("Bohemian Rhapsody"));
    assert_eq!(isrc.url, "https://musicbrainz.org/isrc/GBUM71029604");
    let shared = card("https://musicbrainz.org/isrc/GBCEE0100112").await;
    assert_eq!(shared.name.as_deref(), Some("ISRC GBCEE0100112"));
    // No page was read: MusicBrainz's web service only.
    let seen = t.seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|s| s.starts_with("musicbrainz.org/ws/2/")),
        "{seen:?}"
    );
}

/// An answer over its hub's cap is no answer: a MusicBrainz page's, a Wikidata item's and a
/// Wikipedia article's sources record only the key their own service proves and don't retry (asking again would read the same
/// megabytes), and the MusicBrainz card is read from the page, the second time too, from the
/// failure cache.
#[sqlx::test(migrations = "../../migrations")]
async fn an_entity_over_the_cap_is_no_answer(pool: sqlx::PgPool) {
    let uri = format!("https://musicbrainz.org/artist/{BIG_BAND}");
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
    });
    let e = egress(t.clone());
    let web = pinakes_core::resolver::web::WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        events: None,
        egress: e.clone(),
        config: Default::default(),
    };
    for _ in 0..2 {
        let card = web
            .resolve_with_policy(&uri, Default::default())
            .await
            .expect("the page is read instead");
        assert_eq!(card.name.as_deref(), Some("Big Band"));
    }
    let item = "https://www.wikidata.org/wiki/Q999999999";
    let article = "https://en.wikipedia.org/wiki/Big_Country";
    let mut ids = vec![];
    for page in [uri.as_str(), item, article] {
        let id = store::ensure_resource(&pool, page).await.unwrap();
        let mut d = ResourceMeta::new(page, "web", 86400);
        d.set_type("MusicGroup");
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    let cx = also_on::Context {
        egress: e,
        config: Arc::new(Config::default()),
    };
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    for (id, own) in [
        (&ids[0], format!("mb:artist:{BIG_BAND}")),
        (&ids[1], "wikidata:Q999999999".to_owned()),
        // The article's own Wikipedia named the item: that key stays.
        (&ids[2], "wikidata:Q999999999".to_owned()),
    ] {
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT key FROM resource_key WHERE resource_id=$1 ORDER BY key")
                .bind(id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(keys, [own]);
        let retrying: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM also_on_dirty WHERE resource_id=$1 AND attempts>0",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retrying, 0);
    }
    // The record was asked once: the card's second read and the source's came from the cache.
    let seen = t.seen.lock().unwrap().clone();
    let asked = seen
        .iter()
        .filter(|s| s.starts_with("musicbrainz.org/ws/2/artist/"))
        .count();
    assert_eq!(asked, 1, "{seen:?}");
}
