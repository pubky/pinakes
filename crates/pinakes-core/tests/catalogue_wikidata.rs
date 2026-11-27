//! The Wikidata importer over a fresh database, against a local server that plays Wikimedia:
//! the dumps directory (with a dump made of real entities, `fixtures/wikidata`), its checksums,
//! ranges of the file, and the API (labels, changed items, recent changes).
//!
//! - A first import finds the newest dump, downloads it on two connections, checks its SHA-1,
//!   reads it and writes one card per item with a Wikipedia article: canonical link, type, keys,
//!   labels (also those of genres read after their cards, and those only the API has).
//! - A cut-off import resumes where it stopped (a broken connection, a failed label lookup).
//! - With a limit the dump is read as a stream and only up to the limit.
//! - An update applies the recent changes, one request at a time, and touches only changed
//!   rows; a long gap waits for the next dump.
use axum::{
    body::Body,
    extract::{Query, State as AxumState},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use pinakes_core::{
    catalogue::{
        run::{self, Context, Pace, Run},
        store,
        wikidata::{Endpoints, Wikidata},
        Catalogue,
    },
    config::Config,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DATE: &str = "20261123";
const ETAG: &str = "\"fixture-1\"";

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/wikidata/dump-sample.json.bz2"
    ))
    .unwrap()
}
fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::Digest;
    sha1::Sha1::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the local Wikimedia serves and what it saw.
#[derive(Default)]
struct Mock {
    dump: Mutex<Vec<u8>>,
    /// Ranges starting at or after this byte answer 400 while set.
    fail_ranges_from: Mutex<Option<u64>>,
    /// The first open-ended range (a stream) breaks after this many bytes, and the ranges after
    /// it answer 400 until `fail_ranges_from` is cleared (unless `reconnect` is set).
    cut_stream: Mutex<Option<usize>>,
    reconnect: AtomicBool,
    /// The dumps directory answers 400 while set.
    index_fails: AtomicBool,
    /// Q-ids read in full with `wbgetentities` (not labels, not sitelinks only).
    asked: Mutex<Vec<String>>,
    /// `wbgetentities` answers 400 once when asked for this Q-id.
    fail_entity_once: Mutex<Option<String>>,
    /// Dated directories the index lists.
    dates: Mutex<Vec<String>>,
    /// The SHA-1 the checksum file lists (none: no line for the file).
    sha1: Mutex<Option<String>>,
    /// The first ranged answer is cut off after this many bytes.
    cut_first_range: Mutex<Option<usize>>,
    /// Range requests seen, as their first byte.
    ranges: Mutex<Vec<u64>>,
    /// Labels the API knows (Q-id -> English label).
    labels: Mutex<HashMap<String, String>>,
    /// Label lookups answer 400 while set.
    labels_fail: AtomicBool,
    /// Items the API serves for `wbgetentities` (Q-id -> entity; `Value::Null` = missing).
    entities: Mutex<HashMap<String, Value>>,
    /// Recent changes served (title, timestamp).
    changes: Mutex<Vec<(String, String)>>,
    /// API requests seen (their `action` and `props`/`list`).
    api: Mutex<Vec<String>>,
    /// Items asked for their sitelinks only (`asked`: read in full).
    asked_sitelinks: Mutex<Vec<String>>,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
    /// How long each API answer takes, in ms (0: 20 ms).
    api_delay_ms: std::sync::atomic::AtomicU64,
    /// The next API requests get these answers first: a status and a `Retry-After`, or (status 0)
    /// Wikimedia's `maxlag` error body.
    forced: Mutex<std::collections::VecDeque<(u16, Option<String>)>>,
    /// API answers this many (a count of forced answers given).
    forced_given: AtomicUsize,
    /// `wbgetentities` leaves this Q-id out of its answer while set.
    omit_entity: Mutex<Option<String>>,
    /// The dump's `ETag` (default `ETAG`).
    etag: Mutex<Option<String>>,
    /// A HEAD of the dump says this size instead.
    head_size: Mutex<Option<u64>>,
    /// The dumps directory redirects elsewhere while set; how often the target was asked.
    index_redirects: AtomicBool,
    redirect_target_hits: AtomicUsize,
    /// A range asked from a byte past 0 is answered from the byte after it while set.
    shift_ranges: AtomicBool,
    /// The dump's `ETag` becomes this when a stream is cut (the file replaced meanwhile).
    etag_after_cut: Mutex<Option<String>>,
    /// The dumps directory's page is sent in chunks, without a `Content-Length`, padded with this
    /// many bytes while set.
    index_padding: AtomicUsize,
    /// Recent changes are answered without their list (warnings only) while set.
    changes_without_list: AtomicBool,
    /// Ranges of the dump are sent 16 KB at a time, this many ms apart, while set.
    drip_ms: std::sync::atomic::AtomicU64,
}

