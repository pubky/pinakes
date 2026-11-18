//! A show's latest episodes through the store and the route (`episodes`): the save writes a
//! show's list and keeps or clears it as later saves decide; `GET /v1/resources/{id}/episodes`
//! lists it in pages, matches names, names the episodes Pinakes has as resources, and falls back
//! to a group member's; a show's card carries the first of them that have audio
//! (`Card::episodes`). An entry's feed item reads as its episode, which names its show, and the
//! episode's page on the feed's own site answers as the episode (`episodes::join_page`).
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
        aliases: Vec::new(),
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
    common::get_from(app, "127.0.0.2:1234", path).await
}
async fn rows(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM podcast_episode WHERE resource_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A show's episodes are stored without a link that carries a credential: an episode whose page
/// does is not kept, and an image or enclosure that does goes from its row.
#[sqlx::test(migrations = "../../migrations")]
async fn a_shows_episodes_store_no_credential(pool: PgPool) {
    let mut leaky = episode(2, true);
    leaky.image = Some("https://img.example/2.png?token=SECRET".into());
    leaky.stream.as_mut().unwrap().url = "https://cdn.example/2.mp3?key=SECRET".into();
    let mut page = episode(1, true);
    page.uri = "https://feeds.example.org/ep-1?auth=SECRET".into();
    let id = show(
        &pool,
        "https://feeds.example.org/guarded.xml",
        Some(vec![episode(3, true), leaky, page]),
    )
    .await;
    let stored: String = sqlx::query_scalar("SELECT coalesce(string_agg(concat_ws(' ',guid,url,name,image,stream::text),' '),'') FROM podcast_episode WHERE resource_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!stored.contains("SECRET"), "{stored}");
    assert_eq!(rows(&pool, &id).await, 2);
}

#[sqlx::test(migrations = "../../migrations")]
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
/// a thousand are kept.
#[sqlx::test(migrations = "../../migrations")]
async fn the_save_replaces_keeps_or_clears_the_list(pool: PgPool) {
    let uri = "https://feeds.example.org/show.xml";
    let id = show(
        &pool,
        uri,
        Some((1..=1030).map(|n| episode(n, true)).collect()),
    )
    .await;
    assert_eq!(rows(&pool, &id).await, 1000);
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
    assert_eq!(rows(&pool, &id).await, 1000);
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

/// A feed as a show's read fetches it.
fn fetched(body: String) -> pinakes_core::resolver::egress::Fetched {
    fetched_at("https://feeds.example.org/show.xml", body)
}
/// A feed answering at `url`.
fn fetched_at(url: &str, body: String) -> pinakes_core::resolver::egress::Fetched {
    pinakes_core::resolver::egress::Fetched {
        retry_after_secs: 0,
        ..common::fetched(
            url.parse().unwrap(),
            200,
            "application/rss+xml".into(),
            body.into_bytes(),
        )
    }
}
fn feed(items: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Show</title>{items}</channel></rss>"#
    )
}

/// From the feed to the route: an item whose only name is a signed enclosure is listed, without
/// a stream, and the whole response shows nothing of the link, not the token.
#[sqlx::test(migrations = "../../migrations")]
async fn the_route_shows_nothing_of_a_rejected_enclosure(pool: PgPool) {
    let body = feed(concat!(
        r#"<item><title>Hidden</title><enclosure url="https://cdn.example/ep.mp3?token=SECRET" type="audio/mpeg"/></item>"#,
        r#"<item><title>Open</title><guid>ep-2</guid><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg"/></item>"#,
    ));
    let episodes = pinakes_core::episodes::from_feed(&fetched(body)).unwrap();
    assert_eq!(episodes.len(), 2);
    let id = show(&pool, "https://feeds.example.org/show.xml", Some(episodes)).await;
    assert_eq!(rows(&pool, &id).await, 2);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(status, 200, "{page}");
    let text = page.to_string();
    assert!(!text.contains("SECRET"), "{text}");
    assert!(!text.contains("ep.mp3"), "{text}");
    let hidden = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "Hidden")
        .unwrap();
    assert_eq!(hidden["stream"], Value::Null);
    assert!(
        hidden["guid"].as_str().unwrap().starts_with("sha256:"),
        "{hidden}"
    );
    assert!(
        hidden["uri"]
            .as_str()
            .unwrap()
            .starts_with("https://feeds.example.org/show.xml?guid=sha256%3A"),
        "{hidden}"
    );
}

/// An enclosure with an absurd media type (`audio/` and 5,000 characters) is no stream, and the
/// show saves with the item listed: the stored stream is bounded by `Stream::public`, which the
/// table's own bound (4,096 bytes) would otherwise fail the whole save on.
#[sqlx::test(migrations = "../../migrations")]
async fn an_oversized_media_type_is_no_stream_and_the_show_saves(pool: PgPool) {
    let body = feed(&format!(
        r#"<item><title>Long</title><guid>ep-1</guid><enclosure url="https://cdn.example/1.mp3" type="audio/{}"/></item><item><title>Short</title><guid>ep-2</guid><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg"/></item>"#,
        "x".repeat(5000)
    ));
    let episodes = pinakes_core::episodes::from_feed(&fetched(body)).unwrap();
    assert_eq!(episodes.len(), 2);
    assert_eq!(episodes[0].stream, None, "{:?}", episodes[0]);
    assert!(episodes[1].stream.is_some());
    let id = show(&pool, "https://feeds.example.org/show.xml", Some(episodes)).await;
    assert_eq!(rows(&pool, &id).await, 2);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"][0]["stream"], Value::Null);
    assert_eq!(page["items"][1]["stream"]["type"], "audio/mpeg");
}

/// A valid stream whose JSON would not fit the table (a 2,042-byte URL of backslashes, which
/// JSON escapes to twice that) is stored as none; the episode is listed and the show saves.
#[sqlx::test(migrations = "../../migrations")]
async fn a_stream_too_long_for_the_table_is_none_and_the_show_saves(pool: PgPool) {
    let long = format!("https://cdn.example/a?{}", "\\".repeat(2020));
    assert_eq!(long.len(), 2042);
    let body = feed(&format!(
        r#"<item><title>Long</title><guid>ep-1</guid><enclosure url="{long}" type="audio/mpeg"/></item><item><title>Short</title><guid>ep-2</guid><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg"/></item>"#
    ));
    let episodes = pinakes_core::episodes::from_feed(&fetched(body)).unwrap();
    assert_eq!(episodes.len(), 2);
    let stream = episodes[0]
        .stream
        .as_ref()
        .expect("a public stream of 2,042 bytes");
    assert_eq!(stream.url, long, "the URL as given");
    assert!(
        serde_json::to_string(stream).unwrap().len() > 4096,
        "its JSON would not fit the table"
    );
    let id = show(&pool, "https://feeds.example.org/show.xml", Some(episodes)).await;
    assert_eq!(
        rows(&pool, &id).await,
        2,
        "the show saved with both episodes"
    );
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"][0]["guid"], "ep-1");
    assert_eq!(page["items"][0]["stream"], Value::Null);
    assert_eq!(
        page["items"][1]["stream"]["url"],
        "https://cdn.example/2.mp3"
    );
}

/// A card cleared without a save (a page too complex to parse, an error card in its place) lists
/// nothing: the rows follow the document, whatever writes it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_cleared_card_lists_nothing(pool: PgPool) {
    let id = show(
        &pool,
        "https://feeds.example.org/show.xml",
        Some(vec![episode(2, true), episode(1, true)]),
    )
    .await;
    assert_eq!(rows(&pool, &id).await, 2);
    pinakes_core::changes::clear(&pool, &id, 0, "too_complex")
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "error", "the card was cleared");
    assert_eq!(rows(&pool, &id).await, 0);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 0, "{page}");
}

