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
    config::Config,
    model::ResourceMeta,
    resolver::egress::{Egress, FetchOptions, Fetched, Transport},
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
use common::media::PublicDns;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hubs");
const GET_LUCKY: &str = "https://open.spotify.com/track/69kOkLUCkxIZYexIgSG8rq";
const RECORDING: &str = "833f00e1-781f-4edd-90e4-e52712618862";
/// Random Access Memories, 2013: the worldwide digital release, and Apple's Digital Master.
const RAM: &str = "5000a285-b67e-4cfc-b54b-2b98f1810d2e";
const RAM_APPLE: &str = "36e2aede-346d-4931-8565-78d810d167c7";

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
/// token request with a token, a request to a host of `busy` with 503, anything else with 404;
/// each request noted.
struct Captured {
    answers: BTreeMap<String, (u16, String, Vec<u8>)>,
    busy: Vec<&'static str>,
    /// A recording's release browse claims more releases than its pages list (cut short).
    truncated: bool,
    seen: Mutex<Vec<String>>,
}
impl Captured {
    fn new() -> Arc<Self> {
        Self::busy(&[])
    }
    fn busy(busy: &[&'static str]) -> Arc<Self> {
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
            busy: busy.to_vec(),
            truncated: false,
            seen: Mutex::default(),
        })
    }
    fn truncated() -> Arc<Self> {
        let mut captured = Arc::into_inner(Self::new()).unwrap();
        captured.truncated = true;
        Arc::new(captured)
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
        let (status, content_type, body) = if self.busy.contains(&url.host_str().unwrap_or("")) {
            (503, "text/plain".to_owned(), b"busy".to_vec())
        } else if url.host_str() == Some("api.spotify.com") {
            // Spotify's answer to an app whose owner has no premium plan (2026-11-05).
            (
                403,
                "application/json".to_owned(),
                br#"{"error":{"status":403,"message":"Active premium subscription required for the owner of the app."}}"#.to_vec(),
            )
        } else if matches!(
            url.host_str(),
            Some("auth.tidal.com" | "accounts.spotify.com")
        ) {
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
        let body = if self.truncated && url.as_str().contains("/ws/2/release?recording=") {
            let mut v: Value = serde_json::from_slice(&body)?;
            v["release-count"] = 999.into();
            serde_json::to_vec(&v)?
        } else {
            body
        };
        Ok(common::fetched(url.clone(), status, content_type, body))
    }
}

