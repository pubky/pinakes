use axum::{
    body::Body,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, OsmConfig, QueueConfig},
    model::{ResourceMeta, Status},
    queue,
    resolver::{
        geo,
        osm::{self, OsmResolver},
        Resolver,
    },
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Mock {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    paths: Arc<Mutex<Vec<String>>>,
    limited: Arc<AtomicBool>,
    omit: Arc<AtomicBool>,
}
async fn nominatim(
    State(state): State<Mock>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    assert_eq!(
        headers["user-agent"],
        "pinakes/0.1.0 (+https://github.com/pubky/pinakes)"
    );
    assert!(headers["accept-encoding"]
        .to_str()
        .unwrap()
        .contains("gzip"));
    for (k, v) in [
        ("format", "jsonv2"),
        ("addressdetails", "1"),
        ("extratags", "1"),
        ("namedetails", "1"),
    ] {
        assert_eq!(q[k], v);
    }
    let ids = q["osm_ids"]
        .split(',')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(ids.len() <= 50);
    state.calls.lock().await.push(ids.clone());
    if state.limited.load(Ordering::SeqCst) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "1")],
            "limited",
        )
            .into_response();
    }
    if state.omit.load(Ordering::SeqCst) {
        return Json(json!([])).into_response();
    }
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/nominatim.json")).unwrap();
    Json(
        fixtures
            .into_iter()
            .filter(|v| {
                ids.contains(
                    &osm::parse_id(&format!(
                        "https://www.openstreetmap.org/{}/{}",
                        v["osm_type"].as_str().unwrap(),
                        v["osm_id"]
                    ))
                    .unwrap()
                    .lookup_id(),
                )
            })
            .collect::<Vec<_>>(),
    )
    .into_response()
}
async fn fallback(State(state): State<Mock>, uri: axum::http::Uri) -> Response {
    state.paths.lock().await.push(uri.path().into());
    if uri.path().contains("404") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if uri.path().contains("410") {
        return StatusCode::GONE.into_response();
    }
    let kind = uri.path().split('/').nth(3).unwrap();
    let id: i64 = uri
        .path()
        .split('/')
        .nth(4)
        .unwrap()
        .trim_end_matches(".json")
        .parse()
        .unwrap();
    Json(json!({"elements":[{"type":kind,"id":id,"lat":46.0,"lon":8.0,"tags":{"name":"Synthetic café","amenity":"cafe","currency:XBT":"yes","secret":"drop"}},{"type":"node","id":901,"lat":45.0,"lon":7.0},{"type":"node","id":902,"lat":47.0,"lon":9.0}]})).into_response()
}
async fn mock(pool: &PgPool) -> (Mock, OsmResolver, tokio::task::JoinHandle<()>) {
    let state = Mock::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/lookup", get(nominatim))
        .fallback(get(fallback))
        .with_state(state.clone());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = OsmConfig {
        nominatim_url: format!("http://{address}"),
        api_url: format!("http://{address}"),
        requests_per_second: 1000.0,
        ..Default::default()
    };
    (
        state,
        OsmResolver::new(pool.clone(), config).unwrap(),
        handle,
    )
}
async fn call(app: &Router, path: &str, etag: Option<&str>) -> (u16, HeaderMap, Value) {
    let mut request = Request::builder().uri(path);
    if let Some(etag) = etag {
        request = request.header("if-none-match", etag);
    }
    let mut request = request.body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, headers, value)
}
async fn reference(pool: &PgPool, id: &str, label: &str) {
    sqlx::query("INSERT INTO reference(resource_id,kind,author,label,object_path,created_at) VALUES($1,'tag',$2,$3,$1,now())")
    .bind(id).bind("y".repeat(52)).bind(label).execute(pool).await.unwrap();
}
#[sqlx::test(migrations = "../../migrations")]
async fn fifty_one_jobs_two_calls_cache_refresh_and_separation(pool: PgPool) {
    let (state, resolver, server) = mock(&pool).await;
    let mut uris = vec![
        "https://www.openstreetmap.org/node/240109189".to_owned(),
        "https://www.openstreetmap.org/way/50637691".to_owned(),
        "https://www.openstreetmap.org/relation/146656".to_owned(),
    ];
    uris.extend((1..=48).map(|id| format!("https://www.openstreetmap.org/node/{id}")));
    for uri in &uris {
        store::ensure_resource(&pool, uri).await.unwrap();
    }
    let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
    for size in [50, 1] {
        let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
            .await
            .unwrap();
        assert_eq!(jobs.len(), size);
        let (result, observations) = pinakes_core::telemetry::observe(
            &metrics,
            &resolver.manifest(),
            queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs),
        )
        .await;
        result.unwrap();
        assert_eq!(observations.len(), 1);
    }
    assert_eq!(
        state
            .calls
            .lock()
            .await
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        vec![50, 1]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM osm_place WHERE status='ok'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        3
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM resource WHERE osm_id IS NOT NULL AND (name IS NOT NULL OR point IS NOT NULL OR category IS NOT NULL OR type IS NOT NULL OR type_path IS NOT NULL OR doc-ARRAY['type','url','osm_id','source','status']::text[]<>'{}'::jsonb)").fetch_one(&pool).await.unwrap(),0);
    let doc = resolver.resolve(&uris[1]).await.unwrap();
    assert_eq!(doc.name.as_deref(), Some("Brandenburger Tor"));
    assert_eq!(doc.address["city"], "Potsdam");
    assert!(doc.attributes.contains_key("wheelchair"));
    assert!(!doc.attributes.contains_key("architect"));
    assert_eq!(state.calls.lock().await.len(), 2);
    sqlx::query("UPDATE osm_place SET refresh_at=now()-interval '1 second' WHERE osm_type='way'")
        .execute(&pool)
        .await
        .unwrap();
    resolver.resolve(&uris[1]).await.unwrap();
    assert_eq!(state.calls.lock().await.len(), 3);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let id = pinakes_core::model::identity(&uris[0]).unwrap().0;
    let (status, _, view) = call(&app, &format!("/v1/resources/{id}"), None).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["name"], "Berlin");
    assert_eq!(view["attribution"], osm::ATTRIBUTION);
    assert_eq!(view["document"]["location_source"], "osm");
    assert_eq!(view["osm_id"], "node/240109189");
    let (status, _, dump) = call(&app, "/v1/dumps/osm_place.ndjson", None).await;
    assert_eq!(status, 200);
    let rows = dump
        .as_str()
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 52);
    assert_eq!(rows[0]["licence"], "ODbL-1.0");
    assert!(rows.iter().any(|r| r["name"] == "Berlin"));
    assert_eq!(call(&app, "/v1/dumps/osm_place.ndjson", None).await.0, 200);
    assert_eq!(call(&app, "/v1/dumps/osm_place.ndjson", None).await.0, 429);
    assert!(
        sqlx::query("UPDATE resource SET name='OSM leak' WHERE osm_id IS NOT NULL")
            .execute(&pool)
            .await
            .is_err()
    );
    server.abort();
}
#[sqlx::test(migrations = "../../migrations")]
async fn unknown_429_and_optional_fallback(pool: PgPool) {
    let (state, mut resolver, server) = mock(&pool).await;
    let uri = "https://www.openstreetmap.org/node/111";
    assert_eq!(resolver.resolve(uri).await.unwrap().status, Status::Missing);
    assert!(state.paths.lock().await.is_empty());
    state.limited.store(true, Ordering::SeqCst);
    assert!(resolver
        .resolve("https://www.openstreetmap.org/node/112")
        .await
        .unwrap_err()
        .is::<pinakes_core::resolver::RetryLater>());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM osm_place WHERE osm_id=112")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    state.limited.store(false, Ordering::SeqCst);
    resolver.config.fallback_enabled = true;
    for kind in ["node", "way", "relation"] {
        let doc = resolver
            .resolve(&format!("https://www.openstreetmap.org/{kind}/222"))
            .await
            .unwrap();
        if kind == "node" {
            assert_eq!(doc.type_name, "CafeOrCoffeeShop");
            assert_eq!(doc.attributes["currency:XBT"], "yes");
            assert_eq!(doc.location.unwrap().lat, 46.0);
        } else {
            assert_eq!(doc.status, Status::Missing);
        }
    }
    for id in [404, 410] {
        assert_eq!(
            resolver
                .resolve(&format!("https://www.openstreetmap.org/node/{id}"))
                .await
                .unwrap()
                .status,
            Status::Missing
        );
    }
    let paths = state.paths.lock().await;
    assert!(paths.contains(&"/api/0.6/node/222.json".into()));
    assert!(!paths.iter().any(|p| p.contains("/full")));
    drop(paths);
    let uri = "https://www.openstreetmap.org/node/222";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let doc = resolver.resolve(uri).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    reference(&pool, &id, "coffee").await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, _, found) = call(
        &app,
        "/v1/search?bbox=7,45,9,47&type=Place&category=amenity%2Fcafe&q=Synthetic&facet=tags",
        None,
    )
    .await;
    assert_eq!(status, 200, "{found}");
    assert_eq!(found["items"][0]["type"], "CafeOrCoffeeShop");
    assert_eq!(found["facets"]["tags"][0]["count"], 1);
    server.abort();
}
#[sqlx::test(migrations = "../../migrations")]
async fn geo_is_final_and_sql_h3_agrees_with_h3o(pool: PgPool) {
    let uri = "geo:46.0051,8.9528,20;crs=wgs84;u=5";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let job = queue::claim(&pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    queue::run_job(&pool, &geo::GeoResolver, &QueueConfig::default(), &job)
        .await
        .unwrap();
    let (final_state, no_refresh, no_ttl): (bool, bool, bool) = sqlx::query_as(
        "SELECT final,refresh_at IS NULL,NOT(doc?'ttl_secs') FROM resource WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(final_state && no_refresh && no_ttl);
    sqlx::query("UPDATE resource SET next_attempt_at=now()-interval '1 day'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(queue::claim(&pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
        .is_none());
    for res in 0u8..=11 {
        let sql: String =
            sqlx::query_scalar("SELECT h3_latlng_to_cell(point(8.9528,46.0051),$1)::text")
                .bind(i32::from(res))
                .fetch_one(&pool)
                .await
                .unwrap();
        let rust = h3o::LatLng::new(46.0051, 8.9528)
            .unwrap()
            .to_cell(res.try_into().unwrap())
            .to_string();
        assert_eq!(sql, rust);
    }
}
#[sqlx::test(migrations = "../../migrations")]
async fn viewport_distance_time_facets_clustering_and_etag(pool: PgPool) {
    let mut ids = Vec::new();
    for (i, (lat, lon)) in [
        (46.0, 8.0),
        (46.001, 8.001),
        (46.002, 8.002),
        (47.0, 9.0),
        (0.0, 179.9),
        (0.0, -179.9),
    ]
    .into_iter()
    .enumerate()
    {
        let uri = format!("geo:{lat:.5},{lon:.5}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let doc = geo::parse(&uri).unwrap();
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        reference(&pool, &id, if i < 4 { "swiss" } else { "dateline" }).await;
        ids.push(id);
    }
    for (i, date) in ["2026-10-15T12:00:00Z", "2026-10-16T12:00:00Z"]
        .into_iter()
        .enumerate()
    {
        let uri = format!("https://example.org/event/{i}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let mut doc = ResourceMeta::new(&uri, "web", 86400);
        doc.set_type("MusicEvent");
        doc.start = Some(date.parse().unwrap());
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        reference(&pool, &id, "music").await;
    }
    let (mock, resolver, server) = mock(&pool).await;
    let uri = "https://www.openstreetmap.org/node/240109189";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let doc = resolver.resolve(uri).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    reference(&pool, &id, "berlin").await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, headers, view) = call(
        &app,
        "/v1/search?bbox=5,45,11,48&type=Place&facet=tags",
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["items"].as_array().unwrap().len(), 4);
    assert!(view["items"][0]["point"].is_object());
    assert_eq!(view["facets"]["tags"][0]["count"], 4);
    assert_eq!(headers["cache-control"], "public, max-age=30");
    let etag = headers["etag"].to_str().unwrap();
    assert_eq!(
        call(
            &app,
            "/v1/search?bbox=5,45,11,48&type=Place&facet=tags",
            Some(etag)
        )
        .await
        .0,
        304
    );
    sqlx::query("UPDATE resource SET resolved_at=now() WHERE id=$1")
        .bind(&ids[0])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &app,
            "/v1/search?bbox=5,45,11,48&type=Place&facet=tags",
            Some(etag)
        )
        .await
        .0,
        200
    );
    let (_, _, view) = call(&app, "/v1/search?bbox=170,-10,-170,10&facet=tags", None).await;
    assert_eq!(view["items"].as_array().unwrap().len(), 2);
    assert_eq!(view["facets"]["tags"][0]["label"], "dateline");
    let (status, _, view) = call(
        &app,
        "/v1/search?near=46,8&radius=1000&sort=distance&limit=1&facet=tags",
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["items"][0]["resource_id"], ids[0]);
    assert_eq!(view["facets"]["tags"][0]["count"], 3);
    let cursor = view["cursor"].as_str().unwrap();
    let (_, _, next) = call(
        &app,
        &format!(
            "/v1/search?near=46,8&radius=1000&sort=distance&limit=1&facet=tags&cursor={cursor}"
        ),
        None,
    )
    .await;
    assert_eq!(next["items"][0]["resource_id"], ids[1]);
    let (status, _, view) = call(
        &app,
        "/v1/search?from=2026-10-15T12:00:00Z&to=1792065600000&sort=start&type=Event",
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["items"].as_array().unwrap().len(), 1);
    let (_, _, view) = call(&app, "/v1/search?sort=start&limit=1", None).await;
    assert!(view["items"][0]["uri"].as_str().unwrap().ends_with("/0"));
    let cursor = view["cursor"].as_str().unwrap();
    let (_, _, view) = call(
        &app,
        &format!("/v1/search?sort=start&limit=1&cursor={cursor}"),
        None,
    )
    .await;
    assert!(view["items"][0]["uri"].as_str().unwrap().ends_with("/1"));
    let (status, _, view) = call(
        &app,
        "/v1/search?bbox=5,45,11,48&cluster=8&type=Place&limit=2&facet=tags",
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["truncated"], false);
    let clusters = view["clusters"].as_array().unwrap();
    let points = view["points"].as_array().unwrap();
    assert_eq!(
        clusters
            .iter()
            .map(|c| c["count"].as_u64().unwrap())
            .sum::<u64>()
            + points.len() as u64,
        4
    );
    assert_eq!(view["total"], 4);
    assert!(clusters
        .iter()
        .all(|c| c["top"].as_array().unwrap().len() <= 3));
    assert!((clusters[0]["lat"].as_f64().unwrap() - 46.001).abs() < 1e-6);
    let (_, _, view) = call(
        &app,
        "/v1/search?bbox=5,45,11,48&cluster=8&type=Place&limit=50",
        None,
    )
    .await;
    assert_eq!(view["clusters"], json!([]));
    assert_eq!(view["points"].as_array().unwrap().len(), 4);
    assert!(view["points"]
        .as_array()
        .unwrap()
        .iter()
        .all(|point| point.get("score").is_none()));
    let (_, _, view) = call(
        &app,
        "/v1/search?bbox=13,52,14,53&type=Place&scheme=https&facet=tags",
        None,
    )
    .await;
    assert_eq!(view["items"][0]["osm_id"], "node/240109189");
    assert_eq!(view["items"][0]["attribution"], osm::ATTRIBUTION);
    assert!(view["items"][0]["point"].is_object());
    assert_eq!(mock.calls.lock().await.len(), 1);
    for query in [
        "bbox=0,0,1,1&near=0,0&radius=10",
        "near=0,0&radius=200001",
        "near=91,0&radius=1",
        "near=0,0&radius=NaN",
        "bbox=0,0,1,1&cluster=NaN",
        "bbox=0,0,1,1&cluster=8&cursor=abc",
        "from=2027-01-01T00:00:00Z&to=2026-01-01T00:00:00Z",
    ] {
        assert_eq!(
            call(&app, &format!("/v1/search?{query}"), None).await.0,
            400,
            "{query}"
        );
    }
    let (status, _, limited) =
        call(&app, "/v1/search?bbox=5,45,11,48&cluster=8&limit=1", None).await;
    assert_eq!(status, 200, "{limited}");
    assert_eq!(limited["truncated"], true);
    let (_, before, _) = call(&app, "/v1/search?bbox=5,45,11,48&facet=tags", None).await;
    sqlx::query(
        "INSERT INTO feed_cursor(nexus,cursor,epoch,feed_length) VALUES('test',1,'epoch',1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        call(
            &app,
            "/v1/search?bbox=5,45,11,48&facet=tags",
            Some(before["etag"].to_str().unwrap())
        )
        .await
        .0,
        200
    );
    server.abort();
}

