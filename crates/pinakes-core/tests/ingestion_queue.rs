use chrono::Utc;
use pinakes_core::{
    config::QueueConfig,
    ingest,
    model::{identity, ResourceMeta, Status},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    queue,
    resolver::Resolver,
    store,
};
use sqlx::PgPool;
use std::time::Duration;

fn change(uri: &str, author: &str, path: &str) -> Change {
    let (resource_id, uri, _) = identity(uri).unwrap();
    Change {
        op: Op::Put,
        resource_id,
        uri,
        kind: Kind::Tag,
        author: author.into(),
        object_path: path.into(),
        label: Some("rust".into()),
        at: Utc::now().timestamp_millis(),
        recurrence_id: None,
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
    b.author = "author-b".into();
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
    let current: String =
        sqlx::query_scalar("SELECT resource_id FROM reference WHERE author='author-a'")
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
    assert!(!ingest::apply(&pool, "fixture", &saved, &empty)
        .await
        .unwrap());
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
    bad.at = i64::MAX;
    let saved = ingest::cursor(&pool, "fixture").await.unwrap();
    let invalid = FeedPage {
        page: Changes {
            lines: vec![a.clone(), bad],
            next_cursor: 2,
        },
        epoch: None,
        length: 2,
    };
    assert!(ingest::apply(&pool, "fixture", &saved, &invalid)
        .await
        .is_err());
    assert_eq!(refs(&pool).await, 0);
    assert_eq!(ingest::cursor(&pool, "fixture").await.unwrap().cursor, 0);
    batch(&pool, vec![a], None).await;
    assert!(!ingest::apply(&pool, "fixture", &saved, &invalid)
        .await
        .unwrap());
}
#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_pollers_only_commit_a_batch_once(pool: PgPool) {
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
        ingest::apply(&pool, "fixture", &saved, &page),
        ingest::apply(&pool, "fixture", &saved, &page)
    );
    assert_ne!(a.unwrap(), b.unwrap());
    assert_eq!(refs(&pool).await, 1);
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
    sqlx::query("UPDATE resource SET final=true,next_attempt_at='infinity'")
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
