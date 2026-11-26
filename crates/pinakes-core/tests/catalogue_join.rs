//! Links joined with catalogue cards: a shared link a card holds (by its link or an exact key)
//! takes its card at once and is never fetched; it follows the card's changes and goes back to
//! the fetch queue when the card goes or its catalogue is off. A card nobody shared opens by its
//! link and by its ID without a stored row. Undated fetched cards wait longer each time they
//! answer the same.
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    catalogue::{join, link_key, store, Card, Catalogue},
    config::{Config, QueueConfig},
    model::{self, ResourceMeta, Status},
    queue,
    resolver::Resolver,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tower::ServiceExt;

const ARTICLE: &str = "https://en.wikipedia.org/wiki/Douglas_Adams";
const SPOTIFY: &str = "https://example.com/hitchhikers-guide/author";
const MB: &str = "https://musicbrainz.org/artist/e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4";
const MB_KEY: &str = "mb:artist:e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4";
const WEBSITE: &str = "https://douglasadams.com/";

fn queue_cfg() -> QueueConfig {
    QueueConfig {
        catalogues: join::joinable(&Default::default()),
        ..Default::default()
    }
}
/// The Wikidata card of Douglas Adams: its article, his MusicBrainz key, the Spotify link people
/// share for him as a `url:` key, and his website as a related link.
fn adams() -> Card {
    Card {
        source_id: "Q42".into(),
        uri: ARTICLE.into(),
        r#type: "Person".into(),
        name: "Douglas Adams".into(),
        description: Some("English writer and humourist".into()),
        labels: vec!["Science Fiction".into(), "humourist".into()],
        keys: vec![
            "wikidata:Q42".into(),
            MB_KEY.into(),
            link_key(SPOTIFY).unwrap(),
        ],
        related_uris: vec![WEBSITE.into()],
        fame: 0.9,
        ..Default::default()
    }
}
async fn import(pool: &PgPool, catalogue: Catalogue, cards: &[Card]) {
    import_at(pool, catalogue, 1, cards).await;
}
/// An import of `generation`: a later one stamps the cards it reads unchanged.
async fn import_at(pool: &PgPool, catalogue: Catalogue, generation: i64, cards: &[Card]) {
    let report = store::upsert(pool, catalogue, generation, cards, None)
        .await
        .unwrap();
    assert_eq!(report.rejected, 0);
}
/// A link someone tagged: its resource ID.
async fn tagged(pool: &PgPool, uri: &str) -> String {
    let id = pinakes_core::store::ensure_resource(pool, uri)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=NULL WHERE id=$1")
        .bind(&id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,kind,author,label,object_path,created_at,unit_key) VALUES($1,'tag',$2,'t','/pub/t/'||$1,now(),'tag:t') ON CONFLICT DO NOTHING")
        .bind(&id)
        .bind("y".repeat(52))
        .execute(pool)
        .await
        .unwrap();
    id
}
#[derive(sqlx::FromRow, Debug)]
struct Backed {
    catalogue_card: Option<i64>,
    name: Option<String>,
    status: String,
    #[sqlx(rename = "final")]
    is_final: bool,
    due: bool,
    labels: Option<Value>,
}
async fn backed(pool: &PgPool, id: &str) -> Backed {
    sqlx::query_as("SELECT catalogue_card,name,status,final AS final,next_attempt_at<=now() AS due,doc->'labels' AS labels FROM resource WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn catalogue_keys(pool: &PgPool, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT key FROM resource_key WHERE resource_id=$1 AND source='catalogue' ORDER BY key COLLATE \"C\"")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}
async fn drain(pool: &PgPool, cfg: &QueueConfig) {
    while join::drain(pool, cfg, 100).await.unwrap() > 0 {}
}

