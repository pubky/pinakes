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
    /// A service asked to wait this long (`RetryLater`), per URI.
    waits: Mutex<BTreeMap<String, u64>>,
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
    fn busy(&self, uri: &str, seconds: u64) {
        self.answer(uri, Ok(Found::default()));
        self.waits.lock().unwrap().insert(uri.into(), seconds);
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
        if let Some(seconds) = self.waits.lock().unwrap().get(&subject.uri) {
            return Err(pinakes_core::resolver::RetryLater {
                class: "fake_busy",
                seconds: *seconds,
            }
            .into());
        }
        match self.answers.lock().unwrap().get(&subject.uri) {
            Some(Ok(found)) => Ok(found.clone()),
            Some(Err(e)) => Err(anyhow::anyhow!("{e}")),
            None => Ok(Found::default()),
        }
    }
}

/// Whether this build has each lookup family (`lookups::FAMILIES`): a test of how a family's
/// services take their place on a card returns early without the family, and with it asserts
/// the names (`named`), never the other way round.
fn families(names: &[&str]) -> bool {
    let registered: Vec<&str> = pinakes_core::lookups::FAMILIES
        .iter()
        .map(|f| f.name())
        .collect();
    names.iter().all(|name| registered.contains(name))
}
/// Each link's service is named (`lookups::service_name`): asserted once its family is in the
/// build, so a classification that stops naming a page fails the test instead of skipping it.
fn named(uris: &[&str]) {
    for uri in uris {
        assert!(
            pinakes_core::lookups::service_name(uri).is_some(),
            "{uri} is on no service"
        );
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
            ..Default::default()
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
            ..Default::default()
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
        json!({"id": b, "uri": deezer, "site": "Deezer"}),
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
            // Each under its root spelling.
            (
                "Apple Music",
                "https://music.apple.com/song/1",
                "musicbrainz"
            ),
            ("TIDAL", "https://tidal.com/track/9", "musicbrainz"),
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

/// The card as `POST /v1/resources/lookup` shows it: one `also_on` entry per service, where a
/// resource read lists every link.
async fn lookup_card(app: &axum::Router, id: &str) -> Value {
    let cards = post(app, "/v1/resources/lookup", json!({"ids": [id]})).await;
    cards.as_array().unwrap()[0].clone()
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
            ..Default::default()
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
            ..Default::default()
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
            ..Default::default()
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
                "music https://tidal.com/track/1".into(),
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
            ..Default::default()
        }),
    );
    drain(&pool, &cx, &sources).await;
    let (keys, links) = rows(pool.clone()).await;
    assert_eq!(keys, ["music mb:recording:bb", "wiki wikidata:Q1"]);
    assert_eq!(
        links,
        [
            "music https://music.apple.com/song/2",
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
            ..Default::default()
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
        ..Default::default()
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
    // A value may hold `:` (a DOI); the namespace before it is still checked.
    assert!(also_on::valid_key("doi:10.46298/lmcs-20(1:8)2024"));
    assert!(!also_on::valid_key("Doi:10.46298/lmcs-20(1:8)2024"));
    assert!(!also_on::valid_key("doi:"));
    assert!(!also_on::valid_key(":10.1000/x"));
    assert!(!also_on::valid_key("doi:10.1000/a b"));
}

/// A saved document makes its sources due; an unchanged refetch does not; an alias never runs;
/// the config refuses a source it does not know.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
    // A MusicBrainz mirror: an http(s) web service root; no other source takes a base URL. The
    // setting is the musicbrainz source's: nothing to check in a build without it.
    if !also_on::registry().iter().any(|s| s.id() == "musicbrainz") {
        return;
    }
    let base = |id: &str, url: &str| {
        toml::from_str::<Config>(&format!("[also_on.sources.{id}]\nbase_url = \"{url}\"\n"))
            .unwrap()
    };
    let mirror = base("musicbrainz", "http://mb.example:5000/ws/2");
    mirror.validate().unwrap();
    assert_eq!(
        mirror.also_on.musicbrainz_base(),
        "http://mb.example:5000/ws/2"
    );
    assert_eq!(
        Config::default().also_on.musicbrainz_base(),
        "https://musicbrainz.org/ws/2"
    );
    for (id, url) in [
        ("musicbrainz", "http://mb.example/ws/2/"),
        ("musicbrainz", "ftp://mb.example/ws/2"),
        ("musicbrainz", "https://user:pw@mb.example/ws/2"),
        ("musicbrainz", "https://mb.example/ws/2?x=1"),
        ("wikidata", "https://wd.example/w/api.php"),
    ] {
        assert!(base(id, url).validate().is_err(), "{id} {url}");
    }
}

/// Every spelling of one music object is an alias of its canonical URL (`service`), so the
/// card and its references are the one resource's.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn music_link_spellings_are_one_resource(pool: PgPool) {
    let canonical = "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq";
    let root = store::ensure_resource(&pool, canonical).await.unwrap();
    for variant in [
        "https://open.spotify.com/intl-de/track/6LgJvl0Xdtc73RJ1mmpotq?si=abc",
        "spotify:track:6LgJvl0Xdtc73RJ1mmpotq",
    ] {
        let id = store::ensure_resource(&pool, variant).await.unwrap();
        let (root_id, basis): (String, String) =
            sqlx::query_as("SELECT root_id,basis FROM resource_alias WHERE alias_id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (root_id.as_str(), basis.as_str()),
            (root.as_str(), "service"),
            "{variant}"
        );
    }
    let video = store::ensure_resource(&pool, "https://youtu.be/dQw4w9WgXcQ")
        .await
        .unwrap();
    let canonical_video =
        pinakes_core::model::identity("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
            .unwrap()
            .0;
    let root_of: String =
        sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
            .bind(&video)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(root_of, canonical_video);
}

/// A source's facts reach the read's `details.facts` (several values of one name joined), are
/// checked like everything else, and never join anything.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn facts_are_shown_checked_and_never_join(pool: PgPool) {
    use pinakes_core::also_on::Fact;
    let (a, b) = (
        "https://open.spotify.com/track/aaaaaaaaaaaaaaaaaaaaaa",
        "https://www.deezer.com/track/1",
    );
    let (ia, ib) = (page(&pool, a, "Song").await, page(&pool, b, "Song").await);
    let hub = Fake::new("music");
    let facts = vec![
        Fact::new("isrc", "GBAYE9701376"),
        Fact::new("isrc", "GBBKS1700108"),
        Fact::new("artist", "Radiohead"),
        // Refused: an upper-case name, a padded value, a control character, an empty value.
        Fact::new("ISRC", "X"),
        Fact::new("upc", " 123 "),
        Fact::new("note", "a\u{7}b"),
        Fact::new("empty", ""),
    ];
    for uri in [a, b] {
        hub.answer(
            uri,
            Ok(Found {
                facts: facts.clone(),
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = get(&app, &format!("/v1/resources/{ia}")).await;
    assert_eq!(
        read["facts"],
        json!({"artist": "Radiohead", "isrc": "GBAYE9701376; GBBKS1700108"})
    );
    // The same ISRC on both links joins nothing: a fact is not a key.
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let grouped: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM group_unit WHERE resource_id IN ($1,$2) AND group_id IS NOT NULL",
    )
    .bind(&ia)
    .bind(&ib)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grouped, 0);
    // A link without facts has none.
    let other = page(&pool, "https://tidal.com/track/9", "Song").await;
    assert!(get(&app, &format!("/v1/resources/{other}")).await["facts"].is_null());
}

/// `key` finds every tagged link carrying one exact identifier, on any service: an MBID at any
/// level, a Q-id, an ISRC or a barcode. Malformed keys are refused; the lookup is indexed.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn search_by_key_finds_the_same_object_on_every_service(pool: PgPool) {
    use pinakes_core::also_on::Fact;
    let mbid = "9f9cf187-d6f9-437f-9d98-d59cdbd52757";
    let links = [
        "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
        "https://www.deezer.com/track/138539973",
        "https://tidal.com/track/1",
    ];
    let mut ids = vec![];
    let hub = Fake::new("music");
    for (n, uri) in links.iter().enumerate() {
        ids.push(page(&pool, uri, "Paranoid Android").await);
        // The first two are the recording; the third is something else.
        let (key, isrc) = if n < 2 {
            (format!("mb:recording:{mbid}"), "GBAYE9701376")
        } else {
            (
                "mb:recording:00000000-0000-0000-0000-000000000001".into(),
                "USAAA0000001",
            )
        };
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![Key {
                    key,
                    via: "musicbrainz".into(),
                }],
                facts: vec![Fact::new("isrc", isrc)],
                ..Default::default()
            }),
        );
    }
    sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT id,repeat('a',52),'tag','/pub/test/tags/'||id,'music',now() FROM unnest($1::text[]) id")
        .bind(&ids)
        .execute(&pool)
        .await
        .unwrap();
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let found = |path: String| {
        let app = app.clone();
        async move {
            let mut rows: Vec<String> = get(&app, &path).await["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_owned())
                .collect();
            rows.sort();
            rows
        }
    };
    let mut both = ids[..2].to_vec();
    both.sort();
    assert_eq!(found(format!("/v1/search?key=mbid:{mbid}")).await, both);
    assert_eq!(
        found(format!("/v1/search?key=mbid:{}", mbid.to_uppercase())).await,
        both
    );
    // An ISRC in any letter case.
    assert_eq!(found("/v1/search?key=isrc:gbaye9701376".into()).await, both);
    assert!(found("/v1/search?key=wikidata:Q202996".into())
        .await
        .is_empty());
    // With another filter: still the same object only.
    assert_eq!(
        found(format!("/v1/search?key=mbid:{mbid}&host=www.deezer.com")).await,
        [ids[1].clone()]
    );
    for bad in [
        "mbid:nope",
        "isrc:ABC",
        "upc:12a",
        "wikidata:Q0",
        "spotify:track:x",
        "mbid",
    ] {
        let mut request = Request::builder()
            .uri(format!("/v1/search?key={bad}"))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let status = app.clone().oneshot(request).await.unwrap().status();
        assert_eq!(status, 400, "{bad}");
    }
    // The key and fact lookups use their indexes.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan=off")
        .execute(&mut *tx)
        .await
        .unwrap();
    for (query, index) in [
        (
            "EXPLAIN SELECT resource_id FROM resource_key WHERE key=ANY(ARRAY['mb:recording:x'])",
            "resource_key_key_idx",
        ),
        (
            "EXPLAIN SELECT resource_id FROM resource_fact WHERE name='isrc' AND value='X'",
            "resource_fact_value_idx",
        ),
    ] {
        let plan: Vec<String> = sqlx::query_scalar(query).fetch_all(&mut *tx).await.unwrap();
        assert!(plan.join("\n").contains(index), "{index}: {plan:?}");
    }
}

/// A stream an also-on source heard of reaches the card through its facts: the hints read it
/// while the source is on and its rows live, the card takes it, and a changed stream outdates
/// the stored card (a 304 then reads it again in full).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_heard_stream_reaches_the_card(pool: PgPool) {
    use also_on::media::{outdated, type_card, type_hints};
    let uri = "https://podcasts.apple.com/us/podcast/id1584274529?i=1000796718231";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "media", 86400);
    doc.set_type("PodcastEpisode");
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let enclosure = "https://op3.dev/e/mp3s.nashownotes.com/PC20-275-2026-10-16-Final.mp3";
    for (name, value) in [
        ("stream_url", enclosure),
        ("stream_type", "audio/mpeg"),
        ("stream_duration", "5995"),
    ] {
        sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) VALUES($1,$2,$3,'podcasts',now()+interval '1 day')")
            .bind(&id)
            .bind(name)
            .bind(value)
            .execute(&pool)
            .await
            .unwrap();
    }
    let hints = |pool: PgPool, id: String| async move {
        type_hints(&pool, std::slice::from_ref(&id))
            .await
            .unwrap()
            .remove(&id)
            .unwrap_or_default()
    };
    let heard = pinakes_core::model::Stream::public(enclosure, "audio/mpeg", Some(5995), false);
    let now = hints(pool.clone(), id.clone()).await;
    assert_eq!(
        now.stream.clone().map(|(s, stream)| (s, Some(stream))),
        Some(("podcasts".into(), heard.clone()))
    );
    type_card(uri, &now, &mut doc);
    assert_eq!(doc.stream, heard);
    assert!(!outdated(&pool, &id, &doc).await.unwrap());
    // The episode's audio moved: the stored card is outdated.
    sqlx::query("UPDATE resource_fact SET value='https://op3.dev/e/mp3s.nashownotes.com/PC20-275-v2.mp3' WHERE resource_id=$1 AND name='stream_url'")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(outdated(&pool, &id, &doc).await.unwrap());
    // A switched-off source's stream is none, and an expired row's.
    sqlx::query("INSERT INTO also_on_source_off(source) VALUES('podcasts')")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(hints(pool.clone(), id.clone()).await.stream, None);
    sqlx::query("DELETE FROM also_on_source_off")
        .execute(&pool)
        .await
        .unwrap();
    assert!(hints(pool.clone(), id.clone()).await.stream.is_some());
    sqlx::query(
        "UPDATE resource_fact SET expires_at=now()-interval '1 second' WHERE resource_id=$1",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(hints(pool.clone(), id.clone()).await.stream, None);
}

