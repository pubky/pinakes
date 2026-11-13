//! The `spotify` also-on source through Egress, against Spotify answers written by hand in the
//! Web API's documented shape (`/v1/albums/{id}`, `/v1/tracks/{id}`: the members this source
//! reads) and recorded iTunes, Deezer and MusicBrainz answers (`tests/fixtures/hubs`). Spotify
//! refused to answer the owner's app when these tests were written (403: "Active premium
//! subscription required for the owner of the app"), so no Spotify answer here is a recording;
//! the album's ID is made up, its barcode HYDRA 3D's (as in the TIDAL tests).
use pinakes_core::{
    also_on::{self, SourceOptions},
    config::Config,
    groups,
    model::ResourceMeta,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
    store,
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
mod common;

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/hubs/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// A made-up Spotify album ID (22 base-62 characters) with HYDRA 3D's barcode as a UPC-A.
const ALBUM: &str = "0hydra3DexampleAlbum01";
/// "Bohemian Rhapsody - Remastered 2011", the track REPORT-matching names.
const TRACK: &str = "7tFiyTwD0nx5a1eklYtX2J";
/// `GET /v1/albums/{ALBUM}`, the members read: `id`, `type`, `external_ids.upc`.
fn album_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "album_type": "album", "id": ALBUM, "type": "album", "name": "HYDRA 3D",
        "external_ids": {"upc": "4018939306052"}, "total_tracks": 1,
        "uri": format!("spotify:album:{ALBUM}"),
    }))
    .unwrap()
}
/// Two made-up tracks with an ISRC MusicBrainz gives to two recordings (3:33 and 2:59, as in
/// the hubs tests): one 3:34 long, which picks the first, and one 3:16, which picks neither.
const SHARED_PICKED: &str = "1sharedIsrcTrack214sec";
const SHARED_NEITHER: &str = "1sharedIsrcTrack196sec";
fn shared_track_body(id: &str, ms: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id, "type": "track", "name": "Never Gonna Give You Up", "duration_ms": ms,
        "external_ids": {"isrc": "GBARL9300135"},
    }))
    .unwrap()
}
/// A made-up album the barcode search lists beside HYDRA 3D's, whose own record carries
/// Underclass Hero's barcode.
const OTHER_UPC_ALBUM: &str = "0otherUpcAlbum00000000";
/// A made-up album a barcode search lists, which Spotify has no album for (404).
const MISSING_UPC_ALBUM: &str = "0missingUpcAlbum000000";
/// A made-up album that carries Random Access Memories' barcode, listed by a search that found
/// more albums than it lists.
const PAGED_UPC_ALBUM: &str = "0ramUpcAlbum0000000000";
/// A made-up track the ISRC search lists beside Bohemian Rhapsody's, with another ISRC.
const OTHER_ISRC_TRACK: &str = "0otherIsrcTrack0000000";
/// `GET /v1/tracks/{TRACK}`: `id`, `type`, `external_ids.isrc` and `duration_ms` (5:54).
fn track_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": TRACK, "type": "track", "name": "Bohemian Rhapsody - Remastered 2011",
        "duration_ms": 354_320, "disc_number": 1, "track_number": 11,
        "external_ids": {"isrc": "GBUM71029604"},
        "artists": [{"name": "Queen", "type": "artist"}],
        "uri": format!("spotify:track:{TRACK}"),
    }))
    .unwrap()
}