#[sqlx::test(migrations = "../../migrations")]
async fn osm_history_and_unconfirmed_fetches_stay_in_derivative_table(pool: PgPool) {
    let (state, resolver, server) = mock(&pool).await;
    let uri = "https://www.openstreetmap.org/node/240109189";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = resolver.resolve(uri).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    reference(&pool, &id, "city").await;
    doc.name = Some("Synthetic renamed city".into());
    doc.fetched_at += 1;
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, _, feed) = call(&app, "/v1/changes?source=osm&type=Place", None).await;
    assert_eq!(status, 200, "{feed}");
    assert_eq!(feed["lines"][1]["before"]["name"], "Berlin");
    assert_eq!(feed["lines"][1]["after"]["name"], "Synthetic renamed city");
    assert_eq!(feed["lines"][1]["attribution"], osm::ATTRIBUTION);
    let leaked:i64=sqlx::query_scalar("SELECT count(*) FROM resource_change WHERE source='osm' AND (type_path IS NOT NULL OR before-ARRAY['osm_id','version']::text[]<>'{}'::jsonb OR after-ARRAY['osm_id','version']::text[]<>'{}'::jsonb)").fetch_one(&pool).await.unwrap();
    assert_eq!(leaked, 0);
    state.omit.store(true, Ordering::SeqCst);
    sqlx::query("UPDATE osm_place SET refresh_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let missing = resolver.resolve(uri).await.unwrap();
    assert_eq!(missing.status, Status::Missing);
    assert_eq!(
        call(&app, &format!("/v1/resources/{id}"), None).await.2["name"],
        "Synthetic renamed city"
    );
    store::save_document(&pool, &id, 0, &missing).await.unwrap();
    let (_, _, spatial) = call(&app, "/v1/search?bbox=13,52,14,53", None).await;
    assert_eq!(spatial["items"][0]["name"], "Synthetic renamed city");
    assert_eq!(spatial["items"][0]["status"], "ok");
    // A cache hit of the same observation is not a second successful fetch.
    store::save_document(&pool, &id, 0, &resolver.resolve(uri).await.unwrap())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT missing_streak FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    sqlx::query("UPDATE osm_place SET refresh_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    store::save_document(&pool, &id, 0, &resolver.resolve(uri).await.unwrap())
        .await
        .unwrap();
    let (_, _, feed) = call(
        &app,
        &format!("/v1/resources/{id}/changes?class=cancelled"),
        None,
    )
    .await;
    assert_eq!(feed["lines"].as_array().unwrap().len(), 1);
    assert_eq!(feed["lines"][0]["after"]["name"], "Synthetic renamed city");
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_change")
        .fetch_one(&pool)
        .await
        .unwrap();
    let cache: Value = sqlx::query_scalar("SELECT to_jsonb(o) FROM osm_place o")
        .fetch_one(&pool)
        .await
        .unwrap();
    resolver.probe(uri).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, Value>("SELECT to_jsonb(o) FROM osm_place o")
            .fetch_one(&pool)
            .await
            .unwrap(),
        cache
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change")
            .fetch_one(&pool)
            .await
            .unwrap(),
        before
    );
    pinakes_core::changes::retain(&pool, 90).await.unwrap();
    assert_eq!(
        call(&app, "/v1/changes?class=renamed&source=osm", None)
            .await
            .2["lines"][0]["before"]["name"],
        "Berlin"
    );
    let xid: String = sqlx::query_scalar("SELECT xmin::text FROM osm_place")
        .fetch_one(&pool)
        .await
        .unwrap();
    pinakes_core::changes::retain(&pool, 90).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT xmin::text FROM osm_place")
            .fetch_one(&pool)
            .await
            .unwrap(),
        xid,
        "retention must not rewrite unchanged versions"
    );
    server.abort();
}

