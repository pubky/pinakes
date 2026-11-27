//! The OpenAlex, Podcast Index, software and Tranco importers, each run through the daemon's own
//! runner (`catalogue::run`) over a fresh database, reading recorded real records served by a
//! local HTTP server or from local files: the first import (cards, keys, labels, links), an
//! update that changes only what changed, an update refused for removing too much (parked, the
//! file not read again), and the joins of shared links with the cards.
mod common;

use pinakes_core::{
    catalogue::{
        fetch::Input,
        join, openalex, podcastindex,
        run::{self, Context, Pace, Run},
        software, store, tranco, Catalogue,
    },
    config::Config,
};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Each path's body and ETag.
type Served = HashMap<String, (Vec<u8>, String)>;
/// A local HTTP server of files: each path's body and ETag; it counts GETs and HEADs per path.
#[derive(Clone, Default)]
struct Files {
    files: Arc<Mutex<Served>>,
    gets: Arc<Mutex<HashMap<String, usize>>>,
    heads: Arc<Mutex<HashMap<String, usize>>>,
}
impl Files {
    fn put(&self, path: &str, body: Vec<u8>, etag: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), (body, format!("\"{etag}\"")));
    }
    fn gets(&self, path: &str) -> usize {
        *self.gets.lock().unwrap().get(path).unwrap_or(&0)
    }
    fn heads(&self, path: &str) -> usize {
        *self.heads.lock().unwrap().get(path).unwrap_or(&0)
    }
    async fn serve(&self) -> String {
        use axum::{http::StatusCode, response::IntoResponse};
        let files = self.clone();
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri| {
                let files = files.clone();
                async move {
                    let path = uri.path().to_owned();
                    let found = files.files.lock().unwrap().get(&path).cloned();
                    let Some((body, etag)) = found else {
                        return StatusCode::NOT_FOUND.into_response();
                    };
                    if method == axum::http::Method::GET {
                        *files.gets.lock().unwrap().entry(path).or_default() += 1;
                    } else if method == axum::http::Method::HEAD {
                        *files.heads.lock().unwrap().entry(path).or_default() += 1;
                    }
                    let body = if method == axum::http::Method::HEAD {
                        vec![]
                    } else {
                        body
                    };
                    ([(axum::http::header::ETAG, etag)], body).into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }
}

fn fixture(dir: &str, name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/catalogue/{dir}/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes).unwrap();
    gz.finish().unwrap()
}
/// A tar archive of `files`, as the dumps pack them.
fn tar(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, body) in files {
        let mut h = [0u8; 512];
        h[..path.len()].copy_from_slice(path.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        h[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
        h[156] = b'0';
        h[257..265].copy_from_slice(b"ustar  \0");
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend(h);
        out.extend(*body);
        out.extend(vec![0u8; (512 - body.len() % 512) % 512]);
    }
    out.extend([0u8; 1024]);
    out
}
fn context(pool: &PgPool, dir: &std::path::Path, edit: impl FnOnce(&mut Config)) -> Context {
    let mut config = Config::default();
    config.catalogues.dir = dir.to_string_lossy().into_owned();
    config.egress.contact = "test@example.org".into();
    edit(&mut config);
    let mut ctx = Context::new(pool.clone(), Arc::new(config));
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    ctx.pace = Pace {
        retry: Duration::from_millis(20),
        disk: Duration::from_millis(20),
        turn: Duration::from_millis(20),
        lock_check: Duration::from_millis(200),
    };
    ctx
}
async fn count(pool: &PgPool, catalogue: &str, source: Option<&str>) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM catalogue_card WHERE catalogue=$1 AND ($2::text IS NULL OR source=$2)")
        .bind(catalogue)
        .bind(source)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// Each card's source ID with its row version (`xmin`, and `xmax`, which a lock sets): unchanged
/// only when the row was neither written nor locked.
async fn versions(pool: &PgPool, catalogue: &str) -> HashMap<String, String> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT source_id,xmin::text||'/'||xmax::text FROM catalogue_card WHERE catalogue=$1",
    )
    .bind(catalogue)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .collect()
}
async fn row(
    pool: &PgPool,
    catalogue: &str,
    source_id: &str,
) -> (String, String, Vec<String>, Vec<String>, Vec<String>, f32) {
    sqlx::query_as("SELECT uri,name,keys,labels,related_uris,fame FROM catalogue_card WHERE catalogue=$1 AND source_id=$2")
        .bind(catalogue)
        .bind(source_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// --- software ------------------------------------------------------------------------------------

/// A crates.io dump of the fixture crates, plus `extra` crates named `extra-{n}`.
fn crates_dump(extra: usize, serde_description: &str) -> Vec<u8> {
    let mut crates = String::from_utf8(fixture("software", "crates.csv")).unwrap();
    crates = crates.replace(
        "A generic serialization/deserialization framework",
        serde_description,
    );
    for n in 0..extra {
        crates.push_str(&format!(
            "2020-01-01 00:00:00+00,Extra crate {n},,,{},,,extra-{n},,,f,2026-01-01 00:00:00+00\n",
            900_000 + n
        ));
    }
    gzip(&tar(&[
        ("2026-11-26-020015/metadata.json", b"{}"),
        (
            "2026-11-26-020015/data/categories.csv",
            &fixture("software", "categories.csv"),
        ),
        (
            "2026-11-26-020015/data/crate_downloads.csv",
            &fixture("software", "crate_downloads.csv"),
        ),
        ("2026-11-26-020015/data/crates.csv", crates.as_bytes()),
        (
            "2026-11-26-020015/data/keywords.csv",
            &fixture("software", "keywords.csv"),
        ),
        (
            "2026-11-26-020015/data/crates_categories.csv",
            &fixture("software", "crates_categories.csv"),
        ),
        (
            "2026-11-26-020015/data/crates_keywords.csv",
            &fixture("software", "crates_keywords.csv"),
        ),
    ]))
}
/// F-Droid's entry for `index`.
fn fdroid_entry(index: &[u8], timestamp: i64) -> Vec<u8> {
    use sha2::Digest;
    let sha: String = sha2::Sha256::digest(index)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    serde_json::json!({"timestamp": timestamp, "version": 30000, "index": {"name": "/index-v2.json", "sha256": sha, "size": index.len(), "numPackages": 3}})
        .to_string()
        .into_bytes()
}
async fn software_server(files: &Files, extra: usize) -> software::Sources {
    files.put(
        "/db-dump.tar.gz",
        crates_dump(extra, "A generic serialization/deserialization framework"),
        "c1",
    );
    let index = fixture("software", "fdroid-index-v2.json");
    files.put(
        "/repo/entry.json",
        fdroid_entry(&index, 1_795_453_653_400),
        "e1",
    );
    files.put("/repo/index-v2.json", index, "i1");
    files.put(
        "/appstream.xml.gz",
        gzip(&fixture("software", "flathub-appstream.xml")),
        "f1",
    );
    let base = files.serve().await;
    let at = |p: &str| Input::Url(url::Url::parse(&format!("{base}{p}")).unwrap());
    software::Sources {
        crates: at("/db-dump.tar.gz"),
        fdroid: at("/repo/entry.json"),
        flathub: at("/appstream.xml.gz"),
    }
}

/// The software catalogue's first import writes a card per crate and app with the package page
/// as its link, the registry key, labels and related links; an update with nothing new writes
/// nothing; a changed F-Droid index rewrites only the app that changed; a shared package page
/// joins its card.
#[sqlx::test(migrations = "../../migrations")]
async fn software_imports_and_updates_only_what_changed(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let files = Files::default();
    let sources = software_server(&files, 0).await;
    let importer = Arc::new(software::Software::new(sources, "test@example.org").unwrap());
    let (state, written) = run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "software", Some("crates.io")).await, 6);
    assert_eq!(count(&pool, "software", Some("f-droid")).await, 3);
    // Three applications; the runtime and the add-on are no cards.
    assert_eq!(count(&pool, "software", Some("flathub")).await, 3);
    assert_eq!(state.cards, 12);
    assert_eq!(written.inserted, 12);
    let (uri, name, keys, labels, related, fame) = row(&pool, "software", "crates.io/serde").await;
    assert_eq!(
        (uri.as_str(), name.as_str()),
        ("https://crates.io/crates/serde", "serde")
    );
    assert!(keys.contains(&"crate:serde".to_owned()), "{keys:?}");
    assert!(
        labels.contains(&"serde".to_owned()) && labels.contains(&"encoding".to_owned()),
        "{labels:?}"
    );
    assert!(
        related
            .iter()
            .any(|r| r.starts_with("https://github.com/serde-rs/serde")),
        "{related:?}"
    );
    assert!(fame > 0.5);
    let (uri, _, keys, labels, _, _) = row(&pool, "software", "f-droid/org.fdroid.fdroid").await;
    assert_eq!(uri, "https://f-droid.org/packages/org.fdroid.fdroid");
    assert!(keys.contains(&"android:org.fdroid.fdroid".to_owned()));
    assert!(!labels.is_empty());
    let (uri, name, keys, labels, _, _) = row(&pool, "software", "flathub/org.gnome.Maps").await;
    assert_eq!(
        (uri.as_str(), name.as_str()),
        ("https://flathub.org/apps/org.gnome.Maps", "Maps")
    );
    assert!(keys.contains(&"flathub:org.gnome.Maps".to_owned()));
    assert!(labels.contains(&"maps".to_owned()), "{labels:?}");
    let state = store::state(&pool, Catalogue::Software)
        .await
        .unwrap()
        .unwrap();
    assert!(state.current_as_of.is_some());
    let position = state.position.unwrap();
    for registry in ["crates.io", "f-droid", "flathub"] {
        assert!(position[registry]["version"].is_string(), "{position}");
    }

    // Nothing new: nothing is read or written.
    let before = versions(&pool, "software").await;
    let gets = files.gets("/db-dump.tar.gz");
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(versions(&pool, "software").await, before);
    assert_eq!(files.gets("/db-dump.tar.gz"), gets);
    // A registry's file that names a time ahead of now (its own word) counts as now: the cards
    // are never more current than the clock, so a wrong date can't hide the update lag.
    sqlx::query("UPDATE catalogue_state SET position=jsonb_set(position,'{flathub,at}','\"2099-01-01T00:00:00Z\"') WHERE catalogue='software'")
        .execute(&pool)
        .await
        .unwrap();
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    let as_of = state_of(&pool, Catalogue::Software)
        .await
        .current_as_of
        .unwrap();
    assert!(as_of <= chrono::Utc::now(), "{as_of}");

    // A new F-Droid index with one app's name changed: that card alone is rewritten.
    let index = String::from_utf8(fixture("software", "fdroid-index-v2.json"))
        .unwrap()
        .replacen("\"AntennaPod - Podcast Player\"", "\"AntennaPod 4\"", 1)
        .into_bytes();
    files.put(
        "/repo/entry.json",
        fdroid_entry(&index, 1_795_500_000_000),
        "e2",
    );
    files.put("/repo/index-v2.json", index, "i2");
    let (_, written) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(
        (written.changed, written.inserted, written.removed),
        (1, 0, 0),
        "{written:?}"
    );
    let after = versions(&pool, "software").await;
    let moved: Vec<&String> = before.keys().filter(|k| before[*k] != after[*k]).collect();
    assert_eq!(moved, [&"f-droid/de.danoeh.antennapod".to_owned()]);
    assert_eq!(
        files.gets("/db-dump.tar.gz"),
        gets,
        "the crates dump is not read again"
    );

    // A shared package page joins its card.
    let found = join::find(&pool, &["software"], "https://crates.io/crates/serde")
        .await
        .unwrap();
    assert_eq!(found.card.unwrap().source_id, "crates.io/serde");
    let found = join::find(
        &pool,
        &["software"],
        "https://f-droid.org/en/packages/org.fdroid.fdroid/",
    )
    .await
    .unwrap();
    assert_eq!(
        found.card.map(|c| c.source_id).as_deref(),
        Some("f-droid/org.fdroid.fdroid")
    );
    // A related link (the repository) joins the group only.
    let found = join::find(&pool, &["software"], "https://github.com/serde-rs/serde")
        .await
        .unwrap();
    assert!(found.card.is_none());
}

