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
mod common;

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
    assert_eq!(headers["user-agent"], "pinakes/0.1.0 (+https://pubky.app)");
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
async fn mock(pool: &PgPool) -> (Mock, OsmResolver, tokio::task::JoinHandle<()>) {
    let state = Mock::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/lookup", get(nominatim))
        .fallback(get(
            |State(state): State<Mock>, uri: axum::http::Uri| async move {
                state.paths.lock().await.push(uri.path().into());
                StatusCode::NOT_FOUND
            },
        ))
        .with_state(state.clone());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = OsmConfig {
        nominatim_url: format!("http://{address}").into(),
        requests_per_second: 1000.0,
        ..Default::default()
    };
    (
        state,
        OsmResolver::new(pool.clone(), config).unwrap(),
        handle,
    )
}
/// The change lines of one resource since the epoch.
async fn changes_of(app: &Router, id: &str) -> Value {
    let body = serde_json::json!({"resource_ids":[id],"since":0,"limit":500});
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/changes/lookup")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    common::check_response("POST", "/v1/changes/lookup", 200, &value);
    value
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
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    if let Some(value) = &parsed {
        common::check_response("GET", path, status, value);
    }
    let value = parsed.unwrap_or_else(|| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, headers, value)
}
/// A fresh, empty dump directory for one test.
fn dump_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pinakes-dump-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
/// A dump download with request headers; the raw body.
async fn download(app: &Router, headers: &[(&str, &str)]) -> (u16, HeaderMap, Vec<u8>) {
    request_dump(app, "GET", headers).await
}
async fn request_dump(
    app: &Router,
    method: &str,
    headers: &[(&str, &str)],
) -> (u16, HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri("/v1/dumps/osm_place.ndjson.gz");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}
/// The lines of a gzip NDJSON dump.
fn lines(gz: &[u8]) -> Vec<Value> {
    use std::io::Read;
    let mut text = String::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_string(&mut text)
        .unwrap();
    text.lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .collect()
}
fn dump_config(dir: &std::path::Path, downloads: usize) -> Config {
    Config {
        osm: OsmConfig {
            dump_dir: dir.to_str().unwrap().into(),
            dump_downloads_per_hour: downloads,
            ..Default::default()
        },
        ..Default::default()
    }
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
    assert!(view["point"].is_object(), "{view}");
    assert_eq!(view["category"], "place/city");
    // The table carries its licence, so a pg_dump of it does too.
    let notice: String = sqlx::query_scalar("SELECT obj_description('osm_place'::regclass)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(notice.contains("ODbL 1.0"), "{notice}");
    // The daemon's file: gzip NDJSON, served as a download.
    let dir = dump_dir("lines");
    assert_eq!(
        pinakes_core::osm_dump::write(&pool, &dir).await.unwrap(),
        51
    );
    let dumps = api::router(AppState::new(pool.clone(), dump_config(&dir, 6)).unwrap());
    let (status, headers, dump) = download(&dumps, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/gzip");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"osm_place.ndjson.gz\""
    );
    let rows = lines(&dump);
    assert_eq!(rows.len(), 52);
    assert_eq!(rows[0]["licence"], "ODbL-1.0");
    assert!(rows.iter().any(|r| r["name"] == "Berlin"));
    // Every line has exactly the documented fields; no internal column leaks.
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
    // The version a resource pins (and change lines name) is in the line.
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
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        sqlx::query("UPDATE resource SET name='OSM leak' WHERE osm_id IS NOT NULL")
            .execute(&pool)
            .await
            .is_err()
    );
    server.abort();
}
#[sqlx::test(migrations = "../../migrations")]
async fn unknown_objects_are_missing_429_defers_and_category_filters(pool: PgPool) {
    let (state, resolver, server) = mock(&pool).await;
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
    // Without Nominatim's answer an object is missing. The resolver asks the one server it is
    // configured with: its lookup for each object, and no other path (so nothing in place of
    // the lookup, such as the editing API's `/api/0.6/…`).
    let asked = state.calls.lock().await.len();
    for kind in ["node", "way", "relation"] {
        let doc = resolver
            .resolve(&format!("https://www.openstreetmap.org/{kind}/222"))
            .await
            .unwrap();
        assert_eq!(doc.status, Status::Missing);
    }
    let lookups = state.calls.lock().await[asked..].concat();
    for id in ["N222", "W222", "R222"] {
        assert!(lookups.iter().any(|l| l == id), "{id}: {lookups:?}");
    }
    assert!(state.paths.lock().await.is_empty());
    let uri = "https://www.openstreetmap.org/node/240109189";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let doc = resolver.resolve(uri).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    reference(&pool, &id, "city").await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (status, _, found) = call(
        &app,
        "/v1/search?bbox=13,52,14,53&type=Place&category=place%2Fcity&q=Berlin&facet=tags,category,host",
        None,
    )
    .await;
    assert_eq!(status, 200, "{found}");
    assert_eq!(found["items"][0]["id"], id.as_str());
    assert_eq!(found["facets"]["tags"][0]["count"], 1);
    // An OSM place's category is its object's, as `category` takes it.
    assert_eq!(
        found["facets"]["category"],
        serde_json::json!([{"category":"place/city","count":1}])
    );
    assert_eq!(
        found["facets"]["host"],
        serde_json::json!([{"host":"www.openstreetmap.org","site":"OpenStreetMap","count":1}])
    );
    server.abort();
}
#[sqlx::test(migrations = "../../migrations")]
async fn geo_is_final(pool: PgPool) {
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
}
#[sqlx::test(migrations = "../../migrations")]
async fn viewport_distance_time_facets_and_etag(pool: PgPool) {
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
    assert_eq!(view["items"][0]["id"], ids[0]);
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
    assert_eq!(next["items"][0]["id"], ids[1]);
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
    let (_, _, view) = call(
        &app,
        "/v1/search?bbox=13,52,14,53&type=Place&facet=tags",
        None,
    )
    .await;
    assert_eq!(
        view["items"][0]["uri"],
        "https://www.openstreetmap.org/node/240109189"
    );
    assert_eq!(view["items"][0]["attribution"], osm::ATTRIBUTION);
    assert!(view["items"][0]["point"].is_object());
    assert_eq!(mock.calls.lock().await.len(), 1);
    for query in [
        "bbox=0,0,1,1&near=0,0&radius=10",
        "near=0,0&radius=200001",
        "near=91,0&radius=1",
        "near=0,0&radius=NaN",
        "from=2027-01-01T00:00:00Z&to=2026-01-01T00:00:00Z",
    ] {
        assert_eq!(
            call(&app, &format!("/v1/search?{query}"), None).await.0,
            400,
            "{query}"
        );
    }
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
    let feed = changes_of(&app, &id).await;
    assert_eq!(feed["lines"][1]["class"], "renamed", "{feed}");
    assert_eq!(feed["lines"][1]["resource_id"], id.as_str());
    let renamed: (String, String) = sqlx::query_as(
        "SELECT before->>'name',after->>'name' FROM change_document WHERE source='osm' AND class='renamed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(renamed, ("Berlin".into(), "Synthetic renamed city".into()));
    let leaked:i64=sqlx::query_scalar("SELECT count(*) FROM resource_change WHERE source='osm' AND (before-ARRAY['osm_id','version']::text[]<>'{}'::jsonb OR after-ARRAY['osm_id','version']::text[]<>'{}'::jsonb)").fetch_one(&pool).await.unwrap();
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
    let feed = changes_of(&app, &id).await;
    let cancelled: Vec<&Value> = feed["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["class"] == "cancelled")
        .collect();
    assert_eq!(cancelled.len(), 1, "{feed}");
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
    assert!(changes_of(&app, &id).await["lines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["class"] == "renamed" && l["resource_id"] == id.as_str()));
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
async fn batch_failures_are_per_object_and_only_a_5xx_is_charged(pool: PgPool) {
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
        nominatim_url: url.into(),
        requests_per_second: 1000.0,
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
        // Node 3 is cached as missing; drop that so the retry asks Nominatim again.
        sqlx::query("DELETE FROM osm_place WHERE osm_id=3")
            .execute(&pool)
            .await
            .unwrap();
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
        let (attempts,dead,delay,streak,error):(i32,bool,f64,i32,String)=sqlx::query_as("SELECT attempts,dead_letter,extract(epoch FROM next_attempt_at-now())::float8,timeout_streak,last_error FROM resource WHERE osm_id=3").fetch_one(&pool).await.unwrap();
        // A 429 is the operator's own quota: no charge. A 5xx starts a streak of waits, which
        // dead-letters only after days; neither is an attempt, nor is an oversized answer.
        assert_eq!(attempts, 0, "{status}");
        assert!(!dead);
        match status {
            429 => assert_eq!((streak, error.as_str()), (0, "quota")),
            503 => assert_eq!((streak, error.as_str()), (1, "http_5xx")),
            _ => {}
        }
        if status == 413 {
            assert!(delay > 50.0 && delay <= 60.0);
        } else {
            assert!(delay > 3590.0 && delay <= 3600.0);
        }
    }
    server.abort();
}

/// One resolver, as the daemon runs it: after a 429 with `Retry-After: 600` the next batch is
/// put off for the rest of that wait at once, with no request, no sleep under the shared lock
/// and no charge.
#[sqlx::test(migrations = "../../migrations")]
async fn a_cooldown_defers_the_next_batch_at_once(pool: PgPool) {
    use std::sync::atomic::AtomicUsize;
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/lookup",
        get(move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "600")],
                    "retry",
                )
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = OsmConfig {
        nominatim_url: url.into(),
        requests_per_second: 1000.0,
        ..Default::default()
    };
    let resolver = OsmResolver::new(pool.clone(), config).unwrap();
    let cfg = QueueConfig::default();
    for id in [11, 12] {
        store::ensure_resource(&pool, &format!("https://www.openstreetmap.org/node/{id}"))
            .await
            .unwrap();
        let jobs = queue::claim_batch(&pool, "osm", &cfg, 50).await.unwrap();
        assert_eq!(jobs.len(), 1);
        let started = std::time::Instant::now();
        queue::run_batch(&pool, &resolver, &cfg, &jobs)
            .await
            .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let (attempts, dead, delay, error): (i32, bool, f64, Option<String>) = sqlx::query_as(
            "SELECT attempts,dead_letter,extract(epoch FROM next_attempt_at-now())::float8,last_error FROM resource WHERE osm_id=$1",
        )
        .bind(id as i64)
        .fetch_one(&pool)
        .await
        .unwrap();
        // The 429 is the operator's own quota, and the job put off by the cooldown waits for
        // the daemon's budget: neither is charged.
        assert_eq!((attempts, dead), (0, false), "node {id}");
        assert!(delay > 590.0 && delay <= 601.0, "node {id}: {delay}");
        assert_eq!(
            error.as_deref(),
            Some(if id == 11 { "quota" } else { "rate_limited" })
        );
    }
    // The second batch sent nothing.
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

/// An upstream that refuses Pinakes (401 or 403: a key expired or out of quota) says nothing of
/// the link: no charge, no dead letter, read again later. Another answer that is no place data
/// is the link's failure, stored by its status class (a redirect as `http_3xx`).
#[sqlx::test(migrations = "../../migrations")]
async fn a_refused_upstream_key_is_not_the_link_s_failure(pool: PgPool) {
    use std::sync::atomic::AtomicU16;
    let code = Arc::new(AtomicU16::new(401));
    let mode = code.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/lookup",
        get(move || {
            let mode = mode.clone();
            async move { StatusCode::from_u16(mode.load(Ordering::SeqCst)).unwrap() }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = OsmConfig {
        nominatim_url: url.into(),
        requests_per_second: 1000.0,
        ..Default::default()
    };
    store::ensure_resource(&pool, "https://www.openstreetmap.org/node/21")
        .await
        .unwrap();
    let cfg = QueueConfig::default();
    let refusals = || {
        pinakes_core::sources::token_refusals()
            .into_iter()
            .find(|(source, _)| *source == "osm")
            .map_or(0, |(_, n)| n)
    };
    for (status, class, attempts) in [
        (401, "upstream_denied", 0),
        (403, "upstream_denied", 0),
        (400, "http_4xx", 1),
        // A redirect (the client follows none) is stored as one, not as a client error.
        (302, "http_3xx", 2),
    ] {
        code.store(status, Ordering::SeqCst);
        let refused_before = refusals();
        sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE osm_id=21")
            .execute(&pool)
            .await
            .unwrap();
        let jobs = queue::claim_batch(&pool, "osm", &cfg, 50).await.unwrap();
        assert_eq!(jobs.len(), 1, "{status}");
        let resolver = OsmResolver::new(pool.clone(), config.clone()).unwrap();
        queue::run_batch(&pool, &resolver, &cfg, &jobs)
            .await
            .unwrap();
        let row: (i32, bool, Option<String>) =
            sqlx::query_as("SELECT attempts,dead_letter,last_error FROM resource WHERE osm_id=21")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            row,
            (attempts, false, Some(class.to_owned())),
            "status {status}"
        );
        // A refused key is counted where the `ProviderKeyRefused` alert reads it; no other
        // answer is (no other test of this file is refused).
        assert_eq!(
            refusals() - refused_before,
            u64::from(matches!(status, 401 | 403)),
            "status {status}"
        );
    }
    server.abort();
}

#[sqlx::test(migrations = "../../migrations")]
async fn object_spellings_are_aliases_of_one_object(pool: PgPool) {
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
    // Each spelling keeps its own ID but is an alias of the canonical object URL.
    for id in &ids {
        assert_eq!(
            pinakes_core::alias::resolve_id(&pool, id).await.unwrap(),
            ids[0]
        );
    }
    assert_eq!(jobs.len(), 1);
    assert_eq!(mock.calls.lock().await[0], vec!["N240109189"]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM osm_place")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM resource WHERE name IS NULL AND point IS NULL AND doc->>'type'='Place'").fetch_one(&pool).await.unwrap(),1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource WHERE osm_id IS NULL AND resolver IS NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        2
    );
    assert!(
        sqlx::query("UPDATE resource SET doc='{}' WHERE osm_id IS NOT NULL")
            .execute(&pool)
            .await
            .is_err()
    );
    server.abort();
}

/// The dump endpoint serves the daemon's file and nothing else: 503 before the first file, then
/// 200 with validators, 304 on revalidation, byte ranges, 429 over the hourly budget (counting
/// transfers only, not HEAD, 304 or 416), all with
/// the database closed. A rewrite replaces the file atomically: a download in progress keeps the
/// file it opened, and a failed write leaves the previous file and no temporary one.
#[sqlx::test(migrations = "../../migrations")]
async fn the_dump_is_a_static_file_written_atomically(pool: PgPool) {
    let (_, resolver, server) = mock(&pool).await;
    let node = "https://www.openstreetmap.org/node/240109189";
    let id = store::ensure_resource(&pool, node).await.unwrap();
    let doc = resolver.resolve(node).await.unwrap();
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    let dir = dump_dir("serve");
    // A pool of its own that is closed before any download: no request touches the database.
    let api_pool = sqlx::postgres::PgPoolOptions::new()
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let app = api::router(AppState::new(api_pool.clone(), dump_config(&dir, 4)).unwrap());
    api_pool.close().await;
    let (status, headers, _) = download(&app, &[]).await;
    assert_eq!(status, 503);
    assert!(headers.contains_key("retry-after"));
    pinakes_core::osm_dump::write(&pool, &dir).await.unwrap();
    let (status, headers, first) = download(&app, &[]).await;
    assert_eq!(status, 200);
    let etag = headers["etag"].to_str().unwrap().to_owned();
    let modified = headers["last-modified"].to_str().unwrap().to_owned();
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_eq!(headers["content-length"], first.len().to_string().as_str());
    assert_eq!(lines(&first)[0]["licence"], "ODbL-1.0");
    assert_eq!(lines(&first)[1]["osm_id"], 240109189);
    // Revalidation is free: it neither reads the file nor counts as a download.
    for _ in 0..5 {
        assert_eq!(download(&app, &[("if-none-match", &etag)]).await.0, 304);
        assert_eq!(
            download(&app, &[("if-modified-since", &modified)]).await.0,
            304
        );
    }
    let (status, headers, part) = download(&app, &[("range", "bytes=0-9")]).await;
    assert_eq!((status, part.as_slice()), (206, &first[..10]));
    assert_eq!(
        headers["content-range"],
        format!("bytes 0-9/{}", first.len()).as_str()
    );
    let (status, _, tail) = download(&app, &[("range", "bytes=-5"), ("if-range", &etag)]).await;
    assert_eq!((status, tail.as_slice()), (206, &first[first.len() - 5..]));
    let (status, headers, _) =
        download(&app, &[("range", &format!("bytes={}-", first.len()))]).await;
    assert_eq!(status, 416);
    assert_eq!(
        headers["content-range"],
        format!("bytes */{}", first.len()).as_str()
    );
    // HEAD transfers no file: free, like revalidation and the unsatisfiable range.
    for _ in 0..5 {
        let (status, headers, body) = request_dump(&app, "HEAD", &[]).await;
        assert_eq!((status, body.len()), (200, 0));
        assert_eq!(headers["etag"].to_str().unwrap(), etag);
    }
    // The fourth download this hour (after the file and two ranges), then over budget.
    assert_eq!(download(&app, &[]).await.0, 200);
    assert_eq!(download(&app, &[]).await.0, 429);
    // A download in progress keeps the file it opened while a newer one is renamed into place.
    let app = api::router(AppState::new(pool.clone(), dump_config(&dir, 100)).unwrap());
    let mut request = Request::builder()
        .uri("/v1/dumps/osm_place.ndjson.gz")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let open = app.clone().oneshot(request).await.unwrap();
    sqlx::query("INSERT INTO osm_place(osm_type,osm_id,type,type_path,doc,fetch_doc,fetched_at,refresh_at,status) VALUES('node',1,'Place',ARRAY['Thing','Place'],'{}','{}',now(),now(),'ok')")
        .execute(&pool)
        .await
        .unwrap();
    pinakes_core::osm_dump::write(&pool, &dir).await.unwrap();
    let old = open.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        old.as_ref(),
        first.as_slice(),
        "the open download is the old file, whole"
    );
    let (_, headers, second) = download(&app, &[]).await;
    assert_eq!(lines(&second).len(), lines(&first).len() + 1);
    assert_ne!(headers["etag"].to_str().unwrap(), etag);
    // A failed write keeps the previous file and leaves no temporary one.
    sqlx::query("ALTER TABLE osm_place RENAME TO osm_place_gone")
        .execute(&pool)
        .await
        .unwrap();
    assert!(pinakes_core::osm_dump::write(&pool, &dir).await.is_err());
    assert_eq!(
        std::fs::read(pinakes_core::osm_dump::path(&dir)).unwrap(),
        second
    );
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    let _ = std::fs::remove_dir_all(&dir);
    server.abort();
}

/// Each writer has its own temporary file; the lock file keeps one writer per directory, and the
/// writer holding it clears the temporary files an interrupted one left.
#[sqlx::test(migrations = "../../migrations")]
async fn one_writer_per_directory_and_leftover_temporary_files_go(pool: PgPool) {
    use pinakes_core::osm_dump::{lock, path, temp_path, write, FILE};
    let dir = dump_dir("temps");
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (temp_path(&dir), temp_path(&dir));
    assert_ne!(a, b);
    let name = a.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(
        name.starts_with(&format!("{FILE}.{}.", std::process::id())) && name.ends_with(".tmp"),
        "{name}"
    );
    std::fs::write(path(&dir), b"the dump").unwrap();
    // Another daemon holds the lock: no write, the file and its temporary file stay.
    let other = lock(&dir).unwrap().expect("free");
    std::fs::write(&a, b"another writer's partial file").unwrap();
    assert!(lock(&dir).unwrap().is_none());
    let refused = write(&pool, &dir).await.unwrap_err();
    assert!(format!("{refused:#}").contains("lock"), "{refused:#}");
    assert_eq!(std::fs::read(path(&dir)).unwrap(), b"the dump");
    assert!(a.exists());
    // Released: the next writer clears the leftovers and publishes a whole file.
    drop(other);
    std::fs::write(dir.join(format!("{FILE}.1.dead.tmp")), b"partial").unwrap();
    write(&pool, &dir).await.unwrap();
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [FILE, pinakes_core::osm_dump::LOCK]);
    assert_ne!(std::fs::read(path(&dir)).unwrap(), b"the dump");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The daemon writes a missing or stale file at startup and leaves a fresh one alone.
#[sqlx::test(migrations = "../../migrations")]
async fn startup_writes_a_missing_or_stale_dump(pool: PgPool) {
    use pinakes_core::osm_dump::{due_in, path, run};
    let day = std::time::Duration::from_secs(86400);
    let dir = dump_dir("startup");
    assert_eq!(due_in(&dir, day), std::time::Duration::ZERO, "missing");
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(run(pool.clone(), dir.clone(), day, stopped.clone()));
    for _ in 0..100 {
        if path(&dir).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(path(&dir).exists(), "startup wrote the missing file");
    assert!(due_in(&dir, day) > day - std::time::Duration::from_secs(60));
    stop.send(true).unwrap();
    task.await.unwrap();
    // A write the stop cancelled at its rename still finishes it, holding the lock.
    for _ in 0..100 {
        if pinakes_core::osm_dump::lock(&dir).unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Two days old: stale, so the next start rewrites it.
    let file = std::fs::File::options()
        .write(true)
        .open(path(&dir))
        .unwrap();
    file.set_modified(std::time::SystemTime::now() - 2 * day)
        .unwrap();
    drop(file);
    assert_eq!(due_in(&dir, day), std::time::Duration::ZERO, "stale");
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(run(pool.clone(), dir.clone(), day, stopped));
    for _ in 0..100 {
        if due_in(&dir, day) > std::time::Duration::ZERO {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        due_in(&dir, day) > day - std::time::Duration::from_secs(60),
        "rewritten"
    );
    stop.send(true).unwrap();
    task.await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Startup removes an interrupted write's temporary file even when no write is due.
#[sqlx::test(migrations = "../../migrations")]
async fn startup_removes_leftover_temporary_files(pool: PgPool) {
    use pinakes_core::osm_dump::{path, run, FILE};
    let day = std::time::Duration::from_secs(86400);
    let dir = dump_dir("leftovers");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(path(&dir), b"a fresh dump").unwrap();
    let leftover = dir.join(format!("{FILE}.1.dead.tmp"));
    std::fs::write(&leftover, b"partial").unwrap();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(run(pool, dir.clone(), day, stopped));
    for _ in 0..100 {
        if !leftover.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!leftover.exists(), "the leftover went at startup");
    assert_eq!(
        std::fs::read(path(&dir)).unwrap(),
        b"a fresh dump",
        "no write was due"
    );
    stop.send(true).unwrap();
    task.await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Shutdown does not wait for a write in progress: it is cancelled, the previous state stays, and
/// the next writer removes its temporary file.
#[sqlx::test(migrations = "../../migrations")]
async fn shutdown_cancels_a_dump_write_in_progress(pool: PgPool) {
    use pinakes_core::osm_dump::{path, run, write};
    let day = std::time::Duration::from_secs(86400);
    let dir = dump_dir("shutdown");
    // Another session holds the table, so the write blocks on its first read.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE osm_place IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(run(pool.clone(), dir.clone(), day, stopped));
    let temp = || {
        std::fs::read_dir(&dir).ok().is_some_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        })
    };
    for _ in 0..100 {
        if temp() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(temp(), "the write started");
    stop.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("shutdown waited for the write")
        .unwrap();
    blocker.rollback().await.unwrap();
    assert!(!path(&dir).exists());
    write(&pool, &dir).await.unwrap();
    assert!(path(&dir).exists());
    assert!(
        !temp(),
        "the next writer removed the cancelled one's temporary file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_osm_member_keeps_its_own_card_and_the_group_holds_no_osm_fact(pool: PgPool) {
    let (_, resolver, server) = mock(&pool).await;
    let node = "https://www.openstreetmap.org/node/240109189";
    let osm_id = store::ensure_resource(&pool, node).await.unwrap();
    let doc = resolver.resolve(node).await.unwrap();
    store::save_document(&pool, &osm_id, 0, &doc).await.unwrap();
    let uri = "https://example.org/berlin";
    let page = store::ensure_resource(&pool, uri).await.unwrap();
    let mut doc = pinakes_core::model::ResourceMeta::new(uri, "web", 3600);
    doc.name = Some("Own page".into());
    store::save_document(&pool, &page, 0, &doc).await.unwrap();
    let id = "c".repeat(32);
    pinakes_core::groups::curate(
        &pool,
        &id,
        pinakes_core::groups::Curation {
            members: vec![
                pinakes_core::groups::Member {
                    resource_id: osm_id.clone(),
                    occurrence: None,
                },
                pinakes_core::groups::Member {
                    resource_id: page.clone(),
                    occurrence: None,
                },
            ],
        },
        &Default::default(),
    )
    .await
    .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    // The page keeps its own card; its hint names the OSM member by link only.
    let (_, _, doc) = call(&app, &format!("/v1/resources/{page}"), None).await;
    assert_eq!(doc["name"], "Own page");
    assert!(doc.get("attribution").is_none(), "{doc}");
    assert_eq!(doc["group"], json!({"id": id, "size": 2}));
    // Its entry carries the map its own card shows (`embed`), read from the OSM version.
    let mut entry = doc["also_on"][0].clone();
    let embed = entry.as_object_mut().unwrap().remove("embed").unwrap();
    assert_eq!(doc["also_on"].as_array().map(Vec::len), Some(1), "{doc}");
    assert_eq!(
        entry,
        json!({"id": osm_id, "uri": node, "site": "OpenStreetMap"})
    );
    assert_eq!(
        (&embed["provider"], &embed["kind"]),
        (&json!("openstreetmap"), &json!("map")),
        "{doc}"
    );
    assert!(
        embed["src"]
            .as_str()
            .unwrap()
            .starts_with("https://www.openstreetmap.org/export/embed.html?bbox="),
        "{doc}"
    );
    // The group read lists both members' own cards; the OSM card carries its facts and credit.
    let (_, _, read) = call(&app, &format!("/v1/groups/{id}"), None).await;
    assert_eq!(read["curated"], true);
    assert_eq!(read["keys"], json!([]));
    let osm = read["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == osm_id.as_str())
        .unwrap();
    assert_eq!(osm["name"], "Berlin", "{read}");
    assert_eq!(osm["attribution"], osm::ATTRIBUTION);
    // The group tables hold membership only, and the boundary script refuses anything more.
    if let Some(out) = boundary_script(&pool).await {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        sqlx::query("ALTER TABLE \"group\" ADD COLUMN name text")
            .execute(&pool)
            .await
            .unwrap();
        let out = boundary_script(&pool).await.unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("more than membership"));
    }
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

/// Every OSM URL style for one object (Berlin, node 240109189).
#[sqlx::test(migrations = "../../migrations")]
async fn osm_url_styles_resolve_as_the_object_a_point_or_one_final_refusal(pool: PgPool) {
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
    let root = pinakes_core::model::identity(canonical).unwrap().0;
    for uri in objects {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        // Every spelling is the object itself: an alias of the canonical object URL.
        assert_eq!(
            pinakes_core::alias::resolve_id(&pool, &id).await.unwrap(),
            root,
            "{uri}"
        );
    }
    let (resolver_name, key): (String, String) =
        sqlx::query_as("SELECT resolver,osm_type||'/'||osm_id FROM resource WHERE id=$1")
            .bind(&root)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (resolver_name.as_str(), key.as_str()),
        ("osm", "node/240109189")
    );
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    // One Nominatim object and one derivative row serve every spelling.
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
    let geo = geo::GeoResolver;
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
        egress: common::offline_egress(Default::default()),
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

#[sqlx::test(migrations = "../../migrations")]
async fn reextract_bypasses_the_osm_cache(pool: PgPool) {
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
    assert_eq!(queue::reextract(&pool, &["osm".into()]).await.unwrap(), 1);
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    // The object was looked up and mapped again, not served from the cache.
    assert_eq!(mock.calls.lock().await.len(), 2);
    // An unknown route is refused.
    assert!(queue::reextract(&pool, &["nonsense".into()]).await.is_err());
    let _ = id;
    server.abort();
}

/// Re-extracting osm.org object shortlinks stored as points makes them aliases of their object,
/// and changeset/note shortlinks drop their point facts on the way to one final refusal.
#[sqlx::test(migrations = "../../migrations")]
async fn reextract_makes_object_shortlinks_aliases_and_refuses_osm_host_pages(pool: PgPool) {
    let (_, resolver, server) = mock(&pool).await;
    let canonical = "https://www.openstreetmap.org/node/240109189";
    let short = "https://osm.org/go/0GAjIv8h?node=240109189";
    let changeset = "https://osm.org/go/0EEQjE--?changeset=1";
    let note = "https://www.openstreetmap.org/go/0EEQjE--?note=5";
    let mut ids = HashMap::new();
    for uri in [canonical, short, changeset, note] {
        ids.insert(uri, store::ensure_resource(&pool, uri).await.unwrap());
    }
    // Rows the geo route stored: final `ok` points at the shortlink's map centre. Re-extraction
    // applies the current rule; a new object shortlink is an alias from the start.
    for uri in [short, changeset, note] {
        sqlx::query("DELETE FROM resource_alias WHERE alias_id=$1")
            .bind(&ids[uri])
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE resource SET resolver='geo',osm_type=NULL,osm_id=NULL,doc='{}',type_path='{}',status='pending',generation=0 WHERE id=$1")
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
    assert_eq!(queue::reextract(&pool, &["all".into()]).await.unwrap(), 4);
    // The object shortlink becomes an alias of its object: never routed, no OSM envelope.
    let (route, ..) = state(ids[short].clone()).await;
    assert_eq!(route, None);
    assert_eq!(
        pinakes_core::alias::resolve_id(&pool, &ids[short])
            .await
            .unwrap(),
        ids[canonical]
    );
    // Unresolved until the OSM worker maps it, so the boundary proof holds meanwhile.
    for uri in [changeset, note] {
        let resolved: (bool, bool, Option<String>) = sqlx::query_as(
            "SELECT resolved_at IS NOT NULL,refresh_at IS NOT NULL,osm_version FROM resource WHERE id=$1",
        )
        .bind(&ids[uri])
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(resolved, (false, false, None), "{uri}");
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

    sqlx::query("UPDATE resource SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let jobs = queue::claim_batch(&pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    queue::run_batch(&pool, &resolver, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    let web = pinakes_core::resolver::web::WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        events: None,
        egress: common::offline_egress(Default::default()),
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

/// `scripts/verify-osm-boundary` against this test database. Where the tools it runs (`psql`,
/// `pg_dump`, `rg`, `python3`) are missing it fails under `scripts/test` and is None, with a message,
/// under a bare `cargo test`.
async fn boundary_script(pool: &PgPool) -> Option<std::process::Output> {
    for tool in ["psql", "pg_dump", "rg", "python3"] {
        if std::process::Command::new(tool)
            .arg("--version")
            .output()
            .is_err()
        {
            let message =
                format!("scripts/verify-osm-boundary needs `{tool}`, which is not installed");
            // scripts/test requires the tools; a bare `cargo test` skips the check.
            assert!(
                std::env::var_os("PINAKES_REQUIRE_BOUNDARY_TOOLS").is_none(),
                "{message}"
            );
            eprintln!("SKIPPED: {message}");
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

/// `scripts/verify-osm-boundary` accepts the geo change lines of OSM-host location
/// links (as the schema's `change_web_osm_boundary` does) and still refuses web facts on OSM
/// hosts.
#[sqlx::test(migrations = "../../migrations")]
async fn boundary_script_accepts_location_link_history(pool: PgPool) {
    let marker = "https://www.openstreetmap.org/?mlat=52.5186925&mlon=13.3996024";
    let id = store::ensure_resource(&pool, marker).await.unwrap();
    let job = queue::claim(&pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    queue::run_job(&pool, &geo::GeoResolver, &QueueConfig::default(), &job)
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

/// A resolved OSM-route row that re-extraction moves to web (a Nominatim alias with a lowercase
/// osmtype, which the OSM route refuses) loses every resolution mark, not only its facts.
#[sqlx::test(migrations = "../../migrations")]
async fn reextract_unresolves_an_osm_row_moving_to_web(pool: PgPool) {
    let alias = "https://nominatim.openstreetmap.org/details?osmtype=n&osmid=240109189";
    let id = store::ensure_resource(&pool, alias).await.unwrap();
    // An OSM alias resolved through the shared osm_place row.
    sqlx::query("UPDATE resource SET resolver='osm',osm_type='node',osm_id=240109189,type_path=NULL,doc=jsonb_build_object('type','Place','url',uri,'osm_id','node/240109189','source','osm','status','ok'),status='ok',resolved_at=now(),refresh_at=now()+interval '30 days',next_attempt_at=now()+interval '30 days',osm_version='0123456789abcdef0123456789abcdef' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let marks = || async {
        sqlx::query_as::<_, (bool, bool, Option<String>)>(
            "SELECT resolved_at IS NOT NULL,refresh_at IS NOT NULL,osm_version FROM resource WHERE id=$1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(
        marks().await,
        (true, true, Some("0123456789abcdef0123456789abcdef".into()))
    );
    assert_eq!(queue::reextract(&pool, &["all".into()]).await.unwrap(), 1);
    let (route, status, doc): (String, String, Value) =
        sqlx::query_as("SELECT resolver,status,doc FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (route.as_str(), status.as_str(), doc),
        ("web", "pending", json!({}))
    );
    assert_eq!(marks().await, (false, false, None));
    if let Some(proof) = boundary_script(&pool).await {
        assert!(
            proof.status.success(),
            "{}",
            String::from_utf8_lossy(&proof.stderr)
        );
    }
}
