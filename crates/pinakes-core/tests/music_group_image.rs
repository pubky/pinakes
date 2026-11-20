//! Reuse held exact group evidence, without turning album containment into identity.
mod common;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Found, Key, Link},
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
const IMAGE: &str =
    "https://coverartarchive.org/release/2b4cd0d4-fe21-46ac-a4fc-ece7e990a9d4/front-500";
/// An image no open database credits: a service's own.
const OWN_IMAGE: &str = "https://images.example.com/cover.jpg";
async fn save(pool: &PgPool, uri: &str, kind: &str, image: bool) -> String {
    save_with(pool, uri, kind, image.then_some(IMAGE), image).await
}
/// Saves a `kind` card for `uri` with `image`; `credited` gives the image its Cover Art Archive
/// credit.
async fn save_with(
    pool: &PgPool,
    uri: &str,
    kind: &str,
    image: Option<&str>,
    credited: bool,
) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "web", 86400);
    doc.set_type(kind);
    doc.name = Some("Song".into());
    doc.image = image.map(str::to_owned);
    if credited {
        doc.credits=serde_json::from_value(json!([{"field":"image","provider":"cover_art_archive","source_url":"https://musicbrainz.org/release/2b4cd0d4-fe21-46ac-a4fc-ece7e990a9d4/cover-art","license":"","excerpted":false,"modified":false}])).unwrap();
    }
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    id
}
/// Gives each of `ids` the hub key `key` and groups them: one group.
async fn grouped(pool: &PgPool, cfg: &Config, key: &str, ids: &[&String]) {
    for id in ids {
        also_on::record(
            pool,
            id,
            "musicbrainz",
            Found {
                keys: vec![Key {
                    key: key.into(),
                    via: "musicbrainz".into(),
                }],
                ..Default::default()
            },
            Duration::from_secs(86400),
            &cfg.also_on,
        )
        .await
        .unwrap();
    }
    common::drain_groups(pool, &cfg.groups).await;
}
async fn read(app: &axum::Router, id: &str) -> Value {
    let mut req = Request::builder()
        .uri(format!("/v1/resources/{id}"))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.3:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let r = app.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), 200);
    serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
