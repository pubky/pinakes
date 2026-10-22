//! Pubky app profiles (`resolver::pubky_apps`): each app's recorded objects, the general card for
//! other apps, paths that are never fetched, and hostile objects.
use axum::{body::Body, extract::ConnectInfo, http::Request};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    ingest,
    model::{identity, ResourceMeta, Status},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    resolver::{classify, pubky_apps},
    store,
    telemetry::Metrics,
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
fn marketplace() {
    let shop = "rxtcr8ezoja67qq3wcrxtueefpzaau65tr8nf5o8d8e8oc7a33jo";
    let doc = card("marketplace/shop");
    assert_eq!(doc.type_name, "OnlineStore");
    assert_eq!(doc.name.as_deref(), Some("Dezentralshop"));
    assert_eq!(doc.description.as_deref(), Some("Your Swiss Bitcoin store"));
    assert_eq!(
        doc.image,
        Some(format!(
            "pubky://{shop}/pub/pubky.app/marketplace/v1/media/2970e7ce535d4a39ae1ecc7fcdf39ece"
        ))
    );
    assert_eq!(doc.site, "pubky.app");
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
    let doc = auction(Utc::now() + Duration::days(1));
    assert_eq!(facts(&doc)["price"], "0.00001000 BTC");
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
        "pubky.app/marketplace/v1/media/2970e7ce535d4a39ae1ecc7fcdf39ece",
        "pubky.app/marketplace/v1/reviews/0035SFYCGVBV2",
        "pubky.app/marketplace/v1/deliverables/a/1",
        "pubky.app/marketplace/v1/delivery-keys/current",
        "pubky.app/marketplace/v1/session-proofs/x",
        "pubky.app/marketplace/v2/listings/x",
        "pubky.app/marketplace-service/v1/x",
        "pubky.app/posts/0034A0X7NJ52G",
    ]);
}

