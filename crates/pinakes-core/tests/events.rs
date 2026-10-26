use chrono::{DateTime, Duration, Utc};
use pinakes_core::{
    config::{EgressConfig, EventConfig},
    event::{calendar, page, time, Attendance, EventStatus},
    model::ResourceMeta,
};
use serde_json::{json, Value};
mod common;
fn now() -> DateTime<Utc> {
    "2026-10-16T00:00:00Z".parse().unwrap()
}
fn parse(body: &str) -> ResourceMeta {
    calendar::parse(
        "https://example.org/feed.ics",
        body.as_bytes(),
        &EventConfig::default(),
        now(),
    )
    .unwrap()
}
fn cal(events: &str) -> String {
    format!("BEGIN:VCALENDAR\nVERSION:2.0\n{events}\nEND:VCALENDAR\n")
}
#[test]
fn custom_timezone_date_budget_preserves_healthy_siblings() {
    for expansion in [
        "RRULE:FREQ=DAILY;COUNT=100000".to_owned(),
        format!("RDATE:{}", vec!["20261020T020000"; 32].join(",")),
    ] {
        let zone = format!("BEGIN:VTIMEZONE\nTZID:CustomBudget\nBEGIN:STANDARD\nDTSTART:19700101T000000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0100\n{expansion}\nEND:STANDARD\nEND:VTIMEZONE");
        let body = cal(&format!(
            "{zone}\n{}\n{}",
            event("UID:bad\nDTSTART;TZID=CustomBudget:20261020T120000\nSUMMARY:Bad zone"),
            event("UID:good\nDTSTART:20261020T120000Z\nSUMMARY:Healthy")
        ));
        let cfg = EventConfig {
            max_date_values: 16,
            ..Default::default()
        };
        let container =
            calendar::parse("https://example.org/feed.ics", body.as_bytes(), &cfg, now())
                .unwrap()
                .container
                .unwrap();
        assert_eq!(container.occurrences.len(), 1);
        assert_eq!(container.occurrences[0].uid, "good");
        assert!(!container.warnings.is_empty());
    }
}
fn event(props: &str) -> String {
    format!("BEGIN:VEVENT\n{props}\nEND:VEVENT")
}
#[test]
fn vendor_calendar_folding_escaping_params_dates_location_conferences_images() {
    let doc = parse(include_str!("fixtures/events/vendors.ics"));
    let c = doc.container.unwrap();
    assert_eq!(c.occurrences.len(), 6);
    assert_eq!(c.feed_ttl, Some(14400));
    let apple = &c.occurrences[0].doc;
    assert_eq!(apple.name.as_deref(), Some("Folded very long event title"));
    assert_eq!(
        apple.description.as_deref(),
        Some("Line one\nLine two; comma, slash\\")
    );
    assert_eq!(
        apple.event.organizer.as_ref().unwrap().name,
        "Alice\"s ^ group\nTeam"
    );
    assert_eq!(apple.location.as_ref().unwrap().lat, 46.0051);
    assert_eq!(apple.event.tz.as_deref(), Some("Europe/Zurich"));
    assert_eq!(
        apple.end.unwrap() - apple.start.unwrap(),
        Duration::hours(2)
    );
    assert!(apple.image.is_some());
    let windows = &c.occurrences[1].doc;
    assert_eq!(windows.event.tz.as_deref(), Some("Europe/Berlin"));
    assert_eq!(windows.event.all_day, Some(false));
    assert_eq!(windows.event.attendance, Some(Attendance::Online));
    assert_eq!(
        c.occurrences[2].doc.end.unwrap() - c.occurrences[2].doc.start.unwrap(),
        Duration::days(1)
    );
    assert_eq!(c.occurrences[2].doc.event.all_day, Some(true));
    assert_eq!(
        c.occurrences[3].doc.event.tz.as_deref(),
        Some("Europe/Zurich")
    );
    assert!(c.occurrences[3].doc.image.is_none());
    assert_eq!(c.occurrences[3].doc.start, c.occurrences[3].doc.end);
    assert_eq!(
        c.occurrences[4].doc.event.attendance,
        Some(Attendance::Online)
    );
    assert_eq!(c.occurrences[5].doc.location.as_ref().unwrap().lat, 47.0);
}
#[test]
fn recurrence_dst_rdate_exdate_overrides_range_and_orphans() {
    let body=cal(&["X-WR-TIMEZONE:Europe/Zurich".into(),event("UID:series\nDTSTART:20261018T163000Z\nDTEND:20261018T173000Z\nRRULE:FREQ=WEEKLY;COUNT=5\nSUMMARY:Weekly\nEXDATE;VALUE=DATE:20261025\nRDATE;VALUE=PERIOD:20261125T173000Z/PT3H"),event("UID:series\nRECURRENCE-ID;TZID=Europe/Zurich:20261101T183000\nDTSTART;TZID=Europe/Zurich:20261101T193000\nDURATION:PT2H\nSUMMARY:Moved\nSEQUENCE:2"),event("UID:series\nRECURRENCE-ID;TZID=Europe/Zurich;RANGE=THISANDFUTURE:20261108T183000\nDTSTART;TZID=Europe/Zurich:20261108T203000\nDURATION:PT2H\nSUMMARY:Future\nSEQUENCE:3"),event("UID:orphan\nRECURRENCE-ID:20261110T180000Z\nDTSTART:20261110T190000Z\nSUMMARY:Orphan")].join("\n"));
    let c = parse(&body).container.unwrap();
    assert_eq!(c.occurrences.len(), 6);
    let rows: Vec<_> = c.occurrences.iter().filter(|o| o.uid == "series").collect();
    assert_eq!(
        rows[0].doc.start.unwrap().to_rfc3339(),
        "2026-10-18T16:30:00+00:00"
    );
    assert_eq!(rows[1].recurrence_id, "2026-11-01T17:30:00Z");
    assert_eq!(
        rows[1].doc.start.unwrap().to_rfc3339(),
        "2026-11-01T18:30:00+00:00"
    );
    assert_eq!(
        rows[2].doc.start.unwrap().to_rfc3339(),
        "2026-11-08T19:30:00+00:00"
    );
    assert_eq!(rows[3].doc.name.as_deref(), Some("Future"));
}
#[test]
fn recurrence_limits_monthly_setpos_until_skip_invalid_and_dtstart() {
    let cfg = EventConfig {
        max_occurrences_per_series: 3,
        ..Default::default()
    };
    let body = cal(&event(
        "UID:daily\nDTSTART:20261016T120000Z\nRRULE:FREQ=DAILY\nSUMMARY:Daily",
    ));
    assert_eq!(
        calendar::parse("https://example.org/a.ics", body.as_bytes(), &cfg, now())
            .unwrap()
            .container
            .unwrap()
            .occurrences
            .len(),
        3
    );
    let body=cal(&event("UID:monthly\nDTSTART:20261001T120000Z\nRRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1;UNTIL=20261231\nSUMMARY:Month end"));
    let c = parse(&body).container.unwrap();
    let dates: Vec<_> = c
        .occurrences
        .iter()
        .map(|o| o.doc.start.unwrap().date_naive().to_string())
        .collect();
    assert_eq!(
        dates,
        vec!["2026-10-01", "2026-10-30", "2026-11-30", "2026-12-31"]
    );
    let body = cal(&event(
        "UID:skip\nDTSTART:20261031T120000Z\nRRULE:FREQ=MONTHLY;COUNT=3\nSUMMARY:31",
    ));
    let dates: Vec<_> = parse(&body)
        .container
        .unwrap()
        .occurrences
        .into_iter()
        .map(|o| o.doc.start.unwrap().date_naive().to_string())
        .collect();
    assert_eq!(dates, vec!["2026-10-31", "2026-12-31", "2027-01-31"]);
}
#[test]
fn timezones_custom_windows_iana_precedence_unknown_float_and_gap() {
    assert_eq!(
        time::zone("/freeassociation.sourceforge.net/Tzfile/Europe/Zurich")
            .unwrap()
            .to_string(),
        "Europe/Zurich"
    );
    assert_eq!(
        time::parse("2026-03-29T02:30:00", Some("Europe/Zurich"))
            .unwrap()
            .to_rfc3339(),
        "2026-03-29T01:30:00+00:00"
    );
    assert_eq!(
        time::parse("2026-10-25T02:30:00", Some("Europe/Zurich"))
            .unwrap()
            .to_rfc3339(),
        "2026-10-25T00:30:00+00:00"
    );
    let timezone="BEGIN:VTIMEZONE\nTZID:Custom\nBEGIN:STANDARD\nDTSTART:19701025T030000\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\nTZOFFSETFROM:+0200\nTZOFFSETTO:+0100\nEND:STANDARD\nBEGIN:DAYLIGHT\nDTSTART:19700329T020000\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nEND:DAYLIGHT\nEND:VTIMEZONE";
    let body = cal(&format!(
        "{timezone}\n{}",
        event("UID:custom\nDTSTART;TZID=Custom:20261020T180000\nSUMMARY:Custom")
    ));
    let c = parse(&body).container.unwrap();
    assert_eq!(
        c.occurrences[0].doc.start.unwrap().to_rfc3339(),
        "2026-10-20T16:00:00+00:00"
    );
    let body = cal(&event(
        "UID:float\nDTSTART:20261020T180000\nSUMMARY:Floating",
    ));
    let c = parse(&body).container.unwrap();
    assert!(c.occurrences[0].doc.event.tz.is_none());
    assert!(c.occurrences[0].doc.event.start_local.is_some());
    let body = cal(&event(
        "UID:unknown\nDTSTART;TZID=Unknown/Zone:20261020T180000\nSUMMARY:Unknown",
    ));
    let c = parse(&body).container.unwrap();
    assert!(c.occurrences[0]
        .doc
        .event
        .warnings
        .contains(&"unknown_tzid_utc_fallback".into()));
}
#[test]
fn identities_duplicate_versions_cancel_and_export_noise() {
    assert_eq!(
        calendar::canonical_feed(
            "webcal://calendar.google.com/calendar/ical/%41%2Eb%40example.com/public/basic.ics"
        ),
        calendar::canonical_feed(
            "https://calendar.google.com/calendar/ical/A.b@example.com/public/basic.ics"
        )
    );
    assert_eq!(
        calendar::canonical_feed("https://cloud.example/index.php/apps/calendar/p/abc/"),
        "https://cloud.example/apps/calendar/p/abc"
    );
    let base = event(
        "UID:same\nDTSTART:20261020T180000Z\nSUMMARY:Old\nSEQUENCE:1\nDTSTAMP:20261015T000000Z",
    );
    let newer = base
        .replace("Old", "New")
        .replace("SEQUENCE:1", "SEQUENCE:2");
    let c = parse(&cal(&format!("{newer}\n{base}"))).container.unwrap();
    assert_eq!(c.occurrences.len(), 1);
    assert_eq!(c.occurrences[0].doc.name.as_deref(), Some("New"));
    assert!(c.warnings.contains(&"duplicate_uid".into()));
    let d = parse(&cal(&newer.replace("20261015T000000Z", "20261016T000000Z")))
        .container
        .unwrap();
    assert_eq!(c.occurrences[0].content_hash, d.occurrences[0].content_hash);
    let missing = parse(&cal(&base.replace("UID:same\n", "")))
        .container
        .unwrap();
    assert!(missing.occurrences[0].uid.starts_with("volatile:"));
    let body=cal(&base.replace("UID:same","UID:20261015T000000Z-1234567891234567890@eventfrog.ch\nURL:https://eventfrog.ch/de/p/test-1234567891234567890.html"));
    let c = calendar::parse(
        "https://eventfrog.ch/stream/de/event/1234567891234567890.ics",
        body.as_bytes(),
        &EventConfig::default(),
        now(),
    )
    .unwrap()
    .container
    .unwrap();
    assert_eq!(c.occurrences[0].uid, "eventfrog:1234567891234567890");
    let cancel = parse(&cal(&format!("METHOD:CANCEL\n{base}")))
        .container
        .unwrap();
    assert_eq!(
        cancel.occurrences[0].doc.event.event_status,
        Some(EventStatus::Cancelled)
    );
    assert!(calendar::parse(
        "https://example.org/f.ics",
        cal(&base).trim_end_matches("END:VCALENDAR\n").as_bytes(),
        &EventConfig::default(),
        now()
    )
    .is_err());
}
#[test]
fn generic_event_page_field_precedence_lenient_dates_offsets_and_online() {
    let url = "https://example.org/e".parse().unwrap();
    let mut d = ResourceMeta::new("https://example.org/e", "web", 1);
    let v=page::lenient_json("{\"@graph\":[{\"@type\":\"Organization\",\"name\":\"Ignore\"},{\"@type\":\"MusicEvent\",\"name\":\"First\",\"description\":\"Raw\ncontrol\",\"startDate\":\"2026-10-20T18:00:00+0100\",\"endDate\":\"2026-10-21\",\"location\":{\"@type\":\"Place\",\"name\":\"Venue\",\"address\":{\"streetAddress\":\"Road 5\",\"addressLocality\":\"City\"}},\"eventStatus\":\"EventPostponed\",},]}").unwrap();
    page::jsonld(&mut d, &v, &url);
    let html = scraper::Html::parse_document(
        "<div class='h-event'><b class='p-name'>Second</b><p class='p-summary'>Other</p></div>",
    );
    page::microformats(&mut d, &html, &url);
    page::finalize(&mut d, false);
    assert_eq!(d.name.as_deref(), Some("First"));
    assert_eq!(d.start.unwrap().to_rfc3339(), "2026-10-20T17:00:00+00:00");
    assert!(d.event.tz.is_none());
    assert_eq!(d.event.event_status, Some(EventStatus::Postponed));
    assert_eq!(d.type_name, "MusicEvent");
    assert_eq!(d.event.venue.unwrap().address["street"], "Road 5");
    let mut d = ResourceMeta::new("https://example.org/e", "web", 1);
    page::jsonld(
        &mut d,
        &json!({"@type":"Event","name":"Online","startDate":"2026-10-20T19:00:00Z","location":{"@type":"VirtualLocation","url":"https://meet.google.com/abc-defg-hij"},"eventAttendanceMode":"https://schema.org/OnlineEventAttendanceMode","offers":{"lowPrice":"3","highPrice":8,"priceCurrency":"CHF"}}),
        &url,
    );
    page::finalize(&mut d, true);
    assert!(d.location.is_none());
    assert_eq!(d.event.attendance, Some(Attendance::Online));
    assert_eq!(d.event.offers.as_ref().unwrap().high_price, Some(8.0));
    assert!(!page::activity(
        &mut d,
        &json!({"type":"Person","summary":"2026 event"}),
        &url
    ));
}
#[test]
fn activitypub_mobilizon_and_feed_extensions() {
    let url: url::Url = "https://mobilizon.example/events/1".parse().unwrap();
    let mut d = ResourceMeta::new(url.as_str(), "web", 1);
    assert!(page::activity(
        &mut d,
        &json!({"type":"Event","name":"AP","startTime":"2026-10-20T19:00:00+02:00","mz:timezone":"Europe/Paris","ical:status":"CANCELLED","joinMode":"free","location":{"type":"Place","latitude":45.7,"longitude":4.8,"name":"Lyon"},"attributedTo":"https://mobilizon.example/@group","attachment":[{"type":"Document","name":"Banner","url":"https://img.example/a.jpg"}]}),
        &url
    ));
    page::finalize(&mut d, true);
    assert_eq!(d.event.tz.as_deref(), Some("Europe/Paris"));
    assert_eq!(d.event.event_status, Some(EventStatus::Cancelled));
    assert!(d.image.is_some());
    let xml=br#"<feed xmlns:mc="urn:starticket"><entry><id>ticket1</id><title>Concert</title><link href="https://example.org/e"/><mc:dates><mc:date><mc:startdate>2026-10-20T19:00:00+02:00</mc:startdate></mc:date></mc:dates><mc:locations><mc:location><mc:name>Hall</mc:name><mc:latitude>47</mc:latitude><mc:longitude>8</mc:longitude></mc:location></mc:locations></entry></feed>"#;
    let c = calendar::structured_feed(
        "https://example.org/rss",
        xml,
        &EventConfig::default(),
        now(),
    )
    .unwrap()
    .unwrap()
    .container
    .unwrap();
    assert_eq!(c.occurrences.len(), 1);
    assert_eq!(c.occurrences[0].uid, "ticket1");
    let json = json!({"version":"https://jsonfeed.org/version/1","items":[{"id":"id1","title":"Gancio","url":"https://example.org/event/a","_gancio":{"start_datetime":1792522800,"place":{"name":"Hall","lat":47,"lon":8}}}]});
    assert_eq!(
        calendar::structured_feed(
            "https://example.org/json",
            json.to_string().as_bytes(),
            &EventConfig::default(),
            now()
        )
        .unwrap()
        .unwrap()
        .container
        .unwrap()
        .occurrences
        .len(),
        1
    );
}

