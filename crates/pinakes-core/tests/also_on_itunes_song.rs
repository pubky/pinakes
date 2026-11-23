//! The `itunes_song` also-on source through Egress, against recorded Deezer, TIDAL and iTunes
//! answers (`tests/fixtures/media/itunes_song`, `tests/fixtures/hubs`): Get Lucky's Apple Music
//! song from its album barcode and place on Deezer and TIDAL; Walking Disaster's barcode answers
//! several collections and gives nothing.
use pinakes_core::{
    also_on::{self, SourceOptions},
    config::Config,
    model::ResourceMeta,
    resolver::egress::{Egress, FetchOptions, Fetched, Transport},
    store,
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
mod common;
use common::media::PublicDns;

/// A recording of this source's own, else one of the hubs' shared ones.
fn fixture(name: &str) -> Option<Vec<u8>> {
    let dir = env!("CARGO_MANIFEST_DIR");
    std::fs::read(format!("{dir}/tests/fixtures/media/itunes_song/{name}"))
        .or_else(|_| std::fs::read(format!("{dir}/tests/fixtures/hubs/{name}")))
        .ok()
}

/// iTunes answers this status instead of its recordings (a 429 with a `Retry-After` of an hour).
#[derive(Clone, Default)]
struct Answers {
    itunes_status: Option<u16>,
}
/// Answers Deezer, TIDAL and iTunes from the recordings; records each request.
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
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        let host = url.host_str().unwrap_or("");
        self.seen.lock().unwrap().push(format!(
            "{host}{}?{}",
            url.path(),
            url.query().unwrap_or("")
        ));
        let answer = |status: u16, body: Vec<u8>, content_type: &str| Fetched {
            retry_after_secs: if status == 429 { 3600 } else { 120 },
            ..common::fetched(url.clone(), status, content_type.into(), body)
        };
        let query = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        Ok(match (host, url.path()) {
            ("api.deezer.com", path) => {
                // `track/67238735` is `deezer-track-67238735.json`.
                let name = format!(
                    "deezer-{}.json",
                    path.trim_start_matches('/').replace(['/', ':'], "-")
                );
                let none = br#"{"error":{"type":"DataException","message":"no data","code":800}}"#;
                answer(
                    200,
                    fixture(&name).unwrap_or_else(|| none.to_vec()),
                    "application/json",
                )
            }
            ("itunes.apple.com", "/lookup") => {
                if let Some(status) = self.answers.itunes_status {
                    return Ok(answer(status, b"{}".to_vec(), "text/javascript"));
                }
                assert_eq!(query("entity"), "song");
                assert_eq!(query("country"), "us");
                let body = fixture(&format!("itunes-upc-{}-songs.json", query("upc")))
                    .unwrap_or_else(|| b"\n\n\n{\"resultCount\":0,\"results\":[]}".to_vec());
                answer(200, body, "text/javascript")
            }
            ("auth.tidal.com", "/v1/oauth2/token") => answer(
                200,
                br#"{"access_token":"recorded-token","token_type":"Bearer","expires_in":14400}"#
                    .to_vec(),
                "application/json",
            ),
            ("openapi.tidal.com", path) => {
                // `/v2/tracks/20115564?include=albums` and `/v2/albums/20115556/relationships/items`.
                let name = match path.strip_prefix("/v2/tracks/") {
                    Some(track) if query("include") == "albums" => {
                        format!("tidal-track-{track}-albums.json")
                    }
                    _ => match path
                        .strip_prefix("/v2/albums/")
                        .and_then(|p| p.strip_suffix("/relationships/items"))
                    {
                        Some(album) => format!("tidal-album-{album}-items.json"),
                        None => String::new(),
                    },
                };
                match fixture(&name) {
                    Some(body) => answer(200, body, "application/vnd.api+json"),
                    None => answer(
                        404,
                        br#"{"errors":[{"code":"NOT_FOUND"}]}"#.to_vec(),
                        "application/vnd.api+json",
                    ),
                }
            }
            _ => panic!("unexpected request {url}"),
        })
    }
}