#[sqlx::test(migrations = "../../migrations")]
async fn batch_failures_are_per_object_and_upstream_retries_do_not_charge(pool: PgPool) {
    use std::sync::atomic::AtomicU16;
    let code = Arc::new(AtomicU16::new(200));
    let mode = code.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app=Router::new().route("/lookup",get(move || {let mode=mode.clone();async move {
        let status=mode.load(Ordering::SeqCst);
        if status==413 {return (StatusCode::OK,vec![b'x';2*1024*1024+1]).into_response();}
        if status!=200 {return (StatusCode::from_u16(status).unwrap(),[("retry-after","7200")],"retry").into_response();}
        Json(json!([
            {"osm_type":"node","osm_id":1,"lat":"broken","lon":"8","namedetails":{"name":"Bad"}},
            {"osm_type":"node","osm_id":2,"lat":"46","lon":"8","namedetails":{"name":"Good"}},
            {"osm_type":"node","osm_id":999,"lat":"47","lon":"9","namedetails":{"name":"Unrequested"}}
        ])).into_response()
    }})).fallback(get(||async {StatusCode::INTERNAL_SERVER_ERROR}));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = OsmConfig {
        nominatim_url: url.clone(),
        api_url: url,
        requests_per_second: 1000.0,
        fallback_enabled: true,
        ..Default::default()
    };
    let resolver = OsmResolver::new(pool.clone(), config.clone()).unwrap();
    for id in 1..=3 {
        store::ensure_resource(&pool, &format!("https://www.openstreetmap.org/node/{id}"))
            .await
            .unwrap();
    }
    let cfg = QueueConfig::default();
    let jobs = queue::claim_batch(&pool, "osm", &cfg, 50).await.unwrap();
    queue::run_batch(&pool, &resolver, &cfg, &jobs)
        .await
        .unwrap();
    let outcomes: Vec<(i64, i32, String, bool)> = sqlx::query_as(
        "SELECT osm_id,attempts,status,lease_until IS NULL FROM resource ORDER BY osm_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        outcomes,
        vec![
            (1, 1, "error".into(), true),
            (2, 0, "ok".into(), true),
            (3, 0, "pending".into(), true)
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM osm_place WHERE osm_id=999")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    for status in [429, 503, 413] {
        code.store(status, Ordering::SeqCst);
        sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE osm_id=3")
            .execute(&pool)
            .await
            .unwrap();
        let jobs = queue::claim_batch(&pool, "osm", &cfg, 50).await.unwrap();
        let resolver = OsmResolver::new(pool.clone(), config.clone()).unwrap();
        let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
        let (result, observations) = pinakes_core::telemetry::observe(
            &metrics,
            &resolver.manifest(),
            queue::run_batch(&pool, &resolver, &cfg, &jobs),
        )
        .await;
        result.unwrap();
        assert_eq!(
            observations[0].outcome,
            if status == 429 {
                "rate_limited"
            } else {
                "error"
            }
        );
        let (attempts,dead,delay):(i32,bool,f64)=sqlx::query_as("SELECT attempts,dead_letter,extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE osm_id=3").fetch_one(&pool).await.unwrap();
        assert_eq!(attempts, 0);
        assert!(!dead);
        if status == 413 {
            assert!(delay > 50.0 && delay <= 60.0);
        } else {
            assert!(delay > 3590.0 && delay <= 3600.0);
        }
    }
    server.abort();
}

#[sqlx::test(migrations = "../../migrations")]
async fn object_aliases_share_only_the_derivative_data(pool: PgPool) {
    let (mock, resolver, server) = mock(&pool).await;
    let uris = [
        "https://www.openstreetmap.org/node/240109189",
        "https://osm.org/node/240109189/",
        "http://openstreetmap.org/node/240109189?x=1",
    ];
    let mut ids = Vec::new();
    for uri in uris {
        ids.push(store::ensure_resource(&pool, uri).await.unwrap());
    }
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    assert_eq!(mock.calls.lock().await[0], vec!["N240109189"]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM osm_place")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM resource WHERE name IS NULL AND point IS NULL AND doc->>'type'='Place'").fetch_one(&pool).await.unwrap(),3);
    assert!(
        sqlx::query("UPDATE resource SET doc='{}' WHERE osm_id IS NOT NULL")
            .execute(&pool)
            .await
            .is_err()
    );
    server.abort();
}

