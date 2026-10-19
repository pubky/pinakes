//! The "also on" machinery with fake sources: keys join groups, derived links reach cards, each
//! run replaces its source's rows, failures keep them, and nothing a page claims is a key.
use async_trait::async_trait;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Context, Found, Key, Link, Source, Subject},
    api::{self, AppState},
    config::Config,
    groups,
    model::ResourceMeta,
    resolver::egress::Egress,
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;
mod common;

/// A source whose answers the test sets per URI; it counts its calls.
#[derive(Default)]
struct Fake {
    id: &'static str,
    answers: Mutex<BTreeMap<String, anyhow::Result<Found>>>,
    calls: Mutex<Vec<String>>,
}
impl Fake {
    fn new(id: &'static str) -> Arc<Self> {
        Arc::new(Self {
            id,
            ..Default::default()
        })
    }
    fn answer(&self, uri: &str, found: anyhow::Result<Found>) {
        self.answers.lock().unwrap().insert(uri.into(), found);
    }
}
#[async_trait]
impl Source for Fake {
    fn id(&self) -> &'static str {
        self.id
    }
    fn applies(&self, subject: &Subject) -> bool {
        subject.status == "ok" && self.answers.lock().unwrap().contains_key(&subject.uri)
    }
    fn ttl(&self) -> Duration {
        Duration::from_secs(86_400)
    }
    async fn lookup(&self, _: &Context, subject: &Subject) -> anyhow::Result<Found> {
        self.calls.lock().unwrap().push(subject.uri.clone());
        match self.answers.lock().unwrap().get(&subject.uri) {
            Some(Ok(found)) => Ok(found.clone()),
            Some(Err(e)) => Err(anyhow::anyhow!("{e}")),
            None => Ok(Found::default()),
        }
    }
}

fn cx(config: Config) -> Context {
    Context {
        egress: Egress::new(config.egress.clone()),
        config: Arc::new(config),
    }
}
async fn page(pool: &PgPool, uri: &str, name: &str) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 86400);
    d.name = Some(name.into());
    d.set_type("MusicRecording");
    store::save_document(pool, &id, 0, &d).await.unwrap();
    id
}
fn key(k: &str) -> Key {
    Key {
        key: k.into(),
        via: "musicbrainz".into(),
    }
}
async fn get(app: &axum::Router, path: &str) -> Value {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let value = serde_json::from_slice(&bytes).unwrap();
    common::check_response("GET", path, status.as_u16(), &value);
    value
}
async fn drain(pool: &PgPool, cx: &Context, sources: &[Arc<dyn Source>]) {
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(pool)
        .await
        .unwrap();
    also_on::drain(pool, cx, sources, 100).await.unwrap();
}

