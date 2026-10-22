//! Pubky app profiles (`resolver::pubky_apps`): each app's recorded objects, the general card for
//! other apps, paths that are never fetched, and hostile objects.
use axum::{body::Body, extract::ConnectInfo, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    model::{ResourceMeta, Status},
    resolver::{classify, pubky_apps},
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::BTreeMap;
use tower::ServiceExt;
mod common;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pubky_apps");

/// A recorded object: its URI and body.
fn fixture(name: &str) -> (String, Vec<u8>) {
    let read = |ext: &str| std::fs::read(format!("{FIXTURES}/{name}.{ext}")).unwrap();
    let uri = String::from_utf8(read("uri")).unwrap().trim().to_owned();
    (uri, read("json"))
}
/// The stored card of `body` at `uri`, as the resolver saves it.
fn card_of(uri: &str, body: &[u8]) -> ResourceMeta {
    pubky_apps::card(uri, "application/octet-stream", body, 86400)
        .bounded(pubky_apps::attributes())
        .unwrap()
}
fn card(name: &str) -> ResourceMeta {
    let (uri, body) = fixture(name);
    card_of(&uri, &body)
}
/// A recorded object with some members changed.
fn edited(name: &str, change: Value) -> ResourceMeta {
    let (uri, body) = fixture(name);
    let mut v: Value = serde_json::from_slice(&body).unwrap();
    for (k, value) in change.as_object().unwrap() {
        v[k] = value.clone();
    }
    card_of(&uri, v.to_string().as_bytes())
}
fn links(doc: &ResourceMeta) -> Vec<(String, String)> {
    doc.open_in
        .iter()
        .map(|o| (o.client.clone(), o.url.clone()))
        .collect()
}
fn link(client: &str, url: &str) -> Vec<(String, String)> {
    vec![(client.to_owned(), url.to_owned())]
}
fn facts(doc: &ResourceMeta) -> BTreeMap<String, String> {
    pubky_apps::facts(doc).unwrap_or_default()
}
/// No card: kept as a missing object, without content or links.
fn no_card(doc: &ResourceMeta) {
    assert_eq!(doc.status, Status::Missing, "{doc:?}");
    assert_eq!(
        (&doc.name, &doc.description, &doc.image),
        (&None, &None, &None)
    );
    assert!(doc.open_in.is_empty() && facts(doc).is_empty(), "{doc:?}");
}
/// Each path below `owner`'s `/pub/` is never fetched.
fn never_fetched(paths: &[&str]) {
    let owner = "ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi";
    for path in paths {
        let uri = format!("pubky://{owner}/pub/{path}");
        assert_eq!(classify(&uri), None, "{uri}");
    }
}

#[test]
fn loopky_decks() {
    let doc = card("loopky/medeng2026a-url-cover");
    assert_eq!(doc.status, Status::Ok);
    assert_eq!(doc.type_name, "LearningResource");
    assert_eq!(
        doc.name.as_deref(),
        Some("Inglês para medicina e enfermagem")
    );
    assert!(doc
        .description
        .as_ref()
        .unwrap()
        .starts_with("1210 cartões"));
    assert!(doc
        .image
        .as_ref()
        .unwrap()
        .starts_with("https://encrypted-tbn0.gstatic.com/"));
    assert_eq!(doc.site, "loopky");
    assert_eq!(
        links(&doc),
        link(
            "Loopky",
            "https://loopky.app/deck/?author=3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo&id=medeng2026a"
        )
    );
    assert_eq!(
        facts(&doc),
        BTreeMap::from([
            ("card_count".into(), "1210".into()),
            ("front_lang".into(), "pt-BR".into()),
            ("back_lang".into(), "en-US".into()),
            (
                "topics".into(),
                "medicine; nursing; healthcare; 🩺; language; portuguese; english".into()
            ),
        ])
    );
    // A cover in the deck's own folder is a file there.
    let doc = card("loopky/p2uuj5jo5ln2-media-cover");
    assert_eq!(
        doc.image.as_deref(),
        Some("pubky://yjn8k7ww4sc4hskkmbcsep6b1xje8g4f8j3azpdqnwhhqckt7kso/pub/loopky/decks/p2uuj5jo5ln2/media/f67d9cfd14b856e57895c96de53e510385d90f0d2c12d598a6b8386e2bcb2f7b.jpg")
    );
    // A deck ID as the CLI writes it; anything outside `-._~` and alphanumerics is encoded.
    for (deck, id) in [("bobesponja-t1", "bobesponja-t1"), ("a b&c", "a%20b%26c")] {
        assert_eq!(
            pubky_apps::open_in(&format!(
                "pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/{}/manifest.json",
                deck.replace(' ', "%20")
            ))[0]
                .url,
            format!("https://loopky.app/deck/?author=3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo&id={id}")
        );
    }
    let doc = card("loopky/y9zq5xw1ng1e-bare");
    assert_eq!((doc.image, doc.description), (None, None));
    // A cover path that leaves public storage is no image.
    let doc = edited(
        "loopky/p2uuj5jo5ln2-media-cover",
        json!({"cover_image_ref": {"path": "../../../../priv/loopky/x.jpg", "mime": "image/jpeg"}}),
    );
    assert_eq!(doc.image, None);
    // A deck still being uploaded, or a manifest another key claims, has no card.
    no_card(&edited(
        "loopky/medeng2026a-url-cover",
        json!({"incomplete": true}),
    ));
    no_card(&edited(
        "loopky/medeng2026a-url-cover",
        json!({"author_pubky": "yjn8k7ww4sc4hskkmbcsep6b1xje8g4f8j3azpdqnwhhqckt7kso"}),
    ));
    never_fetched(&[
        "loopky/settings.json",
        "loopky/srs/yjn8k7ww4sc4hskkmbcsep6b1xje8g4f8j3azpdqnwhhqckt7kso/p2uuj5jo5ln2/0.json",
        "loopky/subscriptions/yjn8k7ww4sc4hskkmbcsep6b1xje8g4f8j3azpdqnwhhqckt7kso/p2uuj5jo5ln2.json",
        "loopky/decks/p2uuj5jo5ln2/cards/0.json",
        "loopky/decks/p2uuj5jo5ln2/media/f67d.jpg",
        "loopky/tags/0035SFYCGVBV2",
        "loopky/decks//manifest.json",
    ]);
}

#[test]
fn rooms() {
    let doc = card("rooms/0035SFYCGVBV2-introductions");
    assert_eq!(doc.type_name, "Conversation");
    assert_eq!(doc.name.as_deref(), Some("Introductions"));
    assert_eq!(
        doc.description.as_deref(),
        Some("Who you are and what you are building.")
    );
    assert_eq!(
        links(&doc),
        link(
            "Pubky Rooms",
            "https://pubky-rooms-staging.fly.dev/r/47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/0035SFYCGVBV2"
        )
    );
    assert_eq!(card("rooms/0035TNXH48JYE-null-topic").description, None);
    // An unlisted room, or one in a format this build doesn't know, has no card.
    no_card(&edited(
        "rooms/0035SFYCGVBV2-introductions",
        json!({"visibility": "unlisted"}),
    ));
    no_card(&edited(
        "rooms/0035SFYCGVBV2-introductions",
        json!({"v": 2}),
    ));
    never_fetched(&[
        "pubky-rooms/members/47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/0035SFYCGVBV2",
        "pubky-rooms/messages/47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/0035SFYCGVBV2/0035SG5BDKZ18",
        "pubky-rooms/mutes/47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao",
        "pubky-rooms/profile.json",
        "pubky-rooms/rooms/0035SFYCGVBV2/extra",
    ]);
}

#[test]
fn other_apps_get_the_general_card() {
    let uri =
        "pubky://ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi/pub/other.app/items/1";
    assert_eq!(classify(uri), Some("pubky"));
    let doc = card_of(
        uri,
        br#"{"topic":"A topic","bio":"About it","imageUrl":"pic.png","password":"never kept"}"#,
    );
    assert_eq!(doc.type_name, "CreativeWork");
    assert_eq!(doc.name.as_deref(), Some("A topic"));
    assert_eq!(doc.description.as_deref(), Some("About it"));
    assert_eq!(
        doc.image.as_deref(),
        Some("pubky://ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi/pub/other.app/items/pic.png")
    );
    assert!(doc.open_in.is_empty() && facts(&doc).is_empty());
    assert!(!serde_json::to_string(&doc).unwrap().contains("never kept"));
    let doc = card_of(uri, br#"{"text":"Just words"}"#);
    assert_eq!(doc.type_name, "SocialMediaPosting");
    // A known app's path that no kind matches is never fetched, and pubky.app's own objects are
    // Nexus's.
    never_fetched(&["pubky-rooms/settings.json", "pubky.app/profile.json"]);
}

#[test]
fn hostile_objects_get_no_card_or_a_bounded_one() {
    let room = "pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2";
    // Nesting past the JSON reader's depth: not JSON, so no card for a known kind and a plain
    // one for another app.
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    no_card(&card_of(room, deep.as_bytes()));
    let other = "pubky://ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi/pub/other.app/x";
    let doc = card_of(other, deep.as_bytes());
    assert_eq!((doc.status, doc.name), (Status::Ok, None));
    // Huge members are cut to the document's bounds.
    let huge = json!({"v": 1, "visibility": "public", "name": "n".repeat(3_000_000),
        "topic": "t".repeat(3_000_000)});
    let doc = card_of(room, huge.to_string().as_bytes());
    assert_eq!(doc.name.as_ref().unwrap().len(), 512);
    assert!(serde_json::to_vec(&doc).unwrap().len() < 8192);
    // Not an object, or the wrong types.
    for body in [
        &b"[]"[..],
        b"\"x\"",
        b"null",
        br#"{"v":"1","visibility":"public","name":7}"#,
    ] {
        no_card(&card_of(room, body));
    }
    // Paths outside the namespace or spelled to leave it.
    never_fetched(&[
        "pubky-rooms/rooms/..%2F..%2F..%2Fpriv%2Fx",
        "loopky/decks/%2E%2E/manifest.json",
    ]);
}

async fn get(app: &axum::Router, path: &str) -> Value {
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200, "{path}");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap();
    common::check_response("GET", path, 200, &value);
    value
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cards_carry_their_client_links_and_facts(pool: PgPool) {
    // A read deck: its stored links and facts.
    let (uri, body) = fixture("loopky/medeng2026a-url-cover");
    let id = store::ensure_resource(&pool, &uri).await.unwrap();
    store::save_document(&pool, &id, 0, &card_of(&uri, &body))
        .await
        .unwrap();
    // A room not read yet: its kind's link.
    let (room, _) = fixture("rooms/0035SFYCGVBV2-introductions");
    let room_id = store::ensure_resource(&pool, &room).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let deck = get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(
        deck["open_in"],
        json!([{"client": "Loopky", "url": "https://loopky.app/deck/?author=3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo&id=medeng2026a"}])
    );
    assert_eq!(deck["details"]["facts"]["card_count"], "1210");
    let room = get(&app, &format!("/v1/resources/{room_id}")).await;
    assert_eq!(room["status"], "pending");
    assert_eq!(room["open_in"][0]["client"], "Pubky Rooms");
    // A web page has none.
    let web = store::ensure_resource(&pool, "https://example.org/")
        .await
        .unwrap();
    assert_eq!(
        get(&app, &format!("/v1/resources/{web}")).await["open_in"],
        json!([])
    );
}
