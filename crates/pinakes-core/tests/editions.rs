//! One link per service, by release: the `editions` source places a recording's or an album's
//! links on MusicBrainz releases (captured MusicBrainz, iTunes, Spotify, TIDAL and Deezer
//! answers), and `also_on()` shows, of each service, the link on the card's own release, else on
//! the original edition, never another page of the card's own service, on every card a read
//! included.
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    also_on::{self, editions::Editions, Edition, Found, Link, Source, SourceOptions, Subject},
    api::{self, AppState},
    config::{Config, EgressConfig},
    model::ResourceMeta,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
mod common;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hubs");
const GET_LUCKY: &str = "https://open.spotify.com/track/69kOkLUCkxIZYexIgSG8rq";
const RECORDING: &str = "833f00e1-781f-4edd-90e4-e52712618862";
/// Random Access Memories, 2013: the worldwide digital release, and Apple's Digital Master.
const RAM: &str = "5000a285-b67e-4cfc-b54b-2b98f1810d2e";
const RAM_APPLE: &str = "36e2aede-346d-4931-8565-78d810d167c7";

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

/// A request's URL as the fixtures are found by: host, path and its query in any order.
fn key(url: &url::Url) -> String {
    let mut query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    query.sort();
    format!(
        "{}{}?{query:?}",
        url.host_str().unwrap_or_default(),
        url.path()
    )
}

/// Every request answered from the captured fixtures (`provenance.json`) by its URL, TIDAL's
/// token request with a token, anything else with 404; each request noted.
struct Captured {
    answers: BTreeMap<String, (u16, String, Vec<u8>)>,
    seen: Mutex<Vec<String>>,
}
impl Captured {
    fn new() -> Arc<Self> {
        let provenance: Value =
            serde_json::from_slice(&std::fs::read(format!("{DIR}/provenance.json")).unwrap())
                .unwrap();
        let answers = provenance
            .as_object()
            .unwrap()
            .values()
            .filter_map(|e| {
                let url = url::Url::parse(e["url"].as_str()?).ok()?;
                let body = std::fs::read(format!("{DIR}/{}", e["file"].as_str()?)).ok()?;
                Some((
                    key(&url),
                    (
                        e["status"].as_u64().unwrap_or(200) as u16,
                        e["content_type"]
                            .as_str()
                            .unwrap_or("application/json")
                            .into(),
                        body,
                    ),
                ))
            })
            .collect();
        Arc::new(Self {
            answers,
            seen: Mutex::default(),
        })
    }
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}
#[async_trait::async_trait]
impl Transport for Captured {
    async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, a, &FetchOptions::default()).await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen.lock().unwrap().push(url.to_string());
        let (status, content_type, body) = if url.host_str() == Some("auth.tidal.com") {
            (
                200,
                "application/json".to_owned(),
                br#"{"access_token":"recorded-token","token_type":"Bearer","expires_in":14400}"#
                    .to_vec(),
            )
        } else {
            self.answers.get(&key(url)).cloned().unwrap_or((
                404,
                "application/json".into(),
                b"{}".to_vec(),
            ))
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status,
            retry_after_secs: 60,
            location: None,
            content_type,
            body,
        })
    }
}

