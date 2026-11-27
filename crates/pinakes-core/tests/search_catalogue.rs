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

/// An update run's changes reach the index through its queue, and so do a later full import's
/// (its new and removed cards too, so the catalogue is not read again); a first full import's
/// by the index reading the catalogue's cards again.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_run_and_a_later_import_change_the_index_through_its_queue(pool: PgPool) {
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
    let queued = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM search_index_catalogue_queue")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let before = queued().await;
    assert!(before > 0, "an update run queues its cards");
    // A later full import (one has finished before) queues its new card and the card its end
    // removes, and each reaches the index while the import still runs.
    sqlx::query("UPDATE catalogue_state SET run='import',generation=2 WHERE catalogue='wikidata'")
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
    assert_eq!(queued().await, before + 1);
    assert_eq!(names(&common::get(&app, "/v1/search?q=dom").await), ["Dom"]);
    store::finish_import(&pool, Catalogue::Wikidata, 2)
        .await
        .unwrap();
    assert!(items(&common::get(&app, "/v1/search?q=cervino").await).is_empty());
    let later = queued().await;
    // A first full import queues nothing: its cards are found once it has finished.
    sqlx::query("INSERT INTO catalogue_state(catalogue,run,phase,generation) VALUES('musicbrainz','import','import',1)")
        .execute(&pool)
        .await
        .unwrap();
    let mut song = card(9, "Hörnli Song", "MusicRecording", &[], None, 0.4);
    song.source_id = "recording/1".into();
    store::upsert(&pool, Catalogue::Musicbrainz, 1, &[song], None)
        .await
        .unwrap();
    assert_eq!(queued().await, later);
    sqlx::query(
        "UPDATE catalogue_state SET run=NULL,last_import_at=now() WHERE catalogue='musicbrainz'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=hornli").await),
        ["Hörnli Song"]
    );
    assert_eq!(queued().await, later, "read again, not queued");
}

