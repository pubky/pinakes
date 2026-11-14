//! The `youtube_match` also-on source through Egress, against recorded YouTube, Deezer and TIDAL
//! answers (`tests/fixtures/media/youtube_match`): Queen's Topic upload of Bohemian Rhapsody
//! matches one Deezer track (unkeyed) and none on TIDAL (two candidates); a sign-in page is a
//! retry hours later; an artist's own upload asks nothing.
use pinakes_core::{
    also_on::{self, SourceOptions},
    config::Config,
    model::ResourceMeta,
    resolver::egress::{Egress, FetchOptions, Fetched, Transport},
    store,
};
use std::{
    io::Read,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
mod common;
use common::media::PublicDns;

fn fixture(name: &str) -> Vec<u8> {
    let bytes = std::fs::read(format!(
        "{}/tests/fixtures/media/youtube_match/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    if !name.ends_with(".gz") {
        return bytes;
    }
    let mut body = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice())
        .read_to_end(&mut body)
        .unwrap();
    body
}

/// Which watch page YouTube serves: the video's own, or the sign-in page it served this host
/// for another video (`watch-5NV6Rdv1a3I.html.gz`).
#[derive(Clone, Copy, Default)]
struct Answers {
    sign_in: bool,
    /// Deezer's search answers nothing.
    deezer_none: bool,
    /// TIDAL's search answers this status instead.
    tidal_status: Option<u16>,
}
/// Answers YouTube's watch pages, Deezer's search and TIDAL's (with its token) from the
/// recordings; records each request.
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
        let answer = |body: Vec<u8>, content_type: &str| Fetched {
            retry_after_secs: 120,
            ..common::fetched(url.clone(), 200, content_type.into(), body)
        };
        let query = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        Ok(match (host, url.path()) {
            ("www.youtube.com", "/watch") => {
                let video = if self.answers.sign_in {
                    "5NV6Rdv1a3I".to_owned()
                } else {
                    query("v")
                };
                answer(
                    fixture(&format!("watch-{video}.html.gz")),
                    "text/html; charset=utf-8",
                )
            }
            ("api.deezer.com", "/search") => {
                let body = match query("q").as_str() {
                    "queen bohemian rhapsody" if !self.answers.deezer_none => {
                        fixture("deezer-search-queen-bohemian-rhapsody.json")
                    }
                    _ => fixture("deezer-search-none.json"),
                };
                answer(body, "application/json")
            }
            ("auth.tidal.com", "/v1/oauth2/token") => answer(
                br#"{"access_token":"recorded-token","token_type":"Bearer","expires_in":14400}"#
                    .to_vec(),
                "application/json",
            ),
            ("openapi.tidal.com", "/v2/searchResults") if self.answers.tidal_status.is_some() => {
                Fetched {
                    status: self.answers.tidal_status.unwrap(),
                    ..answer(
                        br#"{"errors":[{"code":"TOLD"}]}"#.to_vec(),
                        "application/vnd.api+json",
                    )
                }
            }
            ("openapi.tidal.com", "/v2/searchResults") => {
                assert_eq!(query("filter[query]"), "queen bohemian rhapsody");
                assert_eq!(query("include"), "tracks.artists");
                answer(
                    fixture("tidal-search-queen-bohemian-rhapsody.json"),
                    "application/vnd.api+json",
                )
            }
            _ => panic!("unexpected request {url}"),
        })
    }
}

