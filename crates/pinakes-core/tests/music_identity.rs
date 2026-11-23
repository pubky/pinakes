//! Exact music identity is symmetric; album containment is not identity.
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Found, Key, Link},
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
mod common;

async fn save(pool: &PgPool, uri: &str) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "web", 86400);
    doc.set_type("MusicRecording");
    doc.name = Some("Song".into());
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    id
}
async fn read(app: &axum::Router, id: &str) -> Value {
    let mut req = Request::builder()
        .uri(format!("/v1/resources/{id}"))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), 200);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
#[sqlx::test(migrations = "../../migrations")]
async fn held_exact_edges_share_keys_in_both_directions(pool: PgPool) {
    let cfg = Config::default();
    let a = save(
        &pool,
        "https://musicbrainz.org/recording/3276d651-6f19-4e3b-bfb2-d136f823f620",
    )
    .await;
    let b_uri = "https://www.deezer.com/track/3507590481";
    let b = save(&pool, b_uri).await;
    let c_uri = "https://open.spotify.com/track/0AAAAAAAAAAAAAAAAAAAAA";
    let c = save(&pool, c_uri).await;
    let mb = "mb:recording:3276d651-6f19-4e3b-bfb2-d136f823f620";
    let isrc = "isrc:NLH2L2500268";
    for (id, key, link) in [
        (&a, mb, Link::new(b_uri, "Deezer", "musicbrainz").keyed(mb)),
        (&b, isrc, Link::new(c_uri, "Spotify", "deezer").keyed(isrc)),
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
                links: vec![link],
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
    let mut group = None;
    for id in [&a, &b, &c] {
        let card = read(&app, id).await;
        assert_eq!(card["group"]["size"], 3, "{card}");
        if let Some(g) = &group {
            assert_eq!(g, &card["group"]["id"]);
        }
        group = Some(card["group"]["id"].clone());
        let keys = card["keys"].as_array().unwrap();
        assert!(
            keys.contains(&Value::String(mb.into())) && keys.contains(&Value::String(isrc.into())),
            "every member must expose the exact group keys: {card}"
        );
    }
    // Membership can briefly lag a missing observation: never borrow that member's
    // keys or its outgoing keyed evidence while reconciliation is queued.
    sqlx::query("UPDATE resource SET status='missing',doc=jsonb_set(doc,'{status}','\"missing\"'::jsonb) WHERE id=$1")
        .bind(&a).execute(&pool).await.unwrap();
    let live = read(&app, &c).await;
    assert!(
        !live["keys"]
            .as_array()
            .unwrap()
            .contains(&Value::String(mb.into())),
        "missing donor leaked: {live}"
    );
}