/// A catalogue read again after its first full import, a part per pass: the queues are drained
/// before each part, so a new tag is found while the re-read is under way; a card changed in place
/// during the import is found by its new name before the import ends; at the end the cards gone
/// since leave.
#[sqlx::test(migrations = "../../migrations")]
async fn a_reread_goes_a_part_per_pass(pool: PgPool) {
    // The catalogue's first full import is under way (none has finished): the index reads the
    // cards there are when it is built.
    let cards: Vec<Card> = (1..=6)
        .map(|n| card(n, &format!("Peak {n}"), "Mountain", &[], None, 0.5))
        .collect();
    store::upsert(&pool, Catalogue::Wikidata, 1, &cards, None)
        .await
        .unwrap();
    let index = search_index::SearchIndex::in_memory_with_reread_part(2);
    index.set_catalogues(&["wikidata"]);
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    assert_eq!(
        items(&common::get(&app, "/v1/search?q=peak").await).len(),
        6
    );
    // The import goes on: a card renamed in place, one added, one gone at its end.
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
    let root = common::unique_dir("index-bench");
    let dir = root.join("index");
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
    // The re-read goes a part (20,000 cards) per pass: as many passes as it takes, and one more.
    let started = std::time::Instant::now();
    let parts = n_cards.div_ceil(20_000) + 1;
    for _ in 0..parts {
        index.sync(&pool).await.unwrap();
    }
    let bulk = started.elapsed();
    assert_eq!(index.documents().len(), n_cards, "every card read");
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
    // One batch (20,000 rows) of the catalogue queue per pass.
    let started = std::time::Instant::now();
    let mut passes = 0;
    while queued().await > 0 {
        index.sync(&pool).await.unwrap();
        passes += 1;
    }
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
        "bench: import write {write:?}, queue rows after the import {after_import}; bulk re-read {bulk:?} ({parts} passes), index {} MB ({} B per card); update write {update_write:?}, queue rows {after_update}; drain {drain:?} ({passes} passes), left {left}; first candidate {:?}",
        size / 1_000_000,
        size / n_cards as u64,
        found.units.first().map(|c| c.card)
    );
    assert_eq!(after_import, 0);
    assert_eq!(left, 0);
    // A link that loses its text while its document sits in one segment with every card (the
    // largest segment a merge after a removal may have to rewrite): the pass returns at once, a
    // commit and a search go on while the merge rewrites the segment in the background.
    let link = pinakes_core::store::ensure_resource(&pool, "https://example.org/bench-vanishing")
        .await
        .unwrap();
    let mut doc = ResourceMeta::new("https://example.org/bench-vanishing", "web", 86400);
    doc.name = Some("Bench vanishing zyzzyva".into());
    pinakes_core::store::save_document(&pool, &link, 0, &doc)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,'bench','tag','bench','/pub/app/tags/bench',now())")
        .bind(&link)
        .execute(&pool)
        .await
        .unwrap();
    index.sync(&pool).await.unwrap();
    let started = std::time::Instant::now();
    index.merge_all().unwrap();
    let one_segment = started.elapsed();
    let segments = index.segments();
    assert!(index.holds_word("zyzzyva"));
    sqlx::query("UPDATE resource SET name=NULL, doc=doc-'name'-'description' WHERE id=$1")
        .bind(&link)
        .execute(&pool)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    index.sync(&pool).await.unwrap();
    let lost_pass = started.elapsed();
    let other =
        pinakes_core::store::ensure_resource(&pool, "https://example.org/bench-during-merge")
            .await
            .unwrap();
    let mut doc = ResourceMeta::new("https://example.org/bench-during-merge", "web", 86400);
    doc.name = Some("Bench during merge".into());
    pinakes_core::store::save_document(&pool, &other, 0, &doc)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,'bench2','tag','bench','/pub/app/tags/bench2',now())")
        .bind(&other)
        .execute(&pool)
        .await
        .unwrap();
    let started = std::time::Instant::now();
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
    let purged = index.purges();
    println!(
        "bench: {n_cards} cards in {segments} segment(s) (merged in {one_segment:?}); a link losing its text: pass {lost_pass:?}; a commit meanwhile {commit_during:?}, a search {search_during:?} ({} found); merge done {merge_left:?} after; merges after a removal {purged}",
        during.units.len()
    );
    assert!(merged_out(&index, &pool, "zyzzyva").await);
    // The catalogue switched off: every card's document deleted (whole segments, which Tantivy drops
    // or merges by itself), the words out of the files.
    index.set_catalogues(&[]);
    let started = std::time::Instant::now();
    index.sync(&pool).await.unwrap();
    let off_pass = started.elapsed();
    let started = std::time::Instant::now();
    let gone = merged_out(&index, &pool, "synthetic").await;
    println!(
        "bench: {n_cards} cards switched off: pass {off_pass:?}; words out of the files {:?} after; index {} MB",
        started.elapsed(),
        walk(&dir) / 1_000_000
    );
    assert!(gone);
    let _ = std::fs::remove_dir_all(&root);
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