/// An OpenStreetMap place has no `type_path` on its resource row (its card lives in `osm_place`):
/// it is no show, and the route says so with an empty list, not an error.
#[sqlx::test(migrations = "../../migrations")]
async fn a_place_without_a_type_path_lists_nothing(pool: PgPool) {
    let uri = "https://www.openstreetmap.org/node/123";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    // As `store::write_document` leaves a place's row.
    sqlx::query("UPDATE resource SET type_path=NULL,osm_type='node',osm_id=123 WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let path: Option<Vec<String>> =
        sqlx::query_scalar("SELECT type_path FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(path, None);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 0, "{page}");
}

/// A show card without a list of its own (an Apple Podcasts page whose feed could not be read)
/// lists the episodes of a member of its group that has them: the same show elsewhere.
#[sqlx::test(migrations = "../../migrations")]
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
    // A member that is no show (the show's website, grouped with it) lists nothing of the feed's.
    let site = store::ensure_resource(&pool, "https://show.example/")
        .await
        .unwrap();
    let mut d = ResourceMeta::new("https://show.example/", "web", 86_400);
    d.set_type("WebSite");
    store::save_document(&pool, &site, 0, &d).await.unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id=$2")
        .bind(group)
        .bind(&site)
        .execute(&pool)
        .await
        .unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{site}/episodes")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 0, "{page}");
    // The Apple page cleared (a page too complex to parse): no longer a show, it lists nothing,
    // grouped as it still is.
    pinakes_core::changes::clear(&pool, &apple, 0, "too_complex")
        .await
        .unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{apple}/episodes")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 0, "{page}");
    // Read again as the show it is (and grouped again, as the grouping pass would leave it), it
    // lists the feed's once more.
    let mut d = ResourceMeta::new(
        "https://podcasts.apple.com/us/podcast/show/id123",
        "web",
        86_400,
    );
    d.set_type("PodcastSeries");
    store::save_document(&pool, &apple, 0, &d).await.unwrap();
    // The save dissolved the group it was in; the grouping pass would form it again.
    sqlx::query("INSERT INTO \"group\"(id) VALUES($1) ON CONFLICT DO NOTHING")
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
    assert_eq!(page["items"].as_array().unwrap().len(), 2, "{page}");
}

async fn lookup(app: &axum::Router, body: Value) -> Value {
    let path = "/v1/resources/lookup";
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
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, 200, "{value}");
    common::check_response("POST", path, status, &value);
    value
}
/// The episodes of the synthetic feed `fixtures/episodes/latest-feed.xml`, as a show's read
/// parses them.
fn fixture_episodes() -> Vec<Episode> {
    let body = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/episodes/latest-feed.xml"
    ))
    .unwrap();
    pinakes_core::episodes::from_feed(&fetched(body)).unwrap()
}

/// A show's card carries its feed's latest episodes that have audio (`episodes`), from a fixture
/// feed: newest first whatever the feed's order, a video enclosure as well as audio, a missing
/// `itunes:duration` as a null length, an item without an enclosure or with a document one left
/// out, at most five; the same entries the route lists, on a resource read, a read by URI and a
/// lookup. No other card has the field, nor a show without such an episode.
#[sqlx::test(migrations = "../../migrations")]
async fn a_shows_card_carries_its_latest_episodes_with_audio(pool: PgPool) {
    let episodes = fixture_episodes();
    let names: Vec<&str> = episodes
        .iter()
        .map(|e| e.name.as_deref().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "A note to listeners",
            "Episode 5 (video)",
            "Episode 4",
            "Episode 3",
            "Show notes (PDF)",
            "Episode 2",
            "Episode 1",
            "Episode 0"
        ],
        "every item a GUID names, newest first"
    );
    let uri = "https://feeds.example.org/show.xml";
    let id = show(&pool, uri, Some(episodes)).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, card) = request(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(status, 200, "{card}");
    let list = card["episodes"].as_array().expect("a show's episodes");
    let names: Vec<&str> = list.iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "Episode 5 (video)",
            "Episode 4",
            "Episode 3",
            "Episode 2",
            "Episode 1"
        ],
        "the latest five with audio or video, newest first: {card}"
    );
    assert_eq!(
        list[0]["uri"],
        "https://feeds.example.org/show.xml?guid=ep-5"
    );
    assert_eq!(list[0]["guid"], "ep-5");
    assert_eq!(list[0]["published"], "2026-11-05T08:00:00Z");
    assert_eq!(
        list[0]["stream"],
        serde_json::json!({"url":"https://cdn.example/5.mp4","type":"video/mp4","duration_s":720,"preview":false})
    );
    assert_eq!(
        list[1]["stream"],
        serde_json::json!({"url":"https://cdn.example/4.mp3","type":"audio/mpeg","duration_s":null,"preview":false}),
        "no itunes:duration: a null length"
    );
    assert_eq!(list[2]["stream"]["duration_s"], 3723);
    assert_eq!(
        list[3]["stream"],
        serde_json::json!({"url":"https://cdn.example/2.m4a","type":"audio/x-m4a","duration_s":3557,"preview":false})
    );
    assert!(list.iter().all(|e| e.get("id").is_none()), "{card}");
    // The route's entries with a stream, in its order.
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    let route: Vec<&Value> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| !e["stream"].is_null())
        .take(5)
        .collect();
    assert_eq!(list.iter().collect::<Vec<_>>(), route);
    // An episode Pinakes has as a resource is named by its card's ID, as on the route.
    let known = store::ensure_resource(&pool, "https://feeds.example.org/show.xml?guid=ep-4")
        .await
        .unwrap();
    let (_, card) = request(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(card["episodes"][1]["id"], known);
    assert!(card["episodes"][0].get("id").is_none());
    let (status, by_uri) = request(&app, &format!("/v1/resources/by-uri?uri={uri}")).await;
    assert_eq!(status, 200, "{by_uri}");
    assert_eq!(by_uri["episodes"], card["episodes"]);
    // A page, a show not read yet and a show whose items have no audio carry no field.
    let page_id = store::ensure_resource(&pool, "https://example.org/page")
        .await
        .unwrap();
    let mut d = ResourceMeta::new("https://example.org/page", "web", 86_400);
    d.name = Some("A page".into());
    store::save_document(&pool, &page_id, 0, &d).await.unwrap();
    let unread = show(&pool, "https://feeds.example.org/unread.xml", None).await;
    let silent = show(
        &pool,
        "https://feeds.example.org/silent.xml",
        Some(vec![episode(2, false), episode(1, false)]),
    )
    .await;
    let (_, silent_page) = request(&app, &format!("/v1/resources/{silent}/episodes")).await;
    assert_eq!(silent_page["items"].as_array().unwrap().len(), 2);
    let cards = lookup(
        &app,
        serde_json::json!({"ids": [id.clone(), page_id.clone(), unread.clone(), silent.clone()]}),
    )
    .await;
    let cards = cards.as_array().unwrap();
    assert_eq!(cards.len(), 4, "{cards:?}");
    for c in cards {
        if c["id"] == id {
            assert_eq!(c["episodes"], card["episodes"], "a lookup's card: {c}");
        } else {
            assert!(c.get("episodes").is_none(), "{c}");
        }
    }
}

/// A show's card without episodes of its own carries those of a member of its group that has
/// them (an Apple Podcasts or Spotify show grouped with its feed), as the route lists them; a
/// show outside the group, a member whose own list has no audio, and a member cleared after an
/// error, carry none.
#[sqlx::test(migrations = "../../migrations")]
async fn a_show_card_without_episodes_carries_its_groups(pool: PgPool) {
    let feed = show(
        &pool,
        "https://feeds.example.org/show.xml",
        Some(fixture_episodes()),
    )
    .await;
    let apple = show(
        &pool,
        "https://podcasts.apple.com/us/podcast/show/id123",
        None,
    )
    .await;
    let spotify = show(&pool, "https://open.spotify.com/show/abc123", None).await;
    let other = show(&pool, "https://pca.st/itunes/123", None).await;
    let group = "0123456789abcdef0123456789abcdef";
    sqlx::query("INSERT INTO \"group\"(id) VALUES($1)")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id=ANY($2)")
        .bind(group)
        .bind(vec![feed.clone(), apple.clone(), spotify.clone()])
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (_, own) = request(&app, &format!("/v1/resources/{feed}")).await;
    assert_eq!(own["episodes"].as_array().map(Vec::len), Some(5), "{own}");
    for member in [&apple, &spotify] {
        let (status, card) = request(&app, &format!("/v1/resources/{member}")).await;
        assert_eq!(status, 200, "{card}");
        assert_eq!(card["episodes"], own["episodes"], "{card}");
    }
    let cards = lookup(
        &app,
        serde_json::json!({"ids": [apple.clone(), other.clone()]}),
    )
    .await;
    for c in cards.as_array().unwrap() {
        if c["id"] == apple {
            assert_eq!(c["episodes"], own["episodes"], "{c}");
        } else {
            assert!(c.get("episodes").is_none(), "outside the group: {c}");
        }
    }
    // The group's read lists each member's card with the same episodes.
    let (status, read) = request(&app, &format!("/v1/groups/{group}")).await;
    assert_eq!(status, 200, "{read}");
    let text = read.to_string();
    assert!(text.contains("https://cdn.example/5.mp4"), "{text}");
    // A member with a list of its own, none of it playable, lists its own on the route and
    // carries nothing on its card: the card shows the route's list, never another member's.
    let silent = show(
        &pool,
        "https://feeds.example.org/silent.xml",
        Some(vec![episode(2, false), episode(1, false)]),
    )
    .await;
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id=$2")
        .bind(group)
        .bind(&silent)
        .execute(&pool)
        .await
        .unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{silent}/episodes")).await;
    assert_eq!(page["items"][0]["guid"], "ep-2", "{page}");
    let (_, card) = request(&app, &format!("/v1/resources/{silent}")).await;
    assert!(card.get("episodes").is_none(), "{card}");
    let (_, card) = request(&app, &format!("/v1/resources/{apple}")).await;
    assert_eq!(
        card["episodes"], own["episodes"],
        "still the feed's: {card}"
    );
    // Cleared after an error, the Apple card is no show: it carries nothing.
    pinakes_core::changes::clear(&pool, &apple, 0, "too_complex")
        .await
        .unwrap();
    let (_, card) = request(&app, &format!("/v1/resources/{apple}")).await;
    assert!(card.get("episodes").is_none(), "{card}");
}

