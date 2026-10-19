//! Isolated e2e stack with real migrations, ingestion, resolver parsers, storage and API.
//! Default mode mocks external boundaries with no public network; --live scopes real fetches
//! to the (at most twelve) sample IDs.
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
        let samples = json!([
            {"kind":"osm","uri":"https://www.openstreetmap.org/node/501","type":"Place"},
            {"kind":"geo","uri":"geo:47,8","type":"Place"},
            {"kind":"osm_alias","uri":"https://www.openstreetmap.org/node/501/history","type":"Place","group_with":"osm"},
            {"kind":"osm_short","uri":"https://osm.org/go/0GAjIv8h?node=501","type":"Place","group_with":"osm"},
            {"kind":"marker","uri":"https://www.openstreetmap.org/?mlat=47.0&mlon=8.0","type":"Place"},
            {"kind":"event","uri":"https://events.example/e2e","type":"Event"},
            {"kind":"ics","uri":"https://calendar.example/e2e.ics","type":"DataFeed","expectedOccurrences":2},
            {"kind":"naddr","uri":naddr,"type":"Event"},
            {"kind":"doi","uri":"doi:10.1234/pinakes","type":"ScholarlyArticle"},
            {"kind":"doi_page","uri":"https://publisher.example/articles/pinakes","type":"CreativeWork","group_with":"doi"},
            {"kind":"isbn","uri":"isbn:9780140328721","type":"Book"},
            {"kind":"isbn_twin","uri":"isbn:0140328726","type":"Book","group_with":"isbn"},
            {"kind":"isbn_page","uri":"https://openlibrary.org/books/OL7353617M","type":"CreativeWork","group_with":"isbn"},
            {"kind":"media","uri":"https://www.youtube.com/watch?v=fixture","type":"VideoObject"},
            {"kind":"web","uri":"https://web.example/e2e","type":"WebPage"},
            {"kind":"federal_register","uri":"fr:2026-18291","type":"Legislation"},
            {"kind":"federal_register_page","uri":"https://www.federalregister.gov/d/2026-18291","type":"Legislation","group_with":"federal_register"},
            {"kind":"sec","uri":"https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959/0001104659-26-000959-index.htm","type":"Report"},
            {"kind":"ecfr","uri":"https://www.ecfr.gov/current/title-17/section-240.10b-5","type":"Legislation"},
            {"kind":"gleif","uri":"lei:KY37LUS27QQX7BB93L28","type":"Organization"},
            {"kind":"uk_parliament_bills","uri":"https://bills.parliament.uk/bills/3737","type":"Legislation"},
            {"kind":"swiss_parliament","uri":"https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista/geschaeft?AffairId=20223765","type":"CreativeWork"},
            {"kind":"courtlistener","uri":"https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/","type":"CreativeWork"},
            {"kind":"legislation_gov_uk","uri":"https://www.legislation.gov.uk/ukpga/2018/12/contents","type":"Legislation"},
            {"kind":"eur_lex","uri":"https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX:32016R0679","type":"Legislation"},
            {"kind":"fedlex","uri":"https://www.fedlex.admin.ch/eli/cc/1999/404/de","type":"Legislation"},
            {"kind":"zefix","uri":"https://register.ld.admin.ch/zefix/company/126286","type":"Organization"},
            {"kind":"govinfo","uri":"https://www.govinfo.gov/app/details/PLAW-117publ58","type":"Legislation"},
            {"kind":"stacker_news","uri":"https://stacker.news/items/1596515","type":"DiscussionForumPosting"},
            {"kind":"hacker_news","uri":"https://news.ycombinator.com/item?id=8863","type":"DiscussionForumPosting"},
            {"kind":"lobsters","uri":"https://lobste.rs/s/wtthju/a5_v1_0","type":"DiscussionForumPosting"},
            {"kind":"bluesky","uri":"https://bsky.app/profile/did:plc:z72i7hdynmk6r22z27h6tvur/post/3mxzez2m7wk2r","type":"SocialMediaPosting"},
            {"kind":"mastodon","uri":"https://mastodon.social/@Gargron/1","type":"SocialMediaPosting"},
            {"kind":"discourse","uri":"https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304","type":"DiscussionForumPosting"},
            {"kind":"forgejo","uri":"https://codeberg.org/forgejo/forgejo","type":"SoftwareSourceCode"}
        ]);
        Ok(Self {
            boundary: Arc::default(),
            samples,
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
                | "api.gleif.org"
                | "bills-api.parliament.uk"
                | "ws.parlament.ch"
                | "www.courtlistener.com"
                | "www.legislation.gov.uk"
                | "publications.europa.eu"
                | "fedlex.data.admin.ch"
                | "ld.admin.ch"
                | "www.govinfo.gov"
                | "stacker.news"
                | "hacker-news.firebaseio.com"
                | "lobste.rs"
                | "public.api.bsky.app"
                | "mastodon.social"
                | "delvingbitcoin.org"
                | "codeberg.org"
                | "web.example"
                | "www.federalregister.gov"
                | "www.sec.gov"
                | "www.ecfr.gov"
        ) {
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
            "api.gleif.org" if u.path()=="/api/v1/lei-records/KY37LUS27QQX7BB93L28"=>("application/vnd.api+json",include_str!("../tests/fixtures/sources/gleif/lei_KY37LUS27QQX7BB93L28_nestle.json").into()),
            "bills-api.parliament.uk" if u.path()=="/api/v1/Bills/3737"=>("application/json",include_str!("../tests/fixtures/sources/uk_parliament_bills/bill_3737_employment_rights_act_2025.json").into()),
            "ws.parlament.ch" if u.path()=="/odata.svc/Business(ID=20223765,Language='DE')"=>("application/json",include_str!("../tests/fixtures/sources/swiss_parliament/business_20223765_DE_select.json").into()),
            "www.courtlistener.com" if u.path()=="/api/rest/v4/search/"=>("application/json",include_str!("../tests/fixtures/sources/courtlistener/search_o_cluster_id_2812209_obergefell.json").into()),
            "www.legislation.gov.uk" if u.path()=="/ukpga/2018/12/introduction/data.xml"=>("application/xml",include_str!("../tests/fixtures/sources/legislation_gov_uk/ukpga_2018_12_introduction.xml").into()),
            "publications.europa.eu" if u.path()=="/webapi/rdf/sparql"=>("application/sparql-results+json",include_str!("../tests/fixtures/sources/eur_lex/celex_32016R0679.json").into()),
            "fedlex.data.admin.ch" if u.path()=="/sparqlendpoint"=>("application/sparql-results+json",include_str!("../tests/fixtures/sources/fedlex/eli_cc_1999_404.json").into()),
            "ld.admin.ch" if u.path()=="/query"=>("application/sparql-results+json",include_str!("../tests/fixtures/sources/zefix/sparql_company_126286_spo.json").into()),
            "www.govinfo.gov" if u.path()=="/metadata/pkg/PLAW-117publ58/mods.xml"=>("application/xml",gunzip(include_bytes!("../tests/fixtures/sources/govinfo/mods_pkg_PLAW-117publ58.xml.gz"))),
            "stacker.news" if u.path()=="/api/graphql"=>("application/json",include_str!("../tests/fixtures/sources/stacker_news/sn-item-1596515.json").into()),
            "hacker-news.firebaseio.com" if u.path()=="/v0/item/8863.json"=>("application/json",include_str!("../tests/fixtures/sources/hacker_news/hn-item-8863.json").into()),
            "lobste.rs" if u.path()=="/s/wtthju.json"=>("application/json",include_str!("../tests/fixtures/sources/lobsters/lobsters-wtthju.json").into()),
            "public.api.bsky.app" if u.path()=="/xrpc/app.bsky.feed.getPostThread"=>("application/json",include_str!("../tests/fixtures/sources/bluesky/bsky-thread-3mxzez2m7wk2r.json").into()),
            "mastodon.social" if u.path()=="/api/v1/statuses/1"=>("application/json",include_str!("../tests/fixtures/sources/mastodon/status-1.json").into()),
            "delvingbitcoin.org" if u.path()=="/t/304.json"=>("application/json",include_str!("../tests/fixtures/sources/discourse/topic-304.json").into()),
            "codeberg.org" if u.path()=="/api/v1/repos/forgejo/forgejo"=>("application/json",include_str!("../tests/fixtures/sources/forgejo/repo-forgejo.json").into()),
            "web.example"=>("text/html","<title>Fixture page</title>".into()),
            "www.ecfr.gov" if u.path()=="/api/versioner/v1/ancestry/current/title-17.json"=>("application/json",include_str!("../tests/fixtures/sources/ecfr/ancestry_current_title-17_section-240.10b-5.json").into()),
            "www.sec.gov" if u.path()=="/Archives/edgar/data/2103612/000110465926000959/0001104659-26-000959.hdr.sgml"=>("text/sgml",include_str!("../tests/fixtures/sources/sec/sec-hdr-ms-s1.sgml").into()),
            "www.federalregister.gov" if u.path()=="/api/v1/documents/2026-18291.json"=>("application/json",include_str!("../tests/fixtures/sources/federal_register/fr-2026-18291.json").into()),
            _=>bail!("no fixture for upstream host {}",u.host_str().unwrap_or("")),
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
#[tokio::main]
async fn main() -> Result<()> {
    let fixture = Fixture::new()?;
    let args: Vec<_> = std::env::args().collect();
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
    cfg.egress.contact = "info@synonym.to".into();
    cfg.pubky.enabled = false;
    cfg.api.requests_per_minute = 10000;
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
            .with_state(f.clone()),
    );
    let (shutdown, rx) = tokio::sync::watch::channel(false);
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
            wake.notify_waiters();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    println!("{}", json!({"pinakes":base,"nexus":base,"mock":true}));
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
    let mut cfg = Config::default();
    cfg.canary.enabled = false;
    // Services that need a contact (SEC EDGAR) wait without one.
    cfg.egress.contact = "info@synonym.to".into();
    cfg.api.requests_per_minute = 10000;
    cfg.nexus_url = std::env::var("PINAKES_E2E_NEXUS").unwrap_or_else(|_| cfg.nexus_url.clone());
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
    let mut selected = vec![];
    for sample in samples
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("samples must be an array"))?
    {
        let (id, uri, _) = identity(
            sample["uri"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("sample URI"))?,
        )?;
        let kind = pinakes_core::resolver::classify(&uri)
            .ok_or_else(|| anyhow::anyhow!("unclassified sample"))?;
        let resolver = resolvers
            .iter()
            .find(|r| r.name() == kind)
            .ok_or_else(|| anyhow::anyhow!("missing sample resolver"))?
            .clone();
        selected.push((id, resolver));
    }
    // The scope stays small and explicit: only listed samples are ever fetched.
    ensure!(
        (1..=12).contains(&selected.len()),
        "live driver requires one to twelve sample resources"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let state = AppState::new(pool.clone(), cfg.as_ref().clone())?;
    let metrics = state.metrics.clone();
    let app = api::router(state);
    let nexus = Nexus::new(&cfg.nexus_url)?;
    let ready = json!({"pinakes":base,"nexus":cfg.nexus_url,"mock":false});
    let polling = tokio::spawn(async move {
        loop {
            if let Err(e) =
                ingest::poll_once(&pool, &cfg.nexus_url, &nexus, &cfg.ingest, &metrics).await
            {
                eprintln!("live ingestion: {e}");
            }
            for (id, resolver) in &selected {
                let job=sqlx::query_as::<_,queue::Job>("UPDATE resource SET lease_until=now()+interval '60 seconds',generation=generation+1 WHERE id=$1 AND NOT dead_letter AND NOT final AND next_attempt_at<=now() AND (lease_until IS NULL OR lease_until<now()) RETURNING id,uri,attempts,generation").bind(id).fetch_optional(&pool).await;
                match job {
                    Ok(Some(job)) => {
                        if let Err(e) =
                            queue::run_job(&pool, resolver.as_ref(), &cfg.queue, &job).await
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