/// Every key a resource read lists (`keys`, as stored) searches back to that resource:
/// `GET /v1/search?key=` takes each exactly as listed (review of d6d0aeb: MusicBrainz and podcast
/// keys did not; of e48807e: Open Library keys did not).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn every_key_a_read_lists_searches_back(pool: PgPool) {
    let uri = "https://example.org/a-show";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type("PodcastSeries");
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) VALUES($1,repeat('a',52),'tag','/pub/test/tags/1','listen',now())")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    // One key of each form the core and the hubs store, and one of each form every registered
    // source lists (`Source::key_forms`: the books source's Open Library keys among them).
    let core = [
        "mb:recording:0f2b3d4e-1111-4222-8333-444455556666",
        "mb:release-group:0f2b3d4e-1111-4222-8333-444455556667",
        "mb:artist:0f2b3d4e-1111-4222-8333-444455556668",
        "podcast:guid:917393e3-1b1e-5cef-ace4-edaa54e1f810",
        "podcast:apple:1234567890",
        "podcast:apple_episode:1000123456789",
        "podcast:item:917393e3-1b1e-5cef-ace4-edaa54e1f810:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        "isrc:GBUM71029604",
        "upc:0602557656527",
        "wikidata:Q42",
        "imdb:tt0111161",
        "osm:node/1",
        "doi:10.1000/xyz123",
    ];
    let mut keys: Vec<String> = core.iter().map(|k| k.to_string()).collect();
    for source in also_on::registered() {
        for (namespace, example) in source.key_forms() {
            let key = format!("{namespace}:{example}");
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    for key in &keys {
        sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,$2,'test','test',now()+interval '1 day')")
            .bind(&id)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
    }
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = get(&app, &format!("/v1/resources/{id}")).await;
    let listed: Vec<&str> = read["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert_eq!(listed.len(), keys.len(), "{read}");
    for key in listed {
        let path = format!(
            "/v1/search?key={}",
            url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>()
        );
        let page = get(&app, &path).await;
        let found: Vec<&str> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(found, [id.as_str()], "{key}");
    }
    // An episode's enclosure key is found by the file's own URL too, whatever its fragment.
    let file = "https://cdn.example.org/episode-1.mp3";
    let enclosure = also_on::media::enclosure_key(file).unwrap();
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,$2,'test','test',now()+interval '1 day')")
        .bind(&id)
        .bind(&enclosure)
        .execute(&pool)
        .await
        .unwrap();
    for asked in [enclosure.clone(), format!("podcast:enclosure:{file}#t=30")] {
        let path = format!(
            "/v1/search?key={}",
            url::form_urlencoded::byte_serialize(asked.as_bytes()).collect::<String>()
        );
        let page = get(&app, &path).await;
        assert_eq!(page["items"][0]["id"], id.as_str(), "{asked}: {page}");
    }
}

/// `sort=trending` ranks by the references made inside the window (7 days by default), shows
/// only links referenced in it, combines with tags and `collapse=group` (one song across
/// services is one row), and refuses windows outside 1h–90d.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn trending_counts_references_inside_the_window(pool: PgPool) {
    let (spotify, deezer, old) = (
        "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
        "https://www.deezer.com/track/138539973",
        "https://tidal.com/track/1",
    );
    let ids = [
        page(&pool, spotify, "Song").await,
        page(&pool, deezer, "Song").await,
        page(&pool, old, "Old song").await,
    ];
    // Spotify: three this week. Deezer: five three weeks ago and one today. Old: two in spring.
    for (id, days_ago, count, label) in [
        (&ids[0], 1, 3, "rock"),
        (&ids[1], 21, 5, "rock"),
        (&ids[1], 0, 1, "rock"),
        (&ids[2], 60, 2, "jazz"),
    ] {
        sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT $1,repeat(chr(97+n%20),52),'tag','/pub/test/tags/'||$1||'/'||$2||'/'||n,$4,now()-$2*interval '1 day' FROM generate_series(1,$3) n")
            .bind(id)
            .bind(days_ago)
            .bind(count)
            .bind(label)
            .execute(&pool)
            .await
            .unwrap();
    }
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let rows = |path: &'static str| {
        let app = app.clone();
        async move {
            get(&app, path).await["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        rows("/v1/search?sort=trending").await,
        [ids[0].clone(), ids[1].clone()]
    );
    assert_eq!(
        rows("/v1/search?sort=trending&window=30d").await,
        [ids[1].clone(), ids[0].clone()]
    );
    assert_eq!(
        rows("/v1/search?sort=trending&window=90d&tags=jazz").await,
        [ids[2].clone()]
    );
    // Within the last hour: only today's Deezer reference (made at the start of the test).
    assert_eq!(
        rows("/v1/search?sort=trending&window=1h").await,
        [ids[1].clone()]
    );
    // The two services' links are one recording: collapsed, one row.
    let hub = Fake::new("music");
    for uri in [spotify, deezer] {
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![key("mb:recording:9f9cf187-d6f9-437f-9d98-d59cdbd52757")],
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    assert_eq!(
        rows("/v1/search?sort=trending&collapse=group").await.len(),
        1
    );
    // A collapsed row ranks by its group's references, not by its member's: three more on Deezer
    // today make the song 3 + 4 in the window, above a lone link with 5 (Deezer, with the most
    // references overall, is the row).
    let lone = page(&pool, "https://tidal.com/track/2", "Lone song").await;
    for (id, count) in [(&ids[1], 3), (&lone, 5)] {
        sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT $1,repeat(chr(97+n%20),52),'tag','/pub/test/tags/'||$1||'/today/'||n,'pop',now() FROM generate_series(1,$2) n")
            .bind(id)
            .bind(count)
            .execute(&pool)
            .await
            .unwrap();
    }
    assert_eq!(
        rows("/v1/search?sort=trending&collapse=group").await,
        [ids[1].clone(), lone.clone()]
    );
    // Uncollapsed, each link keeps its own count.
    assert_eq!(
        rows("/v1/search?sort=trending").await,
        [lone, ids[1].clone(), ids[0].clone()]
    );
    for bad in [
        "sort=trending&window=0h",
        "sort=trending&window=91d",
        "sort=trending&window=7w",
        "sort=trending&window=d",
        "sort=activity&window=7d",
        "window=7d",
    ] {
        let mut request = Request::builder()
            .uri(format!("/v1/search?{bad}"))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
        ));
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            400,
            "{bad}"
        );
    }
    // The window's references are found by their time index.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan=off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let plan: Vec<String> = sqlx::query_scalar(
        "EXPLAIN SELECT resource_id,count(*) FROM effective_reference WHERE created_at>=now()-interval '7 days' GROUP BY resource_id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert!(
        plan.join("\n").contains("reference_created_idx"),
        "{plan:?}"
    );
}

/// An artist page is a `MusicGroup`, or a `Person` once MusicBrainz says the artist is one
/// (`artist_type`): the next resolve types it so.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_artist_is_a_person_when_musicbrainz_says_so(pool: PgPool) {
    use pinakes_core::{also_on::Fact, config::QueueConfig, queue, resolver::Resolver};
    struct Page;
    #[async_trait]
    impl Resolver for Page {
        fn name(&self) -> &'static str {
            "web"
        }
        fn matches(&self, _: &str) -> bool {
            true
        }
        async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
            let mut d = ResourceMeta::new(uri, "web", 3600);
            d.name = Some("Thom Yorke".into());
            Ok(d)
        }
        fn refresh_after(&self, _: &ResourceMeta) -> Duration {
            Duration::from_secs(3600)
        }
    }
    let uri = "https://www.deezer.com/artist/1";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let resolve = || async {
        sqlx::query("UPDATE resource SET next_attempt_at=now(),final=false WHERE id=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let job = queue::claim(&pool, "web", &QueueConfig::default())
            .await
            .unwrap()
            .expect("due");
        queue::run_job(&pool, &Page, &QueueConfig::default(), &job)
            .await
            .unwrap();
        sqlx::query_scalar::<_, String>("SELECT type FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(resolve().await, "MusicGroup");
    pinakes_core::also_on::record(
        &pool,
        &id,
        "musicbrainz",
        Found {
            facts: vec![Fact::new("artist_type", "Person")],
            ..Default::default()
        },
        Duration::from_secs(86400),
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(resolve().await, "Person");
}

/// The production queue (batched claims, `run_batch`) types and names every music link by what
/// it is, whatever its page says: the links the candidate showed as VideoObject,
/// MusicComposition, WebApplication, WebPage, AudioObject or an album.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_production_queue_types_and_names_music_links(pool: PgPool) {
    use pinakes_core::{config::QueueConfig, queue, resolver::Resolver};
    /// Each page as the live service describes itself.
    struct Pages;
    #[async_trait]
    impl Resolver for Pages {
        fn name(&self) -> &'static str {
            "web"
        }
        fn matches(&self, _: &str) -> bool {
            true
        }
        async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
            let (kind, site) = match uri {
                u if u.contains("spotify") => ("VideoObject", "Spotify"),
                u if u.contains("apple") => ("MusicComposition", "Apple Music - Web Player"),
                u if u.contains("tidal") => ("WebApplication", "Music on TIDAL"),
                u if u.contains("bandcamp") => ("MusicAlbum", "Radiohead"),
                u if u.contains("soundcloud") => ("AudioObject", "SoundCloud"),
                u if u.contains("youtube") => ("VideoObject", "www.youtube.com"),
                _ => ("WebPage", ""),
            };
            let mut d = ResourceMeta::new(uri, "web", 3600);
            d.set_type(kind);
            d.site = site.into();
            d.name = Some("Radiohead".into());
            Ok(d)
        }
        fn refresh_after(&self, _: &ResourceMeta) -> Duration {
            Duration::from_secs(3600)
        }
    }
    let expected = [
        (
            "https://open.spotify.com/track/003vvx7Niy0yvhvHt4a68B",
            "MusicRecording",
            "Spotify",
        ),
        (
            "https://music.apple.com/gb/song/1440717826",
            "MusicRecording",
            "Apple Music",
        ),
        (
            "https://open.qobuz.com/track/443937120",
            "MusicRecording",
            "Qobuz",
        ),
        (
            "https://music.amazon.com/tracks/B0H9CSQQ1L",
            "MusicRecording",
            "Amazon Music",
        ),
        (
            "https://open.qobuz.com/album/0634904078164",
            "MusicAlbum",
            "Qobuz",
        ),
        ("https://tidal.com/artist/64518", "MusicGroup", "TIDAL"),
        ("https://radiohead.bandcamp.com/", "MusicGroup", "Bandcamp"),
        (
            "https://soundcloud.com/radiohead",
            "MusicGroup",
            "SoundCloud",
        ),
        (
            "https://music.amazon.co.uk/artists/B000QJXOGC",
            "MusicGroup",
            "Amazon Music",
        ),
        (
            "https://www.youtube.com/watch?v=m2zUrruKjDQ",
            "VideoObject",
            "YouTube",
        ),
        (
            "https://www.youtube.com/channel/UCr_iyUANcn9OX_yy9piYoLw",
            "MusicGroup",
            "YouTube",
        ),
    ];
    let mut ids = vec![];
    for (uri, ..) in expected {
        ids.push(store::ensure_resource(&pool, uri).await.unwrap());
    }
    sqlx::query("UPDATE resource SET resolver='web' WHERE id=ANY($1)")
        .bind(&ids)
        .execute(&pool)
        .await
        .unwrap();
    let cfg = QueueConfig::default();
    loop {
        let jobs = queue::claim_batch(&pool, "web", &cfg, 4).await.unwrap();
        if jobs.is_empty() {
            break;
        }
        queue::run_batch(&pool, &Pages, &cfg, &jobs).await.unwrap();
    }
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    for ((uri, kind, site), id) in expected.iter().zip(&ids) {
        let card = get(&app, &format!("/v1/resources/{id}")).await;
        assert_eq!(
            (card["type"].as_str(), card["site"].as_str()),
            (Some(*kind), Some(*site)),
            "{uri}"
        );
    }
}

