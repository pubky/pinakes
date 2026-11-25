//! The catalogue importer base, proved with a fixture importer over a fresh database: a full
//! import, a re-import that rewrites only changed cards and removes the ones it no longer reads,
//! an update, resuming after a failure and after a restart, the catalogue's lock, the wait for
//! free disk, one large full import at a time, the drop, the switches and the metrics.
use anyhow::Result;
use async_trait::async_trait;
use pinakes_core::{
    catalogue::{
        run::{self, Context, Importer, Job, Next, Pace, Plan, Run, SessionLock, UpdateEnd},
        store::{self, State},
        Card, Catalogue, CataloguesConfig,
    },
    config::Config,
    telemetry::Metrics,
};
use serde_json::json;
use sqlx::PgPool;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{watch, Notify};

/// A catalogue "dump" of cards, written `batch` at a time with the next index as the cursor.
struct Fixture {
    catalogue: Catalogue,
    items: Mutex<Vec<Card>>,
    batch: usize,
    /// The next run the importer asks for; none: nothing is due.
    next: Mutex<Option<Run>>,
    /// Fail the next import after writing this many batches.
    fail_after: Mutex<Option<usize>>,
    /// Stop after the first batch until notified (a run in progress).
    gate: Option<Arc<Notify>>,
    /// Notified when the first batch is written.
    entered: Arc<Notify>,
    /// The cursor each run started from.
    started_from: Mutex<Vec<Option<serde_json::Value>>>,
    /// For an update: the cards that changed and the source IDs removed upstream.
    changes: Mutex<(Vec<Card>, Vec<String>)>,
    /// How an update ends.
    update_end: Mutex<UpdateEnd>,
    /// The upstream time a run states (`Job::set_current_as_of`); none: it states nothing.
    current: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    /// Panic once, in the next import.
    panic_once: AtomicBool,
    need: u64,
}
impl Fixture {
    fn new(catalogue: Catalogue, items: Vec<Card>) -> Self {
        Self {
            catalogue,
            items: Mutex::new(items),
            batch: 10,
            next: Mutex::new(Some(Run::Import)),
            fail_after: Mutex::new(None),
            gate: None,
            entered: Arc::default(),
            started_from: Mutex::default(),
            changes: Mutex::default(),
            update_end: Mutex::new(UpdateEnd::Done),
            current: Mutex::default(),
            panic_once: AtomicBool::new(false),
            need: 1000,
        }
    }
}
#[async_trait]
impl Importer for Fixture {
    fn catalogue(&self) -> Catalogue {
        self.catalogue
    }
    fn disk_need(&self, _run: Run, _state: &State) -> u64 {
        self.need
    }
    async fn plan(&self, _state: &State) -> Result<Plan> {
        Ok(match self.next.lock().unwrap().take() {
            Some(run) => Plan::Run(run),
            None => Plan::Wait(Duration::from_secs(3600)),
        })
    }
    async fn import(&self, job: &mut Job<'_>) -> Result<()> {
        self.started_from
            .lock()
            .unwrap()
            .push(job.cursor().cloned());
        if self.panic_once.swap(false, Ordering::SeqCst) {
            panic!("the fixture importer panicked");
        }
        let start = job.cursor().and_then(|c| c["next"].as_u64()).unwrap_or(0) as usize;
        let items = self.items.lock().unwrap().clone();
        let mut written = 0;
        for at in (start..items.len()).step_by(self.batch) {
            let fail = *self.fail_after.lock().unwrap();
            if fail == Some(written) {
                *self.fail_after.lock().unwrap() = None;
                anyhow::bail!("the fixture dump broke off");
            }
            let end = (at + self.batch).min(items.len());
            job.write(&items[at..end], json!({ "next": end })).await?;
            written += 1;
            if written == 1 {
                self.entered.notify_one();
                if let Some(gate) = &self.gate {
                    gate.notified().await;
                }
            }
        }
        job.upstream_version("2026-11-24").await?;
        if let Some(at) = *self.current.lock().unwrap() {
            job.set_current_as_of(at);
        }
        Ok(())
    }
    async fn update(&self, job: &mut Job<'_>) -> Result<UpdateEnd> {
        self.started_from
            .lock()
            .unwrap()
            .push(job.cursor().cloned());
        let (changed, removed) = std::mem::take(&mut *self.changes.lock().unwrap());
        job.write(&changed, json!({ "step": "written" })).await?;
        job.remove(&removed, json!({ "step": "removed" })).await?;
        // The next update starts one sequence on.
        let sequence = job
            .position()
            .and_then(|p| p["sequence"].as_u64())
            .unwrap_or(0);
        job.set_position(json!({ "sequence": sequence + 1 }));
        if let Some(at) = *self.current.lock().unwrap() {
            job.set_current_as_of(at);
        }
        Ok(*self.update_end.lock().unwrap())
    }
}

