//! The same-recording hint: `maybe_same` on recording cards names other recordings that are
//! possibly the same one, never merging anything.
use axum::{body::Body, extract::ConnectInfo, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Fact, Found, Key},
    api::{self, AppState},
    config::{Config, SameEventHint},
    groups,
    model::ResourceMeta,
    same_recording, store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;

/// A recording's page, saved and tagged once, as its service states it: the title, the artist
/// and the length (an ISO duration) in its attributes.
async fn recording(
    pool: &PgPool,
    uri: &str,
    name: &str,
    artist: Option<&str>,
    duration: Option<&str>,
) -> String {
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type("MusicRecording");
    doc.name = Some(name.into());
    if let Some(artist) = artist {
        doc.attributes.insert("artist".into(), artist.into());
    }
    if let Some(duration) = duration {
        doc.attributes.insert("duration".into(), duration.into());
    }
    save(pool, &doc).await
}

async fn save(pool: &PgPool, doc: &ResourceMeta) -> String {
    let id = store::ensure_resource(pool, &doc.url).await.unwrap();
    store::save_document(pool, &id, 0, doc).await.unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','music',$3,now()) ON CONFLICT DO NOTHING")
        .bind(&id)
        .bind("pubkyseedauthorpubkyseedauthorpubkyseedauthorpubkyse")
        .bind(format!("/pub/pubky.app/tags/{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

/// What a source found for a resource, recorded as the worker records it.
async fn found(pool: &PgPool, id: &str, source: &str, keys: &[&str], facts: &[(&str, &str)]) {
    let found = Found {
        keys: keys
            .iter()
            .map(|k| Key {
                key: (*k).into(),
                via: source.into(),
            })
            .collect(),
        facts: facts.iter().map(|(n, v)| Fact::new(*n, *v)).collect(),
        ..Default::default()
    };
    also_on::record(
        pool,
        id,
        source,
        found,
        Duration::from_secs(86_400),
        &Config::default().also_on,
    )
    .await
    .unwrap();
}

async fn pass(pool: &PgPool, config: &Config) {
    while same_recording::drain(pool, config, 200).await.unwrap() > 0 {}
}

async fn response(config: &Config, pool: &PgPool, path: &str) -> (Value, String) {
    let app = api::router(AppState::new(pool.clone(), config.clone()).unwrap());
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200, "{path}");
    let etag = response
        .headers()
        .get("etag")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    (body, etag)
}

async fn card(config: &Config, pool: &PgPool, id: &str) -> Value {
    response(config, pool, &format!("/v1/resources/{id}"))
        .await
        .0
}

/// The IDs a card's hint names, best first.
fn named(card: &Value) -> Vec<&str> {
    card["maybe_same"]
        .as_array()
        .unwrap_or_else(|| panic!("no maybe_same: {card}"))
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn recordings_nothing_exact_joins_name_the_ones_they_may_be(pool: PgPool) {
    let config = Config::default();
    // A 2021 re-release whose ISRC nobody else knows: no key joins it to the recording.
    let tidal = recording(
        &pool,
        "https://tidal.com/track/551040646",
        "Bohemian Rhapsody",
        Some("Queen"),
        Some("PT5M56S"),
    )
    .await;
    // The recording's own page and a Spotify track MusicBrainz relates to it: one group.
    let mb = recording(
        &pool,
        "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69",
        "Bohemian Rhapsody",
        None,
        None,
    )
    .await;
    found(
        &pool,
        &mb,
        "musicbrainz_entity",
        &["mb:recording:b1a9c0e9-d987-4042-ae91-78d6a3267d69"],
        &[("artist", "Queen"), ("duration", "355")],
    )
    .await;
    let spotify = recording(
        &pool,
        "https://open.spotify.com/track/2OBofMJx94NryV2SK8p8Zf",
        "Bohemian Rhapsody - Remastered 2011",
        Some("Queen"),
        Some("PT5M54S"),
    )
    .await;
    found(
        &pool,
        &spotify,
        "musicbrainz",
        &["mb:recording:b1a9c0e9-d987-4042-ae91-78d6a3267d69"],
        &[],
    )
    .await;
    // A live version, a page without a length, three other songs of that name, and a page that
    // is no recording.
    let live = recording(
        &pool,
        "https://www.deezer.com/track/1",
        "Bohemian Rhapsody (Live Aid)",
        Some("Queen"),
        Some("PT6M07S"),
    )
    .await;
    let unmeasured = recording(
        &pool,
        "https://www.deezer.com/track/2",
        "Bohemian Rhapsody",
        Some("Queen"),
        None,
    )
    .await;
    let mut chicks = vec![];
    for (n, artist) in [(3, "Sido"), (4, "Haftbefehl"), (5, "Kalim")] {
        chicks.push(
            recording(
                &pool,
                &format!("https://www.deezer.com/track/{n}"),
                "Bad Chick",
                Some(artist),
                Some("PT3M"),
            )
            .await,
        );
    }
    let mut page = ResourceMeta::new("https://example.org/bohemian-rhapsody", "web", 86400);
    page.set_type("WebPage");
    page.name = Some("Bohemian Rhapsody".into());
    let page = save(&pool, &page).await;
    groups::drain(&pool, &config.groups, 100).await.unwrap();
    let (_, before) = response(&config, &pool, "/v1/search?q=bohemian").await;
    // The sweep reads what the triggers already marked and what they did not; the pass pairs.
    same_recording::sweep(&pool).await.unwrap();
    pass(&pool, &config).await;

    // The re-release names the recording's pages, lengths agreeing first, then the page
    // without a length; never the live version or another song of that name.
    let tidal_card = card(&config, &pool, &tidal).await;
    assert_eq!(
        named(&tidal_card),
        [mb.as_str(), spotify.as_str(), unmeasured.as_str()],
        "{tidal_card}"
    );
    let first = &tidal_card["maybe_same"][0];
    assert_eq!(first["score"], 1.0);
    assert!(first["start"].is_null());
    assert_eq!(
        first["uri"],
        "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"
    );
    assert_eq!(tidal_card["maybe_same"][2]["score"], 0.7);
    assert_eq!(
        tidal_card["details"]["maybe_same"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // A member of the card's own group is never listed.
    let mb_card = card(&config, &pool, &mb).await;
    assert_eq!(
        named(&mb_card),
        [tidal.as_str(), unmeasured.as_str()],
        "{mb_card}"
    );
    assert!(mb_card["group"]["id"].is_string());
    assert_eq!(
        named(&card(&config, &pool, &live).await),
        Vec::<&str>::new()
    );
    for id in &chicks {
        assert_eq!(named(&card(&config, &pool, id).await), Vec::<&str>::new());
    }
    assert!(card(&config, &pool, &page).await["maybe_same"].is_null());
    // Search pages carry the hint on recording rows and change their ETag with it.
    let (search, after) = response(&config, &pool, "/v1/search?q=bohemian").await;
    assert_ne!(before, after);
    let row = search["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == tidal.as_str())
        .unwrap();
    assert_eq!(named(row).len(), 3);
    // A group's members carry it too.
    let (group, _) = response(
        &config,
        &pool,
        &format!("/v1/groups/{}", mb_card["group"]["id"].as_str().unwrap()),
    )
    .await;
    assert!(group["members"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["maybe_same"].is_array()));

    // A changed length is read again: the unmeasured page measured at 3:34 pairs with nothing.
    found(&pool, &unmeasured, "service", &[], &[("duration", "214")]).await;
    pass(&pool, &config).await;
    assert_eq!(
        named(&card(&config, &pool, &tidal).await),
        [mb.as_str(), spotify.as_str()]
    );
    assert_eq!(
        named(&card(&config, &pool, &unmeasured).await),
        Vec::<&str>::new()
    );
    // A recording that becomes an alias leaves the hint on the next sweep.
    let mut tx = pool.begin().await.unwrap();
    pinakes_core::alias::assign(
        &mut tx,
        &tidal,
        &pinakes_core::alias::Alias {
            root: "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69".into(),
            basis: "service",
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(same_recording::sweep(&pool).await.unwrap() >= 1);
    pass(&pool, &config).await;
    assert_eq!(named(&card(&config, &pool, &mb).await), Vec::<&str>::new());
}

/// `[groups] same_recording_hint = "off"` removes the field from recording cards and stops the
/// pass; the OpenAPI document keeps the field while either hint is on.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_switch_removes_the_field_and_stops_the_pass(pool: PgPool) {
    let on = Config::default();
    let off: Config = toml::from_str("[groups]\nsame_recording_hint = \"off\"\n").unwrap();
    assert_eq!(off.groups.same_recording_hint, SameEventHint::Off);
    let a = recording(
        &pool,
        "https://tidal.com/track/1",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        Some("PT5M24S"),
    )
    .await;
    let b = recording(
        &pool,
        "https://www.deezer.com/track/1031007",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        Some("PT5M25S"),
    )
    .await;
    assert_eq!(same_recording::drain(&pool, &off, 200).await.unwrap(), 0);
    assert!(card(&off, &pool, &a).await["maybe_same"].is_null());
    pass(&pool, &on).await;
    assert_eq!(named(&card(&on, &pool, &a).await), [b.as_str()]);
    assert!(card(&off, &pool, &a).await["maybe_same"].is_null());
    let field = |config: &Config| {
        api::openapi_document_for(config)["components"]["schemas"]["Card"]["properties"]
            ["maybe_same"]
            .is_object()
    };
    assert!(field(&on) && field(&off));
    let both_off: Config = toml::from_str(
        "[groups]\nsame_recording_hint = \"off\"\n[event]\nsame_event_hint = \"off\"\n",
    )
    .unwrap();
    assert!(!field(&both_off));
}

/// A pass stopped after claiming loses nothing: its claim runs out and the next pass reads the
/// recording again, dropping the pair its old length made. A change marked while a claim is held
/// is read too.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_stopped_pass_leaves_no_pair_stale(pool: PgPool) {
    let config = Config::default();
    let take_five = |pool: &PgPool, uri: &'static str, length: &'static str| {
        let pool = pool.clone();
        async move {
            recording(
                &pool,
                uri,
                "Take Five",
                Some("The Dave Brubeck Quartet"),
                Some(length),
            )
            .await
        }
    };
    let a = take_five(&pool, "https://tidal.com/track/1", "PT1M40S").await;
    let b = take_five(&pool, "https://www.deezer.com/track/1031007", "PT1M40S").await;
    pass(&pool, &config).await;
    assert_eq!(named(&card(&config, &pool, &a).await), [b.as_str()]);
    let pairs = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM same_recording")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    // The first track is now 3:20; a pass claims it and stops before reading it.
    take_five(&pool, "https://tidal.com/track/1", "PT3M20S").await;
    let claimed = same_recording::claim(&pool, 25, Duration::ZERO)
        .await
        .unwrap();
    assert!(claimed.iter().any(|(id, _)| id == &a), "{claimed:?}");
    // The claim has run out: the next pass reads the track and the 1:40 pair goes.
    pass(&pool, &config).await;
    assert_eq!(pairs().await, 0);
    assert!(named(&card(&config, &pool, &a).await).is_empty());
    // A claim is held (another pass is reading the track) when it changes back to 1:40: the
    // change moves the entry back to now, and the next pass pairs the two again.
    let held = same_recording::claim(&pool, 25, Duration::from_secs(300))
        .await
        .unwrap();
    assert!(held.is_empty(), "nothing is due: {held:?}");
    take_five(&pool, "https://tidal.com/track/1", "PT3M21S").await;
    let held = same_recording::claim(&pool, 25, Duration::from_secs(300))
        .await
        .unwrap();
    assert!(held.iter().any(|(id, _)| id == &a));
    take_five(&pool, "https://tidal.com/track/1", "PT1M40S").await;
    pass(&pool, &config).await;
    assert_eq!(named(&card(&config, &pool, &a).await), [b.as_str()]);
}