#[sqlx::test(migrations = "../../migrations")]
async fn unpolled_dumps_release_permits_and_connections(pool: PgPool) {
    let (_, resolver, server) = mock(&pool).await;
    resolver
        .resolve("https://www.openstreetmap.org/node/240109189")
        .await
        .unwrap();
    let config = Config {
        osm: OsmConfig {
            dump_requests_per_minute: 100,
            dump_chunk_secs: 1,
            dump_total_secs: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let app = api::router(AppState::new(pool.clone(), config).unwrap());
    async fn unread(app: &Router) -> axum::response::Response {
        let mut r = Request::builder()
            .uri("/v1/dumps/osm_place.ndjson")
            .body(Body::empty())
            .unwrap();
        r.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:1".parse::<std::net::SocketAddr>().unwrap(),
        ));
        app.clone().oneshot(r).await.unwrap()
    }
    let mut held = Vec::new();
    for _ in 0..4 {
        let r = unread(&app).await;
        assert_eq!(r.status(), 200);
        held.push(r);
    }
    assert_eq!(unread(&app).await.status(), 429);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(
        unread(&app).await.status(),
        200,
        "deadlines release permits even without body polling"
    );
    let idle:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND state='idle in transaction'").fetch_one(&pool).await.unwrap();
    assert_eq!(idle, 0);
    assert!(
        held.remove(0).into_body().collect().await.is_err(),
        "an incomplete dump must not look successful"
    );
    drop(held);
    server.abort();
}