#[sqlx::test(migrations = "../../migrations")]
async fn exact_chain_shares_image_credit_and_also_on_both_ways(pool: PgPool) {
    let cfg = Config::default();
    let a_uri = "https://musicbrainz.org/recording/3276d651-6f19-4e3b-bfb2-d136f823f620";
    let b_uri = "https://www.deezer.com/track/3507590481";
    let c_uri = "https://open.spotify.com/track/0AAAAAAAAAAAAAAAAAAAAA";
    let a = save(&pool, a_uri, "MusicRecording", true).await;
    let b = save(&pool, b_uri, "MusicRecording", false).await;
    let c = save(&pool, c_uri, "MusicRecording", false).await;
    for (id, key, uri) in [
        (
            &a,
            "mb:recording:3276d651-6f19-4e3b-bfb2-d136f823f620",
            b_uri,
        ),
        (&b, "isrc:NLH2L2500268", c_uri),
    ] {
        also_on::record(
            &pool,
            id,
            "musicbrainz",
            Found {
                keys: vec![Key {
                    key: key.into(),
                    via: "musicbrainz".into(),
                }],
                links: vec![Link::new(
                    uri,
                    if uri == b_uri { "Deezer" } else { "Spotify" },
                    "musicbrainz",
                )
                .keyed(key)],
                ..Default::default()
            },
            Duration::from_secs(86400),
            &cfg.also_on,
        )
        .await
        .unwrap();
    }
    common::drain_groups(&pool, &cfg.groups).await;
    let app = api::router(AppState::new(pool.clone(), cfg.clone()).unwrap());
    for id in [&a, &b, &c] {
        let v = read(&app, id).await;
        assert_eq!(v["image"], IMAGE, "{v}");
        assert!(v["credits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["provider"] == "cover_art_archive"));
        assert_eq!(v["group"]["size"], 3);
    }
    let v = read(&app, &c).await;
    for uri in [a_uri, b_uri] {
        assert!(
            v["also_on"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["uri"] == uri),
            "reverse member {uri}: {v}"
        );
    }
    // Reconciliation retracts expired edges; no borrowed image is copied into a document.
    sqlx::query("UPDATE derived_link SET expires_at=now()-interval '1 second';")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO group_dirty(resource_id) SELECT id FROM resource ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    common::drain_groups(&pool, &cfg.groups).await;
    assert!(read(&app, &c).await["image"].is_null());
}
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}
#[sqlx::test(migrations = "../../migrations")]
async fn committed_music_groups_are_logged_and_capped(pool: PgPool) {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let mut cfg = Config::default();
    cfg.groups.max_members = 2;
    let mut ids = Vec::new();
    for n in 1..=4 {
        let id = save(
            &pool,
            &format!("https://example.com/music/{n}"),
            "MusicRecording",
            false,
        )
        .await;
        also_on::record(
            &pool,
            &id,
            "musicbrainz",
            Found {
                keys: vec![Key {
                    key: "isrc:NLH2L2500268".into(),
                    via: "musicbrainz".into(),
                }],
                ..Default::default()
            },
            Duration::from_secs(86400),
            &cfg.also_on,
        )
        .await
        .unwrap();
        ids.push(id);
    }
    common::drain_groups(&pool, &cfg.groups).await;
    let sizes: Vec<i64> = sqlx::query_scalar(
        "SELECT count(*) FROM group_unit WHERE group_id IS NOT NULL GROUP BY group_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(sizes.contains(&2));
    assert!(sizes.iter().all(|n| *n <= 2), "{sizes:?}");
    // The subscriber is the process's: the cap is reported from a blocking thread, which a
    // thread's own subscriber never sees. So each line is told from a sibling test's by a value
    // only this test has: one of its resources, and its cap of 2.
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.lines().any(|line| {
            line.contains("group reconciliation committed")
                && ids.iter().any(|id| line.contains(id.as_str()))
        }),
        "{text}"
    );
    assert!(
        text.lines().any(|line| {
            line.contains("group size cap reached")
                && line.split_whitespace().any(|field| field == "cap=2")
        }),
        "{text}"
    );
}
/// Two tracks of one album each name the album with a link keyed by the album's own key. A
/// link's key belongs to what it names: the album takes it, the tracks do not, so neither track
/// joins the album or the other track. A page that holds the album's key itself joins the album,
/// and a page a track's link names by the track's key joins that track.
#[sqlx::test(migrations = "../../migrations")]
async fn album_link_does_not_union_track_groups(pool: PgPool) {
    let cfg = Config::default();
    let release = "mb:release:2b4cd0d4-fe21-46ac-a4fc-ece7e990a9d4";
    let album_uri = "https://musicbrainz.org/release/2b4cd0d4-fe21-46ac-a4fc-ece7e990a9d4";
    let same_track_uri = "https://open.spotify.com/track/0AAAAAAAAAAAAAAAAAAAAA";
    let album = save(&pool, album_uri, "MusicAlbum", true).await;
    let same_album = save(
        &pool,
        "https://example.com/albums/2b4cd0d4",
        "MusicAlbum",
        false,
    )
    .await;
    let a = save(
        &pool,
        "https://www.deezer.com/track/3507590481",
        "MusicRecording",
        false,
    )
    .await;
    let b = save(
        &pool,
        "https://www.deezer.com/track/3507590482",
        "MusicRecording",
        false,
    )
    .await;
    let same_track = save(&pool, same_track_uri, "MusicRecording", false).await;
    for (id, key, links) in [
        (&same_album, release, vec![]),
        (
            &a,
            "isrc:NLH2L2500268",
            vec![
                Link::new(album_uri, "MusicBrainz", "musicbrainz").keyed(release),
                Link::new(same_track_uri, "Spotify", "musicbrainz").keyed("isrc:NLH2L2500268"),
            ],
        ),
        (
            &b,
            "isrc:NLH2L2500269",
            vec![Link::new(album_uri, "MusicBrainz", "musicbrainz").keyed(release)],
        ),
    ] {
        also_on::record(
            &pool,
            id,
            "musicbrainz",
            Found {
                keys: vec![Key {
                    key: key.into(),
                    via: "musicbrainz".into(),
                }],
                links,
                ..Default::default()
            },
            Duration::from_secs(86400),
            &cfg.also_on,
        )
        .await
        .unwrap();
    }
    common::drain_groups(&pool, &cfg.groups).await;
    let app = api::router(AppState::new(pool.clone(), cfg).unwrap());
    let group = |v: Value| v["group"].clone();
    // The keys do join here: the album with the page that holds its key, the first track with
    // the page its link names by the track's key.
    let albums = group(read(&app, &album).await);
    assert_eq!(albums["size"], 2, "{albums}");
    assert_eq!(group(read(&app, &same_album).await), albums);
    let tracks = group(read(&app, &a).await);
    assert_eq!(tracks["size"], 2, "{tracks}");
    assert_eq!(group(read(&app, &same_track).await), tracks);
    // Containment is not identity: the tracks' group is not the album's, and the second track,
    // which shares only the album with the first, is in neither.
    assert_ne!(tracks["id"], albums["id"]);
    let v = read(&app, &b).await;
    assert!(v["group"].is_null(), "{v}");
}
/// A card takes a group member's image only from a healthy member of its own type whose image
/// an open database credits, and only when it has none of its own. Each case is refused first
/// and lends once the one thing in the way is changed (or, for the last, lends until it is).
#[sqlx::test(migrations = "../../migrations")]
async fn an_image_is_borrowed_only_from_a_healthy_same_type_member_with_a_credited_image(
    pool: PgPool,
) {
    let cfg = Config::default();
    let app = api::router(AppState::new(pool.clone(), cfg.clone()).unwrap());
    let lent = |v: &Value| {
        v["credits"]
            .as_array()
            .is_some_and(|credits| credits.iter().any(|c| c["provider"] == "cover_art_archive"))
    };

    // A member of another type: an album's cover is not a recording's.
    let borrower_uri = "https://example.com/borrow/type/track";
    let donor_uri = "https://example.com/borrow/type/album";
    let borrower = save(&pool, borrower_uri, "MusicRecording", false).await;
    let donor = save(&pool, donor_uri, "MusicAlbum", true).await;
    grouped(&pool, &cfg, "isrc:NLH2L2500301", &[&borrower, &donor]).await;
    let v = read(&app, &borrower).await;
    assert_eq!(v["group"]["size"], 2, "{v}");
    assert!(v["image"].is_null() && !lent(&v), "another type: {v}");
    save(&pool, donor_uri, "MusicRecording", true).await;
    common::drain_groups(&pool, &cfg.groups).await;
    let v = read(&app, &borrower).await;
    assert_eq!(v["group"]["size"], 2, "{v}");
    assert!(v["image"] == IMAGE && lent(&v), "the same type: {v}");

    // A member's image that no open database credits.
    let borrower_uri = "https://example.com/borrow/store/track";
    let donor_uri = "https://example.com/borrow/store/other";
    let borrower = save(&pool, borrower_uri, "MusicRecording", false).await;
    let donor = save_with(&pool, donor_uri, "MusicRecording", Some(OWN_IMAGE), false).await;
    grouped(&pool, &cfg, "isrc:NLH2L2500302", &[&borrower, &donor]).await;
    let v = read(&app, &borrower).await;
    assert_eq!(v["group"]["size"], 2, "{v}");
    assert!(
        v["image"].is_null() && !lent(&v),
        "an uncredited image: {v}"
    );
    save(&pool, donor_uri, "MusicRecording", true).await;
    common::drain_groups(&pool, &cfg.groups).await;
    let v = read(&app, &borrower).await;
    assert!(v["image"] == IMAGE && lent(&v), "a credited image: {v}");

    // A card with an image of its own keeps it.
    let borrower_uri = "https://example.com/borrow/own/track";
    let donor_uri = "https://example.com/borrow/own/other";
    let borrower = save_with(
        &pool,
        borrower_uri,
        "MusicRecording",
        Some(OWN_IMAGE),
        false,
    )
    .await;
    let donor = save(&pool, donor_uri, "MusicRecording", true).await;
    grouped(&pool, &cfg, "isrc:NLH2L2500303", &[&borrower, &donor]).await;
    let v = read(&app, &borrower).await;
    assert_eq!(v["group"]["size"], 2, "{v}");
    assert!(v["image"] == OWN_IMAGE && !lent(&v), "its own image: {v}");
    save(&pool, borrower_uri, "MusicRecording", false).await;
    common::drain_groups(&pool, &cfg.groups).await;
    let v = read(&app, &borrower).await;
    assert!(v["image"] == IMAGE && lent(&v), "no image of its own: {v}");

    // A member that is not readable lends nothing.
    let borrower_uri = "https://example.com/borrow/missing/track";
    let donor_uri = "https://example.com/borrow/missing/other";
    let borrower = save(&pool, borrower_uri, "MusicRecording", false).await;
    let donor = save(&pool, donor_uri, "MusicRecording", true).await;
    grouped(&pool, &cfg, "isrc:NLH2L2500304", &[&borrower, &donor]).await;
    let v = read(&app, &borrower).await;
    assert!(v["image"] == IMAGE && lent(&v), "a healthy member: {v}");
    sqlx::query("UPDATE resource SET status='missing' WHERE id=$1")
        .bind(&donor)
        .execute(&pool)
        .await
        .unwrap();
    let v = read(&app, &borrower).await;
    assert_eq!(v["group"]["size"], 2, "{v}");
    assert!(v["image"].is_null() && !lent(&v), "a missing member: {v}");
}
/// A solo artist's card is a `Person`: on MusicBrainz or a music service it takes its group's
/// keys and image like any other music card. A `Person` page anywhere else takes neither.
#[sqlx::test(migrations = "../../migrations")]
async fn a_solo_artists_card_takes_its_groups_keys_and_image(pool: PgPool) {
    let cfg = Config::default();
    let artist = "mb:artist:a74b1b7f-71a5-4011-9441-d0b5e4122711";
    let item = "wikidata:Q7259";
    let mb_uri = "https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711";
    let spotify_uri = "https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb";
    let page_uri = "https://example.org/people/ada";
    let other_uri = "https://example.com/ada";
    let mb = save(&pool, mb_uri, "Person", true).await;
    let spotify = save(&pool, spotify_uri, "Person", false).await;
    let page = save(&pool, page_uri, "Person", true).await;
    let other = save(&pool, other_uri, "Person", false).await;
    for (id, key, uri, site) in [
        (&mb, artist, spotify_uri, "Spotify"),
        (&page, item, other_uri, "Example"),
    ] {
        also_on::record(
            &pool,
            id,
            "musicbrainz",
            Found {
                keys: vec![Key {
                    key: key.into(),
                    via: "musicbrainz".into(),
                }],
                links: vec![Link::new(uri, site, "musicbrainz").keyed(key)],
                ..Default::default()
            },
            Duration::from_secs(86400),
            &cfg.also_on,
        )
        .await
        .unwrap();
    }
    common::drain_groups(&pool, &cfg.groups).await;
    let app = api::router(AppState::new(pool.clone(), cfg).unwrap());
    let has_key = |card: &Value, key: &str| {
        card["keys"]
            .as_array()
            .unwrap()
            .contains(&Value::String(key.into()))
    };
    let v = read(&app, &spotify).await;
    assert_eq!(v["group"]["size"], 2, "{v}");
    assert_eq!(v["type"], "Person", "{v}");
    assert_eq!(v["image"], IMAGE, "{v}");
    assert!(has_key(&v, artist), "{v}");
    // The other pair is grouped the same way, and is no music card.
    let v = read(&app, &other).await;
    assert_eq!(v["group"]["size"], 2, "{v}");
    assert!(v["image"].is_null(), "{v}");
    assert!(!has_key(&v, item), "{v}");
}
