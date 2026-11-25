//! Catalogue cards in search: found by text, label, type and place; a catalogue switched off
//! leaves search at once and the index at its next pass; `referenced=true` keeps referenced
//! units only; fame and activity weigh as the ranking says; labels and tags stay apart; a card
//! whose link someone referenced is listed once, as the resource; an update run's changes reach
//! the index through its queue.
use pinakes_core::{
    api::{self, AppState},
    catalogue::{store, Card, Catalogue},
    config::Config,
    model::{Point, ResourceMeta},
    search_index,
};
use serde_json::Value;
use sqlx::PgPool;
mod common;
use common::author;

/// A Wikidata card.
fn card(
    n: usize,
    name: &str,
    type_name: &str,
    labels: &[&str],
    point: Option<(f64, f64)>,
    fame: f32,
) -> Card {
    Card {
        source_id: format!("Q{n}"),
        uri: format!("https://en.wikipedia.org/wiki/Item_{n}"),
        r#type: type_name.into(),
        name: name.into(),
        description: Some(format!("{name}, an item.")),
        labels: labels.iter().map(|l| l.to_string()).collect(),
        keys: vec![format!("wikidata:Q{n}")],
        point: point.map(|(lat, lon)| Point { lat, lon }),
        fame,
        ..Card::default()
    }
}
/// `cards` written by a full import of `catalogue` that has finished.
async fn import(pool: &PgPool, catalogue: Catalogue, cards: &[Card]) {
    store::upsert(pool, catalogue, 1, cards, None)
        .await
        .unwrap();
    sqlx::query("UPDATE catalogue_state SET generation=1,last_import_at=now() WHERE catalogue=$1")
        .bind(catalogue.name())
        .execute(pool)
        .await
        .unwrap();
}
/// A referenced link named `name`, tagged `label` by `taggers` authors.
async fn referenced(pool: &PgPool, uri: &str, name: &str, label: &str, taggers: usize) -> String {
    let id = pinakes_core::store::ensure_resource(pool, uri)
        .await
        .unwrap();
    let mut doc = ResourceMeta::new(uri, "web", 86400);
    doc.name = Some(name.into());
    doc.description = Some("A page.".into());
    doc.set_type("WebPage");
    pinakes_core::store::save_document(pool, &id, 0, &doc)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) SELECT $1,'bot'||$2||'-'||g,'tag',$3,'/pub/app/tags/'||$2||'-'||g,now() FROM generate_series(1,$4) g")
        .bind(&id)
        .bind(uri)
        .bind(label)
        .bind(taggers as i32)
        .execute(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    pinakes_core::store::recompute_scores(&mut tx, std::slice::from_ref(&id))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    id
}
fn app(pool: &PgPool, config: Config) -> axum::Router {
    api::router(AppState::new(pool.clone(), config).unwrap())
}
fn items(page: &Value) -> Vec<Value> {
    page["items"].as_array().unwrap().clone()
}
fn names(page: &Value) -> Vec<String> {
    items(page)
        .iter()
        .map(|i| i["name"].as_str().unwrap_or_default().to_owned())
        .collect()
}
/// Passes (at most three) until the index files no longer hold `word`: a merge after a removal
/// that a merge of Tantivy's own raced is owed to a later pass.
async fn merged_out(index: &search_index::SearchIndex, pool: &PgPool, word: &str) -> bool {
    for _ in 0..3 {
        index.settle();
        if !index.holds_word(word) {
            return true;
        }
        index.sync(pool).await.unwrap();
    }
    index.settle();
    !index.holds_word(word)
}
fn unit(n: usize) -> String {
    search_index::card_unit(&format!("https://en.wikipedia.org/wiki/Item_{n}")).unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn catalogue_cards_are_found_by_text_label_type_and_place(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[
            card(
                1,
                "Matterhorn",
                "Mountain",
                &["alps", "mountain"],
                Some((45.9763, 7.6586)),
                0.9,
            ),
            card(
                2,
                "Aletsch Glacier",
                "Landform",
                &["alps"],
                Some((46.4422, 8.0703)),
                0.6,
            ),
            card(3, "Kind of Blue", "MusicAlbum", &["jazz"], None, 0.8),
        ],
    )
    .await;
    let app = app(&pool, Config::default());
    // By text: the card as the app reads it.
    let page = common::get(&app, "/v1/search?q=matterhorn").await;
    let first = &items(&page)[0];
    assert_eq!(first["id"], unit(1).as_str());
    assert_eq!(first["uri"], "https://en.wikipedia.org/wiki/Item_1");
    assert_eq!(first["name"], "Matterhorn");
    assert_eq!(first["type"], "Mountain");
    assert_eq!(first["status"], "ok");
    assert_eq!(first["labels"], serde_json::json!(["alps", "mountain"]));
    assert_eq!(first["refs"], serde_json::json!([]));
    assert_eq!(first["collections"], 0);
    assert!(first["point"].is_object());
    // A typo, a word start, folded letters.
    for q in ["matterhrn", "matter", "MATTERHORN"] {
        assert_eq!(
            names(&common::get(&app, &format!("/v1/search?q={q}")).await)[0],
            "Matterhorn",
            "{q}"
        );
    }
    // By label (as `tags`), by type (with its subtypes), near a point, without text.
    assert_eq!(
        names(&common::get(&app, "/v1/search?tags=alps").await),
        ["Matterhorn", "Aletsch Glacier"],
        "best known first"
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?tags=alps,jazz&match=all").await),
        Vec::<String>::new()
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?type=Mountain").await),
        ["Matterhorn"]
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?type=Place").await),
        ["Matterhorn", "Aletsch Glacier"]
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?exclude_type=Place").await),
        ["Kind of Blue"]
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?near=45.97,7.65&radius=5000").await),
        ["Matterhorn"]
    );
    assert_eq!(
        names(
            &common::get(
                &app,
                "/v1/search?q=alps&tags=alps&near=46.44,8.07&radius=5000"
            )
            .await
        ),
        Vec::<String>::new(),
        "a label never matches text alone"
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=glacier&near=46.44,8.07&radius=5000").await),
        ["Aletsch Glacier"]
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?near=46.2,7.8&radius=60000&sort=distance").await),
        ["Matterhorn", "Aletsch Glacier"]
    );
    // Facets count the labels.
    let facets = common::get(&app, "/v1/search?tags=alps&facet=tags,type").await["facets"].clone();
    assert_eq!(
        facets["tags"][0],
        serde_json::json!({"label": "alps", "count": 2})
    );
    assert_eq!(facets["type"].as_array().unwrap().len(), 2);
    // Searches about references never list a card nobody referenced.
    for path in [
        "/v1/search?q=matterhorn&referenced=true",
        "/v1/search?tags=alps&referenced=true",
        "/v1/search?q=matterhorn&sort=recent",
        "/v1/search?sort=trending",
        &format!("/v1/search?q=matterhorn&author={}", author(1)),
    ] {
        assert!(items(&common::get(&app, path).await).is_empty(), "{path}");
    }
}

