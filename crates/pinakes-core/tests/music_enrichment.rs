//! A MusicBrainz card's image and description from its own Wikidata item, with their credits,
//! through the web resolver and the queue, against recorded answers
//! (`tests/fixtures/shared/music_enrichment`, see its provenance.json).
mod common;
use axum::{body::Body, http::Request, Router};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, IngestConfig, QueueConfig, ResolverConfig},
    ingest,
    model::{identity, ResourceMeta},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    queue,
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        web::WebResolver,
    },
    telemetry::Metrics,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

const SEFA: &str = "https://musicbrainz.org/artist/b5416a54-4941-4fcf-b2a0-7f98da6f9344";
const MILES: &str = "https://musicbrainz.org/artist/561d854a-6a28-4aa7-8c99-323e6ce46c2a";
const DAFT_PUNK: &str = "https://musicbrainz.org/artist/056e4f3e-d505-4dad-8ec1-d04f521cbb56";
const KIND_OF_BLUE: &str =
    "https://musicbrainz.org/release-group/8e8a594f-2175-38c7-a871-abb68ec363e7";
const GET_LUCKY: &str = "https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862";
const COLBURN: &str = "https://musicbrainz.org/artist/e7528aba-c5e2-4917-9a52-9b61d56f9a13";

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/shared/music_enrichment/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
fn fixture_json(name: &str) -> Value {
    serde_json::from_slice(&fixture(name)).unwrap()
}

/// How the services answer: as recorded, or with a failure.
#[derive(Clone, Copy, Default)]
struct Mode {
    /// Wikipedia's summaries: `Some((status, Retry-After))` instead of the recorded answer (404
    /// is the recorded missing article's).
    wikipedia: Option<(u16, u64)>,
    /// Commons: `Some((status, Retry-After))` instead of the recorded answer.
    commons: Option<(u16, u64)>,
    /// Daft Punk's image claim names the real file without an author.
    no_author: bool,
}
/// Answers by host, path and query from the recorded fixtures; records each request.
#[derive(Default)]
struct Recorded {
    mode: Mutex<Mode>,
    seen: Mutex<Vec<String>>,
}
impl Recorded {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(mode),
            seen: Mutex::default(),
        })
    }
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
    fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .seen()
            .iter()
            .map(|s| s.split('/').next().unwrap().to_owned())
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }
}
#[async_trait::async_trait]
impl Transport for Recorded {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &FetchOptions::default())
            .await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        options: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        assert_eq!(options.accept.as_deref(), Some("application/json"), "{url}");
        let host = url.host_str().unwrap_or_default().to_owned();
        let path = percent_encoding::percent_decode_str(url.path())
            .decode_utf8()
            .unwrap()
            .into_owned();
        let query = |k: &str| {
            url.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        self.seen
            .lock()
            .unwrap()
            .push(format!("{host}{path}?{}", url.query().unwrap_or_default()));
        let mode = *self.mode.lock().unwrap();
        let (status, retry_after, body) = match (host.as_str(), path.as_str()) {
            ("musicbrainz.org", path) => {
                let file = match path.trim_start_matches("/ws/2/") {
                    "artist/b5416a54-4941-4fcf-b2a0-7f98da6f9344" => "mb-artist-sefa.json",
                    "artist/561d854a-6a28-4aa7-8c99-323e6ce46c2a" => "mb-artist-miles-davis.json",
                    "artist/056e4f3e-d505-4dad-8ec1-d04f521cbb56" => "mb-artist-daft-punk.json",
                    "artist/e7528aba-c5e2-4917-9a52-9b61d56f9a13" => {
                        "mb-artist-richard-colburn.json"
                    }
                    "release-group/8e8a594f-2175-38c7-a871-abb68ec363e7" => {
                        "mb-release-group-kind-of-blue.json"
                    }
                    "recording/833f00e1-781f-4edd-90e4-e52712618862"
                        if query("inc").contains("work-rels") =>
                    {
                        "mb-recording-get-lucky-works.json"
                    }
                    "recording/833f00e1-781f-4edd-90e4-e52712618862" => {
                        "mb-recording-get-lucky.json"
                    }
                    other => panic!("unexpected MusicBrainz request {other}"),
                };
                (200, 60, fixture(file))
            }
            ("www.wikidata.org", "/w/api.php") => match query("action").as_str() {
                "wbgetentities" => {
                    assert_eq!(query("sitefilter"), "enwiki");
                    (
                        200,
                        60,
                        fixture(&format!("wd-sitelink-{}.json", query("ids"))),
                    )
                }
                "wbgetclaims" => {
                    assert_eq!(query("property"), "P18");
                    let entity = query("entity");
                    let mut body = fixture(&format!("wd-p18-{entity}.json"));
                    if mode.no_author && entity == "Q185828" {
                        // The one edit of a recorded answer (provenance.json, wd-p18-no-author).
                        let file = r#"\" \"Gruss\" a. d. Res. Lazarett in Engers a. Rh. 1915.\", Gruppenbild - LABW - Staatsarchiv Sigmaringen N 1-85 T 1 Nr. 159.jpeg"#;
                        body = String::from_utf8(body)
                            .unwrap()
                            .replace("Daft Punk in 2013.jpg", file)
                            .into_bytes();
                    }
                    (200, 60, body)
                }
                other => panic!("unexpected Wikidata request {other}"),
            },
            ("en.wikipedia.org", path) => {
                let title = path
                    .strip_prefix("/api/rest_v1/page/summary/")
                    .unwrap_or_else(|| panic!("unexpected Wikipedia request {path}"));
                match mode.wikipedia {
                    Some((404, _)) => (404, 60, fixture("enwiki-summary-404.json")),
                    Some((status, retry)) => (status, retry, b"{}".to_vec()),
                    None => {
                        let file = match title {
                            "Miles_Davis" => "enwiki-summary-miles-davis.json",
                            "Daft_Punk" => "enwiki-summary-daft-punk.json",
                            "Kind_of_Blue" => "enwiki-summary-kind-of-blue.json",
                            "Get_Lucky_(Daft_Punk_song)" => "enwiki-summary-get-lucky-song.json",
                            other => panic!("unexpected article {other}"),
                        };
                        (200, 60, fixture(file))
                    }
                }
            }
            ("commons.wikimedia.org", "/w/api.php") => {
                assert_eq!(query("iiurlwidth"), "640");
                match mode.commons {
                    Some((status, retry)) => (status, retry, b"{}".to_vec()),
                    None => {
                        let file = match query("titles").as_str() {
                            "File:Miles Davis by Palumbo cropped.jpg" => "commons-miles-davis.json",
                            "File:Daft Punk in 2013.jpg" => "commons-daft-punk.json",
                            t if t.contains("Lazarett in Engers") => "commons-no-author.json",
                            other => panic!("unexpected Commons file {other}"),
                        };
                        (200, 60, fixture(file))
                    }
                }
            }
            (host, path) => panic!("unexpected request {host}{path}"),
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status,
            retry_after_secs: retry_after,
            location: None,
            content_type: "application/json".into(),
            body,
        })
    }
}

