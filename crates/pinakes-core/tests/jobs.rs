//! Job postings: recorded career pages become JobPosting cards with their facts; a posting's
//! workplace is placed on the map, and search finds postings by place, remoteness and date posted,
//! leaving closed ones out unless asked.
use axum::{
    body::Body,
    extract::{ConnectInfo, Query},
    http::Request,
    routing::get,
    Router,
};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, EgressConfig, EventConfig, OsmConfig},
    model::ResourceMeta,
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        osm::OsmResolver,
        web::{self, WebResolver},
        Resolver,
    },
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{collections::HashMap, io::Read, net::SocketAddr, sync::Arc};
use tower::ServiceExt;
mod common;

const ASHBY: &str = "https://jobs.ashbyhq.com/ashby/7458d4e9-da2e-47bd-98cb-adfda43d42b2";
const LEVER: &str = "https://jobs.lever.co/palantir/6ed76ce8-4156-4b60-b120-403538bd66cd";
const RIVER: &str = "https://bitcoinerjobs.com/job/1682142-staff-software-engineer-river";
const BATTERY: &str = "https://bitcoinerjobs.com/job/2563734-investment-associateanalyst-battery";

fn recorded(file: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/web/jobs/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).unwrap();
    if !file.ends_with(".gz") {
        return bytes;
    }
    let mut out = vec![];
    flate2::read::GzDecoder::new(bytes.as_slice())
        .read_to_end(&mut out)
        .unwrap();
    out
}
fn page(uri: &str, body: Vec<u8>) -> Fetched {
    Fetched {
        permanent_redirect: false,
        truncated: false,
        url: uri.parse().unwrap(),
        status: 200,
        content_type: "text/html".into(),
        body,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    }
}
fn extracted(uri: &str, file: &str) -> ResourceMeta {
    web::extract(uri, &page(uri, recorded(file)), 3600)
        .unwrap()
        .doc
}

