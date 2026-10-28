//! The same-recording hint: `maybe_same` on recording cards names other recordings that are
//! possibly the same one, never merging anything.
use axum::{body::Body, extract::ConnectInfo, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Fact, Found, Key},
    api::{self, AppState},
    changes,
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
    // A read lists up to ten in the card itself (#52); here there are three.
    assert_eq!(tidal_card["maybe_same"].as_array().unwrap().len(), 3);
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

/// A crowd of recordings with one title and artist: each keeps at most twenty pairs, its own and
/// those others gave it, the lengths that agree before the ones unknown.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_crowd_of_one_recording_keeps_twenty_pairs_each(pool: PgPool) {
    let config = Config::default();
    for i in 0..101 {
        recording(
            &pool,
            &format!("https://tidal.com/track/{}", 1000 + i),
            "Take Five",
            Some("The Dave Brubeck Quartet"),
            Some("PT5M24S"),
        )
        .await;
    }
    let unknown = recording(
        &pool,
        "https://www.deezer.com/track/1031007",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        None,
    )
    .await;
    pass(&pool, &config).await;
    let most: i64 = sqlx::query_scalar(
        "SELECT max(n) FROM (SELECT count(*) AS n FROM same_recording GROUP BY resource_id) c",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(most, 20);
    let fewest: i64 = sqlx::query_scalar(
        "SELECT min(n) FROM (SELECT count(*) AS n FROM same_recording GROUP BY resource_id) c",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(fewest >= 1, "every recording keeps a pair");
    // A recording with a length keeps twenty whose lengths agree, never the one without.
    let first: String =
        sqlx::query_scalar("SELECT id FROM resource WHERE uri='https://tidal.com/track/1000'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let kept: Vec<(String, f32)> =
        sqlx::query_as("SELECT other_id,score FROM same_recording WHERE resource_id=$1")
            .bind(&first)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kept.len(), 20);
    assert!(
        kept.iter()
            .all(|(other, score)| other != &unknown && *score > 0.9),
        "{kept:?}"
    );
}

/// A claim that ran out never settles: a change saved after it ran out leaves the entry's time
/// as it was (the trigger only moves it earlier), so the old pass finishing late must not drop
/// the entry; the next pass reads the change.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_expired_claim_never_drops_a_later_change(pool: PgPool) {
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
    // A pass claims the first track (the lease already over), it becomes 3:20, then that pass
    // finishes.
    take_five(&pool, "https://tidal.com/track/1", "PT1M41S").await;
    let claimed = same_recording::claim(&pool, 25, Duration::ZERO)
        .await
        .unwrap();
    let (_, claim) = claimed.iter().find(|(id, _)| id == &a).unwrap().clone();
    take_five(&pool, "https://tidal.com/track/1", "PT3M20S").await;
    same_recording::settle(&pool, &a, &claim).await.unwrap();
    let due: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM same_recording_dirty WHERE resource_id=$1 AND next_attempt_at<=now()",
    )
    .bind(&a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(due, 1, "the expired claim left the entry due");
    pass(&pool, &config).await;
    assert!(named(&card(&config, &pool, &a).await).is_empty());
}

