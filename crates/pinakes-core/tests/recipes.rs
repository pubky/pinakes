//! Step 6 round 3 E2: every named example in the public OpenAPI document runs against a fixture
//! built from the same staging seed URIs and returns data, and docs/api.md lists every recipe.
use axum::{body::Body, extract::ConnectInfo, http::Request, routing::get, Json, Router};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, OsmConfig, QueueConfig},
    model::{identity, Point, ResourceMeta},
    queue, store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use tower::ServiceExt;

/// A seed identity: it tagged every seed entry, and the reach example's viewer follows it.
const SEED: &str = "pubkyseedauthorpubkyseedauthorpubkyseedauthorpubkyse";

fn recorded(url: &str, content_type: &str, body: &[u8]) -> pinakes_core::resolver::egress::Fetched {
    pinakes_core::resolver::egress::Fetched {
        permanent_redirect: false,
        truncated: false,
        url: url.parse().unwrap(),
        body: body.to_vec(),
        content_type: content_type.into(),
        status: 200,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    }
}

async fn save(pool: &PgPool, doc: &ResourceMeta) -> String {
    let id = store::ensure_resource(pool, &doc.url).await.unwrap();
    store::save_document(pool, &id, 0, doc).await.unwrap();
    id
}

fn card(uri: &str, source: &str, kind: &str, name: &str) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, source, 7 * 86400);
    doc.set_type(kind);
    doc.name = Some(name.into());
    doc
}