/// A directory of a test's own, removed when it is dropped.
struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pinakes-catalogue-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn card(n: usize) -> Card {
    Card {
        source_id: format!("Q{n}"),
        uri: format!("https://en.wikipedia.org/wiki/Item_{n}"),
        r#type: "Thing".into(),
        name: format!("Item {n}"),
        labels: vec!["Some Genre".into()],
        keys: vec![format!("wikidata:Q{n}")],
        fame: (n as f32) / 100.0,
        ..Card::default()
    }
}
fn cards(n: usize) -> Vec<Card> {
    (0..n).map(card).collect()
}
/// A runner's context over its own temporary `catalogues.dir` (removed with the `TempDir`), with
/// short waits.
fn context(pool: &PgPool, free: Arc<AtomicU64>) -> (Context, TempDir) {
    let dir = TempDir::new();
    let mut config = Config::default();
    config.catalogues.dir = dir.0.to_string_lossy().into_owned();
    let mut ctx = Context::new(pool.clone(), Arc::new(config));
    ctx.free_disk = Arc::new(move |_| Ok(free.load(Ordering::SeqCst)));
    ctx.pace = Pace {
        retry: Duration::from_millis(20),
        disk: Duration::from_millis(20),
        turn: Duration::from_millis(20),
        lock_check: Duration::from_millis(100),
    };
    (ctx, dir)
}
/// How long a test waits for what a task does: long, so a loaded build slot doesn't fail it.
const DEADLINE: Duration = Duration::from_secs(60);
fn plenty() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(u64::MAX))
}
async fn state(pool: &PgPool, catalogue: Catalogue) -> State {
    store::state(pool, catalogue).await.unwrap().unwrap()
}
async fn count(pool: &PgPool, catalogue: Catalogue) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM catalogue_card WHERE catalogue=$1")
        .bind(catalogue.name())
        .fetch_one(pool)
        .await
        .unwrap()
}
/// Each row's source ID with its transaction ID and updated time: unchanged only when the row
/// was not written.
async fn versions(pool: &PgPool) -> Vec<(String, String, chrono::DateTime<chrono::Utc>)> {
    sqlx::query_as("SELECT source_id,xmin::text,updated_at FROM catalogue_card ORDER BY source_id")
        .fetch_all(pool)
        .await
        .unwrap()
}
async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = std::time::Instant::now();
    while started.elapsed() < DEADLINE {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A full import writes every card, normalized, and records the run; a re-import rewrites only
/// the card whose content changed, stamps the unchanged ones without rewriting their content,
/// and removes the card it no longer reads; an update writes only what changed and removes
/// what was removed upstream, touching no other row.
#[sqlx::test(migrations = "../../migrations")]
async fn imports_rewrite_only_changed_cards_and_remove_unseen_ones(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let fixture = Fixture::new(Catalogue::Wikidata, cards(25));
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!((s.phase.as_str(), s.run.as_deref()), ("idle", None));
    assert_eq!((s.generation, s.cards, s.cursor), (1, 25, None));
    assert!(s.last_import_at.is_some() && s.last_update_at.is_none());
    assert_eq!(s.upstream_version.as_deref(), Some("2026-11-24"));
    assert_eq!(count(&pool, Catalogue::Wikidata).await, 25);
    let (uri, labels, keys, generation): (String, Vec<String>, Vec<String>, i64) = sqlx::query_as(
        "SELECT uri,labels,keys,seen_generation FROM catalogue_card WHERE source_id='Q3'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(uri, "https://en.wikipedia.org/wiki/Item_3");
    assert_eq!(
        (labels, keys),
        (
            vec!["some-genre".to_owned()],
            vec!["wikidata:Q3".to_owned()]
        )
    );
    assert_eq!(generation, 1);
    let before = versions(&pool).await;

    // The dump changes: Q7 is renamed, Q5 is gone.
    {
        let mut items = fixture.items.lock().unwrap();
        items[7].name = "Item seven".into();
        items.remove(5);
    }
    *fixture.next.lock().unwrap() = Some(Run::Import);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!((s.generation, s.cards), (2, 24));
    let after = versions(&pool).await;
    assert_eq!(after.len(), 24);
    assert!(!after.iter().any(|(id, _, _)| id == "Q5"));
    for (id, _, updated_at) in &after {
        let (_, _, was) = before.iter().find(|(b, _, _)| b == id).unwrap();
        // Only the changed card's content was rewritten.
        assert_eq!(updated_at != was, id == "Q7", "{id}");
    }
    let unseen: i64 =
        sqlx::query_scalar("SELECT count(*) FROM catalogue_card WHERE seen_generation<>2")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unseen, 0);
    let name: String = sqlx::query_scalar("SELECT name FROM catalogue_card WHERE source_id='Q7'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Item seven");

    // An update: Q8 changes, Q9 is removed upstream, Q10 comes again unchanged.
    let before = versions(&pool).await;
    let mut q8 = card(8);
    q8.description = Some("changed".into());
    *fixture.changes.lock().unwrap() = (vec![q8, card(10)], vec!["Q9".into()]);
    *fixture.next.lock().unwrap() = Some(Run::Update);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!(
        (s.generation, s.cards, s.run, s.cursor),
        (2, 23, None, None)
    );
    assert!(s.last_update_at.is_some());
    let after = versions(&pool).await;
    assert_eq!(after.len(), 23);
    for (id, xmin, _) in &after {
        let (_, was, _) = before.iter().find(|(b, _, _)| b == id).unwrap();
        // An update writes no row but the changed one, not even a stamp.
        assert_eq!(xmin != was, id == "Q8", "{id}");
    }
    // The position the update left is kept for the next, which starts from it.
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!(s.position, Some(json!({"sequence": 1})));
    *fixture.next.lock().unwrap() = Some(Run::Update);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!(s.position, Some(json!({"sequence": 2})));
    // A full import keeps it too.
    *fixture.next.lock().unwrap() = Some(Run::Import);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    assert_eq!(
        state(&pool, Catalogue::Wikidata).await.position,
        Some(json!({"sequence": 2}))
    );
    // Nothing is due: the task waits.
    assert!(matches!(
        run::step(&fixture, &ctx).await.unwrap(),
        Next::Wait(_)
    ));
}

/// A run that fails keeps its cursor: the next step resumes there, in the same generation, and
/// removes nothing a failed import did not read. The task records the failure and tries again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_failed_import_resumes_from_its_cursor(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    // A first import of 25 that breaks off after one batch.
    let fixture = Fixture::new(Catalogue::Tranco, cards(25));
    let tranco = |c: Card| Card {
        uri: format!("https://{}.example/", c.source_id.to_lowercase()),
        r#type: "WebSite".into(),
        ..c
    };
    *fixture.items.lock().unwrap() = cards(25).into_iter().map(tranco).collect();
    *fixture.fail_after.lock().unwrap() = Some(1);
    let error = run::step(&fixture, &ctx).await.unwrap_err();
    assert!(error.to_string().contains("broke off"));
    let s = state(&pool, Catalogue::Tranco).await;
    assert_eq!(s.run.as_deref(), Some("import"));
    assert_eq!(s.cursor, Some(json!({"next": 10})));
    assert_eq!((s.generation, s.cards), (1, 10));
    assert!(s.last_import_at.is_none());
    // Resumed: from its cursor, same generation, all 25 at the end.
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    assert_eq!(
        *fixture.started_from.lock().unwrap(),
        [None, Some(json!({"next": 10}))]
    );
    let s = state(&pool, Catalogue::Tranco).await;
    assert_eq!((s.generation, s.cards, s.run), (1, 25, None));
    assert_eq!(count(&pool, Catalogue::Tranco).await, 25);

    // Through the task: a re-import that fails once is recorded and tried again.
    let fixture = Arc::new(Fixture::new(Catalogue::Tranco, vec![]));
    *fixture.items.lock().unwrap() = cards(25).into_iter().map(tranco).collect();
    *fixture.fail_after.lock().unwrap() = Some(2);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(run::run(fixture.clone(), ctx.clone(), shutdown));
    until("the re-import after its failure", || async {
        state(&pool, Catalogue::Tranco).await.generation == 2
            && state(&pool, Catalogue::Tranco).await.run.is_none()
    })
    .await;
    let s = state(&pool, Catalogue::Tranco).await;
    assert_eq!(s.error_count, 1);
    assert_eq!(s.last_error.as_deref(), Some("unclassified"));
    assert_eq!(s.phase, "idle");
    assert_eq!(
        fixture.started_from.lock().unwrap()[1],
        Some(json!({"next": 20}))
    );
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// A daemon stopped in the middle of a run (a restart) leaves the run and its cursor in the state
/// row; the next task resumes from there.
#[sqlx::test(migrations = "../../migrations")]
async fn a_run_cut_off_by_a_restart_resumes(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let mut fixture = Fixture::new(Catalogue::Osm, vec![]);
    *fixture.items.lock().unwrap() = cards(30)
        .into_iter()
        .map(|c| Card {
            uri: format!("https://www.openstreetmap.org/node/{}", &c.source_id[1..]),
            r#type: "Place".into(),
            ..c
        })
        .collect();
    fixture.gate = Some(Arc::default());
    let fixture = Arc::new(fixture);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(run::run(fixture.clone(), ctx.clone(), shutdown));
    tokio::time::timeout(DEADLINE, fixture.entered.notified())
        .await
        .unwrap();
    stop.send(true).unwrap();
    task.await.unwrap();
    let s = state(&pool, Catalogue::Osm).await;
    assert_eq!(
        (s.phase.as_str(), s.run.as_deref()),
        ("import", Some("import"))
    );
    assert_eq!(s.cursor, Some(json!({"next": 10})));
    // The large-import lock went with the stopped task.
    until("the large-import lock released", || async {
        match SessionLock::try_take(&pool, pinakes_core::locks::CATALOGUE_LARGE_IMPORT)
            .await
            .unwrap()
        {
            Some(lock) => {
                lock.release().await;
                true
            }
            None => false,
        }
    })
    .await;

    let again = Fixture::new(Catalogue::Osm, fixture.items.lock().unwrap().clone());
    *again.next.lock().unwrap() = None;
    let again = Arc::new(again);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(run::run(again.clone(), ctx.clone(), shutdown));
    until("the resumed import", || async {
        state(&pool, Catalogue::Osm).await.last_import_at.is_some()
    })
    .await;
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(
        *again.started_from.lock().unwrap(),
        [Some(json!({"next": 10}))]
    );
    let s = state(&pool, Catalogue::Osm).await;
    assert_eq!((s.generation, s.cards, s.run), (1, 30, None));
}

/// Two processes never import one catalogue: while another session holds its lock the task
/// waits, writing nothing (the state row is the importing process's), and goes on once it is
/// free. The drop takes the same lock.
#[sqlx::test(migrations = "../../migrations")]
async fn the_catalogue_lock_keeps_a_second_importer_out(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let other = SessionLock::try_take(&pool, Catalogue::Software.lock_key())
        .await
        .unwrap()
        .unwrap();
    assert!(SessionLock::try_take(&pool, Catalogue::Software.lock_key())
        .await
        .unwrap()
        .is_none());
    let mut fixture = Fixture::new(Catalogue::Software, vec![]);
    *fixture.items.lock().unwrap() = cards(5)
        .into_iter()
        .map(|c| Card {
            source: "crates.io".into(),
            uri: format!("https://crates.io/crates/{}", c.source_id),
            r#type: "SoftwareSourceCode".into(),
            ..c
        })
        .collect();
    fixture.batch = 2;
    let fixture = Arc::new(fixture);
    // The importing process's row, as it left it.
    let before = store::ensure_state(&pool, Catalogue::Software)
        .await
        .unwrap();
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(run::run(fixture.clone(), ctx.clone(), shutdown));
    // Many tries of the lock (one every 20 ms) leave the row as it was.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(state(&pool, Catalogue::Software).await, before);
    assert_eq!(count(&pool, Catalogue::Software).await, 0);
    assert!(fixture.started_from.lock().unwrap().is_empty());
    other.release().await;
    until("the import after the lock", || async {
        state(&pool, Catalogue::Software)
            .await
            .last_import_at
            .is_some()
    })
    .await;
    let s = state(&pool, Catalogue::Software).await;
    assert_eq!((s.cards, s.waiting), (5, None));
    // The task holds the lock while it runs, between runs too: a drop cannot take it.
    assert!(SessionLock::try_take(&pool, Catalogue::Software.lock_key())
        .await
        .unwrap()
        .is_none());
    stop.send(true).unwrap();
    task.await.unwrap();
    // Stopped, the lock is free again.
    until("the lock released", || async {
        match SessionLock::try_take(&pool, Catalogue::Software.lock_key())
            .await
            .unwrap()
        {
            Some(lock) => {
                lock.release().await;
                true
            }
            None => false,
        }
    })
    .await;
}

/// Short of free disk, a run does not start: the state says why, nothing is written and no
/// generation begins. With room it starts.
#[sqlx::test(migrations = "../../migrations")]
async fn a_run_waits_for_free_disk(pool: PgPool) {
    let free = Arc::new(AtomicU64::new(999));
    let (ctx, _dir) = context(&pool, free.clone());
    let fixture = Fixture::new(Catalogue::Musicbrainz, vec![]);
    *fixture.items.lock().unwrap() = cards(3)
        .into_iter()
        .map(|c| Card {
            uri: format!("https://musicbrainz.org/artist/{}", c.source_id),
            r#type: "MusicGroup".into(),
            ..c
        })
        .collect();
    let next = run::step(&fixture, &ctx).await.unwrap();
    assert_eq!(next, Next::Wait(ctx.pace.disk));
    let s = state(&pool, Catalogue::Musicbrainz).await;
    let waiting = s.waiting.unwrap();
    assert!(
        waiting.starts_with("free disk: the import needs"),
        "{waiting}"
    );
    assert_eq!((s.run, s.generation, s.phase.as_str()), (None, 0, "idle"));
    assert_eq!(count(&pool, Catalogue::Musicbrainz).await, 0);
    assert!(fixture.started_from.lock().unwrap().is_empty());
    // The large-import turn is not kept while it waits.
    let large = SessionLock::try_take(&pool, pinakes_core::locks::CATALOGUE_LARGE_IMPORT)
        .await
        .unwrap()
        .unwrap();
    large.release().await;
    free.store(1000, Ordering::SeqCst);
    // Planned again: the wait started nothing.
    *fixture.next.lock().unwrap() = Some(Run::Import);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Musicbrainz).await;
    assert_eq!((s.cards, s.waiting, s.generation), (3, None, 1));
}