/// Card reads without text are bounded: by place, the twice `cap` best known cards are read first
/// and those in the circle or box kept (the answer a read of the whole place gives, when they are
/// `cap`); when they are fewer, the twice `cap` cards nearest to the circle's centre (or each
/// box's) are read too and the best known of all of them listed, however many lie in it. By type,
/// the best known of each type and subtype. A box across the antimeridian holds the cards on both
/// sides.
#[sqlx::test(migrations = "../../migrations")]
async fn card_reads_without_text_are_bounded_by_place_and_type(pool: PgPool) {
    // 450 peaks going north from 46,7, about 111 m apart, the farther the better known.
    let peaks: Vec<Card> = (0..450)
        .map(|n| {
            card(
                n + 1,
                &format!("Peak {n}"),
                "Mountain",
                &[],
                Some((46.0 + 0.001 * n as f64, 7.0)),
                n as f32 / 450.0,
            )
        })
        .collect();
    import(&pool, Catalogue::Wikidata, &peaks).await;
    let mut config = Config::default();
    config.search.text_candidates = 50;
    let app = app(&pool, config);
    // Every peak is in the circle: the 100 best known are, and the best 50 of them are listed.
    let page = names(&common::get(&app, "/v1/search?near=46.0,7.0&radius=100000&limit=50").await);
    assert_eq!(page.len(), 50);
    assert_eq!(page.first().map(String::as_str), Some("Peak 449"));
    // A circle of the first 135 peaks holds none of the 100 best known: of the 100 nearest the
    // best known is listed first, not the best known in the circle (Peak 134).
    let page = names(&common::get(&app, "/v1/search?near=46.0,7.0&radius=15000&limit=50").await);
    assert_eq!(
        page.first().map(String::as_str),
        Some("Peak 99"),
        "{page:?}"
    );
    assert!(
        !page.iter().any(|n| n == "Peak 100" || n == "Peak 134"),
        "{page:?}"
    );
    // Nearest first: the nearest 50.
    let page = names(
        &common::get(
            &app,
            "/v1/search?near=46.0,7.0&radius=100000&sort=distance&limit=50",
        )
        .await,
    );
    assert_eq!(page.first().map(String::as_str), Some("Peak 0"));
    assert_eq!(page.len(), 50);
    // A box of every peak: the best known.
    let page = names(&common::get(&app, "/v1/search?bbox=6.9,45.99,7.1,46.46&limit=50").await);
    assert_eq!(page.len(), 50);
    assert_eq!(page.first().map(String::as_str), Some("Peak 449"));
    // A box of the first 200: the best known of the 100 nearest to its centre (46.095).
    let page = names(&common::get(&app, "/v1/search?bbox=6.9,45.99,7.1,46.1995&limit=50").await);
    assert_eq!(page.len(), 50);
    assert!(
        !page
            .iter()
            .any(|n| n == "Peak 199" || n == "Peak 150" || n == "Peak 0"),
        "{page:?}"
    );
    // By type: the best known of each subtype of Place come first, whichever type they are.
    import(
        &pool,
        Catalogue::Musicbrainz,
        &(0..3)
            .map(|n| {
                let mut c = card(
                    1000 + n,
                    &format!("Glacier {n}"),
                    "Landform",
                    &[],
                    None,
                    1.0,
                );
                c.source_id = format!("place/{n}");
                c
            })
            .collect::<Vec<_>>(),
    )
    .await;
    let page = names(&common::get(&app, "/v1/search?type=Place&limit=5").await);
    assert_eq!(page[..3].len(), 3);
    assert!(
        page[..3].iter().all(|n| n.starts_with("Glacier")),
        "{page:?}"
    );
    assert_eq!(page[3], "Peak 449");
    // Across the antimeridian.
    import(
        &pool,
        Catalogue::Openalex,
        &[
            card(
                2001,
                "East of the line",
                "Place",
                &[],
                Some((0.0, 179.9)),
                0.1,
            ),
            card(
                2002,
                "West of the line",
                "Place",
                &[],
                Some((0.0, -179.9)),
                0.2,
            ),
            card(2003, "Far away", "Place", &[], Some((0.0, 0.0)), 0.3),
        ]
        .map(|mut c| {
            c.source_id = c.source_id.replace('Q', "W");
            c
        }),
    )
    .await;
    let mut page = names(&common::get(&app, "/v1/search?bbox=179,-1,-179,1").await);
    page.sort();
    assert_eq!(page, ["East of the line", "West of the line"]);
}

