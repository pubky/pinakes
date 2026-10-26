//! App posts and the social specs' events and rsvps (TASK-app-posts.md, its second amendment):
//! `post` lines on the resource feed name a post, which the Pubky resolver reads in its latest
//! version by its namespace and kind. Every URI and identity here is made up.
use chrono::{DateTime, Duration, Utc};
use pinakes_core::{
    config::IngestConfig,
    event::EventStatus,
    ingest,
    model::{identity, ResourceMeta, Status},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    resolver::{classify, pubky_apps::card_with, pubky_apps::posts::AppKind},
    telemetry::Metrics,
};
use pubky_social_specs::{traits::TimestampId, PostEnvelope};
use serde_json::{json, Value};
use sqlx::PgPool;

/// The z32 key of a made-up keypair (`pkarr::Keypair::from_secret_key(&[7; 32])`): a canonical
/// Pubky key, as the envelope's reference gate requires.
const AUTHOR: &str = "7jfgaa9nutjyixzikb7tgmsf9gkwq7iqz498zr1nd5ig1fng4esy";

/// A fresh post id, as an app mints it.
fn id() -> String {
    PostEnvelope::<AppKind>::default().create_id()
}
fn post(namespace: &str, id: &str) -> String {
    format!("pubky://{AUTHOR}/pub/{namespace}/v1/posts/{id}")
}
fn envelope(kind: &str, content: Value) -> Value {
    json!({"content": content.to_string(), "kind": kind, "parent": null, "embed": null, "attachments": []})
}
fn now() -> DateTime<Utc> {
    "2026-10-16T12:00:00Z".parse().unwrap()
}
/// The card of a post's latest version, read at `now()`.
fn card(uri: &str, version: &Value) -> ResourceMeta {
    card_with(
        uri,
        "application/octet-stream",
        &serde_json::to_vec(version).unwrap(),
        3600,
        now(),
        pinakes_core::event::time::MAX_PERIOD_DATES,
    )
}
fn no_card(doc: &ResourceMeta) -> bool {
    doc.status == Status::Missing && doc.withheld && doc.name.is_none()
}
fn fact<'a>(doc: &'a ResourceMeta, key: &str) -> Option<&'a str> {
    doc.attributes.get(key).map(String::as_str)
}