async fn index(AxumState(m): AxumState<Arc<Mock>>) -> Response {
    if m.index_fails.load(Ordering::SeqCst) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if m.index_redirects.load(Ordering::SeqCst) {
        let mut h = HeaderMap::new();
        h.insert("location", "/elsewhere/".parse().unwrap());
        return (StatusCode::FOUND, h).into_response();
    }
    let mut page = String::from("<html><a href=\"../\">../</a>\n");
    for d in m.dates.lock().unwrap().iter() {
        page.push_str(&format!("<a href=\"{d}/\">{d}/</a>\n"));
    }
    page.push_str("<a href=\"latest-all.json.bz2\">latest</a></html>");
    let padding = m.index_padding.load(Ordering::SeqCst);
    if padding > 0 {
        page.push_str(&" ".repeat(padding));
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = page
            .into_bytes()
            .chunks(64 << 10)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        return Body::from_stream(futures_util::stream::iter(chunks)).into_response();
    }
    page.into_response()
}
async fn dated(
    AxumState(m): AxumState<Arc<Mock>>,
    axum::extract::Path((date, file)): axum::extract::Path<(String, String)>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    if !m.dates.lock().unwrap().contains(&date) || date == "20261125" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if file == format!("wikidata-{date}-sha1sums.txt") {
        let sums = match m.sha1.lock().unwrap().clone() {
            Some(s) => format!("{s}  wikidata-{date}-all.json.bz2\nabc  other.gz\n"),
            None => "abc  other.gz\n".into(),
        };
        return sums.into_response();
    }
    if file != format!("wikidata-{date}-all.json.bz2") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let dump = m.dump.lock().unwrap().clone();
    let len = dump.len();
    let mut base = HeaderMap::new();
    let etag = m
        .etag
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| ETAG.to_owned());
    base.insert("etag", etag.parse().unwrap());
    base.insert("accept-ranges", "bytes".parse().unwrap());
    if method == axum::http::Method::HEAD {
        let size = m.head_size.lock().unwrap().unwrap_or(len as u64);
        base.insert("content-length", size.to_string().parse().unwrap());
        return (StatusCode::OK, base).into_response();
    }
    let range = headers
        .get("range")
        .and_then(|r| r.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
        .map(|(a, b)| {
            let a: usize = a.parse().unwrap();
            let b: usize = if b.is_empty() {
                len - 1
            } else {
                b.parse().unwrap()
            };
            (a, b.min(len - 1))
        });
    let if_range_ok = headers.get("if-range").is_none_or(|v| *v == etag);
    let Some((from, to)) = range.filter(|_| if_range_ok) else {
        base.insert("content-length", len.to_string().parse().unwrap());
        return (StatusCode::OK, base, dump).into_response();
    };
    m.ranges.lock().unwrap().push(from as u64);
    let from = if from > 0 && m.shift_ranges.load(Ordering::SeqCst) {
        from + 1
    } else {
        from
    };
    if m.fail_ranges_from
        .lock()
        .unwrap()
        .is_some_and(|f| from as u64 >= f)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let body = dump[from..=to].to_vec();
    let open_ended = headers
        .get("range")
        .is_some_and(|r| r.to_str().unwrap().ends_with('-'));
    if open_ended {
        if let Some(cut) = m.cut_stream.lock().unwrap().take() {
            if let Some(etag) = m.etag_after_cut.lock().unwrap().take() {
                *m.etag.lock().unwrap() = Some(etag);
            }
            if !m.reconnect.load(Ordering::SeqCst) {
                *m.fail_ranges_from.lock().unwrap() = Some(0);
            }
            // The part in chunks, a pause so it is sent, then the connection breaks.
            let part: Vec<Result<bytes::Bytes, std::io::Error>> = body[..cut.min(body.len())]
                .chunks(16 << 10)
                .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                .collect();
            use futures_util::StreamExt as _;
            let stream =
                futures_util::stream::iter(part).chain(futures_util::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Err(std::io::Error::other("cut"))
                }));
            base.insert(
                "content-range",
                format!("bytes {from}-{to}/{len}").parse().unwrap(),
            );
            base.insert("content-length", body.len().to_string().parse().unwrap());
            return (StatusCode::PARTIAL_CONTENT, base, Body::from_stream(stream)).into_response();
        }
    }
    base.insert(
        "content-range",
        format!("bytes {from}-{to}/{len}").parse().unwrap(),
    );
    if let Some(cut) = m.cut_first_range.lock().unwrap().take() {
        // Part of an answer (sent), then the connection breaks.
        let part: Vec<Result<bytes::Bytes, std::io::Error>> = body[..cut.min(body.len())]
            .chunks(16 << 10)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        use futures_util::StreamExt as _;
        let stream = futures_util::stream::iter(part).chain(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Err(std::io::Error::other("cut"))
        }));
        base.insert("content-length", body.len().to_string().parse().unwrap());
        return (StatusCode::PARTIAL_CONTENT, base, Body::from_stream(stream)).into_response();
    }
    base.insert("content-length", body.len().to_string().parse().unwrap());
    let drip = m.drip_ms.load(Ordering::SeqCst);
    if drip > 0 {
        use futures_util::StreamExt as _;
        let parts: Vec<bytes::Bytes> = body
            .chunks(16 << 10)
            .map(bytes::Bytes::copy_from_slice)
            .collect();
        let slow = futures_util::stream::iter(parts).then(move |part| async move {
            tokio::time::sleep(Duration::from_millis(drip)).await;
            Ok::<_, std::io::Error>(part)
        });
        return (StatusCode::PARTIAL_CONTENT, base, Body::from_stream(slow)).into_response();
    }
    (StatusCode::PARTIAL_CONTENT, base, body).into_response()
}
async fn api(
    AxumState(m): AxumState<Arc<Mock>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let now = m.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    m.most_in_flight.fetch_max(now, Ordering::SeqCst);
    let delay = match m.api_delay_ms.load(Ordering::SeqCst) {
        0 => 20,
        ms => ms,
    };
    tokio::time::sleep(Duration::from_millis(delay)).await;
    let forced = m.forced.lock().unwrap().pop_front();
    if let Some((status, retry_after)) = forced {
        m.forced_given.fetch_add(1, Ordering::SeqCst);
        m.in_flight.fetch_sub(1, Ordering::SeqCst);
        let mut h = HeaderMap::new();
        if let Some(r) = retry_after {
            h.insert("retry-after", r.parse().unwrap());
        }
        return match status {
            0 => (
                h,
                axum::Json(
                    json!({"error": {"code": "maxlag", "info": "Waiting for a database server"}}),
                ),
            )
                .into_response(),
            s => (StatusCode::from_u16(s).unwrap(), h).into_response(),
        };
    }
    let answer = answer(&m, &q);
    m.in_flight.fetch_sub(1, Ordering::SeqCst);
    answer
}
fn answer(m: &Mock, q: &HashMap<String, String>) -> Response {
    assert_eq!(q.get("format").map(String::as_str), Some("json"));
    assert_eq!(q.get("maxlag").map(String::as_str), Some("5"));
    let action = q.get("action").cloned().unwrap_or_default();
    let what = q
        .get("props")
        .or_else(|| q.get("list"))
        .cloned()
        .unwrap_or_default();
    m.api.lock().unwrap().push(format!("{action} {what}"));
    match (action.as_str(), what.as_str()) {
        ("wbgetentities", "labels") => {
            if m.labels_fail.load(Ordering::SeqCst) {
                return StatusCode::BAD_REQUEST.into_response();
            }
            let ids: Vec<&str> = q["ids"].split('|').collect();
            assert!(ids.len() <= 50, "{}", ids.len());
            let labels = m.labels.lock().unwrap();
            let mut entities = serde_json::Map::new();
            for id in ids {
                entities.insert(
                    id.into(),
                    match labels.get(id) {
                        Some(l) => {
                            json!({"id": id, "labels": {"en": {"language": "en", "value": l}}})
                        }
                        None => json!({"id": id, "missing": ""}),
                    },
                );
            }
            axum::Json(json!({ "entities": entities })).into_response()
        }
        ("wbgetentities", _) => {
            let ids: Vec<&str> = q["ids"].split('|').collect();
            assert!(ids.len() <= 50, "{}", ids.len());
            {
                let mut fail = m.fail_entity_once.lock().unwrap();
                if fail.as_deref().is_some_and(|f| ids.contains(&f)) {
                    *fail = None;
                    return StatusCode::BAD_REQUEST.into_response();
                }
            }
            // A full read (with claims) or only the sitelinks; each answered with what it asks.
            let full = what.split('|').any(|p| p == "claims");
            if full { &m.asked } else { &m.asked_sitelinks }
                .lock()
                .unwrap()
                .extend(ids.iter().map(|i| i.to_string()));
            let omit = m.omit_entity.lock().unwrap().clone();
            let served = m.entities.lock().unwrap();
            let mut entities = serde_json::Map::new();
            for id in ids {
                if omit.as_deref() == Some(id) {
                    continue;
                }
                // An item asked by its own id is found also when the mock serves it only under
                // an id that now redirects to it.
                let by_own_id = || served.values().find(|e| e["id"] == id);
                match served.get(id).or_else(by_own_id) {
                    Some(Value::Null) | None => {
                        entities.insert(id.into(), json!({"id": id, "missing": ""}));
                    }
                    Some(e) => {
                        let own = e["id"].as_str().unwrap().to_owned();
                        let mut e = e.clone();
                        if own != id {
                            e["redirects"] = json!({"from": id, "to": own});
                        }
                        if !full {
                            for field in ["labels", "descriptions", "aliases", "claims"] {
                                e.as_object_mut().unwrap().remove(field);
                            }
                        }
                        entities.insert(own, e);
                    }
                }
            }
            axum::Json(json!({ "entities": entities })).into_response()
        }
        ("query", "recentchanges") => {
            if m.changes_without_list.load(Ordering::SeqCst) {
                return axum::Json(json!({"batchcomplete": true, "warnings": {"main": {"warnings": "Unrecognized parameter."}}})).into_response();
            }
            let start = q["rcstart"].clone();
            let end = q["rcend"].clone();
            let all: Vec<Value> = m
                .changes
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, at)| *at >= start && *at <= end)
                .map(
                    |(title, at)| json!({"type": "edit", "ns": 0, "title": title, "timestamp": at}),
                )
                .collect();
            // Two a page, so the continuation is used.
            let from: usize = q.get("rccontinue").map_or(0, |c| c.parse().unwrap());
            let page: Vec<Value> = all.iter().skip(from).take(2).cloned().collect();
            let mut answer = json!({"batchcomplete": true, "query": {"recentchanges": page}});
            if from + 2 < all.len() {
                answer["continue"] =
                    json!({"rccontinue": (from + 2).to_string(), "continue": "-||"});
            }
            axum::Json(answer).into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn serve(mock: Arc<Mock>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    serve_on(mock, listener)
}
/// A server whose port no other test of this process gets afterwards (its socket stays open):
/// for a test that leaves a host hold (`Retry-After` over five minutes), kept by host and port.
fn serve_held(mock: Arc<Mock>) -> String {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    std::mem::forget(socket.try_clone().unwrap());
    serve_on(mock, tokio::net::TcpListener::from_std(socket).unwrap())
}
fn serve_on(mock: Arc<Mock>, listener: tokio::net::TcpListener) -> String {
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/dumps/", get(index))
        .route(
            "/elsewhere/",
            get(|AxumState(m): AxumState<Arc<Mock>>| async move {
                m.redirect_target_hits.fetch_add(1, Ordering::SeqCst);
                "redirected"
            }),
        )
        .route("/dumps/{date}/{file}", get(dated))
        .route("/w/api.php", get(api))
        .with_state(mock);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}
fn mock() -> Arc<Mock> {
    let dump = fixture();
    let sha1 = sha1_hex(&dump);
    let labels = [
        ("Q471839", "science fiction film"),
        ("Q188473", "action film"),
        ("Q2484376", "thriller film"),
        ("Q11424", "film"),
        ("Q482994", "album"),
    ]
    .into_iter()
    .map(|(q, l)| (q.to_owned(), l.to_owned()))
    .collect();
    Arc::new(Mock {
        dump: Mutex::new(dump),
        dates: Mutex::new(vec!["20261116".into(), DATE.into(), "20261125".into()]),
        sha1: Mutex::new(Some(sha1)),
        labels: Mutex::new(labels),
        ..Mock::default()
    })
}

/// A test's work directory: its own, readable by its owner only, removed when dropped.
struct Dir(std::path::PathBuf, #[allow(dead_code)] tempfile::TempDir);
fn context(pool: &PgPool, limit: Option<u64>) -> (Context, Dir) {
    let temp = tempfile::Builder::new()
        .prefix("pinakes-wikidata-test-")
        .tempdir()
        .unwrap();
    let dir = temp.path().to_owned();
    let mut config = Config::default();
    config.catalogues.dir = dir.to_string_lossy().into_owned();
    config.catalogues.wikidata.limit = limit;
    config.http.contact = "ops@example.org".into();
    config.egress.contact = "ops@example.org".into();
    let mut ctx = Context::new(pool.clone(), Arc::new(config));
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    ctx.pace = Pace {
        retry: Duration::from_millis(20),
        disk: Duration::from_millis(20),
        turn: Duration::from_millis(20),
        lock_check: Duration::from_millis(500),
    };
    (ctx, Dir(dir, temp))
}
fn importer(base: &str) -> Arc<Wikidata> {
    let mut w = Wikidata::new();
    w.endpoints = Endpoints {
        dumps: format!("{base}/dumps/"),
        api: format!("{base}/w/api.php"),
    };
    w.threads = (3, 2);
    Arc::new(w)
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    source_id: String,
    uri: String,
    #[sqlx(rename = "type")]
    r#type: String,
    name: String,
    labels: Vec<String>,
    keys: Vec<String>,
    related_uris: Vec<String>,
    fame: f32,
    lat: Option<f64>,
    updated_at: chrono::DateTime<chrono::Utc>,
}
async fn rows(pool: &PgPool) -> HashMap<String, Row> {
    let rows: Vec<Row> = sqlx::query_as("SELECT source_id,uri,type,name,labels,keys,related_uris,fame,ST_Y(point) AS lat,updated_at FROM catalogue_card WHERE catalogue='wikidata'")
        .fetch_all(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| (r.source_id.clone(), r)).collect()
}
fn url_key(uri: &str) -> String {
    pinakes_core::catalogue::link_key(uri).unwrap()
}

async fn import(_pool: &PgPool, ctx: &Context, base: &str) -> anyhow::Result<Once> {
    let mut w = Wikidata::for_config(&ctx.config);
    let base_importer = importer(base);
    w.endpoints = base_importer.endpoints.clone();
    w.threads = base_importer.threads;
    once(Arc::new(w), ctx, Run::Import).await
}

/// A first import: the newest dump with its file (the newest directory lists none), downloaded
/// on two connections and checked, read into one card per item with an article.
#[sqlx::test(migrations = "../../migrations")]
async fn the_first_import_downloads_checks_and_reads_the_dump(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, dir) = context(&pool, None);
    let done = import(&pool, &ctx, &base).await.unwrap();
    let Once::Finished(state) = done else {
        panic!("{done:?}")
    };
    // Two connections, from the start and from the middle.
    let mut ranges = m.ranges.lock().unwrap().clone();
    ranges.sort();
    assert_eq!(ranges, [0, m.dump.lock().unwrap().len() as u64 / 2]);
    let rows = rows(&pool).await;
    assert_eq!(state.cards as usize, rows.len());
    assert_eq!(state.upstream_version.as_deref(), Some(DATE));
    assert_eq!(
        state.current_as_of.unwrap().to_rfc3339(),
        "2026-11-23T00:00:00+00:00"
    );
    assert_eq!(
        state.position,
        Some(json!({"time": "2026-11-23T00:00:00Z", "import": true, "limit": null}))
    );
    // No article, a list, a disambiguation page, a category, a property: no card.
    for id in ["Q5063", "Q2041", "Q3263", "Q4829", "P136"] {
        assert!(!rows.contains_key(id), "{id}");
    }
    for id in [
        "Q11366",
        "Q202996",
        "Q44190",
        "Q42",
        "Q83495",
        "Q72",
        "Q312",
        "Q140",
        "Q171477",
        "Q23013817",
        "Q208460",
        "Q11399",
        "Q24925",
        "Q2711",
    ] {
        assert!(rows.contains_key(id), "{id}");
    }
    let adams = &rows["Q42"];
    assert_eq!(adams.uri, "https://en.wikipedia.org/wiki/Douglas_Adams");
    assert_eq!(adams.r#type, "Person");
    assert_eq!(adams.name, "Douglas Adams");
    for key in [
        "wikidata:Q42".to_owned(),
        "mb:artist:e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4".to_owned(),
        url_key("https://en.wikipedia.org/wiki/Douglas_Adams"),
        url_key("https://de.wikipedia.org/wiki/Douglas_Adams"),
        url_key("https://www.imdb.com/name/nm0010930/"),
    ] {
        assert!(adams.keys.contains(&key), "{key} {:?}", adams.keys);
    }
    assert_eq!(adams.related_uris, ["https://douglasadams.com/"]);
    assert!(adams.fame > 0.75 && adams.fame <= 1.0, "{}", adams.fame);
    // Science fiction (Q24925) comes after Douglas Adams in the dump: his card is written again
    // with it at the end.
    assert!(
        adams.labels.contains(&"science-fiction".to_owned()),
        "{:?}",
        adams.labels
    );
    // Alternative rock (Q11366) comes before OK Computer: labelled as it is read.
    let album = &rows["Q202996"];
    assert_eq!(album.r#type, "MusicAlbum");
    assert!(album
        .keys
        .contains(&"mb:release-group:b1392450-e666-3926-a536-22c65f834433".to_owned()));
    assert!(
        album.labels.contains(&"alternative-rock".to_owned()),
        "{:?}",
        album.labels
    );
    assert!(
        album.labels.contains(&"album".to_owned()),
        "{:?}",
        album.labels
    );
    let film = &rows["Q83495"];
    assert_eq!(film.r#type, "Movie");
    for key in [
        "imdb:tt0133093".to_owned(),
        url_key("https://www.imdb.com/title/tt0133093/"),
        url_key("https://www.themoviedb.org/movie/603"),
    ] {
        assert!(film.keys.contains(&key), "{key} {:?}", film.keys);
    }
    // Genres only the API has the labels of.
    for label in ["science-fiction-film", "action-film", "thriller-film"] {
        assert!(
            film.labels.contains(&label.to_owned()),
            "{label} {:?}",
            film.labels
        );
    }
    let zurich = &rows["Q72"];
    assert!(zurich.lat.is_some_and(|lat| (47.0..48.0).contains(&lat)));
    for key in [
        "osm:relation/1682248".to_owned(),
        url_key("https://www.google.com/maps?cid=16604364946265857027"),
    ] {
        assert!(zurich.keys.contains(&key), "{key} {:?}", zurich.keys);
    }
    let game = &rows["Q23013817"];
    assert_eq!(game.r#type, "VideoGame");
    assert!(game
        .keys
        .contains(&url_key("https://store.steampowered.com/app/413150/")));
    assert_eq!(rows["Q140"].r#type, "Taxon");
    assert_eq!(rows["Q11366"].r#type, "Thing");
    // Every label is a valid tag label; every key is in the stored form.
    for row in rows.values() {
        for label in &row.labels {
            assert_eq!(
                pinakes_core::catalogue::label(label).as_deref(),
                Some(label.as_str())
            );
        }
        for key in &row.keys {
            assert_eq!(
                pinakes_core::catalogue::stored_key(key).as_deref(),
                Some(key.as_str())
            );
        }
    }
    // The dump is gone from the work directory; the labels stay for the updates.
    let files: Vec<String> = std::fs::read_dir(dir.0.join("wikidata"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, ["labels.tsv"]);
    // One API request at a time.
    assert_eq!(m.most_in_flight.load(Ordering::SeqCst), 1);
}

/// A dump whose SHA-1 is not the listed one fails the import (`checksum`), before any card.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_with_another_checksum_is_not_read(pool: PgPool) {
    let m = mock();
    *m.sha1.lock().unwrap() = Some("0".repeat(40));
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    assert!(import(&pool, &ctx, &base).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.last_error.as_deref(), Some("checksum (127.0.0.1)"));
    assert_eq!(state.cards, 0);
}

/// A connection broken mid-file is taken up again where it stopped; a label lookup that fails
/// stops the import at its last step, and the next run resumes there without reading the dump
/// again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_cut_off_import_resumes_where_it_stopped(pool: PgPool) {
    let m = mock();
    *m.cut_first_range.lock().unwrap() = Some(100_000);
    m.labels_fail.store(true, Ordering::SeqCst);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    assert!(import(&pool, &ctx, &base).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.run.as_deref(), Some("import"));
    assert_eq!(state.cursor.as_ref().unwrap()["step"]["step"], "labels");
    assert_eq!(state.last_error.as_deref(), Some("api (127.0.0.1)"));
    let written = state.cards;
    assert!(written > 20, "{written}");
    let ranges = m.ranges.lock().unwrap().clone();
    // Two halves and the broken one again from where it stopped (100,000 bytes in).
    assert_eq!(ranges.len(), 3, "{ranges:?}");
    let half = m.dump.lock().unwrap().len() as u64 / 2;
    assert!(
        ranges[2] == 100_000 || ranges[2] == half + 100_000,
        "{ranges:?}"
    );
    let ranges = ranges.len();
    m.labels_fail.store(false, Ordering::SeqCst);
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards, written);
    assert_eq!(
        m.ranges.lock().unwrap().len(),
        ranges,
        "the dump is not read again"
    );
    let rows = rows(&pool).await;
    assert!(rows["Q83495"].labels.contains(&"action-film".to_owned()));
    assert!(rows["Q42"].labels.contains(&"science-fiction".to_owned()));
}

/// With a limit, the dump is read as a stream (nothing downloaded) up to the limit.
#[sqlx::test(migrations = "../../migrations")]
async fn a_limit_streams_the_dump_and_stops_at_it(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, dir) = context(&pool, Some(5));
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards, 5);
    assert_eq!(*m.ranges.lock().unwrap(), [0]);
    assert!(std::fs::read_dir(dir.0.join("wikidata"))
        .unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".bz2")));
}

/// A minimal item with an English article.
fn item(id: &str, name: &str) -> Value {
    json!({"type": "item", "id": id, "labels": {"en": {"language": "en", "value": name}},
        "descriptions": {}, "aliases": {}, "claims": {},
        "sitelinks": {"enwiki": {"site": "enwiki", "title": name, "badges": []}}})
}

/// An update reads the changes since the position, fetches the changed items one request at a
/// time and writes only what changed: an edit, a deletion, a merge into another item, an item
/// that lost its article; untouched cards stay as they are.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_applies_the_recent_changes(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let before = rows(&pool).await;
    // Douglas Adams renamed; the lion deleted; Q2711 merged into Q30 (new here); Q4378 left
    // without an article; Q4940 edited where it shows nothing; Q999 a new item with no
    // article; a change before the position is not read.
    let mut adams: Value = {
        let text = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/wikidata/dump-sample.json.bz2"
        ))
        .unwrap();
        let mut d = bzip2::read::BzDecoder::new(&text[..]);
        let mut s = String::new();
        std::io::Read::read_to_string(&mut d, &mut s).unwrap();
        let line = s
            .lines()
            .find(|l| l.starts_with("{\"type\":\"item\",\"id\":\"Q42\""))
            .unwrap();
        serde_json::from_str(line.trim_end_matches(',')).unwrap()
    };
    adams["labels"]["en"]["value"] = json!("Douglas Noël Adams");
    let mut q4378 = item("Q4378", "x");
    q4378["sitelinks"] = json!({});
    {
        let mut e = m.entities.lock().unwrap();
        e.insert("Q42".into(), adams);
        e.insert("Q140".into(), Value::Null);
        e.insert("Q2711".into(), item("Q30", "United States"));
        e.insert("Q4378".into(), q4378);
        let mut q999 = item("Q999", "nothing");
        q999["sitelinks"] = json!({});
        e.insert("Q999".into(), q999);
    }
    let now = chrono::Utc::now();
    let at = |mins: i64| {
        (now - chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    *m.changes.lock().unwrap() = vec![
        ("Q83495".into(), at(200)),
        ("Q42".into(), at(50)),
        ("Q140".into(), at(40)),
        ("Q42".into(), at(35)),
        ("Q2711".into(), at(30)),
        ("Q4378".into(), at(20)),
        ("Q999".into(), at(10)),
    ];
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind(at(60))
        .execute(&pool)
        .await
        .unwrap();
    m.api.lock().unwrap().clear();
    m.asked.lock().unwrap().clear();
    // Answers slower than the gap between requests: two at once would overlap here.
    m.api_delay_ms.store(400, Ordering::SeqCst);
    let Once::Finished(state) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    let after = rows(&pool).await;
    assert_eq!(after["Q42"].name, "Douglas Noël Adams");
    assert!(!after.contains_key("Q140"));
    assert!(!after.contains_key("Q2711"));
    assert!(!after.contains_key("Q4378"));
    assert!(!after.contains_key("Q999"));
    assert_eq!(
        after["Q30"].uri,
        "https://en.wikipedia.org/wiki/United_States"
    );
    assert_eq!(state.cards as usize, after.len());
    assert_eq!(after.len(), before.len() - 3 + 1);
    // Only the changed rows moved.
    for (id, row) in &after {
        if id != "Q42" && id != "Q30" {
            assert_eq!(row.updated_at, before[id].updated_at, "{id}");
        }
    }
    // The Matrix changed before the position: not fetched; Douglas Adams, changed twice, once.
    let asked = m.asked.lock().unwrap().clone();
    assert!(!asked.contains(&"Q83495".to_owned()), "{asked:?}");
    assert_eq!(asked.iter().filter(|q| *q == "Q42").count(), 1, "{asked:?}");
    // Items without an article (Q999; Q4378, which lost its own) or deleted (Q140) are known
    // from their sitelinks: never read in full.
    let sitelinks = m.asked_sitelinks.lock().unwrap().clone();
    for q in ["Q999", "Q4378", "Q140"] {
        assert!(sitelinks.contains(&q.to_owned()), "{q} {sitelinks:?}");
        assert!(!asked.contains(&q.to_owned()), "{q} {asked:?}");
    }
    let api = m.api.lock().unwrap().clone();
    assert!(
        api.iter()
            .filter(|a| a.starts_with("query recentchanges"))
            .count()
            >= 3,
        "{api:?}"
    );
    assert_eq!(m.most_in_flight.load(Ordering::SeqCst), 1);
    let end = state.position.unwrap()["time"].as_str().unwrap().to_owned();
    assert!(end > at(2) && end <= at(0), "{end}");
    assert_eq!(
        state
            .current_as_of
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        end
    );
    // A second update right after finds nothing new and writes nothing.
    let unchanged = rows(&pool).await;
    once(importer(&base), &ctx, Run::Update).await.unwrap();
    for (id, row) in rows(&pool).await {
        assert_eq!(row.updated_at, unchanged[&id].updated_at, "{id}");
    }
}

/// With a limit, an update changes the cards it holds but adds new items only below the limit.
#[sqlx::test(migrations = "../../migrations")]
async fn a_limited_catalogue_does_not_grow_past_its_limit(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(5));
    import(&pool, &ctx, &base).await.unwrap();
    let held: Vec<String> = rows(&pool).await.into_keys().collect();
    let first = held.iter().min().unwrap().clone();
    {
        let mut e = m.entities.lock().unwrap();
        e.insert("Q30".into(), item("Q30", "United States"));
        e.insert(first.clone(), item(&first, "Renamed"));
    }
    let at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    *m.changes.lock().unwrap() = vec![("Q30".into(), at.clone()), (first.clone(), at)];
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind((chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&pool)
        .await
        .unwrap();
    m.asked.lock().unwrap().clear();
    once(importer(&base), &ctx, Run::Update).await.unwrap();
    let after = rows(&pool).await;
    assert_eq!(after.len(), 5);
    assert!(!after.contains_key("Q30"));
    // Full: a new item is not even asked for.
    assert!(!m.asked.lock().unwrap().contains(&"Q30".to_owned()));
    assert!(!m
        .asked_sitelinks
        .lock()
        .unwrap()
        .contains(&"Q30".to_owned()));
    assert_eq!(after[&first].name, "Renamed");
}

/// At its limit, a held item merged into an item the catalogue doesn't hold is replaced by it: the
/// card of the item it now is joins as the merged one leaves, and the catalogue stays at its limit.
#[sqlx::test(migrations = "../../migrations")]
async fn a_held_item_merged_at_the_limit_is_replaced_by_the_item_it_is_now(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(5));
    import(&pool, &ctx, &base).await.unwrap();
    let merged = rows(&pool).await.into_keys().min().unwrap();
    assert!(!rows(&pool).await.contains_key("Q30"));
    m.entities
        .lock()
        .unwrap()
        .insert(merged.clone(), item("Q30", "United States"));
    let at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    m.changes.lock().unwrap().push((merged.clone(), at));
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind((chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&pool)
        .await
        .unwrap();
    let Once::Finished(state) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    let after = rows(&pool).await;
    assert!(!after.contains_key(&merged));
    assert_eq!(after["Q30"].name, "United States");
    assert_eq!((after.len(), state.cards), (5, 5));
}

/// A limited catalogue just under its limit takes new items only up to it: of two new items on
/// one page, one joins.
#[sqlx::test(migrations = "../../migrations")]
async fn a_limited_catalogue_under_its_limit_fills_up_to_it(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(5));
    import(&pool, &ctx, &base).await.unwrap();
    let first = rows(&pool).await.into_keys().min().unwrap();
    store::remove(&pool, Catalogue::Wikidata, &[first], None)
        .await
        .unwrap();
    assert_eq!(rows(&pool).await.len(), 4);
    let at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for (id, name) in [("Q900011", "One new item"), ("Q900012", "Another new item")] {
        m.entities.lock().unwrap().insert(id.into(), item(id, name));
        m.changes.lock().unwrap().push((id.into(), at.clone()));
    }
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind((chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&pool)
        .await
        .unwrap();
    let Once::Finished(state) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    let after = rows(&pool).await;
    assert_eq!((after.len(), state.cards), (5, 5));
    let joined = ["Q900011", "Q900012"]
        .iter()
        .filter(|q| after.contains_key(**q))
        .count();
    assert_eq!(joined, 1);
}

/// A position more than five days old is not caught up change by change: the update waits for
/// a dump newer than the imported one, then asks for a full import.
#[sqlx::test(migrations = "../../migrations")]
async fn a_long_gap_waits_for_the_next_dump(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(3));
    import(&pool, &ctx, &base).await.unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::days(6))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind(&old)
        .execute(&pool)
        .await
        .unwrap();
    m.api.lock().unwrap().clear();
    let Once::Finished(state) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.position.unwrap()["time"], old.as_str());
    assert!(m.api.lock().unwrap().is_empty(), "no change is read");
    let today = chrono::Utc::now().format("%Y%m%d").to_string();
    m.dates.lock().unwrap().push(today.clone());
    // The update asks for a full import of the newer dump, which the run then makes.
    let Once::Finished(state) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.upstream_version.as_deref(), Some(today.as_str()));
    assert!(m.ranges.lock().unwrap().contains(&0));
}

