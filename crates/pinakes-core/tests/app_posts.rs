//! App posts (TASK-app-posts.md, its second amendment): `post` lines on the resource feed name a
//! post, which the Pubky resolver reads in its latest version by its namespace and kind. The
//! social specs' posts (events, rsvps) are Nexus's: never fetched, their lines rejected (owner
//! decision, 2026-10-26). Every URI and identity here is made up.
use chrono::{DateTime, Duration, Utc};
use pinakes_core::{
    config::{IngestConfig, QueueConfig},
    ingest,
    model::{identity, ResourceMeta, Status},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    queue,
    resolver::Resolver,
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
    let good = post("maps", &id());
    let other = "ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u";
    let mut elsewhere = line(Op::Put, &good, "0034A", Utc::now());
    elsewhere.uri = good.replace(AUTHOR, other);
    let mut not_a_post = line(Op::Put, &good, "0034A", Utc::now());
    not_a_post.uri = format!("pubky://{AUTHOR}/pub/maps/v1/calendars/1");
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

/// The posts of every namespace go to the Pubky resolver, those no profile reads (`events`, whose
/// availability is now social's, among them) for the general card; the social specs' objects
/// (their posts, events, rsvps and availability included) and pubky.app's are never fetched.
#[test]
fn posts_route_by_namespace() {
    let id = id();
    for namespace in ["maps", "music", "chess", "events"] {
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
        post("social", &id),
        format!("pubky://{AUTHOR}/pub/social/v1/tags/{id}"),
        format!("pubky://{AUTHOR}/pub/social/v1.1/posts/{id}"),
        format!("pubky://{AUTHOR}/pub/pubky.app/posts/{id}"),
    ] {
        assert_eq!(classify(&uri), None, "{uri}");
    }
}

/// Maps routes and music sessions: each its own card; a music listen, a kind its namespace does
/// not read, and an envelope the specs refuse, have none.
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

    // A session: the track it embeds, from `started_at` to `expires_at` (microseconds), its host
    // and service, named by its optional `name`.
    let host = format!("pubky://{AUTHOR}");
    let mut session = envelope(
        "session",
        json!({
            "started_at": 1_792_152_000_000_000_i64, "expires_at": 1_792_155_600_000_000_i64,
            "host": host, "source": {"service": "tidal"}, "name": "Late jam",
        }),
    );
    session["embed"] = "https://tracks.example/track/1".into();
    let doc = card(&post("music", &id()), &session);
    assert_eq!(
        doc.type_path.last().map(String::as_str),
        Some("BroadcastEvent")
    );
    assert_eq!(doc.name.as_deref(), Some("Late jam"));
    assert_eq!(doc.start, Some("2026-10-16T12:00:00Z".parse().unwrap()));
    assert_eq!(doc.end, Some("2026-10-16T13:00:00Z".parse().unwrap()));
    assert_eq!(fact(&doc, "track"), Some("https://tracks.example/track/1"));
    assert_eq!(fact(&doc, "host"), Some(host.as_str()));
    assert_eq!(fact(&doc, "service"), Some("tidal"));
    // Without an expiry it is open-ended; without a track, a start, or with an expiry before
    // its start, it has no card.
    let mut open = session.clone();
    open["content"] = json!({"started_at": 1_792_152_000_000_000_i64, "host": host})
        .to_string()
        .into();
    let doc = card(&post("music", &id()), &open);
    // Without a name it is named generically, as clients name it, with its author shown.
    assert_eq!(
        (doc.end, doc.name.as_deref(), fact(&doc, "author")),
        (None, Some("Listening session"), Some(AUTHOR))
    );
    // A name it could not store (a NUL, over 100 characters) is no name: the generic one; the
    // card stays. A hundred characters, two bytes each, are kept.
    let long = "é".repeat(101);
    let longest = "é".repeat(100);
    for (name, title) in [
        ("Late\0jam", "Listening session"),
        (long.as_str(), "Listening session"),
        (longest.as_str(), longest.as_str()),
    ] {
        let mut unstorable = session.clone();
        unstorable["content"] = json!({
            "started_at": 1_792_152_000_000_000_i64, "host": host, "name": name,
        })
        .to_string()
        .into();
        let doc = card(&post("music", &id()), &unstorable);
        assert_eq!((doc.status, doc.name.as_deref()), (Status::Ok, Some(title)));
    }
    let mut trackless = session.clone();
    trackless["embed"] = Value::Null;
    let mut unstarted = session.clone();
    unstarted["content"] = json!({"name": "Late jam", "host": host}).to_string().into();
    let mut backwards = session.clone();
    backwards["content"] = json!({
        "started_at": 1_792_155_600_000_000_i64, "expires_at": 1_792_152_000_000_000_i64,
        "host": host,
    })
    .to_string()
    .into();
    for version in [&trackless, &unstarted, &backwards] {
        assert!(no_card(&card(&post("music", &id()), version)), "{version}");
    }

    // Once its expiry has passed, a session has no card; until then it is read again when it
    // passes (the queue's scheduling of it: `an_expiring_card_is_read_again_when_it_expires`).
    assert_eq!(
        card(&post("music", &id()), &session).refresh_by,
        Some("2026-10-16T13:00:00Z".parse().unwrap())
    );
    let mut ended = session.clone();
    ended["content"] = json!({
        "started_at": 1_792_144_800_000_000_i64, "expires_at": 1_792_148_400_000_000_i64,
        "host": host,
    })
    .to_string()
    .into();
    assert!(no_card(&card(&post("music", &id()), &ended)));

    // Now playing: the track until its expiry, half an hour away, when it is read again.
    let mut listening = envelope(
        "listening",
        json!({
            "started_at": 1_792_151_000_000_000_i64, "expires_at": 1_792_153_800_000_000_i64,
            "source": {"service": "tidal"},
        }),
    );
    listening["embed"] = "https://tracks.example/track/1".into();
    let doc = card(&post("music", &id()), &listening);
    assert_eq!(
        doc.type_path.last().map(String::as_str),
        Some("ListenAction")
    );
    assert_eq!(doc.name.as_deref(), Some("Now playing"));
    assert_eq!(fact(&doc, "author"), Some(AUTHOR));
    assert_eq!(doc.end, Some("2026-10-16T12:30:00Z".parse().unwrap()));
    assert_eq!(doc.refresh_by, doc.end);
    assert_eq!(fact(&doc, "track"), Some("https://tracks.example/track/1"));
    assert_eq!(fact(&doc, "service"), Some("tidal"));
    // No longer current, current for more than a day, without an expiry, or not yet started
    // (beyond a minute's grace for the writer's clock): no card.
    for (start, end) in [
        (1_792_152_120_000_000_i64, Some(1_792_153_800_000_000_i64)),
        (1_792_144_800_000_000_i64, Some(1_792_148_400_000_000_i64)),
        (
            1_792_151_000_000_000_i64,
            Some(1_792_151_000_000_000_i64 + 90_000_000_000),
        ),
        (1_792_151_000_000_000_i64, None),
    ] {
        let mut stale = listening.clone();
        stale["content"] = json!({"started_at": start, "expires_at": end})
            .to_string()
            .into();
        assert!(no_card(&card(&post("music", &id()), &stale)), "{stale}");
    }

    // Started thirty seconds from now, within the grace: current.
    let mut early = listening.clone();
    early["content"] = json!({
        "started_at": 1_792_152_030_000_000_i64, "expires_at": 1_792_153_800_000_000_i64,
    })
    .to_string()
    .into();
    assert!(!no_card(&card(&post("music", &id()), &early)));
    // Named by its optional `name`, as a session.
    let mut named = listening.clone();
    named["content"] = json!({
        "started_at": 1_792_151_000_000_000_i64, "expires_at": 1_792_153_800_000_000_i64,
        "name": "Sunday records",
    })
    .to_string()
    .into();
    assert_eq!(
        card(&post("music", &id()), &named).name.as_deref(),
        Some("Sunday records")
    );
    // A name over 100 characters, or with a NUL, gets the generic title, as a session's.
    for (name, title) in [
        (long.as_str(), "Now playing"),
        ("Sunday\0records", "Now playing"),
        (longest.as_str(), longest.as_str()),
    ] {
        let mut unstorable = listening.clone();
        unstorable["content"] = json!({
            "started_at": 1_792_151_000_000_000_i64, "expires_at": 1_792_153_800_000_000_i64,
            "name": name,
        })
        .to_string()
        .into();
        let doc = card(&post("music", &id()), &unstorable);
        assert_eq!((doc.status, doc.name.as_deref()), (Status::Ok, Some(title)));
    }

    // A listen counts on its track: no card of its own, one per play.
    let mut listen = envelope("listen", json!({"played_at": 1_792_152_000_000_000_i64}));
    listen["embed"] = "https://tracks.example/track/1".into();
    assert!(no_card(&card(&post("music", &id()), &listen)));

    // A namespace no profile reads: the general card of its content's standard fields, or of a
    // text's excerpt, with its namespace and kind.
    let move_ = envelope(
        "game",
        json!({"title": "Sicilian, Najdorf", "description": "Round 3", "moves": 41}),
    );
    let doc = card(&post("chess", &id()), &move_);
    assert_eq!(doc.name.as_deref(), Some("Sicilian, Najdorf"));
    assert_eq!(doc.description.as_deref(), Some("Round 3"));
    assert_eq!(fact(&doc, "namespace"), Some("chess"));
    assert_eq!(fact(&doc, "kind"), Some("game"));
    let mut note = envelope("note", json!({}));
    note["content"] = "Blitz tonight at eight".into();
    let doc = card(&post("chess", &id()), &note);
    assert_eq!(
        doc.type_path.last().map(String::as_str),
        Some("SocialMediaPosting")
    );
    assert_eq!(doc.description.as_deref(), Some("Blitz tonight at eight"));
    assert_eq!(fact(&doc, "kind"), Some("note"));
    // An availability in the events namespace is no longer read as one: it is any post.
    let availability = envelope("availability", json!({"timeZone": "Europe/Zurich"}));
    let doc = card(&post("events", &id()), &availability);
    assert_eq!(
        (doc.start, fact(&doc, "kind")),
        (None, Some("availability"))
    );
    // A field it could not store (a NUL) or longer than its cap is left out, not the card.
    let unstorable = envelope(
        "game",
        json!({"title": "Sicilian\0Najdorf", "description": "Round 3"}),
    );
    let doc = card(&post("chess", &id()), &unstorable);
    assert_eq!(
        (doc.name.as_deref(), doc.description.as_deref()),
        (None, Some("Round 3"))
    );
    let long = envelope(
        "game",
        json!({"title": "N".repeat(201), "description": "D".repeat(2001), "image": "https://img.example/a.png"}),
    );
    let doc = card(&post("chess", &id()), &long);
    assert_eq!(
        (doc.name.as_deref(), doc.description.as_deref()),
        (None, None)
    );
    assert_eq!(doc.image.as_deref(), Some("https://img.example/a.png"));
    // A text, in JSON or raw, with a NUL is no excerpt; the card stays, with its facts.
    let json_text = envelope("note", json!({"text": "Blitz\0tonight"}));
    let mut raw_text = envelope("note", json!({}));
    raw_text["content"] = "Blitz\0tonight".into();
    for version in [&json_text, &raw_text] {
        let doc = card(&post("chess", &id()), version);
        assert_eq!(
            (&doc.status, doc.description.as_deref(), fact(&doc, "kind")),
            (&Status::Ok, None, Some("note"))
        );
    }
    // A namespace a stored text cannot hold (an encoded NUL in the path) or over 100
    // characters: no card.
    assert!(no_card(&card(&post("ch%00ess", &id()), &move_)));
    assert!(no_card(&card(&post(&"c".repeat(101), &id()), &move_)));
    // The envelope's checks hold for these too.
    assert!(no_card(&card(&post("chess", "0000000000000"), &move_)));

    // A route in the music namespace, an id the specs refuse, an explicit `null` where the
    // envelope wants a list.
    assert!(no_card(&card(&post("music", &id()), &route)));
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
    let named = |name: &str| {
        envelope(
            "route",
            json!({"name": name, "waypoints": [{"uri": "geo:46.95,7.44"}, {"uri": "geo:46.9,7.5"}]}),
        )
    };
    assert!(!no_card(&card(&post("maps", &id()), &named("Aare walk"))));
    assert!(no_card(&card(&post("maps", &id()), &named("Aare\0walk"))));
    let mut session = envelope(
        "session",
        json!({"started_at": 1_792_152_000_000_000_i64, "host": private}),
    );
    session["embed"] = "https://tracks.example/track/1".into();
    assert!(no_card(&card(&post("music", &id()), &session)));
}

