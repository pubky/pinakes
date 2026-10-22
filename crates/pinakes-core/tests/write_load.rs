//! Concurrent writers against one database: resource saves (each followed by its group refresh),
//! derived links, ingested batches that add and remove tags, new alias spellings, and change
//! retention deleting calendar feeds' expired occurrences while those feeds are tagged and saved.
//! None waits on another for the search revision, none deadlocks, and every write succeeds.
//! `WRITE_LOAD_ROUNDS` scales the run; the numbers go to standard error.
use pinakes_core::{
    changes,
    config::EventConfig,
    event::calendar,
    group_keys, ingest,
    model::ResourceMeta,
    nexus::{Change, Changes, FeedPage, Kind, Op},
    store,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const WRITERS: usize = 8;
const FEEDS: usize = 4;

/// Lock waits seen by polling `pg_stat_activity`: all of them, and those on the search revision
/// (a statement naming it, or a tuple lock on its relation).
#[derive(Default, Debug)]
struct Waits {
    samples: u64,
    waiting: u64,
    revision: u64,
    peak: i64,
    /// Waiting backends seen, by the lock they wait for.
    by_lock: std::collections::BTreeMap<String, u64>,
}

async fn sample(pool: &PgPool, done: Arc<AtomicBool>) -> Waits {
    let mut waits = Waits::default();
    while !done.load(Ordering::SeqCst) {
        let (waiting, revision): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE a.wait_event_type='Lock'),
               count(*) FILTER (WHERE a.wait_event_type='Lock' AND (a.query LIKE '%search_write%'
                 OR EXISTS(SELECT 1 FROM pg_locks l WHERE l.pid=a.pid AND l.locktype='tuple' AND l.relation=to_regclass('search_write'))))
             FROM pg_stat_activity a WHERE a.datname=current_database() AND a.pid<>pg_backend_pid()",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        waits.samples += 1;
        waits.waiting += u64::from(waiting > 0);
        waits.revision += u64::from(revision > 0);
        waits.peak = waits.peak.max(waiting);
        let locks: Vec<String> = sqlx::query_scalar(
            "SELECT CASE WHEN l.locktype='advisory' THEN 'advisory '||l.objid||' '||l.mode WHEN a.query LIKE '%search_write%' OR EXISTS(SELECT 1 FROM pg_locks t WHERE t.pid=a.pid AND t.locktype='tuple' AND t.relation=to_regclass('search_write')) THEN 'search revision' ELSE l.locktype||coalesce(' '||l.relation::regclass::text,'') END
             FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE NOT l.granted AND a.datname=current_database()",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        for lock in locks {
            *waits.by_lock.entry(lock).or_default() += 1;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    waits
}

fn doc(uri: &str, name: &str, key: usize) -> ResourceMeta {
    let mut d = ResourceMeta::new(uri, "web", 86400);
    d.name = Some(name.into());
    d.set_type("Article");
    group_keys::add(&mut d, "cites_doi", &format!("10.5555/load-{key}"));
    d
}

/// A valid Pubky ID for writer `n`.
fn author(n: usize) -> String {
    let alphabet = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut v = vec![b'y'; 52];
    v[51] = alphabet[n & 31];
    String::from_utf8(v).unwrap()
}

/// Calendar feed `f` with two events ahead, fetched at `at` (milliseconds; a later save needs a
/// later fetch).
fn feed(f: usize, at: i64) -> ResourceMeta {
    let uri = format!("https://load.example/feed{f}.ics");
    let start = chrono::Utc::now() + chrono::Duration::days(2);
    let events: String = (0..2)
        .map(|i| {
            format!(
                "BEGIN:VEVENT\nUID:load{f}-{i}\nDTSTART:{}Z\nDURATION:PT1H\nSUMMARY:Load {i}\nEND:VEVENT\n",
                (start + chrono::Duration::days(i)).format("%Y%m%dT%H%M%S")
            )
        })
        .collect();
    let text = format!("BEGIN:VCALENDAR\nVERSION:2.0\n{events}END:VCALENDAR\n");
    let mut d = calendar::parse(
        &uri,
        text.as_bytes(),
        &EventConfig::default(),
        chrono::Utc::now(),
    )
    .unwrap();
    d.fetched_at = at;
    d
}

async fn deadlocks(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT deadlocks FROM pg_stat_database WHERE datname=current_database()")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn concurrent_writers_never_wait_on_the_search_revision(pool: PgPool) {
    let rounds: usize = std::env::var("WRITE_LOAD_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    let pool = PgPoolOptions::new()
        .max_connections(2 * WRITERS as u32 + 4)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let mut ids = vec![];
    for w in 0..WRITERS {
        let uri = format!("https://load.example/{w}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        store::save_document(&pool, &id, 0, &doc(&uri, "start", w / 2))
            .await
            .unwrap();
        ids.push((id, uri));
    }
    let mut feeds = vec![];
    let fetched = chrono::Utc::now().timestamp_millis();
    for f in 0..FEEDS {
        let uri = format!("https://load.example/feed{f}.ics");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        store::save_document(&pool, &id, 0, &feed(f, fetched))
            .await
            .unwrap();
        feeds.push((id, uri));
    }
    let before = deadlocks(&pool).await;
    let writes = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = tokio::spawn({
        let pool = pool.clone();
        let done = done.clone();
        async move { sample(&pool, done).await }
    });
    let started = Instant::now();
    let mut tasks = vec![];
    for (w, (id, uri)) in ids.iter().cloned().enumerate() {
        let pool = pool.clone();
        let writes = writes.clone();
        tasks.push(tokio::spawn(async move {
            for r in 0..rounds {
                // A save of its own resource (two writers share each group key), then a link it
                // derives; one writer adds new spellings that become aliases instead.
                store::save_document(&pool, &id, 0, &doc(&uri, &format!("round {r}"), w / 2))
                    .await
                    .unwrap();
                if w != 0 {
                    store::derive_resource(
                        &pool,
                        &uri,
                        &format!("https://load.example/{w}/link/{r}"),
                        "btcmap",
                        10_000,
                    )
                    .await
                    .unwrap();
                } else {
                    store::ensure_resource(
                        &pool,
                        &format!("https://load.example/{w}/page/{r}?utm_source=load"),
                    )
                    .await
                    .unwrap();
                }
                writes.fetch_add(2, Ordering::SeqCst);
            }
        }));
    }
    // Ingested batches of tags on the same resources.
    tasks.push(tokio::spawn({
        let pool = pool.clone();
        let writes = writes.clone();
        let uris: Vec<String> = ids
            .iter()
            .chain(&feeds)
            .map(|(_, uri)| uri.clone())
            .collect();
        async move {
            for r in 0..rounds {
                let saved = ingest::cursor(&pool, "load").await.unwrap();
                // Each round tags every resource and removes the previous round's tags.
                let line = |op, r: usize, i: usize, uri: &String| Change {
                    op,
                    uri: uri.clone(),
                    kind: Kind::Tag,
                    author: author(i),
                    object_path: format!("/pub/app/tags/load-{r}-{i}"),
                    label: Some(format!("load{r}")),
                    at: 1_790_000_000_000,
                    recurrence_id: None,
                    status: None,
                    start: None,
                    end: None,
                };
                let mut lines: Vec<Change> = uris
                    .iter()
                    .enumerate()
                    .map(|(i, uri)| line(Op::Put, r, i, uri))
                    .collect();
                if r > 0 {
                    lines.extend(
                        uris.iter()
                            .enumerate()
                            .map(|(i, uri)| line(Op::Del, r - 1, i, uri)),
                    );
                }
                let next = saved.cursor + lines.len() as i64;
                assert!(ingest::apply(
                    &pool,
                    "load",
                    &saved,
                    &FeedPage {
                        page: Changes {
                            lines,
                            next_cursor: next,
                        },
                        epoch: Some("load".into()),
                        length: next,
                    },
                    &Default::default(),
                    &pinakes_core::telemetry::Metrics::new().unwrap(),
                )
                .await
                .unwrap());
                writes.fetch_add(1, Ordering::SeqCst);
            }
        }
    }));
    // Change retention: each round the feeds' occurrences expire (one feed at a time, as a save
    // would change them), retention deletes them, and the feeds are fetched again.
    tasks.push(tokio::spawn({
        let pool = pool.clone();
        let writes = writes.clone();
        let feeds = feeds.clone();
        async move {
            for r in 0..rounds {
                for (id, _) in &feeds {
                    let mut tx = pool.begin().await.unwrap();
                    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR UPDATE")
                        .bind(id)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    sqlx::query("UPDATE occurrence SET status='cancelled',start_at=now()-interval '50 days',end_at=now()-interval '49 days' WHERE resource_id=$1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    tx.commit().await.unwrap();
                }
                changes::retain(&pool, 90).await.unwrap();
                for (f, (id, _)) in feeds.iter().enumerate() {
                    store::save_document(&pool, id, 0, &feed(f, fetched + 1 + r as i64))
                        .await
                        .unwrap();
                }
                writes.fetch_add(2 * FEEDS as u64 + 1, Ordering::SeqCst);
            }
        }
    }));
    for task in tasks {
        task.await.unwrap();
    }
    let elapsed = started.elapsed();
    done.store(true, Ordering::SeqCst);
    let waits = sampler.await.unwrap();
    let writes = writes.load(Ordering::SeqCst);
    eprintln!(
        "write load: {writes} write transactions in {:.2} s ({:.0}/s); {} samples, {} with a lock wait (peak {} waiting), {} waiting on the search revision; {} deadlocks; waiting backends by lock: {:?}",
        elapsed.as_secs_f64(),
        writes as f64 / elapsed.as_secs_f64(),
        waits.samples,
        waits.waiting,
        waits.peak,
        waits.revision,
        deadlocks(&pool).await - before,
        waits.by_lock
    );
    assert_eq!(waits.revision, 0, "{waits:?}");
    assert_eq!(deadlocks(&pool).await, before, "no deadlock");
}
