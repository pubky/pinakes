//! Concurrent writers against one database: resource saves (each followed by its group refresh;
//! two writers' pages share each hub key, so their refreshes meet on one group), derived links,
//! ingested batches that add and remove tags, new alias spellings, and change retention deleting
//! calendar feeds' expired occurrences while those feeds are tagged and saved.
//! None waits on another for the search revision, none deadlocks, and no write is lost: the state
//! the load leaves is read back (waits are sampled from `pg_locks` on a connection of the
//! sampler's own).
//! `WRITE_LOAD_ROUNDS` scales the run; the numbers go to standard error.
mod common;
use pinakes_core::{
    changes,
    config::EventConfig,
    event::calendar,
    group_keys, ingest,
    model::ResourceMeta,
    nexus::{Change, Changes, FeedPage, Kind, Op},
    store,
};
use sqlx::{postgres::PgPoolOptions, Connection, PgConnection, PgPool};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const WRITERS: usize = 8;
const FEEDS: usize = 4;

/// Lock waits seen by polling `pg_locks`: all of them, and those on the search revision.
#[derive(Default, Debug)]
struct Waits {
    samples: u64,
    waiting: u64,
    revision: u64,
    peak: i64,
    /// Waiting backends seen, by the lock they wait for.
    by_lock: std::collections::BTreeMap<String, u64>,
}

/// What a backend waits for when it waits on the search revision.
const REVISION: &str = "search revision";

/// Each backend of this database (the caller's aside) waiting for a lock, with what it waits for:
/// [`REVISION`] for a row or relation lock of `search_write` or its sequence (an insert's `nextval`
/// waits for one that is being altered), or while it holds a row lock of `search_write` and waits
/// for that row's transaction (relations of this database only); else the advisory key, or the lock type and relation (a
/// relation extension, `extend search_write`, lasts microseconds, not a transaction). One
/// `pg_locks` snapshot, and no statement text: `pg_stat_activity` reads a backend's statement
/// from a snapshot but its wait live, so a statement that named `search_write` (this sampler's
/// own, on a connection a writer took over) could stand beside an unrelated wait.
async fn waiting(conn: &mut PgConnection) -> Vec<(i32, String)> {
    sqlx::query_as(
        "WITH l AS MATERIALIZED (SELECT * FROM pg_locks),
           db AS (SELECT oid FROM pg_database WHERE datname=current_database()),
           here AS (SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid())
         SELECT w.pid, CASE
           WHEN (w.locktype IN ('relation','page','tuple') AND w.database=(SELECT oid FROM db)
                 AND w.relation IN (to_regclass('search_write'), to_regclass('search_write_id_seq')))
             OR EXISTS(SELECT 1 FROM l t WHERE t.pid=w.pid AND t.granted AND t.locktype='tuple'
                 AND t.database=(SELECT oid FROM db) AND t.relation=to_regclass('search_write'))
             THEN 'search revision'
           WHEN w.locktype='advisory' THEN 'advisory '||w.objid||' '||w.mode
           ELSE w.locktype||coalesce(' '||w.relation::regclass::text,'') END
         FROM l w WHERE NOT w.granted AND w.pid IN (SELECT pid FROM here) ORDER BY w.pid",
    )
    .fetch_all(conn)
    .await
    .unwrap()
}