/// A Pubky resolver standing in for the homeserver: it reads the post as `name`, or fails.
struct Homeserver(Option<&'static str>);
#[async_trait::async_trait]
impl Resolver for Homeserver {
    fn refresh_after(&self, _: &ResourceMeta) -> std::time::Duration {
        std::time::Duration::from_secs(7 * 86_400)
    }
    fn name(&self) -> &'static str {
        "pubky"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        let name = self
            .0
            .ok_or_else(|| anyhow::anyhow!("homeserver unreachable"))?;
        let mut doc = ResourceMeta::new(uri, "pubky", 7 * 86_400);
        doc.set_type("Trip");
        doc.name = Some(name.into());
        Ok(doc)
    }
}
async fn card_name(pool: &PgPool, uri: &str) -> Option<String> {
    let (id, _, _) = identity(uri).unwrap();
    sqlx::query_scalar("SELECT doc->>'name' FROM resource WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A post whose every read failed (its homeserver down) until it was dead-lettered, never
/// resolved: a new version line makes it due again, and it resolves (review of f275248).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_dead_lettered_post_is_read_again_at_a_new_version(pool: PgPool) {
    let metrics = Metrics::new().unwrap();
    let cfg = QueueConfig::default();
    let route = post("maps", &id());
    batch(
        &pool,
        vec![line(Op::Put, &route, "0034A", Utc::now())],
        &metrics,
    )
    .await;
    let (rid, _, _) = identity(&route).unwrap();
    // Its first reads all failed: dead-lettered, never resolved.
    sqlx::query("UPDATE resource SET dead_letter=true,attempts=$2,resolved_at=NULL,last_error='unreachable',next_attempt_at=now()+interval '7 days' WHERE id=$1")
        .bind(&rid)
        .bind(cfg.max_failures as i32)
        .execute(&pool)
        .await
        .unwrap();
    assert!(queue::claim(&pool, "pubky", &cfg).await.unwrap().is_none());
    batch(
        &pool,
        vec![line(Op::Put, &route, "0034B", Utc::now())],
        &metrics,
    )
    .await;
    let job = queue::claim(&pool, "pubky", &cfg)
        .await
        .unwrap()
        .expect("the post is due again");
    queue::run_job(&pool, &Homeserver(Some("Aare walk")), &cfg, &job)
        .await
        .unwrap();
    assert_eq!(card_name(&pool, &route).await.as_deref(), Some("Aare walk"));
}

/// A read in flight when a newer version arrives is fenced: what it read (the older version) is
/// not saved with the card's normal TTL over the newer version's "due now"; the post is read
/// again at once and keeps the newer version (review of f275248).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_read_in_flight_never_undoes_a_newer_version(pool: PgPool) {
    let metrics = Metrics::new().unwrap();
    let cfg = QueueConfig::default();
    let route = post("maps", &id());
    batch(
        &pool,
        vec![line(
            Op::Put,
            &route,
            "0034A",
            Utc::now() - Duration::hours(2),
        )],
        &metrics,
    )
    .await;
    let first = queue::claim(&pool, "pubky", &cfg).await.unwrap().unwrap();
    queue::run_job(&pool, &Homeserver(Some("Version A")), &cfg, &first)
        .await
        .unwrap();
    assert_eq!(card_name(&pool, &route).await.as_deref(), Some("Version A"));
    assert!(!due(&pool, &route).await);
    // A refresh starts reading version A again…
    let (rid, _, _) = identity(&route).unwrap();
    sqlx::query("UPDATE resource SET next_attempt_at=now(),refresh_at=now() WHERE id=$1")
        .bind(&rid)
        .execute(&pool)
        .await
        .unwrap();
    let in_flight = queue::claim(&pool, "pubky", &cfg).await.unwrap().unwrap();
    // …when the feed brings version B (written a second after that read began).
    batch(
        &pool,
        vec![line(
            Op::Put,
            &route,
            "0034B",
            Utc::now() + Duration::seconds(1),
        )],
        &metrics,
    )
    .await;
    // The read in flight answers with version A: not saved, the post stays due now.
    queue::run_job(
        &pool,
        &Homeserver(Some("Version A, again")),
        &cfg,
        &in_flight,
    )
    .await
    .unwrap();
    assert_eq!(card_name(&pool, &route).await.as_deref(), Some("Version A"));
    assert!(due(&pool, &route).await);
    let next = queue::claim(&pool, "pubky", &cfg)
        .await
        .unwrap()
        .expect("the post is read again at once");
    queue::run_job(&pool, &Homeserver(Some("Version B")), &cfg, &next)
        .await
        .unwrap();
    assert_eq!(card_name(&pool, &route).await.as_deref(), Some("Version B"));
    assert!(!due(&pool, &route).await);
}

