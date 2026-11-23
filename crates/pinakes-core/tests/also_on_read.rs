//! A resource read's `also_on` is a card's: one link per service, never another page of the
//! card's own service (`also_on()`), whatever the sources found.
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, Found, Link},
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
mod common;

/// Get Lucky's Spotify card as stable had it (2026-11-05): fifteen derived links, several of
/// one service, two of them Spotify's. Its read lists one per service and no other Spotify page.
#[sqlx::test(migrations = "../../migrations")]
async fn a_read_shows_one_link_per_service_and_none_of_its_own(pool: PgPool) {
    let uri = "https://open.spotify.com/track/69kOkLUCkxIZYexIgSG8rq";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type("MusicRecording");
    doc.name = Some("Get Lucky".into());
    doc.service = Some("Spotify".into());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let links = [
        ("https://music.apple.com/us/song/1673536443", "Apple Music"),
        ("https://music.apple.com/us/song/617154366", "Apple Music"),
        ("https://tidal.com/track/293301151", "TIDAL"),
        ("https://tidal.com/track/20115564", "TIDAL"),
        ("https://www.deezer.com/track/2271581007", "Deezer"),
        ("https://www.deezer.com/track/67238735", "Deezer"),
        (
            "https://open.spotify.com/track/118Opfz7qv2KA1g3Pmyk9T",
            "Spotify",
        ),
        (
            "https://open.spotify.com/track/2olIQt0rL0hOHau1SJ4xf2",
            "Spotify",
        ),
    ];
    also_on::record(
        &pool,
        &id,
        "musicbrainz",
        Found {
            links: links
                .iter()
                .map(|(u, s)| Link::new(*u, *s, "musicbrainz"))
                .collect(),
            ..Default::default()
        },
        std::time::Duration::from_secs(86_400),
        &Config::default().also_on,
    )
    .await
    .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let path = format!(
        "/v1/resources/by-uri?uri={}",
        url::form_urlencoded::byte_serialize(uri.as_bytes()).collect::<String>()
    );
    let mut request = Request::builder().uri(&path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let v: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    common::check_response("GET", &path, 200, &v);
    let mut sites: Vec<&str> = v["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["site"].as_str().unwrap())
        .collect();
    sites.sort();
    assert_eq!(sites, ["Apple Music", "Deezer", "TIDAL"], "{v}");
}

/// A card and each of its `also_on` links carry the frame that shows them (docs/api.md, Embed):
/// a service's player from the link alone, `null` for a link without one.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_and_its_also_on_links_carry_their_embed(pool: PgPool) {
    let uri = "https://open.spotify.com/track/69kOkLUCkxIZYexIgSG8rq";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type("MusicRecording");
    doc.name = Some("Get Lucky".into());
    doc.service = Some("Spotify".into());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let links = [
        ("https://tidal.com/track/20115564", "TIDAL"),
        ("https://www.deezer.com/track/67238735", "Deezer"),
        ("https://www.discogs.com/master/1234", "Discogs"),
    ];
    also_on::record(
        &pool,
        &id,
        "musicbrainz",
        Found {
            links: links
                .iter()
                .map(|(u, s)| Link::new(*u, *s, "musicbrainz"))
                .collect(),
            ..Default::default()
        },
        std::time::Duration::from_secs(86_400),
        &Config::default().also_on,
    )
    .await
    .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let path = format!(
        "/v1/resources/by-uri?uri={}",
        url::form_urlencoded::byte_serialize(uri.as_bytes()).collect::<String>()
    );
    let mut request = Request::builder().uri(&path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let v: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    common::check_response("GET", &path, 200, &v);
    assert_eq!(
        v["embed"],
        serde_json::json!({
            "provider": "spotify",
            "kind": "audio",
            "src": "https://open.spotify.com/embed/track/69kOkLUCkxIZYexIgSG8rq",
            "height": 152,
            "aspect": null,
            "allow": ["accelerometer", "autoplay", "clipboard-write", "encrypted-media", "gyroscope", "picture-in-picture", "web-share"],
            "autoplay_param": null
        })
    );
    let src = |site: &str| {
        v["also_on"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["site"] == site)
            .map(|e| e["embed"]["src"].clone())
            .unwrap_or_else(|| panic!("{site}: {v}"))
    };
    assert_eq!(src("TIDAL"), "https://embed.tidal.com/tracks/20115564");
    assert_eq!(
        src("Deezer"),
        "https://widget.deezer.com/widget/dark/track/67238735"
    );
    assert_eq!(src("Discogs"), Value::Null);
}