/// Wikidata has wrong IDs: two links of different recordings that share a Q-id stay apart, a
/// third with only the Q-id joins one of them, and the refusal is counted.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_shared_q_id_never_joins_two_recordings(pool: PgPool) {
    let (a, b, c) = (
        "https://example.org/a",
        "https://example.org/b",
        "https://example.org/c",
    );
    let mut ids = vec![];
    let hub = Fake::new("music");
    for (uri, keys) in [
        (a, vec!["wikidata:Q1", "mb:recording:aaaa"]),
        (b, vec!["wikidata:Q1", "mb:recording:bbbb"]),
        (c, vec!["wikidata:Q1"]),
    ] {
        ids.push(page(&pool, uri, "Song").await);
        hub.answer(
            uri,
            Ok(Found {
                keys: keys.into_iter().map(key).collect(),
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let group = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT group_id FROM group_unit WHERE resource_id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (ga, gb, gc) = (
        group(ids[0].clone()).await,
        group(ids[1].clone()).await,
        group(ids[2].clone()).await,
    );
    assert!(gc.is_some(), "the Q-id alone joins one of them");
    assert_ne!(ga, gb, "two recordings never share a group");
    assert!(gc == ga || gc == gb);
    let refused: i64 = sqlx::query_scalar("SELECT count(*) FROM group_input WHERE key_conflicts>0")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(refused > 0);
    let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    assert!(metrics.group_key_conflicts.get() > 0);
}

/// A removed source's keys keep their rule: the database remembers every single-object namespace
/// a source registered (`groups::configure`), so two objects with distinct IDs in a namespace
/// whose source this build no longer has, sharing a barcode, stay two groups; the same pair in a
/// namespace nobody ever registered is one.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_removed_source_s_keys_keep_their_single_object_rule(pool: PgPool) {
    // What the daemon's start writes: this build's namespaces, each with its source.
    groups::configure(&pool, &Default::default()).await.unwrap();
    let remembered: Vec<(String, String)> =
        sqlx::query_as("SELECT namespace,source FROM single_object_namespace ORDER BY namespace")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        remembered.contains(&("mb:recording".into(), "groups".into())),
        "{remembered:?}"
    );
    for source in also_on::registered() {
        for namespace in source.single_namespaces() {
            assert!(
                remembered.contains(&((*namespace).into(), source.id().into())),
                "{namespace} of {}",
                source.id()
            );
        }
    }
    // A source gone from this build registered `gone:release` once; its keys are still stored.
    sqlx::query(
        "INSERT INTO single_object_namespace(namespace,source) VALUES('gone:release','gone')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let hub = Fake::new("music");
    let pairs = [
        [
            (
                "https://example.org/gone-1",
                vec!["gone:release:1", "upc:0000000000017"],
            ),
            (
                "https://example.org/gone-2",
                vec!["gone:release:2", "upc:0000000000017"],
            ),
        ],
        [
            (
                "https://example.org/nobody-1",
                vec!["nobody:1", "upc:0000000000024"],
            ),
            (
                "https://example.org/nobody-2",
                vec!["nobody:2", "upc:0000000000024"],
            ),
        ],
    ];
    let mut ids = vec![];
    for pair in &pairs {
        let mut these = vec![];
        for (uri, keys) in pair {
            these.push(page(&pool, uri, "Album").await);
            hub.answer(
                uri,
                Ok(Found {
                    keys: keys.iter().map(|k| key(k)).collect(),
                    ..Default::default()
                }),
            );
        }
        ids.push(these);
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let group = |ids: &[String]| {
        let pool = pool.clone();
        let ids = ids.to_vec();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT g.group_id FROM unnest($1::text[]) i(id) LEFT JOIN group_unit g ON g.resource_id=i.id",
            )
            .bind(&ids)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    // Apart: a unit alone has no group, so neither has one, or they differ.
    let gone = group(&ids[0]).await;
    assert!(
        gone[0].is_none() || gone[0] != gone[1],
        "two objects of the removed source's namespace stay apart: {gone:?}"
    );
    let nobody = group(&ids[1]).await;
    assert!(
        nobody[0].is_some() && nobody[0] == nobody[1],
        "a namespace nobody registered joins on the barcode: {nobody:?}"
    );
    let refused: i64 = sqlx::query_scalar("SELECT count(*) FROM group_input WHERE key_conflicts>0")
        .fetch_one(&pool)
        .await
        .unwrap();
    // The refusal is the removed source's pair's: the other pair joined.
    assert!(refused >= 1, "{refused}");
}

/// The lookups' own namespaces name one object each too: an OSM object, a crate, a Steam game.
/// Two of them sharing a second key (an item naming a node and a relation, one repository behind
/// two crates, an item with two appids) never share a group.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_shared_key_never_joins_two_places_crates_or_games(pool: PgPool) {
    let hub = Fake::new("lookups");
    // Each case with the family that records its namespace (OSM's is built in): a case runs
    // whenever its family is in the build, and asserts the namespace is declared single-object,
    // so a declaration lost from a registered family fails here instead of dropping the case.
    let cases = [
        (
            None,
            "osm",
            [
                (
                    "https://example.org/node",
                    vec!["osm:node/240109189", "wikidata:Q64"],
                ),
                (
                    "https://example.org/relation",
                    vec!["osm:relation/62422", "wikidata:Q64"],
                ),
            ],
        ),
        (
            Some("packages"),
            "crate",
            [
                (
                    "https://example.org/rand",
                    vec!["crate:rand", "repo:github.com/rust-random/rand"],
                ),
                (
                    "https://example.org/rand-core",
                    vec!["crate:rand-core", "repo:github.com/rust-random/rand"],
                ),
            ],
        ),
        (
            Some("games"),
            "steam",
            [
                ("https://example.org/app-1", vec!["steam:1", "wikidata:Q2"]),
                ("https://example.org/app-2", vec!["steam:2", "wikidata:Q2"]),
            ],
        ),
    ];
    let cases: Vec<_> = cases
        .into_iter()
        .filter(|(family, _, _)| family.is_none_or(|f| families(&[f])))
        .collect();
    for (family, namespace, _) in &cases {
        if family.is_some() {
            assert!(
                also_on::registered()
                    .iter()
                    .any(|s| s.single_namespaces().contains(namespace)),
                "{namespace} names one object"
            );
        }
    }
    let cases: Vec<_> = cases.into_iter().map(|(_, _, pair)| pair).collect();
    let mut pairs = vec![];
    for pair in &cases {
        let mut ids = vec![];
        for (uri, keys) in pair {
            ids.push(page(&pool, uri, "Thing").await);
            hub.answer(
                uri,
                Ok(Found {
                    keys: keys.iter().map(|k| key(k)).collect(),
                    ..Default::default()
                }),
            );
        }
        pairs.push(ids);
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    for ids in pairs {
        let groups: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT g.group_id FROM unnest($1::text[]) i(id) LEFT JOIN group_unit g ON g.resource_id=i.id",
        )
        .bind(&ids)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            groups[0].is_none() || groups[0] != groups[1],
            "{ids:?} share {groups:?}"
        );
    }
    // Grouping ran and refused the joins (the pairs share a key), so the check above is not
    // vacuous.
    let refused: i64 = sqlx::query_scalar("SELECT count(*) FROM group_input WHERE key_conflicts>0")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(refused >= cases.len() as i64, "{refused}");
}

/// BTC Map's merchant page names one OSM object: an object it lists carries its own `osm:` key,
/// and its merchant link names that key, so the tagged merchant link joins the object.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_tagged_btc_map_link_joins_its_osm_object(pool: PgPool) {
    let way = "https://example.org/way-259338174";
    let merchant = "https://btcmap.org/merchant/way:259338174";
    let x = page(&pool, way, "LAC Lugano").await;
    let y = page(&pool, merchant, "LAC Lugano").await;
    let btcmap = Fake::new("btcmap");
    btcmap.answer(
        way,
        Ok(Found {
            keys: vec![key("osm:way/259338174")],
            links: vec![Link::new(merchant, "BTC Map", "btcmap").keyed("osm:way/259338174")],
            ..Default::default()
        }),
    );
    let sources: Vec<Arc<dyn Source>> = vec![btcmap];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let groups: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT g.group_id FROM unnest($1::text[]) i(id) LEFT JOIN group_unit g ON g.resource_id=i.id",
    )
    .bind([x, y])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(groups[0].is_some());
    assert_eq!(groups[0], groups[1]);
}

/// One entry per service on any card: a tagged member named after a service a derived link
/// names takes that service's place; a tagged member of another site still shows.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_tagged_member_and_a_derived_link_of_one_service_show_once(pool: PgPool) {
    if !families(&["games", "apps"]) {
        return;
    }
    named(&[
        "https://play.google.com/store/apps/details?id=io.anuke.mindustry",
        "https://f-droid.org/packages/io.anuke.mindustry",
        "https://store.steampowered.com/app/1127400/",
    ]);
    let app_page = |uri: &'static str, site: &'static str| {
        let pool = pool.clone();
        async move {
            let id = store::ensure_resource(&pool, uri).await.unwrap();
            let mut d = ResourceMeta::new(uri, "web", 86400);
            d.name = Some("Mindustry".into());
            d.site = site.into();
            // As the save path records it: the service its URL is on, if any.
            d.service = (!uri.starts_with("https://example.org/")).then(|| site.to_owned());
            d.set_type("SoftwareApplication");
            store::save_document(&pool, &id, 0, &d).await.unwrap();
            id
        }
    };
    let play = "https://play.google.com/store/apps/details?id=io.anuke.mindustry";
    // The F-Droid page's own address: its other spellings are aliases of it.
    let fdroid = "https://f-droid.org/packages/io.anuke.mindustry";
    let blog = "https://example.org/mindustry";
    let card = app_page(play, "Google Play").await;
    app_page(fdroid, "F-Droid").await;
    app_page(blog, "Example").await;
    let hub = Fake::new("stores");
    for uri in [play, fdroid, blog] {
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![key("wikidata:Q1")],
                links: if uri == play {
                    vec![
                        Link::new(
                            "https://f-droid.org/packages/io.anuke.mindustry",
                            "F-Droid",
                            "wikidata",
                        ),
                        Link::new(
                            "https://store.steampowered.com/app/1127400/",
                            "Steam",
                            "wikidata",
                        ),
                    ]
                } else {
                    vec![]
                },
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = lookup_card(&app, &card).await;
    let mut shown: Vec<String> = read["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{} {} {}",
                e["site"].as_str().unwrap(),
                e["uri"].as_str().unwrap(),
                e["derived"] == true
            )
        })
        .collect();
    shown.sort();
    assert_eq!(
        shown,
        [
            format!("Example {blog} false"),
            format!("F-Droid {fdroid} false"),
            "Steam https://store.steampowered.com/app/1127400/ true".to_owned(),
        ]
    );
    // A resource read lists every link. The tagged F-Droid page is the derived F-Droid link
    // itself (its own address, which every other spelling is an alias of): it is listed once,
    // beside Steam and the blog.
    let read = get(&app, &format!("/v1/resources/{card}")).await;
    assert_eq!(read["also_on"].as_array().unwrap().len(), 3);
}

