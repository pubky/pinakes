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
    let (_, uri, _) = identity(uri).unwrap();
    Change {
        op: Op::Put,
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

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
    assert_eq!(current, identity(&edit.uri).unwrap().0);
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
        .bind(identity(&edit.uri).unwrap().0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(store::cleanup(&pool, 7).await.unwrap(), 1);
    assert_eq!(refs(&pool).await, 1);
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
    // Nexus's own ID is not read: a line naming another one is keyed by the ID of its URI.
    let mut foreign = serde_json::to_value(&a).unwrap();
    foreign["resource_id"] = serde_json::json!("0".repeat(32));
    foreign["object_path"] = serde_json::json!("/pub/app/tags/foreign");
    let foreign: Change = serde_json::from_value(foreign).unwrap();
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    // An invalid URI is rejected without stalling the batch.
    let mut bad_uri = a.clone();
    bad_uri.uri = "no scheme".into();
    let invalid = FeedPage {
        page: Changes {
            lines: vec![a.clone(), foreign, bad_uri],
            next_cursor: 3,
        },
        epoch: None,
        length: 3,
    };
    assert!(
        ingest::apply(&pool, "fixture", &saved, &invalid, &limits, &metrics)
            .await
            .unwrap()
    );
    assert_eq!(refs(&pool).await, 2);
    let keyed: Vec<String> = sqlx::query_scalar("SELECT DISTINCT resource_id FROM reference")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(keyed, [identity(&a.uri).unwrap().0]);
    assert_eq!(ingest::cursor(&pool, "fixture").await.unwrap().cursor, 3);
    assert_eq!(metrics.ingest_rejected.with_label_values(&["uri"]).get(), 1);
    assert!(
        !ingest::apply(&pool, "fixture", &saved, &invalid, &limits, &metrics)
            .await
            .unwrap()
    );
    assert_eq!(metrics.ingest_rejected.with_label_values(&["uri"]).get(), 1);
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn rejected_counts_follow_commit_and_concurrent_cursor_fencing(pool: PgPool) {
    let limits = IngestConfig::default();
    let metrics = Metrics::new().unwrap();
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    let mut rejected = change("https://example.org/rejected", "a", "/pub/a");
    rejected.author = "invalid".into();
    let invalid = change("https://example.org/rollback", "a", "/pub/b");
    // Force an actual storage failure after the rejected line: an identity failure is skipped,
    // not rolled back.
    sqlx::query("ALTER TABLE resource ADD CONSTRAINT rollback_fixture CHECK(uri<>'https://example.org/rollback')")
        .execute(&pool).await.unwrap();
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
struct DeferredResolver(&'static str);
#[async_trait::async_trait]
impl Resolver for DeferredResolver {
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(60)
    }
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        Err(pinakes_core::resolver::RetryLater {
            class: self.0,
            seconds: 60,
        }
        .into())
    }
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn timeout_escalation_is_durable_fenced_and_resets(pool: PgPool) {
    let cfg = QueueConfig {
        timeout_grace: 3,
        max_failures: 2,
        backoff_secs: 30,
        max_backoff_secs: 240,
        ..Default::default()
    };
    let uri = "https://example.org/tarpit";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let accepted = ResourceMeta::new(uri, "web", 3600);
    store::save_document(&pool, &id, 0, &accepted)
        .await
        .unwrap();
    let before: serde_json::Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut old = None;
    for (streak, attempts, delay) in [
        (1, 0, 60),
        (2, 0, 120),
        (3, 0, 240),
        (4, 1, 240),
        (5, 2, 240),
    ] {
        sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
        queue::run_job(&pool, &DeferredResolver("timeout"), &cfg, &job)
            .await
            .unwrap();
        if let Some(old) = old.as_ref() {
            queue::run_job(&pool, &DeferredResolver("timeout"), &cfg, old)
                .await
                .unwrap();
        }
        old = Some(job);
        let row: (i32,i32,bool,f64,serde_json::Value) = sqlx::query_as("SELECT timeout_streak,attempts,dead_letter,extract(epoch FROM next_attempt_at-now())::float8,doc FROM resource WHERE id=$1").bind(&id).fetch_one(&pool).await.unwrap();
        assert_eq!((row.0, row.1, row.2), (streak, attempts, attempts == 2));
        assert!(
            row.3 <= delay as f64 && row.3 > delay as f64 - 2.0,
            "{row:?}"
        );
        assert_eq!(row.4, before);
    }
    assert!(queue::claim(&pool, "web", &cfg).await.unwrap().is_none());
    let uri = "https://example.org/recovery";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    for (kind, expected) in [
        ("timeout", 1),
        ("http_5xx", 0),
        ("timeout", 1),
        ("success", 0),
        ("timeout", 1),
        ("parse", 0),
        ("timeout", 1),
    ] {
        sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
        if matches!(kind, "success" | "parse") {
            queue::run_job(
                &pool,
                &FakeResolver {
                    fail: kind == "parse",
                },
                &cfg,
                &job,
            )
            .await
            .unwrap();
        } else {
            queue::run_job(&pool, &DeferredResolver(kind), &cfg, &job)
                .await
                .unwrap();
        }
        let actual: i32 = sqlx::query_scalar("SELECT timeout_streak FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(actual, expected, "{kind}");
    }
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
/// A resolver that answers an upstream failure with the given HTTP status, as the web resolver
/// does for a 5xx, a 429 or another error status.
struct FailingResolver(u16);
#[async_trait::async_trait]
impl Resolver for FailingResolver {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        let mut doc = ResourceMeta::new(uri, "web", 86400);
        doc.status = Status::Error;
        doc.http_status = Some(self.0);
        Ok(doc)
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(86400)
    }
}
/// An upstream failure is recorded by its HTTP status (`http_5xx`, `http_4xx`), never as a parser
/// failure: on stable, three sites that answered errors for ten minutes were dead-lettered as
/// `parse`.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_upstream_failure_is_recorded_by_its_status(pool: PgPool) {
    let a = change("https://example.org/flaky", "a", "/pub/app/tags/a");
    batch(&pool, vec![a], Some("one")).await;
    let cfg = QueueConfig::default();
    for (status, class) in [(503, "http_5xx"), (429, "http_4xx")] {
        sqlx::query("UPDATE resource SET next_attempt_at=now()")
            .execute(&pool)
            .await
            .unwrap();
        let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
        let _ = queue::run_job(&pool, &FailingResolver(status), &cfg, &job).await;
        let error: Option<String> = sqlx::query_scalar("SELECT last_error FROM resource")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(error.as_deref(), Some(class), "{status}");
    }
}
#[derive(Clone, Default)]
struct Lines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// The JSON log lines down to debug, from a process-wide subscriber: a thread-local one misses events
/// while other tests run (the callsites' interest is cached for the whole process). A test finds
/// its lines by its resource.
fn log_lines() -> Lines {
    static LINES: std::sync::OnceLock<Lines> = std::sync::OnceLock::new();
    LINES
        .get_or_init(|| {
            let lines = Lines::default();
            let writer = lines.clone();
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
            lines
        })
        .clone()
}
/// A resolver whose fetch fails as reqwest reports it outside the egress: the error names the
/// URL, query and all.
struct UnreachableResolver;
#[async_trait::async_trait]
impl Resolver for UnreachableResolver {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = reqwest::get(format!("http://127.0.0.1:{port}/calendar.ics?token=CANARY"))
            .await
            .unwrap_err();
        Err(error.into())
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(86400)
    }
}
/// A failure's logged error never carries a URL's query: the canary token of a refused
/// connection stays out of the line.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_failure_log_keeps_no_query_secret(pool: PgPool) {
    let lines = log_lines();
    let a = change(
        "https://cal.example.org/share?token=CANARY",
        "a",
        "/pub/app/tags/a",
    );
    batch(&pool, vec![a], Some("one")).await;
    let cfg = QueueConfig::default();
    let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    let _ = queue::run_job(&pool, &UnreachableResolver, &cfg, &job).await;
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let line = text
        .lines()
        .find(|l| l.contains("resolution failed") && l.contains(job.id.as_str()))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("connect"), "{line}");
    assert!(!line.contains("CANARY"), "{line}");
}