/// What Spotify answers instead: a refusal at the token endpoint, or a status from the API (with
/// the body Spotify sends when the app owner has no Premium subscription for a 403).
#[derive(Clone, Default)]
struct Answers {
    refuse: bool,
    api_status: Option<u16>,
    /// A status for one album's read (`/v1/albums/{id}`) alone.
    album_status: Option<u16>,
}
/// Answers Spotify (its token and API), MusicBrainz's ISRC lookup, iTunes and Deezer; records
/// each request.
struct Recorded {
    seen: Mutex<Vec<String>>,
    answers: Answers,
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
        let host = url.host_str().unwrap_or("");
        self.seen.lock().unwrap().push(format!(
            "{host}{}?{}",
            url.path(),
            url.query().unwrap_or("")
        ));
        let header = |name: &str| {
            options
                .headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        };
        let answer = |status: u16, body: Vec<u8>, content_type: &str| Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status,
            retry_after_secs: 120,
            location: None,
            content_type: content_type.into(),
            body,
        };
        Ok(match (host, url.path()) {
            ("accounts.spotify.com", "/api/token") => {
                assert!(options.sensitive);
                assert_eq!(
                    options.form_body,
                    Some(vec![("grant_type", "client_credentials".to_owned())])
                );
                assert!(header("authorization").unwrap().starts_with("Basic "));
                if self.answers.refuse {
                    answer(
                        400,
                        br#"{"error":"invalid_client","error_description":"Invalid client"}"#
                            .to_vec(),
                        "application/json",
                    )
                } else {
                    answer(
                        200,
                        br#"{"access_token":"written-token","token_type":"Bearer","expires_in":3600}"#
                            .to_vec(),
                        "application/json",
                    )
                }
            }
            ("api.spotify.com", path) => {
                assert!(options.sensitive);
                assert_eq!(
                    header("authorization").as_deref(),
                    Some("Bearer written-token")
                );
                if let Some(status) = self.answers.api_status {
                    return Ok(answer(
                        status,
                        b"Active premium subscription required for the owner of the app.".to_vec(),
                        "text/plain",
                    ));
                }
                if let Some(status) = self
                    .answers
                    .album_status
                    .filter(|_| path.starts_with("/v1/albums/"))
                {
                    return Ok(answer(
                        status,
                        br#"{"error":{"status":403,"message":"Forbidden"}}"#.to_vec(),
                        "application/json",
                    ));
                }
                if path == format!("/v1/albums/{ALBUM}") {
                    answer(200, album_body(), "application/json; charset=utf-8")
                } else if path == format!("/v1/albums/{OTHER_UPC_ALBUM}") {
                    answer(
                        200,
                        serde_json::to_vec(&serde_json::json!({
                            "album_type": "album", "id": OTHER_UPC_ALBUM, "type": "album",
                            "name": "Underclass Hero", "external_ids": {"upc": "602517416987"},
                        }))
                        .unwrap(),
                        "application/json; charset=utf-8",
                    )
                } else if path == format!("/v1/tracks/{TRACK}") {
                    answer(200, track_body(), "application/json; charset=utf-8")
                } else if path == format!("/v1/tracks/{SHARED_PICKED}") {
                    answer(200, shared_track_body(SHARED_PICKED, 214_000), "application/json")
                } else if path == format!("/v1/tracks/{SHARED_NEITHER}") {
                    answer(200, shared_track_body(SHARED_NEITHER, 196_000), "application/json")
                } else if path == "/v1/search" {
                    let q = url
                        .query_pairs()
                        .find(|(k, _)| k == "q")
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default();
                    let kind = url
                        .query_pairs()
                        .find(|(k, _)| k == "type")
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default();
                    // The barcode search (`q=upc:…&type=album`), whose albums carry no barcode:
                    // HYDRA 3D's UPC-A lists its album and a made-up one the read-back drops;
                    // Underclass Hero's lists the album that carries it and one Spotify has no
                    // record of (so no link); Random Access Memories' lists one album that carries
                    // it, of 4 found (a next page), which the source refuses unread.
                    if kind == "album" {
                        let albums = if q == "upc:4018939306052" {
                            serde_json::json!({"items": [
                                {"album_type": "album", "id": ALBUM, "type": "album", "name": "HYDRA 3D"},
                                {"album_type": "album", "id": OTHER_UPC_ALBUM, "type": "album", "name": "HYDRA 3D"},
                            ], "limit": 3, "offset": 0, "total": 2, "next": null})
                        } else if q == "upc:602517416987" {
                            serde_json::json!({"items": [
                                {"album_type": "album", "id": OTHER_UPC_ALBUM, "type": "album", "name": "Underclass Hero"},
                                {"album_type": "album", "id": MISSING_UPC_ALBUM, "type": "album", "name": "Underclass Hero"},
                            ], "limit": 3, "offset": 0, "total": 2, "next": null})
                        } else if q == "upc:886443927087" {
                            serde_json::json!({"items": [
                                {"album_type": "album", "id": PAGED_UPC_ALBUM, "type": "album", "name": "Random Access Memories"},
                            ], "limit": 3, "offset": 0, "total": 4,
                               "next": "https://api.spotify.com/v1/search?offset=3&limit=3&query=upc%3A886443927087&type=album"})
                        } else {
                            serde_json::json!({"items": [], "limit": 3, "offset": 0, "total": 0, "next": null})
                        };
                        return Ok(answer(
                            200,
                            serde_json::to_vec(&serde_json::json!({ "albums": albums })).unwrap(),
                            "application/json; charset=utf-8",
                        ));
                    }
                    // The ISRC search (`q=isrc:…&type=track`): Bohemian Rhapsody's ISRC answers
                    // its track and a track of another ISRC (never Spotify's answer in truth: the
                    // filter is exact, but the source checks); any other ISRC, nothing.
                    assert_eq!(kind, "track");
                    let items = if q == "isrc:GBUM71029604" {
                        serde_json::json!([
                            serde_json::from_slice::<serde_json::Value>(&track_body()).unwrap(),
                            {"id": OTHER_ISRC_TRACK, "type": "track", "name": "Another Song",
                             "duration_ms": 200_000, "external_ids": {"isrc": "GBUM71029605"}},
                        ])
                    } else {
                        serde_json::json!([])
                    };
                    answer(
                        200,
                        serde_json::to_vec(&serde_json::json!({"tracks": {"items": items, "limit": 10, "offset": 0}})).unwrap(),
                        "application/json; charset=utf-8",
                    )
                } else {
                    answer(
                        404,
                        br#"{"error":{"status":404,"message":"Resource not found"}}"#.to_vec(),
                        "application/json",
                    )
                }
            }
            ("musicbrainz.org", "/ws/2/isrc/GBARL9300135") => answer(
                200,
                br#"{"isrc":"GBARL9300135","recordings":[{"id":"8f3471b5-7e6a-48da-86a9-c1c07a0f47ae","length":212946,"title":"Never Gonna Give You Up"},{"id":"e00ebb77-541e-41f3-86ba-a90530414922","length":178933,"title":"Never Gonna Give You Up"}]}"#.to_vec(),
                "application/json",
            ),
            ("musicbrainz.org", "/ws/2/isrc/GBUM71029604") => answer(
                200,
                fixture("mb-isrc-GBUM71029604.json"),
                "application/json",
            ),
            // The two recordings of the shared ISRC, read to see whether they agree: the album
            // version and an edit 34 s shorter do not.
            ("musicbrainz.org", "/ws/2/recording/8f3471b5-7e6a-48da-86a9-c1c07a0f47ae")
            | ("musicbrainz.org", "/ws/2/recording/e00ebb77-541e-41f3-86ba-a90530414922") => {
                let album = url.path().ends_with("8f3471b5-7e6a-48da-86a9-c1c07a0f47ae");
                answer(
                    200,
                    serde_json::to_vec(&serde_json::json!({
                        "id": url.path().trim_start_matches("/ws/2/recording/"),
                        "title": "Never Gonna Give You Up",
                        "length": if album { 212946 } else { 178933 },
                        "isrcs": ["GBARL9300135"],
                        "artist-credit": [{"name": "Rick Astley", "joinphrase": "",
                            "artist": {"id": "db92a151-1ac2-438b-bc43-b82e149ddd50", "name": "Rick Astley"}}],
                        "relations": [],
                    }))
                    .unwrap(),
                    "application/json",
                )
            }
            ("itunes.apple.com", "/lookup") => {
                let upc = url
                    .query_pairs()
                    .find(|(k, _)| k == "upc")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let body = match upc.as_str() {
                    "4018939306052" => fixture("itunes-upc-4018939306052.json"),
                    _ => br#"{"resultCount":0,"results":[]}"#.to_vec(),
                };
                answer(200, body, "text/javascript")
            }
            ("api.deezer.com", path) => {
                let name = format!(
                    "deezer-{}.json",
                    path.trim_start_matches('/').replace(['/', ':'], "-")
                );
                let recorded = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
                let none = br#"{"error":{"type":"DataException","message":"no data","code":800}}"#;
                answer(
                    200,
                    std::fs::read(recorded).unwrap_or_else(|_| none.to_vec()),
                    "application/json",
                )
            }
            _ => panic!("unexpected request {url}"),
        })
    }
}

