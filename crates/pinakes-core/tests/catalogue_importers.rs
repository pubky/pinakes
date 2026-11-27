//! The OpenAlex, Podcast Index, software and Tranco importers, each run through the daemon's own
//! runner (`catalogue::run`) over a fresh database, reading recorded real records served by a
//! local HTTP server or from local files: the first import (cards, keys, labels, links), an
//! update that changes only what changed, an update refused for removing too much (parked, the
//! file not read again), and the joins of shared links with the cards.
mod common;

use pinakes_core::{
    catalogue::{
        fetch::Input,
        join, podcastindex,
        run::{self, Context, Pace, Run},
        software, store, Catalogue,
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
/// A local HTTP server of files: each path's body and ETag; it counts GETs per path.
#[derive(Clone, Default)]
struct Files {
    files: Arc<Mutex<Served>>,
    gets: Arc<Mutex<HashMap<String, usize>>>,
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
    assert!(failed.to_string().contains("removals"), "{failed}");
    assert_eq!(count(&pool, "software", Some("crates.io")).await, 66);
    let state = state_of(&pool, Catalogue::Software).await;
    assert_eq!((state.run.as_deref(), state.cursor.as_ref()), (None, None));
    assert_eq!(state.error_count, 1);
    let position = state.position.clone().unwrap();
    assert_eq!(
        position["refused_file"]["crates.io"], "\"c2\"",
        "{position}"
    );
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
        state.position.as_ref().unwrap()["refused_file"]["crates.io"],
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
    assert!(state
        .position
        .as_ref()
        .unwrap()
        .get("refused_file")
        .is_none());
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
        .get("refused_file")
        .is_some());
    run::request_import(
        &pool,
        Catalogue::Software,
        pinakes_core::catalogue::musicbrainz::REQUESTED,
    )
    .await
    .unwrap();
    run::step(&*importer, &ctx).await.unwrap();
    assert_eq!(count(&pool, "software", Some("crates.io")).await, 46);
    let state = state_of(&pool, Catalogue::Software).await;
    assert!(state.import_requested.is_none());
    assert!(state
        .position
        .as_ref()
        .unwrap()
        .get("refused_file")
        .is_none());
    assert_eq!(state.run, None);
}

/// A full import of a registry whose file holds no card fails and removes nothing.
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
    assert!(failed.to_string().contains("parse"), "{failed}");
    assert_eq!(count(&pool, "software", Some("flathub")).await, 3);
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
/// again; a full import asked for then runs, and fails (`parse`) on a database with no live feed,
/// removing nothing; a newer archive is then imported.
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
    assert!(failed.to_string().contains("removals"), "{failed}");
    assert_eq!(count(&pool, "podcastindex", None).await, 11);
    let work = ctx.config.catalogues.work_dir(Catalogue::Podcastindex);
    assert!(
        !work.join("podcastindex_feeds.db").exists(),
        "deleted on refusal"
    );
    let state = state_of(&pool, Catalogue::Podcastindex).await;
    assert_eq!((state.run.as_deref(), state.error_count), (None, 1));
    assert_eq!(state.position.as_ref().unwrap()["refused_file"], "\"p2\"");
    let table = store::status_table(&ctx.config.catalogues, std::slice::from_ref(&state));
    assert!(table.contains("file \"p2\" refused"), "{table}");
    // The task's next update: no download, no failure.
    let gets = files.gets("/podcastindex_feeds.db.tgz");
    run::step(&*importer, &ctx).await.unwrap();
    assert_eq!(files.gets("/podcastindex_feeds.db.tgz"), gets);
    assert_eq!(
        state_of(&pool, Catalogue::Podcastindex).await.error_count,
        1
    );
    // A full import asked for: it runs, finds no live feed and fails, removing nothing.
    run::request_import(
        &pool,
        Catalogue::Podcastindex,
        pinakes_core::catalogue::musicbrainz::REQUESTED,
    )
    .await
    .unwrap();
    let failed = run::step(&*importer, &ctx).await.unwrap_err();
    assert!(
        pinakes_core::catalogue::failure::shown(&failed).contains("parse"),
        "{failed}"
    );
    assert_eq!(count(&pool, "podcastindex", None).await, 11);
    assert_eq!(files.gets("/podcastindex_feeds.db.tgz"), gets + 1);
    // A newer archive, one feed gone: the import under way takes it.
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