/// Past the gap, a dump newer than the imported one but dated before the position is not
/// imported (its cards would be older than those the updates made): the update waits.
#[sqlx::test(migrations = "../../migrations")]
async fn a_long_gap_imports_no_dump_older_than_the_position(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(3));
    import(&pool, &ctx, &base).await.unwrap();
    let days_ago = |n| chrono::Utc::now() - chrono::Duration::days(n);
    let old = days_ago(6).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sqlx::query("UPDATE catalogue_state SET upstream_version='20200101',position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind(&old)
        .execute(&pool)
        .await
        .unwrap();
    *m.dates.lock().unwrap() = vec![days_ago(7).format("%Y%m%d").to_string()];
    m.ranges.lock().unwrap().clear();
    let Once::Finished(state) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.position.unwrap()["time"], old.as_str());
    assert_eq!(state.upstream_version.as_deref(), Some("20200101"));
    assert!(m.ranges.lock().unwrap().is_empty());
}

/// An update under way whose position is past the gap (it kept failing and was resumed run after
/// run) is not resumed: it is dropped, and the gap rule waits for the next weekly dump.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_stuck_past_the_gap_is_dropped(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(3));
    import(&pool, &ctx, &base).await.unwrap();
    let old = chrono::Utc::now() - chrono::Duration::days(6);
    sqlx::query("UPDATE catalogue_state SET run='update',position=jsonb_build_object('time',$1::text),cursor=jsonb_build_object('end',$2::text) WHERE catalogue='wikidata'")
        .bind(old.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .bind((old + chrono::Duration::hours(6)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&pool)
        .await
        .unwrap();
    m.api.lock().unwrap().clear();
    let Once::Finished(state) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    assert!(m.api.lock().unwrap().is_empty(), "no change is read");
    assert_eq!((state.run, state.cursor), (None, None));
}

/// Without the operator's contact nothing is asked of Wikimedia: the run waits for one, as every
/// importer's does, and `catalogue status` says why.
#[sqlx::test(migrations = "../../migrations")]
async fn no_contact_no_request(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let mut config = (*ctx.config).clone();
    config.http.contact = String::new();
    config.egress.contact = String::new();
    let ctx = Context {
        config: Arc::new(config),
        ..ctx
    };
    let next = run::step(importer(&base).as_ref(), &ctx).await.unwrap();
    assert!(matches!(next, run::Next::Wait(_)), "{next:?}");
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.waiting.as_deref(), Some(run::NEEDS_CONTACT));
    assert_eq!((state.last_error, state.error_count), (None, 0));
    assert!(m.ranges.lock().unwrap().is_empty() && m.api.lock().unwrap().is_empty());
}

