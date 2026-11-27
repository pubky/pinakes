//! The same-recording hint: `maybe_same` on recording cards names other recordings that are
//! possibly the same one, never merging anything.
use axum::{body::Body, extract::ConnectInfo, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Fact, Found, Key},
    api::{self, AppState},
    changes,
    config::{Config, SameEventHint},
    model::ResourceMeta,
    same_recording, store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
mod common;
use common::maybe_same_ids as named;

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

/// The pass, until nothing is due. A recording that stays due fails the test instead of hanging
/// it.
async fn pass(pool: &PgPool, config: &Config) {
    for _ in 0..10_000 {
        if same_recording::drain(pool, config, 200).await.unwrap() == 0 {
            return;
        }
    }
    panic!("the same-recording pass still has work after 10000 rounds");
}

/// The ids on a page of search results.
fn listed(page: &Value) -> Vec<&str> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect()
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

#[sqlx::test(migrations = "../../migrations")]
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
    common::drain_groups(&pool, &config.groups).await;
    let (_, before) = response(&config, &pool, "/v1/search?q=bohemian").await;
    // What the cards hold before any pair is stored: the keys, and the links each lists.
    let key_count = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_key")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let keys = key_count().await;
    assert_eq!(
        keys, 2,
        "the hub's key on its own page and on the Spotify track"
    );
    let tidal_links = card(&config, &pool, &tidal).await["also_on"].clone();
    let mb_links = card(&config, &pool, &mb).await["also_on"].clone();
    assert!(mb_links.to_string().contains(&spotify), "{mb_links}");
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
    // A read lists up to ten in the card itself; here there are three.
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
    let members = group["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{group}");
    assert!(members.iter().all(|m| m["maybe_same"].is_array()));

    // A hint, never a group, a key or a link: with the pairs stored, every group is computed
    // anew. The re-release and the page without a length still have no group, the recording's
    // group still has its two members, no key was added, and each card lists the links it did.
    common::regroup_all(&pool, &config.groups).await;
    let hinted = [tidal.clone(), unmeasured.clone()];
    let ungrouped: (i64, i64) =
        sqlx::query_as("SELECT count(group_id),count(*) FROM group_unit WHERE resource_id=ANY($1)")
            .bind(&hinted[..])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ungrouped, (0, 2));
    let mb_card = card(&config, &pool, &mb).await;
    let members: Vec<String> = sqlx::query_scalar(
        "SELECT resource_id FROM group_unit WHERE group_id=$1 ORDER BY resource_id",
    )
    .bind(mb_card["group"]["id"].as_str().unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut joined = vec![mb.clone(), spotify.clone()];
    joined.sort();
    assert_eq!(members, joined);
    assert_eq!(key_count().await, keys);
    let tidal_card = card(&config, &pool, &tidal).await;
    assert!(tidal_card["group"].is_null(), "{tidal_card}");
    assert_eq!(tidal_card["also_on"], tidal_links, "{tidal_card}");
    assert_eq!(named(&tidal_card).len(), 3, "the hint stands: {tidal_card}");
    assert!(card(&config, &pool, &unmeasured).await["group"].is_null());
    assert_eq!(mb_card["also_on"], mb_links, "{mb_card}");
    // A search by group lists each hinted card: only the recording's group of two is one row.
    let (all, _) = response(&config, &pool, "/v1/search?q=bohemian").await;
    let (collapsed, _) = response(&config, &pool, "/v1/search?q=bohemian&collapse=group").await;
    let rows = listed(&collapsed);
    for id in &hinted {
        assert!(rows.contains(&id.as_str()), "{id} is not listed: {rows:?}");
    }
    assert_eq!(rows.len() + 1, listed(&all).len(), "{rows:?}");

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

/// Any page may copy a track's title, artist and length into its markup. Such a page never comes
/// before a recording whose artist and length a source states, on a card or a search card,
/// whatever their scores and addresses.
#[sqlx::test(migrations = "../../migrations")]
async fn recordings_a_source_states_come_before_pages_that_state_themselves(pool: PgPool) {
    let config = Config::default();
    let track = recording(
        &pool,
        "https://tidal.com/track/77",
        "Copied Song",
        Some("Singer"),
        Some("PT3M20S"),
    )
    .await;
    let mut copies = vec![];
    for n in 1..=3 {
        copies.push(
            recording(
                &pool,
                &format!("https://aaa.example/{n}"),
                "Copied Song",
                Some("Singer"),
                Some("PT3M20S"),
            )
            .await,
        );
    }
    // A service's track whose page states nothing: its artist is a source's fact and its length
    // unknown, so its pair scores below the copies'.
    let mut doc = ResourceMeta::new("https://zzz.example/track/77", "web", 86400);
    doc.set_type("MusicRecording");
    doc.name = Some("Copied Song".into());
    let vouched = save(&pool, &doc).await;
    found(&pool, &vouched, "service", &[], &[("artist", "Singer")]).await;
    pass(&pool, &config).await;
    let read = card(&config, &pool, &track).await;
    assert_eq!(named(&read).len(), 4, "{read}");
    assert_eq!(named(&read)[0], vouched.as_str(), "{read}");
    assert_eq!(read["maybe_same"][0]["score"], 0.7, "{read}");
    // A search card keeps three: the vouched one first.
    let (page, _) = response(&config, &pool, "/v1/search?q=copied").await;
    let row = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == track.as_str())
        .unwrap_or_else(|| panic!("{page}"));
    assert_eq!(named(row).len(), 3, "{row}");
    assert_eq!(named(row)[0], vouched.as_str(), "{row}");
    // A copy's card lists the vouched one first too, ahead of the other copies and the track.
    let copy = card(&config, &pool, &copies[0]).await;
    assert_eq!(named(&copy)[0], vouched.as_str(), "{copy}");
}