/// An artist or duration fact that had expired and is found again (its expiry extended, the row
/// kept) marks its recording: the pairs follow the fact's value again.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_fact_found_again_after_it_expired_moves_the_pairs(pool: PgPool) {
    let config = Config::default();
    let a = recording(
        &pool,
        "https://tidal.com/track/1",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        Some("PT1M40S"),
    )
    .await;
    let b = recording(
        &pool,
        "https://www.deezer.com/track/1031007",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        Some("PT1M40S"),
    )
    .await;
    // MusicBrainz once read the first track as 3:20; that fact has expired, so the page's 1:40
    // counts and the two pair.
    sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,seen_at,expires_at) VALUES($1,'duration','200','musicbrainz',now()-interval '2 days',now()-interval '1 day')")
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    pass(&pool, &config).await;
    assert_eq!(named(&card(&config, &pool, &a).await), [b.as_str()]);
    // MusicBrainz finds it again: the row stays, its expiry moves ahead, and the pair goes.
    found(&pool, &a, "musicbrainz", &[], &[("duration", "200")]).await;
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM resource_fact WHERE resource_id=$1 AND expires_at>now()",
    )
    .bind(&a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
    pass(&pool, &config).await;
    assert!(named(&card(&config, &pool, &a).await).is_empty());
    assert!(named(&card(&config, &pool, &b).await).is_empty());
    // A plain refresh of the valid fact marks nothing.
    found(&pool, &a, "musicbrainz", &[], &[("duration", "200")]).await;
    let due: i64 =
        sqlx::query_scalar("SELECT count(*) FROM same_recording_dirty WHERE resource_id=$1")
            .bind(&a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(due, 0);
}

/// Each read that changes pairs moves the revision search pages hash into their ETag, in its own
/// commit: a pass stopped after one recording leaves no cached page answering with old hints.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn each_read_that_changes_pairs_moves_the_revision(pool: PgPool) {
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
    let revision = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT revision FROM search_revision")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let (_, etag) = response(&config, &pool, "/v1/search?q=take+five").await;
    let before = revision().await;
    // The first track is now 3:20 and is read alone, as by a pass stopped right after it.
    take_five(&pool, "https://tidal.com/track/1", "PT3M20S").await;
    assert!(same_recording::read(&pool, &a).await.unwrap().changed);
    assert!(revision().await > before);
    assert!(named(&card(&config, &pool, &b).await).is_empty());
    let (_, after) = response(&config, &pool, "/v1/search?q=take+five").await;
    assert_ne!(etag, after);
    // A read that changes nothing leaves it.
    let still = revision().await;
    assert!(!same_recording::read(&pool, &a).await.unwrap().changed);
    assert_eq!(revision().await, still);
}

/// How many pairs each recording names (`by` resource_id) or is named by (`by` other_id): the
/// most and the fewest among those with any.
async fn spread(pool: &PgPool, by: &str) -> (i64, i64) {
    sqlx::query_as(&format!(
        "SELECT max(n),min(n) FROM (SELECT count(*) AS n FROM same_recording GROUP BY {by}) c"
    ))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A recording that changes takes its pairs from the recordings that named it, and those are read
/// again, so none is left without the hint (no sweep needed). Its pairs spread over the crowd, and
/// no recording is named by more than forty, which bounds the recordings one change marks.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_changed_recording_leaves_no_neighbour_without_a_hint(pool: PgPool) {
    let config = Config::default();
    let mut crowd = vec![];
    for i in 0..101 {
        crowd.push(
            recording(
                &pool,
                &format!("https://tidal.com/track/{}", 1000 + i),
                "Take Five",
                Some("The Dave Brubeck Quartet"),
                Some("PT5M24S"),
            )
            .await,
        );
    }
    pass(&pool, &config).await;
    assert_eq!(spread(&pool, "resource_id").await.0, 20);
    assert!(spread(&pool, "other_id").await.0 <= 40);
    // The twenty the first recording names are renamed.
    let picked: Vec<String> =
        sqlx::query_scalar("SELECT other_id FROM same_recording WHERE resource_id=$1")
            .bind(&crowd[0])
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(picked.len(), 20);
    for id in &picked {
        let uri: String = sqlx::query_scalar("SELECT uri FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        recording(
            &pool,
            &uri,
            "Blue Rondo à la Turk",
            Some("The Dave Brubeck Quartet"),
            Some("PT6M44S"),
        )
        .await;
    }
    pass(&pool, &config).await;
    for id in crowd.iter().filter(|id| !picked.contains(id)) {
        let left = named(&card(&config, &pool, id).await).len();
        assert!(left >= 1, "{id} lost its hint");
    }
    for id in &picked {
        // A card shows three; the stored list holds them all.
        let names: Vec<String> =
            sqlx::query_scalar("SELECT other_id FROM same_recording WHERE resource_id=$1")
                .bind(id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(names.len(), 19, "the renamed ones pair among themselves");
        assert!(names.iter().all(|n| picked.contains(n)));
    }
    let (most, _) = spread(&pool, "resource_id").await;
    assert!(most <= 20);
    assert!(spread(&pool, "other_id").await.0 <= 40);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM same_recording_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "the refills settled");
}

