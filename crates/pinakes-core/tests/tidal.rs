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
}
/// Answers TIDAL (its token and API), MusicBrainz's ISRC lookup, iTunes and Deezer from the
/// recordings, or as `answers` says; records each request.
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
                if self.answers.refuse {
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
                    "/v2/albums" | "/v2/tracks" => self.answers.batch_status,
                    _ => self.answers.item_status,
                };
                if let Some(status) = told {
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
                    "/v2/tracks" if filters == ["199574242"] => {
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
                404,
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
/// Every resource's sources, once; the requests made.
async fn run(pool: &sqlx::PgPool, config: Config, answers: Answers) -> Vec<String> {
    let t = Arc::new(Recorded {
        seen: Mutex::default(),
        answers,
    });
    let cx = also_on::Context {
        egress: Egress::with_backends(EgressConfig::default(), Arc::new(PublicDns), t.clone()),
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
const KEYS: &str = "SELECT key||' '||source FROM resource_key WHERE resource_id=$1 ORDER BY key";

#[sqlx::test(migrations = "../../migrations")]
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
            "SELECT uri||' '||via||' '||key FROM derived_link WHERE resource_id=$1 ORDER BY uri",
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

#[sqlx::test(migrations = "../../migrations")]
async fn without_credentials_nothing_asks_tidal(pool: sqlx::PgPool) {
    let hydra = save(&pool, "https://tidal.com/album/199574241", "MusicAlbum").await;
    let seen = run(&pool, config(None), Answers::default()).await;
    assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
    assert!(rows(&pool, KEYS, &hydra).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
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

#[sqlx::test(migrations = "../../migrations")]
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
    assert!(seconds > 3000.0, "{seconds}");
    assert!(rows(&pool, KEYS, &hydra).await.is_empty());
    // Within the five minutes the refusal holds, nothing asks TIDAL, not even for a token.
    let seen = run(&pool, config, forbidden).await;
    assert!(!seen.iter().any(|s| s.contains("tidal.com")), "{seen:?}");
}