/// TIDAL with a pair (each test its own: tokens are kept per pair), Deezer on or off.
fn config(pair: &str, deezer: bool) -> Config {
    let mut config = Config::default();
    config.sections.set(
        "tidal",
        serde_json::json!({"client_id": pair, "client_secret": format!("{pair}-secret")}),
    );
    config.also_on.sources.insert(
        "deezer".into(),
        SourceOptions {
            enabled: Some(deezer),
            ..Default::default()
        },
    );
    config
}
fn context(transport: Arc<Captured>, config: Config) -> also_on::Context {
    also_on::Context {
        egress: Egress::with_backends(
            EgressConfig {
                host_requests_per_second: 1000.0,
                ..Default::default()
            },
            Arc::new(PublicDns),
            transport,
        ),
        config: Arc::new(config),
    }
}
fn subject(uri: &str, keys: &[&str], candidates: &[&str]) -> Subject {
    Subject {
        id: pinakes_core::model::identity(uri).unwrap().0,
        uri: uri.into(),
        status: "ok".into(),
        type_name: None,
        doc: None,
        keys: keys
            .iter()
            .map(|k| (k.to_string(), "musicbrainz".to_owned()))
            .collect(),
        first_seen_at: None,
        candidates: candidates.iter().map(|c| c.to_string()).collect(),
    }
}
/// The links MusicBrainz relates Get Lucky's recording to (captured), a Spotify one included.
const GET_LUCKY_LINKS: &[&str] = &[
    "https://music.apple.com/us/song/1673536443",
    "https://music.apple.com/us/song/617154366",
    "https://music.apple.com/gb/song/1771867453",
    "https://tidal.com/track/293301151",
    "https://tidal.com/track/20115564",
    "https://tidal.com/track/211822900",
    "https://www.deezer.com/track/2271581007",
    "https://www.deezer.com/track/67238735",
    "https://open.qobuz.com/track/209446467",
    "https://open.qobuz.com/track/9140031",
    "https://open.spotify.com/track/118Opfz7qv2KA1g3Pmyk9T",
    "https://www.youtube.com/watch?v=5NV6Rdv1a3I",
];
fn placed(found: &Found) -> Vec<(String, String, i16)> {
    let mut placed: Vec<(String, String, i16)> = found
        .editions
        .iter()
        .map(|e| (e.uri.clone(), e.release.clone(), e.rank))
        .collect();
    placed.sort();
    placed
}

/// Get Lucky's Spotify card is on Random Access Memories of 2013 (its page names the album,
/// which MusicBrainz relates to that release): TIDAL's link on that album is placed on it (rank
/// 0), Apple Music's on the original edition's Digital Master (rank 1), never the 10th
/// anniversary edition's or a soundtrack's. Deezer is off, so nothing asks its API and its links
/// stay unplaced, as Qobuz's and YouTube's do (no track list here).
#[tokio::test]
async fn get_luckys_links_are_placed_on_random_access_memories() {
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-off", false));
    let subject = subject(
        GET_LUCKY,
        &[&format!("mb:recording:{RECORDING}")],
        GET_LUCKY_LINKS,
    );
    assert!(Editions.applies(&subject));
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    assert_eq!(
        placed(&found),
        [
            (
                "https://music.apple.com/us/song/617154366".to_owned(),
                RAM_APPLE.to_owned(),
                1
            ),
            (
                "https://tidal.com/track/20115564".to_owned(),
                RAM.to_owned(),
                0
            ),
        ],
        "{:?}",
        transport.seen()
    );
    assert!(found.retry_after.is_none());
    assert!(
        transport
            .seen()
            .iter()
            .all(|u| !u.contains("api.deezer.com")),
        "{:?}",
        transport.seen()
    );
}

