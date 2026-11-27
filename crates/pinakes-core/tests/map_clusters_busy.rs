//! `GET /v1/map/clusters` under load: with every turn taken the answer is a 503 at once, and a
//! count that outlasts `limits.map_cluster_timeout_ms` is a 503 too. A test binary of its own:
//! the turns are one process-wide semaphore.
use pinakes_core::{
    api::{self, AppState},
    catalogue::{osm::place, store, Catalogue},
    config::Config,
    model::Point,
};
use sqlx::PgPool;
mod common;

#[sqlx::test(migrations = "../../migrations")]
async fn a_busy_or_slow_count_is_a_503(pool: PgPool) {
    let tags = place::tags([("amenity", "cafe"), ("name", "Engel")]);
    let geometry = place::Geometry {
        point: Some(Point {
            lat: 47.141,
            lon: 9.5215,
        }),
        bbox: None,
    };
    let card = place::read("node", 1, &tags, geometry).unwrap();
    store::upsert(&pool, Catalogue::Osm, 1, &[card], None)
        .await
        .unwrap();
    let mut config = Config::default();
    config.search.map_cluster_timeout_ms = 200;
    let app = api::router(AppState::new(pool.clone(), config).unwrap());
    let path = "/v1/map/clusters?bbox=9.4,47.0,9.7,47.3&zoom=10";
    assert_eq!(common::get(&app, path).await["total"], 1);
    // One client holding its two turns: its next count is a 503, another client's is answered.
    let client: std::net::IpAddr = "127.0.0.9".parse().unwrap();
    let held = (
        api::map_turn(Some(client)).unwrap(),
        api::map_turn(Some(client)).unwrap(),
    );
    let share = pinakes_core::telemetry::map_clusters_busy_count("share");
    let (status, body) = common::get_from(&app, "127.0.0.9:1", path).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(
        pinakes_core::telemetry::map_clusters_busy_count("share"),
        share + 1
    );
    let (status, _) = common::get_from(&app, "127.0.0.10:1", path).await;
    assert_eq!(status, 200);
    drop(held);
    // Every turn taken.
    let turns = api::MAP_TURNS.acquire_many(4).await.unwrap();
    let every = pinakes_core::telemetry::map_clusters_busy_count("turns");
    let (status, _) = common::get_from(&app, "127.0.0.2:1", path).await;
    assert_eq!(status, 503);
    assert_eq!(
        pinakes_core::telemetry::map_clusters_busy_count("turns"),
        every + 1
    );
    drop(turns);
    // The totals locked by another writer for longer than the limit.
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE catalogue_map_cell IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *writer)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let slow = pinakes_core::telemetry::map_clusters_busy_count("timeout");
    let (status, _) = common::get_from(&app, "127.0.0.2:1", path).await;
    assert_eq!(status, 503);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        pinakes_core::telemetry::map_clusters_busy_count("timeout"),
        slow + 1
    );
    writer.rollback().await.unwrap();
    assert_eq!(common::get(&app, path).await["total"], 1);
}