/// The pass compares lengths in whole seconds: two recordings up to `LENGTH_WINDOW_SECS` (3 s)
/// apart agree and pair with score 1; 4 s apart they are no pair.
#[sqlx::test(migrations = "../../migrations")]
async fn lengths_three_seconds_apart_agree_and_four_do_not(pool: PgPool) {
    let config = Config::default();
    assert_eq!(same_recording::LENGTH_WINDOW_SECS, 3);
    let mut ids = vec![];
    // 5:24, 5:27 and 5:28: the first and the last are four seconds apart.
    for (n, length) in [(1, "PT5M24S"), (2, "PT5M27S"), (3, "PT5M28S")] {
        ids.push(
            recording(
                &pool,
                &format!("https://example.org/take-five/{n}"),
                "Take Five",
                Some("The Dave Brubeck Quartet"),
                Some(length),
            )
            .await,
        );
    }
    pass(&pool, &config).await;
    let (short, middle, long) = (&ids[0], &ids[1], &ids[2]);
    let short_card = card(&config, &pool, short).await;
    assert_eq!(named(&short_card), [middle.as_str()], "{short_card}");
    assert_eq!(short_card["maybe_same"][0]["score"], 1.0);
    let long_card = card(&config, &pool, long).await;
    assert_eq!(named(&long_card), [middle.as_str()], "{long_card}");
    assert_eq!(long_card["maybe_same"][0]["score"], 1.0);
    let middle_card = card(&config, &pool, middle).await;
    let mut both = named(&middle_card);
    both.sort();
    let mut want = vec![short.as_str(), long.as_str()];
    want.sort();
    assert_eq!(both, want, "{middle_card}");
}

/// `[groups] same_recording_hint = "off"` removes the field from recording cards and stops the
/// pass; the OpenAPI document keeps the field while either hint is on.
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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