fn web(transport: Arc<Recorded>, enrich: bool) -> WebResolver {
    WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        events: None,
        egress: Egress::with_backends(Default::default(), Arc::new(PublicDns), transport),
        config: ResolverConfig {
            music_enrichment: enrich,
            ..Default::default()
        },
    }
}
async fn card(web: &WebResolver, uri: &str) -> ResourceMeta {
    web.resolve_with_policy(uri, Default::default())
        .await
        .unwrap()
}
fn credits(doc: &ResourceMeta) -> Value {
    serde_json::to_value(&doc.credits).unwrap()
}
fn extract(file: &str) -> String {
    fixture_json(file)["extract"].as_str().unwrap().to_owned()
}
const WIKIPEDIA: (&str, &str) = (
    "CC-BY-SA-4.0",
    "https://creativecommons.org/licenses/by-sa/4.0/",
);
fn article_credit(url: &str, subject: Option<&str>) -> Value {
    json!({
        "field": "description", "provider": "wikipedia", "source_url": url,
        "author": "Wikipedia contributors", "license": WIKIPEDIA.0, "license_url": WIKIPEDIA.1,
        "lang": "en", "excerpted": true, "modified": false, "subject": subject,
    })
}
fn miles_credits() -> Value {
    json!([
        {
            "field": "image", "provider": "wikimedia_commons",
            "source_url": "https://commons.wikimedia.org/wiki/File:Miles_Davis_by_Palumbo_cropped.jpg",
            "author": "Tom Palumbo from New York City, USA",
            "license": "CC-BY-SA-2.0", "license_url": "https://creativecommons.org/licenses/by-sa/2.0/",
            "lang": null, "excerpted": false, "modified": false, "subject": null,
        },
        article_credit("https://en.wikipedia.org/wiki/Miles_Davis", None),
    ])
}
const MILES_IMAGE: &str = "https://thumb.wikimedia.org/wikipedia/commons/thumb/2/24/Miles_Davis_by_Palumbo_cropped.jpg/960px-Miles_Davis_by_Palumbo_cropped.jpg";