/// A search page reads the pairs and the revision its ETag hashes in one snapshot: a page that
/// read before a change carries the old revision with the old pairs, never the new revision (a
/// sequence moves outside the snapshot, and the next page would be answered 304 with old hints).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_page_reads_the_pairs_and_the_revision_from_one_snapshot(pool: PgPool) {
    let config = Config::default();
    let take_five = |uri: &'static str, length: &'static str| {
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
    let a = take_five("https://tidal.com/track/1", "PT1M40S").await;
    take_five("https://www.deezer.com/track/1031007", "PT1M40S").await;
    pass(&pool, &config).await;
    let seen = format!(
        "SELECT (SELECT count(*) FROM same_recording),{}",
        api::REVISION
    );
    // A page's snapshot, taken before the change.
    let mut page = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *page)
        .await
        .unwrap();
    let before: (i64, Value) = sqlx::query_as(&seen).fetch_one(&mut *page).await.unwrap();
    assert_eq!(before.0, 2);
    // The first track is now 3:20 and is read: its pairs go, the revision moves.
    take_five("https://tidal.com/track/1", "PT3M20S").await;
    assert!(same_recording::read(&pool, &a).await.unwrap().changed);
    let again: (i64, Value) = sqlx::query_as(&seen).fetch_one(&mut *page).await.unwrap();
    assert_eq!(again, before, "the old pairs come with the old revision");
    page.commit().await.unwrap();
    let after: (i64, Value) = sqlx::query_as(&seen).fetch_one(&pool).await.unwrap();
    assert_eq!(after.0, 0);
    assert_ne!(after.1, before.1);
}

/// A crowd of one recording's pages, with or without a length.
async fn crowd(pool: &PgPool, title: &str, length: Option<&str>, n: usize) -> Vec<String> {
    let mut ids = vec![];
    for i in 0..n {
        ids.push(
            recording(
                pool,
                &format!("https://tidal.com/track/{}", title.len() * 100_000 + i),
                title,
                Some("The Dave Brubeck Quartet"),
                length,
            )
            .await,
        );
    }
    ids
}

/// How many recordings name no other.
async fn without_a_hint(pool: &PgPool, ids: &[String]) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT id FROM unnest($1::text[]) AS t(id) WHERE NOT EXISTS(SELECT 1 FROM same_recording p WHERE p.resource_id=t.id)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Every recording of a crowd without lengths has a hint once the crowd is read, whatever the
/// order: one read late passes over the recordings already named by forty and names those with
/// room (round the index from just after itself), so the cap pushes nothing out that is not
/// replaced, and what it pushes out is refilled.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_crowd_without_lengths_read_in_any_order_leaves_none_without_a_hint(pool: PgPool) {
    let config = Config::default();
    let mut ascending = crowd(&pool, "Take Five", None, 101).await;
    ascending.sort();
    let mut descending = crowd(&pool, "Blue Rondo à la Turk", None, 101).await;
    descending.sort();
    descending.reverse();
    let mut shuffled = crowd(&pool, "Unsquare Dance", None, 101).await;
    shuffled.sort();
    // A fixed shuffle (an LCG), so a failure is reproducible.
    let mut seed = 42u64;
    for i in (1..shuffled.len()).rev() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        shuffled.swap(i, (seed >> 33) as usize % (i + 1));
    }
    for order in [&ascending, &descending, &shuffled] {
        for id in order.iter() {
            same_recording::read(&pool, id).await.unwrap();
        }
    }
    // The queue settles the refills.
    pass(&pool, &config).await;
    for (name, order) in [
        ("ascending", &ascending),
        ("descending", &descending),
        ("shuffled", &shuffled),
    ] {
        let none = without_a_hint(&pool, order).await;
        assert!(none.is_empty(), "{name}: without a hint: {none:?}");
    }
    assert_eq!(spread(&pool, "resource_id").await.0, 20);
    assert!(spread(&pool, "other_id").await.0 <= 40);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM same_recording_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "the refills settled");
}