/// The Q-ids of the fixture's entities that make a card, read without the importer.
fn kept_ids() -> std::collections::BTreeSet<String> {
    let packed = fixture();
    let mut text = Vec::new();
    std::io::Read::read_to_end(&mut bzip2::read::BzDecoder::new(&packed[..]), &mut text).unwrap();
    let labels = pinakes_core::catalogue::wikidata::entity::Labels::new();
    text.split(|b| *b == b'\n')
        .filter_map(|l| pinakes_core::catalogue::wikidata::entity::parse_line(l, &labels).card)
        .map(|c| c.source_id)
        .collect()
}
fn lines_of(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A download cut off between runs resumes each connection where its last round ended; it never
/// reads the file again from the start.
#[sqlx::test(migrations = "../../migrations")]
async fn a_download_cut_off_between_runs_resumes(pool: PgPool) {
    let m = mock();
    let size = m.dump.lock().unwrap().len() as u64;
    *m.fail_ranges_from.lock().unwrap() = Some(150_000);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.download_round = 100_000;
    let w = Arc::new(w);
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state.cursor.as_ref().unwrap()["step"]["step"],
        "download",
        "{:?}",
        state.cursor
    );
    let parts = state.cursor.as_ref().unwrap()["step"]["parts"].clone();
    assert_eq!(parts[0], 100_000);
    m.ranges.lock().unwrap().clear();
    *m.fail_ranges_from.lock().unwrap() = None;
    let Once::Finished(state) = once(w, &ctx, Run::Import).await.unwrap() else {
        panic!()
    };
    let ranges = m.ranges.lock().unwrap().clone();
    assert!(!ranges.contains(&0), "{ranges:?}");
    assert!(ranges.contains(&100_000), "{ranges:?}");
    assert!(ranges.iter().all(|r| *r < size));
    assert_eq!(rows(&pool).await.len() as i64, state.cards);
}

/// An import started without a limit keeps reading everything when it resumes after a limit was
/// configured meanwhile (the limit stopping it would sweep the cards it had not read again).
#[sqlx::test(migrations = "../../migrations")]
async fn a_resumed_import_keeps_the_limit_it_started_with(pool: PgPool) {
    let m = mock();
    *m.fail_ranges_from.lock().unwrap() = Some(150_000);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.download_round = 100_000;
    let w = Arc::new(w);
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    *m.fail_ranges_from.lock().unwrap() = None;
    // The same work directory, with a limit configured.
    let mut config = (*ctx.config).clone();
    config.catalogues.wikidata.limit = Some(3);
    let limited = Context {
        pool: pool.clone(),
        config: Arc::new(config),
        free_disk: ctx.free_disk.clone(),
        pace: ctx.pace,
    };
    let Once::Finished(state) = once(w, &limited, Run::Import).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards as usize, kept_ids().len());
}

/// A read cut off mid-dump (a stream that breaks for good) resumes at its last written batch in
/// the next run: the same cards as a read in one go, and no label line twice.
#[sqlx::test(migrations = "../../migrations")]
async fn a_read_cut_off_between_runs_resumes_at_its_last_batch(pool: PgPool) {
    let m = mock();
    *m.cut_stream.lock().unwrap() = Some(300_000);
    let base = serve(m.clone()).await;
    let (ctx, dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    // More than the dump holds: the only write before the break is the one the failure makes
    // of the cards parsed so far.
    w.write_cards = 10_000;
    w.from = Some(pinakes_core::catalogue::wikidata::From::Url(format!(
        "{base}/dumps/{DATE}/wikidata-{DATE}-all.json.bz2"
    )));
    let w = Arc::new(w);
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    let cursor = state.cursor.clone().unwrap_or_else(|| panic!("{state:?}"));
    assert_eq!(cursor["step"]["step"], "read", "{cursor}");
    let resume_byte = cursor["at"]["bit"]
        .as_u64()
        .unwrap_or_else(|| panic!("{state:?}"))
        / 8;
    assert!(resume_byte > 0 && state.cards > 0, "{cursor}");
    assert_eq!(state.last_error.as_deref(), Some("download (127.0.0.1)"));
    m.ranges.lock().unwrap().clear();
    *m.fail_ranges_from.lock().unwrap() = None;
    let Once::Finished(_) = once(w, &ctx, Run::Import).await.unwrap() else {
        panic!()
    };
    assert_eq!(*m.ranges.lock().unwrap(), [resume_byte]);
    let ids: std::collections::BTreeSet<String> = rows(&pool).await.into_keys().collect();
    assert_eq!(ids, kept_ids());
    let labels = lines_of(&dir.0.join("wikidata/labels.tsv"));
    let once: std::collections::BTreeSet<&String> = labels.iter().collect();
    assert_eq!(once.len(), labels.len(), "a label line written twice");
    let rows = rows(&pool).await;
    assert!(rows["Q42"].labels.contains(&"science-fiction".to_owned()));
}

/// A dump that fails its checksum is not read again from the cursor: its files go, and the next
/// run downloads the newest dump again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_that_fails_its_checksum_is_downloaded_again(pool: PgPool) {
    let m = mock();
    let right = m.sha1.lock().unwrap().clone();
    *m.sha1.lock().unwrap() = Some("0".repeat(40));
    let base = serve(m.clone()).await;
    let (ctx, dir) = context(&pool, None);
    assert!(import(&pool, &ctx, &base).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.last_error.as_deref(), Some("checksum (127.0.0.1)"));
    assert_eq!(state.cursor.as_ref().unwrap()["restart"]["count"], 1);
    assert!(std::fs::read_dir(dir.0.join("wikidata"))
        .unwrap()
        .next()
        .is_none());
    *m.sha1.lock().unwrap() = right;
    m.ranges.lock().unwrap().clear();
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    assert!(m.ranges.lock().unwrap().contains(&0));
    assert!(state.cards > 20);
}

/// A dump that holds no entity is no import: it is refused (`empty`), and no card is removed. One that holds far
/// fewer cards than the catalogue meets the catalogue's shrink floor: it removes nothing until the
/// operator allows it.
#[sqlx::test(migrations = "../../migrations")]
async fn an_empty_dump_removes_nothing(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let held = rows(&pool).await.len();
    let mut empty = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
    std::io::Write::write_all(&mut empty, b"[\n]\n").unwrap();
    let empty = empty.finish().unwrap();
    *m.sha1.lock().unwrap() = Some(sha1_hex(&empty));
    *m.dump.lock().unwrap() = empty;
    assert!(import(&pool, &ctx, &base).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert!(
        state.last_error.as_deref().unwrap().starts_with("empty"),
        "{:?}",
        state.last_error
    );
    assert!(state.position.unwrap()["refused"].is_string());
    assert_eq!(rows(&pool).await.len(), held);
    // A whole dump with one item, where the catalogue holds dozens, is not taken either.
    let packed = fixture();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut bzip2::read::BzDecoder::new(&packed[..]), &mut text)
        .unwrap();
    let line = text
        .lines()
        .find(|l| l.contains("\"id\":\"Q42\""))
        .unwrap()
        .trim_end_matches(',');
    let mut small = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
    std::io::Write::write_all(&mut small, format!("[\n{line}\n]\n").as_bytes()).unwrap();
    let small = small.finish().unwrap();
    *m.sha1.lock().unwrap() = Some(sha1_hex(&small));
    *m.dump.lock().unwrap() = small;
    assert!(import(&pool, &ctx, &base).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    // The catalogue's shrink floor: nothing is removed, and the next try fails the same way.
    assert_eq!(state.last_error.as_deref(), Some("shrunk"));
    assert_eq!(rows(&pool).await.len(), held);
    // The next try resumes at the import's end, and fails the same way.
    assert!(import(&pool, &ctx, &base).await.is_err());
    assert_eq!(last_error(&pool).await.as_deref(), Some("shrunk"));
    assert_eq!(rows(&pool).await.len(), held);
    // Once the operator allows it (`catalogue allow-shrink`), the next try removes the rest.
    assert_eq!(
        store::allow_shrink(&pool, Catalogue::Wikidata)
            .await
            .unwrap(),
        store::Allowance::Allowed
    );
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards, 1);
    assert_eq!(rows(&pool).await.into_keys().collect::<Vec<_>>(), ["Q42"]);
}