/// The card paths a search takes besides text, label, type and place: a host, with and without
/// text; text ranked by activity (cards nobody referenced after every referenced link, the best
/// known first); pages through a cursor over cards and links together, each unit once; a dropped
/// catalogue's documents leave the index.
#[sqlx::test(migrations = "../../migrations")]
async fn card_paths_by_host_activity_cursor_and_drop(pool: PgPool) {
    let mut cards: Vec<Card> = (1..=30)
        .map(|n| {
            card(
                n,
                &format!("Ridge {n}"),
                "Mountain",
                &["alps"],
                None,
                n as f32 / 30.0,
            )
        })
        .collect();
    for c in cards.iter_mut().take(5) {
        c.uri = c.uri.replace("en.wikipedia.org", "www.example.net");
    }
    import(&pool, Catalogue::Wikidata, &cards).await;
    for n in 0..5 {
        referenced(
            &pool,
            &format!("https://example.org/ridge-{n}"),
            &format!("Ridge guide {n}"),
            "alps",
            1 + n,
        )
        .await;
    }
    let index = search_index::SearchIndex::in_memory();
    index.set_catalogues(&Catalogue::ALL.map(|c| c.name()));
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    for path in [
        "/v1/search?host=www.example.net",
        "/v1/search?q=ridge&host=www.example.net",
    ] {
        let mut page = names(&common::get(&app, path).await);
        page.sort();
        assert_eq!(
            page,
            ["Ridge 1", "Ridge 2", "Ridge 3", "Ridge 4", "Ridge 5"],
            "{path}"
        );
    }
    let page = names(&common::get(&app, "/v1/search?q=ridge&sort=activity&limit=7").await);
    assert!(
        page[..5].iter().all(|n| n.starts_with("Ridge guide")),
        "{page:?}"
    );
    assert_eq!(&page[5..], ["Ridge 30", "Ridge 29"]);
    // Every unit once across the pages.
    let mut seen = vec![];
    let mut path = "/v1/search?tags=alps&limit=8".to_owned();
    loop {
        let page = common::get(&app, &path).await;
        seen.extend(names(&page));
        match page["cursor"].as_str() {
            Some(cursor) => path = format!("/v1/search?tags=alps&limit=8&cursor={cursor}"),
            None => break,
        }
    }
    let count = seen.len();
    seen.sort();
    seen.dedup();
    assert_eq!((count, seen.len()), (35, 35));
    // Dropped while off: its documents leave the index.
    index.set_catalogues(&["musicbrainz"]);
    store::drop_catalogue(&pool, Catalogue::Wikidata, |_| {})
        .await
        .unwrap();
    index.sync(&pool).await.unwrap();
    let held = index.documents();
    assert!(
        !held.iter().any(|d| d.starts_with("catalogue:")),
        "{held:?}"
    );
    assert_eq!(held.len(), 5);
}

/// A later full import of a catalogue the index has read reaches it through the queue and is not
/// read again; a first import after a drop (its generation starts at 1 again) is read in full.
#[sqlx::test(migrations = "../../migrations")]
async fn a_later_import_is_not_read_again_but_a_first_one_after_a_drop_is(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[card(1, "Matterhorn", "Mountain", &[], None, 0.9)],
    )
    .await;
    let index = search_index::SearchIndex::in_memory();
    index.set_catalogues(&["wikidata"]);
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=matterhorn").await),
        ["Matterhorn"]
    );
    // A re-import: a new card and the old one gone, then its end.
    sqlx::query("UPDATE catalogue_state SET run='import',generation=2 WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    store::upsert(
        &pool,
        Catalogue::Wikidata,
        2,
        &[card(2, "Weisshorn", "Mountain", &[], None, 0.5)],
        None,
    )
    .await
    .unwrap();
    store::finish_import(&pool, Catalogue::Wikidata, 2)
        .await
        .unwrap();
    sqlx::query("UPDATE catalogue_state SET run=NULL,last_import_at=now()+interval '1 second' WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=weisshorn").await),
        ["Weisshorn"]
    );
    assert!(items(&common::get(&app, "/v1/search?q=matterhorn").await).is_empty());
    common::get(&app, "/v1/search?q=weisshorn").await;
    assert_eq!(index.rereads(), 0, "read through the queue, not again");
    // Dropped while the index still holds it, then imported anew: generation 1 again.
    store::drop_catalogue(&pool, Catalogue::Wikidata, |_| {})
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalogue_state(catalogue,run,phase,generation) VALUES('wikidata','import','import',1)")
        .execute(&pool)
        .await
        .unwrap();
    store::upsert(
        &pool,
        Catalogue::Wikidata,
        1,
        &[card(3, "Dom", "Mountain", &[], None, 0.4)],
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE catalogue_state SET run=NULL,last_import_at=now()+interval '2 seconds' WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(names(&common::get(&app, "/v1/search?q=dom").await), ["Dom"]);
    assert_eq!(index.rereads(), 1);
    let held = index.documents();
    assert_eq!(held.len(), 1, "only the new import's card: {held:?}");
}