/// A resolver whose page is too complex to parse.
struct TooComplexResolver;
#[async_trait::async_trait]
impl Resolver for TooComplexResolver {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        Err(pinakes_core::resolver::html::TooComplex {
            reason: "the CPU budget",
        }
        .into())
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(86400)
    }
}
/// A resource whose refresh is too complex to parse loses its card: no title, no description,
/// status `error`, final with `too_complex`, and the change is recorded. Before, the old card
/// stayed (Codex on 58ed270).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_too_complex_refresh_clears_the_card(pool: PgPool) {
    let uri = "https://complex.example.org/page";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut card = ResourceMeta::new(uri, "web", 3600);
    card.name = Some("Old title".into());
    card.description = Some("Old description".into());
    store::save_document(&pool, &id, 0, &card).await.unwrap();
    let changes_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("UPDATE resource SET next_attempt_at=now()-interval '1 second' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let cfg = QueueConfig::default();
    let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    assert_eq!(job.id, id);
    queue::run_job(&pool, &TooComplexResolver, &cfg, &job)
        .await
        .unwrap();
    let (status, last_error, is_final, name, doc): (
        String,
        Option<String>,
        bool,
        Option<String>,
        serde_json::Value,
    ) = sqlx::query_as("SELECT status,last_error,final,name,doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "error");
    assert_eq!(last_error.as_deref(), Some("too_complex"));
    assert!(is_final);
    assert_eq!(name, None);
    assert!(
        doc["name"].is_null() && doc["description"].is_null(),
        "{doc}"
    );
    let changes_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(changes_after, changes_before + 1);
    // The public card says so too.
    let app = pinakes_core::api::router(
        pinakes_core::api::AppState::new(pool.clone(), Default::default()).unwrap(),
    );
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::get(format!("/v1/resources/{id}"))
            .extension(axum::extract::ConnectInfo(
                "127.0.0.9:1".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("Old title") && !text.contains("Old description"),
        "{text}"
    );
}

/// A resolver whose error repeats what an upstream answered: two links in one span, the second
/// with user info, a query and a fragment (Codex's case), behind a 503.
struct UpstreamTextResolver;
#[async_trait::async_trait]
impl Resolver for UpstreamTextResolver {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        Err(anyhow::anyhow!(
            "failed [https://first.example/a,HTTPS://user:CANARY@second.example/b?t=CANARY#CANARY]"
        )
        .context(pinakes_core::resolver::Upstream(503)))
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(86400)
    }
}
/// A failure's log line prints the failure's kind, the upstream status and the host, never the
/// error's text: nothing an upstream answered reaches the log, however it is shaped.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_failure_log_prints_no_upstream_text(pool: PgPool) {
    let lines = log_lines();
    let a = change("https://site.example.org/page", "a", "/pub/app/tags/a");
    batch(&pool, vec![a], Some("one")).await;
    let cfg = QueueConfig::default();
    let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    let _ = queue::run_job(&pool, &UpstreamTextResolver, &cfg, &job).await;
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let line = text
        .lines()
        .find(|l| l.contains("resolution failed") && l.contains(job.id.as_str()))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("\"host\":\"site.example.org\""), "{line}");
    assert!(line.contains("\"status\":\"503\""), "{line}");
    assert!(line.contains("\"kind\":\""), "{line}");
    assert!(
        !line.contains("CANARY") && !line.to_lowercase().contains("http"),
        "{line}"
    );
}