#[test]
fn mapky() {
    let owner = "c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey";
    let doc = card("mapky/route-giggelitrain");
    assert_eq!(doc.type_name, "Trip");
    assert_eq!(doc.name.as_deref(), Some("Giggelitrain"));
    let point = doc.location.as_ref().unwrap();
    assert_eq!(
        (point.lat, point.lon),
        (47.042543417729036, 8.289489434862617)
    );
    assert_eq!(
        facts(&doc),
        BTreeMap::from([
            ("activity".into(), "cycling".into()),
            ("distance_m".into(), "753".into()),
            ("duration_s".into(), "190".into()),
        ])
    );
    assert_eq!(
        links(&doc),
        link(
            "Mapky",
            &format!("https://mapky.app/route/{owner}/00351MV7WF0WG")
        )
    );
    assert_eq!(card("mapky/route-merlischachen-luzern").description, None);

    // A review: its rating and the place's page.
    let doc = card("mapky/review-with-content");
    assert_eq!(doc.type_name, "Review");
    assert_eq!(doc.description.as_deref(), Some("Great beer for sats!"));
    assert_eq!(
        facts(&doc),
        BTreeMap::from([
            ("rating".into(), "10".into()),
            (
                "place".into(),
                "https://www.openstreetmap.org/node/1573053883".into()
            ),
        ])
    );
    assert_eq!(
        links(&doc),
        link("Mapky", "https://mapky.app/place/node/1573053883")
    );
    let doc = card("mapky/review-rating-only-way");
    assert_eq!(
        links(&doc),
        link("Mapky", "https://mapky.app/place/way/135207248")
    );
    no_card(&edited("mapky/review-with-content", json!({"rating": 11})));

    // Posts: social content about a place; older posts name the place themselves, and an older
    // review post is a review.
    let doc = card("mapky/post-short-osm-parent");
    assert_eq!(doc.type_name, "SocialMediaPosting");
    assert!(pinakes_core::resolver::cards::social(&doc));
    assert_eq!(doc.description.as_deref(), Some("Amazing food"));
    assert_eq!(
        links(&doc),
        link("Mapky", "https://mapky.app/place/node/341342368")
    );
    let doc = card("mapky/legacy-post-review-place-object");
    assert_eq!(
        (doc.type_name.as_str(), facts(&doc)["rating"].as_str()),
        ("Review", "10")
    );
    assert_eq!(
        links(&doc),
        link("Mapky", "https://mapky.app/place/node/5651014955")
    );
    let doc = card("mapky/legacy-post-review-place-url");
    assert_eq!(
        links(&doc),
        link("Mapky", "https://mapky.app/place/node/1573053883")
    );
    let doc = card("mapky/legacy-post-kind-post");
    assert_eq!(doc.type_name, "SocialMediaPosting");
    assert_eq!(
        links(&doc),
        link("Mapky", "https://mapky.app/place/node/2735447242")
    );
    // A reply to a route opens the route.
    let post = format!("pubky://{owner}/pub/mapky.app/posts/0035C8FNKDQS0");
    let doc = card_of(
        &post,
        json!({"content": "Nice ride", "kind": "short",
            "parent": format!("pubky://{owner}/pub/mapky.app/routes/00351MV7WF0WG")})
        .to_string()
        .as_bytes(),
    );
    assert_eq!(
        links(&doc),
        link(
            "Mapky",
            &format!("https://mapky.app/route/{owner}/00351MV7WF0WG")
        )
    );
    // A collection post, and the older collection objects.
    let doc = card_of(
        &post,
        json!({"kind": "collection",
            "content": json!({"name": "Lakes", "items": ["https://www.openstreetmap.org/way/1"]}).to_string()})
        .to_string()
        .as_bytes(),
    );
    assert_eq!(
        (doc.type_name.as_str(), doc.name.as_deref()),
        ("Collection", Some("Lakes"))
    );
    let doc = card("mapky/legacy-collection-color");
    assert_eq!(doc.type_name, "Collection");
    assert_eq!(doc.name.as_deref(), Some("Test"));
    assert_eq!(facts(&doc)["items"], "1");
    assert!(doc.open_in.is_empty());
    assert_eq!(
        card("mapky/legacy-collection-empty").description.as_deref(),
        Some("testdesc")
    );

    // An incident: until it expires.
    no_card(&card("mapky/incident-hazard-medium"));
    let incident = format!("pubky://{owner}/pub/mapky.app/incidents/00357W2047V80");
    let expires = (Utc::now() + Duration::hours(2)).timestamp_micros();
    let doc = card_of(
        &incident,
        json!({"incident_type": "hazard", "severity": "high", "lat": 47.05, "lon": 8.33,
            "description": "Tree on the road", "expires_at": expires})
        .to_string()
        .as_bytes(),
    );
    assert_eq!(doc.type_name, "SpecialAnnouncement");
    assert_eq!(facts(&doc)["incident_type"], "hazard");
    assert!(doc.location.is_some());
    assert_eq!(
        links(&doc),
        link(
            "Mapky",
            &format!("https://mapky.app/incident/{owner}/00357W2047V80")
        )
    );
    // A capture (none recorded: the public ones are at someone's home) and a sequence.
    let capture = format!("pubky://{owner}/pub/mapky.app/geo_captures/0035C4V0GVX90");
    let doc = card_of(
        &capture,
        json!({"file_uri": format!("pubky://{owner}/pub/mapky.app/files/0035C4V0GVX90"),
            "kind": "photo", "lat": 46.95, "lon": 7.44, "caption": "Bear pit"})
        .to_string()
        .as_bytes(),
    );
    assert_eq!(doc.type_name, "Photograph");
    assert_eq!(doc.description.as_deref(), Some("Bear pit"));
    assert_eq!(
        links(&doc),
        link(
            "Mapky",
            &format!("https://mapky.app/capture/{owner}/0035C4V0GVX90")
        )
    );
    no_card(&card_of(&capture, br#"{"kind":"photo","lat":95,"lon":7}"#));
    let sequence = format!("pubky://{owner}/pub/mapky.app/sequences/0035C4V0GVX91");
    let doc = card_of(&sequence, br#"{"name":"Old town walk","capture_count":12}"#);
    assert_eq!(doc.type_name, "ImageGallery");
    never_fetched(&[
        "mapky.app/files/0035C4V0GVX90",
        "mapky.app/blobs/JVYD",
        "mapky.app/tags/0035C4V0GVX90",
    ]);
}

#[test]
fn eventky() {
    let owner = "51da3n5m8s6oaq38uqs7jznp6ezbc3qbtmic8oy6fj3g6mokdyco";
    let doc = card("eventky/event-psywalk-location-no-uri");
    assert_eq!(doc.type_name, "Event");
    assert_eq!(doc.name.as_deref(), Some("₿itcoin ₽sy₩alk"));
    assert!(doc
        .description
        .as_ref()
        .unwrap()
        .starts_with("Bitcoiner stampfen"));
    // Local times in Zurich, summer time.
    assert_eq!(doc.start.unwrap().to_rfc3339(), "2026-07-31T16:00:00+00:00");
    assert_eq!(doc.end.unwrap().to_rfc3339(), "2026-07-31T21:59:00+00:00");
    assert_eq!(
        facts(&doc)["location"],
        "Moonlight Festzelt, Niederried bei Kallnach"
    );
    assert_eq!(
        links(&doc),
        link(
            "Eventky",
            &format!("https://eventky.app/event/{owner}/0035DS0K1ZTEG")
        )
    );
    // The text of an HTML-only description; an end from a duration.
    let (uri, body) = fixture("eventky/event-bern-meetup-recurring");
    let new_year: chrono::DateTime<chrono::Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
    let doc = pubky_apps::card_at(&uri, "application/octet-stream", &body, 86400, new_year);
    let text = doc.description.as_deref().unwrap();
    assert!(
        text.contains("Monthly Bitcoin Meetup") && !text.contains("<p>"),
        "{text}"
    );
    assert_eq!(doc.start.unwrap().to_rfc3339(), "2026-01-08T18:00:14+00:00");
    assert_eq!(doc.end.unwrap().to_rfc3339(), "2026-01-08T21:00:14+00:00");
    assert_eq!(facts(&doc)["location"], "Department 66");
    // HTML of any depth is read in one pass: 100,000 nested elements are plain text.
    let deep = format!(
        "{}deep{}",
        "<span><b>".repeat(100_000),
        "</b></span>".repeat(100_000)
    );
    let doc = edited(
        "eventky/event-psywalk-location-no-uri",
        json!({"description": null, "styled_description": {"format": "html", "content": deep}}),
    );
    assert_eq!(doc.description.as_deref(), Some("deep"));
    for (html, text) in [
        ("<p>One &amp; two</p><p>3 < 4</p>", "One & two\n3 < 4"),
        (
            "a<br>b<!-- x > y -->c<script>alert('<p>')</script>d",
            "a\nbcd",
        ),
        ("<style>p{}</style><li>x</li><li>y", "x\ny"),
    ] {
        assert_eq!(
            pubky_apps::read::html_text(html).as_deref(),
            Some(text),
            "{html}"
        );
    }
    assert_eq!(pubky_apps::read::html_text("<p> </p><unterminated"), None);
    // A series is shown, and fetched again, by its next occurrence: the monthly meetup's first
    // was in January, its next after 22 October is the second Tuesday of November, 19:00:14 in
    // Zurich. A finished series, or a rule that does not read, keeps its first occurrence.
    let now: chrono::DateTime<chrono::Utc> = "2026-10-22T12:00:00Z".parse().unwrap();
    let at = |name: &str, change: Value| {
        let (uri, body) = fixture(name);
        let mut v: Value = serde_json::from_slice(&body).unwrap();
        for (k, value) in change.as_object().unwrap() {
            v[k] = value.clone();
        }
        pubky_apps::card_at(
            &uri,
            "application/octet-stream",
            v.to_string().as_bytes(),
            86400,
            now,
        )
    };
    let doc = at("eventky/event-bern-meetup-recurring", json!({}));
    assert_eq!(doc.start.unwrap().to_rfc3339(), "2026-11-10T18:00:14+00:00");
    assert_eq!(doc.end.unwrap().to_rfc3339(), "2026-11-10T21:00:14+00:00");
    let refresh = Config::default().refresh;
    let pubky = pinakes_core::adapter::manifest("pubky");
    assert!(pinakes_core::scheduler::next(&pubky, &doc, now, &refresh).is_some());
    // During an occurrence, that one.
    let during: chrono::DateTime<chrono::Utc> = "2026-11-10T19:00:00Z".parse().unwrap();
    let (uri, body) = fixture("eventky/event-bern-meetup-recurring");
    let doc = pubky_apps::card_at(&uri, "application/octet-stream", &body, 86400, during);
    assert_eq!(doc.start.unwrap().to_rfc3339(), "2026-11-10T18:00:14+00:00");
    for rule in [
        "FREQ=MONTHLY;BYDAY=TU;BYSETPOS=2;UNTIL=20260301T000000Z",
        "FREQ=SOMETIMES",
        "FREQ=DAILY\nRDATE:20261101T000000Z",
    ] {
        let doc = at(
            "eventky/event-bern-meetup-recurring",
            json!({"rrule": rule}),
        );
        assert_eq!(
            doc.start.unwrap().to_rfc3339(),
            "2026-01-08T18:00:14+00:00",
            "{rule}"
        );
        assert_eq!(
            pinakes_core::scheduler::next(&pubky, &doc, now, &refresh),
            None,
            "{rule}"
        );
    }
    // A duration past the calendar's end gives no end; the card keeps its start.
    let doc = edited(
        "eventky/event-psywalk-location-no-uri",
        json!({"dtend": null, "duration": "P1000000000D"}),
    );
    assert_eq!(doc.status, Status::Ok);
    assert_eq!(doc.start.unwrap().to_rfc3339(), "2026-07-31T16:00:00+00:00");
    assert_eq!(doc.end, None);
    // A time without a zone is left out.
    let doc = edited(
        "eventky/event-psywalk-location-no-uri",
        json!({"dtstart_tzid": null}),
    );
    assert_eq!((doc.start, doc.end), (None, None));

    let doc = card("eventky/calendar-dezentralschweiz");
    assert_eq!(doc.type_name, "Collection");
    assert_eq!(doc.name.as_deref(), Some("Dezentralschweiz"));
    assert_eq!(facts(&doc)["timezone"], "Europe/Zurich");
    assert_eq!(
        links(&doc),
        link(
            "Eventky",
            &format!("https://eventky.app/calendar/{owner}/0034PXJ40MGQG")
        )
    );
    assert_eq!(card("eventky/calendar-minimal").description, None);
    no_card(&card("eventky/invalid-calendar-wbg-ptr"));
    never_fetched(&[
        "eventky.app/attendees/51da3n5m-07NX3HM0D2SMPS7KBGN1AK9RGW",
        "eventky.app/tags/07NX3HM0D2SMPS7KBGN1AK9RGW",
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
    never_fetched(&["mapky.app/settings.json", "pubky.app/profile.json"]);
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

    // An Eventky event a Nexus event reference names: its event card opens the event.
    let venue = "geo:47.37,8.54";
    let (_, venue_uri, _) = identity(venue).unwrap();
    let author = "51da3n5m8s6oaq38uqs7jznp6ezbc3qbtmic8oy6fj3g6mokdyco";
    let cursor = ingest::cursor(&pool, "events").await.unwrap();
    let start = Utc::now() + Duration::days(3);
    let line = Change {
        op: Op::Put,
        uri: venue_uri,
        kind: Kind::Event,
        author: author.into(),
        object_path: "/pub/eventky.app/events/0035DS0K1ZTEG".into(),
        label: None,
        at: Utc::now().timestamp_millis(),
        recurrence_id: None,
        start: Some(start.timestamp_millis()),
        end: Some((start + Duration::hours(2)).timestamp_millis()),
        status: None,
    };
    ingest::apply(
        &pool,
        "events",
        &cursor,
        &FeedPage {
            page: Changes {
                lines: vec![line],
                next_cursor: cursor.cursor + 1,
            },
            epoch: Some("1".into()),
            length: cursor.cursor + 1,
        },
        &Config::default().ingest,
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap();
    let found = get(&app, "/v1/search?type=Event").await;
    let event = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["event"]["author"] == author)
        .unwrap_or_else(|| panic!("{found}"));
    assert_eq!(
        event["open_in"],
        json!([{"client": "Eventky", "url": format!("https://eventky.app/event/{author}/0035DS0K1ZTEG")}])
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
        // Withdrawn by its owner: missing once a second fetch confirms it, with nothing of the
        // card it had.
        let mut v: Value = serde_json::from_slice(&body).unwrap();
        for (k, value) in change.as_object().unwrap() {
            v[k] = value.clone();
        }
        let mut gone = card_of(&uri, v.to_string().as_bytes());
        assert_eq!(gone.status, Status::Missing);
        for _ in 0..2 {
            doc.fetched_at += 1000;
            gone.fetched_at = doc.fetched_at;
            store::save_document(&pool, &id, 0, &gone).await.unwrap();
        }
        let shown = get(&app, &format!("/v1/resources/{id}")).await;
        assert_eq!(shown["status"], "missing", "{shown}");
        for field in ["name", "description", "image", "point"] {
            assert!(shown[field].is_null(), "{name}: {field} in {shown}");
        }
        assert_eq!(shown["type"], "CreativeWork", "{shown}");
        assert_eq!(shown["open_in"], json!([]), "{shown}");
        assert!(shown["details"]["facts"].is_null(), "{shown}");
    }
}