/// A dense crowd read as it arrives (each recording read when only those before it are units,
/// in id order: the worst case, the ring wraps to the full ones first): a read examines at most
/// `SCAN` candidates, with room or not, so its work is bounded however large the crowd. A read
/// that ran out with room left stopped at the candidate it examined last and is due again; the
/// later read keeps the pairs it has and goes on from there, until every recording names twenty.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_read_in_a_dense_crowd_examines_a_bounded_number_of_candidates(pool: PgPool) {
    let config = Config::default();
    let mut ids = crowd(&pool, "Take Five", None, 301).await;
    ids.sort();
    let mut most = 0;
    let mut stopped = vec![];
    let mut partial: Option<(String, Vec<String>)> = None;
    for id in &ids {
        let read = same_recording::read(&pool, id).await.unwrap();
        assert!(
            read.examined as i64 <= same_recording::SCAN,
            "{id}: examined {}",
            read.examined
        );
        most = most.max(read.examined);
        let (pairs, stopped_at): (Vec<String>, Option<String>) = sqlx::query_as(
            "SELECT coalesce((SELECT array_agg(p.other_id) FROM same_recording p WHERE p.resource_id=u.resource_id),'{}'),u.scan_after \
             FROM same_recording_unit u WHERE u.resource_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        if read.examined as i64 == same_recording::SCAN && pairs.len() < 20 {
            assert!(stopped_at.is_some(), "{id}: ran out and did not stop");
            stopped.push(id.clone());
            if !pairs.is_empty() && partial.is_none() {
                partial = Some((id.clone(), pairs));
            }
        } else {
            assert!(
                stopped_at.is_none(),
                "{id}: stopped with {} pairs after {}",
                pairs.len(),
                read.examined
            );
        }
    }
    assert_eq!(most as i64, same_recording::SCAN, "the late reads ran out");
    assert!(stopped.len() > 100, "{} stopped", stopped.len());
    // (Every recording is due from its save here; the pass below reads them all once more.)
    let marked: Vec<String> = sqlx::query_scalar(
        "SELECT resource_id FROM same_recording_unit WHERE scan_after IS NOT NULL AND fingerprint LIKE 'refill:%' ORDER BY resource_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        marked, stopped,
        "the reads that stopped are marked, no other"
    );
    let (id, kept) = partial.expect("a read took some pairs before it ran out");
    // The later reads go on from where the first stopped, keeping their pairs.
    pass(&pool, &config).await;
    assert!(without_a_hint(&pool, &ids).await.is_empty());
    assert_eq!(spread(&pool, "resource_id").await, (20, 20));
    assert!(spread(&pool, "other_id").await.0 <= 40);
    let now: Vec<String> =
        sqlx::query_scalar("SELECT other_id FROM same_recording WHERE resource_id=$1")
            .bind(&id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        kept.iter().all(|k| now.contains(k)),
        "{id} kept {kept:?}, has {now:?}"
    );
    let (due, stopped): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM same_recording_dirty),(SELECT count(*) FROM same_recording_unit WHERE scan_after IS NOT NULL)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((due, stopped), (0, 0), "every read ended");
}