/// Each kind of MusicBrainz card takes what its own item has, credited: a person and a group
/// their portrait and summary, an album its article only (the article's cover thumbnail is never
/// taken), a recording without an item its work's article, naming the work; an artist without an
/// item stays sparse. MusicBrainz's own type, name and facts stay as they are.
#[tokio::test]
async fn cards_take_credited_content_from_their_own_item() {
    let t = Recorded::new(Mode::default());
    let web = web(t.clone(), true);
    let plain = self::web(Recorded::new(Mode::default()), false);

    let sefa = card(&web, SEFA).await;
    assert_eq!(
        (sefa.type_name.as_str(), sefa.name.as_deref()),
        ("Person", Some("Sefa"))
    );
    assert_eq!(
        (sefa.image.as_deref(), sefa.description.as_deref()),
        (None, None)
    );
    assert_eq!(credits(&sefa), json!([]));
    assert!(sefa.unread_enriched.is_empty());

    let miles = card(&web, MILES).await;
    assert_eq!(
        (miles.type_name.as_str(), miles.name.as_deref()),
        ("Person", Some("Miles Davis"))
    );
    assert_eq!(miles.image.as_deref(), Some(MILES_IMAGE));
    assert_eq!(
        miles.description.as_deref(),
        Some(extract("enwiki-summary-miles-davis.json").as_str())
    );
    assert_eq!(credits(&miles), miles_credits());

    let band = card(&web, DAFT_PUNK).await;
    assert_eq!(
        (band.type_name.as_str(), band.name.as_deref()),
        ("MusicGroup", Some("Daft Punk"))
    );
    assert_eq!(
        band.image.as_deref(),
        Some("https://thumb.wikimedia.org/wikipedia/commons/thumb/d/db/Daft_Punk_in_2013.jpg/960px-Daft_Punk_in_2013.jpg")
    );
    assert_eq!(
        credits(&band),
        json!([
            {
                "field": "image", "provider": "wikimedia_commons",
                "source_url": "https://commons.wikimedia.org/wiki/File:Daft_Punk_in_2013.jpg",
                "author": "Sony Music Entertainment",
                "license": "CC-BY-4.0", "license_url": "https://creativecommons.org/licenses/by/4.0/",
                "lang": null, "excerpted": false, "modified": false, "subject": null,
            },
            article_credit("https://en.wikipedia.org/wiki/Daft_Punk", None),
        ])
    );

    let album = card(&web, KIND_OF_BLUE).await;
    assert_eq!(
        (album.type_name.as_str(), album.name.as_deref()),
        ("MusicAlbum", Some("Kind of Blue"))
    );
    assert_eq!(album.image, None, "the article's cover is never the card's");
    assert_eq!(
        album.description.as_deref(),
        Some(extract("enwiki-summary-kind-of-blue.json").as_str())
    );
    assert_eq!(
        credits(&album),
        json!([article_credit(
            "https://en.wikipedia.org/wiki/Kind_of_Blue",
            None
        )])
    );

    let song = card(&web, GET_LUCKY).await;
    assert_eq!(
        (song.type_name.as_str(), song.name.as_deref()),
        ("MusicRecording", Some("Get Lucky"))
    );
    assert_eq!(song.image, None, "a work's image is never its recording's");
    assert_eq!(
        song.description.as_deref(),
        Some(extract("enwiki-summary-get-lucky-song.json").as_str())
    );
    assert_eq!(
        credits(&song),
        json!([article_credit(
            "https://en.wikipedia.org/wiki/Get_Lucky_(Daft_Punk_song)",
            Some("https://musicbrainz.org/work/b11bf816-1844-47e5-9075-9239b7609c6a")
        )])
    );

    // MusicBrainz's own description of an album or a recording (its artist credit and length)
    // gives way to the article's summary.
    for (enriched, uri) in [(&album, KIND_OF_BLUE), (&song, GET_LUCKY)] {
        let own = card(&plain, uri).await;
        let line = own.description.unwrap_or_default();
        assert!(
            line.starts_with("Daft Punk") || line.starts_with("Miles Davis"),
            "{line}"
        );
        assert_ne!(enriched.description.as_deref(), Some(line.as_str()));
    }
    // Everything else is MusicBrainz's card as it is with enrichment off.
    for (enriched, uri) in [
        (&sefa, SEFA),
        (&miles, MILES),
        (&band, DAFT_PUNK),
        (&album, KIND_OF_BLUE),
        (&song, GET_LUCKY),
    ] {
        let own = card(&plain, uri).await;
        assert!(own.credits.is_empty() && own.image.is_none(), "{uri}");
        let strip = |d: &ResourceMeta| {
            let mut v = serde_json::to_value(d).unwrap();
            for key in ["image", "description", "credits", "fetched_at"] {
                v.as_object_mut().unwrap().remove(key);
            }
            v
        };
        assert_eq!(strip(enriched), strip(&own), "{uri}");
    }
    // A sparse artist asked nothing past MusicBrainz; no thumbnail of a summary was ever read.
    let seen = t.seen();
    assert!(
        seen.iter()
            .filter(|s| s.contains("b5416a54"))
            .all(|s| s.starts_with("musicbrainz.org/ws/2/artist/")),
        "{seen:?}"
    );
    assert!(seen.iter().all(|s| !s.contains("upload.wikimedia.org")));
    // At most: MusicBrainz 2, Wikidata 2, Wikipedia 1, Commons 1 per card.
    assert_eq!(
        seen.iter()
            .filter(|s| s.starts_with("musicbrainz.org"))
            .count(),
        6,
        "{seen:?}"
    );
    assert_eq!(
        t.hosts(),
        [
            "commons.wikimedia.org",
            "en.wikipedia.org",
            "musicbrainz.org",
            "www.wikidata.org"
        ]
    );
}

