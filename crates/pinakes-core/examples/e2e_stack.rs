//! Isolated e2e stack with the real schema, ingestion, resolver parsers, storage and API.
//! Default mode mocks external boundaries with no public network; --live scopes real fetches
//! to the sample IDs (at most `MAX_LIVE_SAMPLES`).
// The mock's sample list is one `json!` literal, deeper than the default macro recursion limit.
#![recursion_limit = "256"]
use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use axum::{
    extract::{Query, State},
    routing::{get, post, put},
    Json, Router,
};
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    ingest,
    model::identity,
    nexus::{Change, Changes, Kind, Nexus, Op},
    queue,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use url::Url;

const AUTHOR: &str = "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy";
#[derive(Default)]
struct Boundary {
    tags: BTreeMap<String, Value>,
    lines: Vec<Change>,
}
#[derive(Clone)]
struct Fixture {
    boundary: Arc<Mutex<Boundary>>,
    samples: Value,
    /// The samples left out for a source this build does not have (`by_needs`).
    skipped: Vec<Value>,
    event: nostr::event::Event,
    start: chrono::DateTime<chrono::Utc>,
}
impl Fixture {
    fn new() -> Result<Self> {
        use chrono::Timelike;
        use nostr::prelude::*;
        let start = (chrono::Utc::now() + chrono::Duration::days(7))
            .with_nanosecond(0)
            .unwrap();
        let keys = Keys::parse(&"01".repeat(32))?; // Public fixture secret, never an account.
        let unsigned = EventBuilder::new(Kind::from(31923), "Fixture Nostr event")
            .tags([
                Tag::parse(["d", "pinakes-e2e"])?,
                Tag::parse(["title", "Nostr meetup"])?,
                Tag::parse(["start", &start.timestamp().to_string()])?,
                Tag::parse(["location", "47.0,8.0"])?,
                Tag::parse(["g", "u0k8ug"])?,
            ])
            .custom_created_at(Timestamp::from_secs(1700000000))
            .finalize_unsigned(keys.public_key());
        let sig = keys.sign_schnorr_with_aux_rand(
            &secp256k1::Secp256k1::new(),
            unsigned.compute_id().as_bytes(),
            &[0; 32],
        );
        let event = unsigned.add_signature(sig)?;
        let naddr = pinakes_core::event::adapters::nostr::Target::coordinate(&format!(
            "31923:{}:pinakes-e2e",
            keys.public_key().to_hex()
        ))?
        .uri()?;
        let mut samples = json!([
            {"kind":"osm","uri":"https://www.openstreetmap.org/node/501","type":"Place"},
            {"kind":"geo","uri":"geo:47,8","type":"Place"},
            {"kind":"osm_alias","uri":"https://www.openstreetmap.org/node/501/history","type":"Place","group_with":"osm"},
            {"kind":"osm_short","uri":"https://osm.org/go/0GAjIv8h?node=501","type":"Place","group_with":"osm"},
            {"kind":"marker","uri":"https://www.openstreetmap.org/?mlat=47.0&mlon=8.0","type":"Place"},
            {"kind":"event","uri":"https://events.example/e2e","type":"Event"},
            {"kind":"event_elsewhere","uri":"https://events.example/e2e/elsewhere","type":"Event","maybe_same_with":"event"},
            {"kind":"ics","uri":"https://calendar.example/e2e.ics","type":"DataFeed","expectedOccurrences":2},
            {"kind":"naddr","uri":naddr,"type":"Event"},
            {"kind":"doi","uri":"doi:10.1234/pinakes","type":"ScholarlyArticle"},
            {"kind":"doi_page","uri":"https://publisher.example/articles/pinakes","type":"CreativeWork","group_with":"doi"},
            {"kind":"isbn","uri":"isbn:9780140328721","type":"Book"},
            {"kind":"isbn_twin","uri":"isbn:0140328726","type":"Book","group_with":"isbn"},
            // Typed a Book by the `books` source (a build without it leaves the sample out).
            {"kind":"isbn_page","uri":"https://openlibrary.org/books/OL7353617M","type":"Book","group_with":"isbn","needs":"books"},
            {"kind":"media","uri":"https://www.youtube.com/watch?v=fixture","type":"VideoObject"},
            {"kind":"web","uri":"https://web.example/e2e","type":"WebPage"},
        ]);
        // A sample that needs an also-on source (`needs`) runs only in a build that has it.
        let (kept, skipped) = by_needs(
            std::mem::take(samples.as_array_mut().expect("an array")),
            &built(),
        );
        *samples.as_array_mut().expect("an array") = kept;
        let own = samples.as_array_mut().expect("an array");
        // Each official source's own samples (`Source::e2e`).
        for e2e in pinakes_core::sources::REGISTRY
            .iter()
            .filter_map(|s| s.e2e())
        {
            for sample in e2e.samples {
                let mut v =
                    json!({"kind": sample.kind, "uri": sample.uri, "type": sample.card_type});
                if let Some(other) = sample.group_with {
                    v["group_with"] = json!(other);
                }
                own.push(v);
            }
        }
        Ok(Self {
            boundary: Arc::default(),
            samples,
            skipped,
            event,
            start,
        })
    }
    fn place(&self) -> Value {
        json!({"osm_type":"node","osm_id":501,"lat":"47.0","lon":"8.0","category":"amenity","type":"cafe","name":"Fixture Cafe","display_name":"Fixture Cafe, Main Street 1, Zurich","namedetails":{"name":"Fixture Cafe"},"importance":0.9,"place_rank":30,"address":{"house_number":"1","road":"Main Street","city":"Zurich","postcode":"8000","country_code":"ch"},"extratags":{},"boundingbox":["47","47","8","8"]})
    }
}
struct FixtureDns;
#[async_trait]
impl Dns for FixtureDns {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        // The core services', and every official source's in its mock answers.
        let ip = if matches!(
            host,
            "events.example"
                | "calendar.example"
                | "api.nostr.wine"
                | "api.nostrarchives.com"
                | "doi.org"
                | "openlibrary.org"
                | "publisher.example"
                | "www.youtube.com"
                | "web.example"
        ) || source_answers().any(|(_, a)| a.host == host)
        {
            "93.184.216.34"
        } else {
            "192.0.2.1"
        };
        Ok(vec![SocketAddr::new(ip.parse()?, port)])
    }
}
#[async_trait]
impl Transport for Fixture {
    async fn get(&self, u: &Url, a: &[SocketAddr]) -> Result<Fetched> {
        self.get_with(u, a, &Default::default()).await
    }
    async fn get_with(&self, u: &Url, _: &[SocketAddr], o: &FetchOptions) -> Result<Fetched> {
        let (ct,body)=match u.host_str().unwrap_or("") {
            "events.example" => ("text/html",format!("<script type='application/ld+json'>{}</script>",json!({"@type":"Event","name":"Fixture meetup","startDate":self.start,"location":{"@type":"Place","name":"Fixture Cafe","address":{"@type":"PostalAddress","streetAddress":"1 Main Street","addressLocality":"Zurich","postalCode":"8000","addressCountry":"CH"}}}))),
            "calendar.example" => ("text/calendar",format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:e2e-series\r\nDTSTART:{}\r\nRRULE:FREQ=DAILY;COUNT=2\r\nSUMMARY:Fixture calendar meetup\r\nGEO:47;8\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",self.start.format("%Y%m%dT%H%M%SZ"))),
            "api.nostr.wine"|"api.nostrarchives.com"=>("application/json",json!([self.event]).to_string()),
            // Crossref shape: CSL `URL` is the DOI proxy, the landing page is resource.primary.
            "doi.org"=>{ensure!(o.accept.as_deref()==Some("application/vnd.citationstyles.csl+json"),"missing CSL negotiation");("application/json",json!({"type":"article-journal","title":"Fixture research","URL":"http://dx.doi.org/10.1234/pinakes","resource":{"primary":{"URL":"https://publisher.example/articles/pinakes"}}}).to_string())},
            "publisher.example"=>("text/html","<title>Fixture research | Publisher</title><link rel=\"canonical\" href=\"https://publisher.example/articles/pinakes\"><meta name=\"citation_doi\" content=\"10.1234/pinakes\">".into()),
            "openlibrary.org" if u.path().starts_with("/books/")=>("text/html","<title>Matilda | Open Library</title><link rel=\"canonical\" href=\"https://openlibrary.org/books/OL7353617M/Matilda\">".into()),
            "openlibrary.org"=>("application/json",json!({"ISBN:9780140328721":{"title":"Matilda","authors":[{"name":"Roald Dahl"}],"url":"https://openlibrary.org/books/OL7353617M/Matilda","key":"/books/OL7353617M"},"ISBN:0140328726":{"title":"Matilda","authors":[{"name":"Roald Dahl"}],"url":"https://openlibrary.org/books/OL7353617M/Matilda","key":"/books/OL7353617M"}}).to_string()),
            "www.youtube.com"=>("application/json",json!({"type":"video","title":"Fixture video","thumbnail_url":"https://image.example/a.jpg"}).to_string()),
            "web.example"=>("text/html","<title>Fixture page</title>".into()),
            _=>match source_answers().find(|(_,a)|a.matches(u,&o.headers)) {
                // An official source's recorded answer (its `Source::e2e`).
                Some((source,a))=>(a.content_type,fixture(source,a.file)?),
                None=>bail!("no fixture for upstream host {}",u.host_str().unwrap_or("")),
            },
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: u.clone(),
            status: 200,
            content_type: ct.into(),
            body: body.into_bytes(),
            location: None,
            retry_after_secs: 60,
            validators: Default::default(),
        })
    }
}
#[derive(Deserialize)]
struct Write {
    path: String,
    tag: Value,
}
/// The most sample resources one live run fetches.
const MAX_LIVE_SAMPLES: usize = 128;
/// The contact a run gives the services it reads when the operator set none (`http.contact`);
/// `e2e/harness.mjs`'s preflight fetches name the same one.
const E2E_CONTACT: &str = "info@synonym.to";