/// The state row of `catalogue`.
async fn state_of(pool: &PgPool, catalogue: Catalogue) -> store::State {
    store::state(pool, catalogue).await.unwrap().unwrap()
}
/// The last full import and update a day and more ago, so the task's plan runs an update now.
async fn age(pool: &PgPool, catalogue: Catalogue) {
    sqlx::query("UPDATE catalogue_state SET last_import_at=now()-interval '2 days',last_update_at=NULL WHERE catalogue=$1")
        .bind(catalogue.name())
        .execute(pool)
        .await
        .unwrap();
}

/// An update that would remove more than a tenth of a registry's cards is refused: nothing is
/// removed, the run is abandoned and its failure counted once, and the position names the file;
/// the task's next update finishes without reading it again or failing; a newer file that
/// removes few is applied; a full import asked for (`--request`) while a refusal stands runs.
#[sqlx::test(migrations = "../../migrations")]
async fn a_software_update_that_removes_too_much_is_refused_once(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let files = Files::default();
    let sources = software_server(&files, 60).await;
    let importer = Arc::new(software::Software::new(sources, "test@example.org").unwrap());
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "software", Some("crates.io")).await, 66);
    age(&pool, Catalogue::Software).await;
    // A broken dump: 40 crates gone.
    files.put("/db-dump.tar.gz", crates_dump(20, "Serialization"), "c2");
    let failed = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(
        failed.to_string().contains("refused (crates.io file"),
        "{failed}"
    );
    assert_eq!(count(&pool, "software", Some("crates.io")).await, 66);
    let state = state_of(&pool, Catalogue::Software).await;
    assert_eq!((state.run.as_deref(), state.cursor.as_ref()), (None, None));
    assert_eq!(state.error_count, 1);
    let position = state.position.clone().unwrap();
    assert_eq!(position["refused"]["crates.io"], "\"c2\"", "{position}");
    // Read from the test's files, not the registries' own.
    assert!(position["refused_source"].is_string(), "{position}");
    // The registries' versions are kept: the next update starts from them.
    assert_eq!(position["crates.io"]["version"], "\"c1\"", "{position}");
    // The task's next step: an update that reads nothing, finishes and fails nothing.
    let gets = files.gets("/db-dump.tar.gz");
    run::step(&*importer, &ctx).await.unwrap();
    assert_eq!(files.gets("/db-dump.tar.gz"), gets);
    let state = state_of(&pool, Catalogue::Software).await;
    assert_eq!(state.error_count, 1);
    assert!(state.last_update_at.is_some());
    assert_eq!(
        state.position.as_ref().unwrap()["refused"]["crates.io"],
        "\"c2\""
    );
    // And the next day's too.
    age(&pool, Catalogue::Software).await;
    run::step(&*importer, &ctx).await.unwrap();
    assert_eq!(files.gets("/db-dump.tar.gz"), gets);
    assert_eq!(state_of(&pool, Catalogue::Software).await.error_count, 1);
    // A newer dump with five crates gone (within the floor) is applied; the refusal goes.
    files.put("/db-dump.tar.gz", crates_dump(55, "Serialization"), "c3");
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "software", Some("crates.io")).await, 61);
    let state = state_of(&pool, Catalogue::Software).await;
    assert_eq!(state.run, None);
    assert!(state.position.as_ref().unwrap().get("refused").is_none());
    // F-Droid and Flathub were left as they were.
    assert_eq!(count(&pool, "software", Some("f-droid")).await, 3);
    // Refused again (15 of 61 crates gone); then a full import asked for while the refusal
    // stands: the task runs it (it reads more than half the cards: no shrink check stops it).
    files.put("/db-dump.tar.gz", crates_dump(40, "Serialization"), "c4");
    age(&pool, Catalogue::Software).await;
    run::step(&*importer, &ctx).await.unwrap_err();
    assert!(state_of(&pool, Catalogue::Software)
        .await
        .position
        .unwrap()
        .get("refused")
        .is_some());
    run::request_import(
        &pool,
        Catalogue::Software,
        pinakes_core::catalogue::run::REQUESTED,
    )
    .await
    .unwrap();
    run::step(&*importer, &ctx).await.unwrap();
    assert_eq!(count(&pool, "software", Some("crates.io")).await, 46);
    let state = state_of(&pool, Catalogue::Software).await;
    assert!(state.import_requested.is_none());
    assert!(state.position.as_ref().unwrap().get("refused").is_none());
    assert_eq!(state.run, None);
}

/// A full import of a registry whose file holds no card is refused (`empty`) and removes
/// nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_software_import_that_reads_no_card_fails(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let files = Files::default();
    let sources = software_server(&files, 0).await;
    let importer = Arc::new(software::Software::new(sources, "test@example.org").unwrap());
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    // A Flathub catalogue with no application in it.
    files.put(
        "/appstream.xml.gz",
        gzip(
            b"<?xml version=\"1.0\"?><components version=\"0.16\" origin=\"flathub\"></components>",
        ),
        "f2",
    );
    let failed = run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap_err();
    assert!(
        failed.to_string().contains("empty (flathub file"),
        "{failed}"
    );
    assert_eq!(count(&pool, "software", Some("flathub")).await, 3);
    // Refused: the file is named in the position, with the others' versions.
    let position = state_of(&pool, Catalogue::Software).await.position.unwrap();
    assert_eq!(position["refused"]["flathub"], "\"f2\"", "{position}");
    assert!(position.get("crates.io").is_some(), "{position}");
}

// --- Podcast Index -------------------------------------------------------------------------------

fn podcast_archive(db: &[u8]) -> Vec<u8> {
    gzip(&tar(&[("./podcastindex_feeds.db", db)]))
}

/// The Podcast Index database's live feeds become cards with their podcast keys; the same
/// archive again writes nothing; a new archive rewrites the changed feed and removes the gone
/// one; the unpacked database is deleted after each run; a feed link joins the card's group
/// without being built from it.
#[sqlx::test(migrations = "../../migrations")]
async fn podcast_index_imports_and_updates(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let files = Files::default();
    let db = fixture("podcastindex", "podcastindex_feeds.db");
    files.put("/podcastindex_feeds.db.tgz", podcast_archive(&db), "p1");
    let base = files.serve().await;
    let input = Input::Url(url::Url::parse(&format!("{base}/podcastindex_feeds.db.tgz")).unwrap());
    let importer = Arc::new(podcastindex::PodcastIndex::new(input, "test@example.org").unwrap());
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "podcastindex", None).await, 11);
    let (uri, name, keys, _, _, _) = row(&pool, "podcastindex", "1").await;
    assert_eq!(
        uri,
        "https://rss.libsyn.com/shows/60231/destinations/278964.xml"
    );
    assert_eq!(name, "Christianity Questions and Answers");
    assert!(
        keys.contains(&"podcast:guid:78e9f51b-0ac3-5711-a921-1f2c7b5dbb35".to_owned()),
        "{keys:?}"
    );
    assert!(
        keys.contains(&"podcast:apple:1000000618".to_owned()),
        "{keys:?}"
    );
    assert!(keys.contains(
        &pinakes_core::catalogue::link_key("https://podcastindex.org/podcast/1").unwrap()
    ));
    let work = dir.path().join("podcastindex");
    assert!(
        !work.join("podcastindex_feeds.db").exists(),
        "the database is deleted after the run"
    );

    // The same archive: nothing to do.
    let before = versions(&pool, "podcastindex").await;
    let gets = files.gets("/podcastindex_feeds.db.tgz");
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(files.gets("/podcastindex_feeds.db.tgz"), gets);
    assert_eq!(versions(&pool, "podcastindex").await, before);

    // A new archive: feed 2 renamed, feed 4 gone.
    let edited = dir.path().join("edited.db");
    std::fs::write(&edited, &db).unwrap();
    {
        let c = rusqlite::Connection::open(&edited).unwrap();
        c.execute(
            "UPDATE podcasts SET title='Rahdo Talks Through (new)' WHERE id=2",
            [],
        )
        .unwrap();
        c.execute("DELETE FROM podcasts WHERE id=4", []).unwrap();
    }
    files.put(
        "/podcastindex_feeds.db.tgz",
        podcast_archive(&std::fs::read(&edited).unwrap()),
        "p2",
    );
    let (_, written) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(
        (written.changed, written.removed, written.inserted),
        (1, 1, 0),
        "{written:?}"
    );
    let after = versions(&pool, "podcastindex").await;
    for (id, version) in &before {
        if id != "2" && id != "4" {
            assert_eq!(after.get(id), Some(version), "{id} rewritten");
        }
    }
    assert!(!after.contains_key("4"));

    // A feed link is a related link of its card (fetched for its episodes), never built from it.
    let found = join::find(
        &pool,
        &["podcastindex"],
        "https://rss.libsyn.com/shows/60231/destinations/278964.xml",
    )
    .await
    .unwrap();
    assert!(found.card.is_none());
    assert_eq!(found.related, ["catalogue:podcastindex:1"]);
    // So is the show's Apple Podcasts page.
    let found = join::find(
        &pool,
        &["podcastindex"],
        "https://podcasts.apple.com/us/podcast/x/id1000000618",
    )
    .await
    .unwrap();
    assert_eq!(found.related, ["catalogue:podcastindex:1"]);
}

/// A copy of the fixture database with `sql` run on it.
fn edited_db(dir: &std::path::Path, name: &str, sql: &str) -> Vec<u8> {
    let path = dir.join(name);
    std::fs::write(&path, fixture("podcastindex", "podcastindex_feeds.db")).unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
    std::fs::read(&path).unwrap()
}