/// Resources deleted as orphans take their units and pairs with them (the cascade); the
/// recordings that named them are marked before, and refill.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn deleted_orphans_leave_their_neighbours_to_refill(pool: PgPool) {
    let config = Config::default();
    let crowd = crowd(&pool, "Take Five", Some("PT5M24S"), 101).await;
    pass(&pool, &config).await;
    let picked: Vec<String> =
        sqlx::query_scalar("SELECT other_id FROM same_recording WHERE resource_id=$1")
            .bind(&crowd[0])
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(picked.len(), 20);
    sqlx::query("DELETE FROM reference WHERE resource_id=ANY($1)")
        .bind(&picked)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '30 days' WHERE id=ANY($1)")
        .bind(&picked)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(changes::delete_orphans(&pool, 7).await.unwrap(), 20);
    let units: i64 = sqlx::query_scalar("SELECT count(*) FROM same_recording_unit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(units, 81);
    pass(&pool, &config).await;
    let survivors: Vec<String> = crowd
        .iter()
        .filter(|id| !picked.contains(id))
        .cloned()
        .collect();
    let none = without_a_hint(&pool, &survivors).await;
    assert!(none.is_empty(), "lost their hint: {none:?}");
    assert!(spread(&pool, "other_id").await.0 <= 40);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM same_recording_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "the refills settled");
}

/// A database from before the caps and the refills (matching-4 and -5) is repaired by the
/// upgrade's steps: lists the old pass left over the caps (it trimmed its own to twenty, but
/// nothing bounded the namers of one recording) are cut to the best twenty and forty as the pass
/// ranks them, and every recording with fewer than twenty pairs (its list emptied by the old cap
/// or a cascade, or a pair cut now) is marked to refill and queued, once.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_upgrade_cuts_the_old_lists_and_requeues_recordings_left_with_short_lists(
    pool: PgPool,
) {
    let config = Config::default();
    let mut ids = crowd(&pool, "Take Five", Some("PT5M24S"), 50).await;
    ids.sort();
    pass(&pool, &config).await;
    assert_eq!(spread(&pool, "resource_id").await, (20, 20));
    // The damage: the last five lose their lists, as the old cap trimmed them, and nothing marks
    // them; the first is named by the 44 others and names them all back (the old pass had no cap
    // on the namers). The first by id, so nothing cut from a list of twenty-one is its pair.
    sqlx::query("DELETE FROM same_recording WHERE resource_id=ANY($1)")
        .bind(&ids[45..])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO same_recording(resource_id,other_id,score) \
         SELECT o,$1,1 FROM unnest($2::text[]) AS t(o) UNION ALL SELECT $1,o,1 FROM unnest($2::text[]) AS t(o) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&ids[0])
    .bind(&ids[1..45])
    .execute(&pool)
    .await
    .unwrap();
    pass(&pool, &config).await;
    assert_eq!(
        without_a_hint(&pool, &ids).await.len(),
        5,
        "nothing queued them"
    );
    assert!(spread(&pool, "resource_id").await.0 >= 44);
    assert_eq!(spread(&pool, "other_id").await.0, 44);
    // The upgrade's steps (upgrade-matching-7.sql, upgrade-matching-4-to-7.sql).
    for step in [
        "DELETE FROM same_recording p USING (SELECT resource_id,other_id,row_number() OVER (PARTITION BY resource_id ORDER BY score DESC,other_id) AS n FROM same_recording) r WHERE p.resource_id=r.resource_id AND p.other_id=r.other_id AND r.n>20",
        "DELETE FROM same_recording p USING (SELECT resource_id,other_id,row_number() OVER (PARTITION BY other_id ORDER BY score DESC,resource_id) AS n FROM same_recording) r WHERE p.resource_id=r.resource_id AND p.other_id=r.other_id AND r.n>40",
        "UPDATE same_recording_unit u SET fingerprint='refill:'||fingerprint WHERE fingerprint NOT LIKE 'refill:%' AND (SELECT count(*) FROM same_recording p WHERE p.resource_id=u.resource_id)<20",
        "INSERT INTO same_recording_dirty(resource_id) SELECT resource_id FROM same_recording_unit WHERE fingerprint LIKE 'refill:%' ON CONFLICT DO NOTHING",
    ] {
        sqlx::query(step).execute(&pool).await.unwrap();
    }
    assert_eq!(spread(&pool, "resource_id").await.0, 20, "cut to twenty");
    assert_eq!(spread(&pool, "other_id").await.0, 40, "cut to forty");
    let (queued, short): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM same_recording_dirty),\
         (SELECT count(*) FROM same_recording_unit u WHERE (SELECT count(*) FROM same_recording p WHERE p.resource_id=u.resource_id)<20)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((queued, short), (9, 9), "the five, and the four namers cut");
    pass(&pool, &config).await;
    assert!(without_a_hint(&pool, &ids).await.is_empty());
    assert_eq!(spread(&pool, "resource_id").await, (20, 20));
    assert!(spread(&pool, "other_id").await.0 <= 40);
}