/// `key=doi:` finds the DOI card by its own DOI, and the preprint its publisher names, as the
/// `datacite` source keys them (the card: its DOI and the preprint's arXiv ID; the preprint: its
/// arXiv ID and the corroborated journal DOI). A DOI holding `:` and any letter case.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn search_by_doi_finds_the_card_and_its_preprint(pool: PgPool) {
    let journal = "doi:10.46298/lmcs-20(1:8)2024";
    let preprint = "https://arxiv.org/abs/2302.04668";
    let other = "doi:10.1038/nature12373";
    let datacite = Fake::new("datacite");
    let mut ids = vec![];
    for (uri, keys) in [
        (
            journal,
            vec!["doi:10.46298/lmcs-20(1:8)2024", "arxiv:2302.04668"],
        ),
        (
            preprint,
            vec!["arxiv:2302.04668", "doi:10.46298/lmcs-20(1:8)2024"],
        ),
        (other, vec!["doi:10.1038/nature12373"]),
    ] {
        ids.push(page(&pool, uri, "Paper").await);
        datacite.answer(
            uri,
            Ok(Found {
                keys: keys
                    .into_iter()
                    .map(|k| Key {
                        key: k.into(),
                        via: "datacite".into(),
                    })
                    .collect(),
                ..Default::default()
            }),
        );
    }
    sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT id,repeat('a',52),'tag','/pub/test/tags/'||id,'paper',now() FROM unnest($1::text[]) id")
        .bind(&ids)
        .execute(&pool)
        .await
        .unwrap();
    let sources: Vec<Arc<dyn Source>> = vec![datacite];
    drain(&pool, &cx(Config::default()), &sources).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let found = |key: &str| {
        let app = app.clone();
        let path = format!("/v1/search?key={}", urlencoding(key));
        async move {
            let mut rows: Vec<String> = get(&app, &path).await["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_owned())
                .collect();
            rows.sort();
            rows
        }
    };
    let mut pair = ids[..2].to_vec();
    pair.sort();
    assert_eq!(found("doi:10.46298/LMCS-20(1:8)2024").await, pair);
    assert_eq!(found("doi:10.1038/nature12373").await, [ids[2].clone()]);
}
fn urlencoding(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Two tagged spellings of one F-Droid page share its slot even when the derived F-Droid link is
/// one of them (and is therefore dropped as a member before the slots are counted).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn two_tagged_spellings_of_a_derived_link_show_once(pool: PgPool) {
    let app_page = |uri: &'static str, site: &'static str| {
        let pool = pool.clone();
        async move {
            let id = store::ensure_resource(&pool, uri).await.unwrap();
            let mut d = ResourceMeta::new(uri, "web", 86400);
            d.name = Some("Mindustry".into());
            d.site = site.into();
            // As the save path records it: the service its URL is on, if any.
            d.service = (!uri.starts_with("https://example.org/")).then(|| site.to_owned());
            d.set_type("SoftwareApplication");
            store::save_document(&pool, &id, 0, &d).await.unwrap();
            id
        }
    };
    let play = "https://play.google.com/store/apps/details?id=io.anuke.mindustry";
    let canonical = "https://f-droid.org/packages/io.anuke.mindustry";
    let localized = "https://f-droid.org/en/packages/io.anuke.mindustry/";
    let card = app_page(play, "Google Play").await;
    app_page(canonical, "F-Droid").await;
    app_page(localized, "F-Droid").await;
    let hub = Fake::new("stores");
    for uri in [play, canonical, localized] {
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![key("wikidata:Q1")],
                links: if uri == play {
                    vec![Link::new(canonical, "F-Droid", "wikidata")]
                } else {
                    vec![]
                },
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = lookup_card(&app, &card).await;
    let sites: Vec<&str> = read["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["site"].as_str().unwrap())
        .collect();
    assert_eq!(sites, ["F-Droid"]);
}

/// A page that only names itself after a service (its own `og:site_name`) takes no service's
/// place: the derived link to the real service still shows.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_page_naming_itself_after_a_service_takes_no_slot(pool: PgPool) {
    if !families(&["apps"]) {
        return;
    }
    named(&[
        "https://play.google.com/store/apps/details?id=io.anuke.mindustry",
        "https://f-droid.org/en/packages/io.anuke.mindustry/",
    ]);
    let save = |uri: &'static str, site: &'static str, service: Option<&'static str>| {
        let pool = pool.clone();
        async move {
            let id = store::ensure_resource(&pool, uri).await.unwrap();
            let mut d = ResourceMeta::new(uri, "web", 86400);
            d.name = Some("Mindustry".into());
            d.site = site.into();
            d.service = service.map(str::to_owned);
            d.set_type("SoftwareApplication");
            store::save_document(&pool, &id, 0, &d).await.unwrap();
            id
        }
    };
    let play = "https://play.google.com/store/apps/details?id=io.anuke.mindustry";
    let impostor = "https://example.org/mindustry-on-fdroid";
    let card = save(play, "Google Play", Some("Google Play")).await;
    save(impostor, "F-Droid", None).await;
    let hub = Fake::new("stores");
    for uri in [play, impostor] {
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![key("wikidata:Q1")],
                links: if uri == play {
                    vec![Link::new(
                        "https://f-droid.org/packages/io.anuke.mindustry",
                        "F-Droid",
                        "wikidata",
                    )]
                } else {
                    vec![]
                },
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = get(&app, &format!("/v1/resources/{card}")).await;
    let entries: Vec<(String, bool)> = read["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["uri"].as_str().unwrap().to_owned(), e["derived"] == true))
        .collect();
    assert!(
        entries.contains(&(impostor.to_owned(), false)),
        "{entries:?}"
    );
    assert!(
        entries.contains(&(
            "https://f-droid.org/packages/io.anuke.mindustry".to_owned(),
            true
        )),
        "{entries:?}"
    );
}

/// A run whose secondary service failed is recorded (its keys stand) and comes back after a
/// failed run's backoff, which grows while the service keeps failing (GOG answering 500 for
/// hours must not be asked every minute); a run with every service answering resets it.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_failed_secondary_service_backs_off_until_it_answers(pool: PgPool) {
    let uri = "https://example.org/paper";
    let id = page(&pool, uri, "Paper").await;
    let hub = Fake::new("datacite");
    let partial = || {
        Ok(Found {
            keys: vec![key("doi:10.1000/x")],
            // The service named a wait shorter than the backoff: the backoff wins.
            retry_after: Some(Duration::from_secs(60)),
            ..Default::default()
        })
    };
    let state = || {
        let pool = pool.clone();
        let id = id.clone();
        async move {
            sqlx::query_as::<_, (i64, f64, i32, Option<String>)>(
                "SELECT (SELECT count(*) FROM resource_key WHERE resource_id=d.resource_id),extract(epoch FROM d.next_attempt_at-now())::float8,d.attempts,d.last_error FROM also_on_dirty d WHERE d.resource_id=$1",
            )
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let due_now = || {
        let pool = pool.clone();
        let id = id.clone();
        async move {
            sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now() WHERE resource_id=$1")
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    hub.answer(uri, partial());
    let sources: Vec<Arc<dyn Source>> = vec![hub.clone()];
    let cx = cx(Config::default());
    drain(&pool, &cx, &sources).await;
    let (keys, due_in, attempts, error) = state().await;
    assert_eq!((keys, attempts, error), (1, 1, None));
    assert!((290.0..=305.0).contains(&due_in), "{due_in}");
    // Still failing: the wait doubles.
    due_now().await;
    drain(&pool, &cx, &sources).await;
    let (keys, due_in, attempts, error) = state().await;
    assert_eq!((keys, attempts, error), (1, 2, None));
    assert!((590.0..=605.0).contains(&due_in), "{due_in}");
    // Every service answered: back to the source's lifetime, the count reset.
    hub.answer(
        uri,
        Ok(Found {
            keys: vec![key("doi:10.1000/x")],
            ..Default::default()
        }),
    );
    due_now().await;
    drain(&pool, &cx, &sources).await;
    let (keys, due_in, attempts, error) = state().await;
    assert_eq!((keys, attempts, error), (1, 0, None));
    assert!(due_in > 3600.0, "{due_in}");
}

/// A source that answers as `inner`, but whose lookup of `claimed` first waits `delay`, then
/// claims that resource as another worker would whose drain began after the lease ran out.
struct Outlives {
    inner: Arc<Fake>,
    pool: PgPool,
    claimed: &'static str,
    delay: Duration,
}
#[async_trait]
impl Source for Outlives {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn applies(&self, subject: &Subject) -> bool {
        self.inner.applies(subject)
    }
    fn ttl(&self) -> Duration {
        self.inner.ttl()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> anyhow::Result<Found> {
        if subject.uri == self.claimed {
            tokio::time::sleep(self.delay).await;
            sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()+interval '1 hour' WHERE resource_id=$1")
                .bind(&subject.id)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        self.inner.lookup(cx, subject).await
    }
}

/// A batch that outlives its lease still records every run against it (Kimi, train-1): a clean
/// run schedules its next one, a run no source applies to leaves the queue, a partial or failed
/// run backs off and counts. Only a row another claim took meanwhile keeps that claim.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_batch_that_outlives_its_lease_still_records_its_runs(pool: PgPool) {
    let uris = [
        "https://example.org/clean",
        "https://example.org/nothing",
        "https://example.org/partial",
        "https://example.org/failed",
        "https://example.org/claimed",
    ];
    let mut ids = vec![];
    for uri in uris {
        ids.push(page(&pool, uri, "Paper").await);
    }
    let hub = Fake::new("datacite");
    let found = |retry_after| {
        Ok(Found {
            keys: vec![key("doi:10.1000/x")],
            retry_after,
            ..Default::default()
        })
    };
    hub.answer(uris[0], found(None));
    hub.answer(uris[2], found(Some(Duration::from_secs(60))));
    hub.answer(uris[3], Err(anyhow::anyhow!("datacite answered 500")));
    hub.answer(uris[4], found(None));
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(Outlives {
        inner: hub,
        pool: pool.clone(),
        claimed: uris[4],
        delay: Duration::from_millis(1500),
    })];
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let handled = also_on::drain_leased(
        &pool,
        &cx(Config::default()),
        &sources,
        100,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(handled, 5);
    let state = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (f64, i32, Option<String>)>(
                "SELECT extract(epoch FROM next_attempt_at-now())::float8,attempts,last_error FROM also_on_dirty WHERE resource_id=$1",
            )
            .bind(&id)
            .fetch_optional(&pool)
            .await
            .unwrap()
        }
    };
    let (due_in, attempts, error) = state(ids[0].clone()).await.unwrap();
    assert!(
        due_in > 3600.0 && attempts == 0 && error.is_none(),
        "{due_in} {attempts} {error:?}"
    );
    assert_eq!(state(ids[1].clone()).await, None);
    let (due_in, attempts, error) = state(ids[2].clone()).await.unwrap();
    assert!((290.0..=305.0).contains(&due_in), "{due_in}");
    assert_eq!((attempts, error), (1, None));
    let (due_in, attempts, error) = state(ids[3].clone()).await.unwrap();
    assert!((290.0..=305.0).contains(&due_in), "{due_in}");
    assert_eq!(attempts, 1);
    assert!(error.is_some());
    // The other claim stands; this run's clean outcome resets the count.
    let (due_in, attempts, error) = state(ids[4].clone()).await.unwrap();
    assert!((3500.0..=3600.0).contains(&due_in), "{due_in}");
    assert_eq!((attempts, error), (0, None));
}

/// A page that only types itself as music and names a service (its own JSON-LD) is on no
/// service: it takes no service's place on the group's cards, and its own card hides no derived
/// link of the service it names.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_music_typed_impostor_takes_no_slot_and_hides_nothing(pool: PgPool) {
    let tidal = "https://tidal.com/track/534050211";
    let impostor = "https://example.org/song";
    let spotify = "https://open.spotify.com/track/2OBofMJx94NryV2SK8p8Zf";
    let mut ids = vec![];
    for (uri, site) in [(tidal, "TIDAL"), (impostor, "Spotify")] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.name = Some("Song".into());
        d.site = site.into();
        d.set_type("MusicRecording");
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    let hub = Fake::new("music");
    for uri in [tidal, impostor] {
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![key("wikidata:Q1")],
                links: if uri == tidal {
                    vec![Link::new(spotify, "Spotify", "musicbrainz")]
                } else {
                    vec![]
                },
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    for id in &ids {
        let read = get(&app, &format!("/v1/resources/{id}")).await;
        let entries: Vec<(String, bool)> = read["also_on"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["uri"].as_str().unwrap().to_owned(), e["derived"] == true))
            .collect();
        assert!(
            entries.contains(&(spotify.to_owned(), true)),
            "{id}: {entries:?}"
        );
    }
}

/// A page on a service's host that is not an object's own page (GitHub's topic list) is on no
/// service: in a group with a card that derives a GitHub repository, the topic page shows as
/// itself and the repository keeps GitHub's place.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_non_object_page_on_a_service_host_takes_no_slot(pool: PgPool) {
    if !families(&["packages"]) {
        return;
    }
    named(&["https://github.com/rust-random/rand"]);
    let topics = "https://github.com/topics/rust";
    let repo = "https://github.com/rust-random/rand";
    let krate = "https://crates.io/crates/rand";
    assert_eq!(ResourceMeta::new(topics, "web", 86400).service, None);
    assert_eq!(
        ResourceMeta::new(repo, "web", 86400).service.as_deref(),
        Some("GitHub")
    );
    let mut ids = vec![];
    for uri in [krate, topics] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.name = Some("rand".into());
        d.set_type("SoftwareSourceCode");
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    let hub = Fake::new("packages");
    for uri in [krate, topics] {
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![key("wikidata:Q1")],
                links: if uri == krate {
                    vec![Link::new(repo, "GitHub", "crates")]
                } else {
                    vec![]
                },
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = get(&app, &format!("/v1/resources/{}", ids[0])).await;
    let entries: Vec<(String, bool)> = read["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["uri"].as_str().unwrap().to_owned(), e["derived"] == true))
        .collect();
    assert!(entries.contains(&(topics.to_owned(), false)), "{entries:?}");
    assert!(entries.contains(&(repo.to_owned(), true)), "{entries:?}");
}

/// GitHub's share link (`?tab=readme-ov-file`) is the repository's own page: tagged in a group
/// whose crate derives the repository, it takes GitHub's one place and the derived link is not
/// listed beside it.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_shared_repository_link_with_a_tab_is_the_repository_once(pool: PgPool) {
    if !families(&["packages"]) {
        return;
    }
    named(&["https://github.com/rust-random/rand"]);
    let shared = "https://github.com/rust-random/rand?tab=readme-ov-file";
    let repo = "https://github.com/rust-random/rand";
    let krate = "https://crates.io/crates/rand";
    assert_eq!(
        ResourceMeta::new(shared, "web", 86400).service.as_deref(),
        Some("GitHub")
    );
    let mut ids = vec![];
    for uri in [krate, shared] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.name = Some("rand".into());
        d.set_type("SoftwareSourceCode");
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    let hub = Fake::new("packages");
    for uri in [krate, shared] {
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![key("wikidata:Q1")],
                links: if uri == krate {
                    vec![Link::new(repo, "GitHub", "crates")]
                } else {
                    vec![]
                },
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = lookup_card(&app, &ids[0]).await;
    let entries: Vec<(String, String, bool)> = read["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["site"].as_str().unwrap().to_owned(),
                e["uri"].as_str().unwrap().to_owned(),
                e["derived"] == true,
            )
        })
        .collect();
    assert_eq!(
        entries,
        [("GitHub".to_owned(), shared.to_owned(), false)],
        "{entries:?}"
    );
}

/// A source that panics on every lookup, as a parser meeting an unexpected shape would.
struct Panics;
#[async_trait]
impl Source for Panics {
    fn id(&self) -> &'static str {
        "panics"
    }
    fn applies(&self, _: &Subject) -> bool {
        true
    }
    async fn prefetch(&self, _: &Context, _: &[String]) {
        panic!("a prefetch that panics");
    }
    async fn lookup(&self, _: &Context, _: &Subject) -> anyhow::Result<Found> {
        panic!("a lookup that panics");
    }
}

/// A panicking source fails its run like an error: the sources after it still record, the
/// resource retries with backoff, and the worker drains the next resource.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_panicking_source_never_stops_the_worker(pool: PgPool) {
    let (a, b) = ("https://example.org/a", "https://example.org/b");
    let ids = [page(&pool, a, "A").await, page(&pool, b, "B").await];
    let hub = Fake::new("music");
    for uri in [a, b] {
        hub.answer(
            uri,
            Ok(Found {
                keys: vec![key("wikidata:Q1")],
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(Panics), hub.clone()];
    drain(&pool, &cx(Config::default()), &sources).await;
    assert_eq!(
        hub.calls.lock().unwrap().len(),
        2,
        "the source after it ran for both"
    );
    let rows: Vec<(i32, Option<String>, i64)> = sqlx::query_as(
        "SELECT d.attempts,d.last_error,(SELECT count(*) FROM resource_key k WHERE k.resource_id=d.resource_id) FROM also_on_dirty d WHERE resource_id=ANY($1) ORDER BY resource_id",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for (attempts, error, keys) in rows {
        assert_eq!((attempts, keys), (1, 1));
        assert!(error.is_some());
    }
}

/// Looks up the barcodes the sources before it found, like iTunes: one keyed link each. Off by
/// default.
struct ByBarcode {
    named: &'static str,
}
#[async_trait]
impl Source for ByBarcode {
    fn id(&self) -> &'static str {
        "by_barcode"
    }
    fn applies(&self, subject: &Subject) -> bool {
        subject.keys_in("upc").next().is_some()
    }
    fn enabled_by_default(&self) -> bool {
        false
    }
    async fn lookup(&self, _: &Context, subject: &Subject) -> anyhow::Result<Found> {
        let upc = subject.keys_in("upc").next().unwrap();
        Ok(Found {
            links: vec![
                Link::new(self.named, "Apple Music", "itunes").keyed(format!("upc:{upc}")),
                // A malformed key is dropped; its link stays.
                Link::new("https://tidal.com/album/1", "TIDAL", "itunes").keyed("UPC 1"),
            ],
            ..Default::default()
        })
    }
}

/// A later source sees the keys an earlier one found in the same run, and the tagged link an
/// exact lookup names takes the lookup's key: it joins the group and `key=` finds it.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_link_an_exact_lookup_names_joins_by_its_key(pool: PgPool) {
    let deezer = "https://www.deezer.com/album/41839661";
    let apple = "https://music.apple.com/album/1445304934";
    let x = page(&pool, deezer, "Cut to the Feeling").await;
    let y = page(&pool, apple, "Cut to the Feeling").await;
    let hub = Fake::new("music");
    hub.answer(
        deezer,
        Ok(Found {
            keys: vec![key("upc:0602557656527")],
            ..Default::default()
        }),
    );
    let sources: Vec<Arc<dyn Source>> = vec![hub.clone(), Arc::new(ByBarcode { named: apple })];
    // Off by default: nothing is named.
    drain(&pool, &cx(Config::default()), &sources).await;
    let named = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT uri||' '||coalesce(key,'-') FROM derived_link ORDER BY uri",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    assert!(named().await.is_empty());
    let mut on = Config::default();
    on.also_on.sources.insert(
        "by_barcode".into(),
        also_on::SourceOptions {
            enabled: Some(true),
            ..Default::default()
        },
    );
    sqlx::query("INSERT INTO also_on_dirty(resource_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(&x)
        .execute(&pool)
        .await
        .unwrap();
    drain(&pool, &cx(on.clone()), &sources).await;
    assert_eq!(
        named().await,
        [
            format!("{apple} upc:0602557656527"),
            "https://tidal.com/album/1 -".to_owned()
        ]
    );
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let group = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT group_id FROM group_unit WHERE resource_id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert!(group(x.clone()).await.is_some());
    assert_eq!(group(x.clone()).await, group(y.clone()).await);
    // Search lists tagged resources.
    sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT id,repeat('a',52),'tag','/pub/test/tags/'||id,'music',now() FROM unnest($1::text[]) id")
        .bind([x.clone(), y.clone()])
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), on.clone()).unwrap());
    // Any spelling of the barcode finds both.
    let mut found: Vec<String> = get(&app, "/v1/search?key=upc:602557656527").await["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    found.sort();
    let mut both = vec![x.clone(), y.clone()];
    both.sort();
    assert_eq!(found, both);
    // The lookup stops naming it: the named resource regroups and leaves.
    hub.answer(deezer, Ok(Found::default()));
    sqlx::query("INSERT INTO also_on_dirty(resource_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(&x)
        .execute(&pool)
        .await
        .unwrap();
    drain(&pool, &cx(on), &sources).await;
    assert!(named().await.is_empty());
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    assert_eq!(group(y).await, None);
}

/// A card shows one link per service: the tagged member first (a person used it, so it plays for
/// them), then the derived link most members name. The details read lists them all.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_card_shows_one_link_per_service(pool: PgPool) {
    let deezer = "https://www.deezer.com/track/4329888262";
    let tagged = "https://tidal.com/track/534050211";
    let regional = "https://tidal.com/track/567396540";
    let pending = "https://music.apple.com/song/1";
    let pending_too = "https://music.apple.com/song/3";
    let pending_tidal = "https://tidal.com/track/999";
    let mut ids = vec![];
    for (uri, site) in [(deezer, "Deezer"), (tagged, "TIDAL")] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.set_type("MusicRecording");
        d.site = site.into();
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    // A tagged Apple link whose card is not read yet.
    for uri in [pending_too, pending_tidal] {
        ids.push(store::ensure_resource(&pool, uri).await.unwrap());
    }
    ids.push(store::ensure_resource(&pool, pending).await.unwrap());
    let hub = Fake::new("music");
    let recording = "mb:recording:b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    hub.answer(
        deezer,
        Ok(Found {
            keys: vec![key(recording)],
            links: vec![
                Link::new(regional, "TIDAL", "musicbrainz"),
                Link::new("https://open.spotify.com/track/a", "Spotify", "musicbrainz"),
                Link::new("https://open.spotify.com/track/b", "Spotify", "musicbrainz"),
                // Tagged but not read yet: they join by this key.
                Link::new(pending, "Apple Music", "musicbrainz").keyed(recording),
                Link::new(pending_too, "Apple Music", "musicbrainz").keyed(recording),
                Link::new(pending_tidal, "TIDAL", "musicbrainz").keyed(recording),
                Link::new(
                    "https://music.apple.com/us/song/2",
                    "Apple Music",
                    "musicbrainz",
                ),
            ],
            ..Default::default()
        }),
    );
    hub.answer(
        tagged,
        Ok(Found {
            keys: vec![key(recording)],
            // Two members name Spotify b: it comes first.
            links: vec![Link::new(
                "https://open.spotify.com/track/b",
                "Spotify",
                "musicbrainz",
            )],
            ..Default::default()
        }),
    );
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT id,repeat('a',52),'tag','/pub/test/tags/'||id,'music',now() FROM unnest($1::text[]) id")
        .bind(&ids)
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let of = |also: &Value, site: &str| -> Vec<String> {
        also.as_array()
            .unwrap()
            .iter()
            .filter(|e| e["site"] == site)
            .map(|e| e["uri"].as_str().unwrap().to_owned())
            .collect()
    };
    let items = get(
        &app,
        "/v1/search?key=mbid:b1a9c0e9-d987-4042-ae91-78d6a3267d69",
    )
    .await;
    let card = items["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == ids[0].as_str())
        .unwrap()
        .clone();
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    // Tagged members all show, read or not: the read TIDAL member and the pending one. The
    // regional derived TIDAL link does not.
    assert_eq!(
        sorted(of(&card["also_on"], "TIDAL")),
        sorted(vec![tagged.into(), pending_tidal.into()]),
        "{card}"
    );
    assert_eq!(
        of(&card["also_on"], "Spotify"),
        ["https://open.spotify.com/track/b"]
    );
    // Pending members count for Apple Music by their URLs: both show (tagged members always do),
    // named by their service, and the derived Apple link does not.
    assert_eq!(
        sorted(of(&card["also_on"], "Apple Music")),
        sorted(vec![pending.into(), pending_too.into()]),
        "{card}"
    );
    // A lookup card shows the card's view; a resource read lists every link.
    let looked_up = lookup_card(&app, &ids[0]).await;
    assert_eq!(
        sorted(of(&looked_up["also_on"], "TIDAL")),
        sorted(vec![tagged.into(), pending_tidal.into()])
    );
    let read = get(&app, &format!("/v1/resources/{}", ids[0])).await;
    assert_eq!(
        sorted(of(&read["also_on"], "TIDAL")),
        sorted(vec![tagged.into(), regional.into(), pending_tidal.into()])
    );
    assert_eq!(of(&read["also_on"], "Spotify").len(), 2);
    assert_eq!(
        sorted(of(&looked_up["also_on"], "Apple Music")),
        sorted(vec![pending.into(), pending_too.into()])
    );
    assert_eq!(of(&read["also_on"], "Apple Music").len(), 3);
}

/// A card's own service has its link, the card: Radiohead's Discogs artist never shows "On A
/// Friday", the band's earlier name, which Wikidata lists as another Discogs ID. Its details do.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_card_shows_no_other_page_of_its_own_service(pool: PgPool) {
    let artist = "https://www.discogs.com/artist/3840";
    let earlier = "https://www.discogs.com/artist/840842";
    let spotify = "https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb";
    let id = store::ensure_resource(&pool, artist).await.unwrap();
    let mut d = ResourceMeta::new(artist, "web", 86400);
    d.set_type("MusicGroup");
    d.site = "Discogs".into();
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let hub = Fake::new("music");
    hub.answer(
        artist,
        Ok(Found {
            keys: vec![key("wikidata:Q44190")],
            links: vec![
                Link::new(earlier, "Discogs", "wikidata"),
                Link::new(spotify, "Spotify", "wikidata"),
            ],
            ..Default::default()
        }),
    );
    let sources: Vec<Arc<dyn Source>> = vec![hub];
    drain(&pool, &cx(Config::default()), &sources).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = get(&app, &format!("/v1/resources/{id}")).await;
    let uris = |also: &Value| -> Vec<String> {
        also.as_array()
            .unwrap()
            .iter()
            .map(|e| e["uri"].as_str().unwrap().to_owned())
            .collect()
    };
    let looked_up = lookup_card(&app, &id).await;
    assert_eq!(uris(&looked_up["also_on"]), [spotify], "{looked_up}");
    assert_eq!(uris(&read["also_on"]), [earlier, spotify]);
}

