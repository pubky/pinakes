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
    let (status, headers, dump) = call(&app, "/v1/dumps/osm_place.ndjson", None).await;
    assert_eq!(status, 200);
    // D1: a file download, so Swagger UI and browsers do not parse it as one JSON value.
    assert_eq!(headers["content-type"], "application/x-ndjson");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"osm_place.ndjson\""
    );
    let rows = dump
        .as_str()
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 52);
    assert_eq!(rows[0]["licence"], "ODbL-1.0");
    assert!(rows.iter().any(|r| r["name"] == "Berlin"));
    // D2: every line has exactly the documented fields; no internal column leaks.
    let spec = serde_json::to_value(api::openapi_document()).unwrap();
    let fields = |schema: &str| {
        spec["components"]["schemas"][schema]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
    };
    let keys = |row: &Value| {
        row.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(keys(&rows[0]), fields("OsmDumpNotice"));
    let place = fields("OsmDumpPlace");
    assert_eq!(
        place.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "address",
            "bbox",
            "category",
            "display_name",
            "fetched_at",
            "name",
            "osm_id",
            "osm_type",
            "point",
            "status",
            "tags",
            "type",
            "type_path",
            "versions"
        ],
        "the public dump line format changed"
    );
    let required = spec["components"]["schemas"]["OsmDumpPlace"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(required, place, "every field is on every line");
    for row in &rows[1..] {
        assert_eq!(keys(row), place, "{row}");
    }
    let berlin = rows.iter().find(|r| r["name"] == "Berlin").unwrap();
    assert_eq!(berlin["osm_type"], "node");
    assert_eq!(berlin["osm_id"], 240109189);
    assert!(berlin["point"]["lat"].is_f64() && berlin["point"]["lon"].is_f64());
    assert_eq!(berlin["status"], "ok");
    // The version a resource pins (and /v1/changes serves) is in the line.
    let pinned: String = sqlx::query_scalar(
        "SELECT osm_version FROM resource WHERE osm_type='node' AND osm_id=240109189 LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(berlin["versions"][&pinned]["name"], "Berlin");
    for column in [
        "fetch_doc",
        "fetch_display_name",
        "search_text",
        "refresh_at",
        "doc",
    ] {
        assert!(rows.iter().all(|r| r.get(column).is_none()), "{column}");
    }
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
    queue::run_job(
        &pool,
        &geo::GeoResolver::default(),
        &QueueConfig::default(),
        &job,
    )
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

#[sqlx::test(migrations = "../../migrations")]
async fn review_group_osm_best_member_keeps_attribution_and_resource_own_name(pool: PgPool) {
    let (_, resolver, server) = mock(&pool).await;
    let uri = "https://www.openstreetmap.org/node/240109189";
    let osm_id = store::ensure_resource(&pool, uri).await.unwrap();
    let doc = resolver.resolve(uri).await.unwrap();
    store::save_document(&pool, &osm_id, 0, &doc).await.unwrap();
    let uri = "https://example.org/berlin";
    let page = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = pinakes_core::model::ResourceMeta::new(uri, "web", 3600);
    doc.name = Some("Own page".into());
    store::save_document(&pool, &page, 0, &doc).await.unwrap();
    let group = pinakes_core::groups::curate(
        &pool,
        &"c".repeat(32),
        pinakes_core::groups::Curation {
            members: vec![
                pinakes_core::groups::Member {
                    resource_id: osm_id,
                    occurrence: None,
                },
                pinakes_core::groups::Member {
                    resource_id: page.clone(),
                    occurrence: None,
                },
            ],
            name: None,
            image: None,
        },
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(group["attribution"], osm::ATTRIBUTION);
    let cached: serde_json::Value = sqlx::query_scalar("SELECT card FROM \"group\" WHERE id=$1")
        .bind(group["id"].as_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        cached["location"].is_null() && cached["name"].is_null() && cached["kind"].is_null(),
        "{cached}"
    );
    assert_eq!(group["member_count"], 2);

    assert_eq!(group["name"], "Berlin");
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (_, _, doc) = call(&app, &format!("/v1/resources/{page}"), None).await;
    assert_eq!(doc["name"], "Own page");
    assert_eq!(doc["group"]["name"], "Berlin"); // Curated metadata survives member reads (C2).
    server.abort();
}

async fn group_of(pool: &PgPool, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT group_id FROM group_unit WHERE resource_id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .flatten()
}

/// Every URL style of the round-11 live probe (Berlin, node 240109189).
#[sqlx::test(migrations = "../../migrations")]
async fn round11_osm_url_styles_resolve_as_the_object_a_point_or_one_final_refusal(pool: PgPool) {
    let (mock, resolver, server) = mock(&pool).await;
    let canonical = "https://www.openstreetmap.org/node/240109189";
    let objects = [
        canonical,
        "https://openstreetmap.org/node/240109189",
        "https://osm.org/node/240109189",
        "http://www.openstreetmap.org/node/240109189",
        "https://www.openstreetmap.org/node/240109189/history",
        "https://www.openstreetmap.org/node/240109189/history/3",
        "https://www.openstreetmap.org/edit?node=240109189",
        "https://api.openstreetmap.org/api/0.6/node/240109189",
        "https://api.openstreetmap.org/api/0.6/node/240109189.json",
        "https://nominatim.openstreetmap.org/ui/details.html?osmtype=N&osmid=240109189",
        "https://nominatim.openstreetmap.org/details?osmtype=N&osmid=240109189",
    ];
    // The id rule drops fragments, so a #map suffix is the same resource.
    assert_eq!(
        pinakes_core::model::identity(&format!("{canonical}#map=12/52.5/13.4"))
            .unwrap()
            .0,
        pinakes_core::model::identity(canonical).unwrap().0
    );
    let mut ids = Vec::new();
    for uri in objects {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let (resolver_name, key): (String, String) =
            sqlx::query_as("SELECT resolver,osm_type||'/'||osm_id FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (resolver_name.as_str(), key.as_str()),
            ("osm", "node/240109189"),
            "{uri}"
        );
        ids.push(id);
    }
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    assert_eq!(jobs.len(), objects.len());
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    // One Nominatim object and one derivative row serve every alias.
    assert_eq!(
        *mock.calls.lock().await,
        vec![vec!["N240109189".to_owned()]]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM osm_place")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let group = group_of(&pool, &ids[0]).await.expect("aliases group");
    for (uri, id) in objects.iter().zip(&ids) {
        assert_eq!(
            group_of(&pool, id).await.as_deref(),
            Some(group.as_str()),
            "{uri}"
        );
        assert!(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM resource_group WHERE resource_id=$1 AND key='osm:node/240109189')"
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap());
    }

    // Location links are offline points that never group, not even with each other.
    let points = [
        "https://www.openstreetmap.org/?mlat=52.5186925&mlon=13.3996024",
        "https://osm.org/go/0GAjIv8h?m",
        "https://osmand.net/map?pin=52.5186925,13.3996024",
        "https://osmand.net/go?lat=52.5186925&lon=13.3996024&z=17",
        "https://omaps.app/B4srhdHVVt/Berlin",
    ];
    let mut point_ids = Vec::new();
    for uri in points {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        point_ids.push(id);
    }
    let geo = geo::GeoResolver::default();
    while let Some(job) = queue::claim(&pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
    {
        queue::run_job(&pool, &geo, &QueueConfig::default(), &job)
            .await
            .unwrap();
    }
    for (uri, id) in points.iter().zip(&point_ids) {
        let (status, kind, source, has_point): (String, String, String, bool) = sqlx::query_as(
            "SELECT status,type,doc->>'source',point IS NOT NULL FROM resource WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (status.as_str(), kind.as_str(), source.as_str(), has_point),
            ("ok", "Place", "geo", true),
            "{uri}"
        );
        assert_eq!(group_of(&pool, id).await, None, "{uri}");
    }
    assert!(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM resource_change WHERE resource_id=$1 AND source='geo')"
    )
    .bind(&point_ids[0])
    .fetch_one(&pool)
    .await
    .unwrap());

    // A map view is the homepage; it, like any other OSM page, is refused once and stays final.
    let web = pinakes_core::resolver::web::WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        events: None,
        egress: pinakes_core::resolver::egress::Egress::new(Default::default()),
        config: Default::default(),
    };
    let mut refused = Vec::new();
    for uri in [
        "https://www.openstreetmap.org/#map=12/52.5/13.4",
        "https://www.openstreetmap.org/user/someone",
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let job = queue::claim(&pool, "web", &QueueConfig::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.id, id);
        queue::run_job(&pool, &web, &QueueConfig::default(), &job)
            .await
            .unwrap();
        refused.push(id);
    }
    for id in &refused {
        let state: (String, i32, bool, bool, Option<String>, bool) = sqlx::query_as("SELECT status,attempts,dead_letter,final,last_error,next_attempt_at='infinity' FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            state,
            (
                "error".into(),
                1,
                false,
                true,
                Some("unsupported".into()),
                true
            )
        );
    }
    sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=ANY($1)")
        .bind(&refused)
        .execute(&pool)
        .await
        .unwrap();
    assert!(queue::claim(&pool, "web", &QueueConfig::default())
        .await
        .unwrap()
        .is_none());
    server.abort();
}

#[sqlx::test(migrations = false)]
async fn round11_upgrade_reroutes_legacy_osm_aliases_and_location_links_once(pool: PgPool) {
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            pinakes_core::MIGRATOR.iter().take(37).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before.run(&pool).await.unwrap();
    let alias = "https://www.openstreetmap.org/node/240109189/history";
    let pending_alias = "https://www.openstreetmap.org/edit?node=240109189";
    let marker = "https://www.openstreetmap.org/?mlat=52.5186925&mlon=13.3996024";
    let view = "https://www.openstreetmap.org/";
    let promoted = "https://example.org/talk";
    let mut ids = HashMap::new();
    for uri in [alias, pending_alias, marker, view, promoted] {
        ids.insert(uri, store::ensure_resource(&pool, uri).await.unwrap());
    }
    // What the previous release stored: web routes, refused once into dead letter.
    sqlx::query("UPDATE resource SET resolver='web',osm_type=NULL,osm_id=NULL,doc='{}',type_path='{}',status='error',attempts=1,dead_letter=true,last_error='policy',next_attempt_at=now()+interval '30 seconds' WHERE id=ANY($1)")
        .bind([&ids[alias], &ids[marker], &ids[view]])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET resolver='web',osm_type=NULL,osm_id=NULL,doc='{}',type_path='{}' WHERE id=$1")
        .bind(&ids[pending_alias])
        .execute(&pool)
        .await
        .unwrap();
    pinakes_core::migrate(&pool).await.unwrap();
    // A page whose own card promoted it to the media route keeps that route (saved through
    // today's save path, so after the schema has caught up).
    let mut card = ResourceMeta::new(promoted, "media", 86400);
    card.set_type("VideoObject");
    card.name = Some("Talk".into());
    store::save_document(&pool, &ids[promoted], 0, &card)
        .await
        .unwrap();
    // 0038 turns the local refusals into one final, non-dead-letter outcome.
    let state = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, String, bool, bool, i32, Option<String>, bool)>(
                "SELECT resolver,status,dead_letter,final,attempts,last_error,next_attempt_at<=now()+interval '1 hour' FROM resource WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        state(ids[view].clone()).await,
        (
            Some("web".into()),
            "error".into(),
            false,
            true,
            1,
            Some("unsupported".into()),
            false
        )
    );
    assert_eq!(queue::reclassify_routes(&pool).await.unwrap(), 3);
    for uri in [alias, pending_alias] {
        let (resolver, status, dead, final_, attempts, error, due) = state(ids[uri].clone()).await;
        assert_eq!(
            (
                resolver.as_deref(),
                status.as_str(),
                dead,
                final_,
                attempts,
                error,
                due
            ),
            (Some("osm"), "pending", false, false, 0, None, true),
            "{uri}"
        );
        let envelope: Value = sqlx::query_scalar(
            "SELECT doc FROM resource WHERE id=$1 AND osm_type='node' AND osm_id=240109189",
        )
        .bind(&ids[uri])
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(envelope["osm_id"], "node/240109189");
    }
    let (resolver, _, dead, final_, attempts, error, due) = state(ids[marker].clone()).await;
    assert_eq!(
        (resolver.as_deref(), dead, final_, attempts, error, due),
        (Some("geo"), false, false, 0, None, true)
    );
    assert_eq!(state(ids[view].clone()).await.0.as_deref(), Some("web"));
    assert_eq!(
        state(ids[promoted].clone()).await.0.as_deref(),
        Some("media")
    );
    // Idempotent, and nothing for the round-10 check to re-queue.
    assert_eq!(queue::reclassify_routes(&pool).await.unwrap(), 0);
    assert_eq!(queue::requeue_rerouted(&pool).await.unwrap(), 0);

    let (_, resolver, server) = mock(&pool).await;
    sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=ANY($1)")
        .bind([&ids[alias], &ids[pending_alias], &ids[marker]])
        .execute(&pool)
        .await
        .unwrap();
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    let job = queue::claim(&pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    queue::run_job(
        &pool,
        &geo::GeoResolver::default(),
        &QueueConfig::default(),
        &job,
    )
    .await
    .unwrap();
    for uri in [alias, pending_alias, marker] {
        assert_eq!(state(ids[uri].clone()).await.1, "ok", "{uri}");
    }
    assert_eq!(
        group_of(&pool, &ids[alias]).await,
        group_of(&pool, &ids[pending_alias]).await
    );
    server.abort();
}

#[sqlx::test(migrations = "../../migrations")]
async fn round11_location_links_switch_off_waits_like_a_disabled_route(pool: PgPool) {
    let marker = "https://www.openstreetmap.org/?mlat=52.5&mlon=13.4";
    let off = geo::GeoResolver {
        location_links: false,
    };
    let error = off.resolve(marker).await.unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<pinakes_core::resolver::RetryLater>()
            .unwrap()
            .class,
        "disabled"
    );
    assert_eq!(
        off.resolve("geo:52.5,13.4").await.unwrap().status,
        Status::Ok
    );
    let mut config = Config::default();
    config.geo.location_links = false;
    let app = api::router(AppState::new(pool, config).unwrap());
    for (uri, expected) in [(marker, 400), ("geo:52.5,13.4", 200)] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/resolve")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "uri": uri }).to_string()))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.2:1".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16() / 100, expected / 100, "{uri}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn round12_osm_reextraction_bypasses_the_fresh_cache(pool: PgPool) {
    let (mock, resolver, server) = mock(&pool).await;
    let id = store::ensure_resource(&pool, "https://www.openstreetmap.org/node/240109189")
        .await
        .unwrap();
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    assert_eq!(mock.calls.lock().await.len(), 1);
    sqlx::query("UPDATE resource SET extracted_with=0 WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        queue::reextract_outdated(&pool, 21600, &pinakes_core::resolver::extractor_versions())
            .await
            .unwrap(),
        1
    );
    sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    // The object was looked up and mapped again, not served from the cache.
    assert_eq!(mock.calls.lock().await.len(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, Option<i32>>("SELECT extracted_with FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        Some(1)
    );
    server.abort();
}

/// Round 13 W1/W4: osm.org object shortlinks stored as points by round 11 become aliases of their
/// object, and changeset/note shortlinks drop their point facts on the way to one final refusal.
#[sqlx::test(migrations = "../../migrations")]
async fn round13_object_shortlinks_join_their_object_and_route_moves_are_complete(pool: PgPool) {
    let (_, resolver, server) = mock(&pool).await;
    let canonical = "https://www.openstreetmap.org/node/240109189";
    let short = "https://osm.org/go/0GAjIv8h?node=240109189";
    let changeset = "https://osm.org/go/0EEQjE--?changeset=1";
    let note = "https://www.openstreetmap.org/go/0EEQjE--?note=5";
    let mut ids = HashMap::new();
    for uri in [canonical, short, changeset, note] {
        ids.insert(uri, store::ensure_resource(&pool, uri).await.unwrap());
    }
    // What round 11 stored: final `ok` geo points at the shortlink's map centre.
    for uri in [short, changeset, note] {
        sqlx::query("UPDATE resource SET resolver='geo',osm_type=NULL,osm_id=NULL,doc='{}',type_path='{}',status='pending' WHERE id=$1")
            .bind(&ids[uri])
            .execute(&pool)
            .await
            .unwrap();
        let code = uri.split("/go/").nth(1).unwrap().split('?').next().unwrap();
        let mut doc = geo::parse_link(&format!("https://osm.org/go/{code}")).unwrap();
        doc.url = uri.into();
        store::save_document(&pool, &ids[uri], 0, &doc)
            .await
            .unwrap();
    }
    let state = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, String, bool, bool, i64, bool, bool, Value)>(
                "SELECT resolver,status,final,dead_letter,generation,next_attempt_at<=now()+interval '1 hour',point IS NULL AND name IS NULL AND type IS NULL,doc FROM resource WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let before = state(ids[short].clone()).await;
    assert_eq!(
        (before.1.as_str(), before.2, before.5, before.6),
        ("ok", true, false, false)
    );
    assert_eq!(queue::reclassify_routes(&pool).await.unwrap(), 3);
    // Entering the OSM route: the envelope, pending, revived, fenced and due within the hour.
    let (route, status, final_, dead, generation, due, cleared, doc) =
        state(ids[short].clone()).await;
    assert_eq!(
        (
            route.as_deref(),
            status.as_str(),
            final_,
            dead,
            due,
            cleared
        ),
        (Some("osm"), "pending", false, false, true, true)
    );
    assert_eq!(generation, before.4 + 1);
    assert_eq!(doc["osm_id"], "node/240109189");
    // Round 14 K60: unresolved until the OSM worker maps it, so the boundary proof holds meanwhile.
    for uri in [short, changeset, note] {
        let resolved: (bool, bool, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT resolved_at IS NOT NULL,refresh_at IS NOT NULL,resolved_by,osm_version FROM resource WHERE id=$1",
        )
        .bind(&ids[uri])
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(resolved, (false, false, None, None), "{uri}");
    }
    if let Some(proof) = boundary_script(&pool).await {
        assert!(
            proof.status.success(),
            "{}",
            String::from_utf8_lossy(&proof.stderr)
        );
    }
    // An OSM-host URL moving to web keeps no facts (resource_web_osm_boundary).
    for uri in [changeset, note] {
        let (route, status, final_, dead, _, due, cleared, doc) = state(ids[uri].clone()).await;
        assert_eq!(
            (
                route.as_deref(),
                status.as_str(),
                final_,
                dead,
                due,
                cleared,
                doc
            ),
            (Some("web"), "pending", false, false, true, true, json!({})),
            "{uri}"
        );
    }
    assert_eq!(queue::reclassify_routes(&pool).await.unwrap(), 0);
    assert_eq!(queue::requeue_rerouted(&pool).await.unwrap(), 0);

    sqlx::query("UPDATE resource SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 2);
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    let group = group_of(&pool, &ids[canonical])
        .await
        .expect("object group");
    assert_eq!(group_of(&pool, &ids[short]).await, Some(group));
    let web = pinakes_core::resolver::web::WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        events: None,
        egress: pinakes_core::resolver::egress::Egress::new(Default::default()),
        config: Default::default(),
    };
    while let Some(job) = queue::claim(&pool, "web", &QueueConfig::default())
        .await
        .unwrap()
    {
        queue::run_job(&pool, &web, &QueueConfig::default(), &job)
            .await
            .unwrap();
    }
    for uri in [changeset, note] {
        let refused: (String, bool, Option<String>) =
            sqlx::query_as("SELECT status,final,last_error FROM resource WHERE id=$1")
                .bind(&ids[uri])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            refused,
            ("error".into(), true, Some("unsupported".into())),
            "{uri}"
        );
        assert_eq!(group_of(&pool, &ids[uri]).await, None, "{uri}");
    }
    server.abort();
}

/// Round 13 W5: a point claims no URL key of its own, so a page naming the exact link URL as its
/// canonical does not group with it.
#[sqlx::test(migrations = "../../migrations")]
async fn round13_a_page_naming_a_map_link_does_not_group_with_the_point(pool: PgPool) {
    let link = "https://osmand.net/map?pin=52.5186925,13.3996024";
    let page = "https://osmand.net/map?pin=52.5186925,13.3996024&utm_source=share";
    let point = store::ensure_resource(&pool, link).await.unwrap();
    let job = queue::claim(&pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    queue::run_job(
        &pool,
        &geo::GeoResolver::default(),
        &QueueConfig::default(),
        &job,
    )
    .await
    .unwrap();
    let other = store::ensure_resource(&pool, "https://osmand.net/blog/berlin")
        .await
        .unwrap();
    let mut doc = ResourceMeta::new("https://osmand.net/blog/berlin", "web", 86400);
    doc.set_type("WebPage");
    doc.name = Some("Berlin on OsmAnd".into());
    pinakes_core::group_keys::add(&mut doc, "canonical", link);
    pinakes_core::group_keys::add(&mut doc, "same_as", page);
    store::save_document(&pool, &other, 0, &doc).await.unwrap();
    assert!(!sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM resource_group WHERE resource_id=$1 AND key LIKE 'url:%')"
    )
    .bind(&point)
    .fetch_one(&pool)
    .await
    .unwrap());
    assert_eq!(group_of(&pool, &point).await, None);
}