/// A hook that runs out of time is logged by the link's host, never the link: a canary token in
/// the query stays out of the line.
#[tokio::test]
async fn a_hook_timeout_names_no_link() {
    // The feed source's hook reads any host, so it is the one a passed deadline times out;
    // another hook answers a host it does not know at once. Nothing to log without it.
    if pinakes_core::sources::by_name("feed").is_none() {
        return;
    }
    let lines = log_lines();
    let egress = pinakes_core::resolver::egress::Egress::new(Default::default());
    // A deadline already passed: every wall hook times out at once.
    let uri = "https://hooks.example.org/page?token=CANARY";
    let _ =
        pinakes_core::sources::wall_hooks(&egress, uri, 3600, tokio::time::Instant::now()).await;
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let timed_out: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("wall hook timed out") && l.contains("hooks.example.org"))
        .collect();
    assert!(!timed_out.is_empty(), "{text}");
    assert!(!text.contains("CANARY"), "{text}");
}

/// A failed resolve is logged with its resource id, its URL's host and the kind of failure, the
/// class stored in `last_error`: the line can be found by site or by kind.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_failed_resolve_is_logged_with_its_host_and_kind(pool: PgPool) {
    let lines = log_lines();
    let a = change(
        "https://flaky.example.org/page?token=x",
        "a",
        "/pub/app/tags/a",
    );
    batch(&pool, vec![a], Some("one")).await;
    let cfg = QueueConfig::default();
    let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    let _ = queue::run_job(&pool, &FailingResolver(503), &cfg, &job).await;
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let line: serde_json::Value = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|l| {
            l["fields"]["message"] == "resolution failed"
                && l["fields"]["resource_id"] == job.id.as_str()
        })
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(line["fields"]["resource_id"], job.id.as_str());
    assert_eq!(line["fields"]["host"], "flaky.example.org");
    assert_eq!(line["fields"]["kind"], "http_5xx");
    // Never the path or query.
    assert!(!text.contains("token=x"), "{text}");
}
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
    let final_geo: bool = sqlx::query_scalar("SELECT final FROM resource WHERE uri LIKE 'geo:%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let final_page: bool =
        sqlx::query_scalar("SELECT final FROM resource WHERE uri LIKE 'https:%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(final_geo);
    assert!(!final_page);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn denials_expire_to_unreachable_backoff_and_recover(pool: PgPool) {
    let uri = "https://example.org/denied";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let accepted = ResourceMeta::new(uri, "web", 3600);
    store::save_document(&pool, &id, 0, &accepted)
        .await
        .unwrap();
    let cfg = QueueConfig {
        max_failures: 1,
        ..Default::default()
    };
    let mut stale = None;
    for (kind, expired, expected) in [
        ("graphql", false, "ok"),
        ("upstream_denied", true, "unreachable"),
        ("graphql", true, "unreachable"),
    ] {
        sqlx::query("UPDATE resource SET next_attempt_at=now(),denied_since=CASE WHEN $2 THEN now()-interval '4 days' ELSE denied_since END WHERE id=$1")
            .bind(&id).bind(expired).execute(&pool).await.unwrap();
        let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
        queue::run_job(&pool, &DeferredResolver(kind), &cfg, &job)
            .await
            .unwrap();
        if let Some(old) = &stale {
            queue::run_job(&pool, &DeferredResolver(kind), &cfg, old)
                .await
                .unwrap();
        }
        let state: (String, i32, bool, serde_json::Value) =
            sqlx::query_as("SELECT status,denial_streak,dead_letter,doc FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state.0, expected);
        assert_eq!(
            state.1,
            if stale.is_none() {
                1
            } else if kind == "graphql" {
                3
            } else {
                2
            }
        );
        assert!(!state.2);
        assert_eq!(state.3["url"], uri);
        stale = Some(job);
    }
    store::save_document(&pool, &id, stale.unwrap().generation, &accepted)
        .await
        .unwrap();
    let state: (String, i32, bool) = sqlx::query_as(
        "SELECT status,denial_streak,denied_since IS NULL FROM resource WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, ("ok".into(), 0, true));
}
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
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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

/// `pinakesd reextract <route…|all>` re-queues only the selected rows, now, fenced by a
/// generation bump, without their HTTP validators.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reextract_requeues_the_selected_routes_without_validators(pool: PgPool) {
    let web = store::ensure_resource(&pool, "https://example.org/page")
        .await
        .unwrap();
    let geo = store::ensure_resource(&pool, "geo:46.0,8.9").await.unwrap();
    sqlx::query(
        "UPDATE resource SET status='ok',next_attempt_at=now()+interval '30 days',attempts=3",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO event_fetch(resource_id,validators) VALUES($1,'{\"etag\":\"x\"}')")
        .bind(&web)
        .execute(&pool)
        .await
        .unwrap();
    let state = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (bool, i32, i64)>(
                "SELECT next_attempt_at<=now(),attempts,generation FROM resource WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (_, _, before) = state(web.clone()).await;
    assert_eq!(queue::reextract(&pool, &["web".into()]).await.unwrap(), 1);
    assert_eq!(state(web.clone()).await, (true, 0, before + 1));
    assert!(!state(geo.clone()).await.0, "another route is left alone");
    let validators: serde_json::Value =
        sqlx::query_scalar("SELECT validators FROM event_fetch WHERE resource_id=$1")
            .bind(&web)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(validators, serde_json::json!({}));
    assert_eq!(queue::reextract(&pool, &["all".into()]).await.unwrap(), 2);
    assert!(queue::reextract(&pool, &[]).await.is_err());
}

/// Aliases an earlier build made outlive its rules until `reextract` checks them: a LinkedIn
/// `/posts/` link and a Reddit comment's permalink, once aliases of the activity and the post,
/// answer for themselves again and are re-queued, each with an `aliased` line naming itself; a
/// post spelling under a stale root moves to the post's root. A page's canonical and an alias
/// the rules still make stay.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reextract_moves_or_releases_the_aliases_rules_no_longer_make(pool: PgPool) {
    if pinakes_core::sources::by_name("linkedin").is_none()
        || pinakes_core::sources::by_name("reddit").is_none()
    {
        return;
    }
    let post = "https://www.reddit.com/comments/1k3d0pk/";
    let stale = "https://example.org/stale-root";
    let posts = "https://www.linkedin.com/posts/linkedin_activity-7517918134654218241-7OWA";
    let activity = "https://www.linkedin.com/feed/update/urn:li:activity:7517918134654218241";
    let comment = "https://www.reddit.com/r/announcements/comments/1k3d0pk/a_title/abcdefg/";
    let spelling = "https://old.reddit.com/r/announcements/comments/1k3d0pk/a_title/";
    let short = "https://redd.it/1k3d0pk";
    let page = "https://example.org/a/";
    let canonical = "https://example.org/a";
    // A link an official API named as another object's (no rule of its URI names that root).
    let named = "https://example.org/release/1";
    let record = "https://example.org/record/1";
    let mut ids = std::collections::BTreeMap::new();
    for uri in [
        post, stale, posts, activity, comment, spelling, short, page, canonical, named, record,
    ] {
        ids.insert(uri, store::ensure_resource(&pool, uri).await.unwrap());
    }
    // As an earlier build left them: aliases without a route, their roots holding the cards.
    for (alias, root, basis) in [
        (posts, activity, "identifier"),
        (comment, post, "identifier"),
        (spelling, stale, "identifier"),
        (short, post, "identifier"),
        (page, canonical, "canonical"),
        (named, record, "source"),
    ] {
        sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,$3) ON CONFLICT(alias_id) DO UPDATE SET root_id=excluded.root_id,basis=excluded.basis")
            .bind(&ids[alias])
            .bind(&ids[root])
            .bind(basis)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE resource r SET resolver=NULL,status='ok',next_attempt_at=now()+interval '30 days' FROM resource_alias a WHERE a.alias_id=r.id")
        .execute(&pool)
        .await
        .unwrap();
    let root_of = |uri: &str| {
        let (pool, id) = (pool.clone(), ids[uri].clone());
        async move {
            sqlx::query_scalar::<_, String>("SELECT root_id FROM resource_alias WHERE alias_id=$1")
                .bind(id)
                .fetch_optional(&pool)
                .await
                .unwrap()
        }
    };
    // The stale root counts the spelling's reference, as an earlier build scored it.
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','x','/pub/app/tags/stale',now())")
        .bind(&ids[spelling])
        .bind(author("stale"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_score(resource_id,score,updated_at,newest_at) VALUES($1,1,now(),now())")
        .bind(&ids[stale])
        .execute(&pool)
        .await
        .unwrap();
    // A route none of them is on leaves them all.
    queue::reextract(&pool, &["osm".into()]).await.unwrap();
    assert_eq!(root_of(posts).await, Some(ids[activity].clone()));
    assert_eq!(root_of(spelling).await, Some(ids[stale].clone()));

    sqlx::query("DELETE FROM group_dirty")
        .execute(&pool)
        .await
        .unwrap();
    queue::reextract(&pool, &["all".into()]).await.unwrap();
    for uri in [posts, comment] {
        assert_eq!(root_of(uri).await, None, "{uri} answers for itself");
        let (resolver, due): (Option<String>, bool) =
            sqlx::query_as("SELECT resolver,next_attempt_at<=now() FROM resource WHERE id=$1")
                .bind(&ids[uri])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(resolver.is_some() && due, "{uri} is re-queued on a route");
        let root: Option<String> = sqlx::query_scalar(
            "SELECT after->>'root' FROM resource_change WHERE resource_id=$1 AND class='aliased' ORDER BY seq DESC LIMIT 1",
        )
        .bind(&ids[uri])
        .fetch_optional(&pool)
        .await
        .unwrap()
        .flatten();
        assert_eq!(root.as_ref(), Some(&ids[uri]), "{uri}'s line names itself");
    }
    assert_eq!(root_of(spelling).await, Some(ids[post].clone()));
    assert_eq!(root_of(short).await, Some(ids[post].clone()));
    assert_eq!(root_of(page).await, Some(ids[canonical].clone()));
    assert_eq!(
        root_of(named).await,
        Some(ids[record].clone()),
        "an API's answer is no rule of the URI: it stays"
    );
    // The spelling moved: its reference counts for the post, no longer for the stale root.
    let scored = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM resource_score WHERE resource_id=$1)",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert!(
        !scored(ids[stale].clone()).await,
        "the old root is rescored"
    );
    assert!(scored(ids[post].clone()).await);
    let dirty: i64 =
        sqlx::query_scalar("SELECT count(*) FROM group_dirty WHERE resource_id=ANY($1)")
            .bind(vec![ids[posts].clone(), ids[activity].clone()])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(dirty, 2, "the released alias and its old root regroup");
}

/// A spelling whose rule names a link that is itself a stale alias moves to that link once it is
/// released, never to the link's old root, in one batch of `reextract` and across two (the
/// spelling's batch read first). Here, LinkedIn's `/posts/` link and its `?utm_source=` spelling
/// were both aliases of the activity.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reextract_releases_a_link_before_moving_its_spellings(pool: PgPool) {
    if pinakes_core::sources::by_name("linkedin").is_none() {
        return;
    }
    let activity = "https://www.linkedin.com/feed/update/urn:li:activity:7517918134654218241";
    let posts = "https://www.linkedin.com/posts/linkedin_activity-7517918134654218241-7OWA";
    let id = |uri: &str| identity(uri).unwrap().0;
    let (activity_id, posts_id) = (id(activity), id(posts));
    // The share spelling whose ID sorts before the link's.
    let spelling = (0..64)
        .map(|n| format!("{posts}?utm_source=share{n}"))
        .find(|u| id(u) < posts_id)
        .unwrap();
    assert_eq!(
        pinakes_core::alias::root(&spelling)
            .map(|a| a.root)
            .as_deref(),
        Some(posts),
        "the spelling's rule names the link"
    );
    let spelling_id = id(&spelling);
    for uri in [activity, posts, spelling.as_str()] {
        store::ensure_resource(&pool, uri).await.unwrap();
    }
    let root_of = |alias: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT root_id FROM resource_alias WHERE alias_id=$1")
                .bind(alias)
                .fetch_optional(&pool)
                .await
                .unwrap()
        }
    };
    for batches in [1, 2] {
        if batches == 2 {
            // A thousand rows between the two IDs: the spelling's batch ends before the link.
            let low = u128::from_str_radix(&spelling_id, 16).unwrap();
            assert!(u128::from_str_radix(&posts_id, 16).unwrap() - low > 1000);
            let fillers: Vec<String> = (1..=1000u128)
                .map(|n| format!("{:032x}", low + n))
                .collect();
            sqlx::query("INSERT INTO resource(id,uri) SELECT f, 'https://example.org/filler/'||f FROM unnest($1::text[]) f")
                .bind(&fillers)
                .execute(&pool)
                .await
                .unwrap();
        }
        // As an earlier build left them: both aliases of the activity.
        for alias in [&posts_id, &spelling_id] {
            sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'identifier') ON CONFLICT(alias_id) DO UPDATE SET root_id=excluded.root_id,basis=excluded.basis")
                .bind(alias)
                .bind(&activity_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE resource SET resolver=NULL WHERE id=ANY($1)")
            .bind([&posts_id, &spelling_id])
            .execute(&pool)
            .await
            .unwrap();
        queue::reextract(&pool, &["all".into()]).await.unwrap();
        assert_eq!(root_of(posts_id.clone()).await, None, "{batches} batches");
        assert_eq!(
            root_of(spelling_id.clone()).await,
            Some(posts_id.clone()),
            "{batches} batches: the spelling follows the released link"
        );
    }
}

/// `pinakesd` holds the instance lock for its whole life, `reextract` included: no writer waits
/// on it, shared or exclusive, and a second daemon is refused.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn writers_never_wait_on_the_instance_lock(pool: PgPool) {
    let _instance = pinakes_core::locks::own_database(&pool).await.unwrap();
    let refused = pinakes_core::locks::own_database(&pool).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        "another pinakesd already owns this database"
    );
    let within = Duration::from_secs(10);
    let parent = "https://example.org/page";
    store::ensure_resource(&pool, parent).await.unwrap();
    // A derived link takes the write order shared, a re-extraction exclusive.
    let child = tokio::time::timeout(
        within,
        store::derive_resource(&pool, parent, "https://example.org/child", "test", 10),
    )
    .await
    .expect("a shared writer waited")
    .unwrap();
    assert!(child.is_some());
    let requeued = tokio::time::timeout(within, queue::reextract(&pool, &["all".into()]))
        .await
        .expect("a re-extraction waited")
        .unwrap();
    assert_eq!(requeued, 2);
}