use axum::{
    body::Body,
    extract::{ConnectInfo, Query},
    http::Request,
    routing::get,
    Router,
};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, IngestConfig, OsmConfig},
    ingest,
    model::{identity, Point},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        osm::OsmResolver,
        web::WebResolver,
        Resolver,
    },
    store,
    telemetry::Metrics,
};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tower::ServiceExt;
async fn call(app: &Router, path: &str) -> (u16, Value) {
    send(app, "GET", path, None).await
}
async fn send(app: &Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1111".parse::<SocketAddr>().unwrap()));
    let r = app.clone().oneshot(request).await.unwrap();
    let status = r.status().as_u16();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    if let Some(value) = &parsed {
        common::check_response(method, path, status, value);
    }
    (
        status,
        parsed.unwrap_or(json!(String::from_utf8_lossy(&bytes))),
    )
}
/// A post's event occurrences at a venue, as event references: each an hour long, scheduled; its
/// previous occurrences replaced. Written directly: no feed writes event references today.
async fn occurrences_at(
    pool: &PgPool,
    venue: &str,
    path: &str,
    occurrences: &[(Option<&str>, DateTime<Utc>)],
) {
    let (id, uri, _) = identity(venue).unwrap();
    let author = "y".repeat(52);
    let mut tx = pool.begin().await.unwrap();
    let mut affected: Vec<String> = sqlx::query_scalar(
        "DELETE FROM reference WHERE author=$1 AND object_path=$2 RETURNING resource_id",
    )
    .bind(&author)
    .bind(path)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    let aliased = pinakes_core::store::insert_resource(&mut tx, &id, &uri)
        .await
        .unwrap();
    for (rid, start) in occurrences {
        sqlx::query(r#"INSERT INTO reference(resource_id,kind,author,object_path,recurrence_id,start_at,end_at,created_at,unit_key,event_status)
            VALUES($1,'event',$2,$3,$4,$5,$6,now(),$7,'scheduled')"#)
            .bind(&id).bind(&author).bind(path).bind(rid).bind(start).bind(*start + Duration::hours(1))
            .bind(format!("{id}:{}", rid.unwrap_or(""))).execute(&mut *tx).await.unwrap();
    }
    affected.push(id);
    pinakes_core::store::recompute_scores(&mut tx, &affected)
        .await
        .unwrap();
    pinakes_core::alias::record(&mut tx, aliased.into_iter().collect())
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
async fn reference(pool: &PgPool, uri: &str, path: &str, kind: Kind, op: Op) {
    let (_, uri, _) = identity(uri).unwrap();
    let cursor = ingest::cursor(pool, "events").await.unwrap();
    let line = Change {
        op,
        uri,
        kind,
        author: "y".repeat(52),
        object_path: path.into(),
        label: Some("bitcoin".into()),
        at: Utc::now().timestamp_millis(),
    };
    ingest::apply(
        pool,
        "events",
        &cursor,
        &FeedPage {
            page: Changes {
                lines: vec![line],
                next_cursor: cursor.cursor + 1,
                skipped: Default::default(),
            },
            epoch: Some("1".into()),
            length: cursor.cursor + 1,
        },
        &IngestConfig::default(),
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap();
}
fn future_calendar(n: usize) -> String {
    cal(&(0..n)
        .map(|i| {
            event(&format!(
                "UID:id{i}\nDTSTART:{}Z\nDURATION:PT1H\nSUMMARY:Event {i}\nGEO:47;8",
                (Utc::now() + Duration::days(i as i64 + 2)).format("%Y%m%dT%H%M%S")
            ))
        })
        .collect::<Vec<_>>()
        .join("\n"))
}
fn dynamic_parse(uri: &str, text: &str) -> ResourceMeta {
    calendar::parse(uri, text.as_bytes(), &EventConfig::default(), Utc::now()).unwrap()
}
fn fresh(doc: &ResourceMeta) -> ResourceMeta {
    static FETCH: Mutex<i64> = Mutex::new(0);
    let now = Utc::now().timestamp_millis() + 1000;
    let mut at = FETCH.lock().unwrap();
    *at = (*at + 1).max(now);
    let mut doc = doc.clone();
    doc.fetched_at = *at;
    doc
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn calendar_snapshots_atomic_held_disappearance_freeze_and_changes(pool: PgPool) {
    let uri = "https://example.org/test.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let full = future_calendar(4);
    let original = dynamic_parse(uri, &full);
    store::save_document(&pool, &id, 0, &original)
        .await
        .unwrap();
    let count = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM occurrence WHERE status='cancelled'")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let fewer = dynamic_parse(uri, &future_calendar(1));
    store::save_document(&pool, &id, 0, &fewer).await.unwrap();
    assert_eq!(count().await, 0);
    let held: bool =
        sqlx::query_scalar("SELECT held_keys IS NOT NULL FROM event_fetch WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held);
    store::save_document(&pool, &id, 99, &fewer).await.unwrap();
    assert_eq!(count().await, 0);
    store::save_document(&pool, &id, 0, &fewer).await.unwrap();
    assert_eq!(count().await, 0, "replayed snapshot cannot confirm loss");
    store::save_document(&pool, &id, 0, &fresh(&fewer))
        .await
        .unwrap();
    assert_eq!(count().await, 3);
    store::save_document(&pool, &id, 0, &fresh(&original))
        .await
        .unwrap();
    assert_eq!(count().await, 0);
    let mut moved = original.clone();
    let row = &mut moved.container.as_mut().unwrap().occurrences[0];
    row.doc.start = row.doc.start.map(|s| s + Duration::hours(1));
    row.doc.end = row.doc.end.map(|s| s + Duration::hours(1));
    row.sequence += 1;
    moved = fresh(&moved);
    store::save_document(&pool, &id, 0, &moved).await.unwrap();
    let classes:Vec<String>=sqlx::query_scalar("SELECT class FROM resource_change WHERE resource_id=$1 AND occurrence_key IS NOT NULL ORDER BY seq").bind(&id).fetch_all(&pool).await.unwrap();
    assert_eq!(classes.iter().filter(|s| *s == "created").count(), 4);
    assert_eq!(classes.iter().filter(|s| *s == "cancelled").count(), 3);
    assert_eq!(classes.iter().filter(|s| *s == "uncancelled").count(), 3);
    assert_eq!(classes.last().unwrap(), "rescheduled");
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_change")
        .fetch_one(&pool)
        .await
        .unwrap();
    store::save_document(&pool, &id, 0, &moved).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change")
            .fetch_one(&pool)
            .await
            .unwrap(),
        before
    );
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn single_key_loss_requires_two_and_regenerated_uid_links_supersedes(pool: PgPool) {
    let uri = "https://example.org/t.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let full = future_calendar(3);
    let d = dynamic_parse(uri, &full);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let mut smaller = fresh(&d);
    smaller.container.as_mut().unwrap().occurrences.pop();
    store::save_document(&pool, &id, 0, &smaller).await.unwrap();
    smaller = fresh(&smaller);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM occurrence WHERE status='cancelled'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    store::save_document(&pool, &id, 0, &smaller).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM occurrence WHERE status='cancelled'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let changed = fresh(&dynamic_parse(uri, &full.replace("UID:id0", "UID:new0")));
    store::save_document(&pool, &id, 0, &changed).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM occurrence WHERE uid='new0'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "scheduled");
    let change: Value = sqlx::query_scalar(
        "SELECT after FROM resource_change WHERE occurrence_key='[\"new0\",\"\"]'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(change["supersedes"], "[\"id0\",\"\"]");
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn event_units_pagination_occurrence_route_pubky_recurrences_and_venue(pool: PgPool) {
    let feed = "https://example.org/events.ics";
    reference(&pool, feed, "/pub/social/tags/calendar", Kind::Tag, Op::Put).await;
    let id = identity(feed).unwrap().0;
    store::save_document(&pool, &id, 0, &dynamic_parse(feed, &future_calendar(3)))
        .await
        .unwrap();
    let venue = "geo:47,8";
    let venue_id = store::ensure_resource(&pool, venue).await.unwrap();
    let place = pinakes_core::resolver::geo::GeoResolver
        .resolve(venue)
        .await
        .unwrap();
    store::save_document(&pool, &venue_id, 0, &place)
        .await
        .unwrap();
    let second = Utc::now() + Duration::days(3);
    occurrences_at(
        &pool,
        venue,
        "/pub/social/v1/posts/event1",
        &[
            (Some("2026-10-21"), Utc::now() + Duration::days(2)),
            (Some("2026-10-28"), second),
        ],
    )
    .await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reference WHERE kind='event'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    let page_uri = "https://example.org/event";
    reference(
        &pool,
        page_uri,
        "/pub/social/tags/event",
        Kind::Tag,
        Op::Put,
    )
    .await;
    let page_id = identity(page_uri).unwrap().0;
    let mut page = ResourceMeta::new(page_uri, "jsonld", 3600);
    page.set_type("MusicEvent");
    page.name = Some("Page event".into());
    page.start = Some(Utc::now() + Duration::days(2));
    page.location = Some(Point {
        lat: 47.0,
        lon: 8.0,
    });
    page.event.venue = Some(pinakes_core::event::Venue {
        resource_id: Some(venue_id.clone()),
        ..Default::default()
    });
    store::save_document(&pool, &page_id, 0, &page)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, v) = call(
        &app,
        "/v1/search?type=Event&sort=start&bbox=7,46,9,48&limit=2",
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    let mut seen = vec![];
    let mut v = v;
    loop {
        seen.extend(v["items"].as_array().unwrap().iter().cloned());
        let Some(cursor) = v["cursor"].as_str() else {
            break;
        };
        let result = call(
            &app,
            &format!("/v1/search?type=Event&sort=start&bbox=7,46,9,48&limit=2&cursor={cursor}"),
        )
        .await;
        assert_eq!(result.0, 200, "{}", result.1);
        v = result.1;
    }
    assert_eq!(seen.len(), 6);
    assert_eq!(seen.iter().filter(|v| v.get("event").is_some()).count(), 2);
    assert_eq!(
        seen.iter()
            .filter(|v| v.get("occurrence").is_some())
            .count(),
        3
    );
    assert!(seen.iter().all(|v| v.get("start").is_some()));
    let (code, v) = call(&app, &format!("/v1/search?type=Event&venue={venue_id}")).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    let (code, v) = call(&app, &format!("/v1/resources/{id}/occurrences")).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    let (code, v) = call(
        &app,
        "/v1/search?type=Event&bbox=7,46,9,48&cluster=8&limit=1",
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["total"], 6);
    assert_eq!(v["clusters"][0]["count"], 6);
    let top = v["clusters"][0]["top"].as_array().unwrap();
    assert_eq!(top.len(), 3);
    assert_eq!(
        top.iter()
            .map(|v| v.as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );

    // A new version of the post with one occurrence fewer.
    occurrences_at(
        &pool,
        venue,
        "/pub/social/v1/posts/event1",
        &[(Some("2026-10-28"), second)],
    )
    .await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reference WHERE kind='event'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn venue_geocoding_single_match_threshold_and_osm_storage_boundary(pool: PgPool) {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app=Router::new().route("/search",get(move|Query(q):Query<HashMap<String,String>>|{let seen=seen.clone();async move{seen.fetch_add(1,Ordering::SeqCst);assert_eq!(q["addressdetails"],"1");assert_eq!(q["street"],"Via Nassa 5");axum::Json(json!([{"osm_type":"node","osm_id":123,"lat":"46.0051","lon":"8.9528","category":"amenity","type":"cafe","name":"Plan B","namedetails":{"name":"Plan B"},"address":{"road":"Via Nassa","house_number":"5","city":"Lugano","postcode":"6900","country_code":"ch"}}]))}}));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let service = pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: EventConfig::default(),
        osm: Arc::new(
            OsmResolver::new(
                pool.clone(),
                OsmConfig {
                    nominatim_url: format!("http://{addr}"),
                    requests_per_second: 1000.0,
                    ..Default::default()
                },
            )
            .unwrap(),
        ),
    };
    let uri = "https://example.org/event";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 3600);
    page::jsonld(
        &mut doc,
        &json!({"@type":"Event","name":"Meetup","startDate":"2026-12-20T18:00:00","location":{"@type":"Place","name":"Plan B","address":{"streetAddress":"Via Nassa 5","addressLocality":"Lugano","postalCode":"6900","addressCountry":"CH"}}}),
        &uri.parse().unwrap(),
    );
    service.venue(&mut doc).await.unwrap();
    assert!(doc.event.venue.as_ref().unwrap().resource_id.is_some());
    assert!(doc.location.is_none());
    assert_eq!(doc.event.tz.as_deref(), Some("Europe/Zurich"));
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let stored: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(stored.get("location").is_none());
    assert!(stored["venue"].get("location").is_none());
    let hydrated: Value = sqlx::query_scalar("SELECT doc FROM resource_document WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hydrated["location"]["lat"], 46.0051);
    assert!(hydrated["attribution"]
        .as_str()
        .unwrap()
        .contains("OpenStreetMap"));
    let mut online = doc.clone();
    online.event.attendance = Some(Attendance::Online);
    online.event.venue.as_mut().unwrap().osm_id = None;
    service.venue(&mut online).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}
struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}
struct ProtocolTransport {
    seen: Mutex<Vec<(String, Option<String>, bool)>>,
}
#[async_trait::async_trait]
impl Transport for ProtocolTransport {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &FetchOptions::default())
            .await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        o: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen.lock().unwrap().push((
            url.path().into(),
            o.accept.clone(),
            o.validators.etag.is_some(),
        ));
        let mut f = Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status: 200,
            retry_after_secs: 60,
            location: None,
            content_type: "text/html".into(),
            body: b"<title>Fallback</title>".to_vec(),
        };
        if url.path() == "/event" && o.accept.is_none() {
            f.validators.link =
                Some("</event>; rel=alternate; type=application/activity+json".into());
        }
        if url.path() == "/ld" && o.accept.is_none() {
            f.body = b"<link rel='alternate' type='application/ld+json' href='/ld'>".to_vec();
        }
        if url.path() == "/ld-comments" {
            // A rendered comment's link sits in the body.
            f.body = b"<title>Comments</title><p>Nice event</p><link rel='alternate' type='application/ld+json' href='/ld'>".to_vec();
        }
        if url.path() == "/event" && o.accept.is_some() {
            f.status = 301;
            f.location = Some("/federation/m/1".into());
        }
        if url.path() == "/jsonld-event" {
            f.body = if o.accept.is_some() {
                json!({"type":"Event","name":"AP fallback","startTime":"2027-01-20T18:00:00Z","timezone":"Europe/Paris","joinMode":"free","isOnline":true}).to_string().into_bytes()
            } else {
                br#"<script type="application/ld+json">{"@type":"Event","name":"Primary JSON-LD","startDate":"2027-01-20T18:00:00Z"}</script>"#.to_vec()
            };
        }
        if url.path() == "/entities" {
            // Venue and organizer text with HTML references, as event pages serve it.
            f.body = br#"<script type="application/ld+json">{"@type":"Event","name":"Caf&eacute; night","startDate":"2027-01-20T18:00:00Z","location":{"@type":"Place","name":"Caf&eacute; Fran&ccedil;ais","address":{"@type":"PostalAddress","streetAddress":"Rue de l&#39;Arc 1","addressLocality":"Gen&egrave;ve"}},"organizer":{"@type":"Organization","name":"Smith &amp; Sons"}}</script>"#.to_vec();
        }
        if url.path() == "/gathio" {
            f.body = if o.accept.is_some() {
                json!({"type":"Person","name":"Actor"})
                    .to_string()
                    .into_bytes()
            } else {
                b"<div class='h-event'><time class='dt-start' datetime='2027-01-20T18:00:00Z'></time></div><a href='/export/event/gathio'>Calendar</a><title>Fallback</title>".to_vec()
            };
        }
        if url.path() == "/ld"
            && o.accept
                .as_deref()
                .is_some_and(|s| s.starts_with("application/ld+json"))
        {
            f.body=json!({"type":"Event","name":"LD negotiation","startTime":"2027-01-20T18:00:00Z","timezone":"Europe/Paris"}).to_string().into_bytes();
        }
        if url.path() == "/federation/m/1" {
            f.content_type = "application/activity+json".into();
            f.body=json!({"type":"Event","name":"Negotiated","startTime":"2027-01-20T18:00:00Z","timezone":"Europe/Paris","isOnline":true}).to_string().into_bytes();
        }
        if url.path() == "/f.ics" || url.path() == "/export/event/gathio" {
            f.content_type = "text/plain".into();
            f.validators.etag = Some("test".into());
            f.body = future_calendar(1).into_bytes();
            if o.validators.etag.is_some() {
                f.status = 304;
                f.body.clear();
            }
        }
        Ok(f)
    }
}
/// `reextract web` repairs an event page's text stored before the decoding fix: its name and
/// the venue and organizer details come back decoded once.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reextract_repairs_event_text(pool: PgPool) {
    use pinakes_core::{config::QueueConfig, queue};
    let uri = "https://example.org/entities";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut old = ResourceMeta::new(uri, "web", 86400);
    old.set_type("Event");
    old.name = Some("Caf&eacute; night".into());
    old.start = Some(Utc::now() + Duration::days(30));
    old.event.organizer = Some(pinakes_core::event::Organizer {
        name: "Smith &amp; Sons".into(),
        url: None,
    });
    store::save_document(&pool, &id, 0, &old).await.unwrap();
    assert_eq!(queue::reextract(&pool, &["web".into()]).await.unwrap(), 1);
    let cfg = QueueConfig::default();
    let job = queue::claim(&pool, "web", &cfg)
        .await
        .unwrap()
        .expect("re-queued");
    let egress = Egress::with_backends(
        pinakes_core::config::EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        },
        Arc::new(PublicDns),
        Arc::new(ProtocolTransport {
            seen: Mutex::default(),
        }),
    );
    let resolver = WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        egress,
        config: Default::default(),
        events: Some(Arc::new(pinakes_core::event::service::EventService {
            pool: pool.clone(),
            config: EventConfig {
                geocoding: false,
                ..Default::default()
            },
            osm: Arc::new(OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap()),
        })),
    };
    queue::run_job(&pool, &resolver, &cfg, &job).await.unwrap();
    let card = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, v) = call(&card, &format!("/v1/resources/{id}")).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["name"], "Café night");
    assert_eq!(v["venue"]["name"], "Café Français");
    assert_eq!(v["organizer"]["name"], "Smith & Sons");
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn advertised_event_negotiation_and_calendar_not_modified(pool: PgPool) {
    let t = Arc::new(ProtocolTransport {
        seen: Mutex::default(),
    });
    let egress = Egress::with_backends(
        pinakes_core::config::EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        },
        Arc::new(PublicDns),
        t.clone(),
    );
    let service = Arc::new(pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: EventConfig {
            geocoding: false,
            ..Default::default()
        },
        osm: Arc::new(OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap()),
    });
    let resolver = WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        egress,
        config: Default::default(),
        events: Some(service),
    };
    let d = resolver
        .resolve("https://example.org/entities")
        .await
        .unwrap();
    assert_eq!(d.name.as_deref(), Some("Café night"));
    let venue = d.event.venue.as_ref().expect("venue");
    assert_eq!(venue.name.as_deref(), Some("Café Français"));
    assert!(!venue.text.contains('&'), "{}", venue.text);
    assert!(
        venue.address.values().any(|v| v == "Rue de l'Arc 1")
            && venue.address.values().any(|v| v == "Genève"),
        "{:?}",
        venue.address
    );
    assert_eq!(
        d.event.organizer.as_ref().map(|o| o.name.as_str()),
        Some("Smith & Sons")
    );
    let d = resolver.resolve("https://example.org/event").await.unwrap();
    assert_eq!(d.name.as_deref(), Some("Negotiated"));
    assert_eq!(d.event.tz.as_deref(), Some("Europe/Paris"));
    let d = resolver
        .resolve("https://example.org/gathio")
        .await
        .unwrap();
    assert_eq!(d.type_name, "Event");
    assert_eq!(d.name.as_deref(), Some("Event 0"));
    let d = resolver.resolve("https://example.org/ld").await.unwrap();
    assert_eq!(d.name.as_deref(), Some("LD negotiation"));
    // The same alternate link in the body advertises nothing.
    t.seen.lock().unwrap().clear();
    let d = resolver
        .resolve("https://example.org/ld-comments")
        .await
        .unwrap();
    assert_eq!(d.name.as_deref(), Some("Comments"));
    assert!(t
        .seen
        .lock()
        .unwrap()
        .iter()
        .all(|(path, accept, _)| path == "/ld-comments" && accept.is_none()));
    for _ in 0..2 {
        let d = resolver
            .resolve("https://example.org/jsonld-event")
            .await
            .unwrap();
        assert_eq!(d.name.as_deref(), Some("Primary JSON-LD"));
        assert_eq!(d.event.tz.as_deref(), Some("Europe/Paris"));
        assert_eq!(d.event.join_mode.as_deref(), Some("free"));
        assert_eq!(d.event.attendance, Some(Attendance::Online));
    }
    assert_eq!(
        t.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, accept, _)| path == "/jsonld-event" && accept.is_some())
            .count(),
        1
    );
    t.seen.lock().unwrap().clear();
    let id = store::ensure_resource(&pool, "webcal://example.org/f.ics")
        .await
        .unwrap();
    let d = resolver
        .resolve("webcal://example.org/f.ics")
        .await
        .unwrap();
    assert_eq!(d.container.as_ref().unwrap().occurrences.len(), 1);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert!(resolver
        .resolve("webcal://example.org/f.ics")
        .await
        .unwrap_err()
        .is::<pinakes_core::resolver::NotModified>());
    let calls = t.seen.lock().unwrap();
    let calls: Vec<_> = calls
        .iter()
        .filter(|(path, _, _)| path == "/f.ics")
        .map(|(_, _, v)| *v)
        .collect();
    assert_eq!(calls, vec![false, true]);
}