/// A run takes the write order before the resource rows, as saves do: a save holding its
/// resource never deadlocks with a run recording it, and an exclusive writer (a re-extraction)
/// runs alone.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_run_takes_the_write_order_as_saves_do(pool: PgPool) {
    let id = page(&pool, "https://open.spotify.com/track/lock", "Track").await;
    let run = |pool: PgPool, id: String| {
        tokio::spawn(async move {
            let found = Found {
                keys: vec![key("wikidata:Q1")],
                links: vec![Link::new("https://www.deezer.com/track/1", "Deezer", "x")],
                ..Default::default()
            };
            also_on::record(
                &pool,
                &id,
                "music",
                found,
                Duration::from_secs(60),
                &Config::default().also_on,
            )
            .await
        })
    };
    // A save's order: the write order shared, then its resource.
    let mut save = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(pinakes_core::locks::WRITE_ORDER)
        .execute(&mut *save)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR NO KEY UPDATE")
        .bind(&id)
        .execute(&mut *save)
        .await
        .unwrap();
    let recording = run(pool.clone(), id.clone());
    // Long enough for the run to reach the resource's row.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{fetched_at}','1') WHERE id=$1")
        .bind(&id)
        .execute(&mut *save)
        .await
        .unwrap();
    save.commit().await.unwrap();
    recording
        .await
        .unwrap()
        .expect("the run records after the save");
    // An exclusive writer holds the write order: the run waits for it.
    let mut exclusive = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(pinakes_core::locks::WRITE_ORDER)
        .execute(&mut *exclusive)
        .await
        .unwrap();
    let recording = run(pool.clone(), id.clone());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !recording.is_finished(),
        "the run waits for the exclusive writer"
    );
    exclusive.commit().await.unwrap();
    recording.await.unwrap().unwrap();
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_key WHERE resource_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(keys, 1);
}