/// A catalogue switched off leaves every search at once, though the index still holds its cards;
/// the index's next pass removes them.
#[sqlx::test(migrations = "../../migrations")]
async fn a_catalogue_switched_off_leaves_search_at_once(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[card(1, "Matterhorn", "Mountain", &["alps"], None, 0.9)],
    )
    .await;
    import(
        &pool,
        Catalogue::Musicbrainz,
        &[Card {
            source_id: "a".into(),
            uri: "https://musicbrainz.org/artist/0383dadf-2a4e-4d10-a46a-e9e041da8eb3".into(),
            r#type: "MusicGroup".into(),
            name: "Cervino Quartet".into(),
            labels: vec!["jazz".into()],
            ..Card::default()
        }],
    )
    .await;
    let on = AppState::new(pool.clone(), Config::default()).unwrap();
    let index = on.search_index.clone();
    let on = api::router(on);
    assert_eq!(
        names(&common::get(&on, "/v1/search?q=matterhorn").await),
        ["Matterhorn"]
    );
    index.settle();
    assert!(index.holds_word("matterhorn"));
    let mut config = Config::default();
    config.catalogues.wikidata.enabled = false;
    let mut off = AppState::new(pool.clone(), config.clone()).unwrap();
    off.search_index = index.clone();
    let off = api::router(off);
    // At once: the index still holds the card.
    assert!(items(&common::get(&off, "/v1/search?q=matterhorn").await).is_empty());
    assert_eq!(
        names(&common::get(&off, "/v1/search?q=cervino").await),
        ["Cervino Quartet"]
    );
    assert_eq!(
        names(&common::get(&off, "/v1/search?tags=alps").await),
        Vec::<String>::new()
    );
    assert_eq!(
        names(&common::get(&off, "/v1/search?type=Mountain").await),
        Vec::<String>::new()
    );
    // The next pass of an index for this configuration removes its documents.
    index.set_catalogues(&config.catalogues.enabled_names());
    index.sync(&pool).await.unwrap();
    assert!(merged_out(&index, &pool, "matterhorn").await);
    assert!(index.holds_word("cervino"));
    // Every catalogue off: none listed, and the index holds none.
    config.catalogues.enabled = false;
    let none = app(&pool, config.clone());
    assert!(items(&common::get(&none, "/v1/search?q=cervino").await).is_empty());
    index.set_catalogues(&config.catalogues.enabled_names());
    index.sync(&pool).await.unwrap();
    assert!(merged_out(&index, &pool, "cervino").await);
}

