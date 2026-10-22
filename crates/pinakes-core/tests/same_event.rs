//! The same-event hint: `maybe_same` on event cards names other events that are possibly the same
//! one, never merging anything.
use axum::{body::Body, extract::ConnectInfo, http::Request};
use chrono::{DateTime, Duration, DurationRound, Utc};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, SameEventHint},
    event::{calendar, Organizer},
    model::{Point, ResourceMeta},
    same_event, store,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

/// Wattwil, where the Ostschweiz meetup meets.
const WATTWIL: Point = Point {
    lat: 47.2991,
    lon: 9.0866,
};

/// A start in the window, on the minute.
fn soon() -> DateTime<Utc> {
    (Utc::now() + Duration::days(17))
        .duration_trunc(Duration::minutes(1))
        .unwrap()
}

/// `meters` north of `p`.
fn north(p: &Point, meters: f64) -> Point {
    Point {
        lat: p.lat + meters / 111_320.0,
        lon: p.lon,
    }
}

async fn event(
    pool: &PgPool,
    uri: &str,
    name: &str,
    start: DateTime<Utc>,
    at: Option<Point>,
) -> String {
    let mut doc = ResourceMeta::new(uri, "event_platform", 86400);
    doc.set_type("Event");
    doc.name = Some(name.into());
    doc.start = Some(start);
    doc.location = at;
    save(pool, &doc).await
}

