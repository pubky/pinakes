//! A show's latest episodes through the store and the route (`episodes`): the save writes a
//! show's list and keeps or clears it as later saves decide; `GET /v1/resources/{id}/episodes`
//! lists it, names the episodes Pinakes has as resources, and falls back to a group member's;
//! a show's card carries the first of them that have audio (`Card::episodes`).
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
/// twenty are kept.
#[sqlx::test(migrations = "../../migrations")]
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

/// A feed as a show's read fetches it.
fn fetched(body: String) -> pinakes_core::resolver::egress::Fetched {
    pinakes_core::resolver::egress::Fetched {
        retry_after_secs: 0,
        ..common::fetched(
            "https://feeds.example.org/show.xml".parse().unwrap(),
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
    assert_eq!(list[0]["uri"], "https://show.example/episode/5");
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
    let known = store::ensure_resource(&pool, "https://show.example/episode/4")
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
