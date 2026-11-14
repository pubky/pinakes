//! Meetup date pages and their group's calendar, read from trimmed recordings
//! (`fixtures/events/meetup`): a date page queues its group's calendar, joins its own date there
//! in the corroborated mode, and its number and token spellings are one resource.
use axum::{body::Body, http::Request};
use chrono::{Datelike, Duration, NaiveDate, Utc};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, EventConfig, QueueConfig},
    event::adapters,
    groups::{self, CrossContainerOccurrences},
    model::{Point, ResourceMeta},
    queue,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
mod common;

const EVENTS: &str = "https://www.meetup.com/bitcoin-meetup-switzerland/events";
const UID: &str = "event_316908442@meetup.com";

fn url(path: &str) -> String {
    format!("{EVENTS}/{path}")
}
fn numeric() -> String {
    url("316908442/")
}
fn token() -> String {
    url("frxnbvyjcqbmb/")
}
fn feed() -> String {
    url("ical/")
}

/// Whole 52-week steps that move the recordings' first date (2026-11-25) past tomorrow: every
/// date keeps its weekday and its season, so the pages' winter offsets still name the same
/// instants as the calendar's `Europe/Zurich` times.
fn shift_days() -> i64 {
    let first = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
    let tomorrow = (Utc::now() + Duration::days(1)).date_naive();
    let mut days = 0;
    while first + Duration::days(days) <= tomorrow {
        days += 364;
    }
    days
}
/// `text` with every date of the forms `2026-12-09T` and `20261209T` moved by `days`.
fn shifted(text: &str, days: i64) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    while i < bytes.len() {
        let rest = &bytes[i..];
        let date = if rest.len() >= 11
            && rest.starts_with(b"20")
            && digits(&rest[..4])
            && rest[4] == b'-'
            && digits(&rest[5..7])
            && rest[7] == b'-'
            && digits(&rest[8..10])
            && rest[10] == b'T'
        {
            NaiveDate::parse_from_str(&text[i..i + 10], "%Y-%m-%d")
                .ok()
                .map(|d| (d, 10, "%Y-%m-%d"))
        } else if rest.len() >= 9
            && rest.starts_with(b"20")
            && digits(&rest[..8])
            && rest[8] == b'T'
            && (i == 0 || !bytes[i - 1].is_ascii_digit())
        {
            NaiveDate::parse_from_str(&text[i..i + 8], "%Y%m%d")
                .ok()
                .map(|d| (d, 8, "%Y%m%d"))
        } else {
            None
        };
        if let Some((d, len, form)) = date {
            out.push_str(&(d + Duration::days(days)).format(form).to_string());
            i += len;
        } else {
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}
fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/events/meetup")
        .join(name);
    shifted(&std::fs::read_to_string(path).unwrap(), shift_days())
}
/// The recorded site: each date's page under its number and its token (Meetup serves the same
/// page under both), its calendar file under each, and the group's calendar.
fn site() -> BTreeMap<String, String> {
    let mut pages = BTreeMap::new();
    for (number, token) in [
        ("316908442", "frxnbvyjcqbmb"),
        ("316908445", "frxnbvykccbjb"),
    ] {
        let page = if number == "316908442" {
            "frxnbvyjcqbmb.html"
        } else {
            "316908445.html"
        };
        pages.insert(format!("{number}/"), fixture(&format!("{number}.html")));
        pages.insert(format!("{token}/"), fixture(page));
        let ics = fixture(&format!("{number}.ics"));
        pages.insert(format!("{number}/ical/{number}.ics"), ics.clone());
        pages.insert(format!("{token}/ical/{token}.ics"), ics);
    }
    pages.insert("ical/".into(), fixture("group.ics"));
    pages
        .into_iter()
        .map(|(path, body)| {
            (
                format!("www.meetup.com/bitcoin-meetup-switzerland/events/{path}"),
                body,
            )
        })
        .collect()
}