/// A sitelink to a redirect names another entity's article (a band member's leads to the band's):
/// no description, and the article is never asked for. A summary is used only when it is of the
/// sitelink's own article and a standard one: neither a redirect's target nor a disambiguation
/// page gives text.
#[tokio::test]
async fn a_redirect_or_a_disambiguation_page_gives_no_description() {
    let t = Recorded::new(Mode::default());
    let colburn = card(&web(t.clone(), true), COLBURN).await;
    assert_eq!(
        (colburn.type_name.as_str(), colburn.name.as_deref()),
        ("Person", Some("Richard Colburn"))
    );
    assert_eq!((&colburn.image, &colburn.description), (&None, &None));
    assert_eq!(credits(&colburn), json!([]));
    assert!(
        colburn.unread_enriched.is_empty(),
        "a redirect is an answer"
    );
    assert_eq!(
        t.hosts(),
        ["musicbrainz.org", "www.wikidata.org"],
        "{:?}",
        t.seen()
    );

    let redirect = fixture_json("enwiki-summary-richard-colburn-redirect.json");
    assert_eq!(
        pinakes_core::music_enrichment::summary_text(&redirect, "Richard_Colburn"),
        None
    );
    assert!(
        pinakes_core::music_enrichment::summary_text(&redirect, "Belle_and_Sebastian")
            .is_some_and(|t| t.starts_with("Belle and Sebastian"))
    );
    let disambiguation = fixture_json("enwiki-summary-get-lucky-disambiguation.json");
    assert_eq!(
        pinakes_core::music_enrichment::summary_text(&disambiguation, "Get_Lucky"),
        None
    );
}

/// A Commons file whose page names no author under CC BY is never shown: the card keeps its
/// description and has no image credit.
#[tokio::test]
async fn a_file_without_its_author_is_never_shown() {
    let t = Recorded::new(Mode {
        no_author: true,
        ..Default::default()
    });
    let band = card(&web(t.clone(), true), DAFT_PUNK).await;
    assert_eq!(band.image, None);
    assert_eq!(
        credits(&band),
        json!([article_credit(
            "https://en.wikipedia.org/wiki/Daft_Punk",
            None
        )])
    );
    assert!(band.unread_enriched.is_empty(), "a refusal is an answer");
    assert!(t.seen().iter().any(|s| s.contains("Lazarett")));
}

/// Concurrent cards of one entity ask each service once: the egress shares the flight and keeps
/// the answer.
#[tokio::test]
async fn concurrent_cards_of_one_entity_ask_once() {
    let t = Recorded::new(Mode::default());
    let web = web(t.clone(), true);
    let (a, b) = tokio::join!(card(&web, MILES), card(&web, MILES));
    assert_eq!(credits(&a), miles_credits());
    assert_eq!(credits(&b), miles_credits());
    let seen = t.seen();
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(seen.len(), unique.len(), "{seen:?}");
    assert_eq!(seen.len(), 5, "{seen:?}");
}