/// An import that starts over (here after a dump of broken lines) has a generation of its own:
/// the cards the abandoned attempt wrote that the next dump no longer holds leave at its end.
#[sqlx::test(migrations = "../../migrations")]
async fn an_import_that_starts_over_sweeps_what_the_abandoned_attempt_wrote(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let packed = fixture();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut bzip2::read::BzDecoder::new(&packed[..]), &mut text)
        .unwrap();
    let pack = |body: &str| {
        let mut out = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
        std::io::Write::write_all(&mut out, body.as_bytes()).unwrap();
        out.finish().unwrap()
    };
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with('{')).collect();
    // Every item, then lines that are no entities: the cards are written, then the dump fails.
    let mut broken = String::from("[\n");
    for line in &lines {
        broken.push_str(line);
        broken.push('\n');
    }
    for n in 0..300 {
        broken.push_str(&format!("{{\"site\": broken {n},\n"));
    }
    broken.push_str("]\n");
    let broken = pack(&broken);
    *m.sha1.lock().unwrap() = Some(sha1_hex(&broken));
    *m.dump.lock().unwrap() = broken;
    // One card a write, so the abandoned attempt has written its cards before it fails.
    let mut w = Wikidata::for_config(&ctx.config);
    w.endpoints = importer(&base).endpoints.clone();
    w.write_cards = 1;
    assert!(once(Arc::new(w), &ctx, Run::Import).await.is_err());
    assert_eq!(last_error(&pool).await.as_deref(), Some("bad_dump"));
    assert!(rows(&pool).await.contains_key("Q42"));
    // The newest dump now holds the items without Q42.
    let mut fixed = String::from("[\n");
    for line in lines.iter().filter(|l| !l.contains("\"id\":\"Q42\"")) {
        fixed.push_str(line);
        fixed.push('\n');
    }
    fixed.push_str("]\n");
    let fixed = pack(&fixed);
    *m.sha1.lock().unwrap() = Some(sha1_hex(&fixed));
    *m.dump.lock().unwrap() = fixed;
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    let after = rows(&pool).await;
    assert!(
        !after.contains_key("Q42"),
        "the abandoned attempt's card stays"
    );
    assert_eq!(after.len() as i64, state.cards);
    assert_eq!(after.len(), kept_ids().len() - 1);
}

/// A re-import whose dump holds 40% of the items the catalogue holds, most of them waiting for
/// labels read after them, meets the shrink floor: the label rewrite writes those cards again, but
/// they count once toward what the import read, so the import fails (`shrunk`) and removes nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn label_rewrites_count_once_for_the_shrink_floor(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let held = rows(&pool).await.len();
    let kept: Vec<String> = kept_ids().into_iter().collect();
    let part: std::collections::BTreeSet<String> =
        kept.iter().take(kept.len() * 2 / 5).cloned().collect();
    let packed = fixture();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut bzip2::read::BzDecoder::new(&packed[..]), &mut text)
        .unwrap();
    let mut dump = String::from("[\n");
    for line in text.lines().filter(|l| l.starts_with('{')) {
        let entity: Value = serde_json::from_str(line.trim_end_matches(',')).unwrap();
        let id = entity["id"].as_str().unwrap();
        // The items not kept (label items among them) stay; 40% of the cards' items.
        if part.contains(id) || !kept.iter().any(|k| k == id) {
            dump.push_str(line);
            dump.push('\n');
        }
    }
    dump.push_str("]\n");
    let mut small = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
    std::io::Write::write_all(&mut small, dump.as_bytes()).unwrap();
    let small = small.finish().unwrap();
    *m.sha1.lock().unwrap() = Some(sha1_hex(&small));
    *m.dump.lock().unwrap() = small;
    assert!(import(&pool, &ctx, &base).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.last_error.as_deref(), Some("shrunk"));
    // What the import read is the items it read, each once.
    assert_eq!(state.generation_cards as usize, part.len());
    assert_eq!(rows(&pool).await.len(), held);
}

/// An update cut off after a page resumes at the next page: every change applied, none fetched
/// twice.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_cut_off_resumes_at_its_next_page(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let names = ["One", "Two", "Three", "Four"];
    let ids = ["Q72", "Q140", "Q312", "Q11399"];
    {
        let mut e = m.entities.lock().unwrap();
        for (id, name) in ids.iter().zip(names) {
            e.insert((*id).into(), item(id, name));
        }
    }
    let at = |mins: i64| {
        (chrono::Utc::now() - chrono::Duration::minutes(mins))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    *m.changes.lock().unwrap() = ids
        .iter()
        .enumerate()
        .map(|(i, id)| ((*id).to_owned(), at(30 - i as i64)))
        .collect();
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind(at(60))
        .execute(&pool)
        .await
        .unwrap();
    // The second page (Q312, Q11399) fails once.
    *m.fail_entity_once.lock().unwrap() = Some("Q312".into());
    assert!(once(importer(&base), &ctx, Run::Update).await.is_err());
    let after = rows(&pool).await;
    assert_eq!(after["Q72"].name, "One");
    assert_ne!(after["Q312"].name, "Three");
    let Once::Finished(_) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    let after = rows(&pool).await;
    for (id, name) in ids.iter().zip(names) {
        assert_eq!(after[*id].name, name, "{id}");
    }
    let asked = m.asked.lock().unwrap().clone();
    assert_eq!(asked.iter().filter(|q| *q == "Q72").count(), 1, "{asked:?}");
}

/// The cards hold the links people share for their items: another edition's article and a
/// service page find the card through the catalogue lookup a shared link makes.
#[sqlx::test(migrations = "../../migrations")]
async fn shared_links_find_their_wikidata_card(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    for (link, qid) in [
        ("https://de.wikipedia.org/wiki/Douglas_Adams", "Q42"),
        ("https://en.m.wikipedia.org/wiki/Douglas_Adams", "Q42"),
        ("https://www.imdb.com/title/tt0133093/", "Q83495"),
        ("https://www.themoviedb.org/movie/603", "Q83495"),
        ("https://www.wikidata.org/wiki/Q72", "Q72"),
        ("https://store.steampowered.com/app/413150/", "Q23013817"),
    ] {
        // As a shared link is looked up: by its root (the mobile host folds into the article's).
        let (_, uri, _) = pinakes_core::model::identity(link).unwrap();
        let root = pinakes_core::alias::root(&uri).map_or(uri, |a| a.root);
        let found = pinakes_core::catalogue::join::find(&pool, &["wikidata"], &root)
            .await
            .unwrap();
        assert_eq!(
            found.card.map(|c| c.source_id),
            Some(qid.to_owned()),
            "{link}"
        );
    }
}

/// An import's position days behind is caught up window by window (6 hours of changes each):
/// it stays an import's catch-up, never held by the gap rule, until it is current.
#[sqlx::test(migrations = "../../migrations")]
async fn an_import_s_catch_up_runs_window_after_window(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(3));
    import(&pool, &ctx, &base).await.unwrap();
    let start = chrono::Utc::now() - chrono::Duration::days(6);
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text,'import',true) WHERE catalogue='wikidata'")
        .bind(start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&pool)
        .await
        .unwrap();
    // While it catches up the lag alert holds back (`pinakes_catalogue_catching_up`).
    let db = &pool;
    let catching_up = move || async move {
        let m = pinakes_core::telemetry::Metrics::new().unwrap();
        m.refresh(db).await.unwrap();
        m.catalogue_catching_up
            .with_label_values(&["wikidata"])
            .get()
    };
    assert_eq!(catching_up().await, 1);
    let mut last = start;
    let mut runs = 0;
    loop {
        let done = once(importer(&base), &ctx, Run::Update).await.unwrap();
        let Once::Finished(state) = done else {
            panic!("{done:?}")
        };
        runs += 1;
        let position = state.position.unwrap();
        let time: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(position["time"].clone()).unwrap();
        assert!(time > last, "{position}");
        last = time;
        if position["import"] == false {
            break;
        }
        assert!(runs < 40, "{position}");
    }
    // Six days in windows of six hours.
    assert!((24..=26).contains(&runs), "{runs}");
    assert!(chrono::Utc::now() - last < chrono::Duration::minutes(5));
    assert_eq!(catching_up().await, 0);
}

/// A stream that breaks is taken up again at the byte it reached, in the same run: the same cards
/// as a read in one go.
#[sqlx::test(migrations = "../../migrations")]
async fn a_broken_stream_is_taken_up_where_it_broke(pool: PgPool) {
    let m = mock();
    *m.cut_stream.lock().unwrap() = Some(200_000);
    m.reconnect.store(true, Ordering::SeqCst);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.from = Some(pinakes_core::catalogue::wikidata::From::Url(format!(
        "{base}/dumps/{DATE}/wikidata-{DATE}-all.json.bz2"
    )));
    let Once::Finished(_) = once(Arc::new(w), &ctx, Run::Import).await.unwrap() else {
        panic!()
    };
    assert_eq!(*m.ranges.lock().unwrap(), [0, 200_000]);
    let ids: std::collections::BTreeSet<String> = rows(&pool).await.into_keys().collect();
    assert_eq!(ids, kept_ids());
}

/// A dump that fails three times is refused at the third: the run is cleared (the large-import
/// turn is free) and the position names the dump; the task's next import does not ask for the
/// file and records nothing, until a newer dump is out.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_that_keeps_failing_is_rejected(pool: PgPool) {
    let m = mock();
    *m.sha1.lock().unwrap() = Some("0".repeat(40));
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let state = || async {
        store::state(&pool, Catalogue::Wikidata)
            .await
            .unwrap()
            .unwrap()
    };
    for count in 1..=2 {
        assert!(import(&pool, &ctx, &base).await.is_err());
        assert_eq!(
            state().await.cursor.as_ref().unwrap()["restart"]["count"],
            count
        );
    }
    let failed = import(&pool, &ctx, &base).await.unwrap_err();
    assert!(
        failed.to_string().contains("failed 3 times (checksum)"),
        "{failed}"
    );
    let refused = state().await;
    assert_eq!(
        (refused.run.as_deref(), refused.cursor.as_ref()),
        (None, None)
    );
    assert_eq!(refused.error_count, 3);
    let position = refused.position.unwrap();
    assert!(position["refused"].is_string(), "{position}");
    m.ranges.lock().unwrap().clear();
    let mut w = Wikidata::for_config(&ctx.config);
    let base_importer = importer(&base);
    w.endpoints = base_importer.endpoints.clone();
    w.threads = base_importer.threads;
    let again = run::step(&w, &ctx).await.unwrap_err();
    assert!(again.is::<run::StillRefused>(), "{again}");
    assert_eq!(state().await.error_count, 3);
    assert!(m.ranges.lock().unwrap().is_empty());
}