/// Spotify's pair (a different one per test: tokens are kept per pair), Deezer on, and only the
/// sources this test reads.
fn config(pair: Option<(&str, &str)>) -> Config {
    let mut config = Config::default();
    if let Some((id, secret)) = pair {
        config.sections.set(
            "spotify",
            serde_json::json!({"client_id": id, "client_secret": secret}),
        );
    }
    // Only the sources this build has: a switch for one it lacks is refused at validation.
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    for (source, enabled) in [
        ("deezer", true),
        ("spotify_isrc", true),
        ("musicbrainz", false),
        ("musicbrainz_entity", false),
        ("wikidata", false),
    ] {
        if !registered.contains(&source) {
            continue;
        }
        config.also_on.sources.insert(
            source.into(),
            SourceOptions {
                enabled: Some(enabled),
                ..Default::default()
            },
        );
    }
    config.validate().unwrap();
    config
}
async fn save(pool: &sqlx::PgPool, uri: &str, kind: &str) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type(kind);
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    id
}
/// Every resource's sources, once, on a fresh egress; the requests made.
async fn run(pool: &sqlx::PgPool, config: Config, answers: Answers) -> Vec<String> {
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
        answers,
    });
    let egress = Egress::with_backends(
        common::unpaced_egress_config(),
        Arc::new(PublicDns),
        t.clone(),
    );
    let cx = also_on::Context {
        egress,
        config: Arc::new(config),
    };
    sqlx::query("INSERT INTO also_on_dirty(resource_id) SELECT id FROM resource ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
        .execute(pool)
        .await
        .unwrap();
    also_on::drain(pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    let seen = t.seen.lock().unwrap().clone();
    seen
}
async fn rows(pool: &sqlx::PgPool, sql: &str, id: &str) -> Vec<String> {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}
const KEYS: &str =
    "SELECT key||' '||source FROM resource_key WHERE resource_id=$1 ORDER BY key COLLATE \"C\"";
fn spotify_requests(seen: &[String]) -> Vec<&String> {
    seen.iter().filter(|s| s.contains("spotify.com")).collect()
}

/// A Spotify album's barcode joins Apple Music and Deezer; a track's ISRC, which MusicBrainz
/// gives to one recording, is its key.
#[sqlx::test(migrations = "../../migrations")]
async fn a_spotify_barcode_joins_apple_music_and_deezer(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !["deezer", "itunes"]
        .iter()
        .all(|id| registered.contains(id))
    {
        return;
    }
    let album = save(
        &pool,
        &format!("https://open.spotify.com/album/{ALBUM}"),
        "MusicAlbum",
    )
    .await;
    let track = save(
        &pool,
        &format!("https://open.spotify.com/track/{TRACK}"),
        "MusicRecording",
    )
    .await;
    let apple = save(
        &pool,
        "https://music.apple.com/album/1680053261",
        "MusicAlbum",
    )
    .await;
    let seen = run(
        &pool,
        config(Some(("join-id", "join-secret"))),
        Answers::default(),
    )
    .await;
    // One token, then one request per album or track.
    let mut asked: Vec<String> = spotify_requests(&seen).into_iter().cloned().collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            "accounts.spotify.com/api/token?".to_owned(),
            format!("api.spotify.com/v1/albums/{ALBUM}?"),
            format!("api.spotify.com/v1/tracks/{TRACK}?"),
        ],
        "{seen:?}"
    );
    assert_eq!(
        rows(&pool, KEYS, &album).await,
        ["upc:4018939306052 spotify"]
    );
    assert!(
        rows(&pool, KEYS, &track)
            .await
            .contains(&"isrc:GBUM71029604 spotify".to_owned()),
        "{seen:?}"
    );
    assert_eq!(
        rows(
            &pool,
            "SELECT name||' '||value FROM resource_fact WHERE resource_id=$1 AND source='spotify'",
            &track
        )
        .await,
        ["isrc GBUM71029604"]
    );
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let group = "SELECT coalesce(group_id,'-') FROM group_unit WHERE resource_id=$1";
    let joined = rows(&pool, group, &album).await;
    assert_ne!(joined, ["-"]);
    assert_eq!(rows(&pool, group, &apple).await, joined);
}