/// A post in a namespace Pinakes never fetches (a social specs' event or rsvp, Nexus's; pubky.app's;
/// the old Mapky and Eventky layouts), however its namespace is spelt: its feed line is rejected
/// and counted (`never_fetched`), and it creates no resource or reference, so nothing fetches it.
/// A post of another app in the same batch is indexed.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_post_line_of_a_never_fetched_namespace_creates_nothing(pool: PgPool) {
    let metrics = Metrics::new().unwrap();
    let never: Vec<String> = [
        "social",
        "pubky.app",
        "mapky.app",
        "eventky.app",
        "%73ocial",
        "%73%6F%63%69%61%6C",
        "pubky%2Eapp",
        "mapky.ap%70",
        "eventky%2eapp",
    ]
    .iter()
    .map(|namespace| post(namespace, &id()))
    .collect();
    let mut lines = vec![];
    for uri in &never {
        lines.push(line(Op::Put, uri, "0034A", Utc::now()));
        lines.push(line(Op::Del, uri, "0034A", Utc::now()));
    }
    let chess = post("chess", &id());
    lines.push(line(Op::Put, &chess, "0034A", Utc::now()));
    batch(&pool, lines, &metrics).await;
    assert_eq!(
        metrics
            .ingest_rejected
            .with_label_values(&["never_fetched"])
            .get(),
        2 * never.len() as u64
    );
    let (chess_id, _, _) = identity(&chess).unwrap();
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT r.id,(SELECT count(*) FROM reference f WHERE f.resource_id=r.id) FROM resource r",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, [(chess_id, 1)]);
    let refs: i64 = sqlx::query_scalar("SELECT count(*) FROM reference")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(refs, 1);
    for uri in &never {
        assert_eq!(classify(uri), None, "{uri}");
    }
}