/// A limit taken out of the configuration is gone: the updates and imports after it don't keep it
/// (only a command line's limit is kept in the position), and the next full import reads everything.
#[sqlx::test(migrations = "../../migrations")]
async fn a_limit_taken_out_is_gone(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(5));
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    assert_eq!(rows(&pool).await.len(), 5);
    let position = state.position.unwrap();
    assert_eq!(position["import"], true, "{position}");
    assert!(position["limit"].is_null(), "{position}");
    let (ctx, _dir2) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    assert_eq!(rows(&pool).await.len(), kept_ids().len());
}

/// A full import an update asked for keeps the limit the last import ran with, also when its first
/// try fails before it writes anything.
#[sqlx::test(migrations = "../../migrations")]
async fn an_asked_import_keeps_its_limit_through_a_failed_try(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    // The limit is the command line's (`catalogue import wikidata --limit 3`): the configuration
    // has none.
    let (ctx, _dir) = context(&pool, None);
    let mut w = Wikidata::for_config(&ctx.config);
    w.endpoints = importer(&base).endpoints.clone();
    w.threads = (3, 2);
    w.cli_limit = Some(3);
    let Once::Finished(state) = once(Arc::new(w), &ctx, Run::Import).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards, 3);
    assert_eq!(state.position.unwrap()["limit"], 3);
    sqlx::query(
        "UPDATE catalogue_state SET import_requested='a newer dump' WHERE catalogue='wikidata'",
    )
    .execute(&pool)
    .await
    .unwrap();
    m.index_fails.store(true, Ordering::SeqCst);
    assert!(import(&pool, &ctx, &base).await.is_err());
    m.index_fails.store(false, Ordering::SeqCst);
    m.ranges.lock().unwrap().clear();
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards, 3);
    // Streamed, never downloaded.
    assert_eq!(*m.ranges.lock().unwrap(), [0]);
    // A plain import after it (nothing asked, no --limit) reads everything.
    assert_eq!(state.import_requested, None);
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards as usize, kept_ids().len());
}

/// The restarts of a dump are counted across a try that failed for another reason in between.
#[sqlx::test(migrations = "../../migrations")]
async fn restarts_are_counted_across_other_failures(pool: PgPool) {
    let m = mock();
    *m.sha1.lock().unwrap() = Some("0".repeat(40));
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.download_round = 100_000;
    let w = Arc::new(w);
    let count = || async {
        let state = store::state(&pool, Catalogue::Wikidata)
            .await
            .unwrap()
            .unwrap();
        let c = state.cursor.unwrap();
        c["restart"]["count"]
            .as_u64()
            .or(c["restarts"]["count"].as_u64())
    };
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    assert_eq!(count().await, Some(1));
    // The next try breaks off in its download: the count it carries stays.
    *m.fail_ranges_from.lock().unwrap() = Some(150_000);
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    assert_eq!(count().await, Some(1));
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.cursor.unwrap()["step"]["step"], "download");
    *m.fail_ranges_from.lock().unwrap() = None;
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    assert_eq!(count().await, Some(2));
}

/// An update of one change, the position an hour back; the importer for `base` with `w`'s tweaks.
async fn one_change(pool: &PgPool, m: &Mock, id: &str, name: &str) {
    m.entities.lock().unwrap().insert(id.into(), item(id, name));
    let at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    m.changes.lock().unwrap().push((id.into(), at));
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(pool)
        .await
        .unwrap();
}
/// `pinakes_catalogue_items_skipped_total` of Wikidata for `reason` (a counter of the process,
/// which other tests may move too).
fn skipped(reason: &str) -> f64 {
    let text = pinakes_core::telemetry::Metrics::new()
        .unwrap()
        .encode()
        .unwrap();
    let series = format!(
        r#"pinakes_catalogue_items_skipped_total{{catalogue="wikidata",reason="{reason}"}} "#
    );
    text.lines()
        .find_map(|l| l.strip_prefix(series.as_str()))
        .unwrap()
        .parse()
        .unwrap()
}
async fn last_error(pool: &PgPool) -> Option<String> {
    store::state(pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap()
        .last_error
}

/// The API is asked again as it says: after `maxlag` (its `Retry-After`), a 429 (its
/// `Retry-After`) and a 503 (a back-off of a few seconds), and the update goes on.
#[sqlx::test(migrations = "../../migrations")]
async fn the_api_is_asked_again_as_it_says(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(3));
    import(&pool, &ctx, &base).await.unwrap();
    one_change(&pool, &m, "Q30", "United States").await;
    *m.forced.lock().unwrap() = [
        (0, Some("1".to_owned())),
        (429, Some("1".to_owned())),
        (503, None),
    ]
    .into();
    let started = std::time::Instant::now();
    let Once::Finished(_) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    assert_eq!(m.forced_given.load(Ordering::SeqCst), 3);
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// A `Retry-After` as a date is waited for; one asking more than five minutes fails the run
/// (`rate_limited`) at once instead of waiting or asking again early.
#[sqlx::test(migrations = "../../migrations")]
async fn a_long_retry_after_fails_the_run(pool: PgPool) {
    let m = mock();
    // The hold this test leaves is the process's: its port stays its own.
    let base = serve_held(m.clone());
    let (ctx, _dir) = context(&pool, Some(3));
    import(&pool, &ctx, &base).await.unwrap();
    one_change(&pool, &m, "Q30", "United States").await;
    let soon = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(3));
    *m.forced.lock().unwrap() = [(503, Some(soon))].into();
    let started = std::time::Instant::now();
    once(importer(&base), &ctx, Run::Update).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    // Wikidata lagging past every try is a wait it asks for too (`rate_limited`), not `api`.
    one_change(&pool, &m, "Q32", "Luxembourg").await;
    *m.forced.lock().unwrap() = std::iter::repeat_n((0, Some("1".to_owned())), 8).collect();
    assert!(once(importer(&base), &ctx, Run::Update).await.is_err());
    assert_eq!(
        last_error(&pool).await.as_deref(),
        Some("rate_limited (127.0.0.1)")
    );
    m.forced.lock().unwrap().clear();
    one_change(&pool, &m, "Q31", "Belgium").await;
    *m.forced.lock().unwrap() = [(429, Some("600".to_owned()))].into();
    let started = std::time::Instant::now();
    assert!(once(importer(&base), &ctx, Run::Update).await.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        last_error(&pool).await.as_deref(),
        Some("rate_limited (127.0.0.1)")
    );
    // The host is not asked again before the ten minutes it asked for: the next run fails at
    // once, asking nothing.
    let asked = m.api.lock().unwrap().len();
    assert!(once(importer(&base), &ctx, Run::Update).await.is_err());
    assert_eq!(
        last_error(&pool).await.as_deref(),
        Some("rate_limited (127.0.0.1)")
    );
    assert_eq!(m.api.lock().unwrap().len(), asked);
}

/// An answer that leaves out an item asked for is no answer: the run fails (`api`), and the item's
/// card stays.
#[sqlx::test(migrations = "../../migrations")]
async fn an_item_left_out_of_an_answer_is_not_removed(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(5));
    import(&pool, &ctx, &base).await.unwrap();
    let mut ids: Vec<String> = rows(&pool).await.into_keys().collect();
    ids.sort();
    let (held, other) = (ids[0].clone(), ids[1].clone());
    // Two items on one page: the one left out is asked again alone, and left out again.
    one_change(&pool, &m, &held, "Renamed").await;
    one_change(&pool, &m, &other, "Renamed too").await;
    *m.omit_entity.lock().unwrap() = Some(held.clone());
    m.asked_sitelinks.lock().unwrap().clear();
    assert!(once(importer(&base), &ctx, Run::Update).await.is_err());
    assert_eq!(last_error(&pool).await.as_deref(), Some("api (127.0.0.1)"));
    let asked = m.asked_sitelinks.lock().unwrap().clone();
    assert_eq!(asked.iter().filter(|q| **q == held).count(), 2, "{asked:?}");
    let after = rows(&pool).await;
    assert!(after.contains_key(&held));
    // Nothing of the page is written: it is read again on the next try.
    assert_ne!(after[&other].name, "Renamed too");
}

/// An answer over the cap is asked again in halves; an item whose answer alone is over the cap
/// keeps its card, and the update goes on.
#[sqlx::test(migrations = "../../migrations")]
async fn an_answer_over_the_cap_is_asked_in_halves(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let before = rows(&pool).await;
    let ids = ["Q72", "Q140", "Q312", "Q11399"];
    for (n, id) in ids.iter().enumerate() {
        one_change(&pool, &m, id, &format!("Name {n}")).await;
    }
    // Q42's answer alone is over the cap.
    let mut big = item("Q42", "Douglas Adams again");
    big["descriptions"] = json!({"en": {"language": "en", "value": "x".repeat(5000)}});
    m.entities.lock().unwrap().insert("Q42".into(), big);
    one_change(&pool, &m, "Q11366", "Alternative").await;
    let at = (chrono::Utc::now() - chrono::Duration::minutes(4))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    m.changes.lock().unwrap().push(("Q42".into(), at));
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.api_cap = 1_500;
    m.asked.lock().unwrap().clear();
    let Once::Finished(_) = once(Arc::new(w), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    let after = rows(&pool).await;
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(after[*id].name, format!("Name {n}"), "{id}");
    }
    assert_eq!(after["Q42"].name, before["Q42"].name);
    // The half of the over-cap answer without Q42 is written.
    assert_eq!(after["Q11366"].name, "Alternative");
    // Q42 left as it was is counted.
    assert!(skipped("over_cap") >= 1.0);
    // Asked in parts: more requests than one chunk of 50.
    assert!(m.asked.lock().unwrap().len() > ids.len() + 2);
}

/// A dump replaced between two runs of a download (another `ETag`) is not spliced: the run fails
/// (`dump_changed`), the files go, and the next run downloads the new file from its start.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_replaced_under_a_download_starts_over(pool: PgPool) {
    let m = mock();
    *m.fail_ranges_from.lock().unwrap() = Some(150_000);
    let base = serve(m.clone()).await;
    let (ctx, dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.download_round = 100_000;
    let w = Arc::new(w);
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    *m.fail_ranges_from.lock().unwrap() = None;
    *m.etag.lock().unwrap() = Some("\"fixture-2\"".into());
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    assert_eq!(
        last_error(&pool).await.as_deref(),
        Some("dump_changed (127.0.0.1)")
    );
    assert!(std::fs::read_dir(dir.0.join("wikidata"))
        .unwrap()
        .next()
        .is_none());
    m.ranges.lock().unwrap().clear();
    let Once::Finished(_) = once(w, &ctx, Run::Import).await.unwrap() else {
        panic!()
    };
    assert!(m.ranges.lock().unwrap().contains(&0));
}

/// A dump gone from the mirror under a download fails it (`dump_gone`) and starts it over.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_gone_under_a_download_starts_over(pool: PgPool) {
    let m = mock();
    *m.fail_ranges_from.lock().unwrap() = Some(150_000);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.download_round = 100_000;
    let w = Arc::new(w);
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    *m.fail_ranges_from.lock().unwrap() = None;
    m.dates.lock().unwrap().retain(|d| d != DATE);
    assert!(once(w, &ctx, Run::Import).await.is_err());
    assert_eq!(
        last_error(&pool).await.as_deref(),
        Some("dump_gone (127.0.0.1)")
    );
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.cursor.unwrap()["restart"]["after"], "dump_gone");
}