#[tokio::test]
async fn feed_transport_stream_cap_wrong_type_sniff_and_page_cap() {
    use pinakes_core::resolver::egress::read_response_with;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/feed",
            get(|| async {
                (
                    [("content-type", "application/octet-stream")],
                    format!(
                        "BEGIN:VCALENDAR\n{}\nEND:VCALENDAR",
                        "X-COMMENT:long\n".repeat(100000)
                    ),
                )
            }),
        )
        .route(
            "/page",
            get(|| async { ([("content-type", "text/html")], "x".repeat(1100000)) }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let uri: url::Url = format!("http://{addr}/feed").parse().unwrap();
    let options = FetchOptions {
        feed_cap: Some(2 * 1024 * 1024),
        ..Default::default()
    };
    let response = read_response_with(
        uri.clone(),
        client.get(uri.clone()).send().await.unwrap(),
        &options,
    )
    .await
    .unwrap();
    assert!(response.body.len() > 1024 * 1024);
    assert!(read_response_with(
        uri.clone(),
        client.get(uri).send().await.unwrap(),
        &FetchOptions {
            feed_cap: Some(1024),
            ..Default::default()
        }
    )
    .await
    .is_err());
    let uri: url::Url = format!("http://{addr}/page").parse().unwrap();
    let page = read_response_with(uri.clone(), client.get(uri).send().await.unwrap(), &options)
        .await
        .unwrap();
    assert!(page.truncated);
    assert_eq!(page.body.len(), pinakes_core::resolver::egress::BODY_CAP);
    server.abort();
}
#[test]
fn remaining_calendar_pitfalls_dates_lists_split_uids_alt_images_and_horizons() {
    let body=cal(&format!("X-WR-TIMEZONE:Europe/Zurich\n{}\n{}\n{}",event("UID:base@google.com\nDTSTART;TZID=Europe/Zurich:20261020T180000\nRRULE:FREQ=WEEKLY;UNTIL=20261101T170000Z\nEXDATE;TZID=Europe/Zurich:20261027T180000,20261103T180000\nRDATE;VALUE=DATE:20261022\nSUMMARY:Before split\nX-ALT-DESC;FMTTYPE=text/HTML:<p>Poster<img src=\"https://example.org/img.png\"></p>"),event("UID:base_R20261101T180000@google.com\nDTSTART;TZID=Europe/Zurich:20261101T180000\nRRULE:FREQ=WEEKLY;COUNT=2\nSUMMARY:After split"),event("UID:date\nDTSTART;VALUE=DATE:20261025\nDTEND;VALUE=DATE:20261027\nSUMMARY:Two days\nSTATUS:CANCELLED")));
    let c = parse(&body).container.unwrap();
    assert!(c.occurrences.iter().any(|o| o.uid == "base@google.com"));
    assert!(c
        .occurrences
        .iter()
        .any(|o| o.uid == "base_R20261101T180000@google.com"));
    assert!(c
        .occurrences
        .iter()
        .find(|o| o.uid == "base@google.com")
        .unwrap()
        .doc
        .image
        .is_some());
    let all_day = &c.occurrences.iter().find(|o| o.uid == "date").unwrap().doc;
    assert_eq!(all_day.event.event_status, Some(EventStatus::Cancelled));
    assert_eq!(
        all_day.end.unwrap() - all_day.start.unwrap(),
        Duration::hours(49)
    );
    let old = cal(&event("UID:old\nDTSTART:20000101T120000Z\nSUMMARY:Ancient"));
    assert!(parse(&old).container.unwrap().occurrences.is_empty());
    let bad = cal(&event(
        "UID:bad\nDTSTART:20261020T120000Z\nRRULE:FREQ=WEEKLY;COUNT=10;UNTIL=20270101T120000Z",
    ));
    assert!(calendar::parse(
        "https://example.org/f.ics",
        bad.as_bytes(),
        &EventConfig::default(),
        now()
    )
    .unwrap()
    .container
    .unwrap()
    .occurrences
    .is_empty());
}

#[test]
fn recurrence_local_days_coordinate_zones_exclusion_budget_and_categories() {
    let c = parse(&cal(&format!("X-WR-TIMEZONE:Europe/Zurich\n{}", event("UID:days\nDTSTART;VALUE=DATE:20261024\nRRULE:FREQ=DAILY;COUNT=3\nCATEGORIES:Music,Talks\\, panels\nCATEGORIES:Music,Community")))).container.unwrap();
    assert_eq!(
        c.occurrences
            .iter()
            .map(|o| (o.doc.end.unwrap() - o.doc.start.unwrap()).num_hours())
            .collect::<Vec<_>>(),
        vec![24, 25, 24]
    );
    assert_eq!(
        c.occurrences[0].doc.event.categories,
        vec!["Community", "Music", "Talks, panels"]
    );
    let c = parse(&cal(&event("UID:floating\nDTSTART:20261024T180000\nDTEND:20261024T190000\nRRULE:FREQ=DAILY;COUNT=3\nGEO:47.37;8.54"))).container.unwrap();
    assert_eq!(
        c.occurrences[0].doc.event.tz.as_deref(),
        Some("Europe/Zurich")
    );
    assert_eq!(
        c.occurrences[0].doc.start.unwrap().to_rfc3339(),
        "2026-10-24T16:00:00+00:00"
    );
    assert_eq!(
        c.occurrences[1].doc.start.unwrap().to_rfc3339(),
        "2026-10-25T17:00:00+00:00"
    );
    let exclusions = (16..=31)
        .map(|n| format!("202610{n}T120000Z"))
        .collect::<Vec<_>>()
        .join(",");
    let c = parse(&cal(&event(&format!(
        "UID:many\nDTSTART:20261016T120000Z\nRRULE:FREQ=DAILY\nEXDATE:{exclusions}"
    ))))
    .container
    .unwrap();
    assert_eq!(c.occurrences.len(), 50);
    let cfg = EventConfig {
        max_recurrence_steps: 10,
        ..Default::default()
    };
    assert!(calendar::parse(
        "https://example.org/f.ics",
        cal(&event(
            "UID:expensive\nDTSTART:19000101T000000Z\nRRULE:FREQ=HOURLY"
        ))
        .as_bytes(),
        &cfg,
        now()
    )
    .is_err());
    let body = cal(&event(
        "UID:noise\nDTSTART:20261020T120000Z\nX-UNKNOWN-EXPORT:one",
    ));
    assert_eq!(
        parse(&body).container.unwrap().occurrences[0].content_hash,
        parse(&body.replace("one", "two"))
            .container
            .unwrap()
            .occurrences[0]
            .content_hash
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn ended_occurrences_freeze_and_horizon_rolloff_is_not_cancellation(pool: PgPool) {
    let uri = "https://example.org/frozen.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let past = event(&format!(
        "UID:past\nDTSTART:{}Z\nDURATION:PT1H\nSUMMARY:Original",
        (Utc::now() - Duration::days(3)).format("%Y%m%dT%H%M%S")
    ));
    let d = fresh(&dynamic_parse(uri, &cal(&past)));
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let changed = fresh(&dynamic_parse(
        uri,
        &cal(&past.replace("Original", "Edited\nSTATUS:CANCELLED")),
    ));
    store::save_document(&pool, &id, 0, &changed).await.unwrap();
    let row: (String, String) =
        sqlx::query_as("SELECT name,status FROM occurrence WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, ("Original".into(), "scheduled".into()));
    let mut empty = fresh(&dynamic_parse(uri, &cal("")));
    empty.container.as_mut().unwrap().horizon_start = Utc::now();
    store::save_document(&pool, &id, 0, &empty).await.unwrap();
    store::save_document(&pool, &id, 0, &fresh(&empty))
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM occurrence WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "scheduled"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource_change WHERE occurrence_key IS NOT NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[test]
fn weekly_week_start_and_iana_over_embedded_timezone() {
    let body = |week| {
        cal(&event(&format!("UID:week\nDTSTART:20261020T120000Z\nRRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,SU;COUNT=4;WKST={week}")))
    };
    let parse_dates = |body: &str| {
        parse(body)
            .container
            .unwrap()
            .occurrences
            .iter()
            .map(|o| o.doc.start.unwrap().date_naive().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        parse_dates(&body("MO")),
        vec!["2026-10-20", "2026-10-25", "2026-11-03", "2026-11-08"]
    );
    assert_eq!(
        parse_dates(&body("SU")),
        vec!["2026-10-20", "2026-11-01", "2026-11-03", "2026-11-15"]
    );
    let tz="BEGIN:VTIMEZONE\nTZID:Europe/Zurich\nBEGIN:STANDARD\nDTSTART:19700101T000000\nTZOFFSETFROM:+0900\nTZOFFSETTO:+0900\nEND:STANDARD\nEND:VTIMEZONE";
    let c = parse(&cal(&format!(
        "{tz}\n{}",
        event("UID:iana\nDTSTART;TZID=Europe/Zurich:20261020T180000")
    )))
    .container
    .unwrap();
    assert_eq!(
        c.occurrences[0].doc.start.unwrap().to_rfc3339(),
        "2026-10-20T16:00:00+00:00"
    );
    let c = parse(&cal(&format!(
        "X-WR-TIMEZONE:Europe/Paris\n{}",
        event("UID:fallback\nDTSTART;TZID=Absent:20261020T180000")
    )))
    .container
    .unwrap();
    assert_eq!(
        c.occurrences[0].doc.event.tz.as_deref(),
        Some("Europe/Paris")
    );
}

#[test]
fn malformed_events_are_isolated_and_hostile_work_is_bounded() {
    let good = event("UID:good\nDTSTART:20261020T120000Z\nSUMMARY:Good");
    for bad in [
        "UID:bad\nDTSTART:20260230T120000Z",
        "UID:bad\nSUMMARY:Missing start",
        "UID:bad\nDTSTART:20261020T120000Z\nDTEND:20261019T120000Z",
        "UID:bad\nDTSTART:20261020T120000Z\nRRULE:FREQ=WEEKLY;BYDAY=1MO",
        "UID:bad\nDTSTART:20261020T120000Z\nRRULE:FREQ=INVALID",
        "UID:bad\nDTSTART;VALUE=DATE:+262142-12-31",
        "UID:bad\nDTSTART:20261020T120000Z\nRDATE;VALUE=PERIOD:20261021T120000Z/P999999999D",
        "UID:bad\nDTSTART:20261020T120000Z\nNO COLON",
    ] {
        let body = cal(&format!("{}\n{good}", event(bad)));
        let result =
            std::panic::catch_unwind(|| parse(&body)).expect("untrusted date must not panic");
        let c = result.container.unwrap();
        assert_eq!(c.occurrences.len(), 1, "{bad}");
        assert!(!c.warnings.is_empty(), "{bad}");
    }
    let mut body = cal(&format!(
        "{}\n{good}",
        event("UID:bad\nDTSTART:20261020T120000Z\nSUMMARY:invalid")
    ))
    .into_bytes();
    let i = body.windows(7).position(|b| b == b"invalid").unwrap();
    body[i] = 0xff;
    assert_eq!(
        calendar::parse(
            "https://example.org/f.ics",
            &body,
            &EventConfig::default(),
            now()
        )
        .unwrap()
        .container
        .unwrap()
        .occurrences
        .len(),
        1
    );
    let large = cal(&event(&format!(
        "UID:bounded\nDTSTART:20261020T120000Z\nRRULE:FREQ=DAILY\nSUMMARY:{}\nDESCRIPTION:{}",
        "x".repeat(32000),
        "y".repeat(32000)
    )));
    let c = parse(&large).container.unwrap();
    assert_eq!(c.occurrences.len(), 50);
    assert!(c
        .occurrences
        .iter()
        .all(|o| o.doc.name.as_ref().unwrap().len() <= 512
            && o.doc.description.as_ref().unwrap().len() <= 2048));
    let cfg = EventConfig {
        max_date_values: 2,
        ..Default::default()
    };
    let rdates = cal(&event(
        "UID:r\nDTSTART:20261020T120000Z\nRDATE:20261021T120000Z,20261022T120000Z,20261023T120000Z",
    ));
    assert!(calendar::parse("https://example.org/f.ics", rdates.as_bytes(), &cfg, now()).is_err());
    let long = cal(&event(&format!(
        "UID:long\nDTSTART:20261020T120000Z\nSUMMARY:{}",
        "a".repeat(65537)
    )));
    assert!(calendar::parse(
        "https://example.org/f.ics",
        long.as_bytes(),
        &EventConfig::default(),
        now()
    )
    .is_err());
}

#[test]
fn ics_address_mixed_privacy_and_date_end() {
    let body=cal(&format!("X-PUBLISHED-TTL:PT6H\n{}",event("UID:hybrid\nDTSTART:20261020T180000Z\nLOCATION:Room A\\nMain Street 10\\, 8000 Zurich\\, Switzerland\nX-ALT-DESC;FMTTYPE=text/html:<p>Join <a href=\"https://meet.google.com/abc-defg-hij\">online</a></p>\nORGANIZER;CN=alice@example.org:mailto:alice@example.org\nSTATUS:TENTATIVE"))).replace('\n',"\r\n");
    let c = parse(&body).container.unwrap();
    assert_eq!(c.feed_ttl, Some(21600));
    let d = &c.occurrences[0].doc;
    assert_eq!(d.event.attendance, Some(Attendance::Mixed));
    assert_eq!(d.event.event_status, Some(EventStatus::Scheduled));
    assert_eq!(d.event.venue.as_ref().unwrap().address["city"], "Zurich");
    assert!(d.event.organizer.is_none());
    assert!(!d.description.as_ref().unwrap().contains('<'));
    let mut d = ResourceMeta::new("https://example.org/e", "web", 3600);
    page::jsonld(
        &mut d,
        &json!({"@type":"Event","startDate":"2026-10-20T19:00:00+02:00","endDate":"2026-10-20"}),
        &"https://example.org/e".parse().unwrap(),
    );
    page::finalize(&mut d, false);
    assert_eq!(d.end.unwrap().to_rfc3339(), "2026-10-20T22:00:00+00:00");
    assert!(time::offset("2026-10-20T19:00:00.000+0100").is_some());
    let mut after = d.clone();
    after.event.warnings.push("venue_unavailable".into());
    assert_eq!(
        pinakes_core::changes::normalize(&d),
        pinakes_core::changes::normalize(&after)
    );
    d.end = Some(DateTime::<Utc>::MAX_UTC);
    assert!(std::panic::catch_unwind(|| pinakes_core::scheduler::next(
        &pinakes_core::adapter::manifest("web"),
        &d,
        now(),
        &Default::default()
    ))
    .is_ok());
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn plain_pages_have_no_discovery_and_advertised_urls_share_ttl(pool: PgPool) {
    let transport = Arc::new(ProtocolTransport {
        seen: Mutex::new(vec![]),
    });
    let egress = Egress::with_backends(
        EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        },
        Arc::new(PublicDns),
        transport.clone(),
    );
    let service = Arc::new(pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: EventConfig {
            geocoding: false,
            ..Default::default()
        },
        osm: Arc::new(OsmResolver::new(pool, OsmConfig::default()).unwrap()),
    });
    let resolver = WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        egress,
        config: Default::default(),
        events: Some(service),
    };
    resolver.resolve("https://example.org/plain").await.unwrap();
    assert_eq!(transport.seen.lock().unwrap().len(), 1);
    for _ in 0..2 {
        resolver.resolve("https://example.org/event").await.unwrap();
    }
    let seen = transport.seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|(p, _, _)| p == "/federation/m/1")
            .count(),
        1
    );
    assert_eq!(
        seen.iter()
            .filter(|(p, a, _)| p == "/event" && a.is_some())
            .count(),
        1
    );
}