/// A post whose namespace is not one plain path segment once decoded (not UTF-8, an encoded `/`,
/// an encoded `.` or `..`) is refused as `uri` and creates no resource or reference: no spelling
/// gets past the post-path check. Letter case is kept: `Social` is not `social` but another app's
/// namespace, read as the resolver reads it (the general card), so it is indexed.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_post_line_whose_namespace_is_not_one_segment_creates_nothing(pool: PgPool) {
    let metrics = Metrics::new().unwrap();
    let malformed: Vec<String> = ["%FF", "ch%2Fess", "%2E", "%2e", "%2E%2E", ".%2E"]
        .iter()
        .map(|namespace| post(namespace, &id()))
        .collect();
    let social = post("Social", &id());
    let mut lines: Vec<Change> = malformed
        .iter()
        .map(|uri| line(Op::Put, uri, "0034A", Utc::now()))
        .collect();
    lines.push(line(Op::Put, &social, "0034A", Utc::now()));
    batch(&pool, lines, &metrics).await;
    let rejected = |reason| metrics.ingest_rejected.with_label_values(&[reason]).get();
    assert_eq!(
        (rejected("uri"), rejected("never_fetched")),
        (malformed.len() as u64, 0)
    );
    let (social_id, _, _) = identity(&social).unwrap();
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT r.id,(SELECT count(*) FROM reference f WHERE f.resource_id=r.id) FROM resource r",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, [(social_id, 1)]);
    let refs: i64 = sqlx::query_scalar("SELECT count(*) FROM reference")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(refs, 1);
    assert_eq!(classify(&social), Some("pubky"));
}