#[test]
fn career_pages_become_postings_with_their_facts() {
    let doc = extracted(ASHBY, "ashby-7458d4e9.html.gz");
    assert_eq!(doc.type_name, "JobPosting");
    assert_eq!(doc.name.as_deref(), Some("Engineering Manager - EU"));
    assert!(doc
        .description
        .as_deref()
        .unwrap()
        .starts_with("Hi 👋 I’m Colin"));
    assert!(!doc.description.as_deref().unwrap().contains('<'));
    assert_eq!(doc.start.unwrap().to_rfc3339(), "2024-03-04T00:00:00+00:00");
    assert_eq!(doc.end, None);
    let a = &doc.attributes;
    assert_eq!(a["hiring_organization"], "Ashby");
    assert_eq!(a["hiring_organization_url"], "https://www.ashbyhq.com/");
    assert_eq!(a["employment_type"], "FULL_TIME");
    assert_eq!(a["remote"], "true");
    assert!(a["remote_regions"].starts_with("Spain; Italy; Germany; Switzerland"));
    assert!(a["remote_regions"].ends_with("European Union"));
    assert_eq!(
        (
            &*a["salary_min"],
            &*a["salary_max"],
            &*a["salary_currency"],
            &*a["salary_unit"]
        ),
        ("150000", "250000", "EUR", "YEAR")
    );
    // One applies on the page itself (`directApply`).
    assert_eq!(a["apply_url"], ASHBY);
    // A remote posting's country alone is no workplace.
    assert!(doc.event.venue.is_none());

    let doc = extracted(RIVER, "bitcoinerjobs-1682142.html.gz");
    assert_eq!(doc.name.as_deref(), Some("Staff Software Engineer"));
    let a = &doc.attributes;
    assert_eq!(a["hiring_organization"], "River");
    assert_eq!(a["remote_regions"], "Anywhere");
    assert_eq!((&*a["salary_min"], &*a["salary_max"]), ("200000", "200000"));
    assert_eq!(a["salary_currency"], "USD");
    assert_eq!(a["valid_through"], "2026-11-07");
    assert_eq!(doc.end.unwrap().to_rfc3339(), "2026-11-08T00:00:00+00:00");

    let doc = extracted(BATTERY, "bitcoinerjobs-2563734.html.gz");
    assert_eq!(doc.name.as_deref(), Some("Investment Associate/Analyst"));
    assert!(!doc.attributes.contains_key("remote"));
    assert_eq!(doc.attributes["apply_url"], BATTERY);
    let venue = doc.event.venue.as_ref().unwrap();
    assert_eq!(venue.text, "Philadelphia, Pennsylvania, United States");
    assert_eq!(venue.address["city"], "Philadelphia");
    assert_eq!(venue.address["country"], "United States");

    let doc = extracted(LEVER, "lever-6ed76ce8.html.gz");
    assert_eq!(doc.name.as_deref(), Some("Administrative Business Partner"));
    assert_eq!(
        doc.attributes["hiring_organization"],
        "Palantir Technologies"
    );
    assert_eq!(doc.attributes["employment_type"], "FULL_TIME");
    assert_eq!(
        doc.event.venue.as_ref().unwrap().address["city"],
        "Singapore, Singapore"
    );
    let bounded = doc.clone().bounded(web::ATTRIBUTES).unwrap();
    assert_eq!(bounded.attributes, doc.attributes, "facts kept");
}

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("93.184.216.34".parse()?, port)])
    }
}
/// The recorded pages, and two made-up ones: a closed posting and a remote one in the future.
struct Pages;
#[async_trait::async_trait]
impl Transport for Pages {
    async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, a, &FetchOptions::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        let body = match url.as_str() {
            ASHBY => recorded("ashby-7458d4e9.html.gz"),
            BATTERY => recorded("bitcoinerjobs-2563734.html.gz"),
            "https://jobs.example/closed" => format!(
                "<script type=\"application/ld+json\">{}</script>",
                json!({"@type": "JobPosting", "title": "Closed role", "datePosted": "2026-01-05",
                    "validThrough": "2026-02-01", "hiringOrganization": "Example",
                    "jobLocationType": "TELECOMMUTE"})
            )
            .into_bytes(),
            other => anyhow::bail!("no page for {other}"),
        };
        Ok(page(url.as_str(), body))
    }
}
async fn call(app: &Router, path: &str) -> (u16, Value) {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1111".parse::<SocketAddr>().unwrap()));
    let r = app.clone().oneshot(request).await.unwrap();
    let status = r.status().as_u16();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status == 200 {
        common::check_response("GET", path.split('?').next().unwrap(), status, &value);
    }
    (status, value)
}
fn ids(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn postings_are_placed_found_and_close(pool: PgPool) {
    // Nominatim knows Philadelphia as the one settlement of that name in Pennsylvania.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/search",
        get(|Query(q): Query<HashMap<String, String>>| async move {
            assert_eq!(q["featureType"], "settlement");
            assert_eq!(q["city"], "Philadelphia");
            assert_eq!(q["state"], "Pennsylvania");
            assert_eq!(q["country"], "United States");
            axum::Json(
                serde_json::from_slice::<Value>(&recorded("nominatim-philadelphia.json")).unwrap(),
            )
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let resolver = WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        egress: Egress::with_backends(
            EgressConfig {
                host_requests_per_second: 1000.0,
                ..Default::default()
            },
            Arc::new(PublicDns),
            Arc::new(Pages),
        ),
        config: Default::default(),
        events: Some(Arc::new(pinakes_core::event::service::EventService {
            pool: pool.clone(),
            config: EventConfig::default(),
            osm: Arc::new(
                OsmResolver::new(
                    pool.clone(),
                    OsmConfig {
                        nominatim_url: format!("http://{addr}"),
                        requests_per_second: 1000.0,
                        ..Default::default()
                    },
                )
                .unwrap(),
            ),
        })),
    };
    // Battery's posting is valid through 2026-11-19; the search and the card read it against
    // the real clock, so it is stored closing 30 days from now, whenever the test runs.
    let open_until = (chrono::Utc::now() + chrono::Duration::days(30))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    let mut saved = HashMap::new();
    for uri in [BATTERY, ASHBY, "https://jobs.example/closed"] {
        let mut doc = resolver.resolve(uri).await.unwrap();
        assert_eq!(doc.type_name, "JobPosting", "{uri}");
        if uri == BATTERY {
            assert_eq!(
                doc.end.map(|e| e.to_rfc3339()).as_deref(),
                Some("2026-11-20T00:00:00+00:00")
            );
            doc.end = Some(open_until);
        }
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','jobs',$3,now())")
            .bind(&id)
            .bind("a".repeat(52))
            .bind(format!("/pub/app/tags/{id}"))
            .execute(&pool)
            .await
            .unwrap();
        saved.insert(uri, id);
    }
    server.abort();
    // The workplace's point is OpenStreetMap's, read from `osm_place`, never stored with the card.
    let stored: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&saved[BATTERY])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(stored.get("location").is_none());
    assert_eq!(stored["venue"]["osm_id"], "relation/188022");

    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (code, card) = call(&app, &format!("/v1/resources/{}", saved[BATTERY])).await;
    assert_eq!(code, 200, "{card}");
    assert_eq!(card["type"], "JobPosting");
    assert_eq!(card["point"]["lat"], 39.9527237);
    assert!(card["attribution"]
        .as_str()
        .unwrap()
        .contains("OpenStreetMap"));
    assert_eq!(card["start"], "2026-10-05T00:00:00Z");
    assert_eq!(
        card["end"],
        open_until.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    );
    let facts = &card["facts"];
    assert_eq!(facts["hiring_organization"], "Battery");
    assert_eq!(facts["employment_type"], "FULL_TIME");
    assert_eq!(facts["job_status"], "open");
    assert_eq!(
        card["venue"]["name"],
        "Philadelphia, Pennsylvania, United States"
    );

    // On the map.
    let (code, v) = call(
        &app,
        "/v1/search?type=JobPosting&near=39.95,-75.16&radius=20000",
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(ids(&v), [saved[BATTERY].clone()]);
    // Remote or not; a closed posting is left out of both.
    let (_, v) = call(&app, "/v1/search?type=JobPosting&remote=true").await;
    assert_eq!(ids(&v), [saved[ASHBY].clone()]);
    let (_, v) = call(&app, "/v1/search?remote=false").await;
    assert_eq!(ids(&v), [saved[BATTERY].clone()]);
    // Closed postings only on request, and they say so.
    let (_, v) = call(&app, "/v1/search?tags=jobs").await;
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    let (_, v) = call(
        &app,
        "/v1/search?tags=jobs&closed=true&remote=true&sort=start",
    )
    .await;
    let closed = saved["https://jobs.example/closed"].clone();
    assert_eq!(ids(&v), [saved[ASHBY].clone(), closed.clone()]);
    let (_, card) = call(&app, &format!("/v1/resources/{closed}")).await;
    assert_eq!(card["facts"]["job_status"], "closed");
    // By the date posted.
    let (_, v) = call(
        &app,
        "/v1/search?type=JobPosting&from=2026-10-01T00:00:00Z&to=2026-10-10T00:00:00Z",
    )
    .await;
    assert_eq!(ids(&v), [saved[BATTERY].clone()]);
    let (code, v) = call(&app, "/v1/search?remote=maybe").await;
    assert_eq!(code, 400, "{v}");
}

/// A Nominatim settlement hit, shaped like its jsonv2 answer.
fn settlement(id: i64, name: &str, country: &str, code: &str) -> Value {
    json!({"osm_type": "relation", "osm_id": id, "lat": "51.5", "lon": "-0.12",
        "category": "boundary", "type": "administrative", "addresstype": "city", "name": name,
        "display_name": format!("{name}, {country}"), "namedetails": {"name": name},
        "address": {"city": name, "country": country, "country_code": code}})
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_workplace_is_never_an_ambiguous_or_other_settlement(pool: PgPool) {
    // Two Springfields; one settlement under another name than the posting's town; and London,
    // United Kingdom for a posting in London, Canada.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/search",
        get(|Query(q): Query<HashMap<String, String>>| async move {
            assert_eq!(q["featureType"], "settlement");
            let hits = match q["city"].as_str() {
                "Springfield" => json!([
                    settlement(1, "Springfield", "United States", "us"),
                    settlement(2, "Springfield", "United States", "us")
                ]),
                "Philly" => json!([settlement(3, "Philadelphia", "United States", "us")]),
                "London" => json!([settlement(4, "London", "United Kingdom", "gb")]),
                other => panic!("asked for {other}"),
            };
            axum::Json(hits)
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let service = pinakes_core::event::service::EventService {
        pool: pool.clone(),
        // Even with a lenient score, a contradicting country never places a posting.
        config: EventConfig {
            geocode_score: 0.5,
            ..Default::default()
        },
        osm: Arc::new(
            OsmResolver::new(
                pool.clone(),
                OsmConfig {
                    nominatim_url: format!("http://{addr}"),
                    requests_per_second: 1000.0,
                    ..Default::default()
                },
            )
            .unwrap(),
        ),
    };
    for (town, country) in [
        ("Springfield", "United States"),
        ("Philly", "United States"),
        ("London", "Canada"),
    ] {
        let base: url::Url = "https://jobs.example/p".parse().unwrap();
        let mut doc = ResourceMeta::new(base.as_str(), "jsonld", 3600);
        pinakes_core::kinds::jobs::from_jsonld(
            &json!({"@type": "JobPosting", "title": "Role",
                "jobLocation": {"address": {"addressLocality": town, "addressCountry": country}}}),
            &base,
        )
        .apply(&mut doc);
        assert!(doc.event.venue.is_some(), "{town}");
        service.workplace(&mut doc).await.unwrap();
        let venue = doc.event.venue.as_ref().unwrap();
        assert_eq!(venue.osm_id, None, "{town}, {country} is not placed");
        assert!(doc.location.is_none(), "{town}");
    }
    let saved: i64 = sqlx::query_scalar("SELECT count(*) FROM osm_place")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(saved, 0, "nothing geocoded is saved");
    server.abort();
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_workplace_is_never_in_another_region_or_postal_region(pool: PgPool) {
    // Nominatim's sole Springfield is Springfield, Missouri: a posting in Springfield, Illinois
    // (by name or code, in its address or in its one-line place) or with a postcode of another
    // postal region is not placed there, even at a lenient score; one in Missouri is.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/search",
        get(|Query(q): Query<HashMap<String, String>>| async move {
            assert_eq!(q["featureType"], "settlement");
            assert!(
                q.get("city").is_some_and(|c| c == "Springfield")
                    || q.get("q").is_some_and(|q| q.starts_with("Springfield,")),
                "{q:?}"
            );
            let mut hit = settlement(5, "Springfield", "United States", "us");
            hit["address"]["state"] = "Missouri".into();
            hit["address"]["ISO3166-2-lvl4"] = "US-MO".into();
            hit["address"]["postcode"] = "65801".into();
            axum::Json(json!([hit]))
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let service = pinakes_core::event::service::EventService {
        pool: pool.clone(),
        config: EventConfig {
            geocode_score: 0.5,
            ..Default::default()
        },
        osm: Arc::new(
            OsmResolver::new(
                pool.clone(),
                OsmConfig {
                    nominatim_url: format!("http://{addr}"),
                    requests_per_second: 1000.0,
                    ..Default::default()
                },
            )
            .unwrap(),
        ),
    };
    let placed = |address: Value| {
        let service = &service;
        async move {
            let base: url::Url = "https://jobs.example/p".parse().unwrap();
            let mut doc = ResourceMeta::new(base.as_str(), "jsonld", 3600);
            pinakes_core::kinds::jobs::from_jsonld(
                &json!({"@type": "JobPosting", "title": "Role",
                    "jobLocation": {"address": address}}),
                &base,
            )
            .apply(&mut doc);
            service.workplace(&mut doc).await.unwrap();
            doc.event.venue.unwrap().osm_id
        }
    };
    for address in [
        json!({"addressLocality": "Springfield", "addressRegion": "Illinois", "addressCountry": "US"}),
        json!({"addressLocality": "Springfield", "addressRegion": "IL", "addressCountry": "US"}),
        json!({"addressLocality": "Springfield", "postalCode": "90210", "addressCountry": "US"}),
        json!({"addressLocality": "Springfield, IL"}),
        json!({"addressLocality": "Springfield, IL, US"}),
        json!({"addressLocality": "Springfield, 90210"}),
        json!({"addressLocality": "Springfield, MO 90210"}),
    ] {
        assert_eq!(placed(address.clone()).await, None, "{address}");
    }
    let saved: i64 = sqlx::query_scalar("SELECT count(*) FROM osm_place")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(saved, 0, "nothing geocoded is saved");
    for address in [
        json!({"addressLocality": "Springfield", "addressRegion": "Missouri", "addressCountry": "US"}),
        json!({"addressLocality": "Springfield", "addressRegion": "MO", "postalCode": "65806", "addressCountry": "US"}),
        json!({"addressLocality": "Springfield", "addressCountry": "US"}),
        json!({"addressLocality": "Springfield, MO"}),
        json!({"addressLocality": "Springfield, MO 65806, US"}),
        // Not written like the settlement's code, or its country: not compared.
        json!({"addressLocality": "Springfield, Missouri"}),
        json!({"addressLocality": "Springfield, USA"}),
        json!({"addressLocality": "Springfield, US"}),
    ] {
        assert_eq!(
            placed(address.clone()).await.as_deref(),
            Some("relation/5"),
            "{address}"
        );
    }
    server.abort();
}