/// Two tagged links a hub names as one recording join a group; each card lists the other first,
/// then the derived links of both, by service name, each URL once, never itself.
#[sqlx::test(migrations = "../../migrations")]
async fn hub_keys_join_and_derived_links_reach_the_cards(pool: PgPool) {
    let spotify = "https://open.spotify.com/track/abc";
    let deezer = "https://www.deezer.com/track/123";
    let a = page(&pool, spotify, "Song on Spotify").await;
    let b = page(&pool, deezer, "Song on Deezer").await;
    let hub = Fake::new("music");
    hub.answer(
        spotify,
        Ok(Found {
            keys: vec![key("mb:recording:0f2b")],
            links: vec![
                Link::new(
                    "https://music.apple.com/us/song/1",
                    "Apple Music",
                    "musicbrainz",
                ),
                Link::new("https://tidal.com/browse/track/9", "TIDAL", "musicbrainz"),
                // The other tagged member: shown once, as the member.
                Link::new(deezer, "Deezer", "musicbrainz"),
                // The card itself: never listed.
                Link::new(spotify, "Spotify", "musicbrainz"),
            ],
        }),
    );
    hub.answer(
        deezer,
        Ok(Found {
            keys: vec![key("mb:recording:0f2b")],
            links: vec![Link::new(
                "https://www.youtube.com/watch?v=x1",
                "YouTube",
                "wikidata",
            )],
        }),
    );
    let sources: Vec<Arc<dyn Source>> = vec![hub.clone()];
    let cx = cx(Config::default());
    drain(&pool, &cx, &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let card = get(&app, &format!("/v1/resources/{a}")).await;
    assert_eq!(card["group"]["size"], 2, "{card}");
    assert!(card["group"].get("members").is_none());
    let also: Vec<Value> = card["also_on"].as_array().unwrap().clone();
    assert_eq!(
        also[0],
        json!({"id": b, "uri": deezer, "site": "www.deezer.com"}),
        "the tagged member first: {card}"
    );
    let derived: Vec<(&str, &str, &str)> = also[1..]
        .iter()
        .map(|e| {
            assert_eq!(e["derived"], true, "{e}");
            assert!(e.get("id").is_none());
            (
                e["site"].as_str().unwrap(),
                e["uri"].as_str().unwrap(),
                e["via"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        derived,
        [
            (
                "Apple Music",
                "https://music.apple.com/us/song/1",
                "musicbrainz"
            ),
            ("TIDAL", "https://tidal.com/browse/track/9", "musicbrainz"),
            ("YouTube", "https://www.youtube.com/watch?v=x1", "wikidata"),
        ],
        "derived by service name, the member's links too: {card}"
    );
    // The other member's card is the mirror: the Spotify member, then the same three.
    let other = get(&app, &format!("/v1/resources/{b}")).await;
    assert_eq!(other["also_on"][0]["id"], a.as_str());
    assert_eq!(other["also_on"].as_array().unwrap().len(), 4);
    // Search and lookups carry the same list.
    let cards = post(&app, "/v1/resources/lookup", json!({"ids": [a]})).await;
    assert_eq!(cards[0]["also_on"], card["also_on"]);
    // Derived links count as nothing: no reference, no resource, no score.
    let resources: i64 = sqlx::query_scalar("SELECT count(*) FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(resources, 2);
}

async fn post(app: &axum::Router, path: &str, body: Value) -> Value {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let value = serde_json::from_slice(&bytes).unwrap();
    common::check_response("POST", path, status.as_u16(), &value);
    value
}

/// A card shows at most eight entries; the resource read shows them all.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_shows_eight_and_the_read_all(pool: PgPool) {
    let uri = "https://open.spotify.com/album/xyz";
    let id = page(&pool, uri, "Album").await;
    let hub = Fake::new("music");
    let links = (0..12)
        .map(|n| {
            Link::new(
                format!("https://service{n:02}.example/album/xyz"),
                format!("Service {n:02}"),
                "wikidata",
            )
        })
        .collect();
    hub.answer(
        uri,
        Ok(Found {
            keys: vec![],
            links,
        }),
    );
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(read["also_on"].as_array().unwrap().len(), 12);
    let cards = post(&app, "/v1/resources/lookup", json!({"ids": [id]})).await;
    let first: Vec<&str> = cards[0]["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["site"].as_str().unwrap())
        .collect();
    assert_eq!(
        first,
        (0..8)
            .map(|n| format!("Service {n:02}"))
            .collect::<Vec<_>>()
    );
}

/// Each run replaces its source's rows; a failed run keeps them and backs off; another source's
/// rows are its own; rows nobody refreshes expire; a switched-off source does not run.
#[sqlx::test(migrations = "../../migrations")]
async fn runs_replace_their_rows_failures_keep_them_and_rows_expire(pool: PgPool) {
    let uri = "https://open.spotify.com/track/r1";
    let id = page(&pool, uri, "Track").await;
    let music = Fake::new("music");
    let wiki = Fake::new("wiki");
    music.answer(
        uri,
        Ok(Found {
            keys: vec![key("mb:recording:aa")],
            links: vec![Link::new(
                "https://tidal.com/browse/track/1",
                "TIDAL",
                "musicbrainz",
            )],
        }),
    );
    wiki.answer(
        uri,
        Ok(Found {
            keys: vec![key("wikidata:Q1")],
            links: vec![Link::new(
                "https://en.wikipedia.org/wiki/T",
                "Wikipedia",
                "wikidata",
            )],
        }),
    );
    let sources: Vec<Arc<dyn Source>> = vec![music.clone(), wiki.clone()];
    let cx = cx(Config::default());
    drain(&pool, &cx, &sources).await;
    let rows = |pool: PgPool| async move {
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT source||' '||key FROM resource_key ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        let links: Vec<String> =
            sqlx::query_scalar("SELECT source||' '||uri FROM derived_link ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        (keys, links)
    };
    assert_eq!(
        rows(pool.clone()).await,
        (
            vec!["music mb:recording:aa".into(), "wiki wikidata:Q1".into()],
            vec![
                "music https://tidal.com/browse/track/1".into(),
                "wiki https://en.wikipedia.org/wiki/T".into()
            ]
        )
    );
    // The next run of `music` replaces its rows only.
    music.answer(
        uri,
        Ok(Found {
            keys: vec![key("mb:recording:bb")],
            links: vec![Link::new(
                "https://music.apple.com/us/song/2",
                "Apple Music",
                "musicbrainz",
            )],
        }),
    );
    drain(&pool, &cx, &sources).await;
    let (keys, links) = rows(pool.clone()).await;
    assert_eq!(keys, ["music mb:recording:bb", "wiki wikidata:Q1"]);
    assert_eq!(
        links,
        [
            "music https://music.apple.com/us/song/2",
            "wiki https://en.wikipedia.org/wiki/T"
        ]
    );
    // A failure keeps the rows and backs off.
    music.answer(uri, Err(anyhow::anyhow!("503 busy")));
    drain(&pool, &cx, &sources).await;
    assert_eq!(
        rows(pool.clone()).await.0,
        ["music mb:recording:bb", "wiki wikidata:Q1"]
    );
    let (attempts, later): (i32, bool) = sqlx::query_as(
        "SELECT attempts,next_attempt_at>now()+interval '4 minutes' FROM also_on_dirty WHERE resource_id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((attempts, later), (1, true));
    // A success schedules the next run at the shortest lifetime (a day here).
    music.answer(uri, Ok(Found::default()));
    drain(&pool, &cx, &sources).await;
    let (attempts, day): (i32, bool) = sqlx::query_as(
        "SELECT attempts,next_attempt_at BETWEEN now()+interval '23 hours' AND now()+interval '25 hours' FROM also_on_dirty WHERE resource_id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((attempts, day), (0, true));
    assert_eq!(
        rows(pool.clone()).await.0,
        ["wiki wikidata:Q1"],
        "an empty answer clears"
    );
    // A switched-off source does not run.
    let mut off = Config::default();
    off.also_on.sources.insert(
        "wiki".into(),
        also_on::SourceOptions {
            enabled: Some(false),
        },
    );
    let before = wiki.calls.lock().unwrap().len();
    drain(&pool, &self::cx(off), &sources).await;
    assert_eq!(wiki.calls.lock().unwrap().len(), before);
    // Rows nobody refreshes expire.
    sqlx::query("UPDATE resource_key SET expires_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE derived_link SET expires_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    assert_eq!(
        get(&app, &format!("/v1/resources/{id}")).await["also_on"],
        json!([]),
        "an expired link is not shown even before pruning"
    );
    assert_eq!(also_on::prune(&pool).await.unwrap(), 2);
    assert_eq!(rows(pool.clone()).await, (vec![], vec![]));
}

/// A source's answer is checked: only valid keys, only HTTP(S) links with a service name, the
/// caps, and with `derive_links` off no link at all (keys still join).
#[sqlx::test(migrations = "../../migrations")]
async fn answers_are_checked_and_bounded(pool: PgPool) {
    let uri = "https://open.spotify.com/track/c1";
    let id = page(&pool, uri, "Track").await;
    let found = Found {
        keys: vec![
            key("mb:recording:ok"),
            key("Bad Key"),
            key("nonamespace"),
            key("mb:recording:"),
            Key {
                key: "wikidata:Q2".into(),
                via: "Not A Token".into(),
            },
        ],
        links: (0..40)
            .map(|n| Link::new(format!("https://s{n:02}.example/x"), "S", "wikidata"))
            .chain([
                Link::new("ftp://files.example/x", "FTP", "wikidata"),
                Link::new("https://u:p@cred.example/x", "Cred", "wikidata"),
                Link::new("https://nosite.example/x", "  ", "wikidata"),
                Link::new("https://badvia.example/x", "B", "Wiki Data"),
            ])
            .collect(),
    };
    let config = Config::default();
    also_on::record(
        &pool,
        &id,
        "music",
        found.clone(),
        Duration::from_secs(60),
        &config.also_on,
    )
    .await
    .unwrap();
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM resource_key")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(keys, ["mb:recording:ok"]);
    let links: Vec<String> = sqlx::query_scalar("SELECT uri FROM derived_link ORDER BY uri")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        links,
        (0..32)
            .map(|n| format!("https://s{n:02}.example/x"))
            .collect::<Vec<_>>(),
        "the source's first 32 valid links; the credentialed one refused"
    );
    let mut keys_only = Config::default();
    keys_only.also_on.derive_links = false;
    also_on::record(
        &pool,
        &id,
        "music",
        found,
        Duration::from_secs(60),
        &keys_only.also_on,
    )
    .await
    .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM derived_link")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    for bad in ["Music", "a-b", "", &"x".repeat(33)] {
        assert!(
            also_on::record(
                &pool,
                &id,
                bad,
                Found::default(),
                Duration::from_secs(60),
                &config.also_on
            )
            .await
            .is_err(),
            "{bad}"
        );
    }
    assert!(also_on::valid_key("mb:release-group:1f2e"));
    assert!(also_on::valid_key("wikidata:Q42"));
    assert!(!also_on::valid_key("MB:x"));
}

/// A saved document makes its sources due; an unchanged refetch does not; an alias never runs;
/// the config refuses a source it does not know.
#[sqlx::test(migrations = "../../migrations")]
async fn saves_make_sources_due_and_unknown_sources_are_refused(pool: PgPool) {
    let uri = "https://open.spotify.com/track/d1";
    let id = page(&pool, uri, "Track").await;
    let due = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM also_on_dirty")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(due(pool.clone()).await, 1);
    let hub = Fake::new("music");
    let sources: Vec<Arc<dyn Source>> = vec![hub.clone()];
    drain(&pool, &cx(Config::default()), &sources).await;
    assert_eq!(
        due(pool.clone()).await,
        0,
        "no source applies: nothing left due"
    );
    // A refetch that changes nothing but the fetch time leaves it alone.
    sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{fetched_at}','1') WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(due(pool.clone()).await, 0);
    sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{name}','\"Renamed\"') WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(due(pool.clone()).await, 1);
    // An alias spelling is never a subject.
    let alias = store::ensure_resource(&pool, &format!("{uri}?utm_source=x"))
        .await
        .unwrap();
    assert_ne!(alias, id);
    hub.answer(&format!("{uri}?utm_source=x"), Ok(Found::default()));
    drain(&pool, &cx(Config::default()), &sources).await;
    assert!(!hub
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|u| u.contains("utm_source")));
    let unknown =
        toml::from_str::<Config>("[also_on.sources.nope]\nenabled = false\n").map(|c| c.validate());
    assert!(matches!(unknown, Ok(Err(_))), "{unknown:?}");
}