/// A redirect is never followed (the dumps directory answering 302 fails the run), and a dump
/// larger than the cap is not downloaded.
#[sqlx::test(migrations = "../../migrations")]
async fn redirects_and_oversized_dumps_are_refused(pool: PgPool) {
    let m = mock();
    m.index_redirects.store(true, Ordering::SeqCst);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    assert!(import(&pool, &ctx, &base).await.is_err());
    assert_eq!(
        last_error(&pool).await.as_deref(),
        Some("download (127.0.0.1)")
    );
    assert_eq!(m.redirect_target_hits.load(Ordering::SeqCst), 0);
    m.index_redirects.store(false, Ordering::SeqCst);
    *m.head_size.lock().unwrap() = Some(600_000_000_000);
    assert!(import(&pool, &ctx, &base).await.is_err());
    let shown = last_error(&pool).await.unwrap();
    assert!(
        shown.starts_with("size (dump ") && shown.contains("600000000000 bytes"),
        "{shown}"
    );
    assert!(m.ranges.lock().unwrap().is_empty());
    // Refused: the run is cleared (the large-import turn is free), and the task's next try ends
    // at once, recording nothing.
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((state.run.as_deref(), state.error_count), (None, 2));
    assert!(state.position.unwrap()["refused"].is_string());
    let mut w = Wikidata::for_config(&ctx.config);
    let base_importer = importer(&base);
    w.endpoints = base_importer.endpoints.clone();
    w.threads = base_importer.threads;
    let again = run::step(&w, &ctx).await.unwrap_err();
    assert!(again.is::<run::StillRefused>(), "{again}");
    assert_eq!(
        store::state(&pool, Catalogue::Wikidata)
            .await
            .unwrap()
            .unwrap()
            .error_count,
        2
    );
}

/// A dump of mostly broken lines is no dump: the run fails (`bad_dump`) and removes nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_of_broken_lines_removes_nothing(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let held = rows(&pool).await.len();
    let packed = fixture();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut bzip2::read::BzDecoder::new(&packed[..]), &mut text)
        .unwrap();
    let mut broken = String::from("[\n");
    for line in text.lines().filter(|l| l.starts_with('{')) {
        broken.push_str(line);
        broken.push('\n');
    }
    for n in 0..300 {
        broken.push_str(&format!("{{\"site\": broken {n},\n"));
    }
    broken.push_str("]\n");
    let mut packed = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
    std::io::Write::write_all(&mut packed, broken.as_bytes()).unwrap();
    let packed = packed.finish().unwrap();
    *m.sha1.lock().unwrap() = Some(sha1_hex(&packed));
    *m.dump.lock().unwrap() = packed;
    assert!(import(&pool, &ctx, &base).await.is_err());
    assert_eq!(last_error(&pool).await.as_deref(), Some("bad_dump"));
    assert_eq!(rows(&pool).await.len(), held);
    // The broken lines are counted as they are read.
    assert!(skipped("bad_line") >= 150.0);
}

/// A link standing where a work file goes is not followed: the run fails (`disk`) and the file
/// the link names is untouched.
#[sqlx::test(migrations = "../../migrations")]
async fn work_files_are_not_written_through_links(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, dir) = context(&pool, Some(3));
    let work = dir.0.join("wikidata");
    std::fs::create_dir_all(&work).unwrap();
    let target = dir.0.join("elsewhere.txt");
    std::fs::write(&target, b"keep me").unwrap();
    std::os::unix::fs::symlink(&target, work.join("labels.tsv")).unwrap();
    assert!(import(&pool, &ctx, &base).await.is_err());
    assert!(last_error(&pool).await.unwrap().starts_with("disk ("));
    assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
}

/// How a run of [`once`] ended.
#[derive(Debug)]
enum Once {
    Finished(Box<store::State>),
    #[allow(dead_code)]
    Never,
}
/// One run now, as `pinakesd catalogue import` makes it (`run::run_once`).
async fn once(importer: Arc<Wikidata>, ctx: &Context, run: Run) -> anyhow::Result<Once> {
    run::run_once(importer, ctx, run, |_| {})
        .await
        .map(|(state, _)| Once::Finished(Box::new(state)))
}

/// Without the operator's contact the daemon starts no Wikidata importer, and the state row and
/// `pinakes_catalogue_needs_contact` say why; with a contact it starts one.
#[sqlx::test(migrations = "../../migrations")]
async fn without_a_contact_the_importer_waits_for_one(pool: PgPool) {
    let mut config = Config::default();
    assert!(pinakes_core::catalogue::wikidata::importer(&config).is_none());
    run::note_unstarted(&pool, &config).await.unwrap();
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.waiting.as_deref(), Some(run::NEEDS_CONTACT));
    assert_eq!(state.error_count, 0);
    let m = pinakes_core::telemetry::Metrics::new().unwrap();
    m.catalogue_contact(&config);
    assert_eq!(
        m.catalogue_needs_contact
            .with_label_values(&["wikidata"])
            .get(),
        1
    );
    config.egress.contact = "ops@example.org".into();
    assert!(pinakes_core::catalogue::wikidata::importer(&config).is_some());
    run::note_unstarted(&pool, &config).await.unwrap();
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.waiting, None);
    m.catalogue_contact(&config);
    assert_eq!(
        m.catalogue_needs_contact
            .with_label_values(&["wikidata"])
            .get(),
        0
    );
}

/// A dump answered without a validator (no `ETag`, no `Last-Modified`) is neither downloaded nor
/// streamed: a file replaced between two ranges could not be told apart, so nothing is asked.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_without_a_validator_is_not_read(pool: PgPool) {
    let m = mock();
    *m.etag.lock().unwrap() = Some(String::new());
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    assert!(import(&pool, &ctx, &base).await.is_err());
    assert_eq!(
        last_error(&pool).await.as_deref(),
        Some("download (127.0.0.1)")
    );
    // The stream a limit reads, from a fresh start.
    sqlx::query("UPDATE catalogue_state SET run=NULL,cursor=NULL WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    let (limited, _dir2) = context(&pool, Some(3));
    assert!(import(&pool, &limited, &base).await.is_err());
    assert_eq!(
        last_error(&pool).await.as_deref(),
        Some("download (127.0.0.1)")
    );
    assert!(m.ranges.lock().unwrap().is_empty());
    assert!(rows(&pool).await.is_empty());
}

/// A resumed download whose cursor names another host than the dumps directory is not followed:
/// the run fails (`cursor`) without asking anything.
#[sqlx::test(migrations = "../../migrations")]
async fn a_cursor_naming_another_host_is_refused(pool: PgPool) {
    let m = mock();
    *m.fail_ranges_from.lock().unwrap() = Some(150_000);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.download_round = 100_000;
    let w = Arc::new(w);
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    *m.fail_ranges_from.lock().unwrap() = None;
    sqlx::query("UPDATE catalogue_state SET cursor=replace(cursor::text,$1,'https://elsewhere.invalid/dumps/')::jsonb WHERE catalogue='wikidata'")
        .bind(&w.endpoints.dumps)
        .execute(&pool)
        .await
        .unwrap();
    m.ranges.lock().unwrap().clear();
    assert!(once(w, &ctx, Run::Import).await.is_err());
    assert_eq!(last_error(&pool).await.as_deref(), Some("cursor"));
    assert!(m.ranges.lock().unwrap().is_empty());
}

/// An item the changes name that is gone upstream and was never in the catalogue removes nothing,
/// so the search revision (search pages' ETags) stays.
#[sqlx::test(migrations = "../../migrations")]
async fn an_item_gone_that_was_never_held_changes_nothing(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    // Q99999999 is in no dump and the API says it is missing.
    let at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    m.changes.lock().unwrap().push(("Q99999999".into(), at));
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&pool)
        .await
        .unwrap();
    let writes = || {
        sqlx::query_scalar::<_, i64>("SELECT coalesce(sum(writes),0)::bigint FROM search_write")
            .fetch_one(&pool)
    };
    let before = writes().await.unwrap();
    let cards = rows(&pool).await.len();
    let Once::Finished(state) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards as usize, cards);
    assert!(m
        .asked_sitelinks
        .lock()
        .unwrap()
        .contains(&"Q99999999".to_owned()));
    // Missing: never read in full.
    assert!(!m.asked.lock().unwrap().contains(&"Q99999999".to_owned()));
    assert_eq!(writes().await.unwrap(), before);
}

/// Two changed items merged into one are listed once in the answer (under the item they now are,
/// with one of them as `redirects.from`): the other is asked again on its own, and the update goes
/// on.
#[sqlx::test(migrations = "../../migrations")]
async fn two_items_merged_into_one_are_asked_again(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for id in ["Q900001", "Q900002"] {
        m.entities
            .lock()
            .unwrap()
            .insert(id.into(), item("Q30", "United States"));
        m.changes.lock().unwrap().push((id.into(), at.clone()));
    }
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&pool)
        .await
        .unwrap();
    let Once::Finished(_) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    let asked = m.asked_sitelinks.lock().unwrap().clone();
    assert_eq!(
        asked.iter().filter(|q| *q == "Q900001").count(),
        2,
        "{asked:?}"
    );
    assert_eq!(rows(&pool).await["Q30"].name, "United States");
}

/// A stream taken up again after a break checks what it is answered: a file replaced meanwhile
/// (`If-Range` no longer holds: `dump_changed`), or a range from another byte than the one asked
/// for (`download`), fails the run instead of splicing two files.
#[sqlx::test(migrations = "../../migrations")]
async fn a_stream_taken_up_again_is_checked(pool: PgPool) {
    for (shift, class) in [(false, "dump_changed"), (true, "download")] {
        let m = mock();
        *m.cut_stream.lock().unwrap() = Some(200_000);
        m.reconnect.store(true, Ordering::SeqCst);
        if shift {
            m.shift_ranges.store(true, Ordering::SeqCst);
        } else {
            *m.etag_after_cut.lock().unwrap() = Some("\"fixture-2\"".into());
        }
        let base = serve(m.clone()).await;
        let (ctx, _dir) = context(&pool, None);
        let mut w = Wikidata::new();
        w.endpoints = importer(&base).endpoints.clone();
        w.from = Some(pinakes_core::catalogue::wikidata::From::Url(format!(
            "{base}/dumps/{DATE}/wikidata-{DATE}-all.json.bz2"
        )));
        assert!(once(Arc::new(w), &ctx, Run::Import).await.is_err());
        assert_eq!(
            last_error(&pool).await,
            Some(format!("{class} (127.0.0.1)")),
            "{shift}"
        );
        // The reconnect was made (a failed `If-Range` is answered whole, which the mock does not
        // count as a range).
        let ranges: &[u64] = if shift { &[0, 200_000] } else { &[0] };
        assert_eq!(*m.ranges.lock().unwrap(), ranges);
        sqlx::query("UPDATE catalogue_state SET run=NULL,cursor=NULL WHERE catalogue='wikidata'")
            .execute(&pool)
            .await
            .unwrap();
    }
}

/// A page answered in chunks without a `Content-Length` is read only up to its cap (4 MB): past
/// it the run fails (`size`); under it the page is read as any other.
#[sqlx::test(migrations = "../../migrations")]
async fn a_page_without_its_length_is_read_only_up_to_the_cap(pool: PgPool) {
    let m = mock();
    m.index_padding.store(5 << 20, Ordering::SeqCst);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(3));
    assert!(import(&pool, &ctx, &base).await.is_err());
    assert_eq!(last_error(&pool).await.as_deref(), Some("size (127.0.0.1)"));
    m.index_padding.store(1 << 20, Ordering::SeqCst);
    let Once::Finished(state) = import(&pool, &ctx, &base).await.unwrap() else {
        panic!()
    };
    assert_eq!(state.cards, 3);
}