/// A Podcast Index update that would remove every feed is refused once: nothing is removed, the
/// unpacked database is deleted, and the task's next update does not download that archive
/// again (it ends at once, recording nothing); a full import asked for then reads it, and is
/// refused (`empty`) on a database with no live feed, removing nothing; a newer archive is then
/// read by the next update.
#[sqlx::test(migrations = "../../migrations")]
async fn a_podcast_index_update_that_removes_too_much_is_refused_once(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let files = Files::default();
    let db = fixture("podcastindex", "podcastindex_feeds.db");
    files.put("/podcastindex_feeds.db.tgz", podcast_archive(&db), "p1");
    let base = files.serve().await;
    let input = Input::Url(url::Url::parse(&format!("{base}/podcastindex_feeds.db.tgz")).unwrap());
    let importer = Arc::new(
        podcastindex::PodcastIndex::new(input, "test@example.org")
            .unwrap()
            .configured(&ctx.config),
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "podcastindex", None).await, 11);
    age(&pool, Catalogue::Podcastindex).await;
    let empty = edited_db(dir.path(), "empty.db", "UPDATE podcasts SET dead=1");
    files.put("/podcastindex_feeds.db.tgz", podcast_archive(&empty), "p2");
    let failed = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(
        failed.to_string().contains("refused (database \"p2\""),
        "{failed}"
    );
    assert_eq!(count(&pool, "podcastindex", None).await, 11);
    let work = ctx.config.catalogues.work_dir(Catalogue::Podcastindex);
    assert!(
        !work.join("podcastindex_feeds.db").exists(),
        "deleted on refusal"
    );
    let state = state_of(&pool, Catalogue::Podcastindex).await;
    assert_eq!((state.run.as_deref(), state.error_count), (None, 1));
    let position = state.position.clone().unwrap();
    assert_eq!(position["refused"], "\"p2\"");
    assert_eq!(
        position["refused_source"],
        format!("{base}/podcastindex_feeds.db.tgz")
    );
    let table = store::status_table(&ctx.config.catalogues, std::slice::from_ref(&state));
    assert!(table.contains("refused: \"p2\""), "{table}");
    // The task's next update: no download, no failure.
    let gets = files.gets("/podcastindex_feeds.db.tgz");
    let again = run::step(&*importer, &ctx).await.unwrap_err();
    assert!(again.is::<run::StillRefused>(), "{again}");
    assert_eq!(files.gets("/podcastindex_feeds.db.tgz"), gets);
    assert_eq!(
        state_of(&pool, Catalogue::Podcastindex).await.error_count,
        1
    );
    // A full import asked for: it runs, finds no live feed and is refused, removing nothing.
    run::request_import(
        &pool,
        Catalogue::Podcastindex,
        pinakes_core::catalogue::run::REQUESTED,
    )
    .await
    .unwrap();
    let failed = run::step(&*importer, &ctx).await.unwrap_err();
    assert!(
        pinakes_core::catalogue::failure::shown(&failed).starts_with("empty"),
        "{failed}"
    );
    assert_eq!(count(&pool, "podcastindex", None).await, 11);
    assert_eq!(files.gets("/podcastindex_feeds.db.tgz"), gets + 1);
    assert!(state_of(&pool, Catalogue::Podcastindex)
        .await
        .import_requested
        .is_none());
    // A newer archive, one feed gone: the next update takes it.
    let fewer = edited_db(dir.path(), "fewer.db", "DELETE FROM podcasts WHERE id=4");
    files.put("/podcastindex_feeds.db.tgz", podcast_archive(&fewer), "p3");
    run::step(&*importer, &ctx).await.unwrap();
    assert_eq!(count(&pool, "podcastindex", None).await, 10);
    let state = state_of(&pool, Catalogue::Podcastindex).await;
    assert!(state.import_requested.is_none());
    assert_eq!(
        state.position.unwrap(),
        serde_json::json!({"version": "\"p3\""})
    );
}

/// An update's refused cards (a record that makes no card the table takes) and unchanged cards
/// are counted as a full import counts them (`pinakes_catalogue_cards_total`); a refused card's
/// stored copy stays.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_counts_its_refused_and_unchanged_cards(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let files = Files::default();
    let db = fixture("podcastindex", "podcastindex_feeds.db");
    files.put("/podcastindex_feeds.db.tgz", podcast_archive(&db), "p1");
    let base = files.serve().await;
    let input = Input::Url(url::Url::parse(&format!("{base}/podcastindex_feeds.db.tgz")).unwrap());
    let importer = Arc::new(podcastindex::PodcastIndex::new(input, "test@example.org").unwrap());
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    let counted =
        |outcome| pinakes_core::telemetry::catalogue_cards_counted("podcastindex", outcome);
    let (rejected, unchanged) = (counted("rejected"), counted("unchanged"));
    // Feed 2's link becomes a post on a social site: no card's own link.
    let edited = edited_db(
        dir.path(),
        "social.db",
        "UPDATE podcasts SET url='https://x.com/someone/status/1795000000000000000' WHERE id=2",
    );
    files.put("/podcastindex_feeds.db.tgz", podcast_archive(&edited), "p2");
    let (_, written) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!((written.rejected, written.removed), (1, 0), "{written:?}");
    assert!(counted("rejected") > rejected);
    assert!(counted("unchanged") >= unchanged + 10);
    assert_eq!(count(&pool, "podcastindex", None).await, 11);
}

// --- OpenAlex ------------------------------------------------------------------------------------

/// A local snapshot: a manifest of `date` and its works files (`(day, lines)`).
fn snapshot(dir: &std::path::Path, date: &str, parts: &[(&str, Vec<String>)]) {
    let mut files = vec![];
    for (n, (day, lines)) in parts.iter().enumerate() {
        let path = format!("data/jsonl/works/updated_date={day}/part_{n:04}.gz");
        let full = dir.join(&path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let body = gzip(lines.join("\n").as_bytes());
        files.push(serde_json::json!({"url": format!("s3://openalex/{path}"), "meta": {"content_length": body.len(), "record_count": lines.len()}}));
        std::fs::write(full, body).unwrap();
    }
    let manifest =
        serde_json::json!({"date": date, "entities": [{"entity": "works", "files": files}]});
    std::fs::create_dir_all(dir.join("data/jsonl")).unwrap();
    std::fs::write(dir.join("data/jsonl/manifest.json"), manifest.to_string()).unwrap();
}
fn works() -> Vec<String> {
    String::from_utf8(fixture("openalex", "works.jsonl"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}
fn with(line: &str, edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
    edit(&mut v);
    v.to_string()
}

/// OpenAlex: works cited more than the threshold become cards; a work in two dates' files
/// takes its newest copy; an update reads only the newer dates' files, rewrites what changed and
/// removes works that fell below the threshold; a changed threshold asks for a full import;
/// shared DOI, PubMed, arXiv and OpenAlex links join the card.
#[sqlx::test(migrations = "../../migrations")]
async fn openalex_imports_the_newest_copy_and_updates_by_date(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let w = works();
    let newer = with(&w[0], |v| {
        v["display_name"] = "A newer title".into();
        v["cited_by_count"] = 90.into();
    });
    // Many files of the older date, so the newer one is read beside them; the older copy comes
    // after 50,000 lines of works under the threshold, so the newer file is read long before it
    // and must wait for the older date to be written.
    let mut parts: Vec<(&str, Vec<String>)> = (0..9).map(|_| ("2026-05-21", vec![])).collect();
    let mut older: Vec<String> = (0..50_000)
        .map(|n| format!("{{\"id\":\"https://openalex.org/W9{n:07}\",\"cited_by_count\":1}}"))
        .collect();
    older.extend(w.clone());
    parts.push(("2026-05-21", older));
    parts.push(("2026-10-14", vec![newer]));
    snapshot(snap.path(), "2026-10-14", &parts);
    let importer = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "openalex", None).await, 5);
    let (uri, name, keys, labels, _, _) = row(&pool, "openalex", "W4225145323").await;
    assert_eq!(name, "A newer title", "the newest copy wins");
    assert!(uri.contains("10.1016/j.kint.2022.02.042"), "{uri}");
    assert!(
        keys.contains(&"pmid:35490842".to_owned())
            && keys.contains(&"openalex:W4225145323".to_owned()),
        "{keys:?}"
    );
    assert!(!labels.is_empty());
    let state = store::state(&pool, Catalogue::Openalex)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.position.as_ref().unwrap()["snapshot"], "2026-10-14");
    assert_eq!(
        state.current_as_of.unwrap().date_naive().to_string(),
        "2026-10-14"
    );

    // Shared links of the same paper join its card.
    for link in [
        "https://doi.org/10.1016/j.kint.2022.02.042",
        "https://pubmed.ncbi.nlm.nih.gov/35490842/",
        "https://openalex.org/W4225145323",
    ] {
        let found = join::find(&pool, &["openalex"], link).await.unwrap();
        assert_eq!(
            found.card.map(|c| c.source_id).as_deref(),
            Some("W4225145323"),
            "{link}"
        );
    }
    let found = join::find(&pool, &["openalex"], "https://arxiv.org/abs/2104.04167")
        .await
        .unwrap();
    assert_eq!(
        found.card.map(|c| c.source_id).as_deref(),
        Some("W3205276578")
    );

    // A new snapshot: one newer date's file, where a review fell to 10 citations and a work new
    // to the catalogue appears; the older files are not read again.
    let before = versions(&pool, "openalex").await;
    let fallen = with(&w[4], |v| v["cited_by_count"] = 10.into());
    let fresh = with(&w[3], |v| {
        v["id"] = "https://openalex.org/W1".into();
        v["doi"] = serde_json::Value::Null;
    });
    // The old files are gone from disk: reading one would fail the update.
    let mut parts2: Vec<(&str, Vec<String>)> = parts.clone();
    parts2.push(("2026-11-01", vec![fallen, fresh]));
    snapshot(snap.path(), "2026-11-14", &parts2);
    std::fs::remove_dir_all(snap.path().join("data/jsonl/works/updated_date=2026-05-21")).unwrap();
    let (_, written) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!((written.inserted, written.removed), (1, 1), "{written:?}");
    let after = versions(&pool, "openalex").await;
    assert!(!after.contains_key("W2131279082"));
    for (id, version) in &before {
        if id != "W2131279082" {
            assert_eq!(after.get(id), Some(version), "{id} rewritten");
        }
    }
    let state = store::state(&pool, Catalogue::Openalex)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.position.as_ref().unwrap()["through"], "2026-11-01");

    // A higher threshold: the update hands over to a full import, which keeps only the works
    // cited more than 100 times, by their newest copy (the review's 232 citations were in an
    // older file; its newest says 10).
    snapshot(snap.path(), "2026-11-14", &parts2);
    let ctx = context(&pool, dir.path(), |c| {
        c.catalogues.openalex.min_citations = 100
    });
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert!(state.import_requested.is_none());
    let mut left: Vec<String> = versions(&pool, "openalex").await.into_keys().collect();
    left.sort();
    assert_eq!(left, ["W1", "W2148189974"]);
    assert_eq!(state.position.as_ref().unwrap()["min_citations"], 100);
}

/// An OpenAlex update whose newer files would remove more than a tenth of the cards is refused:
/// nothing is removed and its failure counted once; the task's next update ends at once without
/// reading that snapshot, recording nothing; a full import asked for (`--request`) applies it.
#[sqlx::test(migrations = "../../migrations")]
async fn an_openalex_update_that_removes_too_much_is_refused_once(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let w = works();
    let many: Vec<String> = (1..=40)
        .map(|n| {
            with(&w[0], |v| {
                v["id"] = format!("https://openalex.org/W{n}").into();
                v["doi"] = serde_json::Value::Null;
            })
        })
        .collect();
    snapshot(snap.path(), "2026-10-14", &[("2026-10-14", many.clone())]);
    let importer = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "openalex", None).await, 40);
    age(&pool, Catalogue::Openalex).await;
    // Fifteen works fall to 3 citations in a newer file.
    let fallen: Vec<String> = many[..15]
        .iter()
        .map(|l| with(l, |v| v["cited_by_count"] = 3.into()))
        .collect();
    let parts = [("2026-10-14", many.clone()), ("2026-11-01", fallen)];
    snapshot(snap.path(), "2026-11-14", &parts);
    let failed = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(
        failed
            .to_string()
            .contains("refused (snapshot 2026-11-14: 15 cards gone"),
        "{failed}"
    );
    assert_eq!(count(&pool, "openalex", None).await, 40);
    let state = state_of(&pool, Catalogue::Openalex).await;
    assert_eq!((state.run.as_deref(), state.error_count), (None, 1));
    let position = state.position.unwrap();
    assert_eq!(position["refused"], "2026-11-14");
    assert_eq!(position["snapshot"], "2026-10-14");
    // The task's next update reads nothing (the newer file is gone from disk: reading it would
    // fail) and fails nothing.
    let newer = snap.path().join("data/jsonl/works/updated_date=2026-11-01");
    std::fs::rename(&newer, snap.path().join("aside")).unwrap();
    let again = run::step(&*importer, &ctx).await.unwrap_err();
    assert!(again.is::<run::StillRefused>(), "{again}");
    let state = state_of(&pool, Catalogue::Openalex).await;
    assert_eq!((state.error_count, state.run), (1, None));
    std::fs::rename(snap.path().join("aside"), &newer).unwrap();
    // A full import asked for: the task runs it and takes the snapshot as it is.
    run::request_import(
        &pool,
        Catalogue::Openalex,
        pinakes_core::catalogue::run::REQUESTED,
    )
    .await
    .unwrap();
    run::step(&*importer, &ctx).await.unwrap();
    assert_eq!(count(&pool, "openalex", None).await, 25);
    let state = state_of(&pool, Catalogue::Openalex).await;
    assert!(state.import_requested.is_none());
    assert!(state.position.unwrap().get("refused").is_none());
}