struct FailedTls(Mutex<Vec<String>>);
#[async_trait::async_trait]
impl Transport for FailedTls {
    async fn get(&self, url: &url::Url, _: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.0.lock().unwrap().push(url.scheme().into());
        anyhow::bail!("wrong version number")
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn webcals_never_downgrades_to_plaintext(pool: PgPool) {
    let transport = Arc::new(FailedTls(Mutex::new(vec![])));
    let egress = Egress::with_backends(
        EgressConfig::default(),
        Arc::new(PublicDns),
        transport.clone(),
    );
    let service = pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: EventConfig::default(),
        osm: Arc::new(OsmResolver::new(pool, OsmConfig::default()).unwrap()),
    };
    assert!(service
        .fetch(&egress, "webcals://example.org/f.ics")
        .await
        .is_err());
    assert_eq!(*transport.0.lock().unwrap(), vec!["https"]);
}

/// An event page's performers record the address its fetch ended on, on every path, the page's
/// JSON-LD and the fallback for self-hosted event software (here Mobilizon, named in the page)
/// alike: a Songkick address that redirected to another site never lends Songkick's authority.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn performers_record_where_the_fetch_ended_on_every_path(pool: PgPool) {
    let egress = Egress::with_backends(
        EgressConfig::default(),
        Arc::new(PublicDns),
        Arc::new(FailedTls(Mutex::new(vec![]))),
    );
    let service = pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: EventConfig::default(),
        osm: Arc::new(OsmResolver::new(pool, OsmConfig::default()).unwrap()),
    };
    let asked = "https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena";
    let body = format!(
        r#"<html><head><title>Gig</title><script type="application/ld+json">{{"@type":"MusicEvent","name":"Radiohead","url":"{asked}","startDate":"2027-05-03T19:30:00+08:00","performer":{{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://www.songkick.com/artists/253846-radiohead"}}}}</script></head><body>Powered by Mobilizon</body></html>"#
    );
    // Off Songkick, the page's own node is not this page's, so only the fallback (which reads
    // the page against the address asked for) finds the performer: it still records where the
    // fetch ended.
    for ended in ["https://evil.example/gig", asked] {
        let fetched = Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url::Url::parse(ended).unwrap(),
            content_type: "text/html".into(),
            status: 200,
            retry_after_secs: 60,
            location: None,
            body: body.clone().into_bytes(),
        };
        let doc = service
            .page(
                asked,
                &fetched,
                ResourceMeta::new(asked, "web", 3600),
                &egress,
                tokio::time::Instant::now() + std::time::Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(doc.event.performers.len(), 1, "{ended}");
        assert_eq!(doc.event.performers_from(), Some(ended));
    }
}
struct XmlTransport(reqwest::Client, String);
#[async_trait::async_trait]
impl Transport for XmlTransport {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &Default::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        options: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        assert!(
            options.feed_cap.is_none(),
            "non-feed URL must retain the page cap"
        );
        pinakes_core::resolver::egress::read_response_with(
            url.clone(),
            self.0.get(&self.1).send().await?,
            options,
        )
        .await
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn xml_past_the_page_cap_is_cut_and_never_a_container(pool: PgPool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                [("content-type", "application/xml")],
                format!("<root>{}</root>", "x".repeat(2 * 1024 * 1024)),
            )
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let egress = Egress::with_backends(
        Default::default(),
        Arc::new(PublicDns),
        Arc::new(XmlTransport(
            reqwest::Client::new(),
            format!("http://{addr}/"),
        )),
    );
    let service = pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: Default::default(),
        osm: Arc::new(OsmResolver::new(pool, OsmConfig::default()).unwrap()),
    };
    // Bounded like a page, and never read as a calendar snapshot.
    let fetched = service
        .fetch(&egress, "https://example.org/page")
        .await
        .unwrap();
    assert!(fetched.truncated);
    assert_eq!(fetched.body.len(), 1024 * 1024);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    assert!(service
        .container("https://example.org/page", &fetched, deadline)
        .await
        .unwrap()
        .is_none());
    server.abort();
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn container_caps_patches_rotation_backoff_retention_and_labels(pool: PgPool) {
    let uri = "https://example.org/limits.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = dynamic_parse(uri, &future_calendar(5));
    d.ttl_secs = 7200;
    d.container.as_mut().unwrap().held_retry_secs = 10800;
    let limits = &mut d.container.as_mut().unwrap().limits;
    limits.max_changes = 2;
    limits.max_rows = 6;
    limits.max_new_keys = 2;
    limits.max_holds = 3;
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let changes: Vec<Value> = sqlx::query_scalar(
        "SELECT after FROM resource_change WHERE source='event' AND class='details'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["occurrence_changes"], 5);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "burst is one feed-level change including the parent creation"
    );
    let mut renamed = fresh(&d);
    renamed.container.as_mut().unwrap().occurrences[0].doc.name = Some("Renamed".into());
    store::save_document(&pool, &id, 0, &renamed).await.unwrap();
    let patch: Value = sqlx::query_scalar(
        "SELECT after FROM resource_change WHERE class='renamed' ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(patch["name"], "Renamed");
    assert!(patch.get("description").is_none() && patch.get("start").is_none());
    assert!(patch.as_object().unwrap().len() <= 2);
    // Rotating a single key (<50%) twice would exceed the total row cap.
    let mut churn = fresh(&renamed);
    churn.container.as_mut().unwrap().occurrences[0].uid = "rotated1".into();
    store::save_document(&pool, &id, 0, &churn).await.unwrap();
    churn.container.as_mut().unwrap().occurrences[0].uid = "rotated2".into();
    for i in 1..=3 {
        churn = fresh(&churn);
        store::save_document(&pool, &id, 0, &churn).await.unwrap();
        let (count,dead,delay):(i32,bool,f64)=sqlx::query_as("SELECT e.hold_count,r.dead_letter,extract(epoch FROM r.next_attempt_at-now())::float8 FROM event_fetch e JOIN resource r ON r.id=e.resource_id WHERE r.id=$1").bind(&id).fetch_one(&pool).await.unwrap();
        assert_eq!(count, i);
        assert_eq!(dead, i == 3);
        assert!(delay >= 10800.0 * 2f64.powi(i - 1) - 2.0, "{delay}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM occurrence")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 6);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM source_state WHERE source LIKE 'calendar:%'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT feed_held_total FROM source_state WHERE source='event'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        3
    );
    sqlx::query("UPDATE occurrence SET start_at=now()-interval '50 days',end_at=now()-interval '49 days' WHERE uid='id0'").execute(&pool).await.unwrap();
    pinakes_core::changes::retain(&pool, 90).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM occurrence")
            .fetch_one(&pool)
            .await
            .unwrap(),
        5
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn links_supersession_and_coordinate_constraints(pool: PgPool) {
    let uri = "https://example.org/link.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = dynamic_parse(uri, &future_calendar(3));
    d.container.as_mut().unwrap().occurrences[1].doc.url = "https://EXAMPLE.org:443/event".into();
    d.container.as_mut().unwrap().occurrences[2].doc.url = "https://foreign.example/event".into();
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let links: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT uid,url FROM occurrence ORDER BY uid")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(links[0].1, None);
    assert_eq!(links[1].1.as_deref(), Some("https://example.org/event"));
    assert_eq!(links[2].1.as_deref(), Some("https://foreign.example/event"));
    let mut next = fresh(&d);
    next.container.as_mut().unwrap().occurrences[0].uid = "replacement".into();
    store::save_document(&pool, &id, 0, &next).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM occurrence WHERE uid='id0'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "superseded"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource_change WHERE class='cancelled'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    let change: Value = sqlx::query_scalar(
        "SELECT after FROM resource_change WHERE occurrence_key='[\"replacement\",\"\"]'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(change["supersedes"], "[\"id0\",\"\"]");
    let invalid = json!({"venue":{"location_source":"geocoded"},"location":{"lat":1,"lon":2}});
    assert!(sqlx::query("UPDATE resource SET doc=$2 WHERE id=$1")
        .bind(&id)
        .bind(&invalid)
        .execute(&pool)
        .await
        .is_err());
    assert!(
        sqlx::query("UPDATE occurrence SET doc=$2 WHERE resource_id=$1")
            .bind(&id)
            .bind(&invalid)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE resource_change SET after=$2 WHERE resource_id=$1")
            .bind(&id)
            .bind(&invalid)
            .execute(&pool)
            .await
            .is_err()
    );
    let invalid = json!({"venue":{"location_source":"osm","location":{"lat":1,"lon":2}}});
    assert!(sqlx::query("UPDATE resource SET doc=$2 WHERE id=$1")
        .bind(&id)
        .bind(&invalid)
        .execute(&pool)
        .await
        .is_err());
    let no_coords = json!({"venue":{"location_source":"geocoded"}});
    assert!(sqlx::query(
        "UPDATE resource SET doc=$2,point=ST_SetSRID(ST_MakePoint(2,1),4326) WHERE id=$1"
    )
    .bind(&id)
    .bind(&no_coords)
    .execute(&pool)
    .await
    .is_err());
    assert!(sqlx::query("UPDATE occurrence SET status='invalid'")
        .execute(&pool)
        .await
        .is_err());
}

#[test]
fn accepted_venue_survives_failed_enrichment_and_floating_date_lists_use_series_zone() {
    let mut old=parse(&cal(&event("UID:v\nDTSTART:20261024T090000\nGEO:47;8\nLOCATION:Main Street 1, 8000 Zurich, Switzerland"))).container.unwrap().occurrences[0].doc.clone();
    old.location = None;
    let venue = old.event.venue.as_mut().unwrap();
    venue.location = None;
    venue.osm_id = Some("node/123".into());
    venue.osm_version = Some("accepted".into());
    venue.location_source = Some(pinakes_core::model::LocationSource::Geocoded);
    let mut retry = old.clone();
    retry.event.venue.as_mut().unwrap().osm_version = None;
    retry.event.warnings.push("geocoding budget".into());
    pinakes_core::event::storage::preserve_venue(&mut retry, &old);
    assert_eq!(
        pinakes_core::changes::normalize(&old),
        pinakes_core::changes::normalize(&retry)
    );
    retry.event.venue.as_mut().unwrap().osm_version = None;
    retry.event.venue.as_mut().unwrap().text = "New address".into();
    pinakes_core::event::storage::preserve_venue(&mut retry, &old);
    assert!(retry.event.venue.as_ref().unwrap().osm_version.is_none());
    let body=cal(&event("UID:z\nDTSTART:20261024T090000\nGEO:47;8\nRRULE:FREQ=DAILY;COUNT=4\nEXDATE:20261025T090000\nRDATE:20261028T090000\nSTATUS:TENTATIVE\nSUMMARY:A title longer than seventy five octets is accepted because feeds often do not fold lines correctly"));
    let d = parse(&body.replace('\n', "\r\n"));
    let c = d.container.unwrap();
    assert_eq!(c.occurrences.len(), 4);
    assert_eq!(
        c.occurrences[3].doc.start.unwrap().to_rfc3339(),
        "2026-10-28T08:00:00+00:00"
    );
    assert!(!c
        .occurrences
        .iter()
        .any(|o| o.doc.start.unwrap().date_naive().to_string() == "2026-10-25"));
    assert_eq!(
        c.occurrences[0].doc.event.event_status,
        Some(EventStatus::Scheduled)
    );
    let ttl = parse(&body.replace("VERSION:2.0", "VERSION:2.0\nX-PUBLISHED-TTL:PT6H"));
    assert_eq!(ttl.container.unwrap().feed_ttl, Some(21600));
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn occurrences_signed_paging_windows_and_indexed_views(pool: PgPool) {
    let uri = "https://example.org/page.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = dynamic_parse(uri, &future_calendar(5));
    // Equal starts exercise every keyset component, including a recurrence tie.
    let first = d.container.as_ref().unwrap().occurrences[0].doc.start;
    for o in &mut d.container.as_mut().unwrap().occurrences {
        o.doc.start = first;
    }
    d.container.as_mut().unwrap().occurrences[1].uid = "id0".into();
    d.container.as_mut().unwrap().occurrences[1].recurrence_id = "second".into();
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let app = api::router(api::AppState::new(pool.clone(), Config::default()).unwrap());
    let mut next = format!("/v1/resources/{id}/occurrences?limit=2");
    let mut keys = std::collections::BTreeSet::new();
    let mut first_cursor = None;
    loop {
        let (code, v) = call(&app, &next).await;
        assert_eq!(code, 200, "{v}");
        for o in v["items"].as_array().unwrap() {
            assert!(keys.insert((
                o["occurrence"]["uid"].clone().to_string(),
                o["occurrence"]["recurrence_id"].clone().to_string()
            )));
            assert!(o.get("search_text").is_none());
        }
        let Some(cursor) = v["cursor"].as_str() else {
            break;
        };
        first_cursor.get_or_insert(cursor.to_owned());
        next = format!("/v1/resources/{id}/occurrences?limit=2&cursor={cursor}");
    }
    assert_eq!(keys.len(), 5);
    let cursor = first_cursor.unwrap();
    for query in [
        "limit=501".to_owned(),
        "limit=0".into(),
        format!("cursor=x{cursor}"),
        format!("cursor={cursor}&from=2000-01-01T00:00:00Z"),
    ] {
        assert_eq!(
            call(&app, &format!("/v1/resources/{id}/occurrences?{query}"))
                .await
                .0,
            400
        );
    }
    let foreign = store::ensure_resource(&pool, "https://example.org/other.ics")
        .await
        .unwrap();
    assert_eq!(
        call(
            &app,
            &format!("/v1/resources/{foreign}/occurrences?cursor={cursor}")
        )
        .await
        .0,
        400
    );
    let at = first
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (code, v) = call(
        &app,
        &format!("/v1/resources/{id}/occurrences?from={at}&to={at}"),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 5);
    let (code, v) = call(
        &app,
        &format!(
            "/v1/resources/{id}/occurrences?from=2000-01-01T00:00:00Z&to=2001-01-01T00:00:00Z"
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
    sqlx::query("UPDATE event_fetch SET horizon_start=now()+interval '1 year'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&app, &format!("/v1/resources/{id}/occurrences"))
            .await
            .1["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan=off")
        .execute(&mut *tx)
        .await
        .unwrap();
    for (query,index) in [
        ("EXPLAIN SELECT id FROM resource_document WHERE point && ST_MakeEnvelope(7,46,9,48,4326)","resource_point_idx"),
        ("EXPLAIN SELECT resource_id FROM event_occurrence WHERE effective_point && ST_MakeEnvelope(7,46,9,48,4326)","occurrence_point_idx"),
        ("EXPLAIN SELECT resource_id FROM event_occurrence WHERE search_text @@ plainto_tsquery('simple','event')","occurrence_search_idx"),
        ("EXPLAIN SELECT resource_id FROM event_occurrence WHERE ST_DWithin(effective_point::geography,ST_SetSRID(ST_MakePoint(8,47),4326)::geography,1000)","osm_place_geography_idx"),
    ] {
        let plan:Vec<String>=sqlx::query_scalar(query).fetch_all(&mut *tx).await.unwrap();
        assert!(plan.join("\n").contains(index),"{index}: {plan:?}");
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_occurrence_lookup_pages_several_feeds_in_time_order(pool: PgPool) {
    let mut feeds = vec![];
    let calendar = future_calendar(3);
    for uri in ["https://example.org/a.ics", "https://example.org/b.ics"] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = dynamic_parse(uri, &calendar);
        // Both feeds share one start, so the feed and then the UID break the ties.
        let first = dynamic_parse(uri, &calendar).container.unwrap().occurrences[0]
            .doc
            .start;
        for o in &mut d.container.as_mut().unwrap().occurrences {
            o.doc.start = first;
        }
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        feeds.push(id);
    }
    let unknown = "0".repeat(32);
    let app = api::router(api::AppState::new(pool.clone(), Config::default()).unwrap());
    let ask = json!({"resource_ids":[feeds[1], unknown, feeds[0], feeds[1]],"limit":2});
    let (mut body, mut seen, mut first_cursor) = (ask.clone(), vec![], None);
    loop {
        let (code, v) = send(&app, "POST", "/v1/occurrences/lookup", Some(body.clone())).await;
        assert_eq!(code, 200, "{v}");
        for o in v["items"].as_array().unwrap() {
            seen.push((
                o["start"].as_str().unwrap().to_owned(),
                o["id"].as_str().unwrap().to_owned(),
                o["occurrence"]["uid"].as_str().unwrap().to_owned(),
            ));
        }
        let Some(cursor) = v["cursor"].as_str() else {
            break;
        };
        first_cursor.get_or_insert(cursor.to_owned());
        body["cursor"] = json!(cursor);
    }
    assert_eq!(seen.len(), 6, "{seen:?}");
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seen, sorted, "time, then feed, then UID; each once");
    let cursor = first_cursor.unwrap();
    let same = json!([feeds[0], unknown, feeds[1]]);
    for bad in [
        json!({"resource_ids":[]}),
        json!({"resource_ids":vec![unknown.clone(); 501]}),
        json!({"resource_ids":["x"]}),
        json!({"resource_ids":[feeds[0]],"limit":0}),
        json!({"resource_ids":[feeds[0]],"from":"tomorrow"}),
        json!({"resource_ids":[feeds[0]],"cursor":cursor}),
        json!({"resource_ids":same,"cursor":format!("x{cursor}")}),
        json!({"resource_ids":same,"cursor":cursor,"from":"2000-01-01T00:00:00Z"}),
    ] {
        let (code, v) = send(&app, "POST", "/v1/occurrences/lookup", Some(bad.clone())).await;
        assert_eq!(code, 400, "{bad}: {v}");
    }
    // The same IDs in another order, without the duplicate or the unknown ID, keep the cursor.
    for ids in [same.clone(), json!([feeds[1], feeds[0]])] {
        let (code, v) = send(
            &app,
            "POST",
            "/v1/occurrences/lookup",
            Some(json!({"resource_ids":ids,"limit":2,"cursor":cursor})),
        )
        .await;
        assert_eq!(code, 200, "{v}");
    }
    // A feed's own cursor is not the lookup's.
    let (_, own) = call(
        &app,
        &format!("/v1/resources/{}/occurrences?limit=1", feeds[0]),
    )
    .await;
    // Each occurrence names its feed's URI: the link to send to Nexus.
    assert_eq!(
        own["items"][0]["occurrence"]["feed_uri"],
        "https://example.org/a.ics"
    );
    let own = own["cursor"].as_str().unwrap();
    let (code, _) = send(
        &app,
        "POST",
        "/v1/occurrences/lookup",
        Some(json!({"resource_ids":[feeds[0]],"cursor":own})),
    )
    .await;
    assert_eq!(code, 400);
    let (code, v) = send(
        &app,
        "POST",
        "/v1/occurrences/lookup",
        Some(json!({"resource_ids":[unknown]})),
    )
    .await;
    assert_eq!((code, v), (200, json!({"items":[],"cursor":null})));
    // Links read as their IDs, alone or with IDs; an alias spelling reads its root, and an
    // unknown link adds nothing. The cursor belongs to the resolved IDs, however they were named.
    let all = |body: Value| {
        let app = app.clone();
        async move {
            let (code, v) = send(&app, "POST", "/v1/occurrences/lookup", Some(body)).await;
            assert_eq!(code, 200, "{v}");
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| (o["id"].clone(), o["occurrence"]["uid"].clone()))
                .collect::<Vec<_>>()
        }
    };
    let by_ids = all(json!({"resource_ids":[feeds[0], feeds[1]]})).await;
    assert_eq!(by_ids.len(), 6);
    let (a, b) = ("https://example.org/a.ics", "https://example.org/b.ics");
    assert_eq!(all(json!({"uris":[a, b]})).await, by_ids);
    assert_eq!(
        all(json!({"resource_ids":[feeds[0]],"uris":[b]})).await,
        by_ids
    );
    assert_eq!(
        all(json!({"uris":[a, b, "https://example.org/never-seen.ics"]})).await,
        by_ids
    );
    let variant = "https://example.org/a.ics?utm_source=newsletter";
    let alias = store::ensure_resource(&pool, variant).await.unwrap();
    assert_ne!(alias, feeds[0]);
    assert_eq!(
        all(json!({"uris":[variant]})).await,
        all(json!({"resource_ids":[feeds[0]]})).await
    );
    let (_, page) = send(
        &app,
        "POST",
        "/v1/occurrences/lookup",
        Some(json!({"uris":[a, b],"limit":2})),
    )
    .await;
    let (code, _) = send(
        &app,
        "POST",
        "/v1/occurrences/lookup",
        Some(json!({"resource_ids":[feeds[1], feeds[0]],"limit":2,"cursor":page["cursor"]})),
    )
    .await;
    assert_eq!(code, 200);
    // Aliases resolve before the cursor's scope is taken: a spelling of the same feed keeps it.
    let (code, v) = send(
        &app,
        "POST",
        "/v1/occurrences/lookup",
        Some(json!({"uris":[variant, b],"limit":2,"cursor":page["cursor"]})),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    for bad in [
        json!({}),
        json!({"uris":[]}),
        json!({"resource_ids":vec![unknown.clone(); 250],"uris":vec![a; 251]}),
    ] {
        let (code, v) = send(&app, "POST", "/v1/occurrences/lookup", Some(bad.clone())).await;
        assert_eq!(code, 400, "{bad}: {v}");
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn disabled_calendars_use_web_queue_and_preview(pool: PgPool) {
    let cfg = Config {
        event: EventConfig {
            calendars: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let uri = "https://example.org/disabled.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let service = Arc::new(pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: cfg.event.clone(),
        osm: Arc::new(OsmResolver::new(pool.clone(), Default::default()).unwrap()),
    });
    let web = pinakes_core::resolver::web::WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        events: Some(service),
        egress: Egress::new(cfg.egress.clone()),
        config: cfg.web.clone(),
    };
    assert_eq!(
        pinakes_core::resolver::classify_config(uri, &cfg),
        Some("web")
    );
    assert!(web.matches(uri));
    let jobs = pinakes_core::queue::claim_sources(&pool, &web.queue_sources(), &cfg.queue, 1)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, id);
    store::save_document(
        &pool,
        &id,
        jobs[0].generation,
        &ResourceMeta::new(uri, "web", 86400),
    )
    .await
    .unwrap();
    let on = Config::default();
    assert_eq!(
        pinakes_core::resolver::classify_config(uri, &on),
        Some("event")
    );
    let app = api::router(api::AppState::new(pool, cfg).unwrap());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .extension(ConnectInfo("127.0.0.1:1111".parse::<SocketAddr>().unwrap()))
                .uri("/v1/resolve")
                .header("content-type", "application/json")
                .body(Body::from(json!({"uri":uri}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn change_cap_includes_parent_metadata_change(pool: PgPool) {
    let uri = "https://example.org/combined.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = dynamic_parse(uri, &future_calendar(2));
    d.container.as_mut().unwrap().limits.max_changes = 2;
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let seq: i64 = sqlx::query_scalar("SELECT max(seq) FROM resource_change")
        .fetch_one(&pool)
        .await
        .unwrap();
    d = fresh(&d);
    d.name = Some("Changed feed name".into());
    for o in &mut d.container.as_mut().unwrap().occurrences {
        o.doc.name = Some("New title".into());
    }
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let rows: Vec<Value> = sqlx::query_scalar("SELECT after FROM resource_change WHERE seq>$1")
        .bind(seq)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["occurrence_changes"], 2);
    assert_eq!(rows[0]["resource_changes"], 1);
    assert_eq!(rows[0]["change_count"], 3);
    assert_eq!(rows[0]["resource_change"]["class"], "renamed");
}

struct ConditionalSnapshots {
    remaining: usize,
    unconditional: bool,
    calls: Mutex<Vec<(bool, u16)>>,
}
#[async_trait::async_trait]
impl Transport for ConditionalSnapshots {
    async fn get(
        &self,
        url: &url::Url,
        addresses: &[std::net::SocketAddr],
    ) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &Default::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[std::net::SocketAddr],
        o: &pinakes_core::resolver::egress::FetchOptions,
    ) -> anyhow::Result<Fetched> {
        let mut calls = self.calls.lock().unwrap();
        let first = calls.is_empty();
        let etag = if first { "first" } else { "removed" };
        let conditional = o.validators.etag.is_some();
        let status = if !self.unconditional && o.validators.etag.as_deref() == Some(etag) {
            304
        } else {
            200
        };
        calls.push((conditional, status));
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: pinakes_core::resolver::egress::Validators {
                etag: Some(etag.into()),
                ..Default::default()
            },
            url: url.clone(),
            content_type: "text/calendar".into(),
            status,
            retry_after_secs: 60,
            location: None,
            body: if status == 304 {
                vec![]
            } else {
                future_calendar(if first { 5 } else { self.remaining }).into_bytes()
            },
        })
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn conditional_feed_confirms_small_loss_and_held_snapshot_through_queue(pool: PgPool) {
    use pinakes_core::{config::QueueConfig, queue};
    for (remaining, unconditional) in [(4, false), (1, false), (4, true), (1, true)] {
        let uri = format!("https://example.org/conditional-{remaining}-{unconditional}.ics");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let t = Arc::new(ConditionalSnapshots {
            remaining,
            unconditional,
            calls: Mutex::default(),
        });
        let resolver = WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress: Egress::with_backends(
                EgressConfig {
                    host_requests_per_second: 1000.0,
                    ..Default::default()
                },
                Arc::new(PublicDns),
                t.clone(),
            ),
            config: Default::default(),
            events: Some(Arc::new(pinakes_core::event::service::EventService {
                pool: pool.clone(),
                config: EventConfig {
                    geocoding: false,
                    ..Default::default()
                },
                osm: Arc::new(OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap()),
            })),
        };
        for round in 0..4 {
            sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
            let job = queue::claim(&pool, "event", &QueueConfig::default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(job.id, id);
            queue::run_job(&pool, &resolver, &QueueConfig::default(), &job)
                .await
                .unwrap();
            let cancelled: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM occurrence WHERE resource_id=$1 AND status='cancelled'",
            )
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                cancelled,
                if round < 2 { 0 } else { (5 - remaining) as i64 },
                "round {round}, remaining {remaining}, unconditional {unconditional}"
            );
        }
        assert_eq!(
            *t.calls.lock().unwrap(),
            vec![
                (false, 200),
                (true, 200),
                (false, 200),
                (true, if unconditional { 200 } else { 304 })
            ]
        );
        let pending: bool = sqlx::query_scalar("SELECT held_keys IS NOT NULL OR missing_keys<>'{}' FROM event_fetch WHERE resource_id=$1").bind(&id).fetch_one(&pool).await.unwrap();
        assert!(!pending);
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM resource_change WHERE resource_id=$1 AND class='cancelled'",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, (5 - remaining) as i64);
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn webcal_url_less_occurrence_never_links_https_twin(pool: PgPool) {
    let uri = "webcal://example.org/calendar.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    store::ensure_resource(&pool, "https://example.org/calendar.ics")
        .await
        .unwrap();
    let doc = dynamic_parse(uri, &future_calendar(1));
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let link: Option<String> =
        sqlx::query_scalar("SELECT url FROM occurrence WHERE resource_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(link, None);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn repeated_key_growth_confirms_but_total_row_limit_stays_hard(pool: PgPool) {
    let uri = "https://example.org/growth.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = dynamic_parse(uri, &future_calendar(1));
    d.container.as_mut().unwrap().limits.max_new_keys = 1;
    d.container.as_mut().unwrap().limits.max_rows = 4;
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let mut more = fresh(&dynamic_parse(uri, &future_calendar(4)));
    more.fetched_at = d.fetched_at + 1;
    more.container.as_mut().unwrap().limits = d.container.as_ref().unwrap().limits.clone();
    store::save_document(&pool, &id, 0, &more).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM occurrence")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    more = fresh(&more);
    store::save_document(&pool, &id, 0, &more).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM occurrence")
            .fetch_one(&pool)
            .await
            .unwrap(),
        4
    );
    let holds: i32 = sqlx::query_scalar("SELECT hold_count FROM event_fetch WHERE resource_id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(holds, 0);
}

#[test]
/// A broken custom zone does not fail its occurrences: its UTC series expand at their published
/// instants with a warning, and the good sibling stays.
fn unconvertible_custom_zone_series_expand_in_utc_beside_good_siblings() {
    let zone = "BEGIN:VTIMEZONE\nTZID:Broken\nEND:VTIMEZONE";
    let body = cal(&[zone.into(),event("UID:broken-recurring\nDTSTART;VALUE=DATE;TZID=Broken:20261020T000000Z\nDURATION:P1D\nRDATE:20261021T000000Z\nSUMMARY:Bad reverse zone"), event("UID:broken-single\nDTSTART;VALUE=DATE;TZID=Broken:20261020T000000Z\nDURATION:P1D"),event("UID:good\nDTSTART:20261022T180000Z\nSUMMARY:Good sibling")].join("\n"));
    let c = parse(&body).container.unwrap();
    // A zone that cannot convert DTSTART expands its series in UTC (the published
    // instants) with a warning, instead of dropping it or reading UTC wall times as local.
    let mut rows: Vec<(String, String)> = c
        .occurrences
        .iter()
        .map(|o| (o.uid.clone(), o.doc.start.unwrap().to_rfc3339()))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            (
                "broken-recurring".into(),
                "2026-10-20T00:00:00+00:00".into()
            ),
            (
                "broken-recurring".into(),
                "2026-10-21T00:00:00+00:00".into()
            ),
            ("broken-single".into(), "2026-10-20T00:00:00+00:00".into()),
            ("good".into(), "2026-10-22T18:00:00+00:00".into()),
        ]
    );
    assert!(c.warnings.iter().any(|w| w == "series_zone_unconvertible"));
}
#[test]
fn retained_revision_memory_is_bounded() {
    let cfg = EventConfig {
        max_event_revisions: 1,
        ..Default::default()
    };
    let body = cal(&[
        event("UID:a\nDTSTART:20261020T000000Z"),
        event("UID:b\nDTSTART:20261021T000000Z"),
    ]
    .join("\n"));
    assert!(
        calendar::parse("https://example.org/f.ics", body.as_bytes(), &cfg, now())
            .unwrap_err()
            .to_string()
            .contains("revision cap")
    );
    let cfg = EventConfig {
        max_event_document_bytes: 1024,
        ..Default::default()
    };
    let body = cal(&event(&format!(
        "UID:a\nDTSTART:20261020T000000Z\nDESCRIPTION:{}",
        "x".repeat(2048)
    )));
    assert!(
        calendar::parse("https://example.org/f.ics", body.as_bytes(), &cfg, now())
            .unwrap_err()
            .to_string()
            .contains("memory cap")
    );
}

#[test]
fn structured_feed_work_is_bounded() {
    // Many dated items must stop at insertion, before materialising an unbounded tuple list.
    let rss = format!(
        "<rss><channel>{}</channel></rss>",
        "<item><guid>id</guid><title>Fixture</title><start>2026-10-20T18:00:00Z</start></item>"
            .repeat(1000)
    );
    let atom = format!(
        "<feed>{}</feed>",
        "<entry><id>id</id><title>Fixture</title><start>2026-10-20T18:00:00Z</start></entry>"
            .repeat(1000)
    );
    let json=json!({"items":vec![json!({"id":"id","title":"Fixture","event":{"start":"2026-10-20T18:00:00Z"}});1000]}).to_string();
    for body in [rss, atom, json] {
        let cfg = EventConfig {
            max_occurrences_per_feed: 2,
            ..Default::default()
        };
        assert!(calendar::structured_feed(
            "https://example.org/f.ics",
            body.as_bytes(),
            &cfg,
            now()
        )
        .unwrap_err()
        .to_string()
        .contains("occurrence cap"));
        let cfg = EventConfig {
            parse_timeout_ms: 0,
            ..Default::default()
        };
        assert!(calendar::structured_feed(
            "https://example.org/f.ics",
            body.as_bytes(),
            &cfg,
            now()
        )
        .unwrap_err()
        .to_string()
        .contains("parse budget"));
    }
    let cfg = EventConfig {
        max_components: 2,
        ..Default::default()
    };
    let body =
        json!({"items":vec![json!({"id":"old","start":"2000-01-01T00:00:00Z"});3]}).to_string();
    assert!(
        calendar::structured_feed("https://example.org/f.ics", body.as_bytes(), &cfg, now())
            .unwrap_err()
            .to_string()
            .contains("item cap")
    );
}

struct ReviewSnapshots {
    responses: Vec<(u16, String, Vec<u8>)>,
    calls: Mutex<Vec<(bool, u16)>>,
}
#[async_trait::async_trait]
impl Transport for ReviewSnapshots {
    async fn get(
        &self,
        url: &url::Url,
        addresses: &[std::net::SocketAddr],
    ) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &Default::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[std::net::SocketAddr],
        options: &pinakes_core::resolver::egress::FetchOptions,
    ) -> anyhow::Result<Fetched> {
        let mut calls = self.calls.lock().unwrap();
        let (status, content_type, body) =
            &self.responses[calls.len().min(self.responses.len() - 1)];
        let conditional = options.validators.etag.is_some();
        let status = if conditional && *status == 200 && content_type == "text/calendar" {
            304
        } else {
            *status
        };
        calls.push((conditional, status));
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            status,
            content_type: content_type.clone(),
            body: if status == 304 { vec![] } else { body.clone() },
            validators: pinakes_core::resolver::egress::Validators {
                etag: Some("static".into()),
                ..Default::default()
            },
            retry_after_secs: 60,
            location: None,
        })
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn conditional_calendar_refreshes_horizon_with_advanced_clock(pool: PgPool) {
    let base = Utc::now();
    let body = cal(&event(&format!(
        "UID:weekly\nDTSTART:{}\nRRULE:FREQ=WEEKLY\nSUMMARY:Weekly",
        base.format("%Y%m%dT%H%M%SZ")
    )));
    let t = Arc::new(ReviewSnapshots {
        responses: vec![(200, "text/calendar".into(), body.into_bytes())],
        calls: Mutex::default(),
    });
    let egress = Egress::with_backends(
        EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        },
        Arc::new(PublicDns),
        t.clone(),
    );
    let service = pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: EventConfig {
            months_forward: 2,
            geocoding: false,
            ..Default::default()
        },
        osm: Arc::new(OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap()),
    };
    let uri = "https://example.org/static.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let fetched = service.fetch_at(&egress, uri, base).await.unwrap();
    let mut doc = calendar::parse(uri, &fetched.body, &service.config, base).unwrap();
    doc.container.as_mut().unwrap().validators = Some(fetched.validators);
    let first_end = doc.container.as_ref().unwrap().horizon_end;
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let persisted: DateTime<Utc> =
        sqlx::query_scalar("SELECT horizon_end FROM event_fetch WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(persisted.timestamp_millis(), first_end.timestamp_millis());
    assert_eq!(
        service
            .fetch_at(&egress, uri, base + Duration::days(1))
            .await
            .unwrap()
            .status,
        304
    );
    let advanced = base + Duration::days(40);
    let fetched = service.fetch_at(&egress, uri, advanced).await.unwrap();
    assert_eq!(fetched.status, 200);
    let mut renewed = calendar::parse(uri, &fetched.body, &service.config, advanced).unwrap();
    renewed.fetched_at = doc.fetched_at + 1;
    renewed.container.as_mut().unwrap().validators = Some(fetched.validators);
    store::save_document(&pool, &id, 0, &renewed).await.unwrap();
    let new: i64 =
        sqlx::query_scalar("SELECT count(*) FROM occurrence WHERE resource_id=$1 AND start_at>$2")
            .bind(&id)
            .bind(first_end)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        new > 0,
        "the stable ETag must not starve future recurrence expansion"
    );
    assert_eq!(
        service
            .fetch_at(&egress, uri, advanced + Duration::days(1))
            .await
            .unwrap()
            .status,
        304
    );
    assert_eq!(
        *t.calls.lock().unwrap(),
        vec![(false, 200), (true, 304), (false, 200), (true, 304)]
    );
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn vanished_feed_confirms_twice_and_recovers_through_queue(pool: PgPool) {
    use pinakes_core::{config::QueueConfig, queue};
    // A 200 that is no longer a feed (HTML) is a wall or a move: the feed is unreachable and
    // keeps its occurrences. 404/410 still confirm after two observations.
    for (platform, status) in [false, true]
        .into_iter()
        .flat_map(|platform| [200, 404, 410].map(|status| (platform, status)))
    {
        let host = if platform {
            "calendar.google.com"
        } else {
            "example.org"
        };
        let path = if platform { "/calendar/ical" } else { "" };
        let uri = format!("https://{host}{path}/vanished-{status}.ics");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let calendar = (200, "text/calendar".into(), future_calendar(5).into_bytes());
        let missing = (
            status,
            "text/html".into(),
            b"<html><title>Gone</title></html>".to_vec(),
        );
        let confirms = status != 200;
        let t = Arc::new(ReviewSnapshots {
            responses: vec![calendar.clone(), missing.clone(), missing, calendar],
            calls: Mutex::default(),
        });
        let resolver = WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress: Egress::with_backends(
                EgressConfig {
                    host_requests_per_second: 1000.0,
                    ..Default::default()
                },
                Arc::new(PublicDns),
                t.clone(),
            ),
            config: Default::default(),
            events: Some(Arc::new(pinakes_core::event::service::EventService {
                pool: pool.clone(),
                config: EventConfig {
                    geocoding: false,
                    ..Default::default()
                },
                osm: Arc::new(OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap()),
            })),
        };
        let resolver: Box<dyn Resolver> = if platform {
            Box::new(pinakes_core::event::adapters::service::PlatformResolver { web: resolver })
        } else {
            Box::new(resolver)
        };
        for round in 0..4 {
            sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
            let job = queue::claim(
                &pool,
                if platform { "event_platform" } else { "event" },
                &QueueConfig::default(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(job.id, id);
            queue::run_job(&pool, resolver.as_ref(), &QueueConfig::default(), &job)
                .await
                .unwrap();
            let cancelled: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM occurrence WHERE resource_id=$1 AND status='cancelled'",
            )
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
            let visible: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM search_unit WHERE id=$1 AND occurrence IS NOT NULL",
            )
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                cancelled,
                if round == 2 && confirms { 5 } else { 0 },
                "HTTP {status}, round {round}"
            );
            assert_eq!(
                visible,
                if round == 2 && confirms { 0 } else { 5 },
                "HTTP {status}, round {round}"
            );
            let state: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
            if !confirms {
                let held = matches!(round, 1 | 2);
                assert_eq!(state == "unreachable", held, "round {round}: {state}");
            }
        }
        assert_eq!(
            *t.calls.lock().unwrap(),
            vec![
                (false, 200),
                (!platform, status),
                (false, status),
                (false, 200)
            ]
        );
    }
}
/// Serves responses in order without validators, so no response becomes a 304.
struct PlainSequence {
    responses: Vec<Vec<u8>>,
    calls: Mutex<usize>,
}
#[async_trait::async_trait]
impl Transport for PlainSequence {
    async fn get(&self, url: &url::Url, _: &[std::net::SocketAddr]) -> anyhow::Result<Fetched> {
        let mut calls = self.calls.lock().unwrap();
        let body = self.responses[(*calls).min(self.responses.len() - 1)].clone();
        *calls += 1;
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            status: 200,
            content_type: "text/calendar".into(),
            body,
            validators: Default::default(),
            retry_after_secs: 60,
            location: None,
        })
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn emptied_feed_is_held_then_confirmed_on_the_second_observation(pool: PgPool) {
    use pinakes_core::{config::QueueConfig, queue};
    // An emptied VCALENDAR at 200 is held on the first observation and confirmed on the
    // second, an hour later.
    let uri = "https://example.org/emptied.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let t = Arc::new(PlainSequence {
        responses: vec![future_calendar(5).into_bytes(), cal("").into_bytes()],
        calls: Mutex::default(),
    });
    let resolver = WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        egress: Egress::with_backends(
            EgressConfig {
                host_requests_per_second: 1000.0,
                ..Default::default()
            },
            Arc::new(PublicDns),
            t.clone(),
        ),
        config: Default::default(),
        events: Some(Arc::new(pinakes_core::event::service::EventService {
            pool: pool.clone(),
            config: EventConfig {
                geocoding: false,
                ..Default::default()
            },
            osm: Arc::new(OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap()),
        })),
    };
    let count = |sql: &'static str| {
        let (pool, id) = (pool.clone(), id.clone());
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    for (round, (holds, cancelled, live)) in
        [(0, 0, 5), (1, 0, 5), (0, 5, 0)].into_iter().enumerate()
    {
        // The clock advances an hour between observations.
        sqlx::query(
            "UPDATE event_fetch SET last_snapshot_at=last_snapshot_at-3600000 WHERE resource_id=$1",
        )
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let job = queue::claim(&pool, "event", &QueueConfig::default())
            .await
            .unwrap()
            .unwrap();
        queue::run_job(&pool, &resolver, &QueueConfig::default(), &job)
            .await
            .unwrap();
        assert_eq!(
            count("SELECT hold_count::bigint FROM event_fetch WHERE resource_id=$1").await,
            holds,
            "round {round}"
        );
        assert_eq!(
            count("SELECT count(*) FROM occurrence WHERE resource_id=$1 AND status='cancelled'")
                .await,
            cancelled,
            "round {round}"
        );
        assert_eq!(
            count("SELECT count(*) FROM occurrence WHERE resource_id=$1 AND status<>'cancelled'")
                .await,
            live,
            "round {round}"
        );
    }
    assert_eq!(*t.calls.lock().unwrap(), 3);
}
#[test]
fn expanded_documents_and_mixed_feeds_obey_budgets() {
    let cfg = EventConfig {
        max_event_document_bytes: 5000,
        ..Default::default()
    };
    let body = cal(&event(&format!(
        "UID:large-weekly\nDTSTART:20261020T120000Z\nRRULE:FREQ=WEEKLY\nDESCRIPTION:{}",
        "x".repeat(2048)
    )));
    assert!(
        calendar::parse("https://example.org/f.ics", body.as_bytes(), &cfg, now())
            .unwrap_err()
            .to_string()
            .contains("expanded event document memory cap")
    );
    for body in [
        json!({"items":vec![json!({"id":"dated","start":"2026-10-20T18:00:00Z","content_text":"x".repeat(2048)});4]}).to_string(),
        format!("<feed>{}</feed>",format!("<entry><id>dated</id><start>2026-10-20T18:00:00Z</start><description>{}</description></entry>","x".repeat(2048)).repeat(4))
    ] {
        assert!(calendar::structured_feed("https://example.org/f.ics",body.as_bytes(),&cfg,now()).unwrap_err().to_string().contains("document memory cap"));
    }
    for body in [
        json!({"items":[{"id":"dated","start":"2026-10-20T18:00:00Z"},{"id":"undated"},{"id":"null","start":null},{"id":"blank","start":" "}]}).to_string(),
        "<feed><entry><id>dated</id><start>2026-10-20T18:00:00Z</start></entry><entry><id>undated</id></entry></feed>".into()
    ] {
        let d=calendar::structured_feed("https://example.org/f.ics",body.as_bytes(),&cfg,now()).unwrap().unwrap();
        assert_eq!(d.container.unwrap().occurrences.len(),1);
        assert!(d.event.warnings.contains(&"undated_feed_item".into()));
    }
    let c = parse(&cal(&format!(
        "REFRESH-INTERVAL:P365D\n{}",
        event("UID:ttl\nDTSTART:20261020T120000Z")
    )))
    .container
    .unwrap();
    assert_eq!(c.feed_ttl, Some(30 * 86400));
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn chunked_occurrence_upsert_remains_one_snapshot(pool: PgPool) {
    let uri = "https://example.org/chunked.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let body = cal(&(0..600)
        .map(|i| {
            event(&format!(
                "UID:chunk-{i}\nDTSTART:{}\nSUMMARY:Chunk {i}",
                (Utc::now() + Duration::hours(i + 24)).format("%Y%m%dT%H%M%SZ")
            ))
        })
        .collect::<Vec<_>>()
        .join("\n"));
    let d = dynamic_parse(uri, &body);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM occurrence WHERE resource_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 600);
    let mut second = fresh(&d);
    for row in &mut second.container.as_mut().unwrap().occurrences {
        row.doc.name = Some("Updated in chunks".into());
    }
    store::save_document(&pool, &id, 0, &second).await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM occurrence WHERE resource_id=$1 AND name='Updated in chunks'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 600);
}