async fn sample(mut conn: PgConnection, done: Arc<AtomicBool>) -> Waits {
    let mut waits = Waits::default();
    while !done.load(Ordering::SeqCst) {
        let locks = waiting(&mut conn).await;
        let backends = locks
            .iter()
            .map(|(pid, _)| pid)
            .collect::<std::collections::BTreeSet<_>>()
            .len() as i64;
        waits.samples += 1;
        waits.waiting += u64::from(backends > 0);
        waits.revision += u64::from(locks.iter().any(|(_, lock)| lock == REVISION));
        waits.peak = waits.peak.max(backends);
        for (_, lock) in locks {
            *waits.by_lock.entry(lock).or_default() += 1;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    waits
}

/// Page `w` as a save writes it; the citation is a grouping key of this page alone.
fn doc(uri: &str, name: &str, w: usize) -> ResourceMeta {
    let mut d = ResourceMeta::new(uri, "web", 86400);
    d.name = Some(name.into());
    d.set_type("Article");
    group_keys::add(&mut d, "cites_doi", &format!("10.5555/load-{w}"));
    d
}

/// The hub key pages `2k` and `2k+1` share: the two are one group.
fn shared_key(w: usize) -> String {
    format!("isrc:QZLD1260{:04}", w / 2)
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

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_writers_never_wait_on_the_search_revision(pool: PgPool) {
    let rounds: usize = std::env::var("WRITE_LOAD_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|rounds| *rounds > 0)
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
        store::save_document(&pool, &id, 0, &doc(&uri, "start", w))
            .await
            .unwrap();
        sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,$2,'test','test',now()+interval '1 day')")
            .bind(&id)
            .bind(shared_key(w))
            .execute(&pool)
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
    // The sampler has its own connection: the writers' pool never runs its statements.
    let sampler = tokio::spawn(sample(
        PgConnection::connect_with(&pool.connect_options())
            .await
            .unwrap(),
        done.clone(),
    ));
    let started = Instant::now();
    let mut tasks = vec![];
    for (w, (id, uri)) in ids.iter().cloned().enumerate() {
        let pool = pool.clone();
        let writes = writes.clone();
        tasks.push(tokio::spawn(async move {
            for r in 0..rounds {
                // A save of its own resource (its group refresh reads the hub key it shares
                // with one other writer's page), then a link it derives; one writer adds new
                // spellings that become aliases instead.
                store::save_document(&pool, &id, 0, &doc(&uri, &format!("round {r}"), w))
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
                            skipped: Default::default(),
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
                    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR NO KEY UPDATE")
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
    // Bounded: writers that wait on each other for ever fail the test instead of hanging it.
    tokio::time::timeout(Duration::from_secs(120 + 10 * rounds as u64), async {
        for task in tasks {
            task.await.unwrap();
        }
    })
    .await
    .expect("the writers never finished");
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
    assert!(waits.samples > 0, "the sampler never ran: {waits:?}");
    assert_eq!(waits.revision, 0, "{waits:?}");
    assert_eq!(deadlocks(&pool).await, before, "no deadlock");
    // No write is lost. Each page holds its last save.
    let last = rounds - 1;
    for (id, uri) in &ids {
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name, Some(format!("round {last}")), "{uri}");
    }
    // Every link a writer derived is there, live, under the page that derived it, and no other.
    let derived: Vec<(String, String)> = sqlx::query_as(
        "SELECT parent_id,child_id FROM resource_derivation WHERE expires_at IS NULL OR expires_at>now() ORDER BY 1,2",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected: Vec<(String, String)> = ids
        .iter()
        .enumerate()
        .skip(1)
        .flat_map(|(w, (id, _))| {
            (0..rounds).map(move |r| {
                let link = format!("https://load.example/{w}/link/{r}");
                (id.clone(), pinakes_core::model::identity(&link).unwrap().0)
            })
        })
        .collect();
    expected.sort();
    assert_eq!(expected.len(), (WRITERS - 1) * rounds);
    assert_eq!(derived, expected, "derived links");
    // Every spelling the first writer added is an alias.
    let spellings: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_alias a JOIN resource r ON r.id=a.alias_id WHERE a.basis='tracking' AND r.uri LIKE 'https://load.example/0/page/%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(spellings, rounds as i64, "alias spellings");
    // Each page and feed carries the last round's tag and no earlier one.
    for (id, uri) in ids.iter().chain(&feeds) {
        let labels: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT label FROM reference WHERE resource_id=$1 AND kind='tag' ORDER BY label",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(labels, [Some(format!("load{last}"))], "{uri}");
    }
    // Each feed's two events are live again after the last deletion and save.
    for (id, uri) in &feeds {
        let (all, live): (i64, i64) = sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE status<>'cancelled' AND start_at>now()) FROM occurrence WHERE resource_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((all, live), (2, 2), "{uri}");
    }
    // Every group refresh the load put off runs now (`drain_groups` fails the test on one that
    // cannot be done), and the two pages of each hub key are one group, each key's a group of
    // its own.
    common::drain_groups(&pool, &Default::default()).await;
    let mut groups = vec![];
    for (id, uri) in &ids {
        let group: Option<String> =
            sqlx::query_scalar("SELECT group_id FROM group_unit WHERE resource_id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        groups.push(group.unwrap_or_else(|| panic!("{uri} is in no group")));
    }
    for pair in groups.chunks(2) {
        assert_eq!(pair[0], pair[1], "the pages of one hub key: {groups:?}");
    }
    let distinct: std::collections::BTreeSet<&String> = groups.iter().collect();
    assert_eq!(
        distinct.len(),
        WRITERS / 2,
        "one group per hub key: {groups:?}"
    );
}

/// The sampler counts a wait on a `search_write` row (the transaction holding it, and the row
/// lock) and on its sequence, and not an advisory wait by a backend whose last statement named
/// `search_write`.
#[sqlx::test(migrations = "../../migrations")]
async fn the_sampler_counts_waits_on_the_search_revision_and_nothing_else(pool: PgPool) {
    let connect = || async {
        PgConnection::connect_with(&pool.connect_options())
            .await
            .unwrap()
    };
    let row = "SELECT id FROM search_write ORDER BY id LIMIT 1 FOR UPDATE";
    // A holds a search_write row and the advisory key 42.
    let mut a = connect().await;
    sqlx::query("BEGIN").execute(&mut a).await.unwrap();
    sqlx::query(row).execute(&mut a).await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(42)")
        .execute(&mut a)
        .await
        .unwrap();
    // B and C wait for the row (B for A's transaction, holding the row lock; C for the row
    // lock); D names search_write, then waits for the advisory key.
    let mut d = connect().await;
    sqlx::query("SELECT 'search_write'")
        .execute(&mut d)
        .await
        .unwrap();
    let mut blocked = vec![];
    for sql in [row, row] {
        let mut c = connect().await;
        blocked.push(tokio::spawn(async move {
            sqlx::query("BEGIN").execute(&mut c).await.unwrap();
            sqlx::query(sql).execute(&mut c).await.unwrap();
            sqlx::query("COMMIT").execute(&mut c).await.unwrap();
        }));
        // In order: the first waiter takes the row lock.
        let mut probe = connect().await;
        let n = blocked.len();
        while waiting(&mut probe).await.len() < n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    blocked.push(tokio::spawn(async move {
        sqlx::query("SELECT pg_advisory_lock(42)")
            .execute(&mut d)
            .await
            .unwrap();
    }));
    let mut probe = connect().await;
    let locks = loop {
        let locks = waiting(&mut probe).await;
        if locks.len() == 3 {
            break locks;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let mut kinds: Vec<&str> = locks.iter().map(|(_, lock)| lock.as_str()).collect();
    kinds.sort();
    assert_eq!(
        kinds,
        ["advisory 42 ExclusiveLock", REVISION, REVISION],
        "{locks:?}"
    );
    sqlx::query("COMMIT").execute(&mut a).await.unwrap();
    for task in blocked {
        task.await.unwrap();
    }
    // An insert whose nextval waits for the sequence being altered waits on the revision too.
    sqlx::query("BEGIN").execute(&mut a).await.unwrap();
    sqlx::query("ALTER SEQUENCE search_write_id_seq RESTART WITH 1000000")
        .execute(&mut a)
        .await
        .unwrap();
    let mut e = connect().await;
    let insert = tokio::spawn(async move {
        sqlx::query("INSERT INTO search_write DEFAULT VALUES")
            .execute(&mut e)
            .await
            .unwrap();
    });
    let locks = loop {
        let locks = waiting(&mut probe).await;
        if !locks.is_empty() {
            break locks;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(locks.len(), 1, "{locks:?}");
    assert_eq!(locks[0].1, REVISION, "{locks:?}");
    sqlx::query("ROLLBACK").execute(&mut a).await.unwrap();
    insert.await.unwrap();
}