/// One large full import at a time: while Wikidata's runs, OpenStreetMap's waits its turn; a
/// small catalogue's import and a large catalogue's update go ahead.
#[sqlx::test(migrations = "../../migrations")]
async fn large_full_imports_run_one_at_a_time(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let mut wikidata = Fixture::new(Catalogue::Wikidata, cards(15));
    let gate = Arc::new(Notify::new());
    wikidata.gate = Some(gate.clone());
    let wikidata = Arc::new(wikidata);
    let running = {
        let (wikidata, ctx) = (wikidata.clone(), ctx.clone());
        tokio::spawn(async move { run::step(&*wikidata, &ctx).await })
    };
    tokio::time::timeout(DEADLINE, wikidata.entered.notified())
        .await
        .unwrap();
    let place = |c: Card| Card {
        uri: format!("https://www.openstreetmap.org/node/{}", &c.source_id[1..]),
        r#type: "Place".into(),
        ..c
    };
    let osm = Fixture::new(Catalogue::Osm, cards(4).into_iter().map(place).collect());
    assert_eq!(
        run::step(&osm, &ctx).await.unwrap(),
        Next::Wait(ctx.pace.turn)
    );
    let s = state(&pool, Catalogue::Osm).await;
    assert!(s.waiting.unwrap().contains("one runs at a time"));
    assert_eq!((s.run, s.generation), (None, 0));
    // A small catalogue does not wait.
    let tranco = Fixture::new(
        Catalogue::Tranco,
        cards(2)
            .into_iter()
            .map(|c| Card {
                uri: format!("https://{}.example/", c.source_id.to_lowercase()),
                r#type: "WebSite".into(),
                ..c
            })
            .collect(),
    );
    assert_eq!(run::step(&tranco, &ctx).await.unwrap(), Next::Again);
    // Nor does a large catalogue's update.
    *osm.next.lock().unwrap() = Some(Run::Update);
    *osm.changes.lock().unwrap() = (cards(1).into_iter().map(place).collect(), vec![]);
    assert_eq!(run::step(&osm, &ctx).await.unwrap(), Next::Again);
    assert_eq!(count(&pool, Catalogue::Osm).await, 1);
    // Once Wikidata's import is done, OpenStreetMap's goes.
    gate.notify_one();
    assert_eq!(running.await.unwrap().unwrap(), Next::Again);
    *osm.next.lock().unwrap() = Some(Run::Import);
    assert_eq!(run::step(&osm, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Osm).await;
    assert_eq!((s.cards, s.waiting, s.generation), (4, None, 1));
}

/// The drop deletes a catalogue's cards and its state row, and nothing of another catalogue.
#[sqlx::test(migrations = "../../migrations")]
async fn a_drop_deletes_one_catalogue(pool: PgPool) {
    store::upsert(&pool, Catalogue::Wikidata, 1, &cards(30), None)
        .await
        .unwrap();
    let software: Vec<Card> = cards(3)
        .into_iter()
        .map(|c| Card {
            source: "flathub".into(),
            uri: format!("https://flathub.org/apps/{}", c.source_id),
            r#type: "SoftwareApplication".into(),
            ..c
        })
        .collect();
    store::upsert(&pool, Catalogue::Software, 1, &software, None)
        .await
        .unwrap();
    assert_eq!(state(&pool, Catalogue::Wikidata).await.cards, 30);
    let mut seen = vec![];
    let dropped = store::drop_catalogue(&pool, Catalogue::Wikidata, |n| seen.push(n))
        .await
        .unwrap();
    assert_eq!(dropped, 30);
    assert_eq!(seen.last(), Some(&30));
    assert_eq!(count(&pool, Catalogue::Wikidata).await, 0);
    assert!(store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .is_none());
    assert_eq!(count(&pool, Catalogue::Software).await, 3);
    assert_eq!(state(&pool, Catalogue::Software).await.cards, 3);
}

/// The batch writer: parts of at most `BATCH_ROWS`, one row per source ID (the last wins),
/// refused cards counted and left out, and the count in the state row.
#[sqlx::test(migrations = "../../migrations")]
async fn the_batch_writer_writes_in_bounded_parts(pool: PgPool) {
    let mut many = cards(store::BATCH_ROWS + 5);
    let mut dup = card(1);
    dup.name = "Item one again".into();
    many.push(dup);
    many.push(Card {
        r#type: "NoSuchType".into(),
        ..card(99_999)
    });
    let report = store::upsert(
        &pool,
        Catalogue::Wikidata,
        1,
        &many,
        Some(&json!({"at": 1})),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted, store::BATCH_ROWS as u64 + 5);
    assert_eq!(report.rejected, 1);
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!(s.cards, store::BATCH_ROWS as i64 + 5);
    assert_eq!(s.cursor, Some(json!({"at": 1})));
    let name: String = sqlx::query_scalar("SELECT name FROM catalogue_card WHERE source_id='Q1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Item one again");
    // A point is stored as PostGIS.
    let placed = Card {
        point: Some(pinakes_core::model::Point {
            lat: 47.37,
            lon: 8.54,
        }),
        ..card(2)
    };
    let report = store::upsert(&pool, Catalogue::Wikidata, 1, &[placed], None)
        .await
        .unwrap();
    assert_eq!((report.changed, report.inserted), (1, 0));
    let (lon, lat): (f64, f64) =
        sqlx::query_as("SELECT ST_X(point),ST_Y(point) FROM catalogue_card WHERE source_id='Q2'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((lon, lat), (8.54, 47.37));
}

/// The daemon's start sets each state row's phase from its switches: `disabled` while off, `idle`
/// again when on, with the run under way and its cursor kept.
#[sqlx::test(migrations = "../../migrations")]
async fn switches_set_the_phase_and_keep_the_run(pool: PgPool) {
    store::ensure_state(&pool, Catalogue::Osm).await.unwrap();
    sqlx::query("UPDATE catalogue_state SET run='import',phase='import',cursor='{\"next\":5}' WHERE catalogue='osm'")
        .execute(&pool)
        .await
        .unwrap();
    let mut config = CataloguesConfig::default();
    config.osm.enabled = false;
    store::sync_switches(&pool, &config).await.unwrap();
    let s = state(&pool, Catalogue::Osm).await;
    assert_eq!(
        (s.phase.as_str(), s.run.as_deref()),
        ("disabled", Some("import"))
    );
    // No row is made for a catalogue that never ran.
    assert!(store::state(&pool, Catalogue::Tranco)
        .await
        .unwrap()
        .is_none());
    store::sync_switches(&pool, &CataloguesConfig::default())
        .await
        .unwrap();
    let s = state(&pool, Catalogue::Osm).await;
    assert_eq!(
        (s.phase.as_str(), s.cursor),
        ("idle", Some(json!({"next": 5})))
    );
    // A step resumes it.
    let (ctx, _dir) = context(&pool, plenty());
    let fixture = Fixture::new(
        Catalogue::Osm,
        cards(8)
            .into_iter()
            .map(|c| Card {
                uri: format!("https://www.openstreetmap.org/node/{}", &c.source_id[1..]),
                r#type: "Place".into(),
                ..c
            })
            .collect(),
    );
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    assert_eq!(
        *fixture.started_from.lock().unwrap(),
        [Some(json!({"next": 5}))]
    );
    assert_eq!(count(&pool, Catalogue::Osm).await, 3);
}

/// The metrics: every catalogue's series from the start; cards, the last import and update,
/// the lag and the failed runs from the state rows; the switch from the configuration.
#[sqlx::test(migrations = "../../migrations")]
async fn catalogue_metrics_follow_the_state_and_the_switches(pool: PgPool) {
    let metrics = Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    let text = metrics.encode().unwrap();
    for catalogue in Catalogue::ALL {
        for series in [
            "pinakes_catalogue_cards",
            "pinakes_catalogue_enabled",
            "pinakes_catalogue_last_import_timestamp_seconds",
            "pinakes_catalogue_last_update_timestamp_seconds",
            "pinakes_catalogue_update_lag_seconds",
            "pinakes_catalogue_import_errors_total",
        ] {
            assert!(
                text.contains(&format!("{series}{{catalogue=\"{catalogue}\"}} 0")),
                "{series} {catalogue}"
            );
        }
    }
    let mut config = CataloguesConfig::default();
    config.tranco.enabled = false;
    metrics.catalogue_switches(&config);
    store::ensure_state(&pool, Catalogue::Wikidata)
        .await
        .unwrap();
    sqlx::query("UPDATE catalogue_state SET cards=42,error_count=3,last_import_at=now()-interval '3 days',last_update_at=now()-interval '1 hour',current_as_of=now()-interval '50 hours' WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    let metrics2 = Metrics::new().unwrap();
    metrics2.catalogue_switches(&config);
    metrics2.refresh(&pool).await.unwrap();
    let get = |gauge: &prometheus::IntGaugeVec, c: &str| gauge.with_label_values(&[c]).get();
    assert_eq!(get(&metrics2.catalogue_cards, "wikidata"), 42);
    assert_eq!(get(&metrics2.catalogue_enabled, "wikidata"), 1);
    assert_eq!(get(&metrics2.catalogue_enabled, "tranco"), 0);
    // The lag counts from how current the data is, not from the update that finished an hour
    // ago.
    let lag = get(&metrics2.catalogue_lag, "wikidata");
    assert!((50 * 3600..50 * 3600 + 60).contains(&lag), "{lag}");
    let now = chrono::Utc::now().timestamp();
    let import = get(&metrics2.catalogue_import, "wikidata");
    assert!((now - 3 * 86400 - 60..=now - 3 * 86400 + 60).contains(&import));
    assert!(get(&metrics2.catalogue_update, "wikidata") > import);
    assert_eq!(
        metrics2
            .catalogue_errors
            .with_label_values(&["wikidata"])
            .get(),
        3
    );
    // Before its first full import a catalogue has no lag.
    assert_eq!(get(&metrics2.catalogue_lag, "osm"), 0);
}

/// `pinakesd` starts the importers of the catalogues that are on, and none with every catalogue
/// off.
#[test]
fn only_the_importers_of_catalogues_that_are_on_start() {
    let all = || -> Vec<Arc<dyn Importer>> {
        Catalogue::ALL
            .into_iter()
            .map(|c| Arc::new(Fixture::new(c, vec![])) as Arc<dyn Importer>)
            .collect()
    };
    let names = |config: &Config| -> Vec<&str> {
        run::to_start(all(), config)
            .iter()
            .map(|i| i.catalogue().name())
            .collect()
    };
    let mut config = Config::default();
    assert_eq!(names(&config).len(), 7);
    config.catalogues.osm.enabled = false;
    config.catalogues.software.enabled = false;
    assert_eq!(
        names(&config),
        [
            "wikidata",
            "musicbrainz",
            "openalex",
            "podcastindex",
            "tranco"
        ]
    );
    config.catalogues.enabled = false;
    assert!(names(&config).is_empty());
    let started = AtomicUsize::new(names(&config).len());
    assert_eq!(started.load(Ordering::SeqCst), 0);
}

/// A large catalogue's full import cut off by a failure keeps the large-import turn until it
/// finishes: another large import waits though no import runs, unless the cut-off catalogue is
/// switched off.
#[sqlx::test(migrations = "../../migrations")]
async fn a_cut_off_large_import_keeps_its_turn(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let wikidata = Fixture::new(Catalogue::Wikidata, cards(25));
    *wikidata.fail_after.lock().unwrap() = Some(1);
    assert!(run::step(&wikidata, &ctx).await.is_err());
    assert_eq!(
        state(&pool, Catalogue::Wikidata).await.run.as_deref(),
        Some("import")
    );
    let place = |c: Card| Card {
        uri: format!("https://www.openstreetmap.org/node/{}", &c.source_id[1..]),
        r#type: "Place".into(),
        ..c
    };
    let osm = Fixture::new(Catalogue::Osm, cards(3).into_iter().map(place).collect());
    assert_eq!(
        run::step(&osm, &ctx).await.unwrap(),
        Next::Wait(ctx.pace.turn)
    );
    let s = state(&pool, Catalogue::Osm).await;
    assert!(
        s.waiting
            .as_deref()
            .unwrap()
            .starts_with("wikidata's full import is under way"),
        "{:?}",
        s.waiting
    );
    assert_eq!((s.run, s.generation), (None, 0));
    // The wait shows on `/metrics`: OpenStreetMap waits for the turn, Wikidata holds it.
    let metrics = Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    let turn = |c: &str| metrics.catalogue_turn.with_label_values(&[c]).get();
    assert_eq!((turn("osm"), turn("wikidata")), (1, 0));
    // The large-import lock is free again: nothing holds the turn but the state row.
    let large = SessionLock::try_take(&pool, pinakes_core::locks::CATALOGUE_LARGE_IMPORT)
        .await
        .unwrap()
        .unwrap();
    large.release().await;
    // Wikidata switched off: its cut-off import no longer holds the turn.
    let mut config = CataloguesConfig::default();
    config.wikidata.enabled = false;
    store::sync_switches(&pool, &config).await.unwrap();
    *osm.next.lock().unwrap() = Some(Run::Import);
    assert_eq!(run::step(&osm, &ctx).await.unwrap(), Next::Again);
    assert_eq!(count(&pool, Catalogue::Osm).await, 3);
    // On again, Wikidata resumes and finishes.
    store::sync_switches(&pool, &CataloguesConfig::default())
        .await
        .unwrap();
    assert_eq!(run::step(&wikidata, &ctx).await.unwrap(), Next::Again);
    assert_eq!(state(&pool, Catalogue::Wikidata).await.cards, 25);
}

/// Two large catalogues whose full imports were both cut off (one switched off and on again
/// while the other ran) do not wait for each other: the first in `Catalogue::ALL` resumes, then
/// the other.
#[sqlx::test(migrations = "../../migrations")]
async fn two_cut_off_large_imports_resume_one_after_the_other(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let place = |c: Card| Card {
        uri: format!("https://www.openstreetmap.org/node/{}", &c.source_id[1..]),
        r#type: "Place".into(),
        ..c
    };
    let wikidata = Fixture::new(Catalogue::Wikidata, cards(25));
    let osm = Fixture::new(Catalogue::Osm, cards(25).into_iter().map(place).collect());
    // Both cut off after a batch: OpenStreetMap's while Wikidata is switched off.
    *wikidata.fail_after.lock().unwrap() = Some(1);
    assert!(run::step(&wikidata, &ctx).await.is_err());
    let mut off = CataloguesConfig::default();
    off.wikidata.enabled = false;
    store::sync_switches(&pool, &off).await.unwrap();
    *osm.fail_after.lock().unwrap() = Some(1);
    assert!(run::step(&osm, &ctx).await.is_err());
    store::sync_switches(&pool, &CataloguesConfig::default())
        .await
        .unwrap();
    for catalogue in [Catalogue::Wikidata, Catalogue::Osm] {
        assert_eq!(state(&pool, catalogue).await.run.as_deref(), Some("import"));
    }
    // OpenStreetMap waits for Wikidata, which goes.
    assert_eq!(
        run::step(&osm, &ctx).await.unwrap(),
        Next::Wait(ctx.pace.turn)
    );
    assert!(state(&pool, Catalogue::Osm)
        .await
        .waiting
        .unwrap()
        .starts_with("wikidata's"));
    assert_eq!(run::step(&wikidata, &ctx).await.unwrap(), Next::Again);
    assert_eq!(run::step(&osm, &ctx).await.unwrap(), Next::Again);
    for catalogue in [Catalogue::Wikidata, Catalogue::Osm] {
        let s = state(&pool, catalogue).await;
        assert_eq!((s.cards, s.run), (25, None), "{catalogue}");
    }
}

/// A row the table's checks refuse is left out alone: the rest of its part is written.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_the_table_refuses_does_not_stop_its_part(pool: PgPool) {
    // Checked as the table checks it, a card over the details cap never reaches it.
    let mut big = card(3);
    for n in 0..800 {
        big.details.insert(format!("k{n:03}"), json!(1));
    }
    let mut part = cards(5);
    part[3] = big;
    let report = store::upsert(&pool, Catalogue::Wikidata, 1, &part, None)
        .await
        .unwrap();
    assert_eq!((report.inserted, report.rejected), (4, 1));
    // The measure agrees with PostgreSQL's own.
    for value in [
        json!({"a": 1, "b": [1, 2.5, -3], "c": {"d": null, "e": "x\"y\u{e9}\n"}}),
        json!({"big": 1e30, "small": 1.5e-7, "neg": -12345678901234i64}),
        json!([2.0, 0.5, 100.0, 1e-3]),
    ] {
        let printed: i32 = sqlx::query_scalar("SELECT octet_length($1::jsonb::text)")
            .bind(&value)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            pinakes_core::catalogue::jsonb_text_len(&value),
            printed as usize,
            "{value}"
        );
    }
}

/// How current the cards are is what the importer says (a dump's date, the time of the last
/// change applied), and the update lag counts from it: an update that says nothing leaves it
/// where it was. An update can hand over to a full import, which starts in a new generation
/// without marking the data current.
#[sqlx::test(migrations = "../../migrations")]
async fn current_as_of_follows_upstream_and_an_update_can_ask_for_a_full_import(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let at = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
    let fixture = Fixture::new(Catalogue::Wikidata, cards(5));
    *fixture.current.lock().unwrap() = Some(at("2026-11-20T00:00:00Z"));
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    assert_eq!(
        state(&pool, Catalogue::Wikidata).await.current_as_of,
        Some(at("2026-11-20T00:00:00Z"))
    );
    // An update that finds nothing new and says nothing: finished, the data no more current.
    *fixture.current.lock().unwrap() = None;
    *fixture.next.lock().unwrap() = Some(Run::Update);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Wikidata).await;
    assert!(s.last_update_at.is_some());
    assert_eq!(s.current_as_of, Some(at("2026-11-20T00:00:00Z")));
    let metrics = Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    let lag = metrics.catalogue_lag.with_label_values(&["wikidata"]).get();
    let behind = (chrono::Utc::now() - at("2026-11-20T00:00:00Z")).num_seconds();
    assert!((behind - 60..=behind + 60).contains(&lag), "{lag} {behind}");
    // An update that applied changes up to a time: current as of it.
    *fixture.current.lock().unwrap() = Some(at("2026-11-22T00:00:00Z"));
    *fixture.next.lock().unwrap() = Some(Run::Update);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    assert_eq!(
        state(&pool, Catalogue::Wikidata).await.current_as_of,
        Some(at("2026-11-22T00:00:00Z"))
    );
    // An update that can't apply the changes asks for a full import: nothing marked current,
    // the update not counted as finished, a new generation begun.
    let before = state(&pool, Catalogue::Wikidata).await;
    *fixture.current.lock().unwrap() = Some(at("2026-11-25T00:00:00Z"));
    *fixture.update_end.lock().unwrap() =
        UpdateEnd::NeedsImport("the change stream is more than five days behind");
    *fixture.next.lock().unwrap() = Some(Run::Update);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!(
        (s.run.as_deref(), s.phase.as_str()),
        (Some("import"), "import")
    );
    assert_eq!(s.generation, before.generation + 1);
    assert_eq!(
        (s.current_as_of, s.last_update_at),
        (before.current_as_of, before.last_update_at)
    );
    // The next step runs that import from the start, unasked.
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!(fixture.started_from.lock().unwrap().last(), Some(&None));
    assert_eq!((s.run, s.generation, s.cards), (None, 2, 5));
    assert_eq!(s.current_as_of, Some(at("2026-11-25T00:00:00Z")));
    // A full import that says nothing is current as of its end.
    *fixture.current.lock().unwrap() = None;
    *fixture.next.lock().unwrap() = Some(Run::Import);
    assert_eq!(run::step(&fixture, &ctx).await.unwrap(), Next::Again);
    let current = state(&pool, Catalogue::Wikidata)
        .await
        .current_as_of
        .unwrap();
    assert!((chrono::Utc::now() - current).num_seconds() < 60);
}