/// The seed entries the recipes touch, resolved offline: recorded registry responses, an OSM
/// cache entry, a parsed holiday calendar and synthetic cards, each tagged with its seed labels.
async fn fixture(pool: &PgPool) {
    let now = chrono::Utc::now();
    let lugano = Point {
        lat: 46.0037,
        lon: 8.9511,
    };
    let mut docs = vec![
        card(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "media",
            "VideoObject",
            "Rick Astley - Never Gonna Give You Up",
        ),
        card("isbn:9781119473862", "isbn", "Book", "Mastering Bitcoin"),
        card(
            "https://en.wikipedia.org/wiki/Bitcoin",
            "web",
            "Article",
            "Bitcoin - Wikipedia",
        ),
    ];
    let mut event = card(
        "https://luma.com/376whqyw",
        "event_platform",
        "Event",
        "Bitcoin conference Lugano",
    );
    event.start = Some(now + chrono::Duration::days(30));
    event.end = Some(now + chrono::Duration::days(30) + chrono::Duration::hours(8));
    event.location = Some(lugano.clone());
    docs.push(event);
    let mut wiki = card(
        "https://en.wikipedia.org/wiki/Lugano",
        "web",
        "Article",
        "Lugano - Wikipedia",
    );
    wiki.location = Some(lugano.clone());
    docs.push(wiki);
    for doc in &docs {
        save(pool, doc).await;
    }
    // A geo pin, resolved by the offline geo resolver.
    store::ensure_resource(pool, "geo:46.0037,8.9511")
        .await
        .unwrap();
    let job = queue::claim(pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    queue::run_job(
        pool,
        &pinakes_core::resolver::geo::GeoResolver::default(),
        &QueueConfig::default(),
        &job,
    )
    .await
    .unwrap();
    // Berlin from a fresh osm_place cache entry: the OSM worker maps it without Nominatim.
    let berlin = "https://www.openstreetmap.org/node/240109189";
    let nominatim: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/nominatim.json")).unwrap();
    let place = pinakes_core::resolver::osm::map_place(
        nominatim.iter().find(|v| v["osm_id"] == 240109189).unwrap(),
        30 * 86400,
    )
    .unwrap();
    let at = place.location.as_ref().unwrap();
    sqlx::query("INSERT INTO osm_place(osm_type,osm_id,point,name,category,type,type_path,doc,fetched_at,refresh_at,status) VALUES('node',240109189,ST_SetSRID(ST_MakePoint($1,$2),4326),$3,$4,$5,$6,$7,now(),now()+interval '30 days','ok')")
        .bind(at.lon)
        .bind(at.lat)
        .bind(&place.name)
        .bind(&place.category)
        .bind(&place.type_name)
        .bind(&place.type_path)
        .bind(serde_json::to_value(&place).unwrap())
        .execute(pool)
        .await
        .unwrap();
    store::ensure_resource(pool, berlin).await.unwrap();
    let osm = pinakes_core::resolver::osm::OsmResolver::new(
        pool.clone(),
        OsmConfig {
            nominatim_url: "http://127.0.0.1:9".into(),
            api_url: "http://127.0.0.1:9".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let jobs = queue::claim_batch(pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    queue::run_batch(pool, &osm, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    let state: (String, Option<String>) =
        sqlx::query_as("SELECT status,last_error FROM resource WHERE uri=$1")
            .bind(berlin)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(state.0, "ok", "{state:?}");
    // The Swiss holiday feed with two dates ahead.
    let feed = "https://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics";
    let day = |d: i64| {
        (now + chrono::Duration::days(d))
            .format("%Y%m%d")
            .to_string()
    };
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:holiday-a\r\nDTSTART;VALUE=DATE:{}\r\nSUMMARY:Holiday A\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:holiday-b\r\nDTSTART;VALUE=DATE:{}\r\nSUMMARY:Holiday B\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        day(20),
        day(60)
    );
    let calendar =
        pinakes_core::event::calendar::parse(feed, ics.as_bytes(), &Default::default(), now)
            .unwrap();
    let feed_id = store::ensure_resource(pool, feed).await.unwrap();
    store::save_document(pool, &feed_id, 0, &calendar)
        .await
        .unwrap();
    // The Nature article and its DOI card form one group (recorded Crossref and page head).
    let csl = include_bytes!("fixtures/registry/crossref-nature12373.csl.json");
    let doi = pinakes_core::resolver::cards::parse_doi(
        "doi:10.1038/nature12373",
        &recorded(
            "https://api.crossref.org/v1/works/x/transform",
            "application/json",
            csl,
        ),
        86400,
    )
    .unwrap();
    save(pool, &doi).await;
    let landing = "https://www.nature.com/articles/nature12373";
    let page = pinakes_core::resolver::web::extract(
        landing,
        &recorded(
            landing,
            "text/html",
            include_bytes!("fixtures/registry/nature12373-head.html"),
        ),
        86400,
    )
    .doc;
    save(pool, &page).await;
    // The book from its recorded Open Library record.
    let book = pinakes_core::resolver::cards::parse_isbn(
        "isbn:9780140328721",
        "9780140328721",
        &recorded(
            "https://openlibrary.org/api/books?bibkeys=ISBN:9780140328721&format=json&jscmd=data",
            "application/json",
            include_bytes!("fixtures/registry/openlibrary-9780140328721.json"),
        ),
        86400,
    )
    .unwrap();
    save(pool, &book).await;
    // Seed labels, by the seed identity.
    for (uri, labels) in [
        (
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            &["music", "video"][..],
        ),
        ("https://luma.com/376whqyw", &["conference", "bitcoin"]),
        ("isbn:9781119473862", &["books", "bitcoin"]),
        (
            "https://en.wikipedia.org/wiki/Bitcoin",
            &["bitcoin", "wikipedia"],
        ),
        (
            "https://en.wikipedia.org/wiki/Lugano",
            &["lugano", "wikipedia"],
        ),
        ("geo:46.0037,8.9511", &["lugano", "pubky"]),
        (berlin, &["berlin", "travel"]),
        (feed, &["calendar", "switzerland"]),
        (landing, &["science", "physics"]),
        ("doi:10.1038/nature12373", &["science", "physics"]),
        ("isbn:9780140328721", &["books", "children"]),
    ] {
        let id = identity(uri).unwrap().0;
        for label in labels {
            sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag',$3,$4,now())")
                .bind(&id)
                .bind(SEED)
                .bind(label)
                .bind(format!("/pub/pubky.app/tags/{id}-{label}"))
                .execute(pool)
                .await
                .unwrap();
        }
        let mut tx = pool.begin().await.unwrap();
        store::recompute_scores(&mut tx, std::slice::from_ref(&id))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
}

async fn following() -> Json<Vec<String>> {
    Json(vec![SEED.to_owned()])
}

async fn mock_nexus() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v0/user/{id}/following", get(following))
        .with_state(());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), task)
}

async fn send(app: &Router, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
    let mut request = Request::builder().method(method).uri(target);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let mut request = request
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string())),
    )
}

