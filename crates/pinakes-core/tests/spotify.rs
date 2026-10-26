//! The `spotify` also-on source through Egress, against Spotify answers written by hand in the
//! Web API's documented shape (`/v1/albums/{id}`, `/v1/tracks/{id}`: the members this source
//! reads) and recorded iTunes, Deezer and MusicBrainz answers (`tests/fixtures/hubs`). Spotify
//! refused to answer the owner's app when these tests were written (403: "Active premium
//! subscription required for the owner of the app"), so no Spotify answer here is a recording;
//! the album's ID is made up, its barcode HYDRA 3D's (as in the TIDAL tests).
use pinakes_core::{
    also_on::{self, SourceOptions},
    config::{Config, EgressConfig},
    groups,
    model::ResourceMeta,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
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
                if path == format!("/v1/albums/{ALBUM}") {
                    answer(200, album_body(), "application/json; charset=utf-8")
                } else if path == format!("/v1/tracks/{TRACK}") {
                    answer(200, track_body(), "application/json; charset=utf-8")
                } else {
                    answer(
                        404,
                        br#"{"error":{"status":404,"message":"Resource not found"}}"#.to_vec(),
                        "application/json",
                    )
                }
            }
            ("musicbrainz.org", "/ws/2/isrc/GBUM71029604") => answer(
                200,
                fixture("mb-isrc-GBUM71029604.json"),
                "application/json",
            ),
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
    let egress = Egress::with_backends(EgressConfig::default(), Arc::new(PublicDns), t.clone());
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
