//! One search unit per thing: a referenced link built from a catalogue card stands for its card
//! and the cards of other catalogues sharing its keys, with people's tags and the card's labels
//! together and the card's fame and other names in the text index; cards of several catalogues
//! sharing a key are one unit, the Wikidata card's; the join and its release reach the index.
use pinakes_core::{
    api::{self, AppState},
    catalogue::{join, link_key, store, Card, Catalogue},
    config::{Config, QueueConfig},
    search_index::SearchIndex,
};
use serde_json::{json, Value};
use sqlx::PgPool;
mod common;

const ARTICLE: &str = "https://en.wikipedia.org/wiki/Douglas_Adams";
const SPOTIFY: &str = "https://example.com/hitchhikers-guide/author";
const MB_ID: &str = "e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4";
const MB_KEY: &str = "mb:artist:e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4";

fn queue_cfg() -> QueueConfig {
    QueueConfig {
        catalogues: join::joinable(&Default::default()),
        ..Default::default()
    }
}
/// The Wikidata card of Douglas Adams: the Spotify link people share for him as a `url:` key.
fn adams() -> Card {
    Card {
        source_id: "Q42".into(),
        uri: ARTICLE.into(),
        r#type: "Person".into(),
        name: "Douglas Adams".into(),
        other_names: vec!["Bop Ad".into()],
        description: Some("English writer and humourist".into()),
        labels: vec!["Science Fiction".into(), "humourist".into()],
        keys: vec![
            "wikidata:Q42".into(),
            MB_KEY.into(),
            link_key(SPOTIFY).unwrap(),
        ],
        fame: 0.9,
        ..Default::default()
    }
}
/// His MusicBrainz card, which names the Wikidata item.
fn mb_adams() -> Card {
    Card {
        source_id: format!("artist/{MB_ID}"),
        uri: format!("https://musicbrainz.org/artist/{MB_ID}"),
        r#type: "Person".into(),
        name: "Douglas Adams".into(),
        description: Some("British author".into()),
        labels: vec!["writer".into()],
        keys: vec![MB_KEY.into(), "wikidata:Q42".into()],
        fame: 0.4,
        ..Default::default()
    }
}
async fn import(pool: &PgPool, catalogue: Catalogue, generation: i64, cards: &[Card]) {
    let report = store::upsert(pool, catalogue, generation, cards, None)
        .await
        .unwrap();
    assert_eq!(report.rejected, 0);
    sqlx::query("UPDATE catalogue_state SET generation=$2,last_import_at=now() WHERE catalogue=$1")
        .bind(catalogue.name())
        .bind(generation)
        .execute(pool)
        .await
        .unwrap();
}
/// A link `authors` people tagged `label`: its resource ID.
async fn tagged(pool: &PgPool, uri: &str, label: &str, authors: usize) -> String {
    let id = pinakes_core::store::ensure_resource(pool, uri)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=NULL WHERE id=$1")
        .bind(&id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,kind,author,label,object_path,created_at,unit_key) SELECT $1,'tag',lpad(g::text,52,'y'),$2,'/pub/t/'||$1||'/'||g,now(),'tag:'||$2 FROM generate_series(1,$3) g ON CONFLICT DO NOTHING")
        .bind(&id)
        .bind(label)
        .bind(authors as i32)
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
/// The API over an index in memory, read up before each search.
fn app(pool: &PgPool) -> (axum::Router, std::sync::Arc<SearchIndex>) {
    let index = SearchIndex::in_memory();
    index.set_catalogues(&Catalogue::ALL.map(|c| c.name()));
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    state.search_index = index.clone();
    (api::router(state), index)
}
fn items(page: &Value) -> Vec<Value> {
    page["items"].as_array().unwrap().clone()
}
fn urls(page: &Value) -> Vec<String> {
    items(page)
        .iter()
        .map(|i| i["uri"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Cards of two catalogues that share a key are one unit, the Wikidata card's, with and without
/// text and in the facets; a card a search finds alone is listed as itself; two cards of one
/// catalogue sharing a key stay two things.
#[sqlx::test(migrations = "../../migrations")]
async fn cards_of_two_catalogues_sharing_a_key_are_one_unit(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    import(&pool, Catalogue::Musicbrainz, 1, &[mb_adams()]).await;
    let (app, _) = app(&pool);
    for path in [
        "/v1/search?q=adams&facet=type",
        "/v1/search?type=Person&facet=type",
        "/v1/search?facet=type",
    ] {
        let page = common::get(&app, path).await;
        assert_eq!(urls(&page), [ARTICLE], "{path}: {page}");
        assert_eq!(
            page["facets"]["type"],
            json!([{"type": "Person", "count": 1, "type_path": ["Thing", "Person"]}]),
            "{path}"
        );
    }
    // Only the MusicBrainz card has the label, or the words: it is the unit there.
    for path in ["/v1/search?tags=writer", "/v1/search?q=british"] {
        assert_eq!(
            urls(&common::get(&app, path).await),
            [mb_adams().uri],
            "{path}"
        );
    }
    // Two Wikidata items sharing a key are two things.
    let mut other = adams();
    other.source_id = "Q4242".into();
    other.uri = "https://en.wikipedia.org/wiki/Douglas_Adams_(other)".into();
    other.keys = vec![MB_KEY.into()];
    other.fame = 0.1;
    import(&pool, Catalogue::Wikidata, 1, &[adams(), other]).await;
    let page = common::get(&app, "/v1/search?type=Person").await;
    assert_eq!(items(&page).len(), 2, "{page}");
}

/// A shared link built from a card is the one unit of its thing: its card and the card of another
/// catalogue sharing a key leave the page; it is found by the card's labels, other names and
/// text, its people's tags and the card's labels filter and count together, and it shows them.
#[sqlx::test(migrations = "../../migrations")]
async fn a_shared_link_built_from_a_card_stands_for_its_cards(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    import(&pool, Catalogue::Musicbrainz, 1, &[mb_adams()]).await;
    let id = tagged(&pool, SPOTIFY, "books", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    let card: Option<i64> = sqlx::query_scalar("SELECT catalogue_card FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(card.is_some());
    let (app, _) = app(&pool);
    for path in [
        "/v1/search?q=adams",
        "/v1/search?q=bop",
        "/v1/search?q=adams&tags=science-fiction",
        "/v1/search?q=adams&tags=humourist",
        "/v1/search?q=adams&tags=books",
        "/v1/search?tags=humourist",
        "/v1/search?tags=books,humourist&match=all",
        "/v1/search?type=Person",
        "/v1/search",
    ] {
        let page = common::get(&app, path).await;
        assert_eq!(urls(&page), [SPOTIFY], "{path}: {page}");
        assert_eq!(
            items(&page)[0]["labels"],
            json!(["science-fiction", "humourist"]),
            "{path}"
        );
    }
    for path in [
        "/v1/search?tags=writer",
        "/v1/search?tags=books,writer&match=all",
    ] {
        assert!(
            items(&common::get(&app, path).await).is_empty(),
            "{path}: the MusicBrainz card is the shared link's thing"
        );
    }
    let page = common::get(&app, "/v1/search?q=adams&facet=type,tags").await;
    assert_eq!(
        page["facets"]["type"],
        json!([{"type": "Person", "count": 1, "type_path": ["Thing", "Person"]}])
    );
    assert_eq!(
        page["facets"]["tags"],
        json!([
            {"label": "books", "count": 1},
            {"label": "humourist", "count": 1},
            {"label": "science-fiction", "count": 1}
        ])
    );
    // A social filter reads people's tags only.
    let page = common::get(&app, "/v1/search?tags=humourist&referenced=true").await;
    assert_eq!(urls(&page), Vec::<String>::new(), "{page}");
}

/// The card's fame ranks the link built from it as it ranks the card: of two links that read
/// the same, the one built from a famous card comes first.
#[sqlx::test(migrations = "../../migrations")]
async fn a_link_built_from_a_card_ranks_with_its_fame(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    let built = tagged(&pool, SPOTIFY, "books", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&built)).await,
        1
    );
    let doc: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&built)
        .fetch_one(&pool)
        .await
        .unwrap();
    // A page that reads the same, unknown to any catalogue.
    // More active than the built link: only the card's fame puts that first.
    let plain = tagged(&pool, "https://example.org/adams", "books", 2).await;
    let mut same: pinakes_core::model::ResourceMeta = serde_json::from_value(doc).unwrap();
    same.url = "https://example.org/adams".into();
    same.catalogue = None;
    same.labels = vec![];
    same.source = "web".into();
    pinakes_core::store::save_document(&pool, &plain, 0, &same)
        .await
        .unwrap();
    let (app, index) = app(&pool);
    for _ in 0..3 {
        let page = common::get(&app, "/v1/search?q=douglas%20adams").await;
        assert_eq!(
            urls(&page),
            [SPOTIFY, "https://example.org/adams"],
            "{page}"
        );
    }
    assert!(index.served() > 0);
}

/// The join and its release reach the index: a link built from a card is found by the card's
/// labels and other names as they change, and no longer once the card is gone.
#[sqlx::test(migrations = "../../migrations")]
async fn the_join_and_its_release_reach_the_index(pool: PgPool) {
    let cfg = queue_cfg();
    let id = tagged(&pool, SPOTIFY, "books", 1).await;
    let (app, index) = app(&pool);
    // `science-fiction` is only a label (a label never matches text alone; with text it filters
    // in the index), `bop` only an other name.
    let labelled = "/v1/search?q=adams&tags=science-fiction";
    assert!(items(&common::get(&app, labelled).await).is_empty());
    // The card comes: the link joins it when the card is written.
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    while join::drain(&pool, &cfg, 100).await.unwrap() > 0 {}
    assert!(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT catalogue_card FROM resource WHERE id=$1"
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap()
    .is_some());
    for path in [labelled, "/v1/search?q=bop"] {
        assert_eq!(urls(&common::get(&app, path).await), [SPOTIFY], "{path}");
    }
    // An update renames the other name: the index reads the link again with it.
    let mut renamed = adams();
    renamed.other_names = vec!["Zaphod".into()];
    import(&pool, Catalogue::Wikidata, 1, &[renamed]).await;
    while join::drain(&pool, &cfg, 100).await.unwrap() > 0 {}
    assert_eq!(
        urls(&common::get(&app, "/v1/search?q=zaphod").await),
        [SPOTIFY]
    );
    assert!(items(&common::get(&app, "/v1/search?q=bop").await).is_empty());
    // The card goes: the link is released, and its labels leave the index.
    sqlx::query("DELETE FROM catalogue_card")
        .execute(&pool)
        .await
        .unwrap();
    while join::drain(&pool, &cfg, 100).await.unwrap() > 0 {}
    for path in [labelled, "/v1/search?q=zaphod"] {
        assert!(items(&common::get(&app, path).await).is_empty(), "{path}");
    }
    // The link stays found by its own name.
    assert!(index.documents().contains(&id));
    assert_eq!(
        urls(&common::get(&app, "/v1/search?q=adams").await),
        [SPOTIFY]
    );
}

/// A card whose only key is the link people share for it leaves the page for the link built
/// from it, by its card ID: the two have different unit IDs and no key in common.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_with_only_a_shared_link_leaves_for_the_link_built_from_it(pool: PgPool) {
    const SHARED: &str = "https://example.com/the-salmon-of-doubt";
    let card = Card {
        source_id: "Q1140".into(),
        uri: "https://en.wikipedia.org/wiki/The_Salmon_of_Doubt".into(),
        r#type: "Book".into(),
        name: "The Salmon of Doubt".into(),
        labels: vec!["novel".into()],
        keys: vec![link_key(SHARED).unwrap()],
        fame: 0.5,
        ..Default::default()
    };
    import(&pool, Catalogue::Wikidata, 1, &[card]).await;
    let id = tagged(&pool, SHARED, "books", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    let (app, _) = app(&pool);
    for path in ["/v1/search", "/v1/search?q=salmon", "/v1/search?tags=novel"] {
        assert_eq!(urls(&common::get(&app, path).await), [SHARED], "{path}");
    }
}