/// The route pages through the whole stored list, newest first (`offset`, `next_offset`), and
/// with `q` lists only the episodes whose name holds it in any letter case, paged the same way;
/// the bounds are 400s, and the show's card still carries five.
#[sqlx::test(migrations = "../../migrations")]
async fn the_route_pages_through_the_list_and_matches_names(pool: PgPool) {
    let mut list: Vec<Episode> = (1..=45).rev().map(|n| episode(n, true)).collect();
    list[5].name = Some("#500 – Khabib Nurmagomedov: Fighting".into());
    list[30].name = Some("ÜBER khabib, again".into());
    let id = show(&pool, "https://feeds.example.org/show.xml", Some(list)).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let names = |page: &Value| -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap().to_owned())
            .collect()
    };
    // Every episode once, in order, twenty at a time.
    let (mut seen, mut offset) = (vec![], Some(0u64));
    while let Some(at) = offset {
        let (status, page) =
            request(&app, &format!("/v1/resources/{id}/episodes?offset={at}")).await;
        assert_eq!(status, 200, "{page}");
        assert!(names(&page).len() <= 20);
        seen.extend(names(&page));
        offset = page["next_offset"].as_u64();
        assert!(offset.is_none_or(|next| next == at + 20), "{page}");
    }
    assert_eq!(seen.len(), 45);
    assert_eq!(seen[0], "Episode 45");
    assert_eq!(seen[44], "Episode 1");
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=40")).await;
    assert_eq!(
        (names(&page).len(), &page["next_offset"]),
        (5, &Value::Null)
    );
    let (_, page) = request(
        &app,
        &format!("/v1/resources/{id}/episodes?offset=10&limit=3"),
    )
    .await;
    assert_eq!(names(&page), ["Episode 35", "Episode 34", "Episode 33"]);
    assert_eq!(page["next_offset"], 13);
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=1000")).await;
    assert_eq!((status, names(&page).len()), (200, 0));
    // The feed's whole list is here: nothing older to link to.
    assert_eq!(page["truncated"], false, "{page}");
    // A name match, in any letter case, Unicode's included, paged as the whole list is.
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?q=KHABIB")).await;
    assert_eq!(
        names(&page),
        ["#500 – Khabib Nurmagomedov: Fighting", "ÜBER khabib, again"]
    );
    assert_eq!(page["next_offset"], Value::Null);
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?q=%C3%BCber")).await;
    assert_eq!(names(&page), ["ÜBER khabib, again"]);
    let (_, page) = request(
        &app,
        &format!("/v1/resources/{id}/episodes?q=khabib&limit=1"),
    )
    .await;
    assert_eq!(names(&page), ["#500 – Khabib Nurmagomedov: Fighting"]);
    assert_eq!(page["next_offset"], 1);
    let (_, page) = request(
        &app,
        &format!("/v1/resources/{id}/episodes?q=khabib&offset=1&limit=1"),
    )
    .await;
    assert_eq!(names(&page), ["ÜBER khabib, again"]);
    assert_eq!(page["next_offset"], Value::Null);
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?q=no%20such")).await;
    assert_eq!(
        (names(&page).len(), &page["next_offset"]),
        (0, &Value::Null)
    );
    // Spaces around `q` are ignored; a blank one matches every episode.
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?q=%20khabib%20")).await;
    assert_eq!(names(&page).len(), 2);
    for blank in ["q=", "q=%20%20"] {
        let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?{blank}")).await;
        assert_eq!(names(&page).len(), 20, "{blank}");
        assert_eq!(page["next_offset"], 20, "{blank}");
    }
    let long = format!("q={}", "a".repeat(257));
    for bad in ["offset=1001", "offset=-1", "offset=x", long.as_str()] {
        assert_eq!(
            request(&app, &format!("/v1/resources/{id}/episodes?{bad}"))
                .await
                .0,
            400,
            "{bad}"
        );
    }
    let (status, _) = request(
        &app,
        &format!("/v1/resources/{id}/episodes?q={}", "a".repeat(256)),
    )
    .await;
    assert_eq!(status, 200);
    // The card's list stays the first five.
    let (_, card) = request(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(card["episodes"].as_array().unwrap().len(), 5);
}