struct AnyDns;
#[async_trait::async_trait]
impl Dns for AnyDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("93.184.216.34".parse()?, port)])
    }
}
/// Answers each `host/path` from its map (404 otherwise) and records what was asked.
struct Site {
    pages: BTreeMap<String, String>,
    seen: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl Transport for Site {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &Default::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen.lock().unwrap().push(url.to_string());
        let page = self
            .pages
            .get(&format!("{}{}", url.host_str().unwrap_or(""), url.path()));
        let content_type = match page {
            Some(p) if p.starts_with("BEGIN:VCALENDAR") => "text/calendar",
            _ => "text/html; charset=utf-8",
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            validators: Default::default(),
            content_type: content_type.into(),
            status: if page.is_some() { 200 } else { 404 },
            retry_after_secs: 60,
            location: None,
            body: page.cloned().unwrap_or_default().into_bytes(),
        })
    }
}
fn resolver(
    pool: &PgPool,
    pages: BTreeMap<String, String>,
) -> (adapters::service::PlatformResolver, Arc<Site>) {
    let site = Arc::new(Site {
        pages,
        seen: Mutex::default(),
    });
    let config = EventConfig {
        geocoding: false,
        months_back: 12,
        months_forward: 36,
        ..Default::default()
    };
    let r = adapters::service::PlatformResolver {
        web: pinakes_core::resolver::web::WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress: Egress::with_backends(
                common::unpaced_egress_config(),
                Arc::new(AnyDns),
                site.clone(),
            ),
            config: Default::default(),
            events: Some(Arc::new(pinakes_core::event::service::EventService {
                pool: pool.clone(),
                config,
                osm: Arc::new(
                    pinakes_core::resolver::osm::OsmResolver::new(pool.clone(), Default::default())
                        .unwrap(),
                ),
            })),
        },
    };
    (r, site)
}
fn queue_config(mode: CrossContainerOccurrences) -> QueueConfig {
    QueueConfig {
        groups: groups::GroupConfig {
            cross_container_occurrences: mode,
            ..Default::default()
        },
        ..Default::default()
    }
}
/// One read of `uri` through the queue, as the daemon runs it; its resource ID.
async fn read(
    pool: &PgPool,
    r: &adapters::service::PlatformResolver,
    cfg: &QueueConfig,
    uri: &str,
) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(pool)
        .await
        .unwrap();
    let job = queue::Job {
        id: id.clone(),
        uri: uri.into(),
        attempts: 0,
        generation,
    };
    queue::run_job(pool, r, cfg, &job).await.unwrap();
    id
}
async fn get(app: &axum::Router, path: &str) -> Value {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&bytes));
    let value = serde_json::from_slice(&bytes).unwrap();
    common::check_response("GET", path, status.as_u16(), &value);
    value
}
async fn by_uri(app: &axum::Router, uri: &str) -> Value {
    let q: String = url::form_urlencoded::byte_serialize(uri.as_bytes()).collect();
    get(app, &format!("/v1/resources/by-uri?uri={q}")).await
}
fn app(pool: &PgPool, mode: CrossContainerOccurrences) -> axum::Router {
    let mut config = Config::default();
    config.groups.cross_container_occurrences = mode;
    api::router(AppState::new(pool.clone(), config).unwrap())
}
/// Every group: its members' resource IDs and occurrence UIDs (`""` for a page), sorted.
async fn all_groups(pool: &PgPool) -> Vec<Vec<(String, String)>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT group_id,resource_id,coalesce(occurrence->>'uid','') FROM group_unit WHERE group_id IS NOT NULL ORDER BY 1,2,3",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (group, resource, uid) in rows {
        groups.entry(group).or_default().push((resource, uid));
    }
    groups.into_values().collect()
}
async fn alias_root(pool: &PgPool, id: &str) -> Option<(String, String)> {
    sqlx::query_as("SELECT root_id,basis FROM resource_alias WHERE alias_id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// A date page read by its number: its token spelling (the page's canonical) becomes its root,
/// the root's own calendar file gives it the date's UID, and the group's calendar is queued as a
/// resource derived from each spelling read. In the corroborated mode the page joins exactly
/// its own date in that calendar, and a client gets from the page's card to every date in two
/// reads: the group, whose occurrence member names the feed, then the feed's occurrences.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_date_page_joins_its_own_date_in_its_groups_calendar(pool: PgPool) {
    let (r, site) = resolver(&pool, site());
    let cfg = queue_config(CrossContainerOccurrences::Corroborated);
    let numeric_id = read(&pool, &r, &cfg, &numeric()).await;
    let token_id = store::ensure_resource(&pool, &token()).await.unwrap();
    assert_eq!(
        alias_root(&pool, &numeric_id).await,
        Some((token_id.clone(), "canonical".into()))
    );
    let feed_id = store::ensure_resource(&pool, &feed()).await.unwrap();
    let derived = |pool: PgPool| async move {
        sqlx::query_as::<_, (String, String)>(
            "SELECT parent_id,adapter FROM resource_derivation WHERE child_id=$1 AND expires_at>now() ORDER BY parent_id",
        )
        .bind(store::ensure_resource(&pool, &feed()).await.unwrap())
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    assert_eq!(
        derived(pool.clone()).await,
        vec![(numeric_id.clone(), "meetup".into())]
    );
    assert!(
        site.seen
            .lock()
            .unwrap()
            .contains(&url("316908442/ical/316908442.ics")),
        "the page's own calendar file is read for its UID"
    );
    // The root is read in its place, by its token; its own calendar file names the same UID.
    read(&pool, &r, &cfg, &token()).await;
    let doc: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&token_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        doc["group_keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["key"] == format!("ics_uid:{UID}") && k["basis"] == "ics_uid"),
        "{doc}"
    );
    let mut parents = vec![
        (numeric_id.clone(), "meetup".to_owned()),
        (token_id.clone(), "meetup".to_owned()),
    ];
    parents.sort();
    assert_eq!(derived(pool.clone()).await, parents);
    read(&pool, &r, &cfg, &feed()).await;
    common::drain_groups(&pool, &cfg.groups).await;
    assert_eq!(
        all_groups(&pool).await,
        vec![{
            let mut g = vec![
                (feed_id.clone(), UID.to_owned()),
                (token_id.clone(), "".into()),
            ];
            g.sort();
            g
        }]
    );

    let app = app(&pool, CrossContainerOccurrences::Corroborated);
    let card = by_uri(&app, &numeric()).await;
    assert_eq!(card["id"], token_id.as_str(), "{card}");
    assert_eq!(card["uri"], token().as_str());
    assert_eq!(by_uri(&app, &token()).await["id"], token_id.as_str());
    assert_eq!(card["group"]["size"], 2, "{card}");
    // Read one: the group. Its occurrence member names the feed.
    let group = get(
        &app,
        &format!("/v1/groups/{}", card["group"]["id"].as_str().unwrap()),
    )
    .await;
    let members = group["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{group}");
    let occurrence = members
        .iter()
        .find(|m| m["occurrence"].is_object())
        .unwrap();
    assert_eq!(occurrence["occurrence"]["feed_id"], feed_id.as_str());
    assert_eq!(occurrence["occurrence"]["feed_uri"], feed().as_str());
    assert_eq!(occurrence["occurrence"]["uid"], UID);
    assert_eq!(occurrence["start"], card["start"]);
    // Read two: every date of the feed.
    let items = get(&app, &format!("/v1/resources/{feed_id}/occurrences")).await["items"].clone();
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 10, "{items:?}");
    let joined: Vec<&Value> = items.iter().filter(|o| o["group"].is_object()).collect();
    assert_eq!(joined.len(), 1, "only the page's own date: {items:?}");
    assert_eq!(joined[0]["occurrence"]["uid"], UID);
}

/// Without the corroborated mode, the page and the feed's occurrence stay apart.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_date_page_stays_apart_from_its_calendar_when_the_mode_is_off(pool: PgPool) {
    let (r, _) = resolver(&pool, site());
    let cfg = queue_config(CrossContainerOccurrences::Off);
    read(&pool, &r, &cfg, &numeric()).await;
    read(&pool, &r, &cfg, &token()).await;
    read(&pool, &r, &cfg, &feed()).await;
    common::drain_groups(&pool, &cfg.groups).await;
    assert_eq!(all_groups(&pool).await, Vec::<Vec<(String, String)>>::new());
    let card = by_uri(&app(&pool, CrossContainerOccurrences::Off), &numeric()).await;
    assert!(card["group"].is_null(), "{card}");
}