/// A page promoted to the media or activitypub route by its own card keeps that route through
/// `reextract`, and is re-queued there; an ordinary page is re-classified.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reextract_keeps_card_promoted_routes(pool: PgPool) {
    let promoted = store::ensure_resource(&pool, "https://video.example/watch/1")
        .await
        .unwrap();
    let plain = store::ensure_resource(&pool, "https://example.org/page")
        .await
        .unwrap();
    for (id, route) in [(&promoted, "media"), (&plain, "media")] {
        sqlx::query(
            "UPDATE resource SET resolver=$2,next_attempt_at=now()+interval '1 day' WHERE id=$1",
        )
        .bind(id)
        .bind(route)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE resource SET resolver='activitypub' WHERE id=$1")
        .bind(&plain)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(queue::reextract(&pool, &["all".into()]).await.unwrap(), 2);
    let routes: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT id,resolver,next_attempt_at<=now() FROM resource WHERE id=ANY($1) ORDER BY id",
    )
    .bind(vec![promoted.clone(), plain.clone()])
    .fetch_all(&pool)
    .await
    .unwrap();
    for (id, route, due) in routes {
        let expected = if id == promoted {
            "media"
        } else {
            "activitypub"
        };
        assert_eq!((route.as_str(), due), (expected, true), "{id}");
    }
}