/// A new snapshot 180 days after the last full import is read by a full import, which removes
/// the works the snapshot no longer lists (merged or deleted upstream); before that, an update
/// keeps them. A full import whose manifest lists no works file is refused and removes nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn openalex_imports_in_full_again_after_180_days(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let w = works();
    snapshot(snap.path(), "2026-05-21", &[("2026-05-21", w.clone())]);
    let importer = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "openalex", None).await, 5);
    // A newer snapshot where the last work is gone (merged into another): an update keeps it.
    let kept: Vec<String> = w[..4].to_vec();
    let fresh = with(&w[3], |v| {
        v["id"] = "https://openalex.org/W1".into();
        v["doi"] = serde_json::Value::Null;
    });
    snapshot(
        snap.path(),
        "2026-06-14",
        &[
            ("2026-05-21", kept.clone()),
            ("2026-06-01", vec![fresh.clone()]),
        ],
    );
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "openalex", None).await, 6);
    // 200 days after the full import, the next snapshot's update is a full import.
    sqlx::query("UPDATE catalogue_state SET last_import_at=now()-interval '200 days' WHERE catalogue='openalex'")
        .execute(&pool)
        .await
        .unwrap();
    snapshot(
        snap.path(),
        "2026-12-14",
        &[("2026-05-21", kept), ("2026-06-01", vec![fresh])],
    );
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "openalex", None).await, 5);
    assert!(versions(&pool, "openalex").await.contains_key("W1"));
    assert!(state.last_import_at.unwrap() > chrono::Utc::now() - chrono::Duration::minutes(5));
    // An empty manifest: the import is refused (`empty`), every card stays.
    snapshot(snap.path(), "2026-12-15", &[]);
    let failed = run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap_err();
    assert!(
        failed
            .to_string()
            .contains("empty (no record read in 2026-12-15)"),
        "{failed}"
    );
    assert_eq!(count(&pool, "openalex", None).await, 5);
}

/// A full import cut off after its older date's file is taken up from its cursor: the file
/// written is not read again, the newer date's file is, and its newer copy of a work wins.
#[sqlx::test(migrations = "../../migrations")]
async fn a_cut_off_openalex_import_resumes_across_dates(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let w = works();
    let newer = with(&w[0], |v| v["display_name"] = "A newer title".into());
    // The import as it stood when it was cut off: the older date's file written.
    snapshot(snap.path(), "2026-10-14", &[("2026-05-21", w.clone())]);
    let importer = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    let generation = state_of(&pool, Catalogue::Openalex).await.generation;
    sqlx::query("UPDATE catalogue_state SET run='import',phase='import',cursor='{\"snapshot\":\"2026-10-14\",\"done\":[0]}' WHERE catalogue='openalex'")
        .execute(&pool)
        .await
        .unwrap();
    // The whole snapshot: the older file (now gone from disk: reading it would fail) and a newer
    // date's.
    snapshot(
        snap.path(),
        "2026-10-14",
        &[("2026-05-21", w.clone()), ("2026-10-01", vec![newer])],
    );
    std::fs::remove_dir_all(snap.path().join("data/jsonl/works/updated_date=2026-05-21")).unwrap();
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(state.generation, generation, "taken up, not begun anew");
    assert_eq!(state.run, None);
    assert_eq!(count(&pool, "openalex", None).await, 5);
    let (_, name, _, _, _, _) = row(&pool, "openalex", "W4225145323").await;
    assert_eq!(name, "A newer title");
}

/// Two works that share a DOI: the more cited one holds the DOI link and keys, so a shared DOI
/// link joins it (not neither); the other one's link is its openalex.org page. An update that
/// reads neither work leaves them so. When the holder goes, the other one takes the DOI back, and
/// that move reaches the joins (a stored DOI link is checked again) and the text index.
#[sqlx::test(migrations = "../../migrations")]
async fn one_openalex_card_holds_a_shared_doi(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let w = works();
    let original = with(&w[0], |v| v["cited_by_count"] = 900.into());
    let twin = with(&w[0], |v| {
        v["id"] = "https://openalex.org/W2".into();
        v["cited_by_count"] = 60.into();
    });
    let first = ("2026-10-14", vec![original.clone(), twin.clone()]);
    snapshot(snap.path(), "2026-10-14", std::slice::from_ref(&first));
    let importer = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    let doi_link = "https://doi.org/10.1016/j.kint.2022.02.042";
    let doi_key = "doi:10.1016/j.kint.2022.02.042";
    let holds = |keys: &[String]| keys.contains(&doi_key.to_owned());
    let (held_uri, _, keys, _, _, _) = row(&pool, "openalex", "W4225145323").await;
    assert!(holds(&keys), "{keys:?}");
    let (uri, _, keys, _, _, _) = row(&pool, "openalex", "W2").await;
    assert_eq!(uri, "https://openalex.org/W2");
    assert!(!holds(&keys), "{keys:?}");
    let joined = || async {
        join::find(&pool, &["openalex"], doi_link)
            .await
            .unwrap()
            .card
            .map(|c| c.source_id)
    };
    assert_eq!(joined().await.as_deref(), Some("W4225145323"));
    // A resource stored on the DOI link (as the holder's link is stored).
    sqlx::query("INSERT INTO resource(id,uri) VALUES (md5($1),$1)")
        .bind(&held_uri)
        .execute(&pool)
        .await
        .unwrap();
    // An update that reads neither work (a newer date's file of another work, with a DOI of its
    // own, so the pass runs over the DOIs it wrote): nothing moves.
    let other = with(&w[3], |v| {
        v["id"] = "https://openalex.org/W1".into();
        v["doi"] = "https://doi.org/10.5555/other.1".into();
    });
    let second = ("2026-11-01", vec![other]);
    snapshot(snap.path(), "2026-11-14", &[first.clone(), second.clone()]);
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    let (uri, _, keys, _, _, _) = row(&pool, "openalex", "W2").await;
    assert_eq!(uri, "https://openalex.org/W2");
    assert!(!holds(&keys), "{keys:?}");
    assert_eq!(joined().await.as_deref(), Some("W4225145323"));
    // The holder falls to 10 citations in a newer file: its card goes, the twin takes the DOI.
    sqlx::query("DELETE FROM catalogue_join_due")
        .execute(&pool)
        .await
        .unwrap();
    let queued: i64 =
        sqlx::query_scalar("SELECT coalesce(max(seq),0) FROM search_index_catalogue_queue")
            .fetch_one(&pool)
            .await
            .unwrap();
    let fallen = with(&w[0], |v| v["cited_by_count"] = 10.into());
    snapshot(
        snap.path(),
        "2026-12-14",
        &[first, second, ("2026-12-01", vec![fallen])],
    );
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert!(!versions(&pool, "openalex")
        .await
        .contains_key("W4225145323"));
    let (uri, _, keys, _, _, _) = row(&pool, "openalex", "W2").await;
    assert_eq!(uri, held_uri);
    assert!(holds(&keys), "{keys:?}");
    assert_eq!(joined().await.as_deref(), Some("W2"));
    // The move is a change: the stored DOI link is checked again, the text index reads the card.
    let due: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogue_join_due j JOIN resource r ON r.id=j.resource_id WHERE r.uri=$1")
        .bind(&held_uri)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(due, 1);
    let reindexed: i64 = sqlx::query_scalar("SELECT count(*) FROM search_index_catalogue_queue q JOIN catalogue_card c ON c.id=q.card_id WHERE c.source_id='W2' AND q.seq>$1")
        .bind(queued)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(reindexed >= 1);
}

/// An update cut off between its removals and the DOI pass (the holder of a shared DOI removed,
/// the twin not yet given the DOI back) ends, when it is taken up, with a pass over every card's
/// DOIs: the twin takes the DOI back though the files read again name neither work.
#[sqlx::test(migrations = "../../migrations")]
async fn an_openalex_update_cut_off_after_its_removals_shares_dois_again(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let w = works();
    let original = with(&w[0], |v| v["cited_by_count"] = 900.into());
    let twin = with(&w[0], |v| {
        v["id"] = "https://openalex.org/W2".into();
        v["cited_by_count"] = 60.into();
    });
    let first = ("2026-10-14", vec![original, twin]);
    snapshot(snap.path(), "2026-10-14", std::slice::from_ref(&first));
    let importer = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    let doi_key = "doi:10.1016/j.kint.2022.02.042";
    let (held_uri, _, _, _, _, _) = row(&pool, "openalex", "W4225145323").await;
    let (_, _, keys, _, _, _) = row(&pool, "openalex", "W2").await;
    assert!(!keys.contains(&doi_key.to_owned()), "{keys:?}");
    // The update as it stood when it was cut off: the holder, fallen below the threshold in a
    // newer file, removed with the update's cursor; the DOI pass not run.
    let fallen = with(&w[0], |v| v["cited_by_count"] = 10.into());
    snapshot(
        snap.path(),
        "2026-11-14",
        &[first, ("2026-11-01", vec![fallen])],
    );
    sqlx::query(
        "UPDATE catalogue_state SET run='update',phase='update' WHERE catalogue='openalex'",
    )
    .execute(&pool)
    .await
    .unwrap();
    store::remove(
        &pool,
        Catalogue::Openalex,
        &["W4225145323".to_owned()],
        Some(&serde_json::json!({"snapshot": "2026-11-14", "phase": "removing"})),
    )
    .await
    .unwrap();
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(state.run, None);
    let (uri, _, keys, _, _, _) = row(&pool, "openalex", "W2").await;
    assert_eq!(uri, held_uri);
    assert!(keys.contains(&doi_key.to_owned()), "{keys:?}");
}