/// A show keeps the latest 1000 episodes of its feed (`episodes::KEPT`): a longer list is
/// stored to its 1000th, and the route pages through all of them (offsets to 1000) and says on
/// every page, whatever `q`, that the feed lists older ones (`truncated`); a group member that
/// borrows the list says the same. A list shorter than that is the feed's whole list.
#[sqlx::test(migrations = "../../migrations")]
async fn a_show_keeps_a_thousand_episodes_and_says_when_its_feed_has_more(pool: PgPool) {
    let list: Vec<Episode> = (1..=1001).rev().map(|n| episode(n, n % 2 == 0)).collect();
    let id = show(&pool, "https://feeds.example.org/long.xml", Some(list)).await;
    assert_eq!(rows(&pool, &id).await, 1000);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let names = |page: &Value| -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(names(&page)[0], "Episode 1001");
    assert_eq!(
        (&page["next_offset"], &page["truncated"]),
        (&Value::from(20), &Value::Bool(true))
    );
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=160")).await;
    assert_eq!(names(&page).len(), 20);
    assert_eq!(names(&page)[0], "Episode 841");
    assert_eq!(page["next_offset"], 180);
    assert_eq!(page["truncated"], true);
    // The last page ends at the 1000th: Episode 2; Episode 1 was not kept.
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=980")).await;
    assert_eq!(names(&page).len(), 20);
    assert_eq!(names(&page)[19], "Episode 2");
    assert_eq!(
        (&page["next_offset"], &page["truncated"]),
        (&Value::Null, &Value::Bool(true))
    );
    let (_, page) = request(
        &app,
        &format!("/v1/resources/{id}/episodes?offset=995&limit=3"),
    )
    .await;
    assert_eq!(names(&page), ["Episode 6", "Episode 5", "Episode 4"]);
    assert_eq!(page["next_offset"], 998);
    let (status, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=1000")).await;
    assert_eq!((status, names(&page).len()), (200, 0));
    assert_eq!(page["truncated"], true, "{page}");
    // A name match over the whole list: the oldest kept one matches too.
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?q=episode%202")).await;
    assert_eq!(names(&page)[0], "Episode 299");
    assert_eq!(page["truncated"], true);
    let (_, page) = request(
        &app,
        &format!("/v1/resources/{id}/episodes?q=episode%202&offset=100"),
    )
    .await;
    assert_eq!(names(&page).len(), 11);
    assert_eq!(names(&page)[10], "Episode 2");
    assert_eq!(page["next_offset"], Value::Null);
    // The card still carries the first five that have audio.
    let (_, card) = request(&app, &format!("/v1/resources/{id}")).await;
    let carried: Vec<&str> = card["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        carried,
        [
            "Episode 1000",
            "Episode 998",
            "Episode 996",
            "Episode 994",
            "Episode 992"
        ]
    );
    // A show of the same group without a list of its own borrows the list, and says so too.
    let apple = show(
        &pool,
        "https://podcasts.apple.com/us/podcast/long/id1",
        None,
    )
    .await;
    let group = "0123456789abcdef0123456789abcde1";
    sqlx::query("INSERT INTO \"group\"(id) VALUES($1)")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id=ANY($2)")
        .bind(group)
        .bind(vec![id.clone(), apple.clone()])
        .execute(&pool)
        .await
        .unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{apple}/episodes?offset=980")).await;
    assert_eq!(names(&page)[19], "Episode 2", "{page}");
    assert_eq!(page["truncated"], true);
    // Read again with fewer, the list is the feed's whole list.
    let list: Vec<Episode> = (1..=183).rev().map(|n| episode(n, true)).collect();
    show(&pool, "https://feeds.example.org/long.xml", Some(list)).await;
    assert_eq!(rows(&pool, &id).await, 183);
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=180")).await;
    assert_eq!(names(&page), ["Episode 3", "Episode 2", "Episode 1"]);
    assert_eq!(
        (&page["next_offset"], &page["truncated"]),
        (&Value::Null, &Value::Bool(false))
    );
    // Exactly as many as are kept: nothing older.
    let list: Vec<Episode> = (1..=1000).rev().map(|n| episode(n, true)).collect();
    show(&pool, "https://feeds.example.org/long.xml", Some(list)).await;
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(page["truncated"], false, "{page}");
    // A read that ended before the feed did (cut at its cap) says so, however short its list.
    let mut d = ResourceMeta::new("https://feeds.example.org/long.xml", "web", 86_400);
    d.set_type("PodcastSeries");
    d.episodes = Some((1..=179).rev().map(|n| episode(n, true)).collect());
    d.episodes_older = true;
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=160")).await;
    assert_eq!(
        (names(&page).len(), &page["truncated"]),
        (19, &Value::Bool(true))
    );
    // A save without a list keeps the list and what it says; a save that is no show clears both.
    let listed = |pool: PgPool, id: String| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM podcast_episode_list WHERE resource_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let mut d = ResourceMeta::new("https://feeds.example.org/long.xml", "web", 86_400);
    d.set_type("PodcastSeries");
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(page["truncated"], true);
    assert_eq!(listed(pool.clone(), id.clone()).await, 1);
    d.set_type("DataFeed");
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(rows(&pool, &id).await, 0);
    assert_eq!(listed(pool.clone(), id.clone()).await, 0);
}

/// A show whose feed is on its own site, WordPress-style (lexfridman.com): each item links a blog
/// post, shared with `utm_*` parameters, whose markup reads as an `Article` and names no feed.
const BLOG_FEED: &str = "https://blog.example/feed/podcast/";
fn blog_feed() -> String {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Blog Show</title>{}</channel></rss>"#,
        concat!(
            r#"<item><title>#2 – Second</title><guid isPermaLink="false">https://blog.example/?p=2</guid><link>https://blog.example/second/?utm_source=rss&amp;utm_medium=rss&amp;utm_campaign=second</link><pubDate>Tue, 10 Nov 2026 10:00:00 +0000</pubDate><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg"/></item>"#,
            r#"<item><title>#1 – First</title><guid isPermaLink="false">https://blog.example/?p=1</guid><link>https://blog.example/first/?utm_source=rss</link><pubDate>Mon, 09 Nov 2026 10:00:00 +0000</pubDate><enclosure url="https://cdn.example/1.mp3" type="audio/mpeg"/></item>"#,
            r#"<item><title>#0 – Elsewhere</title><guid isPermaLink="false">ep-0</guid><link>https://elsewhere.example/zero/</link><pubDate>Sun, 08 Nov 2026 10:00:00 +0000</pubDate><enclosure url="https://cdn.example/0.mp3" type="audio/mpeg"/></item>"#,
        )
    )
}
/// The blog feed answering at whatever address a job asks for (its item forms), read as the
/// web route reads a feed (`resolver::cards::podcast`), on the route the job's row names.
struct BlogFeed(&'static str, String);
#[async_trait::async_trait]
impl pinakes_core::resolver::Resolver for BlogFeed {
    fn name(&self) -> &'static str {
        self.0
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        let f = fetched_at(uri, self.1.clone());
        Ok(pinakes_core::resolver::cards::podcast(uri, &f, 3600, 1000)?.expect("a podcast feed"))
    }
    fn refresh_after(&self, _: &ResourceMeta) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }
}
/// Reads `uri` through the queue (its job alone due), as a client's `POST /v1/resolve` gets it
/// read: its resource ID.
async fn read(pool: &PgPool, uri: &str) -> String {
    read_with(pool, uri, blog_feed()).await
}
/// [`read`], the blog feed answering with `body`.
async fn read_with(pool: &PgPool, uri: &str, body: String) -> String {
    use pinakes_core::{config::QueueConfig, queue};
    let id = store::ensure_resource(pool, uri).await.unwrap();
    sqlx::query("UPDATE resource SET next_attempt_at=CASE WHEN id=$1 THEN now() ELSE now()+interval '1 day' END,final=false,dead_letter=false,lease_until=NULL")
        .bind(&id)
        .execute(pool)
        .await
        .unwrap();
    let route: Option<String> = sqlx::query_scalar("SELECT resolver FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(pool)
        .await
        .unwrap();
    let route: &'static str = Box::leak(route.expect("a route").into_boxed_str());
    let job = queue::claim(pool, route, &QueueConfig::default())
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{uri} due on {route}"));
    assert_eq!(job.id, id);
    queue::run_job(pool, &BlogFeed(route, body), &QueueConfig::default(), &job)
        .await
        .unwrap();
    id
}
fn by_uri(uri: &str) -> String {
    format!(
        "/v1/resources/by-uri?uri={}",
        url::form_urlencoded::byte_serialize(uri.as_bytes()).collect::<String>()
    )
}

/// Every entry of a WordPress-style show names its feed item, which reads as its
/// `PodcastEpisode` card naming its show; read, it is named by `id` on the route and the show's
/// card, and the post it links (on the feed's site) answers as the episode, with its tracked
/// spelling: one card, not an article beside an episode. A post not read yet is not added, and
/// joins at the item's next read after its own; a link on another site stays its own; a refresh
/// and a reextract keep the join.
#[sqlx::test(migrations = "../../migrations")]
async fn an_episodes_post_on_its_feeds_site_answers_as_the_episode(pool: PgPool) {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let episodes = pinakes_core::episodes::from_feed(&fetched_at(BLOG_FEED, blog_feed())).unwrap();
    let show_id = show(&pool, BLOG_FEED, Some(episodes)).await;
    // The second episode's post, already known as the article its markup makes of it, and its
    // tracked spelling (an alias of it).
    let post = "https://blog.example/second/";
    let tracked = "https://blog.example/second/?utm_source=rss&utm_medium=rss&utm_campaign=second";
    let post_id = store::ensure_resource(&pool, post).await.unwrap();
    let mut d = ResourceMeta::new(post, "web", 86_400);
    d.set_type("Article");
    d.name = Some("Second, the post".into());
    store::save_document(&pool, &post_id, 0, &d).await.unwrap();
    store::ensure_resource(&pool, tracked).await.unwrap();
    let (_, card) = request(&app, &by_uri(tracked)).await;
    assert_eq!(
        (&card["id"], &card["type"]),
        (&Value::from(post_id.clone()), &Value::from("Article"))
    );
    // Every entry names its feed item, none its post; nothing read yet, nothing named.
    let (_, page) = request(&app, &format!("/v1/resources/{show_id}/episodes")).await;
    let items = page["items"].as_array().unwrap().clone();
    let uris: Vec<&str> = items.iter().map(|i| i["uri"].as_str().unwrap()).collect();
    assert_eq!(
        uris,
        [
            "https://blog.example/feed/podcast/?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D2",
            "https://blog.example/feed/podcast/?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D1",
            "https://blog.example/feed/podcast/?guid=ep-0",
        ]
    );
    assert!(items.iter().all(|i| i.get("id").is_none()), "{page}");
    // The entry read: the episode's card, naming its show.
    let second = read(&pool, uris[0]).await;
    let (_, card) = request(&app, &format!("/v1/resources/{second}")).await;
    assert_eq!(card["type"], "PodcastEpisode", "{card}");
    assert_eq!(card["name"], "#2 – Second");
    assert_eq!(card["stream"]["url"], "https://cdn.example/2.mp3");
    assert_eq!(
        card["related"],
        serde_json::json!([{"rel": "show", "uri": BLOG_FEED, "name": "Blog Show"}])
    );
    assert!(
        card["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == post),
        "{card}"
    );
    // The route and the show's card name it.
    let (_, page) = request(&app, &format!("/v1/resources/{show_id}/episodes")).await;
    assert_eq!(page["items"][0]["id"], second);
    let (_, show_card) = request(&app, &format!("/v1/resources/{show_id}")).await;
    assert_eq!(show_card["episodes"][0]["id"], second);
    // Its post and the post's tracked spelling answer as the episode.
    for spelling in [post, tracked] {
        let (status, card) = request(&app, &by_uri(spelling)).await;
        assert_eq!(status, 200, "{spelling}: {card}");
        assert_eq!(card["id"], second, "{spelling}");
        assert_eq!(card["type"], "PodcastEpisode", "{spelling}");
    }
    let basis: String =
        sqlx::query_scalar("SELECT basis FROM resource_alias WHERE alias_id=$1 AND root_id=$2")
            .bind(&post_id)
            .bind(&second)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(basis, "source");
    // The first episode's post was not known: its address alone says nothing of what it is, so
    // nothing is added. Once read (as the article its markup makes of it), the item's next read
    // joins it.
    let first = read(&pool, uris[1]).await;
    assert_eq!(
        request(&app, &by_uri("https://blog.example/first/"))
            .await
            .0,
        404,
        "an unknown page is not added"
    );
    let first_post = store::ensure_resource(&pool, "https://blog.example/first/")
        .await
        .unwrap();
    let mut d = ResourceMeta::new("https://blog.example/first/", "web", 86_400);
    d.set_type("BlogPosting");
    store::save_document(&pool, &first_post, 0, &d)
        .await
        .unwrap();
    read(&pool, uris[1]).await;
    let (status, card) = request(&app, &by_uri("https://blog.example/first/")).await;
    assert_eq!(
        (status, &card["id"]),
        (200, &Value::from(first.clone())),
        "{card}"
    );
    // A link on another site than the feed's is not joined, nor added.
    let zero = read(&pool, uris[2]).await;
    let (_, card) = request(&app, &format!("/v1/resources/{zero}")).await;
    assert_eq!(card["type"], "PodcastEpisode");
    assert_eq!(
        request(&app, &by_uri("https://elsewhere.example/zero/"))
            .await
            .0,
        404
    );
    // Read again, and after a reextract, the join stands.
    read(&pool, uris[0]).await;
    pinakes_core::queue::reextract(&pool, &["all".into()])
        .await
        .unwrap();
    let (_, card) = request(&app, &by_uri(post)).await;
    assert_eq!(card["id"], second);
    let joined: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_alias WHERE basis='source'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(joined, 2);
}

/// A post that is already another card's alias is left as it is: the feed's claim never moves it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_post_already_an_alias_is_left_as_it_is(pool: PgPool) {
    let episodes = pinakes_core::episodes::from_feed(&fetched_at(BLOG_FEED, blog_feed())).unwrap();
    show(&pool, BLOG_FEED, Some(episodes)).await;
    let other = store::ensure_resource(&pool, "https://blog.example/2/")
        .await
        .unwrap();
    let post = "https://blog.example/second/";
    let post_id = store::ensure_resource(&pool, post).await.unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&post_id)
        .bind(&other)
        .execute(&pool)
        .await
        .unwrap();
    let episode = read(
        &pool,
        "https://blog.example/feed/podcast/?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D2",
    )
    .await;
    let root: String = sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
        .bind(&post_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(root, other);
    assert_ne!(root, episode);
}

/// A stale job (its row read again meanwhile) joins nothing; a known page grouped with another
/// link, or read as something else than a page or an article, is left as it is.
#[sqlx::test(migrations = "../../migrations")]
async fn a_stale_job_or_a_page_of_its_own_joins_nothing(pool: PgPool) {
    use pinakes_core::{config::QueueConfig, queue};
    let episodes = pinakes_core::episodes::from_feed(&fetched_at(BLOG_FEED, blog_feed())).unwrap();
    show(&pool, BLOG_FEED, Some(episodes)).await;
    // The post, read as an article.
    let post = "https://blog.example/second/";
    let post_id = store::ensure_resource(&pool, post).await.unwrap();
    let mut d = ResourceMeta::new(post, "web", 86_400);
    d.set_type("Article");
    store::save_document(&pool, &post_id, 0, &d).await.unwrap();
    let item = "https://blog.example/feed/podcast/?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D2";
    let id = store::ensure_resource(&pool, item).await.unwrap();
    let route: String = sqlx::query_scalar("SELECT resolver FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let route: &'static str = Box::leak(route.into_boxed_str());
    sqlx::query("UPDATE resource SET next_attempt_at=CASE WHEN id=$1 THEN now() ELSE now()+interval '1 day' END,final=false,dead_letter=false,lease_until=NULL")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let job = queue::claim(&pool, route, &QueueConfig::default())
        .await
        .unwrap()
        .expect("due");
    sqlx::query("UPDATE resource SET generation=generation+1 WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    // Fenced: whatever the run answers, it writes nothing.
    let _ = queue::run_job(
        &pool,
        &BlogFeed(route, blog_feed()),
        &QueueConfig::default(),
        &job,
    )
    .await;
    let aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_alias WHERE root_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(aliases, 0, "a stale job joins nothing");
    // The post read as a product (something else than a page): left as it is.
    let mut d = ResourceMeta::new(post, "web", 86_400);
    d.set_type("Product");
    d.name = Some("A product".into());
    store::save_document(&pool, &post_id, 0, &d).await.unwrap();
    read(&pool, item).await;
    let root: Option<String> =
        sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
            .bind(&post_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(root, None, "a product page stays its own");
    // Read as an article but grouped with another link: left as it is.
    let mut d = ResourceMeta::new(post, "web", 86_400);
    d.set_type("Article");
    store::save_document(&pool, &post_id, 0, &d).await.unwrap();
    let group = "0123456789abcdef0123456789abcdef";
    sqlx::query("INSERT INTO \"group\"(id) VALUES($1)")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id=$2")
        .bind(group)
        .bind(&post_id)
        .execute(&pool)
        .await
        .unwrap();
    let grouped: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM group_unit WHERE resource_id=$1 AND group_id IS NOT NULL",
    )
    .bind(&post_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grouped, 1);
    read(&pool, item).await;
    let root: Option<String> =
        sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
            .bind(&post_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(root, None, "a grouped page stays its own");
}

/// As the web route reads a feed item (`resolver::cards::enrich` on an RSS answer): read at the
/// address asked, the episode names its show and its page to join; where a redirect led
/// elsewhere, no page; an HTML answer is no feed.
#[tokio::test]
async fn the_web_routes_read_of_a_feed_item_names_its_page_only_where_it_was_asked() {
    use pinakes_core::model::Status;
    use pinakes_core::resolver::cards::{enrich, Alternates, CardConfig};
    let egress = common::offline_egress(common::unpaced_egress_config());
    let cfg = CardConfig::default();
    let item = "https://blog.example/feed/podcast/?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D2";
    let page_read = |uri: &str| {
        let mut d = ResourceMeta::new(uri, "web", 3600);
        d.status = Status::Ok;
        d
    };
    let f = fetched_at(item, blog_feed());
    let d = enrich(
        item,
        &f,
        page_read(item),
        Some(Alternates::default()),
        &egress,
        &cfg,
        true,
    )
    .await
    .unwrap();
    assert_eq!(d.type_name, "PodcastEpisode");
    assert_eq!(
        d.episode_page.as_deref(),
        Some("https://blog.example/second/")
    );
    assert_eq!(d.related.len(), 1);
    assert_eq!(d.related[0].uri.as_deref(), Some(BLOG_FEED));
    // Redirected to the feed at another address: still the episode, no page to join.
    let moved = fetched_at(
        "https://cdn.blog.example/feed.xml?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D2",
        blog_feed(),
    );
    let d = enrich(
        item,
        &moved,
        page_read(item),
        Some(Alternates::default()),
        &egress,
        &cfg,
        true,
    )
    .await
    .unwrap();
    assert_eq!(d.type_name, "PodcastEpisode");
    assert_eq!(d.episode_page, None);
    // An HTML answer is no feed: the page read stands.
    let mut html = fetched_at(item, "<html><head><title>x</title></head></html>".into());
    html.content_type = "text/html".into();
    let d = enrich(
        item,
        &html,
        page_read(item),
        Some(Alternates::default()),
        &egress,
        &cfg,
        true,
    )
    .await
    .unwrap();
    assert_ne!(d.type_name, "PodcastEpisode");
    assert_eq!(d.episode_page, None);
}

/// The join's other bounds: a feed on a subdomain joins a post on its parent domain; an item no
/// show's stored list names joins nothing (a site's search feed lists posts too); a post that is
/// the root of other aliases than its own spellings is left; a page read as an episode joins
/// only when it names the same feed.
#[sqlx::test(migrations = "../../migrations")]
async fn the_join_holds_to_its_bounds(pool: PgPool) {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    async fn article(pool: &PgPool, uri: &str, kind: &str) -> String {
        let id = store::ensure_resource(pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86_400);
        d.set_type(kind);
        store::save_document(pool, &id, 0, &d).await.unwrap();
        id
    }
    let second = "https://blog.example/second/";
    let item = |feed: &str| format!("{feed}?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D2");
    // Not in any stored list: nothing joins.
    let post = article(&pool, second, "Article").await;
    read(
        &pool,
        &item("https://blog.example/search/second/feed/rss2/"),
    )
    .await;
    let (_, card) = request(&app, &by_uri(second)).await;
    assert_eq!(card["id"], post, "an unlisted feed's item joins nothing");
    // A feed on a subdomain, listed: its post on the parent domain joins.
    let sub = "https://podcast.blog.example/feed/";
    let episodes = pinakes_core::episodes::from_feed(&fetched_at(sub, blog_feed())).unwrap();
    show(&pool, sub, Some(episodes)).await;
    let episode = read(&pool, &item(sub)).await;
    let (_, card) = request(&app, &by_uri(second)).await;
    assert_eq!(card["id"], episode, "{card}");
    // The blog feed's own list: the first post is the root of an alias of its own (an official
    // source's answer, say): left as it is.
    let episodes = pinakes_core::episodes::from_feed(&fetched_at(BLOG_FEED, blog_feed())).unwrap();
    show(&pool, BLOG_FEED, Some(episodes)).await;
    let first = article(&pool, "https://blog.example/first/", "Article").await;
    let elsewhere = store::ensure_resource(&pool, "https://mirror.example/first")
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'source')")
        .bind(&elsewhere)
        .bind(&first)
        .execute(&pool)
        .await
        .unwrap();
    let first_episode = read(
        &pool,
        "https://blog.example/feed/podcast/?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D1",
    )
    .await;
    let (_, card) = request(&app, &by_uri("https://blog.example/first/")).await;
    assert_eq!(card["id"], first, "a root of other aliases stays its own");
    assert_ne!(card["id"], first_episode);
    // A post that is the root of a canonical claim from another path (no spelling of its own)
    // does not join either: a release could not give it back.
    sqlx::query("DELETE FROM resource_alias WHERE alias_id=$1")
        .bind(&elsewhere)
        .execute(&pool)
        .await
        .unwrap();
    let canonical_of_first = |uri: &'static str| {
        let (pool, first) = (pool.clone(), first.clone());
        async move {
            let id = store::ensure_resource(&pool, uri).await.unwrap();
            sqlx::query(
                "INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')",
            )
            .bind(&id)
            .bind(&first)
            .execute(&pool)
            .await
            .unwrap();
            id
        }
    };
    let amp = canonical_of_first("https://blog.example/first/amp/").await;
    let first_item =
        "https://blog.example/feed/podcast/?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D1";
    read(&pool, first_item).await;
    let (_, card) = request(&app, &by_uri("https://blog.example/first/")).await;
    assert_eq!(
        card["id"], first,
        "a root of a canonical claim stays its own"
    );
    // Its own slash-less spelling instead: the same read joins the post, the spelling with it.
    sqlx::query("DELETE FROM resource_alias WHERE alias_id=$1")
        .bind(&amp)
        .execute(&pool)
        .await
        .unwrap();
    canonical_of_first("https://blog.example/first").await;
    read(&pool, first_item).await;
    let (_, card) = request(&app, &by_uri("https://blog.example/first")).await;
    assert_eq!(card["id"], first_episode, "{card}");
}

/// A page already read as an episode joins an item's card only when it names the same feed.
#[sqlx::test(migrations = "../../migrations")]
async fn an_episode_page_joins_only_its_own_feeds_item(pool: PgPool) {
    use pinakes_core::{config::QueueConfig, queue};
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let page = "https://blog.example/own-episode/";
    let own = "https://blog.example/own.xml";
    let body = format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Blog Show</title><item><title>Own</title><guid>own-1</guid><link>{page}</link><enclosure url="https://cdn.example/own.mp3" type="audio/mpeg"/></item></channel></rss>"#
    );
    let episodes = pinakes_core::episodes::from_feed(&fetched_at(own, body.clone())).unwrap();
    show(&pool, own, Some(episodes)).await;
    struct OwnFeed(&'static str, String);
    #[async_trait::async_trait]
    impl pinakes_core::resolver::Resolver for OwnFeed {
        fn name(&self) -> &'static str {
            self.0
        }
        fn matches(&self, _: &str) -> bool {
            true
        }
        async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
            let f = fetched_at(uri, self.1.clone());
            Ok(pinakes_core::resolver::cards::podcast(uri, &f, 3600, 1000)?.expect("a feed"))
        }
        fn refresh_after(&self, _: &ResourceMeta) -> std::time::Duration {
            std::time::Duration::from_secs(3600)
        }
    }
    let item = format!("{own}?guid=own-1");
    let item_id = store::ensure_resource(&pool, &item).await.unwrap();
    let run = || async {
        sqlx::query("UPDATE resource SET next_attempt_at=CASE WHEN id=$1 THEN now() ELSE now()+interval '1 day' END,final=false,dead_letter=false,lease_until=NULL")
            .bind(&item_id)
            .execute(&pool)
            .await
            .unwrap();
        // Its route as it is now: a read may move it.
        let route: String = sqlx::query_scalar("SELECT resolver FROM resource WHERE id=$1")
            .bind(&item_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let route: &'static str = Box::leak(route.into_boxed_str());
        let Some(job) = queue::claim(&pool, route, &QueueConfig::default())
            .await
            .unwrap()
        else {
            let row: String =
                sqlx::query_scalar("SELECT (to_jsonb(r)-'doc')::text FROM resource r WHERE id=$1")
                    .bind(&item_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            let derived: i64 =
                sqlx::query_scalar("SELECT count(*) FROM resource_derivation WHERE child_id=$1")
                    .bind(&item_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            panic!("not due on {route}: {row} derivations {derived}");
        };
        assert_eq!(job.id, item_id);
        queue::run_job(
            &pool,
            &OwnFeed(route, body.clone()),
            &QueueConfig::default(),
            &job,
        )
        .await
        .unwrap();
    };
    let page_id = store::ensure_resource(&pool, page).await.unwrap();
    let save = |feed: &'static str| {
        let (pool, page_id) = (pool.clone(), page_id.clone());
        async move {
            let mut d = ResourceMeta::new(page, "web", 86_400);
            d.set_type("PodcastEpisode");
            d.attributes.insert("podcast_feed".into(), feed.into());
            store::save_document(&pool, &page_id, 0, &d).await.unwrap();
        }
    };
    save("https://other.blog.example/feed.xml").await;
    run().await;
    let (_, card) = request(&app, &by_uri(page)).await;
    assert_eq!(
        card["id"], page_id,
        "an episode of another feed stays its own"
    );
    save(own).await;
    run().await;
    let (_, card) = request(&app, &by_uri(page)).await;
    assert_eq!(
        card["id"], item_id,
        "an episode of the same feed joins: {card}"
    );
}

/// A joined post lets go when its item no longer names it: the item's link moved to another post
/// (which joins instead), or the item left the feed (its card confirmed missing; one missing
/// answer moves nothing). A post let go answers for itself again, with its tracked spelling, and
/// is read again on its own route.
#[sqlx::test(migrations = "../../migrations")]
async fn a_joined_post_lets_go_when_its_item_no_longer_names_it(pool: PgPool) {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let episodes = pinakes_core::episodes::from_feed(&fetched_at(BLOG_FEED, blog_feed())).unwrap();
    show(&pool, BLOG_FEED, Some(episodes)).await;
    async fn article(pool: &PgPool, uri: &str) -> String {
        let id = store::ensure_resource(pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86_400);
        d.set_type("Article");
        store::save_document(pool, &id, 0, &d).await.unwrap();
        id
    }
    let post = "https://blog.example/second/";
    let tracked = "https://blog.example/second/?utm_source=rss&utm_medium=rss&utm_campaign=second";
    let post_id = article(&pool, post).await;
    let tracked_id = store::ensure_resource(&pool, tracked).await.unwrap();
    let item = "https://blog.example/feed/podcast/?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D2";
    let episode = read(&pool, item).await;
    for spelling in [post, tracked] {
        assert_eq!(
            request(&app, &by_uri(spelling)).await.1["id"],
            episode,
            "{spelling}"
        );
    }
    // Canonical spellings read since the join, both aliases of the episode now: the post's
    // slash-less address (it followed the post) and the item's own slash-less one.
    let alias_of_episode = |uri: &'static str| {
        let (pool, episode) = (pool.clone(), episode.clone());
        async move {
            let id = store::ensure_resource(&pool, uri).await.unwrap();
            sqlx::query(
                "INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical') ON CONFLICT(alias_id) DO UPDATE SET root_id=$2,basis='canonical'",
            )
            .bind(&id)
            .bind(&episode)
            .execute(&pool)
            .await
            .unwrap();
            id
        }
    };
    let slashless = alias_of_episode("https://blog.example/second").await;
    let item_slashless = alias_of_episode(
        "https://blog.example/feed/podcast?guid=https%3A%2F%2Fblog.example%2F%3Fp%3D2",
    )
    .await;
    // The item now links another post: the first lets go, with its tracked spelling; the other
    // joins.
    let moved_post = "https://blog.example/second-moved/";
    let moved_id = article(&pool, moved_post).await;
    let moved = blog_feed().replace(
        "https://blog.example/second/?utm_source=rss&amp;utm_medium=rss&amp;utm_campaign=second",
        moved_post,
    );
    assert_ne!(moved, blog_feed());
    read_with(&pool, item, moved.clone()).await;
    assert_eq!(request(&app, &by_uri(post)).await.1["id"], post_id);
    assert_eq!(request(&app, &by_uri(tracked)).await.1["id"], post_id);
    let tracked_root: String =
        sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
            .bind(&tracked_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tracked_root, post_id);
    // The post's slash-less spelling went back with it; the item's own stays on the episode.
    let root_of = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT root_id FROM resource_alias WHERE alias_id=$1")
                .bind(&id)
                .fetch_optional(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        root_of(slashless.clone()).await.as_deref(),
        Some(post_id.as_str())
    );
    assert_eq!(
        root_of(item_slashless.clone()).await.as_deref(),
        Some(episode.as_str())
    );
    let (resolver, due): (Option<String>, bool) =
        sqlx::query_as("SELECT resolver,next_attempt_at<=now() FROM resource WHERE id=$1")
            .bind(&post_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (resolver.as_deref(), due),
        (Some("web"), true),
        "read again on its own route"
    );
    assert_eq!(request(&app, &by_uri(moved_post)).await.1["id"], episode);
    // The link flips to its slash-less spelling, already a canonical alias of the episode: that
    // spelling is the page the item names now and stays; the post with the slash lets go.
    let slash_flip = moved.replace(moved_post, "https://blog.example/second-moved");
    assert_ne!(slash_flip, moved);
    let flipped = alias_of_episode("https://blog.example/second-moved").await;
    read_with(&pool, item, slash_flip.clone()).await;
    assert_eq!(
        root_of(flipped.clone()).await.as_deref(),
        Some(episode.as_str()),
        "the kept page stays"
    );
    assert_eq!(
        request(&app, &by_uri(moved_post)).await.1["id"],
        episode,
        "the same page in another spelling stays joined"
    );
    // Unlinked right after the flip: the post lets go, its slash-less spelling with it.
    let flip_unlinked = slash_flip.replace("<link>https://blog.example/second-moved</link>", "");
    assert_ne!(flip_unlinked, slash_flip);
    read_with(&pool, item, flip_unlinked).await;
    assert_eq!(request(&app, &by_uri(moved_post)).await.1["id"], moved_id);
    assert_eq!(root_of(flipped).await.as_deref(), Some(moved_id.as_str()));
    read_with(&pool, item, moved.clone()).await;
    assert_eq!(request(&app, &by_uri(moved_post)).await.1["id"], episode);
    // The item links no page of its own any more (its link removed): the post lets go; linked
    // again, it joins again.
    let unlinked = blog_feed().replace(
        "<link>https://blog.example/second/?utm_source=rss&amp;utm_medium=rss&amp;utm_campaign=second</link>",
        "",
    );
    assert_ne!(unlinked, blog_feed());
    read_with(&pool, item, unlinked).await;
    assert_eq!(request(&app, &by_uri(moved_post)).await.1["id"], moved_id);
    read_with(&pool, item, moved.clone()).await;
    assert_eq!(request(&app, &by_uri(moved_post)).await.1["id"], episode);
    // The item leaves the feed: its card reads as missing, and the post it had lets go.
    let start = blog_feed().find("<item><title>#2").unwrap();
    let end = blog_feed()[start..].find("</item>").unwrap() + start + "</item>".len();
    let mut gone = blog_feed();
    gone.replace_range(start..end, "");
    assert!(!gone.contains("#2 –"));
    // One missing answer is not yet a confirmed one: the post stays joined.
    read_with(&pool, item, gone.clone()).await;
    assert_eq!(request(&app, &by_uri(moved_post)).await.1["id"], episode);
    // Confirmed: the post lets go.
    read_with(&pool, item, gone).await;
    let status: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
        .bind(&episode)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "missing");
    assert_eq!(request(&app, &by_uri(moved_post)).await.1["id"], moved_id);
}

