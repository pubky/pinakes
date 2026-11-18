//! The `tidal_isrc` also-on source through Egress, against recorded TIDAL answers
//! (`tests/fixtures/media/tidal_isrc`): another service's track found on TIDAL by its ISRC, an
//! album by its barcode, each read back; refusals and rate limits keep what TIDAL gave before.
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
use common::{also_on_due as due, media::PublicDns};

fn fixture(name: &str) -> Option<Vec<u8>> {
    std::fs::read(format!(
        "{}/tests/fixtures/media/tidal_isrc/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .ok()
}

/// What TIDAL answers instead of the recordings: a refusal at the token endpoint, or a status
/// from the API (a 429 with a `Retry-After` of an hour).
#[derive(Clone, Default)]
struct Answers {
    refuse: bool,
    api_status: Option<u16>,
}
/// Answers TIDAL's token endpoint and its lookups by ISRC and barcode from the recordings;
/// records each request.
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
        let answer = |status: u16, body: Vec<u8>| Fetched {
            retry_after_secs: if status == 429 { 3600 } else { 120 },
            ..common::fetched(url.clone(), status, "application/vnd.api+json".into(), body)
        };
        let query = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        Ok(match (host, url.path()) {
            ("auth.tidal.com", "/v1/oauth2/token") => {
                assert!(options.sensitive);
                if self.answers.refuse {
                    answer(401, br#"{"error":"invalid_client"}"#.to_vec())
                } else {
                    answer(
                        200,
                        br#"{"access_token":"recorded-token","token_type":"Bearer","expires_in":14400}"#
                            .to_vec(),
                    )
                }
            }
            ("openapi.tidal.com", path) => {
                assert!(options.sensitive);
                assert_eq!(
                    header("authorization").as_deref(),
                    Some("Bearer recorded-token")
                );
                assert_eq!(query("countryCode"), "US");
                if let Some(status) = self.answers.api_status {
                    return Ok(answer(status, br#"{"errors":[{"code":"TOLD"}]}"#.to_vec()));
                }
                let empty = br#"{"data":[],"links":{}}"#.to_vec();
                match path {
                    // An ISRC TIDAL has no track for: its recorded answer.
                    "/v2/tracks" => answer(
                        200,
                        fixture(&format!("tracks-isrc-{}.json", query("filter[isrc]")))
                            .or_else(|| fixture("tracks-isrc-missing.json"))
                            .unwrap(),
                    ),
                    "/v2/albums" => answer(
                        200,
                        fixture(&format!(
                            "albums-barcode-{}.json",
                            query("filter[barcodeId]")
                        ))
                        .unwrap_or(empty),
                    ),
                    _ => answer(404, br#"{"errors":[{"code":"NOT_FOUND"}]}"#.to_vec()),
                }
            }
            _ => panic!("unexpected request {url}"),
        })
    }
}

/// TIDAL's pair (one per test: tokens and refusals are kept per pair) and the source's switch
/// (`None`: its default).
fn config(pair: Option<(&str, &str)>, on: impl Into<Option<bool>>) -> Config {
    let mut config = Config::default();
    if let Some((id, secret)) = pair {
        config.sections.set(
            "tidal",
            serde_json::json!({"client_id": id, "client_secret": secret}),
        );
    }
    if let Some(on) = on.into() {
        config.also_on.sources.insert(
            "tidal_isrc".into(),
            SourceOptions {
                enabled: Some(on),
                ..Default::default()
            },
        );
    }
    config.validate().unwrap();
    config
}
/// A resource of `kind` with `key` (as a hub would key it).
async fn save(pool: &sqlx::PgPool, uri: &str, kind: &str, key: Option<&str>) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type(kind);
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
        .filter(|s| s.id() == "tidal_isrc")
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
    sqlx::query_scalar("SELECT uri||' '||coalesce(key,'-')||' '||via FROM derived_link WHERE resource_id=$1 AND source='tidal_isrc' ORDER BY uri COLLATE \"C\"")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Walking Disaster's ISRC on Deezer finds TIDAL's tracks (five of eight, streamed ones first),
/// Underclass Hero's barcode its one album; an ISRC TIDAL lacks and a barcode several albums
/// carry find nothing; TIDAL's own track is not asked.
#[sqlx::test(migrations = "../../migrations")]
async fn tracks_and_albums_are_found_on_tidal_by_their_keys(pool: sqlx::PgPool) {
    let track = save(
        &pool,
        "https://www.deezer.com/track/1171872",
        "MusicRecording",
        Some("isrc:USUM70740306"),
    )
    .await;
    let album = save(
        &pool,
        "https://www.deezer.com/album/125431",
        "MusicAlbum",
        Some("upc:0602517416987"),
    )
    .await;
    let missing = save(
        &pool,
        "https://soundcloud.com/someone/nothing",
        "MusicRecording",
        Some("isrc:QZZZZ2600001"),
    )
    .await;
    let several = save(
        &pool,
        "https://www.deezer.com/album/1",
        "MusicAlbum",
        Some("upc:0000000000000"),
    )
    .await;
    let own = save(
        &pool,
        "https://tidal.com/track/3524506",
        "MusicRecording",
        Some("isrc:USUM70740306"),
    )
    .await;
    let seen = run(
        &pool,
        config(Some(("found-id", "found-secret")), true),
        Answers::default(),
    )
    .await;
    let mut asked: Vec<&String> = seen.iter().filter(|s| s.contains("tidal.com")).collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            "auth.tidal.com/v1/oauth2/token?",
            "openapi.tidal.com/v2/albums?countryCode=US&filter%5BbarcodeId%5D=0000000000000",
            "openapi.tidal.com/v2/albums?countryCode=US&filter%5BbarcodeId%5D=0602517416987",
            "openapi.tidal.com/v2/tracks?countryCode=US&filter%5Bisrc%5D=QZZZZ2600001",
            "openapi.tidal.com/v2/tracks?countryCode=US&filter%5Bisrc%5D=USUM70740306",
        ],
        "{seen:?}"
    );
    assert_eq!(
        links(&pool, &track).await,
        [
            "https://tidal.com/track/199286892 isrc:USUM70740306 tidal_isrc",
            "https://tidal.com/track/199571302 isrc:USUM70740306 tidal_isrc",
            "https://tidal.com/track/31067660 isrc:USUM70740306 tidal_isrc",
            "https://tidal.com/track/3524506 isrc:USUM70740306 tidal_isrc",
            "https://tidal.com/track/56947135 isrc:USUM70740306 tidal_isrc",
        ]
    );
    assert_eq!(
        links(&pool, &album).await,
        ["https://tidal.com/album/3524504 upc:0602517416987 tidal_isrc"]
    );
    for id in [&missing, &several, &own] {
        assert!(links(&pool, id).await.is_empty(), "{id}");
    }
}

/// On by default with TIDAL's pair; switched off, or without the pair, it asks TIDAL nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn off_or_without_credentials_nothing_asks_tidal(pool: sqlx::PgPool) {
    let track = save(
        &pool,
        "https://www.deezer.com/track/1171872",
        "MusicRecording",
        Some("isrc:USUM70740306"),
    )
    .await;
    for config in [
        config(Some(("off-id", "off-secret")), false),
        config(None, true),
    ] {
        let seen = run(&pool, config, Answers::default()).await;
        assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
        assert!(links(&pool, &track).await.is_empty());
    }
    let seen = run(
        &pool,
        config(Some(("default-id", "default-secret")), None),
        Answers::default(),
    )
    .await;
    assert!(
        seen.iter().any(|s| s.contains("openapi.tidal.com")),
        "{seen:?}"
    );
    assert!(!links(&pool, &track).await.is_empty());
}

/// A refusal of the credentials (at the token endpoint, or a 403 from the API) and a rate limit
/// (429) keep the links an earlier run found and retry later: after the five-minute hold
/// (`tidal_auth`), or after the hour TIDAL asked for (`tidal_busy`).
#[sqlx::test(migrations = "../../migrations")]
async fn refusals_and_rate_limits_keep_the_links(pool: sqlx::PgPool) {
    let album = save(
        &pool,
        "https://www.deezer.com/album/125431",
        "MusicAlbum",
        Some("upc:0602517416987"),
    )
    .await;
    run(
        &pool,
        config(Some(("kept-id", "kept-secret")), true),
        Answers::default(),
    )
    .await;
    let found = ["https://tidal.com/album/3524504 upc:0602517416987 tidal_isrc"];
    assert_eq!(links(&pool, &album).await, found);
    for (pair, answers, class, wait) in [
        (
            ("refused-id", "refused-secret"),
            Answers {
                refuse: true,
                ..Default::default()
            },
            "tidal_auth",
            290.0..=300.0,
        ),
        (
            ("forbidden-id", "forbidden-secret"),
            Answers {
                api_status: Some(403),
                ..Default::default()
            },
            "tidal_auth",
            290.0..=300.0,
        ),
        (
            ("busy-id", "busy-secret"),
            Answers {
                api_status: Some(429),
                ..Default::default()
            },
            "tidal_busy",
            3500.0..=3600.0,
        ),
    ] {
        // Each case's retry counts from its own first failure.
        sqlx::query("UPDATE also_on_dirty SET attempts=0")
            .execute(&pool)
            .await
            .unwrap();
        let before = common::token_refusals("tidal");
        let seen = run(&pool, config(Some(pair), true), answers.clone()).await;
        assert!(seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
        assert_eq!(links(&pool, &album).await, found, "{class}");
        // A refusal of the credentials (401 at the token endpoint, a 403 from the API) is counted
        // where an alert reads it; a rate limit is not.
        let counted = common::token_refusals("tidal") - before;
        if answers.api_status == Some(429) {
            assert_eq!(counted, 0, "{class}");
        } else {
            assert!(counted >= 1, "{class}");
        }
        let (error, seconds) = due(&pool, &album).await;
        assert_eq!(error.as_deref(), Some(class));
        assert!(wait.contains(&seconds), "{class}: {seconds}");
        // Within the hold, the retry asks TIDAL nothing.
        let seen = run(&pool, config(Some(pair), true), answers).await;
        assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
    }
}