/// The rows of `catalogue_card` itself the plans read (its indexes' entries left out): those each
/// scan of the table returned or filtered out, over all its loops.
fn card_heap_rows_read(plans: &[Value]) -> u64 {
    fn walk(node: &Value) -> u64 {
        let rows = |k: &str| node[k].as_f64().unwrap_or(0.0);
        let own = if node["Relation Name"] == "catalogue_card" {
            ((rows("Actual Rows")
                + rows("Rows Removed by Filter")
                + rows("Rows Removed by Index Recheck"))
                * rows("Actual Loops").max(1.0)) as u64
        } else {
            0
        };
        own + node["Plans"]
            .as_array()
            .into_iter()
            .flatten()
            .map(walk)
            .sum::<u64>()
    }
    plans
        .iter()
        .flat_map(|p| p.as_array().cloned().unwrap_or_default())
        .map(|p| walk(&p["Plan"]))
        .sum()
}
/// The catalogue rows the plans read: those each scan of `catalogue_card` or of its indexes
/// returned or filtered out, over all its loops.
fn card_rows_read(plans: &[Value]) -> u64 {
    fn walk(node: &Value) -> u64 {
        let on_cards = node["Relation Name"] == "catalogue_card"
            || node["Index Name"]
                .as_str()
                .is_some_and(|i| i.starts_with("catalogue_card"));
        let rows = |k: &str| node[k].as_f64().unwrap_or(0.0);
        let own = if on_cards {
            ((rows("Actual Rows")
                + rows("Rows Removed by Filter")
                + rows("Rows Removed by Index Recheck"))
                * rows("Actual Loops").max(1.0)) as u64
        } else {
            0
        };
        own + node["Plans"]
            .as_array()
            .into_iter()
            .flatten()
            .map(walk)
            .sum::<u64>()
    }
    plans
        .iter()
        .flat_map(|p| p.as_array().cloned().unwrap_or_default())
        .map(|p| walk(&p["Plan"]))
        .sum()
}

/// Reads without text go catalogue by catalogue, each in its `(catalogue, …)` index's order: the
/// cards of a catalogue switched off but not dropped are never read, and by type only each type's
/// best known are (not the better known cards of other types). A host is read in its index's
/// order too, and a place in each catalogue's own `point` index, nearest first.
#[sqlx::test(migrations = "../../migrations")]
async fn card_reads_without_text_skip_a_catalogue_that_is_off(pool: PgPool) {
    // On: 2,000 people and 30 lesser known mountains. Off: 2,000 mountains better known than
    // anything on, and nearer to 46,7 than any of them, so an index not led by the catalogue
    // would read them all first, in fame order or nearest first.
    let mut on: Vec<Card> = (0..2000)
        .map(|n| {
            card(
                n + 1,
                &format!("Person {n}"),
                "Person",
                &[],
                None,
                0.3 + n as f32 / 10_000.0,
            )
        })
        .collect();
    on.extend((0..30).map(|n| {
        card(
            5000 + n,
            &format!("Hill {n}"),
            "Mountain",
            &[],
            Some((46.0 + 0.01 * n as f64, 7.0)),
            n as f32 / 100.0,
        )
    }));
    import(&pool, Catalogue::Wikidata, &on).await;
    let off: Vec<Card> = (0..2000)
        .map(|n| {
            let mut c = card(
                10_000 + n,
                &format!("Alp {n}"),
                "Mountain",
                &[],
                Some((46.0 + 0.0001 * n as f64, 7.0)),
                0.6 + n as f32 / 10_000.0,
            );
            c.source_id = format!("place/{n}");
            c
        })
        .collect();
    import(&pool, Catalogue::Musicbrainz, &off).await;
    sqlx::query("ANALYZE catalogue_card")
        .execute(&pool)
        .await
        .unwrap();
    let mut config = Config::default();
    config.catalogues.musicbrainz.enabled = false;
    config.search.text_candidates = 20;
    let app = app(&pool, config);
    for (path, first) in [
        ("/v1/search?type=Mountain&limit=20", "Hill 29"),
        ("/v1/search?limit=20", "Person 1999"),
        ("/v1/search?host=en.wikipedia.org&limit=20", "Person 1999"),
        ("/v1/search?near=46.0,7.0&radius=100000&limit=20", "Hill 29"),
        (
            "/v1/search?near=46.0,7.0&radius=100000&sort=distance&limit=20",
            "Hill 0",
        ),
        ("/v1/search?bbox=6.9,45.9,7.1,46.4&limit=20", "Hill 29"),
    ] {
        api::record_card_read_plans();
        let page = names(&common::get(&app, path).await);
        let read = card_rows_read(&api::card_read_plans());
        assert_eq!(page.first().map(String::as_str), Some(first), "{path}");
        assert!(!page.iter().any(|n| n.starts_with("Alp")), "{path}");
        assert!(read > 0 && read <= 100, "{path}: {read} card rows read");
    }
}