/// A run locks its resource and the ones its keyed links name in ID order; an ingested batch,
/// holding the write order shared and the alias lock, locks its lines in feed order, the other
/// way round. The run takes the alias lock too, so it waits for the batch instead of deadlocking.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_run_naming_another_resource_waits_for_an_ingested_batch(pool: PgPool) {
    let mut pages = vec![];
    for uri in [
        "https://open.spotify.com/track/order-a",
        "https://www.deezer.com/track/12345",
    ] {
        pages.push((page(&pool, uri, "Track").await, uri));
    }
    pages.sort();
    // The run is for the lower ID, and its keyed link names the higher.
    let ((a, _), (b, b_uri)) = (pages[0].clone(), pages[1].clone());
    let mut batch = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1), pg_advisory_xact_lock($2)")
        .bind(pinakes_core::locks::WRITE_ORDER)
        .bind(pinakes_core::locks::ALIAS)
        .execute(&mut *batch)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR NO KEY UPDATE")
        .bind(&b)
        .execute(&mut *batch)
        .await
        .unwrap();
    let run = tokio::spawn({
        let (pool, a) = (pool.clone(), a.clone());
        async move {
            let found = Found {
                keys: vec![key("wikidata:Q1")],
                links: vec![Link::new(b_uri, "Deezer", "x").keyed("wikidata:Q1")],
                ..Default::default()
            };
            also_on::record(
                &pool,
                &a,
                "music",
                found,
                Duration::from_secs(60),
                &Config::default().also_on,
            )
            .await
        }
    });
    // Long enough for the run to reach its first lock.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR NO KEY UPDATE")
        .bind(&a)
        .execute(&mut *batch)
        .await
        .expect("the batch takes its second line");
    batch.commit().await.unwrap();
    run.await.unwrap().expect("the run records after the batch");
    let named: i64 =
        sqlx::query_scalar("SELECT count(*) FROM derived_link WHERE resource_id=$1 AND link_id=$2")
            .bind(&a)
            .bind(&b)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(named, 1);
}

/// A source's keys and facts are searched (`key=`), so their changes change the search revision
/// (and every search ETag), as derived links do: pruning an expired fact alone too.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn pruned_keys_and_facts_change_the_search_revision(pool: PgPool) {
    let id = page(&pool, "https://open.spotify.com/track/rev", "Track").await;
    let revision = || async {
        sqlx::query_scalar::<_, i64>("SELECT revision FROM search_revision")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    for (table, row) in [
        ("resource_fact", "name,value,source"),
        ("resource_key", "key,source,via"),
    ] {
        let values = if table == "resource_fact" {
            "'isrc','GBAYE9701376','music'"
        } else {
            "'isrc:GBAYE9701376','music','music'"
        };
        sqlx::query(&format!("INSERT INTO {table}(resource_id,{row},expires_at) VALUES($1,{values},now()-interval '1 second')"))
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let before = revision().await;
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("DELETE FROM {table} WHERE expires_at<=now()"))
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(revision().await > before, "{table}");
    }
    // prune's own fact deletion is one of them.
    sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) VALUES($1,'upc','0602557656527','music',now()-interval '1 second')")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let before = revision().await;
    assert_eq!(also_on::prune(&pool).await.unwrap(), 1);
    assert!(revision().await > before);
}

/// A source that found what changes the card itself (`reread`) makes the resource due again at
/// once; one that did not leaves its refresh where it was.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_found_title_reads_the_card_again(pool: PgPool) {
    let named = "https://www.themoviedb.org/tv/1396";
    let plain = "https://www.themoviedb.org/movie/603";
    let a = page(&pool, named, "The Movie Database").await;
    let b = page(&pool, plain, "The Matrix").await;
    let fake = Fake::new("titles");
    fake.answer(
        named,
        Ok(Found {
            facts: vec![also_on::Fact::new("title", "Breaking Bad")],
            reread: true,
            ..Default::default()
        }),
    );
    fake.answer(plain, Ok(Found::default()));
    let cx = cx(Config::default());
    let sources: Vec<Arc<dyn Source>> = vec![fake];
    drain(&pool, &cx, &sources).await;
    let due = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT next_attempt_at<=now() AND priority_at IS NOT NULL FROM resource WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert!(due(a).await, "read again now");
    assert!(!due(b).await, "left for its refresh");
}

/// A source that saves the resource's card while it reads it, as a resolve finishing meanwhile.
struct SavesMeanwhile(PgPool);
#[async_trait]
impl Source for SavesMeanwhile {
    fn id(&self) -> &'static str {
        "meanwhile"
    }
    fn applies(&self, _: &Subject) -> bool {
        true
    }
    async fn lookup(&self, _: &Context, subject: &Subject) -> anyhow::Result<Found> {
        sqlx::query("UPDATE resource SET doc=doc||'{\"name\":\"Saved meanwhile\"}' WHERE id=$1")
            .bind(&subject.id)
            .execute(&self.0)
            .await?;
        Ok(Found::default())
    }
}

/// A card saved while the sources read its resource is read too: the run does not push the
/// mark the save made 30 days out.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_change_saved_during_a_run_is_read_again(pool: PgPool) {
    let id = page(&pool, "https://example.org/meanwhile", "Pending").await;
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(SavesMeanwhile(pool.clone()))];
    drain(&pool, &cx(Config::default()), &sources).await;
    let due: bool =
        sqlx::query_scalar("SELECT next_attempt_at<=now() FROM also_on_dirty WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(due, "the save's mark stands");
    // A run with nothing saved meanwhile schedules the next one as before.
    let quiet = Fake::new("quiet");
    quiet.answer("https://example.org/quiet", Ok(Found::default()));
    let other = page(&pool, "https://example.org/quiet", "Quiet").await;
    let sources: Vec<Arc<dyn Source>> = vec![quiet];
    drain(&pool, &cx(Config::default()), &sources).await;
    let later: bool = sqlx::query_scalar(
        "SELECT next_attempt_at>now()+interval '1 hour' FROM also_on_dirty WHERE resource_id=$1",
    )
    .bind(&other)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(later);
}