async fn write(State(f): State<Fixture>, Json(w): Json<Write>) -> axum::http::StatusCode {
    if !w.path.starts_with("/pub/pinakes.e2e/tags/") {
        return axum::http::StatusCode::BAD_REQUEST;
    }
    let uri = w.tag["uri"].as_str().unwrap_or("");
    let Ok((_, uri, _)) = identity(uri) else {
        return axum::http::StatusCode::BAD_REQUEST;
    };
    let line = Change {
        op: Op::Put,
        uri,
        kind: Kind::Tag,
        author: AUTHOR.into(),
        object_path: w.path.clone(),
        label: w.tag["label"].as_str().map(str::to_owned),
        at: chrono::Utc::now().timestamp_millis(),
        recurrence_id: None,
        status: None,
        start: None,
        end: None,
    };
    let mut b = f.boundary.lock().unwrap();
    b.tags.insert(w.path, w.tag);
    b.lines.push(line);
    axum::http::StatusCode::NO_CONTENT
}
#[derive(Deserialize)]
struct Delete {
    path: String,
}
async fn delete(State(f): State<Fixture>, Json(w): Json<Delete>) -> axum::http::StatusCode {
    let mut b = f.boundary.lock().unwrap();
    if b.tags.remove(&w.path).is_some() {
        if let Some(mut line) = b
            .lines
            .iter()
            .rev()
            .find(|l| l.object_path == w.path)
            .cloned()
        {
            line.op = Op::Del;
            line.at = chrono::Utc::now().timestamp_millis();
            b.lines.push(line);
        }
    }
    axum::http::StatusCode::NO_CONTENT
}
#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    cursor: usize,
    #[serde(default = "page_limit")]
    limit: usize,
}
/// A recorded fixture stored gzip-compressed, as text.
/// Every official source's recorded mock answers, with the source's name.
fn source_answers() -> impl Iterator<Item = (&'static str, pinakes_core::sources::E2eAnswer)> {
    pinakes_core::sources::REGISTRY
        .iter()
        .filter_map(|s| Some((s.name(), s.e2e()?)))
        .flat_map(|(name, e2e)| e2e.answers.iter().map(move |a| (name, *a)))
}
/// A recorded answer of `source` (`tests/fixtures/sources/{source}/{file}`, `.gz` unpacked).
fn fixture(source: &str, file: &str) -> Result<String> {
    let path = format!(
        "{}/tests/fixtures/sources/{source}/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read(&path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    Ok(if file.ends_with(".gz") {
        gunzip(&raw)
    } else {
        String::from_utf8(raw)?
    })
}
fn gunzip(raw: &[u8]) -> String {
    use std::io::Read;
    let mut out = String::new();
    flate2::read::GzDecoder::new(raw)
        .read_to_string(&mut out)
        .expect("gzip fixture");
    out
}
fn page_limit() -> usize {
    1000
}
async fn feed(
    State(f): State<Fixture>,
    Query(p): Query<Page>,
) -> ([(String, String); 2], Json<Changes>) {
    let b = f.boundary.lock().unwrap();
    let start = p.cursor.min(b.lines.len());
    let end = (start + p.limit.min(1000)).min(b.lines.len());
    (
        [
            ("x-events-epoch".into(), "e2e-fixture".into()),
            ("x-events-length".into(), b.lines.len().to_string()),
        ],
        Json(Changes {
            lines: b.lines[start..end].to_vec(),
            next_cursor: end as i64,
        }),
    )
}
/// Nexus's batch view by `uris`, answered in request order.
async fn hydrate(State(f): State<Fixture>, Json(q): Json<Value>) -> Json<Value> {
    let b = f.boundary.lock().unwrap();
    let uris = q["uris"].as_array().cloned().unwrap_or_default();
    Json(Value::Array(uris.iter().filter_map(|v| {
        let (id, uri, _) = identity(v.as_str()?).ok()?;
        let lines:Vec<_>=b.lines.iter().filter(|l|l.uri==uri && matches!(l.op,Op::Put) && b.tags.contains_key(&l.object_path)).collect();
        lines.first().map(|line|json!({"details":{"id":id,"uri":line.uri},"taggers_count":1,"tags":lines.iter().map(|l|json!({"label":l.label,"taggers":[AUTHOR],"taggers_count":1,"relationship":true})).collect::<Vec<_>>()}))
    }).collect()))
}
fn main() -> Result<()> {
    // The stack is its own HTML parse worker, as the daemon is: before any runtime starts, so
    // the worker's memory limit is for one thread's parse.
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "parse-worker") {
        let secs = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
        std::process::exit(pinakes_core::resolver::html::worker_main(secs));
    }
    pinakes_core::resolver::html::set_worker(std::env::current_exe()?, ["parse-worker"]);
    pinakes_core::resolver::html::check_worker()?;
    tokio::runtime::Runtime::new()?.block_on(stack(args))
}
async fn stack(args: Vec<String>) -> Result<()> {
    let fixture = Fixture::new()?;
    let mut live = if args.get(1).is_some_and(|s| s == "--live") {
        Some(serde_json::from_slice::<Value>(&std::fs::read(
            args.get(2)
                .ok_or_else(|| anyhow::anyhow!("missing live samples"))?,
        )?)?)
    } else {
        None
    };
    if let (Some(samples), Ok(uri)) = (
        live.as_mut().and_then(Value::as_array_mut),
        std::env::var("PINAKES_E2E_ICS_URL"),
    ) {
        for sample in samples {
            if sample["kind"] == "ics" {
                sample["uri"] = json!(uri);
            }
        }
    }
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(
            &std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgresql:///postgres?host=/var/run/postgresql".into()),
        )
        .await?;
    let database = format!(
        "pinakes_e2e_owned_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp()
    );
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await?;
    let options = admin.connect_options().as_ref().clone().database(&database);
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect_with(options)
        .await?;
    let result = if let Some(samples) = live {
        run_live(pool.clone(), samples).await
    } else {
        run(pool.clone(), fixture).await
    };
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database} WITH (FORCE)"))
        .execute(&admin)
        .await?;
    result
}
async fn run(pool: sqlx::PgPool, f: Fixture) -> Result<()> {
    pinakes_core::migrate(&pool).await?;
    // As `pinakesd` does: every write below runs while this connection holds the instance lock.
    let _instance = pinakes_core::locks::own_database(&pool).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let base = format!("http://{address}");
    let mut cfg = Config {
        nexus_url: base.clone(),
        ..Default::default()
    };
    cfg.osm.nominatim_url = base.clone();
    cfg.osm.requests_per_second = 10.;
    cfg.canary.enabled = false;
    // Services that need a contact (SEC EDGAR) wait without one.
    cfg.egress.contact = E2E_CONTACT.into();
    cfg.osm.contact = E2E_CONTACT.into();
    cfg.pubky.enabled = false;
    cfg.api.requests_per_minute = 10000;
    // The harness tags as one author: one shared tagger relates two samples.
    cfg.search.related_min_shared = 1;
    let state = AppState::new(pool.clone(), cfg.clone())?;
    let metrics = state.metrics.clone();
    let wake = state.wake.clone();
    let app = api::router(state).merge(
        Router::new()
            .route(
                "/mock/tags",
                put(write).delete(delete).get(
                    |State(f): State<Fixture>, Query(q): Query<Delete>| async move {
                        Json(
                            json!({"exists":f.boundary.lock().unwrap().tags.contains_key(&q.path)}),
                        )
                    },
                ),
            )
            .route(
                "/mock/config",
                get(|State(f): State<Fixture>| async move {
                    Json(json!({"author":AUTHOR,"samples":f.samples,"positive_venue":true}))
                }),
            )
            .route(
                "/lookup",
                get(|State(f): State<Fixture>| async move { Json(vec![f.place()]) }),
            )
            .route(
                "/search",
                get(|State(f): State<Fixture>| async move { Json(vec![f.place()]) }),
            )
            .route("/v0/resources/changes", get(feed))
            .route("/v0/stream/resources/by_ids", post(hydrate))
            // The run's author follows and befriends itself: a reach or boost set of one.
            .route(
                "/v0/user/{id}/following",
                get(|| async { Json(vec![AUTHOR]) }),
            )
            .route(
                "/v0/user/{id}/friends",
                get(|| async { Json(vec![AUTHOR]) }),
            )
            .with_state(f.clone()),
    );
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let skipped = f.skipped.clone();
    let egress = Egress::with_backends(cfg.egress.clone(), Arc::new(FixtureDns), Arc::new(f));
    ensure!(
        egress
            .nostr_request("wss://api.nostr.wine", json!({}), 100)
            .await
            .unwrap_err()
            .to_string()
            .contains("disabled"),
        "mock relay must never connect"
    );
    let mut workers = queue::spawn_with_egress(
        pool.clone(),
        cfg.clone(),
        wake.clone(),
        rx,
        metrics.clone(),
        egress,
    )?;
    let nexus = Nexus::new(&base)?;
    let url = base.clone();
    let polling = tokio::spawn(async move {
        loop {
            if let Err(e) = ingest::poll_once(&pool, &url, &nexus, &cfg.ingest, &metrics).await {
                eprintln!("mock ingestion: {e}");
            }
            if let Err(e) = pinakes_core::groups::drain(&pool, &cfg.queue.groups, 25).await {
                eprintln!("mock grouping: {e}");
            }
            if let Err(e) = pinakes_core::same_event::drain(&pool, &cfg, 200).await {
                eprintln!("mock same-event pass: {e}");
            }
            wake.notify_waiters();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    println!(
        "{}",
        json!({"pinakes":base,"nexus":base,"mock":true,"skipped":skipped})
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        term.recv().await;
    })
    .await?;
    shutdown.send(true)?;
    polling.abort();
    workers.abort_all();
    while workers.join_next().await.is_some() {}
    Ok(())
}

// A scoped live driver keeps a private copy of the real Nexus feed, but claims only
// the selected sample IDs. It uses the same leases, completion fences and resolvers.
// No canaries, refresh sweep or unrelated backlog is sent to public upstreams.
async fn run_live(pool: sqlx::PgPool, samples: Value) -> Result<()> {
    use pinakes_core::event::{
        adapters::service::PlatformResolver,
        service::{CalendarResolver, EventService},
    };
    use pinakes_core::resolver::{
        cards::CardResolver, geo::GeoResolver, osm::OsmResolver, pubky::PubkyResolver,
        web::WebResolver, Resolver,
    };
    pinakes_core::migrate(&pool).await?;
    let _instance = pinakes_core::locks::own_database(&pool).await?;
    // Settings and secrets come from the environment as the daemon reads them (the TIDAL
    // credentials, `PINAKES__…`); there is no file.
    let mut cfg = Config::load(None)?;
    cfg.canary.enabled = false;
    // Services that need a contact (SEC EDGAR) wait without one: the operator's
    // (`PINAKES__HTTP__CONTACT`, which `Config::load` gave every client), else this run's.
    if cfg.egress.contact.is_empty() {
        cfg.egress.contact = E2E_CONTACT.into();
        cfg.osm.contact = E2E_CONTACT.into();
    }
    cfg.api.requests_per_minute = 10000;
    cfg.nexus_url = std::env::var("PINAKES_E2E_NEXUS").unwrap_or_else(|_| cfg.nexus_url.clone());
    // As the daemon: the sources' switches and tokens (GitHub, Discogs) apply to this run.
    pinakes_core::configure_sources(&cfg);
    let cfg = Arc::new(cfg);
    let egress = Egress::new(cfg.egress.clone());
    let osm = Arc::new(OsmResolver::new(pool.clone(), cfg.osm.clone())?);
    let events = Arc::new(EventService {
        pool: pool.clone(),
        config: cfg.event.clone(),
        osm: osm.clone(),
    });
    let web = || WebResolver {
        oembed_policy: cfg.media.oembed,
        cards: Some(Arc::new(cfg.cards.clone())),
        events: Some(events.clone()),
        egress: egress.clone(),
        config: cfg.web.clone(),
    };
    let mut resolvers: Vec<Arc<dyn Resolver>> = vec![
        osm,
        Arc::new(GeoResolver),
        Arc::new(PlatformResolver { web: web() }),
        Arc::new(CalendarResolver { web: web() }),
        Arc::new(web()),
        Arc::new(PubkyResolver::new(cfg.pubky.clone(), egress.clone())?),
    ];
    for kind in pinakes_core::resolver::cards::KINDS {
        resolvers.push(Arc::new(CardResolver {
            kind,
            config: cfg.clone(),
            web: web(),
        }));
    }
    // A sample whose expectations come from one also-on source (`needs`: a TIDAL album's own
    // barcode) runs only in a build that has the source, as in the mock stack: without it the
    // sample is left out and named in the ready line. A build that has the source but cannot run
    // it (TIDAL without its credentials) refuses at once, rather than timing out later.
    let sources = pinakes_core::also_on::registry();
    let (samples, skipped) = live_selection(
        samples
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("samples must be an array"))?,
        &built(),
        |need| {
            sources
                .iter()
                .any(|s| s.id() == need && cfg.also_on.source_enabled(s.as_ref()) && s.ready(&cfg))
        },
    )?;
    let samples = Value::Array(samples);
    let mut selected = vec![];
    for sample in samples
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("samples must be an array"))?
    {
        let uri = sample["uri"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("sample URI"))?;
        // A spelling (a TIDAL `/u` share link) is resolved as its root.
        let root = pinakes_core::alias::root(uri).map_or_else(|| uri.to_owned(), |a| a.root);
        let (id, uri, _) = identity(&root)?;
        let kind = pinakes_core::resolver::classify(&uri)
            .ok_or_else(|| anyhow::anyhow!("unclassified sample"))?;
        let resolver = resolvers
            .iter()
            .find(|r| r.name() == kind)
            .ok_or_else(|| anyhow::anyhow!("missing sample resolver"))?
            .clone();
        selected.push((id, resolver));
    }
    // The scope stays explicit: only listed samples (and what a fetch shows them to be spellings
    // of, below) are ever fetched, one at a time with a second between them. The cap bounds how
    // many, not how long a run takes.
    ensure!(
        (1..=MAX_LIVE_SAMPLES).contains(&selected.len()),
        "live driver requires one to {MAX_LIVE_SAMPLES} sample resources, got {}",
        selected.len()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let state = AppState::new(pool.clone(), cfg.as_ref().clone())?;
    let metrics = state.metrics.clone();
    let app = api::router(state);
    let nexus = Nexus::new(&cfg.nexus_url)?;
    let ready = json!({"pinakes":base,"nexus":cfg.nexus_url,"mock":false,"skipped":skipped});
    let also_on = pinakes_core::also_on::Context {
        egress: egress.clone(),
        config: cfg.clone(),
    };
    let polling = tokio::spawn(async move {
        loop {
            if let Err(e) =
                ingest::poll_once(&pool, &cfg.nexus_url, &nexus, &cfg.ingest, &metrics).await
            {
                eprintln!("live ingestion: {e}");
            }
            for (id, resolver) in &selected {
                // A fetch can show a sample to be a spelling of another URI (a Bluesky handle's
                // post under its DID): the sample becomes that root's alias, and its card is the
                // root's, so the root is fetched in its place, by the root's own resolver.
                let job=sqlx::query_as::<_,queue::Job>("UPDATE resource SET lease_until=now()+interval '60 seconds',generation=generation+1 WHERE id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1) AND NOT dead_letter AND NOT final AND next_attempt_at<=now() AND (lease_until IS NULL OR lease_until<now()) RETURNING id,uri,attempts,generation").bind(id).fetch_optional(&pool).await;
                let resolver = match &job {
                    Ok(Some(job)) if job.id != *id => {
                        let Some(root) = pinakes_core::resolver::classify(&job.uri)
                            .and_then(|kind| resolvers.iter().find(|r| r.name() == kind))
                        else {
                            eprintln!("live sample root: no resolver for {}", job.uri);
                            continue;
                        };
                        root
                    }
                    _ => resolver,
                };
                match job {
                    Ok(Some(job)) => {
                        // The production path: the workers finish every job through a batch.
                        if let Err(e) = queue::run_batch(
                            &pool,
                            resolver.as_ref(),
                            &cfg.queue,
                            std::slice::from_ref(&job),
                        )
                        .await
                        {
                            eprintln!("live sample completion: {e}");
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => eprintln!("live sample claim: {e}"),
                    _ => {}
                }
            }
            if let Err(e) = pinakes_core::groups::drain(&pool, &cfg.queue.groups, 25).await {
                eprintln!("live grouping: {e}");
            }
            if let Err(e) = pinakes_core::same_event::drain(&pool, &cfg, 200).await {
                eprintln!("live same-event pass: {e}");
            }
            // The samples' also-on sources (their hubs, live): derived links and hub keys.
            if let Err(e) = pinakes_core::also_on::drain(&pool, &also_on, &sources, 16).await {
                eprintln!("live also-on: {e}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    println!("{ready}");
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        term.recv().await;
    })
    .await?;
    polling.abort();
    let _ = polling.await;
    Ok(())
}

/// The also-on sources this build has.
fn built() -> Vec<&'static str> {
    pinakes_core::also_on::registry()
        .iter()
        .map(|s| s.id())
        .collect()
}

/// The samples a build runs, and those it leaves out: a sample that `needs` an also-on source
/// this build does not have (removed with its files) is left out, as `{kind, uri, reason}`.
fn by_needs(samples: Vec<Value>, built: &[&str]) -> (Vec<Value>, Vec<Value>) {
    let (kept, skipped): (Vec<Value>, Vec<Value>) = samples
        .into_iter()
        .partition(|s| s["needs"].as_str().is_none_or(|need| built.contains(&need)));
    let skipped = skipped
        .into_iter()
        .map(|s| {
            json!({
                "kind": s["kind"],
                "uri": s["uri"],
                "reason": format!("needs {}, which this build does not have", s["needs"].as_str().unwrap_or("?")),
            })
        })
        .collect();
    (kept, skipped)
}

/// The live stack's samples (`by_needs`), refusing a sample whose source this build has but
/// cannot run (`runs`: switched on and ready, its credentials set).
fn live_selection(
    samples: Vec<Value>,
    built: &[&str],
    runs: impl Fn(&str) -> bool,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let (kept, skipped) = by_needs(samples, built);
    let unmet: Vec<String> = kept
        .iter()
        .filter_map(|sample| {
            let need = sample["needs"].as_str()?;
            (!runs(need))
                .then(|| format!("{} needs {need}", sample["kind"].as_str().unwrap_or("?")))
        })
        .collect();
    ensure!(
        unmet.is_empty(),
        "live samples need an also-on source this stack cannot run ({}); for tidal, set \
         PINAKES_TIDAL_CLIENT_ID and PINAKES_TIDAL_CLIENT_SECRET",
        unmet.join(", ")
    );
    Ok((kept, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic samples, so the tests hold whatever sources this build has: one needs a source
    /// the build has (`kept_source`), one a source it does not (`gone_source`), one needs none,
    /// and one joins the sample that is left out.
    fn samples() -> Vec<Value> {
        vec![
            json!({"kind": "plain", "uri": "https://plain.example/1"}),
            json!({"kind": "with_kept", "uri": "https://kept.example/1", "needs": "kept_source"}),
            json!({"kind": "with_gone", "uri": "https://gone.example/1", "needs": "gone_source"}),
            json!({"kind": "joins_gone", "uri": "https://plain.example/2", "group_with": "with_gone", "needs": "gone_source"}),
        ]
    }

    /// A build without a source runs the samples but those that need it, and names each one it
    /// leaves out with its reason; the live selection starts (no refusal for a source the build
    /// does not have at all).
    #[test]
    fn a_sample_whose_source_the_build_lacks_is_left_out_and_named() {
        let (kept, skipped) =
            live_selection(samples(), &["kept_source"], |_| true).expect("starts");
        let kinds: Vec<&str> = kept.iter().filter_map(|s| s["kind"].as_str()).collect();
        assert_eq!(kinds, ["plain", "with_kept"]);
        assert_eq!(
            skipped,
            [
                json!({"kind": "with_gone", "uri": "https://gone.example/1", "reason": "needs gone_source, which this build does not have"}),
                json!({"kind": "joins_gone", "uri": "https://plain.example/2", "reason": "needs gone_source, which this build does not have"}),
            ]
        );
        // The mock stack's selection is the same.
        assert_eq!(by_needs(samples(), &["kept_source"]), (kept, skipped));
    }

    /// A source the build has but cannot run (switched off, or without its credentials) still
    /// refuses the live start, naming every sample that needs it; with every source built and
    /// running, nothing is left out.
    #[test]
    fn a_source_the_build_has_but_cannot_run_refuses() {
        let error = live_selection(samples(), &["kept_source"], |need| need != "kept_source")
            .unwrap_err()
            .to_string();
        assert!(error.contains("with_kept needs kept_source"), "{error}");
        assert!(!error.contains("gone_source"), "{error}");
        let (kept, skipped) =
            live_selection(samples(), &["kept_source", "gone_source"], |_| true).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(kept, samples());
    }
}