/// An update taken up after its removals and cut off again while it reads (before its DOI pass)
/// still ends, when it is taken up once more, with the pass over every DOI: each cursor it saves
/// keeps saying the removals began.
#[sqlx::test(migrations = "../../migrations")]
async fn an_openalex_update_cut_off_twice_after_its_removals_shares_dois_again(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let w = works();
    let original = with(&w[0], |v| v["cited_by_count"] = 900.into());
    let twin = with(&w[0], |v| {
        v["id"] = "https://openalex.org/W2".into();
        v["cited_by_count"] = 60.into();
    });
    let first = ("2026-10-14", vec![original, twin]);
    snapshot(snap.path(), "2026-10-14", std::slice::from_ref(&first));
    let importer = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    let doi_key = "doi:10.1016/j.kint.2022.02.042";
    let (held_uri, _, _, _, _, _) = row(&pool, "openalex", "W4225145323").await;
    // The update's files: the holder fallen below the threshold and a new work in the first, a
    // long file of works under the threshold after it (so the run is still reading when it is
    // cut off again).
    let fallen = with(&w[0], |v| v["cited_by_count"] = 10.into());
    let new_work = with(&w[3], |v| {
        v["id"] = "https://openalex.org/W5".into();
        v["doi"] = "https://doi.org/10.5555/new.5".into();
    });
    let long: Vec<String> = (0..300_000)
        .map(|n| format!("{{\"id\":\"https://openalex.org/W8{n:07}\",\"cited_by_count\":1}}"))
        .collect();
    snapshot(
        snap.path(),
        "2026-11-14",
        &[
            first,
            ("2026-11-01", vec![fallen, new_work]),
            ("2026-11-02", long),
        ],
    );
    // Cut off the first time after its removals: the holder removed, no DOI pass.
    sqlx::query(
        "UPDATE catalogue_state SET run='update',phase='update' WHERE catalogue='openalex'",
    )
    .execute(&pool)
    .await
    .unwrap();
    store::remove(
        &pool,
        Catalogue::Openalex,
        &["W4225145323".to_owned()],
        Some(&serde_json::json!({"snapshot": "2026-11-14", "phase": "removing"})),
    )
    .await
    .unwrap();
    // Taken up, and cut off again (a restart) once it saved a cursor of its own.
    let running = tokio::spawn({
        let (importer, ctx) = (importer.clone(), ctx.clone());
        async move { run::run_once(importer, &ctx, Run::Update, |_| {}).await }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let cursor: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT cursor FROM catalogue_state WHERE catalogue='openalex'")
                .fetch_one(&pool)
                .await
                .unwrap();
        if cursor.as_ref().is_some_and(|c| c.get("done").is_some()) {
            break;
        }
        assert!(
            !running.is_finished(),
            "the update ended before it was cut off"
        );
        assert!(std::time::Instant::now() < deadline, "no cursor saved");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    running.abort();
    let _ = running.await;
    let state = state_of(&pool, Catalogue::Openalex).await;
    assert_eq!(
        state.run.as_deref(),
        Some("update"),
        "cut off while reading"
    );
    let (_, _, keys, _, _, _) = row(&pool, "openalex", "W2").await;
    assert!(!keys.contains(&doi_key.to_owned()), "{keys:?}");
    // Taken up once more (its catalogue lock goes with the cut-off run's connection).
    let ended = loop {
        match run::run_once(importer.clone(), &ctx, Run::Update, |_| {}).await {
            Ok(ended) => break ended,
            Err(error) if error.to_string().contains("lock is held") => {
                assert!(std::time::Instant::now() < deadline, "{error}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("{error}"),
        }
    };
    assert_eq!(ended.0.run, None);
    let (uri, _, keys, _, _, _) = row(&pool, "openalex", "W2").await;
    assert_eq!(uri, held_uri);
    assert!(keys.contains(&doi_key.to_owned()), "{keys:?}");
}

// --- Tranco --------------------------------------------------------------------------------------

/// What one URL answers.
#[derive(Clone)]
struct Answer {
    status: u16,
    kind: &'static str,
    body: Vec<u8>,
    location: Option<String>,
    retry_after: u64,
}
/// Homepages and robots.txt files as the egress' transport answers them (changed between runs);
/// a URL not named answers 404, any URL of `gone.example` nothing at all. It counts the requests.
#[derive(Clone, Default)]
struct Sites {
    answers: Arc<Mutex<HashMap<String, Answer>>>,
    asked: Arc<Mutex<Vec<String>>>,
}
impl Sites {
    fn set(&self, url: &str, status: u16, kind: &'static str, body: Vec<u8>) {
        self.answers.lock().unwrap().insert(
            url.to_owned(),
            Answer {
                status,
                kind,
                body,
                location: None,
                retry_after: 0,
            },
        );
    }
    fn page(&self, url: &str, title: &str) {
        self.set(url, 200, "text/html; charset=utf-8", page(title));
    }
    fn redirect(&self, url: &str, to: &str) {
        self.answers.lock().unwrap().insert(
            url.to_owned(),
            Answer {
                status: 301,
                kind: "text/html",
                body: vec![],
                location: Some(to.to_owned()),
                retry_after: 0,
            },
        );
    }
    /// A busy answer (a 429, a 5xx) with its `Retry-After`.
    fn busy(&self, url: &str, status: u16, retry_after: u64) {
        self.answers.lock().unwrap().insert(
            url.to_owned(),
            Answer {
                status,
                kind: "text/plain",
                body: b"slow down".to_vec(),
                location: None,
                retry_after,
            },
        );
    }
    fn asked(&self, url: &str) -> usize {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .filter(|u| *u == url)
            .count()
    }
}
#[async_trait::async_trait]
impl pinakes_core::resolver::egress::Transport for Sites {
    async fn get(
        &self,
        u: &url::Url,
        addrs: &[std::net::SocketAddr],
    ) -> anyhow::Result<pinakes_core::resolver::egress::Fetched> {
        self.get_with(u, addrs, &Default::default()).await
    }
    async fn get_with(
        &self,
        u: &url::Url,
        _: &[std::net::SocketAddr],
        _: &pinakes_core::resolver::egress::FetchOptions,
    ) -> anyhow::Result<pinakes_core::resolver::egress::Fetched> {
        self.asked.lock().unwrap().push(u.to_string());
        let answer = self.answers.lock().unwrap().get(u.as_str()).cloned();
        match answer {
            Some(a) => {
                let mut f = common::fetched(u.clone(), a.status, a.kind.to_owned(), a.body);
                f.location = a.location;
                f.retry_after_secs = a.retry_after;
                Ok(f)
            }
            None if u.host_str() == Some("gone.example") => {
                anyhow::bail!(pinakes_core::resolver::Unreached)
            }
            None => Ok(common::fetched(u.clone(), 404, "text/plain".into(), vec![])),
        }
    }
}
fn page(title: &str) -> Vec<u8> {
    format!("<!doctype html><html><head><title>{title}</title><meta name=\"description\" content=\"About {title}\"></head><body><p>{title}</p></body></html>").into_bytes()
}
/// A Tranco importer over `list` whose homepage reads go to `sites`; `slow` hosts get their pace
/// at the egress (requests a second).
fn tranco_over(list: &std::path::Path, sites: &Sites, slow: &[(&str, f64)]) -> tranco::Tranco {
    let mut cfg = common::unpaced_egress_config();
    for (host, rate) in slow {
        cfg.host_rates.insert((*host).to_owned(), *rate);
    }
    let egress = pinakes_core::resolver::egress::Egress::with_backends(
        cfg,
        Arc::new(common::PublicDns),
        Arc::new(sites.clone()),
    );
    tranco::Tranco::new(
        tranco::ListSource::Given(Input::File(list.to_owned())),
        egress,
        "test@example.org",
    )
    .unwrap()
}
/// The test configuration with the large catalogues (MusicBrainz, OpenStreetMap, Wikidata,
/// OpenAlex: all have an importer) off, so homepage reads do not wait for their first imports.
fn without_large(config: &mut Config) {
    config.catalogues.musicbrainz.enabled = false;
    config.catalogues.osm.enabled = false;
    config.catalogues.wikidata.enabled = false;
    config.catalogues.openalex.enabled = false;
}
async fn outcomes(pool: &PgPool) -> HashMap<String, Option<String>> {
    sqlx::query_as::<_, (String, Option<String>)>("SELECT host,outcome FROM catalogue_site")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect()
}
/// Each site's wait until it is due (seconds) and its failures in a row.
async fn due(pool: &PgPool) -> HashMap<String, (f64, i32)> {
    sqlx::query_as::<_, (String, f64, i32)>(
        "SELECT host,extract(epoch FROM due_at-now())::float8,failures FROM catalogue_site",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(h, d, f)| (h, (d, f)))
    .collect()
}
/// A day passed since Tranco last looked its list up: the next update looks again.
async fn a_day_passed(pool: &PgPool) {
    sqlx::query("UPDATE catalogue_state SET position=position-'checked' WHERE catalogue='tranco'")
        .execute(pool)
        .await
        .unwrap();
}
async fn make_due(pool: &PgPool, host: &str) {
    sqlx::query("UPDATE catalogue_site SET due_at=now() WHERE host=$1")
        .bind(host)
        .execute(pool)
        .await
        .unwrap();
}
/// An Anubis challenge page, answered with 200 (the egress's wall check knows it).
const WALL: &str = "<html><head><title>Making sure you&#39;re not a bot!</title><script id=\"anubis_challenge\" type=\"application/json\">{}</script></head><body><h1>Making sure you&#39;re not a bot!</h1><p>Loading...</p><p>Protected by <a href=\"https://github.com/TecharoHQ/anubis\">Anubis</a></p><script src=\"/.within.website/x/cmd/anubis/static/js/main.mjs\"></script></body></html>";

/// Tranco: the list fills `catalogue_site`; a homepage that answers with a page becomes a card
/// (its title, the homepage link, the homepage key, fame from the rank), also through a redirect
/// on the site; a robots.txt that says no, a wall, another type, a 404, no answer, a redirect off
/// the site and a page without a name make none; a 429 waits for its `Retry-After`; a read the
/// egress gives no turn is deferred, not counted; reads are counted by result.
#[sqlx::test(migrations = "../../migrations")]
async fn tranco_reads_homepages_by_the_rules(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    let list = dir.path().join("list.csv");
    std::fs::write(&list, "1,example.org\n2,closed.example\n3,walled.example\n4,api.example\n5,missing.example\n6,gone.example\n7,moved.example\n8,away.example\n9,nameless.example\n10,busy.example\n11,slow.example\n12,down.example\n13,self.example\n14,lazy.example\n").unwrap();
    let sites = Sites::default();
    sites.set(
        "https://example.org/robots.txt",
        200,
        "text/plain",
        b"User-agent: *\nDisallow: /private\n".to_vec(),
    );
    sites.page("https://example.org/", "Example");
    sites.set(
        "https://closed.example/robots.txt",
        200,
        "text/plain",
        b"User-agent: pinakes\nDisallow: /\n".to_vec(),
    );
    sites.page("https://closed.example/", "Closed");
    sites.set(
        "https://walled.example/",
        200,
        "text/html; charset=utf-8",
        WALL.as_bytes().to_vec(),
    );
    sites.set(
        "https://api.example/",
        200,
        "application/json",
        b"{}".to_vec(),
    );
    // A redirect on the site: followed, the new host's robots.txt asked first.
    sites.redirect("https://moved.example/", "https://www.moved.example/home");
    sites.page("https://www.moved.example/home", "Moved");
    // A redirect off the site: no card.
    sites.redirect("https://away.example/", "https://elsewhere.example/");
    sites.page("https://elsewhere.example/", "Elsewhere");
    sites.set(
        "https://nameless.example/",
        200,
        "text/html",
        b"<!doctype html><html><head></head><body></body></html>".to_vec(),
    );
    // A page whose answer names its own address in a `Location` (no redirect).
    sites.page("https://self.example/", "Self");
    sites
        .answers
        .lock()
        .unwrap()
        .get_mut("https://self.example/")
        .unwrap()
        .location = Some("https://self.example/".into());
    // A 429 that asks for two hours (the egress holds a wait to an hour).
    sites.busy("https://busy.example/", 429, 7200);
    // A 503 that asks for three days: not read before then.
    sites.busy("https://down.example/", 503, 3 * 86400);
    sites.page("https://slow.example/", "Slow");
    sites.page("https://lazy.example/", "Lazy");
    // slow.example: one request in 10,000 s; lazy.example: one in 4 s (its page waits a turn).
    let importer = Arc::new(tranco_over(
        &list,
        &sites,
        &[("slow.example", 0.0001), ("lazy.example", 0.25)],
    ));
    let reads = |r| pinakes_core::telemetry::homepage_reads(r);
    let before: Vec<u64> = ["ok", "robots", "wall", "busy", "deferred", "other"]
        .into_iter()
        .map(reads)
        .collect();
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    let listed: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogue_site")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(listed, 14);
    let mut cards: Vec<String> = versions(&pool, "tranco").await.into_keys().collect();
    cards.sort();
    assert_eq!(cards, ["example.org", "moved.example", "self.example"]);
    let (uri, name, keys, _, _, fame) = row(&pool, "tranco", "example.org").await;
    assert_eq!(
        (uri.as_str(), name.as_str()),
        ("https://example.org/", "Example")
    );
    assert!(keys.contains(&"homepage:example.org".to_owned()));
    assert_eq!(fame, 1.0);
    let (uri, name, _, _, _, _) = row(&pool, "tranco", "moved.example").await;
    assert_eq!(
        (uri.as_str(), name.as_str()),
        ("https://moved.example/", "Moved")
    );
    assert_eq!(sites.asked("https://www.moved.example/robots.txt"), 1);
    assert_eq!(sites.asked("https://elsewhere.example/"), 0);
    let outcomes = outcomes(&pool).await;
    let outcome = |h: &str| outcomes[h].clone();
    assert_eq!(outcome("example.org").as_deref(), Some("ok"));
    assert_eq!(outcome("moved.example").as_deref(), Some("ok"));
    assert_eq!(outcome("closed.example").as_deref(), Some("robots"));
    assert_eq!(outcome("walled.example").as_deref(), Some("wall"));
    assert_eq!(outcome("api.example").as_deref(), Some("not_html"));
    assert_eq!(outcome("missing.example").as_deref(), Some("status"));
    assert_eq!(outcome("gone.example").as_deref(), Some("unreached"));
    assert_eq!(outcome("away.example").as_deref(), Some("other"));
    assert_eq!(outcome("nameless.example").as_deref(), Some("other"));
    assert_eq!(outcome("busy.example").as_deref(), Some("busy"));
    // No turn at the egress (its robots.txt took the host's one request): no read at all.
    assert_eq!(outcome("slow.example"), None);
    // Counted by result: exactly this test's reads for the results only it makes (the counters
    // are the process's, and other tests read homepages at the same time), at least for the rest.
    let delta = |n: usize, r| reads(r) - before[n];
    assert_eq!(
        (delta(1, "robots"), delta(4, "deferred"), delta(5, "other")),
        (1, 2, 2)
    );
    assert!(delta(0, "ok") >= 3 && delta(2, "wall") >= 1 && delta(3, "busy") >= 2);
    let due = due(&pool).await;
    // Read again in 90 days; no answer: tomorrow, counted.
    assert!(due["example.org"].0 > 89.0 * 86400.0);
    assert!(due["walled.example"].0 > 89.0 * 86400.0);
    let (wait, failures) = due["gone.example"];
    assert!(wait < 86400.0 + 60.0 && wait > 86400.0 - 60.0, "{wait}");
    assert_eq!(failures, 1);
    // The 429: the next day (later than its Retry-After), counted as a failure.
    let (wait, failures) = due["busy.example"];
    assert!(wait > 86400.0 - 60.0 && wait < 86400.0 + 60.0, "{wait}");
    assert_eq!(failures, 1);
    // The 503: after its three days, counted as a failure.
    assert_eq!(outcome("down.example").as_deref(), Some("busy"));
    let (wait, failures) = due["down.example"];
    assert!(
        wait > 3.0 * 86400.0 - 60.0 && wait < 3.0 * 86400.0 + 60.0,
        "{wait}"
    );
    assert_eq!(failures, 1);
    // Deferred: when the egress's wait for the host ends (about 10,000 s: later than the hour),
    // nothing counted.
    let (wait, failures) = due["slow.example"];
    assert!(wait > 9000.0 && wait < 10_100.0, "{wait}");
    assert_eq!(failures, 0);
    // lazy.example's page waited for its turn after its robots.txt: tried again once its host's
    // pace allows one request, it reads the page with the rules kept (asking robots.txt again
    // would take that one request).
    assert_eq!(outcome("lazy.example"), None);
    tokio::time::sleep(Duration::from_millis(4500)).await;
    make_due(&pool, "lazy.example").await;
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(sites.asked("https://lazy.example/robots.txt"), 1);
    let lazy: Option<String> =
        sqlx::query_scalar("SELECT outcome FROM catalogue_site WHERE host='lazy.example'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(lazy.as_deref(), Some("ok"));
    // A shared homepage joins the card, under any spelling.
    for link in ["https://example.org/", "http://www.example.org/"] {
        let found = join::find(&pool, &["tranco"], link).await.unwrap();
        assert_eq!(
            found.card.map(|c| c.source_id).as_deref(),
            Some("example.org"),
            "{link}"
        );
    }
}

/// A listed site that is itself a public suffix (`github.io`, `blogspot.com`) is read on its own
/// host: a redirect on that host is followed and its page becomes the site's card; a redirect to
/// one of its subdomains (another person's site) makes none.
#[sqlx::test(migrations = "../../migrations")]
async fn a_tranco_site_that_is_a_public_suffix_reads_on_its_own_host(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    let list = dir.path().join("list.csv");
    std::fs::write(&list, "1,github.io\n2,blogspot.com\n").unwrap();
    let sites = Sites::default();
    sites.redirect("https://github.io/", "https://github.io/home");
    sites.page("https://github.io/home", "GitHub Pages");
    sites.redirect("https://blogspot.com/", "https://someone.blogspot.com/");
    sites.page("https://someone.blogspot.com/", "Someone's blog");
    let importer = Arc::new(tranco_over(&list, &sites, &[]));
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    let cards: Vec<String> = versions(&pool, "tranco").await.into_keys().collect();
    assert_eq!(cards, ["github.io"]);
    let (uri, name, keys, _, _, _) = row(&pool, "tranco", "github.io").await;
    assert_eq!(
        (uri.as_str(), name.as_str()),
        ("https://github.io/", "GitHub Pages")
    );
    assert!(keys.contains(&"homepage:github.io".to_owned()), "{keys:?}");
    assert_eq!(sites.asked("https://someone.blogspot.com/"), 0);
    let outcomes = outcomes(&pool).await;
    assert_eq!(outcomes["blogspot.com"].as_deref(), Some("other"));
}

/// `catalogue status` shows the list's sites and how many have been read: counted when a list
/// is loaded, and between loads carried from the last run with each run's first reads (a run
/// does not count the table).
#[sqlx::test(migrations = "../../migrations")]
async fn tranco_counts_its_sites_when_a_list_loads(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    let list = dir.path().join("list.csv");
    std::fs::write(&list, "1,a.example\n2,b.example\n3,c.example\n").unwrap();
    let sites = Sites::default();
    for host in ["a", "b", "c", "d"] {
        sites.page(&format!("https://{host}.example/"), host);
    }
    let importer = Arc::new(tranco_over(&list, &sites, &[]));
    let shown = || async {
        let version: String = sqlx::query_scalar(
            "SELECT upstream_version FROM catalogue_state WHERE catalogue='tranco'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        version
            .rsplit_once(", ")
            .map_or(String::new(), |(head, read)| {
                format!("{}, {read}", head.rsplit_once(", ").unwrap().1)
            })
    };
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(shown().await, "3 sites, 3 read)");
    // Between loads the stored count is carried, not the table counted.
    sqlx::query("UPDATE catalogue_state SET position=position||'{\"sites\":100,\"read\":40}' WHERE catalogue='tranco'")
        .execute(&pool)
        .await
        .unwrap();
    make_due(&pool, "a.example").await;
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(sites.asked("https://a.example/"), 2);
    assert_eq!(shown().await, "100 sites, 40 read)");
    // A site read for the first time counts.
    sqlx::query("UPDATE catalogue_site SET read_at=NULL,due_at=now() WHERE host='b.example'")
        .execute(&pool)
        .await
        .unwrap();
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(shown().await, "100 sites, 41 read)");
    // A new list is counted again.
    std::fs::write(
        &list,
        "1,a.example\n2,b.example\n3,c.example\n4,d.example\n",
    )
    .unwrap();
    a_day_passed(&pool).await;
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(shown().await, "4 sites, 4 read)");
}

/// A card stays through two transient failures in a row (no answer, a server error) and goes
/// with the third; a page read between them starts the count again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_tranco_card_goes_after_three_transient_failures(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    let list = dir.path().join("list.csv");
    std::fs::write(&list, "1,flaky.example\n").unwrap();
    let sites = Sites::default();
    sites.page("https://flaky.example/", "Flaky");
    let importer = Arc::new(tranco_over(&list, &sites, &[]));
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 1);
    let round = |status: u16| {
        let sites = sites.clone();
        let pool = pool.clone();
        let ctx = &ctx;
        let importer = importer.clone();
        async move {
            sites.set(
                "https://flaky.example/",
                status,
                "text/plain",
                b"down".to_vec(),
            );
            make_due(&pool, "flaky.example").await;
            run::run_once(importer, ctx, Run::Update, |_| {})
                .await
                .unwrap();
            (
                count(&pool, "tranco", None).await,
                due(&pool).await["flaky.example"].1,
            )
        }
    };
    assert_eq!(round(503).await, (1, 1));
    // The run leaves when the next site falls due (the plan wakes then): this one's retry.
    let position = state_of(&pool, Catalogue::Tranco).await.position.unwrap();
    let next_due = chrono::DateTime::parse_from_rfc3339(position["next_due"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    let wait = (next_due - chrono::Utc::now()).num_seconds();
    assert!((86_000..86_500).contains(&wait), "{wait}");
    assert_eq!(round(503).await, (1, 2));
    // A page again: the count starts over.
    sites.page("https://flaky.example/", "Flaky");
    make_due(&pool, "flaky.example").await;
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(due(&pool).await["flaky.example"].1, 0);
    assert_eq!(round(503).await, (1, 1));
    assert_eq!(round(503).await, (1, 2));
    assert_eq!(round(503).await, (0, 3));
    assert!(due(&pool).await["flaky.example"].0 > 89.0 * 86400.0);
    // A page again, then a wall in front of it three times: a site with a card keeps it through
    // two walls (a challenge may pass), and loses it with the third.
    sites.page("https://flaky.example/", "Flaky");
    make_due(&pool, "flaky.example").await;
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 1);
    for (n, cards) in [(1, 1), (2, 1), (3, 0)] {
        sites.set(
            "https://flaky.example/",
            200,
            "text/html; charset=utf-8",
            WALL.as_bytes().to_vec(),
        );
        make_due(&pool, "flaky.example").await;
        run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
            .await
            .unwrap();
        assert_eq!(
            (
                count(&pool, "tranco", None).await,
                due(&pool).await["flaky.example"].1
            ),
            (cards, n),
            "wall {n}"
        );
    }
    assert_eq!(
        outcomes(&pool).await["flaky.example"].as_deref(),
        Some("wall")
    );
}

/// Homepage reads wait for the bulk imports: while a large catalogue that is on and has an
/// importer has not finished its first full import, the list loads but nothing is read, and the
/// list is not looked for again within the day; once that import has finished, they run.
#[sqlx::test(migrations = "../../migrations")]
async fn tranco_reads_wait_for_the_large_imports(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    // The other large catalogues off; OpenAlex on (its first import not finished).
    let ctx = context(&pool, dir.path(), |c| {
        c.catalogues.musicbrainz.enabled = false;
        c.catalogues.osm.enabled = false;
        c.catalogues.wikidata.enabled = false;
    });
    let files = Files::default();
    files.put("/list.csv", b"1,example.org\n".to_vec(), "l1");
    let base = files.serve().await;
    let sites = Sites::default();
    sites.page("https://example.org/", "Example");
    let cfg = common::unpaced_egress_config();
    let egress = pinakes_core::resolver::egress::Egress::with_backends(
        cfg,
        Arc::new(common::PublicDns),
        Arc::new(sites.clone()),
    );
    let input = Input::Url(url::Url::parse(&format!("{base}/list.csv")).unwrap());
    let importer = Arc::new(
        tranco::Tranco::new(tranco::ListSource::Given(input), egress, "test@example.org").unwrap(),
    );
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(state.position.as_ref().unwrap()["next"], "large_import");
    assert_eq!(state.position.as_ref().unwrap()["waits_for"], "openalex");
    assert_eq!(sites.asked("https://example.org/"), 0);
    // `catalogue status` says why.
    let table = store::status_table(&ctx.config.catalogues, std::slice::from_ref(&state));
    assert!(
        table.contains("homepage reads wait for openalex's full import"),
        "{table}"
    );
    let gets = files.gets("/list.csv");
    let heads = files.heads("/list.csv");
    // The next run within the day: the list is not asked for, nothing is read.
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(state.position.as_ref().unwrap()["next"], "large_import");
    assert_eq!(
        (files.gets("/list.csv"), files.heads("/list.csv")),
        (gets, heads)
    );
    assert_eq!(sites.asked("https://example.org/"), 0);
    // A large full import under way (MusicBrainz's, though it is off here) holds them too.
    store::ensure_state(&pool, Catalogue::Openalex)
        .await
        .unwrap();
    sqlx::query("UPDATE catalogue_state SET last_import_at=now() WHERE catalogue='openalex'")
        .execute(&pool)
        .await
        .unwrap();
    store::ensure_state(&pool, Catalogue::Musicbrainz)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE catalogue_state SET run='import',phase='import' WHERE catalogue='musicbrainz'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        tranco::reads_wait(&ctx.config, &pool)
            .await
            .unwrap()
            .as_deref(),
        Some("musicbrainz")
    );
    sqlx::query("UPDATE catalogue_state SET run=NULL,phase='idle' WHERE catalogue='musicbrainz'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(tranco::reads_wait(&ctx.config, &pool).await.unwrap(), None);
    // OpenAlex's first import has finished: the reads run.
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(state.position.as_ref().unwrap()["next"], "list");
    assert_eq!(count(&pool, "tranco", None).await, 1);
    // The list can't be looked up any more (its server answers 404): the update still reads the
    // homepages due on the list loaded, and fails nothing.
    files.files.lock().unwrap().remove("/list.csv");
    a_day_passed(&pool).await;
    make_due(&pool, "example.org").await;
    let asked = sites.asked("https://example.org/");
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(sites.asked("https://example.org/"), asked + 1);
    assert_eq!(state.error_count, 0);
    // With no contact, a large catalogue's importer does not start: it holds no reads.
    let mut config = (*ctx.config).clone();
    config.egress.contact = String::new();
    sqlx::query("DELETE FROM catalogue_state WHERE catalogue='openalex'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(tranco::reads_wait(&config, &pool).await.unwrap(), None);
}

/// The list: a known site's row (its rank, its last listed day) is written at most weekly; a
/// list dated ahead is read as of now; a site off the list for a month loses its card and row;
/// a full import again keeps the cards of homepages read; `--limit` caps the reads of a run.
#[sqlx::test(migrations = "../../migrations")]
async fn tranco_lists_move_rows_weekly_and_drop_sites_off_the_list(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    let list = dir.path().join("list.csv");
    std::fs::write(
        &list,
        "1,example.org\n2,closed.example\n3,third.example\n4,fourth.example\n",
    )
    .unwrap();
    let sites = Sites::default();
    for host in [
        "example.org",
        "closed.example",
        "third.example",
        "fourth.example",
    ] {
        sites.page(&format!("https://{host}/"), host);
    }
    // At most two reads a run.
    let importer = Arc::new(tranco_over(&list, &sites, &[]).reads(2));
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 2);
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 4);
    assert_eq!(state.position.as_ref().unwrap()["next"], "reads");
    let rows = || async {
        sqlx::query_as::<_, (String, i32, String, String)>(
            "SELECT host,rank,listed_on::text,xmin::text FROM catalogue_site ORDER BY host",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    // A new list the next day with other ranks: no row is written.
    let before = rows().await;
    std::fs::write(
        &list,
        "1,closed.example\n2,example.org\n3,fourth.example\n4,third.example\n",
    )
    .unwrap();
    let later = std::time::SystemTime::now() + Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&list)
        .unwrap()
        .set_modified(later)
        .unwrap();
    // Within the day the list is not looked up again: a run that reads what fell due keeps the
    // list it has.
    let listed = |s: &store::State| s.position.as_ref().unwrap()["list"].clone();
    let had = listed(&state_of(&pool, Catalogue::Tranco).await);
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(listed(&state), had);
    // The next day it is: the new list loads, and no row is written.
    a_day_passed(&pool).await;
    let (state, _) = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_ne!(listed(&state), had);
    assert_eq!(rows().await, before);
    // A week on, the ranks and days move.
    sqlx::query("UPDATE catalogue_site SET listed_on=listed_on-8")
        .execute(&pool)
        .await
        .unwrap();
    std::fs::write(
        &list,
        "1,closed.example\n2,example.org\n3,fourth.example\n4,third.example\n\n",
    )
    .unwrap();
    a_day_passed(&pool).await;
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    let ranks: HashMap<String, i32> = rows()
        .await
        .into_iter()
        .map(|(h, r, _, _)| (h, r))
        .collect();
    assert_eq!((ranks["closed.example"], ranks["example.org"]), (1, 2));
    // A full import again keeps the cards of the homepages read.
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 4);
    // A list dated 40 days ahead, without example.org: read as of now, nothing drops off.
    std::fs::write(&list, "1,closed.example\n").unwrap();
    let ahead = std::time::SystemTime::now() + Duration::from_secs(40 * 86400);
    std::fs::File::options()
        .write(true)
        .open(&list)
        .unwrap()
        .set_modified(ahead)
        .unwrap();
    a_day_passed(&pool).await;
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 4);
    // Every site last listed 40 days ago, and a list without example.org: the sites off it go
    // with their cards; the one on it stays.
    sqlx::query("UPDATE catalogue_site SET listed_on=current_date-40")
        .execute(&pool)
        .await
        .unwrap();
    std::fs::write(&list, "1,closed.example\n\n").unwrap();
    a_day_passed(&pool).await;
    run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap();
    let left: Vec<String> = rows().await.into_iter().map(|(h, ..)| h).collect();
    assert_eq!(left, ["closed.example"]);
    assert_eq!(count(&pool, "tranco", None).await, 1);
}

/// A full import asked for (`--request`) while an update is under way: the update hands over at
/// once without reading anything, and the next step runs the full import (for each importer).
#[sqlx::test(migrations = "../../migrations")]
async fn a_requested_import_takes_over_an_update_under_way(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    // Software.
    let files = Files::default();
    let sources = software_server(&files, 0).await;
    let software = Arc::new(software::Software::new(sources, "test@example.org").unwrap());
    // Podcast Index.
    let podcasts = Files::default();
    podcasts.put(
        "/podcastindex_feeds.db.tgz",
        podcast_archive(&fixture("podcastindex", "podcastindex_feeds.db")),
        "p1",
    );
    let base = podcasts.serve().await;
    let input = Input::Url(url::Url::parse(&format!("{base}/podcastindex_feeds.db.tgz")).unwrap());
    let podcast = Arc::new(podcastindex::PodcastIndex::new(input, "test@example.org").unwrap());
    // OpenAlex.
    let snap = tempfile::tempdir().unwrap();
    snapshot(snap.path(), "2026-10-14", &[("2026-10-14", works())]);
    let alex = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    // Tranco.
    let list = dir.path().join("list.csv");
    std::fs::write(&list, "1,example.org\n").unwrap();
    let sites = Sites::default();
    sites.page("https://example.org/", "Example");
    let tranco = Arc::new(tranco_over(&list, &sites, &[]));
    let importers: [Arc<dyn run::Importer>; 4] = [software, podcast, alex, tranco];
    for importer in importers {
        let catalogue = importer.catalogue();
        run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
            .await
            .unwrap();
        let imported = state_of(&pool, catalogue).await.last_import_at.unwrap();
        // An update under way (cut off), and a request.
        sqlx::query("UPDATE catalogue_state SET run='update',phase='update',cursor='{}',last_update_at=NULL WHERE catalogue=$1")
            .bind(catalogue.name())
            .execute(&pool)
            .await
            .unwrap();
        run::request_import(&pool, catalogue, pinakes_core::catalogue::run::REQUESTED)
            .await
            .unwrap();
        let gets = (
            files.gets("/db-dump.tar.gz"),
            podcasts.gets("/podcastindex_feeds.db.tgz"),
        );
        run::step(&*importer, &ctx).await.unwrap();
        let state = state_of(&pool, catalogue).await;
        assert_eq!(
            state.last_update_at, None,
            "{catalogue}: the update did not run"
        );
        assert_eq!(state.run, None, "{catalogue}");
        assert!(state.import_requested.is_some(), "{catalogue}");
        assert_eq!(
            (
                files.gets("/db-dump.tar.gz"),
                podcasts.gets("/podcastindex_feeds.db.tgz")
            ),
            gets,
            "{catalogue}: nothing read"
        );
        run::step(&*importer, &ctx).await.unwrap();
        let state = state_of(&pool, catalogue).await;
        assert!(state.last_import_at.unwrap() > imported, "{catalogue}");
        assert!(state.import_requested.is_none(), "{catalogue}");
    }
}

/// The importers the daemon registers: one per catalogue with an importer, in their order, and
/// none without the operator's contact.
#[test]
fn the_daemon_registers_these_importers_only_with_a_contact() {
    let egress = common::offline_egress(Default::default());
    let mut config = Config::default();
    assert!(run::importers(&config, &egress).is_empty());
    config.egress.contact = "ops@example.org".into();
    let names: Vec<&str> = run::importers(&config, &egress)
        .iter()
        .map(|i| i.catalogue().name())
        .collect();
    assert_eq!(
        names,
        [
            "musicbrainz",
            "osm",
            "wikidata",
            "openalex",
            "podcastindex",
            "tranco",
            "software"
        ]
    );
}

/// Tranco's plan reads what the last run stored in its position: the next site due
/// (`next_due`) and when the day's list was looked up (`checked`), both RFC 3339 times. With the
/// last run an hour ago (past the ten-minute floor), a site due in 20 minutes wakes it then; the
/// list looked up 25 hours ago wakes it now, whatever the next site; an unreadable time counts
/// as none.
#[sqlx::test(migrations = "../../migrations")]
async fn tranco_plans_from_its_stored_position(pool: PgPool) {
    use pinakes_core::catalogue::run::{Importer, Plan};
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("list.csv");
    std::fs::write(&list, "1,example.org\n").unwrap();
    let importer = tranco_over(&list, &Sites::default(), &[]);
    store::ensure_state(&pool, Catalogue::Tranco).await.unwrap();
    let now = chrono::Utc::now();
    let plan_with = |position: serde_json::Value| {
        let pool = pool.clone();
        let importer = &importer;
        async move {
            sqlx::query("UPDATE catalogue_state SET position=$1,last_update_at=now()-interval '1 hour' WHERE catalogue='tranco'")
                .bind(position)
                .execute(&pool)
                .await
                .unwrap();
            importer
                .plan(&state_of(&pool, Catalogue::Tranco).await)
                .await
                .unwrap()
        }
    };
    let minutes = |plan: Plan| match plan {
        Plan::Wait(wait) => wait.as_secs_f64() / 60.0,
        Plan::Run(_) => 0.0,
    };
    let at = |t: chrono::DateTime<chrono::Utc>| t.to_rfc3339();

    // The next site due in 20 minutes, the list looked up just now: wait about 20 minutes.
    let plan = plan_with(serde_json::json!({
        "next": "list",
        "next_due": at(now + chrono::Duration::minutes(20)),
        "checked": at(now),
    }))
    .await;
    assert!((19.0..=20.1).contains(&minutes(plan)), "{plan:?}");
    // Stored in another offset, the same instant.
    let plan = plan_with(serde_json::json!({
        "next": "list",
        "next_due": (now + chrono::Duration::minutes(20))
            .with_timezone(&chrono::FixedOffset::east_opt(5 * 3600).unwrap())
            .to_rfc3339(),
        "checked": at(now),
    }))
    .await;
    assert!((19.0..=20.1).contains(&minutes(plan)), "{plan:?}");
    // The list looked up 25 hours ago: the day's lookup is due now, before the next site.
    let plan = plan_with(serde_json::json!({
        "next": "list",
        "next_due": at(now + chrono::Duration::hours(5)),
        "checked": at(now - chrono::Duration::hours(25)),
    }))
    .await;
    assert!(matches!(plan, Plan::Run(_)), "{plan:?}");
    // An unreadable `next_due`: no site counts as due, so the plan waits its longest (an hour,
    // then asks again), not the 20 minutes above.
    let plan = plan_with(serde_json::json!({
        "next": "list",
        "next_due": "in twenty minutes",
        "checked": at(now),
    }))
    .await;
    assert!(minutes(plan) > 59.0, "{plan:?}");
}

/// The free-disk check counts the files a run writes under `catalogues.dir` only: OpenAlex,
/// software and Tranco write none there (they read streams; their cards' share of the database is
/// sized apart); Podcast Index needs room for its unpacked database alone, less what a kept one
/// holds, and a local `.db` none.
#[sqlx::test(migrations = "../../migrations")]
async fn the_file_catalogues_ask_disk_for_their_own_files_only(pool: PgPool) {
    use pinakes_core::catalogue::run::Importer;
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let state = store::ensure_state(&pool, Catalogue::Openalex)
        .await
        .unwrap();
    let url = |u: &str| Input::Url(url::Url::parse(u).unwrap());
    let openalex = openalex::OpenAlex::new(url(openalex::BUCKET), "test@example.org").unwrap();
    let software =
        software::Software::new(software::Sources::official(), "test@example.org").unwrap();
    let list = dir.path().join("list.csv");
    std::fs::write(&list, "1,example.org\n").unwrap();
    let tranco = tranco_over(&list, &Sites::default(), &[]);
    for run in [Run::Import, Run::Update] {
        assert_eq!(openalex.disk_need(run, &state), 0);
        assert_eq!(software.disk_need(run, &state), 0);
        assert_eq!(tranco.disk_need(run, &state), 0);
    }
    let podcasts = podcastindex::PodcastIndex::new(url(podcastindex::DATABASE), "test@example.org")
        .unwrap()
        .configured(&ctx.config);
    // Room for the database at its cap (12 GiB).
    assert_eq!(podcasts.disk_need(Run::Import, &state), 12 << 30);
    let work = ctx.config.catalogues.work_dir(Catalogue::Podcastindex);
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("podcastindex_feeds.db"), vec![0u8; 4096]).unwrap();
    assert_eq!(podcasts.disk_need(Run::Update, &state), (12 << 30) - 4096);
    let local = podcastindex::PodcastIndex::new(
        Input::File(dir.path().join("feeds.db")),
        "test@example.org",
    )
    .unwrap();
    assert_eq!(local.disk_need(Run::Import, &state), 0);
}

/// Tranco removes at most a tenth of its cards (or ten) in a run: a list that leaves more off it
/// for a month is refused (nothing removed, the list named in the position, nothing read within
/// the day), and so is the rest of a run whose homepage reads would remove more (every read
/// failing).
#[sqlx::test(migrations = "../../migrations")]
async fn tranco_removes_at_most_a_tenth_in_a_run(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    let list = dir.path().join("list.csv");
    let hosts: Vec<String> = (1..=12).map(|n| format!("site{n}.example")).collect();
    let text: String = hosts
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{},{h}\n", i + 1))
        .collect();
    std::fs::write(&list, text).unwrap();
    let sites = Sites::default();
    for host in &hosts {
        sites.page(&format!("https://{host}/"), host);
    }
    let importer = Arc::new(tranco_over(&list, &sites, &[]));
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 12);
    let newer = || {
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&list)
            .unwrap()
            .set_modified(later)
            .unwrap();
    };
    // A new list leaves eleven sites off it for over a month: refused, nothing removed.
    sqlx::query("UPDATE catalogue_site SET listed_on=listed_on-60")
        .execute(&pool)
        .await
        .unwrap();
    std::fs::write(&list, "1,site1.example\n").unwrap();
    newer();
    a_day_passed(&pool).await;
    let failed = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(failed.to_string().contains("off the list"), "{failed}");
    assert_eq!(count(&pool, "tranco", None).await, 12);
    let position = state_of(&pool, Catalogue::Tranco).await.position.unwrap();
    assert!(position["refused"].is_string(), "{position}");
    // Within the day nothing is read.
    let again = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(again.to_string().contains("was refused before"), "{again}");
    // The sites are listed again, the refusal cleared; every homepage now answers 404 (no card):
    // the run removes at most ten, then refuses the rest.
    store::clear_refusal(&pool, Catalogue::Tranco)
        .await
        .unwrap();
    sqlx::query("UPDATE catalogue_site SET listed_on=current_date,due_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    for host in &hosts {
        sites.set(&format!("https://{host}/"), 404, "text/plain", vec![]);
    }
    let failed = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(failed.to_string().contains("homepage reads"), "{failed}");
    assert!(count(&pool, "tranco", None).await >= 2);
}