/// A run whose catalogue lock's session ends (an administrator, a database restart) is cut off
/// as a failure (`lock_lost`), and the task takes the lock again and resumes from the cursor.
#[sqlx::test(migrations = "../../migrations")]
async fn a_run_that_loses_its_lock_stops_and_resumes(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let mut fixture = Fixture::new(Catalogue::Wikidata, cards(25));
    let gate = Arc::new(Notify::new());
    fixture.gate = Some(gate.clone());
    let fixture = Arc::new(fixture);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(run::run(fixture.clone(), ctx.clone(), shutdown));
    tokio::time::timeout(DEADLINE, fixture.entered.notified())
        .await
        .unwrap();
    // End the session that holds the catalogue's lock.
    let ended: bool = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype='advisory' AND granted AND objid=($1::bigint)::oid",
    )
    .bind(Catalogue::Wikidata.lock_key())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(ended);
    until("the lost lock recorded", || async {
        state(&pool, Catalogue::Wikidata).await.error_count == 1
    })
    .await;
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!(s.last_error.as_deref(), Some("lock_lost"));
    assert_eq!(s.cursor, Some(json!({"next": 10})));
    // Let the resumed run past its first batch.
    gate.notify_one();
    until("the resumed import", || async {
        state(&pool, Catalogue::Wikidata)
            .await
            .last_import_at
            .is_some()
    })
    .await;
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(
        fixture.started_from.lock().unwrap()[..2],
        [None, Some(json!({"next": 10}))]
    );
    let s = state(&pool, Catalogue::Wikidata).await;
    assert_eq!((s.cards, s.error_count, s.phase.as_str()), (25, 1, "idle"));
}