/// A recent-changes answer without its list is no page: the update fails (`api`) and its position
/// stays, so the window's changes are read on the next try instead of skipped.
#[sqlx::test(migrations = "../../migrations")]
async fn changes_answered_without_their_list_are_read_again(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, Some(3));
    import(&pool, &ctx, &base).await.unwrap();
    one_change(&pool, &m, "Q30", "United States").await;
    let before = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap()
        .position;
    m.changes_without_list.store(true, Ordering::SeqCst);
    assert!(once(importer(&base), &ctx, Run::Update).await.is_err());
    assert_eq!(last_error(&pool).await.as_deref(), Some("api (127.0.0.1)"));
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.position, before);
}

/// The free disk is checked again before each download round (the disk may fill meanwhile, as the
/// task reads it): short, the run waits (its cursor kept, as for any disk the run needs) and goes
/// on from where it was once there is room.
#[sqlx::test(migrations = "../../migrations")]
async fn a_download_short_of_disk_on_the_way_waits(pool: PgPool) {
    let m = mock();
    *m.fail_ranges_from.lock().unwrap() = Some(150_000);
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    let mut w = Wikidata::new();
    w.endpoints = importer(&base).endpoints.clone();
    w.download_round = 100_000;
    let w = Arc::new(w);
    assert!(once(w.clone(), &ctx, Run::Import).await.is_err());
    *m.fail_ranges_from.lock().unwrap() = None;
    m.ranges.lock().unwrap().clear();
    // Room before the run and its first round; none once a range of this run has come, until the
    // run has said it waits.
    let short = Arc::new(AtomicBool::new(true));
    let (seen, is_short) = (m.clone(), short.clone());
    let ctx = Context {
        pool: pool.clone(),
        config: ctx.config.clone(),
        free_disk: Arc::new(move |_| {
            Ok(
                if is_short.load(Ordering::SeqCst) && !seen.ranges.lock().unwrap().is_empty() {
                    0
                } else {
                    u64::MAX
                },
            )
        }),
        pace: ctx.pace,
    };
    let waits = Arc::new(Mutex::new(Vec::new()));
    let said = waits.clone();
    let (state, _) = run::run_once(w, &ctx, Run::Import, move |why| {
        said.lock().unwrap().push(why.to_owned());
        short.store(false, Ordering::SeqCst);
    })
    .await
    .unwrap();
    let waits = waits.lock().unwrap().clone();
    assert!(
        waits.iter().any(|w| w.starts_with("free disk")),
        "{waits:?}"
    );
    // Not taken up from the start: the rounds before the wait were kept.
    assert!(!m.ranges.lock().unwrap().contains(&0));
    assert_eq!(rows(&pool).await.len() as i64, state.cards);
}

/// The label rewrite cut off part way through `pending.tsv` resumes at the byte it reached: the
/// cards end with the labels a run in one go gives them (a wrong seek would cut a line and lose a
/// card's labels).
#[sqlx::test(migrations = "../../migrations")]
async fn a_label_rewrite_cut_off_resumes_where_it_stopped(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, dir) = context(&pool, None);
    let importer_of = || {
        let mut w = Wikidata::new();
        w.endpoints = importer(&base).endpoints.clone();
        w.threads = (3, 2);
        // One card a write: the rewrite is cut after its first.
        w.write_cards = 1;
        Arc::new(w)
    };
    let labels = |rows: HashMap<String, Row>| -> std::collections::BTreeMap<String, Vec<String>> {
        rows.into_iter().map(|(k, r)| (k, r.labels)).collect()
    };
    let Once::Finished(_) = once(importer_of(), &ctx, Run::Import).await.unwrap() else {
        panic!()
    };
    let whole = labels(rows(&pool).await);
    // From nothing again, with the rewrite's second write refused (a sequence counts the writes:
    // its values outlive the rolled-back one).
    sqlx::raw_sql("DELETE FROM catalogue_card WHERE catalogue='wikidata'; DELETE FROM catalogue_state WHERE catalogue='wikidata'")
        .execute(&pool)
        .await
        .unwrap();
    std::fs::remove_dir_all(dir.0.join("wikidata")).unwrap();
    sqlx::raw_sql(
        "CREATE SEQUENCE label_writes;
         CREATE FUNCTION cut_label_rewrite() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
           -- Nested: SQL evaluates an AND in no set order, and only the rewrite's writes count.
           IF NEW.cursor->'step'->>'step' = 'labels' THEN
             IF (NEW.cursor->'step'->>'done')::bigint > 0 THEN
               IF nextval('label_writes') = 2 THEN
                 RAISE EXCEPTION 'cut';
               END IF;
             END IF;
           END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER cut_label_rewrite BEFORE UPDATE ON catalogue_state FOR EACH ROW
           EXECUTE FUNCTION cut_label_rewrite();",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(once(importer_of(), &ctx, Run::Import).await.is_err());
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    let step = &state.cursor.as_ref().unwrap()["step"];
    assert_eq!(step["step"], "labels", "{step}");
    let resumed_at = step["done"].as_u64().unwrap() as i64;
    assert!(resumed_at > 0, "{step}");
    // The lines left after the first one rewritten; the resumed run writes each once.
    let pending = lines_of(&dir.0.join("wikidata/pending.tsv")).len();
    assert!(pending >= 2, "{pending}");
    sqlx::raw_sql(
        "DROP TRIGGER cut_label_rewrite ON catalogue_state;
         CREATE TABLE label_rewrites (done bigint);
         CREATE FUNCTION log_label_rewrite() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
           IF NEW.cursor->'step'->>'step' = 'labels' THEN
             IF (NEW.cursor->'step'->>'done')::bigint > 0 THEN
               INSERT INTO label_rewrites VALUES ((NEW.cursor->'step'->>'done')::bigint);
             END IF;
           END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER log_label_rewrite BEFORE UPDATE ON catalogue_state FOR EACH ROW
           EXECUTE FUNCTION log_label_rewrite();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let Once::Finished(_) = once(importer_of(), &ctx, Run::Import).await.unwrap() else {
        panic!()
    };
    assert_eq!(labels(rows(&pool).await), whole);
    // One write a card (`write_cards` 1) past the byte it resumed at, each at its own byte (a write
    // updates the state row more than once): a seek past that byte would cut a line and skip it.
    let after: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT done) FROM label_rewrites WHERE done > $1")
            .bind(resumed_at)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after as usize, pending - 1);
}

/// A class or genre whose label the catalogue keeps is read in full when it changes, also once it
/// has no article of its own (its sitelinks alone would not say it): its new label is kept for the
/// cards read after it, on a line of its own after a line a hard stop cut off.
#[sqlx::test(migrations = "../../migrations")]
async fn a_renamed_genre_without_an_article_gives_its_new_label(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let mut genre: Value = {
        let packed = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/wikidata/dump-sample.json.bz2"
        ))
        .unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut bzip2::read::BzDecoder::new(&packed[..]), &mut text)
            .unwrap();
        let line = text
            .lines()
            .find(|l| l.starts_with("{\"type\":\"item\",\"id\":\"Q11399\""))
            .unwrap();
        serde_json::from_str(line.trim_end_matches(',')).unwrap()
    };
    genre["labels"] = json!({"en": {"language": "en", "value": "Loud Music"}});
    genre["sitelinks"] = json!({});
    // A line a hard stop cut off: the update ends the file on its last whole line first.
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.0.join("wikidata/labels.tsv"))
            .unwrap();
        file.write_all(b"123\tcut of").unwrap();
    }
    m.entities.lock().unwrap().insert("Q11399".into(), genre);
    let at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    m.changes.lock().unwrap().push(("Q11399".into(), at));
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&pool)
        .await
        .unwrap();
    let Once::Finished(_) = once(importer(&base), &ctx, Run::Update).await.unwrap() else {
        panic!()
    };
    assert!(m.asked.lock().unwrap().contains(&"Q11399".to_owned()));
    let labels = lines_of(&dir.0.join("wikidata/labels.tsv"));
    assert_eq!(
        labels
            .iter()
            .rev()
            .find(|l| l.starts_with("11399\t"))
            .map(String::as_str),
        Some("11399\tloud-music"),
        "{labels:?}"
    );
    assert!(!labels.iter().any(|l| l.starts_with("123\t")), "{labels:?}");
}

/// A dump read slower than its floor (here 16 KB every 100 ms against a 300 ms window: the
/// daemon's is 8 MB in ten minutes of waiting) fails `download` instead of holding the import:
/// a download round and a stream alike.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_read_slower_than_its_floor_fails(pool: PgPool) {
    // No limit: a download; a limit above the dump's items: a stream of all of it.
    for limit in [None, Some(1000)] {
        let m = mock();
        m.drip_ms.store(100, Ordering::SeqCst);
        let base = serve(m.clone()).await;
        let (ctx, _dir) = context(&pool, limit);
        let mut w = Wikidata::for_config(&ctx.config);
        w.endpoints = importer(&base).endpoints.clone();
        w.slow_window = Duration::from_millis(300);
        assert!(
            once(Arc::new(w), &ctx, Run::Import).await.is_err(),
            "{limit:?}"
        );
        assert_eq!(
            last_error(&pool).await.as_deref(),
            Some("download (127.0.0.1)"),
            "{limit:?}"
        );
        // Not read whole (a stream writes what it parsed before it failed).
        assert!(rows(&pool).await.len() < kept_ids().len(), "{limit:?}");
        sqlx::query("UPDATE catalogue_state SET run=NULL,cursor=NULL,last_error=NULL,phase='idle' WHERE catalogue='wikidata'")
            .execute(&pool)
            .await
            .unwrap();
    }
}

/// An update whose removals would pass a tenth of the catalogue's cards (and ten: every item
/// deleted, a broken answer) is refused at the page that would take it past: that page and the
/// rest are not written, the position names the changes it read, and the next update does not
/// read them again.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_that_removes_too_much_is_refused(pool: PgPool) {
    let m = mock();
    let base = serve(m.clone()).await;
    let (ctx, _dir) = context(&pool, None);
    import(&pool, &ctx, &base).await.unwrap();
    let before = rows(&pool).await;
    assert!(before.len() > 20, "{}", before.len());
    let now = chrono::Utc::now();
    let at = |mins: i64| {
        (now - chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    {
        let mut e = m.entities.lock().unwrap();
        for id in before.keys() {
            e.insert(id.clone(), Value::Null);
        }
    }
    *m.changes.lock().unwrap() = before.keys().map(|id| (id.clone(), at(30))).collect();
    sqlx::query("UPDATE catalogue_state SET position=jsonb_build_object('time',$1::text) WHERE catalogue='wikidata'")
        .bind(at(60))
        .execute(&pool)
        .await
        .unwrap();
    let failed = once(importer(&base), &ctx, Run::Update).await.unwrap_err();
    assert!(
        failed.to_string().contains("refused (the changes after"),
        "{failed}"
    );
    // The pages before the one that would pass the cap are applied: at most ten removals.
    assert!(rows(&pool).await.len() >= before.len() - 10);
    let state = store::state(&pool, Catalogue::Wikidata)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((state.run.as_deref(), state.error_count), (None, 1));
    assert!(state.position.unwrap()["refused"].is_string());
    m.asked_sitelinks.lock().unwrap().clear();
    let again = once(importer(&base), &ctx, Run::Update).await.unwrap_err();
    assert!(again.to_string().contains("was refused before"), "{again}");
    assert!(m.asked_sitelinks.lock().unwrap().is_empty());
}
