//! Pubky app profiles (`resolver::pubky_apps`): each app's recorded objects, the general card for
//! other apps, paths that are never fetched, and hostile objects.
use axum::{body::Body, extract::ConnectInfo, http::Request};
use chrono::{Duration, Utc};
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
    pubky_apps::card_at(uri, "application/octet-stream", body, 86400, Utc::now())
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
    // A cover with a user name or a port is no image (a profile's links are read as the general
    // card's), nor is one that leaves public storage.
    for url in [
        "https://u:p@example.org/c.jpg",
        "https://example.org:8443/c.jpg",
    ] {
        let doc = edited(
            "loopky/medeng2026a-url-cover",
            json!({"cover_image_ref": {"url": url, "mime": "image/jpeg"}}),
        );
        assert_eq!(doc.image, None, "{url}");
    }
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

/// An app whose client links are held back opens in no client, and one with a link has no
/// held-back note: switching a link on means removing the note the startup log reads.
#[test]
fn held_back_links_are_off_and_said_so() {
    for profile in pubky_apps::REGISTRY {
        let linked = profile.kinds.iter().any(|kind| !kind.open_in.is_empty());
        assert!(
            !(linked && profile.links_off.is_some()),
            "{:?} opens in a client and says its links are off",
            profile.namespaces
        );
    }
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
    // No client link until rooms.pubky.app is live; the startup log says why.
    assert!(doc.open_in.is_empty(), "{:?}", doc.open_in);
    assert!(pubky_apps::rooms::PROFILE
        .links_off
        .is_some_and(|why| why.contains("rooms.pubky.app")));
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
fn marketplace() {
    let shop = "rxtcr8ezoja67qq3wcrxtueefpzaau65tr8nf5o8d8e8oc7a33jo";
    let doc = card("marketplace/shop");
    assert_eq!(doc.type_name, "OnlineStore");
    assert_eq!(doc.name.as_deref(), Some("Dezentralshop"));
    assert_eq!(doc.description.as_deref(), Some("Your Swiss Bitcoin store"));
    // Its avatar: the media file the record names.
    assert!(doc
        .image
        .as_ref()
        .unwrap()
        .ends_with("/media/2970e7ce535d4a39ae1ecc7fcdf39ece"));
    assert_eq!(doc.site, "app.marketplace");
    assert_eq!(facts(&doc)["country"], "CH");
    assert_eq!(
        links(&doc),
        link(
            "Pubky Shop",
            &format!("https://pubky.app/shop/marketplace/shop/{shop}")
        )
    );

    let doc = card("marketplace/listing-active");
    assert_eq!(doc.type_name, "Product");
    assert_eq!(doc.name.as_deref(), Some("Hydrangea paniculata"));
    assert!(doc
        .image
        .as_ref()
        .unwrap()
        .contains("/marketplace/v1/media/"));
    assert_eq!(
        facts(&doc),
        BTreeMap::from([
            ("price".into(), "1.00 USD".into()),
            ("condition".into(), "excellent".into()),
            ("category".into(), "home-garden".into()),
            ("country".into(), "FR".into()),
        ])
    );
    assert_eq!(
        links(&doc),
        link(
            "Pubky Shop",
            "https://pubky.app/shop/marketplace/listing/8fwmk5o1wfmn6whew47zoq31ka1mss15xnkcihattr5zbthp5nbo/14838d8cf28d413ea274ac7ec608c5f8"
        )
    );
    // An adult-only listing shows no image.
    let doc = edited("marketplace/listing-active", json!({"adultOnly": true}));
    assert_eq!((doc.status, doc.image), (Status::Ok, None));
    // An auction: from its starting price, until its end.
    let auction = |ends: chrono::DateTime<Utc>| {
        edited(
            "marketplace/listing-active",
            json!({"sale": {"format": "auction",
                "startingPrice": {"amountMinor": 1000, "currency": "BTC", "exponent": 8},
                "minimumIncrement": {"amountMinor": 100, "currency": "BTC", "exponent": 8},
                "startsAt": "2026-10-01T00:00:00Z", "endsAt": ends.to_rfc3339()}}),
        )
    };
    let ends = Utc::now() + Duration::days(1);
    let doc = auction(ends);
    assert_eq!(facts(&doc)["price"], "0.00001000 BTC");
    // Fetched again when it ends; a fixed-price listing has no such time.
    assert_eq!(doc.refresh_by.unwrap().timestamp(), ends.timestamp());
    assert_eq!(card("marketplace/listing-active").refresh_by, None);
    no_card(&auction(Utc::now() - Duration::days(1)));
    // Paused, ended or removed listings, and records of another kind or owner, have no card.
    no_card(&card("marketplace/listing-paused"));
    for state in ["ended", "removed"] {
        no_card(&edited(
            "marketplace/listing-active",
            json!({"state": state}),
        ));
    }
    no_card(&edited(
        "marketplace/listing-active",
        json!({"ownerPubky": shop}),
    ));
    no_card(&edited(
        "marketplace/listing-active",
        json!({"recordType": "drop"}),
    ));
    no_card(&edited(
        "marketplace/listing-active",
        json!({"listingId": "another"}),
    ));

    let doc = card("marketplace/drop");
    assert_eq!(doc.type_name, "SaleEvent");
    assert_eq!(doc.name.as_deref(), Some("faraday bag"));
    assert_eq!(doc.start.unwrap().to_rfc3339(), "2026-09-10T11:45:00+00:00");
    assert_eq!(doc.end, None);
    assert!(doc
        .image
        .as_ref()
        .unwrap()
        .contains("/marketplace/v1/media/"));
    assert_eq!(
        links(&doc),
        link(
            "Pubky Shop",
            "https://pubky.app/shop/marketplace/drop/nkcct8tzquo8n4z5ysz9t963ye9kq1w7gb55aad1z4tmsgjjhmto/0700b93b984743c39f6d8009f4aa1271"
        )
    );
    never_fetched(&[
        "app.marketplace/v1/media/2970e7ce535d4a39ae1ecc7fcdf39ece",
        "app.marketplace/v1/reviews/0035SFYCGVBV2",
        "app.marketplace/v1/deliverables/a/1",
        "app.marketplace/v1/posts/0035SFYCGVBV2/0035SG5BDKZ18.json",
        "app.marketplace/v2/listings/x",
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
    // An image is a link as any the profiles read: https or public Pubky storage, with no user
    // name, password or port; plain http, a private path, a port or a user name is none.
    for image in [
        "http://example.org/pic.png",
        "https://u:p@example.org/pic.png",
        "https://example.org:8443/pic.png",
        "pubky://ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi/priv/other.app/pic.png",
        "../../../priv/other.app/pic.png",
    ] {
        let doc = card_of(
            uri,
            json!({"name": "n", "image": image}).to_string().as_bytes(),
        );
        assert_eq!(doc.image, None, "{image}");
    }
    let doc = card_of(
        uri,
        br#"{"name":"n","image":"https://example.org/pic.png"}"#,
    );
    assert_eq!(doc.image.as_deref(), Some("https://example.org/pic.png"));
    // A known app's path that no kind matches is never fetched; pubky.app's own objects and the
    // social specs' (their posts, events and rsvps included) are Nexus's, and the old Mapky and
    // Eventky layouts are disregarded.
    never_fetched(&[
        "loopky/settings.json",
        "pubky.app/profile.json",
        "social/v1/posts/0034A0X7NJ52G",
        "social/v1/tags/0034A0X7NJ52G",
        "mapky.app/routes/00351MV7WF0WG",
        "eventky.app/events/0035DS0K1ZTEG",
    ]);
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

#[sqlx::test(migrations = "../../migrations")]
async fn cards_carry_their_client_links_and_facts(pool: PgPool) {
    // A read deck: its stored links and facts.
    let (uri, body) = fixture("loopky/medeng2026a-url-cover");
    let id = store::ensure_resource(&pool, &uri).await.unwrap();
    store::save_document(&pool, &id, 0, &card_of(&uri, &body))
        .await
        .unwrap();
    // A room not read yet: its kind's links (none until rooms.pubky.app is live).
    let (room, _) = fixture("rooms/0035SFYCGVBV2-introductions");
    let room_id = store::ensure_resource(&pool, &room).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let deck = get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(
        deck["open_in"],
        json!([{"client": "Loopky", "url": "https://loopky.app/deck/?author=3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo&id=medeng2026a"}])
    );
    assert_eq!(deck["facts"]["card_count"], "1210");
    let room = get(&app, &format!("/v1/resources/{room_id}")).await;
    assert_eq!(room["status"], "pending");
    assert_eq!(room["open_in"], json!([]));
    // A web page has none.
    let web = store::ensure_resource(&pool, "https://example.org/")
        .await
        .unwrap();
    assert_eq!(
        get(&app, &format!("/v1/resources/{web}")).await["open_in"],
        json!([])
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_object_that_loses_its_card_keeps_nothing_of_it(pool: PgPool) {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    for (name, change) in [
        (
            "rooms/0035SFYCGVBV2-introductions",
            json!({"visibility": "unlisted"}),
        ),
        ("marketplace/listing-active", json!({"state": "paused"})),
    ] {
        let (uri, body) = fixture(name);
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let mut doc = card_of(&uri, &body);
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        let shown = get(&app, &format!("/v1/resources/{id}")).await;
        assert_eq!(shown["status"], "ok", "{shown}");
        // Withdrawn by its owner: missing at once (its content says so, it cannot flap), with
        // nothing of the card it had.
        let mut v: Value = serde_json::from_slice(&body).unwrap();
        for (k, value) in change.as_object().unwrap() {
            v[k] = value.clone();
        }
        let mut gone = pubky_apps::card_at(
            &uri,
            "application/octet-stream",
            v.to_string().as_bytes(),
            86400,
            Utc::now(),
        );
        assert_eq!(gone.status, Status::Missing);
        doc.fetched_at += 1000;
        gone.fetched_at = doc.fetched_at;
        store::save_document(&pool, &id, 0, &gone).await.unwrap();
        let shown = get(&app, &format!("/v1/resources/{id}")).await;
        assert_eq!(shown["status"], "missing", "{shown}");
        for field in ["name", "description", "image", "point"] {
            assert!(shown[field].is_null(), "{name}: {field} in {shown}");
        }
        assert_eq!(shown["type"], "CreativeWork", "{shown}");
        assert_eq!(shown["open_in"], json!([]), "{shown}");
        assert!(shown["facts"].is_null(), "{shown}");
        // Its history keeps its lines (created, then withdrawn) but none of its text.
        let lines: Vec<(String, Option<Value>, Option<Value>)> = sqlx::query_as(
            "SELECT class,before,after FROM resource_change WHERE resource_id=$1 ORDER BY seq",
        )
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(lines.len(), 2, "{name}: {lines:?}");
        assert!(
            lines.iter().all(|(_, b, a)| b.is_none() && a.is_none()),
            "{name}: {lines:?}"
        );
        let name_text = doc.name.clone().unwrap();
        let journal: String =
            sqlx::query_scalar("SELECT coalesce(string_agg(r::text,' '),'') FROM resource_change r WHERE resource_id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!journal.contains(&name_text), "{name}: {journal}");
    }
}

/// The card an object of `uri` had before Pinakes stopped fetching its path: the general card of
/// its JSON `object`, typed `type_name` (`None`: as the general card types it).
fn stored_before(uri: &str, object: Value, type_name: Option<&str>) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, "pubky", 86400);
    doc.set_type("CreativeWork");
    pubky_apps::general(uri, &object, &mut doc);
    if let Some(type_name) = type_name {
        doc.set_type(type_name);
    }
    doc
}
/// A tag on `id`, so its card is a search result.
async fn tagged(pool: &PgPool, id: &str) {
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','place',$3,now())")
        .bind(id)
        .bind("a".repeat(52))
        .bind(format!("/pub/app/tags/{id}"))
        .execute(pool)
        .await
        .unwrap();
}
/// The URIs a search lists.
async fn found(app: &axum::Router, query: &str) -> Vec<String> {
    get(app, &format!("/v1/search?{query}")).await["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uri"].as_str().unwrap().to_owned())
        .collect()
}
/// What a resource still keeps of a card: its name, type, description and search text, and the
/// lines of its history that keep a document.
async fn kept(
    pool: &PgPool,
    id: &str,
) -> (Option<String>, Option<String>, Option<String>, i32, i64) {
    sqlx::query_as(
        "SELECT name,type,doc->>'description',length(search_text),\
         (SELECT count(*) FROM resource_change c WHERE c.resource_id=r.id AND (c.before IS NOT NULL OR c.after IS NOT NULL)) \
         FROM resource r WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Objects at paths Pinakes never fetches (the old Mapky and Eventky layouts, a path a profile
/// leaves out) get no route when a reference names them, so nothing fetches them. One that holds
/// a card all the same (stored while its path was read) keeps nothing of it once it is read
/// again: the resolver gives it no card, without a request, and its save leaves a missing
/// `CreativeWork` with nothing of the object and a history without documents, found neither by
/// its text nor by its old type. Another app's object keeps its route and its card.
#[sqlx::test(migrations = "../../migrations")]
async fn an_object_at_a_path_never_fetched_keeps_nothing_once_read_again(pool: PgPool) {
    let owner = "ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi";
    let at = |path: &str| format!("pubky://{owner}/pub/{path}");
    let left_out = [
        stored_before(
            &at("mapky.app/posts/00358SJCCNK1G"),
            json!({"content": "Excited to check out this place one day!"}),
            None,
        ),
        stored_before(
            &at("eventky.app/events/0034FED2YCDKG"),
            json!({"summary": "Lakeside meetup", "description": "Bring a lantern to the pier"}),
            Some("Event"),
        ),
        stored_before(
            &at("mapky.app/routes/0034FED2YCDKH"),
            json!({"name": "Old town loop", "description": "Cobbles and fountains"}),
            Some("Trip"),
        ),
        stored_before(
            &at("mapky.app/collections/0034FED2YCDKJ"),
            json!({"name": "Quiet cafés"}),
            None,
        ),
        stored_before(
            &at("loopky/settings.json"),
            json!({"name": "Night owl settings"}),
            None,
        ),
        stored_before(
            &at("mapky.app/reviews/0034FED2YCDKK"),
            json!({"content": "Lovely terrace, slow service."}),
            None,
        ),
    ];
    let other = stored_before(
        &at("chess/games/0034FED2YCDKM"),
        json!({"content": "A quiet opening, then fireworks."}),
        None,
    );
    let mut ids = vec![];
    for doc in left_out.iter().chain([&other]) {
        let id = store::ensure_resource(&pool, &doc.url).await.unwrap();
        // No route for a path never fetched: no worker ever leases it.
        let route: Option<String> = sqlx::query_scalar("SELECT resolver FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            route.as_deref(),
            (doc.url == other.url).then_some("pubky"),
            "{}",
            doc.url
        );
        store::save_document(&pool, &id, 0, doc).await.unwrap();
        tagged(&pool, &id).await;
        ids.push(id);
    }
    let other_id = ids.pop().unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let posting = found(&app, "type=SocialMediaPosting").await;
    assert!(posting.contains(&left_out[0].url), "{posting:?}");
    assert_eq!(
        found(&app, "q=fountains").await,
        std::slice::from_ref(&left_out[2].url)
    );
    for id in &ids {
        let (name, _, description, _, lines) = kept(&pool, id).await;
        assert!(name.is_some() || description.is_some(), "{id}");
        assert_eq!(lines, 1, "{id}");
    }

    // Read again: the resolver gives each no card, without a request, and its save keeps
    // nothing of the card it had.
    for (id, doc) in ids.iter().zip(&left_out) {
        let fetched = pubky_apps::no_card(&doc.url, 86400);
        store::save_document(&pool, id, 0, &fetched).await.unwrap();
        assert_eq!(
            kept(&pool, id).await,
            (None, Some("CreativeWork".into()), None, 0, 0),
            "{}",
            doc.url
        );
        let shown = get(&app, &format!("/v1/resources/{id}")).await;
        assert_eq!(shown["status"], "missing", "{shown}");
        for field in ["name", "description", "image", "point", "facts"] {
            assert!(shown[field].is_null(), "{field} in {shown}");
        }
        let text: String = sqlx::query_scalar(
            "SELECT r.doc::text||coalesce((SELECT string_agg(c::text,' ') FROM resource_change c WHERE c.resource_id=r.id),'') FROM resource r WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        for word in [
            "Excited",
            "lantern",
            "Lakeside",
            "Cobbles",
            "Quiet",
            "Night owl",
            "terrace",
        ] {
            assert!(!text.contains(word), "{word} in {text}");
        }
    }
    assert!(!found(&app, "type=SocialMediaPosting")
        .await
        .contains(&left_out[0].url));
    assert!(found(&app, "q=fountains").await.is_empty());
    // Another app's object keeps its card and stays a search result.
    assert_eq!(
        found(&app, "q=fireworks").await,
        std::slice::from_ref(&other.url),
        "{other_id}"
    );
}

/// A deck whose title, description or topic holds a NUL (which no stored text may hold) keeps
/// its card without it: the card saves, and the link does not dead-letter.
#[test]
fn a_deck_with_a_nul_in_its_text_keeps_its_card_without_it() {
    let doc = edited(
        "loopky/medeng2026a-url-cover",
        json!({"title": "Medical\u{0}English", "description": "Terms\u{0} and phrases",
            "tags": ["anatomy\u{0}"]}),
    );
    assert_eq!(doc.name.as_deref(), Some("MedicalEnglish"));
    assert_eq!(doc.description.as_deref(), Some("Terms and phrases"));
    assert_eq!(doc.attributes["topics"], "anatomy");
    let stored = serde_json::to_string(&doc).unwrap();
    assert!(!stored.contains("\\u0000"), "{stored}");
}