#[sqlx::test(migrations = "../../migrations")]
async fn without_credentials_nothing_asks_spotify(pool: sqlx::PgPool) {
    let album = save(
        &pool,
        &format!("https://open.spotify.com/album/{ALBUM}"),
        "MusicAlbum",
    )
    .await;
    let seen = run(&pool, config(None), Answers::default()).await;
    assert!(spotify_requests(&seen).is_empty(), "{seen:?}");
    assert!(rows(&pool, KEYS, &album).await.is_empty());
}

/// Credentials the token endpoint refuses, and an app the API refuses (403, as for an owner
/// without Premium): a refusal of the credentials, retried after the hold, asking Spotify
/// nothing meanwhile; what Spotify gave before stays.
#[sqlx::test(migrations = "../../migrations")]
async fn refused_credentials_keep_what_spotify_gave_and_retry_later(pool: sqlx::PgPool) {
    let album = save(
        &pool,
        &format!("https://open.spotify.com/album/{ALBUM}"),
        "MusicAlbum",
    )
    .await;
    run(
        &pool,
        config(Some(("first-id", "first-secret"))),
        Answers::default(),
    )
    .await;
    assert_eq!(
        rows(&pool, KEYS, &album).await,
        ["upc:4018939306052 spotify"]
    );
    let queued = || async {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT last_error FROM also_on_dirty WHERE resource_id=$1",
        )
        .bind(&album)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    for (pair, answers, asked) in [
        (
            ("refused-id", "refused-secret"),
            Answers {
                refuse: true,
                ..Default::default()
            },
            vec!["accounts.spotify.com/api/token?".to_owned()],
        ),
        (
            ("premium-id", "premium-secret"),
            Answers {
                api_status: Some(403),
                ..Default::default()
            },
            vec![
                "accounts.spotify.com/api/token?".to_owned(),
                format!("api.spotify.com/v1/albums/{ALBUM}?"),
            ],
        ),
    ] {
        let seen = run(&pool, config(Some(pair)), answers.clone()).await;
        assert_eq!(
            spotify_requests(&seen),
            asked.iter().collect::<Vec<_>>(),
            "{seen:?}"
        );
        assert_eq!(
            rows(&pool, KEYS, &album).await,
            ["upc:4018939306052 spotify"]
        );
        assert_eq!(queued().await.as_deref(), Some("spotify_auth"));
        // The refusal is kept five minutes: the retry asks Spotify nothing.
        let seen = run(&pool, config(Some(pair)), answers).await;
        assert!(spotify_requests(&seen).is_empty(), "{seen:?}");
    }
}

