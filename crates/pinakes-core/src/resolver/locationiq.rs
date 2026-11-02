//! LocationIQ as the OSM provider (`osm.provider = "locationiq"`): its answers normalized to
//! the Nominatim form the OSM resolver reads.
use serde_json::Value;

/// The credit on a place LocationIQ answered: the OSM data is ODbL, and LocationIQ's terms
/// govern what LocationIQ adds.
pub const ATTRIBUTION: &str =
    "© OpenStreetMap contributors (ODbL); geocoded by LocationIQ.com under its terms";
/// The most objects one lookup names.
pub const LOOKUP_MAX: usize = 3;

/// A row in the form the OSM resolver reads (Nominatim's): its OSM identity with a numeric id,
/// `class` as `category`, a `name` from its name details, and without LocationIQ's own
/// `place_id`. `None` for a row without OSM identity: a `place_id` never stands in for one.
pub(crate) fn normalize(mut row: Value) -> Option<Value> {
    let kind = row["osm_type"]
        .as_str()
        .filter(|kind| matches!(*kind, "node" | "way" | "relation"))?
        .to_owned();
    let id = match &row["osm_id"] {
        Value::Number(n) => n.as_i64().filter(|id| *id > 0),
        Value::String(s) => super::osm::positive(s),
        _ => None,
    }?;
    let name = row["namedetails"]["name"].clone();
    let object = row.as_object_mut()?;
    object.remove("place_id");
    object.insert("osm_type".into(), kind.into());
    object.insert("osm_id".into(), id.into());
    if let Some(class) = object.get("class").cloned() {
        object.entry("category").or_insert(class);
    }
    if name.is_string() {
        object.entry("name").or_insert(name);
    }
    Some(row)
}

/// How many hits a search asks for: LocationIQ documents no settlement filter, so a settlement
/// search takes more and keeps the settlements (`settlement`).
pub(crate) fn search_limit(settlement: bool) -> &'static str {
    if settlement {
        "10"
    } else {
        "2"
    }
}
/// Whether a hit is a settlement, as Nominatim's `featureType=settlement` reads one: a place
/// that is a city, town, village, hamlet or municipality, or an administrative boundary.
pub(crate) fn settlement(hit: &Value) -> bool {
    match (hit["category"].as_str(), hit["type"].as_str()) {
        (Some("place"), Some(kind)) => {
            matches!(
                kind,
                "city" | "town" | "village" | "hamlet" | "municipality"
            )
        }
        (Some("boundary"), Some(kind)) => kind == "administrative",
        _ => false,
    }
}

/// What a LocationIQ answer other than 2xx means.
#[derive(Debug, PartialEq)]
pub(crate) enum Refusal {
    /// No match (404 `Unable to geocode`): an empty answer.
    NotFound,
    /// Throttled or failing: retry after this many seconds.
    Later(&'static str, u64),
    /// Refused for good (a refused key, a malformed request, a redirect): the message names
    /// the status alone, never the body or the URL.
    Error(String),
}
/// Classify a LocationIQ answer that is not 2xx by its status, its `Retry-After` and its
/// `{"error": …}` body.
pub(crate) fn refusal(status: u16, headers: &reqwest::header::HeaderMap, body: &[u8]) -> Refusal {
    let error = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_default();
    match status {
        401 | 403 => Refusal::Error(format!(
            "LocationIQ refused the request (HTTP {status}): check {}, its scopes and the plan",
            crate::config::LOCATIONIQ_API_KEY
        )),
        404 if error == "Unable to geocode" => Refusal::NotFound,
        429 => {
            // Without Retry-After, the window the body names, read only from LocationIQ's own
            // labels (any other text is ignored). A daily quota always waits the longest wait
            // allowed, whatever Retry-After says.
            let named = super::osm::retry_after_named(headers);
            let seconds = match error.as_str() {
                "Rate Limited Day" => 3600,
                "Rate Limited Second" => named.unwrap_or(1),
                _ => named.unwrap_or(60),
            };
            Refusal::Later("http_4xx", seconds)
        }
        500..=599 => Refusal::Later("http_5xx", super::osm::retry_after(headers)),
        _ => Refusal::Error(format!("LocationIQ HTTP {status}")),
    }
}

/// At startup, before anything is served: what LocationIQ answered is served only with the
/// authorization (`osm.locationiq.public_use_authorized`), whichever provider is selected. Its
/// answers stay in `osm_place` after a switch to Nominatim (in the place's document and in the
/// versions resources and change lines show) and in the dump file; while any remain, startup
/// without the authorization is refused. A place is LocationIQ's when its stored credit is not
/// the OSM credit alone.
pub async fn check_retained(
    pool: &sqlx::PgPool,
    osm: &crate::config::OsmConfig,
) -> anyhow::Result<()> {
    if osm.locationiq.public_use_authorized {
        return Ok(());
    }
    let places: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM osm_place WHERE coalesce(doc->>'attribution',$1)<>$1 \
         OR coalesce(fetch_doc->>'attribution',$1)<>$1 \
         OR EXISTS(SELECT 1 FROM jsonb_each(versions) v WHERE coalesce(v.value->>'attribution',$1)<>$1)",
    )
    .bind(super::osm::ATTRIBUTION)
    .fetch_one(pool)
    .await?;
    let dir = std::path::PathBuf::from(&osm.dump_dir);
    let dump =
        tokio::task::spawn_blocking(move || crate::osm_dump::credits_beside_osm(&dir)).await??;
    let held = match (places, dump) {
        (0, false) => return Ok(()),
        (0, true) => "the dump file holds places LocationIQ answered".to_owned(),
        (n, false) => format!("places LocationIQ answered are stored ({n})"),
        (n, true) => {
            format!("places LocationIQ answered are stored ({n}) and in the dump file")
        }
    };
    anyhow::bail!(
        "{held}, and osm.locationiq.public_use_authorized is false: Pinakes would keep serving them. \
         Nothing was changed. Keep the authorization while they remain, or withdraw them as \
         docs/operations.md (OpenStreetMap provider) describes"
    )
}