/// Fame and activity: a famous card the text names exactly comes before a link a thousand
/// accounts tagged; of a tagged link and a card the text matches equally, the tagged link first.
#[sqlx::test(migrations = "../../migrations")]
async fn fame_and_tags_rank_as_the_ranking_says(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[
            card(1, "Aurora", "Thing", &[], None, 1.0),
            card(2, "Comet", "Thing", &[], None, 0.0),
        ],
    )
    .await;
    referenced(
        &pool,
        "https://example.org/aurora-timelapse",
        "Aurora borealis timelapse",
        "sky",
        1000,
    )
    .await;
    referenced(&pool, "https://example.org/comet", "Comet", "space", 5).await;
    let app = app(&pool, Config::default());
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=aurora").await),
        ["Aurora", "Aurora borealis timelapse"]
    );
    let page = common::get(&app, "/v1/search?q=comet").await;
    assert_eq!(items(&page)[0]["uri"], "https://example.org/comet");
    assert_eq!(
        items(&page)[1]["uri"],
        "https://en.wikipedia.org/wiki/Item_2"
    );
    // Without text: referenced first, then the cards, best known first.
    assert_eq!(
        names(&common::get(&app, "/v1/search?limit=10").await),
        ["Aurora borealis timelapse", "Comet", "Aurora", "Comet"]
    );
}

/// Labels and tags stay apart: a card's labels in `labels`, people's tags on the link (Nexus); a
/// tag filter and the tags facet take both.
#[sqlx::test(migrations = "../../migrations")]
async fn labels_and_tags_stay_apart(pool: PgPool) {
    import(
        &pool,
        Catalogue::Musicbrainz,
        &[Card {
            source_id: "rg".into(),
            uri: "https://musicbrainz.org/release-group/8e8a594f-2175-37b0-b3a6-0d6c3b2a6a8a"
                .into(),
            r#type: "MusicAlbum".into(),
            name: "Kind of Blue".into(),
            labels: vec!["jazz".into(), "modal-jazz".into()],
            fame: 0.9,
            ..Card::default()
        }],
    )
    .await;
    let link = referenced(
        &pool,
        "https://example.org/blue-train",
        "Blue Train",
        "jazz",
        3,
    )
    .await;
    let app = app(&pool, Config::default());
    let page = common::get(&app, "/v1/search?tags=jazz&facet=tags").await;
    let rows = items(&page);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], link.as_str());
    assert_eq!(rows[0]["labels"], serde_json::json!([]));
    assert!(!rows[0]["refs"].as_array().unwrap().is_empty());
    assert_eq!(rows[1]["labels"], serde_json::json!(["jazz", "modal-jazz"]));
    assert_eq!(rows[1]["refs"], serde_json::json!([]));
    assert_eq!(
        page["facets"]["tags"],
        serde_json::json!([{"label": "jazz", "count": 2}, {"label": "modal-jazz", "count": 1}])
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?tags=modal-jazz").await),
        ["Kind of Blue"]
    );
    // A resource read has no labels; the card is not a resource yet.
    let read = common::get(&app, &format!("/v1/resources/{link}")).await;
    assert!(read.get("labels").is_none());
}

