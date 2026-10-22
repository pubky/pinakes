use chrono::{Duration, Utc};
use pinakes_core::{
    config::EventConfig,
    event::{
        adapters::{
            self,
            cursor::{self, Checkpoint},
            state,
        },
        Occurrence,
    },
    model::{Point, ResourceMeta},
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn paginated_nostr_and_inconclusive_queue_retry(pool: PgPool) {
    use adapters::nostr::{self as nip52, Target};
    let other = signed(
        31923,
        200,
        &[
            &["d", "other"],
            &["title", "Other"],
            &["start", "1792519200"],
        ],
    );
    let wanted = signed(
        31923,
        100,
        &[
            &["d", "wanted"],
            &["title", "Wanted"],
            &["start", "1792519200"],
        ],
    );
    let target = Target::coordinate(&format!("31923:{}:wanted", wanted.pubkey.to_hex())).unwrap();
    let (egress, t) = mock_egress(vec![
        (200, json!(vec![other.clone(); 100])),
        (200, json!([wanted.clone()])),
    ]);
    assert_eq!(
        Box::pin(nip52::lookup(&target, &egress, &Default::default()))
            .await
            .unwrap()
            .unwrap()
            .id,
        wanted.id
    );
    assert!(t.seen.lock().unwrap()[1].0.contains("until=200"));
    let (egress, t) = mock_egress(vec![
        (200, json!([])),
        (200, json!(vec![other.clone(); 100])),
        (200, json!([wanted])),
    ]);
    assert!(
        Box::pin(nip52::lookup(&target, &egress, &Default::default()))
            .await
            .unwrap()
            .is_some()
    );
    assert!(t.seen.lock().unwrap()[2].0.contains("until=200"));
    let (egress, _) = mock_egress(vec![(404, Value::Null), (200, json!([]))]);
    assert!(
        Box::pin(nip52::lookup(&target, &egress, &Default::default()))
            .await
            .is_err()
    );
    let mut cfg = EventConfig::default();
    cfg.adapters.max_indexer_pages = 1;
    let (egress, transport) = mock_egress(vec![(200, json!(vec![other; 100])), (200, json!([]))]);
    let r = platform(&pool, cfg, egress);
    let uri = target.uri().unwrap();
    let id = store::ensure_resource(&pool, &uri).await.unwrap();
    let q = pinakes_core::config::QueueConfig {
        max_failures: 1,
        ..Default::default()
    };
    let job = pinakes_core::queue::claim(&pool, "event_platform", &q)
        .await
        .unwrap()
        .unwrap();
    Box::pin(pinakes_core::queue::run_job(&pool, &r, &q, &job))
        .await
        .unwrap();
    let state: (i32, bool, String) =
        sqlx::query_as("SELECT attempts,dead_letter,last_error FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, (0, false, "inconclusive".into()));
    use pinakes_core::resolver::Resolver;
    let err = r.resolve(&uri).await.unwrap_err();
    assert_eq!(
        err.downcast_ref::<pinakes_core::resolver::RetryLater>()
            .unwrap()
            .seconds,
        300
    );
    assert_eq!(
        transport.seen.lock().unwrap().len(),
        2,
        "retry reuses fetched pages"
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cursor_resets_head_clock_missing_and_title_fallback(pool: PgPool) {
    let cfg = EventConfig::default();
    let uri = "https://portal.einundzwanzig.space/api/changes";
    let def = adapters::definition("einundzwanzig").unwrap();
    store::ensure_resource(&pool, uri).await.unwrap();
    let (egress, t) = mock_egress(vec![
        (
            200,
            json!({"next_since":3000,"has_more":false,"changes":[]}),
        ),
        (200, json!([])),
    ]);
    let d = cursor::resolve(&pool, uri, def, &egress, &cfg, false)
        .await
        .unwrap();
    assert_eq!(d.container.unwrap().cursor.unwrap().next, "3000");
    assert!(!t.seen.lock().unwrap()[0].0.contains("since_sequence"));
    install_cursor(&pool, uri, "9999999999999999").await;
    let row = json!({"id":7,"meetup_id":1,"title":null,"meetup.portalLink":"https://portal.einundzwanzig.space/ch/meetup/one","start":(Utc::now()+Duration::days(1)).to_rfc3339()});
    let (egress, _) = mock_egress(vec![
        (
            200,
            json!({"next_since":3001,"cursor_expired":true,"has_more":false,"changes":[]}),
        ),
        (200, json!([row])),
        (503, Value::Null),
    ]);
    let d = cursor::resolve(&pool, uri, def, &egress, &cfg, false)
        .await
        .unwrap();
    let c = d.container.unwrap();
    assert!(c.cursor.unwrap().reset);
    assert_eq!(c.occurrences[0].doc.name.as_deref(), Some("Meetup 7"));
    // Its own portal page, not the API.
    assert_eq!(
        c.occurrences[0].doc.url,
        "https://portal.einundzwanzig.space/ch/meetup/one/event/7"
    );
    let uri = "https://api.btcmap.org/v4/events";
    let def = adapters::definition("btcmap").unwrap();
    let (egress, _) = mock_egress(vec![(
        200,
        json!([{"id":1,"status":"deleted","updated_at":"2035-01-01T00:00:00Z"}]),
    )]);
    let d = cursor::resolve(&pool, uri, def, &egress, &cfg, false)
        .await
        .unwrap();
    let next = d
        .container
        .as_ref()
        .unwrap()
        .cursor
        .as_ref()
        .unwrap()
        .next
        .parse::<chrono::DateTime<Utc>>()
        .unwrap();
    assert_eq!(next.timestamp_millis(), d.fetched_at);
    for (status, body) in [
        (404, Value::Null),
        (410, Value::Null),
        (200, json!({"id":1,"status":"deleted"})),
        (200, json!({"id":1,"status":"rejected"})),
    ] {
        let (egress, _) = mock_egress(vec![(status, body)]);
        let d = cursor::resolve(
            &pool,
            "https://api.btcmap.org/v4/events/1",
            def,
            &egress,
            &cfg,
            false,
        )
        .await
        .unwrap();
        assert_eq!(d.status, pinakes_core::model::Status::Missing);
    }
}

/// A derived link takes its parent's row before the adapter lock, as a feed apply does: the two
/// never deadlock, though neither waits for the other to start.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_derived_link_and_a_feed_apply_take_locks_in_one_order(pool: PgPool) {
    let parent = store::ensure_resource(&pool, "https://example.org/parent")
        .await
        .unwrap();
    // A feed apply (`changes::save`): the write order shared, the parent's row, then the adapter.
    let mut apply = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(pinakes_core::locks::WRITE_ORDER)
        .execute(&mut *apply)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR UPDATE")
        .bind(&parent)
        .execute(&mut *apply)
        .await
        .unwrap();
    let link = tokio::spawn({
        let pool = pool.clone();
        async move {
            store::derive_resource(
                &pool,
                "https://example.org/parent",
                "https://example.org/child",
                "btcmap",
                10,
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('btcmap',17614))")
        .execute(&mut *apply)
        .await
        .unwrap();
    apply.commit().await.unwrap();
    assert!(link.await.unwrap().unwrap().is_some());
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn derivation_slots_expire_and_adapter_total_is_bounded(pool: PgPool) {
    for p in [
        "https://example.org/parent-a",
        "https://example.org/parent-b",
    ] {
        store::ensure_resource(&pool, p).await.unwrap();
    }
    let until = Some(Utc::now() + Duration::days(1));
    let a = store::derive_resource_until(
        &pool,
        "https://example.org/parent-a",
        "https://example.org/a",
        "btcmap",
        1,
        1,
        until,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(store::derive_resource_until(
        &pool,
        "https://example.org/parent-b",
        "https://example.org/b",
        "btcmap",
        1,
        1,
        until
    )
    .await
    .unwrap()
    .is_none());
    sqlx::query(
        "UPDATE resource_derivation SET expires_at=now()-interval '1 second' WHERE child_id=$1",
    )
    .bind(&a)
    .execute(&pool)
    .await
    .unwrap();
    assert!(store::derive_resource_until(
        &pool,
        "https://example.org/parent-a",
        "https://example.org/b",
        "btcmap",
        1,
        1,
        until
    )
    .await
    .unwrap()
    .is_some());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource_derivation WHERE expires_at IS NULL OR expires_at>now()"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        store::ensure_resource(&pool, "https://example.org/a")
            .await
            .unwrap(),
        a
    );
}

struct WikiFailure {
    failure: u16,
    calls: std::sync::atomic::AtomicUsize,
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn wikipedia_entity_miss_keeps_article_alive(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    for status in [404, 410] {
        let uri = format!("https://en.wikipedia.org/wiki/Alive_{status}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let r = platform(
            &pool,
            EventConfig {
                geocoding: false,
                ..Default::default()
            },
            Egress::with_backends(
                Default::default(),
                Arc::new(AdapterDns),
                Arc::new(WikiFailure {
                    failure: status,
                    calls: Default::default(),
                }),
            ),
        );
        for _ in 0..2 {
            let doc = r.web.resolve(&uri).await.unwrap();
            assert_eq!(doc.status, pinakes_core::model::Status::Ok);
            assert_eq!(doc.type_name, "Article");
            store::save_document(&pool, &id, 0, &doc).await.unwrap();
        }
        let misses: i32 = sqlx::query_scalar("SELECT missing_streak FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(misses, 0);
    }
}

struct SecondaryFailure(u16);
#[async_trait::async_trait]
impl Transport for SecondaryFailure {
    async fn get(&self, u: &url::Url, _: &[SocketAddr]) -> anyhow::Result<Fetched> {
        let secondary = u.path().starts_with("/api/") || u.host_str() == Some("api.lu.ma");
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: u.clone(),
            status: if secondary { self.0 } else { 200 },
            content_type: "text/html".into(),
            body: if secondary {
                b"unavailable".to_vec()
            } else {
                b"<title>Event page</title><meta name='generator' content='Gancio'>".to_vec()
            },
            validators: Default::default(),
            location: None,
            retry_after_secs: 60,
        })
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn secondary_denial_or_miss_never_erases_event_start(pool: PgPool) {
    use pinakes_core::{queue, resolver::Resolver};
    for uri in [
        "https://self.example/event/42",
        "https://luma.com/event-metadata",
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut doc = ResourceMeta::new(uri, "web", 3600);
        doc.set_type("MusicEvent");
        doc.start = Some(Utc::now() + Duration::days(1));
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        let before: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        for status in [401, 403, 400, 404] {
            let r = platform(
                &pool,
                EventConfig {
                    geocoding: false,
                    ..Default::default()
                },
                Egress::with_backends(
                    Default::default(),
                    Arc::new(AdapterDns),
                    Arc::new(SecondaryFailure(status)),
                ),
            );
            if uri.starts_with("https://luma.com/") && matches!(status, 401 | 403) {
                assert!(r
                    .resolve(uri)
                    .await
                    .unwrap_err()
                    .is::<pinakes_core::resolver::RetryLater>());
            }
            queue::run_job(
                &pool,
                &r,
                &Default::default(),
                &queue::Job {
                    id: id.clone(),
                    uri: uri.into(),
                    attempts: 0,
                    generation: 0,
                },
            )
            .await
            .unwrap();
            let (after, attempts, dead): (Value, i32, bool) =
                sqlx::query_as("SELECT doc,attempts,dead_letter FROM resource WHERE id=$1")
                    .bind(&id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(before, after, "{uri}: {status}");
            assert_eq!((attempts, dead), (0, false));
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM resource_change WHERE resource_id=$1"
            )
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
    }
}
#[async_trait::async_trait]
impl Transport for WikiFailure {
    async fn get(&self, u: &url::Url, _: &[SocketAddr]) -> anyhow::Result<Fetched> {
        // The initial entity endpoint redirects to an HTML page identifying Q12345.
        let entity = u.path().ends_with("Special:EntityData/Q12345.json");
        if entity {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if entity && self.failure == 0 {
            return Err(pinakes_core::resolver::RetryLater {
                class: "fixture_timeout",
                seconds: 60,
            }
            .into());
        }
        if entity && self.failure == 1 {
            anyhow::bail!("fixture transport failure");
        }
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: u.clone(),
            status: if entity { self.failure } else { 200 },
            content_type: "text/html".into(),
            body: if !entity {
                include_bytes!("fixtures/adapters/wikipedia-item.html").to_vec()
            } else {
                b"invalid JSON".to_vec()
            },
            validators: Default::default(),
            location: None,
            retry_after_secs: 60,
        })
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn only_parse_errors_fall_back_and_plain_wikipedia_skips_fetch(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    for failure in [0, 1, 401, 403, 451, 503] {
        let uri = "https://www.wikidata.org/wiki/Q67890";
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut doc = ResourceMeta::new(uri, "wikidata", 3600);
        doc.set_type("Event");
        doc.start = Some(Utc::now() + Duration::days(1));
        doc.name = Some("Accepted".into());
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        let before: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let changes: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_change")
            .fetch_one(&pool)
            .await
            .unwrap();
        let transport = Arc::new(WikiFailure {
            failure,
            calls: Default::default(),
        });
        let egress =
            Egress::with_backends(Default::default(), Arc::new(AdapterDns), transport.clone());
        let r = platform(
            &pool,
            EventConfig {
                geocoding: false,
                ..Default::default()
            },
            egress,
        );
        // Two calls also cover a negatively cached transport failure.
        assert!(r.resolve(uri).await.is_err());
        if failure == 0 {
            assert!(r.resolve(uri).await.is_err());
        }
        assert!(r
            .web
            .resolve("https://en.wikipedia.org/wiki/Bitcoin_Conference")
            .await
            .is_err());
        let job = pinakes_core::queue::Job {
            id: id.clone(),
            uri: uri.into(),
            generation: 0,
            attempts: 0,
        };
        pinakes_core::queue::run_job(&pool, &r, &Default::default(), &job)
            .await
            .unwrap();
        if failure <= 1 {
            assert_eq!(
                transport.calls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "EntityData negative cache must survive page refetch"
            );
        }
        let after: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change")
                .fetch_one(&pool)
                .await
                .unwrap(),
            changes
        );
        // Reset fixture state for the next error case without changing immutable IDs.
        sqlx::query("UPDATE resource SET attempts=0,dead_letter=false WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let r = platform(
        &pool,
        EventConfig {
            geocoding: false,
            ..Default::default()
        },
        Egress::with_backends(
            Default::default(),
            Arc::new(AdapterDns),
            Arc::new(WikiFailure {
                failure: 200,
                calls: Default::default(),
            }),
        ),
    );
    assert_eq!(
        r.resolve("https://www.wikidata.org/wiki/Q67890")
            .await
            .unwrap()
            .type_name,
        "Article"
    );
    let uri = "https://en.wikipedia.org/wiki/Person";
    let body="<title>Person</title><meta property='og:type' content='article'><script>mw.config.set({\"wgWikibaseItemId\":\"Q42\"})</script>";
    let r = platform(
        &pool,
        EventConfig::default(),
        recorded_egress(vec![(uri, body, Value::Null)]),
    );
    assert_eq!(r.web.resolve(uri).await.unwrap().type_name, "Article");
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn billetto_denied_venue_never_dead_letters(pool: PgPool) {
    use pinakes_core::{
        queue,
        resolver::{osm::OsmResolver, Resolver},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/search",
                axum::routing::get(move || {
                    requests.fetch_add(1, Ordering::SeqCst);
                    async { (axum::http::StatusCode::FORBIDDEN, "Forbidden") }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let uri = "https://billetto.co.uk/e/venue-denied-tickets-123";
    let body = format!(
        "<script type='application/ld+json'>{}</script>",
        json!({
            "@type":"Event", "name":"Billetto fixture", "startDate":(Utc::now()+Duration::days(1)).to_rfc3339(),
            "location":{"@type":"Place","name":"Hall","address":{"@type":"PostalAddress","streetAddress":"1 Main St","addressLocality":"London","postalCode":"SW1A 1AA"}}
        })
    );
    let mut r = platform(
        &pool,
        EventConfig {
            ..Default::default()
        },
        recorded_egress(vec![(uri, &body, Value::Null)]),
    );
    Arc::get_mut(r.web.events.as_mut().unwrap()).unwrap().osm = Arc::new(
        OsmResolver::new(
            pool.clone(),
            pinakes_core::config::OsmConfig {
                nominatim_url: format!("http://{address}"),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let doc = r.resolve(uri).await.unwrap();
    assert_eq!(doc.type_name, "Event");
    assert!(doc.event.warnings.iter().any(|w| w == "venue_unavailable"));
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let q = pinakes_core::config::QueueConfig::default();
    for _ in 0..5 {
        sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let job = queue::claim(&pool, "event_platform", &q)
            .await
            .unwrap()
            .unwrap();
        queue::run_job(&pool, &r, &q, &job).await.unwrap();
        let (doc, attempts, dead): (Value, i32, bool) =
            sqlx::query_as("SELECT doc,attempts,dead_letter FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(doc["type"], "Event");
        assert!(doc["start"].is_string());
        assert!(doc["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("venue_unavailable")));
        assert_eq!((attempts, dead), (0, false));
    }
    server.abort();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "one geocode attempt per resolution"
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cursor_outages_preserve_documents_and_checkpoints(pool: PgPool) {
    use pinakes_core::{
        queue,
        resolver::{Resolver, RetryLater},
    };
    let mut cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    cfg.adapters.platforms.insert(
        "google_calendar_api".into(),
        adapters::Options {
            key: Some("fixture-key".into()),
            ..Default::default()
        },
    );
    for (uri, source, previous) in [
        (
            "https://portal.einundzwanzig.space/api/changes",
            "einundzwanzig",
            "42",
        ),
        (
            "https://api.btcmap.org/v4/events",
            "btcmap",
            "2026-10-15T00:00:00Z",
        ),
        (
            "https://www.googleapis.com/calendar/v3/calendars/public/events",
            "google_calendar_api",
            "accepted-token",
        ),
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut doc = ResourceMeta::new(uri, source, 3600);
        doc.set_type("Collection");
        doc.name = Some("Accepted collection".into());
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        install_cursor(&pool, uri, previous).await;
        let before: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        for status in [503, 408, 429] {
            let (egress, _) = mock_egress_with_retry(vec![(status, Value::Null); 6], 720);
            let r = platform(&pool, cfg.clone(), egress);
            let error = r.resolve(uri).await.unwrap_err();
            let retry = error.downcast_ref::<RetryLater>().unwrap();
            assert_eq!(retry.seconds, 720);
            assert_eq!(
                retry.class,
                if status == 503 {
                    "http_5xx"
                } else {
                    "http_4xx"
                }
            );
            for _ in 0..5 {
                let job = queue::Job {
                    id: id.clone(),
                    uri: uri.into(),
                    generation: 0,
                    attempts: 0,
                };
                queue::run_job(&pool, &r, &Default::default(), &job)
                    .await
                    .unwrap();
            }
            let (after, attempts, dead, delay): (Value, i32, bool, bool) = sqlx::query_as("SELECT doc,attempts,dead_letter,next_attempt_at>now()+interval '11 minutes' FROM resource WHERE id=$1").bind(&id).fetch_one(&pool).await.unwrap();
            assert_eq!(after, before);
            assert_eq!((attempts, dead, delay), (0, false, true));
            assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some(previous));
        }
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn btc_status_drift_preserves_single_event(pool: PgPool) {
    use pinakes_core::{
        queue,
        resolver::{Resolver, RetryLater},
    };
    let uri = "https://api.btcmap.org/v4/events/1";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "btcmap", 3600);
    doc.set_type("Event");
    doc.start = Some(Utc::now() + Duration::days(1));
    doc.name = Some("Accepted event".into());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let before: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    for body in [json!({"id":1,"status":"new-unknown"}), json!({"id":1})] {
        let (egress, _) = mock_egress(vec![(200, body); 6]);
        let r = platform(&pool, EventConfig::default(), egress);
        let error = r.resolve(uri).await.unwrap_err();
        assert_eq!(error.downcast_ref::<RetryLater>().unwrap().class, "drift");
        for _ in 0..5 {
            queue::run_job(
                &pool,
                &r,
                &Default::default(),
                &queue::Job {
                    id: id.clone(),
                    uri: uri.into(),
                    generation: 0,
                    attempts: 0,
                },
            )
            .await
            .unwrap();
        }
        let (after, attempts, dead, misses): (Value, i32, bool, i32) = sqlx::query_as(
            "SELECT doc,attempts,dead_letter,missing_streak FROM resource WHERE id=$1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after, before);
        assert_eq!((attempts, dead, misses), (0, false, 0));
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn portal_expiration_restarts_comparison_baseline(pool: PgPool) {
    let uri = "https://portal.einundzwanzig.space/api/changes";
    install_cursor(&pool, uri, "900").await;
    let (egress, transport) = mock_egress(vec![
        (
            200,
            json!({"next_since":10,"cursor_expired":true,"has_more":true,"changes":[]}),
        ),
        (200, json!({"next_since":11,"has_more":false,"changes":[]})),
        (200, json!([])),
    ]);
    let doc = cursor::resolve(
        &pool,
        uri,
        adapters::definition("einundzwanzig").unwrap(),
        &egress,
        &Default::default(),
        false,
    )
    .await
    .unwrap();
    let c = doc.container.as_ref().unwrap();
    let checkpoint = c.cursor.as_ref().unwrap();
    assert_eq!(checkpoint.previous.as_deref(), Some("900"));
    assert_eq!(checkpoint.next, "11");
    assert!(checkpoint.reset);
    assert!(!checkpoint.resync && !c.delta);
    assert!(transport.seen.lock().unwrap()[1]
        .0
        .contains("since_sequence=10"));
    let id = pinakes_core::model::identity(uri).unwrap().0;
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("11"));
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn slow_optional_venue_keeps_single_event_and_attempts(pool: PgPool) {
    use pinakes_core::{queue, resolver::osm::OsmResolver};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/search",
                axum::routing::get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    axum::Json(json!([]))
                }),
            ),
        )
        .await
        .unwrap();
    });
    let cfg = EventConfig {
        ..Default::default()
    };
    let (egress, _) = mock_egress(vec![
        (200, json!({})),
        (
            200,
            json!({"data":{"event":{"name":"Slow venue", "start_at":(Utc::now()+Duration::days(1)).to_rfc3339(),"geo_address_info":{"name":"Hall","address":{"streetAddress":"1 Main St","addressLocality":"Zurich"}}}}}),
        ),
    ]);
    let mut r = platform(&pool, cfg, egress);
    Arc::get_mut(r.web.events.as_mut().unwrap()).unwrap().osm = Arc::new(
        OsmResolver::new(
            pool.clone(),
            pinakes_core::config::OsmConfig {
                nominatim_url: format!("http://{address}"),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let uri = "https://luma.com/slow-venue";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let q = pinakes_core::config::QueueConfig {
        max_failures: 1,
        ..Default::default()
    };
    let job = queue::claim(&pool, "event_platform", &q)
        .await
        .unwrap()
        .unwrap();
    queue::run_job(&pool, &r, &q, &job).await.unwrap();
    server.abort();
    let (doc, attempts, dead): (Value, i32, bool) =
        sqlx::query_as("SELECT doc,attempts,dead_letter FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(doc["type"], "Event");
    assert!(doc["start"].is_string());
    assert!(doc["warnings"]
        .as_array()
        .unwrap()
        .contains(&json!("venue_unavailable")));
    assert_eq!((attempts, dead), (0, false));
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn graphql_failure_preserves_accepted_event(pool: PgPool) {
    use pinakes_core::{queue, resolver::RetryLater};
    let uri = "https://mobilizon.fr/events/86a665ae-e495-4e4f-88d4-4cffd44a82f1";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut accepted = ResourceMeta::new(uri, "mobilizon", 3600);
    accepted.set_type("Event");
    accepted.start = Some(Utc::now() + Duration::days(1));
    store::save_document(&pool, &id, 0, &accepted)
        .await
        .unwrap();
    let before: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (egress, _) = mock_egress(vec![
        (200, json!({})),
        (
            200,
            json!({"errors":[{"message":"temporary upstream error"}]}),
        ),
    ]);
    let r = platform(
        &pool,
        EventConfig {
            geocoding: false,
            ..Default::default()
        },
        egress,
    );
    use pinakes_core::resolver::Resolver;
    let error = r.resolve(uri).await.unwrap_err();
    assert_eq!(error.downcast_ref::<RetryLater>().unwrap().class, "graphql");
    // Reuse the same secondary failure cache when the queue retries.
    let (egress, _) = mock_egress(vec![
        (200, json!({})),
        (200, json!({"errors":[{"message":"temporary"}]})),
    ]);
    let r = platform(
        &pool,
        EventConfig {
            geocoding: false,
            ..Default::default()
        },
        egress,
    );
    queue::run_job(
        &pool,
        &r,
        &pinakes_core::config::QueueConfig {
            max_failures: 1,
            ..Default::default()
        },
        &queue::Job {
            id: id.clone(),
            uri: uri.into(),
            generation: 0,
            attempts: 0,
        },
    )
    .await
    .unwrap();
    let (after, attempts, dead): (Value, i32, bool) =
        sqlx::query_as("SELECT doc,attempts,dead_letter FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert_eq!((attempts, dead), (0, false));
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn deleted_wikidata_confirms_missing_twice(pool: PgPool) {
    use pinakes_core::{model::Status, resolver::Resolver};
    for status in [404, 410] {
        let uri = format!("https://www.wikidata.org/wiki/Q678{status}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let mut doc = ResourceMeta::new(&uri, "wikidata", 3600);
        doc.set_type("Event");
        doc.start = Some(Utc::now() + Duration::days(1));
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        for expected in ["ok", "missing"] {
            // Each observation is a fresh fetch, as it is after the one-hour Missing cadence.
            let r = platform(
                &pool,
                EventConfig {
                    geocoding: false,
                    ..Default::default()
                },
                Egress::with_backends(
                    Default::default(),
                    Arc::new(AdapterDns),
                    Arc::new(WikiFailure {
                        failure: status,
                        calls: Default::default(),
                    }),
                ),
            );
            let d = r.resolve(&uri).await.unwrap();
            assert_eq!(d.status, Status::Missing);
            store::save_document(&pool, &id, 0, &d).await.unwrap();
            let state: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(state, expected);
        }
    }
}

#[test]
fn manifests_cover_every_platform_and_keys_disable_without_exposing_credentials() {
    let mut cfg = EventConfig::default();
    let ids: std::collections::BTreeSet<_> =
        adapters::definitions().iter().map(|d| &d.id).collect();
    assert_eq!(ids.len(), adapters::definitions().len());
    for id in [
        "einundzwanzig",
        "btcmap",
        "nip52",
        "google_calendar_api",
        "luma",
        "meetup",
        "eventbrite",
        "partiful",
        "posh",
        "ringcentral",
        "airmeet",
        "mobilizon",
        "gancio",
        "wikidata",
        "dice",
        "tito",
        "petzi",
        "weezevent",
        "pretix",
        "eventyay",
        "gathio",
        "guidle",
        "eventfrog",
        "allevents",
        "ticketmaster",
        "skiddle",
        "billetto_api",
        "eventfrog_api",
        "resident_advisor",
    ] {
        assert!(adapters::definition(id).is_some(), "{id}");
    }
    for m in adapters::manifests(&cfg).iter().filter(|m| m.requires_key) {
        assert!(!m.enabled);
        assert_eq!(m.disabled_reason.as_deref(), Some("missing API key"));
    }
    assert!(!adapters::manifest(adapters::definition("resident_advisor").unwrap(), &cfg).enabled);
    cfg.adapters.platforms.insert(
        "ticketmaster".into(),
        adapters::Options {
            key: Some("secret-fixture-key".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        adapters::detect("https://billetto.dk/api/v3/public/events/123")
            .unwrap()
            .id,
        "billetto_api"
    );
    let target = adapters::service::endpoint(
        adapters::definition("ticketmaster").unwrap(),
        "https://app.ticketmaster.com/discovery/v2/events/abc.json",
        None,
        &cfg,
    )
    .unwrap()
    .unwrap()
    .0;
    assert!(target.contains("/abc.json?"));
    assert!(!target.contains(".json.json"));
    let m = adapters::manifest(adapters::definition("ticketmaster").unwrap(), &cfg);
    assert!(m.enabled);
    assert!(!serde_json::to_string(&m)
        .unwrap()
        .contains("secret-fixture-key"));
    assert!(
        adapters::manifest(adapters::definition("facebook").unwrap(), &cfg).cadence_floor
            >= 7 * 86400
    );
    cfg.adapters
        .platforms
        .insert("no-such-platform".into(), Default::default());
    assert!(adapters::validate(&cfg).is_err());
    assert!(adapters::profile("browser").contains("Chrome/"));
    assert!(adapters::profile("firefox").contains("Firefox/"));
    assert_eq!(adapters::profile("plain"), "curl/8.10.1");
    assert_eq!(
        adapters::detect("https://pretix.example/test/ical/")
            .unwrap()
            .id,
        "ics"
    );
    assert_eq!(
        adapters::detected_html(
            "https://tickets.example/org/test/",
            "/static/pretixbase/ /ical/"
        )
        .unwrap()
        .id,
        "pretix"
    );
}

#[test]
fn embedded_platform_shapes_and_jsonld_precedence() {
    let cases = [
        (
            "luma",
            json!({"data":{"event":{"name":"Luma","start_at":"2026-10-20T17:00:00Z","end_at":"2026-10-20T18:00:00Z","timezone":"Europe/Zurich","coordinate":{"latitude":47,"longitude":8}}}}),
        ),
        (
            "meetup",
            json!({"props":{"pageProps":{"event":{"title":"Meetup","dateTime":"2026-10-20T17:00:00Z","group":{"timezone":"Europe/Zurich"},"venue":{"lat":47,"lon":8}}}}}),
        ),
        (
            "eventbrite",
            json!({"event":{"name":"Eventbrite","startDate":{"utc":"2026-10-20T17:00:00Z","timezone":"Europe/Zurich"},"venue":{"address":{"latitude":47,"longitude":8}},"status":"cancelled"}}),
        ),
        (
            "partiful",
            json!({"props":{"pageProps":{"event":{"title":"Partiful","startDate":"2026-10-20T17:00:00Z","timezone":"Europe/Zurich","guestAction":"APPLY"}}}}),
        ),
        (
            "posh",
            json!({"event":{"name":"Posh","startUtc":"2026-10-20T17:00:00Z","timezone":"Europe/Zurich","lat":47,"lng":8}}),
        ),
        (
            "ringcentral",
            json!({"props":{"pageProps":{"event":{"name":"RingCentral","time_start":"2026-10-20T17:00:00Z","timezone":"Eastern Time (US & Canada)","venue_type":"virtual"}}}}),
        ),
        (
            "airmeet",
            json!({"name":"Airmeet","start_time":1792515600000i64,"end_time":1792519200000i64,"timezone":"Europe/Zurich"}),
        ),
        (
            "mobilizon",
            json!({"data":{"event":{"title":"Mobilizon","beginsOn":"2026-10-20T17:00:00Z","timezone":"Europe/Zurich","isOnline":true}}}),
        ),
        (
            "gancio",
            json!({"title":"Gancio","start_datetime":1792515600i64,"place":{"lat":47,"lon":8}}),
        ),
        (
            "ticketmaster",
            json!({"name":"Ticketmaster","dates":{"start":{"dateTime":"2026-10-20T17:00:00Z"},"timezone":"Europe/Zurich","status":{"code":"postponed"}},"_embedded":{"venues":[{"name":"Hall","location":{"latitude":"47","longitude":"8"}}]}}),
        ),
        (
            "skiddle",
            json!({"results":{"eventname":"Skiddle","startdate":"2026-10-20T18:00:00+01:00","venue":{"latitude":47,"longitude":8}}}),
        ),
        (
            "billetto_api",
            json!({"event":{"name":"Billetto","start_at":"2026-10-20T17:00:00Z","state":"canceled"}}),
        ),
        (
            "eventfrog_api",
            json!({"events":[{"name":"Eventfrog","startDate":"2026-10-20T17:00:00Z","cancelled":true}]}),
        ),
        (
            "resident_advisor",
            json!({"data":{"event":{"title":"RA","startTime":"2026-10-20T19:00:00","venue":{"area":{"ianaTimeZone":"Europe/Zurich"},"location":{"latitude":47,"longitude":8}}}}}),
        ),
        (
            "xceed",
            json!({"event":{"name":"Xceed","startDate":"2026-10-20T17:00:00Z"}}),
        ),
    ];
    for (id, v) in cases {
        let html = if matches!(id, "posh" | "xceed") {
            format!(
                "<script>self.__next_f.push([1,{}]);</script>",
                serde_json::to_string(&v.to_string()).unwrap()
            )
        } else {
            format!("<script type='application/json'>{v}</script>")
        };
        let doc = state::parse(id, "https://example.org/event", html.as_bytes())
            .unwrap_or_else(|| panic!("{id}"));
        assert!(doc.start.is_some(), "{id}");
        assert!(doc.name.is_some(), "{id}");
        if id == "eventbrite" {
            assert_eq!(
                doc.event.event_status,
                Some(pinakes_core::event::EventStatus::Cancelled)
            );
            assert!(doc.location.is_some());
        }
        if id == "ringcentral" {
            assert_eq!(
                doc.event.tz.as_deref(),
                Some("America/New_York"),
                "html={html}, direct={:?}",
                state::object(
                    id,
                    "https://example.org/e",
                    &v["props"]["pageProps"]["event"]
                )
                .unwrap()
                .event
                .tz
            );
        }
    }
    // A JSON-in-string payload is parsed before its text is decoded: an escaped quote inside it
    // must not end a string early.
    let payload = json!({"event":{"name":"Tom &amp; Jerry","description":"say &quot;hi&quot;","startDate":"2026-10-20T17:00:00Z"}});
    let html = format!(
        "<script>self.__next_f.push([1,{}]);</script>",
        serde_json::to_string(&payload.to_string()).unwrap()
    );
    let d = state::parse("posh", "https://example.org/event", html.as_bytes()).unwrap();
    assert_eq!(d.name.as_deref(), Some("Tom & Jerry"));
    assert_eq!(d.description.as_deref(), Some(r#"say "hi""#));
    let nuxt = r#"<script id="__NUXT_DATA__">[{"event":1},{"name":2,"startDate":3},"Nuxt Concert","2026-10-20T17:00:00Z"]</script>"#;
    let d = state::parse("ents24", "https://example.org/e", nuxt.as_bytes()).unwrap();
    assert_eq!(d.name.as_deref(), Some("Nuxt Concert"));
    assert_eq!(d.start.unwrap().timestamp(), 1792515600);
    let atc = r#"<title>Ticketino</title><span class="atc_date_start">2026-10-20 19:00:00</span><span class="atc_date_end">2026-10-20 21:00:00</span><span class="atc_timezone">Europe/Zurich</span>"#;
    assert_eq!(
        state::parse("ticketino", "https://example.org/e", atc.as_bytes())
            .unwrap()
            .event
            .tz
            .as_deref(),
        Some("Europe/Zurich")
    );
    assert!(state::parse(
        "luma",
        "https://example.org/e",
        br#"{"recommendations":[{"name":"Unrelated","startDate":"2026-10-20T17:00:00Z"}]}"#
    )
    .is_none());
}

fn snapshot(uri: &str, n: usize, at: i64) -> ResourceMeta {
    let mut doc = cursor::container(uri, &EventConfig::default());
    doc.fetched_at = at;
    for i in 0..n {
        let mut d = ResourceMeta::new(uri, "btcmap", 3600);
        d.set_type("Event");
        d.start = Some(Utc::now() + Duration::days(1));
        d.end = d.start;
        d.name = Some(format!("Event {i}"));
        d.location = Some(Point { lat: 47., lon: 8. });
        doc.container
            .as_mut()
            .unwrap()
            .occurrences
            .push(Occurrence {
                uid: i.to_string(),
                recurrence_id: String::new(),
                doc: d,
                sequence: 0,
                modified: None,
                content_hash: i.to_string(),
            });
    }
    doc
}
fn checkpoint(doc: &mut ResourceMeta, id: &str, previous: Option<&str>, next: &str, reset: bool) {
    doc.container.as_mut().unwrap().cursor = Some(Checkpoint {
        source: id.into(),
        adapter: "btcmap".into(),
        previous: previous.map(str::to_owned),
        next: next.into(),
        reset,
        resync: false,
    });
}
async fn saved_cursor(pool: &PgPool, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT adapter_cursor FROM event_fetch WHERE resource_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cursor_deltas_tombstones_rollback_hold_reset_and_compare_swap(pool: PgPool) {
    let uri = "https://example.org/cursor.ics";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let now = Utc::now().timestamp_millis();
    let mut d = snapshot(uri, 3, now);
    checkpoint(&mut d, &id, None, "1", false);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let mut delta = snapshot(uri, 1, now + 1);
    delta.container.as_mut().unwrap().delta = true;
    delta.container.as_mut().unwrap().occurrences[0].doc.name = Some("Changed".into());
    delta.container.as_mut().unwrap().deleted_uids = vec!["1".into()];
    checkpoint(&mut delta, &id, Some("1"), "2", false);
    store::save_document(&pool, &id, 0, &delta).await.unwrap();
    let states: Vec<(String, String)> =
        sqlx::query_as("SELECT uid,status FROM occurrence ORDER BY uid")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(states[1].1, "cancelled");
    assert_eq!(states[2].1, "scheduled");
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("2"));
    delta.fetched_at += 1;
    checkpoint(&mut delta, &id, Some("1"), "stale", false);
    store::save_document(&pool, &id, 0, &delta).await.unwrap();
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("2"));
    sqlx::query("ALTER TABLE occurrence ADD CONSTRAINT fixture_reject CHECK(name<>'reject')")
        .execute(&pool)
        .await
        .unwrap();
    delta.fetched_at += 1;
    delta.container.as_mut().unwrap().occurrences[0].doc.name = Some("reject".into());
    checkpoint(&mut delta, &id, Some("2"), "3", false);
    assert!(store::save_document(&pool, &id, 0, &delta).await.is_err());
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("2"));
    let mut empty = snapshot(uri, 0, now + 4);
    checkpoint(&mut empty, &id, Some("2"), "4", true);
    store::save_document(&pool, &id, 0, &empty).await.unwrap();
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("2"));
    empty.fetched_at += 1;
    store::save_document(&pool, &id, 0, &empty).await.unwrap();
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("4"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT cursor_resets_total FROM source_state WHERE source='btcmap'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM source_state WHERE source LIKE '%:%'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

fn signed(kind: u16, created: u64, tags: &[&[&str]]) -> nostr::event::Event {
    use nostr::prelude::*;
    // Deliberately public deterministic fixture key; never used for a real identity.
    let keys = Keys::parse(&"01".repeat(32)).unwrap();
    let unsigned = EventBuilder::new(Kind::from(kind), "Fixture description")
        .tags(tags.iter().map(|t| Tag::parse(t.iter().copied()).unwrap()))
        .custom_created_at(Timestamp::from_secs(created))
        .finalize_unsigned(keys.public_key());
    let sig = keys.sign_schnorr_with_aux_rand(
        &secp256k1::Secp256k1::new(),
        unsigned.compute_id().as_bytes(),
        &[0; 32],
    );
    unsigned.add_signature(sig).unwrap()
}
#[test]
fn nip19_vectors_client_forms_and_signed_nip52_revisions() {
    use nostr::prelude::{Nip19Event, ToBech32};
    use pinakes_core::event::adapters::nostr::{self as nip52, Target};
    let public =
        Target::decode("nostr:npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg")
            .unwrap();
    assert_eq!(
        public.author.as_deref(),
        Some("7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e")
    );
    let profile=Target::decode("nostr:nprofile1qqsrhuxx8l9ex335q7he0f09aej04zpazpl0ne2cgukyawd24mayt8gpp4mhxue69uhhytnc9e3k7mgpz4mhxue69uhkg6nzv9ejuumpv34kytnrdaksjlyr9p").unwrap();
    assert_eq!(
        profile.author.as_deref(),
        Some("3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d")
    );
    assert_eq!(
        profile.relays,
        vec!["wss://r.x.com", "wss://djbas.sadkb.com"]
    );
    assert!(Target::decode(
        "nostr:nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5"
    )
    .is_err());
    let old = signed(
        31923,
        100,
        &[
            &["d", "fixture"],
            &["title", "Old"],
            &["start", "1792515600"],
            &["start_tzid", "Europe/Zurich"],
            &["g", "47", "8"],
        ],
    );
    let new = signed(
        31923,
        101,
        &[
            &["d", "fixture"],
            &["title", "New"],
            &["start", "1792519200"],
        ],
    );
    let coordinate = Target::coordinate(&format!("31923:{}:fixture", old.pubkey.to_hex())).unwrap();
    let uri = coordinate.uri().unwrap();
    assert_eq!(
        Target::decode(&uri).unwrap().filter()["#d"],
        json!(["fixture"])
    );
    for base in [
        "https://flockstr.com/event/",
        "https://njump.me/",
        "https://nostr.at/",
        "https://honey.hivetalk.org/dashboard/event/",
        "https://letsmiti.app/calendar/",
    ] {
        assert!(
            Target::decode(&format!("{base}{}", uri.trim_start_matches("nostr:")))
                .unwrap()
                .matches(&old)
        );
    }
    let mut forged = json!(new);
    forged["content"] = json!("tampered");
    assert_eq!(
        nip52::newest(&coordinate, &[json!(old), forged])
            .unwrap()
            .id,
        old.id
    );
    assert_eq!(
        nip52::newest(&coordinate, &[json!(new), json!(old)])
            .unwrap()
            .id,
        new.id
    );
    let same_time = signed(
        31923,
        101,
        &[
            &["d", "fixture"],
            &["title", "Tie"],
            &["start", "1792519200"],
        ],
    );
    assert_eq!(
        nip52::newest(&coordinate, &[json!(new), json!(same_time)])
            .unwrap()
            .id,
        new.id.min(same_time.id)
    );
    let pinned = Target::decode(&format!(
        "nostr:{}",
        Nip19Event::from(&old).to_bech32().unwrap()
    ))
    .unwrap();
    assert_eq!(
        nip52::newest(&pinned, &[json!(new), json!(old)])
            .unwrap()
            .id,
        old.id
    );
    let parsed = nip52::parse(&uri, &old).unwrap();
    assert_eq!(parsed.event.tz.as_deref(), Some("Europe/Zurich"));
    assert_eq!(parsed.location.unwrap().lat, 47.);
    assert!(parsed
        .event
        .warnings
        .iter()
        .any(|s| s == "nostr_legacy_missing_D"));
    let all_day = signed(
        31922,
        102,
        &[
            &["d", "day"],
            &["title", "Day"],
            &["start", "2026-10-20"],
            &["g", "u0qj"],
        ],
    );
    let parsed = nip52::parse(&uri, &all_day).unwrap();
    assert_eq!(
        parsed.end.unwrap() - parsed.start.unwrap(),
        Duration::days(1)
    );
    assert_eq!(parsed.event.all_day, Some(true));
    assert_eq!(parsed.event.tz, None);
    assert!(parsed.location.is_some());
    assert!(nip52::parse(&uri, &signed(31925, 100, &[&["d", "rsvp"]])).is_err());
}

use pinakes_core::resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
struct AdapterDns;
#[async_trait::async_trait]
impl Dns for AdapterDns {
    async fn lookup(&self, host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new(
            if host == "private.example" {
                "127.0.0.1"
            } else {
                "93.184.216.34"
            }
            .parse()?,
            port,
        )])
    }
}
struct Replies {
    retry_after_secs: u64,
    replies: Mutex<VecDeque<(u16, Value)>>,
    seen: Mutex<Vec<(String, FetchOptions)>>,
}
#[async_trait::async_trait]
impl Transport for Replies {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &Default::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        options: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen
            .lock()
            .unwrap()
            .push((url.to_string(), options.clone()));
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected HTTP request {url} {:?}", options.accept));
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            validators: Default::default(),
            content_type: "application/json".into(),
            status,
            retry_after_secs: self.retry_after_secs,
            location: (status == 302).then(|| "https://foreign.example/".into()),
            body: serde_json::to_vec(&body)?,
        })
    }
}
fn mock_egress(replies: Vec<(u16, Value)>) -> (Egress, Arc<Replies>) {
    mock_egress_with_retry(replies, 60)
}
fn mock_egress_with_retry(
    replies: Vec<(u16, Value)>,
    retry_after_secs: u64,
) -> (Egress, Arc<Replies>) {
    let transport = Arc::new(Replies {
        retry_after_secs,
        replies: Mutex::new(replies.into()),
        seen: Mutex::default(),
    });
    (
        Egress::with_backends(
            pinakes_core::config::EgressConfig {
                host_requests_per_second: 1000.,
                ..Default::default()
            },
            Arc::new(AdapterDns),
            transport.clone(),
        ),
        transport,
    )
}
#[tokio::test]
async fn nostr_indexer_outage_is_not_disappearance_and_private_relays_never_connect() {
    use pinakes_core::event::adapters::nostr::{self as nip52, Target};
    let event = signed(
        31923,
        100,
        &[
            &["d", "fixture"],
            &["title", "Event"],
            &["start", "1792515600"],
        ],
    );
    let target = Target::coordinate(&format!("31923:{}:fixture", event.pubkey.to_hex())).unwrap();
    let (egress, t) = mock_egress(vec![(200, json!([])), (503, Value::Null)]);
    assert!(nip52::lookup(&target, &egress, &EventConfig::default())
        .await
        .is_err());
    assert!(nip52::lookup(&target, &egress, &EventConfig::default())
        .await
        .is_err());
    assert_eq!(
        t.seen.lock().unwrap().len(),
        2,
        "failed indexer request is cached too"
    );
    let (egress, _) = mock_egress(vec![(200, json!([])), (200, json!([]))]);
    assert!(nip52::lookup(&target, &egress, &EventConfig::default())
        .await
        .unwrap()
        .is_none());
    assert!(egress
        .nostr_request("wss://private.example/", target.filter(), 100)
        .await
        .unwrap_err()
        .to_string()
        .contains("non-public"));
    let (egress, t) = mock_egress(vec![(200, json!([event]))]);
    assert!(nip52::lookup(&target, &egress, &EventConfig::default())
        .await
        .unwrap()
        .is_some());
    assert_eq!(t.seen.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn api_discovery_keys_include_body_and_credentials_never_redirect() {
    let (egress, t) = mock_egress(vec![
        (200, json!({"id":1})),
        (200, json!({"id":2})),
        (302, Value::Null),
    ]);
    let mut options = FetchOptions {
        json_body: Some(json!({"id":1})),
        ..Default::default()
    };
    let a = egress
        .discover("https://example.org/api", &options, 3600)
        .await
        .unwrap();
    assert_eq!(
        egress
            .discover("https://example.org/api#fragment", &options, 3600)
            .await
            .unwrap()
            .body,
        a.body
    );
    options.json_body = Some(json!({"id":2}));
    assert_ne!(
        egress
            .discover("https://example.org/api", &options, 3600)
            .await
            .unwrap()
            .body,
        a.body
    );
    let error = egress
        .fetch_with(
            "https://example.org/private?key=fixture-secret",
            &FetchOptions {
                sensitive: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains("fixture-secret"));
    assert_eq!(t.seen.lock().unwrap().len(), 3);
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn nostr_calendar_queues_members_and_reads_accepted_children(pool: PgPool) {
    use pinakes_core::event::adapters::nostr::{self as nip52, Target};
    let at = (Utc::now() + Duration::days(1)).timestamp().to_string();
    let child = signed(
        31923,
        100,
        &[&["d", "child"], &["title", "Member"], &["start", &at]],
    );
    let reference = format!("31923:{}:child", child.pubkey.to_hex());
    let parent = signed(
        31924,
        101,
        &[
            &["d", "calendar"],
            &["title", "Calendar"],
            &["a", &reference],
        ],
    );
    let parent_uri = Target::coordinate(&format!("31924:{}:calendar", parent.pubkey.to_hex()))
        .unwrap()
        .uri()
        .unwrap();
    store::ensure_resource(&pool, &parent_uri).await.unwrap();
    let (egress, t) = mock_egress(vec![(200, json!([parent]))]);
    let error = nip52::resolve(&parent_uri, &egress, &EventConfig::default(), &pool, false)
        .await
        .unwrap_err();
    assert!(error.is::<pinakes_core::resolver::RetryLater>());
    let child_uri = Target::coordinate(&reference).unwrap().uri().unwrap();
    let id = pinakes_core::model::identity(&child_uri).unwrap().0;
    let mut doc = nip52::parse(&child_uri, &child).unwrap();
    doc.description = Some("x".repeat(2048));
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let limited = EventConfig {
        max_event_document_bytes: 1024,
        ..Default::default()
    };
    assert!(nip52::resolve(&parent_uri, &egress, &limited, &pool, false)
        .await
        .unwrap_err()
        .to_string()
        .contains("document memory cap"));
    let doc = nip52::resolve(&parent_uri, &egress, &EventConfig::default(), &pool, false)
        .await
        .unwrap();
    assert_eq!(doc.container.unwrap().occurrences.len(), 1);
    assert_eq!(
        t.seen.lock().unwrap().len(),
        1,
        "child network work stays in its own queue"
    );
}

async fn install_cursor(pool: &PgPool, uri: &str, value: &str) {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    sqlx::query("INSERT INTO event_fetch(resource_id,adapter_cursor) VALUES($1,$2) ON CONFLICT(resource_id) DO UPDATE SET adapter_cursor=$2")
        .bind(id).bind(value).execute(pool).await.unwrap();
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cursor_http_resync_pagination_and_tombstones(pool: PgPool) {
    let mut cfg = EventConfig {
        ..Default::default()
    };
    let uri = "https://portal.einundzwanzig.space/api/changes";
    let definition = adapters::definition("einundzwanzig").unwrap();
    let at = (Utc::now() + Duration::days(2)).to_rfc3339();
    let event = json!({"id":7,"title":"Meetup","start":at,"link":"https://luma.com/fixture"});
    let (egress, t) = mock_egress(vec![
        (
            200,
            json!({"next_since":12,"has_more":false,"cursor_expired":false,"changes":[]}),
        ),
        (200, json!([event])),
    ]);
    let doc = cursor::resolve(&pool, uri, definition, &egress, &cfg, false)
        .await
        .unwrap();
    let c = doc.container.unwrap();
    assert!(!c.delta);
    assert_eq!(c.occurrences.len(), 1);
    assert_eq!(c.cursor.unwrap().next, "12");
    assert!(t.seen.lock().unwrap().iter().all(|(_, o)| o.feed_hint));
    install_cursor(&pool, uri, "12").await;
    let (egress, _) = mock_egress(vec![
        (
            200,
            json!({"next_since":30,"has_more":false,"cursor_expired":true,"changes":[]}),
        ),
        (200, json!([event])),
    ]);
    let c = cursor::resolve(&pool, uri, definition, &egress, &cfg, false)
        .await
        .unwrap()
        .container
        .unwrap();
    assert!(c.cursor.unwrap().reset);
    assert!(!c.delta);
    let (egress, t) = mock_egress(vec![
        (
            200,
            json!({"next_since":13,"has_more":true,"changes":[{"resource":"meetup-event","id":7,"action":"deleted"}]}),
        ),
        (
            200,
            json!({"next_since":14,"has_more":false,"changes":[{"resource":"meetup-event","action":"created","data":event}]}),
        ),
    ]);
    let c = cursor::resolve(&pool, uri, definition, &egress, &cfg, false)
        .await
        .unwrap()
        .container
        .unwrap();
    assert!(c.delta);
    assert!(c.deleted_uids.is_empty(), "recreation wins across pages");
    assert_eq!(c.occurrences.len(), 1);
    assert!(t.seen.lock().unwrap()[1].0.contains("since_sequence=13"));
    assert_eq!(
        saved_cursor(&pool, &pinakes_core::model::identity(uri).unwrap().0)
            .await
            .as_deref(),
        Some("12"),
        "fetch alone never checkpoints"
    );
    let (egress, _) = mock_egress(vec![(
        200,
        json!({"next_since":12,"has_more":true,"changes":[]}),
    )]);
    assert!(
        cursor::resolve(&pool, uri, definition, &egress, &cfg, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("non-advancing")
    );
    cfg.adapters.max_cursor_pages = 1;
    let (egress, _) = mock_egress(vec![(
        200,
        json!({"next_since":13,"has_more":true,"changes":[]}),
    )]);
    let partial = cursor::resolve(&pool, uri, definition, &egress, &cfg, false)
        .await
        .unwrap();
    assert_eq!(partial.container.unwrap().cursor.unwrap().next, "13");
    cfg.adapters.max_cursor_pages = 10;
    let btc_uri = "https://api.btcmap.org/v4/events";
    let (egress, t) = mock_egress(vec![(
        200,
        json!([
            {"id":1,"status":"live","starts_at":at,"name":"One","updated_at":"2026-10-15T13:00:00Z"},
            {"id":2,"status":"live","starts_at":at,"name":"Two","updated_at":"2026-10-15T13:00:00Z"},
            {"id":3,"status":"rejected","updated_at":"2026-10-15T13:00:00Z"},
            {"id":4,"status":"live","deleted_at":"2026-10-15T13:00:00Z","updated_at":"2026-10-15T13:00:00Z"}
        ]),
    )]);
    let c = cursor::resolve(
        &pool,
        btc_uri,
        adapters::definition("btcmap").unwrap(),
        &egress,
        &cfg,
        false,
    )
    .await
    .unwrap()
    .container
    .unwrap();
    assert!(c.delta);
    assert_eq!(
        c.occurrences.len(),
        2,
        "tied timestamps are consumed together"
    );
    assert_eq!(c.deleted_uids, ["3", "4"]);
    assert_eq!(c.cursor.unwrap().next, "2026-10-15T13:00:00+00:00");
    let seen = t.seen.lock().unwrap();
    assert!(seen[0].0.contains("include_deleted=true&status=all"));
    assert!(!seen[0].0.contains("limit="));
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn google_expiration_resets_then_commits_only_final_token(pool: PgPool) {
    let uri = "https://www.googleapis.com/calendar/v3/calendars/public/events";
    let mut cfg = EventConfig {
        ..Default::default()
    };
    cfg.adapters.platforms.insert(
        "google_calendar_api".into(),
        adapters::Options {
            key: Some("private-key".into()),
            ..Default::default()
        },
    );
    install_cursor(&pool, uri, "expired").await;
    let event = json!({"id":"e1","summary":"Google","start":{"dateTime":(Utc::now()+Duration::days(1)).to_rfc3339()}});
    let (egress, t) = mock_egress(vec![
        (410, Value::Null),
        (200, json!({"items":[event],"nextPageToken":"page-two"})),
        (
            200,
            json!({"items":[{"id":"old","status":"cancelled"}],"nextSyncToken":"current"}),
        ),
    ]);
    let doc = cursor::resolve(
        &pool,
        uri,
        adapters::definition("google_calendar_api").unwrap(),
        &egress,
        &cfg,
        false,
    )
    .await
    .unwrap();
    let c = doc.container.as_ref().unwrap();
    assert!(!c.delta);
    assert_eq!(c.deleted_uids, ["old"]);
    let checkpoint = c.cursor.as_ref().unwrap();
    assert!(checkpoint.reset);
    assert_eq!(checkpoint.next, "current");
    {
        let seen = t.seen.lock().unwrap();
        assert!(seen[0].0.contains("syncToken=expired"));
        assert!(!seen[1].0.contains("syncToken"));
        assert!(!seen[0].0.contains("timeMin="));
        assert!(seen[1].0.contains("timeMin=") && seen[1].0.contains("timeMax="));
        assert!(seen[2].0.contains("timeMin=") && seen[2].0.contains("timeMax="));
        assert!(seen[2].0.contains("pageToken=page-two"));
        assert!(seen.iter().all(|(_, o)| o.sensitive));
    }
    let id = pinakes_core::model::identity(uri).unwrap().0;
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("expired"));
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("current"));
    let resets: i64 = sqlx::query_scalar(
        "SELECT cursor_resets_total FROM source_state WHERE source='google_calendar_api'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(resets, 1);
}

#[test]
fn own_calendar_links_and_deterministic_platform_endpoints() {
    use adapters::service::{endpoint, ics_link};
    let cfg = EventConfig::default();
    let base = "https://tickets.example/org/current/".parse().unwrap();
    assert_eq!(
        ics_link("pretix", &base, "<a href='/org/other/ical/'>Other</a>"),
        None
    );
    assert_eq!(
        ics_link(
            "pretix",
            &base,
            "<a href='/org/other/ical/'>Other</a><a href='/org/current/ical/'>Mine</a>"
        )
        .unwrap(),
        "https://tickets.example/org/current/ical/"
    );
    for (id, uri, expected) in [
        (
            "nextcloud",
            "https://cloud.example/apps/calendar/p/abc",
            "https://cloud.example/remote.php/dav/public-calendars/abc?export",
        ),
        (
            "petzi",
            "https://www.petzi.ch/en/events/123-name/",
            "https://www.petzi.ch/en/events/123-name.ics",
        ),
        (
            "gathio",
            "https://gath.io/abc",
            "https://gath.io/export/event/abc",
        ),
    ] {
        assert_eq!(
            endpoint(adapters::definition(id).unwrap(), uri, None, &cfg)
                .unwrap()
                .unwrap()
                .0,
            expected
        );
    }
    let f = Fetched {
        permanent_redirect: false,
        truncated: false,
        url: base,
        validators: Default::default(),
        content_type: "text/html".into(),
        status: 200,
        retry_after_secs: 60,
        location: None,
        body: vec![],
    };
    for (id, uri, expected) in [
        (
            "partiful",
            "https://partiful.com/e/abc",
            "https://storage.googleapis.com/getpartiful.appspot.com/public/events/abc/event.ics",
        ),
        (
            "meetup",
            "https://www.meetup.com/group/events/123/",
            "https://www.meetup.com/group/events/123/ical/123.ics",
        ),
        (
            "luma",
            "https://luma.com/event/evt-abc",
            "https://api.lu.ma/event/get?event_api_id=evt-abc",
        ),
        (
            "guidle",
            "https://www.guidle.com/de/veranstaltungen/title_A123",
            "https://www.guidle.com/iCalendar/?lang=de&id=A123",
        ),
    ] {
        assert_eq!(
            endpoint(adapters::definition(id).unwrap(), uri, Some(&f), &cfg)
                .unwrap()
                .unwrap()
                .0,
            expected
        );
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn portal_coordinates_are_page_facts_and_btc_website_has_lineage(pool: PgPool) {
    use pinakes_core::{
        event::service::EventService,
        resolver::{osm::OsmResolver, web::WebResolver, Resolver},
    };
    let at = (Utc::now() + Duration::days(1)).to_rfc3339();
    let row = json!({"id":7,"title":"Portal","start":at,"osm_type":"node","osm_id":42,"osm_name":"Explicit Hall","osm_lat":47.3,"osm_lon":8.5,"osm_address":{"city":"Zurich"}});
    let (egress, _) = mock_egress(vec![(200, json!([row]))]);
    let platform = adapters::service::PlatformResolver {
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress,
            config: Default::default(),
            events: Some(Arc::new(EventService {
                pool: pool.clone(),
                config: EventConfig {
                    geocoding: false,
                    ..Default::default()
                },
                osm: Arc::new(OsmResolver::new(pool.clone(), Default::default()).unwrap()),
            })),
        },
    };
    let doc = platform
        .resolve("https://portal.einundzwanzig.space/de/meetup/test/event/7")
        .await
        .unwrap();
    assert!(doc.container.is_none());
    assert!(matches!(
        doc.location_source,
        Some(pinakes_core::model::LocationSource::Page)
    ));
    let venue = doc.event.venue.as_ref().unwrap();
    assert!(venue.osm_id.is_none() && venue.osm_version.is_none() && venue.resource_id.is_none());
    assert_eq!(doc.location.as_ref().unwrap().lat, 47.3);
    assert_eq!(doc.event.tz.as_deref(), Some("Europe/Zurich"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM osm_place")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    store::ensure_resource(&pool, "https://api.btcmap.org/v4/events/1")
        .await
        .unwrap();
    let (egress, _) = mock_egress(vec![(
        200,
        json!({"id":1,"status":"live","starts_at":at,"name":"BTC","website":"https://luma.com/chained"}),
    )]);
    let platform = adapters::service::PlatformResolver {
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress,
            ..platform.web
        },
    };
    let doc = platform
        .resolve("https://api.btcmap.org/v4/events/1")
        .await
        .unwrap();
    assert!(doc.aliases.iter().any(|s| s == "https://luma.com/chained"));
    let resolver: String =
        sqlx::query_scalar("SELECT resolver FROM resource WHERE uri='https://luma.com/chained'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(resolver, "event_platform");
}

#[test]
fn all_page_and_calendar_adapters_parse_their_public_contracts() {
    use pinakes_core::event::calendar;
    let uri = "https://events.example/current";
    let schema=br#"<script type="application/ld+json">{"@type":"MusicEvent","name":"Contract fixture","startDate":"2026-10-20T19:00:00+0200","endDate":"2026-10-20T21:00:00+0200","location":{"@type":"Place","name":"Hall","geo":{"latitude":47,"longitude":8}},"eventStatus":"EventCancelled"}</script>"#;
    let cfg = EventConfig {
        ..Default::default()
    };
    for d in adapters::definitions()
        .iter()
        .filter(|d| d.parser == "page")
    {
        let doc = state::parse(&d.id, uri, schema).unwrap_or_else(|| panic!("{}", d.id));
        assert_eq!(doc.name.as_deref(), Some("Contract fixture"));
        assert_eq!(
            doc.event.event_status,
            Some(pinakes_core::event::EventStatus::Cancelled)
        );
        assert!(doc.location.is_some());
    }
    for d in adapters::definitions()
        .iter()
        .filter(|d| d.parser == "ics_link" || d.parser == "calendar")
    {
        let bytes=b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:contract\r\nDTSTART;TZID=Europe/Zurich:20261020T190000\r\nDTEND;TZID=Europe/Zurich:20261020T210000\r\nSUMMARY:Contract fixture\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let f = Fetched {
            permanent_redirect: false,
            truncated: false,
            url: uri.parse().unwrap(),
            validators: Default::default(),
            content_type: "text/calendar".into(),
            status: 200,
            retry_after_secs: 60,
            location: None,
            body: bytes.to_vec(),
        };
        let doc = adapters::service::calendar_document(uri, d, &f, &cfg)
            .unwrap()
            .unwrap();
        let event = doc
            .container
            .as_ref()
            .map(|c| &c.occurrences[0].doc)
            .unwrap_or(&doc);
        assert_eq!(event.name.as_deref(), Some("Contract fixture"), "{}", d.id);
        assert_eq!(event.event.tz.as_deref(), Some("Europe/Zurich"));
        assert_eq!(
            event.event.event_status,
            Some(pinakes_core::event::EventStatus::Cancelled)
        );
    }
    let xml=br#"<feed xmlns="http://www.w3.org/2005/Atom" xmlns:mc="https://starticket.example/feeds"><title>Atom sample</title><entry><id>42</id><title>Concert</title><link href="https://starticket.example/42"/><mc:dates fromdate="2026-10-20T17:00:00Z" todate="2026-10-20T19:00:00Z"/><mc:locations><mc:location name="Hall"/></mc:locations></entry></feed>"#;
    let doc = calendar::structured_feed(uri, xml, &cfg, "2026-10-16T00:00:00Z".parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(doc.name.as_deref(), Some("Atom sample"));
    let c = doc.container.unwrap();
    assert_eq!(c.occurrences.len(), 1);
    assert_eq!(
        c.occurrences[0].doc.event.venue.as_ref().unwrap().text,
        "Hall"
    );
    assert_eq!(
        c.occurrences[0].doc.end.unwrap() - c.occurrences[0].doc.start.unwrap(),
        Duration::hours(2)
    );
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn platform_ua_config_class_c_and_secret_projection(pool: PgPool) {
    use pinakes_core::{
        config::{Config, QueueConfig},
        event::service::EventService,
        queue,
        resolver::{osm::OsmResolver, web::WebResolver, Resolver},
    };
    let mut cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let schema = json!({"@type":"Event","name":"Fixture","startDate":"2026-10-20T17:00:00Z"});
    // Each zoneless event page is followed by one ActivityPub negotiation.
    let (egress, t) = mock_egress(vec![(200, schema.clone()); 8]);
    let mut platform = adapters::service::PlatformResolver {
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress,
            config: Default::default(),
            events: Some(Arc::new(EventService {
                pool: pool.clone(),
                config: cfg.clone(),
                osm: Arc::new(OsmResolver::new(pool.clone(), Default::default()).unwrap()),
            })),
        },
    };
    for uri in [
        "https://dice.fm/event/fixture",
        "https://feverup.com/m/fixture",
        "https://www.songkick.com/concerts/fixture",
        "https://posh.vip/e/fixture",
    ] {
        platform.resolve(uri).await.unwrap();
    }
    let profiles: Vec<_> = t
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, o)| o.accept.is_none())
        .map(|(_, o)| o.user_agent.clone().unwrap())
        .collect();
    assert!(profiles[0].contains("Chrome/"));
    assert!(profiles[1].contains("Chrome/"));
    assert!(profiles[2].contains("Firefox/"));
    assert_eq!(profiles[3], "curl/8.10.1");
    cfg.adapters.platforms.insert(
        "ticketmaster".into(),
        adapters::Options {
            key: Some("test-secret".into()),
            ..Default::default()
        },
    );
    let (egress, t) = mock_egress(vec![
        (
            200,
            json!({"name":"Keyed","dates":{"start":{"dateTime":"2026-10-20T17:00:00Z"}}}),
        ),
        (404, Value::Null),
    ]);
    platform.web.egress = egress;
    Arc::get_mut(platform.web.events.as_mut().unwrap())
        .unwrap()
        .config = cfg;
    let doc = platform
        .resolve("https://www.ticketmaster.com/event/fixture")
        .await
        .unwrap();
    assert!(t.seen.lock().unwrap()[0].0.contains("apikey=test-secret"));
    assert!(!serde_json::to_string(&doc).unwrap().contains("test-secret"));
    let (egress, _) = mock_egress(vec![(503, Value::Null)]);
    platform.web.egress = egress;
    let uri = "https://www.facebook.com/events/fixture";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let q = QueueConfig::default();
    let job = queue::claim(&pool, "event_platform", &q)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.id, id);
    queue::run_job(&pool, &platform, &q, &job).await.unwrap();
    let seconds: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(seconds >= 7. * 86400. - 2., "class C retry: {seconds}");
    let mut config = Config::default();
    config.event.adapters.enabled = false;
    assert_eq!(
        pinakes_core::resolver::classify_config("https://dice.fm/event/fixture", &config),
        Some("web")
    );
    config.event.adapters.enabled = true;
    config.event.adapters.platforms.insert(
        "dice".into(),
        adapters::Options {
            enabled: Some(false),
            ..Default::default()
        },
    );
    assert_eq!(
        pinakes_core::resolver::classify_config("https://dice.fm/event/fixture", &config),
        Some("web")
    );
}

struct RecordedResponses(std::collections::HashMap<String, Result<Fetched, String>>);
#[async_trait::async_trait]
impl Transport for RecordedResponses {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &Default::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        options: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        let key = format!(
            "{}:{}",
            url,
            options
                .json_body
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default()
        );
        self.0
            .get(&key)
            .cloned()
            .unwrap_or_else(|| Err(format!("uncaptured fixture request: {url}")))
            .map_err(anyhow::Error::msg)
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn recorded_public_adapter_responses_replay_without_network(pool: PgPool) {
    use pinakes_core::resolver::{osm::OsmResolver, web::WebResolver, Resolver};
    use sha2::Digest;
    use std::io::Read;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/adapters");
    let records: Vec<Value> =
        serde_json::from_slice(&std::fs::read(root.join("responses.json")).unwrap()).unwrap();
    let mut responses = std::collections::HashMap::new();
    for r in records {
        let uri = r["uri"].as_str().unwrap();
        let result = if let Some(error) = r["error"].as_str() {
            Err(error.to_owned())
        } else {
            let mut body = vec![];
            flate2::read::GzDecoder::new(
                std::fs::File::open(root.join(r["path"].as_str().unwrap())).unwrap(),
            )
            .read_to_end(&mut body)
            .unwrap();
            assert_eq!(
                format!("{:x}", sha2::Sha256::digest(&body)),
                r["sha256"].as_str().unwrap()
            );
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                url: uri.parse().unwrap(),
                status: r["status"].as_u64().unwrap_or(200) as u16,
                content_type: r["content_type"].as_str().unwrap_or("text/html").into(),
                body,
                validators: serde_json::from_value(r["validators"].clone()).unwrap_or_default(),
                location: r["location"].as_str().map(str::to_owned),
                retry_after_secs: 60,
            })
        };
        let key = format!(
            "{}:{}",
            uri,
            r["json_body"]
                .as_object()
                .map(|_| r["json_body"].to_string())
                .unwrap_or_default()
        );
        responses.insert(key, result);
    }
    let mut cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    cfg.adapters.websocket = false;
    let platform = adapters::service::PlatformResolver {
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress: Egress::with_backends(
                pinakes_core::config::EgressConfig {
                    host_requests_per_second: 1000.0,
                    ..Default::default()
                },
                Arc::new(AdapterDns),
                Arc::new(RecordedResponses(responses)),
            ),
            config: Default::default(),
            events: Some(Arc::new(pinakes_core::event::service::EventService {
                pool: pool.clone(),
                config: cfg.clone(),
                osm: Arc::new(OsmResolver::new(pool.clone(), Default::default()).unwrap()),
            })),
        },
    };
    let cases: Vec<Value> =
        serde_json::from_slice(&std::fs::read(root.join("cases.json")).unwrap()).unwrap();
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let manifest = adapters::manifest(adapters::definition(id).unwrap(), &cfg);
        let result = platform
            .probe_manifest(case["uri"].as_str().unwrap(), &manifest)
            .await;
        if case["error"] == true {
            assert!(result.is_err(), "{id}");
            continue;
        }
        let doc = result.unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(
            doc.status.as_str(),
            case["status"].as_str().unwrap(),
            "{id}"
        );
        assert_eq!(doc.type_name, case["type"].as_str().unwrap(), "{id}");
        assert_eq!(
            json!(manifest.missing_fields(&doc)),
            case["missing_fields"],
            "{id}"
        );
        assert_eq!(json!(doc.event.tz), case["tz"], "{id}");
    }
}

fn platform(
    pool: &PgPool,
    config: EventConfig,
    egress: Egress,
) -> adapters::service::PlatformResolver {
    adapters::service::PlatformResolver {
        web: pinakes_core::resolver::web::WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress,
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
    }
}

#[test]
fn registry_credentials_and_cadence() {
    use pinakes_core::{adapter::Mode, resolver::classify};
    adapters::validate(&EventConfig::default()).unwrap();
    for uri in [
        "https://en.wikipedia.org/wiki/Bitcoin",
        "https://facebook.com/person",
        "https://zoom.us/about",
        "https://api.btcmap.org/v4/users",
        "https://portal.einundzwanzig.space/api/anything",
    ] {
        assert_eq!(classify(uri), Some("web"), "{uri}");
    }
    assert_eq!(classify("https://api.nostr.wine/search?kind=0"), None);
    for (id, uri, path) in [
        ("billetto_api", "https://api.billetto.com/v3/public/events/42?admin=true", "/api/v3/public/events/42"),
        ("eventfrog_api", "https://api.eventfrog.net/public/v1/events?id=42&admin=true", "/public/v1/events"),
        ("google_calendar_api", "https://www.googleapis.com/calendar/v3/calendars/abc%40example.org/events?syncToken=attacker", "/calendar/v3/calendars/abc@example.org/events"),
    ] {
        let url = adapters::service::keyed_url(id, uri).unwrap();
        assert_eq!(url.path(), path);
        assert!(!url.as_str().contains("admin") && !url.as_str().contains("attacker"));
    }
    for (id, uri) in [
        ("billetto_api", "https://api.billetto.com/v3/admin/events"),
        (
            "eventfrog_api",
            "https://api.eventfrog.net/public/v1/orders?id=42",
        ),
        (
            "eventfrog_api",
            "https://api.eventfrog.net/public/v1/events?id=42&id=43",
        ),
        (
            "google_calendar_api",
            "https://www.googleapis.com/calendar/v3/calendars/a/acl",
        ),
        (
            "google_calendar_api",
            "https://www.googleapis.com/calendar/v3/calendars/a%2fb/events",
        ),
    ] {
        assert!(adapters::service::keyed_url(id, uri).is_err());
        assert!(adapters::detect(uri).is_none());
    }
    let options = adapters::Options {
        key: Some("never-print-this".into()),
        ..Default::default()
    };
    assert!(!format!("{options:?}").contains("never-print-this"));
    assert!(!serde_json::to_string(&options)
        .unwrap()
        .contains("never-print-this"));
    let now = Utc::now();
    for (id, seconds) in [
        ("einundzwanzig", 300),
        ("btcmap", 900),
        ("google_calendar_api", 900),
    ] {
        let m = adapters::manifest(adapters::definition(id).unwrap(), &Default::default());
        assert_eq!(m.mode, Mode::Cursor);
        assert_eq!(
            pinakes_core::scheduler::container_next(
                &m,
                [(Some(now + Duration::hours(1)), None)],
                None,
                now,
                &Default::default()
            )
            .unwrap()
                - now,
            Duration::seconds(seconds)
        );
        assert_eq!(m.expected_fields, ["cursor"]);
    }
    assert_eq!(
        adapters::manifest(adapters::definition("nip52").unwrap(), &Default::default()).mode,
        Mode::Resource
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn shared_roots_and_derived_author_budgets(pool: PgPool) {
    use pinakes_core::queue::{AuthorBudget, Job};
    let root_uri = "https://api.btcmap.org/v4/events";
    // Twenty URI variants of one cursor feed are aliases of one root, fetched once.
    for n in 0..20 {
        store::ensure_resource(&pool, &format!("{root_uri}?x={n}"))
            .await
            .unwrap();
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource WHERE resolver IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(DISTINCT root_id) FROM resource_alias")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let parent = store::ensure_resource(&pool, "https://example.org/parent")
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,object_path,kind,created_at) VALUES($1,'sponsor','/pub/test/tag','tag',now())").bind(&parent).execute(&pool).await.unwrap();
    let a = store::derive_resource(
        &pool,
        "https://example.org/parent",
        "https://example.org/a",
        "btcmap",
        2,
    )
    .await
    .unwrap()
    .unwrap();
    let b = store::derive_resource(
        &pool,
        "https://example.org/parent",
        "https://example.org/b",
        "btcmap",
        2,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(store::derive_resource(
        &pool,
        "https://example.org/parent",
        "https://example.org/c",
        "btcmap",
        2
    )
    .await
    .unwrap()
    .is_none());
    assert!(store::derive_resource(
        &pool,
        "https://example.org/parent",
        "https://example.org/a",
        "btcmap",
        2
    )
    .await
    .unwrap()
    .is_some());
    let budgets = AuthorBudget::default();
    let job = |id| Job {
        id,
        uri: String::new(),
        attempts: 0,
        generation: 0,
    };
    assert!(budgets.admit(&pool, &job(a), 0.01).await.unwrap());
    assert!(!budgets.admit(&pool, &job(b), 0.01).await.unwrap());
    // A derived variant must still sponsor the canonical root's actual fetch.
    let alias_uri = "https://api.btcmap.org/v4/events?derived=1";
    store::derive_resource(&pool, "https://example.org/parent", alias_uri, "btcmap", 3)
        .await
        .unwrap()
        .unwrap();
    let root = pinakes_core::model::identity(root_uri).unwrap().0;
    assert!(!budgets.admit(&pool, &job(root), 0.01).await.unwrap());

    let orphan = "https://example.org/no-author";
    store::ensure_resource(&pool, orphan).await.unwrap();
    let c = store::derive_resource(&pool, orphan, "https://example.org/c", "nip52", 2)
        .await
        .unwrap()
        .unwrap();
    let d = store::derive_resource(&pool, orphan, "https://example.org/d", "nip52", 2)
        .await
        .unwrap()
        .unwrap();
    assert!(budgets.admit(&pool, &job(c), 0.01).await.unwrap());
    assert!(!budgets.admit(&pool, &job(d), 0.01).await.unwrap());
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn portal_cap_commits_progress_and_expires_on_later_page(pool: PgPool) {
    let uri = "https://portal.einundzwanzig.space/api/changes";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut cfg = EventConfig::default();
    cfg.adapters.max_cursor_pages = 1;
    let def = adapters::definition("einundzwanzig").unwrap();
    let at = (Utc::now() + Duration::days(1)).to_rfc3339();
    let row = json!({"id":1,"title":"One","start":at});
    let (egress, _) = mock_egress(vec![(
        200,
        json!({"changes":[{"resource":"meetup-event","action":"created","data":row}],"has_more":true,"next_since":10}),
    )]);
    let doc = cursor::resolve(&pool, uri, def, &egress, &cfg, false)
        .await
        .unwrap();
    assert!(
        doc.container
            .as_ref()
            .unwrap()
            .cursor
            .as_ref()
            .unwrap()
            .resync
    );
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("10"));
    let (egress, t) = mock_egress(vec![
        (200, json!({"changes":[],"has_more":false,"next_since":11})),
        (200, json!([row])),
    ]);
    let mut doc = cursor::resolve(&pool, uri, def, &egress, &cfg, false)
        .await
        .unwrap();
    doc.fetched_at += 1;
    assert!(!doc.container.as_ref().unwrap().delta);
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    assert!(t.seen.lock().unwrap()[0].0.contains("since_sequence=10"));
    assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("11"));
    assert!(!sqlx::query_scalar::<_, bool>(
        "SELECT adapter_resync FROM event_fetch WHERE resource_id=$1"
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap());
    cfg.adapters.max_cursor_pages = 2;
    let (egress, _) = mock_egress(vec![
        (200, json!({"changes":[],"has_more":true,"next_since":12})),
        (
            200,
            json!({"changes":[],"has_more":false,"next_since":20,"cursor_expired":true}),
        ),
        (200, json!([row])),
    ]);
    let doc = cursor::resolve(&pool, uri, def, &egress, &cfg, false)
        .await
        .unwrap();
    assert!(
        doc.container
            .as_ref()
            .unwrap()
            .cursor
            .as_ref()
            .unwrap()
            .reset
    );
    assert_eq!(doc.container.unwrap().cursor.unwrap().next, "20");
    for invalid in [
        json!("bad"),
        json!(-1),
        json!("9".repeat(4097)),
        json!("9999999999999999"),
    ] {
        let (egress, _) = mock_egress(vec![(
            200,
            json!({"changes":[],"has_more":true,"next_since":invalid}),
        )]);
        assert!(cursor::resolve(&pool, uri, def, &egress, &cfg, false)
            .await
            .is_err());
        assert_eq!(saved_cursor(&pool, &id).await.as_deref(), Some("11"));
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn delta_reschedule_unknown_status_hold_and_labels(pool: PgPool) {
    let uri = "https://api.btcmap.org/v4/events";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let now = Utc::now().timestamp_millis();
    let mut original = snapshot(uri, 3, now);
    original.source = "btcmap".into();
    checkpoint(&mut original, &id, None, "2026-10-15T00:00:00Z", false);
    store::save_document(&pool, &id, 0, &original)
        .await
        .unwrap();
    let cfg = EventConfig::default();
    let far = (Utc::now() + Duration::days(1000)).to_rfc3339();
    let (egress, _) = mock_egress(vec![(
        200,
        json!([
            {"id":0,"status":"live","name":"Moved","starts_at":far,"updated_at":"2035-01-01T00:00:00Z"},
            {"id":1,"status":"new-unknown","updated_at":"2026-10-15T01:00:00Z"},
            {"id":99,"status":"live","name":"New outside","starts_at":far,"updated_at":"2026-10-15T01:00:00Z"}
        ]),
    )]);
    let mut doc = cursor::resolve(
        &pool,
        uri,
        adapters::definition("btcmap").unwrap(),
        &egress,
        &cfg,
        false,
    )
    .await
    .unwrap();
    doc.source = "btcmap".into();
    doc.fetched_at = now + 1;
    let next: chrono::DateTime<Utc> = doc
        .container
        .as_ref()
        .unwrap()
        .cursor
        .as_ref()
        .unwrap()
        .next
        .parse()
        .unwrap();
    assert!(next <= Utc::now() + Duration::minutes(5));
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT name FROM occurrence WHERE resource_id=$1 AND uid='0'"
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "Moved"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM occurrence WHERE resource_id=$1 AND uid='1'"
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "scheduled"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM occurrence WHERE uid='99'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let next = saved_cursor(&pool, &id).await.unwrap();
    let mut deleted = snapshot(uri, 0, now + 2);
    deleted.source = "btcmap".into();
    deleted.container.as_mut().unwrap().delta = true;
    deleted.container.as_mut().unwrap().deleted_uids = vec!["1".into(), "2".into()];
    checkpoint(&mut deleted, &id, Some(&next), "next", false);
    store::save_document(&pool, &id, 0, &deleted).await.unwrap();
    assert_eq!(
        saved_cursor(&pool, &id).await.as_deref(),
        Some(next.as_str()),
        "held tombstones must not checkpoint"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT feed_held_total FROM source_state WHERE source='btcmap'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    deleted.fetched_at += 1;
    store::save_document(&pool, &id, 0, &deleted).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource_change WHERE source='btcmap' AND class='cancelled'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource_change WHERE source IN ('event','event_platform')"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn full_indexer_page_is_inconclusive_and_ws_is_forbidden() {
    use adapters::nostr::{self as nip52, Target};
    let event = signed(
        31923,
        100,
        &[
            &["d", "unrelated"],
            &["title", "Other"],
            &["start", "1792519200"],
        ],
    );
    let target = Target::coordinate(&format!(
        "31923:{}:not-in-first-page",
        event.pubkey.to_hex()
    ))
    .unwrap();
    let (egress, _) = mock_egress(vec![(200, json!(vec![event; 100])), (200, json!([]))]);
    let mut cfg = EventConfig::default();
    cfg.adapters.max_indexer_pages = 1;
    assert!(nip52::lookup(&target, &egress, &cfg)
        .await
        .unwrap_err()
        .is::<pinakes_core::resolver::RetryLater>());
    let error = egress
        .nostr_request("ws://example.org/", target.filter(), 100)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid relay"), "{error}");
}

fn recorded_egress(rows: Vec<(&str, &str, Value)>) -> Egress {
    let map = rows
        .into_iter()
        .map(|(uri, body, request)| {
            (
                format!(
                    "{}:{}",
                    uri,
                    if request.is_null() {
                        String::new()
                    } else {
                        request.to_string()
                    }
                ),
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: uri.parse().unwrap(),
                    body: body.as_bytes().to_vec(),
                    content_type: if body.starts_with('<') {
                        "text/html".into()
                    } else {
                        "application/json".into()
                    },
                    status: 200,
                    retry_after_secs: 60,
                    location: None,
                    validators: Default::default(),
                }),
            )
        })
        .collect();
    Egress::with_backends(
        pinakes_core::config::EgressConfig {
            host_requests_per_second: 1000.,
            ..Default::default()
        },
        Arc::new(AdapterDns),
        Arc::new(RecordedResponses(map)),
    )
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn wikipedia_primary_item_and_plain_web_fallback(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let uri = "https://en.wikipedia.org/wiki/Bitcoin_Conference";
    let html = include_str!("fixtures/adapters/wikipedia-item.html");
    assert_eq!(
        adapters::service::wikipedia_item(html).as_deref(),
        Some("Q12345")
    );
    assert_eq!(
        adapters::service::wikipedia_item(
            "<a href='https://www.wikidata.org/wiki/Q999'>unrelated</a>"
        ),
        None
    );
    assert_eq!(
        adapters::service::wikipedia_item(
            "<a href='https://www.wikidata.org/wiki/Special:EntityPage/Q123'>item</a>"
        )
        .as_deref(),
        Some("Q123")
    );
    let entity = "https://www.wikidata.org/wiki/Special:EntityData/Q12345.json";
    let cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let event=json!({"entities":{"Q12345":{"id":"Q12345","labels":{"en":{"value":"Conference"}},"claims":{"P580":[{"mainsnak":{"datavalue":{"value":{"time":"+2026-10-20T00:00:00Z","precision":11}}}}]}}}}).to_string();
    let resolver = platform(
        &pool,
        cfg.clone(),
        recorded_egress(vec![
            (uri, html, Value::Null),
            (entity, &event, Value::Null),
        ]),
    );
    assert_eq!(resolver.web.resolve(uri).await.unwrap().type_name, "Event");
    let person=json!({"entities":{"Q12345":{"id":"Q12345","labels":{"en":{"value":"Person"}},"claims":{"P31":[{"mainsnak":{"datavalue":{"value":{"id":"Q5"}}}}]}}}}).to_string();
    let resolver = platform(
        &pool,
        cfg.clone(),
        recorded_egress(vec![
            (uri, html, Value::Null),
            (entity, &person, Value::Null),
        ]),
    );
    assert_eq!(
        resolver.web.resolve(uri).await.unwrap().type_name,
        "Article"
    );
    let resolver = platform(
        &pool,
        cfg,
        recorded_egress(vec![(
            uri,
            "<title>Plain article</title><meta property='og:type' content='article'>",
            Value::Null,
        )]),
    );
    let doc = resolver.resolve(uri).await.unwrap(); // stale event_platform routing hint is harmless
    assert_eq!(doc.type_name, "Article");
    assert_eq!(doc.name.as_deref(), Some("Plain article"));
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn portal_missing_title_join_and_derivation_disabled(pool: PgPool) {
    use pinakes_core::{model::Status, resolver::Resolver};
    let cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let uri = "https://portal.einundzwanzig.space/de/meetup/one/event/7";
    let (egress, _) = mock_egress(vec![(200, json!([])), (200, json!([]))]);
    let doc = platform(&pool, cfg.clone(), egress)
        .resolve(uri)
        .await
        .unwrap();
    assert!(matches!(doc.status, Status::Missing));
    let (egress, t) = mock_egress(vec![
        (
            200,
            json!([{"id":7,"title":null,"meetup_id":3,"start":(Utc::now()+Duration::days(1)).to_rfc3339()}]),
        ),
        (200, json!([{"id":3,"name":"Local Meetup"}])),
    ]);
    let doc = platform(&pool, cfg.clone(), egress)
        .resolve(uri)
        .await
        .unwrap();
    assert_eq!(doc.name.as_deref(), Some("Local Meetup"));
    assert!(t.seen.lock().unwrap()[1].0.ends_with("/api/meetups"));
    let uri = "https://api.btcmap.org/v4/events/1";
    store::ensure_resource(&pool, uri).await.unwrap();
    let mut cfg = cfg;
    cfg.adapters.derive_resources = false;
    let (egress, _) = mock_egress(vec![(
        200,
        json!({"id":1,"status":"live","name":"Event","starts_at":(Utc::now()+Duration::days(1)).to_rfc3339(),"website":"https://example.org/no-derivation"}),
    )]);
    platform(&pool, cfg, egress).resolve(uri).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_derivation")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource WHERE uri='https://example.org/no-derivation'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn public_canary_redaction_unknown_size_and_disabled_nostr(pool: PgPool) {
    use pinakes_core::{
        api::{self, AppState},
        config::Config,
        queue,
        resolver::{web::WebResolver, Resolver},
    };
    use tower::ServiceExt;
    // Public info shows a canary's host only; the admin listener keeps the full URL.
    let cfg = Config::default();
    let state = AppState::new(pool.clone(), cfg.clone()).unwrap();
    for (app, private) in [
        (api::router(state.clone()), false),
        (api::admin_router(state), true),
    ] {
        let r = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/info")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let bytes = axum::body::to_bytes(r.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&bytes).contains("updated_since"),
            private
        );
    }
    let m = adapters::manifest(adapters::definition("btcmap").unwrap(), &cfg.event);
    let url = m.canary.as_ref().unwrap();
    let since: chrono::DateTime<Utc> = url
        .query_pairs()
        .find(|(k, _)| k == "updated_since")
        .unwrap()
        .1
        .parse()
        .unwrap();
    assert!((Utc::now() - since - Duration::days(1)).num_seconds().abs() <= 1);
    sqlx::query("INSERT INTO source_state(source,canary_generation,canary_ok,canary_failures) VALUES('btcmap',1,false,12)").execute(&pool).await.unwrap();
    pinakes_core::canary::record(
        &pool,
        &m,
        1,
        &Err(pinakes_core::resolver::egress::BodyTooLarge { cap: 1024 * 1024 }.into()),
        3600,
    )
    .await
    .unwrap();
    let state: (Option<bool>, i32) =
        sqlx::query_as("SELECT canary_ok,canary_failures FROM source_state WHERE source='btcmap'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, (None, 0));
    let uri = "nostr:naddr1invalid-fixture";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let (egress, t) = mock_egress(vec![]);
    let web = WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        egress,
        events: None,
        config: Default::default(),
    };
    assert!(web.queue_sources().contains(&"event_platform"));
    let q = Default::default();
    let job = queue::claim(&pool, "event_platform", &q)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.id, id);
    queue::run_job(&pool, &web, &q, &job).await.unwrap();
    let state: (i32, bool) =
        sqlx::query_as("SELECT attempts,dead_letter FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, (0, false));
    assert!(t.seen.lock().unwrap().is_empty());
}

#[test]
fn nested_flight_and_nuxt_share_one_deadline() {
    let nested = json!(["{\"noise\":[1,2,3]}"]);
    let strings = vec![nested; 10000];
    let html = format!(
        "<script>{}</script>",
        serde_json::to_string(&strings).unwrap()
    );
    for id in ["xceed", "ents24"] {
        let before = std::time::Instant::now();
        assert!(state::parse(id, "https://example.org/event", html.as_bytes()).is_none());
        assert!(
            before.elapsed() < std::time::Duration::from_millis(1200),
            "deadline exceeded for {id}"
        );
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn alias_references_scope_search_and_occurrence_reads(pool: PgPool) {
    use pinakes_core::api::{self, AppState};
    use tower::ServiceExt;
    let alias_uri = "https://api.btcmap.org/v4/events?variant=one";
    let alias = store::ensure_resource(&pool, alias_uri).await.unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,object_path,kind,label,created_at) VALUES($1,$2,'/pub/tag/1','tag','review',now())").bind(&alias).bind("y".repeat(52)).execute(&pool).await.unwrap();
    let root_uri = "https://api.btcmap.org/v4/events";
    let root = pinakes_core::model::identity(root_uri).unwrap().0;
    // The variant is an alias from the start: never routed, its root queued in its place.
    let (mapped, resolver): (String, Option<String>) = sqlx::query_as("SELECT a.root_id,r.resolver FROM resource_alias a JOIN resource r ON r.id=a.alias_id WHERE a.alias_id=$1").bind(&alias).fetch_one(&pool).await.unwrap();
    assert_eq!((mapped.as_str(), resolver), (root.as_str(), None));
    let doc = snapshot(root_uri, 3, Utc::now().timestamp_millis());
    store::save_document(&pool, &root, 0, &doc).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), Default::default()).unwrap());
    for path in [
        format!("/v1/resources/{alias}/occurrences?limit=2"),
        "/v1/search?type=Event&tags=review&facet=tags".into(),
    ] {
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(&path)
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let items = v["items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            if path.contains("occurrences") { 2 } else { 3 }
        );
        assert!(items.iter().all(|v| v["id"] == root));
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn future_stored_cursor_resyncs_without_skipping_data(pool: PgPool) {
    let uri = "https://api.btcmap.org/v4/events";
    install_cursor(&pool, uri, "2035-01-01T00:00:00Z").await;
    let (egress, t) = mock_egress(vec![(200, json!([]))]);
    let doc = cursor::resolve(
        &pool,
        uri,
        adapters::definition("btcmap").unwrap(),
        &egress,
        &Default::default(),
        false,
    )
    .await
    .unwrap();
    assert!(doc.container.unwrap().cursor.unwrap().reset);
    assert!(t.seen.lock().unwrap()[0].0.contains("updated_since=1970"));
    let uri = "https://portal.einundzwanzig.space/api/changes";
    install_cursor(&pool, uri, "9999999999999999").await;
    let (egress, t) = mock_egress(vec![
        (200, json!({"next_since":20,"has_more":false,"changes":[]})),
        (200, json!([])),
    ]);
    let doc = cursor::resolve(
        &pool,
        uri,
        adapters::definition("einundzwanzig").unwrap(),
        &egress,
        &Default::default(),
        false,
    )
    .await
    .unwrap();
    assert!(doc.container.unwrap().cursor.unwrap().reset);
    assert!(t.seen.lock().unwrap()[0].0.contains("since_sequence=0"));
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn luma_calendar_discovery_and_non_calendar_client_url(pool: PgPool) {
    use nostr::nips::nip19::ToBech32;
    use pinakes_core::resolver::Resolver;
    let event = signed(31923, 100, &[&["d", "fixture"]]);
    let npub = event.pubkey.to_bech32().unwrap();
    assert_eq!(
        pinakes_core::resolver::classify(&format!("https://njump.me/{npub}")),
        Some("web")
    );
    let uri = "https://luma.com/fixture-calendar";
    let start = (Utc::now() + Duration::days(1)).format("%Y%m%dT%H%M%SZ");
    let ics=format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:luma-calendar\r\nDTSTART:{start}\r\nSUMMARY:Calendar member\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
    let egress = recorded_egress(vec![
        (uri, "<title>My calendar</title>", Value::Null),
        (
            "https://api.lu.ma/url?url=fixture-calendar",
            r#"{"kind":"calendar","data":{"calendar":{"api_id":"cal-fixture"}}}"#,
            Value::Null,
        ),
        (
            "https://api.lu.ma/ics/get?entity=calendar&id=cal-fixture",
            &ics,
            Value::Null,
        ),
    ]);
    let cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let doc = platform(&pool, cfg, egress).resolve(uri).await.unwrap();
    assert_eq!(doc.type_name, "DataFeed");
    assert_eq!(doc.container.unwrap().occurrences.len(), 1);
}

struct HintedGraphql;
#[async_trait::async_trait]
impl Transport for HintedGraphql {
    async fn get(&self, u: &url::Url, _: &[SocketAddr]) -> anyhow::Result<Fetched> {
        let api = u.path() == "/api";
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: u.clone(),
            status: 200,
            content_type: if api { "application/json" } else { "text/html" }.into(),
            body: if api {
                br#"{"errors":[{"message":"not that GraphQL API"}]}"#.to_vec()
            } else {
                b"<title>A page about mobilizon</title>".to_vec()
            },
            validators: Default::default(),
            location: None,
            retry_after_secs: 60,
        })
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn unregistered_platform_hint_cannot_veto_page(pool: PgPool) {
    use pinakes_core::{queue, resolver::Resolver};
    let uri = "https://generic.example/events/86a665ae-e495-4e4f-88d4-4cffd44a82f1";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let r = platform(
        &pool,
        EventConfig {
            geocoding: false,
            ..Default::default()
        },
        Egress::with_backends(
            Default::default(),
            Arc::new(AdapterDns),
            Arc::new(HintedGraphql),
        ),
    );
    let doc = r.resolve(uri).await.unwrap();
    assert_eq!(doc.name.as_deref(), Some("A page about mobilizon"));
    assert!(doc
        .event
        .warnings
        .iter()
        .any(|w| w == "enrichment_unavailable"));
    queue::run_job(
        &pool,
        &r,
        &Default::default(),
        &queue::Job {
            id: id.clone(),
            uri: uri.into(),
            attempts: 0,
            generation: 0,
        },
    )
    .await
    .unwrap();
    let state: (String, i32, bool) =
        sqlx::query_as("SELECT status,attempts,dead_letter FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, ("ok".into(), 0, false));
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn registered_secondary_451_confirms_disappearance(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let uri = "https://luma.com/event-removed";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "luma", 3600);
    doc.set_type("Event");
    doc.start = Some(Utc::now() + Duration::days(1));
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let r = platform(
        &pool,
        EventConfig {
            geocoding: false,
            ..Default::default()
        },
        Egress::with_backends(
            Default::default(),
            Arc::new(AdapterDns),
            Arc::new(SecondaryFailure(451)),
        ),
    );
    for n in 1..=2 {
        let mut missing = r.resolve(uri).await.unwrap();
        assert_eq!(missing.status, pinakes_core::model::Status::Missing);
        missing.fetched_at += n * 1000;
        store::save_document(&pool, &id, 0, &missing).await.unwrap();
        let state: (String, i32, bool) =
            sqlx::query_as("SELECT status,missing_streak,dead_letter FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            state,
            (
                if n == 1 { "ok" } else { "missing" }.into(),
                n as i32,
                false
            )
        );
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cursor_privacy_is_confirmed_and_bad_requests_back_off(pool: PgPool) {
    let mut cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    cfg.adapters.platforms.insert(
        "google_calendar_api".into(),
        adapters::Options {
            key: Some("fixture".into()),
            ..Default::default()
        },
    );
    for (uri, source) in [
        (
            "https://portal.einundzwanzig.space/api/changes",
            "einundzwanzig",
        ),
        ("https://api.btcmap.org/v4/events", "btcmap"),
        (
            "https://www.googleapis.com/calendar/v3/calendars/public/events",
            "google_calendar_api",
        ),
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        for (index, status) in [401, 403, 404, 451].into_iter().enumerate() {
            if source == "google_calendar_api" && matches!(status, 401 | 403) {
                continue;
            }
            let mut accepted = cursor::container(uri, &cfg);
            accepted.fetched_at += index as i64 * 10000;
            store::save_document(&pool, &id, 0, &accepted)
                .await
                .unwrap();
            for n in 1..=2 {
                let (egress, _) = mock_egress(vec![(status, Value::Null)]);
                let mut doc = cursor::resolve(
                    &pool,
                    uri,
                    adapters::definition(source).unwrap(),
                    &egress,
                    &cfg,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(doc.status, pinakes_core::model::Status::Missing);
                assert!(doc
                    .container
                    .as_ref()
                    .is_some_and(|c| !c.delta && c.cursor.is_none()));
                doc.fetched_at = accepted.fetched_at + n * 1000;
                store::save_document(&pool, &id, 0, &doc).await.unwrap();
                let state: (String, i32, bool) = sqlx::query_as(
                    "SELECT status,missing_streak,dead_letter FROM resource WHERE id=$1",
                )
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(
                    state,
                    (
                        if n == 1 { "ok" } else { "missing" }.into(),
                        n as i32,
                        false
                    ),
                    "{source} {status}"
                );
            }
        }
        for status in [400, 418] {
            let (egress, _) = mock_egress(vec![(status, Value::Null)]);
            let err = cursor::resolve(
                &pool,
                uri,
                adapters::definition(source).unwrap(),
                &egress,
                &cfg,
                false,
            )
            .await
            .unwrap_err();
            assert_eq!(
                err.downcast_ref::<pinakes_core::resolver::RetryLater>()
                    .unwrap()
                    .seconds,
                3600
            );
        }
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn google_credential_denials_keep_occurrences_and_count_drift(pool: PgPool) {
    use pinakes_core::{queue, resolver::Resolver, telemetry};
    let uri = "https://www.googleapis.com/calendar/v3/calendars/public/events";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    cfg.adapters.platforms.insert(
        "google_calendar_api".into(),
        adapters::Options {
            key: Some("fixture".into()),
            ..Default::default()
        },
    );
    for status in [401, 403, 404] {
        let mut accepted = cursor::container(uri, &cfg);
        cursor::append(&mut accepted,"google_calendar_api",&json!({"id":"keep","summary":"Retained event","start":{"dateTime":(Utc::now()+Duration::days(1)).to_rfc3339()}}),&cfg).unwrap();
        accepted.fetched_at += i64::from(status) * 10000;
        store::save_document(&pool, &id, 0, &accepted)
            .await
            .unwrap();
        for n in 1..=2 {
            let (egress, _) = mock_egress(vec![(status, Value::Null)]);
            let r = platform(&pool, cfg.clone(), egress);
            let metrics = telemetry::Metrics::new().unwrap();
            if status == 404 {
                let mut missing = r.resolve(uri).await.unwrap();
                assert_eq!(missing.status, pinakes_core::model::Status::Missing);
                missing.fetched_at = accepted.fetched_at + n * 3_600_000;
                store::save_document(&pool, &id, 0, &missing).await.unwrap();
            } else {
                let (result, observations) = telemetry::observe(
                    &metrics,
                    &r.manifest(),
                    queue::run_job(
                        &pool,
                        &r,
                        &Default::default(),
                        &queue::Job {
                            id: id.clone(),
                            uri: uri.into(),
                            attempts: 0,
                            generation: 0,
                        },
                    ),
                )
                .await;
                result.unwrap();
                assert!(observations.iter().any(|o| o.outcome == "drift"));
                let state:(String,i32,String,i32,bool)=sqlx::query_as("SELECT status,missing_streak,last_error,attempts,dead_letter FROM resource WHERE id=$1").bind(&id).fetch_one(&pool).await.unwrap();
                assert_eq!(state, ("ok".into(), 0, "upstream_denied".into(), 0, false));
                let active:i64=sqlx::query_scalar("SELECT count(*) FROM occurrence WHERE resource_id=$1 AND coalesce(doc->>'event_status','scheduled')<>'cancelled'").bind(&id).fetch_one(&pool).await.unwrap();
                assert_eq!(active, 1);
            }
        }
        if status == 404 {
            let status: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(status, "missing");
        }
    }
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cursor_quarantines_bad_rows_without_inventing_deletions(pool: PgPool) {
    let uri = "https://portal.einundzwanzig.space/api/changes";
    let cfg = EventConfig {
        ..Default::default()
    };
    let good = json!({"id":7,"title":"Good","start":(Utc::now()+Duration::days(1)).to_rfc3339()});
    let (egress, _) = mock_egress(vec![
        (
            200,
            json!({"next_since":12,"has_more":false,"changes":[{"resource":"meetup-event","action":"created","data":{"title":"No id"}}]}),
        ),
        (
            200,
            json!([{"id":1,"title":"Bad date","start":"garbage"},{"title":"No id"},good]),
        ),
    ]);
    let doc = cursor::resolve(
        &pool,
        uri,
        adapters::definition("einundzwanzig").unwrap(),
        &egress,
        &cfg,
        false,
    )
    .await
    .unwrap();
    let c = doc.container.unwrap();
    assert_eq!(c.occurrences.len(), 1);
    assert_eq!(c.occurrences[0].uid, "7");
    assert!(c.delta);
    assert!(c.warnings.iter().any(|w| w == "invalid_cursor_row"));
    assert_eq!(c.cursor.unwrap().next, "12");
}

/// NIP-52 dates, NIP-52 timestamps and BTC Map's `updated_at` take the same years
/// as the calendar parsers (0000–9999). A Nostr event is its own resource, so a bad date refuses
/// that event's parse, like any other malformed NIP-52 event; a BTC Map row is dropped with the
/// feed's row warning while its siblings are kept.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn adapter_dates_have_four_digit_years(pool: PgPool) {
    use pinakes_core::event::adapters::nostr::{self as nip52, Target};
    let cfg = EventConfig::default();
    let event = |kind: u16, start: &str, end: Option<&str>| {
        let mut tags: Vec<&[&str]> = vec![&["d", "edge"], &["title", "Edge"]];
        let start = ["start", start];
        tags.push(&start);
        let end = end.map(|e| ["end", e]);
        if let Some(end) = &end {
            tags.push(end);
        }
        signed(kind, 100, &tags)
    };
    let uri = |e: &nostr::event::Event| {
        Target::coordinate(&format!("{}:{}:edge", e.kind.as_u16(), e.pubkey.to_hex()))
            .unwrap()
            .uri()
            .unwrap()
    };
    for (kind, start, end) in [
        (31922, "+12026-10-20", None),
        (31922, "-262143-01-01", None),
        (31922, "2026-10-20", Some("+12026-10-21")),
        (31922, "2026-10-20", Some("-262143-01-01")),
        (31923, "253402300800", None),
        (31923, "-62167219201", None),
        // Year 0000 is refused too (Postgres cannot read it back).
        (31923, "-62167219200", None),
        (31923, "1792519200", Some("253402300800")),
    ] {
        let e = event(kind, start, end);
        let error = nip52::parse(&uri(&e), &e).unwrap_err().to_string();
        assert!(
            error.starts_with("invalid"),
            "{kind} {start} {end:?}: {error}"
        );
    }
    for (kind, start, end, first) in [
        (
            31922,
            "2026-10-20",
            Some("2026-10-22"),
            "2026-10-20T00:00:00Z",
        ),
        (31922, "20261020", None, "2026-10-20T00:00:00Z"),
        (31923, "253402300799", None, "9999-12-31T23:59:59Z"),
        (31923, "-62135596800", None, "0001-01-01T00:00:00Z"),
    ] {
        let e = event(kind, start, end);
        let doc = nip52::parse(&uri(&e), &e).unwrap();
        assert_eq!(doc.start, Some(first.parse().unwrap()), "{kind} {start}");
    }
    let at = (Utc::now() + Duration::days(2)).to_rfc3339();
    let (egress, _) = mock_egress(vec![(
        200,
        json!([
            {"id":1,"status":"live","starts_at":at,"name":"One","updated_at":"2026-10-15T13:00:00Z"},
            {"id":2,"status":"live","starts_at":at,"name":"Two","updated_at":"+12026-10-15T13:00:00Z"},
            {"id":3,"status":"live","starts_at":at,"name":"Three","updated_at":"-262143-01-01T00:00:00Z"}
        ]),
    )]);
    let c = cursor::resolve(
        &pool,
        "https://api.btcmap.org/v4/events",
        adapters::definition("btcmap").unwrap(),
        &egress,
        &cfg,
        false,
    )
    .await
    .unwrap()
    .container
    .unwrap();
    assert_eq!(
        c.occurrences
            .iter()
            .map(|o| o.uid.as_str())
            .collect::<Vec<_>>(),
        ["1"]
    );
    assert_eq!(c.warnings, ["invalid_cursor_row"]);
    assert_eq!(c.cursor.unwrap().next, "2026-10-15T13:00:00+00:00");
}

/// One BTC Map row with a year-0000 `updated_at`, and one whose start lands in year
/// 10000 UTC, neither stall the source: the delta applies, keeps the good rows, and commits its
/// cursor at the last good row.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_bad_btcmap_row_never_stalls_the_cursor(pool: PgPool) {
    let cfg = EventConfig::default();
    let uri = "https://api.btcmap.org/v4/events";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let at = (Utc::now() + Duration::days(2)).to_rfc3339();
    let (egress, _) = mock_egress(vec![(
        200,
        json!([
            {"id":1,"status":"live","starts_at":at,"name":"One","updated_at":"2026-10-15T13:00:00Z"},
            {"id":2,"status":"live","starts_at":at,"name":"Zero","updated_at":"0000-01-01T00:00:00Z"},
            {"id":3,"status":"live","starts_at":"9999-12-31T23:59:59-05:00","name":"Far","updated_at":"2026-10-15T12:00:00Z"}
        ]),
    )]);
    let doc = cursor::resolve(
        &pool,
        uri,
        adapters::definition("btcmap").unwrap(),
        &egress,
        &cfg,
        false,
    )
    .await
    .unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let uids: Vec<String> =
        sqlx::query_scalar("SELECT uid FROM occurrence WHERE resource_id=$1 ORDER BY uid")
            .bind(&id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(uids, ["1"]);
    let warnings: Value = sqlx::query_scalar("SELECT doc->'warnings' FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(warnings, json!(["occurrence_start_out_of_range"]));
    assert_eq!(
        saved_cursor(&pool, &id).await.as_deref(),
        Some("2026-10-15T13:00:00+00:00")
    );
}

/// A NIP-52 calendar member whose own resolution failed (refused parse, unreachable
/// or dead-lettered) is skipped with `calendar_member_unavailable` like a missing one, so the
/// calendar applies its other members; a member not yet resolved still makes it wait.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_failed_calendar_member_never_wedges_its_calendar(pool: PgPool) {
    use pinakes_core::event::adapters::nostr::{self as nip52, Target};
    let at = (Utc::now() + Duration::days(1)).timestamp().to_string();
    let member = |d: &str, start: &str| {
        signed(
            31923,
            100,
            &[&["d", d], &["title", "Member"], &["start", start]],
        )
    };
    let good = member("good", &at);
    let bad = member("bad", "253402300800");
    let reference = |e: &nostr::event::Event, d: &str| format!("31923:{}:{d}", e.pubkey.to_hex());
    let (good_ref, bad_ref) = (reference(&good, "good"), reference(&bad, "bad"));
    let parent = signed(
        31924,
        101,
        &[
            &["d", "calendar"],
            &["title", "Calendar"],
            &["a", &good_ref],
            &["a", &bad_ref],
        ],
    );
    let parent_uri = Target::coordinate(&format!("31924:{}:calendar", parent.pubkey.to_hex()))
        .unwrap()
        .uri()
        .unwrap();
    store::ensure_resource(&pool, &parent_uri).await.unwrap();
    let (egress, _) = mock_egress(vec![(200, json!([parent]))]);
    let cfg = EventConfig::default();
    let resolve = || nip52::resolve(&parent_uri, &egress, &cfg, &pool, false);
    let pending = |r: anyhow::Result<pinakes_core::model::ResourceMeta>| {
        r.unwrap_err()
            .downcast_ref::<pinakes_core::resolver::RetryLater>()
            .is_some_and(|r| r.class == "calendar_members_pending")
    };
    assert!(pending(resolve().await), "both members unresolved");
    let id = |r: &str| {
        pinakes_core::model::identity(&Target::coordinate(r).unwrap().uri().unwrap())
            .unwrap()
            .0
    };
    let good_uri = Target::coordinate(&good_ref).unwrap().uri().unwrap();
    let doc = nip52::parse(&good_uri, &good).unwrap();
    store::save_document(&pool, &id(&good_ref), 0, &doc)
        .await
        .unwrap();
    let bad_uri = Target::coordinate(&bad_ref).unwrap().uri().unwrap();
    assert!(
        nip52::parse(&bad_uri, &bad).is_err(),
        "the bad member's parse is refused"
    );
    assert!(pending(resolve().await), "the bad member is still pending");
    for failure in [
        "status='error',attempts=1,last_error='invalid start'",
        "status='unreachable'",
        "status='pending',dead_letter=true",
    ] {
        sqlx::query(
            "UPDATE resource SET status='pending',dead_letter=false,final=false WHERE id=$1",
        )
        .bind(id(&bad_ref))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(&format!("UPDATE resource SET {failure} WHERE id=$1"))
            .bind(id(&bad_ref))
            .execute(&pool)
            .await
            .unwrap();
        let doc = resolve().await.unwrap();
        assert_eq!(
            doc.event.warnings,
            ["calendar_member_unavailable"],
            "{failure}"
        );
        let uids: Vec<_> = doc
            .container
            .unwrap()
            .occurrences
            .into_iter()
            .map(|o| o.uid)
            .collect();
        assert_eq!(uids, std::slice::from_ref(&good_ref), "{failure}");
    }
}

/// A NIP-52 calendar over `members` (kind 31923 events), saved as a resource.
async fn nip52_calendar(
    pool: &PgPool,
    members: &[&nostr::event::Event],
) -> (String, nostr::event::Event) {
    use pinakes_core::event::adapters::nostr::Target;
    let refs: Vec<String> = members
        .iter()
        .map(|m| {
            format!(
                "31923:{}:{}",
                m.pubkey.to_hex(),
                m.tags
                    .iter()
                    .find(|t| t.as_slice()[0] == "d")
                    .unwrap()
                    .as_slice()[1]
            )
        })
        .collect();
    let mut tags: Vec<Vec<&str>> = vec![vec!["d", "calendar"], vec!["title", "Calendar"]];
    for r in &refs {
        tags.push(vec!["a", r]);
    }
    let tags: Vec<&[&str]> = tags.iter().map(Vec::as_slice).collect();
    let parent = signed(31924, 101, &tags);
    let uri = Target::coordinate(&format!("31924:{}:calendar", parent.pubkey.to_hex()))
        .unwrap()
        .uri()
        .unwrap();
    store::ensure_resource(pool, &uri).await.unwrap();
    (uri, parent)
}
fn member_id(e: &nostr::event::Event) -> (String, String) {
    use pinakes_core::event::adapters::nostr::Target;
    let d = e
        .tags
        .iter()
        .find(|t| t.as_slice()[0] == "d")
        .unwrap()
        .as_slice()[1]
        .clone();
    let uri = Target::coordinate(&format!("{}:{}:{d}", e.kind.as_u16(), e.pubkey.to_hex()))
        .unwrap()
        .uri()
        .unwrap();
    (pinakes_core::model::identity(&uri).unwrap().0, uri)
}

/// NIP-52 all-day dates whose midnight is not storable (0000-01-01, or 9999-12-31
/// with its default next-day end) are refused at parse; and an accepted member without a start
/// is skipped as unavailable instead of keeping its calendar pending.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn year_0000_nip52_dates_never_wedge_a_calendar(pool: PgPool) {
    use pinakes_core::event::adapters::nostr as nip52;
    let cfg = EventConfig::default();
    let day = |d: &str, start: &str, end: Option<&str>| {
        let mut tags: Vec<&[&str]> = vec![&["title", "Day"]];
        let d = ["d", d];
        let start = ["start", start];
        tags.push(&d);
        tags.push(&start);
        let end = end.map(|e| ["end", e]);
        if let Some(end) = &end {
            tags.push(end);
        }
        signed(31922, 100, &tags)
    };
    for (start, end) in [
        ("0000-01-01", None),
        ("0000-12-31", Some("0001-01-02")),
        ("9999-12-31", None),
        ("2026-10-20", Some("0000-01-01")),
    ] {
        let e = day("edge", start, end);
        let error = nip52::parse(&member_id(&e).1, &e).unwrap_err().to_string();
        assert!(
            error.starts_with("invalid all-day"),
            "{start} {end:?}: {error}"
        );
    }
    for (start, end, last) in [
        ("0001-01-01", None, "0001-01-02T00:00:00Z"),
        ("9999-12-30", None, "9999-12-31T00:00:00Z"),
    ] {
        let e = day("edge", start, end);
        let doc = nip52::parse(&member_id(&e).1, &e).unwrap();
        assert_eq!(doc.end, Some(last.parse().unwrap()), "{start}");
    }
    // The backstop: an `ok` member whose start was dropped at the store.
    let at = (Utc::now() + Duration::days(1)).timestamp().to_string();
    let timed = |d: &str| {
        signed(
            31923,
            100,
            &[&["d", d], &["title", "Member"], &["start", &at]],
        )
    };
    let (good, startless) = (timed("good"), timed("startless"));
    let (uri, parent) = nip52_calendar(&pool, &[&good, &startless]).await;
    let (egress, _) = mock_egress(vec![(200, json!([parent]))]);
    let _ = nip52::resolve(&uri, &egress, &cfg, &pool, false).await;
    for (member, start) in [(&good, true), (&startless, false)] {
        let (id, member_uri) = member_id(member);
        let mut doc = nip52::parse(&member_uri, member).unwrap();
        if !start {
            doc.start = None;
            doc.end = None;
        }
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
    }
    let doc = nip52::resolve(&uri, &egress, &cfg, &pool, false)
        .await
        .unwrap();
    assert_eq!(doc.event.warnings, ["calendar_member_unavailable"]);
    assert_eq!(doc.container.unwrap().occurrences.len(), 1);
}

/// A member whose newest version is refused (its resource is `error`, its stored
/// document the last accepted version) is skipped like a missing member: its row is cancelled
/// after two snapshots; the last accepted version is not kept.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_refused_newest_member_version_cancels_after_two_snapshots(pool: PgPool) {
    use pinakes_core::event::adapters::nostr as nip52;
    let cfg = EventConfig::default();
    let at = (Utc::now() + Duration::days(1)).timestamp().to_string();
    let timed = |d: &str| {
        signed(
            31923,
            100,
            &[&["d", d], &["title", "Member"], &["start", &at]],
        )
    };
    let (kept, refused) = (timed("kept"), timed("refused"));
    let (uri, parent) = nip52_calendar(&pool, &[&kept, &refused]).await;
    let (egress, _) = mock_egress(vec![(200, json!([parent]))]);
    let _ = nip52::resolve(&uri, &egress, &cfg, &pool, false).await;
    for member in [&kept, &refused] {
        let (id, member_uri) = member_id(member);
        let doc = nip52::parse(&member_uri, member).unwrap();
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
    }
    let calendar = pinakes_core::model::identity(&uri).unwrap().0;
    let base = Utc::now().timestamp_millis();
    let mut snapshot = 0;
    let mut save = || {
        snapshot += 1;
        let (pool, egress, cfg, uri, calendar) =
            (&pool, &egress, &cfg, uri.clone(), calendar.clone());
        let n = snapshot;
        async move {
            let mut doc = nip52::resolve(&uri, egress, cfg, pool, false)
                .await
                .unwrap();
            doc.fetched_at = base + n * 1000;
            store::save_document(pool, &calendar, 0, &doc)
                .await
                .unwrap();
            sqlx::query_as::<_, (String, String)>(
                "SELECT uid,status FROM occurrence WHERE resource_id=$1 ORDER BY uid",
            )
            .bind(&calendar)
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|(uid, status)| (uid.rsplit(':').next().unwrap().to_owned(), status))
            .collect::<Vec<_>>()
        }
    };
    let rows = |a: &str, b: &str| {
        vec![
            ("kept".to_owned(), a.to_owned()),
            ("refused".to_owned(), b.to_owned()),
        ]
    };
    assert_eq!(save().await, rows("scheduled", "scheduled"));
    // The newest version of `refused` fails its parse: the resource is `error`, its stored
    // document stays the accepted one.
    sqlx::query(
        "UPDATE resource SET status='error',attempts=1,last_error='invalid start' WHERE id=$1",
    )
    .bind(member_id(&refused).0)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(save().await, rows("scheduled", "scheduled"), "missing once");
    assert_eq!(
        save().await,
        rows("scheduled", "cancelled"),
        "cancelled after two"
    );
}

/// A BTC Map end late on 9999-12-31 (year 10000 UTC) never becomes a derived
/// link's expiry: the start is used instead, and `derive_resource_until` clamps any expiry to
/// 9999-12-31T23:59:59Z.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn derived_link_expiry_is_bounded(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let uri = "https://api.btcmap.org/v4/events";
    store::ensure_resource(&pool, uri).await.unwrap();
    let at =
        chrono::DateTime::from_timestamp((Utc::now() + Duration::days(2)).timestamp(), 0).unwrap();
    let (egress, _) = mock_egress(vec![(
        200,
        json!([{"id":1,"status":"live","starts_at":at.to_rfc3339(),"ends_at":"9999-12-31T23:59:59-05:00","name":"One","website":"https://example.org/one","updated_at":"2026-10-15T13:00:00Z"}]),
    )]);
    let r = platform(
        &pool,
        EventConfig {
            geocoding: false,
            ..Default::default()
        },
        egress,
    );
    r.resolve(uri).await.unwrap();
    let expiry = |child: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<chrono::DateTime<Utc>>>(
                "SELECT d.expires_at FROM resource_derivation d JOIN resource r ON r.id=d.child_id WHERE r.uri=$1",
            )
            .bind(child)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(expiry("https://example.org/one").await, Some(at));
    store::derive_resource_until(
        &pool,
        uri,
        "https://example.org/two",
        "btcmap",
        10,
        10,
        chrono::DateTime::from_timestamp(253402318799, 0),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        expiry("https://example.org/two").await,
        chrono::DateTime::from_timestamp(253402300799, 0)
    );
}

/// A distant but storable BTC Map event never holds a derived link's slot past the
/// event horizon, and a sub-microsecond `updated_at` just below year 10000 is stored in year
/// 9999.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn derived_links_expire_within_the_horizon(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let uri = "https://api.btcmap.org/v4/events";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let (egress, _) = mock_egress(vec![(
        200,
        json!([
            {"id":1,"status":"live","starts_at":"9999-06-01T00:00:00Z","name":"Far","website":"https://example.org/far","updated_at":"2026-10-15T13:00:00Z"},
            {"id":2,"status":"live","starts_at":(Utc::now() + Duration::days(2)).to_rfc3339(),"name":"Near","updated_at":"9999-12-31T23:59:59.9999995Z"}
        ]),
    )]);
    let doc = platform(&pool, cfg.clone(), egress)
        .resolve(uri)
        .await
        .unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let modified: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT modified FROM occurrence WHERE resource_id=$1 AND uid='2'")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        modified,
        Some("9999-12-31T23:59:59.999999Z".parse().unwrap())
    );
    let horizon = store::derivation_horizon(&cfg);
    let expiry = |child: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<chrono::DateTime<Utc>>>(
                "SELECT d.expires_at FROM resource_derivation d JOIN resource r ON r.id=d.child_id WHERE r.uri=$1",
            )
            .bind(child)
            .fetch_one(&pool)
            .await
            .unwrap()
            .unwrap()
        }
    };
    let near = |a: chrono::DateTime<Utc>| (a - horizon).num_seconds().abs() < 60;
    assert!(near(expiry("https://example.org/far").await));
}

/// A member that never resolves (its lookup keeps answering "try later") stops
/// blocking its calendar after `event.adapters.calendar_member_wait_hours`: the calendar applies
/// its other members with `calendar_member_unavailable`.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_member_that_never_resolves_stops_blocking_its_calendar(pool: PgPool) {
    use pinakes_core::event::adapters::nostr as nip52;
    let cfg = EventConfig::default();
    assert_eq!(cfg.adapters.calendar_member_wait_hours, 48);
    let mut invalid = cfg.clone();
    invalid.adapters.calendar_member_wait_hours = 0;
    assert!(adapters::validate(&invalid).is_err());
    let at = (Utc::now() + Duration::days(1)).timestamp().to_string();
    let timed = |d: &str| {
        signed(
            31923,
            100,
            &[&["d", d], &["title", "Member"], &["start", &at]],
        )
    };
    let (good, stuck) = (timed("good"), timed("stuck"));
    let (uri, parent) = nip52_calendar(&pool, &[&good, &stuck]).await;
    let (egress, _) = mock_egress(vec![(200, json!([parent]))]);
    let resolve = |cfg: EventConfig| {
        let (uri, egress, pool) = (uri.clone(), &egress, &pool);
        async move { nip52::resolve(&uri, egress, &cfg, pool, false).await }
    };
    let pending = |r: anyhow::Result<pinakes_core::model::ResourceMeta>| {
        r.unwrap_err()
            .downcast_ref::<pinakes_core::resolver::RetryLater>()
            .is_some_and(|r| r.class == "calendar_members_pending")
    };
    assert!(pending(resolve(cfg.clone()).await));
    let (good_id, good_uri) = member_id(&good);
    let doc = nip52::parse(&good_uri, &good).unwrap();
    store::save_document(&pool, &good_id, 0, &doc)
        .await
        .unwrap();
    assert!(pending(resolve(cfg.clone()).await), "within the wait");
    // The wait runs from when this calendar first listed the member.
    sqlx::query(
        "UPDATE resource_derivation SET first_seen_at=now()-interval '49 hours' WHERE child_id=$1",
    )
    .bind(member_id(&stuck).0)
    .execute(&pool)
    .await
    .unwrap();
    let doc = resolve(cfg.clone()).await.unwrap();
    assert_eq!(doc.event.warnings, ["calendar_member_unavailable"]);
    assert_eq!(doc.container.unwrap().occurrences.len(), 1);
    let mut patient = cfg.clone();
    patient.adapters.calendar_member_wait_hours = 72;
    assert!(pending(resolve(patient).await), "a longer wait still waits");
}

/// The member wait is the calendar's own. (a) A member referenced long before this
/// calendar listed it (anyone can reference a `nostr:` URI early) does not shorten the wait.
/// (b) A calendar whose last stored snapshot, or first sighting, is older than the wait stops
/// waiting for unresolved members, however recently they were listed.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_member_wait_is_per_calendar_and_bounded(pool: PgPool) {
    use pinakes_core::event::adapters::nostr as nip52;
    let cfg = EventConfig::default();
    let at = (Utc::now() + Duration::days(1)).timestamp().to_string();
    let timed = |d: &str| {
        signed(
            31923,
            100,
            &[&["d", d], &["title", "Member"], &["start", &at]],
        )
    };
    let (good, stuck) = (timed("good"), timed("stuck"));
    // Referenced 100 hours before any calendar listed it.
    let (stuck_id, stuck_uri) = member_id(&stuck);
    store::ensure_resource(&pool, &stuck_uri).await.unwrap();
    sqlx::query("UPDATE resource SET created_at=now()-interval '100 hours' WHERE id=$1")
        .bind(&stuck_id)
        .execute(&pool)
        .await
        .unwrap();
    let (uri, parent) = nip52_calendar(&pool, &[&good, &stuck]).await;
    let calendar = pinakes_core::model::identity(&uri).unwrap().0;
    let (egress, _) = mock_egress(vec![(200, json!([parent]))]);
    let resolve = || nip52::resolve(&uri, &egress, &cfg, &pool, false);
    let pending = |r: anyhow::Result<pinakes_core::model::ResourceMeta>| {
        r.unwrap_err()
            .downcast_ref::<pinakes_core::resolver::RetryLater>()
            .is_some_and(|r| r.class == "calendar_members_pending")
    };
    let _ = resolve().await;
    let (good_id, good_uri) = member_id(&good);
    let doc = nip52::parse(&good_uri, &good).unwrap();
    store::save_document(&pool, &good_id, 0, &doc)
        .await
        .unwrap();
    assert!(
        pending(resolve().await),
        "(a) an early reference does not shorten this calendar's wait"
    );
    // (b) The calendar itself was first seen 49 hours ago and never stored a snapshot.
    sqlx::query("UPDATE resource SET created_at=now()-interval '49 hours' WHERE id=$1")
        .bind(&calendar)
        .execute(&pool)
        .await
        .unwrap();
    let doc = resolve().await.unwrap();
    assert_eq!(doc.event.warnings, ["calendar_member_unavailable"]);
    assert_eq!(doc.container.unwrap().occurrences.len(), 1);
    // A recent stored snapshot restores the wait.
    sqlx::query("INSERT INTO event_fetch(resource_id,last_snapshot_at) VALUES($1,$2)")
        .bind(&calendar)
        .bind(Utc::now().timestamp_millis())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        pending(resolve().await),
        "(b) a recent snapshot: still waiting"
    );
    sqlx::query("UPDATE event_fetch SET last_snapshot_at=$2 WHERE resource_id=$1")
        .bind(&calendar)
        .bind((Utc::now() - Duration::hours(49)).timestamp_millis())
        .execute(&pool)
        .await
        .unwrap();
    assert!(resolve().await.is_ok(), "(b) an old snapshot: applies");
}

/// A far BTC Map event's link is capped at the horizon when derived, keeps its
/// event's own instant, and is extended to it once the event enters the horizon, by the next
/// delta apply or at startup; a year-9999 event never qualifies and keeps the cap.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_far_event_keeps_its_link_as_the_horizon_moves(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let uri = "https://api.btcmap.org/v4/events";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let start =
        chrono::DateTime::from_timestamp((Utc::now() + chrono::Months::new(18)).timestamp(), 0)
            .unwrap();
    let row = json!({"id":1,"status":"live","starts_at":start.to_rfc3339(),"name":"Far","website":"https://example.org/far18","updated_at":"2026-10-15T13:00:00Z"});
    let (egress, _) = mock_egress(vec![(200, json!([row]))]);
    let doc = platform(&pool, cfg.clone(), egress)
        .resolve(uri)
        .await
        .unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let link = || {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)>(
                "SELECT d.expires_at,d.until FROM resource_derivation d JOIN resource r ON r.id=d.child_id WHERE r.uri='https://example.org/far18'",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (expires, until) = link().await;
    assert_eq!(until, Some(start), "the event's own instant is kept");
    assert!(
        (expires - store::derivation_horizon(&cfg))
            .num_seconds()
            .abs()
            < 60,
        "capped at the horizon"
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM occurrence WHERE resource_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0, "a delta feed stores no row beyond the horizon");
    // Seven months later (shifted back instead of waiting): the event is 11 months out, the
    // link expires in 5, and BTC Map never resends the unedited event.
    let shift = || {
        let pool = pool.clone();
        async move {
            sqlx::query("UPDATE resource_derivation SET expires_at=expires_at-interval '7 months',until=until-interval '7 months'")
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    shift().await;
    let (egress, _) = mock_egress(vec![(200, json!([]))]);
    let mut doc = platform(&pool, cfg.clone(), egress)
        .resolve(uri)
        .await
        .unwrap();
    doc.fetched_at += 1000;
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let (expires, until) = link().await;
    assert_eq!(
        Some(expires),
        until,
        "the delta apply extended it to the event"
    );
    // A year-9999 event never enters the horizon: the next apply leaves its cap alone.
    store::derive_resource_within(
        &pool,
        uri,
        "https://example.org/never",
        "btcmap",
        100,
        100,
        chrono::DateTime::from_timestamp(253385000000, 0),
        Some(store::derivation_horizon(&cfg)),
    )
    .await
    .unwrap()
    .unwrap();
    let (egress, _) = mock_egress(vec![(200, json!([]))]);
    let mut doc = platform(&pool, cfg.clone(), egress)
        .resolve(uri)
        .await
        .unwrap();
    doc.fetched_at += 2000;
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let capped: chrono::DateTime<Utc> = sqlx::query_scalar(
        "SELECT d.expires_at FROM resource_derivation d JOIN resource r ON r.id=d.child_id WHERE r.uri='https://example.org/never'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        (capped - store::derivation_horizon(&cfg))
            .num_seconds()
            .abs()
            < 60
    );
}

/// A single BTC Map event document carries storable horizons, not chrono's
/// sentinels, and saves through the full save path with its container.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_single_event_document_saves_with_storable_horizons(pool: PgPool) {
    let cfg = EventConfig::default();
    let uri = "https://api.btcmap.org/v4/events/7";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let at = (Utc::now() + Duration::days(2)).to_rfc3339();
    let (egress, _) = mock_egress(vec![(
        200,
        json!({"id":7,"status":"live","starts_at":at,"name":"Seven","updated_at":"2026-10-15T13:00:00Z"}),
    )]);
    let doc = cursor::resolve(
        &pool,
        uri,
        adapters::definition("btcmap").unwrap(),
        &egress,
        &cfg,
        false,
    )
    .await
    .unwrap();
    let c = doc.container.as_ref().unwrap();
    assert_eq!(c.horizon_start, pinakes_core::event::time::earliest());
    assert_eq!(c.horizon_end, pinakes_core::event::time::latest());
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let (start, end): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT horizon_start,horizon_end FROM event_fetch WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((start, end), (c.horizon_start, c.horizon_end));
}

/// HTML pages by host and path (any query), 404 otherwise.
struct HtmlPages(std::collections::BTreeMap<String, String>);
#[async_trait::async_trait]
impl Transport for HtmlPages {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &Default::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        let page = self
            .0
            .get(&format!("{}{}", url.host_str().unwrap_or(""), url.path()));
        // `REDIRECT <url>` answers 302 there, so a page can move to another host.
        if let Some(target) = page.and_then(|p| p.strip_prefix("REDIRECT ")) {
            return Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                url: url.clone(),
                validators: Default::default(),
                content_type: "text/html".into(),
                status: 302,
                retry_after_secs: 60,
                location: Some(target.into()),
                body: vec![],
            });
        }
        // JSON and ICS bodies carry their own content types.
        let content_type = match page.map(String::as_str) {
            Some(p) if p.starts_with('{') => "application/json",
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
fn html_platform(pool: &PgPool, pages: &[(&str, String)]) -> adapters::service::PlatformResolver {
    platform(
        pool,
        EventConfig {
            geocoding: false,
            ..Default::default()
        },
        Egress::with_backends(
            Default::default(),
            Arc::new(AdapterDns),
            Arc::new(HtmlPages(
                pages
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), v.clone()))
                    .collect(),
            )),
        ),
    )
}
/// An event page as Meetup and Luma publish them: a head canonical and og:url naming the clean
/// URL, and a JSON-LD Event whose `url` is the same.
fn event_page(canonical: &str, name: &str, same_as: Option<&str>) -> String {
    let start = (Utc::now() + Duration::days(20)).to_rfc3339();
    let same_as = same_as.map_or(String::new(), |s| format!(r#","sameAs":"{s}""#));
    format!(
        r#"<html><head><title>{name}</title><link rel="canonical" href="{canonical}"><meta property="og:url" content="{canonical}"><script type="application/ld+json">{{"@context":"https://schema.org","@type":"Event","name":"{name}","startDate":"{start}","eventAttendanceMode":"https://schema.org/OfflineEventAttendanceMode","location":{{"@type":"Place","name":"Hall","address":"Zurich","geo":{{"latitude":47.37,"longitude":8.54}}}},"url":"{canonical}"{same_as}}}</script></head><body><h1>{name}</h1></body></html>"#
    )
}
async fn group_of(pool: &PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT coalesce(group_id,unit_id) FROM group_unit WHERE resource_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Meetup's tracking variants of one event are one resource: the known tracking parameters are
/// aliases from the start, and a variant with another parameter becomes one once its page names
/// the clean URL as its canonical. Nothing joins across platforms.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn tracking_variants_of_an_event_are_one_resource(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let meetup = "https://www.meetup.com/zurich-bitcoin/events/316480149/";
    let luma = "https://lu.ma/abc123";
    let page = event_page(meetup, "Bitcoin Meetup", None);
    let r = html_platform(
        &pool,
        &[
            (
                "www.meetup.com/zurich-bitcoin/events/316480149/",
                page.clone(),
            ),
            ("lu.ma/abc123", event_page(luma, "Bitcoin Meetup", None)),
        ],
    );
    let root = |uri: &str| {
        let pool = pool.clone();
        let uri = uri.to_owned();
        async move {
            let id = store::ensure_resource(&pool, &uri).await.unwrap();
            pinakes_core::alias::resolve_id(&pool, &id).await.unwrap()
        }
    };
    let clean = pinakes_core::model::identity(meetup).unwrap().0;
    for uri in [
        format!("{meetup}?recId=a1&recSource=x&searchId=s1"),
        format!("{meetup}?eventOrigin=group_upcoming_events"),
        format!("{meetup}?utm_medium=referral&utm_source=newsletter"),
    ] {
        assert_eq!(root(&uri).await, clean, "{uri}");
    }
    let luma_root = root(&format!("{luma}?tk=one")).await;
    assert_eq!(luma_root, root(&format!("{luma}?tk=two")).await);
    assert_ne!(
        luma_root, clean,
        "the same event on another platform does not join"
    );
    // Fetched once, its page names the clean URL; `ref` is not a tracking parameter, so it may
    // name the object and the variant stays its own resource.
    let other = format!("{meetup}?ref=calendar");
    let id = store::ensure_resource(&pool, &other).await.unwrap();
    assert_eq!(root(&other).await, id);
    let doc = r.resolve(&other).await.unwrap();
    assert!(
        doc.type_path.contains(&"Event".into()),
        "{:?}",
        doc.type_path
    );
    let job = pinakes_core::queue::Job {
        id: id.clone(),
        uri: other.clone(),
        attempts: 0,
        generation: 0,
    };
    pinakes_core::queue::run_job(&pool, &r, &Default::default(), &job)
        .await
        .unwrap();
    let aliased: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1)")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!aliased);
    let routed: Vec<String> = sqlx::query_scalar(
        "SELECT uri FROM resource WHERE resolver IS NOT NULL AND host='www.meetup.com' ORDER BY uri",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(routed, vec![meetup.to_string(), other]);
}

/// Through the web route, a sameAs between event pages on two origins never groups them,
/// one-sided or reciprocated: a claim is not evidence.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn cross_origin_event_same_as_never_groups(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let (a, b) = (
        "https://events.a.example/talk",
        "https://tickets.b.example/talk",
    );
    let one_sided = html_platform(
        &pool,
        &[
            ("events.a.example/talk", event_page(a, "Talk", Some(b))),
            ("tickets.b.example/talk", event_page(b, "Talk", None)),
        ],
    );
    let (ia, ib) = (
        store::ensure_resource(&pool, a).await.unwrap(),
        store::ensure_resource(&pool, b).await.unwrap(),
    );
    for (uri, id) in [(a, &ia), (b, &ib)] {
        let doc = one_sided.web.resolve(uri).await.unwrap();
        assert!(doc.type_path.contains(&"Event".into()));
        store::save_document(&pool, id, 0, &doc).await.unwrap();
    }
    assert_ne!(group_of(&pool, &ia).await, group_of(&pool, &ib).await);
    let reciprocal = html_platform(
        &pool,
        &[("tickets.b.example/talk", event_page(b, "Talk", Some(a)))],
    );
    let mut doc = reciprocal.web.resolve(b).await.unwrap();
    doc.fetched_at += 1000;
    store::save_document(&pool, &ib, 0, &doc).await.unwrap();
    assert_ne!(group_of(&pool, &ia).await, group_of(&pool, &ib).await);
}

/// Pages that name others only by their own claims never group: two thebiz.pro episodes whose
/// canonical is the homepage, docs.pubky.org naming pubky.org, and pubky.org pages naming a
/// sibling page by canonical and sameAs. Each stays its own resource, ungrouped.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn claim_shaped_pages_never_merge(pool: PgPool) {
    let html = |title: &str, canonical: Option<&str>, same_as: Option<&str>| {
        let canonical = canonical.map_or(String::new(), |c| {
            format!(r#"<link rel="canonical" href="{c}"><meta property="og:url" content="{c}">"#)
        });
        let same_as = same_as.map_or(String::new(), |s| {
            format!(r#"<script type="application/ld+json">{{"@type":"WebPage","name":"{title}","sameAs":"{s}"}}</script>"#)
        });
        format!("<html><head><title>{title}</title>{canonical}{same_as}</head><body><p>{title} text</p></body></html>")
    };
    let pages = [
        (
            "thebiz.pro/episodes/1/making-cypherpunk-products-and-businesses",
            html("Episode 1", Some("https://thebiz.pro/"), None),
        ),
        (
            "thebiz.pro/episodes/2/bitcoin-and-the-builders",
            html("Episode 2", Some("https://thebiz.pro/"), None),
        ),
        ("thebiz.pro/", html("The Biz", None, None)),
        (
            "docs.pubky.org/",
            html(
                "Pubky docs",
                Some("https://pubky.org/"),
                Some("https://pubky.org/"),
            ),
        ),
        ("pubky.org/", html("Pubky", None, None)),
        (
            "pubky.org/eli5/",
            html(
                "ELI5",
                Some("https://pubky.org/introduction/"),
                Some("https://pubky.org/introduction/"),
            ),
        ),
        (
            "pubky.org/introduction/",
            html("Introduction", None, Some("https://pubky.org/eli5/")),
        ),
        (
            "pubky.org/api/",
            html("API", Some("https://pubky.org/homeserver/"), None),
        ),
        (
            "pubky.org/homeserver/",
            html("Homeserver", Some("https://pubky.org/api/"), None),
        ),
    ];
    let r = html_platform(
        &pool,
        &pages
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect::<Vec<_>>(),
    );
    let mut ids = vec![];
    for (path, _) in &pages {
        let uri = format!("https://{path}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let job = pinakes_core::queue::Job {
            id: id.clone(),
            uri,
            attempts: 0,
            generation: 0,
        };
        pinakes_core::queue::run_job(&pool, &r.web, &Default::default(), &job)
            .await
            .unwrap();
        ids.push(id);
    }
    pinakes_core::groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let (resolved, aliases, grouped): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM resource WHERE status='ok'),(SELECT count(*) FROM resource_alias),(SELECT count(*) FROM group_unit WHERE group_id IS NOT NULL)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((resolved, aliases, grouped), (ids.len() as i64, 0, 0));
}

/// A NIP-52 calendar `d` of the fixture key, at version `created`, listing `members`.
fn calendar_event(d: &str, created: u64, members: &[&nostr::event::Event]) -> nostr::event::Event {
    let refs: Vec<String> = members
        .iter()
        .map(|m| {
            let tag = m
                .tags
                .iter()
                .find(|t| t.as_slice()[0] == "d")
                .unwrap()
                .as_slice()[1]
                .clone();
            format!("31923:{}:{tag}", m.pubkey.to_hex())
        })
        .collect();
    let mut tags: Vec<Vec<&str>> = vec![vec!["d", d], vec!["title", "Calendar"]];
    for r in &refs {
        tags.push(vec!["a", r]);
    }
    let tags: Vec<&[&str]> = tags.iter().map(Vec::as_slice).collect();
    signed(31924, created, &tags)
}
fn calendar_uri(e: &nostr::event::Event, d: &str) -> String {
    pinakes_core::event::adapters::nostr::Target::coordinate(&format!(
        "31924:{}:{d}",
        e.pubkey.to_hex()
    ))
    .unwrap()
    .uri()
    .unwrap()
}
fn class_of(r: anyhow::Result<pinakes_core::model::ResourceMeta>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => e
            .downcast_ref::<pinakes_core::resolver::RetryLater>()
            .map_or_else(|| e.to_string(), |r| r.class.to_owned()),
    }
}

/// NIP-52 member links end. A calendar that rotates through more members than its
/// cap keeps applying (the links to members it no longer lists end at once); a listed member's
/// link lives for a bounded time renewed at each fetch, so the global budget recovers when a
/// calendar is no longer fetched.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn calendar_member_links_rotate_and_lapse(pool: PgPool) {
    use pinakes_core::event::adapters::nostr as nip52;
    let mut cfg = EventConfig::default();
    cfg.adapters.max_calendar_members = 3;
    cfg.adapters.max_derived_resources = 3;
    let at = (Utc::now() + Duration::days(1)).timestamp().to_string();
    let member = |d: String| {
        signed(
            31923,
            100,
            &[&["d", &d], &["title", "Member"], &["start", &at]],
        )
    };
    let first: Vec<_> = (0..3).map(|i| member(format!("a{i}"))).collect();
    let second: Vec<_> = (0..3).map(|i| member(format!("b{i}"))).collect();
    let v1 = calendar_event("rotating", 101, &first.iter().collect::<Vec<_>>());
    let uri = calendar_uri(&v1, "rotating");
    store::ensure_resource(&pool, &uri).await.unwrap();
    let resolve = |e: &nostr::event::Event, uri: &str, cfg: &EventConfig| {
        let (egress, _) = mock_egress(vec![(200, json!([e]))]);
        let (uri, cfg, pool) = (uri.to_owned(), cfg.clone(), pool.clone());
        async move { nip52::resolve(&uri, &egress, &cfg, &pool, false).await }
    };
    assert_eq!(
        class_of(resolve(&v1, &uri, &cfg).await),
        "calendar_members_pending"
    );
    let live = |parent: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM resource_derivation WHERE parent_id=$1 AND expires_at>now()",
            )
            .bind(parent)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let calendar = pinakes_core::model::identity(&uri).unwrap().0;
    assert_eq!(live(calendar.clone()).await, 3);
    let lifetime: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM min(expires_at)-now())::float8 FROM resource_derivation WHERE parent_id=$1",
    )
    .bind(&calendar)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        (23.0 * 3600.0..=24.0 * 3600.0).contains(&lifetime),
        "{lifetime}"
    );
    // The next version lists three other members: no derivation_cap, the old links ended.
    let v2 = calendar_event("rotating", 102, &second.iter().collect::<Vec<_>>());
    assert_eq!(
        class_of(resolve(&v2, &uri, &cfg).await),
        "calendar_members_pending"
    );
    assert_eq!(live(calendar.clone()).await, 3);
    let (old, _) = member_id(&first[0]);
    let ended: bool = sqlx::query_scalar(
        "SELECT expires_at<=now() FROM resource_derivation WHERE parent_id=$1 AND child_id=$2",
    )
    .bind(&calendar)
    .bind(&old)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(ended, "an un-listed member's link ends");
    // Another calendar finds the adapter's global budget taken until the first one lapses.
    let others: Vec<_> = (0..3).map(|i| member(format!("c{i}"))).collect();
    let other = calendar_event("other", 101, &others.iter().collect::<Vec<_>>());
    let other_uri = calendar_uri(&other, "other");
    store::ensure_resource(&pool, &other_uri).await.unwrap();
    assert_eq!(
        class_of(resolve(&other, &other_uri, &cfg).await),
        "derivation_cap"
    );
    // A day without fetching the first calendar (shifted instead of waited).
    sqlx::query("UPDATE resource_derivation SET expires_at=expires_at-interval '25 hours' WHERE parent_id=$1")
        .bind(&calendar)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        class_of(resolve(&other, &other_uri, &cfg).await),
        "calendar_members_pending",
        "the global budget recovered"
    );
}

/// A Luma page missing fields takes the API branch, whose *event* response also
/// names the event's calendar; the page stays the event and is filled from the API, instead
/// of becoming the calendar.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_luma_event_stays_an_event(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let fixture = {
        use std::io::Read;
        let mut body = String::new();
        flate2::read::GzDecoder::new(
            std::fs::File::open(
                "tests/fixtures/adapters/ba78d4587e1c75e26b58912a94afef2259be53b3c816e073ace5cf62c8552102.gz",
            )
            .unwrap(),
        )
        .read_to_string(&mut body)
        .unwrap();
        body
    };
    let canonical = "https://luma.com/376whqyw";
    // No start in the page, so the adapter asks the API.
    let page = format!(
        r#"<html><head><title>Summit</title><link rel="canonical" href="{canonical}"><script type="application/ld+json">{{"@type":"Event","name":"Summit","url":"{canonical}"}}</script></head><body></body></html>"#
    );
    let calendar = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:other\r\nSUMMARY:Another event\r\nDTSTART:20361022T180000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let r = html_platform(
        &pool,
        &[
            ("luma.com/376whqyw", page),
            ("api.lu.ma/url", fixture),
            ("api.lu.ma/ics/get", calendar.into()),
        ],
    );
    let uri = format!("{canonical}?tk=x");
    let doc = r.resolve(&uri).await.unwrap();
    assert!(doc.container.is_none(), "not the calendar container");
    assert!(
        doc.type_path.contains(&"Event".into()),
        "{:?}",
        doc.type_path
    );
    assert_eq!(doc.name.as_deref(), Some("Summit"));
    assert!(
        doc.aliases.contains(&canonical.to_owned()),
        "{:?}",
        doc.aliases
    );
}

/// A lapsed link keeps its child through the orphan grace, counted from the link's expiry: a
/// calendar that failed for a day deletes no member, and a delta feed can still revive the link,
/// but only within the per-parent and global caps a new link would meet.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn lapsed_links_age_through_the_grace_and_revive_within_the_caps(pool: PgPool) {
    let parent = "https://api.btcmap.org/v4/events";
    store::ensure_resource(&pool, parent).await.unwrap();
    let ahead = Utc::now() + chrono::Duration::days(30);
    let mut children = vec![];
    for (n, days) in [(0, 10), (1, 20), (2, 25)] {
        let child = store::derive_resource_within(
            &pool,
            parent,
            &format!("https://example.org/site{n}"),
            "btcmap",
            100,
            100,
            Some(Utc::now() + chrono::Duration::days(days)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        children.push(child);
    }
    // Every link lapsed a day ago (a cap, or a feed that stopped answering); the children were
    // orphaned when they were derived.
    sqlx::query("UPDATE resource_derivation SET expires_at=now()-interval '1 day'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '30 days' WHERE id=ANY($1)")
        .bind(&children)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        pinakes_core::changes::delete_orphans(&pool, 7)
            .await
            .unwrap(),
        0
    );
    let deleted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_change WHERE class='deleted'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deleted, 0);
    // Revival meets the caps: one slot per parent revives the nearest event only.
    assert_eq!(
        store::extend_parent_links(&pool, parent, ahead, (1, 100))
            .await
            .unwrap(),
        1
    );
    let live: Vec<String> = sqlx::query_scalar(
        "SELECT child_id FROM resource_derivation WHERE expires_at>now() ORDER BY child_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(live, vec![children[0].clone()]);
    // The global cap binds too; a full adapter revives nothing.
    assert_eq!(
        store::extend_parent_links(&pool, parent, ahead, (100, 1))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store::extend_parent_links(&pool, parent, ahead, (100, 2))
            .await
            .unwrap(),
        1,
        "room for one more"
    );
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_derivation WHERE expires_at>now()")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 2);
    // Past the grace from expiry, a still-lapsed child goes.
    sqlx::query(
        "UPDATE resource_derivation SET expires_at=now()-interval '8 days' WHERE child_id=$1",
    )
    .bind(&children[2])
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        pinakes_core::changes::delete_orphans(&pool, 7)
            .await
            .unwrap(),
        1
    );
}

/// A link whose event is more than one horizon away lapses at the derivation-time
/// cap, and comes back once its instant enters the horizon while it is still ahead.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_30_month_event_link_comes_back(pool: PgPool) {
    let cfg = EventConfig::default();
    let parent = "https://api.btcmap.org/v4/events";
    store::ensure_resource(&pool, parent).await.unwrap();
    let until =
        chrono::DateTime::from_timestamp((Utc::now() + chrono::Months::new(30)).timestamp(), 0)
            .unwrap();
    store::derive_resource_within(
        &pool,
        parent,
        "https://example.org/far30",
        "btcmap",
        100,
        100,
        Some(until),
        Some(store::derivation_horizon(&cfg)),
    )
    .await
    .unwrap()
    .unwrap();
    // Nineteen months later: the capped link lapsed seven months ago, the event is 11 out.
    sqlx::query("UPDATE resource_derivation SET expires_at=expires_at-interval '19 months',until=until-interval '19 months'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        store::extend_parent_links(&pool, parent, store::derivation_horizon(&cfg), (100, 100))
            .await
            .unwrap(),
        1
    );
    let (expires, until): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT expires_at,until FROM resource_derivation")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(expires, until);
    assert!(expires > Utc::now());
}

/// A link records at most 64 naming rows, those asking for the farthest instants, so its
/// `until` is still the farthest any row asked for.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn link_sources_are_capped_per_link(pool: PgPool) {
    let parent = "https://api.btcmap.org/v4/events";
    store::ensure_resource(&pool, parent).await.unwrap();
    let site = "https://example.org/shared";
    let base = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 86400, 0).unwrap();
    store::derive_resource_within(&pool, parent, site, "btcmap", 100, 100, Some(base), None)
        .await
        .unwrap()
        .unwrap();
    let rows: Vec<_> = (0..100)
        .map(|n| {
            (
                format!("event{n}"),
                site.to_string(),
                base + chrono::Duration::hours(n),
            )
        })
        .collect();
    store::link_sources(&pool, parent, &rows, &[])
        .await
        .unwrap();
    let (count, until): (i64, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM jsonb_object_keys(sources)),until FROM resource_derivation",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 64);
    assert_eq!(until, base + chrono::Duration::hours(99));
}

/// Which BTC Map row names which link. A tombstoned row, and a row that
/// moves to another website, stop extending the link they named; a website shared by two
/// events keeps the farther instant when only the nearer event is edited.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn link_sources_follow_tombstones_relinks_and_shared_sites(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let uri = "https://api.btcmap.org/v4/events";
    store::ensure_resource(&pool, uri).await.unwrap();
    let months = |n: u32| {
        chrono::DateTime::from_timestamp((Utc::now() + chrono::Months::new(n)).timestamp(), 0)
            .unwrap()
    };
    let (far, near) = (months(30), months(2));
    let row = |id: u32, at: chrono::DateTime<Utc>, site: &str| json!({"id":id,"status":"live","starts_at":at.to_rfc3339(),"name":format!("E{id}"),"website":site,"updated_at":"2026-10-15T13:00:00Z"});
    let batch = |rows: Value| {
        let (egress, _) = mock_egress(vec![(200, rows)]);
        let (pool, cfg) = (pool.clone(), cfg.clone());
        async move {
            platform(&pool, cfg, egress).resolve(uri).await.unwrap();
        }
    };
    let link = |site: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<chrono::DateTime<Utc>>>(
                "SELECT d.until FROM resource_derivation d JOIN resource r ON r.id=d.child_id WHERE r.uri=$1",
            )
            .bind(site)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    // One website, a far and a near event; then only the near one is edited.
    batch(json!([
        row(1, far, "https://example.org/shared"),
        row(2, near, "https://example.org/shared")
    ]))
    .await;
    assert_eq!(link("https://example.org/shared").await, Some(far));
    batch(json!([row(2, near, "https://example.org/shared")])).await;
    assert_eq!(
        link("https://example.org/shared").await,
        Some(far),
        "the far instant stays"
    );
    // And so does its expiry: capped at the horizon, never cut to the near event.
    let expires: chrono::DateTime<Utc> = sqlx::query_scalar(
        "SELECT d.expires_at FROM resource_derivation d JOIN resource r ON r.id=d.child_id WHERE r.uri='https://example.org/shared'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(expires > near, "{expires} > {near}");
    // The far event is cancelled; the shared site keeps the near event's instant.
    batch(json!([{"id":1,"status":"deleted","updated_at":"2026-10-15T14:00:00Z"}])).await;
    assert_eq!(
        link("https://example.org/shared").await,
        Some(near),
        "a tombstone stops naming the link"
    );
    // A far event moves to another website; the old one is no longer extended.
    batch(json!([row(3, far, "https://example.org/old")])).await;
    assert_eq!(link("https://example.org/old").await, Some(far));
    batch(json!([row(3, far, "https://example.org/new")])).await;
    assert_eq!(
        link("https://example.org/old").await,
        None,
        "a re-linked row leaves its old link"
    );
    assert_eq!(link("https://example.org/new").await, Some(far));
}

/// An event page inherits its single-event calendar file's UID (the corroborated mode's
/// evidence) only when the file is on the page's own origin, not on a host a redirect took the
/// page to.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_page_inherits_an_ics_uid_only_from_its_own_origin(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let page = |name: &str| {
        format!(
            r#"<html><head><title>{name}</title><script type="application/ld+json">{{"@context":"https://schema.org","@type":"Event","name":"{name}","startDate":"2027-03-01T19:00:00"}}</script></head><body><a href="/{name}.ics">Add to calendar</a></body></html>"#
        )
    };
    let ics = |uid: &str| {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART;TZID=Europe/Zurich:20270301T190000\r\nSUMMARY:Talk\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
    };
    let r = html_platform(
        &pool,
        &[
            ("own.example/show", page("show")),
            ("own.example/show.ics", ics("show-uid")),
            (
                "moved.example/talk",
                "REDIRECT https://landing.example/talk".into(),
            ),
            ("landing.example/talk", page("talk")),
            ("landing.example/talk.ics", ics("talk-uid")),
        ],
    );
    let uids = |d: &ResourceMeta| {
        d.group_keys
            .iter()
            .filter(|k| k.basis == "ics_uid")
            .map(|k| k.key.clone())
            .collect::<Vec<_>>()
    };
    let own = r.resolve("https://own.example/show").await.unwrap();
    assert_eq!(uids(&own), ["ics_uid:show-uid"], "{:?}", own.group_keys);
    let moved = r.resolve("https://moved.example/talk").await.unwrap();
    assert!(uids(&moved).is_empty(), "{:?}", moved.group_keys);
}