/// One request per example name, rebuilt from the served document exactly as Swagger UI sends
/// it: body examples as the JSON body, parameter examples by name with empty values left out.
fn examples(doc: &Value) -> BTreeMap<String, (String, String, Option<Value>)> {
    let mut requests = BTreeMap::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            let mut names = BTreeSet::new();
            let body = &operation["requestBody"]["content"]["application/json"]["examples"];
            names.extend(
                body.as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k.clone()),
            );
            let parameters = operation["parameters"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for p in &parameters {
                names.extend(
                    p["examples"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(k, _)| k.clone()),
                );
            }
            for name in names {
                let mut target = path.clone();
                let mut query = vec![];
                for p in &parameters {
                    let Some(example) = p["examples"].get(&name) else {
                        continue;
                    };
                    let value = match &example["value"] {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if value.is_empty() {
                        continue;
                    }
                    let key = p["name"].as_str().unwrap();
                    if p["in"] == "path" {
                        target = target.replace(&format!("{{{key}}}"), &value);
                    } else {
                        query.push(format!("{key}={value}"));
                    }
                }
                if !query.is_empty() {
                    target = format!("{target}?{}", query.join("&"));
                }
                assert!(
                    !target.contains('{'),
                    "{name}: unfilled path parameter in {target}"
                );
                let value = body.get(&name).map(|e| e["value"].clone());
                let previous =
                    requests.insert(name.clone(), (method.to_ascii_uppercase(), target, value));
                assert!(previous.is_none(), "example name {name} is used twice");
            }
        }
    }
    requests
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_named_example_returns_data(pool: PgPool) {
    fixture(&pool).await;
    let (nexus_url, nexus) = mock_nexus().await;
    let app = api::router(
        AppState::new(
            pool,
            Config {
                nexus_url,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let (status, doc) = send(&app, "GET", "/v1/openapi.json", None).await;
    assert_eq!(status, 200);
    assert!(!doc.to_string().contains("\"parameters\":null"));
    let requests = examples(&doc);
    let recipes = pinakes_core::recipes::recipes();
    assert_eq!(
        requests.keys().cloned().collect::<BTreeSet<_>>(),
        recipes
            .iter()
            .map(|r| r.name.clone())
            .collect::<BTreeSet<_>>(),
        "every recipe is a named example and every named example a recipe"
    );
    for recipe in &recipes {
        let (method, target, body) = &requests[&recipe.name];
        assert_eq!(method, &recipe.method, "{}", recipe.name);
        let (status, response) = send(&app, method, target, body.as_ref()).await;
        assert_eq!(status, 200, "{} {method} {target}: {response}", recipe.name);
        let count = |v: &Value| v.as_array().map_or(0, Vec::len);
        let found = match recipe.expect.as_str() {
            "card" => usize::from(response["name"].is_string()),
            "items" => count(&response).max(count(&response["items"])),
            "map" => count(&response["clusters"]) + count(&response["points"]),
            "group" => usize::from(response["member_count"].as_i64() >= Some(2)),
            "lines" => count(&response["lines"]),
            other => panic!("{}: unknown expectation {other}", recipe.name),
        };
        assert!(
            found > 0,
            "{} {method} {target} returned no data: {response}",
            recipe.name
        );
    }
    nexus.abort();
}

/// E3: docs/api.md lists every recipe's request line, so the recipes there cannot drift.
#[test]
fn the_api_docs_list_every_recipe() {
    let docs = include_str!("../../../docs/api.md");
    let section = docs
        .split("## Recipes for pubky.app")
        .nth(1)
        .expect("a 'Recipes for pubky.app' section");
    for recipe in pinakes_core::recipes::recipes() {
        let line = recipe.request_line();
        assert!(section.contains(&line), "docs/api.md lacks {line}");
        assert!(
            recipe
                .query
                .values()
                .chain(recipe.params.values())
                .all(|v| v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".,:_-".contains(&b))),
            "{}: a query value needs URL encoding",
            recipe.name
        );
    }
}