/// One character found inside names (a CJK character here), which the index leaves to Postgres,
/// still lists the catalogue cards that hold it: the best known of each catalogue's best known
/// (twice the candidates), read in fame order, with the character in their name.
#[sqlx::test(migrations = "../../migrations")]
async fn one_character_inside_names_finds_catalogue_cards(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[
            card(
                1,
                "東京タワー",
                "LandmarksOrHistoricalBuildings",
                &[],
                None,
                0.9,
            ),
            card(2, "京都", "City", &[], None, 0.8),
            card(3, "Mount Fuji", "Mountain", &[], None, 0.7),
        ],
    )
    .await;
    let index = search_index::SearchIndex::in_memory();
    index.set_catalogues(&Catalogue::ALL.map(|c| c.name()));
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    // 東
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=%E6%9D%B1").await),
        ["東京タワー"]
    );
    // 京
    let mut both = names(&common::get(&app, "/v1/search?q=%E4%BA%AC").await);
    both.sort();
    assert_eq!(both, ["京都", "東京タワー"]);
    // With a type, those of it.
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=%E4%BA%AC&type=City").await),
        ["京都"]
    );
}

/// One character inside names reads each catalogue's own twice `cap` best known cards, not the
/// twice `cap` best known of all catalogues together: a catalogue whose cards are less known
/// still lists those whose name holds the character.
#[sqlx::test(migrations = "../../migrations")]
async fn one_character_reads_each_catalogue_s_best_known(pool: PgPool) {
    // Wikidata: 30 well known cards without the character. MusicBrainz: 3 lesser known with it.
    let famous: Vec<Card> = (1..=30)
        .map(|n| card(n, &format!("Famous {n}"), "Person", &[], None, 0.9))
        .collect();
    import(&pool, Catalogue::Wikidata, &famous).await;
    let cats: Vec<Card> = (1..=3)
        .map(|n| {
            let mut c = card(100 + n, &format!("猫 {n}"), "MusicGroup", &[], None, 0.1);
            c.source_id = format!("artist/{n}");
            c
        })
        .collect();
    import(&pool, Catalogue::Musicbrainz, &cats).await;
    let index = search_index::SearchIndex::in_memory();
    index.set_catalogues(&Catalogue::ALL.map(|c| c.name()));
    let mut config = Config::default();
    // Twice cap (10) is under each catalogue's count of Wikidata's 30.
    config.search.text_candidates = 5;
    let mut state = AppState::new(pool.clone(), config).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    let mut found = names(&common::get(&app, "/v1/search?q=%E7%8C%AB").await);
    found.sort();
    assert_eq!(found, ["猫 1", "猫 2", "猫 3"]);
}

