//! A show's latest episodes through the store and the route (`episodes`): the save writes a
//! show's list and keeps or clears it as later saves decide; `GET /v1/resources/{id}/episodes`
//! lists it, names the episodes Pinakes has as resources, and falls back to a group member's.
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    episodes::Episode,
    model::{ResourceMeta, Stream},
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
mod common;

fn episode(n: u32, stream: bool) -> Episode {
    Episode {
        id: None,
        uri: format!("https://show.example/episode/{n}"),
        name: Some(format!("Episode {n}")),
        image: Some(format!("https://show.example/art/{n}.jpg")),
        published: chrono::DateTime::from_timestamp(1_760_000_000 + i64::from(n) * 86_400, 0),
        guid: format!("ep-{n}"),
        stream: stream.then(|| Stream {
            url: format!("https://cdn.example/{n}.mp3"),
            media_type: "audio/mpeg".into(),
            duration_s: Some(3_000 + n),
            preview: false,
        }),
    }
}
async fn show(pool: &PgPool, uri: &str, episodes: Option<Vec<Episode>>) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 86_400);
    d.set_type("PodcastSeries");
    d.name = Some("Show".into());
    d.episodes = episodes;
    store::save_document(pool, &id, 0, &d).await.unwrap();
    id
}
async fn request(app: &axum::Router, path: &str) -> (u16, Value) {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status == 200 {
        common::check_response("GET", path, status, &value);
    }
    (status, value)
}
async fn rows(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM podcast_episode WHERE resource_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_shows_latest_episodes_are_listed_newest_first_and_name_the_ones_pinakes_has(
    pool: PgPool,
) {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let id = show(
        &pool,
        "https://feeds.example.org/show.xml",
        Some(vec![episode(3, true), episode(2, false), episode(1, true)]),
    )
    .await;
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(status, 200, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["name"], "Episode 3");
    assert_eq!(items[0]["uri"], "https://show.example/episode/3");
    assert_eq!(items[0]["guid"], "ep-3");
    assert_eq!(items[0]["image"], "https://show.example/art/3.jpg");
    assert!(
        items[0]["published"]
            .as_str()
            .unwrap()
            .starts_with("2025-10-"),
        "{}",
        items[0]
    );
    assert_eq!(
        items[0]["stream"],
        serde_json::json!({"url":"https://cdn.example/3.mp3","type":"audio/mpeg","duration_s":3003,"preview":false})
    );
    assert_eq!(
        items[1]["stream"],
        Value::Null,
        "an item without a public enclosure is listed without a stream"
    );
    assert!(
        items.iter().all(|i| i.get("id").is_none()),
        "no episode is a resource yet: {page}"
    );
    // One episode's page becomes a resource: the list names its card.
    let known = store::ensure_resource(&pool, "https://show.example/episode/2")
        .await
        .unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(page["items"][1]["id"], known);
    assert!(page["items"][0].get("id").is_none());
    // The limit, and its bounds.
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?limit=1")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["name"], "Episode 3");
    for bad in ["limit=0", "limit=21", "limit=x"] {
        assert_eq!(
            request(&app, &format!("/v1/resources/{id}/episodes?{bad}"))
                .await
                .0,
            400,
            "{bad}"
        );
    }
    // Unknown, invalid, and a known resource that is no show.
    assert_eq!(
        request(
            &app,
            "/v1/resources/00000000000000000000000000000000/episodes"
        )
        .await
        .0,
        404
    );
    assert_eq!(
        request(&app, "/v1/resources/not-an-id/episodes").await.0,
        400
    );
    let page_id = store::ensure_resource(&pool, "https://example.org/page")
        .await
        .unwrap();
    let mut d = ResourceMeta::new("https://example.org/page", "web", 86_400);
    d.name = Some("A page".into());
    store::save_document(&pool, &page_id, 0, &d).await.unwrap();
    let (status, page) = request(&app, &format!("/v1/resources/{page_id}/episodes")).await;
    assert_eq!((status, page["items"].as_array().unwrap().len()), (200, 0));
}

/// A save with a list replaces the show's rows whole; a show's save without one (a record read
/// without its feed) keeps them; a save that makes the resource no show clears them; at most
/// twenty are kept.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_save_replaces_keeps_or_clears_the_list(pool: PgPool) {
    let uri = "https://feeds.example.org/show.xml";
    let id = show(
        &pool,
        uri,
        Some((1..=25).map(|n| episode(n, true)).collect()),
    )
    .await;
    assert_eq!(rows(&pool, &id).await, 20);
    let first: String =
        sqlx::query_scalar("SELECT guid FROM podcast_episode WHERE resource_id=$1 AND position=0")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        first, "ep-1",
        "the list is stored as given: the parser orders it"
    );
    // A record read without its feed: the stored list stays.
    let mut d = ResourceMeta::new(uri, "web", 86_400);
    d.set_type("PodcastSeries");
    d.name = Some("Show, renamed".into());
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(rows(&pool, &id).await, 20);
    // A shorter list replaces it whole.
    d.episodes = Some(vec![episode(30, true), episode(29, true)]);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(rows(&pool, &id).await, 2);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(page["items"][0]["guid"], "ep-30");
    // No longer a show: nothing to list.
    let mut d = ResourceMeta::new(uri, "web", 86_400);
    d.set_type("DataFeed");
    d.name = Some("A feed".into());
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(rows(&pool, &id).await, 0);
    // An empty list, stored as such.
    let mut d = ResourceMeta::new(uri, "web", 86_400);
    d.set_type("PodcastSeries");
    d.episodes = Some(vec![]);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(rows(&pool, &id).await, 0);
}

/// A show card without a list of its own (an Apple Podcasts page whose feed could not be read)
/// lists the episodes of a member of its group that has them: the same show elsewhere.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_show_without_a_list_lists_a_group_members(pool: PgPool) {
    let feed = show(
        &pool,
        "https://feeds.example.org/show.xml",
        Some(vec![episode(2, true), episode(1, true)]),
    )
    .await;
    let apple = show(
        &pool,
        "https://podcasts.apple.com/us/podcast/show/id123",
        None,
    )
    .await;
    let other = show(&pool, "https://pca.st/itunes/123", None).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (_, page) = request(&app, &format!("/v1/resources/{apple}/episodes")).await;
    assert_eq!(
        page["items"].as_array().unwrap().len(),
        0,
        "not grouped yet: {page}"
    );
    // The feed and the Apple page are one show (a group as the grouping pass would leave it).
    let group = "0123456789abcdef0123456789abcdef";
    sqlx::query("INSERT INTO \"group\"(id) VALUES($1)")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id=ANY($2)")
        .bind(group)
        .bind(vec![feed.clone(), apple.clone()])
        .execute(&pool)
        .await
        .unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{apple}/episodes")).await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{page}");
    assert_eq!(items[0]["guid"], "ep-2");
    // A show outside the group has nothing.
    let (_, page) = request(&app, &format!("/v1/resources/{other}/episodes")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 0);
    // The feed's own list is its own, whatever the group.
    let (_, page) = request(&app, &format!("/v1/resources/{feed}/episodes")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
}
