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
    // Only the MusicBrainz card has the label `writer`: the shared link fails that filter, so it
    // stands for nothing there and the card is the row. Nothing has both `books` and `writer`.
    assert_eq!(
        urls(&common::get(&app, "/v1/search?tags=writer").await),
        [mb_adams().uri]
    );
    assert!(items(&common::get(&app, "/v1/search?tags=books,writer&match=all").await).is_empty());
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
    // No card has the tag `books`: the plain read, whose facets count the link's labels too.
    let page = common::get(&app, "/v1/search?tags=books&facet=tags").await;
    assert_eq!(urls(&page), [SPOTIFY]);
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
    let bop = common::get(&app, "/v1/search?q=bop").await;
    assert!(items(&bop).is_empty(), "{bop}");
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

/// A link built from a card stands for that card and for other catalogues' cards of its thing,
/// never for another item of its own catalogue that shares a key (each is its own thing there).
#[sqlx::test(migrations = "../../migrations")]
async fn a_link_built_from_a_card_leaves_other_items_of_its_catalogue_listed(pool: PgPool) {
    let mut other = adams();
    other.source_id = "Q4242".into();
    other.uri = "https://en.wikipedia.org/wiki/Douglas_Adams_(other)".into();
    other.keys = vec![MB_KEY.into()];
    other.labels = vec![];
    other.fame = 0.1;
    import(&pool, Catalogue::Wikidata, 1, &[adams(), other.clone()]).await;
    let id = tagged(&pool, SPOTIFY, "books", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    let (app, _) = app(&pool);
    for path in ["/v1/search?type=Person", "/v1/search?q=adams"] {
        let mut found = urls(&common::get(&app, path).await);
        found.sort();
        let mut want = vec![SPOTIFY.to_owned(), other.uri.clone()];
        want.sort();
        assert_eq!(found, want, "{path}");
    }
}

/// A link built from a card stands for another catalogue's card only where the link itself is
/// listed: a type only the other card has lists that card.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_is_left_out_only_where_the_link_that_stands_for_it_is_listed(pool: PgPool) {
    let mut band = mb_adams();
    band.r#type = "MusicGroup".into();
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    import(
        &pool,
        Catalogue::Musicbrainz,
        1,
        std::slice::from_ref(&band),
    )
    .await;
    let id = tagged(&pool, SPOTIFY, "books", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    let (app, _) = app(&pool);
    for (path, want) in [
        ("/v1/search?type=MusicGroup", vec![band.uri.clone()]),
        ("/v1/search?q=adams&type=MusicGroup", vec![band.uri.clone()]),
        ("/v1/search?type=Person", vec![SPOTIFY.to_owned()]),
        ("/v1/search", vec![SPOTIFY.to_owned()]),
        ("/v1/search?q=adams", vec![SPOTIFY.to_owned()]),
    ] {
        assert_eq!(urls(&common::get(&app, path).await), want, "{path}");
    }
    let page = common::get(&app, "/v1/search?facet=type").await;
    assert_eq!(
        page["facets"]["type"],
        json!([{"type": "Person", "count": 1, "type_path": ["Thing", "Person"]}])
    );
}

/// A link stands for its card only while it is a unit of its own: once it loses its last
/// reference, or becomes another link's alias, its card is listed again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_link_without_a_reference_or_turned_alias_stands_for_no_card(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    import(&pool, Catalogue::Musicbrainz, 1, &[mb_adams()]).await;
    let id = tagged(&pool, SPOTIFY, "books", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    let (app, _) = app(&pool);
    assert_eq!(urls(&common::get(&app, "/v1/search").await), [SPOTIFY]);
    // An alias of another link: the cards again (the Wikidata card the main one).
    let root = tagged(&pool, "https://example.org/adams-root", "books", 1).await;
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&id)
        .bind(&root)
        .execute(&pool)
        .await
        .unwrap();
    let found = urls(&common::get(&app, "/v1/search?type=Person").await);
    assert_eq!(found, [ARTICLE], "the alias stands for nothing");
    sqlx::query("DELETE FROM resource_alias WHERE alias_id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        urls(&common::get(&app, "/v1/search?type=Person").await),
        [SPOTIFY]
    );
    // No reference left: the cards again.
    sqlx::query("DELETE FROM reference WHERE resource_id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        urls(&common::get(&app, "/v1/search?type=Person").await),
        [ARTICLE]
    );
}