/// The shrink floor holds per registry: a full import reading a short Flathub file (one app of
/// three) fails `shrunk` and removes none of Flathub's cards, though the catalogue as a whole
/// read most of its cards; `allow-shrink` lets the next try through.
#[sqlx::test(migrations = "../../migrations")]
async fn a_short_registry_file_meets_its_own_floor(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let files = Files::default();
    let sources = software_server(&files, 60).await;
    let importer = Arc::new(software::Software::new(sources, "test@example.org").unwrap());
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "software", Some("flathub")).await, 3);
    let full = String::from_utf8(fixture("software", "flathub-appstream.xml")).unwrap();
    let second = full.match_indices("<component ").nth(1).unwrap().0;
    let short = format!("{}</components>\n", &full[..second]);
    files.put("/appstream.xml.gz", gzip(short.as_bytes()), "f2");
    let failed = run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap_err();
    assert!(
        failed
            .to_string()
            .contains("shrunk (flathub: 1 of 3 cards read)"),
        "{failed}"
    );
    assert_eq!(count(&pool, "software", Some("flathub")).await, 3);
    assert_eq!(
        store::allow_shrink(&pool, Catalogue::Software)
            .await
            .unwrap(),
        store::Allowance::Allowed
    );
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "software", Some("flathub")).await, 1);
}