/// An entry's feed item that the publisher's own page names as its podcast item (a certain
/// `podcast:item:` link of the `podcasts` source) is that page's episode: the route and the show's
/// card name the page's card (`id`), its link as `uri` and the feed item plus the card's aliases
/// in `aliases`, so tags on the page show on the entry. Of two such pages, the card Pinakes knew
/// first wins and the other follows in `aliases`; a publisher's page read as an article is no
/// episode but its link follows in `aliases`. A directory's page naming the same item (Podcast
/// Index), a page on another site or a sibling host, a `maybe` link, an expired one, a switched-off
/// source's, a card that is an alias or not `ok`, and a card of another type are never named;
/// without a publisher's page the entry names the card its feed item reads as, as before.
#[sqlx::test(migrations = "../../migrations")]
async fn an_entry_names_the_publishers_episode_page_and_its_other_links(pool: PgPool) {
    let feed = "https://podcast.show.example.com/feed.xml";
    let item = |n: u32| format!("{feed}?guid=ep-{n}");
    // 10: an entry whose feed address carries a tracking parameter.
    let tracked_item = format!("{feed}?guid=ep-10&utm_source=x");
    let list: Vec<Episode> = (1..=10)
        .rev()
        .map(|n| Episode {
            uri: if n == 10 {
                tracked_item.clone()
            } else {
                item(n)
            },
            ..episode(n, true)
        })
        .collect();
    let show_id = show(&pool, feed, Some(list)).await;
    async fn card(pool: &PgPool, uri: &str, kind: &str) -> String {
        let id = store::ensure_resource(pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86_400);
        d.set_type(kind);
        d.name = Some(uri.into());
        store::save_document(pool, &id, 0, &d).await.unwrap();
        id
    }
    async fn names(
        pool: &PgPool,
        card: &str,
        link: &str,
        kind: Option<&str>,
        ttl: &str,
        seen: &str,
    ) {
        let link_id = pinakes_core::model::identity(link).unwrap().0;
        sqlx::query(&format!("INSERT INTO derived_link(resource_id,link_id,uri,site,source,via,kind,key,seen_at,expires_at) VALUES($1,$2,$3,'Podcast feed','podcasts','feed',$4,$5,now()-interval '{seen}',now()+interval '{ttl}')"))
            .bind(card)
            .bind(link_id)
            .bind(link)
            .bind(kind)
            .bind(format!("podcast:item:00000000-0000-5000-8000-000000000000:{}", "a".repeat(64)))
            .execute(pool)
            .await
            .unwrap();
    }
    // 6: the publisher's page (on the feed's registrable domain) names it, as does Podcast Index.
    let index = card(
        &pool,
        "https://podcastindex.org/podcast/1?episode=6",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &index, &item(6), None, "1 day", "0 s").await;
    let page = card(
        &pool,
        "https://show.example.com/episode/6/",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &page, &item(6), None, "1 day", "1 hour").await;
    let tracked = store::ensure_resource(&pool, "https://show.example.com/episode/6/?utm_source=x")
        .await
        .unwrap();
    // Its tracked spelling is its alias already (a rule of its URI names the page).
    let root: String = sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
        .bind(&tracked)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(root, page);
    // Its feed item was read too, as a card of its own, with a spelling of its own.
    let item_card = card(&pool, &item(6), "PodcastEpisode").await;
    let item_spelling = store::ensure_resource(&pool, "https://show.example.com/p/6")
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'source')")
        .bind(&item_spelling)
        .bind(&item_card)
        .execute(&pool)
        .await
        .unwrap();
    // 5: only a directory's page, a page on another site, a sibling host and a `maybe` link.
    let other = card(
        &pool,
        "https://other.example.org/episode/5",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &other, &item(5), None, "1 day", "0 s").await;
    let maybe = card(
        &pool,
        "https://show.example.com/episode/5/",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &maybe, &item(5), Some("maybe"), "1 day", "0 s").await;
    let sibling = card(
        &pool,
        "https://www.show.example.com/episode/5/",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &sibling, &item(5), None, "1 day", "0 s").await;
    let directory = card(
        &pool,
        "https://podcastindex.org/podcast/1?episode=5",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &directory, &item(5), None, "1 day", "0 s").await;
    // 4: the publisher's page names it, but expired; 3: switched off; 2: read as an article.
    let expired = card(
        &pool,
        "https://show.example.com/episode/4/",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &expired, &item(4), None, "-1 day", "2 days").await;
    let off = card(
        &pool,
        "https://show.example.com/episode/3/",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &off, &item(3), None, "1 day", "0 s").await;
    sqlx::query("INSERT INTO also_on_source_off(source) VALUES('podcasts')")
        .execute(&pool)
        .await
        .unwrap();
    let article = card(&pool, "https://show.example.com/episode/2/", "Article").await;
    names(&pool, &article, &item(2), None, "1 day", "0 s").await;
    // 1: the feed item is a card of its own, with its page as an alias.
    let own = card(&pool, &item(1), "PodcastEpisode").await;
    let joined = store::ensure_resource(&pool, "https://show.example.com/episode/1/")
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'source')")
        .bind(&joined)
        .bind(&own)
        .execute(&pool)
        .await
        .unwrap();
    // 7: two publisher's pages: the card known first wins, the other follows.
    let older = card(&pool, "https://show.example.com/e/7", "PodcastEpisode").await;
    names(&pool, &older, &item(7), None, "1 day", "2 days").await;
    let newer = card(
        &pool,
        "https://show.example.com/episode/7/",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &newer, &item(7), None, "1 day", "1 hour").await;
    // 8: a publisher's page that is an alias, and one not `ok`; 9: one of another type.
    let host = card(
        &pool,
        "https://show.example.com/episode/8/",
        "PodcastEpisode",
    )
    .await;
    let aliased = card(&pool, "https://show.example.com/ep/8", "PodcastEpisode").await;
    names(&pool, &aliased, &item(8), None, "1 day", "0 s").await;
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'source')")
        .bind(&aliased)
        .bind(&host)
        .execute(&pool)
        .await
        .unwrap();
    let missing = card(
        &pool,
        "https://show.example.com/episode/8/old",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &missing, &item(8), None, "1 day", "0 s").await;
    sqlx::query("UPDATE resource SET status='missing' WHERE id=$1")
        .bind(&missing)
        .execute(&pool)
        .await
        .unwrap();
    let event = card(&pool, "https://show.example.com/episode/9/", "Event").await;
    names(&pool, &event, &item(9), None, "1 day", "0 s").await;
    // 10: the page's link names the item by its root spelling, without the tracking parameter.
    let ten = card(
        &pool,
        "https://show.example.com/episode/10/",
        "PodcastEpisode",
    )
    .await;
    names(&pool, &ten, &item(10), None, "1 day", "0 s").await;

    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let entries = |page: &Value| -> Vec<Value> { page["items"].as_array().unwrap().clone() };
    let find = |items: &[Value], g: &str| items.iter().find(|e| e["guid"] == g).unwrap().clone();
    let (status, page_json) = request(&app, &format!("/v1/resources/{show_id}/episodes")).await;
    assert_eq!(status, 200, "{page_json}");
    let three = find(&entries(&page_json), "ep-3");
    assert_eq!(three["uri"], item(3), "a switched-off source names nothing");
    assert!(three.get("id").is_none() && three.get("aliases").is_none());
    sqlx::query("DELETE FROM also_on_source_off")
        .execute(&pool)
        .await
        .unwrap();
    let (_, page_json) = request(&app, &format!("/v1/resources/{show_id}/episodes")).await;
    let items = entries(&page_json);
    let six = find(&items, "ep-6");
    assert_eq!(
        six["id"], page,
        "the publisher's page, not Podcast Index's: {six}"
    );
    assert_eq!(six["uri"], "https://show.example.com/episode/6/");
    assert_eq!(
        six["aliases"],
        serde_json::json!([
            item(6),
            "https://show.example.com/episode/6/?utm_source=x",
            "https://show.example.com/p/6"
        ]),
        "the feed item first, then the card's aliases, then the feed item's card's"
    );
    for (guid, why) in [
        (
            "ep-5",
            "a directory, another site, a sibling host and a maybe link",
        ),
        ("ep-4", "an expired link"),
        ("ep-8", "an alias and a card not ok"),
        ("ep-9", "a card of another type"),
    ] {
        let e = find(&items, guid);
        assert_eq!(e["uri"], item(guid[3..].parse().unwrap()), "{why}: {e}");
        assert!(e.get("id").is_none(), "{why}: {e}");
        assert!(e.get("aliases").is_none(), "{why}: {e}");
    }
    let two = find(&items, "ep-2");
    assert_eq!(two["uri"], item(2), "an article is no episode: {two}");
    assert!(two.get("id").is_none(), "{two}");
    assert_eq!(
        two["aliases"],
        serde_json::json!(["https://show.example.com/episode/2/"]),
        "its link is read with the entry"
    );
    assert_eq!(find(&items, "ep-3")["id"], off, "with the source on again");
    let seven = find(&items, "ep-7");
    assert_eq!(seven["id"], older, "the card known first: {seven}");
    assert_eq!(
        seven["aliases"],
        serde_json::json!([item(7), "https://show.example.com/episode/7/"])
    );
    // Whichever was known first, not whichever sorts first.
    sqlx::query("UPDATE resource SET created_at=now()-interval '1 day' WHERE id=$1")
        .bind(&newer)
        .execute(&pool)
        .await
        .unwrap();
    let (_, again) = request(&app, &format!("/v1/resources/{show_id}/episodes")).await;
    let seven_again = find(&entries(&again), "ep-7");
    assert_eq!(seven_again["id"], newer, "{seven_again}");
    sqlx::query("UPDATE resource SET created_at=now()-interval '2 days' WHERE id=$1")
        .bind(&older)
        .execute(&pool)
        .await
        .unwrap();
    let one = find(&items, "ep-1");
    assert_eq!(one["id"], own, "the feed item's own card: {one}");
    assert_eq!(one["uri"], item(1));
    assert_eq!(
        one["aliases"],
        serde_json::json!(["https://show.example.com/episode/1/"])
    );
    // The show's card names the same.
    let (_, show_card) = request(&app, &format!("/v1/resources/{show_id}")).await;
    let on_card = show_card["episodes"].as_array().unwrap();
    assert_eq!(on_card[0]["guid"], "ep-10", "{show_card}");
    assert_eq!(
        on_card[0]["id"], ten,
        "a link stored under the root spelling: {show_card}"
    );
    assert_eq!(on_card[0]["aliases"], serde_json::json!([tracked_item]));
    let card_seven = on_card.iter().find(|e| e["guid"] == "ep-7").unwrap();
    assert_eq!(card_seven["id"], older, "{show_card}");
    assert_eq!(*card_seven, seven, "{show_card}");
}