/// A card whose link someone referenced is listed once: as the resource.
#[sqlx::test(migrations = "../../migrations")]
async fn a_referenced_card_is_listed_once(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[card(1, "Matterhorn", "Mountain", &["alps"], None, 0.9)],
    )
    .await;
    let id = referenced(
        &pool,
        "https://en.wikipedia.org/wiki/Item_1",
        "Matterhorn",
        "climbing",
        2,
    )
    .await;
    assert_eq!(id, unit(1));
    let app = app(&pool, Config::default());
    for path in [
        "/v1/search?q=matterhorn",
        "/v1/search?type=WebPage,Mountain",
        "/v1/search",
        "/v1/search?q=matterhorn&collapse=group",
        "/v1/search?type=WebPage,Mountain&collapse=group",
        "/v1/search?collapse=group",
    ] {
        let page = common::get(&app, path).await;
        assert_eq!(items(&page).len(), 1, "{path}: {page}");
        assert_eq!(items(&page)[0]["labels"], serde_json::json!([]), "{path}");
        assert_eq!(items(&page)[0]["name"], "Matterhorn", "{path}");
        assert_eq!(items(&page)[0]["type"], "WebPage", "{path}");
        // The facets count the unit as the page lists it: the resource's type and tags, not the
        // card's.
        let sep = if path.contains('?') { '&' } else { '?' };
        let page = common::get(&app, &format!("{path}{sep}facet=type,tags")).await;
        assert_eq!(
            page["facets"]["type"],
            serde_json::json!([{"type": "WebPage", "count": 1, "type_path": ["Thing", "CreativeWork", "WebPage"]}]),
            "{path}: {page}"
        );
        assert_eq!(
            page["facets"]["tags"],
            serde_json::json!([{"label": "climbing", "count": 1}]),
            "{path}: {page}"
        );
    }
}

/// An update run's changes reach the index through its queue; an import's through its end.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_run_changes_the_index(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[card(1, "Matterhorn", "Mountain", &[], None, 0.9)],
    )
    .await;
    let app = app(&pool, Config::default());
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=matterhorn").await),
        ["Matterhorn"]
    );
    sqlx::query("UPDATE catalogue_state SET run='update' WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    store::upsert(
        &pool,
        Catalogue::Wikidata,
        1,
        &[
            card(1, "Monte Cervino", "Mountain", &[], None, 0.9),
            card(4, "Weisshorn", "Mountain", &[], None, 0.5),
        ],
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=cervino").await),
        ["Monte Cervino"]
    );
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=weisshorn").await),
        ["Weisshorn"]
    );
    assert!(items(&common::get(&app, "/v1/search?q=matterhorn").await).is_empty());
    store::remove(&pool, Catalogue::Wikidata, &["Q4".into()], None)
        .await
        .unwrap();
    assert!(items(&common::get(&app, "/v1/search?q=weisshorn").await).is_empty());
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM search_index_catalogue_queue")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(queued > 0, "an update run queues its cards");
    // An import queues its new cards nothing; the index reads the catalogue again when it ends.
    sqlx::query("UPDATE catalogue_state SET run='import' WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    store::upsert(
        &pool,
        Catalogue::Wikidata,
        2,
        &[card(5, "Dom", "Mountain", &[], None, 0.4)],
        None,
    )
    .await
    .unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM search_index_catalogue_queue")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, queued);
    assert!(items(&common::get(&app, "/v1/search?q=dom").await).is_empty());
    sqlx::query("UPDATE catalogue_state SET run=NULL,last_import_at=now()+interval '1 second' WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(names(&common::get(&app, "/v1/search?q=dom").await), ["Dom"]);
}