/// An alias whose named root has become an alias meanwhile follows it to its own root (reads
/// follow one hop), and becoming an alias fences any job still running on the resource.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn aliases_never_chain_and_fence_running_jobs(pool: PgPool) {
    let first = store::ensure_resource(&pool, "https://example.org/a/")
        .await
        .unwrap();
    let generation = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT generation FROM resource WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let second = store::ensure_resource(&pool, "https://example.org/b")
        .await
        .unwrap();
    let before = generation(first.clone()).await;
    let mut tx = pool.begin().await.unwrap();
    let mut lines = vec![];
    for (id, root) in [
        (&first, "https://example.org/a"),
        // Named after its old root, which is an alias now.
        (&second, "https://example.org/a/"),
    ] {
        lines.extend(
            pinakes_core::alias::assign(
                &mut tx,
                id,
                &pinakes_core::alias::Alias {
                    root: root.into(),
                    basis: "canonical",
                },
            )
            .await
            .unwrap(),
        );
    }
    pinakes_core::alias::record(&mut tx, lines).await.unwrap();
    tx.commit().await.unwrap();
    let roots: Vec<(String, String)> =
        sqlx::query_as("SELECT alias_id,root_id FROM resource_alias ORDER BY alias_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    let root = pinakes_core::model::identity("https://example.org/a")
        .unwrap()
        .0;
    assert!(roots.iter().all(|(_, r)| *r == root), "{roots:?}");
    // Each becoming an alias is one `aliased` line naming its root.
    let lines: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT resource_id,after->>'root' FROM resource_change WHERE class='aliased' ORDER BY seq",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        lines,
        vec![
            (first.clone(), Some(root.clone())),
            (second, Some(root.clone()))
        ]
    );
    assert_eq!(generation(first).await, before + 1);
}