/// A feed card's cut read whose read on gave nothing (`ResourceMeta::episodes_cut`: too slow, or
/// the read failed) keeps the older episodes a longer stored list holds under its own, newer
/// ones first, and what that list says of its feed; a list it shares no episode with, or a save
/// without the flag, replaces the stored list as before.
#[sqlx::test(migrations = "../../migrations")]
async fn a_failed_read_on_keeps_the_longer_stored_list(pool: PgPool) {
    let uri = "https://feeds.example.org/long.xml";
    let list: Vec<Episode> = (1..=1001).rev().map(|n| episode(n, true)).collect();
    let id = show(&pool, uri, Some(list)).await;
    assert_eq!(rows(&pool, &id).await, 1000);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let names = |page: &Value| -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let cut = |list: Vec<Episode>, flagged: bool| {
        let mut d = ResourceMeta::new(uri, "web", 86_400);
        d.set_type("PodcastSeries");
        d.episodes = Some(list);
        d.episodes_older = true;
        d.episodes_cut = flagged;
        d
    };
    // The cut read's 100 with one new episode on top: still 1000, the new one first.
    let d = cut((902..=1002).rev().map(|n| episode(n, true)).collect(), true);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(rows(&pool, &id).await, 1000);
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(names(&page)[0], "Episode 1002");
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=100")).await;
    assert_eq!(names(&page)[0], "Episode 902", "{page}");
    assert_eq!(page["truncated"], true);
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=980")).await;
    assert_eq!(names(&page)[19], "Episode 3", "{page}");
    // A stored list that was the feed's whole one stays whole: `truncated` does not flip.
    let whole: Vec<Episode> = (1..=183).rev().map(|n| episode(n, true)).collect();
    show(&pool, uri, Some(whole)).await;
    let d = cut((85..=184).rev().map(|n| episode(n, true)).collect(), true);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(rows(&pool, &id).await, 184);
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes?offset=180")).await;
    assert_eq!(
        names(&page),
        ["Episode 4", "Episode 3", "Episode 2", "Episode 1"]
    );
    assert_eq!(page["truncated"], false, "{page}");
    // A cut list the stored one shares nothing with (another feed's) is the list.
    let other: Vec<Episode> = (1..=100)
        .rev()
        .map(|n| Episode {
            guid: format!("other-{n}"),
            ..episode(n + 2000, true)
        })
        .collect();
    store::save_document(&pool, &id, 0, &cut(other, true))
        .await
        .unwrap();
    assert_eq!(rows(&pool, &id).await, 100);
    let (_, page) = request(&app, &format!("/v1/resources/{id}/episodes")).await;
    assert_eq!(page["truncated"], true);
    // Without the flag, a cut read's list replaces the stored one, as any list does.
    let list: Vec<Episode> = (1..=1001).rev().map(|n| episode(n, true)).collect();
    show(&pool, uri, Some(list)).await;
    let d = cut(
        (902..=1001).rev().map(|n| episode(n, true)).collect(),
        false,
    );
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(rows(&pool, &id).await, 100);
}