/// A catalogue read again after its import, a part per pass: the queues are drained before each
/// part, so a new tag is found while the re-read is under way; a card changed in place during the
/// import is found by its new name before the import ends; at the end the cards gone since leave.
#[sqlx::test(migrations = "../../migrations")]
async fn a_reread_goes_a_part_per_pass(pool: PgPool) {
    let cards: Vec<Card> = (1..=6)
        .map(|n| card(n, &format!("Peak {n}"), "Mountain", &[], None, 0.5))
        .collect();
    import(&pool, Catalogue::Wikidata, &cards).await;
    let index = search_index::SearchIndex::in_memory_with_reread_part(2);
    index.set_catalogues(&["wikidata"]);
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    assert_eq!(
        items(&common::get(&app, "/v1/search?q=peak").await).len(),
        6
    );
    // A re-import: a card renamed in place, one added, one gone.
    sqlx::query("UPDATE catalogue_state SET run='import',generation=2 WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    let mut next: Vec<Card> = (1..=5)
        .map(|n| card(n, &format!("Peak {n}"), "Mountain", &[], None, 0.5))
        .collect();
    next[0].name = "Summit One".into();
    next[0].description = Some("Renamed.".into());
    next.push(card(7, "Peak 7", "Mountain", &[], None, 0.5));
    store::upsert(&pool, Catalogue::Wikidata, 2, &next, None)
        .await
        .unwrap();
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=summit").await),
        ["Summit One"]
    );
    let key = |source_id: &'static str| {
        let pool = pool.clone();
        async move {
            let id: i64 = sqlx::query_scalar("SELECT id FROM catalogue_card WHERE source_id=$1")
                .bind(source_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            format!("catalogue:{id}")
        }
    };
    let (gone, added) = (key("Q6").await, key("Q7").await);
    let kept = [key("Q1").await, key("Q2").await, key("Q5").await];
    store::finish_import(&pool, Catalogue::Wikidata, 2)
        .await
        .unwrap();
    sqlx::query("UPDATE catalogue_state SET run=NULL,last_import_at=now()+interval '1 second' WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    // The first pass after the import: one part (two cards); the new card is not read yet, and a
    // tag written meanwhile is found.
    let link = referenced(
        &pool,
        "https://example.org/peak-guide",
        "Peak guide",
        "hiking",
        1,
    )
    .await;
    let page = names(&common::get(&app, "/v1/search?q=peak&limit=20").await);
    assert!(page.contains(&"Peak guide".to_owned()), "{page:?}");
    assert!(!page.contains(&"Peak 7".to_owned()), "{page:?}");
    // The index itself, not only the page: the new card has no document yet, the gone one keeps
    // its document until the last part.
    let held = index.documents();
    assert!(held.contains(&link), "{held:?}");
    assert!(!held.contains(&added) && held.contains(&gone), "{held:?}");
    assert!(kept.iter().all(|k| held.contains(k)), "{held:?}");
    // The passes after: every part, then the gone card leaves.
    for _ in 0..4 {
        common::get(&app, "/v1/search?q=peak").await;
    }
    let mut page = names(&common::get(&app, "/v1/search?q=peak&limit=20").await);
    page.sort();
    assert_eq!(
        page,
        [
            "Peak 2",
            "Peak 3",
            "Peak 4",
            "Peak 5",
            "Peak 7",
            "Peak guide"
        ]
    );
    let held = index.documents();
    assert!(held.contains(&added) && !held.contains(&gone), "{held:?}");
    assert_eq!(held.len(), 7, "six cards and the link: {held:?}");
}

/// The catalogue queue is read a batch per pass: an update run that writes faster than the index
/// reads holds back neither the resource queue nor a re-read.
#[sqlx::test(migrations = "../../migrations")]
async fn the_catalogue_queue_is_read_a_batch_per_pass(pool: PgPool) {
    let cards: Vec<Card> = (1..=5)
        .map(|n| card(n, &format!("Peak {n}"), "Mountain", &[], None, 0.5))
        .collect();
    import(&pool, Catalogue::Wikidata, &cards).await;
    let index = search_index::SearchIndex::in_memory_with_batch(2);
    index.set_catalogues(&["wikidata"]);
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    assert_eq!(
        items(&common::get(&app, "/v1/search?q=peak").await).len(),
        5
    );
    sqlx::query("UPDATE catalogue_state SET run='update' WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    let renamed: Vec<Card> = (1..=5)
        .map(|n| card(n, &format!("Summit {n}"), "Mountain", &[], None, 0.5))
        .collect();
    store::upsert(&pool, Catalogue::Wikidata, 1, &renamed, None)
        .await
        .unwrap();
    // Each pass (one before each search here) reads two of the five changes.
    for found in [2, 4, 5] {
        assert_eq!(
            items(&common::get(&app, "/v1/search?q=summit&limit=20").await).len(),
            found
        );
    }
}

/// A catalogue switched off during its first re-read (documents written, no import recorded
/// yet) leaves the index files at the next pass.
#[sqlx::test(migrations = "../../migrations")]
async fn a_catalogue_switched_off_during_its_reread_leaves_the_index(pool: PgPool) {
    let cards: Vec<Card> = (1..=6)
        .map(|n| card(n, &format!("Peak {n}"), "Mountain", &[], None, 0.5))
        .collect();
    import(&pool, Catalogue::Wikidata, &cards).await;
    let index = search_index::SearchIndex::in_memory_with_reread_part(2);
    index.set_catalogues(&[]);
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    common::get(&app, "/v1/search?q=peak").await;
    index.settle();
    assert!(!index.holds_word("peak"));
    // Switched on: the first part of its re-read.
    index.set_catalogues(&["wikidata"]);
    common::get(&app, "/v1/search?q=peak").await;
    index.settle();
    assert!(index.holds_word("peak"));
    // Switched off before the re-read ends.
    index.set_catalogues(&[]);
    common::get(&app, "/v1/search?q=peak").await;
    assert!(merged_out(&index, &pool, "peak").await);
}

/// A card whose link a resource has that search does not list (a link referenced but not read
/// yet) is still found as the card.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_behind_a_pending_link_is_found(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[card(1, "Matterhorn", "Mountain", &["alps"], None, 0.9)],
    )
    .await;
    let id = pinakes_core::store::ensure_resource(&pool, "https://en.wikipedia.org/wiki/Item_1")
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','alps','/pub/app/tags/x',now())")
        .bind(&id)
        .bind(author(1))
        .execute(&pool)
        .await
        .unwrap();
    let app = app(&pool, Config::default());
    let page = common::get(&app, "/v1/search?q=matterhorn").await;
    assert_eq!(names(&page), ["Matterhorn"]);
    assert_eq!(items(&page)[0]["labels"], serde_json::json!(["alps"]));
}