/// A pass that pairs with an orphan's recording while the orphan is being deleted: the delete
/// locks the orphan's unit before it collects the namers, so the pass's pair (committed while the
/// delete waits for it) is seen and its owner marked; without the lock the cascade would take the
/// pair unseen and the owner would keep an empty list for good.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_orphan_deleted_while_a_pass_pairs_with_it_leaves_the_partner_to_refill(pool: PgPool) {
    let config = Config::default();
    // Two recordings that do not pair (different titles), each with a unit.
    let live = recording(
        &pool,
        "https://tidal.com/track/1",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        Some("PT5M24S"),
    )
    .await;
    let orphan = recording(
        &pool,
        "https://tidal.com/track/2",
        "Blue Rondo à la Turk",
        Some("The Dave Brubeck Quartet"),
        Some("PT6M44S"),
    )
    .await;
    pass(&pool, &config).await;
    assert_eq!(
        without_a_hint(&pool, std::slice::from_ref(&live))
            .await
            .len(),
        1
    );
    sqlx::query("DELETE FROM reference WHERE resource_id=$1")
        .bind(&orphan)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '30 days' WHERE id=$1")
        .bind(&orphan)
        .execute(&pool)
        .await
        .unwrap();
    // A pass mid-flight: it has written the pair both ways and not committed yet (its FK checks
    // hold the orphan's unit FOR KEY SHARE).
    let mut pairing = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO same_recording(resource_id,other_id,score) VALUES($1,$2,0.7),($2,$1,0.7)",
    )
    .bind(&live)
    .bind(&orphan)
    .execute(&mut *pairing)
    .await
    .unwrap();
    let deleting = {
        let pool = pool.clone();
        tokio::spawn(async move { changes::delete_orphans(&pool, 7).await.unwrap() })
    };
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(!deleting.is_finished(), "the delete waits for the pass");
    pairing.commit().await.unwrap();
    assert_eq!(deleting.await.unwrap(), 1);
    let (fingerprint, queued): (String, bool) = sqlx::query_as(
        "SELECT fingerprint,EXISTS(SELECT 1 FROM same_recording_dirty d WHERE d.resource_id=u.resource_id) FROM same_recording_unit u WHERE resource_id=$1",
    )
    .bind(&live)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        fingerprint.starts_with("refill:"),
        "the partner is marked: {fingerprint}"
    );
    assert!(queued);
    assert_eq!(
        without_a_hint(&pool, std::slice::from_ref(&live))
            .await
            .len(),
        1,
        "the pair went with the orphan"
    );
}