#[test]
fn malformed_structured_feed_dates_keep_healthy_siblings() {
    for invalid in [
        json!("TBD"),
        json!("n/a"),
        json!(1.5),
        json!({"date":"later"}),
    ] {
        let body=json!({"items":[{"id":"bad","start":invalid},{"id":"good","start":"2026-10-20T18:00:00Z"}]}).to_string();
        let d = calendar::structured_feed(
            "https://example.org/feed.json",
            body.as_bytes(),
            &EventConfig::default(),
            now(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(d.container.unwrap().occurrences.len(), 1);
        assert!(d.event.warnings.contains(&"invalid_feed_item_date".into()));
    }
    let body=b"<feed><entry><id>bad</id><start>TBD</start></entry><entry><id>good</id><start>2026-10-20T18:00:00Z</start></entry></feed>";
    let d = calendar::structured_feed(
        "https://example.org/feed.xml",
        body,
        &EventConfig::default(),
        now(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(d.container.unwrap().occurrences.len(), 1);
    assert!(d.event.warnings.contains(&"invalid_feed_item_date".into()));
    assert!(calendar::structured_feed(
        "https://example.org/feed.json",
        br#"{"items":[{"id":"bad","start":"TBD"}]}"#,
        &EventConfig::default(),
        now()
    )
    .unwrap()
    .is_none());
}

/// Occurrence starts of a one-event feed parsed at a fixed `now` (no test may depend on the
/// clock landing near a DST change).
fn starts_at(header: &str, event: &str, now: &str, cfg: &EventConfig) -> Vec<String> {
    let body = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{header}BEGIN:VEVENT\r\nUID:dst-series\r\nSUMMARY:Series\r\n{event}END:VEVENT\r\nEND:VCALENDAR\r\n"
    );
    let doc = calendar::parse(
        "https://example.org/dst.ics",
        body.as_bytes(),
        cfg,
        now.parse().unwrap(),
    )
    .unwrap();
    doc.container
        .unwrap()
        .occurrences
        .iter()
        .map(|o| o.doc.start.unwrap().format("%Y-%m-%dT%H:%MZ").to_string())
        .collect()
}

/// Europe/Zurich leaves summer time on 2026-10-25 at 01:00 UTC (local 02:00–03:00
/// happens twice) and enters it on 2027-03-28 at 01:00 UTC (local 02:00–03:00 does not exist).
#[test]
fn dst_transitions_give_each_series_its_exact_count() {
    let cfg = EventConfig::default();
    let autumn = "2026-10-18T01:18:00Z";
    let x_wr = "X-WR-TIMEZONE:Europe/Zurich\r\n";
    // The mock e2e case: a UTC DTSTART in the repeated hour, wall-clock expanded (X-WR-TIMEZONE),
    // gave three dates for COUNT=2. DTSTART is the first instance itself.
    assert_eq!(
        starts_at(
            x_wr,
            "DTSTART:20261025T011800Z\r\nRRULE:FREQ=DAILY;COUNT=2\r\n",
            autumn,
            &cfg
        ),
        ["2026-10-25T01:18Z", "2026-10-26T01:18Z"]
    );
    // The same with only GEO: UTC expansion, still two.
    assert_eq!(
        starts_at(
            "",
            "DTSTART:20261025T011800Z\r\nRRULE:FREQ=DAILY;COUNT=2\r\nGEO:47;8\r\n",
            autumn,
            &cfg
        ),
        ["2026-10-25T01:18Z", "2026-10-26T01:18Z"]
    );
    // A local series crossing the repeated hour: the ambiguous 02:18 is its first occurrence
    // (summer time, 00:18Z), once.
    let local = "DTSTART;TZID=Europe/Zurich:20261024T021800\r\n";
    assert_eq!(
        starts_at(
            "",
            &format!("{local}RRULE:FREQ=DAILY;COUNT=3\r\n"),
            autumn,
            &cfg
        ),
        [
            "2026-10-24T00:18Z",
            "2026-10-25T00:18Z",
            "2026-10-26T01:18Z"
        ]
    );
    assert_eq!(
        starts_at(
            "",
            &format!("{local}RRULE:FREQ=DAILY;UNTIL=20261026T011800Z\r\n"),
            autumn,
            &cfg
        ),
        [
            "2026-10-24T00:18Z",
            "2026-10-25T00:18Z",
            "2026-10-26T01:18Z"
        ]
    );
    // An EXDATE on the DTSTART instance in the repeated hour removes exactly that date.
    assert_eq!(
        starts_at(
            x_wr,
            "DTSTART:20261025T011800Z\r\nRRULE:FREQ=DAILY;COUNT=2\r\nEXDATE:20261025T011800Z\r\n",
            autumn,
            &cfg
        ),
        ["2026-10-26T01:18Z"]
    );
    // Spring: a nonexistent 02:30 moves forward by the gap (RFC 5545: the offset before it),
    // 03:30 summer time = 01:30Z, still one date per day.
    assert_eq!(
        starts_at(
            "",
            "DTSTART;TZID=Europe/Zurich:20270327T023000\r\nRRULE:FREQ=DAILY;COUNT=3\r\n",
            "2027-03-20T00:00:00Z",
            &cfg
        ),
        [
            "2027-03-27T01:30Z",
            "2027-03-28T01:30Z",
            "2027-03-29T00:30Z"
        ]
    );
}

/// A UTC series expands in UTC unless its zone source is configured for wall-clock
/// expansion (`event.utc_series_wall_clock`).
#[test]
fn utc_series_wall_clock_modes() {
    use pinakes_core::config::UtcSeriesWallClock::{Coordinates, Never, XWrTimezone};
    let autumn = "2026-10-18T00:00:00Z";
    let series = "DTSTART:20261025T004800Z\r\nRRULE:FREQ=DAILY;COUNT=2\r\n";
    let geo = format!("{series}GEO:47;8\r\n");
    let x_wr = "X-WR-TIMEZONE:Europe/Zurich\r\n";
    let utc = ["2026-10-25T00:48Z", "2026-10-26T00:48Z"];
    // 02:48 summer time, then 02:48 winter time.
    let wall = ["2026-10-25T00:48Z", "2026-10-26T01:48Z"];
    for (mode, geo_only, with_x_wr) in [
        (XWrTimezone, utc, wall),
        (Coordinates, wall, wall),
        (Never, utc, utc),
    ] {
        let cfg = EventConfig {
            utc_series_wall_clock: mode,
            ..Default::default()
        };
        assert_eq!(
            starts_at("", &geo, autumn, &cfg),
            geo_only,
            "{mode:?} GEO only"
        );
        assert_eq!(
            starts_at(x_wr, series, autumn, &cfg),
            with_x_wr,
            "{mode:?} X-WR-TIMEZONE"
        );
    }
    assert_eq!(
        EventConfig::default().utc_series_wall_clock,
        XWrTimezone,
        "RFC 5545 unless X-WR-TIMEZONE or TZID says otherwise"
    );
}

/// An Outlook-style embedded VTIMEZONE with Central European rules under a name no IANA lookup
/// knows, so it resolves only through the embedded definition.
const CUSTOM_EUROPE: &str = "BEGIN:VTIMEZONE\r\nTZID:Custom Europe\r\nBEGIN:STANDARD\r\nDTSTART:16010101T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:16010101T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=3\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\n";

/// A UTC DTSTART carrying a TZID that names an embedded VTIMEZONE (not RFC
/// 5545, seen in the wild) expands on that zone's wall clock consistently: DTSTART 08:00Z is
/// 10:00 summer time, so the series stays at 10:00 local, 09:00Z after the change.
#[test]
fn utc_value_with_an_embedded_zone_tzid_expands_consistently() {
    assert_eq!(
        starts_at(
            CUSTOM_EUROPE,
            "DTSTART;TZID=Custom Europe:20261024T080000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\n",
            "2026-10-18T00:00:00Z",
            &EventConfig::default()
        ),
        [
            "2026-10-24T08:00Z",
            "2026-10-25T09:00Z",
            "2026-10-26T09:00Z"
        ]
    );
}

/// UNTIL is an instant. UNTIL=…011000Z is 02:10 in the second pass of the repeated
/// hour; the 02:15 instance that day is its first pass (00:15Z), before UNTIL, so it stays.
#[test]
fn until_in_the_repeated_hour_compares_instants() {
    assert_eq!(
        starts_at(
            "X-WR-TIMEZONE:Europe/Zurich\r\n",
            "DTSTART:20261023T001500Z\r\nRRULE:FREQ=DAILY;UNTIL=20261025T011000Z\r\n",
            "2026-10-18T00:00:00Z",
            &EventConfig::default()
        ),
        [
            "2026-10-23T00:15Z",
            "2026-10-24T00:15Z",
            "2026-10-25T00:15Z"
        ]
    );
}

/// A floating RECURRENCE-ID resolves with the series stamp, like floating RDATE and
/// EXDATE: in a GEO-only UTC series it names the UTC instance, not a Zurich wall time.
#[test]
fn floating_recurrence_id_overrides_the_utc_instance() {
    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:override-series\r\nSUMMARY:Daily\r\nGEO:47;8\r\nDTSTART:20261020T100000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:override-series\r\nSUMMARY:Moved\r\nGEO:47;8\r\nRECURRENCE-ID:20261021T100000\r\nDTSTART:20261021T120000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let doc = calendar::parse(
        "https://example.org/override.ics",
        body.as_bytes(),
        &EventConfig::default(),
        "2026-10-18T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let rows: Vec<(String, String)> = doc
        .container
        .unwrap()
        .occurrences
        .iter()
        .map(|o| {
            (
                o.doc.start.unwrap().format("%Y-%m-%dT%H:%MZ").to_string(),
                o.doc.name.clone().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("2026-10-20T10:00Z".into(), "Daily".into()),
            ("2026-10-21T12:00Z".into(), "Moved".into()),
            ("2026-10-22T10:00Z".into(), "Daily".into())
        ]
    );
}

/// The configured `default_timezone` is a derived zone like coordinates, and UNTIL,
/// RDATE and EXDATE follow the series under every `utc_series_wall_clock` mode.
#[test]
fn default_zone_and_series_parts_per_mode() {
    use pinakes_core::config::UtcSeriesWallClock::{Coordinates, Never, XWrTimezone};
    let now = "2026-10-18T00:00:00Z";
    let series = "DTSTART:20261025T004800Z\r\nRRULE:FREQ=DAILY;COUNT=2\r\n";
    for (mode, expected) in [
        (XWrTimezone, ["2026-10-25T00:48Z", "2026-10-26T00:48Z"]),
        (Coordinates, ["2026-10-25T00:48Z", "2026-10-26T01:48Z"]),
        (Never, ["2026-10-25T00:48Z", "2026-10-26T00:48Z"]),
    ] {
        let cfg = EventConfig {
            utc_series_wall_clock: mode,
            default_timezone: Some("Europe/Zurich".into()),
            ..Default::default()
        };
        assert_eq!(
            starts_at("", series, now, &cfg),
            expected,
            "{mode:?} default zone"
        );
    }
    // X-WR-TIMEZONE feed: UNTIL (UTC), an RDATE and an EXDATE on the series.
    let parts = "DTSTART:20261024T004800Z\r\nRRULE:FREQ=DAILY;UNTIL=20261026T004800Z\r\nRDATE:20261027T004800Z\r\nEXDATE:20261025T004800Z\r\n";
    let x_wr = "X-WR-TIMEZONE:Europe/Zurich\r\n";
    for (mode, expected) in [
        // Wall clock: 26 Oct 02:48 winter time is 01:48Z, after UNTIL.
        (XWrTimezone, vec!["2026-10-24T00:48Z", "2026-10-27T00:48Z"]),
        (Coordinates, vec!["2026-10-24T00:48Z", "2026-10-27T00:48Z"]),
        (
            Never,
            vec![
                "2026-10-24T00:48Z",
                "2026-10-26T00:48Z",
                "2026-10-27T00:48Z",
            ],
        ),
    ] {
        let cfg = EventConfig {
            utc_series_wall_clock: mode,
            ..Default::default()
        };
        assert_eq!(
            starts_at(x_wr, parts, now, &cfg),
            expected,
            "{mode:?} parts"
        );
    }
}

/// Save one parse of `body` under `cfg` as a fresh snapshot.
async fn snapshot(pool: &PgPool, id: &str, uri: &str, body: &str, cfg: &EventConfig) {
    let now: DateTime<Utc> = "2036-10-20T00:00:00Z".parse().unwrap();
    let doc = fresh(&calendar::parse(uri, body.as_bytes(), cfg, now).unwrap());
    store::save_document(pool, id, 0, &doc).await.unwrap();
}

/// In a UTC series a floating EXDATE is UTC, not the calendar's X-WR-TIMEZONE or the
/// configured default zone.
#[test]
fn floating_values_in_a_utc_series_are_utc() {
    use pinakes_core::config::UtcSeriesWallClock::{Never, XWrTimezone};
    let now = "2026-10-18T00:00:00Z";
    let series =
        "DTSTART:20261020T100000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEXDATE:20261021T100000\r\n";
    let expected = ["2026-10-20T10:00Z", "2026-10-22T10:00Z"];
    // Default mode with default_timezone: the series stays UTC, so does the EXDATE.
    let cfg = EventConfig {
        default_timezone: Some("Europe/Zurich".into()),
        ..Default::default()
    };
    assert_eq!(
        starts_at("", series, now, &cfg),
        expected,
        "{XWrTimezone:?}"
    );
    // `never` with an X-WR-TIMEZONE feed.
    let cfg = EventConfig {
        utc_series_wall_clock: Never,
        ..Default::default()
    };
    assert_eq!(
        starts_at("X-WR-TIMEZONE:Europe/Zurich\r\n", series, now, &cfg),
        expected,
        "{Never:?}"
    );
}

/// Embedded zones convert instants in the second pass of the repeated hour, so a UTC
/// UNTIL there does not abort the series and a DTSTART there expands on the zone's wall clock.
#[test]
fn embedded_zone_repeated_hour() {
    let now = "2026-10-18T00:00:00Z";
    let cfg = EventConfig::default();
    // UNTIL 01:10Z is 02:10 in the second pass; the 02:15 first-pass instance (00:15Z) stays.
    assert_eq!(
        starts_at(
            CUSTOM_EUROPE,
            "DTSTART;TZID=Custom Europe:20261023T001500Z\r\nRRULE:FREQ=DAILY;UNTIL=20261025T011000Z\r\n",
            now,
            &cfg
        ),
        ["2026-10-23T00:15Z", "2026-10-24T00:15Z", "2026-10-25T00:15Z"]
    );
    // DTSTART 01:30Z is 02:30 in the second pass: the next day is 02:30 winter time, 01:30Z.
    assert_eq!(
        starts_at(
            CUSTOM_EUROPE,
            "DTSTART;TZID=Custom Europe:20261025T013000Z\r\nRRULE:FREQ=DAILY;COUNT=2\r\n",
            now,
            &cfg
        ),
        ["2026-10-25T01:30Z", "2026-10-26T01:30Z"]
    );
}

/// An embedded VTIMEZONE that cannot convert (a STANDARD block without
/// TZOFFSETTO) leaves a UTC series at its published instants, with a warning.
#[test]
fn an_unconvertible_zone_keeps_the_utc_instants() {
    let odd = "BEGIN:VTIMEZONE\r\nTZID:Odd Zone\r\nBEGIN:STANDARD\r\nDTSTART:16010101T030000\r\nTZOFFSETFROM:+0200\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n";
    let body = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{odd}BEGIN:VEVENT\r\nUID:odd\r\nSUMMARY:Odd\r\nDTSTART;TZID=Odd Zone:20261020T100000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
    let doc = calendar::parse(
        "https://example.org/odd.ics",
        body.as_bytes(),
        &EventConfig::default(),
        "2026-10-18T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let c = doc.container.unwrap();
    let starts: Vec<String> = c
        .occurrences
        .iter()
        .map(|o| o.doc.start.unwrap().format("%Y-%m-%dT%H:%MZ").to_string())
        .collect();
    assert_eq!(
        starts,
        [
            "2026-10-20T10:00Z",
            "2026-10-21T10:00Z",
            "2026-10-22T10:00Z"
        ]
    );
    assert!(c.warnings.iter().any(|w| w == "series_zone_unconvertible"));
}

/// Years are exactly four digits (RFC 5545 `date-fullyear`). Signed or five-digit
/// years in a VTIMEZONE DTSTART, a VEVENT DTSTART, an RDATE or a UNTIL are invalid values scoped to
/// their event: no panic, the event's warning, siblings kept.
#[test]
fn years_have_four_digits() {
    // chrono's `%Y` accepts the dashed forms (signed, or more than four digits); the compact
    // signed forms are refused by chrono itself.
    for bad in [
        "-262143-01-01T00:00:00",
        "-262143-01-01",
        "+12026-10-20",
        "-0001-01-01",
        "-2621430101T000000",
        "02026-10-20",
    ] {
        assert!(time::naive(bad).is_none(), "{bad}");
        assert!(time::offset(&format!("{bad}Z")).is_none(), "{bad}");
    }
    assert!(time::naive("20261020T100000").is_some());
    assert!(time::naive("2026-10-20").is_some());
    assert!(time::offset("2026-10-20T10:00:00+02:00").is_some());
    let good =
        "BEGIN:VEVENT\r\nUID:good\r\nSUMMARY:Good\r\nDTSTART:20261022T180000Z\r\nEND:VEVENT\r\n";
    let edge_zone = "BEGIN:VTIMEZONE\r\nTZID:Edge\r\nBEGIN:STANDARD\r\nDTSTART:-262143-01-01T00:00:00\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n";
    for (case, warning, header, event) in [
        (
            "VTIMEZONE DTSTART",
            "invalid_vevent_document",
            edge_zone,
            "UID:edge\r\nSUMMARY:Edge\r\nDTSTART;TZID=Edge:20261024T100000\r\nRRULE:FREQ=DAILY;COUNT=2\r\n",
        ),
        (
            "VEVENT DTSTART",
            "invalid_vevent_document",
            "",
            "UID:edge\r\nSUMMARY:Edge\r\nDTSTART:+12026-10-24T10:00:00Z\r\n",
        ),
        (
            "RDATE",
            "invalid_vevent_recurrence",
            "",
            "UID:edge\r\nSUMMARY:Edge\r\nDTSTART:20261024T100000Z\r\nRDATE:-0001-01-01T00:00:00Z\r\n",
        ),
        (
            "UNTIL",
            "invalid_vevent_recurrence",
            "",
            "UID:edge\r\nSUMMARY:Edge\r\nDTSTART:20261024T100000Z\r\nRRULE:FREQ=DAILY;UNTIL=-262143-01-01T00:00:00Z\r\n",
        ),
    ] {
        let body = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{header}BEGIN:VEVENT\r\n{event}END:VEVENT\r\n{good}END:VCALENDAR\r\n");
        let doc = calendar::parse(
            "https://example.org/edge.ics",
            body.as_bytes(),
            &EventConfig::default(),
            "2026-10-18T00:00:00Z".parse().unwrap(),
        )
        .unwrap();
        let c = doc.container.unwrap();
        assert!(
            c.occurrences.iter().any(|o| o.uid == "good"),
            "{case}: the sibling is kept"
        );
        assert!(
            c.occurrences.iter().all(|o| o.uid == "good"),
            "{case}: the broken event yields nothing: {:?}",
            c.occurrences.iter().map(|o| (&o.uid, o.doc.start)).collect::<Vec<_>>()
        );
        assert_eq!(c.warnings, [warning], "{case}: the event's warning");
    }
}

/// Leading whitespace and a compact date with `Z` parse, in JSON-LD and in a JSON feed, and a
/// feed's integer timestamps take the same years as its strings.
#[test]
fn whitespace_compact_z_and_bounded_feed_timestamps() {
    use chrono::Datelike;
    let utc = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
    assert_eq!(
        time::offset(" 2026-10-20T19:00:00+02:00"),
        Some(utc("2026-10-20T17:00:00Z"))
    );
    assert_eq!(
        time::parse("\t2026-10-20T19:00:00", Some("Europe/Zurich")),
        Some(utc("2026-10-20T17:00:00Z"))
    );
    assert_eq!(
        time::parse("20261020Z", None),
        Some(utc("2026-10-20T00:00:00Z"))
    );
    assert!(time::naive(" +12026-10-20").is_none());
    assert!(time::offset("2026102Z").is_none());
    assert_eq!(
        time::from_timestamp(253402300799),
        Some(utc("9999-12-31T23:59:59Z"))
    );
    assert_eq!(time::from_timestamp(253402300800), None);
    assert_eq!(time::from_timestamp(-62167219201), None);
    let url = "https://example.org/e".parse().unwrap();
    let mut d = ResourceMeta::new("https://example.org/e", "web", 1);
    page::jsonld(
        &mut d,
        &json!({"@type":"Event","name":"Padded","startDate":" 2026-10-20T19:00:00+02:00"}),
        &url,
    );
    assert_eq!(d.start, Some(utc("2026-10-20T17:00:00Z")));
    let mut d = ResourceMeta::new("https://example.org/e", "web", 1);
    page::jsonld(
        &mut d,
        &json!({"@type":"Event","name":"Padded day","startDate":" 2026-10-20"}),
        &url,
    );
    assert_eq!(d.event.all_day, Some(true), "the trimmed value is a date");
    let feed = json!({"version":"https://jsonfeed.org/version/1","items":[
        {"id":"padded","title":"Padded","event":{"start":" 2026-10-20T19:00:00+02:00"}},
        {"id":"compact","title":"Compact","event":{"start":"20261020Z"}},
        {"id":"far","title":"Far","event":{"start":253402300800_i64}},
        {"id":"far-end","title":"Far end","event":{"start":1792522800,"end":253402300800_i64}}
    ]});
    let c = calendar::structured_feed(
        "https://example.org/json",
        feed.to_string().as_bytes(),
        &EventConfig::default(),
        now(),
    )
    .unwrap()
    .unwrap()
    .container
    .unwrap();
    let start = |uid: &str| {
        c.occurrences
            .iter()
            .find(|o| o.uid == uid)
            .map(|o| (o.doc.start, o.doc.end))
    };
    assert_eq!(
        start("padded").unwrap().0,
        Some(utc("2026-10-20T17:00:00Z"))
    );
    assert_eq!(
        start("compact").unwrap().0,
        Some(utc("2026-10-20T00:00:00Z"))
    );
    assert_eq!(start("far"), None, "a start beyond 9999 is refused");
    // Like an unparsable string start: the item's warning, and the feed-level count of items
    // without a usable date.
    assert_eq!(c.warnings, ["invalid_feed_item_date", "undated_feed_item"]);
    let (_, end) = start("far-end").unwrap();
    assert!(
        end.is_none_or(|e| e.year() <= 9999),
        "an end beyond 9999 is refused: {end:?}"
    );
}

/// An occurrence end or modification time Postgres cannot store (UTC year 0000 or
/// 10000) is dropped with the item's warning instead of failing the whole feed's apply; the good
/// items are stored. ICS: a zoned DTEND late on 9999-12-31, a huge DURATION, a year-0000
/// LAST-MODIFIED. JSON feed: an offset end late on 9999-12-31.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn out_of_range_instants_never_fail_a_feed(pool: PgPool) {
    let uri = "https://example.org/far.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let event = |uid: &str, extra: &str| {
        format!("BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{uid}\r\nDTSTART:20361022T180000Z\r\n{extra}END:VEVENT\r\n")
    };
    let body = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{}{}{}{}END:VCALENDAR\r\n",
        event("good", "DTEND:20361022T200000Z\r\n"),
        event(
            "zoned-end",
            "DTEND;TZID=America/New_York:99991231T235959\r\n"
        ),
        event("duration", "DURATION:P3000000D\r\n"),
        event("modified", "LAST-MODIFIED:00000101T000000Z\r\n"),
    );
    snapshot(&pool, &id, uri, &body, &EventConfig::default()).await;
    let rows = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Value)>(
                "SELECT uid,end_at,modified,coalesce(doc->'warnings','null') FROM occurrence WHERE resource_id=$1 ORDER BY uid",
            )
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    let stored = rows(id.clone()).await;
    let summary: Vec<_> = stored
        .iter()
        .map(|(uid, end, modified, warnings)| {
            (
                uid.as_str(),
                end.is_some(),
                modified.is_some(),
                warnings.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("duration", false, false, json!(["end_out_of_range"])),
            ("good", true, false, Value::Null),
            ("modified", true, false, json!(["modified_out_of_range"])),
            ("zoned-end", false, false, json!(["end_out_of_range"])),
        ],
        "{stored:?}"
    );
    let uri = "https://example.org/far.json";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let feed = json!({"version":"https://jsonfeed.org/version/1","items":[
        {"id":"good","title":"Good","event":{"start":"2026-10-20T19:00:00Z","end":"2026-10-20T21:00:00Z"}},
        {"id":"far-end","title":"Far end","event":{"start":"2026-10-20T19:00:00Z","end":"9999-12-31T23:59:59-05:00"}}
    ]});
    let doc = calendar::structured_feed(
        uri,
        feed.to_string().as_bytes(),
        &EventConfig::default(),
        now(),
    )
    .unwrap()
    .unwrap();
    store::save_document(&pool, &id, 0, &fresh(&doc))
        .await
        .unwrap();
    let summary: Vec<_> = rows(id)
        .await
        .into_iter()
        .map(|(uid, end, _, warnings)| (uid, end.is_some(), warnings))
        .collect();
    assert_eq!(
        summary,
        [
            ("far-end".to_owned(), false, json!(["end_out_of_range"])),
            ("good".to_owned(), true, Value::Null),
        ]
    );
}

/// A padded date-only JSON-LD end is re-anchored to the start's offset like an
/// unpadded one.
#[test]
fn a_padded_date_only_end_is_reanchored() {
    let url = "https://example.org/e".parse().unwrap();
    for end in ["2026-10-22", " 2026-10-22 "] {
        let mut d = ResourceMeta::new("https://example.org/e", "web", 1);
        page::jsonld(
            &mut d,
            &json!({"@type":"Event","name":"Days","startDate":" 2026-10-20T19:00:00+02:00 ","endDate":end}),
            &url,
        );
        assert_eq!(
            d.end,
            Some("2026-10-22T22:00:00Z".parse().unwrap()),
            "{end:?}"
        );
    }
}

/// Times re-derived after `bound_instants` (`preserve_venue` re-applies the stored
/// venue's zone when this refresh's geocode failed) are bounded where the upsert rows are built:
/// a floating 9999-12-31T20:00 at a New York venue (10000-01-01T01:00Z) keeps the stored row and
/// the feed applies.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn times_rederived_after_the_bound_never_reach_the_cast(pool: PgPool) {
    use pinakes_core::event::{Container, Occurrence, Venue};
    let uri = "https://example.org/venue.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let now = Utc::now();
    // A second, unchanged occurrence keeps the snapshot from being held (one key of two lost),
    // so its rows reach the upsert.
    let feed = |talk: &ResourceMeta, other: &ResourceMeta, hash: &str| {
        let item = |uid: &str, doc: &ResourceMeta, hash: &str| Occurrence {
            uid: uid.into(),
            recurrence_id: String::new(),
            doc: doc.clone(),
            sequence: 0,
            modified: None,
            content_hash: hash.into(),
        };
        let mut doc = ResourceMeta::new(uri, "event", 3600);
        doc.set_type("Collection");
        doc.container = Some(Box::new(Container {
            horizon_start: now - Duration::days(30),
            horizon_end: now + Duration::days(365),
            occurrences: vec![item("talk", talk, hash), item("other", other, "other")],
            ..Default::default()
        }));
        fresh(&doc)
    };
    let local = (now + Duration::days(10))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let mut first = ResourceMeta::new("https://example.org/talk", "event", 3600);
    first.set_type("Event");
    first.name = Some("Talk".into());
    first.event.start_local = Some(local.clone());
    first.event.tz = Some("America/New_York".into());
    first.start = time::parse(&local, Some("America/New_York"));
    first.event.venue = Some(Venue {
        text: "Hall".into(),
        osm_version: Some("v1".into()),
        location: Some(pinakes_core::model::Point {
            lat: 40.7,
            lon: -74.0,
        }),
        ..Default::default()
    });
    let mut other = first.clone();
    other.name = Some("Other".into());
    store::save_document(&pool, &id, 0, &feed(&first, &other, "a"))
        .await
        .unwrap();
    // The next refresh: DTSTART moved to the last evening of 9999, and the geocode failed.
    let mut second = first.clone();
    second.event.start_local = Some("9999-12-31T20:00:00".into());
    second.event.tz = None;
    second.start = Some("9999-12-31T20:00:00Z".parse().unwrap());
    second.event.venue = Some(Venue {
        text: "Hall".into(),
        ..Default::default()
    });
    second.event.warnings = vec!["venue_unavailable".into()];
    store::save_document(&pool, &id, 0, &feed(&second, &other, "b"))
        .await
        .unwrap();
    let start: DateTime<Utc> =
        sqlx::query_scalar("SELECT start_at FROM occurrence WHERE resource_id=$1 AND uid='talk'")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(Some(start), first.start, "the stored row stays");
    // The late skip is recorded on the feed, as the early pass does.
    let warnings: Value = sqlx::query_scalar("SELECT doc->'warnings' FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(warnings, json!(["occurrence_start_out_of_range"]));
}

/// The same re-derivation on an event document itself
/// (not a feed row) is bounded after `preserve_venue`, and when that drops the start while the
/// stored start is valid, the save answers `drift` and keeps the stored document, like the early
/// guard and like the occurrence path, which keeps its stored row.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_document_start_rederived_after_the_bound_is_drift(pool: PgPool) {
    use pinakes_core::event::Venue;
    let uri = "https://example.org/talk-page";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let local = (Utc::now() + Duration::days(10))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let local_first = local.clone();
    let mut first = ResourceMeta::new(uri, "web", 3600);
    first.set_type("Event");
    first.name = Some("Talk".into());
    first.event.start_local = Some(local.clone());
    first.event.tz = Some("America/New_York".into());
    first.start = time::parse(&local, Some("America/New_York"));
    first.event.venue = Some(Venue {
        text: "Hall".into(),
        osm_version: Some("v1".into()),
        location: Some(pinakes_core::model::Point {
            lat: 40.7,
            lon: -74.0,
        }),
        ..Default::default()
    });
    store::save_document(&pool, &id, 0, &fresh(&first))
        .await
        .unwrap();
    let mut second = first.clone();
    second.event.start_local = Some("9999-12-31T20:00:00".into());
    second.event.tz = None;
    second.start = Some("9999-12-31T20:00:00Z".parse().unwrap());
    second.event.venue = Some(Venue {
        text: "Hall".into(),
        ..Default::default()
    });
    let error = store::save_document(&pool, &id, 0, &fresh(&second))
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<pinakes_core::resolver::RetryLater>()
            .is_some_and(|r| r.class == "drift"),
        "{error}"
    );
    let (start_at, warnings, local): (Option<DateTime<Utc>>, Value, Option<String>) =
        sqlx::query_as(
            "SELECT start_at,coalesce(doc->'warnings','null'),doc->>'start_local' FROM resource WHERE id=$1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(start_at, first.start, "the stored start stays");
    assert_eq!(local, Some(local_first), "the stored document stays");
    assert_eq!(warnings, Value::Null);
    let lines: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(lines, 1, "only the creation line, no rescheduled line");
}

/// Postgres keeps microseconds and rounds finer values, so 9999-12-31T23:59:59.9999995Z would land
/// in year 10000: instants are truncated before the range check. A JSON-feed end like that is
/// stored in year 9999 and its page reads; the schema refuses a year-10000 value outright.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn sub_microsecond_instants_never_round_past_9999(pool: PgPool) {
    let edge: DateTime<Utc> = "9999-12-31T23:59:59.9999995Z".parse().unwrap();
    let kept: DateTime<Utc> = "9999-12-31T23:59:59.999999Z".parse().unwrap();
    assert_eq!(time::bounded(edge), Some(kept));
    let uri = "https://example.org/edge.json";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let feed = json!({"version":"https://jsonfeed.org/version/1","items":[
        {"id":"edge","title":"Edge","event":{"start":"2026-10-20T19:00:00Z","end":"9999-12-31T23:59:59.9999995Z"}}
    ]});
    let doc = calendar::structured_feed(
        uri,
        feed.to_string().as_bytes(),
        &EventConfig::default(),
        now(),
    )
    .unwrap()
    .unwrap();
    store::save_document(&pool, &id, 0, &fresh(&doc))
        .await
        .unwrap();
    let end: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT end_at FROM occurrence WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(end, Some(kept));
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, page) = call(
        &app,
        &format!("/v1/resources/{id}/occurrences?from=2026-01-01T00:00:00Z"),
    )
    .await;
    assert_eq!(code, 200, "{page}");
    assert_eq!(
        page["items"][0]["end"],
        json!("9999-12-31T23:59:59.999999Z")
    );

    let uri = "https://example.org/modified.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let event = |uid: &str| {
        format!("BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{uid}\r\nDTSTART:20361022T180000Z\r\nEND:VEVENT\r\n")
    };
    let body = |events: &[&str]| {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{}END:VCALENDAR\r\n",
            events.iter().map(|e| event(e)).collect::<String>()
        )
    };
    let cfg = EventConfig::default();
    snapshot(&pool, &id, uri, &body(&["a", "b", "c"]), &cfg).await;
    // The schema itself refuses an instant Postgres rounded into year 10000.
    let refused = sqlx::query("UPDATE occurrence SET modified=make_timestamptz(10000,1,1,0,0,0,'UTC') WHERE resource_id=$1 AND uid='a'")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("occurrence_modified_range"), "{refused}");
}