/// An ISRC MusicBrainz gives to several recordings is never Spotify's key: the recording the
/// track's length picks is (its MBID), else nothing; the ISRC is a fact either way.
#[sqlx::test(migrations = "../../migrations")]
async fn a_shared_isrc_is_never_a_spotify_key(pool: sqlx::PgPool) {
    let picked = save(
        &pool,
        &format!("https://open.spotify.com/track/{SHARED_PICKED}"),
        "MusicRecording",
    )
    .await;
    let neither = save(
        &pool,
        &format!("https://open.spotify.com/track/{SHARED_NEITHER}"),
        "MusicRecording",
    )
    .await;
    run(
        &pool,
        config(Some(("shared-id", "shared-secret"))),
        Answers::default(),
    )
    .await;
    let spotify_keys =
        "SELECT key FROM resource_key WHERE resource_id=$1 AND source='spotify' ORDER BY key";
    assert_eq!(
        rows(&pool, spotify_keys, &picked).await,
        ["mb:recording:8f3471b5-7e6a-48da-86a9-c1c07a0f47ae"]
    );
    assert!(rows(&pool, spotify_keys, &neither).await.is_empty());
    let any_isrc_key = "SELECT key FROM resource_key WHERE resource_id=$1 AND key LIKE 'isrc:%'";
    let facts =
        "SELECT name||' '||value FROM resource_fact WHERE resource_id=$1 AND source='spotify'";
    for id in [&picked, &neither] {
        assert!(rows(&pool, any_isrc_key, id).await.is_empty());
        assert_eq!(rows(&pool, facts, id).await, ["isrc GBARL9300135"]);
    }
}