/// An importer that panics fails its run (`panic`), recorded like any failure: the task goes on
/// and the run is tried again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_panicking_importer_is_a_failed_run(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let fixture = Fixture::new(
        Catalogue::Tranco,
        cards(3)
            .into_iter()
            .map(|c| Card {
                uri: format!("https://{}.example/", c.source_id.to_lowercase()),
                r#type: "WebSite".into(),
                ..c
            })
            .collect(),
    );
    fixture.panic_once.store(true, Ordering::SeqCst);
    let fixture = Arc::new(fixture);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(run::run(fixture.clone(), ctx.clone(), shutdown));
    until("the import after the panic", || async {
        store::state(&pool, Catalogue::Tranco)
            .await
            .unwrap()
            .is_some_and(|s| s.last_import_at.is_some())
    })
    .await;
    stop.send(true).unwrap();
    task.await.unwrap();
    let s = state(&pool, Catalogue::Tranco).await;
    assert_eq!((s.error_count, s.last_error.as_deref()), (1, Some("panic")));
    assert_eq!(s.cards, 3);
}

/// A failure is stored and logged by its class: an importer's error text never reaches
/// `last_error`.
#[sqlx::test(migrations = "../../migrations")]
async fn a_failure_is_stored_by_its_class(pool: PgPool) {
    let (ctx, _dir) = context(&pool, plenty());
    let fixture = Fixture::new(Catalogue::Tranco, vec![]);
    *fixture.items.lock().unwrap() = cards(25)
        .into_iter()
        .map(|c| Card {
            uri: format!("https://{}.example/", c.source_id.to_lowercase()),
            r#type: "WebSite".into(),
            ..c
        })
        .collect();
    *fixture.fail_after.lock().unwrap() = Some(1);
    let fixture = Arc::new(fixture);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(run::run(fixture.clone(), ctx.clone(), shutdown));
    until("the failure recorded", || async {
        store::state(&pool, Catalogue::Tranco)
            .await
            .unwrap()
            .is_some_and(|s| s.error_count == 1)
    })
    .await;
    stop.send(true).unwrap();
    task.await.unwrap();
    let s = state(&pool, Catalogue::Tranco).await;
    // The fixture's error text ("the fixture dump broke off") stands for an upstream's.
    assert_eq!(s.last_error.as_deref(), Some("unclassified"));
}