/// A card merged into another catalogue's card is listed where that card fails a filter the
/// index only approximates: an OpenStreetMap place inside a `near` circle whose Wikidata card's
/// point lies just outside it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_merged_card_is_listed_where_its_main_card_fails_the_exact_distance(pool: PgPool) {
    let item = Card {
        source_id: "Q1374".into(),
        uri: "https://en.wikipedia.org/wiki/Matterhorn_(test)".into(),
        r#type: "Mountain".into(),
        name: "Matterhorn".into(),
        keys: vec!["wikidata:Q1374".into()],
        // About 1,040 m north of the place.
        point: Some(pinakes_core::model::Point {
            lat: 46.0094,
            lon: 7.0,
        }),
        fame: 0.9,
        ..Default::default()
    };
    let place = Card {
        source_id: "node/1374".into(),
        uri: "https://www.openstreetmap.org/node/1374".into(),
        r#type: "Mountain".into(),
        name: "Matterhorn".into(),
        keys: vec!["wikidata:Q1374".into()],
        point: Some(pinakes_core::model::Point {
            lat: 46.0,
            lon: 7.0,
        }),
        fame: 0.5,
        ..Default::default()
    };
    import(&pool, Catalogue::Wikidata, 1, std::slice::from_ref(&item)).await;
    import(&pool, Catalogue::Osm, 1, std::slice::from_ref(&place)).await;
    let (app, _) = app(&pool);
    assert_eq!(
        urls(&common::get(&app, "/v1/search?q=matterhorn").await),
        [item.uri.as_str()],
        "one thing, the Wikidata card"
    );
    assert_eq!(
        urls(&common::get(&app, "/v1/search?q=matterhorn&near=46.0,7.0&radius=1000").await),
        [place.uri.as_str()]
    );
    assert_eq!(
        urls(&common::get(&app, "/v1/search?q=matterhorn&near=46.0,7.0&radius=5000").await),
        [item.uri.as_str()]
    );
}

/// A link built from a card of a catalogue switched off (before its sweep gives the link back to
/// fetching) stands for no other catalogue's card: that card is listed beside it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_link_of_a_catalogue_switched_off_stands_for_no_other_card(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    import(&pool, Catalogue::Musicbrainz, 1, &[mb_adams()]).await;
    let id = tagged(&pool, SPOTIFY, "books", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    let mut config = Config::default();
    config.catalogues.wikidata.enabled = false;
    let index = SearchIndex::in_memory();
    index.set_catalogues(&config.catalogues.enabled_names());
    let mut state = AppState::new(pool.clone(), config).unwrap();
    state.search_index = index;
    let app = api::router(state);
    let mut found = urls(&common::get(&app, "/v1/search?type=Person").await);
    found.sort();
    assert_eq!(found, [SPOTIFY.to_owned(), mb_adams().uri], "both listed");
}

/// `POST path` with a JSON body, as the client at 127.0.0.2:1234: the status and the JSON body.
async fn post(app: &axum::Router, path: &str, body: Value) -> (u16, Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A card whose own link is stored, built from it, but not listed (resolved and never shared,
/// shared and its last tag removed, or turned an alias) is still listed, as the card: the row of
/// its own link, which is the same unit, never hides it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_card_whose_own_link_is_stored_but_not_listed_stays_listed(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    let (app, _) = app(&pool);
    // Resolved, never shared: a stored row built from the card, with no reference.
    let (status, resolved) = post(&app, "/v1/resolve", json!({"uri": ARTICLE})).await;
    assert_eq!(status, 200, "{resolved}");
    let backed: Option<i64> =
        sqlx::query_scalar("SELECT catalogue_card FROM resource WHERE uri=$1")
            .bind(ARTICLE)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(backed.is_some(), "the resolve built the row from the card");
    for path in ["/v1/search?type=Person", "/v1/search?q=adams", "/v1/search"] {
        assert_eq!(
            urls(&common::get(&app, path).await),
            [ARTICLE],
            "resolved: {path}"
        );
    }
    // Shared: the row is listed (one row, the resource's).
    let id = tagged(&pool, ARTICLE, "books", 1).await;
    assert_eq!(
        urls(&common::get(&app, "/v1/search?type=Person").await),
        [ARTICLE]
    );
    let listed = common::get(&app, "/v1/search?type=Person").await;
    assert_eq!(
        items(&listed)[0]["refs"],
        json!([ARTICLE]),
        "the resource's row"
    );
    // Its last tag removed: the card again.
    sqlx::query("DELETE FROM reference WHERE resource_id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        urls(&common::get(&app, "/v1/search?type=Person").await),
        [ARTICLE],
        "last tag removed"
    );
    // Shared again, then an alias of another link: the card again.
    tagged(&pool, ARTICLE, "books", 1).await;
    let root = tagged(&pool, "https://example.org/adams-root", "books", 1).await;
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&id)
        .bind(&root)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        urls(&common::get(&app, "/v1/search?type=Person").await),
        [ARTICLE],
        "an alias"
    );
}

/// The ETag of `GET path`.
async fn etag(app: &axum::Router, path: &str) -> String {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    response.headers()["etag"].to_str().unwrap().to_owned()
}

