//! The `tidal` also-on source through Egress, against recorded TIDAL, iTunes and Deezer answers
//! (`tests/fixtures/hubs`): HYDRA 3D's barcode from TIDAL's own record joins Apple Music and
//! Deezer.
use pinakes_core::{
    also_on::{self, SourceOptions},
    config::{Config, EgressConfig},
    event::adapters::SecretKey,
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
/// The recorded batch of two albums, as TIDAL answers for one of them alone.
fn album(id: &str) -> Option<Vec<u8>> {
    let batch: serde_json::Value =
        serde_json::from_slice(&fixture("tidal-albums-199574241-495402659.json")).unwrap();
    let item = batch["data"].as_array()?.iter().find(|i| i["id"] == id)?;
    Some(serde_json::to_vec(&serde_json::json!({ "data": item })).unwrap())
}

/// What TIDAL answers instead of the recordings: a refusal of the credentials at the token
/// endpoint, a status for a batch, a status for a single item (with a `Retry-After` of an hour).
#[derive(Clone, Default)]
struct Answers {
    refuse: bool,
    batch_status: Option<u16>,
    item_status: Option<u16>,
    /// A single item's 429 is released only once this pair has a second lookup pending at its
    /// slot; the test fails when none is within five seconds.
    await_pending: Option<(&'static str, &'static str)>,
    /// MusicBrainz's ISRC lookup answers this status (its shared limit: 503).
    musicbrainz_status: Option<u16>,
}
/// Answers TIDAL (its token and API), MusicBrainz's ISRC lookup, iTunes and Deezer from the
/// recordings, or as `answers` says; records each request.
struct Recorded {
    seen: Mutex<Vec<String>>,
    answers: Mutex<Answers>,
}
impl Recorded {
    /// The recordings again (MusicBrainz answers, TIDAL answers), for a later run.
    fn answers_reset(&self) {
        *self.answers.lock().unwrap() = Answers::default();
    }
    fn answers(&self) -> Answers {
        self.answers.lock().unwrap().clone()
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
        let host = url.host_str().unwrap_or("");
        let target = format!("{host}{}?{}", url.path(), url.query().unwrap_or(""));
        self.seen.lock().unwrap().push(target);
        let header = |name: &str| {
            options
                .headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        };
        let json = |status: u16, body: Vec<u8>, content_type: &str| Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status,
            retry_after_secs: if status == 429 { 3600 } else { 120 },
            location: None,
            content_type: content_type.into(),
            body,
        };
        Ok(match (host, url.path()) {
            ("auth.tidal.com", "/v1/oauth2/token") => {
                assert!(options.sensitive);
                assert_eq!(
                    options.form_body,
                    Some(vec![("grant_type", "client_credentials".to_owned())])
                );
                let basic = header("authorization").unwrap();
                assert!(basic.starts_with("Basic "), "{basic}");
                if self.answers().refuse {
                    json(
                        401,
                        br#"{"error":"invalid_client"}"#.to_vec(),
                        "application/json",
                    )
                } else {
                    json(
                        200,
                        br#"{"access_token":"recorded-token","token_type":"Bearer","expires_in":14400}"#
                            .to_vec(),
                        "application/json",
                    )
                }
            }
            ("openapi.tidal.com", path) => {
                assert!(options.sensitive);
                assert_eq!(
                    header("authorization").as_deref(),
                    Some("Bearer recorded-token")
                );
                assert_eq!(options.accept.as_deref(), Some("application/vnd.api+json"));
                let filters: Vec<String> = url
                    .query_pairs()
                    .filter(|(k, _)| k == "filter[id]")
                    .map(|(_, v)| v.into_owned())
                    .collect();
                let told = match path {
                    "/v2/albums" | "/v2/tracks" => self.answers().batch_status,
                    _ => self.answers().item_status,
                };
                if let Some(status) = told {
                    // The test's own second lookup must be pending at the pair's slot before its
                    // 429 is released (on code without the slot none ever is): a timeout fails
                    // the test rather than letting the answer through.
                    if let Some((id, secret)) = self.answers().await_pending {
                        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
                        while also_on::tidal::lookups_waiting(id, secret) == 0 {
                            assert!(
                                std::time::Instant::now() < until,
                                "no second lookup of {id} pending at its slot within five seconds"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                    }
                    return Ok(json(
                        status,
                        br#"{"errors":[{"code":"TOLD"}]}"#.to_vec(),
                        "application/vnd.api+json",
                    ));
                }
                let body = match path {
                    "/v2/albums" if filters == ["199574241", "495402659"] => {
                        Some(fixture("tidal-albums-199574241-495402659.json"))
                    }
                    // HYDRA 3D's title track, in a batch of any size (the others TIDAL lacks).
                    "/v2/tracks" if filters.iter().any(|f| f == "199574242") => {
                        Some(fixture("tidal-tracks-199574242.json"))
                    }
                    "/v2/tracks/199574242" => {
                        let batch: serde_json::Value =
                            serde_json::from_slice(&fixture("tidal-tracks-199574242.json"))
                                .unwrap();
                        Some(
                            serde_json::to_vec(&serde_json::json!({"data": batch["data"][0]}))
                                .unwrap(),
                        )
                    }
                    path => path.strip_prefix("/v2/albums/").and_then(album),
                };
                match body {
                    Some(body) => json(200, body, "application/vnd.api+json"),
                    None => json(
                        404,
                        br#"{"errors":[{"code":"NOT_FOUND"}]}"#.to_vec(),
                        "application/vnd.api+json",
                    ),
                }
            }
            // HYDRA 3D's title track: MusicBrainz knows no recording with its ISRC.
            ("musicbrainz.org", "/ws/2/isrc/DEQ121641300") => json(
                self.answers().musicbrainz_status.unwrap_or(404),
                br#"{"error":"Not Found"}"#.to_vec(),
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
                json(200, body, "text/javascript")
            }
            ("api.deezer.com", path) => {
                let name = format!(
                    "deezer-{}.json",
                    path.trim_start_matches('/').replace(['/', ':'], "-")
                );
                let recorded = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
                let none = br#"{"error":{"type":"DataException","message":"no data","code":800}}"#;
                json(
                    200,
                    std::fs::read(recorded).unwrap_or_else(|_| none.to_vec()),
                    "application/json",
                )
            }
            _ => panic!("unexpected request {url}"),
        })
    }
}

/// TIDAL's pair (a different one per test: tokens are kept per pair), Deezer on, and only the
/// sources this test reads.
fn config(pair: Option<(&str, &str)>) -> Config {
    let mut config = Config::default();
    if let Some((id, secret)) = pair {
        config.tidal.client_id = Some(SecretKey::from(id.to_owned()));
        config.tidal.client_secret = Some(SecretKey::from(secret.to_owned()));
    }
    for (source, enabled) in [
        ("deezer", true),
        ("musicbrainz", false),
        ("musicbrainz_entity", false),
        ("wikidata", false),
    ] {
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
        answers: Mutex::new(answers),
    });
    let egress = Egress::with_backends(EgressConfig::default(), Arc::new(PublicDns), t.clone());
    run_with(pool, config, &t, &egress).await
}
/// Every resource's sources, once, on `egress` (kept across runs: its cache is the daemon's);
/// the requests made in this run.
async fn run_with(
    pool: &sqlx::PgPool,
    config: Config,
    t: &Arc<Recorded>,
    egress: &Egress,
) -> Vec<String> {
    let before = t.seen.lock().unwrap().len();
    let cx = also_on::Context {
        egress: egress.clone(),
        config: Arc::new(config),
    };
    sqlx::query("INSERT INTO also_on_dirty(resource_id) SELECT id FROM resource ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
        .execute(pool)
        .await
        .unwrap();
    also_on::drain(pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    let seen = t.seen.lock().unwrap()[before..].to_vec();
    seen
}
async fn rows(pool: &sqlx::PgPool, sql: &str, id: &str) -> Vec<String> {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}
const KEYS: &str = "SELECT key||' '||source FROM resource_key WHERE resource_id=$1 ORDER BY key";

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_tidal_barcode_joins_apple_music_and_deezer(pool: sqlx::PgPool) {
    let hydra = save(&pool, "https://tidal.com/album/199574241", "MusicAlbum").await;
    let dracula = save(&pool, "https://tidal.com/album/495402659", "MusicAlbum").await;
    let track = save(&pool, "https://tidal.com/track/199574242", "MusicRecording").await;
    let apple = save(
        &pool,
        "https://music.apple.com/us/album/1680053261",
        "MusicAlbum",
    )
    .await;
    let seen = run(
        &pool,
        config(Some(("hydra-id", "hydra-secret"))),
        Answers::default(),
    )
    .await;
    let tidal: Vec<&String> = seen
        .iter()
        .filter(|s| s.starts_with("openapi.tidal.com") || s.starts_with("auth.tidal.com"))
        .collect();
    // One token, one batch for the two albums; the track alone (a batch of one asks nothing).
    assert_eq!(
        tidal,
        [
            "auth.tidal.com/v1/oauth2/token?",
            "openapi.tidal.com/v2/albums?countryCode=US&filter%5Bid%5D=199574241&filter%5Bid%5D=495402659",
            "openapi.tidal.com/v2/tracks/199574242?countryCode=US",
        ],
        "{seen:?}"
    );
    assert_eq!(rows(&pool, KEYS, &hydra).await, ["upc:4018939306052 tidal"]);
    // A UPC-A barcode as GTIN-13.
    assert_eq!(
        rows(&pool, KEYS, &dracula).await,
        ["upc:0196874054246 tidal"]
    );
    assert_eq!(rows(&pool, KEYS, &track).await, ["isrc:DEQ121641300 tidal"]);
    assert_eq!(
        rows(
            &pool,
            "SELECT uri||' '||via||' '||key FROM derived_link WHERE resource_id=$1 ORDER BY uri COLLATE \"C\"",
            &hydra
        )
        .await,
        [
            "https://music.apple.com/us/album/1680053261 itunes upc:4018939306052",
            "https://www.deezer.com/album/320429237 deezer upc:4018939306052",
        ]
    );
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let group = "SELECT coalesce(group_id,'-') FROM group_unit WHERE resource_id=$1";
    let joined = rows(&pool, group, &hydra).await;
    assert_ne!(joined, ["-"]);
    assert_eq!(rows(&pool, group, &apple).await, joined);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn without_credentials_nothing_asks_tidal(pool: sqlx::PgPool) {
    let hydra = save(&pool, "https://tidal.com/album/199574241", "MusicAlbum").await;
    let seen = run(&pool, config(None), Answers::default()).await;
    assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
    assert!(rows(&pool, KEYS, &hydra).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn refused_credentials_keep_what_tidal_gave_and_retry_later(pool: sqlx::PgPool) {
    let hydra = save(&pool, "https://tidal.com/album/199574241", "MusicAlbum").await;
    // A first run with credentials TIDAL accepts.
    run(
        &pool,
        config(Some(("first-id", "first-secret"))),
        Answers::default(),
    )
    .await;
    assert_eq!(rows(&pool, KEYS, &hydra).await, ["upc:4018939306052 tidal"]);
    // Then TIDAL refuses them: one token request, no API request, the key kept, a retry queued.
    let refused = config(Some(("refused-id", "refused-secret")));
    let refuse = Answers {
        refuse: true,
        ..Default::default()
    };
    let seen = run(&pool, refused.clone(), refuse.clone()).await;
    assert_eq!(
        seen.iter()
            .filter(|s| s.contains("tidal.com"))
            .collect::<Vec<_>>(),
        ["auth.tidal.com/v1/oauth2/token?"],
        "{seen:?}"
    );
    assert_eq!(rows(&pool, KEYS, &hydra).await, ["upc:4018939306052 tidal"]);
    let queued: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM also_on_dirty WHERE resource_id=$1")
            .bind(&hydra)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(queued.as_deref(), Some("tidal_auth"));
    // The refusal is kept five minutes: the retry asks TIDAL nothing.
    let seen = run(&pool, refused, refuse).await;
    assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
}

/// The time a reference is due again, in seconds from now, with its error.
async fn due(pool: &sqlx::PgPool, id: &str) -> (Option<String>, f64) {
    sqlx::query_as("SELECT last_error, extract(epoch FROM next_attempt_at-now())::float8 FROM also_on_dirty WHERE resource_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_busy_batch_holds_the_single_requests_for_the_time_tidal_asked(pool: sqlx::PgPool) {
    let hydra = save(&pool, "https://tidal.com/album/199574241", "MusicAlbum").await;
    let dracula = save(&pool, "https://tidal.com/album/495402659", "MusicAlbum").await;
    let config = config(Some(("busy-id", "busy-secret")));
    let busy = Answers {
        batch_status: Some(429),
        ..Default::default()
    };
    // The batch answers 429 with `Retry-After: 3600`: no album asks alone.
    let seen = run(&pool, config.clone(), busy.clone()).await;
    let tidal: Vec<&String> = seen.iter().filter(|s| s.contains("tidal.com")).collect();
    assert_eq!(
        tidal,
        [
            "auth.tidal.com/v1/oauth2/token?",
            "openapi.tidal.com/v2/albums?countryCode=US&filter%5Bid%5D=199574241&filter%5Bid%5D=495402659",
        ],
        "{seen:?}"
    );
    // Each album retries after the hour TIDAL asked for, not the source's five minutes.
    for id in [&hydra, &dracula] {
        let (error, seconds) = due(&pool, id).await;
        assert_eq!(error.as_deref(), Some("tidal_busy"));
        assert!((3500.0..=3600.0).contains(&seconds), "{seconds}");
        assert!(rows(&pool, KEYS, id).await.is_empty());
    }
    // Due again at once (a restart): still within the hour, nothing asks TIDAL.
    let seen = run(&pool, config, busy).await;
    assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_forbidden_answer_is_a_refusal_of_the_credentials(pool: sqlx::PgPool) {
    let hydra = save(&pool, "https://tidal.com/album/199574241", "MusicAlbum").await;
    let config = config(Some(("forbidden-id", "forbidden-secret")));
    let forbidden = Answers {
        item_status: Some(403),
        ..Default::default()
    };
    let seen = run(&pool, config.clone(), forbidden.clone()).await;
    assert_eq!(
        seen.iter()
            .filter(|s| s.contains("tidal.com"))
            .collect::<Vec<_>>(),
        [
            "auth.tidal.com/v1/oauth2/token?",
            "openapi.tidal.com/v2/albums/199574241?countryCode=US",
        ],
        "{seen:?}"
    );
    let (error, seconds) = due(&pool, &hydra).await;
    assert_eq!(error.as_deref(), Some("tidal_auth"));
    // The refusal's hold: five minutes.
    assert!((290.0..=300.0).contains(&seconds), "{seconds}");
    assert!(rows(&pool, KEYS, &hydra).await.is_empty());
    // Within the five minutes the refusal holds, nothing asks TIDAL, not even for a token.
    let seen = run(&pool, config, forbidden).await;
    assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
}
/// Two lookups at once (the worker's concurrency) and a 429 on the first, released only once
/// this test's second lookup is pending at the pair's slot (the test fails when it never is):
/// the second then sees the wait TIDAL asked for, and exactly one request reaches TIDAL.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn two_concurrent_lookups_send_one_request_after_a_429(pool: sqlx::PgPool) {
    // An album and a track: one of each kind, so neither is batched and both ask alone.
    let album = save(&pool, "https://tidal.com/album/199574241", "MusicAlbum").await;
    let track = save(&pool, "https://tidal.com/track/199574242", "MusicRecording").await;
    let mut config = config(Some(("race-id", "race-secret")));
    config.also_on.concurrency = 2;
    let busy = Answers {
        item_status: Some(429),
        await_pending: Some(("race-id", "race-secret")),
        ..Default::default()
    };
    let seen = run(&pool, config, busy).await;
    let api: Vec<&String> = seen
        .iter()
        .filter(|s| s.starts_with("openapi.tidal.com"))
        .collect();
    assert_eq!(api.len(), 1, "{seen:?}");
    for id in [&album, &track] {
        let (error, seconds) = due(&pool, id).await;
        assert_eq!(error.as_deref(), Some("tidal_busy"));
        assert!((3500.0..=3600.0).contains(&seconds), "{seconds}");
    }
}

/// A retry within the hour asks TIDAL nothing: a busy MusicBrainz fails both tracks' lookups
/// after their batch, and on the retry the batch skips what the items have cached.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_retry_s_batch_skips_cached_items(pool: sqlx::PgPool) {
    let track = save(&pool, "https://tidal.com/track/199574242", "MusicRecording").await;
    save(&pool, "https://tidal.com/track/199574243", "MusicRecording").await;
    let config = config(Some(("cached-id", "cached-secret")));
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
        answers: Mutex::new(Answers {
            musicbrainz_status: Some(503),
            ..Default::default()
        }),
    });
    let egress = Egress::with_backends(EgressConfig::default(), Arc::new(PublicDns), t.clone());
    let seen = run_with(&pool, config.clone(), &t, &egress).await;
    // One batch for both tracks, and no single request: the batch seeded both answers.
    let tidal: Vec<&String> = seen
        .iter()
        .filter(|s| s.starts_with("openapi.tidal.com"))
        .collect();
    assert_eq!(tidal.len(), 1, "{seen:?}");
    assert!(
        tidal[0].starts_with("openapi.tidal.com/v2/tracks?"),
        "{seen:?}"
    );
    assert_eq!(
        due(&pool, &track).await.0.as_deref(),
        Some("musicbrainz_busy")
    );
    assert!(rows(&pool, KEYS, &track).await.is_empty());
    // MusicBrainz answers on the retry (its failure's cache entry expired by hand: the retry is
    // later than that in the daemon): no TIDAL request at all, and the key follows.
    t.answers_reset();
    egress
        .shorten_discovery(
            "https://musicbrainz.org/ws/2/isrc/DEQ121641300?fmt=json",
            &FetchOptions {
                accept: Some("application/json".into()),
                ..Default::default()
            },
            1,
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let seen = run_with(&pool, config, &t, &egress).await;
    assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
    assert_eq!(rows(&pool, KEYS, &track).await, ["isrc:DEQ121641300 tidal"]);
}