/// A group's calendar mixes its series: a second series on the same evenings, under its own
/// UIDs, never joins a date page, and no two dates of one feed join each other. Each page joins
/// only its own date.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_calendar_with_two_series_joins_each_page_to_its_own_date_only(pool: PgPool) {
    let mut pages = site();
    let key = "www.meetup.com/bitcoin-meetup-switzerland/events/ical/".to_owned();
    let calendar = pages[&key].clone();
    let days = shift_days();
    let other: String = [("20261209", "900000001"), ("20270106", "900000002")]
        .iter()
        .map(|(date, id)| {
            shifted(
                &format!(
                    "BEGIN:VEVENT\r\nUID:event_{id}@meetup.com\r\nDTSTART;TZID=Europe/Zurich:{date}T190000\r\nDTEND;TZID=Europe/Zurich:{date}T210000\r\nSUMMARY:Lightning workshop\r\nURL;VALUE=URI:{EVENTS}/{id}/\r\nSTATUS:CONFIRMED\r\nEND:VEVENT\r\n"
                ),
                days,
            )
        })
        .collect();
    pages.insert(
        key,
        calendar.replacen("END:VCALENDAR", &format!("{other}END:VCALENDAR"), 1),
    );
    let (r, _) = resolver(&pool, pages);
    let cfg = queue_config(CrossContainerOccurrences::Corroborated);
    for uri in [
        numeric(),
        token(),
        url("316908445/"),
        url("frxnbvykccbjb/"),
        feed(),
    ] {
        read(&pool, &r, &cfg, &uri).await;
    }
    common::drain_groups(&pool, &cfg.groups).await;
    let feed_id = store::ensure_resource(&pool, &feed()).await.unwrap();
    let occurrences: i64 =
        sqlx::query_scalar("SELECT count(*) FROM event_occurrence WHERE resource_id=$1")
            .bind(&feed_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(occurrences, 12);
    let mut expected: Vec<Vec<(String, String)>> = vec![];
    for (page, uid) in [
        (token(), UID),
        (url("frxnbvykccbjb/"), "event_316908445@meetup.com"),
    ] {
        let mut g = vec![
            (feed_id.clone(), uid.to_owned()),
            (
                store::ensure_resource(&pool, &page).await.unwrap(),
                "".into(),
            ),
        ];
        g.sort();
        expected.push(g);
    }
    let mut found = all_groups(&pool).await;
    found.sort();
    expected.sort();
    assert_eq!(found, expected);
}