/// Whether this build has every also-on source a test needs: a test of some sources returns
/// early without them.
fn has(ids: &[&str]) -> bool {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    ids.iter().all(|id| registered.contains(id))
}
/// The switches (`itunes_song` and `deezer`) and TIDAL's pair.
fn config(on: bool, deezer: bool, tidal: Option<(&str, &str)>) -> Config {
    let mut config = Config::default();
    if let Some((id, secret)) = tidal {
        config.sections.set(
            "tidal",
            serde_json::json!({"client_id": id, "client_secret": secret}),
        );
    }
    for (source, enabled) in [("itunes_song", on), ("deezer", deezer)] {
        // Only the sources this build has: a switch for one it lacks is refused.
        if !has(&[source]) {
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
/// A track resource, with `key` as a hub would key it.
async fn save(pool: &sqlx::PgPool, uri: &str, key: Option<&str>) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type("MusicRecording");
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    if let Some(key) = key {
        sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,$2,'test','test',now()+interval '1 day')")
            .bind(&id)
            .bind(key)
            .execute(pool)
            .await
            .unwrap();
    }
    id
}
/// This source alone, once for every resource, on a fresh egress; the requests made.
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
    let sources: Vec<_> = also_on::registry()
        .into_iter()
        .filter(|s| s.id() == "itunes_song")
        .collect();
    sqlx::query("INSERT INTO also_on_dirty(resource_id) SELECT id FROM resource ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
        .execute(pool)
        .await
        .unwrap();
    also_on::drain(pool, &cx, &sources, 100).await.unwrap();
    let seen = t.seen.lock().unwrap().clone();
    seen
}
async fn links(pool: &sqlx::PgPool, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT uri||' '||coalesce(key,'-')||' '||via||' '||coalesce(kind,'-') FROM derived_link WHERE resource_id=$1 AND source='itunes_song' ORDER BY uri")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}
/// A maybe: no key, `kind` `maybe`.
const GET_LUCKY: &str = "https://music.apple.com/song/617154366 - itunes_song maybe";

/// Get Lucky on Deezer (367 s, disc 1 track 8 of Random Access Memories) is Apple's song
/// 617154366 (369.6 s), unkeyed. Walking Disaster, by its ISRC key on another service, is
/// Deezer's track 2 of Underclass Hero, whose barcode answers several collections: nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_deezer_track_s_place_names_its_apple_song(pool: sqlx::PgPool) {
    if !has(&["deezer", "itunes"]) {
        return;
    }
    let lucky = save(&pool, "https://www.deezer.com/track/67238735", None).await;
    let walking = save(
        &pool,
        "https://soundcloud.com/someone/walking-disaster",
        Some("isrc:USUM70740306"),
    )
    .await;
    let seen = run(&pool, config(true, true, None), Answers::default()).await;
    let mut asked = seen.clone();
    asked.sort();
    assert_eq!(
        asked,
        [
            "api.deezer.com/album/125431?",
            "api.deezer.com/album/6575789?",
            "api.deezer.com/track/67238735?",
            "api.deezer.com/track/isrc:USUM70740306?",
            "itunes.apple.com/lookup?upc=0602517416987&entity=song&country=us",
            "itunes.apple.com/lookup?upc=0886443927087&entity=song&country=us",
        ]
    );
    assert_eq!(links(&pool, &lucky).await, [GET_LUCKY]);
    assert!(links(&pool, &walking).await.is_empty());
}

/// TIDAL's Get Lucky (370 s): its one album's barcode and its place among the album's items.
/// TIDAL's Walking Disaster: Underclass Hero again, several collections, nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_tidal_track_s_place_names_its_apple_song(pool: sqlx::PgPool) {
    if !has(&["tidal", "itunes"]) {
        return;
    }
    let lucky = save(&pool, "https://tidal.com/track/20115564", None).await;
    let walking = save(&pool, "https://tidal.com/track/3524506", None).await;
    let seen = run(
        &pool,
        config(true, false, Some(("song-id", "song-secret"))),
        Answers::default(),
    )
    .await;
    assert!(!seen.iter().any(|s| s.contains("deezer")), "{seen:?}");
    assert!(seen.contains(
        &"openapi.tidal.com/v2/albums/20115556/relationships/items?countryCode=US".to_owned()
    ));
    assert_eq!(links(&pool, &lucky).await, [GET_LUCKY]);
    assert!(links(&pool, &walking).await.is_empty());
}

/// On by default; switched off, without the source of a track's place (Deezer off, no TIDAL
/// pair) or with `itunes` off, it asks nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn without_its_switch_or_a_place_s_source_nothing_is_asked(pool: sqlx::PgPool) {
    if !has(&["deezer", "itunes"]) {
        return;
    }
    save(&pool, "https://www.deezer.com/track/67238735", None).await;
    save(&pool, "https://tidal.com/track/20115564", None).await;
    save(
        &pool,
        "https://soundcloud.com/someone/walking-disaster",
        Some("isrc:USUM70740306"),
    )
    .await;
    let mut by_default = config(true, true, None);
    by_default.also_on.sources.remove("itunes_song");
    by_default.validate().unwrap();
    let seen = run(&pool, by_default, Answers::default()).await;
    assert!(
        seen.iter().any(|s| s.starts_with("itunes.apple.com")),
        "{seen:?}"
    );
    // `itunes` off: Apple is not asked, nor anything for it.
    let mut no_itunes = config(true, true, None);
    no_itunes.also_on.sources.insert(
        "itunes".into(),
        SourceOptions {
            enabled: Some(false),
            ..Default::default()
        },
    );
    no_itunes.validate().unwrap();
    for config in [
        config(false, true, None),
        config(true, false, None),
        no_itunes,
    ] {
        let seen = run(&pool, config, Answers::default()).await;
        assert!(seen.is_empty(), "{seen:?}");
    }
}

/// iTunes rate-limiting the lookup: retried after the hour it asked for, the link an earlier run
/// found kept.
#[sqlx::test(migrations = "../../migrations")]
async fn a_busy_itunes_keeps_the_song(pool: sqlx::PgPool) {
    if !has(&["deezer", "itunes"]) {
        return;
    }
    let lucky = save(&pool, "https://www.deezer.com/track/67238735", None).await;
    run(&pool, config(true, true, None), Answers::default()).await;
    assert_eq!(links(&pool, &lucky).await, [GET_LUCKY]);
    run(
        &pool,
        config(true, true, None),
        Answers {
            itunes_status: Some(429),
        },
    )
    .await;
    assert_eq!(links(&pool, &lucky).await, [GET_LUCKY]);
    // The retry is queued for the hour iTunes asked for (`Retry-After: 3600`), not the worker's
    // own first backoff of a minute or two.
    let (error, seconds) = common::also_on_due(&pool, &lucky).await;
    assert!(error.is_some(), "a retry is queued");
    assert!((3500.0..=3600.5).contains(&seconds), "{error:?}: {seconds}");
}
