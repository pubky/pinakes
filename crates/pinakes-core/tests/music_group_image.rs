//! Reuse held exact group evidence, without turning album containment into identity.
mod common;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Found, Key, Link},
    api::{self, AppState},
    config::Config,
    groups,
    model::ResourceMeta,
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
const IMAGE: &str =
    "https://coverartarchive.org/release/2b4cd0d4-fe21-46ac-a4fc-ece7e990a9d4/front-500";
async fn save(pool: &PgPool, uri: &str, kind: &str, image: bool) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "web", 86400);
    doc.set_type(kind);
    doc.name = Some("Song".into());
    if image {
        doc.image = Some(IMAGE.into());
        doc.credits=serde_json::from_value(json!([{"field":"image","provider":"cover_art_archive","source_url":"https://musicbrainz.org/release/2b4cd0d4-fe21-46ac-a4fc-ece7e990a9d4/cover-art","license":"","excerpted":false,"modified":false}])).unwrap();
    }
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    id
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
    groups::drain(&pool, &cfg.groups, 100).await.unwrap();
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
    groups::drain(&pool, &cfg.groups, 100).await.unwrap();
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn committed_music_groups_are_logged_and_capped(pool: PgPool) {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
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
    tracing::instrument::WithSubscriber::with_subscriber(
        groups::drain(&pool, &cfg.groups, 100),
        subscriber,
    )
    .await
    .unwrap();
    let sizes: Vec<i64> = sqlx::query_scalar(
        "SELECT count(*) FROM group_unit WHERE group_id IS NOT NULL GROUP BY group_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(sizes.contains(&2));
    assert!(sizes.iter().all(|n| *n <= 2), "{sizes:?}");
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("group reconciliation committed"), "{text}");
    assert!(text.contains("group size cap reached"), "{text}");
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn album_link_does_not_union_track_groups(pool: PgPool) {
    let cfg = Config::default();
    let album_uri = "https://musicbrainz.org/release/2b4cd0d4-fe21-46ac-a4fc-ece7e990a9d4";
    let album = save(&pool, album_uri, "MusicAlbum", true).await;
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
    for (id, key) in [(&a, "isrc:NLH2L2500268"), (&b, "isrc:NLH2L2500269")] {
        also_on::record(
            &pool,
            id,
            "musicbrainz",
            Found {
                keys: vec![Key {
                    key: key.into(),
                    via: "musicbrainz".into(),
                }],
                links: vec![Link::new(album_uri, "MusicBrainz", "musicbrainz")],
                ..Default::default()
            },
            Duration::from_secs(86400),
            &cfg.also_on,
        )
        .await
        .unwrap();
    }
    groups::drain(&pool, &cfg.groups, 100).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), cfg).unwrap());
    for id in [&a, &b, &album] {
        let v = read(&app, id).await;
        assert!(
            v["group"].is_null() || v["group"]["size"] == 1,
            "containment isn't identity: {v}"
        );
    }
}