/// With Deezer's source on, its link on that album is placed too.
#[tokio::test]
async fn with_deezer_on_its_link_on_the_album_is_placed() {
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-on", true));
    let subject = subject(
        GET_LUCKY,
        &[&format!("mb:recording:{RECORDING}")],
        GET_LUCKY_LINKS,
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    assert!(
        placed(&found).contains(&(
            "https://www.deezer.com/track/67238735".to_owned(),
            RAM.to_owned(),
            0
        )),
        "{:?}",
        transport.seen()
    );
    assert!(!placed(&found)
        .iter()
        .any(|(uri, ..)| uri.ends_with("/2271581007")));
}

/// Kind of Blue's Spotify album is a page of its 2011 worldwide release: the other services'
/// pages of that release are placed on it; another Deezer album is not.
#[tokio::test]
async fn kind_of_blues_album_pages_are_placed_on_its_own_release() {
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-album", false));
    let subject = subject(
        "https://open.spotify.com/album/1weenld61qoidwYuZ1GESA",
        &[
            "mb:release:bee5e0cd-1767-4a8e-9578-6455e87ba60b",
            "mb:release:4c8af71f-5746-4981-92d7-0d6c64db1155",
        ],
        &[
            "https://www.deezer.com/album/1441464",
            "https://www.deezer.com/album/7824595",
            "https://tidal.com/album/655910",
            "https://music.apple.com/us/album/268443092",
            "https://open.qobuz.com/album/5099749522428",
        ],
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    let release = "4c8af71f-5746-4981-92d7-0d6c64db1155";
    assert_eq!(
        placed(&found),
        [
            (
                "https://music.apple.com/us/album/268443092".to_owned(),
                release.to_owned(),
                0
            ),
            (
                "https://open.qobuz.com/album/5099749522428".to_owned(),
                release.to_owned(),
                0
            ),
            (
                "https://tidal.com/album/655910".to_owned(),
                release.to_owned(),
                0
            ),
            (
                "https://www.deezer.com/album/1441464".to_owned(),
                release.to_owned(),
                0
            ),
        ],
        "{:?}",
        transport.seen()
    );
}

/// A card with no link to place asks nothing.
#[tokio::test]
async fn nothing_to_place_asks_nothing() {
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-none", false));
    let subject = subject(
        GET_LUCKY,
        &[&format!("mb:recording:{RECORDING}")],
        &["https://open.spotify.com/track/118Opfz7qv2KA1g3Pmyk9T"],
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    assert!(found.editions.is_empty());
    assert!(transport.seen().is_empty(), "{:?}", transport.seen());
}

async fn save(pool: &PgPool, uri: &str, kind: &str, service: Option<&str>) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type(kind);
    doc.name = Some("Get Lucky".into());
    doc.service = service.map(str::to_owned);
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    id
}
fn found_links(links: &[(&str, &str)]) -> Found {
    Found {
        links: links
            .iter()
            .map(|(uri, site)| Link::new(*uri, *site, "musicbrainz"))
            .collect(),
        ..Default::default()
    }
}
fn found_editions(editions: &[(&str, &str, i16)]) -> Found {
    Found {
        editions: editions
            .iter()
            .map(|(uri, release, rank)| Edition {
                uri: (*uri).into(),
                release: (*release).into(),
                rank: *rank,
            })
            .collect(),
        ..Default::default()
    }
}
/// The ID at the end of each link (`…/617154366`, `…?v=5NV6Rdv1a3I`), sorted.
fn tails(uris: Vec<String>) -> Vec<String> {
    let mut tails: Vec<String> = uris
        .iter()
        .map(|u| u.rsplit(['/', '=']).next().unwrap_or_default().to_owned())
        .collect();
    tails.sort();
    tails
}
async fn also_on(pool: &PgPool, id: &str, per_site: bool) -> Vec<String> {
    let v: Value = sqlx::query_scalar("SELECT also_on($1,$1,256,true,$2)")
        .bind(id)
        .bind(per_site)
        .fetch_one(pool)
        .await
        .unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| e["uri"].as_str().unwrap().to_owned())
        .collect()
}
async fn read(pool: &PgPool, uri: &str) -> Vec<String> {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let path = format!("/v1/resources/by-uri?uri={}", urlencoding(uri));
    let mut request = Request::builder().uri(&path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    common::check_response("GET", &path, 200, &v);
    v["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["uri"].as_str().unwrap().to_owned())
        .collect()
}
fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Get Lucky's card as stable had it (2026-11-05): fifteen derived links, three of Apple Music,
/// TIDAL and its own Spotify. Placed, each service shows one: the link on the card's own release
/// (TIDAL, Deezer), else on the original edition (Apple), else the shorter one (Qobuz); never
/// another Spotify page. A read shows the same as a search card.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_card_shows_each_services_link_on_its_own_release_or_the_original(pool: PgPool) {
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    let cfg = Config::default().also_on;
    let links = [
        ("https://music.apple.com/us/song/1673536443", "Apple Music"),
        ("https://music.apple.com/us/song/617154366", "Apple Music"),
        ("https://music.apple.com/gb/song/1771867453", "Apple Music"),
        ("https://tidal.com/track/293301151", "TIDAL"),
        ("https://tidal.com/track/20115564", "TIDAL"),
        ("https://tidal.com/track/211822900", "TIDAL"),
        ("https://www.deezer.com/track/2271581007", "Deezer"),
        ("https://www.deezer.com/track/67238735", "Deezer"),
        ("https://open.qobuz.com/track/209446467", "Qobuz"),
        ("https://open.qobuz.com/track/9140031", "Qobuz"),
        (
            "https://open.spotify.com/track/118Opfz7qv2KA1g3Pmyk9T",
            "Spotify",
        ),
        ("https://www.youtube.com/watch?v=5NV6Rdv1a3I", "YouTube"),
    ];
    also_on::record(
        &pool,
        &card,
        "musicbrainz",
        found_links(&links),
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    also_on::record(
        &pool,
        &card,
        "editions",
        found_editions(&[
            ("https://music.apple.com/us/song/617154366", RAM_APPLE, 1),
            ("https://tidal.com/track/20115564", RAM, 0),
            ("https://tidal.com/track/211822900", RAM, 3),
            ("https://www.deezer.com/track/67238735", RAM, 0),
        ]),
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    let expected = [
        "20115564",
        "5NV6Rdv1a3I",
        "617154366",
        "67238735",
        "9140031",
    ];
    assert_eq!(tails(also_on(&pool, &card, true).await), expected);
    assert_eq!(tails(read(&pool, GET_LUCKY).await), expected);
}

/// Of one service, a tagged member on the card's own release wins over a derived link on the
/// original edition; a member placed on no release does not beat a placed derived link.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_tagged_member_wins_only_on_as_good_a_release(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    // Its canonical link: a storefront's spelling is an alias of it.
    let member_uri = "https://music.apple.com/song/1673536443";
    let member = save(&pool, member_uri, "MusicRecording", Some("Apple Music")).await;
    // One recording: both carry its key, and the grouping pass joins them.
    for id in [&card, &member] {
        also_on::record(
            &pool,
            id,
            "musicbrainz",
            Found {
                keys: vec![also_on::Key {
                    key: format!("mb:recording:{RECORDING}"),
                    via: "musicbrainz".into(),
                }],
                ..Default::default()
            },
            std::time::Duration::from_secs(86_400),
            &cfg,
        )
        .await
        .unwrap();
    }
    pinakes_core::groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    also_on::record(
        &pool,
        &card,
        "wikidata",
        found_links(&[("https://music.apple.com/us/song/617154366", "Apple Music")]),
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    let apple = |shown: Vec<String>| -> Vec<String> {
        tails(
            shown
                .into_iter()
                .filter(|u| u.contains("apple.com"))
                .collect(),
        )
    };
    // Nothing placed: the member, as before.
    assert_eq!(apple(also_on(&pool, &card, true).await), ["1673536443"]);
    // The derived link on the original edition, the member on none: the derived link.
    let place = |editions: Vec<(&'static str, &'static str, i16)>| {
        let (pool, card, cfg) = (pool.clone(), card.clone(), cfg.clone());
        async move {
            also_on::record(
                &pool,
                &card,
                "editions",
                found_editions(&editions),
                std::time::Duration::from_secs(86_400),
                &cfg,
            )
            .await
            .unwrap()
        }
    };
    place(vec![(
        "https://music.apple.com/us/song/617154366",
        RAM_APPLE,
        1,
    )])
    .await;
    assert_eq!(apple(also_on(&pool, &card, true).await), ["617154366"]);
    // The member on the card's own release: the member.
    place(vec![
        ("https://music.apple.com/us/song/617154366", RAM_APPLE, 1),
        (member_uri, RAM, 0),
    ])
    .await;
    assert_eq!(apple(also_on(&pool, &card, true).await), ["1673536443"]);
}

/// Of two pages of one document on a service (GovInfo's details page and its PDF), the page.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn of_a_page_and_a_file_on_one_service_the_page_shows(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(
        &pool,
        "https://regulations.example/documents/2026-18291",
        "Legislation",
        None,
    )
    .await;
    also_on::record(
        &pool,
        &card,
        "federal_register",
        found_links(&[
            (
                "https://www.govinfo.gov/content/pkg/FR-2026-09-09/pdf/2026-18291.pdf",
                "GovInfo",
            ),
            (
                "https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291",
                "GovInfo",
            ),
        ]),
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    assert_eq!(
        also_on(&pool, &card, true).await,
        ["https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291"]
    );
}