/// Whether this build has each of these sources. `editions` asks a service which album a track
/// is on only while that service's own source runs (`tidal`, `deezer`, `itunes`, `spotify`; a
/// Spotify card's own album too), so a test returns early without the ones its expectations
/// rest on. An album card's links are placed by MusicBrainz's release pages alone.
fn has(ids: &[&str]) -> bool {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    ids.iter().all(|id| registered.contains(id))
}
/// TIDAL with a pair (each test its own: tokens are kept per pair), Deezer on or off.
fn config(pair: &str, deezer: bool) -> Config {
    switched(pair, &[("deezer", deezer)])
}
/// TIDAL with a pair, and these sources on or off; Spotify's public pages off unless a test reads
/// them (`with_pages`).
fn switched(pair: &str, sources: &[(&str, bool)]) -> Config {
    let mut config = Config::default();
    config.also_on.spotify_pages = false;
    config.sections.set(
        "tidal",
        serde_json::json!({"client_id": pair, "client_secret": format!("{pair}-secret")}),
    );
    for (source, enabled) in sources {
        // Only the sources this build has: a switch for one it lacks is refused at validation.
        if !has(&[*source]) {
            continue;
        }
        config.also_on.sources.insert(
            (*source).into(),
            SourceOptions {
                enabled: Some(*enabled),
                ..Default::default()
            },
        );
    }
    config
}
/// `config` that reads Spotify's public pages (`[also_on] spotify_pages`).
fn with_pages(mut config: Config) -> Config {
    config.also_on.spotify_pages = true;
    config
}
fn context(transport: Arc<Captured>, config: Config) -> also_on::Context {
    also_on::Context {
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
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
/// The links placed on a release, with it and their rank, sorted.
fn placed(found: &Found) -> Vec<(String, String, i16)> {
    let mut placed: Vec<(String, String, i16)> = found
        .editions
        .iter()
        .filter_map(|e| Some((e.uri.clone(), e.release.clone()?, e.rank)))
        .collect();
    placed.sort();
    placed
}
/// The links their service puts on another release (rank 1000, no release), sorted.
fn others(found: &Found) -> Vec<String> {
    let mut others: Vec<String> = found
        .editions
        .iter()
        .filter(|e| e.release.is_none() && e.rank == 1000)
        .map(|e| e.uri.clone())
        .collect();
    others.sort();
    others
}

/// Get Lucky's Spotify card is on Random Access Memories of 2013 (its page names the album,
/// which MusicBrainz relates to that release): TIDAL's link on that album is placed on it (rank
/// 0), Apple Music's on the original edition's Digital Master (rank 3, after two releases of May
/// 2013 with no service pages), never the 10th anniversary edition's or a soundtrack's. Deezer is
/// off, so nothing asks its API and its links stay unplaced, as Qobuz's and YouTube's do (their
/// services name no track's album here).
#[tokio::test]
async fn get_luckys_links_are_placed_on_random_access_memories() {
    if !has(&["spotify", "itunes", "tidal"]) {
        return;
    }
    let transport = Captured::new();
    let cx = context(transport.clone(), with_pages(config("editions-off", false)));
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
                3
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
    // The 10th anniversary's and the soundtrack's Apple links, the hi-res and 10th anniversary
    // TIDAL links: on another album of a service the original has a page on.
    assert_eq!(
        others(&found),
        [
            "https://music.apple.com/gb/song/1771867453",
            "https://music.apple.com/us/song/1673536443",
            "https://tidal.com/track/211822900",
            "https://tidal.com/track/293301151",
        ]
    );
    assert!(found.retry_after.is_none());
    assert!(found.keep.is_empty());
    let facts: Vec<(&str, &str)> = found
        .facts
        .iter()
        .map(|f| (f.name.as_str(), f.value.as_str()))
        .collect();
    assert!(facts.contains(&("release", RAM)), "{facts:?}");
    assert!(
        facts.contains(&("original_release", RAM_APPLE)),
        "{facts:?}"
    );
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
    if !has(&["spotify", "deezer"]) {
        return;
    }
    let transport = Captured::new();
    let cx = context(transport.clone(), with_pages(config("editions-on", true)));
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

/// Album links of Kind of Blue: four pages of its 2011 release and another Deezer album.
const KIND_OF_BLUE_LINKS: &[&str] = &[
    "https://www.deezer.com/album/1441464",
    "https://www.deezer.com/album/7824595",
    "https://tidal.com/album/655910",
    "https://music.apple.com/us/album/268443092",
    "https://open.qobuz.com/album/5099749522428",
];
/// Kind of Blue's Spotify album is a page of its 2011 worldwide release: the other services'
/// pages of that release are placed on it; another Deezer album is not.
#[tokio::test]
async fn kind_of_blues_album_pages_are_placed_on_its_own_release() {
    // Keyed by its releases, or (as MusicBrainz relates the link to two releases of one group) by
    // the group alone: its own releases are then those its URL is related to.
    for keys in [
        &[
            "mb:release:bee5e0cd-1767-4a8e-9578-6455e87ba60b",
            "mb:release:4c8af71f-5746-4981-92d7-0d6c64db1155",
        ][..],
        &["mb:release-group:8e8a594f-2175-38c7-a871-abb68ec363e7"][..],
    ] {
        let transport = Captured::new();
        let cx = context(transport.clone(), config("editions-album", false));
        let subject = subject(
            "https://open.spotify.com/album/1weenld61qoidwYuZ1GESA",
            keys,
            KIND_OF_BLUE_LINKS,
        );
        let found = Editions.lookup(&cx, &subject).await.unwrap();
        let release = "4c8af71f-5746-4981-92d7-0d6c64db1155";
        assert_eq!(
            placed(&found)
                .into_iter()
                .filter(|(uri, ..)| KIND_OF_BLUE_LINKS.contains(&uri.as_str()))
                .collect::<Vec<_>>(),
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
        assert_eq!(others(&found), ["https://www.deezer.com/album/7824595"]);
    }
}

/// A service that fails while the card's links are placed leaves its links' places as recorded
/// before (kept), and the run comes back sooner; the other services' links are placed.
#[tokio::test]
async fn a_busy_service_keeps_its_links_places() {
    if !has(&["spotify", "itunes", "tidal"]) {
        return;
    }
    let transport = Captured::busy(&["itunes.apple.com"]);
    let cx = context(transport.clone(), config("editions-busy", false));
    let subject = subject(
        GET_LUCKY,
        &[&format!("mb:recording:{RECORDING}")],
        GET_LUCKY_LINKS,
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    // Apple's links keep their places, and the run comes back sooner; TIDAL's are placed.
    let mut keep = found.keep.clone();
    keep.sort();
    assert_eq!(
        keep,
        [
            "https://music.apple.com/gb/song/1771867453",
            "https://music.apple.com/us/song/1673536443",
            "https://music.apple.com/us/song/617154366",
        ]
    );
    assert!(found.retry_after.is_some());
    assert!(placed(&found).contains(&(
        "https://tidal.com/track/20115564".to_owned(),
        RAM.to_owned(),
        0
    )));
}

/// A Spotify link is placed by its own page (the album it names, `music:album`), never by an
/// album page nobody linked; with the `spotify` source off, no Spotify page is read.
#[tokio::test]
async fn a_spotify_link_is_placed_by_its_own_page() {
    if !has(&["spotify"]) {
        return;
    }
    let apple = "https://music.apple.com/us/song/617154366";
    let links = &[GET_LUCKY];
    let transport = Captured::new();
    let cx = context(
        transport.clone(),
        with_pages(config("editions-spotify", false)),
    );
    let card = subject(apple, &[&format!("mb:recording:{RECORDING}")], links);
    let found = Editions.lookup(&cx, &card).await.unwrap();
    // Not the card's own release (MusicBrainz is not asked about its album here), so the
    // original edition: the release Spotify's album is of, fourth by date.
    assert_eq!(
        placed(&found),
        [(GET_LUCKY.to_owned(), RAM.to_owned(), 4)],
        "{:?}",
        transport.seen()
    );
    assert!(
        !transport
            .seen()
            .iter()
            .any(|u| u.starts_with("https://open.spotify.com/album/")),
        "{:?}",
        transport.seen()
    );
    // Pages off (the default), no credentials: Spotify places nothing, and nothing is asked.
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-spotify-off", false));
    let found = Editions.lookup(&cx, &card).await.unwrap();
    assert!(found.editions.is_empty());
    assert!(transport.seen().is_empty(), "{:?}", transport.seen());
    // The source off: the same, pages allowed or not.
    let transport = Captured::new();
    let cx = context(
        transport.clone(),
        with_pages(switched(
            "editions-spotify-source-off",
            &[("deezer", false), ("spotify", false)],
        )),
    );
    let found = Editions.lookup(&cx, &card).await.unwrap();
    assert!(found.editions.is_empty());
    assert!(transport.seen().is_empty(), "{:?}", transport.seen());
}

/// Credentials Spotify refuses (an app whose owner has no premium plan) place nothing and fail
/// nothing: Spotify's links keep their places, and the run records what the other services
/// placed.
#[tokio::test]
async fn refused_spotify_credentials_place_nothing_and_fail_nothing() {
    if !has(&["spotify", "tidal"]) {
        return;
    }
    let mut config = config("editions-refused", false);
    config.sections.set(
        "spotify",
        serde_json::json!({"client_id": "editions-refused", "client_secret": "editions-refused-secret"}),
    );
    let transport = Captured::new();
    let cx = context(transport.clone(), config);
    let card = subject(
        "https://music.apple.com/us/song/617154366",
        &[&format!("mb:recording:{RECORDING}")],
        &[GET_LUCKY, "https://tidal.com/track/20115564"],
    );
    let found = Editions.lookup(&cx, &card).await.unwrap();
    // Spotify's link keeps its place (credentials refused last: no early retry).
    assert_eq!(found.keep, [GET_LUCKY]);
    assert!(found.retry_after.is_none());
    assert_eq!(
        placed(&found),
        [(
            "https://tidal.com/track/20115564".to_owned(),
            RAM.to_owned(),
            4
        )],
        "{:?}",
        transport.seen()
    );
    assert!(
        transport
            .seen()
            .iter()
            .any(|u| u.starts_with("https://api.spotify.com/")),
        "{:?}",
        transport.seen()
    );
    assert!(
        !transport
            .seen()
            .iter()
            .any(|u| u.starts_with("https://open.spotify.com/")),
        "{:?}",
        transport.seen()
    );
}

/// A card whose other links are all on services that say no track's album (Qobuz, YouTube) asks
/// nothing.
#[tokio::test]
async fn links_no_service_can_place_ask_nothing() {
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-unplaceable", false));
    let subject = subject(
        GET_LUCKY,
        &[&format!("mb:recording:{RECORDING}")],
        &[
            "https://open.qobuz.com/track/9140031",
            "https://www.youtube.com/watch?v=5NV6Rdv1a3I",
        ],
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    assert!(found.editions.is_empty());
    assert!(transport.seen().is_empty(), "{:?}", transport.seen());
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
                release: Some((*release).into()),
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
    read_json(pool, uri).await["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["uri"].as_str().unwrap().to_owned())
        .collect()
}
async fn read_json(pool: &PgPool, uri: &str) -> Value {
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
    v
}
fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Get Lucky's card as stable had it (2026-11-05): fifteen derived links, three of Apple Music,
/// TIDAL and its own Spotify. Placed, each service shows one: the link on the card's own release
/// (TIDAL, Deezer), else on the original edition (Apple), else the shorter one (Qobuz); never
/// another Spotify page. A read shows the same as a search card.
#[sqlx::test(migrations = "../../migrations")]
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
#[sqlx::test(migrations = "../../migrations")]
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
    common::drain_groups(&pool, &Default::default()).await;
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

/// Of two pages of one document on a service (GovInfo's details page and its PDF), the page,
/// though the PDF's link is shorter and sorts first.
#[sqlx::test(migrations = "../../migrations")]
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
            ("https://www.govinfo.gov/app/2026-18291.pdf", "GovInfo"),
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

/// A tagged member on the card's own service never shows, its own card read or not (one whose
/// document names no service yet).
#[sqlx::test(migrations = "../../migrations")]
async fn no_member_on_the_cards_own_service_shows(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    let other = "https://open.spotify.com/track/118Opfz7qv2KA1g3Pmyk9T";
    let member = save(&pool, other, "MusicRecording", None).await;
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
    common::drain_groups(&pool, &Default::default()).await;
    // Grouped (its document names no service), it shows on the card's list nowhere.
    let grouped: Option<String> =
        sqlx::query_scalar("SELECT group_id FROM group_unit WHERE resource_id=$1")
            .bind(&member)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(grouped.is_some());
    assert!(!tails(also_on(&pool, &card, true).await).contains(&"118Opfz7qv2KA1g3Pmyk9T".into()));
    assert!(!tails(read(&pool, GET_LUCKY).await).contains(&"118Opfz7qv2KA1g3Pmyk9T".into()));
}

/// A group that changes runs its music cards' sources again: the links each may show changed,
/// and `editions` places them.
#[sqlx::test(migrations = "../../migrations")]
async fn a_group_that_changes_places_its_cards_links_again(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    let member = save(
        &pool,
        "https://music.apple.com/song/617154366",
        "MusicRecording",
        Some("Apple Music"),
    )
    .await;
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
    sqlx::query("DELETE FROM also_on_dirty")
        .execute(&pool)
        .await
        .unwrap();
    common::drain_groups(&pool, &Default::default()).await;
    let mut due: Vec<String> = sqlx::query_scalar("SELECT resource_id FROM also_on_dirty")
        .fetch_all(&pool)
        .await
        .unwrap();
    due.sort();
    let mut both = vec![card, member];
    both.sort();
    assert_eq!(due, both);
}

/// A recording's release list cut short (MusicBrainz names more than its pages read) may lack an
/// earlier original release: only the card's own release ranks, and every other link keeps its
/// place.
#[tokio::test]
async fn a_list_cut_short_ranks_only_the_cards_own_release() {
    if !has(&["spotify", "itunes", "tidal"]) {
        return;
    }
    let transport = Captured::truncated();
    let cx = context(transport.clone(), with_pages(config("editions-cut", false)));
    let subject = subject(
        GET_LUCKY,
        &[&format!("mb:recording:{RECORDING}")],
        GET_LUCKY_LINKS,
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    assert_eq!(
        placed(&found),
        [(
            "https://tidal.com/track/20115564".to_owned(),
            RAM.to_owned(),
            0
        )],
        "{:?}",
        transport.seen()
    );
    assert!(others(&found).is_empty());
    assert!(found
        .keep
        .contains(&"https://music.apple.com/us/song/617154366".to_owned()));
}

/// Of one service, at most eight links are asked about; the rest keep their places.
#[tokio::test]
async fn links_past_the_cap_keep_their_places() {
    if !has(&["itunes"]) {
        return;
    }
    let transport = Captured::new();
    let cx = context(transport.clone(), with_pages(config("editions-cap", false)));
    let apple: Vec<String> = (1..=9)
        .map(|n| format!("https://music.apple.com/us/song/90000000{n}"))
        .collect();
    let links: Vec<&str> = apple.iter().map(String::as_str).collect();
    let subject = subject(GET_LUCKY, &[&format!("mb:recording:{RECORDING}")], &links);
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    assert_eq!(found.keep, [apple[8].clone()]);
    // The eight asked: iTunes names no album for them (gone), ranked with another release's.
    assert_eq!(others(&found), apple[..8].to_vec());
}

/// The card's own Spotify track (a link someone referenced) is read for its album where Spotify's
/// API refuses the credentials, the pages switch off: its own release ranks first.
#[tokio::test]
async fn a_spotify_cards_own_page_names_its_own_release() {
    if !has(&["spotify", "tidal"]) {
        return;
    }
    let mut config = config("editions-own-page", false);
    config.sections.set(
        "spotify",
        serde_json::json!({"client_id": "editions-own-page", "client_secret": "editions-own-page-secret"}),
    );
    let transport = Captured::new();
    let cx = context(transport.clone(), config);
    let subject = subject(
        GET_LUCKY,
        &[&format!("mb:recording:{RECORDING}")],
        &["https://tidal.com/track/20115564"],
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    assert_eq!(
        placed(&found),
        [(
            "https://tidal.com/track/20115564".to_owned(),
            RAM.to_owned(),
            0
        )],
        "{:?}",
        transport.seen()
    );
    let seen = transport.seen();
    assert!(seen.iter().any(|u| u == GET_LUCKY), "{seen:?}");
    assert!(
        !seen
            .iter()
            .any(|u| u.starts_with("https://open.spotify.com/album/")),
        "{seen:?}"
    );
}

/// The worker keeps what a run could not tell: with iTunes and TIDAL failing, the card's earlier
/// places stay as recorded, and its run comes back sooner.
#[sqlx::test(migrations = "../../migrations")]
async fn the_worker_keeps_the_places_a_failing_service_could_not_tell(pool: PgPool) {
    if !has(&["itunes", "tidal"]) {
        return;
    }
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    also_on::record(
        &pool,
        &card,
        "musicbrainz",
        Found {
            keys: vec![also_on::Key {
                key: format!("mb:recording:{RECORDING}"),
                via: "musicbrainz".into(),
            }],
            links: vec![
                Link::new(
                    "https://music.apple.com/us/song/617154366",
                    "Apple Music",
                    "musicbrainz",
                ),
                Link::new("https://tidal.com/track/20115564", "TIDAL", "musicbrainz"),
            ],
            ..Default::default()
        },
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
            ("https://music.apple.com/us/song/617154366", RAM_APPLE, 3),
            ("https://tidal.com/track/20115564", RAM, 0),
        ]),
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    let rows = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT link_id||' '||release||' '||rank FROM link_release WHERE resource_id=$1 ORDER BY 1",
        )
        .bind(&card)
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    let before = rows().await;
    assert_eq!(before.len(), 2);
    sqlx::query("INSERT INTO also_on_dirty(resource_id) VALUES($1) ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
        .bind(&card)
        .execute(&pool)
        .await
        .unwrap();
    let transport = Captured::busy(&["itunes.apple.com", "openapi.tidal.com"]);
    let cx = context(
        transport.clone(),
        with_pages(config("editions-worker", false)),
    );
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(Editions)];
    also_on::drain(&pool, &cx, &sources, 10).await.unwrap();
    assert_eq!(rows().await, before, "{:?}", transport.seen());
    let soon: bool = sqlx::query_scalar(
        "SELECT next_attempt_at<now()+interval '1 hour' FROM also_on_dirty WHERE resource_id=$1",
    )
    .bind(&card)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(soon);
}

/// Of one service, a tagged member its service puts on another release (a later edition) comes
/// after a derived link nothing placed.
#[sqlx::test(migrations = "../../migrations")]
async fn a_member_on_another_release_comes_after_an_unplaced_link(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    let member_uri = "https://music.apple.com/song/1673536443";
    let member = save(&pool, member_uri, "MusicRecording", Some("Apple Music")).await;
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
    common::drain_groups(&pool, &Default::default()).await;
    assert!(
        common::one_group(&pool, &[&card, &member]).await,
        "the card and the member are one group"
    );
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
    also_on::record(
        &pool,
        &card,
        "editions",
        Found {
            editions: vec![Edition {
                uri: member_uri.into(),
                release: None,
                rank: 1000,
            }],
            ..Default::default()
        },
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    let apple: Vec<String> = tails(also_on(&pool, &card, true).await)
        .into_iter()
        .filter(|t| t == "617154366" || t == "1673536443")
        .collect();
    assert_eq!(apple, ["617154366"]);
}

/// The releases `editions` reads as the card's own and as the original edition are facts of its
/// read.
#[sqlx::test(migrations = "../../migrations")]
async fn a_read_names_the_releases_as_facts(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    also_on::record(
        &pool,
        &card,
        "editions",
        Found {
            facts: vec![
                also_on::Fact::new("release", RAM),
                also_on::Fact::new("original_release", RAM_APPLE),
            ],
            ..Default::default()
        },
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    let v = read_json(&pool, GET_LUCKY).await;
    assert_eq!(v["facts"]["release"], RAM, "{v}");
    assert_eq!(v["facts"]["original_release"], RAM_APPLE, "{v}");
}

/// Pages of the card's own service whose cards name no service yet never take a card's slots:
/// with eight of them, a TIDAL link still shows.
#[sqlx::test(migrations = "../../migrations")]
async fn pages_of_the_cards_own_service_take_no_slot(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    let mut ids = vec![card.clone()];
    for n in 1..=8 {
        let uri = format!("https://open.spotify.com/track/aaaaaaaaaaaaaaaaaaaaa{n}");
        ids.push(save(&pool, &uri, "MusicRecording", None).await);
    }
    for id in &ids {
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
    common::drain_groups(&pool, &Default::default()).await;
    let members: Vec<&str> = ids.iter().map(String::as_str).collect();
    assert!(
        common::one_group(&pool, &members).await,
        "the card and the eight pages are one group"
    );
    also_on::record(
        &pool,
        &card,
        "tidal",
        found_links(&[("https://tidal.com/track/20115564", "TIDAL")]),
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    let v: Value = sqlx::query_scalar("SELECT also_on($1,$1,8,true)")
        .bind(&card)
        .fetch_one(&pool)
        .await
        .unwrap();
    let uris: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["uri"].as_str().unwrap())
        .collect();
    assert_eq!(uris, ["https://tidal.com/track/20115564"], "{v}");
}

/// A member that becomes another link's alias runs its group's music cards' sources again, in ten
/// minutes.
#[sqlx::test(migrations = "../../migrations")]
async fn an_alias_assignment_places_its_groups_links_again(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    let member = save(
        &pool,
        "https://music.apple.com/song/617154366",
        "MusicRecording",
        Some("Apple Music"),
    )
    .await;
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
    common::drain_groups(&pool, &Default::default()).await;
    sqlx::query("DELETE FROM also_on_dirty")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    pinakes_core::alias::assign(
        &mut tx,
        &member,
        &pinakes_core::alias::Alias {
            root: "https://music.apple.com/song/617154367".into(),
            basis: "canonical",
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let due: Vec<(String, bool)> = sqlx::query_as(
        "SELECT resource_id,next_attempt_at>now()+interval '5 minutes' FROM also_on_dirty ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(due.contains(&(card, true)), "{due:?}");
}

/// Kind of Blue as stable keys it (by its release group) with only Wikidata's album links, one of
/// them another album ("Miles in the Movies"): the pages of its own release are links of its own,
/// placed first; Wikidata's other Deezer and TIDAL albums are on another release; the Discogs
/// master, the group itself, is not.
#[tokio::test]
async fn an_album_cards_own_release_pages_are_its_links() {
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-own-pages", false));
    let subject = subject(
        "https://open.spotify.com/album/1weenld61qoidwYuZ1GESA",
        &["mb:release-group:8e8a594f-2175-38c7-a871-abb68ec363e7"],
        &[
            "https://www.deezer.com/album/521185",
            "https://tidal.com/album/2178485",
            "https://www.discogs.com/master/5460",
        ],
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    let own = "4c8af71f-5746-4981-92d7-0d6c64db1155";
    let mut links: Vec<(String, String, Option<String>)> = found
        .links
        .iter()
        .map(|l| (l.site.clone(), l.uri.clone(), l.key.clone()))
        .collect();
    links.sort();
    let key = Some(format!("mb:release:{own}"));
    for expected in [
        ("Apple Music", "https://music.apple.com/album/268443092"),
        ("Deezer", "https://www.deezer.com/album/1441464"),
        ("Qobuz", "https://open.qobuz.com/album/5099749522428"),
        ("TIDAL", "https://tidal.com/album/655910"),
    ] {
        assert!(
            links.contains(&(expected.0.into(), expected.1.into(), key.clone())),
            "{expected:?} in {links:?}"
        );
    }
    assert!(placed(&found).contains(&(
        "https://www.deezer.com/album/1441464".to_owned(),
        own.to_owned(),
        0
    )));
    assert_eq!(
        others(&found),
        [
            "https://tidal.com/album/2178485",
            "https://www.deezer.com/album/521185"
        ]
    );
    assert!(!placed(&found)
        .iter()
        .any(|(uri, ..)| uri.contains("master/5460")));
}

/// Two runs of an album card keyed by its group: the second names the same links of its own
/// release and keeps the same places (no link flips off and back), and no regrouping asks for a
/// third run.
#[sqlx::test(migrations = "../../migrations")]
async fn an_album_cards_own_pages_stay_from_run_to_run(pool: PgPool) {
    let cfg = Config::default().also_on;
    let uri = "https://open.spotify.com/album/1weenld61qoidwYuZ1GESA";
    let card = save(&pool, uri, "MusicAlbum", Some("Spotify")).await;
    also_on::record(
        &pool,
        &card,
        "musicbrainz",
        Found {
            keys: vec![also_on::Key {
                key: "mb:release-group:8e8a594f-2175-38c7-a871-abb68ec363e7".into(),
                via: "musicbrainz".into(),
            }],
            ..Default::default()
        },
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    also_on::record(
        &pool,
        &card,
        "wikidata",
        found_links(&[
            ("https://www.deezer.com/album/521185", "Deezer"),
            ("https://tidal.com/album/2178485", "TIDAL"),
        ]),
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-twice", false));
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(Editions)];
    let state = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT 'link '||uri||' '||coalesce(key,'') FROM derived_link WHERE resource_id=$1 AND source='editions' UNION ALL SELECT 'place '||link_id||' '||coalesce(release,'-')||' '||rank FROM link_release WHERE resource_id=$1 ORDER BY 1",
        )
        .bind(&card)
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    let mut runs = vec![];
    for _ in 0..2 {
        sqlx::query("INSERT INTO also_on_dirty(resource_id) VALUES($1) ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
            .bind(&card)
            .execute(&pool)
            .await
            .unwrap();
        also_on::drain(&pool, &cx, &sources, 10).await.unwrap();
        runs.push(state().await);
    }
    assert!(
        runs[0]
            .iter()
            .any(|r| r.starts_with("link https://www.deezer.com/album/1441464")),
        "{:?}",
        runs[0]
    );
    assert_eq!(runs[0], runs[1]);
    // Its own release's Deezer album shows, not Wikidata's other one.
    let shown = tails(also_on(&pool, &card, true).await);
    assert!(shown.contains(&"1441464".to_owned()), "{shown:?}");
    assert!(!shown.contains(&"521185".to_owned()), "{shown:?}");
    common::drain_groups(&pool, &Default::default()).await;
    let later: bool = sqlx::query_scalar(
        "SELECT next_attempt_at>now()+interval '1 day' FROM also_on_dirty WHERE resource_id=$1",
    )
    .bind(&card)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(later);
}

/// An album card with no link of another service yet still derives its own release's pages.
#[tokio::test]
async fn an_album_card_with_nothing_to_place_derives_its_own_pages() {
    let transport = Captured::new();
    let cx = context(transport.clone(), config("editions-alone", false));
    let subject = subject(
        "https://open.spotify.com/album/1weenld61qoidwYuZ1GESA",
        &["mb:release-group:8e8a594f-2175-38c7-a871-abb68ec363e7"],
        &[],
    );
    let found = Editions.lookup(&cx, &subject).await.unwrap();
    let uris: Vec<&str> = found.links.iter().map(|l| l.uri.as_str()).collect();
    assert!(
        uris.contains(&"https://www.deezer.com/album/1441464"),
        "{uris:?}"
    );
    assert!(found.facts.iter().any(|f| f.name == "release"));
}

/// Of a group of more members than a card shows, the one the card places on its own release is
/// never cut before the per-service pick.
#[sqlx::test(migrations = "../../migrations")]
async fn a_placed_member_survives_the_cap(pool: PgPool) {
    let cfg = Config::default().also_on;
    let card = save(&pool, GET_LUCKY, "MusicRecording", Some("Spotify")).await;
    let mut members = vec![];
    for n in 1..=9 {
        let uri = format!("https://music.apple.com/song/61715436{n}");
        members.push((
            save(&pool, &uri, "MusicRecording", Some("Apple Music")).await,
            uri,
        ));
    }
    for id in std::iter::once(&card).chain(members.iter().map(|(id, _)| id)) {
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
    common::drain_groups(&pool, &Default::default()).await;
    // The member last by ID is on the card's own release.
    members.sort();
    let (_, last) = members.last().unwrap().clone();
    also_on::record(
        &pool,
        &card,
        "editions",
        found_editions(&[(&last, RAM, 0)]),
        std::time::Duration::from_secs(86_400),
        &cfg,
    )
    .await
    .unwrap();
    // As a search or lookup card shows it: at most eight.
    let v: Value = sqlx::query_scalar("SELECT also_on($1,$1,8,true)")
        .bind(&card)
        .fetch_one(&pool)
        .await
        .unwrap();
    let shown: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["uri"].as_str().unwrap())
        .collect();
    assert_eq!(shown, [last.as_str()], "{v}");
}