/// Nearest first by the distance on the globe, also when the candidates are cut: far north, a
/// card a few degrees of longitude away is nearer than fifty a degree of latitude away.
#[sqlx::test(migrations = "../../migrations")]
async fn nearest_first_is_the_distance_on_the_globe(pool: PgPool) {
    let mut cards: Vec<Card> = (1..=50)
        .map(|n| {
            card(
                n,
                &format!("North {n}"),
                "Place",
                &[],
                Some((80.9, 5.0 + n as f64 / 1000.0)),
                0.5,
            )
        })
        .collect();
    cards.push(card(99, "East", "Place", &[], Some((80.0, 10.0)), 0.5));
    import(&pool, Catalogue::Wikidata, &cards).await;
    let mut config = Config::default();
    config.search.text_candidates = 50;
    let app = app(&pool, config);
    let page = common::get(&app, "/v1/search?near=80.0,5.0&radius=200000&sort=distance").await;
    assert_eq!(names(&page)[0], "East");
}

/// Without text, a card nobody referenced comes after every referenced link, even a card of fame 1
/// and a link whose activity is 0 (its score not counted yet).
#[sqlx::test(migrations = "../../migrations")]
async fn a_famous_card_comes_after_an_inactive_link(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[card(1, "Matterhorn", "Mountain", &["alps"], None, 1.0)],
    )
    .await;
    let id = referenced(&pool, "https://example.org/old", "Old page", "alps", 1).await;
    sqlx::query("DELETE FROM resource_score WHERE resource_id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let app = app(&pool, Config::default());
    // Either order of the two IDs: the link first.
    assert_eq!(
        names(&common::get(&app, "/v1/search?tags=alps").await),
        ["Old page", "Matterhorn"]
    );
}