/// `reextract` queues the re-read resources' also-on lookups too: a card that comes back
/// unchanged would not run them, and their results may predate a source the upgrade added.
/// `reextract also-on` queues every resource's lookups without re-reading the card.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reextract_runs_the_also_on_sources_again(pool: PgPool) {
    let feed = "https://feeds.feedburner.com/dancarlin/history?format=xml";
    let id = page(&pool, feed, "Dan Carlin's Hardcore History").await;
    let cx = cx(Config::default());
    // Read before the source had anything to say: an empty result, next run in a day.
    let source = Fake::new("podcasts");
    source.answer(feed, Ok(Found::default()));
    let sources: Vec<Arc<dyn Source>> = vec![source.clone()];
    drain(&pool, &cx, &sources).await;
    let facts = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_fact WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(facts().await, 0);
    // The upgraded source now has a fact for it; nothing reads it until it is due.
    source.answer(
        feed,
        Ok(Found {
            facts: vec![also_on::Fact::new(
                "podcast_guid",
                "a2d0e6a2-c2e2-5d2e-8d0e-6a6f1f3f2b55",
            )],
            ..Default::default()
        }),
    );
    also_on::drain(&pool, &cx, &sources, 100).await.unwrap();
    assert_eq!(facts().await, 0, "not due");
    // `reextract all` re-reads the card and queues its lookups, unchanged or not.
    assert!(
        pinakes_core::queue::reextract(&pool, &["all".into()])
            .await
            .unwrap()
            >= 1
    );
    also_on::drain(&pool, &cx, &sources, 100).await.unwrap();
    assert_eq!(facts().await, 1);
    // `reextract also-on` alone queues the lookups and leaves the card's fetch where it was.
    sqlx::query("UPDATE resource SET next_attempt_at=now()+interval '7 days' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    source.answer(feed, Ok(Found::default()));
    assert_eq!(
        pinakes_core::queue::reextract(&pool, &["also-on".into()])
            .await
            .unwrap(),
        1
    );
    also_on::drain(&pool, &cx, &sources, 100).await.unwrap();
    assert_eq!(
        facts().await,
        0,
        "the fresh run's empty result replaced the fact"
    );
    let fetch_due: bool =
        sqlx::query_scalar("SELECT next_attempt_at<=now() FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!fetch_due, "no re-read of the card");
}

/// A source's facts are capped, and the cap cuts the many values of one name (ISRCs) before a
/// name's one value (an MBID, the MBID a page was merged from, an artist), which always fit.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_sources_single_valued_facts_survive_its_cap(pool: PgPool) {
    use pinakes_core::also_on::Fact;
    let id = page(&pool, "https://open.spotify.com/track/facts", "Track").await;
    let mut facts = vec![
        Fact::new("mbid", "00000000-0000-4000-8000-00000000000f"),
        Fact::new("mbid_was", "00000000-0000-4000-8000-00000000000e"),
        Fact::new("artist", "Zz Top"),
    ];
    facts.extend((10..50).map(|n| Fact::new("isrc", format!("AAAAA26000{n}"))));
    also_on::record(
        &pool,
        &id,
        "music",
        Found {
            facts,
            ..Default::default()
        },
        Duration::from_secs(60),
        &Config::default().also_on,
    )
    .await
    .unwrap();
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT name FROM resource_fact WHERE resource_id=$1 ORDER BY name")
            .bind(&id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(stored.len(), 32);
    for name in ["artist", "mbid", "mbid_was"] {
        assert!(stored.contains(&name.to_owned()), "{name}: {stored:?}");
    }
    assert_eq!(stored.iter().filter(|n| *n == "isrc").count(), 29);
}

/// At the cap's floor (8), a recording's single-valued facts still fit beside its ISRCs.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_facts_caps_floor_keeps_a_recordings_single_valued_facts(pool: PgPool) {
    use pinakes_core::also_on::Fact;
    let id = page(&pool, "https://open.spotify.com/track/floor", "Track").await;
    let mut config = Config::default();
    config.also_on.facts_per_source = 8;
    let mut facts = vec![
        Fact::new("artist", "Queen"),
        Fact::new("mbid", "00000000-0000-4000-8000-00000000000f"),
        Fact::new("mbid_was", "00000000-0000-4000-8000-00000000000e"),
    ];
    facts.extend((10..42).map(|n| Fact::new("isrc", format!("AAAAA26000{n}"))));
    also_on::record(
        &pool,
        &id,
        "music",
        Found {
            facts,
            ..Default::default()
        },
        Duration::from_secs(60),
        &config.also_on,
    )
    .await
    .unwrap();
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT name FROM resource_fact WHERE resource_id=$1 ORDER BY name")
            .bind(&id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(stored.len(), 8, "{stored:?}");
    for name in ["artist", "mbid", "mbid_was"] {
        assert!(stored.contains(&name.to_owned()), "{name}: {stored:?}");
    }
    assert_eq!(stored.iter().filter(|n| *n == "isrc").count(), 5);
}

/// Two sources fail, the first asking for an hour, the second for five minutes: the resource is
/// due again after the hour, whichever failed last.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_longest_wait_among_failing_sources_sets_the_retry(pool: PgPool) {
    let uri = "https://open.spotify.com/track/waits";
    let id = page(&pool, uri, "Waits").await;
    for order in [["slow", "quick"], ["quick", "slow"]] {
        let sources: Vec<Arc<dyn Source>> = order
            .iter()
            .map(|name| {
                let fake = Fake::new(if *name == "slow" { "slow" } else { "quick" });
                fake.busy(uri, if *name == "slow" { 3600 } else { 300 });
                fake as Arc<dyn Source>
            })
            .collect();
        drain(&pool, &cx(Config::default()), &sources).await;
        let (error, seconds): (Option<String>, f64) = sqlx::query_as("SELECT last_error, extract(epoch FROM next_attempt_at-now())::float8 FROM also_on_dirty WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(error.as_deref(), Some("fake_busy"), "{order:?}");
        assert!((3500.0..=3600.0).contains(&seconds), "{order:?}: {seconds}");
    }
}

/// A source that reads gone links, as the archive lookup does: an archived copy and a fact. It
/// counts its calls.
#[derive(Default)]
struct Archives(Mutex<usize>);
#[async_trait]
impl Source for Archives {
    fn id(&self) -> &'static str {
        "archives"
    }
    fn applies(&self, subject: &Subject) -> bool {
        matches!(subject.status.as_str(), "missing" | "unreachable")
    }
    async fn lookup(&self, _: &Context, subject: &Subject) -> anyhow::Result<Found> {
        *self.0.lock().unwrap() += 1;
        Ok(archived(&subject.uri))
    }
}
fn archived(uri: &str) -> Found {
    let mut link = Link::new(
        format!("https://web.archive.org/web/2016/{uri}"),
        "web.archive.org",
        "archives",
    );
    link.kind = Some("archived".into());
    Found {
        links: vec![link],
        facts: vec![also_on::Fact {
            name: "title".into(),
            value: "Hello world".into(),
        }],
        ..Default::default()
    }
}
/// A source whose lookup runs while the post it reads is found gone.
struct GoneMeanwhile(PgPool);
#[async_trait]
impl Source for GoneMeanwhile {
    fn id(&self) -> &'static str {
        "meanwhile"
    }
    fn applies(&self, subject: &Subject) -> bool {
        subject.status == "ok"
    }
    async fn lookup(&self, _: &Context, subject: &Subject) -> anyhow::Result<Found> {
        delete_post(&self.0, &subject.id, &subject.uri).await;
        Ok(archived(&subject.uri))
    }
}
/// A source whose lookup runs while its resource becomes another link's alias.
struct AliasedMeanwhile(PgPool);
#[async_trait]
impl Source for AliasedMeanwhile {
    fn id(&self) -> &'static str {
        "aliased"
    }
    fn applies(&self, subject: &Subject) -> bool {
        subject.status == "ok"
    }
    async fn lookup(&self, _: &Context, subject: &Subject) -> anyhow::Result<Found> {
        let mut tx = self.0.begin().await?;
        let line = pinakes_core::alias::assign(
            &mut tx,
            &subject.id,
            &pinakes_core::alias::Alias {
                root: "https://example.social/@alice/root".into(),
                basis: "identifier",
            },
        )
        .await?;
        pinakes_core::alias::record(&mut tx, line.into_iter().collect()).await?;
        tx.commit().await?;
        Ok(archived(&subject.uri))
    }
}
/// A source whose lookup runs while the repair scrubs its post, which an earlier build stored
/// as gone with its card.
struct ScrubbedMeanwhile(PgPool);
#[async_trait]
impl Source for ScrubbedMeanwhile {
    fn id(&self) -> &'static str {
        "scrubbed"
    }
    fn applies(&self, subject: &Subject) -> bool {
        subject.status == "ok"
    }
    async fn lookup(&self, _: &Context, subject: &Subject) -> anyhow::Result<Found> {
        sqlx::query("UPDATE resource SET status='missing',doc=jsonb_set(doc,'{status}','\"missing\"') WHERE id=$1")
            .bind(&subject.id)
            .execute(&self.0)
            .await?;
        assert_eq!(pinakes_core::changes::scrub_stored(&self.0).await?, 1);
        Ok(archived(&subject.uri))
    }
}
/// A social post (an ActivityPub note) read and saved. Its ID.
async fn social_post(pool: &PgPool, uri: &str) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "activitypub", 3600);
    d.set_type("SocialMediaPosting");
    d.name = Some("Hello world".into());
    d.fetched_at = 1000;
    store::save_document(pool, &id, 0, &d).await.unwrap();
    id
}
/// Two missing observations: the post is confirmed gone.
async fn delete_post(pool: &PgPool, id: &str, uri: &str) {
    for n in 1..3 {
        let mut gone = ResourceMeta::new(uri, "activitypub", 3600);
        gone.status = pinakes_core::model::Status::Missing;
        gone.fetched_at = 1000 + n;
        store::save_document(pool, id, 0, &gone).await.unwrap();
    }
}
async fn derived(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT (SELECT count(*) FROM resource_fact WHERE resource_id=$1)+(SELECT count(*) FROM derived_link WHERE resource_id=$1)+(SELECT count(*) FROM resource_key WHERE resource_id=$1)")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Nothing is derived from a social post found gone: the worker runs no lookup for it (an
/// archived copy included), as for a gone page it does; a lookup that ran while the post was
/// found gone writes nothing.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_gone_social_post_gets_no_also_on_results(pool: PgPool) {
    let uri = "https://example.social/@alice/1";
    let id = social_post(&pool, uri).await;
    delete_post(&pool, &id, uri).await;
    let page = store::ensure_resource(&pool, "https://example.org/gone-page")
        .await
        .unwrap();
    let mut gone_page = ResourceMeta::new("https://example.org/gone-page", "web", 3600);
    gone_page.set_type("WebPage");
    gone_page.status = pinakes_core::model::Status::Missing;
    sqlx::query("UPDATE resource SET status='missing',doc=$2 WHERE id=$1")
        .bind(&page)
        .bind(serde_json::to_value(&gone_page).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    // Its deletion's save leaves it unqueued.
    let queued = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM also_on_dirty WHERE resource_id=$1)",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert!(!queued(id.clone()).await);
    let archives = Arc::new(Archives::default());
    let sources: Vec<Arc<dyn Source>> = vec![archives.clone()];
    drain(&pool, &cx(Config::default()), &sources).await;
    assert_eq!(*archives.0.lock().unwrap(), 1, "the gone page only");
    assert_eq!(derived(&pool, &page).await, 2);
    assert_eq!(derived(&pool, &id).await, 0);

    let uri = "https://example.social/@alice/2";
    let id = social_post(&pool, uri).await;
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(GoneMeanwhile(pool.clone()))];
    drain(&pool, &cx(Config::default()), &sources).await;
    let status: String = sqlx::query_scalar("SELECT doc->>'status' FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "missing");
    assert_eq!(derived(&pool, &id).await, 0);
    assert!(!queued(id.clone()).await);

    // A lookup across the repair's scrub writes nothing either.
    let uri = "https://example.social/@alice/3";
    let id = social_post(&pool, uri).await;
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(ScrubbedMeanwhile(pool.clone()))];
    drain(&pool, &cx(Config::default()), &sources).await;
    assert_eq!(derived(&pool, &id).await, 0);
    assert!(!queued(id).await);
}

/// A gone post queued again (a failed refresh moves its row's status, which queues it) gets no
/// lookups: the worker skips every source and drops its queue row.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_gone_post_queued_again_gets_no_lookups(pool: PgPool) {
    let uri = "https://example.social/@alice/1";
    let id = social_post(&pool, uri).await;
    delete_post(&pool, &id, uri).await;
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let job = pinakes_core::queue::Job {
        id: id.clone(),
        uri: uri.into(),
        attempts: 0,
        generation,
    };
    pinakes_core::queue::failed(&pool, &job, &Default::default(), "http_5xx")
        .await
        .unwrap();
    let queued = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM also_on_dirty WHERE resource_id=$1)",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(queued().await);
    let any = Arc::new(Any::default());
    let sources: Vec<Arc<dyn Source>> = vec![any.clone()];
    drain(&pool, &cx(Config::default()), &sources).await;
    assert_eq!(*any.0.lock().unwrap(), 0);
    assert!(!queued().await);
    assert_eq!(derived(&pool, &id).await, 0);
}
/// A source that applies to every resource, counting its lookups.
#[derive(Default)]
struct Any(Mutex<usize>);
#[async_trait]
impl Source for Any {
    fn id(&self) -> &'static str {
        "any"
    }
    fn applies(&self, _: &Subject) -> bool {
        true
    }
    async fn lookup(&self, _: &Context, subject: &Subject) -> anyhow::Result<Found> {
        *self.0.lock().unwrap() += 1;
        Ok(archived(&subject.uri))
    }
}

/// A lookup whose resource became another link's alias while it ran writes nothing: an alias
/// runs no lookups, its root's do.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_lookup_whose_resource_became_an_alias_writes_nothing(pool: PgPool) {
    social_post(&pool, "https://example.social/@alice/root").await;
    let id = page(&pool, "https://example.org/spelling", "A spelling").await;
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(AliasedMeanwhile(pool.clone()))];
    drain(&pool, &cx(Config::default()), &sources).await;
    let alias: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1)")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(alias);
    assert_eq!(derived(&pool, &id).await, 0);
}