/// `pinakes_catalogue_items_skipped_total` of `catalogue` for `reason` (a counter of the process,
/// which other tests may move too: compare a rise).
fn skipped(catalogue: &str, reason: &str) -> f64 {
    let text = pinakes_core::telemetry::Metrics::new()
        .unwrap()
        .encode()
        .unwrap();
    let series = format!(
        r#"pinakes_catalogue_items_skipped_total{{catalogue="{catalogue}",reason="{reason}"}} "#
    );
    text.lines()
        .find_map(|l| l.strip_prefix(series.as_str()))
        .unwrap()
        .parse()
        .unwrap()
}

/// OpenAlex counts the works it passes over (a line that doesn't parse) in
/// `pinakes_catalogue_items_skipped_total`, as it logs them.
#[sqlx::test(migrations = "../../migrations")]
async fn openalex_counts_the_lines_it_passes_over(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), |_| {});
    let mut lines = works();
    lines.push(r#"{"id":"https://openalex.org/W9","cited_by_count":99,"#.to_owned());
    snapshot(snap.path(), "2026-10-14", &[("2026-10-14", lines)]);
    let before = skipped("openalex", "bad_line");
    let importer = Arc::new(
        openalex::OpenAlex::new(Input::File(snap.path().to_owned()), "test@example.org").unwrap(),
    );
    run::run_once(importer, &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "openalex", None).await, 5);
    assert!(skipped("openalex", "bad_line") >= before + 1.0);
}

