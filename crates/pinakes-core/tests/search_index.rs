//! The text index (`search_index`): it follows the database's writes within a pass, a deleted
//! card never shows, a broken directory is rebuilt, `reindex` switches generations, typos match
//! only when the words find few cards, and activity counts on a log scale.
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    search_index::{self, SearchIndex, TextQuery},
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::{path::PathBuf, time::Duration};
mod common;
use common::author;

/// A card named `name`, tagged `label` by `taggers` authors.
async fn card(
    pool: &PgPool,
    n: usize,
    name: &str,
    description: &str,
    label: &str,
    taggers: usize,
) -> String {
    let uri = format!("https://example.org/index/{n}");
    let id = store::ensure_resource(pool, &uri).await.unwrap();
    let mut doc = ResourceMeta::new(&uri, "web", 86400);
    doc.name = Some(name.into());
    doc.description = Some(description.into());
    doc.set_type("WebPage");
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    // One statement for many taggers (a flood of bot accounts).
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) SELECT $1,'bot'||$2||'-'||g,'tag',$3,'/pub/app/tags/'||$2||'-'||g,now() FROM generate_series(1,$4) g")
        .bind(&id)
        .bind(n as i64)
        .bind(label)
        .bind(taggers as i32)
        .execute(pool)
        .await
        .unwrap();
    rescore(pool, &id).await;
    id
}
async fn rescore(pool: &PgPool, id: &str) {
    let mut tx = pool.begin().await.unwrap();
    store::recompute_scores(&mut tx, &[id.to_owned()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
async fn tag(pool: &PgPool, id: &str, label: &str, a: usize) {
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag',$3,$4,now())")
        .bind(id)
        .bind(author(a))
        .bind(label)
        .bind(format!("/pub/app/tags/new-{a}"))
        .execute(pool)
        .await
        .unwrap();
    rescore(pool, id).await;
}
fn ids_of(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect()
}
fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pinakes-index-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
fn query(text: &str, tags: &[&str]) -> TextQuery {
    TextQuery {
        text: text.into(),
        at: chrono::Utc::now(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        inside_words: true,
        ..Default::default()
    }
}
/// The unit IDs the index holds for a query.
async fn found(index: &SearchIndex, pool: &PgPool, q: TextQuery) -> Vec<String> {
    index
        .candidates(pool, q, 500)
        .await
        .unwrap()
        .map(|c| c.units)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.unit_id)
        .collect()
}
/// Waits until `check` holds, at most ten seconds: how long it took.
async fn within<F: std::future::Future<Output = bool>>(mut check: impl FnMut() -> F) -> Duration {
    let started = std::time::Instant::now();
    while !check().await {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "not within ten seconds"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    started.elapsed()
}

/// The daemon's index on disk: built at start (the directory is missing), it holds a new tag
/// within a pass; a deleted card leaves every search at once, before the index's pass, and the
/// index within it; the queue rows a commit covers are deleted.
#[sqlx::test(migrations = "../../migrations")]
async fn the_disk_index_follows_writes_within_a_pass(pool: PgPool) {
    let aurora = card(&pool, 1, "Aurora", "Northern lights.", "sky", 1).await;
    card(&pool, 2, "Aurora borealis photos", "A gallery.", "sky", 2).await;
    let dir = scratch("follows");
    let index = SearchIndex::on_disk(&dir);
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    // Before the index is open a text search reads Postgres.
    assert_eq!(
        ids_of(&common::get(&app, "/v1/search?q=aurora").await).len(),
        2
    );
    let passed = prometheus::IntGauge::new("passed", "test").unwrap();
    let opened = prometheus::IntGauge::new("opened", "test").unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(index.clone().run(
        pool.clone(),
        Duration::from_millis(200),
        passed.clone(),
        opened.clone(),
        shutdown,
    ));
    within(|| async { index.is_open() }).await;
    within(|| async { passed.get() > 0 }).await;
    assert!(std::fs::read_to_string(dir.join("CURRENT"))
        .unwrap()
        .starts_with("gen-"));
    assert_eq!(found(&index, &pool, query("aurora", &[])).await.len(), 2);

    // A new tag: searchable within a pass.
    assert!(found(&index, &pool, query("aurora", &["fresh"]))
        .await
        .is_empty());
    tag(&pool, &aurora, "fresh", 7).await;
    let took = within(|| async {
        found(&index, &pool, query("aurora", &["fresh"])).await == std::slice::from_ref(&aurora)
    })
    .await;
    assert!(took < Duration::from_secs(5), "{took:?}");
    let page = common::get(&app, "/v1/search?q=aurora&tags=fresh").await;
    assert_eq!(ids_of(&page), std::slice::from_ref(&aurora));

    // Deleted: gone from search at once, from the index within a pass.
    sqlx::query("DELETE FROM resource WHERE id=$1")
        .bind(&aurora)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!ids_of(&common::get(&app, "/v1/search?q=aurora").await).contains(&aurora));
    within(|| async {
        !found(&index, &pool, query("aurora", &[]))
            .await
            .contains(&aurora)
    })
    .await;
    // The queue keeps nothing the index's commits cover.
    within(|| async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM search_index_queue")
            .fetch_one(&pool)
            .await
            .unwrap()
            == 0
    })
    .await;
    stop.send(true).unwrap();
    task.await.unwrap();

    // A directory whose generation is gone is rebuilt at start.
    std::fs::write(dir.join("CURRENT"), "gen-1\n").unwrap();
    let again = SearchIndex::on_disk(&dir);
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(again.clone().run(
        pool.clone(),
        Duration::from_millis(200),
        passed.clone(),
        opened.clone(),
        shutdown,
    ));
    within(|| async { again.is_open() }).await;
    assert_eq!(found(&again, &pool, query("aurora", &[])).await.len(), 1);
    stop.send(true).unwrap();
    task.await.unwrap();

    // `reindex` builds the next generation and switches to it; the old one is removed.
    let before = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
    let mut reported = 0;
    let units = search_index::reindex(&pool, &dir, &[], |n| reported = n)
        .await
        .unwrap();
    assert_eq!(units, 1);
    let after = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
    assert_ne!(before, after);
    assert!(!dir.join(before.trim()).exists());
    assert!(dir.join(after.trim()).is_dir());
    let _ = reported;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Typos and similar names match as Postgres' trigram similarity takes them (`pg_trgm`'s `%`):
/// the index proposes names sharing pieces of the text, and Postgres keeps the near ones only,
/// whatever the words find. An unrelated name sharing a few pieces is never listed.
#[sqlx::test(migrations = "../../migrations")]
async fn typos_match_names_as_similar_as_postgres(pool: PgPool) {
    let kraftwerk = card(&pool, 1, "Kraftwerk", "A band.", "music", 1).await;
    let musik = card(&pool, 2, "Musik", "A German word.", "words", 1).await;
    let beatles = card(&pool, 3, "The Beatles", "A band.", "music", 1).await;
    let seat = card(&pool, 4, "Seat belt stories", "Safety.", "cars", 1).await;
    card(&pool, 5, "Lolita", "A novel.", "books", 1).await;
    card(&pool, 6, "Don Quixote", "A novel.", "books", 1).await;
    // The fewest candidates the configuration allows, so the proposals' limit is reached below.
    let mut config = Config::default();
    config.search.text_candidates = 50;
    let cap = config.search.text_candidates;
    let index = SearchIndex::in_memory();
    let mut state = AppState::new(pool.clone(), config).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    for q in ["kraftwerg", "kraftwrek"] {
        assert_eq!(
            ids_of(&common::get(&app, &format!("/v1/search?q={q}")).await),
            std::slice::from_ref(&kraftwerk),
            "{q}"
        );
    }
    let found = ids_of(&common::get(&app, "/v1/search?q=beatles").await);
    assert!(found.contains(&beatles), "{found:?}");
    assert!(
        !found.contains(&seat),
        "a few shared pieces are not a similar name"
    );
    for q in ["Moilit%C3%A4t", "Tostoy"] {
        assert!(
            ids_of(&common::get(&app, &format!("/v1/search?q={q}")).await).is_empty(),
            "{q}"
        );
    }
    // A similar name is listed while fewer names than the candidates hold the words; with that
    // many, the candidates are those names, and the proposals are not read.
    for n in 7..7 + cap - 1 {
        card(&pool, n, &format!("Music {n}"), "Sound.", "music", 1).await;
    }
    let before = index.proposed();
    let page = ids_of(&common::get(&app, "/v1/search?q=music&limit=200").await);
    assert!(page.contains(&musik), "{} cards", page.len());
    assert_eq!(index.proposed(), before + 1);
    card(&pool, 500, "Music 500", "Sound.", "music", 1).await;
    let page = ids_of(&common::get(&app, "/v1/search?q=music&limit=200").await);
    assert_eq!(page.len(), cap);
    assert!(!page.contains(&musik));
    assert_eq!(index.proposed(), before + 1, "the proposals are not read");
}

/// A pass that adds, changes or tags documents merges nothing: only a document with text that
/// loses it (a post found gone) merges, and only the segments that held it.
#[sqlx::test(migrations = "../../migrations")]
async fn an_ordinary_pass_merges_nothing(pool: PgPool) {
    let index = SearchIndex::in_memory();
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    let first = card(&pool, 1, "Aurora", "A page.", "sky", 1).await;
    common::get(&app, "/v1/search?q=aurora").await;
    // New links nobody has read yet (no name: no text), tags, a changed name.
    for n in 2..12 {
        let id = store::ensure_resource(&pool, &format!("https://example.org/pending/{n}"))
            .await
            .unwrap();
        tag(&pool, &id, "new", n).await;
        common::get(&app, "/v1/search?q=aurora").await;
    }
    tag(&pool, &first, "fresh", 99).await;
    let second = card(&pool, 20, "Comet", "A page.", "space", 1).await;
    common::get(&app, "/v1/search?q=comet").await;
    let mut doc = ResourceMeta::new("https://example.org/index/20", "web", 86400);
    doc.name = Some("Comet Hale-Bopp".into());
    doc.set_type("WebPage");
    sqlx::query("UPDATE resource SET name=$2, doc=$3 WHERE id=$1")
        .bind(&second)
        .bind("Comet Hale-Bopp")
        .bind(serde_json::to_value(&doc).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    common::get(&app, "/v1/search?q=bopp").await;
    index.settle();
    assert_eq!(index.purges(), 0);
    // A post found gone: one merge.
    sqlx::query("UPDATE resource SET name=NULL, doc=jsonb_build_object('type','SocialMediaPosting','type_path',jsonb_build_array('Thing','CreativeWork','SocialMediaPosting'),'url',uri,'status','missing') WHERE id=$1")
        .bind(&first)
        .execute(&pool)
        .await
        .unwrap();
    common::get(&app, "/v1/search?q=aurora").await;
    index.settle();
    assert_eq!(index.purges(), 1);
    index.settle();
    assert!(!index.holds_word("aurora"));
}

/// An index built from one database is never opened as another's on the same cluster: the daemon
/// rebuilds it from the database it is given.
#[sqlx::test(migrations = "../../migrations")]
async fn an_index_of_another_database_is_rebuilt(pool: PgPool) {
    card(&pool, 1, "Aurora", "A page.", "sky", 1).await;
    let dir = scratch("other-db");
    let run = |pool: PgPool| {
        let index = SearchIndex::on_disk(&dir);
        let passed = prometheus::IntGauge::new("passed", "test").unwrap();
        let opened = prometheus::IntGauge::new("opened", "test").unwrap();
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(index.clone().run(
            pool,
            Duration::from_millis(100),
            passed,
            opened.clone(),
            shutdown,
        ));
        (index, opened, stop, task)
    };
    let (first, opened, stop, task) = run(pool.clone());
    within(|| async { opened.get() == 1 }).await;
    assert_eq!(found(&first, &pool, query("aurora", &[])).await.len(), 1);
    stop.send(true).unwrap();
    task.await.unwrap();
    // A second database on the same cluster, with its own card.
    let name = format!("pinakes_index_other_{}", std::process::id());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&pool)
        .await
        .unwrap();
    let other = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect_with(pool.connect_options().as_ref().clone().database(&name))
        .await
        .unwrap();
    pinakes_core::migrate(&other).await.unwrap();
    // As a copy restored beside it would, it carries the first database's token.
    let token: String = sqlx::query_scalar("SELECT token::text FROM search_index_database")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE search_index_database SET token=$1::uuid")
        .bind(&token)
        .execute(&other)
        .await
        .unwrap();
    card(&other, 2, "Comet", "A page.", "space", 1).await;
    let (second, opened, stop, task) = run(other.clone());
    within(|| async { opened.get() == 1 }).await;
    assert!(found(&second, &other, query("aurora", &[]))
        .await
        .is_empty());
    assert_eq!(found(&second, &other, query("comet", &[])).await.len(), 1);
    stop.send(true).unwrap();
    task.await.unwrap();
    other.close().await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(&pool)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Activity counts on a log scale: a card a thousand accounts tagged does not pass the card the
/// text names exactly, and of two cards the text matches equally, the more tagged comes first.
#[sqlx::test(migrations = "../../migrations")]
async fn activity_counts_on_a_log_scale(pool: PgPool) {
    let exact = card(&pool, 1, "Aurora", "A page.", "sky", 1).await;
    let flooded = card(
        &pool,
        2,
        "Aurora borealis timelapse",
        "A video.",
        "sky",
        1000,
    )
    .await;
    let quiet = card(&pool, 3, "Comet", "A page.", "space", 1).await;
    let tagged = card(&pool, 4, "Comet", "A page.", "space", 5).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    assert_eq!(
        ids_of(&common::get(&app, "/v1/search?q=aurora").await),
        [exact, flooded]
    );
    assert_eq!(
        ids_of(&common::get(&app, "/v1/search?q=comet").await),
        [tagged, quiet]
    );
}

/// Search rows carry `labels` (none from referenced links), other routes do not; `referenced=true`
/// is accepted and, with every card referenced, changes nothing; text matches folded letters.
#[sqlx::test(migrations = "../../migrations")]
async fn labels_referenced_and_folded_letters(pool: PgPool) {
    let zurich = card(&pool, 1, "Zürich", "A city.", "city", 2).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    for q in ["zurich", "ZURICH", "Zürich", "zur"] {
        let page = common::get(&app, &format!("/v1/search?q={q}")).await;
        assert_eq!(ids_of(&page), std::slice::from_ref(&zurich), "{q}");
        assert_eq!(page["items"][0]["labels"], serde_json::json!([]));
    }
    let plain = common::get(&app, "/v1/search?type=WebPage").await;
    assert_eq!(plain["items"][0]["labels"], serde_json::json!([]));
    for path in [
        "/v1/search?q=zurich&referenced=true",
        "/v1/search?type=WebPage&referenced=true",
        "/v1/search?referenced=false",
    ] {
        assert_eq!(
            ids_of(&common::get(&app, path).await),
            std::slice::from_ref(&zurich),
            "{path}"
        );
    }
    let read = common::get(&app, &format!("/v1/resources/{zurich}")).await;
    assert!(read.get("labels").is_none(), "{read}");
}

/// A pass reads the queue in batches until it runs dry: past a batch of transactions it has read
/// already, the next writes are found (with two rows a batch, as with 20,000 under load).
#[sqlx::test(migrations = "../../migrations")]
async fn a_pass_reads_every_batch_of_the_queue(pool: PgPool) {
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = SearchIndex::in_memory_with_batch(2);
    let app = api::router(state);
    let first = card(&pool, 1, "Aurora", "A page.", "sky", 1).await;
    assert_eq!(
        ids_of(&common::get(&app, "/v1/search?q=aurora").await),
        std::slice::from_ref(&first)
    );
    let mut all = vec![first];
    for n in 2..8 {
        all.push(card(&pool, n, &format!("Aurora {n}"), "A page.", "sky", 1).await);
        let mut found = ids_of(&common::get(&app, "/v1/search?q=aurora").await);
        found.sort();
        let mut expected = all.clone();
        expected.sort();
        assert_eq!(found, expected, "after card {n}");
    }
    // Many resources written between two searches: rows of every one of them, batch after batch.
    for n in 8..16 {
        all.push(card(&pool, n, &format!("Aurora {n}"), "A page.", "sky", 1).await);
    }
    let mut found = ids_of(&common::get(&app, "/v1/search?q=aurora").await);
    found.sort();
    all.sort();
    assert_eq!(found, all);
}

/// A post found gone or emptied that keeps its tags is never found by its old words: not before
/// the index's pass (Postgres checks the candidate still has text), and after the pass its words
/// are gone from the index files too.
#[sqlx::test(migrations = "../../migrations")]
async fn an_emptied_post_is_not_found_by_its_old_words(pool: PgPool) {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let post = card(&pool, 1, "Zyxquorum sunset", "A post.", "sky", 3).await;
    card(&pool, 2, "Sunset boulevard", "A film.", "film", 1).await;
    // Enough cards in one segment that one deleted document stays under the merge policy's share:
    // only the pass's own merge takes the post's words out of the files.
    for n in 3..60 {
        card(&pool, n, &format!("Sunset {n}"), "A page.", "sky", 1).await;
    }
    let dir = scratch("emptied");
    let index = SearchIndex::on_disk(&dir);
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    let passed = prometheus::IntGauge::new("passed", "test").unwrap();
    let opened = prometheus::IntGauge::new("opened", "test").unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(index.clone().run(
        pool.clone(),
        Duration::from_millis(200),
        passed.clone(),
        opened.clone(),
        shutdown,
    ));
    within(|| async { opened.get() == 1 && passed.get() > 0 }).await;
    index.settle();
    assert!(index.holds_word("zyxquorum"));
    assert_eq!(
        ids_of(&common::get(&app, "/v1/search?q=zyxquorum").await),
        std::slice::from_ref(&post)
    );
    // No pass from here: the index still holds the post's words.
    stop.send(true).unwrap();
    task.await.unwrap();
    // Found gone: the card keeps nothing of the post (`changes::gone_card`).
    let mut gone = ResourceMeta::new("https://example.org/index/1", "web", 86400);
    gone.set_type("SocialMediaPosting");
    gone.status = pinakes_core::model::Status::Missing;
    sqlx::query("UPDATE resource SET name=NULL, type=$2, type_path=$3, doc=$4 WHERE id=$1")
        .bind(&post)
        .bind(&gone.type_name)
        .bind(&gone.type_path)
        .bind(serde_json::to_value(&gone).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    index.settle();
    assert!(index.holds_word("zyxquorum"));
    for q in ["zyxquorum", "zyxquorum%20sunset"] {
        assert!(
            !ids_of(&common::get(&app, &format!("/v1/search?q={q}")).await).contains(&post),
            "{q}"
        );
    }
    // It keeps its tags: listed without text, as before.
    assert!(ids_of(&common::get(&app, "/v1/search?tags=sky").await).contains(&post));
    // The next pass removes its words from the files.
    index.sync(&pool).await.unwrap();
    index.settle();
    assert!(!index.holds_word("zyxquorum"));
    index.settle();
    assert!(index.holds_word("sunset"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// An index that cannot be built stays closed (its gauge 0, for the alert) and keeps the queue
/// from growing: the next build reads everything again. A build that fails leaves no partial
/// generation, and a generation `CURRENT` does not name is removed before the next build.
#[sqlx::test(migrations = "../../migrations")]
async fn a_closed_index_keeps_the_queue_short_and_no_partial_generation(pool: PgPool) {
    card(&pool, 1, "Aurora", "A page.", "sky", 1).await;
    sqlx::query("INSERT INTO search_index_catalogue_queue(card_id) VALUES (1)")
        .execute(&pool)
        .await
        .unwrap();
    let queued = || async {
        sqlx::query_scalar::<_, i64>("SELECT (SELECT count(*) FROM search_index_queue)+(SELECT count(*) FROM search_index_catalogue_queue)")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert!(queued().await > 0);
    // A file where the directory should be: the index cannot be built.
    let file = scratch("closed");
    std::fs::write(&file, "not a directory").unwrap();
    let index = SearchIndex::on_disk(&file);
    let passed = prometheus::IntGauge::new("passed", "test").unwrap();
    let opened = prometheus::IntGauge::new("opened", "test").unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(index.clone().run(
        pool.clone(),
        Duration::from_millis(200),
        passed.clone(),
        opened.clone(),
        shutdown,
    ));
    within(|| async { queued().await == 0 }).await;
    assert_eq!(opened.get(), 0);
    assert!(!index.is_open());
    stop.send(true).unwrap();
    task.await.unwrap();
    let _ = std::fs::remove_file(&file);

    let dir = scratch("partial");
    std::fs::create_dir_all(dir.join("gen-5")).unwrap();
    std::fs::write(dir.join("CURRENT"), "gen-1\n").unwrap();
    // A database that goes away during the build.
    let gone = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy_with(pool.connect_options().as_ref().clone());
    gone.close().await;
    assert!(search_index::reindex(&gone, &dir, &[], |_| {})
        .await
        .is_err());
    let generations: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("gen-"))
        .collect();
    assert!(generations.is_empty(), "{generations:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The daemon's pass drains the queue batch after batch (two rows a batch here), deleting what it
/// read: every card written meanwhile is found, and the queue ends empty.
#[sqlx::test(migrations = "../../migrations")]
async fn the_daemon_drains_the_queue_batch_after_batch(pool: PgPool) {
    let dir = scratch("drain");
    let index = SearchIndex::on_disk_with_batch(&dir, 2);
    let passed = prometheus::IntGauge::new("passed", "test").unwrap();
    let opened = prometheus::IntGauge::new("opened", "test").unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(index.clone().run(
        pool.clone(),
        Duration::from_millis(100),
        passed.clone(),
        opened.clone(),
        shutdown,
    ));
    within(|| async { opened.get() == 1 }).await;
    let mut all = vec![];
    for n in 1..12 {
        all.push(card(&pool, n, &format!("Aurora {n}"), "A page.", "sky", 1).await);
    }
    all.sort();
    within(|| async {
        let mut found = found(&index, &pool, query("aurora", &[])).await;
        found.sort();
        found == all
    })
    .await;
    within(|| async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM search_index_queue")
            .fetch_one(&pool)
            .await
            .unwrap()
            == 0
    })
    .await;
    stop.send(true).unwrap();
    task.await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The names holding every word are counted only when the proposals may be read: a short text of
/// letters and digits matches only where words start and never reads them, so it counts nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_text_matched_at_word_starts_counts_no_names(pool: PgPool) {
    card(&pool, 1, "Mud flats", "A coast.", "coast", 1).await;
    let index = SearchIndex::in_memory();
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    assert_eq!(
        common::get(&app, "/v1/search?q=mud").await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(index.served() > 0);
    assert_eq!(index.counted(), 0);
    assert_eq!(index.proposed(), 0);
    common::get(&app, "/v1/search?q=mud%20flats").await;
    assert_eq!(index.counted(), 1);
}