/// An alias row is answered by its root and never fetched again, so it holds no failure: a
/// dead-lettered row that becomes an alias is `pending` with no error, a row read fine stays
/// `ok`, and `reextract` repairs an alias that still holds a failure. The admin counts leave
/// aliases out of the queue statuses and count them apart.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_alias_holds_no_failure_of_its_own(pool: PgPool) {
    let walled = store::ensure_resource(&pool, "https://example.org/walled-slug")
        .await
        .unwrap();
    let read = store::ensure_resource(&pool, "https://example.org/read-slug")
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET status='unreachable',last_error='wall',attempts=5,dead_letter=true,denial_streak=5,denied_since=now() WHERE id=$1")
        .bind(&walled)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET status='ok' WHERE id=$1")
        .bind(&read)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let mut lines = vec![];
    for (id, root) in [
        (&walled, "https://example.org/walled"),
        (&read, "https://example.org/read"),
    ] {
        lines.extend(
            pinakes_core::alias::assign(
                &mut tx,
                id,
                &pinakes_core::alias::Alias {
                    root: root.into(),
                    basis: "canonical",
                },
            )
            .await
            .unwrap(),
        );
    }
    pinakes_core::alias::record(&mut tx, lines).await.unwrap();
    tx.commit().await.unwrap();
    let state = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>, i32, bool, i32)>(
                "SELECT status,last_error,attempts,dead_letter,denial_streak FROM resource WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        state(walled.clone()).await,
        ("pending".into(), None, 0, false, 0)
    );
    assert_eq!(state(read.clone()).await.0, "ok");
    // An alias that holds a failure (written as rows were before this rule) is repaired by
    // `reextract`.
    sqlx::query("UPDATE resource SET status='unreachable',last_error='wall',attempts=5,dead_letter=true WHERE id=$1")
        .bind(&walled)
        .execute(&pool)
        .await
        .unwrap();
    queue::reextract(&pool, &["web".into()]).await.unwrap();
    assert_eq!(
        state(walled.clone()).await,
        ("pending".into(), None, 0, false, 0)
    );
    // The admin counts: no queue status for the aliases, which are counted apart.
    sqlx::query("UPDATE resource SET status='unreachable',dead_letter=true WHERE id=$1")
        .bind(&walled)
        .execute(&pool)
        .await
        .unwrap();
    let app = pinakes_core::api::admin_router(
        pinakes_core::api::AppState::new(pool.clone(), pinakes_core::config::Config::default())
            .unwrap(),
    );
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::get("/v1/info")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(info["counts"]["aliases"], 2, "{info}");
    let dead: i64 = info["resolvers"]["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["dead_letter"].as_i64())
        .sum();
    assert_eq!(dead, 0, "{info}");
}

