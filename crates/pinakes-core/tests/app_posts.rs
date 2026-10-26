//! The app-post feed (`app_posts`): a mock Nexus serving `/v0/app-posts/changes` by cursor, and
//! a mock homeserver serving version files. Every URI and identity here is made up.
use axum::{extract::Query, routing::get, Json, Router};
use pinakes_core::{
    app_posts::{self, Attempts, Versions},
    nexus::Nexus,
    resolver::egress::{Fetched, Validators},
    telemetry::Metrics,
};
use pubky_social_specs::{traits::TimestampId, PostEnvelope};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use url::Url;

/// The z32 key of a made-up keypair (`pkarr::Keypair::from_secret_key(&[7; 32])`): a canonical
/// Pubky key, as the envelope's reference gate requires.
const AUTHOR: &str = "7jfgaa9nutjyixzikb7tgmsf9gkwq7iqz498zr1nd5ig1fng4esy";

/// A fresh post id, as an app mints it.
fn id() -> String {
    PostEnvelope::<app_posts::AppKind>::default().create_id()
}

/// The mock homeserver: version files by path; a path in `down` fails as unreachable.
#[derive(Default)]
struct Homeserver {
    files: Mutex<HashMap<String, (u16, Vec<u8>)>>,
    down: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl Versions for Homeserver {
    async fn fetch(&self, object: &Url) -> anyhow::Result<Fetched> {
        assert_eq!(object.host_str(), Some(AUTHOR));
        if self
            .down
            .lock()
            .unwrap()
            .contains(&object.path().to_owned())
        {
            anyhow::bail!("homeserver unreachable");
        }
        let (status, body) = self
            .files
            .lock()
            .unwrap()
            .get(object.path())
            .cloned()
            .unwrap_or((404, vec![]));
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Validators::default(),
            url: object.clone(),
            content_type: "application/octet-stream".into(),
            status,
            retry_after_secs: 0,
            location: None,
            body,
        })
    }
}

