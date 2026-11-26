//! The Spotify Web API as the tests of the `spotify` and `spotify_isrc` also-on sources see it:
//! answers written by hand in the API's documented shape (`/v1/albums/{id}`, `/v1/tracks/{id}`,
//! `/v1/search`: the members those sources read) and recorded iTunes, Deezer and MusicBrainz
//! answers (`tests/fixtures/hubs`). No Spotify answer here is a recording: Spotify answers an app
//! whose owner has no Premium subscription with a 403. The album's ID is made up, its barcode
//! HYDRA 3D's (as in the TIDAL tests).
use super::{hubs_fixture as fixture, media::PublicDns};
use pinakes_core::{
    also_on::{self, SourceOptions},
    config::Config,
    resolver::egress::{Egress, FetchOptions, Fetched, Transport},
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

/// A made-up Spotify album ID (22 base-62 characters) with HYDRA 3D's barcode as a UPC-A.
pub const ALBUM: &str = "0hydra3DexampleAlbum01";
/// "Bohemian Rhapsody - Remastered 2011".
pub const TRACK: &str = "7tFiyTwD0nx5a1eklYtX2J";
/// `GET /v1/albums/{ALBUM}`, the members read: `id`, `type`, `external_ids.upc`.
pub fn album_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "album_type": "album", "id": ALBUM, "type": "album", "name": "HYDRA 3D",
        "external_ids": {"upc": "4018939306052"}, "total_tracks": 1,
        "uri": format!("spotify:album:{ALBUM}"),
    }))
    .unwrap()
}
/// Two made-up tracks with an ISRC MusicBrainz gives to two recordings (3:33 and 2:59, as in
/// the hubs tests): one 3:34 long, which picks the first, and one 3:16, which picks neither.
pub const SHARED_PICKED: &str = "1sharedIsrcTrack214sec";
pub const SHARED_NEITHER: &str = "1sharedIsrcTrack196sec";
pub fn shared_track_body(id: &str, ms: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id, "type": "track", "name": "Never Gonna Give You Up", "duration_ms": ms,
        "external_ids": {"isrc": "GBARL9300135"},
    }))
    .unwrap()
}
/// A made-up album the barcode search lists beside HYDRA 3D's, whose own record carries
/// Underclass Hero's barcode.
pub const OTHER_UPC_ALBUM: &str = "0otherUpcAlbum00000000";
/// A made-up album a barcode search lists, which Spotify has no album for (404).
pub const MISSING_UPC_ALBUM: &str = "0missingUpcAlbum000000";
/// A made-up album that carries Random Access Memories' barcode, listed by a search that found
/// more albums than it lists.
pub const PAGED_UPC_ALBUM: &str = "0ramUpcAlbum0000000000";
/// A made-up track the ISRC search lists beside Bohemian Rhapsody's, with another ISRC.
pub const OTHER_ISRC_TRACK: &str = "0otherIsrcTrack0000000";
/// `GET /v1/tracks/{TRACK}`: `id`, `type`, `external_ids.isrc` and `duration_ms` (5:54).
pub fn track_body() -> Vec<u8> {
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
pub struct Answers {
    pub refuse: bool,
    pub api_status: Option<u16>,
    /// A status for one album's read (`/v1/albums/{id}`) alone.
    pub album_status: Option<u16>,
}
/// Answers Spotify (its token and API), MusicBrainz's ISRC lookup, iTunes and Deezer; records
/// each request.
pub struct Recorded {
    pub seen: Mutex<Vec<String>>,
    pub answers: Answers,
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
            retry_after_secs: 120,
            ..super::fetched(url.clone(), status, content_type.into(), body)
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
/// sources these tests read.
pub fn config(pair: Option<(&str, &str)>) -> Config {
    let mut config = Config::default();
    if let Some((id, secret)) = pair {
        config.sections.set(
            "spotify",
            serde_json::json!({"client_id": id, "client_secret": secret}),
        );
    }
    // Only the sources this build has: a switch for one it lacks is refused at validation.
    for (source, enabled) in [
        ("deezer", true),
        ("spotify_isrc", true),
        ("musicbrainz", false),
        ("musicbrainz_entity", false),
        ("wikidata", false),
    ] {
        if !pinakes_core::test_support::switch_also_on(&[source]) {
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
/// Every resource's sources, once, on a fresh egress; the requests made.
pub async fn run(pool: &sqlx::PgPool, config: Config, answers: Answers) -> Vec<String> {
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
        answers,
    });
    let egress = Egress::with_backends(
        super::unpaced_egress_config(),
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
pub const KEYS: &str =
    "SELECT key||' '||source FROM resource_key WHERE resource_id=$1 ORDER BY key COLLATE \"C\"";
pub fn spotify_requests(seen: &[String]) -> Vec<&String> {
    seen.iter().filter(|s| s.contains("spotify.com")).collect()
}
