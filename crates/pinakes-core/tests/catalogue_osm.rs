//! The OpenStreetMap catalogue as the API reads it: a card nobody shared opens by its link and
//! by its ID, as an OSM link's card; a shared OSM link is built from its card without asking
//! Nominatim; the ODbL dump holds the imported places; the map clusters count everything with a
//! point, filtered like search, each with its cell's search; the place rules (map links maybe the
//! same as an imported place, a deleted object's nearby namesake, a renamed place) and that with
//! the catalogue off nothing of this shows.
use pinakes_core::{
    api::{self, AppState},
    catalogue::{osm::place, store, Card, Catalogue},
    config::Config,
    model::{Point, ResourceMeta},
};
use serde_json::Value;
use sqlx::PgPool;
mod common;

const VADUZ: (f64, f64) = (47.1410, 9.5215);

/// An imported OSM object: `kind/id`, its tags and point.
fn osm(kind: &str, id: i64, tags: &[(&str, &str)], at: (f64, f64)) -> Card {
    let tags = place::tags(tags.iter().copied());
    let geometry = place::Geometry {
        point: Some(Point {
            lat: at.0,
            lon: at.1,
        }),
        bbox: (kind != "node").then_some([at.1 - 0.001, at.0 - 0.001, at.1 + 0.001, at.0 + 0.001]),
    };
    place::read(kind, id, &tags, geometry).expect("a kept object")
}
async fn import(pool: &PgPool, cards: &[Card]) {
    store::upsert(pool, Catalogue::Osm, 1, cards, None)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE catalogue_state SET generation=1,last_import_at=now() WHERE catalogue='osm'",
    )
    .execute(pool)
    .await
    .unwrap();
}
fn app(pool: &PgPool, config: Config) -> axum::Router {
    api::router(AppState::new(pool.clone(), config).unwrap())
}
fn off() -> Config {
    let mut config = Config::default();
    config.catalogues.osm.enabled = false;
    config.queue.catalogues = pinakes_core::catalogue::join::joinable(&config.catalogues);
    config.queue.osm_catalogue = false;
    config
}
/// `GET path` on the map clusters route as a client of its own (each call another address): the
/// per-client share of turns never holds back a parallel test. Must answer 200.
async fn clusters(app: &axum::Router, path: &str) -> Value {
    static PEER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let n = PEER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let peer = format!("10.{}.{}.{}:1", (n >> 16) & 255, (n >> 8) & 255, n & 255);
    let (status, value) = common::get_from(app, &peer, path).await;
    assert_eq!(status, 200, "{path}: {value}");
    value
}
fn id_of(uri: &str) -> String {
    pinakes_core::model::identity(uri).unwrap().0
}
fn engel() -> Card {
    osm(
        "node",
        42,
        &[
            ("amenity", "restaurant"),
            ("name", "Engel"),
            ("addr:street", "Städtle"),
            ("addr:housenumber", "13"),
            ("addr:city", "Vaduz"),
            ("opening_hours", "Mo-Su 10:00-23:00"),
            ("website", "https://www.hotel-engel.li/"),
            ("cuisine", "regional"),
        ],
        VADUZ,
    )
}
/// A map link's card, as its source reads it from the link: a named point.
async fn map_link(pool: &PgPool, uri: &str, source: &str, name: &str, at: (f64, f64)) -> String {
    let id = pinakes_core::store::ensure_resource(pool, uri)
        .await
        .unwrap();
    let mut doc = ResourceMeta::new(uri, source, 0);
    doc.set_type("Place");
    doc.name = Some(name.into());
    doc.location = Some(Point {
        lat: at.0,
        lon: at.1,
    });
    doc.location_source = Some(pinakes_core::model::LocationSource::Uri);
    pinakes_core::store::save_document(pool, &id, 0, &doc)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','good','/pub/app/tags/'||$1,now())")
        .bind(&id)
        .bind(common::author(1))
        .execute(pool)
        .await
        .unwrap();
    id
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_osm_card_nobody_shared_opens_by_its_link_and_its_id(pool: PgPool) {
    import(&pool, &[engel()]).await;
    let uri = "https://www.openstreetmap.org/node/42";
    let app = app(&pool, Config::default());
    let card = common::get(
        &app,
        &format!("/v1/resources/by-uri?uri={}", urlencode(uri)),
    )
    .await;
    assert_eq!(card["id"], id_of(uri));
    assert_eq!(card["uri"], uri);
    assert_eq!(card["name"], "Engel");
    assert_eq!(card["type"], "Restaurant");
    assert_eq!(card["category"], "amenity/restaurant");
    assert_eq!(card["address"]["house_number"], "13");
    assert_eq!(card["attribution"], "© OpenStreetMap contributors (ODbL)");
    assert_eq!(card["point"]["lat"], VADUZ.0);
    assert_eq!(card["facts"], Value::Null);
    // Its other spellings and its ID answer the same card.
    let by_id = common::get(&app, &format!("/v1/resources/{}", id_of(uri))).await;
    assert_eq!(by_id, card);
    let short = common::get(
        &app,
        &format!(
            "/v1/resources/by-uri?uri={}",
            urlencode("https://osm.org/node/42")
        ),
    )
    .await;
    assert_eq!(short["uri"], uri);
    // Search lists it with the ID that opens it.
    let page = common::get(&app, "/v1/search?q=engel").await;
    assert_eq!(page["items"][0]["id"], id_of(uri));
    // Nothing was stored by the reads.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM resource")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    // With the catalogue off, it is an unknown link again.
    let app = self::app(&pool, off());
    let (status, _) = common::get_from(
        &app,
        "127.0.0.2:1",
        &format!("/v1/resources/{}", id_of(uri)),
    )
    .await;
    assert_eq!(status, 404);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_shared_osm_link_is_built_from_its_card_without_nominatim(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    import(&pool, &[engel()]).await;
    let config = pinakes_core::config::OsmConfig {
        // Nothing listens here: a Nominatim request would fail the read.
        nominatim_url: "http://127.0.0.1:9".into(),
        ..Default::default()
    };
    let resolver = pinakes_core::resolver::osm::OsmResolver::new(pool.clone(), config.clone())
        .unwrap()
        .with_catalogue(true);
    let doc = resolver
        .resolve("https://www.openstreetmap.org/node/42")
        .await
        .unwrap();
    assert_eq!(doc.name.as_deref(), Some("Engel"));
    assert_eq!(doc.category.as_deref(), Some("amenity/restaurant"));
    assert_eq!(
        doc.attributes.get("opening_hours").map(String::as_str),
        Some("Mo-Su 10:00-23:00")
    );
    assert_eq!(doc.location.as_ref().map(|p| p.lat), Some(VADUZ.0));
    let (name, display): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT name,display_name FROM osm_place WHERE osm_type='node' AND osm_id=42",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(name.as_deref(), Some("Engel"));
    assert_eq!(display.as_deref(), Some("Engel, Städtle 13, Vaduz"));
    // An object the catalogue does not hold still asks Nominatim (here: unreachable).
    assert!(resolver
        .resolve("https://www.openstreetmap.org/node/43")
        .await
        .is_err());
    // With the catalogue off, the imported object asks Nominatim too.
    let off = pinakes_core::resolver::osm::OsmResolver::new(pool.clone(), config).unwrap();
    sqlx::query("UPDATE osm_place SET refresh_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(off
        .resolve("https://www.openstreetmap.org/node/42")
        .await
        .is_err());
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_odbl_dump_holds_the_imported_places(pool: PgPool) {
    import(
        &pool,
        &[engel(), osm("way", 7, &[("amenity", "parking")], VADUZ)],
    )
    .await;
    sqlx::query("INSERT INTO osm_rename(osm_type,osm_id,changed_at,old_name,old_category,new_name) VALUES('node',42,'2026-01-01T00:00:00Z','Rössli','amenity/restaurant','Engel')")
        .execute(&pool)
        .await
        .unwrap();
    let dir = tempdir();
    let lines = |on: bool| {
        let (pool, dir) = (pool.clone(), dir.clone());
        async move {
            pinakes_core::osm_dump::write(&pool, &dir, on)
                .await
                .unwrap();
            let file = std::fs::read(pinakes_core::osm_dump::path(&dir)).unwrap();
            let mut text = String::new();
            std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&file[..]), &mut text)
                .unwrap();
            text.lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .collect::<Vec<_>>()
        }
    };
    // A rename of an object the catalogue no longer holds, and a successor.
    sqlx::query("INSERT INTO osm_rename(osm_type,osm_id,changed_at,old_name,old_category,new_name) VALUES('node',99,'2026-02-01T00:00:00Z','Adler','amenity/restaurant','Kebab Haus')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO osm_successor(old_type,old_id,new_type,new_id,basis) VALUES('node',98,'node',42,'wikidata')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO osm_removed(osm_type,osm_id,name,category,point) VALUES('node',97,'Sonne','amenity/restaurant',ST_SetSRID(ST_MakePoint(9.52,47.14),4326))")
        .execute(&pool)
        .await
        .unwrap();
    let on = lines(true).await;
    assert_eq!(on.len(), 6, "{on:?}");
    let removed = on.iter().find(|l| l["from"] == "removed").unwrap();
    assert_eq!(
        (
            removed["osm_id"].clone(),
            removed["name"].clone(),
            removed["point"]["lat"].clone()
        ),
        (97.into(), "Sonne".into(), 47.14.into())
    );
    let renamed = on.iter().find(|l| l["from"] == "rename").unwrap();
    assert_eq!(
        (
            renamed["osm_id"].clone(),
            renamed["name"].clone(),
            renamed["new_name"].clone()
        ),
        (99.into(), "Adler".into(), "Kebab Haus".into())
    );
    let successor = on.iter().find(|l| l["from"] == "successor").unwrap();
    assert_eq!(
        (
            successor["old_id"].clone(),
            successor["new_id"].clone(),
            successor["basis"].clone()
        ),
        (98.into(), 42.into(), "wikidata".into())
    );
    let engel = on.iter().find(|l| l["osm_id"] == 42).unwrap();
    assert_eq!(engel["from"], "catalogue");
    assert_eq!(engel["osm_type"], "node");
    assert_eq!(
        engel["type_path"],
        serde_json::json!(pinakes_core::model::type_path("Restaurant"))
    );
    assert_eq!(engel["tags"]["website"], "https://www.hotel-engel.li/");
    assert_eq!(engel["replaced"][0]["name"], "Rössli");
    let parking = on.iter().find(|l| l["osm_id"] == 7).unwrap();
    assert_eq!(parking["name"], "Parking");
    assert_eq!(parking["bbox"].as_array().map(Vec::len), Some(4));
    // Off: the file holds what it held before the import (the notice alone here).
    assert_eq!(lines(false).await.len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn map_clusters_count_everything_with_a_point(pool: PgPool) {
    import(
        &pool,
        &[
            engel(),
            osm(
                "node",
                1,
                &[("amenity", "atm"), ("operator", "LGT")],
                (47.1412, 9.5217),
            ),
            osm(
                "node",
                2,
                &[("place", "village"), ("name", "Triesen")],
                (47.1066, 9.5281),
            ),
        ],
    )
    .await;
    // A referenced link with a point (a map link), and one without.
    map_link(
        &pool,
        "geo:47.1411,9.5216",
        "geo",
        "Engel",
        (47.1411, 9.5216),
    )
    .await;
    let app = app(&pool, Config::default());
    let all = clusters(&app, "/v1/map/clusters?bbox=9.4,47.0,9.7,47.3&zoom=10").await;
    assert_eq!(all["total"], 4, "{all}");
    let cell = 360.0 / f64::from(1 << 12);
    assert!((all["cell_degrees"].as_f64().unwrap() - cell).abs() < 1e-12);
    let first = &all["clusters"][0];
    assert_eq!(first["count"], 3);
    let search = first["search"].as_str().unwrap();
    assert!(search.starts_with("/v1/search?bbox="), "{search}");
    // The cell's search lists what the cell counts.
    let page = common::get(&app, search).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 3, "{page}");
    // Filtered like search: a type, a label.
    let villages = clusters(
        &app,
        "/v1/map/clusters?bbox=9.4,47.0,9.7,47.3&zoom=10&type=City",
    )
    .await;
    assert_eq!(villages["total"], 1);
    assert!(villages["clusters"][0]["search"]
        .as_str()
        .unwrap()
        .ends_with("&type=City"));
    let tagged = clusters(
        &app,
        "/v1/map/clusters?bbox=9.4,47.0,9.7,47.3&zoom=10&tags=good",
    )
    .await;
    assert_eq!(tagged["total"], 1);
    // Only referenced links: the map link.
    let referenced = clusters(
        &app,
        "/v1/map/clusters?bbox=9.4,47.0,9.7,47.3&zoom=10&referenced=true",
    )
    .await;
    assert_eq!(referenced["total"], 1);
    // A view of more cells than allowed, and a bad zoom.
    for bad in [
        "/v1/map/clusters?bbox=-180,-90,180,90&zoom=12",
        "/v1/map/clusters?bbox=9,47,10,48&zoom=21",
        "/v1/map/clusters?zoom=3",
    ] {
        let (status, _) = common::get_from(&app, "127.0.0.2:1", bad).await;
        assert_eq!(status, 400, "{bad}");
    }
    // Off: only the referenced link.
    let app = self::app(&pool, off());
    let all = clusters(&app, "/v1/map/clusters?bbox=9.4,47.0,9.7,47.3&zoom=10").await;
    assert_eq!(all["total"], 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_map_link_is_maybe_the_one_place_of_its_name_nearby(pool: PgPool) {
    import(
        &pool,
        &[
            engel(),
            // Two of one name: neither is the one.
            osm(
                "node",
                50,
                &[("amenity", "cafe"), ("name", "Café Zentral")],
                (47.1500, 9.5300),
            ),
            osm(
                "node",
                51,
                &[("amenity", "cafe"), ("name", "Cafe Zentral")],
                (47.1501, 9.5301),
            ),
        ],
    )
    .await;
    // 20 m from the place, the name spelt another way.
    let link = map_link(
        &pool,
        "https://www.google.com/maps/place/ENGEL/@47.14118,9.5215,17z",
        "google_maps",
        "engel",
        (47.14118, 9.5215),
    )
    .await;
    let crowd = map_link(
        &pool,
        "https://maps.apple.com/?q=Caf%C3%A9+Zentral&ll=47.15,9.53",
        "apple_maps",
        "Café Zentral",
        (47.15, 9.53),
    )
    .await;
    let far = map_link(
        &pool,
        "https://maps.apple.com/?q=Engel&ll=47.1420,9.5215",
        "apple_maps",
        "Engel",
        (47.1420, 9.5215),
    )
    .await;
    let place = id_of("https://www.openstreetmap.org/node/42");
    let app = app(&pool, Config::default());
    let card = common::get(&app, &format!("/v1/resources/{link}")).await;
    assert_eq!(common::maybe_same_ids(&card), [place.as_str()], "{card}");
    let also = card["also_on"].as_array().unwrap();
    assert!(
        also.iter()
            .any(|a| a["id"] == place.as_str() && a["kind"] == "maybe"),
        "{card}"
    );
    // The place lists the link, with its reference, so its tags show with the place.
    let osm = common::get(&app, &format!("/v1/resources/{place}")).await;
    assert_eq!(common::maybe_same_ids(&osm), [link.as_str()], "{osm}");
    let listed = osm["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == link.as_str())
        .unwrap()
        .clone();
    assert_eq!(listed["kind"], "maybe");
    assert_eq!(
        listed["refs"][0],
        "https://www.google.com/maps/place/ENGEL/@47.14118,9.5215,17z"
    );
    // Two places of the name nearby, or the place over 50 m away: a point only.
    for id in [&crowd, &far] {
        let card = common::get(&app, &format!("/v1/resources/{id}")).await;
        assert!(
            card["maybe_same"].is_null() || card["maybe_same"].as_array().unwrap().is_empty(),
            "{card}"
        );
    }
    // Never merged: no group.
    assert!(card["group"].is_null());
    // Off: nothing of it.
    let app = self::app(&pool, off());
    let card = common::get(&app, &format!("/v1/resources/{link}")).await;
    assert!(card["maybe_same"].is_null(), "{card}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_renamed_place_says_what_it_was_and_splits_its_references(pool: PgPool) {
    import(&pool, &[engel()]).await;
    let uri = "https://www.openstreetmap.org/node/42";
    let id = pinakes_core::store::ensure_resource(&pool, uri)
        .await
        .unwrap();
    for (n, at) in [
        (1, "2025-06-01T00:00:00Z"),
        (2, "2025-07-01T00:00:00Z"),
        (3, "2026-03-01T00:00:00Z"),
    ] {
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','x',$3,$4::timestamptz)")
            .bind(&id)
            .bind(common::author(n))
            .bind(format!("/pub/app/tags/{n}"))
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO osm_rename(osm_type,osm_id,changed_at,old_name,old_category,new_name) VALUES('node',42,'2026-01-01T00:00:00Z','Rössli','amenity/restaurant','Engel')")
        .execute(&pool)
        .await
        .unwrap();
    // Read as the OSM route reads it, from the card.
    let resolver = pinakes_core::resolver::osm::OsmResolver::new(
        pool.clone(),
        pinakes_core::config::OsmConfig {
            nominatim_url: "http://127.0.0.1:9".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_catalogue(true);
    let doc = pinakes_core::resolver::Resolver::resolve(&resolver, uri)
        .await
        .unwrap();
    pinakes_core::store::save_document(&pool, &id, 0, &doc)
        .await
        .unwrap();
    let app = app(&pool, Config::default());
    let card = common::get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(card["name"], "Engel");
    assert_eq!(
        card["replaced"],
        serde_json::json!([{"name": "Rössli", "category": "amenity/restaurant", "until": "2026-01-01T00:00:00Z", "references": 2}])
    );
    let app = self::app(&pool, off());
    let card = common::get(&app, &format!("/v1/resources/{id}")).await;
    assert!(card.get("replaced").is_none(), "{card}");
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
fn tempdir() -> std::path::PathBuf {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "pinakes-osm-dump-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_deleted_place_is_maybe_its_namesake_of_its_category_nearby(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let old = osm(
        "node",
        60,
        &[("amenity", "restaurant"), ("name", "Löwen")],
        VADUZ,
    );
    import(&pool, &[old]).await;
    let uri = "https://www.openstreetmap.org/node/60";
    let id = pinakes_core::store::ensure_resource(&pool, uri)
        .await
        .unwrap();
    let resolver = pinakes_core::resolver::osm::OsmResolver::new(
        pool.clone(),
        pinakes_core::config::OsmConfig {
            nominatim_url: "http://127.0.0.1:9".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_catalogue(true);
    let doc = resolver.resolve(uri).await.unwrap();
    pinakes_core::store::save_document(&pool, &id, 0, &doc)
        .await
        .unwrap();
    // Deleted upstream, drawn again 20 m away as a restaurant, and a café of the name beside it.
    store::remove(&pool, Catalogue::Osm, &["node/60".to_owned()], None)
        .await
        .unwrap();
    store::upsert(
        &pool,
        Catalogue::Osm,
        1,
        &[
            osm(
                "way",
                61,
                &[("amenity", "restaurant"), ("name", "Löwen")],
                (VADUZ.0 + 0.00018, VADUZ.1),
            ),
            osm(
                "node",
                62,
                &[("amenity", "cafe"), ("name", "Löwen")],
                (VADUZ.0 + 0.0001, VADUZ.1),
            ),
        ],
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE resource SET status='missing' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let app = app(&pool, Config::default());
    let card = common::get(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(
        common::maybe_same_ids(&card),
        [id_of("https://www.openstreetmap.org/way/61").as_str()],
        "{card}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_link_read_missing_after_its_successor_was_found_becomes_its_alias(pool: PgPool) {
    sqlx::query("INSERT INTO catalogue_state(catalogue,phase) VALUES('osm','idle')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO osm_successor(old_type,old_id,new_type,new_id,basis) VALUES('node',70,'node',71,'wikidata')").execute(&pool).await.unwrap();
    let id = pinakes_core::store::ensure_resource(&pool, "https://www.openstreetmap.org/node/70")
        .await
        .unwrap();
    pinakes_core::catalogue::osm::follow(&pool, "node/70", "node/71")
        .await
        .unwrap();
    let root: String = sqlx::query_scalar(
        "SELECT r.uri FROM resource_alias a JOIN resource r ON r.id=a.root_id WHERE a.alias_id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(root, "https://www.openstreetmap.org/node/71");
}

/// The OSM route reading every object as deleted.
struct Gone;
#[async_trait::async_trait]
impl pinakes_core::resolver::Resolver for Gone {
    fn refresh_after(&self, _: &ResourceMeta) -> std::time::Duration {
        std::time::Duration::from_secs(86_400)
    }
    fn name(&self) -> &'static str {
        "osm"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        let id = pinakes_core::resolver::osm::parse_id(uri).unwrap();
        let mut doc = ResourceMeta::new(&id.uri(), "osm", 86_400);
        doc.set_type("Place");
        doc.osm_id = Some(id.key());
        doc.attribution = Some(pinakes_core::resolver::osm::ATTRIBUTION.into());
        doc.status = pinakes_core::model::Status::Missing;
        Ok(doc)
    }
}

/// A link read missing follows its successor only while the configuration has the OSM catalogue
/// on: with it off, a state row still `idle` (as a one-shot `catalogue import osm` with OSM off
/// leaves it) follows none.
#[sqlx::test(migrations = "../../migrations")]
async fn a_missing_link_follows_its_successor_only_with_the_catalogue_on(pool: PgPool) {
    sqlx::query("INSERT INTO catalogue_state(catalogue,phase) VALUES('osm','idle')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO osm_successor(old_type,old_id,new_type,new_id,basis) VALUES('node',70,'node',71,'wikidata'),('node',72,'node',73,'changeset')").execute(&pool).await.unwrap();
    for (config, old, followed) in [(off(), 70, false), (Config::default(), 72, true)] {
        let id = pinakes_core::store::ensure_resource(
            &pool,
            &format!("https://www.openstreetmap.org/node/{old}"),
        )
        .await
        .unwrap();
        let job = pinakes_core::queue::claim(&pool, "osm", &config.queue)
            .await
            .unwrap()
            .expect("the link is due");
        assert_eq!(job.id, id);
        pinakes_core::queue::run_job(&pool, &Gone, &config.queue, &job)
            .await
            .unwrap();
        let aliased: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1)")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(aliased, followed, "node/{old}");
    }
}

/// A deleted place an operator placed in a curated group hands its place to its successor:
/// following the successor drops the old link's group unit, and the successor's next group run
/// puts it in the curated group instead.
#[sqlx::test(migrations = "../../migrations")]
async fn following_a_successor_keeps_an_operators_curated_group(pool: PgPool) {
    let cfg = pinakes_core::groups::GroupConfig::default();
    sqlx::query("INSERT INTO catalogue_state(catalogue,phase) VALUES('osm','idle')")
        .execute(&pool)
        .await
        .unwrap();
    // An OSM link's card as the OSM route reads it, a web page's as the web route does.
    let place = |uri: &str, name: &str| match pinakes_core::resolver::osm::parse_id(uri) {
        Some(id) => {
            let mut doc = ResourceMeta::new(&id.uri(), "osm", 86400);
            doc.set_type("Restaurant");
            doc.osm_id = Some(id.key());
            doc.attribution = Some(pinakes_core::resolver::osm::ATTRIBUTION.into());
            doc.name = Some(name.into());
            doc.location = Some(Point {
                lat: VADUZ.0,
                lon: VADUZ.1,
            });
            doc
        }
        None => {
            let mut doc = ResourceMeta::new(uri, "web", 86400);
            doc.set_type("Restaurant");
            doc.name = Some(name.into());
            doc
        }
    };
    let mut ids = vec![];
    for (uri, name) in [
        ("https://www.openstreetmap.org/node/70", "Löwen"),
        ("https://loewen.example/", "Gasthaus Löwen"),
    ] {
        let id = pinakes_core::store::ensure_resource(&pool, uri)
            .await
            .unwrap();
        pinakes_core::store::save_document(&pool, &id, 0, &place(uri, name))
            .await
            .unwrap();
        ids.push(id);
    }
    pinakes_core::groups::drain(&pool, &cfg, 100).await.unwrap();
    let group = "c".repeat(32);
    pinakes_core::groups::curate(
        &pool,
        &group,
        serde_json::from_value(serde_json::json!({"members": ids.iter().map(|id| serde_json::json!({"resource_id": id})).collect::<Vec<_>>()})).unwrap(),
        &cfg,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO osm_successor(old_type,old_id,new_type,new_id,basis) VALUES('node',70,'node',71,'wikidata')").execute(&pool).await.unwrap();
    pinakes_core::catalogue::osm::follow(&pool, "node/70", "node/71")
        .await
        .unwrap();
    let successor = id_of("https://www.openstreetmap.org/node/71");
    let uri = "https://www.openstreetmap.org/node/71";
    pinakes_core::store::save_document(&pool, &successor, 0, &place(uri, "Löwen"))
        .await
        .unwrap();
    pinakes_core::groups::drain(&pool, &cfg, 100).await.unwrap();
    let placed: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT u.resource_id,u.group_id,o.group_id FROM group_unit u LEFT JOIN group_override o USING(unit_id) ORDER BY u.resource_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut want = vec![
        (ids[1].clone(), Some(group.clone()), Some(group.clone())),
        (successor.clone(), Some(group.clone()), Some(group.clone())),
    ];
    want.sort();
    assert_eq!(placed, want);
    let curated: bool = sqlx::query_scalar("SELECT curated FROM \"group\" WHERE id=$1")
        .bind(&group)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(curated);
}

/// The per-cell totals (zoom 12 and below) and the card-by-card count (above) agree: a shared
/// OSM link with a card counts once on both, a card whose link is only stored (not referenced)
/// counts on both, and a point on the last edge is in a cell of its own size.
#[sqlx::test(migrations = "../../migrations")]
async fn cluster_totals_agree_across_zoom_12_and_13(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    let atm = osm(
        "node",
        1,
        &[("amenity", "atm"), ("operator", "LGT")],
        (47.1412, 9.5217),
    );
    let corner = osm(
        "node",
        3,
        &[("place", "village"), ("name", "Edge")],
        (90.0, 180.0),
    );
    import(&pool, &[engel(), atm, corner]).await;
    // Engel's link shared (its card read into `osm_place`), the ATM's only stored.
    let config = pinakes_core::config::OsmConfig {
        nominatim_url: "http://127.0.0.1:9".into(),
        ..Default::default()
    };
    let resolver = pinakes_core::resolver::osm::OsmResolver::new(pool.clone(), config)
        .unwrap()
        .with_catalogue(true);
    let doc = resolver
        .resolve("https://www.openstreetmap.org/node/42")
        .await
        .unwrap();
    let shared =
        pinakes_core::store::ensure_resource(&pool, "https://www.openstreetmap.org/node/42")
            .await
            .unwrap();
    pinakes_core::store::save_document(&pool, &shared, 0, &doc)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','good','/pub/app/tags/'||$1,now())")
        .bind(&shared)
        .bind(common::author(1))
        .execute(&pool)
        .await
        .unwrap();
    pinakes_core::store::ensure_resource(&pool, "https://www.openstreetmap.org/node/1")
        .await
        .unwrap();
    let app = app(&pool, Config::default());
    let view = "bbox=9.5,47.1,9.55,47.15";
    let at12 = clusters(&app, &format!("/v1/map/clusters?{view}&zoom=12")).await;
    let at13 = clusters(&app, &format!("/v1/map/clusters?{view}&zoom=13")).await;
    assert_eq!(
        (at12["total"].clone(), at13["total"].clone()),
        (2.into(), 2.into()),
        "{at12} {at13}"
    );
    // The edge point: in the world view at zoom 0 and 13's last cells, each of non-zero size.
    for zoom in [0, 13] {
        let view = if zoom == 0 {
            "bbox=-180,-90,180,90".to_owned()
        } else {
            "bbox=179.99,89.99,180,90".to_owned()
        };
        let all = clusters(&app, &format!("/v1/map/clusters?{view}&zoom={zoom}")).await;
        let edge = all["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["point"]["lat"].as_f64() == Some(90.0))
            .unwrap_or_else(|| panic!("{all}"));
        let b: Vec<f64> = edge["bbox"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        assert!(
            b[2] > b[0] && b[3] > b[1] && b[2] == 180.0 && b[3] == 90.0,
            "{zoom}: {b:?}"
        );
    }
}

/// The per-cell totals follow every write: an import, a move across cells, a type change, a
/// removal and a truncate leave each level equal to the cards' own count.
#[sqlx::test(migrations = "../../migrations")]
async fn the_cell_totals_follow_every_write(pool: PgPool) {
    let check = |pool: PgPool, when: &'static str| async move {
        let cells = "SELECT level::int,cx,cy,catalogue,type,cards,round(sum_lon::numeric,6)::text,round(sum_lat::numeric,6)::text FROM catalogue_map_cell WHERE cards<>0 ORDER BY 1,2,3,4,5";
        let wanted = "SELECT l.level,floor((ST_X(point)+180)/(360.0/(4<<l.level)))::int,floor((ST_Y(point)+90)/(360.0/(4<<l.level)))::int,catalogue,type,count(*),round(sum(ST_X(point))::numeric,6)::text,round(sum(ST_Y(point))::numeric,6)::text FROM catalogue_card CROSS JOIN (VALUES (4),(6),(8),(10),(12)) l(level) WHERE point IS NOT NULL GROUP BY 1,2,3,4,5 ORDER BY 1,2,3,4,5";
        type Row = (i32, i32, i32, String, String, i64, String, String);
        let got: Vec<Row> = sqlx::query_as(cells).fetch_all(&pool).await.unwrap();
        let want: Vec<Row> = sqlx::query_as(wanted).fetch_all(&pool).await.unwrap();
        assert_eq!(got, want, "{when}");
        got.len()
    };
    let atm = osm("node", 1, &[("amenity", "atm")], (47.1412, 9.5217));
    let far = osm(
        "node",
        2,
        &[("place", "village"), ("name", "Triesen")],
        (-33.9, 151.2),
    );
    import(&pool, &[engel(), atm.clone(), far]).await;
    assert_eq!(check(pool.clone(), "import").await, 15);
    // Moved into another cell at every level.
    let mut moved = atm.clone();
    moved.point = Some(Point {
        lat: 10.0,
        lon: -70.0,
    });
    store::upsert(&pool, Catalogue::Osm, 1, &[moved], None)
        .await
        .unwrap();
    check(pool.clone(), "move").await;
    sqlx::query("UPDATE catalogue_card SET type='Bar' WHERE source_id='node/42'")
        .execute(&pool)
        .await
        .unwrap();
    check(pool.clone(), "type").await;
    sqlx::query("UPDATE catalogue_card SET point=NULL WHERE source_id='node/2'")
        .execute(&pool)
        .await
        .unwrap();
    check(pool.clone(), "point gone").await;
    sqlx::query("DELETE FROM catalogue_card WHERE source_id='node/1'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(check(pool.clone(), "delete").await, 5);
    sqlx::query("TRUNCATE catalogue_card CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogue_map_cell")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// Each view cell counts what lies in it, at every zoom: the per-cell totals (zoom 12 and below,
/// whichever level a zoom reads) and the card-by-card count (above) give each cell the count of
/// the cards inside it.
#[sqlx::test(migrations = "../../migrations")]
async fn every_zoom_counts_each_cell_as_its_cards(pool: PgPool) {
    let mut cards = vec![];
    for i in 0..60i64 {
        let (lat, lon) = (
            47.0 + (i * 37 % 61) as f64 * 0.0065,
            9.4 + (i * 53 % 59) as f64 * 0.0084,
        );
        cards.push(osm(
            "node",
            100 + i,
            &[
                ("amenity", if i % 2 == 0 { "atm" } else { "cafe" }),
                ("name", "X"),
            ],
            (lat, lon),
        ));
    }
    import(&pool, &cards).await;
    let app = app(&pool, Config::default());
    for zoom in 0..=13u32 {
        let cell = 360.0 / f64::from(4u32 << zoom);
        let mut want = std::collections::BTreeMap::new();
        for c in &cards {
            let p = c.point.as_ref().unwrap();
            let key = (
                ((p.lon + 180.0) / cell).floor() as i64,
                ((p.lat + 90.0) / cell).floor() as i64,
            );
            *want.entry(key).or_insert(0i64) += 1;
        }
        let bbox = if zoom < 3 {
            "-180,-90,180,90".to_owned()
        } else {
            "9.39,46.99,9.91,47.41".to_owned()
        };
        let got = clusters(&app, &format!("/v1/map/clusters?bbox={bbox}&zoom={zoom}")).await;
        let got: std::collections::BTreeMap<(i64, i64), i64> = got["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let b = &c["bbox"];
                let x = ((b[0].as_f64().unwrap() + 180.0) / cell + 0.5).floor() as i64;
                let y = ((b[1].as_f64().unwrap() + 90.0) / cell + 0.5).floor() as i64;
                ((x, y), c["count"].as_i64().unwrap())
            })
            .collect();
        assert_eq!(got, want, "zoom {zoom}");
    }
}

/// The nearby map links a place checks are the referenced ones first: links nobody referenced,
/// however many and however near, do not crowd out a referenced one.
#[sqlx::test(migrations = "../../migrations")]
async fn referenced_map_links_are_checked_first(pool: PgPool) {
    for i in 0..70 {
        let id = map_link(
            &pool,
            &format!("geo:47.141,9.5215?q=47.141,9.5215(Engel+{i})"),
            "geo",
            "Engel",
            VADUZ,
        )
        .await;
        sqlx::query("DELETE FROM reference WHERE resource_id=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let referenced = map_link(
        &pool,
        "geo:47.1412,9.5215?q=47.1412,9.5215(Engel)",
        "geo",
        "Engel",
        (VADUZ.0 + 0.0002, VADUZ.1),
    )
    .await;
    let links = pinakes_core::catalogue::osm::rules::links_near(&pool, VADUZ.0, VADUZ.1)
        .await
        .unwrap();
    assert_eq!(
        links.first().map(|l| l.id.as_str()),
        Some(referenced.as_str())
    );
    assert!(links.first().unwrap().referenced);
}

/// Two transactions writing points of one catalogue into the same cells in opposite orders do
/// not deadlock: the cells are touched one transaction at a time.
#[sqlx::test(migrations = "../../migrations")]
async fn two_writers_do_not_deadlock_on_the_cells(pool: PgPool) {
    let insert = |catalogue: &'static str, id: &'static str, lon: f64| {
        format!("INSERT INTO catalogue_card(catalogue,source,source_id,uri,type,name,point,seen_generation,content_hash) VALUES('{catalogue}','{catalogue}','{id}','https://example.org/{catalogue}/{id}','Place','X',ST_SetSRID(ST_MakePoint({lon},47.1),4326),1,decode(md5('{id}')||md5('{catalogue}'),'hex'))")
    };
    let mut first = pool.begin().await.unwrap();
    sqlx::query(&insert("osm", "node/1", 9.5))
        .execute(&mut *first)
        .await
        .unwrap();
    let pool2 = pool.clone();
    let second = tokio::spawn(async move {
        let mut tx = pool2.begin().await.unwrap();
        sqlx::query(&insert("osm", "node/3", -70.0))
            .execute(&mut *tx)
            .await?;
        sqlx::query(&insert("osm", "node/4", 9.5))
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    });
    // Go on once the second transaction waits on a lock (the cells' lock, or a cell row the
    // first holds): its first statement has run.
    let mut seen = false;
    for _ in 0..1000 {
        let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'")
            .fetch_one(&pool)
            .await
            .unwrap();
        if waiting > 0 {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(seen, "the second transaction never waited on a lock");
    sqlx::query(&insert("osm", "node/2", -70.0))
        .execute(&mut *first)
        .await
        .unwrap();
    first.commit().await.unwrap();
    second.await.unwrap().unwrap();
    let cells: i64 =
        sqlx::query_scalar("SELECT sum(cards)::bigint FROM catalogue_map_cell WHERE level=4")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cells, 4);
}

/// A filter the shared link fails does not hide its card: with `tags=restaurant`, a restaurant's
/// card is counted (as search lists it) though its shared OSM link carries only an unrelated tag;
/// unfiltered, the two count once. Card by card (zoom 13) and with `tags` at zoom 10 alike.
#[sqlx::test(migrations = "../../migrations")]
async fn a_link_failing_the_filter_leaves_its_card_counted(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    import(&pool, &[engel()]).await;
    let config = pinakes_core::config::OsmConfig {
        nominatim_url: "http://127.0.0.1:9".into(),
        ..Default::default()
    };
    let resolver = pinakes_core::resolver::osm::OsmResolver::new(pool.clone(), config)
        .unwrap()
        .with_catalogue(true);
    let uri = "https://www.openstreetmap.org/node/42";
    let doc = resolver.resolve(uri).await.unwrap();
    let shared = pinakes_core::store::ensure_resource(&pool, uri)
        .await
        .unwrap();
    pinakes_core::store::save_document(&pool, &shared, 0, &doc)
        .await
        .unwrap();
    // The link's one tag is unrelated to restaurants.
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','good','/pub/app/tags/'||$1,now())")
        .bind(&shared)
        .bind(common::author(1))
        .execute(&pool)
        .await
        .unwrap();
    let app = app(&pool, Config::default());
    let view = "bbox=9.5,47.13,9.54,47.15";
    for zoom in [10, 13] {
        let all = clusters(&app, &format!("/v1/map/clusters?{view}&zoom={zoom}")).await;
        assert_eq!(all["total"], 1, "zoom {zoom}: {all}");
        let restaurants = clusters(
            &app,
            &format!("/v1/map/clusters?{view}&zoom={zoom}&tags=restaurant"),
        )
        .await;
        assert_eq!(restaurants["total"], 1, "zoom {zoom}: {restaurants}");
        let good = clusters(
            &app,
            &format!("/v1/map/clusters?{view}&zoom={zoom}&tags=good"),
        )
        .await;
        assert_eq!(good["total"], 1, "zoom {zoom}: {good}");
    }
    // Search lists the card for the label.
    let found = common::get(&app, &format!("/v1/search?{view}&tags=restaurant")).await;
    assert_eq!(found["items"].as_array().map(Vec::len), Some(1), "{found}");
}

/// A map link joined exactly to the place (here both carry the place's Wikidata key, as a
/// Google link whose CID Wikidata lists does) lists it once, as the exact member: no "maybe" beside
/// it, on either side.
#[sqlx::test(migrations = "../../migrations")]
async fn an_exact_join_leaves_no_maybe_beside_it(pool: PgPool) {
    use pinakes_core::resolver::Resolver;
    import(&pool, &[engel()]).await;
    let config = pinakes_core::config::OsmConfig {
        nominatim_url: "http://127.0.0.1:9".into(),
        ..Default::default()
    };
    let resolver = pinakes_core::resolver::osm::OsmResolver::new(pool.clone(), config)
        .unwrap()
        .with_catalogue(true);
    let uri = "https://www.openstreetmap.org/node/42";
    let doc = resolver.resolve(uri).await.unwrap();
    let place = pinakes_core::store::ensure_resource(&pool, uri)
        .await
        .unwrap();
    pinakes_core::store::save_document(&pool, &place, 0, &doc)
        .await
        .unwrap();
    let link = map_link(
        &pool,
        "https://www.google.com/maps/place/Engel/data=!4m2!3m1!1s0x479b31f2c3b8f8b1:0x1234567890abcdef",
        "google_maps",
        "Engel",
        (VADUZ.0 + 0.0001, VADUZ.1),
    )
    .await;
    let groups = Config::default().groups;
    for id in [&place, &link] {
        sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,'wikidata:Q123456','test','test',now()+interval '1 day')")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    for id in [&place, &link] {
        pinakes_core::groups::reconcile(&pool, id, &groups).await;
    }
    let app = app(&pool, Config::default());
    for (me, other) in [(&link, &place), (&place, &link)] {
        let card = common::get(&app, &format!("/v1/resources/{me}")).await;
        let also: Vec<&Value> = card["also_on"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["id"] == other.as_str())
            .collect();
        assert_eq!(also.len(), 1, "{card}");
        assert_ne!(also[0]["kind"], "maybe", "{card}");
        assert!(
            card["maybe_same"]
                .as_array()
                .is_none_or(|m| m.iter().all(|m| m["id"] != other.as_str())),
            "{card}"
        );
    }
}

/// An imported place's official website joins the place's group (it takes the place's `osm:`
/// key, which the place's shared link carries) when exactly one imported place names it; a
/// chain's website, named by several branches, joins none.
#[sqlx::test(migrations = "../../migrations")]
async fn a_places_website_joins_its_group_unless_a_chain_shares_it(pool: PgPool) {
    use pinakes_core::catalogue::join;
    import(&pool, &[engel()]).await;
    let mut config = Config::default();
    config.queue.catalogues = join::joinable(&config.catalogues);
    let site = common::save_card(&pool, "https://www.hotel-engel.li/", "WebSite").await;
    join::join(&pool, &config.queue, &site, None).await.unwrap();
    let keys = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT key FROM resource_key WHERE resource_id=$1 AND key LIKE 'osm:%'",
            )
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(keys(site.clone()).await, ["osm:node/42"]);
    // A second branch with the same website: the site belongs to neither.
    let mut branch = osm(
        "node",
        43,
        &[
            ("amenity", "restaurant"),
            ("name", "Engel Schaan"),
            ("website", "https://www.hotel-engel.li/"),
        ],
        (47.165, 9.51),
    );
    branch.fame = 0.1;
    store::upsert(&pool, Catalogue::Osm, 1, &[branch], None)
        .await
        .unwrap();
    let chain = common::save_card(&pool, "https://www.hotel-engel.li/", "WebSite").await;
    join::join(&pool, &config.queue, &chain, None)
        .await
        .unwrap();
    assert!(keys(chain).await.is_empty());
    // Off in the configuration: no OSM card is read for a website, though the state row still
    // says `idle` (as a one-shot `catalogue import osm` with OSM off leaves it).
    let config = off();
    sqlx::query("DELETE FROM catalogue_card WHERE source_id='node/43'")
        .execute(&pool)
        .await
        .unwrap();
    let off = common::save_card(&pool, "https://www.hotel-engel.li/", "WebSite").await;
    join::join(&pool, &config.queue, &off, None).await.unwrap();
    assert!(keys(off.clone()).await.is_empty());
    // A website that joined while OSM was on loses its key at the next start with OSM off.
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,'osm:node/42','catalogue','catalogue',now()+interval '30 days')")
        .bind(&off)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET next_attempt_at=now()+interval '30 days' WHERE id=$1")
        .bind(&off)
        .execute(&pool)
        .await
        .unwrap();
    let phase: String =
        sqlx::query_scalar("SELECT phase FROM catalogue_state WHERE catalogue='osm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(phase, "idle");
    join::sweep(&pool, &config.queue.catalogues, config.queue.osm_catalogue)
        .await
        .unwrap();
    assert!(keys(off.clone()).await.is_empty());
    let due: bool = sqlx::query_scalar(
        "SELECT next_attempt_at<now()+interval '2 days' FROM resource WHERE id=$1",
    )
    .bind(&off)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(due);
}

/// Dropping the OSM catalogue releases the aliases its successors made: the deleted object's
/// link answers for itself again (a successor forgotten can't be unfollowed later).
#[sqlx::test(migrations = "../../migrations")]
async fn dropping_the_catalogue_releases_the_successors_aliases(pool: PgPool) {
    sqlx::query("INSERT INTO catalogue_state(catalogue,phase) VALUES('osm','idle')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO osm_successor(old_type,old_id,new_type,new_id,basis) VALUES('node',70,'node',71,'wikidata')").execute(&pool).await.unwrap();
    let id = pinakes_core::store::ensure_resource(&pool, "https://www.openstreetmap.org/node/70")
        .await
        .unwrap();
    pinakes_core::catalogue::osm::follow(&pool, "node/70", "node/71")
        .await
        .unwrap();
    let aliased = |pool: PgPool, id: String| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_alias WHERE alias_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(aliased(pool.clone(), id.clone()).await, 1);
    store::drop_catalogue(&pool, Catalogue::Osm, |_| {})
        .await
        .unwrap();
    assert_eq!(aliased(pool.clone(), id.clone()).await, 0);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM osm_successor")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// A cell counts what its search lists: a job posting past its end is left out of both.
#[sqlx::test(migrations = "../../migrations")]
async fn a_closed_job_posting_counts_in_no_cell(pool: PgPool) {
    // A failure here shows the server's own error line.
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();
    for (n, days) in [(1usize, -1i64), (2, 30)] {
        let uri = format!("https://jobs.example/{n}");
        let id = pinakes_core::store::ensure_resource(&pool, &uri)
            .await
            .unwrap();
        let mut doc = ResourceMeta::new(&uri, "jsonld", 86400);
        doc.set_type("JobPosting");
        doc.name = Some(format!("Job {n}"));
        doc.location = Some(Point {
            lat: 47.14,
            lon: 9.52,
        });
        doc.end = Some(chrono::Utc::now() + chrono::Duration::days(days));
        pinakes_core::store::save_document(&pool, &id, 0, &doc)
            .await
            .unwrap();
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','jobs','/pub/app/tags/'||$1,now())")
            .bind(&id)
            .bind(common::author(n))
            .execute(&pool)
            .await
            .unwrap();
    }
    let app = app(&pool, Config::default());
    let all = clusters(&app, "/v1/map/clusters?bbox=9.5,47.13,9.54,47.15&zoom=13").await;
    assert_eq!(all["total"], 1, "{all}");
    let search = all["clusters"][0]["search"].as_str().unwrap();
    let page = common::get(&app, search).await;
    assert_eq!(page["items"].as_array().map(Vec::len), Some(1), "{page}");
}

/// A map link joined exactly by a place key (a Google link whose CID Wikidata lists carries the
/// item's key) is no "maybe" of the namesake beside it, with the place unshared too: neither
/// lists the other.
#[sqlx::test(migrations = "../../migrations")]
async fn an_exactly_placed_link_is_no_maybe_of_an_unshared_place(pool: PgPool) {
    import(&pool, &[engel()]).await;
    let link = map_link(
        &pool,
        "https://www.google.com/maps/place/Engel/@47.1411,9.5215,17z",
        "google_maps",
        "Engel",
        (VADUZ.0 + 0.0001, VADUZ.1),
    )
    .await;
    let place = id_of("https://www.openstreetmap.org/node/42");
    let app = app(&pool, Config::default());
    // Before the key: maybe the same, both ways.
    let card = common::get(&app, &format!("/v1/resources/{link}")).await;
    assert_eq!(common::maybe_same_ids(&card), [place.as_str()], "{card}");
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,'wikidata:Q123456','test','test',now()+interval '1 day')")
        .bind(&link)
        .execute(&pool)
        .await
        .unwrap();
    let none = |card: &Value, other: &str| {
        card["maybe_same"]
            .as_array()
            .is_none_or(|m| m.iter().all(|m| m["id"] != other))
            && card["also_on"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| !(a["id"] == other && a["kind"] == "maybe"))
    };
    let card = common::get(&app, &format!("/v1/resources/{link}")).await;
    assert!(none(&card, &place), "{card}");
    let osm = common::get(&app, &format!("/v1/resources/{place}")).await;
    assert!(none(&osm, &link), "{osm}");
}

/// The start sweep with OSM off takes only the `osm:` keys websites took from places: a resource
/// built from a card that holds an `osm:` key itself (a Wikidata item's OSM object) keeps it.
/// With OSM on it takes none.
#[sqlx::test(migrations = "../../migrations")]
async fn the_website_sweep_spares_keys_a_card_holds(pool: PgPool) {
    use pinakes_core::catalogue::join;
    let card: i64 = sqlx::query_scalar("INSERT INTO catalogue_card(catalogue,source,source_id,uri,type,name,keys,seen_generation,content_hash) \
        VALUES('wikidata','wikidata','Q1','https://www.wikidata.org/wiki/Q1','Place','Engel',ARRAY['wikidata:Q1','osm:node/42'],1,decode(md5('q1')||md5('q2'),'hex')) RETURNING id")
        .fetch_one(&pool)
        .await
        .unwrap();
    let item = pinakes_core::store::ensure_resource(&pool, "https://www.wikidata.org/wiki/Q1")
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET catalogue_card=$2 WHERE id=$1")
        .bind(&item)
        .bind(card)
        .execute(&pool)
        .await
        .unwrap();
    let site = common::save_card(&pool, "https://www.hotel-engel.li/", "WebSite").await;
    for (id, key) in [(&item, "osm:node/42"), (&site, "osm:node/43")] {
        sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,$2,'catalogue','catalogue',now()+interval '30 days')")
            .bind(id)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
    }
    let keys = || {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, String)>("SELECT resource_id,key FROM resource_key WHERE source='catalogue' AND key LIKE 'osm:%' ORDER BY key")
                .fetch_all(&pool)
                .await
                .unwrap()
        }
    };
    sqlx::query("INSERT INTO catalogue_state(catalogue,phase) VALUES('osm','idle')")
        .execute(&pool)
        .await
        .unwrap();
    // On in the configuration: nothing is taken.
    let on = Config::default();
    assert!(on.queue.osm_catalogue);
    join::sweep(&pool, &on.queue.catalogues, on.queue.osm_catalogue)
        .await
        .unwrap();
    assert_eq!(keys().await.len(), 2);
    // Off in the configuration (the state row still `idle`): the website's key goes, the item's
    // own stays.
    let off = off();
    join::sweep(&pool, &off.queue.catalogues, off.queue.osm_catalogue)
        .await
        .unwrap();
    assert_eq!(keys().await, [(item.clone(), "osm:node/42".to_owned())]);
}