/// The switches (`youtube_match`, `deezer`, `tidal_isrc`) and TIDAL's pair with `tidal_isrc`.
fn config(on: bool, deezer: bool, tidal: Option<(&str, &str)>) -> Config {
    let mut config = Config::default();
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if let Some((id, secret)) = tidal {
        config.sections.set(
            "tidal",
            serde_json::json!({"client_id": id, "client_secret": secret}),
        );
    }
    for (source, enabled) in [
        ("youtube_match", on),
        ("deezer", deezer),
        ("tidal_isrc", tidal.is_some()),
    ] {
        // Only the sources this build has: a switch for one it lacks is refused.
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
/// A video's resource with its stored card, as its oEmbed (recorded) names its channel.
async fn save(pool: &sqlx::PgPool, video: &str) -> String {
    let uri = format!("https://www.youtube.com/watch?v={video}");
    let oembed: serde_json::Value =
        serde_json::from_slice(&fixture(&format!("oembed-{video}.json"))).unwrap();
    let id = store::ensure_resource(pool, &uri).await.unwrap();
    let mut doc = ResourceMeta::new(&uri, "oembed", 86400);
    doc.set_type("VideoObject");
    doc.name = oembed["title"].as_str().map(str::to_owned);
    doc.attributes.insert(
        "author".into(),
        oembed["author_name"].as_str().unwrap().to_owned(),
    );
    store::save_document(pool, &id, 0, &doc).await.unwrap();
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
        .filter(|s| s.id() == "youtube_match")
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
    sqlx::query_scalar("SELECT uri||' '||coalesce(key,'-')||' '||via||' '||coalesce(kind,'-') FROM derived_link WHERE resource_id=$1 AND source='youtube_match' ORDER BY uri")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Queen - Topic's Bohemian Rhapsody (354 s): Deezer's one track of that artist and title within
/// 2 s, unkeyed; TIDAL has two (355 s and 356 s), so none. Daft Punk's "Official Audio" on its
/// VEVO channel is no label upload: nothing is asked for it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_topic_upload_matches_one_deezer_track(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"deezer") {
        return;
    }
    let queen = save(&pool, "bSnlKl_PoQU").await;
    let vevo = save(&pool, "5NV6Rdv1a3I").await;
    let tidal = registered.contains(&"tidal_isrc");
    let seen = run(
        &pool,
        config(true, true, Some(("match-id", "match-secret"))),
        Answers::default(),
    )
    .await;
    let mut expected = vec![
        "www.youtube.com/watch?v=bSnlKl_PoQU",
        "api.deezer.com/search?q=queen+bohemian+rhapsody",
    ];
    if tidal {
        expected.extend([
            "auth.tidal.com/v1/oauth2/token?",
            "openapi.tidal.com/v2/searchResults?countryCode=US&filter%5Bquery%5D=queen+bohemian+rhapsody&include=tracks.artists",
        ]);
    }
    assert_eq!(seen, expected);
    // A match, never a key.
    assert_eq!(
        links(&pool, &queen).await,
        ["https://www.deezer.com/track/4091937401 - youtube_match maybe"]
    );
    let keys: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_key WHERE source='youtube_match'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(keys, 0);
    assert!(links(&pool, &vevo).await.is_empty());
}

/// A sign-in page instead of the video's: nothing is searched or recorded, and the lookup is
/// retried hours later.
#[sqlx::test(migrations = "../../migrations")]
async fn a_sign_in_page_is_retried_hours_later(pool: sqlx::PgPool) {
    // Deezer is the one service this test's configuration searches.
    if !also_on::registry().iter().any(|s| s.id() == "deezer") {
        return;
    }
    let queen = save(&pool, "bSnlKl_PoQU").await;
    let seen = run(
        &pool,
        config(true, true, None),
        Answers {
            sign_in: true,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(seen, ["www.youtube.com/watch?v=bSnlKl_PoQU"]);
    assert!(links(&pool, &queen).await.is_empty());
    let (error, seconds): (Option<String>, f64) = sqlx::query_as("SELECT last_error, extract(epoch FROM next_attempt_at-now())::float8 FROM also_on_dirty WHERE resource_id=$1")
        .bind(&queen)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(error.as_deref(), Some("youtube_login"));
    assert!(seconds > 5.0 * 3600.0, "{seconds}");
}

/// On by default; switched off, or on with neither Deezer nor `tidal_isrc` running, it asks
/// nothing (not even YouTube).
#[sqlx::test(migrations = "../../migrations")]
async fn without_its_switch_or_a_service_to_search_nothing_is_asked(pool: sqlx::PgPool) {
    // Deezer is the one service this test's configuration searches.
    if !also_on::registry().iter().any(|s| s.id() == "deezer") {
        return;
    }
    save(&pool, "bSnlKl_PoQU").await;
    let mut by_default = config(true, true, None);
    by_default.also_on.sources.remove("youtube_match");
    by_default.validate().unwrap();
    let seen = run(&pool, by_default, Answers::default()).await;
    assert!(
        seen.iter().any(|s| s.starts_with("www.youtube.com")),
        "{seen:?}"
    );
    for config in [config(false, true, None), config(true, false, None)] {
        let seen = run(&pool, config, Answers::default()).await;
        assert!(seen.is_empty(), "{seen:?}");
    }
}

/// TIDAL failing, by refusing the credentials (403) or with an answer the source does not expect
/// (400), leaves the run's Deezer match standing; the run comes back after five minutes.
#[sqlx::test(migrations = "../../migrations")]
async fn a_tidal_failure_keeps_the_deezer_match(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"tidal_isrc") || !registered.contains(&"deezer") {
        return;
    }
    let queen = save(&pool, "bSnlKl_PoQU").await;
    // A pair per case: a refusal holds its pair's slot.
    for (status, pair) in [
        (403, ("refused-match-id", "refused-match-secret")),
        (400, ("unexpected-match-id", "unexpected-match-secret")),
    ] {
        for sql in [
            "DELETE FROM derived_link",
            "UPDATE also_on_dirty SET attempts=0",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        run(
            &pool,
            config(true, true, Some(pair)),
            Answers {
                tidal_status: Some(status),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            links(&pool, &queen).await,
            ["https://www.deezer.com/track/4091937401 - youtube_match maybe"],
            "{status}"
        );
        let (error, seconds): (Option<String>, f64) = sqlx::query_as("SELECT last_error, extract(epoch FROM next_attempt_at-now())::float8 FROM also_on_dirty WHERE resource_id=$1")
            .bind(&queen)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(error, None, "{status}");
        assert!((290.0..=300.0).contains(&seconds), "{status}: {seconds}");
    }
}

/// TIDAL failing in a run with no Deezer match fails the run, which keeps every link an earlier
/// run found (a TIDAL one too) until the retry.
#[sqlx::test(migrations = "../../migrations")]
async fn a_tidal_failure_without_a_deezer_match_keeps_the_earlier_links(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"tidal_isrc") || !registered.contains(&"deezer") {
        return;
    }
    let queen = save(&pool, "bSnlKl_PoQU").await;
    // An earlier run's links (Deezer's match, and a TIDAL one as such a run would record it).
    run(&pool, config(true, true, None), Answers::default()).await;
    sqlx::query("INSERT INTO derived_link(resource_id,link_id,uri,site,source,via,kind,expires_at) SELECT resource_id,md5('https://tidal.com/track/534050211'),'https://tidal.com/track/534050211','TIDAL',source,via,kind,expires_at FROM derived_link WHERE resource_id=$1 AND source='youtube_match'")
        .bind(&queen)
        .execute(&pool)
        .await
        .unwrap();
    let before = links(&pool, &queen).await;
    assert_eq!(before.len(), 2, "{before:?}");
    run(
        &pool,
        config(true, true, Some(("nodeezer-id", "nodeezer-secret"))),
        Answers {
            deezer_none: true,
            tidal_status: Some(403),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(links(&pool, &queen).await, before);
    let error: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM also_on_dirty WHERE resource_id=$1")
            .bind(&queen)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(error.as_deref(), Some("tidal_auth"));
}
