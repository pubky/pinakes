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
                "https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE" => {
                    (200, fixture("mb-url-spotify-album.json"))
                }
                _ => (404, fixture("mb-url-unknown.json")),
            },
            "/ws/2/recording/9f9cf187-d6f9-437f-9d98-d59cdbd52757" => {
                (200, fixture("mb-recording.json"))
            }
            "/ws/2/release/30702389-5c67-4438-9ea0-2351c8de0f1d" => {
                (200, fixture("mb-release.json"))
            }
            "/lookup" if query("upc") == "634904078164" => (200, fixture("itunes-upc.json")),
            // A Wikipedia's own API: the article's Wikidata item.
            "/w/api.php" if url.host_str() != Some("www.wikidata.org") => {
                match (url.host_str().unwrap_or(""), query("titles").as_str()) {
                    ("en.wikipedia.org", "Kant") => (200, fixture("enwiki-kant-redirect.json")),
                    ("en.wikipedia.org", "OK_Computer") => (200, fixture("enwiki-ok-computer.json")),
                    ("de.wikipedia.org", "Immanuel_Kant") => (
                        200,
                        br#"{"query":{"pages":{"1":{"pageid":1,"ns":0,"title":"Immanuel Kant","pageprops":{"wikibase_item":"Q9312"}}}}}"#.to_vec(),
                    ),
                    _ => (200, fixture("enwiki-missing.json")),
                }
            }
            "/w/api.php" if query("list") == "search" => (200, fixture("wd-search-p436.json")),
            "/w/api.php" if query("action") == "wbgetentities" && query("ids") == "Q9312" => {
                (200, fixture("wd-kant.json"))
            }
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
            "mb:release:30702389-5c67-4438-9ea0-2351c8de0f1d musicbrainz"
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
    let also_on = card["also_on"].as_array().unwrap();
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
    let itunes: String =
        sqlx::query_scalar("SELECT uri FROM derived_link WHERE resource_id=$1 AND via='itunes'")
            .bind(&ids[0])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(itunes, "https://music.apple.com/us/album/1097861387");
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