/// A 503 or a 429 leaves its field unread (kept from the stored card, read again within the
/// hour), and the wait it names holds: a second card within it does not ask again.
#[tokio::test]
async fn a_named_wait_holds_and_leaves_the_field_unread() {
    let t = Recorded::new(Mode {
        wikipedia: Some((503, 120)),
        commons: Some((429, 60)),
        ..Default::default()
    });
    let web = web(t.clone(), true);
    for _ in 0..2 {
        let miles = card(&web, MILES).await;
        assert_eq!(
            (miles.image.as_deref(), miles.description.as_deref()),
            (None, None)
        );
        assert!(miles.credits.is_empty());
        assert_eq!(
            serde_json::to_value(&miles.unread_enriched).unwrap(),
            json!(["image", "description"])
        );
        assert!(miles
            .refresh_by
            .is_some_and(|by| by <= chrono::Utc::now() + chrono::Duration::hours(1)));
    }
    let seen = t.seen();
    for host in ["en.wikipedia.org", "commons.wikimedia.org"] {
        assert_eq!(
            seen.iter().filter(|s| s.starts_with(host)).count(),
            1,
            "{seen:?}"
        );
    }
}

/// With `[web] music_enrichment = false` a MusicBrainz card is MusicBrainz's alone: nothing else
/// is asked.
#[tokio::test]
async fn enrichment_off_asks_musicbrainz_alone() {
    let t = Recorded::new(Mode::default());
    let miles = card(&web(t.clone(), false), MILES).await;
    assert_eq!((miles.image, miles.description), (None, None));
    assert!(miles.credits.is_empty() && miles.unread_enriched.is_empty());
    assert_eq!(t.hosts(), ["musicbrainz.org"]);
    let off = Config::from_value(json!({"web": {"music_enrichment": false}})).unwrap();
    assert!(!off.web.music_enrichment && off.web.enabled);
    assert!(Config::default().web.music_enrichment);
}