/// The cost of catalogue cards in the index, for the handoff: 100,000 cards written by a full
/// import (no queue rows; the next pass reads them all), then 100,000 changed by an update run
/// (queued, drained a batch at a time). Run with `--ignored --nocapture`.
#[sqlx::test(migrations = "../../migrations")]
#[ignore]
async fn catalogue_index_cost_at_100k_cards(pool: PgPool) {
    // `PINAKES_BENCH_CARDS` cards (100 000 unless set), for a run at a catalogue's size.
    let n_cards: usize = std::env::var("PINAKES_BENCH_CARDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(100_000);
    let cards = |round: usize| -> Vec<Card> {
        (0..n_cards)
            .map(|n| Card {
                source_id: format!("Q{n}"),
                uri: format!("https://en.wikipedia.org/wiki/Bench_{n}"),
                r#type: if n % 3 == 0 { "Place" } else { "Person" }.into(),
                name: format!("Bench item {n} round {round}"),
                description: Some(format!("A synthetic card number {n} for the benchmark.")),
                labels: vec![format!("label-{}", n % 50)],
                point: (n % 3 == 0).then(|| Point {
                    lat: 45.0 + (n % 1000) as f64 / 1000.0,
                    lon: 7.0 + (n % 777) as f64 / 777.0,
                }),
                fame: (n % 100) as f32 / 100.0,
                ..Card::default()
            })
            .collect()
    };
    let queued = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM search_index_catalogue_queue")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let dir = std::env::temp_dir().join(format!("pinakes-index-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let index = search_index::SearchIndex::on_disk(&dir);
    index.set_catalogues(&["wikidata"]);
    let passed = prometheus::IntGauge::new("passed", "test").unwrap();
    let opened = prometheus::IntGauge::new("opened", "test").unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(index.clone().run(
        pool.clone(),
        std::time::Duration::from_millis(500),
        passed.clone(),
        opened.clone(),
        shutdown,
    ));
    while opened.get() == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    stop.send(true).unwrap();
    task.await.unwrap();
    // A full import: written, then finished.
    sqlx::query("INSERT INTO catalogue_state(catalogue,run,phase,generation) VALUES('wikidata','import','import',1)")
        .execute(&pool)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    store::upsert(&pool, Catalogue::Wikidata, 1, &cards(1), None)
        .await
        .unwrap();
    let write = started.elapsed();
    let after_import = queued().await;
    sqlx::query("UPDATE catalogue_state SET run=NULL,phase='idle',last_import_at=now() WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    index.sync(&pool).await.unwrap();
    let bulk = started.elapsed();
    let size: u64 = walk(&dir);
    // An update run that changes every card.
    sqlx::query(
        "UPDATE catalogue_state SET run='update',phase='update' WHERE catalogue='wikidata'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let started = std::time::Instant::now();
    store::upsert(&pool, Catalogue::Wikidata, 1, &cards(2), None)
        .await
        .unwrap();
    let update_write = started.elapsed();
    let after_update = queued().await;
    let started = std::time::Instant::now();
    index.sync(&pool).await.unwrap();
    let drain = started.elapsed();
    let left = queued().await;
    let found = index
        .candidates(
            &pool,
            search_index::TextQuery {
                text: "bench item 77777 round 2".into(),
                at: chrono::Utc::now(),
                catalogues: vec!["wikidata".into()],
                inside_words: true,
                ..Default::default()
            },
            10,
        )
        .await
        .unwrap()
        .unwrap();
    println!(
        "bench: import write {write:?}, queue rows after the import {after_import}; bulk pass {bulk:?}, index {} MB ({} B per card); update write {update_write:?}, queue rows {after_update}; drain {drain:?}, left {left}; first candidate {:?}",
        size / 1_000_000,
        size / n_cards as u64,
        found.units.first().map(|c| c.card)
    );
    assert_eq!(after_import, 0);
    assert_eq!(left, 0);
    // The catalogue switched off: its documents leave, and the merge that takes their words out
    // of the files runs outside the writer lock, so the pass returns and searches and commits go
    // on while it runs.
    index.set_catalogues(&[]);
    let started = std::time::Instant::now();
    index.sync(&pool).await.unwrap();
    let off_pass = started.elapsed();
    let started = std::time::Instant::now();
    let link =
        pinakes_core::store::ensure_resource(&pool, "https://example.org/bench-during-merge")
            .await
            .unwrap();
    let mut doc = ResourceMeta::new("https://example.org/bench-during-merge", "web", 86400);
    doc.name = Some("Bench during merge".into());
    pinakes_core::store::save_document(&pool, &link, 0, &doc)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,'bench','tag','bench','/pub/app/tags/bench',now())")
        .bind(&link)
        .execute(&pool)
        .await
        .unwrap();
    index.sync(&pool).await.unwrap();
    let commit_during = started.elapsed();
    let started = std::time::Instant::now();
    let during = index
        .candidates(
            &pool,
            search_index::TextQuery {
                text: "bench during merge".into(),
                at: chrono::Utc::now(),
                ..Default::default()
            },
            10,
        )
        .await
        .unwrap()
        .unwrap();
    let search_during = started.elapsed();
    let started = std::time::Instant::now();
    index.settle();
    let merge_left = started.elapsed();
    println!(
        "bench: {n_cards} cards switched off: pass {off_pass:?}; a commit meanwhile {commit_during:?}, a search {search_during:?} ({} found); merge done {merge_left:?} after; index {} MB after",
        during.units.len(),
        walk(&dir) / 1_000_000
    );
    assert_eq!(index.purges(), 1);
    assert!(!index.holds_word("synthetic"));
    let _ = std::fs::remove_dir_all(&dir);
}
fn walk(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() {
                walk(&path)
            } else {
                e.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// Switching off a catalogue the index holds no document of (an import that read nothing) merges
/// nothing, not even another catalogue's or a link's deleted documents; one with documents merges
/// their words out.
#[sqlx::test(migrations = "../../migrations")]
async fn switching_off_an_empty_catalogue_merges_nothing(pool: PgPool) {
    // Forty cards in the first segment: one deleted document there stays under the merge policy's
    // 5% and waits for a merge after a removal.
    let mut cards = vec![card(1, "Matterhorn", "Mountain", &[], None, 0.5)];
    cards.extend((2..=40).map(|n| card(n, &format!("Peak {n}"), "Mountain", &[], None, 0.1)));
    import(&pool, Catalogue::Wikidata, &cards).await;
    import(&pool, Catalogue::Musicbrainz, &[]).await;
    let index = search_index::SearchIndex::in_memory();
    index.set_catalogues(&["wikidata", "musicbrainz"]);
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    let link = referenced(&pool, "https://example.org/guide", "Alps guide", "alps", 1).await;
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=matterhorn").await),
        ["Matterhorn"]
    );
    // A renamed link leaves its old document deleted in the files (no text lost: no merge).
    sqlx::query("UPDATE resource SET name='Alps guidebook', doc=jsonb_set(doc,'{name}','\"Alps guidebook\"') WHERE id=$1")
        .bind(&link)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=guidebook").await),
        ["Alps guidebook"]
    );
    index.settle();
    assert_eq!(index.purges(), 0);
    assert!(
        index.holds_word("guide"),
        "the old document is still in the files"
    );
    index.set_catalogues(&["wikidata"]);
    index.sync(&pool).await.unwrap();
    index.settle();
    assert_eq!(index.purges(), 0, "MusicBrainz had no document");
    index.set_catalogues(&[]);
    index.sync(&pool).await.unwrap();
    // Most of the segment deleted: the merge policy may take it before the merge after the removal
    // does; either way the words leave the files.
    assert!(merged_out(&index, &pool, "matterhorn").await);
    assert!(!index.holds_word("guide"));
}