/// chrono reads 9999-12-31T23:59:60Z as year 9999 while Postgres rolls it into
/// 10000; a leap second is normalised into the next second before the year check.
#[test]
fn a_leap_second_is_normalised_before_the_bound() {
    let leap = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
    assert_eq!(time::bounded(leap("9999-12-31T23:59:60Z")), None);
    assert_eq!(
        time::bounded(leap("2016-12-31T23:59:60.5Z")),
        Some(leap("2017-01-01T00:00:00.5Z"))
    );
    assert_eq!(
        time::bounded(leap("9999-12-31T23:59:59.999999Z")),
        Some(time::latest())
    );
    assert_eq!(time::bounded(time::earliest()), Some(time::earliest()));
}

/// An occurrence card's `status` is the feed's fetch health, as on every card; the instance's
/// lifecycle is `occurrence.status`, the same in the occurrence route and in search.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn occurrence_cards_report_their_lifecycle_beside_the_feed_status(pool: PgPool) {
    let uri = "https://example.org/lifecycle.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let text = future_calendar(2).replace("SUMMARY:Event 1", "SUMMARY:Event 1\nSTATUS:CANCELLED");
    store::save_document(&pool, &id, 0, &dynamic_parse(uri, &text))
        .await
        .unwrap();
    reference(&pool, uri, "/pub/x/tags/1", Kind::Tag, Op::Put).await;
    let app = api::router(api::AppState::new(pool.clone(), Config::default()).unwrap());
    let lifecycle = |items: &Value| {
        let mut seen: Vec<(String, String, String)> = items
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                (
                    o["occurrence"]["uid"].as_str().unwrap().to_owned(),
                    o["occurrence"]["status"].as_str().unwrap().to_owned(),
                    o["status"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        seen.sort();
        seen
    };
    let expected = vec![
        ("id0".to_owned(), "scheduled".to_owned(), "ok".to_owned()),
        ("id1".to_owned(), "cancelled".to_owned(), "ok".to_owned()),
    ];
    let (code, page) = call(&app, &format!("/v1/resources/{id}/occurrences")).await;
    assert_eq!(code, 200, "{page}");
    assert_eq!(lifecycle(&page["items"]), expected, "{page}");
    let (code, found) = call(&app, "/v1/search?type=Event&from=now&sort=start").await;
    assert_eq!(code, 200, "{found}");
    assert_eq!(lifecycle(&found["items"]), expected, "{found}");
}

/// Retention deletes a resource's expired occurrences in a save's order and holds no other
/// resource's row while it waits for one: a writer that holds one feed and then locks another
/// (an ingested batch, a save of each) never waits on it.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn retention_waits_for_a_writer_holding_nothing_else(pool: PgPool) {
    let mut feeds = vec![];
    for name in ["first", "second"] {
        let uri = format!("https://example.org/{name}.ics");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let d = dynamic_parse(&uri, &future_calendar(2));
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        feeds.push(id);
    }
    sqlx::query(
        "UPDATE occurrence SET status='cancelled',start_at=now()-interval '50 days',end_at=now()-interval '49 days'",
    )
    .execute(&pool)
    .await
    .unwrap();
    // The writer holds each feed's row in turn; retention runs between the two.
    for (held, next) in [(&feeds[0], &feeds[1]), (&feeds[1], &feeds[0])] {
        let mut writer = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("UPDATE resource SET group_revision=group_revision+1 WHERE id=$1")
            .bind(held)
            .execute(&mut *writer)
            .await
            .unwrap();
        let retention = tokio::spawn({
            let pool = pool.clone();
            async move { pinakes_core::changes::retain(&pool, 90).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock')")
                    .fetch_one(&pool).await.unwrap();
                if waiting {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        sqlx::query("UPDATE resource SET group_revision=group_revision+1 WHERE id=$1")
            .bind(next)
            .execute(&mut *writer)
            .await
            .unwrap();
        writer.commit().await.unwrap();
        retention.await.unwrap().unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM occurrence")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        // The second round needs expired occurrences again.
        for (id, uri) in [(&feeds[0], "first"), (&feeds[1], "second")] {
            let uri = format!("https://example.org/{uri}.ics");
            let d = dynamic_parse(&uri, &future_calendar(2));
            store::save_document(&pool, id, 0, &fresh(&d))
                .await
                .unwrap();
        }
        sqlx::query("UPDATE occurrence SET status='cancelled',start_at=now()-interval '50 days',end_at=now()-interval '49 days'")
            .execute(&pool)
            .await
            .unwrap();
    }
}

/// `related` lists a feed and a venue once each, as their own cards: never once per occurrence
/// or per event reference (whose authors are other people).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn related_lists_a_feed_and_a_venue_once(pool: PgPool) {
    let feed = "https://example.org/events.ics";
    reference(&pool, feed, "/pub/social/tags/calendar", Kind::Tag, Op::Put).await;
    let feed_id = identity(feed).unwrap().0;
    store::save_document(
        &pool,
        &feed_id,
        0,
        &dynamic_parse(feed, &future_calendar(50)),
    )
    .await
    .unwrap();
    let venue = "geo:47,8";
    let venue_id = store::ensure_resource(&pool, venue).await.unwrap();
    let place = pinakes_core::resolver::geo::GeoResolver
        .resolve(venue)
        .await
        .unwrap();
    store::save_document(&pool, &venue_id, 0, &place)
        .await
        .unwrap();
    for i in 0..50 {
        occurrences_at(
            &pool,
            venue,
            &format!("/pub/social/v1/posts/event{i}"),
            &[(None, Utc::now() + Duration::days(2 + i))],
        )
        .await;
    }
    let seed = "https://example.org/seed";
    reference(&pool, seed, "/pub/social/tags/seed", Kind::Tag, Op::Put).await;
    let seed_id = identity(seed).unwrap().0;
    // A second tagger of the seed, the feed and the venue: two shared taggers each.
    for (i, id) in [&seed_id, &feed_id, &venue_id].into_iter().enumerate() {
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','bitcoin',$3,now())")
            .bind(id)
            .bind("z".repeat(52))
            .bind(format!("/pub/social/tags/second{i}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let units = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM search_unit WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(units(feed_id.clone()).await, 51);
    assert_eq!(units(venue_id.clone()).await, 51);
    // `resource_unit` is exactly `search_unit`'s own units.
    let differ: i64 = sqlx::query_scalar("SELECT count(*) FROM ((SELECT * FROM search_unit WHERE unit_id=id EXCEPT ALL SELECT * FROM resource_unit) UNION ALL (SELECT * FROM resource_unit EXCEPT ALL SELECT * FROM search_unit WHERE unit_id=id)) d")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(differ, 0);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    for collapse in ["", "&collapse=group"] {
        let (code, v) = call(
            &app,
            &format!("/v1/search?related={seed_id}&limit=500{collapse}"),
        )
        .await;
        assert_eq!(code, 200, "{v}");
        let mut ids: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        ids.sort();
        let mut want = vec![feed_id.as_str(), venue_id.as_str()];
        want.sort();
        assert_eq!(ids, want, "{v}");
        assert!(v["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("occurrence").is_none() && row.get("event").is_none()));
    }
}

#[test]
fn a_rule_whose_periods_would_hold_too_many_dates_is_never_expanded() {
    // Every weekday, hour, minute and second of a year: 31 million dates the `rrule` crate would
    // build before giving the first. In capitals, lower case or mixed: as an event's rule, the
    // event keeps its first occurrence with a warning; as an embedded time zone's, the event in
    // that zone is left out with a warning. The sibling stays, at once.
    let every = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let flood = format!(
        "FREQ=YEARLY;BYDAY=MO,TU,WE,TH,FR,SA,SU;BYHOUR={};BYMINUTE={};BYSECOND={}",
        every(24),
        every(60),
        every(60)
    );
    let spellings = [
        flood.clone(),
        flood.to_lowercase(),
        flood
            .replace("BYHOUR", "byhour")
            .replace("BYMINUTE", "ByMinute")
            .replace("BYSECOND", "bySecond"),
    ];
    for rule in &spellings {
        let zone = format!("BEGIN:VTIMEZONE\nTZID:Flood\nBEGIN:STANDARD\nDTSTART:19700101T000000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0100\nRRULE:{rule}\nEND:STANDARD\nEND:VTIMEZONE");
        for bad in [
            format!("UID:bad\nDTSTART:20270101T000000Z\nRRULE:{rule}\nSUMMARY:Flood"),
            "UID:bad\nDTSTART;TZID=Flood:20261020T120000\nSUMMARY:Flood zone".to_owned(),
        ] {
            let body = cal(&format!(
                "{zone}\n{}\n{}",
                event(&bad),
                event("UID:good\nDTSTART:20261020T120000Z\nSUMMARY:Healthy")
            ));
            let began = std::time::Instant::now();
            let container = calendar::parse(
                "https://example.org/feed.ics",
                body.as_bytes(),
                &EventConfig::default(),
                now(),
            )
            .unwrap()
            .container
            .unwrap();
            assert!(
                began.elapsed() < std::time::Duration::from_secs(5),
                "{rule}"
            );
            let mut uids: Vec<&str> = container
                .occurrences
                .iter()
                .map(|o| o.uid.as_str())
                .collect();
            uids.sort();
            if bad.starts_with("UID:bad\nDTSTART:2027") {
                assert_eq!(uids, ["bad", "good"], "{bad}");
                assert!(
                    container
                        .warnings
                        .contains(&"series_rule_unexpanded".to_string()),
                    "{bad}: {:?}",
                    container.warnings
                );
            } else {
                assert_eq!(uids, ["good"], "{bad}");
                assert!(!container.warnings.is_empty(), "{bad}");
            }
        }
    }
    // Ordinary rules pass the bound, however they spell their names, and expand: a yearly rule
    // keeps one day of the year (three times that day), a plain yearly one once, a monthly one
    // a day a month at three times.
    let dates_in_2027 = |rule: &str| {
        let body = cal(&event(&format!(
            "UID:r\nDTSTART:20270101T100000Z\nRRULE:{rule}\nSUMMARY:r"
        )));
        let cfg = EventConfig {
            max_occurrences_per_series: 1000,
            months_forward: 15,
            ..Default::default()
        };
        calendar::parse("https://example.org/f.ics", body.as_bytes(), &cfg, now())
            .unwrap()
            .container
            .unwrap()
            .occurrences
            .iter()
            .filter(|o| o.doc.start.unwrap().to_rfc3339().starts_with("2027"))
            .count()
    };
    assert_eq!(dates_in_2027("FREQ=YEARLY;BYHOUR=10,14,18"), 3);
    assert_eq!(dates_in_2027("FREQ=YEARLY"), 1);
    assert_eq!(
        dates_in_2027("FREQ=MONTHLY;BYMONTHDAY=1;BYHOUR=10,14,18"),
        36
    );
    // The limit is the operator's: at two dates a period, the three-hour rule keeps its first
    // occurrence with the warning.
    let body = cal(&event(
        "UID:r\nDTSTART:20270101T100000Z\nRRULE:FREQ=YEARLY;BYHOUR=10,14,18\nSUMMARY:r",
    ));
    let cfg = EventConfig {
        max_period_dates: 2,
        months_forward: 15,
        ..Default::default()
    };
    let container = calendar::parse("https://example.org/f.ics", body.as_bytes(), &cfg, now())
        .unwrap()
        .container
        .unwrap();
    assert_eq!(container.occurrences.len(), 1);
    assert!(container
        .warnings
        .contains(&"series_rule_unexpanded".to_string()));
    let bounded = |rule: &str, max: usize| {
        let set: rrule::RRuleSet = format!("DTSTART:20261020T120000Z\nRRULE:{rule}")
            .parse()
            .unwrap();
        pinakes_core::event::time::rrule_bounded(&set, max)
    };
    for rule in [
        "FREQ=WEEKLY;BYDAY=MO,WE,FR;BYHOUR=9,18",
        "freq=monthly;byhour=9,18",
        "FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU",
        "FREQ=HOURLY;BYMINUTE=0,30",
        "FREQ=YEARLY;BYHOUR=10,14,18",
        "FREQ=MONTHLY;BYMONTHDAY=-1,-2,-3;BYHOUR=9",
    ] {
        assert!(bounded(rule, 100_000), "{rule}");
    }
    // Never: every hour and minute of every day (527,040 a year); every minute of every day of
    // a month (44,640) under a 10,000 bound; a minutely and a secondly rule.
    let every = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let hourly_minutes = format!(
        "FREQ=YEARLY;BYDAY=MO,TU,WE,TH,FR,SA,SU;BYHOUR={};BYMINUTE={}",
        every(24),
        every(60)
    );
    let monthly_minutes = format!(
        "FREQ=MONTHLY;BYMONTHDAY={};BYHOUR={};BYMINUTE={}",
        (1..=31)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(","),
        every(24),
        every(60)
    );
    for (rule, max) in [
        (flood.as_str(), 100_000),
        (hourly_minutes.as_str(), 100_000),
        (monthly_minutes.as_str(), 10_000),
        ("FREQ=MINUTELY", 100_000),
        ("freq=secondly", 100_000),
    ] {
        assert!(!bounded(rule, max), "{rule}");
    }
}

#[test]
fn a_rule_in_lower_case_is_read_as_one_in_capitals() {
    // A floating UNTIL converts in the series' zone whatever its case: 11:00 Zurich on the 23rd
    // ends the series before that day's 12:00, so three days, not four as a UTC reading gives.
    let days = |rule: &str| {
        parse(&cal(&event(&format!(
            "UID:u\nDTSTART;TZID=Europe/Zurich:20261020T120000\nRRULE:{rule}\nSUMMARY:u"
        ))))
        .container
        .unwrap()
        .occurrences
        .len()
    };
    assert_eq!(days("FREQ=DAILY;UNTIL=20261023T110000"), 3);
    assert_eq!(days("freq=daily;until=20261023T110000"), 3);
    assert_eq!(days("Freq=Daily;Until=20261023T110000"), 3);
    // COUNT and UNTIL together are refused in any case: the event is left out with a warning.
    for rule in [
        "FREQ=DAILY;COUNT=2;UNTIL=20261023T110000",
        "freq=daily;count=2;until=20261023T110000",
    ] {
        let container = parse(&cal(&format!(
            "{}\n{}",
            event(&format!(
                "UID:both\nDTSTART:20261020T120000Z\nRRULE:{rule}\nSUMMARY:both"
            )),
            event("UID:good\nDTSTART:20261020T120000Z\nSUMMARY:Healthy")
        )))
        .container
        .unwrap();
        let uids: Vec<&str> = container
            .occurrences
            .iter()
            .map(|o| o.uid.as_str())
            .collect();
        assert_eq!(uids, ["good"], "{rule}");
        assert!(
            container
                .warnings
                .contains(&"invalid_vevent_recurrence".to_string()),
            "{rule}: {:?}",
            container.warnings
        );
    }
}

#[test]
fn an_event_with_two_rules_or_an_exception_rule_is_left_out() {
    for props in [
        "RRULE:FREQ=DAILY\nRRULE:FREQ=WEEKLY",
        "RRULE:FREQ=DAILY\nEXRULE:FREQ=WEEKLY",
        "EXRULE:FREQ=WEEKLY",
    ] {
        let container = parse(&cal(&format!(
            "{}\n{}",
            event(&format!(
                "UID:rules\nDTSTART:20261020T120000Z\n{props}\nSUMMARY:rules"
            )),
            event("UID:good\nDTSTART:20261020T120000Z\nSUMMARY:Healthy")
        )))
        .container
        .unwrap();
        let uids: Vec<&str> = container
            .occurrences
            .iter()
            .map(|o| o.uid.as_str())
            .collect();
        assert_eq!(uids, ["good"], "{props}");
        assert!(
            container
                .warnings
                .contains(&"invalid_vevent_recurrence".to_string()),
            "{props}: {:?}",
            container.warnings
        );
    }
}

#[test]
fn a_rule_with_a_huge_interval_expands_within_the_deadline() {
    // The library builds one period at a time: a yearly rule every 10,000 years builds its first
    // year, then steps past the calendar's end and stops, well within the parse deadline.
    let began = std::time::Instant::now();
    let container = parse(&cal(&event(
        "UID:i\nDTSTART:20261020T120000Z\nRRULE:FREQ=YEARLY;INTERVAL=10000\nSUMMARY:i",
    )))
    .container
    .unwrap();
    assert!(began.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(container.occurrences.len(), 1);
    assert!(container.warnings.is_empty(), "{:?}", container.warnings);
}

#[test]
fn a_month_day_filter_counts_as_the_whole_month_whatever_its_signs() {
    // Day 1 and the last 30 days of a month, every hour, minute and second: 2,678,400 dates a
    // month, though the rule lists one positive day. Refused in any case; days from both ends
    // at ordinary times expand.
    let every = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let ends = (1..=30)
        .map(|i| format!("-{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let flood = format!(
        "FREQ=MONTHLY;BYMONTHDAY=1,{ends};BYHOUR={};BYMINUTE={};BYSECOND={}",
        every(24),
        every(60),
        every(60)
    );
    let bounded = |rule: &str| {
        let set: rrule::RRuleSet = format!("DTSTART:20261020T120000Z\nRRULE:{rule}")
            .parse()
            .unwrap();
        pinakes_core::event::time::rrule_bounded(&set, 100_000)
    };
    for rule in [
        flood.clone(),
        flood.to_lowercase(),
        format!(
            "FREQ=MONTHLY;BYMONTHDAY={ends};BYHOUR={};BYMINUTE={};BYSECOND={}",
            every(24),
            every(60),
            every(60)
        ),
        format!(
            "FREQ=YEARLY;BYMONTHDAY=1,{ends};BYHOUR={};BYMINUTE={}",
            every(24),
            every(60)
        ),
    ] {
        assert!(!bounded(&rule), "{rule}");
    }
    for rule in [
        "FREQ=MONTHLY;BYMONTHDAY=1,-1;BYHOUR=9",
        "FREQ=MONTHLY;BYMONTHDAY=-1;BYHOUR=9,18",
        "FREQ=YEARLY;BYMONTHDAY=-1;BYDAY=FR;BYHOUR=9",
    ] {
        assert!(bounded(rule), "{rule}");
    }
    let body = cal(&event(
        "UID:ends\nDTSTART:20261101T090000Z\nRRULE:FREQ=MONTHLY;BYMONTHDAY=1,-1;BYHOUR=9\nSUMMARY:e",
    ));
    let cfg = EventConfig {
        max_occurrences_per_series: 1000,
        ..Default::default()
    };
    let c = calendar::parse("https://example.org/f.ics", body.as_bytes(), &cfg, now())
        .unwrap()
        .container
        .unwrap();
    // 1 November 2026 to the horizon (twelve months on): the first and last day of each month.
    assert!(c.occurrences.len() >= 20, "{}", c.occurrences.len());
    assert!(c.warnings.is_empty(), "{:?}", c.warnings);
}

#[test]
fn an_unexpanded_rule_keeps_its_date_lists_and_the_horizon() {
    // A rule over the bound gives no dates of its own, but its event's DTSTART, RDATE and
    // EXDATE, and the horizon, still apply.
    let every = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let flood = format!(
        "freq=yearly;byday=mo,tu,we,th,fr,sa,su;byhour={};byminute={};bysecond={}",
        every(24),
        every(60),
        every(60)
    );
    let starts = |props: &str| {
        let c = parse(&cal(&event(&format!(
            "UID:u\n{props}\nRRULE:{flood}\nSUMMARY:u"
        ))))
        .container
        .unwrap();
        assert!(
            c.warnings.contains(&"series_rule_unexpanded".to_string()),
            "{props}: {:?}",
            c.warnings
        );
        let mut starts: Vec<String> = c
            .occurrences
            .iter()
            .map(|o| o.doc.start.unwrap().to_rfc3339())
            .collect();
        starts.sort();
        starts
    };
    // DTSTART alone.
    assert_eq!(
        starts("DTSTART:20261120T120000Z"),
        ["2026-11-20T12:00:00+00:00"]
    );
    // An EXDATE removes it.
    assert!(starts("DTSTART:20261120T120000Z\nEXDATE:20261120T120000Z").is_empty());
    // An RDATE adds one.
    assert_eq!(
        starts("DTSTART:20261120T120000Z\nRDATE:20261205T120000Z"),
        ["2026-11-20T12:00:00+00:00", "2026-12-05T12:00:00+00:00"]
    );
    // A DTSTART before the horizon gives nothing; an RDATE within it still counts.
    assert!(starts("DTSTART:20000101T120000Z").is_empty());
    assert_eq!(
        starts("DTSTART:20000101T120000Z\nRDATE:20261205T120000Z"),
        ["2026-12-05T12:00:00+00:00"]
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_one_off_event_reference_has_a_null_recurrence_id(pool: PgPool) {
    occurrences_at(
        &pool,
        "geo:47.37,8.54",
        "/pub/social/v1/posts/once",
        &[(None, Utc::now() + Duration::days(2))],
    )
    .await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, v) = call(&app, "/v1/search?type=Event").await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        v["items"][0]["event"]["object_path"],
        "/pub/social/v1/posts/once"
    );
    assert_eq!(v["items"][0]["event"]["recurrence_id"], Value::Null);
}

#[test]
fn a_plain_rule_counts_its_month_day_as_a_whole_month() {
    // The crate fills a plain monthly or yearly rule's month day from DTSTART; the bound counts
    // that list as the whole month, 31 days (a yearly rule's one month), and each time list it
    // fills as one.
    let bounded = |rule: &str, max: usize| {
        let set: rrule::RRuleSet = format!("DTSTART:20261020T120000Z\nRRULE:{rule}")
            .parse()
            .unwrap();
        pinakes_core::event::time::rrule_bounded(&set, max)
    };
    for rule in ["FREQ=MONTHLY", "FREQ=YEARLY"] {
        assert!(bounded(rule, 31), "{rule}");
        assert!(!bounded(rule, 30), "{rule}");
    }
    // A weekly or daily rule is its days.
    assert!(bounded("FREQ=WEEKLY", 1));
    assert!(bounded("FREQ=DAILY", 1));
}
