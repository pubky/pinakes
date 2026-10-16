use chrono::Utc;
use pinakes_core::{
    config::{IngestConfig, QueueConfig},
    ingest,
    model::{identity, ResourceMeta, Status},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    queue,
    resolver::Resolver,
    store,
    telemetry::Metrics,
};
use sqlx::PgPool;
use std::time::Duration;

fn author(seed: &str) -> String {
    let mut bytes = [0; 52];
    blake3::Hasher::new()
        .update(seed.as_bytes())
        .finalize_xof()
        .fill(&mut bytes);
    bytes
        .iter()
        .map(|b| b"ybndrfg8ejkmcpqxot1uwisza345h769"[(b & 31) as usize] as char)
        .collect()
}
fn change(uri: &str, author_seed: &str, path: &str) -> Change {
    let (resource_id, uri, _) = identity(uri).unwrap();
    Change {
        op: Op::Put,
        resource_id,
        uri,
        kind: Kind::Tag,
        author: author(author_seed),
        object_path: path.into(),
        label: Some("rust".into()),
        at: Utc::now().timestamp_millis(),
        recurrence_id: None,
        status: None,
        start: None,
        end: None,
    }
}
async fn batch(pool: &PgPool, lines: Vec<Change>, epoch: Option<&str>) -> bool {
    let saved = ingest::cursor(pool, "fixture").await.unwrap();
    let next = saved.cursor + lines.len() as i64;
    ingest::apply(
        pool,
        "fixture",
        &saved,
        &FeedPage {
            page: Changes {
                lines,
                next_cursor: next,
            },
            epoch: epoch.map(str::to_owned),
            length: next,
        },
        &IngestConfig::default(),
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap()
}
async fn refs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM reference")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn put_delete_replay_retarget_and_grace(pool: PgPool) {
    let a = change("https://example.org/a", "author-a", "/pub/app/tags/shared");
    let mut b = a.clone();
    b.author = author("author-b");
    assert!(batch(&pool, vec![a.clone(), a.clone(), b.clone()], Some("one")).await);
    assert_eq!(refs(&pool).await, 2);
    let score: f64 = sqlx::query_scalar("SELECT score FROM resource_score")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!((score - 3.0_f64.ln()).abs() < 0.0001);
    let mut edit = change("https://example.org/b", "author-a", &a.object_path);
    edit.at = a.at - 100000;
    batch(&pool, vec![edit.clone()], Some("one")).await;
    assert_eq!(refs(&pool).await, 2);
    let current: String = sqlx::query_scalar("SELECT resource_id FROM reference WHERE author=$1")
        .bind(author("author-a"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(current, edit.resource_id);
    // Delete keys, not target IDs; edits and replays need not be timestamp ordered.
    let mut del = a.clone();
    del.op = Op::Del;
    batch(&pool, vec![del.clone(), del], None).await;
    assert_eq!(refs(&pool).await, 1);
    assert_eq!(
        ingest::cursor(&pool, "fixture")
            .await
            .unwrap()
            .epoch
            .as_deref(),
        Some("one")
    );
    assert_eq!(store::cleanup(&pool, 7).await.unwrap(), 0);
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '8 days' WHERE id=$1")
        .bind(&edit.resource_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(store::cleanup(&pool, 7).await.unwrap(), 1);
    assert_eq!(refs(&pool).await, 1);
}
#[sqlx::test(migrations = "../../migrations")]
async fn epoch_headers_length_reset_and_transaction_rollback(pool: PgPool) {
    let limits = IngestConfig::default();
    let metrics = Metrics::new().unwrap();
    let a = change("https://example.org/a", "author-a", "/pub/app/tags/a");
    batch(&pool, vec![a.clone()], None).await;
    // First appearance of an epoch is not a reset.
    assert!(batch(&pool, vec![a.clone()], Some("first")).await);
    assert_eq!(refs(&pool).await, 1);
    assert!(!batch(&pool, vec![a.clone()], Some("second")).await);
    assert_eq!(refs(&pool).await, 0);
    assert_eq!(ingest::cursor(&pool, "fixture").await.unwrap().cursor, 0);
    batch(&pool, vec![a.clone()], Some("second")).await;
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    let empty = FeedPage {
        page: Changes {
            lines: vec![],
            next_cursor: saved.cursor,
        },
        epoch: None,
        length: 0,
    };
    assert!(
        !ingest::apply(&pool, "fixture", &saved, &empty, &limits, &metrics)
            .await
            .unwrap()
    );
    assert_eq!(refs(&pool).await, 0);
    assert_eq!(
        ingest::cursor(&pool, "fixture")
            .await
            .unwrap()
            .epoch
            .as_deref(),
        Some("second")
    );
    let mut bad = a.clone();
    bad.resource_id.clear(); // Identity disagreement is a transactional feed error.
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    let invalid = FeedPage {
        page: Changes {
            lines: vec![a.clone(), bad],
            next_cursor: 2,
        },
        epoch: None,
        length: 2,
    };
    assert!(
        ingest::apply(&pool, "fixture", &saved, &invalid, &limits, &metrics)
            .await
            .is_err()
    );
    assert_eq!(refs(&pool).await, 0);
    assert_eq!(ingest::cursor(&pool, "fixture").await.unwrap().cursor, 0);
    batch(&pool, vec![a], None).await;
    assert!(
        !ingest::apply(&pool, "fixture", &saved, &invalid, &limits, &metrics)
            .await
            .unwrap()
    );
}
#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_pollers_only_commit_a_batch_once(pool: PgPool) {
    let limits = IngestConfig::default();
    let metrics = Metrics::new().unwrap();
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    let page = FeedPage {
        page: Changes {
            lines: vec![change("https://example.org", "a", "/pub/a")],
            next_cursor: 1,
        },
        epoch: Some("a".into()),
        length: 1,
    };
    let (a, b) = tokio::join!(
        ingest::apply(&pool, "fixture", &saved, &page, &limits, &metrics),
        ingest::apply(&pool, "fixture", &saved, &page, &limits, &metrics)
    );
    assert_ne!(a.unwrap(), b.unwrap());
    assert_eq!(refs(&pool).await, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn rejects_oversized_feed_fields_without_stalling_or_truncating_keys(pool: PgPool) {
    let limits = IngestConfig {
        max_label_bytes: 6,
        max_object_path_bytes: 32,
    };
    let metrics = Metrics::new().unwrap();
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    let mut good = change(
        "https://example.org/kept",
        "a",
        &format!("/pub/app/{}", "p".repeat(23)),
    );
    good.label = Some("ééé".into()); // Six bytes, exactly the configured limit.
    let mut lines = vec![good.clone()];
    for invalid in [
        "y".repeat(51),
        "y".repeat(53),
        "Y".repeat(52),
        "0".repeat(52),
    ] {
        let mut bad = good.clone();
        bad.author = invalid;
        lines.push(bad);
    }
    for label in ["éééé", "", "a\0b"] {
        let mut bad = good.clone();
        bad.label = Some(label.into());
        lines.push(bad);
    }
    for path in [
        format!("{}x", good.object_path),
        "".into(),
        "/pub/\0".into(),
    ] {
        let mut bad = good.clone();
        bad.op = Op::Del; // Truncating the oversized key would delete the good reference.
        bad.object_path = path;
        lines.push(bad);
    }
    let end = lines.len() as i64;
    let page = FeedPage {
        page: Changes {
            lines,
            next_cursor: end,
        },
        epoch: None,
        length: end,
    };
    assert!(
        ingest::apply(&pool, "fixture", &saved, &page, &limits, &metrics)
            .await
            .unwrap()
    );
    assert_eq!(ingest::cursor(&pool, "fixture").await.unwrap().cursor, end);
    assert_eq!(refs(&pool).await, 1);
    let stored: (String, String) = sqlx::query_as("SELECT label, object_path FROM reference")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, (good.label.unwrap(), good.object_path));
    for (reason, count) in [("author", 4), ("label", 3), ("object_path", 3)] {
        assert_eq!(
            metrics.ingest_rejected.with_label_values(&[reason]).get(),
            count
        );
    }
    assert!(
        !ingest::apply(&pool, "fixture", &saved, &page, &limits, &metrics)
            .await
            .unwrap()
    );
    assert_eq!(
        metrics.ingest_rejected.with_label_values(&["author"]).get(),
        4
    );
    assert!(metrics
        .encode()
        .unwrap()
        .contains("pinakes_ingest_rejected_total{reason=\"object_path\"} 3"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn rejected_counts_follow_commit_and_concurrent_cursor_fencing(pool: PgPool) {
    let limits = IngestConfig::default();
    let metrics = Metrics::new().unwrap();
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    let mut rejected = change("https://example.org/rejected", "a", "/pub/a");
    rejected.author = "invalid".into();
    let mut invalid = change("https://example.org/rollback", "a", "/pub/b");
    invalid.resource_id.clear(); // Rejected-field metrics must not survive a rollback.
    let mut page = FeedPage {
        page: Changes {
            lines: vec![rejected, invalid],
            next_cursor: 2,
        },
        epoch: None,
        length: 2,
    };
    assert!(
        ingest::apply(&pool, "fixture", &saved, &page, &limits, &metrics)
            .await
            .is_err()
    );
    assert_eq!(
        metrics.ingest_rejected.with_label_values(&["author"]).get(),
        0
    );
    assert_eq!(ingest::cursor(&pool, "fixture").await.unwrap().cursor, 0);
    page.page.lines.pop();
    page.page.next_cursor = 1;
    page.length = 1;
    let (a, b) = tokio::join!(
        ingest::apply(&pool, "fixture", &saved, &page, &limits, &metrics),
        ingest::apply(&pool, "fixture", &saved, &page, &limits, &metrics)
    );
    assert_ne!(a.unwrap(), b.unwrap());
    assert_eq!(
        metrics.ingest_rejected.with_label_values(&["author"]).get(),
        1
    );
    assert_eq!(ingest::cursor(&pool, "fixture").await.unwrap().cursor, 1);
    assert_eq!(refs(&pool).await, 0);
    let resources: i64 = sqlx::query_scalar("SELECT count(*) FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(resources, 0);
}
struct FakeResolver {
    fail: bool,
}
#[async_trait::async_trait]
impl Resolver for FakeResolver {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        if self.fail {
            anyhow::bail!("backend unavailable");
        }
        let mut doc = ResourceMeta::new(uri, "web", 100);
        doc.name = Some("Resolved once".into());
        Ok(doc)
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(100)
    }
}
#[sqlx::test(migrations = "../../migrations")]
async fn durable_queue_backoff_dead_letter_refresh_and_fencing(pool: PgPool) {
    let cfg = QueueConfig {
        max_failures: 2,
        backoff_secs: 2,
        max_backoff_secs: 10,
        ..Default::default()
    };
    let id = store::ensure_resource(&pool, "https://example.org/")
        .await
        .unwrap();
    let first = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    assert!(queue::claim(&pool, "web", &cfg).await.unwrap().is_none());
    queue::run_job(&pool, &FakeResolver { fail: true }, &cfg, &first)
        .await
        .unwrap();
    assert!(queue::claim(&pool, "web", &cfg).await.unwrap().is_none());
    let seconds: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at-now())::float8 FROM resource",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(seconds > 1.0 && seconds <= 2.0);
    sqlx::query("UPDATE resource SET next_attempt_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let second = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    queue::run_job(&pool, &FakeResolver { fail: true }, &cfg, &second)
        .await
        .unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT dead_letter FROM resource")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    // Repeated preview requests cannot revive dead letters or reset backoff.
    store::ensure_resource(&pool, "https://example.org/#fragment")
        .await
        .unwrap();
    assert!(queue::claim(&pool, "web", &cfg).await.unwrap().is_none());
    assert_eq!(queue::backoff(&cfg, 1), 2);
    assert_eq!(queue::backoff(&cfg, 2), 4);
    assert_eq!(queue::backoff(&cfg, 99), 10);
    sqlx::query("UPDATE resource SET dead_letter=false,next_attempt_at=now(),attempts=0")
        .execute(&pool)
        .await
        .unwrap();
    let third = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    // A worker completing an expired lease cannot overwrite a newer worker's result.
    queue::run_job(&pool, &FakeResolver { fail: false }, &cfg, &first)
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "error");
    queue::run_job(&pool, &FakeResolver { fail: false }, &cfg, &third)
        .await
        .unwrap();
    assert!(queue::claim(&pool, "web", &cfg).await.unwrap().is_none());
    sqlx::query("UPDATE resource SET next_attempt_at=now()-interval '1 second',refresh_at=now()-interval '1 second'").execute(&pool).await.unwrap();
    assert_eq!(
        queue::claim(&pool, "web", &cfg).await.unwrap().unwrap().id,
        id
    );
}
#[sqlx::test(migrations = "../../migrations")]
async fn documents_promote_spatial_time_type_and_category_columns(pool: PgPool) {
    let id = store::ensure_resource(&pool, "https://example.org/")
        .await
        .unwrap();
    let cfg = QueueConfig::default();
    let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    let mut doc = ResourceMeta::new("https://example.org/", "web", 60);
    doc.set_type("MusicEvent");
    doc.location = Some(pinakes_core::model::Point {
        lat: 46.0,
        lon: 8.0,
    });
    doc.start = Some(Utc::now());
    doc.category = Some("music".into());
    store::save_document(&pool, &id, job.generation, &doc)
        .await
        .unwrap();
    let (lat, lon, ty, cat): (f64, f64, String, String) =
        sqlx::query_as("SELECT ST_Y(point),ST_X(point),type,category FROM resource")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((lat, lon), (46.0, 8.0));
    assert_eq!(ty, "MusicEvent");
    assert_eq!(cat, "music");
    assert_eq!(doc.status, Status::Ok);
    let value: bool = sqlx::query_scalar(
        "SELECT type_path @> ARRAY['Event'] AND start_at IS NOT NULL FROM resource",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(value);
}

struct MissingResolver;
#[async_trait::async_trait]
impl Resolver for MissingResolver {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        let mut doc = ResourceMeta::new(uri, "web", 86400);
        doc.status = Status::Missing;
        Ok(doc)
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(86400)
    }
}
#[sqlx::test(migrations = "../../migrations")]
async fn missing_responses_get_short_refresh_without_losing_references(pool: PgPool) {
    let a = change("https://example.org/missing", "a", "/pub/app/tags/a");
    batch(&pool, vec![a], Some("one")).await;
    let cfg = QueueConfig {
        missing_ttl_secs: 60,
        ..Default::default()
    };
    let (a, b) = tokio::join!(
        queue::claim(&pool, "web", &cfg),
        queue::claim(&pool, "web", &cfg)
    );
    let jobs: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(jobs.len(), 1);
    queue::run_job(&pool, &MissingResolver, &cfg, &jobs[0])
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    tokio::time::sleep(Duration::from_millis(2)).await;
    sqlx::query("UPDATE resource SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let next = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    queue::run_job(&pool, &MissingResolver, &cfg, &next)
        .await
        .unwrap();
    let (status,ttl,seconds):(String,i32,f64)=sqlx::query_as("SELECT status,(doc->>'ttl_secs')::int,extract(epoch FROM refresh_at-now())::float8 FROM resource").fetch_one(&pool).await.unwrap();
    assert_eq!(status, "missing");
    assert_eq!(ttl, 3600);
    assert!(seconds > 3590.0 && seconds <= 3600.0);
    assert_eq!(refs(&pool).await, 1);
}
#[sqlx::test(migrations = "../../migrations")]
async fn author_budgets_defer_without_counting_a_failure(pool: PgPool) {
    let a = change("https://example.org/one", "a", "/pub/app/tags/a");
    let b = change("https://example.org/two", "a", "/pub/app/tags/b");
    batch(&pool, vec![a, b], Some("one")).await;
    let cfg = QueueConfig::default();
    let budget = queue::AuthorBudget::default();
    let first = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    let second = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    assert!(budget.admit(&pool, &first, 0.1).await.unwrap());
    assert!(!budget.admit(&pool, &second, 0.1).await.unwrap());
    let (leased, attempts): (bool, i32) =
        sqlx::query_as("SELECT lease_until IS NOT NULL,attempts FROM resource WHERE id=$1")
            .bind(&second.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!leased);
    assert_eq!(attempts, 0);
    assert!(queue::claim(&pool, "web", &cfg).await.unwrap().is_none());
    // Another public author can sponsor this same shared resource.
    let other = change(&second.uri, "b", "/pub/app/tags/c");
    batch(&pool, vec![other], Some("one")).await;
    assert!(budget.admit(&pool, &second, 0.1).await.unwrap());
}

#[sqlx::test(migrations = "../../migrations")]
async fn new_reference_reopens_final_pages_but_never_geo(pool: PgPool) {
    let page = change("https://example.org/final", "a", "/pub/app/tags/a");
    let geo = change("geo:46,8", "b", "/pub/app/tags/b");
    batch(&pool, vec![page.clone(), geo.clone()], Some("one")).await;
    sqlx::query("UPDATE resource SET final=true,resolved_at=now()-interval '1 day',next_attempt_at='infinity'")
        .execute(&pool)
        .await
        .unwrap();
    let mut page2 = page;
    page2.object_path = "/pub/app/tags/new-page".into();
    let mut geo2 = geo;
    geo2.object_path = "/pub/app/tags/new-geo".into();
    batch(&pool, vec![page2, geo2], Some("one")).await;
    let final_geo: bool = sqlx::query_scalar("SELECT final FROM resource WHERE scheme='geo'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let final_page: bool = sqlx::query_scalar("SELECT final FROM resource WHERE scheme='https'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(final_geo);
    assert!(!final_page);
}

#[sqlx::test(migrations = "../../migrations")]
async fn epoch_replay_does_not_reopen_final_resource(pool: PgPool) {
    let old = change("https://example.org/finished", "a", "/pub/app/old");
    batch(&pool, vec![old.clone()], Some("before")).await;
    sqlx::query("UPDATE resource SET final=true,resolved_at=now(),next_attempt_at='infinity'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!batch(&pool, vec![old.clone()], Some("after")).await);
    batch(&pool, vec![old.clone()], Some("after")).await;
    assert!(sqlx::query_scalar::<_, bool>("SELECT final FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap());
    let mut new = old;
    new.object_path = "/pub/app/new".into();
    new.at = Utc::now().timestamp_millis() + 10;
    batch(&pool, vec![new], Some("after")).await;
    assert!(!sqlx::query_scalar::<_, bool>("SELECT final FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap());
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalid_event_dates_do_not_stall_feed_and_unknown_status_is_accepted(pool: PgPool) {
    let mut good = change("geo:47,8", "event-author", "/pub/events/1");
    good.kind = Kind::Event;
    good.start = Some(Utc::now().timestamp_millis());
    good.end = good.start;
    good.status = Some("upstream-new-status".into());
    let mut huge = good.clone();
    huge.object_path = "/pub/events/2".into();
    huge.start = Some(i64::MAX);
    let mut reversed = good.clone();
    reversed.object_path = "/pub/events/3".into();
    reversed.end = Some(good.start.unwrap() - 1);
    assert!(batch(&pool, vec![huge, reversed, good], Some("events")).await);
    assert_eq!(refs(&pool).await, 1);
    assert_eq!(ingest::cursor(&pool, "fixture").await.unwrap().cursor, 3);
    assert!(
        sqlx::query_scalar::<_, Option<String>>("SELECT event_status FROM reference")
            .fetch_one(&pool)
            .await
            .unwrap()
            .is_none()
    );
}
struct PanicOnce(std::sync::atomic::AtomicBool);
#[async_trait::async_trait]
impl Resolver for PanicOnce {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        assert!(
            !self.0.swap(false, std::sync::atomic::Ordering::SeqCst),
            "hostile parser panic"
        );
        Ok(ResourceMeta::new(uri, "fixture", 3600))
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(3600)
    }
}
#[sqlx::test(migrations = "../../migrations")]
async fn resolver_panic_is_a_failed_job_and_next_batch_can_run(pool: PgPool) {
    let id = store::ensure_resource(&pool, "https://example.org/poison")
        .await
        .unwrap();
    let resolver = PanicOnce(std::sync::atomic::AtomicBool::new(true));
    let job = queue::Job {
        id: id.clone(),
        uri: "https://example.org/poison".into(),
        generation: 0,
        attempts: 0,
    };
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &[job])
        .await
        .unwrap();
    let id = store::ensure_resource(&pool, "https://example.org/healthy")
        .await
        .unwrap();
    let job = queue::Job {
        id: id.clone(),
        uri: "https://example.org/healthy".into(),
        generation: 0,
        attempts: 0,
    };
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &[job])
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "ok"
    );
}

struct FinishPanic;
#[async_trait::async_trait]
impl Resolver for FinishPanic {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        Ok(ResourceMeta::new(uri, "fixture", 3600))
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        assert!(!doc.url.ends_with("/poison"), "completion panic");
        Duration::from_secs(3600)
    }
}
#[sqlx::test(migrations = "../../migrations")]
async fn finish_panic_fails_one_job_and_same_worker_finishes_rest(pool: PgPool) {
    let mut jobs = vec![];
    for path in ["poison", "healthy"] {
        let uri = format!("https://example.org/{path}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        jobs.push(queue::Job {
            id,
            uri,
            generation: 0,
            attempts: 0,
        });
    }
    queue::run_batch(&pool, &FinishPanic, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    let first: (String, i32, Option<String>) =
        sqlx::query_as("SELECT status,attempts,last_error FROM resource WHERE id=$1")
            .bind(&jobs[0].id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(first, ("error".into(), 1, Some("parse".into())));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM resource WHERE id=$1")
            .bind(&jobs[1].id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "ok"
    );
    queue::run_job(&pool, &FinishPanic, &QueueConfig::default(), &jobs[0])
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalid_non_event_feed_clock_has_its_own_rejection_reason(pool: PgPool) {
    let mut line = change(
        "https://example.org/clock",
        "fixture",
        "/pub/app/tags/clock",
    );
    line.at = i64::MAX;
    let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
    let saved = ingest::cursor(&pool, "clock-fixture").await.unwrap();
    let page = FeedPage {
        page: pinakes_core::nexus::Changes {
            lines: vec![line],
            next_cursor: 1,
        },
        length: 1,
        epoch: Some("one".into()),
    };
    ingest::apply(
        &pool,
        "clock-fixture",
        &saved,
        &page,
        &Default::default(),
        &metrics,
    )
    .await
    .unwrap();
    assert_eq!(
        metrics
            .ingest_rejected
            .with_label_values(&["timestamp"])
            .get(),
        1
    );
    assert_eq!(
        metrics.ingest_rejected.with_label_values(&["event"]).get(),
        0
    );
    assert_eq!(
        ingest::cursor(&pool, "clock-fixture").await.unwrap().cursor,
        1
    );
}