/// Whether a key two catalogues' listed cards share names one card of each (so the cards merge)
/// is read through the key index, at most two cards per key and catalogue: a catalogue of many
/// cards that holds the key once is not read through to prove there is no second card, and a key
/// 400 cards of one catalogue hold costs two of them for that catalogue. The other catalogue's
/// check of it goes once through the key's holders (the key index does not tell catalogues
/// apart), not once per catalogue as a count of every holder did (828 rows here).
#[sqlx::test(migrations = "../../migrations")]
async fn a_shared_key_is_checked_through_the_key_index(pool: PgPool) {
    const MBID: &str = "0383dadf-2a4e-4d10-a46a-e9e041da8eb3";
    const ISRC: &str = "isrc:GBUM71029604";
    let mut item = card(1, "Shared Band", "MusicGroup", &[], None, 1.0);
    item.keys.push(format!("mb:artist:{MBID}"));
    item.keys.push(ISRC.into());
    import(&pool, Catalogue::Wikidata, &[item]).await;
    let mut artist = card(2, "Shared Band", "MusicGroup", &[], None, 1.0);
    artist.source_id = format!("artist/{MBID}");
    artist.uri = format!("https://musicbrainz.org/artist/{MBID}");
    artist.keys = vec![format!("mb:artist:{MBID}")];
    let mut others: Vec<Card> = (0..3000)
        .map(|n| {
            let mut c = card(10 + n, &format!("Band {n}"), "MusicGroup", &[], None, 0.2);
            c.source_id = format!("artist/{n:08}-0000-4000-8000-000000000000");
            c.uri = format!("https://musicbrainz.org/artist/{n:08}-0000-4000-8000-000000000000");
            c.keys = vec![format!("mb:artist:{n:08}-0000-4000-8000-000000000000")];
            c
        })
        .collect();
    others.push(artist);
    // 400 recordings holding one ISRC, better known than the other bands.
    others.extend((0..400).map(|n| {
        let mut c = card(
            5000 + n,
            &format!("Dup {n}"),
            "MusicRecording",
            &[],
            None,
            0.9,
        );
        c.source_id = format!("recording/{n:08}-0000-4000-8000-000000000001");
        c.uri = format!("https://musicbrainz.org/recording/{n:08}-0000-4000-8000-000000000001");
        c.keys = vec![
            format!("mb:recording:{n:08}-0000-4000-8000-000000000001"),
            ISRC.into(),
        ];
        c
    }));
    import(&pool, Catalogue::Musicbrainz, &others).await;
    sqlx::query("ANALYZE catalogue_card")
        .execute(&pool)
        .await
        .unwrap();
    let mut config = Config::default();
    config.search.text_candidates = 20;
    let app = app(&pool, config);
    api::record_card_read_plans();
    let page = names(&common::get(&app, "/v1/search?limit=20").await);
    let read = card_heap_rows_read(&api::card_read_plans());
    assert_eq!(
        page.iter().filter(|n| *n == "Shared Band").count(),
        1,
        "merged: {page:?}"
    );
    assert!(page.iter().any(|n| n.starts_with("Dup ")), "{page:?}");
    assert!(read <= 500, "{read} card rows read from the table");
}

/// Every read a one-character search makes while it holds its turn stops with the turn's budget,
/// the catalogue cards' too: held up past it (here by a lock on `catalogue_card`), the search
/// answers 503 with `Retry-After: 1`, counted as `budget`.
#[sqlx::test(migrations = "../../migrations")]
async fn a_one_character_card_read_stops_with_the_budget(pool: PgPool) {
    import(
        &pool,
        Catalogue::Wikidata,
        &[card(
            1,
            "東京タワー",
            "LandmarksOrHistoricalBuildings",
            &[],
            None,
            0.9,
        )],
    )
    .await;
    let index = search_index::SearchIndex::in_memory();
    index.set_catalogues(&Catalogue::ALL.map(|c| c.name()));
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    let app = api::router(state);
    assert_eq!(
        names(&common::get(&app, "/v1/search?q=%E6%9D%B1").await),
        ["東京タワー"]
    );
    index.set_bounds(1, std::time::Duration::from_millis(500));
    let spent = pinakes_core::telemetry::search_index_busy_count("budget");
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE catalogue_card IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let (status, _) = common::get_from(&app, "127.0.0.2:1234", "/v1/search?q=%E6%9D%B1").await;
    let took = started.elapsed();
    lock.rollback().await.unwrap();
    assert_eq!(status, 503);
    assert!(
        took < std::time::Duration::from_secs(4),
        "{took:?}: the budget, not the pool's 5 s"
    );
    assert!(pinakes_core::telemetry::search_index_busy_count("budget") > spent);
}

/// Each catalogue has its own two `point` indexes (geometry and geography, partial on it), which
/// the reads near a place use one catalogue at a time.
#[sqlx::test(migrations = "../../migrations")]
async fn every_catalogue_has_its_own_point_indexes(pool: PgPool) {
    for catalogue in Catalogue::ALL {
        for kind in ["point", "geography"] {
            let name = format!("catalogue_card_{kind}_{}_idx", catalogue.name());
            let definition: Option<String> =
                sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE indexname=$1")
                    .bind(&name)
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
            let definition = definition.unwrap_or_else(|| panic!("{name} is missing"));
            assert!(
                definition.contains(&format!("catalogue = '{}'", catalogue.name())),
                "{definition}"
            );
        }
    }
}