/// Saved and tagged once, so search lists it.
async fn save(pool: &PgPool, doc: &ResourceMeta) -> String {
    let id = store::ensure_resource(pool, &doc.url).await.unwrap();
    store::save_document(pool, &id, 0, doc).await.unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','meetup',$3,now()) ON CONFLICT DO NOTHING")
        .bind(&id)
        .bind("pubkyseedauthorpubkyseedauthorpubkyseedauthorpubkyse")
        .bind(format!("/pub/pubky.app/tags/{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn pass(pool: &PgPool, config: &Config) {
    while same_event::drain(pool, config, 200).await.unwrap() > 0 {}
}

async fn get(config: &Config, pool: &PgPool, path: &str) -> Value {
    let app = api::router(AppState::new(pool.clone(), config.clone()).unwrap());
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200, "{path}");
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn card(pool: &PgPool, id: &str) -> Value {
    get(&Config::default(), pool, &format!("/v1/resources/{id}")).await
}

/// The IDs a card's hint names, best first.
fn named(card: &Value) -> Vec<&str> {
    card["maybe_same"]
        .as_array()
        .unwrap_or_else(|| panic!("no maybe_same: {card}"))
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn one_meetup_on_three_sites_names_the_others(pool: PgPool) {
    let start = soon();
    let meetup = event(
        &pool,
        "https://www.meetup.com/einundzwanzig-bitcoin-meetup-schweiz/events/316480149/",
        "EINUNDZWANZIG Bitcoin Meetup Ostschweiz",
        start,
        Some(WATTWIL),
    )
    .await;
    let luma = event(
        &pool,
        "https://luma.com/einundzwanzig-ostschweiz",
        "Einundzwanzig Bitcoin Ostschweiz – November 🍕",
        start + Duration::minutes(2),
        Some(north(&WATTWIL, 150.0)),
    )
    .await;
    let site = event(
        &pool,
        "https://einundzwanzig-ostschweiz.ch/events/2026-11",
        "Einundzwanzig Ostschweiz Bitcoin Meetup (6.11.)",
        start - Duration::minutes(1),
        Some(north(&WATTWIL, 400.0)),
    )
    .await;
    pass(&pool, &Config::default()).await;
    let c = card(&pool, &meetup).await;
    let mut others = named(&c);
    others.sort();
    let mut want = vec![luma.as_str(), site.as_str()];
    want.sort();
    assert_eq!(others, want);
    for m in c["maybe_same"].as_array().unwrap() {
        let score = m["score"].as_f64().unwrap();
        assert!((0.5..=1.0).contains(&score), "{m}");
        assert!(m["uri"].as_str().unwrap().starts_with("https://"));
        assert!(m["site"].is_string() && m["start"].is_string(), "{m}");
    }
    let scores: Vec<f64> = c["maybe_same"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["score"].as_f64().unwrap())
        .collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "best first");
    assert_eq!(c["details"]["maybe_same"], c["maybe_same"]);
    // Symmetric: each names the meetup.
    for id in [&luma, &site] {
        assert!(named(&card(&pool, id).await).contains(&meetup.as_str()));
    }
    // A hint, never a group: each keeps its own card and no group forms from it.
    assert!(c["group"].is_null());
    // Search cards carry it too.
    let page = get(&Config::default(), &pool, "/v1/search?type=Event").await;
    let row = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == meetup.as_str())
        .unwrap();
    assert_eq!(row["maybe_same"].as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_card_shows_three_and_its_details_ten(pool: PgPool) {
    let start = soon();
    let first = event(
        &pool,
        "https://example.org/e0",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    for n in 1..=11 {
        event(
            &pool,
            &format!("https://example.org/e{n}"),
            "Bitcoin Meetup Wattwil",
            start,
            Some(WATTWIL),
        )
        .await;
    }
    pass(&pool, &Config::default()).await;
    let c = card(&pool, &first).await;
    assert_eq!(c["maybe_same"].as_array().unwrap().len(), 3);
    assert_eq!(c["details"]["maybe_same"].as_array().unwrap().len(), 10);
    // Nothing is counted: no group, and search lists every page.
    let page = get(&Config::default(), &pool, "/v1/search?type=Event").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 12);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn other_times_places_names_and_one_feeds_occurrences_are_not_the_same(pool: PgPool) {
    let start = soon();
    let meetup = event(
        &pool,
        "https://www.meetup.com/zurich-bitcoin/events/1/",
        "Bitcoin Meetup Zürich",
        start,
        Some(Point {
            lat: 47.3769,
            lon: 8.5417,
        }),
    )
    .await;
    let zurich = Point {
        lat: 47.3769,
        lon: 8.5417,
    };
    // The same name two hours later; at the same time 30 km away; another name, same place
    // and time; and a place just over 500 m away.
    for (uri, name, at, place) in [
        (
            "https://luma.com/later",
            "Bitcoin Meetup Zürich",
            start + Duration::hours(2),
            zurich.clone(),
        ),
        (
            "https://luma.com/winterthur",
            "Bitcoin Meetup Zürich",
            start,
            Point {
                lat: 47.4988,
                lon: 8.7237,
            },
        ),
        (
            "https://luma.com/ethereum",
            "Ethereum Developers Zürich",
            start,
            zurich.clone(),
        ),
        (
            "https://luma.com/across-town",
            "Bitcoin Meetup Zürich",
            start,
            north(&zurich, 520.0),
        ),
    ] {
        event(&pool, uri, name, at, Some(place)).await;
    }
    // A weekly meetup's feed at the same place: only its occurrence that starts with the page
    // is possibly the same, and its occurrences never name each other.
    let feed = "https://example.org/zurich-bitcoin.ics";
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:weekly\r\nDTSTART:{}\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Bitcoin Meetup Zürich\r\nGEO:47.3769;8.5417\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        start.format("%Y%m%dT%H%M%SZ")
    );
    let parsed = calendar::parse(feed, ics.as_bytes(), &Default::default(), Utc::now()).unwrap();
    let feed_id = save(&pool, &parsed).await;
    pass(&pool, &Config::default()).await;
    let c = card(&pool, &meetup).await;
    assert_eq!(named(&c), [feed_id.as_str()], "{c}");
    let first = &c["maybe_same"][0];
    assert_eq!(
        first["start"]
            .as_str()
            .unwrap()
            .parse::<DateTime<Utc>>()
            .unwrap(),
        start
    );
    let occurrences = get(
        &Config::default(),
        &pool,
        &format!("/v1/resources/{feed_id}/occurrences?limit=10"),
    )
    .await;
    let items = occurrences["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    for o in items {
        let starts_together = o["start"]
            .as_str()
            .unwrap()
            .parse::<DateTime<Utc>>()
            .unwrap()
            == start;
        let names: Vec<&str> = named(o);
        assert_eq!(
            names,
            if starts_together {
                vec![meetup.as_str()]
            } else {
                vec![]
            },
            "{o}"
        );
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_member_of_the_cards_own_group_is_never_listed(pool: PgPool) {
    let start = soon();
    let a = event(
        &pool,
        "https://example.org/a",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    let b = event(
        &pool,
        "https://example.org/b",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    pass(&pool, &Config::default()).await;
    assert_eq!(named(&card(&pool, &a).await), [b.as_str()]);
    // Once both are one group, the group says it; the hint does not repeat it.
    let group = "0123456789abcdef0123456789abcdef";
    sqlx::query("INSERT INTO \"group\"(id,curated) VALUES($1,true)")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE unit_id=ANY($2)")
        .bind(group)
        .bind(vec![a.clone(), b.clone()])
        .execute(&pool)
        .await
        .unwrap();
    assert!(named(&card(&pool, &a).await).is_empty());
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn all_day_events_match_on_their_local_date(pool: PgPool) {
    use chrono::TimeZone;
    let day = (Utc::now() + Duration::days(20)).date_naive();
    let zurich: chrono_tz::Tz = "Europe/Zurich".parse().unwrap();
    let midnight = zurich
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .unwrap()
        .with_timezone(&Utc);
    let all_day = |uri: &str| {
        let mut doc = ResourceMeta::new(uri, "event_platform", 86400);
        doc.set_type("Event");
        doc.name = Some("Bitcoin Konferenz Wattwil".into());
        doc.start = Some(midnight);
        doc.event.all_day = Some(true);
        doc.event.tz = Some("Europe/Zurich".into());
        doc.location = Some(WATTWIL);
        doc
    };
    let a = save(&pool, &all_day("https://example.org/a")).await;
    let b = save(&pool, &all_day("https://example.org/b")).await;
    // A timed listing at 10:00 local that day is the same day; one the next day is not.
    let timed = event(
        &pool,
        "https://example.org/timed",
        "Bitcoin Konferenz Wattwil",
        midnight + Duration::hours(10),
        Some(WATTWIL),
    )
    .await;
    let next = event(
        &pool,
        "https://example.org/next",
        "Bitcoin Konferenz Wattwil",
        midnight + Duration::hours(34),
        Some(WATTWIL),
    )
    .await;
    pass(&pool, &Config::default()).await;
    let mut got = named(&card(&pool, &a).await)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    got.sort();
    let mut want = vec![b, timed];
    want.sort();
    assert_eq!(got, want);
    assert!(!named(&card(&pool, &a).await).contains(&next.as_str()));
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_same_organizer_or_venue_counts_without_a_place(pool: PgPool) {
    let start = soon();
    let online = |uri: &str, name: &str| {
        let mut doc = ResourceMeta::new(uri, "event_platform", 86400);
        doc.set_type("Event");
        doc.name = Some(name.into());
        doc.start = Some(start);
        doc.event.organizer = Some(Organizer {
            name: "SatoShe".into(),
            url: Some("https://luma.com/SatoShe".into()),
        });
        doc
    };
    let a = save(
        &pool,
        &online("https://luma.com/satoshe-1", "SatoShe Bitcoin Basics"),
    )
    .await;
    let b = save(
        &pool,
        &online("https://satoshe.ch/basics", "SatoShe: Bitcoin Basics!"),
    )
    .await;
    // No place and another organizer: nothing to say where it is.
    let mut c = online("https://example.org/basics", "SatoShe Bitcoin Basics");
    c.event.organizer = None;
    save(&pool, &c).await;
    pass(&pool, &Config::default()).await;
    assert_eq!(named(&card(&pool, &a).await), [b.as_str()]);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_change_or_the_window_moves_the_hint(pool: PgPool) {
    let start = soon();
    let a = event(
        &pool,
        "https://example.org/a",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    let b_uri = "https://example.org/b";
    let b = event(&pool, b_uri, "Bitcoin Meetup Wattwil", start, Some(WATTWIL)).await;
    pass(&pool, &Config::default()).await;
    assert_eq!(named(&card(&pool, &a).await), [b.as_str()]);
    // The other page moves its start: the next pass drops the pair, from both sides.
    event(
        &pool,
        b_uri,
        "Bitcoin Meetup Wattwil",
        start + Duration::hours(3),
        Some(WATTWIL),
    )
    .await;
    pass(&pool, &Config::default()).await;
    assert!(named(&card(&pool, &a).await).is_empty());
    assert!(named(&card(&pool, &b).await).is_empty());
    // And back: the pair returns.
    event(&pool, b_uri, "Bitcoin Meetup Wattwil", start, Some(WATTWIL)).await;
    pass(&pool, &Config::default()).await;
    assert_eq!(named(&card(&pool, &a).await), [b.as_str()]);
    // Events that started more than a day ago, or start more than a year ahead, are never read.
    for (uri, at) in [
        ("https://example.org/past-a", Utc::now() - Duration::days(2)),
        ("https://example.org/past-b", Utc::now() - Duration::days(2)),
        (
            "https://example.org/far-a",
            Utc::now() + Duration::days(400),
        ),
        (
            "https://example.org/far-b",
            Utc::now() + Duration::days(400),
        ),
    ] {
        event(&pool, uri, "Bitcoin Meetup Wattwil", at, Some(WATTWIL)).await;
    }
    pass(&pool, &Config::default()).await;
    let read: i64 = sqlx::query_scalar("SELECT count(*) FROM same_event_unit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(read, 2);
    // A unit that falls out of the window leaves with its pairs at the next sweep.
    sqlx::query("UPDATE same_event_unit SET start_at=now()-interval '2 days' WHERE unit_id=$1")
        .bind(&b)
        .execute(&pool)
        .await
        .unwrap();
    same_event::sweep(&pool).await.unwrap();
    assert!(named(&card(&pool, &a).await).is_empty());
    // The sweep marks the event it no longer holds (b, still in the window), and the pass reads
    // it again.
    pass(&pool, &Config::default()).await;
    assert_eq!(named(&card(&pool, &a).await), [b.as_str()]);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn off_removes_the_field_and_computes_nothing(pool: PgPool) {
    let mut off = Config::default();
    off.event.same_event_hint = SameEventHint::Off;
    let start = soon();
    let a = event(
        &pool,
        "https://example.org/a",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    event(
        &pool,
        "https://example.org/b",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    assert_eq!(same_event::drain(&pool, &off, 200).await.unwrap(), 0);
    let c = get(&off, &pool, &format!("/v1/resources/{a}")).await;
    assert!(c.get("maybe_same").is_none() && c["details"].get("maybe_same").is_none());
    let page = get(&off, &pool, "/v1/search?type=Event").await;
    assert!(page["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r.get("maybe_same").is_none()));
    let doc = get(&off, &pool, "/v1/openapi.json").await;
    let text = doc.to_string();
    assert!(!text.contains("maybe_same") && !text.contains("same-event-"));
    let on = get(&Config::default(), &pool, "/v1/openapi.json").await;
    assert!(on["components"]["schemas"]["Card"]["properties"]["maybe_same"].is_object());
    // An event card with the hint on and nothing found says so with an empty list; a card that
    // is no event has no field.
    let c = card(&pool, &a).await;
    assert_eq!(c["maybe_same"], serde_json::json!([]));
    let mut page = ResourceMeta::new("https://example.org/page", "web", 86400);
    page.set_type("WebPage");
    page.name = Some("Bitcoin Meetup Wattwil".into());
    let page = save(&pool, &page).await;
    assert!(card(&pool, &page).await.get("maybe_same").is_none());
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn names_are_compared_without_dates_filler_and_symbols(pool: PgPool) {
    for (name, want) in [
        (
            "EINUNDZWANZIG Bitcoin Meetup Ostschweiz",
            "einundzwanzig bitcoin ostschweiz",
        ),
        (
            "Bitcoin Meetup Zürich 🍻 #42 — 21.10.2026",
            "bitcoin zürich 42",
        ),
        ("Bitcoin-Stammtisch Zürich, Oktober", "bitcoin zürich"),
        ("Monthly Event: Nostr Night (Nov 6)", "nostr night"),
        ("Bitcoin Meetup Zürich (Oct 21), 19:00", "bitcoin zürich"),
        ("Meetup 2026-11-06 at 7pm", "at"),
        ("Bitcoin Weesen Nr. 30", "bitcoin weesen nr 30"),
        ("Meetup", ""),
    ] {
        let got: String = sqlx::query_scalar("SELECT same_event_name($1)")
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(got, want, "{name}");
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn names_that_keep_different_numbers_are_not_the_same(pool: PgPool) {
    let start = soon();
    let fifty = event(
        &pool,
        "https://example.org/50",
        "Bitcoin Meetup Wattwil #50",
        start,
        Some(WATTWIL),
    )
    .await;
    event(
        &pool,
        "https://example.org/51",
        "Bitcoin Meetup Wattwil #51",
        start,
        Some(WATTWIL),
    )
    .await;
    // A listing without the number may still be the same.
    let plain = event(
        &pool,
        "https://example.org/plain",
        "Bitcoin Meetup Wattwil (6.11.)",
        start,
        Some(WATTWIL),
    )
    .await;
    pass(&pool, &Config::default()).await;
    assert_eq!(named(&card(&pool, &fifty).await), [plain.as_str()]);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn one_organizers_meetups_in_two_cities_are_not_the_same(pool: PgPool) {
    let start = soon();
    let city = |uri: &str, name: &str, at: Point| {
        let mut doc = ResourceMeta::new(uri, "event_platform", 86400);
        doc.set_type("Event");
        doc.name = Some(name.into());
        doc.start = Some(start);
        doc.location = Some(at);
        doc.event.organizer = Some(Organizer {
            name: "Einundzwanzig Schweiz".into(),
            url: Some("https://www.meetup.com/einundzwanzig-bitcoin-meetup-schweiz/".into()),
        });
        doc
    };
    let zurich = save(
        &pool,
        &city(
            "https://www.meetup.com/einundzwanzig-bitcoin-meetup-schweiz/events/1/",
            "EINUNDZWANZIG Bitcoin Meetup Zürich",
            Point {
                lat: 47.3769,
                lon: 8.5417,
            },
        ),
    )
    .await;
    let bern = save(
        &pool,
        &city(
            "https://www.meetup.com/einundzwanzig-bitcoin-meetup-schweiz/events/2/",
            "EINUNDZWANZIG Bitcoin Meetup Bern",
            Point {
                lat: 46.948,
                lon: 7.4474,
            },
        ),
    )
    .await;
    pass(&pool, &Config::default()).await;
    assert!(named(&card(&pool, &zurich).await).is_empty());
    assert!(named(&card(&pool, &bern).await).is_empty());
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_event_that_becomes_an_alias_leaves_the_hint(pool: PgPool) {
    let start = soon();
    let b = event(
        &pool,
        "https://example.org/b",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    let c = event(
        &pool,
        "https://example.org/c",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    pass(&pool, &Config::default()).await;
    assert_eq!(named(&card(&pool, &c).await), [b.as_str()]);
    // B turns out to be a spelling of A: C stops naming it at once, and the sweep drops its unit.
    let a = event(
        &pool,
        "https://example.org/a",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&b)
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!named(&card(&pool, &c).await).contains(&b.as_str()));
    same_event::sweep(&pool).await.unwrap();
    pass(&pool, &Config::default()).await;
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM same_event_unit WHERE resource_id=$1")
        .bind(&b)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(held, 0);
    assert_eq!(named(&card(&pool, &c).await), [a.as_str()]);
}

#[test]
fn an_api_feed_is_named_by_a_page_a_person_can_open() {
    use pinakes_core::event::adapters::cursor::human_page;
    for (feed, page) in [
        (
            "https://portal.einundzwanzig.space/api/changes",
            Some("https://portal.einundzwanzig.space/"),
        ),
        (
            "https://api.btcmap.org/v4/events",
            Some("https://btcmap.org/"),
        ),
        (
            "https://www.googleapis.com/calendar/v3/calendars/a%40group.calendar.google.com/events",
            Some("https://calendar.google.com/calendar/embed?src=a%40group.calendar.google.com"),
        ),
        ("https://example.org/meetups.ics", None),
    ] {
        assert_eq!(human_page(feed).as_deref(), page, "{feed}");
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn events_stored_before_the_hint_are_read_at_once(pool: PgPool) {
    let start = soon();
    let mut ids = vec![];
    for n in 0..3 {
        ids.push(
            event(
                &pool,
                &format!("https://example.org/{n}"),
                "Bitcoin Meetup Wattwil",
                start,
                Some(WATTWIL),
            )
            .await,
        );
    }
    // As if nothing had marked them: the sweep finds them.
    sqlx::query("DELETE FROM same_event_dirty")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(same_event::sweep(&pool).await.unwrap(), 3);
    pass(&pool, &Config::default()).await;
    assert_eq!(named(&card(&pool, &ids[0]).await).len(), 2);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_search_page_changes_its_etag_when_its_hint_does(pool: PgPool) {
    let start = soon();
    for n in 0..2 {
        event(
            &pool,
            &format!("https://example.org/{n}"),
            "Bitcoin Meetup Wattwil",
            start,
            Some(WATTWIL),
        )
        .await;
    }
    let etag = || async {
        let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
        let mut request = Request::get("/v1/search?type=Event")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = app.oneshot(request).await.unwrap();
        response.headers()["etag"].to_str().unwrap().to_owned()
    };
    let before = etag().await;
    pass(&pool, &Config::default()).await;
    assert_ne!(etag().await, before);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn map_points_keep_their_group_with_the_hint_on_or_off(pool: PgPool) {
    let start = soon();
    let a = event(
        &pool,
        "https://example.org/a",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    let b = event(
        &pool,
        "https://example.org/b",
        "Bitcoin Meetup Wattwil",
        start,
        Some(WATTWIL),
    )
    .await;
    pass(&pool, &Config::default()).await;
    let group = "0123456789abcdef0123456789abcdef";
    sqlx::query("INSERT INTO \"group\"(id,curated) VALUES($1,true)")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE unit_id=ANY($2)")
        .bind(group)
        .bind(vec![a.clone(), b.clone()])
        .execute(&pool)
        .await
        .unwrap();
    let mut off = Config::default();
    off.event.same_event_hint = SameEventHint::Off;
    for config in [Config::default(), off] {
        let map = get(
            &config,
            &pool,
            "/v1/search?bbox=9.0,47.2,9.2,47.4&cluster=12",
        )
        .await;
        let points = map["points"].as_array().unwrap();
        assert_eq!(points.len(), 2, "{map}");
        for p in points {
            assert_eq!(p["group"]["id"], group, "{p}");
            let on = config.event.same_event_hint == SameEventHint::On;
            // On: the hint is there, and never names a member of the point's own group.
            assert_eq!(p.get("maybe_same").is_some(), on, "{p}");
            if on {
                assert!(named(p).is_empty(), "{p}");
            }
        }
    }
}