/// A track on another service with an ISRC key is looked up on Spotify by it (`spotify_isrc`):
/// the Spotify tracks carrying exactly that ISRC become links keyed by the ISRC, a track the
/// search lists with another ISRC is left out, Spotify's own tracks are not asked, a track with
/// no ISRC key asks nothing, and the switch turns it off.
#[sqlx::test(migrations = "../../migrations")]
async fn a_track_with_an_isrc_key_gets_its_spotify_link(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"spotify_isrc") {
        return;
    }
    // TIDAL's own page states its ISRC (a key: TIDAL vouches for it) and its length.
    let tidal = "https://tidal.com/track/534050211";
    let tidal_id = store::ensure_resource(&pool, tidal).await.unwrap();
    let mut doc = ResourceMeta::new(tidal, "jsonld", 86400);
    doc.set_type("MusicRecording");
    doc.attributes.insert("isrc".into(), "GBUM71029604".into());
    doc.attributes.insert("duration".into(), "PT5M55S".into());
    store::save_document(&pool, &tidal_id, 0, &doc)
        .await
        .unwrap();
    // A TIDAL page without an ISRC, and Spotify's own track.
    let plain = save(&pool, "https://tidal.com/track/551040646", "MusicRecording").await;
    let own = save(
        &pool,
        &format!("https://open.spotify.com/track/{TRACK}"),
        "MusicRecording",
    )
    .await;
    let seen = run(
        &pool,
        config(Some(("isrc-id", "isrc-secret"))),
        Answers::default(),
    )
    .await;
    let searches: Vec<&String> = seen
        .iter()
        .filter(|s| s.starts_with("api.spotify.com/v1/search?"))
        .collect();
    assert_eq!(
        searches,
        ["api.spotify.com/v1/search?q=isrc%3AGBUM71029604&type=track&limit=10"],
        "{seen:?}"
    );
    let links = rows(
        &pool,
        "SELECT uri||' '||coalesce(key,'-')||' '||source||' '||via FROM derived_link WHERE resource_id=$1 AND source='spotify_isrc' ORDER BY uri",
        &tidal_id,
    )
    .await;
    assert_eq!(
        links,
        [format!(
            "https://open.spotify.com/track/{TRACK} isrc:GBUM71029604 spotify_isrc spotify_isrc"
        )]
    );
    assert!(!links.iter().any(|l| l.contains(OTHER_ISRC_TRACK)));
    for id in [&plain, &own] {
        assert!(rows(
            &pool,
            "SELECT uri FROM derived_link WHERE resource_id=$1 AND source='spotify_isrc'",
            id
        )
        .await
        .is_empty());
    }
    // Switched off: nothing asks the search, and the rows it found stop showing.
    let mut off = config(Some(("isrc-id", "isrc-secret")));
    off.also_on.sources.insert(
        "spotify_isrc".into(),
        SourceOptions {
            enabled: Some(false),
            ..Default::default()
        },
    );
    off.validate().unwrap();
    sqlx::query("DELETE FROM derived_link WHERE source='spotify_isrc'")
        .execute(&pool)
        .await
        .unwrap();
    let seen = run(&pool, off, Answers::default()).await;
    assert!(!seen.iter().any(|s| s.contains("/v1/search?")), "{seen:?}");
}