/// The generation fence covers the alias a page's canonical makes: a job whose lease was taken
/// over writes no alias; the current job does.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_stale_job_never_assigns_an_alias(pool: PgPool) {
    struct Canonical;
    #[async_trait::async_trait]
    impl pinakes_core::resolver::Resolver for Canonical {
        fn name(&self) -> &'static str {
            "web"
        }
        fn matches(&self, _: &str) -> bool {
            true
        }
        async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
            let mut d = ResourceMeta::new(uri, "web", 3600);
            d.name = Some("Page".into());
            d.alias_candidates = vec!["https://example.org/page".into()];
            Ok(d)
        }
        fn refresh_after(&self, _: &ResourceMeta) -> Duration {
            Duration::from_secs(3600)
        }
    }
    let uri = "https://example.org/page/";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let generation = |pool: PgPool, id: String| async move {
        sqlx::query_scalar::<_, i64>("SELECT generation FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let job = |generation| queue::Job {
        id: id.clone(),
        uri: uri.into(),
        attempts: 0,
        generation,
    };
    let stale = generation(pool.clone(), id.clone()).await;
    sqlx::query("UPDATE resource SET generation=generation+1 WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let aliased = |pool: PgPool, id: String| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1)",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    queue::run_job(&pool, &Canonical, &Default::default(), &job(stale))
        .await
        .unwrap();
    assert!(!aliased(pool.clone(), id.clone()).await);
    queue::run_job(&pool, &Canonical, &Default::default(), &job(stale + 1))
        .await
        .unwrap();
    assert!(aliased(pool.clone(), id.clone()).await);
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn our_own_rate_limit_is_its_own_class_and_no_failure(pool: PgPool) {
    let cfg = QueueConfig::default();
    let id = store::ensure_resource(&pool, "https://example.org/slow-host")
        .await
        .unwrap();
    let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
    queue::run_job(&pool, &DeferredResolver("rate_limited"), &cfg, &job)
        .await
        .unwrap();
    let (attempts, dead, error, delay): (i32, bool, String, f64) = sqlx::query_as(
        "SELECT attempts,dead_letter,last_error,extract(epoch FROM next_attempt_at-now())::float8 \
         FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((attempts, dead, error.as_str()), (0, false, "rate_limited"));
    assert!(delay > 58.0 && delay <= 60.0, "{delay}");
}

/// A source that answers `RetryLater` with a class other than `timeout` (a server error, a rate
/// limit) is rescheduled and never counts toward dead letter, however often it happens.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn upstream_retries_never_count_toward_dead_letter(pool: PgPool) {
    let cfg = QueueConfig {
        timeout_grace: 1,
        max_failures: 2,
        backoff_secs: 30,
        max_backoff_secs: 240,
        ..Default::default()
    };
    let id = store::ensure_resource(&pool, "https://example.org/slow-upstream")
        .await
        .unwrap();
    for _ in 0..6 {
        sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let job = queue::claim(&pool, "web", &cfg).await.unwrap().unwrap();
        queue::run_job(&pool, &DeferredResolver("upstream"), &cfg, &job)
            .await
            .unwrap();
        let row: (i32, i32, bool, f64) = sqlx::query_as("SELECT timeout_streak,attempts,dead_letter,extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((row.0, row.1, row.2), (0, 0, false), "{row:?}");
        assert!(row.3 <= 60.0 && row.3 > 58.0, "{row:?}");
    }
}