/// The mock Nexus: the app-post feed's lines, its epoch, served by cursor.
#[derive(Clone, Default)]
struct Feed {
    lines: Arc<Mutex<Vec<Value>>>,
    epoch: Arc<Mutex<String>>,
}
async fn nexus(feed: Feed) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v0/app-posts/changes",
        get(move |Query(q): Query<HashMap<String, i64>>| {
            let feed = feed.clone();
            async move {
                let lines = feed.lines.lock().unwrap().clone();
                let start = (q["cursor"] as usize).min(lines.len());
                let end = (start + q["limit"] as usize).min(lines.len());
                (
                    [
                        ("x-events-epoch", feed.epoch.lock().unwrap().clone()),
                        ("x-events-length", lines.len().to_string()),
                    ],
                    Json(json!({"lines": lines[start..end], "next_cursor": end})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

struct Run {
    pool: PgPool,
    url: String,
    nexus: Nexus,
    feed: Feed,
    home: Homeserver,
    metrics: Metrics,
    attempts: Attempts,
    _server: tokio::task::JoinHandle<()>,
}
impl Run {
    async fn new(pool: PgPool) -> Self {
        let feed = Feed::default();
        *feed.epoch.lock().unwrap() = "e1".into();
        let (url, server) = nexus(feed.clone()).await;
        Self {
            nexus: Nexus::new(&url).unwrap(),
            pool,
            url,
            feed,
            home: Homeserver::default(),
            metrics: Metrics::new().unwrap(),
            attempts: Attempts::default(),
            _server: server,
        }
    }
    /// A post written: its version file on the homeserver and its line on the feed. The post's
    /// versionless path.
    fn put(&self, namespace: &str, kind: &str, envelope: Value) -> String {
        let (post_id, edit) = (id(), id());
        let post = format!("/pub/{namespace}/v1/posts/{post_id}");
        self.put_version(&post, &edit, namespace, kind, envelope);
        post
    }
    fn put_version(&self, post: &str, edit: &str, namespace: &str, kind: &str, envelope: Value) {
        let version = format!("{post}/{edit}.json");
        self.home
            .files
            .lock()
            .unwrap()
            .insert(version.clone(), (200, envelope.to_string().into_bytes()));
        self.line("put", post, &version, namespace, kind);
    }
    fn line(&self, op: &str, post: &str, version: &str, namespace: &str, kind: &str) {
        self.feed.lines.lock().unwrap().push(json!({
            "op": op, "author": AUTHOR, "post": post, "version": version,
            "namespace": namespace, "post_kind": kind, "at": 1_790_000_000_000i64,
        }));
    }
    /// Every page read.
    async fn drain(&mut self) {
        for _ in 0..20 {
            let n = app_posts::poll_once(
                &self.pool,
                &self.url,
                &self.nexus,
                &self.home,
                &self.metrics,
                &mut self.attempts,
            )
            .await
            .unwrap();
            if n == 0 {
                return;
            }
        }
        panic!("the feed did not drain");
    }
    fn count(&self, outcome: &str) -> u64 {
        self.metrics.app_posts.with_label_values(&[outcome]).get()
    }
    async fn refs(&self, kind: &str) -> Vec<(String, Option<String>, Option<String>)> {
        sqlx::query_as(
            "SELECT r.uri,f.recurrence_id,f.label FROM reference f JOIN resource r ON r.id=f.resource_id WHERE f.kind=$1 AND f.feed='app_posts' ORDER BY f.start_at,r.uri,f.recurrence_id",
        )
        .bind(kind)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}
fn envelope(kind: &str, content: Value) -> Value {
    json!({"content": content.to_string(), "kind": kind, "parent": null, "embed": null, "attachments": []})
}

/// A social event's occurrences become event references at its location, each with its
/// instant in the event's zone, its end and status; an override of one occurrence replaces it,
/// and the master's later edit does not bring it back.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_social_event_is_its_occurrences_at_its_location(pool: PgPool) {
    let mut run = Run::new(pool).await;
    let event = |extra: Value| {
        let mut c = json!({"title": "Evening", "start": "2027-01-05T18:30:00", "timeZone": "Europe/Zurich",
            "duration": "PT2H", "locations": [{"uri": "https://venue.example/hall"}],
            "recurrenceRules": [{"frequency": "weekly", "count": 3}]});
        c.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        envelope("event", c)
    };
    let master = run.put("social", "event", event(json!({})));
    run.drain().await;
    let rows: Vec<(String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, String)> =
        sqlx::query_as("SELECT recurrence_id,start_at,end_at,event_status FROM reference WHERE kind='event' ORDER BY start_at")
            .fetch_all(&run.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].0, "2027-01-05T18:30:00");
    // 18:30 in Zurich in January is 17:30 UTC; two hours long.
    assert_eq!(rows[0].1.to_rfc3339(), "2027-01-05T17:30:00+00:00");
    assert_eq!((rows[0].2 - rows[0].1).num_hours(), 2);
    assert!(rows.iter().all(|r| r.3 == "scheduled"));
    // Search shows each occurrence as an event at the venue.
    let app = pinakes_core::api::router(
        pinakes_core::api::AppState::new(run.pool.clone(), Default::default()).unwrap(),
    );
    let (code, page) = call(&app, "/v1/search?type=Event&from=2027-01-01T00:00:00Z").await;
    assert_eq!(code, 200, "{page}");
    let mine: Vec<&Value> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["event"]["object_path"] == master.as_str())
        .collect();
    assert_eq!(mine.len(), 3, "{page}");
    assert_eq!(mine[0]["event"]["author"], AUTHOR);
    // An override moves the second occurrence an hour on.
    run.put(
        "social",
        "event",
        envelope(
            "event",
            json!({"title": "Evening, later", "start": "2027-01-12T19:30:00", "timeZone": "Europe/Zurich",
                "duration": "PT2H", "locations": [{"uri": "https://venue.example/hall"}],
                "recurrenceOf": format!("pubky://{AUTHOR}{master}"), "recurrenceId": "2027-01-12T18:30:00"}),
        ),
    );
    run.drain().await;
    let starts = |run: &Run| {
        let pool = run.pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT to_char(start_at AT TIME ZONE 'UTC','MM-DD HH24:MI') FROM reference WHERE kind='event' ORDER BY start_at")
                .fetch_all(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        starts(&run).await,
        ["01-05 17:30", "01-12 18:30", "01-19 17:30"]
    );
    // The master edited: the overridden occurrence stays the override's.
    run.put_version(
        &master,
        &id(),
        "social",
        "event",
        event(json!({"title": "Evening (edited)"})),
    );
    run.drain().await;
    assert_eq!(
        starts(&run).await,
        ["01-05 17:30", "01-12 18:30", "01-19 17:30"]
    );
    // Deleted, the master's occurrences go; the override's stays.
    run.line(
        "del",
        &master,
        &format!("{master}/x.json"),
        "social",
        "event",
    );
    run.drain().await;
    assert_eq!(starts(&run).await, ["01-12 18:30"]);
    assert_eq!(run.count("deleted"), 1);
}

async fn call(app: &Router, path: &str) -> (u16, Value) {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// An rsvp to a page is a reply on it, its status the label; one to a Pubky object is
/// Nexus's. A route is its card (its box from its coordinate waypoints, its measures as facts)
/// with an embed on each waypoint, never queued for a resolver. An availability is a
/// `Schedule` card. A listen is a play on the track it embeds, which no score counts.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn rsvps_routes_availability_and_listens(pool: PgPool) {
    let mut run = Run::new(pool).await;
    let mut answer = envelope("rsvp", json!({"status": "accepted", "guests": 1}));
    answer["parent"] = "https://events.example/meetup".into();
    run.put("social", "rsvp", answer.clone());
    answer["parent"] = format!("pubky://{AUTHOR}/pub/social/v1/posts/0032SSN7Q4EVG").into();
    run.put("social", "rsvp", answer);
    let route = run.put(
        "maps",
        "route",
        envelope(
            "route",
            json!({"name": "Lakeside loop", "activity": "running", "distance_m": 5200,
                "waypoints": [{"uri": "geo:46.00547,8.95186"}, {"uri": "geo:46.1,9"}, {"uri": "https://places.example/cafe"}]}),
        ),
    );
    run.put(
        "events",
        "availability",
        envelope(
            "availability",
            json!({"timeZone": "Europe/Zurich", "horizonDays": 60,
                "available": [{"title": "Office hours", "start": "2027-01-04T09:00:00", "duration": "PT8H"}]}),
        ),
    );
    let mut play = envelope("listen", json!({}));
    play["embed"] = "https://tracks.example/track/1".into();
    run.put("music", "listen", play);
    run.drain().await;
    assert_eq!(
        run.refs("reply").await,
        [(
            "https://events.example/meetup".into(),
            None,
            Some("accepted".into())
        )]
    );
    let embeds: Vec<String> = run.refs("embed").await.into_iter().map(|r| r.0).collect();
    assert_eq!(
        embeds,
        [
            "geo:46.00547,8.95186",
            "geo:46.1,9",
            "https://places.example/cafe"
        ]
    );
    // The route's card, read by its link.
    let app = pinakes_core::api::router(
        pinakes_core::api::AppState::new(run.pool.clone(), Default::default()).unwrap(),
    );
    let uri = format!("pubky://{AUTHOR}{route}");
    let (code, card) = call(
        &app,
        &format!("/v1/resources/by-uri?uri={}", urlencoding(&uri)),
    )
    .await;
    assert_eq!(code, 200, "{card}");
    assert_eq!(card["type"], "Trip");
    assert_eq!(card["name"], "Lakeside loop");
    // A read's fields, in `details` or beside the card's (`api_shape` 2).
    let read = card.get("details").unwrap_or(&card);
    assert_eq!(read["bbox"], json!([8.95186, 46.00547, 9.0, 46.1]));
    assert_eq!(read["facts"]["distance_m"], "5200");
    assert_eq!(read["facts"]["activity"], "running");
    let resolver: Option<String> = sqlx::query_scalar("SELECT resolver FROM resource WHERE uri=$1")
        .bind(&uri)
        .fetch_one(&run.pool)
        .await
        .unwrap();
    assert_eq!(resolver, None, "an app post is never fetched");
    let schedule: String = sqlx::query_scalar(
        "SELECT doc->>'type' FROM resource WHERE uri LIKE '%/pub/events/v1/posts/%'",
    )
    .fetch_one(&run.pool)
    .await
    .unwrap();
    assert_eq!(schedule, "Schedule");
    assert_eq!(
        run.refs("listen").await,
        [("https://tracks.example/track/1".into(), None, None)]
    );
    let counted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM effective_reference f JOIN resource r ON r.id=f.resource_id WHERE r.uri='https://tracks.example/track/1'",
    )
    .fetch_one(&run.pool)
    .await
    .unwrap();
    assert_eq!(counted, 0, "a play counts toward no score or filter");
    assert_eq!((run.count("indexed"), run.count("foreign")), (4, 1));
    // Deleted, the route's card is gone and its embeds with it.
    run.line("del", &route, &format!("{route}/x.json"), "maps", "route");
    run.drain().await;
    assert!(run.refs("embed").await.is_empty());
    let status: String = sqlx::query_scalar("SELECT status FROM resource WHERE uri=$1")
        .bind(&uri)
        .fetch_one(&run.pool)
        .await
        .unwrap();
    assert_eq!(status, "missing");
}
fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Lines that are not read are skipped and counted, and the cursor moves past them: a kind no
/// profile reads, a malformed line, an envelope that fails pubky-social-specs' checks, a
/// version the homeserver no longer has. A homeserver that cannot be reached holds the page for
/// `MAX_ATTEMPTS` polls, then its line is skipped.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn unread_lines_are_skipped_and_an_unreachable_homeserver_does_not_stop_the_feed(
    pool: PgPool,
) {
    let mut run = Run::new(pool).await;
    run.put("music", "session", envelope("session", json!({})));
    run.feed
        .lines
        .lock()
        .unwrap()
        .push(json!({"op": "put", "author": AUTHOR}));
    let mut bad = envelope("listen", json!({}));
    bad["unknown_member"] = 1.into();
    run.put("music", "listen", bad);
    let gone = format!("/pub/music/v1/posts/{}", id());
    run.line(
        "put",
        &gone,
        &format!("{gone}/{}.json", id()),
        "music",
        "listen",
    );
    let mut play = envelope("listen", json!({}));
    play["embed"] = "https://tracks.example/track/2".into();
    let down = run.put("music", "listen", play);
    let version = run
        .home
        .files
        .lock()
        .unwrap()
        .keys()
        .find(|k| k.starts_with(&down))
        .cloned()
        .unwrap();
    run.home.down.lock().unwrap().push(version);
    let key = app_posts::cursor_key(&run.url);
    for _ in 1..app_posts::MAX_ATTEMPTS {
        let polled = app_posts::poll_once(
            &run.pool,
            &run.url,
            &run.nexus,
            &run.home,
            &run.metrics,
            &mut run.attempts,
        )
        .await;
        assert!(polled.is_err());
        assert_eq!(
            pinakes_core::ingest::cursor(&run.pool, &key)
                .await
                .unwrap()
                .cursor,
            0
        );
    }
    run.drain().await;
    assert_eq!(
        pinakes_core::ingest::cursor(&run.pool, &key)
            .await
            .unwrap()
            .cursor,
        5
    );
    for (outcome, n) in [
        ("unprofiled", 1),
        ("malformed", 1),
        ("invalid", 1),
        ("gone", 1),
        ("unreachable", 1),
    ] {
        assert_eq!(run.count(outcome), n, "{outcome}");
    }
    assert!(run.refs("listen").await.is_empty());
}

/// Each feed's reset drops its own references alone: a new epoch of the app-post feed leaves the
/// resource feed's tags, and the resource feed's reset leaves the app posts'.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn each_feed_resets_its_own_references(pool: PgPool) {
    let mut run = Run::new(pool).await;
    let mut play = envelope("listen", json!({}));
    play["embed"] = "https://tracks.example/track/3".into();
    run.put("music", "listen", play);
    run.drain().await;
    // A tag from the resource feed.
    let tagged = pinakes_core::store::ensure_resource(&run.pool, "https://example.org/tagged")
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,kind,author,object_path,created_at) VALUES($1,'tag',$2,'/pub/pubky.app/tags/1',now())")
        .bind(&tagged)
        .bind(AUTHOR)
        .execute(&run.pool)
        .await
        .unwrap();
    let count = |feed: &'static str, pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reference WHERE feed=$1")
            .bind(feed)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    // The app-post feed's new epoch: its references go, the tag stays, and it reads again.
    *run.feed.epoch.lock().unwrap() = "e2".into();
    run.drain().await;
    run.drain().await;
    assert_eq!(count("resources", run.pool.clone()).await, 1);
    assert_eq!(count("app_posts", run.pool.clone()).await, 1);
    // The resource feed's reset (a new epoch): the tag goes, the play stays.
    sqlx::query("INSERT INTO feed_cursor(nexus,cursor,epoch) VALUES('fixture',0,'r1') ON CONFLICT(nexus) DO UPDATE SET epoch='r1'")
        .execute(&run.pool)
        .await
        .unwrap();
    let saved = pinakes_core::ingest::cursor(&run.pool, "fixture")
        .await
        .unwrap();
    let page = pinakes_core::nexus::FeedPage {
        page: pinakes_core::nexus::Changes {
            lines: vec![],
            next_cursor: 0,
            skipped: Default::default(),
        },
        epoch: Some("r2".into()),
        length: 0,
    };
    pinakes_core::ingest::apply(
        &run.pool,
        "fixture",
        &saved,
        &page,
        &Default::default(),
        &run.metrics,
    )
    .await
    .unwrap();
    assert_eq!(count("resources", run.pool.clone()).await, 0);
    assert_eq!(count("app_posts", run.pool.clone()).await, 1);
    let orphaned: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT orphaned_at FROM resource WHERE uri='https://tracks.example/track/3'",
    )
    .fetch_one(&run.pool)
    .await
    .unwrap();
    assert_eq!(orphaned, None, "a resource the app posts reference stays");
}