/// Resolve `uri` once through the queue with `resolver`, and read the stored document.
async fn resolve_once(pool: &PgPool, resolver: &WebResolver, uri: &str) -> ResourceMeta {
    let (id, _, _) = identity(uri).unwrap();
    // Only this one is due.
    sqlx::query("UPDATE resource SET next_attempt_at=CASE WHEN id=$1 THEN now() ELSE now()+interval '1 day' END,refresh_at=now(),lease_until=NULL,priority_at=NULL")
        .bind(&id)
        .execute(pool)
        .await
        .unwrap();
    let cfg = QueueConfig::default();
    let job = queue::claim(pool, "web", &cfg).await.unwrap().unwrap();
    assert_eq!(job.id, id);
    queue::run_job(pool, resolver, &cfg, &job).await.unwrap();
    let doc: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(pool)
        .await
        .unwrap();
    serde_json::from_value(doc).unwrap()
}
fn author(n: usize) -> String {
    let alphabet = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut v = vec![b'y'; 52];
    for i in 0..5 {
        v[50 - i] = alphabet[(n >> (5 * i)) & 31];
    }
    String::from_utf8(v).unwrap()
}
/// A tag on each of `uris`, so each resource exists, is queued and is found by search.
async fn tag(pool: &PgPool, uris: &[&str]) {
    let saved = ingest::cursor(pool, "fixture").await.unwrap();
    let lines: Vec<Change> = uris
        .iter()
        .enumerate()
        .map(|(n, uri)| Change {
            op: Op::Put,
            uri: identity(uri).unwrap().1,
            kind: Kind::Tag,
            author: author(1),
            object_path: format!("/pub/app/tags/{n}-music"),
            label: Some("music".into()),
            at: 1_790_000_000_000,
        })
        .collect();
    let next = lines.len() as i64;
    ingest::apply(
        pool,
        "fixture",
        &saved,
        &FeedPage {
            page: Changes {
                lines,
                next_cursor: next,
                skipped: Default::default(),
            },
            epoch: Some("fixture".into()),
            length: next,
        },
        &IngestConfig::default(),
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap();
}

/// A transient failure (Wikipedia's 503 or 403, Commons' 429) keeps the stored image, description
/// and credits, and is asked about less often while it lasts; a definite miss (the article is gone: 404) clears that field and its credit, and
/// keeps the other. A transient failure with nothing stored saves MusicBrainz's own card.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn transient_failures_keep_stored_content_and_misses_clear_it(pool: PgPool) {
    tag(&pool, &[MILES]).await;
    let busy = Mode {
        wikipedia: Some((503, 120)),
        commons: Some((429, 60)),
        ..Default::default()
    };
    // Nothing stored yet: the card saves without the unread fields, to be read again soon.
    let first = resolve_once(&pool, &web(Recorded::new(busy), true), MILES).await;
    assert_eq!(first.status, pinakes_core::model::Status::Ok);
    assert_eq!(
        (first.name.as_deref(), first.image.as_deref()),
        (Some("Miles Davis"), None)
    );
    assert!(first.credits.is_empty());
    let (id, _, _) = identity(MILES).unwrap();
    let soon: bool = sqlx::query_scalar(
        "SELECT refresh_at <= now() + interval '3601 seconds' FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(soon);
    // Read in full.
    let full = resolve_once(&pool, &web(Recorded::new(Mode::default()), true), MILES).await;
    assert_eq!(full.image.as_deref(), Some(MILES_IMAGE));
    assert_eq!(credits(&full), miles_credits());
    // Busy services: the stored values and credits stay.
    let kept = resolve_once(&pool, &web(Recorded::new(busy), true), MILES).await;
    assert_eq!(kept.image, full.image);
    assert_eq!(kept.description, full.description);
    assert_eq!(credits(&kept), miles_credits());
    assert_eq!(kept.name.as_deref(), Some("Miles Davis"));
    // A refusal of this client (Wikipedia's 403) says nothing about the article: kept too.
    let refused = Mode {
        wikipedia: Some((403, 60)),
        ..Default::default()
    };
    let kept = resolve_once(&pool, &web(Recorded::new(refused), true), MILES).await;
    assert_eq!(kept.description, full.description);
    assert_eq!(credits(&kept), miles_credits());
    // A failure that lasts is asked about less often: the stored reading three hours old, the
    // next one in six.
    sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{fetched_at}',to_jsonb((extract(epoch from now() - interval '3 hours')*1000)::bigint)) WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let kept = resolve_once(&pool, &web(Recorded::new(busy), true), MILES).await;
    assert_eq!(credits(&kept), miles_credits());
    let later: bool = sqlx::query_scalar(
        "SELECT refresh_at BETWEEN now() + interval '359 minutes' AND now() + interval '361 minutes' FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(later);
    // The article is gone: its description and credit go, the image stays.
    let gone = Mode {
        wikipedia: Some((404, 60)),
        ..Default::default()
    };
    let cleared = resolve_once(&pool, &web(Recorded::new(gone), true), MILES).await;
    assert_eq!(cleared.description, None);
    assert_eq!(cleared.image.as_deref(), Some(MILES_IMAGE));
    assert_eq!(credits(&cleared), json!([miles_credits()[0]]));
}

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> Value {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, 200, "{path}: {value}");
    common::check_response(method, path, status, &value);
    value
}

/// Every route that shows a card's image and description shows their credits: a read by URI and
/// by ID, a search and a lookup; a card without any shows `[]`.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn credits_show_on_every_card_route(pool: PgPool) {
    tag(&pool, &[MILES, SEFA]).await;
    resolve_once(&pool, &web(Recorded::new(Mode::default()), true), MILES).await;
    let (id, _, _) = identity(MILES).unwrap();
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let uri = percent_encoding::utf8_percent_encode(MILES, percent_encoding::NON_ALPHANUMERIC);
    let by_uri = call(
        &app,
        "GET",
        &format!("/v1/resources/by-uri?uri={uri}"),
        None,
    )
    .await;
    let read = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    let search = call(&app, "GET", "/v1/search?q=Miles", None).await;
    let lookup = call(
        &app,
        "POST",
        "/v1/resources/lookup",
        Some(json!({"ids": [id]})),
    )
    .await;
    for (route, card) in [
        ("by-uri", &by_uri),
        ("read", &read),
        ("search", &search["items"][0]),
        ("lookup", &lookup[0]),
    ] {
        assert_eq!(card["id"], id, "{route}: {card}");
        assert_eq!(card["image"], MILES_IMAGE, "{route}");
        assert_eq!(card["credits"], miles_credits(), "{route}");
        assert_eq!(card["type"], "Person", "{route}");
    }
    // A card nothing enriched has none.
    resolve_once(&pool, &web(Recorded::new(Mode::default()), true), SEFA).await;
    let (sefa, _, _) = identity(SEFA).unwrap();
    let read = call(&app, "GET", &format!("/v1/resources/{sefa}"), None).await;
    assert_eq!(read["credits"], json!([]));
    assert_eq!(read["description"], Value::Null);
}