#[sqlx::test(migrations = false)]
async fn reviewed_database_upgrade_scrubs_legacy_web_osm_history(pool: PgPool) {
    for migration in pinakes_core::MIGRATOR
        .iter()
        .filter(|m| m.version <= 202610150004)
    {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
    }
    let uri = "https://osm.org/node/240109189/?x=1";
    let id = pubky_resource_id::resource_id(uri);
    let mut doc = ResourceMeta::new(uri, "web", 86400);
    doc.name = Some("Legacy OSM title".into());
    sqlx::query("INSERT INTO resource(id,uri,scheme,host,resolver,name,type,type_path,doc,status,resolved_at,attempts,dead_letter) VALUES($1,$2,'https','osm.org','web',$3,'WebPage',ARRAY['Thing','WebPage'],$4,'ok',now(),4,true)")
        .bind(&id).bind(uri).bind(&doc.name).bind(json!(doc)).execute(&pool).await.unwrap();
    for resource_id in [&id, "11111111111111111111111111111111"] {
        sqlx::query("INSERT INTO resource_change(resource_id,class,fields,after,source) VALUES($1,'created',ARRAY['name'],$2,'web')").bind(resource_id).bind(json!(doc)).execute(&pool).await.unwrap();
    }
    for migration in pinakes_core::MIGRATOR
        .iter()
        .filter(|m| m.version > 202610150004)
    {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
    }
    let (resolver, name, status, attempts, dead, kind): (
        String,
        Option<String>,
        String,
        i32,
        bool,
        String,
    ) = sqlx::query_as(
        "SELECT resolver,name,status,attempts,dead_letter,doc->>'type' FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (
            resolver.as_str(),
            name,
            status.as_str(),
            attempts,
            dead,
            kind.as_str()
        ),
        ("osm", None, "pending", 0, false, "Place")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert!(sqlx::query("UPDATE resource SET name='leak' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .is_err());
    assert!(sqlx::query("INSERT INTO resource_change(resource_id,class,fields,after,source) VALUES($1,'created',ARRAY['name'],$2,'web')").bind(&id).bind(json!(doc)).execute(&pool).await.is_err());
}

#[sqlx::test(migrations = false)]
async fn skeleton_upgrade_prepares_osm_before_original_constraint(pool: PgPool) {
    let first = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            pinakes_core::MIGRATOR.iter().take(1).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    first.run(&pool).await.unwrap();
    let uri = "https://www.openstreetmap.org/node/123";
    let id = pubky_resource_id::resource_id(uri);
    sqlx::query("INSERT INTO resource(id,uri,scheme,host,resolver,name,type,category,point,doc,status,resolved_at,attempts,dead_letter,lease_until) VALUES($1,$2,'https','www.openstreetmap.org','web','Old page title','WebPage','page',ST_SetSRID(ST_Point(8,46),4326),'{\"type\":\"WebPage\",\"name\":\"Old page title\"}','ok',now(),4,true,now()+interval '1 hour')").bind(&id).bind(uri).execute(&pool).await.unwrap();
    pinakes_core::migrate(&pool).await.unwrap();
    let state:Value=sqlx::query_scalar("SELECT jsonb_build_object('resolver',resolver,'name',name,'status',status,'attempts',attempts,'dead',dead_letter,'resolved',resolved_at,'leased',lease_until,'doc',doc) FROM resource WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(state["resolver"], "osm");
    assert_eq!(state["status"], "pending");
    assert_eq!(state["attempts"], 0);
    assert_eq!(state["dead"], false);
    assert!(state["name"].is_null() && state["resolved"].is_null() && state["leased"].is_null());
    assert_eq!(state["doc"]["type"], "Place");
    // A repeat startup validates the original checksums rather than rewriting migration history.
    pinakes_core::migrate(&pool).await.unwrap();
}