/// A source the operator switches off stops showing at once: its keys, facts and derived links
/// stay in the database until they expire, but no card, `also_on` list, search or group reads
/// them. Switched on again, they show again.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_switched_off_source_shows_nothing_until_it_is_on_again(pool: PgPool) {
    use pinakes_core::also_on::{Fact, SourceOptions};
    // Whichever source this build has: the switch is the configuration's.
    let id = also_on::registered()[0].id();
    let config = |on: bool| {
        let mut config = Config::default();
        config.also_on.sources.insert(
            id.into(),
            SourceOptions {
                enabled: Some(on),
                ..Default::default()
            },
        );
        config
    };
    let mbid = "9f9cf187-d6f9-437f-9d98-d59cdbd52757";
    let (a, b) = (
        "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
        "https://www.deezer.com/track/138539973",
    );
    let ids = [
        page(&pool, a, "Paranoid Android").await,
        page(&pool, b, "Paranoid Android").await,
    ];
    sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT id,repeat('a',52),'tag','/pub/test/tags/'||id,'music',now() FROM unnest($1::text[]) id")
        .bind(&ids[..])
        .execute(&pool)
        .await
        .unwrap();
    let source = Fake::new(id);
    for uri in [a, b] {
        source.answer(
            uri,
            Ok(Found {
                keys: vec![key(&format!("mb:recording:{mbid}"))],
                links: vec![Link::new(
                    "https://music.apple.com/us/song/1",
                    "Apple Music",
                    "musicbrainz",
                )],
                facts: vec![Fact::new("isrc", "GBAYE9701376")],
                ..Default::default()
            }),
        );
    }
    let sources: Vec<Arc<dyn Source>> = vec![source];
    drain(&pool, &cx(config(true)), &sources).await;
    let mut both = ids.to_vec();
    both.sort();
    for on in [true, false, true] {
        let config = config(on);
        also_on::configure(&pool, &config.also_on).await.unwrap();
        groups::drain(&pool, &Default::default(), 100)
            .await
            .unwrap();
        let app = api::router(AppState::new(pool.clone(), config).unwrap());
        let card = get(&app, &format!("/v1/resources/{}", ids[0])).await;
        let listed = post(&app, "/v1/resources/lookup", json!({"ids": [ids[0]]})).await;
        let found = |key: String| {
            let app = app.clone();
            async move {
                let mut rows: Vec<String> = get(&app, &format!("/v1/search?key={key}")).await
                    ["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].as_str().unwrap().to_owned())
                    .collect();
                rows.sort();
                rows
            }
        };
        let (by_mbid, by_isrc) = (
            found(format!("mbid:{mbid}")).await,
            found("isrc:GBAYE9701376".into()).await,
        );
        if on {
            assert_eq!(card["group"]["size"], 2, "{card}");
            assert_eq!(card["facts"], json!({"isrc": "GBAYE9701376"}), "{card}");
            assert!(
                card["keys"].as_array().is_some_and(|k| !k.is_empty()),
                "{card}"
            );
            let derived = |also: &Value| {
                also.as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["derived"] == true && e["site"] == "Apple Music")
            };
            assert!(derived(&card["also_on"]), "{card}");
            assert!(derived(&listed[0]["also_on"]), "{listed}");
            assert_eq!((&by_mbid, &by_isrc), (&both, &both));
        } else {
            assert!(card["group"].is_null(), "{card}");
            assert!(card["facts"].is_null(), "{card}");
            assert!(card["keys"].as_array().is_none_or(Vec::is_empty), "{card}");
            assert_eq!(card["also_on"], json!([]), "{card}");
            assert_eq!(listed[0]["also_on"], json!([]), "{listed}");
            assert!(
                by_mbid.is_empty() && by_isrc.is_empty(),
                "{by_mbid:?} {by_isrc:?}"
            );
            // Kept, not deleted.
            let kept: i64 = sqlx::query_scalar(
                "SELECT (SELECT count(*) FROM resource_key WHERE source=$1)+(SELECT count(*) FROM resource_fact WHERE source=$1)+(SELECT count(*) FROM derived_link WHERE source=$1)",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(kept, 6);
        }
    }
}

/// A page's reader for the queue: each read answers a 304 or the page's own card, as the test
/// scripts them, and counts.
struct Scripted {
    answers: Mutex<std::collections::VecDeque<bool>>,
    reads: Mutex<usize>,
}
impl Scripted {
    fn new(not_modified: &[bool]) -> Self {
        Self {
            answers: Mutex::new(not_modified.iter().copied().collect()),
            reads: Mutex::new(0),
        }
    }
}
#[async_trait]
impl pinakes_core::resolver::Resolver for Scripted {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        *self.reads.lock().unwrap() += 1;
        if self.answers.lock().unwrap().pop_front().unwrap_or(false) {
            return Err(pinakes_core::resolver::NotModified.into());
        }
        let mut doc = ResourceMeta::new(uri, "web", 86400);
        doc.name = Some("Netflix".into());
        doc.set_type("CreativeWork");
        Ok(doc)
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(86400)
    }
}

/// What a switched-off source gave a card itself (here the `titles` source's name for a Netflix
/// title, which the queue writes into the card) leaves at its next read, which switching it
/// makes due at once, and a 304 does not keep it: the page is read again in full. Switched on
/// again, the name comes back the same way. The card records the source, so this holds when the
/// fact changed or expired before the switch, and when it expires with the source on.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_switched_off_source_s_name_leaves_the_card_even_on_a_304(pool: PgPool) {
    use pinakes_core::{also_on::Fact, also_on::SourceOptions, config::QueueConfig, queue};
    if !also_on::registered().iter().any(|s| s.id() == "titles") {
        return;
    }
    let uri = "https://www.netflix.com/title/80025678";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let titles = Fake::new("titles");
    titles.answer(
        uri,
        Ok(Found {
            facts: vec![Fact::new("title", "The Crown")],
            ..Default::default()
        }),
    );
    let config = |on: bool| {
        let mut config = Config::default();
        config.also_on.sources.insert(
            "titles".into(),
            SourceOptions {
                enabled: Some(on),
                ..Default::default()
            },
        );
        config
    };
    let cfg = QueueConfig::default();
    // Read once as the queue reads it: the card's own name, then the source's.
    let read = |answers: &'static [bool]| {
        let (pool, cfg, id) = (pool.clone(), cfg.clone(), id.clone());
        async move {
            sqlx::query("UPDATE resource SET resolver='web',next_attempt_at=now() WHERE id=$1")
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
            let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
            let reader = Scripted::new(answers);
            queue::run_job(&pool, &reader, &cfg, &job).await.unwrap();
            let name: Option<String> = sqlx::query_scalar("SELECT name FROM resource WHERE id=$1")
                .bind(&job.id)
                .fetch_one(&pool)
                .await
                .unwrap();
            let reads = *reader.reads.lock().unwrap();
            (name, reads)
        }
    };
    also_on::configure(&pool, &config(true).also_on)
        .await
        .unwrap();
    read(&[false]).await;
    let sources: Vec<Arc<dyn Source>> = vec![titles];
    drain(&pool, &cx(config(true)), &sources).await;
    assert_eq!(read(&[false]).await, (Some("The Crown".into()), 1));
    // On, a 304 keeps the card as it is.
    assert_eq!(read(&[true]).await, (Some("The Crown".into()), 1));
    let due = || async {
        sqlx::query_scalar::<_, bool>("SELECT next_attempt_at<=now() FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    sqlx::query("UPDATE resource SET next_attempt_at=now()+interval '1 day' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    // Off: due at once, and a 304 is followed by a full read without the name.
    also_on::configure(&pool, &config(false).also_on)
        .await
        .unwrap();
    assert!(due().await);
    assert_eq!(read(&[true, false]).await, (Some("Netflix".into()), 2));
    // Once read without it, a 304 is the card's again.
    assert_eq!(read(&[true]).await, (Some("Netflix".into()), 1));
    // On again: due, and read with the name.
    sqlx::query("UPDATE resource SET next_attempt_at=now()+interval '1 day' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    also_on::configure(&pool, &config(true).also_on)
        .await
        .unwrap();
    assert!(due().await);
    assert_eq!(read(&[true]).await, (Some("The Crown".into()), 1));

    // The card records whose finding it carries, so the cleanup needs no fact: one changed, or
    // expired, before the source is switched off goes as well.
    let fact = |set: &'static str| {
        let (pool, id) = (pool.clone(), id.clone());
        async move {
            sqlx::query(&format!(
                "UPDATE resource_fact SET {set} WHERE resource_id=$1 AND source='titles' AND name='title'"
            ))
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let switch = |on: bool| {
        let (pool, config) = (pool.clone(), config(on));
        async move { also_on::configure(&pool, &config.also_on).await.unwrap() }
    };
    // Changed, then off.
    fact("value='The Crown (2016)'").await;
    switch(false).await;
    assert_eq!(read(&[true, false]).await, (Some("Netflix".into()), 2));
    // On, a card that carries none of it takes the source's value on a 304; one that carries an
    // older value is read again in full.
    switch(true).await;
    assert_eq!(read(&[true]).await, (Some("The Crown (2016)".into()), 1));
    fact("value='The Crown'").await;
    assert_eq!(read(&[true, false]).await, (Some("The Crown".into()), 2));
    // Expired, then off.
    fact("expires_at=now()-interval '1 second'").await;
    switch(false).await;
    assert_eq!(read(&[true, false]).await, (Some("Netflix".into()), 2));
    // Expired while on: the source no longer finds it, so the same.
    switch(true).await;
    fact("expires_at=now()+interval '1 day'").await;
    assert_eq!(read(&[true]).await, (Some("The Crown".into()), 1));
    fact("expires_at=now()-interval '1 second'").await;
    assert_eq!(read(&[true, false]).await, (Some("Netflix".into()), 2));

    // Every value the source wrote counts, not only the name: a description it no longer gives,
    // or gives changed, under the same title, is read again in full.
    let description = || {
        let (pool, id) = (pool.clone(), id.clone());
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT doc->>'description' FROM resource WHERE id=$1",
            )
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let describe = |text: Option<&'static str>| {
        let (pool, id) = (pool.clone(), id.clone());
        async move {
            sqlx::query("DELETE FROM resource_fact WHERE resource_id=$1 AND source='titles' AND name='title_description'")
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
            if let Some(text) = text {
                sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) VALUES($1,'title_description',$2,'titles',now()+interval '1 day')")
                    .bind(&id)
                    .bind(text)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
    };
    fact("expires_at=now()+interval '1 day'").await;
    describe(Some("A royal drama")).await;
    assert_eq!(read(&[true]).await, (Some("The Crown".into()), 1));
    assert_eq!(description().await.as_deref(), Some("A royal drama"));
    // Unchanged: the 304 stands.
    assert_eq!(read(&[true]).await, (Some("The Crown".into()), 1));
    assert_eq!(description().await.as_deref(), Some("A royal drama"));
    // Removed, the title unchanged.
    describe(None).await;
    assert_eq!(read(&[true, false]).await, (Some("The Crown".into()), 2));
    assert_eq!(description().await, None);
    // Changed, the title unchanged.
    describe(Some("A royal drama")).await;
    assert_eq!(read(&[true, false]).await, (Some("The Crown".into()), 2));
    assert_eq!(description().await.as_deref(), Some("A royal drama"));
    describe(Some("A drama in six seasons")).await;
    assert_eq!(read(&[true, false]).await, (Some("The Crown".into()), 2));
    assert_eq!(
        description().await.as_deref(),
        Some("A drama in six seasons")
    );
    assert_eq!(read(&[true]).await, (Some("The Crown".into()), 1));
}

/// A card stored by an earlier build lists its sources without fingerprints: it reads, each
/// fingerprint unknown, so its next 304 reads the page again; a card that names none has none.
#[test]
fn an_earlier_list_of_sources_reads_as_unknown_fingerprints() {
    let mut stored =
        serde_json::to_value(ResourceMeta::new("https://example.org/", "web", 60)).unwrap();
    assert!(stored.get("hinted_by").is_none());
    let doc: ResourceMeta = serde_json::from_value(stored.clone()).unwrap();
    assert!(doc.hinted_by.is_empty());
    stored["hinted_by"] = json!(["films", "titles"]);
    let doc: ResourceMeta = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(
        doc.hinted_by,
        BTreeMap::from([
            ("films".into(), String::new()),
            ("titles".into(), String::new())
        ])
    );
    stored["hinted_by"] = json!({"titles": "0123456789abcdef"});
    let doc: ResourceMeta = serde_json::from_value(stored).unwrap();
    assert_eq!(
        serde_json::to_value(&doc).unwrap()["hinted_by"],
        json!({"titles": "0123456789abcdef"})
    );
}