/// `spotify_isrc` is on by default: with the Spotify pair set and no switch, it asks the search.
/// A refusal (403) and a rate limit (429) keep the links an earlier run found; switched off, those
/// links stop showing on the card while their rows stay.
#[sqlx::test(migrations = "../../migrations")]
async fn spotify_isrc_is_on_by_default_and_keeps_its_rows_through_refusals(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"spotify_isrc") {
        return;
    }
    let tidal = "https://tidal.com/track/534050211";
    let tidal_id = store::ensure_resource(&pool, tidal).await.unwrap();
    let mut doc = ResourceMeta::new(tidal, "jsonld", 86400);
    doc.set_type("MusicRecording");
    doc.attributes.insert("isrc".into(), "GBUM71029604".into());
    doc.attributes.insert("duration".into(), "PT5M55S".into());
    store::save_document(&pool, &tidal_id, 0, &doc)
        .await
        .unwrap();
    // On by default: the pair alone turns it on.
    let mut by_default = config(Some(("isrc-default-id", "isrc-default-secret")));
    by_default.also_on.sources.remove("spotify_isrc");
    by_default.validate().unwrap();
    let seen = run(&pool, by_default, Answers::default()).await;
    assert!(seen.iter().any(|s| s.contains("/v1/search?")), "{seen:?}");
    let links = |pool: sqlx::PgPool, id: String| async move {
        sqlx::query_scalar::<_, String>(
            "SELECT uri FROM derived_link WHERE resource_id=$1 AND source='spotify_isrc' ORDER BY uri",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    // The test config (switched on explicitly) finds the same.
    let on = config(Some(("isrc-default-id", "isrc-default-secret")));
    run(&pool, on.clone(), Answers::default()).await;
    let expected = [format!("https://open.spotify.com/track/{TRACK}")];
    assert_eq!(links(pool.clone(), tidal_id.clone()).await, expected);
    // A refusal and a rate limit keep it (each with a pair of its own: a refusal holds the
    // pair's slot for five minutes, a rate limit for the time asked).
    for (status, pair) in [
        (403, ("isrc-refused-id", "isrc-refused-secret")),
        (429, ("isrc-busy-id", "isrc-busy-secret")),
    ] {
        let seen = run(
            &pool,
            config(Some(pair)),
            Answers {
                api_status: Some(status),
                ..Default::default()
            },
        )
        .await;
        assert!(
            seen.iter().any(|s| s.contains("spotify.com")),
            "{status}: {seen:?}"
        );
        assert_eq!(
            links(pool.clone(), tidal_id.clone()).await,
            expected,
            "{status}"
        );
    }
    // Switched off: the rows stay, the card no longer shows them.
    let card = |pool: sqlx::PgPool, id: String| async move {
        let card: serde_json::Value = sqlx::query_scalar("SELECT also_on($1,$1,8,true)")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        card.as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["uri"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    also_on::configure(&pool, &on).await.unwrap();
    assert!(
        card(pool.clone(), tidal_id.clone())
            .await
            .contains(&expected[0]),
        "{:?}",
        card(pool.clone(), tidal_id.clone()).await
    );
    let mut off = config(Some(("isrc-default-id", "isrc-default-secret")));
    off.also_on.sources.insert(
        "spotify_isrc".into(),
        SourceOptions {
            enabled: Some(false),
            ..Default::default()
        },
    );
    off.validate().unwrap();
    also_on::configure(&pool, &off).await.unwrap();
    assert_eq!(links(pool.clone(), tidal_id.clone()).await, expected);
    assert!(
        !card(pool.clone(), tidal_id.clone())
            .await
            .contains(&expected[0]),
        "{:?}",
        card(pool.clone(), tidal_id.clone()).await
    );
}

/// An album on another service with a barcode key is looked up on Spotify by it (`spotify_isrc`):
/// a UPC-A by its 12 digits, an EAN-13 whole; the search's albums (three at most) are read back
/// one by one (`/v1/albums/{id}`: Spotify answers no batch read to an app in development mode),
/// and only the one whose own barcode is the key becomes a link keyed by it. A 403 on an album's
/// read and a refusal of the app keep it.
#[sqlx::test(migrations = "../../migrations")]
async fn an_album_with_a_barcode_key_gets_its_spotify_album(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"spotify_isrc") {
        return;
    }
    let keyed = |uri: &'static str, upc: &'static str| {
        let pool = pool.clone();
        async move {
            let id = save(&pool, uri, "MusicAlbum").await;
            sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,$2,'test','test',now()+interval '1 day')")
                .bind(&id)
                .bind(upc)
                .execute(&pool)
                .await
                .unwrap();
            id
        }
    };
    // HYDRA 3D on Deezer (its EAN-13), and Random Access Memories on Deezer (a UPC-A, which this
    // Spotify's search lists among more it found).
    let hydra = keyed(
        "https://www.deezer.com/album/320429237",
        "upc:4018939306052",
    )
    .await;
    let ram = keyed("https://www.deezer.com/album/6575789", "upc:0886443927087").await;
    // Underclass Hero on Deezer: of its two candidates, one carries the barcode and one gives no
    // album (404), which might have been another: no link.
    let underclass = keyed("https://www.deezer.com/album/125431", "upc:0602517416987").await;
    let seen = run(
        &pool,
        config(Some(("upc-id", "upc-secret"))),
        Answers::default(),
    )
    .await;
    let mut asked: Vec<String> = seen
        .iter()
        .filter(|s| {
            s.starts_with("api.spotify.com/v1/search?")
                || s.starts_with("api.spotify.com/v1/albums/")
        })
        .cloned()
        .collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            format!("api.spotify.com/v1/albums/{ALBUM}?"),
            format!("api.spotify.com/v1/albums/{MISSING_UPC_ALBUM}?"),
            format!("api.spotify.com/v1/albums/{OTHER_UPC_ALBUM}?"),
            "api.spotify.com/v1/search?q=upc%3A4018939306052&type=album&limit=3".to_owned(),
            "api.spotify.com/v1/search?q=upc%3A602517416987&type=album&limit=3".to_owned(),
            "api.spotify.com/v1/search?q=upc%3A886443927087&type=album&limit=3".to_owned(),
        ],
        "{seen:?}"
    );
    let links = "SELECT uri||' '||coalesce(key,'-')||' '||via FROM derived_link WHERE resource_id=$1 AND source='spotify_isrc' ORDER BY uri";
    let expected = [format!(
        "https://open.spotify.com/album/{ALBUM} upc:4018939306052 spotify_isrc"
    )];
    assert_eq!(rows(&pool, links, &hydra).await, expected);
    // Random Access Memories: a search that found more albums than it lists names none, and its
    // album is not read.
    assert!(rows(&pool, links, &ram).await.is_empty());
    assert!(rows(&pool, links, &underclass).await.is_empty());
    assert!(
        !seen.iter().any(|s| s.contains(PAGED_UPC_ALBUM)),
        "{seen:?}"
    );
    let queued = || async {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT last_error FROM also_on_dirty WHERE resource_id=$1",
        )
        .bind(&hydra)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    // A 403 on one album's read alone: the link stays, the lookup is retried, and the pair is not
    // refused: its next run asks Spotify again.
    let pair = ("upc-album-id", "upc-album-secret");
    run(
        &pool,
        config(Some(pair)),
        Answers {
            album_status: Some(403),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(rows(&pool, links, &hydra).await, expected);
    assert_eq!(queued().await.as_deref(), Some("forbidden_request"));
    let seen = run(&pool, config(Some(pair)), Answers::default()).await;
    assert!(
        seen.iter()
            .any(|s| s.starts_with("api.spotify.com/v1/search?q=upc")),
        "{seen:?}"
    );
    assert_eq!(rows(&pool, links, &hydra).await, expected);
    // Spotify refusing the app (no Premium): the link stays, the lookup waits out the hold.
    run(
        &pool,
        config(Some(("upc-refused-id", "upc-refused-secret"))),
        Answers {
            api_status: Some(403),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(rows(&pool, links, &hydra).await, expected);
    assert_eq!(queued().await.as_deref(), Some("spotify_auth"));
}