/// A page of another publisher whose own calendar file carries a Meetup date's UID does not
/// join that date: across publishers the corroborated mode needs both located within 500 m,
/// and the group's calendar gives no place.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn another_publishers_page_with_the_uid_does_not_join(pool: PgPool) {
    let (r, _) = resolver(&pool, site());
    let cfg = queue_config(CrossContainerOccurrences::Corroborated);
    read(&pool, &r, &cfg, &feed()).await;
    let start: chrono::DateTime<Utc> = sqlx::query_scalar(
        "SELECT o.start_at FROM event_occurrence o JOIN resource r ON r.id=o.resource_id WHERE r.uri=$1 AND o.uid=$2",
    )
    .bind(feed())
    .bind(UID)
    .fetch_one(&pool)
    .await
    .unwrap();
    let uri = "https://events.example.com/bitcoin-meetup-zurich";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "web", 86400);
    doc.set_type("Event");
    doc.name = Some("Bitcoin Meetup Zürich".into());
    doc.start = Some(start);
    doc.location = Some(Point {
        lat: 47.376877,
        lon: 8.538444,
    });
    pinakes_core::group_keys::add(&mut doc, "ics_uid", UID);
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    groups::reconcile(&pool, &id, &cfg.groups).await;
    common::drain_groups(&pool, &cfg.groups).await;
    assert_eq!(all_groups(&pool).await, Vec::<Vec<(String, String)>>::new());
}

/// The token spelling read first is the root; the number read later becomes its alias, so both
/// links answer with one card. A page whose canonical names another group's event, or another
/// host, keeps its own card.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_dates_number_and_token_are_one_resource_and_other_groups_are_not(pool: PgPool) {
    let mut pages = site();
    let page =
        pages[&format!("www.meetup.com/bitcoin-meetup-switzerland/events/316908442/")].clone();
    let other_group = "https://www.meetup.com/another-group/events/316908442/";
    pages.insert(
        "www.meetup.com/another-group/events/316908442/".into(),
        page.clone(),
    );
    let elsewhere = url("316908446/");
    pages.insert(
        "www.meetup.com/bitcoin-meetup-switzerland/events/316908446/".into(),
        page.replace(
            &token(),
            "https://meetup.example.com/bitcoin-meetup-switzerland/events/frxnbvyjcqbmb/",
        ),
    );
    let (r, _) = resolver(&pool, pages);
    let cfg = queue_config(CrossContainerOccurrences::Off);
    let token_id = read(&pool, &r, &cfg, &token()).await;
    assert_eq!(alias_root(&pool, &token_id).await, None);
    let numeric_id = read(&pool, &r, &cfg, &numeric()).await;
    assert_eq!(
        alias_root(&pool, &numeric_id).await,
        Some((token_id.clone(), "canonical".into()))
    );
    let app = app(&pool, CrossContainerOccurrences::Off);
    let card = by_uri(&app, &numeric()).await;
    assert_eq!(card["id"], token_id.as_str());
    assert_eq!(by_uri(&app, &token()).await["id"], token_id.as_str());
    assert!(
        card["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == numeric().as_str()),
        "{card}"
    );
    for uri in [other_group.to_owned(), elsewhere] {
        let id = read(&pool, &r, &cfg, &uri).await;
        assert_eq!(alias_root(&pool, &id).await, None, "{uri}");
        assert_eq!(by_uri(&app, &uri).await["id"], id.as_str(), "{uri}");
    }
}

#[test]
fn shifting_keeps_weekdays_and_both_date_forms() {
    let days = shift_days();
    assert_eq!(days % 364, 0);
    let text = shifted(
        "DTSTART;TZID=Europe/Zurich:20261209T190000 \"2026-12-09T19:00:00+01:00\" 316908442",
        days,
    );
    let d = NaiveDate::from_ymd_opt(2026, 12, 9).unwrap() + Duration::days(days);
    assert!(
        text.contains(&d.format("%Y%m%dT190000").to_string()),
        "{text}"
    );
    assert!(
        text.contains(&d.format("%Y-%m-%dT19:00").to_string()),
        "{text}"
    );
    assert!(text.ends_with(" 316908442"));
    assert_eq!(d.weekday(), chrono::Weekday::Wed);
}