/// A crowd of `n` recordings of one length, every one of them full: named by forty others at
/// score 1, so a recording of a lower score (one without a length) finds no room in any of them.
/// The namers are forty recordings of the same title by another artist, so they are candidates
/// for nobody in the crowd and their lists are never trimmed (their own reads find no candidate);
/// the pairs are written as the pass would leave them, with the crowd's own lists empty.
async fn full_crowd(pool: &PgPool, title: &str, length: &str, n: usize) -> Vec<String> {
    let mut ids = crowd(pool, title, Some(length), n).await;
    ids.sort();
    let mut namers = vec![];
    for i in 0..40 {
        namers.push(
            recording(
                pool,
                &format!("https://tidal.com/track/88{i:04}"),
                title,
                Some("Another Quartet"),
                Some(length),
            )
            .await,
        );
    }
    for id in ids.iter().chain(&namers) {
        same_recording::read(pool, id).await.unwrap();
    }
    sqlx::query("DELETE FROM same_recording")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO same_recording(resource_id,other_id,score) SELECT n,c,1 FROM unnest($1::text[]) AS a(n),unnest($2::text[]) AS b(c)")
        .bind(&namers)
        .bind(&ids)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM same_recording_dirty")
        .execute(pool)
        .await
        .unwrap();
    ids
}

/// `n` recordings of one length arriving after a crowd (their pages are their own, not the
/// crowd's), sorted by id.
async fn arrivals(pool: &PgPool, length: &str, n: usize) -> Vec<String> {
    let mut ids = vec![];
    for i in 0..n {
        ids.push(
            recording(
                pool,
                &format!("https://tidal.com/track/77{i:04}"),
                "Take Five",
                Some("The Dave Brubeck Quartet"),
                Some(length),
            )
            .await,
        );
    }
    ids.sort();
    ids
}

async fn pairs_of(pool: &PgPool, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT other_id FROM same_recording WHERE resource_id=$1 ORDER BY other_id")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A read that stopped goes on from the position it stopped at, not from where the candidate it
/// stopped at is now: a recording without a length examines a full crowd of 120 at 101 s, stops
/// at the hundredth; that one's length moves to 103 s, and twenty recordings at 102 s arrive
/// with room. The resumed read passes the last twenty of the crowd and names the twenty at 102 s.
/// (Resumed from the candidate's new position, it would skip them, end with no hints and never
/// look again, its fingerprint current.)
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_resumed_read_goes_on_from_the_position_it_stopped_at(pool: PgPool) {
    let full = full_crowd(&pool, "Take Five", "PT1M41S", 120).await;
    let me = recording(
        &pool,
        "https://tidal.com/track/999999",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        None,
    )
    .await;
    let read = same_recording::read(&pool, &me).await.unwrap();
    assert_eq!(read.examined as i64, same_recording::SCAN);
    assert!(pairs_of(&pool, &me).await.is_empty(), "the crowd is full");
    let (stopped_at, length): (Option<String>, Option<i32>) = sqlx::query_as(
        "SELECT scan_after,scan_after_length FROM same_recording_unit WHERE resource_id=$1",
    )
    .bind(&me)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (stopped_at.as_deref(), length),
        (Some(full[99].as_str()), Some(101))
    );
    // The candidate it stopped at moves (as a duration fact read later would move it).
    sqlx::query("UPDATE same_recording_unit SET length_secs=103 WHERE resource_id=$1")
        .bind(&full[99])
        .execute(&pool)
        .await
        .unwrap();
    let later = arrivals(&pool, "PT1M42S", 20).await;
    for id in &later {
        same_recording::read(&pool, id).await.unwrap();
    }
    let read = same_recording::read(&pool, &me).await.unwrap();
    assert!(read.changed);
    let now = pairs_of(&pool, &me).await;
    assert_eq!(
        now, later,
        "the twenty at 102 s, after the crowd's last twenty"
    );
    assert_eq!(
        read.examined, 40,
        "the crowd's last twenty, then the twenty"
    );
    let stopped: Option<String> =
        sqlx::query_scalar("SELECT scan_after FROM same_recording_unit WHERE resource_id=$1")
            .bind(&me)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stopped, None, "the read ended");
}

