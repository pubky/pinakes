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
    for size in [50, 1] {
        let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
            .await
            .unwrap();
        assert_eq!(jobs.len(), size);
        queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
            .await
            .unwrap();
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
        .to_string()
        .contains("429"));
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
        assert_eq!(doc.type_name, "CafeOrCoffeeShop");
        assert_eq!(doc.attributes["currency:XBT"], "yes");
        assert_eq!(doc.location.unwrap().lat, 46.0);
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
    assert!(paths.contains(&"/api/0.6/way/222/full.json".into()));
    assert!(paths.contains(&"/api/0.6/relation/222/full.json".into()));
    drop(paths);
    let uri = "https://www.openstreetmap.org/way/222";
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
        "bbox=0,0,1,1&cluster=23",
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