/// A search that lists no catalogue card but reads links built from one depends on the catalogues
/// that are on (their labels match `tags` and show): its ETag changes when one is switched off.
#[sqlx::test(migrations = "../../migrations")]
async fn the_etag_of_a_search_over_links_built_from_cards_names_the_catalogues(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    let id = tagged(&pool, SPOTIFY, "books", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    // No card has the label `books`: none is listed.
    let path = "/v1/search?tags=books";
    let on = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    assert_eq!(urls(&common::get(&on, path).await), [SPOTIFY]);
    let mut config = Config::default();
    config.catalogues.musicbrainz.enabled = false;
    let fewer = api::router(AppState::new(pool.clone(), config).unwrap());
    assert_ne!(etag(&on, path).await, etag(&fewer, path).await);
    assert_eq!(etag(&on, path).await, etag(&on, path).await);
}

/// A key two cards of one catalogue hold (two MusicBrainz recordings, studio and live, of one
/// ISRC) names neither: a Wikidata item holding it merges with neither card, and a link built from
/// that item stands for neither. With one recording holding it, the item and the recording are one
/// row again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_key_two_cards_of_one_catalogue_hold_merges_nothing(pool: PgPool) {
    let recording = |n: usize, name: &str| Card {
        source_id: format!("recording/{n}"),
        uri: format!("https://musicbrainz.org/recording/0a1b2c3d-0000-4000-8000-00000000000{n}"),
        r#type: "MusicRecording".into(),
        name: name.into(),
        keys: vec![
            format!("mb:recording:0a1b2c3d-0000-4000-8000-00000000000{n}"),
            "isrc:GBUM71029604".into(),
        ],
        fame: 0.3,
        ..Default::default()
    };
    let song = "https://en.wikipedia.org/wiki/Starman_(song)";
    let item = Card {
        source_id: "Q1361000".into(),
        uri: song.into(),
        r#type: "MusicRecording".into(),
        name: "Starman".into(),
        keys: vec!["wikidata:Q1361000".into(), "isrc:GBUM71029604".into()],
        fame: 0.8,
        ..Default::default()
    };
    import(&pool, Catalogue::Wikidata, 1, &[item.clone()]).await;
    import(
        &pool,
        Catalogue::Musicbrainz,
        1,
        &[recording(1, "Starman"), recording(2, "Starman (live)")],
    )
    .await;
    let (app, _) = app(&pool);
    for path in ["/v1/search?q=starman", "/v1/search?type=MusicRecording"] {
        let page = common::get(&app, path).await;
        assert_eq!(items(&page).len(), 3, "{path}: {page}");
    }
    // A link built from the Wikidata item stands for the item alone.
    let id = tagged(&pool, song, "music", 1).await;
    assert_eq!(
        join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await,
        1
    );
    for path in ["/v1/search?q=starman", "/v1/search?type=MusicRecording"] {
        let mut listed = urls(&common::get(&app, path).await);
        listed.sort();
        assert_eq!(
            listed,
            [
                song,
                "https://musicbrainz.org/recording/0a1b2c3d-0000-4000-8000-000000000001",
                "https://musicbrainz.org/recording/0a1b2c3d-0000-4000-8000-000000000002",
            ],
            "{path}"
        );
    }
    // The live recording loses the ISRC: the item and the studio recording are one thing.
    let mut live = recording(2, "Starman (live)");
    live.keys.truncate(1);
    import(
        &pool,
        Catalogue::Musicbrainz,
        1,
        &[recording(1, "Starman"), live],
    )
    .await;
    let mut listed = urls(&common::get(&app, "/v1/search?type=MusicRecording").await);
    listed.sort();
    assert_eq!(
        listed,
        [
            song,
            "https://musicbrainz.org/recording/0a1b2c3d-0000-4000-8000-000000000002",
        ]
    );
}

/// In the index too, a card's labels match `tags` only where catalogue cards may be listed: a
/// search that reads referenced links alone (`referenced=true`, a social filter) gives no
/// candidate place to a link only its card's labels match.
#[sqlx::test(migrations = "../../migrations")]
async fn labels_count_as_tags_in_the_index_only_where_cards_may_be_listed(pool: PgPool) {
    import(&pool, Catalogue::Wikidata, 1, &[adams()]).await;
    let id = tagged(&pool, SPOTIFY, "books", 1).await;
    join::referenced(&pool, &queue_cfg(), std::slice::from_ref(&id)).await;
    let (app, index) = app(&pool);
    common::get(&app, "/v1/search?q=adams").await;
    let found = |labels: bool| {
        let (pool, index) = (pool.clone(), index.clone());
        async move {
            let q = pinakes_core::search_index::TextQuery {
                text: "adams".into(),
                at: chrono::Utc::now(),
                tags: vec!["science-fiction".into()],
                inside_words: true,
                labels,
                ..Default::default()
            };
            index
                .candidates(&pool, q, 500)
                .await
                .unwrap()
                .unwrap()
                .units
                .into_iter()
                .map(|c| c.resource_id)
                .collect::<Vec<_>>()
        }
    };
    assert!(found(true).await.contains(&id));
    assert!(!found(false).await.contains(&id));
}