/// A Pubky resolver standing in for the homeserver that reads the post's version `body` as the
/// Pubky resolver does, now, with its usual TTL (a week).
struct Version(Value);
#[async_trait::async_trait]
impl Resolver for Version {
    fn refresh_after(&self, doc: &ResourceMeta) -> std::time::Duration {
        std::time::Duration::from_secs(doc.ttl_secs)
    }
    fn name(&self) -> &'static str {
        "pubky"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        Ok(card_with(
            uri,
            "application/octet-stream",
            &serde_json::to_vec(&self.0).unwrap(),
            7 * 86_400,
            Utc::now(),
            pinakes_core::event::time::MAX_PERIOD_DATES,
        ))
    }
}
/// The post's line queued, then read once by `version`.
async fn read_once(pool: &PgPool, uri: &str, version: Value) {
    let metrics = Metrics::new().unwrap();
    let cfg = QueueConfig::default();
    batch(
        pool,
        vec![line(Op::Put, uri, "0034A", Utc::now())],
        &metrics,
    )
    .await;
    let job = queue::claim(pool, "pubky", &cfg).await.unwrap().unwrap();
    queue::run_job(pool, &Version(version), &cfg, &job)
        .await
        .unwrap();
}
/// Seconds from now to the resource's next refresh and next attempt.
async fn next_read(pool: &PgPool, uri: &str) -> (f64, f64) {
    let (id, _, _) = identity(uri).unwrap();
    sqlx::query_as("SELECT extract(epoch FROM refresh_at-now())::float8,extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
fn us(at: DateTime<Utc>) -> i64 {
    at.timestamp_micros()
}

/// A listening or a session is read again when it expires, not at the queue's usual hour or
/// more after: its card never outlives `expires_at` by more than the queue's minute (review of
/// d8e4ceb, M1).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_expiring_card_is_read_again_when_it_expires(pool: PgPool) {
    let now = Utc::now();
    // Now playing for another half hour: read again then.
    let mut listening = envelope(
        "listening",
        json!({"started_at": us(now - Duration::minutes(5)), "expires_at": us(now + Duration::minutes(30))}),
    );
    listening["embed"] = "https://tracks.example/track/1".into();
    let uri = post("music", &id());
    read_once(&pool, &uri, listening).await;
    let (refresh, attempt) = next_read(&pool, &uri).await;
    assert!((1700.0..=1800.0).contains(&refresh), "{refresh}");
    assert!((1700.0..=1800.0).contains(&attempt), "{attempt}");
    // A session thirty seconds from its end: read again in a minute.
    let mut session = envelope(
        "session",
        json!({"started_at": us(now - Duration::hours(1)), "expires_at": us(now + Duration::seconds(30))}),
    );
    session["embed"] = "https://tracks.example/track/1".into();
    let uri = post("music", &id());
    read_once(&pool, &uri, session).await;
    let (refresh, attempt) = next_read(&pool, &uri).await;
    assert!((0.0..=60.0).contains(&refresh), "{refresh}");
    assert!((0.0..=60.0).contains(&attempt), "{attempt}");
    // A session without an expiry keeps its usual schedule (the queue's, a day here).
    let mut open = envelope("session", json!({"started_at": us(now)}));
    open["embed"] = "https://tracks.example/track/1".into();
    let uri = post("music", &id());
    read_once(&pool, &uri, open).await;
    let open = next_read(&pool, &uri).await.0;
    assert!(open > 3600.0, "{open}");
}

/// A post of an unprofiled namespace with a title no stored text may hold (a NUL), or one too
/// long, is saved with its card, without that title, on its first read (review of d8e4ceb, M2:
/// it used to fail to save and dead-letter).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_general_card_with_unstorable_text_is_saved_without_it(pool: PgPool) {
    for title in ["Sicilian\0Najdorf".to_owned(), "N".repeat(100_000)] {
        let uri = post("chess", &id());
        read_once(
            &pool,
            &uri,
            envelope("game", json!({"title": title, "description": "Round 3"})),
        )
        .await;
        let (id, _, _) = identity(&uri).unwrap();
        let (status, name, description, dead): (String, Option<String>, Option<String>, bool) =
            sqlx::query_as("SELECT status,doc->>'name',doc->>'description',dead_letter FROM resource WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (status.as_str(), name, description.as_deref(), dead),
            ("ok", None, Some("Round 3"), false)
        );
    }
}