/// A pass that pairs with an orphan's recording while the cleanup runs: the cleanup finds the unit
/// held and leaves the orphan for its next run, without waiting; that run locks the orphan's unit
/// before it collects the namers, so the pass's pair (committed by then) is seen and its owner
/// marked. Without the lock the cascade could take a pair unseen and the owner would keep an
/// empty list for good.
#[sqlx::test(migrations = "../../migrations")]
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
    // The cleanup leaves the orphan the pass is pairing with for its next run, without waiting.
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        changes::delete_orphans(&pool, 7),
    )
    .await
    .expect("the cleanup never waits for the pass")
    .unwrap();
    assert_eq!(first, 0);
    pairing.commit().await.unwrap();
    assert_eq!(changes::delete_orphans(&pool, 7).await.unwrap(), 1);
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

/// A crowd of `n` recordings of one length (or without one), every one of them full: named by forty others at
/// score 1, so a recording of a lower score (one without a length) finds no room in any of them.
/// The namers are forty recordings of the same title by another artist, so they are candidates
/// for nobody in the crowd and their lists are never trimmed (their own reads find no candidate);
/// the pairs are written as the pass would leave them, with the crowd's own lists empty.
async fn full_crowd(pool: &PgPool, title: &str, length: Option<&str>, n: usize) -> Vec<String> {
    let mut ids = crowd(pool, title, length, n).await;
    ids.sort();
    let mut namers = vec![];
    for i in 0..40 {
        namers.push(
            recording(
                pool,
                &format!("https://tidal.com/track/88{i:04}"),
                title,
                Some("Another Quartet"),
                length,
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
#[sqlx::test(migrations = "../../migrations")]
async fn a_resumed_read_goes_on_from_the_position_it_stopped_at(pool: PgPool) {
    let full = full_crowd(&pool, "Take Five", Some("PT1M41S"), 120).await;
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
#[sqlx::test(migrations = "../../migrations")]
async fn a_pair_kept_from_before_takes_no_place_from_a_resumed_read(pool: PgPool) {
    let full = full_crowd(&pool, "Take Five", Some("PT1M41S"), 120).await;
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

/// A resumed read whose kept pair's unit another transaction holds (an orphan delete takes it
/// `FOR UPDATE`, and may be taking the pair with it) neither waits for it nor keeps the pair: it
/// reads its list anew, passing the held unit over. (Kept unlocked, the pair could go with the
/// orphan and leave the list a pair short; waited for, the read could deadlock with a cleanup
/// batch that holds the orphans in another order.)
#[sqlx::test(migrations = "../../migrations")]
async fn a_resumed_read_keeps_no_pair_whose_unit_is_being_deleted(pool: PgPool) {
    let _full = full_crowd(&pool, "Take Five", Some("PT1M41S"), 120).await;
    let me = recording(
        &pool,
        "https://tidal.com/track/999999",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        None,
    )
    .await;
    same_recording::read(&pool, &me).await.unwrap();
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
    // An orphan delete holds the friend's unit.
    let mut deleting = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM same_recording_unit WHERE resource_id=$1 FOR UPDATE")
        .bind(friend)
        .execute(&mut *deleting)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), same_recording::read(&pool, &me))
        .await
        .expect("the resumed read does not wait for the held unit")
        .unwrap();
    deleting.rollback().await.unwrap();
    let now = pairs_of(&pool, &me).await;
    assert!(!now.contains(friend), "{now:?}");
}

/// A pass reading a recording whose values changed drops its own pairs, then those naming it,
/// and refills the recordings those named; the orphan cleanup deleting one of its partners drops
/// the orphan's pairs, then those naming it, and refills the recordings that named it. They
/// never deadlock: the cleanup takes the hint's write lock, so it waits for the pass's
/// transaction and then goes on.
/// (Waiting for the pass's pair rows while the pass waited for the cleanup's, or for marks the
/// cleanup made first, one of the two failed with a deadlock.) The pass is held right after it
/// dropped its own pairs.
#[sqlx::test(migrations = "../../migrations")]
async fn a_changed_recording_and_the_orphan_cleanup_never_deadlock_over_its_pairs(pool: PgPool) {
    const PAUSE: i64 = 23168;
    let config = Config::default();
    // Four alike: each names the three others.
    let crowd = crowd(&pool, "Take Five", Some("PT5M24S"), 4).await;
    pass(&pool, &config).await;
    let (me, orphan) = (&crowd[0], &crowd[1]);
    assert_eq!(pairs_of(&pool, me).await.len(), 3);
    sqlx::query("DELETE FROM reference WHERE resource_id=$1")
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '30 days' WHERE id=$1")
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
    // Its values changed, as a new length would change them.
    sqlx::query("UPDATE same_recording_unit SET fingerprint='before' WHERE resource_id=$1")
        .bind(me)
        .execute(&pool)
        .await
        .unwrap();
    common::pause_deletes(&pool, "same_recording", "b1-pass", PAUSE).await;
    let reader = common::named_connection(&pool, "b1-pass").await;
    let mut held = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(PAUSE)
        .execute(&mut *held)
        .await
        .unwrap();
    let reading = {
        let me = me.clone();
        tokio::spawn(async move { same_recording::read(&reader, &me).await })
    };
    common::lock_waiters_reach(&pool, 1, "the pass").await;
    // The cleanup waits for the pass's transaction (the hint's write lock), then goes on.
    let deleting = {
        let pool = pool.clone();
        tokio::spawn(async move { changes::delete_orphans(&pool, 7).await })
    };
    common::lock_waiters_reach(&pool, 2, "the cleanup").await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(PAUSE)
        .execute(&mut *held)
        .await
        .unwrap();
    let read = tokio::time::timeout(Duration::from_secs(20), reading)
        .await
        .expect("the pass ends")
        .unwrap()
        .unwrap();
    assert!(read.changed);
    let deleted = tokio::time::timeout(Duration::from_secs(20), deleting)
        .await
        .expect("the cleanup ends")
        .unwrap()
        .unwrap();
    assert_eq!(deleted, 1);
    pass(&pool, &config).await;
    let now = pairs_of(&pool, me).await;
    assert_eq!(now.len(), 2, "{now:?}");
    assert!(!now.contains(orphan));
}

/// The orphan cleanup marks the recordings that named its orphans only once the orphans are
/// deleted: a writer it waits for in the delete (here one holding the orphan's artist fact) can
/// mark them meanwhile, as a pass's refill does. (Marking first, the cleanup held the marks that
/// writer then waited for: a deadlock.)
#[sqlx::test(migrations = "../../migrations")]
async fn the_orphan_cleanup_marks_the_namers_after_its_deletes(pool: PgPool) {
    let config = Config::default();
    let crowd = crowd(&pool, "Take Five", Some("PT5M24S"), 3).await;
    pass(&pool, &config).await;
    let (me, orphan) = (&crowd[0], &crowd[1]);
    sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,seen_at,expires_at) VALUES($1,'artist','The Dave Brubeck Quartet','musicbrainz',now(),now()+interval '1 day')")
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
    pass(&pool, &config).await;
    sqlx::query("DELETE FROM reference WHERE resource_id=$1")
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '30 days' WHERE id=$1")
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
    // A writer holds the orphan's fact.
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM resource_fact WHERE resource_id=$1 FOR UPDATE")
        .bind(orphan)
        .execute(&mut *writer)
        .await
        .unwrap();
    let deleting = {
        let pool = pool.clone();
        tokio::spawn(async move { changes::delete_orphans(&pool, 7).await })
    };
    common::lock_waiters_reach(&pool, 1, "the cleanup").await;
    // The writer marks a recording that named the orphan, as a refill does, and waits for no one:
    // it gives up after half a second, before the deadlock detector's first check (a second),
    // whose retry would hide the wait.
    sqlx::query("SET LOCAL lock_timeout='500ms'")
        .execute(&mut *writer)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        sqlx::query("INSERT INTO same_recording_dirty(resource_id) VALUES($1) ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=excluded.next_attempt_at")
            .bind(me)
            .execute(&mut *writer),
    )
    .await
    .expect("the cleanup holds no mark while it waits")
    .unwrap();
    writer.commit().await.unwrap();
    let deleted = tokio::time::timeout(Duration::from_secs(20), deleting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(deleted, 1);
    let fingerprint: String =
        sqlx::query_scalar("SELECT fingerprint FROM same_recording_unit WHERE resource_id=$1")
            .bind(me)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(fingerprint.starts_with("refill:"), "{fingerprint}");
}

/// A refill that finds its unit held passes it over and leaves a mark in its place, which
/// outlives the holder: here another refill holds the unit and rolls back. The recording is read
/// again as a refill and fills the place the orphan left. (Without the mark, its fingerprint read
/// as current and its list stayed a pair short.)
#[sqlx::test(migrations = "../../migrations")]
async fn a_refill_mark_outlives_a_holder_that_rolls_back(pool: PgPool) {
    let config = Config::default();
    // 22 alike: each names 20 of the other 21.
    let crowd = crowd(&pool, "Take Five", Some("PT5M24S"), 22).await;
    pass(&pool, &config).await;
    let me = &crowd[0];
    let named = pairs_of(&pool, me).await;
    assert_eq!(named.len(), 20);
    let orphan = &named[0];
    sqlx::query("DELETE FROM reference WHERE resource_id=$1")
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '30 days' WHERE id=$1")
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
    // Another refill holds the recording's unit (it marked it) and will roll back.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE same_recording_unit SET fingerprint='refill:'||fingerprint WHERE resource_id=$1",
    )
    .bind(me)
    .execute(&mut *holder)
    .await
    .unwrap();
    let deleted = tokio::time::timeout(Duration::from_secs(10), changes::delete_orphans(&pool, 7))
        .await
        .expect("the cleanup never waits for the held unit")
        .unwrap();
    assert_eq!(deleted, 1);
    holder.rollback().await.unwrap();
    assert_eq!(
        pairs_of(&pool, me).await.len(),
        19,
        "the orphan's pair went"
    );
    // Its own read, alone (the pass's reads of the others could mark it again).
    same_recording::read(&pool, me).await.unwrap();
    let now = pairs_of(&pool, me).await;
    assert_eq!(now.len(), 20, "the place is filled again: {now:?}");
    let marks: i64 = sqlx::query_scalar("SELECT count(*) FROM same_recording_refill")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(marks, 0, "the read took the mark");
}

/// A pass whose recording's values changed (it holds its unit, written anew) and the orphan
/// cleanup deleting the recording it was paired with both ways (holding that unit) never
/// deadlock through their refills: the cleanup's refill of the pass's recording passes over the
/// unit the pass holds, so the delete goes through, and the pass's own refill finds the orphan
/// gone. (Each refill waiting for the other's unit, one of the two failed with a deadlock.)
#[sqlx::test(migrations = "../../migrations")]
async fn a_changed_recording_and_the_orphan_cleanup_never_deadlock_through_refill(pool: PgPool) {
    let config = Config::default();
    let a = recording(
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
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        Some("PT5M24S"),
    )
    .await;
    pass(&pool, &config).await;
    assert_eq!(pairs_of(&pool, &a).await, vec![orphan.clone()]);
    assert_eq!(pairs_of(&pool, &orphan).await, vec![a.clone()]);
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
    // The pass: it has written A's unit with other values (as its upsert does) and not dropped
    // the pairs naming A yet.
    let mut reading = pool.begin().await.unwrap();
    sqlx::query("UPDATE same_recording_unit SET fingerprint='before' WHERE resource_id=$1")
        .bind(&a)
        .execute(&mut *reading)
        .await
        .unwrap();
    // The cleanup: it locks the orphan's unit, then refills A, which named it.
    let deleting = {
        let pool = pool.clone();
        tokio::spawn(async move { changes::delete_orphans(&pool, 7).await })
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while !deleting.is_finished() && common::lock_waiters(&pool).await < 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the cleanup neither ends nor waits");
    // The pass goes on: its values changed, so the pairs naming A go and their recordings refill.
    let read = tokio::time::timeout(
        Duration::from_secs(20),
        same_recording::read_in(&mut reading, &a),
    )
    .await
    .expect("the pass ends")
    .unwrap();
    assert!(read.changed);
    reading.commit().await.unwrap();
    let deleted = tokio::time::timeout(Duration::from_secs(20), deleting)
        .await
        .expect("the cleanup ends")
        .unwrap()
        .unwrap();
    assert_eq!(deleted, 1);
    assert!(pairs_of(&pool, &a).await.is_empty());
    pass(&pool, &config).await;
    assert!(
        pairs_of(&pool, &a).await.is_empty(),
        "nothing left to pair with"
    );
}

/// A stopped read goes on from its position while the recording's values hold; once they change,
/// it starts over. A recording without a length stops in a full crowd of 120 without lengths,
/// among the lengths unknown. Then a duration is read for it and for one of the crowd, 101 s
/// both, and the next read pairs them: in its new ring the lengths that agree come first. (Gone
/// on from the stale position, the read would take up among the lengths unknown, past them.)
#[sqlx::test(migrations = "../../migrations")]
async fn a_changed_recording_starts_its_read_over_not_from_where_it_stopped(pool: PgPool) {
    let full = full_crowd(&pool, "Take Five", None, 120).await;
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
    assert!(stopped_at.is_some_and(|at| full.contains(&at)) && length.is_none());
    // A duration for one of the crowd (as a fact read later would leave it; its namers gone, so
    // it has room) and for the recording.
    let moved = &full[0];
    sqlx::query("UPDATE same_recording_unit SET length_secs=101 WHERE resource_id=$1")
        .bind(moved)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM same_recording WHERE other_id=$1")
        .bind(moved)
        .execute(&pool)
        .await
        .unwrap();
    recording(
        &pool,
        "https://tidal.com/track/999999",
        "Take Five",
        Some("The Dave Brubeck Quartet"),
        Some("PT1M41S"),
    )
    .await;
    let read = same_recording::read(&pool, &me).await.unwrap();
    assert!(read.changed);
    assert_eq!(pairs_of(&pool, &me).await, vec![moved.clone()]);
    assert_eq!(
        read.examined as i64,
        same_recording::SCAN,
        "the one at 101 s, then the crowd without lengths, full"
    );
}

/// A recording whose title compares as nothing (only a tag that goes) keeps a unit that pairs
/// with nothing: it is read once, and the sweep does not mark it again. One that loses its
/// title loses its pairs.
#[sqlx::test(migrations = "../../migrations")]
async fn a_recording_without_a_title_is_read_once(pool: PgPool) {
    let config = Config::default();
    let mut untitled = vec![];
    for n in [1_000_091, 1_000_092] {
        untitled.push(
            recording(
                &pool,
                &format!("https://tidal.com/track/{n}"),
                "[Explicit]",
                Some("Queen"),
                Some("PT5M54S"),
            )
            .await,
        );
    }
    same_recording::sweep(&pool).await.unwrap();
    pass(&pool, &config).await;
    // Nothing is left to mark, and nothing is due.
    assert_eq!(same_recording::sweep(&pool).await.unwrap(), 0);
    let due: i64 = sqlx::query_scalar("SELECT count(*) FROM same_recording_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(due, 0);
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT title_key FROM same_recording_unit ORDER BY resource_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(keys, ["", ""]);
    // Two recordings without a title are no pair.
    for id in &untitled {
        assert_eq!(named(&card(&config, &pool, id).await), Vec::<&str>::new());
    }
    // A pair, one of which loses its title: the pair goes, on both sides.
    let mut pair = vec![];
    for n in [1_000_093, 1_000_094] {
        pair.push(
            recording(
                &pool,
                &format!("https://tidal.com/track/{n}"),
                "Bohemian Rhapsody",
                Some("Queen"),
                Some("PT5M54S"),
            )
            .await,
        );
    }
    pass(&pool, &config).await;
    assert_eq!(
        named(&card(&config, &pool, &pair[0]).await),
        [pair[1].as_str()]
    );
    recording(
        &pool,
        "https://tidal.com/track/1000093",
        "[Explicit]",
        Some("Queen"),
        Some("PT5M54S"),
    )
    .await;
    pass(&pool, &config).await;
    for id in &pair {
        assert_eq!(named(&card(&config, &pool, id).await), Vec::<&str>::new());
    }
    assert_eq!(same_recording::sweep(&pool).await.unwrap(), 0);
}

/// A save holding its recording's row, and a dirty mark written beside it without that row's
/// lock: the sweep marking a recording never read, `also_on::prune` deleting its expired fact.
/// Neither waits for the other, and both marks land. The save is held between its row lock and
/// its update (a test trigger waits on an advisory lock), the mark goes in, then the save goes
/// on and its own trigger marks the recording too. (A save locking its row `FOR UPDATE` makes
/// the mark's foreign-key check wait for the save while holding the mark's new index entry, which
/// the save's trigger then waits for: a deadlock, and one of the two fails.)
#[sqlx::test(migrations = "../../migrations")]
async fn a_save_and_a_dirty_mark_beside_it_never_deadlock(pool: PgPool) {
    const PAUSE: i64 = 23166;
    sqlx::query(&format!(
        "CREATE FUNCTION test_pause() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock_shared({PAUSE}); RETURN NEW; END $$"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER test_pause BEFORE UPDATE ON resource FOR EACH ROW WHEN (NEW.name='Take Five (held)') EXECUTE FUNCTION test_pause()",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (n, mark) in ["sweep", "prune"].into_iter().enumerate() {
        let uri = format!("https://tidal.com/track/{}", 100 + n);
        let id = recording(
            &pool,
            &uri,
            "Take Five",
            Some("The Dave Brubeck Quartet"),
            Some("PT5M24S"),
        )
        .await;
        if mark == "prune" {
            // An expired artist fact (its insert marks the recording; that mark is cleared
            // below), and a unit, so the sweep has nothing to mark.
            sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,seen_at,expires_at) VALUES($1,'artist','Dave Brubeck','musicbrainz',now()-interval '2 days',now()-interval '1 day')")
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
            same_recording::read(&pool, &id).await.unwrap();
        }
        // Read once or never, nothing marked.
        sqlx::query("DELETE FROM same_recording_dirty")
            .execute(&pool)
            .await
            .unwrap();
        let mut held = pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(PAUSE)
            .execute(&mut *held)
            .await
            .unwrap();
        let saving = {
            let pool = pool.clone();
            let mut doc = ResourceMeta::new(&uri, "jsonld", 86400);
            doc.set_type("MusicRecording");
            doc.name = Some("Take Five (held)".into());
            doc.attributes
                .insert("artist".into(), "The Dave Brubeck Quartet".into());
            let id = id.clone();
            tokio::spawn(async move { store::save_document(&pool, &id, 0, &doc).await })
        };
        common::lock_waiters_reach(&pool, 1, "the save").await;
        let marking = {
            let pool = pool.clone();
            tokio::spawn(async move {
                if mark == "sweep" {
                    same_recording::sweep(&pool).await
                } else {
                    also_on::prune(&pool).await
                }
            })
        };
        // The mark is written, or waits for the save's row.
        tokio::time::timeout(Duration::from_secs(20), async {
            while !marking.is_finished() && common::lock_waiters(&pool).await < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the mark neither ends nor waits");
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(PAUSE)
            .execute(&mut *held)
            .await
            .unwrap();
        let marked = tokio::time::timeout(Duration::from_secs(20), marking)
            .await
            .unwrap()
            .unwrap();
        assert!(
            marked.unwrap_or_else(|e| panic!("{mark}: {e:#}")) >= 1,
            "{mark}"
        );
        tokio::time::timeout(Duration::from_secs(20), saving)
            .await
            .unwrap()
            .unwrap()
            .unwrap_or_else(|e| panic!("the save beside {mark}: {e:#}"));
        let due: i64 =
            sqlx::query_scalar("SELECT count(*) FROM same_recording_dirty WHERE resource_id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(due, 1, "{mark}");
    }
}

/// Makes `id` an orphan the cleanup deletes: no reference, orphaned long ago.
async fn orphan(pool: &PgPool, id: &str) {
    sqlx::query("DELETE FROM reference WHERE resource_id=$1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '30 days' WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// A recording that stops being one (its page no longer ok) drops its unit and refills the
/// recordings that named it, while the orphan cleanup deletes a recording paired with both (held
/// just before its deletes, past its hint lock). They never deadlock: the pass takes the hint's
/// write lock, so it waits for the cleanup's transaction. (Without it, the pass marked a namer,
/// then waited for the orphan's row in that mark's foreign key, while the cleanup's refill waited
/// for the pass's mark: a deadlock, the pass failing.)
#[sqlx::test(migrations = "../../migrations")]
async fn a_recording_that_drops_its_unit_beside_the_orphan_cleanup_never_deadlocks(pool: PgPool) {
    let config = Config::default();
    // Three alike: each names the two others.
    let crowd = crowd(&pool, "Take Five", Some("PT5M24S"), 3).await;
    pass(&pool, &config).await;
    let (dropped, gone, namer) = (crowd[0].clone(), crowd[1].clone(), crowd[2].clone());
    orphan(&pool, &gone).await;
    sqlx::query("UPDATE resource SET status='error' WHERE id=$1")
        .bind(&dropped)
        .execute(&pool)
        .await
        .unwrap();
    let reading = {
        let (pool, dropped) = (pool.clone(), dropped.clone());
        async move {
            let mut tx = pool.begin().await?;
            let read = same_recording::read_in(&mut tx, &dropped).await?;
            tx.commit().await?;
            anyhow::Ok(read)
        }
    };
    let (cleaned, read) = common::beside_a_held_cleanup(&pool, 23170, reading).await;
    assert_eq!(cleaned.unwrap(), 1);
    assert!(read.unwrap().changed);
    let units: Vec<String> =
        sqlx::query_scalar("SELECT resource_id FROM same_recording_unit ORDER BY 1")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(units, std::slice::from_ref(&namer));
    pass(&pool, &config).await;
    assert!(pairs_of(&pool, &namer).await.is_empty());
}

/// A pass whose values changed, in a crowd where each names twenty others (its trim drops pairs
/// of the recordings it names), beside the orphan cleanup held just before its deletes: they never
/// deadlock, as the pass waits for the cleanup's hint lock.
#[sqlx::test(migrations = "../../migrations")]
async fn a_trimming_pass_beside_the_orphan_cleanup_never_deadlocks(pool: PgPool) {
    let config = Config::default();
    let crowd = crowd(&pool, "Take Five", Some("PT5M24S"), 22).await;
    pass(&pool, &config).await;
    let (me, gone) = (
        crowd[0].clone(),
        pairs_of(&pool, &crowd[0]).await[0].clone(),
    );
    orphan(&pool, &gone).await;
    sqlx::query("UPDATE same_recording_unit SET fingerprint='before' WHERE resource_id=$1")
        .bind(&me)
        .execute(&pool)
        .await
        .unwrap();
    let reading = {
        let (pool, me) = (pool.clone(), me.clone());
        async move {
            let mut tx = pool.begin().await?;
            let read = same_recording::read_in(&mut tx, &me).await?;
            tx.commit().await?;
            anyhow::Ok(read)
        }
    };
    let (cleaned, read) = common::beside_a_held_cleanup(&pool, 23171, reading).await;
    assert_eq!(cleaned.unwrap(), 1);
    assert!(read.unwrap().changed);
    pass(&pool, &config).await;
    let now = pairs_of(&pool, &me).await;
    assert_eq!(now.len(), 20, "{now:?}");
    assert!(!now.contains(&gone));
}