/// A post line: the versionless post as its URI, a version file as its object path.
fn line(op: Op, uri: &str, version: &str, at: DateTime<Utc>) -> Change {
    let path = uri.strip_prefix(&format!("pubky://{AUTHOR}")).unwrap();
    Change {
        op,
        uri: uri.into(),
        kind: Kind::Post,
        author: AUTHOR.into(),
        object_path: format!("{path}/{version}.json"),
        label: None,
        at: at.timestamp_millis(),
    }
}
async fn batch(pool: &PgPool, lines: Vec<Change>, metrics: &Metrics) {
    let saved = ingest::cursor(pool, "fixture").await.unwrap();
    let next = saved.cursor + lines.len() as i64;
    let page = FeedPage {
        page: Changes {
            lines,
            next_cursor: next,
            skipped: Default::default(),
        },
        epoch: None,
        length: next,
    };
    let applied = ingest::apply(
        pool,
        "fixture",
        &saved,
        &page,
        &IngestConfig::default(),
        metrics,
    )
    .await
    .unwrap();
    assert!(applied);
}
/// The references of a resource: (kind, object path).
async fn refs(pool: &PgPool, uri: &str) -> Vec<(String, String)> {
    let (id, _, _) = identity(uri).unwrap();
    sqlx::query_as(
        "SELECT kind,object_path FROM reference WHERE resource_id=$1 ORDER BY object_path",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
}
/// Whether the resource is due for resolution now.
async fn due(pool: &PgPool, uri: &str) -> bool {
    let (id, _, _) = identity(uri).unwrap();
    sqlx::query_scalar(
        "SELECT refresh_at<=now() AND next_attempt_at<=now() FROM resource WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}
/// The resource read at `at`, its next read a day on, as the queue leaves a card.
async fn resolved(pool: &PgPool, uri: &str, at: DateTime<Utc>) {
    let (id, _, _) = identity(uri).unwrap();
    sqlx::query("UPDATE resource SET resolved_at=$2,refresh_at=now()+interval '1 day',next_attempt_at=now()+interval '1 day' WHERE id=$1")
        .bind(id)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
}

/// A post line is a reference of the post to itself, keyed by its version file: queued for the
/// Pubky resolver, counted nowhere, read again at each new version and at a deletion.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn post_lines_queue_the_post_and_each_version_reads_it_again(pool: PgPool) {
    let metrics = Metrics::new().unwrap();
    let route = post("maps", &id());
    let start = Utc::now() - Duration::hours(2);
    batch(&pool, vec![line(Op::Put, &route, "0034A", start)], &metrics).await;
    let path = route.strip_prefix(&format!("pubky://{AUTHOR}")).unwrap();
    assert_eq!(
        refs(&pool, &route).await,
        [("post".into(), format!("{path}/0034A.json"))]
    );
    let (id, _, _) = identity(&route).unwrap();
    let (resolver, orphaned): (Option<String>, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT resolver,orphaned_at FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(resolver.as_deref(), Some("pubky"));
    // Its own row keeps the card alive and counts nowhere.
    assert_eq!(orphaned, None);
    let counted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM effective_reference WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counted, 0);

    // Read an hour ago: a line from before then changes nothing, a new version is read now.
    resolved(&pool, &route, start + Duration::hours(1)).await;
    batch(&pool, vec![line(Op::Put, &route, "0034B", start)], &metrics).await;
    assert!(!due(&pool, &route).await);
    batch(
        &pool,
        vec![line(Op::Put, &route, "0034C", Utc::now())],
        &metrics,
    )
    .await;
    assert!(due(&pool, &route).await);
    assert_eq!(refs(&pool, &route).await.len(), 3);

    // A deleted version drops its row, and the post is read again: in its latest remaining
    // version, or gone.
    resolved(&pool, &route, Utc::now() - Duration::minutes(1)).await;
    batch(
        &pool,
        vec![line(Op::Del, &route, "0034C", Utc::now())],
        &metrics,
    )
    .await;
    assert!(due(&pool, &route).await);
    assert_eq!(refs(&pool, &route).await.len(), 2);
}

/// A post line must name a post in its author's storage and a version file directly in it.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_post_line_outside_its_post_is_rejected(pool: PgPool) {
    let metrics = Metrics::new().unwrap();
    let good = post("events", &id());
    let other = "ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u";
    let mut elsewhere = line(Op::Put, &good, "0034A", Utc::now());
    elsewhere.uri = good.replace(AUTHOR, other);
    let mut not_a_post = line(Op::Put, &good, "0034A", Utc::now());
    not_a_post.uri = format!("pubky://{AUTHOR}/pub/events/v1/calendars/1");
    let mut nested = line(Op::Put, &good, "0034A", Utc::now());
    nested.object_path = nested.object_path.replace("/0034A.json", "/x/0034A.json");
    let mut other_post = line(Op::Put, &good, "0034A", Utc::now());
    other_post.object_path = other_post.object_path.replace("/v1/posts/", "/v1/posts/0");
    batch(
        &pool,
        vec![
            elsewhere,
            not_a_post,
            nested,
            other_post,
            line(Op::Put, &good, "0034A", Utc::now()),
        ],
        &metrics,
    )
    .await;
    let rejected = |reason: &str| metrics.ingest_rejected.with_label_values(&[reason]).get();
    assert_eq!((rejected("uri"), rejected("object_path")), (2, 2));
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM reference")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 1);
}

/// The posts of the profiled namespaces go to the Pubky resolver; every other social object, and
/// the posts of namespaces no profile reads, are never fetched.
#[test]
fn posts_route_by_namespace() {
    let id = id();
    for namespace in ["social", "maps", "events", "music"] {
        assert_eq!(
            classify(&post(namespace, &id)),
            Some("pubky"),
            "{namespace}"
        );
        // A version file is the resolver's to fetch, never a resource of its own.
        assert_eq!(
            classify(&format!("{}/0034A.json", post(namespace, &id))),
            None
        );
    }
    for uri in [
        post("chess", &id),
        format!("pubky://{AUTHOR}/pub/social/v1/tags/{id}"),
        format!("pubky://{AUTHOR}/pub/social/v1.1/posts/{id}"),
        format!("pubky://{AUTHOR}/pub/pubky.app/posts/{id}"),
    ] {
        assert_eq!(classify(&uri), None, "{uri}");
    }
}

/// A social event: its JSCalendar time in its zone, status and first location as its venue; a
/// recurring one shows its next occurrence.
#[test]
fn a_social_event_is_an_event_card() {
    let uri = post("social", &id());
    let content = json!({
        "title": "Bern meetup", "description": "Monthly.",
        "start": "2026-10-20T18:30:00", "timeZone": "Europe/Zurich", "duration": "PT2H",
        "locations": [{"name": "Café Fédéral", "uri": "https://www.openstreetmap.org/node/1"}],
    });
    let doc = card(&uri, &envelope("event", content.clone()));
    assert_eq!(doc.name.as_deref(), Some("Bern meetup"));
    assert_eq!(doc.type_path.last().map(String::as_str), Some("Event"));
    assert_eq!(doc.start, Some("2026-10-20T16:30:00Z".parse().unwrap()));
    assert_eq!(doc.end, Some("2026-10-20T18:30:00Z".parse().unwrap()));
    assert_eq!(doc.event.tz.as_deref(), Some("Europe/Zurich"));
    assert_eq!(
        doc.event.venue.as_ref().map(|v| v.text.as_str()),
        Some("Café Fédéral")
    );
    assert_eq!(
        fact(&doc, "location_uri"),
        Some("https://www.openstreetmap.org/node/1")
    );
    assert_eq!(doc.event.event_status, None);

    // Weekly from 6 October: on 16 October (`now`) the next is the 20th.
    let mut weekly = content.clone();
    weekly["start"] = "2026-10-06T18:30:00".into();
    weekly["recurrenceRules"] = json!([{"frequency": "weekly", "count": 10}]);
    let doc = card(&uri, &envelope("event", weekly.clone()));
    assert_eq!(doc.start, Some("2026-10-20T16:30:00Z".parse().unwrap()));
    // An excluded date is skipped (the next is after the change to winter time).
    weekly["excludedDates"] = json!(["2026-10-20T18:30:00"]);
    let doc = card(&uri, &envelope("event", weekly.clone()));
    assert_eq!(doc.start, Some("2026-10-27T17:30:00Z".parse().unwrap()));
    // A series that has ended shows its last occurrence.
    weekly["recurrenceRules"] = json!([{"frequency": "weekly", "count": 2}]);
    let doc = card(&uri, &envelope("event", weekly));
    assert_eq!(doc.start, Some("2026-10-13T16:30:00Z".parse().unwrap()));

    let mut cancelled = content.clone();
    cancelled["status"] = "cancelled".into();
    let doc = card(&uri, &envelope("event", cancelled));
    assert_eq!(doc.event.event_status, Some(EventStatus::Cancelled));

    // A geo location is its point.
    let mut at = content.clone();
    at["locations"] = json!([{"uri": "geo:46.948,7.447"}]);
    let doc = card(&uri, &envelope("event", at));
    assert_eq!(
        doc.location.as_ref().map(|p| (p.lat, p.lon)),
        Some((46.948, 7.447))
    );

    for (key, value) in [
        ("status", json!("postponed")),
        ("timeZone", json!("Mars/Olympus")),
        ("start", json!("tomorrow")),
        ("duration", json!("-PT1H")),
    ] {
        let mut bad = content.clone();
        bad[key] = value;
        assert!(no_card(&card(&uri, &envelope("event", bad))), "{key}");
    }
}

/// An rsvp answers an event; every other social post is Nexus's and has no card.
#[test]
fn an_rsvp_is_a_card_and_other_social_posts_have_none() {
    let uri = post("social", &id());
    let event = post("social", &id());
    let mut answer = envelope("rsvp", json!({"status": "accepted", "guests": 2}));
    answer["parent"] = event.clone().into();
    let doc = card(&uri, &answer);
    assert_eq!(doc.type_path.last().map(String::as_str), Some("RsvpAction"));
    assert_eq!(fact(&doc, "status"), Some("accepted"));
    assert_eq!(fact(&doc, "guests"), Some("2"));
    assert_eq!(fact(&doc, "event"), Some(event.as_str()));
    let mut unknown = answer.clone();
    unknown["content"] = json!({"status": "maybe"}).to_string().into();
    assert!(no_card(&card(&uri, &unknown)));
    assert!(no_card(&card(
        &uri,
        &envelope("short", json!({"text": "hello"}))
    )));
    assert!(no_card(&card(
        &uri,
        &envelope("article", json!({"title": "A"}))
    )));
}

/// Maps routes, events availability and music listens and sessions: each its own card; a kind
/// its namespace does not read, and an envelope the specs refuse, have none.
#[test]
fn app_kinds_by_namespace() {
    let route = envelope(
        "route",
        json!({
            "name": "Aare walk", "activity": "hiking", "distance_m": 12000,
            "waypoints": [{"uri": "geo:46.95,7.44"}, {"uri": "geo:46.90,7.50", "name": "Bridge"}],
        }),
    );
    let doc = card(&post("maps", &id()), &route);
    assert_eq!(doc.type_path.last().map(String::as_str), Some("Trip"));
    assert_eq!(doc.bbox, Some([7.44, 46.90, 7.50, 46.95]));
    assert_eq!(fact(&doc, "distance_m"), Some("12000"));
    assert_eq!(fact(&doc, "waypoints"), Some("2"));

    // `for`: the resource the schedule belongs to, as in the spec's example.
    let listing = format!("pubky://{AUTHOR}/pub/shop/v1/listings/0032SSN7Q4EVG");
    let availability = envelope(
        "availability",
        json!({
            "timeZone": "Europe/Zurich", "horizonDays": 30, "for": listing,
            "available": [{"title": "Office hours", "start": "2026-10-19T09:00:00", "duration": "PT3H"}],
            "busy": [],
        }),
    );
    let doc = card(&post("events", &id()), &availability);
    assert_eq!(doc.type_path.last().map(String::as_str), Some("Schedule"));
    assert_eq!(doc.name.as_deref(), Some("Office hours"));
    assert_eq!(doc.start, Some("2026-10-19T07:00:00Z".parse().unwrap()));
    assert_eq!(doc.end, Some("2026-10-19T10:00:00Z".parse().unwrap()));
    assert_eq!(fact(&doc, "for"), Some(listing.as_str()));

    let mut listen = envelope("listen", json!({}));
    listen["embed"] = "https://tracks.example/track/1".into();
    let doc = card(&post("music", &id()), &listen);
    assert_eq!(
        doc.type_path.last().map(String::as_str),
        Some("ListenAction")
    );
    assert_eq!(fact(&doc, "track"), Some("https://tracks.example/track/1"));
    // At the post's own time, its id.
    let written = doc.start.unwrap();
    assert!((Utc::now() - written).num_minutes().abs() < 5, "{written}");

    let session = envelope("session", json!({"name": "Late jam"}));
    assert_eq!(
        card(&post("music", &id()), &session).name.as_deref(),
        Some("Late jam")
    );

    // A route in the events namespace, a listen without a track, an id the specs refuse, an
    // explicit `null` where the envelope wants a list.
    assert!(no_card(&card(&post("events", &id()), &route)));
    assert!(no_card(&card(
        &post("music", &id()),
        &envelope("listen", json!({}))
    )));
    assert!(no_card(&card(&post("maps", "0000000000000"), &route)));
    let mut null = route.clone();
    null["attachments"] = Value::Null;
    assert!(no_card(&card(&post("maps", &id()), &null)));
}

/// The references inside a post's content pass the specs' gate as the envelope's own do: a
/// private one refuses the post. A text the database cannot hold (a NUL) is no text.
#[test]
fn content_references_are_gated_and_text_is_storable() {
    let private = format!("pubky://{AUTHOR}/priv/maps/v1/posts/{}", id());
    let route = envelope(
        "route",
        json!({"name": "Hidden", "waypoints": [{"uri": "geo:46.95,7.44"}, {"uri": private}]}),
    );
    assert!(no_card(&card(&post("maps", &id()), &route)));
    let event = |location: &str, title: &str| {
        envelope(
            "event",
            json!({"title": title, "start": "2026-10-20T18:30:00", "timeZone": "Europe/Zurich",
                "duration": "PT1H", "locations": [{"uri": location}]}),
        )
    };
    let social = post("social", &id());
    assert!(!no_card(&card(&social, &event("geo:46.95,7.44", "Meetup"))));
    assert!(no_card(&card(&social, &event(&private, "Meetup"))));
    assert!(no_card(&card(
        &social,
        &event("geo:46.95,7.44", "Meet\0up")
    )));
    let availability = envelope(
        "availability",
        json!({"timeZone": "Europe/Zurich", "horizonDays": 7, "for": private, "available": []}),
    );
    assert!(no_card(&card(&post("events", &id()), &availability)));
}