/// A show without a list of its own borrows, of its group members whose newest episode is the
/// newest, the one with the longest list: a member whose list was stored before a longer one
/// was kept (the same feed, read under an older bound) does not cut the show's list short.
#[sqlx::test(migrations = "../../migrations")]
async fn a_borrowed_list_is_the_longest_of_the_newest(pool: PgPool) {
    let a = show(&pool, "https://feeds.example.org/show.xml", None).await;
    let b = show(&pool, "https://podcastindex.org/podcast/1", None).await;
    let apple = show(
        &pool,
        "https://podcasts.apple.com/us/podcast/show/id1",
        None,
    )
    .await;
    // The member that wins a tie by ID gets the short list.
    let (low, high) = if a < b { (&a, &b) } else { (&b, &a) };
    let long: Vec<Episode> = (1..=1000).rev().map(|n| episode(n, true)).collect();
    let short: Vec<Episode> = (901..=1000).rev().map(|n| episode(n, true)).collect();
    let save = |id: String, list: Vec<Episode>| {
        let pool = pool.clone();
        async move {
            let uri: String = sqlx::query_scalar("SELECT uri FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
            let mut d = ResourceMeta::new(&uri, "web", 86_400);
            d.set_type("PodcastSeries");
            d.episodes = Some(list);
            store::save_document(&pool, &id, 0, &d).await.unwrap();
        }
    };
    save(low.clone(), short).await;
    save(high.clone(), long).await;
    let group = "0123456789abcdef0123456789abcde2";
    sqlx::query("INSERT INTO \"group\"(id) VALUES($1)")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id=ANY($2)")
        .bind(group)
        .bind(vec![a.clone(), b.clone(), apple.clone()])
        .execute(&pool)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (_, page) = request(&app, &format!("/v1/resources/{apple}/episodes?offset=100")).await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 20, "{page}");
    assert_eq!(items[0]["guid"], "ep-900");
    // A newer episode still wins over a longer list (grouped again, as the grouping pass would
    // leave it after the save).
    save(
        low.clone(),
        (901..=1001).rev().map(|n| episode(n, true)).collect(),
    )
    .await;
    sqlx::query("INSERT INTO \"group\"(id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id=ANY($2)")
        .bind(group)
        .bind(vec![a.clone(), b.clone(), apple.clone()])
        .execute(&pool)
        .await
        .unwrap();
    let (_, page) = request(&app, &format!("/v1/resources/{apple}/episodes")).await;
    assert_eq!(page["items"][0]["guid"], "ep-1001", "{page}");
}