/// `scripts/verify-osm-boundary` against this test database, or None, with a message saying so,
/// where the PostgreSQL client tools it runs (`psql`, `pg_dump`) are not installed.
async fn boundary_script(pool: &PgPool) -> Option<std::process::Output> {
    for tool in ["psql", "pg_dump"] {
        if std::process::Command::new(tool)
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!(
                "SKIPPED: scripts/verify-osm-boundary needs `{tool}` (PostgreSQL client tools), \
                 which is not installed"
            );
            return None;
        }
    }
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .unwrap();
    let mut url = url::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    url.set_path(&format!("/{database}"));
    Some(
        std::process::Command::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/verify-osm-boundary"
        ))
        .env("DATABASE_URL", url.as_str())
        .output()
        .unwrap(),
    )
}

/// Round 13 K58: `scripts/verify-osm-boundary` accepts the geo change lines of OSM-host location
/// links (as migration 0038 does) and still refuses web facts on OSM hosts.
#[sqlx::test(migrations = "../../migrations")]
async fn round13_boundary_script_accepts_location_link_history(pool: PgPool) {
    let marker = "https://www.openstreetmap.org/?mlat=52.5186925&mlon=13.3996024";
    let id = store::ensure_resource(&pool, marker).await.unwrap();
    let job = queue::claim(&pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    queue::run_job(
        &pool,
        &geo::GeoResolver::default(),
        &QueueConfig::default(),
        &job,
    )
    .await
    .unwrap();
    assert!(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM resource_change WHERE resource_id=$1 AND source='geo')"
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap());
    let Some(ok) = boundary_script(&pool).await else {
        return;
    };
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    // A web change line on an OSM host (only possible without the table constraint) still fails.
    sqlx::query("ALTER TABLE resource_change DROP CONSTRAINT change_web_osm_boundary")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource_change SET source='web' WHERE resource_id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let leaked = boundary_script(&pool).await.unwrap();
    assert!(!leaked.status.success());
    assert!(String::from_utf8_lossy(&leaked.stderr).contains("leaked into historical changes"));
}
