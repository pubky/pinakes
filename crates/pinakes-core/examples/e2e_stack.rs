//! Isolated e2e stack with the real schema, ingestion, resolver parsers, storage and API.
//! Default mode mocks external boundaries with no public network; --live scopes real fetches
//! to the sample IDs (at most `MAX_LIVE_SAMPLES`); --live-recipes fetches the public
//! `/v1/resolve` examples once each and reports their cards.
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
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        Resolver,
    },
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
            // Job postings: one on site (its workplace geocoded to the fixture place), one remote.
            {"kind":"job","uri":"https://jobs.example/e2e","type":"JobPosting","facts":{"hiring_organization":"Fixture Co","job_status":"open"}},
            {"kind":"job_remote","uri":"https://jobs.example/e2e/remote","type":"JobPosting","facts":{"remote":"true","remote_regions":"Europe"}},
            // A shop's product page: its barcode and offer (the barcode's key is an also-on
            // source's, which this run does not run).
            {"kind":"shop_product","uri":"https://shop.example/e2e/nutella","type":"Product","facts":{"gtin":"3017620422003","price_currency":"CHF","availability":"in_stock"}},
            // A Q&A page's question, its answers and the accepted one.
            {"kind":"qa_question","uri":"https://qa.example/e2e/q/1","type":"Question","facts":{"answer_count":"2","accepted_answer":"https://qa.example/e2e/q/1#a2"}},
            // A recipe, found by its ingredients.
            {"kind":"recipe","uri":"https://recipes.example/e2e/rosti","type":"Recipe","facts":{"total_minutes":"45","ingredients":"waxy potatoes"}},
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
                | "jobs.example"
                | "shop.example"
                | "qa.example"
                | "recipes.example"
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
            "recipes.example"=>("text/html",format!("<script type='application/ld+json'>{}</script>",json!({"@context":"https://schema.org","@graph":[{"@type":"Article","headline":"Our rösti"},{"@type":"Recipe","name":"Fixture rösti","totalTime":"PT45M","recipeYield":"2 servings","recipeCuisine":"Swiss","recipeIngredient":["800 g waxy potatoes, peeled","2 tbsp butter","1 pinch salt"]}]}))),
            "qa.example"=>("text/html",format!("<script type='application/ld+json'>{}</script>",json!({"@type":"QAPage","mainEntity":{"@type":"Question","name":"Fixture question?","text":"Why is the fixture blue?","answerCount":2,"upvoteCount":5,"author":{"@type":"Person","name":"Fixture asker"},"acceptedAnswer":{"@type":"Answer","url":"https://qa.example/e2e/q/1#a2","text":"Because."}}}))),
            "shop.example"=>("text/html",format!("<script type='application/ld+json'>{}</script>",json!({"@type":"Product","name":"Nutella 400 g","gtin13":"3017620422003","brand":{"@type":"Brand","name":"Ferrero"},"offers":{"@type":"Offer","price":"4.95","priceCurrency":"CHF","availability":"https://schema.org/InStock"}}))),
            "jobs.example" if u.path()=="/e2e/remote"=>("text/html",format!("<script type='application/ld+json'>{}</script>",json!({"@type":"JobPosting","title":"Fixture remote engineer","datePosted":(self.start-chrono::Duration::days(14)).format("%Y-%m-%d").to_string(),"hiringOrganization":{"@type":"Organization","name":"Fixture Co"},"jobLocationType":"TELECOMMUTE","applicantLocationRequirements":{"@type":"AdministrativeArea","name":"Europe"}}))),
            "jobs.example"=>("text/html",format!("<script type='application/ld+json'>{}</script>",json!({"@type":"JobPosting","title":"Fixture barista","datePosted":(self.start-chrono::Duration::days(14)).format("%Y-%m-%d").to_string(),"employmentType":"PART_TIME","hiringOrganization":{"@type":"Organization","name":"Fixture Co"},"jobLocation":{"@type":"Place","address":{"@type":"PostalAddress","streetAddress":"1 Main Street","addressLocality":"Zurich","postalCode":"8000","addressCountry":"CH"}}}))),
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
            charset: None,
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
/// The contact the mock run's clients send: it reads only this stack's own answers.
const MOCK_CONTACT: &str = "e2e@example.invalid";

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
            skipped: Default::default(),
        }),
    )
}
/// Nexus's batch view by `uris` (`/v1`), answered in request order. A tag's `tag_uri` is the
/// address of the viewer's tag file, when the viewer (`viewer_id`) is the run's author.
async fn hydrate(State(f): State<Fixture>, Json(q): Json<Value>) -> Json<Value> {
    let b = f.boundary.lock().unwrap();
    let uris = q["uris"].as_array().cloned().unwrap_or_default();
    let mine = q["viewer_id"] == AUTHOR;
    Json(Value::Array(uris.iter().filter_map(|v| {
        let (id, uri, _) = identity(v.as_str()?).ok()?;
        let lines:Vec<_>=b.lines.iter().filter(|l|l.uri==uri && matches!(l.op,Op::Put) && b.tags.contains_key(&l.object_path)).collect();
        lines.first().map(|line|json!({"details":{"id":id,"uri":line.uri},"taggers_count":1,"tags":lines.iter().map(|l|json!({"label":l.label,"taggers":[AUTHOR],"taggers_count":1,"tag_uri":mine.then(|| format!("pubky://{AUTHOR}{}", l.object_path))})).collect::<Vec<_>>()}))
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
    let recipes = args.get(1).is_some_and(|s| s == "--live-recipes");
    // A cancelled run stops, and its database is still dropped: a terminal's SIGINT in every
    // mode, and the runner's SIGTERM for a recipe run (the other modes shut down on SIGTERM by
    // themselves). Listening from before the database exists leaves no moment it would leak.
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = if recipes {
        Some(signal(SignalKind::terminate())?)
    } else {
        None
    };
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
    let work = async {
        if recipes {
            run_recipes(pool.clone()).await
        } else if let Some(samples) = live {
            run_live(pool.clone(), samples).await
        } else {
            run(pool.clone(), fixture).await
        }
    };
    let terminated = async {
        match &mut terminate {
            Some(terminate) => {
                terminate.recv().await;
            }
            None => std::future::pending().await,
        }
    };
    let result = tokio::select! {
        result = work => result,
        _ = interrupt.recv() => Err(anyhow::anyhow!("interrupted")),
        _ = terminated => Err(anyhow::anyhow!("interrupted")),
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
    cfg.osm.nominatim_url = base.clone().into();
    cfg.osm.requests_per_second = 10.;
    cfg.canary.enabled = false;
    // Services that need a contact (SEC EDGAR) wait without one.
    cfg.egress.contact = MOCK_CONTACT.into();
    cfg.osm.contact = MOCK_CONTACT.into();
    cfg.pubky.enabled = false;
    cfg.api.requests_per_minute = 10000;
    // The harness tags as one author: one shared tagger relates two samples.
    cfg.search.related_min_shared = 1;
    // A source that reads with the operator's token (CourtListener) has one here: its recorded
    // answers (`Source::e2e`) answer only a request that carries it.
    for source in pinakes_core::sources::REGISTRY
        .iter()
        .filter(|s| s.takes_token() && s.e2e().is_some())
    {
        cfg.sources.0.insert(
            source.name().into(),
            pinakes_core::sources::Switch {
                enabled: true,
                token: Some("e2e-token".into()),
                ..Default::default()
            },
        );
    }
    pinakes_core::configure_sources(&cfg);
    pinakes_core::also_on::configure(&pool, &cfg).await?;
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
            .route("/v1/resources/changes", get(feed))
            .route("/v1/stream/resources/by_ids", post(hydrate))
            // The run's author follows and befriends itself: a reach or boost set of one.
            .route(
                "/v1/user/{id}/following",
                get(|| async { Json(vec![AUTHOR]) }),
            )
            .route(
                "/v1/user/{id}/friends",
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
    let nexus = Nexus::new(&base, None)?;
    let url = base.clone();
    let polling = tokio::spawn(async move {
        let mut resets = ingest::Resets::new(Duration::from_secs(cfg.poll_secs));
        loop {
            if let Err(e) =
                ingest::poll_once(&pool, &url, &nexus, &cfg.ingest, &metrics, &mut resets).await
            {
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

/// The live settings and resolvers, as the daemon builds them.
struct Live {
    cfg: Arc<Config>,
    egress: Egress,
    resolvers: Vec<Arc<dyn Resolver>>,
}

fn live(pool: &sqlx::PgPool) -> Result<Live> {
    use pinakes_core::event::{
        adapters::service::PlatformResolver,
        service::{CalendarResolver, EventService},
    };
    use pinakes_core::resolver::{
        cards::CardResolver, geo::GeoResolver, osm::OsmResolver, pubky::PubkyResolver,
        web::WebResolver,
    };
    // Settings and secrets come from the environment as the daemon reads them (the TIDAL
    // credentials, `PINAKES__…`); there is no file.
    let mut cfg = Config::load(None)?;
    cfg.canary.enabled = false;
    // A live run names its operator to every service it reads (`PINAKES__HTTP__CONTACT`, which
    // `Config::load` gave every client): there is no default.
    anyhow::ensure!(
        !cfg.egress.contact.is_empty(),
        "a live run needs the operator's contact: set PINAKES__HTTP__CONTACT"
    );
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
    Ok(Live {
        cfg,
        egress,
        resolvers,
    })
}

/// Claims `id`'s job when it is due and runs it through the production path (batched
/// completion, leases), by `resolver`. A fetch can show a sample to be a spelling of another URI
/// (a Bluesky handle's post under its DID): the sample becomes that root's alias, and its card
/// is the root's, so the root is fetched in its place, by the root's own resolver. Whether a job
/// ran.
async fn run_due(pool: &sqlx::PgPool, live: &Live, id: &str, resolver: &Arc<dyn Resolver>) -> bool {
    let job=sqlx::query_as::<_,queue::Job>("UPDATE resource SET lease_until=now()+interval '60 seconds',generation=generation+1 WHERE id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1) AND NOT dead_letter AND NOT final AND next_attempt_at<=now() AND (lease_until IS NULL OR lease_until<now()) RETURNING id,uri,attempts,generation").bind(id).fetch_optional(pool).await;
    let resolver = match &job {
        Ok(Some(job)) if job.id != id => {
            let Some(root) = pinakes_core::resolver::classify(&job.uri)
                .and_then(|kind| live.resolvers.iter().find(|r| r.name() == kind))
            else {
                eprintln!("live sample root: no resolver for {}", job.uri);
                return false;
            };
            root
        }
        _ => resolver,
    };
    match job {
        Ok(Some(job)) => {
            // The production path: the workers finish every job through a batch.
            if let Err(e) = queue::run_batch(
                pool,
                resolver.as_ref(),
                &live.cfg.queue,
                std::slice::from_ref(&job),
            )
            .await
            {
                eprintln!("live sample completion: {e}");
            }
            true
        }
        Err(e) => {
            eprintln!("live sample claim: {e}");
            false
        }
        _ => false,
    }
}

/// The public `/v1/resolve` examples that promise a card (`data/api-recipes.json` and the
/// sources' own), each fetched once through the live resolvers, one at a time, as the endpoint
/// would: from its root when the URI is a spelling. Prints one JSON line with each recipe's
/// card: its status, type and the type's ancestors, and the last error's class;
/// `judgeRecipes` (`e2e/harness.mjs`) decides which pass.
async fn run_recipes(pool: sqlx::PgPool) -> Result<()> {
    pinakes_core::migrate(&pool).await?;
    let _instance = pinakes_core::locks::own_database(&pool).await?;
    let live = live(&pool)?;
    let mut results = vec![];
    for recipe in pinakes_core::recipes::recipes() {
        let (Some(want), Some(uri)) = (
            &recipe.card_type,
            recipe.body.as_ref().and_then(|b| b["uri"].as_str()),
        ) else {
            continue;
        };
        if recipe.path != "/v1/resolve" || recipe.expect != "card" {
            continue;
        }
        let (id, uri, _) = identity(uri)?;
        let target = pinakes_core::alias::root(&uri).map_or_else(|| uri.clone(), |a| a.root);
        // A recipe the endpoint would answer with a 400 (no resolver, or one switched off) is
        // refused here too, with the endpoint's reason.
        let resolver = match pinakes_core::resolver::admit(&target, &live.cfg) {
            Ok(kind) => live
                .resolvers
                .iter()
                .find(|r| r.name() == kind)
                .expect("live() builds every route"),
            Err(reason) => {
                results.push(json!({"name":recipe.name,"uri":uri,"want":want,"status":"refused","error":reason}));
                continue;
            }
        };
        pinakes_core::store::ensure_resource(&pool, &uri).await?;
        let id = pinakes_core::alias::resolve_id(&pool, &id).await?;
        // A fetch that leaves the card pending with no error (it showed the URI to be a spelling,
        // whose root comes next) runs again; a failure, or a job put off, is the answer.
        for _ in 0..3 {
            let ran = run_due(&pool, &live, &id, resolver).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            let again: bool = sqlx::query_scalar("SELECT status='pending' AND last_error IS NULL FROM resource WHERE id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)")
                .bind(&id)
                .fetch_one(&pool)
                .await?;
            if !ran || !again {
                break;
            }
        }
        // A calendar waiting for its members (a Nostr calendar's events) has them fetched, then
        // is fetched again at once. A member's first missing answer waits for the fetch that
        // confirms it, which a second pass makes at once, so the calendar can leave it out.
        let waiting: bool = sqlx::query_scalar("SELECT status='pending' AND last_error='calendar_members_pending' FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await?;
        if waiting {
            let members: Vec<(String, String)> = sqlx::query_as("SELECT r.id,r.uri FROM resource_derivation l JOIN resource r ON r.id=l.child_id WHERE l.parent_id=$1 ORDER BY r.id")
                .bind(&id)
                .fetch_all(&pool)
                .await?;
            for confirm in [false, true] {
                for (member, member_uri) in &members {
                    let Some(member_resolver) =
                        pinakes_core::resolver::classify_config(member_uri, &live.cfg)
                            .and_then(|kind| live.resolvers.iter().find(|r| r.name() == kind))
                    else {
                        continue;
                    };
                    if confirm {
                        sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1 AND status='pending' AND last_error='missing_unconfirmed'")
                            .bind(member)
                            .execute(&pool)
                            .await?;
                    }
                    if run_due(&pool, &live, member, member_resolver).await {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            sqlx::query("UPDATE resource SET next_attempt_at=now() WHERE id=$1")
                .bind(&id)
                .execute(&pool)
                .await?;
            run_due(&pool, &live, &id, resolver).await;
        }
        let (status, kind, path, error): (String, Option<String>, Option<Vec<String>>, Option<String>) = sqlx::query_as("SELECT d.status,d.type,d.type_path,r.last_error FROM resource r JOIN resource_document d ON d.id=r.id WHERE r.id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)")
            .bind(&id)
            .fetch_one(&pool)
            .await?;
        results.push(json!({"name":recipe.name,"uri":uri,"want":want,"status":status,"type":kind,"type_path":path,"error":error}));
    }
    println!("{}", json!({ "recipes": results }));
    Ok(())
}

// A scoped live driver keeps a private copy of the real Nexus feed, but claims only
// the selected sample IDs. It uses the same leases, completion fences and resolvers.
// No canaries, refresh sweep or unrelated backlog is sent to public upstreams.
async fn run_live(pool: sqlx::PgPool, samples: Value) -> Result<()> {
    pinakes_core::migrate(&pool).await?;
    let _instance = pinakes_core::locks::own_database(&pool).await?;
    let live = live(&pool)?;
    let (cfg, egress) = (live.cfg.clone(), live.egress.clone());
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
        let resolver = live
            .resolvers
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
    pinakes_core::also_on::configure(&pool, &cfg).await?;
    let state = AppState::new(pool.clone(), cfg.as_ref().clone())?;
    let metrics = state.metrics.clone();
    let app = api::router(state);
    let nexus = Nexus::configured(&cfg)?;
    let ready = json!({"pinakes":base,"nexus":cfg.nexus_url,"mock":false,"skipped":skipped});
    let also_on = pinakes_core::also_on::Context {
        egress: egress.clone(),
        config: cfg.clone(),
    };
    let polling = tokio::spawn(async move {
        let mut resets = ingest::Resets::new(Duration::from_secs(cfg.poll_secs));
        loop {
            if let Err(e) = ingest::poll_once(
                &pool,
                &cfg.nexus_url,
                &nexus,
                &cfg.ingest,
                &metrics,
                &mut resets,
            )
            .await
            {
                eprintln!("live ingestion: {e}");
            }
            for (id, resolver) in &selected {
                if run_due(&pool, &live, id, resolver).await {
                    tokio::time::sleep(Duration::from_secs(1)).await;
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
         PINAKES_TIDAL_CLIENT_ID and PINAKES_TIDAL_CLIENT_SECRET; for spotify, \
         PINAKES_SPOTIFY_CLIENT_ID and PINAKES_SPOTIFY_CLIENT_SECRET",
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