/// What an instance may publish, proven against its database and dump directory: built only by
/// `check`. The API state (`AppState::new`) and the dump writer take one, so neither serves
/// what LocationIQ answered without the authorization.
#[derive(Clone, Copy, Debug)]
pub struct Publication {
    locationiq: bool,
}
impl Publication {
    /// `check_retained`, then what may be published.
    pub async fn check(
        pool: &sqlx::PgPool,
        osm: &crate::config::OsmConfig,
    ) -> anyhow::Result<Self> {
        check_retained(pool, osm).await?;
        Ok(Self {
            locationiq: osm.locationiq.public_use_authorized,
        })
    }
    /// Whether places LocationIQ answered may be published.
    pub fn locationiq(self) -> bool {
        self.locationiq
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        config::{LocationIqConfig, OsmConfig, OsmProvider},
        model::Status,
        resolver::{osm::OsmResolver, Resolver},
    };
    use axum::{
        extract::{RawQuery, State},
        http::{StatusCode, Uri},
        response::{IntoResponse, Response},
        Router,
    };
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    pub(crate) const KEY: &str = "pk.KEYCANARY";
    /// A request's path and query.
    pub(crate) type Call = (String, HashMap<String, String>);
    type Reply = Arc<dyn Fn(&str, &HashMap<String, String>) -> Response + Send + Sync>;
    /// A LocationIQ stand-in: each request's path and query, answered by `reply`.
    #[derive(Clone)]
    pub(crate) struct Mock {
        pub calls: Arc<Mutex<Vec<Call>>>,
        reply: Reply,
    }
    async fn answer(State(mock): State<Mock>, uri: Uri, RawQuery(raw): RawQuery) -> Response {
        let query: HashMap<String, String> =
            url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes())
                .into_owned()
                .collect();
        mock.calls
            .lock()
            .unwrap()
            .push((uri.path().to_owned(), query.clone()));
        (mock.reply)(uri.path(), &query)
    }
    /// The mock's base (`http://127.0.0.1:…/v1`) and its call log.
    pub(crate) async fn serve(
        reply: impl Fn(&str, &HashMap<String, String>) -> Response + Send + Sync + 'static,
    ) -> (String, Mock) {
        let mock = Mock {
            calls: Arc::default(),
            reply: Arc::new(reply),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new().fallback(answer).with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, mock)
    }
    pub(crate) fn config() -> OsmConfig {
        OsmConfig {
            provider: OsmProvider::LocationIq,
            locationiq: LocationIqConfig {
                public_use_authorized: true,
                key: Some(KEY.into()),
                requests_per_second: 1000.0,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    /// A lookup row as LocationIQ writes it (`format=json`): ids and coordinates as strings,
    /// `class` for the category, its own `place_id`.
    pub(crate) fn row(kind: &str, id: i64, name: &str) -> Value {
        json!({
            "place_id": format!("33{id}"),
            "licence": "https://locationiq.com/attribution",
            "osm_type": kind,
            "osm_id": id.to_string(),
            "lat": "46.0037",
            "lon": "8.9511",
            "display_name": format!("{name}, Lugano, Switzerland"),
            "class": "amenity",
            "type": "cafe",
            "importance": 0.2,
            "address": {"road": "Via Nassa", "house_number": "5", "city": "Lugano", "postcode": "6900", "country_code": "ch"},
            "namedetails": {"name": name},
            "extratags": {"website": "https://cafe.example"},
            "boundingbox": ["46.0", "46.01", "8.95", "8.96"]
        })
    }
    fn uri(id: i64) -> String {
        format!("https://www.openstreetmap.org/node/{id}")
    }

    /// At most three objects per lookup (LocationIQ's maximum), `format=json` with the details
    /// Nominatim gets, the key in the query alone; rows are read by their OSM identity, whatever
    /// LocationIQ's own id says, and an object no row names is missing.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn lookups_go_three_at_a_time_and_read_osm_identity(pool: PgPool) {
        let (base, mock) = serve(|path, q| {
            assert_eq!(path, "/v1/lookup");
            let rows: Vec<Value> = q["osm_ids"]
                .split(',')
                .filter(|id| ["N1", "N2", "N4"].contains(id))
                .map(|id| row("node", id[1..].parse().unwrap(), &format!("Cafe {id}")))
                // A row without OSM identity names nothing, whatever its own id.
                .chain([json!({"place_id": "4", "lat": "1", "lon": "1", "class": "place", "type": "house"})])
                .collect();
            axum::Json(rows).into_response()
        })
        .await;
        let resolver = OsmResolver::with_base(pool, config(), &base).unwrap();
        assert_eq!(resolver.batch_size(), 3);
        let uris: Vec<String> = (1..=5).map(uri).collect();
        let results = resolver.resolve_batch(&uris).await;
        let calls = mock.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].1["osm_ids"], "N1,N2,N3");
        assert_eq!(calls[1].1["osm_ids"], "N4,N5");
        for (_, q) in &calls {
            for (k, v) in [
                ("format", "json"),
                ("addressdetails", "1"),
                ("namedetails", "1"),
                ("extratags", "1"),
                ("key", KEY),
            ] {
                assert_eq!(q.get(k).map(String::as_str), Some(v), "{k}");
            }
        }
        let docs: Vec<_> = results.into_iter().map(Result::unwrap).collect();
        for (doc, found) in docs.iter().zip([true, true, false, true, false]) {
            assert_eq!(doc.status == Status::Ok, found, "{}", doc.url);
            assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));
        }
        assert_eq!(docs[0].osm_id.as_deref(), Some("node/1"));
        assert_eq!(docs[0].name.as_deref(), Some("Cafe N1"));
        assert_eq!(docs[0].category.as_deref(), Some("amenity/cafe"));
        assert_eq!(docs[0].location.as_ref().unwrap().lat, 46.0037);
        assert_eq!(docs[0].attributes["website"], "https://cafe.example");
        assert_eq!(docs[3].url, uri(4));
        assert!(docs.iter().all(|d| !format!("{d:?}").contains("place_id")));
    }

    /// A 404 that says nothing matched is a missing object; any other 404, a body that is no
    /// list, a refused key (401, 403) and a redirect are errors, never retried as throttling and
    /// never followed; 429 and 5xx wait for `Retry-After`, or the window a 429 names. No error
    /// repeats the key or the body.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn answers_are_classified_without_repeating_the_key_or_the_body(pool: PgPool) {
        enum Expect {
            Missing,
            Error(&'static str),
            Later(&'static str, u64),
        }
        /// A status, headers and body LocationIQ answers, and what they mean.
        type Case = (u16, Vec<(&'static str, String)>, &'static str, Expect);
        let cases: Vec<Case> = vec![
            (
                404,
                vec![],
                r#"{"error":"Unable to geocode"}"#,
                Expect::Missing,
            ),
            (
                404,
                vec![],
                "<html>BODYCANARY</html>",
                Expect::Error("HTTP 404"),
            ),
            (
                200,
                vec![],
                r#"{"error":"BODYCANARY"}"#,
                Expect::Error("LocationIQ response is not a list"),
            ),
            (200, vec![], "BODYCANARY", Expect::Error("")),
            (
                401,
                vec![],
                r#"{"error":"Invalid key BODYCANARY"}"#,
                Expect::Error("LOCATIONIQ_API_KEY"),
            ),
            (
                403,
                vec![],
                r#"{"error":"Access restricted BODYCANARY"}"#,
                Expect::Error("HTTP 403"),
            ),
            (
                400,
                vec![],
                r#"{"error":"BODYCANARY"}"#,
                Expect::Error("HTTP 400"),
            ),
            (
                302,
                vec![("location", "/v1/lookup-elsewhere".into())],
                "",
                Expect::Error("HTTP 302"),
            ),
            (
                429,
                vec![("retry-after", "7".into())],
                r#"{"error":"Rate Limited Minute"}"#,
                Expect::Later("http_4xx", 7),
            ),
            (
                429,
                vec![],
                r#"{"error":"Rate Limited Day"}"#,
                Expect::Later("http_4xx", 3600),
            ),
            (
                429,
                vec![],
                r#"{"error":"Rate Limited Second"}"#,
                Expect::Later("http_4xx", 1),
            ),
            // A daily quota is never retried sooner than the longest wait, whatever Retry-After
            // says; a window it does not know is the default minute.
            (
                429,
                vec![("retry-after", "1".into())],
                r#"{"error":"Rate Limited Day"}"#,
                Expect::Later("http_4xx", 3600),
            ),
            (
                429,
                vec![],
                r#"{"error":"Rate Limited Week BODYCANARY"}"#,
                Expect::Later("http_4xx", 60),
            ),
            (
                503,
                vec![("retry-after", "30".into())],
                "BODYCANARY",
                Expect::Later("http_5xx", 30),
            ),
        ];
        for (status, headers, body, expect) in cases {
            let (base, mock) = serve(move |_, _| {
                let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
                for (name, value) in &headers {
                    response.headers_mut().insert(*name, value.parse().unwrap());
                }
                response
            })
            .await;
            let resolver = OsmResolver::with_base(pool.clone(), config(), &base).unwrap();
            let result = resolver.probe(&uri(1)).await;
            assert_eq!(
                mock.calls.lock().unwrap().len(),
                1,
                "{status}: never followed"
            );
            match (result, expect) {
                (Ok(doc), Expect::Missing) => assert_eq!(doc.status, Status::Missing),
                (Err(error), Expect::Error(says)) => {
                    let text = format!("{error:#}");
                    assert!(text.contains(says), "{status}: {text}");
                    assert!(
                        error
                            .downcast_ref::<crate::resolver::RetryLater>()
                            .is_none(),
                        "{status}"
                    );
                    assert!(
                        !text.contains("KEYCANARY") && !text.contains("BODYCANARY"),
                        "{status}: {text}"
                    );
                }
                (Err(error), Expect::Later(class, seconds)) => {
                    let later = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
                    assert_eq!((later.class, later.seconds), (class, seconds), "{status}");
                }
                (result, _) => panic!("{status}: {result:?}"),
            }
        }
    }

    /// The key is in the request alone: a failed connection's error and the request's log
    /// lines (at any level) never hold it.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn the_key_reaches_no_error_or_log_line(pool: PgPool) {
        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/v1", listener.local_addr().unwrap())
        };
        let resolver = OsmResolver::with_base(pool.clone(), config(), &closed).unwrap();
        let error = resolver.probe(&uri(1)).await.unwrap_err();
        assert!(
            !format!("{error:#} {error:?}").contains("KEYCANARY"),
            "{error:#}"
        );
        let (base, _) = serve(|_, q| {
            let ids: Vec<Value> = q["osm_ids"]
                .split(',')
                .map(|id| row("node", id[1..].parse().unwrap(), "Cafe"))
                .collect();
            axum::Json(ids).into_response()
        })
        .await;
        let resolver = OsmResolver::with_base(pool, config(), &base).unwrap();
        let logs = Lines::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        assert!(resolver.resolve(&uri(1)).await.is_ok());
        drop(guard);
        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            !text.contains("KEYCANARY") && !text.contains("key="),
            "{text}"
        );
    }

    fn events(pool: &PgPool, base: &str) -> crate::event::service::EventService {
        crate::event::service::EventService {
            pool: pool.clone(),
            config: crate::config::EventConfig::default(),
            osm: Arc::new(OsmResolver::with_base(pool.clone(), config(), base).unwrap()),
        }
    }
    fn placed(venue: Value) -> crate::model::ResourceMeta {
        let mut doc = crate::model::ResourceMeta::new("https://events.example/e/1", "event", 3600);
        doc.event.venue = Some(serde_json::from_value(venue).unwrap());
        doc
    }
    async fn attribution(pool: &PgPool, id: i64) -> Option<String> {
        sqlx::query_scalar("SELECT doc->>'attribution' FROM osm_place WHERE osm_id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    /// A venue's street address goes to LocationIQ's structured search, its own endpoint, and
    /// the one hit places the venue as a Nominatim hit would; a hit without OSM identity places
    /// nothing.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn a_venue_is_geocoded_through_structured_search(pool: PgPool) {
        let (base, mock) = serve(|path, q| {
            assert_eq!(path, "/v1/search/structured");
            let hit = if q["city"] == "Lugano" {
                row("node", 7, "Cafe Nassa")
            } else {
                json!({"place_id": "9", "lat": "46", "lon": "9", "class": "place", "type": "house", "address": {"city": "Bellinzona"}})
            };
            axum::Json(vec![hit]).into_response()
        })
        .await;
        let events = events(&pool, &base);
        let mut doc = placed(json!({
            "text": "Cafe Nassa, Via Nassa 5, 6900 Lugano",
            "name": "Cafe Nassa",
            "address": {"street": "Via Nassa 5", "city": "Lugano", "postalcode": "6900", "country": "CH"}
        }));
        events.venue(&mut doc).await.unwrap();
        let venue = doc.event.venue.unwrap();
        assert_eq!(venue.osm_id.as_deref(), Some("node/7"));
        assert!(venue.resource_id.is_some(), "name and full address match");
        assert_eq!(attribution(&pool, 7).await.as_deref(), Some(ATTRIBUTION));
        let (_, q) = mock.calls.lock().unwrap()[0].clone();
        for (k, v) in [
            ("format", "json"),
            ("addressdetails", "1"),
            ("namedetails", "1"),
            ("limit", "2"),
            ("accept-language", "en"),
            ("street", "Via Nassa 5"),
            ("city", "Lugano"),
            ("postalcode", "6900"),
            ("country", "CH"),
            ("key", KEY),
        ] {
            assert_eq!(q.get(k).map(String::as_str), Some(v), "{k}");
        }
        assert!(
            !q.contains_key("q") && !q.contains_key("featureType"),
            "{q:?}"
        );
        let mut doc = placed(json!({
            "text": "Via Lugano 1, Bellinzona",
            "address": {"street": "Via Lugano 1", "city": "Bellinzona"}
        }));
        events.venue(&mut doc).await.unwrap();
        assert!(doc.event.venue.unwrap().osm_id.is_none());
    }
    /// A workplace's one-line town goes to LocationIQ's free-text search and a town with a
    /// country to its structured search; LocationIQ has no settlement filter, so the
    /// settlements among more hits are kept, and exactly one must remain.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn a_workplace_town_is_geocoded_through_free_text_or_structured_search(pool: PgPool) {
        let city = |id: i64, name: &str, country: &str| {
            let mut hit = row("relation", id, name);
            hit["class"] = "place".into();
            hit["type"] = "city".into();
            hit["address"] = json!({"city": name, "country_code": country});
            hit
        };
        let (base, mock) = serve(move |path, q| {
            let hits = match (path, q.get("q").map(String::as_str), q.get("city").map(String::as_str)) {
                ("/v1/search", Some("San Francisco, CA"), None) => {
                    let mut street = row("way", 5, "San Francisco Street");
                    street["class"] = "highway".into();
                    street["type"] = "residential".into();
                    street["address"] = json!({"road": "San Francisco Street", "city": "San Francisco", "country_code": "us"});
                    vec![
                        street,
                        city(111968, "San Francisco", "us"),
                        json!({"place_id": "1", "lat": "37", "lon": "-122", "class": "place", "type": "city"}),
                    ]
                }
                ("/v1/search/structured", None, Some("Lugano")) => {
                    let mut district = row("relation", 1684625, "Distretto di Lugano");
                    district["class"] = "boundary".into();
                    district["type"] = "administrative".into();
                    vec![city(1684624, "Lugano", "ch")]
                        .into_iter()
                        .chain(q.get("postalcode").map(|_| district))
                        .collect()
                }
                other => panic!("{other:?}"),
            };
            axum::Json(hits).into_response()
        })
        .await;
        let events = events(&pool, &base);
        let mut doc =
            placed(json!({"text": "San Francisco, CA", "address": {"city": "San Francisco, CA"}}));
        events.workplace(&mut doc).await.unwrap();
        assert_eq!(
            doc.event.venue.unwrap().osm_id.as_deref(),
            Some("relation/111968")
        );
        let mut doc =
            placed(json!({"text": "Lugano", "address": {"city": "Lugano", "country": "CH"}}));
        events.workplace(&mut doc).await.unwrap();
        assert_eq!(
            doc.event.venue.unwrap().osm_id.as_deref(),
            Some("relation/1684624")
        );
        // Two settlements: not placed.
        let mut doc = placed(
            json!({"text": "Lugano", "address": {"city": "Lugano", "country": "CH", "postalcode": "6900"}}),
        );
        events.workplace(&mut doc).await.unwrap();
        assert!(doc.event.venue.unwrap().osm_id.is_none());
        let calls = mock.calls.lock().unwrap().clone();
        assert_eq!(calls[0].1["q"], "San Francisco, CA");
        assert_eq!(calls[1].1["country"], "CH");
        for (_, q) in &calls {
            assert_eq!(q["format"], "json");
            assert_eq!(q["limit"], "10");
            assert_eq!(q["key"], KEY);
            assert!(!q.contains_key("featureType"), "{q:?}");
        }
    }

    /// A place LocationIQ answered is credited to LocationIQ in the downloadable dump, line by
    /// line, whatever the provider when the file is written.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn the_dump_credits_each_place_to_its_provider(pool: PgPool) {
        let (base, _) =
            serve(|_, _| axum::Json(vec![row("node", 1, "Cafe")]).into_response()).await;
        let resolver = OsmResolver::with_base(pool.clone(), config(), &base).unwrap();
        resolver.resolve(&uri(1)).await.unwrap();
        let dir =
            std::env::temp_dir().join(format!("pinakes-locationiq-dump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::osm_dump::write(
            &pool,
            &dir,
            Publication::check(&pool, &config()).await.unwrap(),
        )
        .await
        .unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(
                std::fs::File::open(crate::osm_dump::path(&dir)).unwrap(),
            ),
            &mut text,
        )
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[1]["attribution"], ATTRIBUTION, "{text}");
        assert!(
            lines[0]["note"].as_str().unwrap().contains("attribution"),
            "{text}"
        );
        assert!(
            !text.contains("KEYCANARY") && !text.contains("place_id"),
            "{text}"
        );
    }

    /// Every way to build the resolver checks LocationIQ's settings, not only the daemon's
    /// configuration loader: without the authorization, with no usable key or at no usable
    /// rate there is no resolver, and nothing is fetched or stored. Nominatim is built as it
    /// always was, whatever LocationIQ's settings say.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn a_resolver_is_never_built_without_the_authorization(pool: PgPool) {
        let (base, mock) =
            serve(|_, _| axum::Json(vec![row("node", 1, "Cafe")]).into_response()).await;
        let unauthorized = || {
            let mut c = config();
            c.locationiq.public_use_authorized = false;
            c
        };
        let keyless = || {
            let mut c = config();
            c.locationiq.key = None;
            c
        };
        let account = || {
            let mut c = config();
            c.locationiq.key = Some("sk.KEYCANARY".into());
            c
        };
        let spaced = || {
            let mut c = config();
            c.locationiq.key = Some("pk.KEY CANARY".into());
            c
        };
        let stalled = || {
            let mut c = config();
            c.locationiq.requests_per_second = 0.0;
            c
        };
        let unbounded = || {
            let mut c = config();
            c.locationiq.requests_per_second = f64::NAN;
            c
        };
        for (case, refused) in [
            ("unauthorized", unauthorized()),
            ("keyless", keyless()),
            ("account token", account()),
            ("spaced key", spaced()),
            ("zero rate", stalled()),
            ("NaN rate", unbounded()),
        ] {
            for built in [
                OsmResolver::new(pool.clone(), refused.clone()).map(|_| ()),
                OsmResolver::with_base(pool.clone(), refused.clone(), &base).map(|_| ()),
            ] {
                let error = format!("{:#}", built.expect_err(case));
                assert!(!error.contains("CANARY"), "{case}: {error}");
            }
        }
        let error = OsmResolver::new(pool.clone(), unauthorized())
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains("public_use_authorized"), "{error}");
        // The queue's own entry point builds the resolver from a configuration that never went
        // through `Config::validate`: it is refused there too.
        let cfg = crate::config::Config {
            osm: unauthorized(),
            ..Default::default()
        };
        let (_, shutdown) = tokio::sync::watch::channel(false);
        let spawned = crate::queue::spawn_with_egress(
            pool.clone(),
            cfg,
            Arc::default(),
            shutdown,
            Arc::new(crate::telemetry::Metrics::new().unwrap()),
            crate::sources::fixture::egress(vec![]).0,
        );
        assert!(spawned.is_err());
        assert!(mock.calls.lock().unwrap().is_empty());
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM osm_place")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 0);
        // Authorized, with a product key: built, at LocationIQ's regional endpoint alone.
        assert!(OsmResolver::new(pool.clone(), config()).is_ok());
        let mut nominatim = unauthorized();
        nominatim.provider = OsmProvider::Nominatim;
        nominatim.locationiq.requests_per_second = 0.0;
        assert!(OsmResolver::new(pool, nominatim).is_ok());
    }

    /// The key goes to the resolver's own LocationIQ endpoint alone: a request for any other
    /// URL is refused before it is sent, and that URL never sees the key.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn the_key_goes_to_the_locationiq_endpoint_alone(pool: PgPool) {
        let (base, mock) = serve(|_, _| axum::Json(json!([])).into_response()).await;
        let (elsewhere, other) = serve(|_, _| axum::Json(json!([])).into_response()).await;
        let resolver = OsmResolver::with_base(pool, config(), &base).unwrap();
        for url in [
            format!("{elsewhere}/search?q=x"),
            format!("{}/search?q=x", base.trim_end_matches("/v1")),
            format!("{base}x/search?q=x"),
            base.replace("http://", "https://") + "/search?q=x",
        ] {
            let error = resolver.request(url.parse().unwrap()).await.unwrap_err();
            assert!(!format!("{error:#}").contains("KEYCANARY"), "{url}");
        }
        assert!(other.calls.lock().unwrap().is_empty());
        assert!(mock.calls.lock().unwrap().is_empty());
        resolver
            .request(format!("{base}/search?q=x").parse().unwrap())
            .await
            .unwrap();
        assert_eq!(mock.calls.lock().unwrap()[0].1["key"], KEY);
    }

    /// `path` on the public API: its JSON.
    async fn read(app: &Router, method: &str, path: &str, body: Option<Value>) -> Value {
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(body.map_or_else(axum::body::Body::empty, |b| {
                axum::body::Body::from(b.to_string())
            }))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, 200, "{path}: {value}");
        value
    }
    async fn tag(pool: &PgPool, id: &str) {
        sqlx::query("INSERT INTO reference(resource_id,kind,author,label,object_path,created_at) VALUES($1,'tag',$2,'place',$1,now())")
            .bind(id)
            .bind("y".repeat(52))
            .execute(pool)
            .await
            .unwrap();
    }
    /// A Nominatim stand-in: `jsonv2` rows with numeric ids, nodes 3 and 4 in Berlin.
    async fn nominatim() -> (String, Mock) {
        serve(|_, q| {
            let rows: Vec<Value> = q["osm_ids"]
                .split(',')
                .map(|id| {
                    let id: i64 = id[1..].parse().unwrap();
                    let mut row = row("node", id, &format!("Place {id}"));
                    row["osm_id"] = id.into();
                    if id > 2 {
                        row["lat"] = "52.52".into();
                        row["lon"] = "13.40".into();
                        row["boundingbox"] = json!(["52.5", "52.53", "13.39", "13.41"]);
                    }
                    row
                })
                .collect();
            axum::Json(rows).into_response()
        })
        .await
    }

    /// What clients display: each card of a place, and of an event or a job placed at one,
    /// carries the credit of the place version it shows (LocationIQ's for a place LocationIQ
    /// answered, the OSM credit alone for Nominatim's), on a resource read, a lookup and a
    /// search; and a map cluster holding a place LocationIQ answered carries LocationIQ's.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn public_cards_and_clusters_credit_each_place_to_its_provider(pool: PgPool) {
        let (liq, _) = serve(|path, q| {
            let ids = if path == "/v1/lookup" {
                q["osm_ids"].clone()
            } else {
                "N7".into()
            };
            axum::Json(
                ids.split(',')
                    .map(|id| row("node", id[1..].parse().unwrap(), "Cafe Nassa"))
                    .collect::<Vec<_>>(),
            )
            .into_response()
        })
        .await;
        let (nom, _) = nominatim().await;
        let locationiq = Arc::new(OsmResolver::with_base(pool.clone(), config(), &liq).unwrap());
        let nominatim = OsmResolver::new(
            pool.clone(),
            OsmConfig {
                nominatim_url: nom,
                requests_per_second: 1000.0,
                ..Default::default()
            },
        )
        .unwrap();
        // Node 1 through LocationIQ, then 2–4 through Nominatim, each a queued job.
        let cfg = crate::config::QueueConfig::default();
        let mut ids = vec![];
        for (from, to, resolver) in [(1usize, 1usize, &*locationiq), (2, 4, &nominatim)] {
            for id in from..=to {
                ids.push(
                    crate::store::ensure_resource(&pool, &uri(id as i64))
                        .await
                        .unwrap(),
                );
            }
            let jobs = crate::queue::claim_batch(&pool, "osm", &cfg, 50)
                .await
                .unwrap();
            assert_eq!(jobs.len(), to - from + 1);
            crate::queue::run_batch(&pool, resolver, &cfg, &jobs)
                .await
                .unwrap();
        }
        // An event and a job posting placed at node 7, which LocationIQ answered.
        let events = crate::event::service::EventService {
            pool: pool.clone(),
            config: crate::config::EventConfig::default(),
            osm: locationiq.clone(),
        };
        let mut placed_ids = vec![];
        for (url, kind) in [
            ("https://events.example/e/1", "MusicEvent"),
            ("https://jobs.example/j/1", "JobPosting"),
        ] {
            let mut doc = structured_venue();
            doc.url = url.into();
            doc.set_type(kind);
            doc.start = Some("2026-12-01T18:00:00Z".parse().unwrap());
            if kind == "JobPosting" {
                events.workplace(&mut doc).await.unwrap();
            } else {
                events.venue(&mut doc).await.unwrap();
            }
            assert_eq!(
                doc.event.venue.as_ref().unwrap().osm_id.as_deref(),
                Some("node/7")
            );
            let id = crate::store::ensure_resource(&pool, url).await.unwrap();
            crate::store::save_document(&pool, &id, 0, &doc)
                .await
                .unwrap();
            placed_ids.push(id);
        }
        for id in ids.iter().chain(&placed_ids) {
            tag(&pool, id).await;
        }
        // Served with LocationIQ's authorization, Nominatim selected.
        let mut authorized = crate::config::Config::default();
        authorized.osm.locationiq.public_use_authorized = true;
        let app = crate::api::router(
            crate::api::AppState::new(pool.clone(), authorized)
                .await
                .unwrap(),
        );
        let osm = crate::resolver::osm::ATTRIBUTION;
        let credit = |card: &Value| card["attribution"].as_str().unwrap_or_default().to_owned();
        let expected = |id: &str| {
            if [&ids[0], &placed_ids[0], &placed_ids[1]].contains(&&id.to_owned()) {
                ATTRIBUTION
            } else {
                osm
            }
        };
        for id in ids.iter().chain(&placed_ids) {
            let card = read(&app, "GET", &format!("/v1/resources/{id}"), None).await;
            assert!(credit(&card).starts_with(expected(id)), "{id}: {card}");
        }
        let uris: Vec<String> = (1..=4)
            .map(uri)
            .chain([
                "https://events.example/e/1".into(),
                "https://jobs.example/j/1".into(),
            ])
            .collect();
        let cards = read(
            &app,
            "POST",
            "/v1/resources/lookup",
            Some(json!({ "uris": uris })),
        )
        .await;
        assert_eq!(cards.as_array().unwrap().len(), 6, "{cards}");
        for card in cards.as_array().unwrap() {
            let id = card["id"].as_str().unwrap();
            assert!(credit(card).starts_with(expected(id)), "{card}");
        }
        let found = read(&app, "GET", "/v1/search?bbox=5,45,15,53&limit=50", None).await;
        let items = found["items"].as_array().unwrap();
        assert_eq!(items.len(), 6, "{found}");
        for card in items {
            let id = card["id"].as_str().unwrap();
            assert!(credit(card).starts_with(expected(id)), "{card}");
        }
        // Two cells: Lugano's mixes LocationIQ's places with Nominatim's node 2; Berlin's
        // holds Nominatim's alone.
        let map = read(
            &app,
            "GET",
            "/v1/search?bbox=5,45,15,53&cluster=6&limit=2",
            None,
        )
        .await;
        let clusters = map["clusters"].as_array().unwrap();
        assert_eq!(clusters.len(), 2, "{map}");
        for cluster in clusters {
            let berlin = cluster["lat"].as_f64().unwrap() > 50.0;
            assert_eq!(
                cluster["attribution"],
                if berlin { osm } else { ATTRIBUTION },
                "{cluster}"
            );
        }
    }

    /// Switching back to Nominatim does not take LocationIQ's answers out of what Pinakes
    /// serves: a Nominatim refresh adds a version, and the resource's change history still
    /// shows the one LocationIQ answered, as does the dump file. While any of them remains,
    /// the authorization must stay: without it startup is refused, whatever the provider. A
    /// dump file holding them is enough; Nominatim's places alone never are.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn retained_answers_keep_needing_the_authorization(pool: PgPool) {
        let (liq, _) = serve(|_, q| {
            axum::Json(
                q["osm_ids"]
                    .split(',')
                    .map(|id| row("node", id[1..].parse().unwrap(), "Cafe Nassa"))
                    .collect::<Vec<_>>(),
            )
            .into_response()
        })
        .await;
        let (nom, _) = nominatim().await;
        let cfg = crate::config::QueueConfig::default();
        let id = crate::store::ensure_resource(&pool, &uri(1)).await.unwrap();
        let run = |resolver: OsmResolver| {
            let (pool, cfg) = (pool.clone(), cfg.clone());
            async move {
                let jobs = crate::queue::claim_batch(&pool, "osm", &cfg, 50)
                    .await
                    .unwrap();
                assert_eq!(jobs.len(), 1);
                crate::queue::run_batch(&pool, &resolver, &cfg, &jobs)
                    .await
                    .unwrap();
            }
        };
        run(OsmResolver::with_base(pool.clone(), config(), &liq).unwrap()).await;
        let dir = std::env::temp_dir().join(format!("pinakes-retained-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::osm_dump::write(
            &pool,
            &dir,
            Publication::check(&pool, &config()).await.unwrap(),
        )
        .await
        .unwrap();
        // The switch: Nominatim selected, the authorization cleared, the place refreshed.
        let mut switched = OsmConfig {
            nominatim_url: nom,
            requests_per_second: 1000.0,
            dump_dir: dir.to_str().unwrap().into(),
            ..Default::default()
        };
        sqlx::query("UPDATE osm_place SET refresh_at=now()-interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE resource SET next_attempt_at=now(),refresh_at=now() WHERE id=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        run(OsmResolver::new(pool.clone(), switched.clone()).unwrap()).await;
        let (pinned, credits): (String, Vec<String>) = sqlx::query_as(
            "SELECT o.versions->r.osm_version->>'attribution',ARRAY(SELECT v->>'attribution' FROM jsonb_each(o.versions) e(k,v) ORDER BY 1) FROM resource r JOIN osm_place o USING(osm_type,osm_id) WHERE r.id=$1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            pinned,
            crate::resolver::osm::ATTRIBUTION,
            "the refresh is shown"
        );
        assert!(credits.contains(&ATTRIBUTION.to_owned()), "{credits:?}");
        let history: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM change_document WHERE resource_id=$1 AND after->>'attribution'=$2",
        )
        .bind(&id)
        .bind(ATTRIBUTION)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(history > 0, "the history shows LocationIQ's version");
        let refused = check_retained(&pool, &switched).await.unwrap_err();
        let text = format!("{refused:#}");
        assert!(text.contains("public_use_authorized"), "{text}");
        // With the permission kept, Nominatim serves beside what LocationIQ answered.
        switched.locationiq.public_use_authorized = true;
        check_retained(&pool, &switched).await.unwrap();
        switched.locationiq.public_use_authorized = false;
        // The places gone (a new database), the dump file still holds them.
        sqlx::query("DELETE FROM osm_place")
            .execute(&pool)
            .await
            .unwrap();
        let refused = check_retained(&pool, &switched).await.unwrap_err();
        assert!(format!("{refused:#}").contains("dump"), "{refused:#}");
        // Nominatim's places, and a dump of them alone, need nothing.
        crate::resolver::osm::OsmResolver::new(pool.clone(), switched.clone())
            .unwrap()
            .resolve(&uri(3))
            .await
            .unwrap();
        crate::osm_dump::write(
            &pool,
            &dir,
            Publication::check(&pool, &config()).await.unwrap(),
        )
        .await
        .unwrap();
        check_retained(&pool, &switched).await.unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        check_retained(&pool, &switched).await.unwrap();
    }

    /// A one-line venue is placed only when every part of its address after the street is the
    /// hit's: a state, postcode, town or country the line names that the hit contradicts, or
    /// that it cannot confirm, places nothing, and words of the venue's name before the street
    /// never stand in for its town.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn a_one_line_address_the_hit_contradicts_places_nothing(pool: PgPool) {
        let springfield = |id: i64, state: &str, postcode: &str| {
            let mut hit = row("node", id, "Springfield Hall");
            hit["address"] = json!({"road": "Main Street", "house_number": "5", "city": "Springfield", "state": state, "postcode": postcode, "country": "United States", "country_code": "us"});
            hit
        };
        let lugano = |id: i64| {
            let mut hit = row("node", id, "Cafe Nassa");
            hit["address"] = json!({"road": "Via Nassa", "house_number": "5", "city": "Lugano", "state": "Ticino", "postcode": "6900", "country": "Switzerland", "country_code": "ch"});
            hit
        };
        // One provider answers Springfield, Massachusetts; the other Springfield, Illinois.
        let (massachusetts, _) = serve(move |_, q| {
            let hits = if q["q"].contains("Main Street") {
                vec![springfield(1, "Massachusetts", "01103")]
            } else {
                vec![lugano(7)]
            };
            axum::Json(hits).into_response()
        })
        .await;
        let (illinois, _) = serve(move |_, _| {
            axum::Json(vec![springfield(2, "Illinois", "62704")]).into_response()
        })
        .await;
        let placed = |base: String, line: &'static str| {
            let pool = pool.clone();
            async move {
                let mut doc = event_page(json!(line), false);
                events(&pool, &base).venue(&mut doc).await.unwrap();
                doc.event.venue.and_then(|v| v.osm_id)
            }
        };
        for line in [
            "5 Main Street, Springfield, IL 62704, USA",
            "5 Main Street, Springfield, Illinois, USA",
            "Lugano Cafe, Via Nassa 5, Bellinzona",
            "Via Nassa 5, 6900 Lugano, Old Town",
            "Via Nassa 5, 6901 Lugano",
            "Via Nassa 5, Lugano, Italy",
            "Cafe Nassa, Via Nassa 5",
        ] {
            assert_eq!(placed(massachusetts.clone(), line).await, None, "{line}");
        }
        for (base, line, at) in [
            (
                &massachusetts,
                "Cafe Nassa, Via Nassa 5, 6900 Lugano, Switzerland",
                "node/7",
            ),
            (
                &massachusetts,
                "Via Nassa 5, 6900 Lugano, Ticino, CH",
                "node/7",
            ),
            (
                &illinois,
                "Springfield Hall, 5 Main Street, Springfield, Illinois 62704, USA",
                "node/2",
            ),
        ] {
            assert_eq!(
                placed(base.clone(), line).await.as_deref(),
                Some(at),
                "{line}"
            );
        }
    }

    /// A map cluster shows its places at their current point (the version `osm_place` holds
    /// now) and so credits that version's provider: events Nominatim placed at one venue, after
    /// LocationIQ refreshed the venue to another point, cluster at LocationIQ's point with
    /// LocationIQ's credit. Each event's own card still shows the version it pinned:
    /// Nominatim's point and credit.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn a_cluster_credits_the_version_whose_point_it_shows(pool: PgPool) {
        let nominatim_row = || {
            let mut row = row("node", 7, "Cafe Nassa");
            row["osm_id"] = 7.into();
            row
        };
        let (nom, _) = serve(move |_, _| axum::Json(vec![nominatim_row()]).into_response()).await;
        let (liq, _) = serve(|_, _| {
            let mut moved = row("node", 7, "Cafe Nassa");
            moved["lat"] = "46.1".into();
            moved["lon"] = "8.9".into();
            moved["boundingbox"] = json!(["46.09", "46.11", "8.89", "8.91"]);
            axum::Json(vec![moved]).into_response()
        })
        .await;
        let nominatim = OsmConfig {
            nominatim_url: nom,
            requests_per_second: 1000.0,
            ..Default::default()
        };
        let events = crate::event::service::EventService {
            pool: pool.clone(),
            config: crate::config::EventConfig::default(),
            osm: Arc::new(OsmResolver::new(pool.clone(), nominatim).unwrap()),
        };
        let mut ids = vec![];
        for n in 1..=2 {
            let url = format!("https://events.example/e/{n}");
            let mut doc = structured_venue();
            doc.url = url.clone();
            doc.set_type("MusicEvent");
            doc.start = Some("2026-12-01T18:00:00Z".parse().unwrap());
            events.venue(&mut doc).await.unwrap();
            assert_eq!(
                doc.event.venue.as_ref().unwrap().osm_id.as_deref(),
                Some("node/7")
            );
            let id = crate::store::ensure_resource(&pool, &url).await.unwrap();
            crate::store::save_document(&pool, &id, 0, &doc)
                .await
                .unwrap();
            tag(&pool, &id).await;
            ids.push(id);
        }
        // The venue's own resource, refreshed through LocationIQ (authorized) to another point
        // once the cached answer is due.
        crate::store::ensure_resource(&pool, &uri(7)).await.unwrap();
        sqlx::query("UPDATE osm_place SET refresh_at=now()-interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let cfg = crate::config::QueueConfig::default();
        let jobs = crate::queue::claim_batch(&pool, "osm", &cfg, 50)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        let locationiq = OsmResolver::with_base(pool.clone(), config(), &liq).unwrap();
        crate::queue::run_batch(&pool, &locationiq, &cfg, &jobs)
            .await
            .unwrap();
        let mut authorized = crate::config::Config::default();
        authorized.osm.locationiq.public_use_authorized = true;
        let app = crate::api::router(crate::api::AppState::new(pool, authorized).await.unwrap());
        let map = read(
            &app,
            "GET",
            "/v1/search?bbox=5,45,11,48&cluster=8&type=Event&limit=1",
            None,
        )
        .await;
        let clusters = map["clusters"].as_array().unwrap();
        assert_eq!(clusters.len(), 1, "{map}");
        assert_eq!(clusters[0]["count"], 2, "{map}");
        assert!(
            (clusters[0]["lat"].as_f64().unwrap() - 46.1).abs() < 1e-6,
            "{map}"
        );
        assert_eq!(clusters[0]["attribution"], ATTRIBUTION, "{map}");
        for id in &ids {
            let card = read(&app, "GET", &format!("/v1/resources/{id}"), None).await;
            assert_eq!(card["point"]["lat"], 46.0037, "{card}");
            let credit = card["attribution"].as_str().unwrap();
            assert!(
                credit.starts_with(crate::resolver::osm::ATTRIBUTION)
                    && !credit.contains("LocationIQ"),
                "{card}"
            );
        }
    }

    /// A request to the public API: its status and body.
    async fn fetch(app: &Router, method: &str, path: &str) -> (StatusCode, Vec<u8>) {
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        (status, bytes.to_vec())
    }
    /// A Nominatim configuration at `base` whose dump directory is `dir`, without LocationIQ's
    /// authorization.
    fn switched(base: &str, dir: &std::path::Path) -> crate::config::Config {
        crate::config::Config {
            osm: OsmConfig {
                nominatim_url: base.into(),
                requests_per_second: 1000.0,
                dump_dir: dir.to_str().unwrap().into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    /// A dump file as another writer leaves it: written beside, then renamed into place.
    fn replace_dump(dir: &std::path::Path, credit: &str) {
        let tmp = dir.join("replacement.tmp");
        let mut gz =
            flate2::write::GzEncoder::new(std::fs::File::create(&tmp).unwrap(), Default::default());
        std::io::Write::write_all(
            &mut gz,
            format!(
                "{}\n{}\n",
                crate::osm_dump::notice(),
                json!({"osm_type": "node", "osm_id": 1, "attribution": credit, "versions": {}})
            )
            .as_bytes(),
        )
        .unwrap();
        gz.finish().unwrap();
        std::fs::rename(tmp, crate::osm_dump::path(dir)).unwrap();
    }

    /// The core serving and export paths enforce the authorization themselves, not only the
    /// daemon: the API state is not built over retained LocationIQ answers without it, the
    /// dump writer writes none of them without it, and a Nominatim resolver does not answer
    /// from a cached LocationIQ document without it. With it, all of them do.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn serving_and_export_refuse_retained_answers_without_the_authorization(pool: PgPool) {
        let (nom, nominatim_calls) = nominatim().await;
        let dir = std::env::temp_dir().join(format!("pinakes-serving-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let clean = switched(&nom, &dir);
        // Checked while the database is clean: a Nominatim instance needs no key.
        let publication = Publication::check(&pool, &clean.osm).await.unwrap();
        assert!(!publication.locationiq());
        crate::api::AppState::new(pool.clone(), clean.clone())
            .await
            .unwrap();
        crate::osm_dump::write(&pool, &dir, publication)
            .await
            .unwrap();
        let written = std::fs::read(crate::osm_dump::path(&dir)).unwrap();
        // LocationIQ answers node 1, which stays cached.
        let (liq, _) =
            serve(|_, _| axum::Json(vec![row("node", 1, "Cafe Nassa")]).into_response()).await;
        OsmResolver::with_base(pool.clone(), config(), &liq)
            .unwrap()
            .resolve(&uri(1))
            .await
            .unwrap();
        assert!(crate::api::AppState::new(pool.clone(), clean.clone())
            .await
            .is_err());
        let refused = crate::osm_dump::write(&pool, &dir, publication)
            .await
            .unwrap_err();
        assert!(format!("{refused:#}").contains("LocationIQ"), "{refused:#}");
        assert_eq!(std::fs::read(crate::osm_dump::path(&dir)).unwrap(), written);
        let unauthorized = OsmResolver::new(pool.clone(), clean.osm.clone()).unwrap();
        let doc = unauthorized.resolve(&uri(1)).await.unwrap();
        assert_eq!(
            doc.attribution.as_deref(),
            Some(crate::resolver::osm::ATTRIBUTION)
        );
        assert_eq!(
            nominatim_calls.calls.lock().unwrap().len(),
            1,
            "asked again"
        );
        // Authorized: written and served.
        let mut authorized = clean.clone();
        authorized.osm.locationiq.public_use_authorized = true;
        let publication = Publication::check(&pool, &authorized.osm).await.unwrap();
        crate::osm_dump::write(&pool, &dir, publication)
            .await
            .unwrap();
        let state = crate::api::AppState::new(pool.clone(), authorized.clone())
            .await
            .unwrap();
        let (status, _) = fetch(
            &crate::api::router(state),
            "GET",
            "/v1/dumps/osm_place.ndjson.gz",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The file a download opens is the one checked: another writer sharing the dump directory
    /// (an authorized daemon on another database) can rename a file of LocationIQ answers into
    /// place after this instance started clean. Without the authorization it is withheld, and
    /// a clean replacement is served again; a file once found clean never vouches for its
    /// replacement.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn a_replaced_dump_file_is_checked_before_it_is_served(pool: PgPool) {
        let (nom, _) = nominatim().await;
        let dir = std::env::temp_dir().join(format!("pinakes-replaced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = switched(&nom, &dir);
        let state = crate::api::AppState::new(pool.clone(), config.clone())
            .await
            .unwrap();
        crate::osm_dump::write(&pool, &dir, state.publication())
            .await
            .unwrap();
        let app = crate::api::router(state);
        let path = "/v1/dumps/osm_place.ndjson.gz";
        assert_eq!(fetch(&app, "GET", path).await.0, StatusCode::OK);
        for _ in 0..2 {
            replace_dump(&dir, ATTRIBUTION);
            for method in ["GET", "HEAD"] {
                let (status, body) = fetch(&app, method, path).await;
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method}");
                assert!(!String::from_utf8_lossy(&body).contains("LocationIQ.com"));
            }
            replace_dump(&dir, crate::resolver::osm::ATTRIBUTION);
            assert_eq!(fetch(&app, "GET", path).await.0, StatusCode::OK);
        }
        // An authorized instance serves the same file.
        replace_dump(&dir, ATTRIBUTION);
        let mut authorized = config;
        authorized.osm.locationiq.public_use_authorized = true;
        let state = crate::api::AppState::new(pool, authorized).await.unwrap();
        assert_eq!(
            fetch(&crate::api::router(state), "GET", path).await.0,
            StatusCode::OK
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An event page's JSON-LD Event at `location`, read as the event resolver reads it.
    fn event_page(location: Value, online: bool) -> crate::model::ResourceMeta {
        let page = "https://events.example/e/1";
        let mut doc = crate::model::ResourceMeta::new(page, "event", 3600);
        let mut event = json!({
            "@context": "https://schema.org",
            "@type": "Event",
            "name": "Night",
            "url": page,
            "startDate": "2026-12-01T19:00:00+01:00",
            "location": location
        });
        if online {
            event["eventAttendanceMode"] = "https://schema.org/OnlineEventAttendanceMode".into();
        }
        crate::event::page::jsonld(&mut doc, &event, &page.parse().unwrap());
        crate::event::page::finalize(&mut doc, true);
        doc
    }

    /// An event page whose venue is one line of text (JSON-LD `location` as a string) is
    /// placed through LocationIQ's free-text search when the line is a street address, and
    /// only at the one OSM hit whose house number and street, and town or postcode, are in
    /// that line. A bare venue name, a line without a number, an online event and a page's own
    /// coordinates are never searched; no hit, two hits, a hit without OSM identity and a hit
    /// at another number or town place nothing. Nominatim searches no line of text.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn an_event_venue_in_one_line_is_placed_only_at_its_own_address(pool: PgPool) {
        let hit = |id: i64, number: &str, city: &str, postcode: &str| {
            let mut hit = row("node", id, "Cafe Nassa");
            hit["address"] = json!({"road": "Via Nassa", "house_number": number, "city": city, "postcode": postcode, "country_code": "ch"});
            hit
        };
        let (base, mock) = serve(move |path, q| {
            assert_eq!(path, "/v1/search", "{q:?}");
            let hits = match q["q"].as_str() {
                "Cafe Nassa, Via Nassa 5, 6900 Lugano" => vec![hit(7, "5", "Lugano", "6900")],
                "Via Nassa 5, Lugano" => vec![hit(8, "7", "Lugano", "6900")],
                "Via Nassa 5, 6900 Lugano, Switzerland" => {
                    vec![hit(9, "5", "Lugano", "6900"), hit(10, "5", "Lugano", "6900")]
                }
                "Via Nassa 5, Bellinzona" => vec![hit(11, "5", "Lugano", "6900")],
                "Nassa 5, Lugano" => vec![hit(13, "5", "Lugano", "6900")],
                "Piazza Grande 3, Locarno" => vec![json!({
                    "place_id": "3", "lat": "46.17", "lon": "8.79", "class": "place", "type": "house",
                    "address": {"road": "Piazza Grande", "house_number": "3", "city": "Locarno"}
                })],
                _ => vec![],
            };
            axum::Json(hits).into_response()
        })
        .await;
        let events = events(&pool, &base);
        let placed_at = |doc: crate::model::ResourceMeta| doc.event.venue.and_then(|v| v.osm_id);
        let mut doc = event_page(json!("Cafe Nassa, Via Nassa 5, 6900 Lugano"), false);
        assert!(doc.event.venue.as_ref().unwrap().address.is_empty());
        events.venue(&mut doc).await.unwrap();
        let venue = doc.event.venue.clone().unwrap();
        assert_eq!(venue.osm_id.as_deref(), Some("node/7"));
        assert!(venue.osm_version.is_some());
        assert!(matches!(
            venue.location_source,
            Some(crate::model::LocationSource::Geocoded)
        ));
        assert_eq!(attribution(&pool, 7).await.as_deref(), Some(ATTRIBUTION));
        let (_, q) = mock.calls.lock().unwrap()[0].clone();
        for (k, v) in [
            ("q", "Cafe Nassa, Via Nassa 5, 6900 Lugano"),
            ("format", "json"),
            ("addressdetails", "1"),
            ("limit", "2"),
            ("key", KEY),
        ] {
            assert_eq!(q.get(k).map(String::as_str), Some(v), "{k}");
        }
        for line in [
            "Via Nassa 5, Lugano",
            "Via Nassa 5, 6900 Lugano, Switzerland",
            "Via Nassa 5, Bellinzona",
            "Nassa 5, Lugano",
            "Piazza Grande 3, Locarno",
            "Via Nassa 5, 6900 Nowhere",
        ] {
            let mut doc = event_page(json!(line), false);
            events.venue(&mut doc).await.unwrap();
            assert_eq!(placed_at(doc), None, "{line}");
        }
        let asked = mock.calls.lock().unwrap().len();
        assert_eq!(asked, 7);
        // Never searched: a bare name, a line without a number, an online event, and a venue
        // the page places itself.
        for mut doc in [
            event_page(json!("Cafe Nassa"), false),
            event_page(json!("Cafe Nassa, Lugano"), false),
            event_page(json!("Cafe Nassa, Via Nassa 5, 6900 Lugano"), true),
            event_page(
                json!({"@type": "Place", "name": "Cafe Nassa, Via Nassa 5, 6900 Lugano", "geo": {"latitude": 46.0, "longitude": 8.95}}),
                false,
            ),
        ] {
            let before = format!(
                "{:?}",
                doc.event.venue.as_ref().and_then(|v| v.location.clone())
            );
            events.venue(&mut doc).await.unwrap();
            let venue = doc.event.venue.clone();
            assert!(venue.as_ref().and_then(|v| v.osm_id.clone()).is_none());
            assert_eq!(format!("{:?}", venue.and_then(|v| v.location)), before);
        }
        assert_eq!(mock.calls.lock().unwrap().len(), asked);
        let stored: Vec<i64> = sqlx::query_scalar("SELECT osm_id FROM osm_place ORDER BY osm_id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(stored, [7]);
        // Nominatim: the same line is not searched.
        let nominatim = crate::event::service::EventService {
            pool: pool.clone(),
            config: crate::config::EventConfig::default(),
            osm: Arc::new(
                OsmResolver::new(
                    pool.clone(),
                    OsmConfig {
                        nominatim_url: base.clone(),
                        requests_per_second: 1000.0,
                        ..Default::default()
                    },
                )
                .unwrap(),
            ),
        };
        let mut doc = event_page(json!("Cafe Nassa, Via Nassa 5, 6900 Lugano"), false);
        nominatim.venue(&mut doc).await.unwrap();
        assert_eq!(placed_at(doc), None);
        assert_eq!(mock.calls.lock().unwrap().len(), asked);
    }

    /// A mock answering 429 with `body` (and `Retry-After: retry`, when given) to its first
    /// request and a row to every later one, and the times it was asked.
    async fn throttled_once(
        body: &'static str,
        retry: Option<&'static str>,
    ) -> (String, Mock, Arc<Mutex<Vec<std::time::Instant>>>) {
        let times: Arc<Mutex<Vec<std::time::Instant>>> = Arc::default();
        let seen = times.clone();
        let (base, mock) = serve(move |path, q| {
            let mut times = seen.lock().unwrap();
            times.push(std::time::Instant::now());
            if times.len() == 1 {
                let mut response = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
                if let Some(retry) = retry {
                    response
                        .headers_mut()
                        .insert("retry-after", retry.parse().unwrap());
                }
                return response;
            }
            let ids = if path == "/v1/lookup" {
                q["osm_ids"].clone()
            } else {
                "N7".into()
            };
            axum::Json(
                ids.split(',')
                    .map(|id| row("node", id[1..].parse().unwrap(), "Cafe Nassa"))
                    .collect::<Vec<_>>(),
            )
            .into_response()
        })
        .await;
        (base, mock, times)
    }
    fn deferred(error: &anyhow::Error) -> (&'static str, u64) {
        let later = error
            .downcast_ref::<crate::resolver::RetryLater>()
            .unwrap_or_else(|| panic!("not deferred: {error:#}"));
        (later.class, later.seconds)
    }
    fn structured_venue() -> crate::model::ResourceMeta {
        placed(json!({
            "text": "Cafe Nassa, Via Nassa 5, 6900 Lugano",
            "name": "Cafe Nassa",
            "address": {"street": "Via Nassa 5", "city": "Lugano", "postalcode": "6900"}
        }))
    }

    /// After LocationIQ throttles, every request on the shared budget (another batch's lookup,
    /// an event venue's search, callers waiting at the same time) is deferred at once with the
    /// wait that remains, without asking LocationIQ and without sleeping it out; a daily quota
    /// defers for the hour. A refused key stays an error and starts no wait.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn a_provider_cooldown_defers_later_requests_at_once(pool: PgPool) {
        let quick = std::time::Duration::from_secs(5);
        for (body, retry, wait) in [
            (r#"{"error":"Rate Limited Minute"}"#, Some("60"), 60),
            (r#"{"error":"Rate Limited Day"}"#, None, 3600),
        ] {
            let (base, mock, _) = throttled_once(body, retry).await;
            let osm = Arc::new(OsmResolver::with_base(pool.clone(), config(), &base).unwrap());
            let events = crate::event::service::EventService {
                pool: pool.clone(),
                config: crate::config::EventConfig::default(),
                osm: osm.clone(),
            };
            let first = osm.resolve(&uri(1)).await.unwrap_err();
            assert_eq!(deferred(&first), ("http_4xx", wait));
            let started = std::time::Instant::now();
            let batch = tokio::time::timeout(quick, osm.resolve_batch(&[uri(2), uri(3), uri(4)]))
                .await
                .expect("a deferral, not a wait");
            for result in &batch {
                let (class, seconds) = deferred(result.as_ref().unwrap_err());
                assert_eq!(class, "http_4xx");
                assert!((wait - 2..=wait).contains(&seconds), "{seconds}");
            }
            let mut doc = structured_venue();
            let venue = tokio::time::timeout(quick, events.venue(&mut doc))
                .await
                .expect("a deferral, not a wait");
            assert_eq!(deferred(&venue.unwrap_err()).0, "http_4xx");
            assert!(doc.event.venue.unwrap().osm_id.is_none());
            // Callers at the same time are each deferred, none asks.
            let (five, six, seven) = (uri(5), uri(6), uri(7));
            let (a, b, c) = tokio::time::timeout(quick, async {
                tokio::join!(osm.resolve(&five), osm.probe(&six), osm.resolve(&seven))
            })
            .await
            .expect("deferrals, not waits");
            for result in [a, b, c] {
                assert_eq!(deferred(&result.unwrap_err()).0, "http_4xx");
            }
            assert!(started.elapsed() < quick);
            assert_eq!(mock.calls.lock().unwrap().len(), 1, "{body}");
        }
        // A refused key is an error, never a wait: the next request is sent.
        let refused = Arc::new(Mutex::new(0));
        let count = refused.clone();
        let (base, mock) = serve(move |_, _| {
            let mut n = count.lock().unwrap();
            *n += 1;
            if *n == 1 {
                (StatusCode::UNAUTHORIZED, r#"{"error":"Invalid key"}"#).into_response()
            } else {
                axum::Json(vec![row("node", 2, "Cafe")]).into_response()
            }
        })
        .await;
        let osm = OsmResolver::with_base(pool.clone(), config(), &base).unwrap();
        let error = osm.resolve(&uri(1)).await.unwrap_err();
        assert!(error
            .downcast_ref::<crate::resolver::RetryLater>()
            .is_none());
        assert!(osm.resolve(&uri(2)).await.is_ok());
        assert_eq!(mock.calls.lock().unwrap().len(), 2);
    }

    /// Requests on one resolver share its rate whoever sends them: lookups and venue searches
    /// at the same time go one after the other, at `limits.locationiq_requests_per_second`.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn lookups_and_venue_searches_share_one_rate(pool: PgPool) {
        let (base, mock, times) = throttled_once(r#"{"error":"Rate Limited Second"}"#, None).await;
        let mut paced = config();
        paced.locationiq.requests_per_second = 4.0;
        let osm = Arc::new(OsmResolver::with_base(pool.clone(), paced, &base).unwrap());
        let events = crate::event::service::EventService {
            pool: pool.clone(),
            config: crate::config::EventConfig::default(),
            osm: osm.clone(),
        };
        // The one-second wait of a per-second limit, then three callers at once.
        assert_eq!(
            deferred(&osm.resolve(&uri(1)).await.unwrap_err()),
            ("http_4xx", 1)
        );
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let mut doc = structured_venue();
        let (two, three) = (uri(2), uri(3));
        let (a, b, c) = tokio::join!(
            osm.resolve(&two),
            events.venue(&mut doc),
            osm.resolve(&three)
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        assert_eq!(doc.event.venue.unwrap().osm_id.as_deref(), Some("node/7"));
        assert_eq!(mock.calls.lock().unwrap().len(), 4);
        let times = times.lock().unwrap().clone();
        for pair in times[1..].windows(2) {
            assert!(
                pair[1] - pair[0] >= std::time::Duration::from_millis(240),
                "{:?}",
                pair[1] - pair[0]
            );
        }
    }

    /// In the queue, jobs LocationIQ's cooldown defers wait for its end: no attempt, timeout or
    /// dead letter is charged to them, the next batch is deferred at once without a request,
    /// and their documents stay as they were.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn queued_jobs_wait_out_a_cooldown_uncharged(pool: PgPool) {
        let (base, mock, _) =
            throttled_once(r#"{"error":"Rate Limited Minute"}"#, Some("60")).await;
        let osm = OsmResolver::with_base(pool.clone(), config(), &base).unwrap();
        for id in 1..=6 {
            crate::store::ensure_resource(&pool, &uri(id))
                .await
                .unwrap();
        }
        let cfg = crate::config::QueueConfig::default();
        for round in 0..2 {
            let jobs = crate::queue::claim_batch(&pool, "osm", &cfg, osm.batch_size())
                .await
                .unwrap();
            assert_eq!(jobs.len(), 3, "round {round}");
            let started = std::time::Instant::now();
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                crate::queue::run_batch(&pool, &osm, &cfg, &jobs),
            )
            .await
            .expect("deferred, not timed out")
            .unwrap();
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            let ids: Vec<String> = jobs.iter().map(|j| j.id.clone()).collect();
            let rows: Vec<(i32, i32, bool, Option<String>, String, f64)> = sqlx::query_as(
                "SELECT attempts,timeout_streak,dead_letter,last_error,status,extract(epoch FROM next_attempt_at-now())::float8 FROM resource WHERE id=ANY($1)",
            )
            .bind(&ids)
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(rows.len(), 3);
            for (attempts, streak, dead, error, status, wait) in rows {
                assert_eq!((attempts, streak, dead), (0, 0, false), "round {round}");
                assert_eq!(error.as_deref(), Some("http_4xx"), "round {round}");
                assert_eq!(status, "pending", "round {round}");
                assert!((50.0..=61.0).contains(&wait), "round {round}: {wait}");
            }
        }
        assert_eq!(mock.calls.lock().unwrap().len(), 1);
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM osm_place")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 0);
    }

    /// An answer that does not come in time is a timeout to retry, whose error names neither
    /// the key nor the URL, after the client's ten seconds and no more.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn a_slow_answer_times_out_without_the_key(pool: PgPool) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            // Reads the request and never answers it.
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(15));
            drop(stream);
        });
        let osm = OsmResolver::with_base(pool.clone(), config(), &base).unwrap();
        let started = std::time::Instant::now();
        let error = osm.resolve(&uri(1)).await.unwrap_err();
        let took = started.elapsed();
        assert_eq!(deferred(&error).0, "timeout");
        assert!(
            (std::time::Duration::from_secs(9)..std::time::Duration::from_secs(13)).contains(&took),
            "{took:?}"
        );
        let text = format!("{error:#} {error:?}");
        assert!(
            !text.contains("KEYCANARY") && !text.contains("127.0.0.1"),
            "{text}"
        );
    }

    /// A row keeps only a real OSM identity: a node, way or relation with a positive id, as a
    /// number or a decimal string; LocationIQ's `place_id` is dropped and never read as one.
    #[test]
    fn rows_keep_only_a_real_osm_identity() {
        let with = |kind: Value, id: Value| {
            let mut row = row("node", 1, "Cafe");
            row["osm_type"] = kind;
            row["osm_id"] = id;
            normalize(row)
        };
        for (id, read) in [
            (json!(5), 5),
            (json!("5"), 5),
            (json!("9223372036854775807"), i64::MAX),
        ] {
            let row = with(json!("way"), id.clone()).unwrap_or_else(|| panic!("{id}"));
            assert_eq!(row["osm_id"], json!(read), "{id}");
            assert_eq!(row["osm_type"], "way");
            assert!(row.get("place_id").is_none());
            assert_eq!(row["category"], "amenity");
            assert_eq!(row["name"], "Cafe");
        }
        for id in [
            json!("05"),
            json!("-5"),
            json!(-5),
            json!(0),
            json!("0"),
            json!("9223372036854775808"),
            json!(5.5),
            json!("5 "),
            json!(null),
        ] {
            assert!(with(json!("node"), id.clone()).is_none(), "{id}");
        }
        for kind in [json!("N"), json!("changeset"), json!(null), json!(1)] {
            assert!(with(kind.clone(), json!("5")).is_none(), "{kind}");
        }
        let mut place_only = row("node", 1, "Cafe");
        place_only.as_object_mut().unwrap().remove("osm_id");
        assert!(normalize(place_only).is_none());
        // A category or name the row has is kept.
        let mut named = row("node", 1, "Cafe");
        named["category"] = "shop".into();
        named["name"] = "Own".into();
        let named = normalize(named).unwrap();
        assert_eq!(
            (&named["category"], &named["name"]),
            (&json!("shop"), &json!("Own"))
        );
    }

    /// The official Lookup HTTP 200 example is one object (not an authenticated recording).
    /// Accept that LocationIQ envelope as well as its batch array, without relaxing Nominatim.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../../tests/fixtures/require_h3.sql")
    )]
    async fn lookup_accepts_the_documented_single_object_envelope(pool: PgPool) {
        let (base, mock) = serve(|_, _| {
            axum::Json(json!({
                "place_id": 115462561,
                "licence": "https://locationiq.com/attribution",
                "osm_type": "way",
                "osm_id": 50637691,
                "lat": "52.399550700000006",
                "lon": "13.048066846939687",
                "display_name": "Brandenburger Tor, Potsdam, Germany",
                "class": "tourism",
                "type": "attraction"
            }))
            .into_response()
        })
        .await;
        let uri = "https://www.openstreetmap.org/way/50637691";
        let resolver = OsmResolver::with_base(pool.clone(), config(), &base).unwrap();
        let doc = resolver.probe(uri).await.unwrap();
        assert_eq!(doc.status, Status::Ok);
        assert_eq!(doc.osm_id.as_deref(), Some("way/50637691"));
        assert_eq!(doc.name.as_deref(), Some("Brandenburger Tor"));
        assert_eq!(doc.category.as_deref(), Some("tourism/attraction"));
        assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));
        assert_eq!(doc.location.as_ref().unwrap().lat, 52.399550700000006);
        // Default Nominatim retains its array-only JSONv2 contract and never gets this key.
        let nominatim = OsmConfig {
            nominatim_url: base.clone(),
            requests_per_second: 1000.0,
            ..Default::default()
        };
        let resolver = OsmResolver::with_base(pool, nominatim, &base).unwrap();
        assert!(resolver
            .probe(uri)
            .await
            .unwrap_err()
            .to_string()
            .contains("not a list"));
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1["format"], "json");
        assert_eq!(calls[0].1["key"], KEY);
        assert_eq!(calls[1].1["format"], "jsonv2");
        assert!(!calls[1].1.contains_key("key"));
    }
}