/// A Tranco refusal never wedges the catalogue: a full import asked for (`--request`) has no cap
/// on a run's removals, so a list that leaves most sites off it for good goes through.
#[sqlx::test(migrations = "../../migrations")]
async fn a_tranco_refusal_gives_way_to_a_full_import_asked_for(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    let list = dir.path().join("list.csv");
    let hosts: Vec<String> = (1..=12).map(|n| format!("site{n}.example")).collect();
    let text: String = hosts
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{},{h}\n", i + 1))
        .collect();
    std::fs::write(&list, text).unwrap();
    let sites = Sites::default();
    for host in &hosts {
        sites.page(&format!("https://{host}/"), host);
    }
    let importer = Arc::new(tranco_over(&list, &sites, &[]));
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    sqlx::query("UPDATE catalogue_site SET listed_on=listed_on-60")
        .execute(&pool)
        .await
        .unwrap();
    std::fs::write(&list, "1,site1.example\n").unwrap();
    let later = std::time::SystemTime::now() + Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&list)
        .unwrap()
        .set_modified(later)
        .unwrap();
    a_day_passed(&pool).await;
    let failed = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(failed.to_string().contains("off the list"), "{failed}");
    assert_eq!(count(&pool, "tranco", None).await, 12);
    // A day on, the list can't be looked up: with the refusal standing nothing is read, and the
    // refusal stays (no position without it is written).
    a_day_passed(&pool).await;
    std::fs::rename(&list, dir.path().join("aside.csv")).unwrap();
    let again = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(again.to_string().contains("was refused before"), "{again}");
    assert!(state_of(&pool, Catalogue::Tranco)
        .await
        .position
        .unwrap()
        .get("refused")
        .is_some());
    std::fs::rename(dir.path().join("aside.csv"), &list).unwrap();
    // Asked for, the task's next step is a full import that removes the sites off the list.
    run::request_import(
        &pool,
        Catalogue::Tranco,
        pinakes_core::catalogue::run::REQUESTED,
    )
    .await
    .unwrap();
    run::step(&*importer, &ctx).await.unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 1);
    let state = state_of(&pool, Catalogue::Tranco).await;
    assert!(state.import_requested.is_none());
    assert!(state.position.unwrap().get("refused").is_none());
}

/// A round of homepage reads that would take the run's removals past the cap writes nothing of
/// itself: every site is either as its round left it (the rounds before, applied whole) or as it
/// was before the run (its card, its last outcome and read), never half. (A site whose read was
/// still in flight keeps its claim's hour, as in any run cut off.)
#[sqlx::test(migrations = "../../migrations")]
async fn a_refused_round_of_homepage_reads_writes_nothing_of_it(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&pool, dir.path(), without_large);
    let list = dir.path().join("list.csv");
    let hosts: Vec<String> = (1..=30).map(|n| format!("site{n}.example")).collect();
    let text: String = hosts
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{},{h}\n", i + 1))
        .collect();
    std::fs::write(&list, text).unwrap();
    let sites = Sites::default();
    for host in &hosts {
        sites.page(&format!("https://{host}/"), host);
    }
    let importer = Arc::new(tranco_over(&list, &sites, &[]));
    run::run_once(importer.clone(), &ctx, Run::Import, |_| {})
        .await
        .unwrap();
    assert_eq!(count(&pool, "tranco", None).await, 30);
    let rows = || async {
        sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
            ),
        >("SELECT host,outcome,read_at FROM catalogue_site ORDER BY host")
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    sqlx::query("UPDATE catalogue_site SET due_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let before = rows().await;
    for host in &hosts {
        sites.set(&format!("https://{host}/"), 404, "text/plain", vec![]);
    }
    let failed = run::run_once(importer.clone(), &ctx, Run::Update, |_| {})
        .await
        .unwrap_err();
    assert!(failed.to_string().contains("homepage reads"), "{failed}");
    let carded: std::collections::HashSet<String> =
        sqlx::query_scalar("SELECT source_id FROM catalogue_card WHERE catalogue='tranco'")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert!(carded.len() >= 20, "{}", carded.len());
    for (row, was) in rows().await.into_iter().zip(before) {
        if carded.contains(&row.0) {
            assert_eq!(row, was, "a site of the refused round is as it was");
        } else {
            assert_eq!(row.1.as_deref(), Some("status"), "{row:?}");
        }
    }
}