/// A pair added to a stopped read's list before it resumes (a recording ahead of its position
/// named it back) takes no place among the candidates the resumed read may still take: the
/// recording ends with twenty, not nineteen.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_pair_kept_from_before_takes_no_place_from_a_resumed_read(pool: PgPool) {
    let full = full_crowd(&pool, "Take Five", "PT1M41S", 120).await;
    let me = recording(
        &pool,
        "https://tidal.com/track/999999",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        None,
    )
    .await;
    same_recording::read(&pool, &me).await.unwrap();
    assert!(pairs_of(&pool, &me).await.is_empty());
    // Twenty-one recordings at 102 s arrive and read: each names the twenty others, so none
    // names this one; then one of them does (as a read with a place to spare would).
    let later = arrivals(&pool, "PT1M42S", 21).await;
    for id in &later {
        same_recording::read(&pool, id).await.unwrap();
    }
    let friend = &later[7];
    sqlx::query(
        "INSERT INTO same_recording(resource_id,other_id,score) VALUES($1,$2,0.7),($2,$1,0.7)",
    )
    .bind(friend)
    .bind(&me)
    .execute(&pool)
    .await
    .unwrap();
    same_recording::read(&pool, &me).await.unwrap();
    let now = pairs_of(&pool, &me).await;
    assert_eq!(now.len(), 20, "twenty, the kept pair among them: {now:?}");
    assert!(now.contains(friend));
    let _ = full;
}

/// A pass reading one recording (holding its unit) and an orphan delete taking the other
/// recording of a pair (holding that unit, then marking the first to refill) never wait for each
/// other: the pass takes its candidates with `SKIP LOCKED`, so the orphan's unit is passed over,
/// the pass commits, and the delete goes on and marks the first recording, which is read again
/// without the orphan. (Waiting for the orphan's unit, the two would deadlock, and one would fail.)
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_pass_and_an_orphan_delete_holding_each_others_unit_never_deadlock(pool: PgPool) {
    let config = Config::default();
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
        "https://tidal.com/track/2",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        Some("PT5M24S"),
    )
    .await;
    pass(&pool, &config).await;
    assert_eq!(pairs_of(&pool, &a).await, vec![b.clone()]);
    // The pass: it holds A's unit (marked to refill, as a lost pair marks it) and has not taken
    // its candidates yet.
    let mut reading = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE same_recording_unit SET fingerprint='refill:'||fingerprint WHERE resource_id=$1",
    )
    .bind(&a)
    .execute(&mut *reading)
    .await
    .unwrap();
    // The delete of B: it holds B's unit and waits to mark A.
    let deleting = {
        let pool = pool.clone();
        let b = b.clone();
        tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            same_recording::before_delete(&mut tx, &b).await.unwrap();
            sqlx::query("DELETE FROM resource WHERE id=$1")
                .bind(&b)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!deleting.is_finished(), "the delete waits for A");
    // The pass takes its candidates: B is held, so it is passed over, and the pass commits.
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        same_recording::read_in(&mut reading, &a),
    )
    .await
    .expect("the pass never waits for the delete")
    .unwrap();
    assert!(read.changed);
    reading.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), deleting)
        .await
        .expect("the delete goes on once the pass committed")
        .unwrap();
    let (fingerprint, queued, units): (String, bool, i64) = sqlx::query_as(
        "SELECT fingerprint,EXISTS(SELECT 1 FROM same_recording_dirty d WHERE d.resource_id=u.resource_id),(SELECT count(*) FROM same_recording_unit) FROM same_recording_unit u WHERE resource_id=$1",
    )
    .bind(&a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        fingerprint.starts_with("refill:"),
        "A is marked by the delete: {fingerprint}"
    );
    assert!(queued);
    assert_eq!(units, 1, "B's unit went with it");
    assert!(pairs_of(&pool, &a).await.is_empty());
    pass(&pool, &config).await;
    assert!(
        pairs_of(&pool, &a).await.is_empty(),
        "nothing left to pair with"
    );
}