/// A web resolver that counts its fetches and answers with a card of its own.
struct Fetcher {
    fetches: AtomicUsize,
    answer: Mutex<Option<ResourceMeta>>,
}
impl Fetcher {
    fn new() -> Self {
        Self {
            fetches: AtomicUsize::new(0),
            answer: Mutex::new(None),
        }
    }
}
#[async_trait::async_trait]
impl Resolver for Fetcher {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer.lock().unwrap().clone().unwrap_or_else(|| {
            let mut doc = ResourceMeta::new(uri, "web", 0);
            doc.name = Some("Fetched page".into());
            doc
        }))
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        Duration::from_secs(doc.ttl_secs)
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_shared_link_joins_its_card_by_link_and_by_key(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    // By its link: the card's own article.
    let article = tagged(&pool, ARTICLE).await;
    // By the `url:` key of a link people share for the item.
    let spotify = tagged(&pool, SPOTIFY).await;
    // By a key the link names: a MusicBrainz artist page, no MusicBrainz card imported.
    let mb = tagged(&pool, MB).await;
    assert_eq!(
        join::referenced(&pool, &cfg, &[article.clone(), spotify.clone(), mb.clone()]).await,
        3
    );
    let card: i64 = sqlx::query_scalar("SELECT id FROM catalogue_card")
        .fetch_one(&pool)
        .await
        .unwrap();
    for id in [&article, &spotify, &mb] {
        let row = backed(&pool, id).await;
        assert_eq!(row.catalogue_card, Some(card), "{id}");
        assert_eq!(row.name.as_deref(), Some("Douglas Adams"));
        assert_eq!(row.status, "ok");
        assert!(row.is_final && !row.due);
        assert_eq!(row.labels, Some(json!(["science-fiction", "humourist"])));
        assert_eq!(
            catalogue_keys(&pool, id).await,
            ["catalogue:wikidata:Q42", MB_KEY, "wikidata:Q42"]
        );
    }
    let doc: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&article)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(doc["type"], "Person");
    assert_eq!(doc["source"], "catalogue");
    assert_eq!(doc["catalogue"]["catalogue"], "wikidata");
    assert_eq!(doc["catalogue"]["source_id"], "Q42");
    assert_eq!(doc["description"], "English writer and humourist");
    // The change journal holds the card as any saved card's.
    let lines: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&article)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(lines, 1);
    // A second reference changes nothing: the card is current.
    assert_eq!(
        join::referenced(&pool, &cfg, std::slice::from_ref(&article)).await,
        1
    );
    let lines_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&article)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(lines_after, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_name_alone_never_joins(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    // The same name, another link: no join, however similar the page.
    let page = tagged(&pool, "https://example.org/douglas-adams").await;
    let other_article = tagged(&pool, "https://de.wikipedia.org/wiki/Douglas_Adams").await;
    let other_mb = tagged(
        &pool,
        "https://musicbrainz.org/artist/00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(
        join::referenced(
            &pool,
            &cfg,
            &[page.clone(), other_article.clone(), other_mb.clone()]
        )
        .await,
        0
    );
    for id in [&page, &other_article, &other_mb] {
        let row = backed(&pool, id).await;
        assert_eq!(row.catalogue_card, None);
        assert_eq!(row.status, "pending");
        assert!(row.due);
    }
    // Two cards of one catalogue holding the same key: neither is taken.
    let mut twin = adams();
    twin.source_id = "Q4200".into();
    twin.uri = "https://en.wikipedia.org/wiki/Someone_else".into();
    twin.keys = vec![MB_KEY.into()];
    twin.related_uris = vec![];
    import(&pool, Catalogue::Wikidata, &[adams(), twin]).await;
    let mb = tagged(&pool, MB).await;
    assert_eq!(
        join::join(&pool, &cfg, &mb, None).await.unwrap(),
        join::Outcome::Nothing
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_catalogue_backed_link_never_enters_the_fetch_queue(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let fetcher = Fetcher::new();
    // A worker claims the new link first: it is built from the card, not fetched.
    let id = tagged(&pool, ARTICLE).await;
    let job = queue::claim_sources(&pool, &["web"], &cfg, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(job.id, id);
    queue::run_job(&pool, &fetcher, &cfg, &job).await.unwrap();
    queue::run_batch(&pool, &fetcher, &cfg, std::slice::from_ref(&job))
        .await
        .unwrap();
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 0);
    assert!(backed(&pool, &id).await.catalogue_card.is_some());
    // Whatever reopens it (a new reference, a requeue, `reextract`), it stays out of the queue.
    sqlx::query("UPDATE resource SET final=false,refresh_at=now(),next_attempt_at=now(),dead_letter=false,priority_at=now() WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    queue::reextract(&pool, &["all".to_owned()]).await.unwrap();
    let row = backed(&pool, &id).await;
    assert!(row.is_final && !row.due);
    assert!(queue::claim_sources(&pool, &["web"], &cfg, 10)
        .await
        .unwrap()
        .is_empty());
    // With the catalogues off, the same link is fetched.
    let off = QueueConfig::default();
    let other = tagged(&pool, SPOTIFY).await;
    let job = queue::claim_sources(&pool, &["web"], &off, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(job.id, other);
    queue::run_job(&pool, &fetcher, &off, &job).await.unwrap();
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 1);
    let row = backed(&pool, &other).await;
    assert_eq!(row.catalogue_card, None);
    assert_eq!(row.name.as_deref(), Some("Fetched page"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_changed_card_rebuilds_its_links(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let id = tagged(&pool, ARTICLE).await;
    join::referenced(&pool, &cfg, std::slice::from_ref(&id)).await;
    // The same card read by the next import (its stamp) marks nothing.
    import_at(&pool, Catalogue::Wikidata, 2, &[adams()]).await;
    let stamped: i64 = sqlx::query_scalar("SELECT seen_generation FROM catalogue_card")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stamped, 2);
    // An also-on source's key of the link, and no also-on run queued by what follows.
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,'isrc:GBUM71029604','musicbrainz','musicbrainz',now()+interval '1 day')")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM also_on_dirty")
        .execute(&pool)
        .await
        .unwrap();
    let due: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogue_join_due")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(due, 0);
    // Only the labels change: the card is built again, and its also-on sources are not asked
    // again (labels and the card's version are no change to what they read).
    let mut relabelled = adams();
    relabelled.labels = vec!["author".into()];
    import_at(&pool, Catalogue::Wikidata, 3, &[relabelled]).await;
    drain(&pool, &cfg).await;
    assert_eq!(backed(&pool, &id).await.labels, Some(json!(["author"])));
    let dirty: i64 = sqlx::query_scalar("SELECT count(*) FROM also_on_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dirty, 0);
    let mut changed = adams();
    changed.name = "Douglas Noël Adams".into();
    changed.labels = vec!["writer".into()];
    import_at(&pool, Catalogue::Wikidata, 4, &[changed]).await;
    let due: Vec<String> = sqlx::query_scalar("SELECT resource_id FROM catalogue_join_due")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(due, std::slice::from_ref(&id));
    drain(&pool, &cfg).await;
    // The other source's key stays.
    let other: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM resource_key WHERE resource_id=$1 AND source<>'catalogue'",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(other, ["isrc:GBUM71029604"]);
    let row = backed(&pool, &id).await;
    assert_eq!(row.name.as_deref(), Some("Douglas Noël Adams"));
    assert_eq!(row.labels, Some(json!(["writer"])));
    assert!(row.is_final);
    let classes: Vec<String> =
        sqlx::query_scalar("SELECT class FROM resource_change WHERE resource_id=$1 ORDER BY seq")
            .bind(&id)
            .fetch_all(&pool)
            .await
            .unwrap();
    // Created, relabelled, renamed.
    assert_eq!(classes.len(), 3, "{classes:?}");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogue_join_due")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_removed_card_or_a_catalogue_off_gives_the_link_back_to_fetching(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let article = tagged(&pool, ARTICLE).await;
    let spotify = tagged(&pool, SPOTIFY).await;
    join::referenced(&pool, &cfg, &[article.clone(), spotify.clone()]).await;
    // The catalogue is switched off: the start's sweep gives its links back to the fetch queue at
    // once, due at times spread over the next day.
    let off = QueueConfig::default();
    let released = pinakes_core::telemetry::catalogue_joins("wikidata", "released");
    assert_eq!(join::sweep(&pool, &off.catalogues).await.unwrap(), 2);
    // At least these two (other tests in this binary count too).
    assert!(pinakes_core::telemetry::catalogue_joins("wikidata", "released") >= released + 2);
    let spread: Vec<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at-now())::float8 FROM resource ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        spread.iter().all(|s| (0.0..86400.0).contains(s)),
        "{spread:?}"
    );
    assert!(spread[0] != spread[1]);
    sqlx::query("UPDATE resource SET next_attempt_at=now(),refresh_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    drain(&pool, &off).await;
    for id in [&article, &spotify] {
        let row = backed(&pool, id).await;
        assert_eq!(row.catalogue_card, None);
        assert!(!row.is_final && row.due);
        assert_eq!(row.labels, None);
        // The card stays until the fetch replaces it; the references stay on the link.
        assert_eq!(row.name.as_deref(), Some("Douglas Adams"));
        assert!(catalogue_keys(&pool, id).await.is_empty());
    }
    let references: i64 = sqlx::query_scalar("SELECT count(*) FROM reference")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(references, 2);
    // On again, the link joins at its next fetch; then the card is removed upstream.
    let fetcher = Fetcher::new();
    for job in queue::claim_sources(&pool, &["web"], &cfg, 10)
        .await
        .unwrap()
    {
        queue::run_job(&pool, &fetcher, &cfg, &job).await.unwrap();
    }
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 0);
    assert!(backed(&pool, &article).await.catalogue_card.is_some());
    store::remove(&pool, Catalogue::Wikidata, &["Q42".to_owned()], None)
        .await
        .unwrap();
    drain(&pool, &cfg).await;
    let row = backed(&pool, &article).await;
    assert_eq!(row.catalogue_card, None);
    assert!(row.due);
    assert_eq!(row.labels, None);
    let job = queue::claim_sources(&pool, &["web"], &cfg, 10)
        .await
        .unwrap();
    assert_eq!(job.len(), 2);
    for job in &job {
        queue::run_job(&pool, &fetcher, &cfg, job).await.unwrap();
    }
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 2);
    assert_eq!(
        backed(&pool, &article).await.name.as_deref(),
        Some("Fetched page")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_card_s_own_link_and_its_related_links_share_its_group(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let article = tagged(&pool, ARTICLE).await;
    let spotify = tagged(&pool, SPOTIFY).await;
    let website = tagged(&pool, WEBSITE).await;
    join::referenced(
        &pool,
        &cfg,
        &[article.clone(), spotify.clone(), website.clone()],
    )
    .await;
    // The website is a related link: in the group, not built from the card, still fetched.
    let row = backed(&pool, &website).await;
    assert_eq!(row.catalogue_card, None);
    assert!(row.due);
    assert_eq!(
        catalogue_keys(&pool, &website).await,
        ["catalogue:wikidata:Q42"]
    );
    // Its fetch keeps it related.
    let fetcher = Fetcher::new();
    let jobs = queue::claim_sources(&pool, &["web"], &cfg, 10)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    queue::run_job(&pool, &fetcher, &cfg, &jobs[0])
        .await
        .unwrap();
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 1);
    assert_eq!(
        catalogue_keys(&pool, &website).await,
        ["catalogue:wikidata:Q42"]
    );
    while pinakes_core::groups::drain(&pool, &cfg.groups, 100)
        .await
        .unwrap()
        > 0
    {}
    let groups: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT (SELECT group_id FROM group_unit u WHERE u.resource_id=x AND u.group_id IS NOT NULL LIMIT 1) FROM unnest($1::text[]) x",
    )
    .bind(vec![article.clone(), spotify.clone(), website.clone()])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(groups[0].is_some(), "{groups:?}");
    assert!(groups.iter().all(|g| g == &groups[0]), "{groups:?}");
    // The card's read lists the other members under Also on.
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let card = get(&app, &format!("/v1/resources/{article}")).await.1;
    let also: Vec<&str> = card["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["uri"].as_str())
        .collect();
    assert!(
        also.contains(&SPOTIFY) && also.contains(&WEBSITE),
        "{also:?}"
    );
    assert_eq!(card["labels"], json!(["science-fiction", "humourist"]));
    assert_eq!(card["from"], "wikidata");
}

async fn get(app: &axum::Router, path: &str) -> (u16, Value) {
    send(app, "GET", path, None).await
}
async fn send(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn uri_query(uri: &str) -> String {
    url::form_urlencoded::byte_serialize(uri.as_bytes()).collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_card_nobody_shared_opens_by_its_link_and_its_id(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (id, _, _) = model::identity(ARTICLE).unwrap();
    let (status, by_uri) = get(
        &app,
        &format!("/v1/resources/by-uri?uri={}", uri_query(ARTICLE)),
    )
    .await;
    assert_eq!(status, 200, "{by_uri}");
    assert_eq!(by_uri["id"], id);
    assert_eq!(by_uri["uri"], ARTICLE);
    assert_eq!(by_uri["status"], "ok");
    assert_eq!(by_uri["type"], "Person");
    assert_eq!(by_uri["name"], "Douglas Adams");
    assert_eq!(by_uri["refs"], json!([]));
    assert_eq!(by_uri["collections"], 0);
    assert_eq!(by_uri["group"], Value::Null);
    assert_eq!(by_uri["also_on"], json!([]));
    assert_eq!(by_uri["from"], "wikidata");
    assert_eq!(by_uri["updated_at"], Value::Null);
    assert_eq!(
        by_uri["keys"],
        json!(["catalogue:wikidata:Q42", MB_KEY, "wikidata:Q42"])
    );
    assert_eq!(by_uri["labels"], json!(["science-fiction", "humourist"]));
    // The same card by its ID, as search returns it.
    let (status, by_id) = get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(by_id, by_uri);
    // Nothing was stored.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    // Another link the card holds (a `url:` key) is not its card's link: unknown.
    let (status, _) = get(
        &app,
        &format!("/v1/resources/by-uri?uri={}", uri_query(SPOTIFY)),
    )
    .await;
    assert_eq!(status, 404);
    // A lookup reads it as by-uri does, in its place among the links asked for.
    let known = tagged(&pool, "https://example.org/known").await;
    let (status, looked) = send(
        &app,
        "POST",
        "/v1/resources/lookup",
        Some(
            json!({"uris": ["https://example.org/known", ARTICLE, "https://example.org/unknown"]}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{looked}");
    let looked = looked.as_array().unwrap();
    assert_eq!(looked.len(), 2);
    assert_eq!(looked[0]["id"], known.as_str());
    assert_eq!(looked[1]["id"], id.as_str());
    assert_eq!(looked[1]["name"], "Douglas Adams");
    assert_eq!(looked[1]["labels"], by_uri["labels"]);
    let (_, by_id) = send(
        &app,
        "POST",
        "/v1/resources/lookup",
        Some(json!({"ids": [id.clone()]})),
    )
    .await;
    assert_eq!(by_id[0]["name"], "Douglas Adams");
    // With the catalogue off, nothing answers.
    let mut config = Config::default();
    config.catalogues.wikidata.enabled = false;
    let off = api::router(AppState::new(pool.clone(), config).unwrap());
    assert_eq!(get(&off, &format!("/v1/resources/{id}")).await.0, 404);
    let (_, none) = send(
        &off,
        "POST",
        "/v1/resources/lookup",
        Some(json!({"ids": [id.clone()]})),
    )
    .await;
    assert_eq!(none, json!([]));
    // `POST /v1/resolve` stores it, built from the card at once.
    let (status, resolved) = send(&app, "POST", "/v1/resolve", Some(json!({"uri": ARTICLE}))).await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["name"], "Douglas Adams");
    assert_eq!(resolved["status"], "ok");
    let row = backed(&pool, &id).await;
    assert!(row.catalogue_card.is_some());
    // A shared card reads the same way, with its stored row's time.
    let (_, stored) = get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(stored["name"], by_uri["name"]);
    assert_eq!(stored["labels"], by_uri["labels"]);
    assert_eq!(stored["keys"], by_uri["keys"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn with_every_catalogue_off_a_link_is_fetched_and_answers_carry_no_labels(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let mut config = Config::default();
    config.catalogues.enabled = false;
    let app = api::router(AppState::new(pool.clone(), config.clone()).unwrap());
    assert_eq!(
        get(
            &app,
            &format!("/v1/resources/by-uri?uri={}", uri_query(ARTICLE))
        )
        .await
        .0,
        404
    );
    let off = QueueConfig {
        catalogues: join::joinable(&config.catalogues),
        ..Default::default()
    };
    assert!(off.catalogues.is_empty());
    let id = tagged(&pool, ARTICLE).await;
    assert_eq!(
        join::referenced(&pool, &off, std::slice::from_ref(&id)).await,
        0
    );
    let fetcher = Fetcher::new();
    let job = queue::claim_sources(&pool, &["web"], &off, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    queue::run_job(&pool, &fetcher, &off, &job).await.unwrap();
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 1);
    let (status, card) = get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(status, 200);
    assert!(card.get("labels").is_none(), "{card}");
    assert_eq!(card["from"], Value::Null);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_undated_card_that_answers_the_same_waits_longer(pool: PgPool) {
    let cfg = QueueConfig::default();
    let fetcher = Arc::new(Fetcher::new());
    tagged(&pool, "https://example.org/essay").await;
    let day = 86400.0;
    let mut waits = vec![];
    let read = |answer: Option<ResourceMeta>| {
        let (pool, fetcher, cfg) = (pool.clone(), fetcher.clone(), cfg.clone());
        async move {
            *fetcher.answer.lock().unwrap() = answer;
            sqlx::query("UPDATE resource SET next_attempt_at=now(),refresh_at=now() WHERE id IN (SELECT id FROM resource)")
                .execute(&pool)
                .await
                .unwrap();
            let job = queue::claim_sources(&pool, &["web"], &cfg, 1)
                .await
                .unwrap()
                .pop()
                .unwrap();
            queue::run_job(&pool, fetcher.as_ref(), &cfg, &job)
                .await
                .unwrap();
            let (ttl, wait): (i64, f64) = sqlx::query_as(
                "SELECT (doc->>'ttl_secs')::bigint,extract(epoch FROM next_attempt_at-now())::float8 FROM resource",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            // A confirmed answer's wait is its stored one; an unconfirmed missing answer waits
            // for its confirming read and keeps the stored wait.
            let unconfirmed: bool = sqlx::query_scalar(
                "SELECT last_error IS NOT DISTINCT FROM 'missing_unconfirmed' FROM resource",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(
                unconfirmed || (wait - ttl as f64).abs() < 60.0,
                "{wait} {ttl}"
            );
            (wait / day * 1000.0).round() / 1000.0
        }
    };
    let page = |name: &str| {
        let mut doc = ResourceMeta::new("https://example.org/essay", "web", 0);
        doc.name = Some(name.into());
        Some(doc)
    };
    // The first read, then the same answer again and again: 7, 14, 28, 56, 90, 90 days.
    for _ in 0..6 {
        waits.push(read(page("Essay")).await);
    }
    assert_eq!(waits, [7.0, 14.0, 28.0, 56.0, 90.0, 90.0]);
    // A change starts over at 7 days.
    assert_eq!(read(page("Essay, revised")).await, 7.0);
    assert_eq!(read(page("Essay, revised")).await, 14.0);
    // Gone for good: two fresh reads agree, then the slowest rate.
    let mut gone = ResourceMeta::new("https://example.org/essay", "web", 0);
    gone.status = Status::Missing;
    gone.http_status = Some(410);
    let first = read(Some(gone.clone())).await;
    assert_eq!(first, 0.042, "{first}");
    sqlx::query("UPDATE resource SET last_missing_at=now()-interval '2 hours'")
        .execute(&pool)
        .await
        .unwrap();
    gone.fetched_at = chrono::Utc::now().timestamp_millis();
    let confirmed = read(Some(gone.clone())).await;
    let status: String = sqlx::query_scalar("SELECT status FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((status.as_str(), confirmed), ("missing", 90.0));
    // Back again: a change, 7 days.
    assert_eq!(read(page("Essay, back")).await, 7.0);
    // A dated card keeps its tier (here an event 60 days away: 7 days, every time).
    let mut event = ResourceMeta::new("https://example.org/essay", "web", 0);
    event.set_type("Event");
    event.name = Some("Reading".into());
    event.start = Some(chrono::Utc::now() + chrono::Duration::days(60));
    assert_eq!(read(Some(event.clone())).await, 7.0);
    assert_eq!(read(Some(event)).await, 7.0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn opening_a_card_whose_refresh_is_due_reads_it_first(pool: PgPool) {
    let id = tagged(&pool, "https://example.org/old").await;
    let mut doc = ResourceMeta::new("https://example.org/old", "web", 7 * 86400);
    doc.name = Some("Old".into());
    pinakes_core::store::save_document(&pool, &id, 0, &doc)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let priority = |pool: PgPool| async move {
        sqlx::query_scalar::<_, bool>("SELECT priority_at IS NOT NULL FROM resource")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    // Not due: opening it changes nothing.
    assert_eq!(get(&app, &format!("/v1/resources/{id}")).await.0, 200);
    assert!(!priority(pool.clone()).await);
    // Older than its wait: opening it moves it to the front of the queue.
    sqlx::query("UPDATE resource SET refresh_at=now()-interval '1 hour',next_attempt_at=now()-interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(
            &app,
            "/v1/resources/by-uri?uri=https%3A%2F%2Fexample.org%2Fold"
        )
        .await
        .0,
        200
    );
    assert!(priority(pool.clone()).await);
}

mod common;

/// Wikipedia's summary of the one article, counting the requests.
struct Summaries(AtomicUsize);
#[async_trait::async_trait]
impl pinakes_core::resolver::egress::Transport for Summaries {
    async fn get(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
    ) -> anyhow::Result<pinakes_core::resolver::egress::Fetched> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let path = percent_encoding::percent_decode_str(url.path()).decode_utf8_lossy();
        let (status, body) = match (url.host_str(), path.as_ref()) {
            (Some("en.wikipedia.org"), "/api/rest_v1/page/summary/Douglas_Adams") => (
                200,
                json!({"type": "standard", "title": "Douglas Adams", "titles": {"canonical": "Douglas_Adams"}, "extract": "Douglas Noël Adams was an English author, humourist and screenwriter."})
                    .to_string()
                    .into_bytes(),
            ),
            _ => (404, b"{}".to_vec()),
        };
        Ok(common::fetched(
            url.clone(),
            status,
            "application/json".into(),
            body,
        ))
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_built_card_reads_its_wikipedia_summary_in_the_background(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let article = tagged(&pool, ARTICLE).await;
    let shared = tagged(&pool, SPOTIFY).await;
    join::referenced(&pool, &cfg, &[article.clone(), shared.clone()]).await;
    let transport = Arc::new(Summaries(AtomicUsize::new(0)));
    let egress = pinakes_core::resolver::egress::Egress::with_backends(
        common::unpaced_egress_config(),
        Arc::new(common::PublicDns),
        transport.clone(),
    );
    assert_eq!(
        pinakes_core::catalogue::extras::run_due(&pool, &egress, &cfg.catalogues, 20)
            .await
            .unwrap(),
        2
    );
    assert!(transport.0.load(Ordering::SeqCst) >= 1);
    let read = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Value)>(
                "SELECT doc->>'description',coalesce(doc->'credits','[]') FROM resource WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let summary = "Douglas Noël Adams was an English author, humourist and screenwriter.";
    // The article's own link: its own text, no credit.
    let (text, credits) = read(article.clone()).await;
    assert_eq!(text.as_deref(), Some(summary));
    assert_eq!(credits, json!([]));
    // Another link built from the card: the text credited to Wikipedia.
    let (text, credits) = read(shared.clone()).await;
    assert_eq!(text.as_deref(), Some(summary));
    assert_eq!(credits[0]["provider"], "wikipedia");
    assert_eq!(credits[0]["source_url"], ARTICLE);
    // Nothing more is due; an open within 30 days reads nothing again.
    assert_eq!(
        pinakes_core::catalogue::extras::run_due(&pool, &egress, &cfg.catalogues, 20)
            .await
            .unwrap(),
        0
    );
    assert!(!pinakes_core::catalogue::extras::opened(&pool, &shared)
        .await
        .unwrap());
    // Due again while Wikidata is off (before the start's sweep gives the links back): no read
    // asks Wikipedia, and with no catalogue on not even the database is read.
    sqlx::query("UPDATE catalogue_extras SET due_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let asked = transport.0.load(Ordering::SeqCst);
    for on in [&["musicbrainz"][..], &[]] {
        assert_eq!(
            pinakes_core::catalogue::extras::run_due(&pool, &egress, on, 20)
                .await
                .unwrap(),
            0,
            "{on:?}"
        );
    }
    assert_eq!(transport.0.load(Ordering::SeqCst), asked);
    sqlx::query("UPDATE catalogue_extras SET due_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    // A card built again keeps the summary, and asks Wikipedia nothing.
    let mut changed = adams();
    changed.labels = vec!["writer".into()];
    import_at(&pool, Catalogue::Wikidata, 2, &[changed]).await;
    drain(&pool, &cfg).await;
    let (text, credits) = read(shared.clone()).await;
    assert_eq!(text.as_deref(), Some(summary));
    assert_eq!(credits[0]["provider"], "wikipedia");
    let due: i64 =
        sqlx::query_scalar("SELECT count(*) FROM catalogue_extras WHERE due_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(due, 0);
    // Opened more than 30 days after the last read, it is read again.
    sqlx::query("UPDATE catalogue_extras SET read_at=now()-interval '31 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(pinakes_core::catalogue::extras::opened(&pool, &shared)
        .await
        .unwrap());
    // A card nobody shared asks no other service.
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let mut other = adams();
    other.source_id = "Q5".into();
    other.uri = "https://en.wikipedia.org/wiki/Human".into();
    other.keys = vec![];
    other.related_uris = vec![];
    import(&pool, Catalogue::Wikidata, &[adams(), other]).await;
    let (status, _) = get(
        &app,
        &format!(
            "/v1/resources/by-uri?uri={}",
            uri_query("https://en.wikipedia.org/wiki/Human")
        ),
    )
    .await;
    assert_eq!(status, 200);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogue_extras")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2);
}

/// A Tranco card for a homepage.
fn homepage_card(host: &str) -> Card {
    Card {
        source_id: host.into(),
        uri: format!("https://{host}/"),
        r#type: "WebSite".into(),
        name: host.into(),
        keys: vec![format!("homepage:{host}")],
        ..Default::default()
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_official_website_a_tranco_card_holds_still_joins_its_item_s_group(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    import(
        &pool,
        Catalogue::Tranco,
        &[homepage_card("douglasadams.com")],
    )
    .await;
    let cfg = queue_cfg();
    let article = tagged(&pool, ARTICLE).await;
    // The `www.` spelling: Tranco's card holds it by its homepage key.
    let website = tagged(&pool, "https://www.douglasadams.com/").await;
    let site = tagged(&pool, WEBSITE).await;
    join::referenced(
        &pool,
        &cfg,
        &[article.clone(), website.clone(), site.clone()],
    )
    .await;
    for id in [&website, &site] {
        assert!(backed(&pool, id).await.catalogue_card.is_some());
        let keys = catalogue_keys(&pool, id).await;
        assert!(
            keys.contains(&"catalogue:tranco:douglasadams.com".to_owned()),
            "{keys:?}"
        );
    }
    // The exact related link also carries Q42's key: it shares the article's group.
    assert!(catalogue_keys(&pool, &site)
        .await
        .contains(&"catalogue:wikidata:Q42".to_owned()));
    while pinakes_core::groups::drain(&pool, &cfg.groups, 100)
        .await
        .unwrap()
        > 0
    {}
    let group = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>("SELECT (SELECT group_id FROM group_unit WHERE resource_id=$1 AND group_id IS NOT NULL LIMIT 1)")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert!(group(article.clone()).await.is_some());
    assert_eq!(group(article).await, group(site).await);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_link_related_to_two_items_of_one_catalogue_joins_neither(pool: PgPool) {
    let mut founder = adams();
    founder.source_id = "Q99".into();
    founder.uri = "https://en.wikipedia.org/wiki/Founder".into();
    founder.keys = vec!["wikidata:Q99".into()];
    import(&pool, Catalogue::Wikidata, &[adams(), founder]).await;
    let site = tagged(&pool, WEBSITE).await;
    assert_eq!(
        join::join(&pool, &queue_cfg(), &site, None).await.unwrap(),
        join::Outcome::Nothing
    );
    assert!(catalogue_keys(&pool, &site).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_link_fetched_before_its_card_was_imported_joins_when_the_card_is_written(pool: PgPool) {
    let cfg = queue_cfg();
    let fetcher = Fetcher::new();
    // A docs.rs page is shared and fetched while no catalogue holds it; also a page with the same
    // name and type as the card to come.
    let docs = tagged(&pool, "https://docs.rs/serde").await;
    let lookalike = tagged(&pool, "https://example.org/serde").await;
    *fetcher.answer.lock().unwrap() = Some({
        let mut doc = ResourceMeta::new("https://example.org/serde", "web", 0);
        doc.set_type("SoftwareApplication");
        doc.name = Some("serde".into());
        doc
    });
    for job in queue::claim_sources(&pool, &["web"], &cfg, 10)
        .await
        .unwrap()
    {
        queue::run_job(&pool, &fetcher, &cfg, &job).await.unwrap();
    }
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 2);
    let serde = Card {
        source: "crates.io".into(),
        source_id: "crates.io/serde".into(),
        uri: "https://crates.io/crates/serde".into(),
        r#type: "SoftwareApplication".into(),
        name: "serde".into(),
        keys: vec!["crate:serde".into()],
        ..Default::default()
    };
    import(&pool, Catalogue::Software, &[serde]).await;
    let due: Vec<String> = sqlx::query_scalar("SELECT resource_id FROM catalogue_join_due")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(due, std::slice::from_ref(&docs));
    drain(&pool, &cfg).await;
    assert!(backed(&pool, &docs).await.catalogue_card.is_some());
    // The page with the same name joins nothing, now or at its next fetch.
    assert_eq!(
        join::join(&pool, &cfg, &lookalike, None).await.unwrap(),
        join::Outcome::Nothing
    );
    assert!(backed(&pool, &lookalike).await.catalogue_card.is_none());
}

/// Every link `catalogue_key_links` names for a key reads that key back (`link_keys`).
#[sqlx::test(migrations = "../../migrations")]
async fn the_links_a_key_names_name_the_key_back(pool: PgPool) {
    let keys = [
        "mb:artist:e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4",
        "mb:release-group:e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4",
        "mb:recording:e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4",
        "mb:release:e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4",
        "isrc:GBUM71029604",
        "wikidata:Q42",
        "doi:10.1000/abc",
        "arxiv:2302.04668",
        "openalex:W2741809807",
        "crate:serde",
        "android:org.fdroid.fdroid",
        "flathub:org.gnome.Maps",
        "homepage:example.org",
    ];
    for key in keys {
        let links: Vec<String> =
            sqlx::query_scalar("SELECT coalesce(catalogue_key_links($1),'{}')")
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!links.is_empty(), "{key}");
        for link in links {
            let (_, normal, _) = model::identity(&link).unwrap();
            assert_eq!(normal, link, "{key}: not normalized");
            let read: Vec<String> = join::link_keys(&link).into_iter().map(|(k, _)| k).collect();
            assert!(
                read.contains(&key.to_owned()),
                "{key}: {link} reads {read:?}"
            );
        }
    }
    for key in [
        "upc:0602537347865",
        "url:00000000000000000000000000000000",
        "mb:work:x",
    ] {
        let links: Option<Vec<String>> = sqlx::query_scalar("SELECT catalogue_key_links($1)")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(links, None, "{key}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_referenced_line_joins_when_its_batch_is_ingested(pool: PgPool) {
    use axum::{extract::Query, routing::get, Json, Router};
    use pinakes_core::nexus::{Change, Changes, Kind, Nexus, Op};
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let line = |op: Op, uri: &str, path: &str| Change {
        op,
        uri: uri.into(),
        kind: Kind::Tag,
        author: common::author(0),
        object_path: path.into(),
        label: Some("writer".into()),
        at: chrono::Utc::now().timestamp_millis(),
    };
    // A tag on an alias spelling of the article, and a removed tag of another link.
    let lines = vec![
        line(
            Op::Put,
            "https://en.m.wikipedia.org/wiki/Douglas_Adams",
            "/pub/app/tags/one",
        ),
        line(Op::Del, "https://example.org/gone", "/pub/app/tags/two"),
    ];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let feed = lines.clone();
    let app = Router::new().route(
        "/v1/resources/changes",
        get(
            move |Query(q): Query<std::collections::HashMap<String, usize>>| {
                let feed = feed.clone();
                async move {
                    let cursor = q["cursor"];
                    let page: Vec<Change> = feed.iter().skip(cursor).cloned().collect();
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert("x-events-epoch", "fixture-epoch".parse().unwrap());
                    headers.insert("x-events-length", feed.len().to_string().parse().unwrap());
                    (
                        headers,
                        Json(Changes {
                            next_cursor: (cursor + page.len()) as i64,
                            lines: page,
                            skipped: Default::default(),
                        }),
                    )
                }
            },
        ),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://{addr}");
    let nexus = Nexus::new(&url, None).unwrap();
    let (poll, referenced) = pinakes_core::ingest::poll_referenced(
        &pool,
        &url,
        &nexus,
        &Default::default(),
        &pinakes_core::telemetry::Metrics::new().unwrap(),
        &mut pinakes_core::ingest::Resets::new(Duration::ZERO),
    )
    .await
    .unwrap();
    task.abort();
    assert_eq!(poll.lines, 2);
    // The Put line's link only.
    let (alias, _, _) = model::identity("https://en.m.wikipedia.org/wiki/Douglas_Adams").unwrap();
    assert_eq!(referenced, [alias]);
    // Joined at once, at the alias's root, with no worker run.
    assert_eq!(join::referenced(&pool, &queue_cfg(), &referenced).await, 1);
    let (root, _, _) = model::identity(ARTICLE).unwrap();
    let row = backed(&pool, &root).await;
    assert!(row.catalogue_card.is_some());
    assert_eq!(row.status, "ok");
}

/// A resolver whose read of an official card is the card asked again, with the stored answer.
struct Official(Mutex<ResourceMeta>);
#[async_trait::async_trait]
impl Resolver for Official {
    fn name(&self) -> &'static str {
        "official"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
        let mut doc = self.0.lock().unwrap().clone();
        doc.fetched_at = chrono::Utc::now().timestamp_millis();
        Ok(doc)
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        Duration::from_secs(doc.ttl_secs)
    }
}
async fn wait_days(pool: &PgPool) -> f64 {
    let wait: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at-now())::float8 FROM resource",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    (wait / 86400.0 * 100.0).round() / 100.0
}
async fn due_now(pool: &PgPool) {
    sqlx::query("UPDATE resource SET next_attempt_at=now(),refresh_at=now(),lease_until=NULL")
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_backoff_covers_a_304_and_official_cards_and_spares_shows_and_dates(pool: PgPool) {
    let cfg = QueueConfig::default();
    tagged(&pool, "https://example.org/a").await;
    let read = |resolver: &'static dyn Resolver| {
        let (pool, cfg) = (pool.clone(), cfg.clone());
        async move {
            due_now(&pool).await;
            let job = queue::claim_sources(&pool, &[resolver.name()], &cfg, 1)
                .await
                .unwrap()
                .pop()
                .unwrap();
            queue::run_job(&pool, resolver, &cfg, &job).await.unwrap();
            wait_days(&pool).await
        }
    };
    // A 304 on a stored card doubles its wait.
    let fetcher: &'static Fetcher = Box::leak(Box::new(Fetcher::new()));
    assert_eq!(read(fetcher).await, 7.0);
    struct NotModified;
    #[async_trait::async_trait]
    impl Resolver for NotModified {
        fn name(&self) -> &'static str {
            "web"
        }
        fn matches(&self, _: &str) -> bool {
            true
        }
        async fn resolve(&self, _: &str) -> anyhow::Result<ResourceMeta> {
            Err(pinakes_core::resolver::NotModified.into())
        }
        fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
            Duration::from_secs(doc.ttl_secs)
        }
    }
    assert_eq!(read(&NotModified).await, 14.0);
    assert_eq!(read(&NotModified).await, 28.0);
    // A podcast show keeps its weekly read, its new episodes being no change to its card.
    let mut show = ResourceMeta::new("https://example.org/a", "web", 0);
    show.set_type("PodcastSeries");
    show.name = Some("A show".into());
    *fetcher.answer.lock().unwrap() = Some(show);
    assert_eq!(read(fetcher).await, 7.0);
    assert_eq!(read(fetcher).await, 7.0);
    // A card that names when it changes keeps that time.
    let mut auction = ResourceMeta::new("https://example.org/a", "web", 0);
    auction.name = Some("An auction".into());
    auction.refresh_by = Some(chrono::Utc::now() + chrono::Duration::hours(2));
    *fetcher.answer.lock().unwrap() = Some(auction);
    assert!(read(fetcher).await <= 0.09);
    assert!(read(fetcher).await <= 0.09);
    // A 451 (a legal block, not gone for good) is checked again as missing links are.
    let mut blocked = ResourceMeta::new("https://example.org/a", "web", 0);
    blocked.status = Status::Missing;
    blocked.http_status = Some(451);
    *fetcher.answer.lock().unwrap() = Some(blocked);
    read(fetcher).await;
    sqlx::query("UPDATE resource SET last_missing_at=now()-interval '2 hours'")
        .execute(&pool)
        .await
        .unwrap();
    let confirmed = read(fetcher).await;
    let status: String = sqlx::query_scalar("SELECT status FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "missing");
    assert!(confirmed <= 0.05, "{confirmed}");
    // An official card doubles from its own lifetime.
    sqlx::query("DELETE FROM resource")
        .execute(&pool)
        .await
        .unwrap();
    tagged(&pool, "https://example.org/b").await;
    sqlx::query("UPDATE resource SET resolver='official'")
        .execute(&pool)
        .await
        .unwrap();
    let mut card = ResourceMeta::new("https://example.org/b", "official", 86400);
    card.name = Some("A repository".into());
    let official: &'static Official = Box::leak(Box::new(Official(Mutex::new(card))));
    assert_eq!(read(official).await, 1.0);
    assert_eq!(read(official).await, 2.0);
    assert_eq!(read(official).await, 4.0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn opening_a_card_the_backoff_put_off_leaves_its_wait(pool: PgPool) {
    let id = tagged(&pool, "https://example.org/slow").await;
    let mut doc = ResourceMeta::new("https://example.org/slow", "web", 56 * 86400);
    doc.name = Some("Slow".into());
    pinakes_core::store::save_document(&pool, &id, 0, &doc)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let due = |pool: PgPool| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT next_attempt_at<=now() AND priority_at IS NOT NULL FROM resource",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    // Read 3 days ago, its next read 53 days away: within the base week, opening waits.
    sqlx::query("UPDATE resource SET resolved_at=now()-interval '3 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&app, &format!("/v1/resources/{id}")).await.0, 200);
    assert!(!due(pool.clone()).await);
    // Read 10 days ago, but not due: opening it by its ID leaves its wait (only a due card moves).
    sqlx::query("UPDATE resource SET resolved_at=now()-interval '10 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&app, &format!("/v1/resources/{id}")).await.0, 200);
    assert!(!due(pool.clone()).await);
    // A catalogue-backed card is never moved by an open.
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let article = tagged(&pool, ARTICLE).await;
    join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&article)).await;
    sqlx::query("UPDATE resource SET resolved_at=now()-interval '100 days' WHERE id=$1")
        .bind(&article)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&app, &format!("/v1/resources/{article}")).await.0, 200);
    let row: (bool, bool) =
        sqlx::query_as("SELECT final,priority_at IS NULL FROM resource WHERE id=$1")
            .bind(&article)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, (true, true));
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_unshared_card_answers_with_the_group_of_the_links_built_from_it(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let spotify = tagged(&pool, SPOTIFY).await;
    let site = tagged(&pool, WEBSITE).await;
    join::referenced(&pool, &cfg, &[spotify.clone(), site.clone()]).await;
    while pinakes_core::groups::drain(&pool, &cfg.groups, 100)
        .await
        .unwrap()
        > 0
    {}
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, card) = get(
        &app,
        &format!("/v1/resources/by-uri?uri={}", uri_query(ARTICLE)),
    )
    .await;
    assert_eq!(status, 200, "{card}");
    let groups: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT resource_id,group_id FROM group_unit ORDER BY resource_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(card["group"]["id"].is_string(), "{card} {groups:?}");
    let also: Vec<&str> = card["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["uri"].as_str())
        .collect();
    assert!(
        also.contains(&SPOTIFY) && also.contains(&WEBSITE),
        "{also:?}"
    );
}

/// The answers a fetched card and a few routes give, with their ETags.
async fn answers(app: &axum::Router, id: &str) -> Vec<(u16, Value, Option<String>)> {
    let mut out = vec![];
    for path in [
        format!("/v1/resources/{id}"),
        format!(
            "/v1/resources/by-uri?uri={}",
            uri_query("https://example.org/plain")
        ),
        "/v1/resources/by-uri?uri=https%3A%2F%2Fexample.org%2Funknown".to_owned(),
        "/v1/search?q=Plain".to_owned(),
    ] {
        let mut request = Request::builder().uri(&path).body(Body::empty()).unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
        ));
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        // A search page's ETag hashes in the state's own cursor key: only the reads' are compared.
        let etag = response
            .headers()
            .get("etag")
            .map(|v| v.to_str().unwrap().to_owned())
            .filter(|_| !path.starts_with("/v1/search"));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        out.push((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            etag,
        ));
    }
    out
}

#[sqlx::test(migrations = "../../migrations")]
async fn with_the_catalogues_on_but_empty_every_answer_is_as_with_them_off(pool: PgPool) {
    let on = queue_cfg();
    assert!(!on.catalogues.is_empty());
    let fetcher = Fetcher::new();
    *fetcher.answer.lock().unwrap() = Some({
        let mut doc = ResourceMeta::new("https://example.org/plain", "web", 0);
        doc.name = Some("Plain page".into());
        doc
    });
    let id = tagged(&pool, "https://example.org/plain").await;
    assert_eq!(
        join::referenced(&pool, &on, std::slice::from_ref(&id)).await,
        0
    );
    let job = queue::claim_sources(&pool, &["web"], &on, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    queue::run_job(&pool, &fetcher, &on, &job).await.unwrap();
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 1);
    // An also-on key of the link stays through a fetch with the catalogues on.
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,'wikidata:Q1','wikidata_item','wikidata_item',now()+interval '1 day')")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    due_now(&pool).await;
    let job = queue::claim_sources(&pool, &["web"], &on, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    queue::run_job(&pool, &fetcher, &on, &job).await.unwrap();
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM resource_key WHERE resource_id=$1")
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(keys, ["wikidata:Q1"]);
    sqlx::query("UPDATE resource SET refresh_at=now()+interval '7 days',next_attempt_at=now()+interval '7 days'")
        .execute(&pool)
        .await
        .unwrap();
    let mut off = Config::default();
    off.catalogues.enabled = false;
    let with_on = answers(
        &api::router(AppState::new(pool.clone(), Config::default()).unwrap()),
        &id,
    )
    .await;
    let with_off = answers(&api::router(AppState::new(pool.clone(), off).unwrap()), &id).await;
    assert_eq!(with_on, with_off);
    assert_eq!(with_on[0].0, 200);
    assert_eq!(with_on[2].0, 404);
    assert!(with_on[0].1.get("labels").is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_join_leaves_a_worker_s_lease_and_skips_what_it_may_not_hold(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    // A worker has the link: a join from a reference fences its fetch but leaves its lease.
    let id = tagged(&pool, ARTICLE).await;
    let job = queue::claim_sources(&pool, &["web"], &cfg, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        join::referenced(&pool, &cfg, std::slice::from_ref(&id)).await,
        1
    );
    let fetcher = Fetcher::new();
    // The worker's own join finds the row taken: it neither fetches nor saves.
    queue::run_job(&pool, &fetcher, &cfg, &job).await.unwrap();
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 0);
    assert_eq!(
        backed(&pool, &id).await.name.as_deref(),
        Some("Douglas Adams")
    );
    // A card naming a link that carries a credential, or a Pubky object, is refused.
    let mut secret = adams();
    secret.source_id = "Q7".into();
    secret.uri = "https://example.org/feed.xml?token=abc".into();
    let mut pubky = adams();
    pubky.source_id = "Q8".into();
    pubky.uri = "pubky://8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo/pub/pubky.app/posts/0033SSE3B1FQ0".into();
    let report = store::upsert(&pool, Catalogue::Wikidata, 1, &[secret, pubky], None)
        .await
        .unwrap();
    assert_eq!(report.rejected, 2);
    // An OSM card backs nothing, and an OSM link keeps its route.
    let osm = Card {
        source_id: "node/240109189".into(),
        uri: "https://www.openstreetmap.org/node/240109189".into(),
        r#type: "Place".into(),
        name: "A place".into(),
        keys: vec!["osm:node/240109189".into()],
        ..Default::default()
    };
    import(&pool, Catalogue::Osm, &[osm]).await;
    let place = tagged(&pool, "https://www.openstreetmap.org/node/240109189").await;
    assert_eq!(
        join::join(&pool, &cfg, &place, None).await.unwrap(),
        join::Outcome::Nothing
    );
    let resolver: Option<String> = sqlx::query_scalar("SELECT resolver FROM resource WHERE id=$1")
        .bind(&place)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(resolver.as_deref(), Some("osm"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_card_that_drops_a_link_gives_it_back_and_a_failed_check_waits(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let article = tagged(&pool, ARTICLE).await;
    let spotify = tagged(&pool, SPOTIFY).await;
    join::referenced(&pool, &cfg, &[article.clone(), spotify.clone()]).await;
    let mut without = adams();
    without.keys = vec!["wikidata:Q42".into()];
    import(&pool, Catalogue::Wikidata, &[without]).await;
    drain(&pool, &cfg).await;
    assert!(backed(&pool, &article).await.catalogue_card.is_some());
    let row = backed(&pool, &spotify).await;
    assert_eq!(row.catalogue_card, None);
    assert!(row.due);
    // A check that fails (here: a catalogue the database refuses) waits and counts.
    sqlx::query("INSERT INTO catalogue_join_due(resource_id) VALUES($1)")
        .bind(&article)
        .execute(&pool)
        .await
        .unwrap();
    let broken = QueueConfig {
        catalogues: vec!["wikidata", "no such catalogue'"],
        ..cfg.clone()
    };
    sqlx::query("ALTER TABLE catalogue_card RENAME COLUMN keys TO keys_gone")
        .execute(&pool)
        .await
        .unwrap();
    let errors = || pinakes_core::telemetry::catalogue_join_errors("drain");
    let before = errors();
    assert_eq!(join::drain(&pool, &broken, 10).await.unwrap(), 1);
    assert!(errors() > before);
    let (attempts, later): (i32, bool) =
        sqlx::query_as("SELECT attempts,due_at>now() FROM catalogue_join_due WHERE resource_id=$1")
            .bind(&article)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((attempts, later), (1, true));
}

#[sqlx::test(migrations = "../../migrations")]
async fn links_stored_before_an_import_are_marked_when_their_cards_come_and_go(pool: PgPool) {
    let cfg = queue_cfg();
    let article = tagged(&pool, ARTICLE).await;
    let spotify = tagged(&pool, SPOTIFY).await;
    let site = tagged(&pool, WEBSITE).await;
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let mut due: Vec<String> = sqlx::query_scalar("SELECT resource_id FROM catalogue_join_due")
        .fetch_all(&pool)
        .await
        .unwrap();
    due.sort();
    let mut expected = vec![article.clone(), spotify.clone(), site.clone()];
    expected.sort();
    assert_eq!(due, expected);
    drain(&pool, &cfg).await;
    assert!(backed(&pool, &article).await.catalogue_card.is_some());
    assert_eq!(
        catalogue_keys(&pool, &site).await,
        ["catalogue:wikidata:Q42"]
    );
    // The card goes: its related link is marked too and loses the card's key.
    store::remove(&pool, Catalogue::Wikidata, &["Q42".to_owned()], None)
        .await
        .unwrap();
    let marked: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM catalogue_join_due WHERE resource_id=$1)")
            .bind(&site)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(marked);
    drain(&pool, &cfg).await;
    assert!(catalogue_keys(&pool, &site).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_start_with_the_catalogues_on_keeps_every_key(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let article = tagged(&pool, ARTICLE).await;
    let site = tagged(&pool, WEBSITE).await;
    join::referenced(&pool, &cfg, &[article.clone(), site.clone()]).await;
    let before = (
        catalogue_keys(&pool, &article).await,
        catalogue_keys(&pool, &site).await,
    );
    assert_eq!(join::sweep(&pool, &cfg.catalogues).await.unwrap(), 0);
    assert_eq!(
        (
            catalogue_keys(&pool, &article).await,
            catalogue_keys(&pool, &site).await
        ),
        before
    );
    assert!(backed(&pool, &article).await.catalogue_card.is_some());
    // The software catalogue switched off: exactly its one link is given back.
    let serde = Card {
        source: "crates.io".into(),
        source_id: "crates.io/serde".into(),
        uri: "https://crates.io/crates/serde".into(),
        r#type: "SoftwareApplication".into(),
        name: "serde".into(),
        keys: vec!["crate:serde".into()],
        ..Default::default()
    };
    import(&pool, Catalogue::Software, &[serde]).await;
    let crate_page = tagged(&pool, "https://crates.io/crates/serde").await;
    join::referenced(&pool, &cfg, std::slice::from_ref(&crate_page)).await;
    let released = pinakes_core::telemetry::catalogue_joins("software", "released");
    let without: Vec<&'static str> = cfg
        .catalogues
        .iter()
        .copied()
        .filter(|c| *c != "software")
        .collect();
    assert_eq!(join::sweep(&pool, &without).await.unwrap(), 1);
    assert_eq!(
        pinakes_core::telemetry::catalogue_joins("software", "released"),
        released + 1
    );
    assert!(backed(&pool, &crate_page).await.catalogue_card.is_none());
    assert_eq!(catalogue_keys(&pool, &article).await, before.0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_podcast_feed_is_fetched_and_joins_its_card_s_group(pool: PgPool) {
    let feed = "https://feeds.example.org/show.xml";
    let card = Card {
        source_id: "920666".into(),
        uri: feed.into(),
        r#type: "PodcastSeries".into(),
        name: "A show".into(),
        ..Default::default()
    };
    import(&pool, Catalogue::Podcastindex, &[card]).await;
    let id = tagged(&pool, feed).await;
    assert_eq!(
        join::join(&pool, &queue_cfg(), &id, None).await.unwrap(),
        join::Outcome::Related
    );
    let row = backed(&pool, &id).await;
    assert_eq!(row.catalogue_card, None);
    assert!(row.due);
    assert_eq!(
        catalogue_keys(&pool, &id).await,
        ["catalogue:podcastindex:920666"]
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_worker_whose_claim_another_join_raced_fetches_nothing(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let id = tagged(&pool, ARTICLE).await;
    let job = queue::claim_sources(&pool, &["web"], &cfg, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    // A reference builds the card under the worker's claim, then the card changes: the worker's
    // join would build it again with its old claim, which the build fenced.
    join::referenced(&pool, &cfg, std::slice::from_ref(&id)).await;
    let mut changed = adams();
    changed.name = "Douglas N. Adams".into();
    import_at(&pool, Catalogue::Wikidata, 2, &[changed]).await;
    let fetcher = Fetcher::new();
    queue::run_job(&pool, &fetcher, &cfg, &job).await.unwrap();
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 0);
    drain(&pool, &cfg).await;
    assert_eq!(
        backed(&pool, &id).await.name.as_deref(),
        Some("Douglas N. Adams")
    );
    // A link that carries a credential joins nothing, whatever key it names.
    let paper = Card {
        source_id: "W2741809807".into(),
        uri: "https://doi.org/10.1000/abc".into(),
        r#type: "ScholarlyArticle".into(),
        name: "A paper".into(),
        keys: vec!["arxiv:2302.04668".into()],
        ..Default::default()
    };
    import(&pool, Catalogue::Openalex, &[paper]).await;
    let secret = tagged(&pool, "https://arxiv.org/abs/2302.04668?token=abc").await;
    assert_eq!(
        join::join(&pool, &cfg, &secret, None).await.unwrap(),
        join::Outcome::Nothing
    );
    assert!(catalogue_keys(&pool, &secret).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn reads_write_nothing_unless_a_refresh_is_moved(pool: PgPool) {
    let id = tagged(&pool, "https://example.org/read").await;
    let mut doc = ResourceMeta::new("https://example.org/read", "web", 56 * 86400);
    doc.name = Some("Read".into());
    pinakes_core::store::save_document(&pool, &id, 0, &doc)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let revision = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT revision FROM search_revision")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let before = revision().await;
    for _ in 0..3 {
        assert_eq!(get(&app, &format!("/v1/resources/{id}")).await.0, 200);
    }
    assert_eq!(revision().await, before);
    // A join that changes nothing writes nothing either.
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let article = tagged(&pool, ARTICLE).await;
    join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&article)).await;
    let before = revision().await;
    join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&article)).await;
    assert_eq!(
        join::join(&pool, &queue_cfg(), &id, None).await.unwrap(),
        join::Outcome::Nothing
    );
    assert_eq!(revision().await, before);
    // A card that waits for the read confirming a missing answer keeps that wait when opened.
    sqlx::query("UPDATE resource SET resolved_at=now()-interval '10 days',last_error='missing_unconfirmed',refresh_at=now()+interval '1 hour',next_attempt_at=now()+interval '1 hour' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&app, &format!("/v1/resources/{id}")).await.0, 200);
    let wait: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(wait > 3000.0, "{wait}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_join_queues_show_on_the_metrics(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    tagged(&pool, ARTICLE).await;
    tagged(&pool, SPOTIFY).await;
    sqlx::query("UPDATE catalogue_join_due SET due_at=now()-interval '10 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    // The two links were stored after the import: marked by hand, as a card's write would.
    sqlx::query("INSERT INTO catalogue_join_due(resource_id,due_at) SELECT id,now()-interval '10 minutes' FROM resource ON CONFLICT DO NOTHING")
        .execute(&pool)
        .await
        .unwrap();
    let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    assert_eq!(
        metrics
            .catalogue_backlog
            .with_label_values(&["joins"])
            .get(),
        2
    );
    assert!(
        metrics
            .catalogue_backlog_age
            .with_label_values(&["joins"])
            .get()
            >= 590
    );
    assert_eq!(
        metrics
            .catalogue_backlog
            .with_label_values(&["extras"])
            .get(),
        0
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_osm_host_page_and_a_social_post_are_never_built_from_a_card(pool: PgPool) {
    let cfg = queue_cfg();
    import(
        &pool,
        Catalogue::Tranco,
        &[homepage_card("openstreetmap.org")],
    )
    .await;
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    // OpenStreetMap's homepage, a web page on an OSM host: no OSM-host page holds card values.
    let osm = tagged(&pool, "https://www.openstreetmap.org/").await;
    join::join(&pool, &cfg, &osm, None).await.unwrap();
    let row = backed(&pool, &osm).await;
    assert_eq!(row.catalogue_card, None);
    assert_eq!(row.name, None);
    // A post at the item's related link keeps its own card and joins no group.
    let post = tagged(&pool, WEBSITE).await;
    let mut doc = ResourceMeta::new(WEBSITE, "activitypub", 3600);
    doc.set_type("SocialMediaPosting");
    doc.name = Some("A post".into());
    pinakes_core::store::save_document(&pool, &post, 0, &doc)
        .await
        .unwrap();
    assert_eq!(
        join::join(&pool, &cfg, &post, None).await.unwrap(),
        join::Outcome::Nothing
    );
    assert!(catalogue_keys(&pool, &post).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_taken_over_key_is_given_back_to_its_source(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    let article = tagged(&pool, ARTICLE).await;
    // An also-on source vouched for the item's key before the card came.
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,'wikidata:Q42','wikidata_item','wikidata_item',now()+interval '1 day')")
        .bind(&article)
        .execute(&pool)
        .await
        .unwrap();
    join::referenced(&pool, &cfg, std::slice::from_ref(&article)).await;
    assert!(catalogue_keys(&pool, &article)
        .await
        .contains(&"wikidata:Q42".to_owned()));
    // The card goes: its keys go, and the also-on sources run again to record theirs.
    sqlx::query("DELETE FROM also_on_dirty")
        .execute(&pool)
        .await
        .unwrap();
    store::remove(&pool, Catalogue::Wikidata, &["Q42".to_owned()], None)
        .await
        .unwrap();
    drain(&pool, &cfg).await;
    assert!(catalogue_keys(&pool, &article).await.is_empty());
    let queued: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM also_on_dirty WHERE resource_id=$1)")
            .bind(&article)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(queued);
    // An OSM catalogue that ran leaves the start's sweep nothing to do.
    let osm = Card {
        source_id: "node/1".into(),
        uri: "https://www.openstreetmap.org/node/1".into(),
        r#type: "Place".into(),
        name: "A place".into(),
        ..Default::default()
    };
    import(&pool, Catalogue::Osm, &[osm]).await;
    assert_eq!(join::sweep(&pool, &cfg.catalogues).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn events_calendars_and_pubky_objects_keep_their_schedule_and_never_join(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let cfg = queue_cfg();
    // The card's own link on an event route: never built from the card.
    let article = tagged(&pool, ARTICLE).await;
    sqlx::query("UPDATE resource SET resolver='event' WHERE id=$1")
        .bind(&article)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        join::join(&pool, &cfg, &article, None).await.unwrap(),
        join::Outcome::Nothing
    );
    // A page read as an event with no date yet: never built from a card either.
    let spotify = tagged(&pool, SPOTIFY).await;
    let mut event = ResourceMeta::new(SPOTIFY, "web", 3600);
    event.set_type("Event");
    event.name = Some("A reading".into());
    pinakes_core::store::save_document(&pool, &spotify, 0, &event)
        .await
        .unwrap();
    assert_eq!(
        join::join(&pool, &cfg, &spotify, None).await.unwrap(),
        join::Outcome::Nothing
    );
    assert!(backed(&pool, &spotify).await.catalogue_card.is_none());
    // An undated event's read, and a Pubky object's, keep the first tier however often unchanged.
    sqlx::query("DELETE FROM resource")
        .execute(&pool)
        .await
        .unwrap();
    tagged(&pool, "https://example.org/event").await;
    let fetcher: &'static Fetcher = Box::leak(Box::new(Fetcher::new()));
    let mut undated = ResourceMeta::new("https://example.org/event", "web", 0);
    undated.set_type("Event");
    undated.name = Some("A festival".into());
    *fetcher.answer.lock().unwrap() = Some(undated);
    let off = QueueConfig::default();
    for _ in 0..3 {
        due_now(&pool).await;
        let job = queue::claim_sources(&pool, &["web"], &off, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        queue::run_job(&pool, fetcher, &off, &job).await.unwrap();
        assert_eq!(wait_days(&pool).await, 7.0);
    }
    sqlx::query("DELETE FROM resource")
        .execute(&pool)
        .await
        .unwrap();
    tagged(&pool, "https://example.org/pubky-object").await;
    sqlx::query("UPDATE resource SET resolver='pubky'")
        .execute(&pool)
        .await
        .unwrap();
    struct Pubky;
    #[async_trait::async_trait]
    impl Resolver for Pubky {
        fn name(&self) -> &'static str {
            "pubky"
        }
        fn matches(&self, _: &str) -> bool {
            true
        }
        async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
            let mut doc = ResourceMeta::new(uri, "pubky", 86400);
            doc.set_type("CreativeWork");
            doc.name = Some("A listing".into());
            Ok(doc)
        }
        fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
            Duration::from_secs(doc.ttl_secs)
        }
    }
    let mut waits = vec![];
    for _ in 0..3 {
        due_now(&pool).await;
        let job = queue::claim_sources(&pool, &["pubky"], &off, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        queue::run_job(&pool, &Pubky, &off, &job).await.unwrap();
        waits.push(wait_days(&pool).await);
    }
    assert_eq!(waits[0], waits[2], "{waits:?}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_catalogue_off_shows_nothing_until_its_sweep_and_each_removal_is_a_change(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let article = tagged(&pool, ARTICLE).await;
    join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&article)).await;
    // Switched off, before the background sweep ran: the card shows no labels and no `from`.
    let mut off = Config::default();
    off.catalogues.wikidata.enabled = false;
    let app = api::router(AppState::new(pool.clone(), off).unwrap());
    let (status, card) = get(&app, &format!("/v1/resources/{article}")).await;
    assert_eq!(status, 200);
    assert!(card.get("labels").is_none(), "{card}");
    assert_eq!(card["from"], Value::Null);
    assert!(backed(&pool, &article).await.catalogue_card.is_some());
    // The sweep: the labels' removal is a change line of the card.
    let lines = |pool: PgPool, id: String| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = lines(pool.clone(), article.clone()).await;
    let joinable: Vec<&'static str> = queue_cfg()
        .catalogues
        .into_iter()
        .filter(|c| *c != "wikidata")
        .collect();
    assert_eq!(join::sweep(&pool, &joinable).await.unwrap(), 1);
    assert_eq!(lines(pool.clone(), article.clone()).await, before + 1);
    assert_eq!(backed(&pool, &article).await.labels, None);
    // A first tier past the backoff's cap is a valid configuration: it is the longest wait.
    let mut long = Config::default();
    long.refresh.resource_secs[0] = 100 * 86400;
    assert!(long.validate().is_ok());
    let rule = pinakes_core::scheduler::Backoff {
        base: 100 * 86400,
        max: long.refresh.max_secs,
    };
    assert_eq!(rule.next(100 * 86400, true), 100 * 86400);
}

/// A card typed as an event (an event, a festival, a sports event) backs no link, even one shared
/// for the first time (no card of its own yet, so no type of its own): the link is fetched, with
/// its event schedule. Another catalogue's card of the same thing that is no event still backs it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_fresh_link_never_takes_an_event_typed_card(pool: PgPool) {
    let cfg = queue_cfg();
    let olympics = "https://en.wikipedia.org/wiki/2024_Summer_Olympics";
    let festival = "https://en.wikipedia.org/wiki/Roskilde_Festival";
    let event = |id: &str, uri: &str, r#type: &str| Card {
        source_id: id.into(),
        uri: uri.into(),
        r#type: r#type.into(),
        name: id.into(),
        keys: vec![format!("wikidata:{id}")],
        ..Default::default()
    };
    import(
        &pool,
        Catalogue::Wikidata,
        &[
            event("Q995653", olympics, "Event"),
            event("Q211919", festival, "Festival"),
        ],
    )
    .await;
    for uri in [olympics, festival] {
        let id = tagged(&pool, uri).await;
        let outcome = join::join(&pool, &cfg, &id, None).await.unwrap();
        assert_ne!(outcome, join::Outcome::Built, "{uri}");
        let row = backed(&pool, &id).await;
        assert_eq!(row.catalogue_card, None, "{uri}");
        assert_eq!(row.name, None, "{uri}");
        assert!(row.due, "{uri}: fetched");
    }
    // A link the Wikidata event card names by its `url:` key (its best hold) and a MusicBrainz
    // card of the same thing, typed as a place, by the key the link names: the place card backs
    // it; the event card never does.
    let page = "https://musicbrainz.org/artist/0a1b2c3d-0000-4000-8000-000000000001";
    let mut festival_card = event("Q211919", festival, "Festival");
    festival_card.keys.push(link_key(page).unwrap());
    import(&pool, Catalogue::Wikidata, &[festival_card]).await;
    import(
        &pool,
        Catalogue::Musicbrainz,
        &[Card {
            source_id: "artist/1".into(),
            uri: "https://musicbrainz.org/place/0a1b2c3d-0000-4000-8000-000000000002".into(),
            r#type: "Place".into(),
            name: "Roskilde Festival".into(),
            keys: vec!["mb:artist:0a1b2c3d-0000-4000-8000-000000000001".into()],
            ..Default::default()
        }],
    )
    .await;
    let item = tagged(&pool, page).await;
    assert_eq!(
        join::join(&pool, &cfg, &item, None).await.unwrap(),
        join::Outcome::Built
    );
    let card: String = sqlx::query_scalar("SELECT c.catalogue FROM resource r JOIN catalogue_card c ON c.id=r.catalogue_card WHERE r.id=$1")
        .bind(&item)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(card, "musicbrainz");
}

/// A link a source of people's own posts reads (a Bluesky profile) that a card names by a `url:`
/// key is never built from the card, even before its first read: it keeps its own card and joins
/// no group. A card whose own link is such a link answers no read of a link nobody shared.
#[sqlx::test(migrations = "../../migrations")]
async fn a_social_link_a_card_names_is_fetched_from_its_first_reference(pool: PgPool) {
    let cfg = queue_cfg();
    let profile = "https://bsky.app/profile/douglasadams.bsky.social";
    assert!(join::social_link(profile));
    assert!(!join::social_link(ARTICLE));
    let mut card = adams();
    card.keys.push(link_key(profile).unwrap());
    import(&pool, Catalogue::Wikidata, &[card]).await;
    let id = tagged(&pool, profile).await;
    assert_eq!(
        join::join(&pool, &cfg, &id, None).await.unwrap(),
        join::Outcome::Nothing
    );
    assert_eq!(backed(&pool, &id).await.catalogue_card, None);
    assert!(catalogue_keys(&pool, &id).await.is_empty());
    // A card whose own link is such a link is refused: no card stands for it.
    let report = store::upsert(
        &pool,
        Catalogue::Musicbrainz,
        1,
        &[Card {
            source_id: "artist/x".into(),
            uri: profile.into(),
            r#type: "Person".into(),
            name: "Douglas Adams".into(),
            ..Default::default()
        }],
        None,
    )
    .await
    .unwrap();
    assert_eq!((report.inserted, report.rejected), (0, 1));
    let (own, _, _) = model::identity(profile).unwrap();
    assert_eq!(
        join::unshared(&pool, &cfg.catalogues, &own).await.unwrap(),
        None
    );
}

/// An event card that ties with another card of its catalogue (both hold the link's `url:`
/// key) still leaves the link to neither: the tie is no join, and the event card backs nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn an_event_card_that_ties_with_another_still_makes_no_join(pool: PgPool) {
    let cfg = queue_cfg();
    let page = "https://example.org/the-festival";
    let card = |id: &str, r#type: &str| Card {
        source_id: id.into(),
        uri: format!("https://en.wikipedia.org/wiki/{id}"),
        r#type: r#type.into(),
        name: id.into(),
        keys: vec![link_key(page).unwrap()],
        ..Default::default()
    };
    import(
        &pool,
        Catalogue::Wikidata,
        &[card("Q1", "Festival"), card("Q2", "Organization")],
    )
    .await;
    let id = tagged(&pool, page).await;
    assert_eq!(
        join::join(&pool, &cfg, &id, None).await.unwrap(),
        join::Outcome::Nothing
    );
    assert_eq!(backed(&pool, &id).await.catalogue_card, None);
}

/// While a large removal is under way (more than a thousand join checks due), a link whose card
/// is removed is read again within the hour, not all at once; a single removal gives it back due
/// at once (`a_removed_card_or_a_catalogue_off_gives_the_link_back_to_fetching`).
#[sqlx::test(migrations = "../../migrations")]
async fn a_large_removal_spreads_its_links_over_the_hour(pool: PgPool) {
    let cfg = queue_cfg();
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let ids = [tagged(&pool, ARTICLE).await, tagged(&pool, SPOTIFY).await];
    join::referenced(&pool, &cfg, &ids).await;
    // 1,001 other checks due.
    sqlx::query("INSERT INTO resource(id,uri,host,resolver,name,doc,status) SELECT substr(md5('b'||n),1,32),'https://example.org/b/'||n,'example.org','web','B','{}'::jsonb,'ok' FROM generate_series(1,1001) n")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalogue_join_due(resource_id,due_at) SELECT substr(md5('b'||n),1,32),now()-interval '1 minute' FROM generate_series(1,1001) n")
        .execute(&pool)
        .await
        .unwrap();
    store::remove(&pool, Catalogue::Wikidata, &["Q42".to_owned()], None)
        .await
        .unwrap();
    for id in &ids {
        join::join(&pool, &cfg, id, None).await.unwrap();
    }
    let waits: Vec<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE id=ANY($1) AND catalogue_card IS NULL ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(waits.len(), 2);
    assert!(waits.iter().all(|w| (0.0..3600.0).contains(w)), "{waits:?}");
    assert!(waits.iter().any(|w| *w > 1.0), "{waits:?}");
}

/// A link built from a card of a catalogue that is dropped (its cards and state row deleted) goes
/// back to the fetch queue with its next read spread over the next day, as the start's sweep
/// spreads them, whether or not the daemon still has the catalogue on; one card removed upstream
/// gives its link back at once.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dropped_catalogue_gives_its_links_back_spread_over_a_day(pool: PgPool) {
    let cfg = queue_cfg();
    import(&pool, Catalogue::Wikidata, &[adams()]).await;
    let ids = [
        tagged(&pool, ARTICLE).await,
        tagged(&pool, SPOTIFY).await,
        tagged(&pool, MB).await,
    ];
    join::referenced(&pool, &cfg, &ids).await;
    for id in &ids {
        assert!(backed(&pool, id).await.catalogue_card.is_some());
    }
    store::drop_catalogue(&pool, Catalogue::Wikidata, |_| {})
        .await
        .unwrap();
    // The daemon still runs with Wikidata on: the catalogue is gone all the same.
    drain(&pool, &cfg).await;
    let waits: Vec<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE id=ANY($1) AND catalogue_card IS NULL ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(waits.len(), 3);
    assert!(
        waits.iter().all(|w| (0.0..86400.0).contains(w)),
        "{waits:?}"
    );
    assert!(waits.iter().filter(|w| **w > 1.0).count() >= 2, "{waits:?}");
    for id in &ids {
        assert_eq!(backed(&pool, id).await.labels, None);
    }
}

/// The start's sweep deletes the related links' keys of a catalogue switched off page by page,
/// each page after the last (more keys than a page), and keeps those of a catalogue still on. It
/// reads only the off catalogue's keys: not the on one's, nor other catalogue keys (`doi:…`), nor
/// those of a catalogue whose name starts with the off one's, in whatever order the database's
/// collation puts them.
#[sqlx::test(migrations = "../../migrations")]
async fn the_sweep_deletes_an_off_catalogue_s_related_keys_page_by_page(pool: PgPool) {
    store::ensure_state(&pool, Catalogue::Wikidata)
        .await
        .unwrap();
    store::ensure_state(&pool, Catalogue::Musicbrainz)
        .await
        .unwrap();
    let a = tagged(&pool, "https://example.org/a").await;
    let b = tagged(&pool, "https://example.org/b").await;
    for (id, from) in [(&a, 0), (&b, 1300)] {
        sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) SELECT $1,'catalogue:wikidata:Q'||n,'catalogue','catalogue','infinity' FROM generate_series($2::int,$2::int+1299) n")
            .bind(id)
            .bind(from)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) SELECT $1,'catalogue:musicbrainz:artist%2F'||n,'catalogue','catalogue','infinity' FROM generate_series(1,700) n")
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) SELECT $1,k,'catalogue','catalogue','infinity' FROM (SELECT 'doi:10.1/'||n AS k FROM generate_series(1,3000) n UNION ALL SELECT 'catalogue:wikidata9:'||n FROM generate_series(1,500) n UNION ALL SELECT 'catalogue:wikidataX'||n FROM generate_series(1,500) n) x")
        .bind(&b)
        .execute(&pool)
        .await
        .unwrap();
    let before = join::sweep_keys_read();
    join::sweep(&pool, &["musicbrainz"]).await.unwrap();
    let read = join::sweep_keys_read() - before;
    assert_eq!(read, 2600, "only the off catalogue's keys are read");
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT CASE WHEN key LIKE 'doi:%' THEN 'doi' WHEN key LIKE 'catalogue:wikidata:%' THEN 'wikidata' WHEN key LIKE 'catalogue:musicbrainz:%' THEN 'musicbrainz' ELSE 'other' END,count(*) FROM resource_key WHERE source='catalogue' GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        counts,
        [
            ("doi".to_owned(), 3000),
            ("musicbrainz".to_owned(), 700),
            ("other".to_owned(), 1000)
        ]
    );
}

/// One lookup answers at most eight catalogue cards nobody shared (the rest are skipped as
/// unknown entries are), each with a lookup card's caps.
#[sqlx::test(migrations = "../../migrations")]
async fn a_lookup_answers_a_bounded_number_of_cards_nobody_shared(pool: PgPool) {
    let cards: Vec<Card> = (1..=25)
        .map(|n| Card {
            source_id: format!("Q{n}"),
            uri: format!("https://en.wikipedia.org/wiki/Item_{n}"),
            r#type: "Thing".into(),
            name: format!("Item {n}"),
            ..Default::default()
        })
        .collect();
    import(&pool, Catalogue::Wikidata, &cards).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let uris: Vec<String> = cards.iter().map(|c| c.uri.clone()).collect();
    let (status, looked) = send(
        &app,
        "POST",
        "/v1/resources/lookup",
        Some(json!({ "uris": uris })),
    )
    .await;
    assert_eq!(status, 200, "{looked}");
    let looked = looked.as_array().unwrap();
    assert_eq!(looked.len(), api::LOOKUP_UNSHARED);
    assert_eq!(api::LOOKUP_UNSHARED, 8);
    assert_eq!(looked[0]["name"], "Item 1");
    assert_eq!(looked[7]["name"], "Item 8");
}

/// A card whose root is no web link (an OpenAlex work shared as its `doi.org` page, stored as
/// its `doi:` root) carries that page: in `open_in` wherever it is read, among the unshared
/// card's aliases, and as its host in search.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_whose_root_is_no_web_link_carries_its_web_page(pool: PgPool) {
    const PAGE: &str = "https://doi.org/10.1000/abc";
    let work = Card {
        source_id: "W1".into(),
        uri: PAGE.into(),
        r#type: "ScholarlyArticle".into(),
        name: "A study of ridges".into(),
        keys: vec!["doi:10.1000/abc".into()],
        fame: 0.5,
        ..Default::default()
    };
    import(&pool, Catalogue::Openalex, &[work]).await;
    let open_in = json!([{"client": "doi.org", "url": PAGE}]);
    let index = pinakes_core::search_index::SearchIndex::in_memory();
    index.set_catalogues(&Catalogue::ALL.map(|c| c.name()));
    index.sync(&pool).await.unwrap();
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    let (status, card) = get(
        &app,
        &format!("/v1/resources/by-uri?uri={}", uri_query(PAGE)),
    )
    .await;
    assert_eq!(status, 200, "{card}");
    assert_eq!(card["uri"], "doi:10.1000/abc");
    assert_eq!(card["open_in"], open_in);
    assert_eq!(card["aliases"], json!([PAGE]));
    for path in [
        "/v1/search?q=ridges",
        "/v1/search?host=doi.org",
        "/v1/search?q=ridges&host=doi.org",
        "/v1/search?type=ScholarlyArticle&host=doi.org",
    ] {
        let (status, page) = get(&app, path).await;
        assert_eq!(status, 200, "{path}: {page}");
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{path}: {page}");
        assert_eq!(items[0]["open_in"], open_in, "{path}");
    }
    // Shared, the link is built from the card, with the page.
    let id = tagged(&pool, PAGE).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    // The page is an alias of the `doi:` root, which is built.
    let (root, _, _) = model::identity("doi:10.1000/abc").unwrap();
    assert!(backed(&pool, &root).await.catalogue_card.is_some());
    let (_, built) = get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(built["uri"], "doi:10.1000/abc");
    assert_eq!(built["open_in"], open_in);
}

/// The host a card is listed under is the same in Postgres (`catalogue_card_host`, behind its
/// index) as in the code (`catalogue::card_host`): its link's host, or for a root that is no web
/// link its identifier page's, for every identifier page there is.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_s_host_is_the_same_in_postgres_and_in_the_code(pool: PgPool) {
    let spellings: Value =
        serde_json::from_str(include_str!("../../../data/spellings.json")).unwrap();
    let mut uris: Vec<String> = spellings["pages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| !e["page"].is_null())
        .filter_map(|e| model::identity(e["id"].as_str().unwrap()).ok())
        .map(|(_, uri, _)| uri)
        .collect();
    assert!(uris.len() > 30, "{}", uris.len());
    uris.extend(
        [
            ARTICLE,
            MB,
            WEBSITE,
            "https://Example.COM:8080/a?b#c",
            "doi:10.1000/ABC",
        ]
        .into_iter()
        .filter_map(|u| model::identity(u).ok())
        .map(|(_, uri, _)| uri),
    );
    for uri in &uris {
        let code = pinakes_core::catalogue::card_host(uri);
        assert!(code.is_some(), "{uri}");
        let sql: Option<String> = sqlx::query_scalar("SELECT catalogue_card_host($1)")
            .bind(uri)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sql, code, "{uri}");
    }
}

/// Whether a link is social content is decided by what it names, as its source parses it: a
/// Reddit post or user and a Substack profile are, a subreddit, a Substack publication and its
/// posts (articles) are not, so these are built from a card that names them.
#[test]
fn a_link_is_social_by_what_it_names() {
    for (uri, social) in [
        (
            "https://www.reddit.com/r/rust/comments/abc123/a_post/",
            true,
        ),
        ("https://www.reddit.com/user/someone", true),
        ("https://www.reddit.com/r/rust", false),
        ("https://substack.com/@someone", true),
        ("https://example.substack.com/", false),
        ("https://example.substack.com/p/a-post", false),
        ("https://bsky.app/profile/someone.bsky.social", true),
        ("https://en.wikipedia.org/wiki/Rust", false),
    ] {
        assert_eq!(join::social_link(uri), social, "{uri}");
    }
}
