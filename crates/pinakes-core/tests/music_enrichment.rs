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

/// How Wikidata answers a search (`haswbstatement`): as recorded, or derived from the recorded
/// answer (provenance.json names each edit), or with a failure.
#[derive(Clone, Copy, Default, PartialEq)]
enum Search {
    #[default]
    Recorded,
    /// No hit: `search` emptied, `totalhits` 0.
    Empty,
    /// More items carry the ID than a search returns: `totalhits` 6.
    Crowded,
    /// 503 with Retry-After: 120.
    Busy,
    /// Wikidata's `maxlag` error, sent with 200.
    Maxlag,
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
    /// Wikidata's searches for a MusicBrainz ID (P434, P4404).
    mbid_search: Search,
    /// Wikidata's searches for a Discogs artist (P1953).
    discogs_search: Search,
    /// The read-back of Sefa's item names another artist's MBID as its P434.
    other_mbid: bool,
    /// Sefa's item has no English description.
    no_en_description: bool,
    /// Get Lucky's work has no `wikidata` relation.
    work_without_item: bool,
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
    /// The requests to `host`.
    fn to(&self, host: &str) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter(|s| s.starts_with(&format!("{host}/")))
            .collect()
    }
}
/// A recorded JSON answer with one edit.
fn edited(name: &str, edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut v = fixture_json(name);
    edit(&mut v);
    serde_json::to_vec(&v).unwrap()
}
const SEFA_MBID: &str = "b5416a54-4941-4fcf-b2a0-7f98da6f9344";
const MILES_MBID: &str = "561d854a-6a28-4aa7-8c99-323e6ce46c2a";
/// Wikidata's answer to a `haswbstatement` search, by `mode`.
fn search(statement: &str, mode: Search) -> (u16, u64, Vec<u8>) {
    let file = match statement {
        "haswbstatement:P434=b5416a54-4941-4fcf-b2a0-7f98da6f9344" => "wd-search-P434-sefa.json",
        "haswbstatement:P4404=833f00e1-781f-4edd-90e4-e52712618862" => {
            "wd-search-P4404-get-lucky.json"
        }
        "haswbstatement:P1953=6743320" => "wd-search-P1953-6743320.json",
        "haswbstatement:P1953=4454360" => "wd-search-P1953-4454360.json",
        other => panic!("unexpected Wikidata search {other}"),
    };
    match mode {
        Search::Recorded => (200, 60, fixture(file)),
        Search::Empty => (
            200,
            60,
            edited(file, |v| {
                v["query"]["search"] = json!([]);
                v["query"]["searchinfo"]["totalhits"] = json!(0);
            }),
        ),
        Search::Crowded => (
            200,
            60,
            edited(file, |v| v["query"]["searchinfo"]["totalhits"] = json!(6)),
        ),
        Search::Busy => (503, 120, b"{}".to_vec()),
        Search::Maxlag => (
            200,
            60,
            br#"{"error":{"code":"maxlag","info":"Waiting for all: 5.2 seconds lagged."}}"#
                .to_vec(),
        ),
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
                let body = if mode.work_without_item && file == "mb-recording-get-lucky-works.json"
                {
                    edited(file, |v| {
                        for r in v["relations"].as_array_mut().unwrap() {
                            if let Some(urls) = r["work"]["relations"].as_array_mut() {
                                urls.retain(|u| u["type"] != "wikidata");
                            }
                        }
                    })
                } else {
                    fixture(file)
                };
                (200, 60, body)
            }
            ("www.wikidata.org", "/w/api.php") => match query("action").as_str() {
                "query" => {
                    assert_eq!(query("list"), "search");
                    let statement = query("srsearch");
                    let mode = if statement.starts_with("haswbstatement:P1953=") {
                        mode.discogs_search
                    } else {
                        mode.mbid_search
                    };
                    search(&statement, mode)
                }
                // An item read back after a search: its claims and sitelinks.
                "wbgetentities" if query("props") == "claims|sitelinks/urls" => {
                    let ids = query("ids");
                    let file = format!("wd-entities-{ids}.json");
                    let body = if mode.other_mbid && ids == "Q55825905" {
                        String::from_utf8(fixture(&file))
                            .unwrap()
                            .replace(SEFA_MBID, MILES_MBID)
                            .into_bytes()
                    } else {
                        fixture(&file)
                    };
                    (200, 60, body)
                }
                // An item's pages and English description.
                "wbgetentities" => {
                    let ids = query("ids");
                    let file = format!("wd-pages-{ids}.json");
                    let body = if mode.no_en_description && ids == "Q55825905" {
                        edited(&file, |v| v["entities"][&ids]["descriptions"] = json!({}))
                    } else {
                        fixture(&file)
                    };
                    (200, 60, body)
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
            (wiki @ ("en.wikipedia.org" | "de.wikipedia.org"), path) => {
                let title = path
                    .strip_prefix("/api/rest_v1/page/summary/")
                    .unwrap_or_else(|| panic!("unexpected Wikipedia request {path}"));
                match mode.wikipedia {
                    Some((404, _)) => (404, 60, fixture("enwiki-summary-404.json")),
                    Some((status, retry)) => (status, retry, b"{}".to_vec()),
                    None => {
                        let file = match (wiki, title) {
                            ("en.wikipedia.org", "Miles_Davis") => {
                                "enwiki-summary-miles-davis.json"
                            }
                            ("en.wikipedia.org", "Daft_Punk") => "enwiki-summary-daft-punk.json",
                            ("en.wikipedia.org", "Kind_of_Blue") => {
                                "enwiki-summary-kind-of-blue.json"
                            }
                            ("en.wikipedia.org", "Get_Lucky_(Daft_Punk_song)") => {
                                "enwiki-summary-get-lucky-song.json"
                            }
                            ("de.wikipedia.org", "Sefa_Vlaarkamp") => "dewiki-summary-sefa.json",
                            other => panic!("unexpected article {other:?}"),
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
                            "File:Sefa Thumb Wiki.jpg" => "commons-sefa.json",
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
/// taken), a recording whose own item (named by its MBID) has no article the English article of
/// the one work it is a performance of, naming the work, before its item's one-line English
/// description. MusicBrainz's own type, name and facts stay as they are.
#[tokio::test]
async fn cards_take_credited_content_from_their_own_item() {
    let t = Recorded::new(Mode::default());
    let web = web(t.clone(), true);
    let plain = self::web(Recorded::new(Mode::default()), false);

    let miles = card(&web, MILES).await;
    assert_eq!(
        (miles.type_name.as_str(), miles.name.as_deref()),
        ("Person", Some("Miles Davis"))
    );
    assert_eq!(miles.image.as_deref(), Some(MILES_IMAGE));
    // The English article, not the item's shorter English description.
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

    // The recording's own item (P4404 names it) has no article and no image: the work's English
    // article comes before the item's line, and the work's image is never the recording's.
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
    assert_eq!(credits(&song), json!([song_credit()]));

    // MusicBrainz's own description of an album or a recording (its artist credit and length)
    // gives way to the enriched one.
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
    let seen = t.seen();
    assert!(seen.iter().all(|s| !s.contains("upload.wikimedia.org")));
    // An item named by the entity's own relation is not searched for. A recording whose own item
    // has no English article reads its works once, and never the work's image.
    assert_eq!(
        t.to("www.wikidata.org")
            .iter()
            .filter(|s| s.contains("action=query"))
            .count(),
        1,
        "{seen:?}"
    );
    assert_eq!(t.to("musicbrainz.org").len(), 5, "{seen:?}");
    assert!(
        seen.iter()
            .all(|s| !s.contains("wbgetclaims&entity=Q11510350")),
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

/// A recording no item names by its MBID takes the article of the one work it is a performance
/// of, naming the work; never the work's image.
#[tokio::test]
async fn a_recording_without_an_item_takes_its_works_article() {
    let t = Recorded::new(Mode {
        mbid_search: Search::Empty,
        ..Default::default()
    });
    let song = card(&web(t.clone(), true), GET_LUCKY).await;
    assert_eq!(song.image, None, "a work's image is never its recording's");
    assert_eq!(
        song.description.as_deref(),
        Some(extract("enwiki-summary-get-lucky-song.json").as_str())
    );
    assert_eq!(credits(&song), json!([song_credit()]));
    // A recording never looks for a Discogs artist.
    assert!(
        t.seen().iter().all(|s| !s.contains("P1953")),
        "{:?}",
        t.seen()
    );
}

/// The credit of Get Lucky's work's English article on the recording's card.
fn song_credit() -> Value {
    article_credit(
        "https://en.wikipedia.org/wiki/Get_Lucky_(Daft_Punk_song)",
        Some("https://musicbrainz.org/work/b11bf816-1844-47e5-9075-9239b7609c6a"),
    )
}
/// Get Lucky's card asks exactly: MusicBrainz 2 (the recording, its works), Wikidata 5 (search,
/// read-back, the recording item's pages and image claim, the work item's pages), English
/// Wikipedia 1 (the work's article; the recording item has none), nothing of Commons.
#[tokio::test]
async fn a_recordings_requests_are_bounded() {
    let t = Recorded::new(Mode::default());
    card(&web(t.clone(), true), GET_LUCKY).await;
    let wd = "www.wikidata.org/w/api.php?action=";
    let mut expected = vec![
        "musicbrainz.org/ws/2/recording/833f00e1-781f-4edd-90e4-e52712618862?inc=url-rels+isrcs+artist-credits+genres&fmt=json".to_owned(),
        "musicbrainz.org/ws/2/recording/833f00e1-781f-4edd-90e4-e52712618862?inc=work-rels+work-level-rels+url-rels&fmt=json".to_owned(),
        format!("{wd}query&list=search&srsearch=haswbstatement%3AP4404%3D833f00e1-781f-4edd-90e4-e52712618862&srlimit=5&format=json"),
        format!("{wd}wbgetentities&ids=Q139827657&props=claims%7Csitelinks%2Furls&format=json"),
        format!("{wd}wbgetentities&ids=Q139827657&props=sitelinks%2Furls%7Cdescriptions&languages=en&format=json"),
        format!("{wd}wbgetclaims&entity=Q139827657&property=P18&format=json"),
        format!("{wd}wbgetentities&ids=Q11510350&props=sitelinks%2Furls%7Cdescriptions&languages=en&format=json"),
        "en.wikipedia.org/api/rest_v1/page/summary/Get_Lucky_(Daft_Punk_song)?".to_owned(),
    ];
    expected.sort();
    assert_eq!(sorted(t.seen()), expected);
}
/// A recording whose own item has no article and whose one work has no item of its own takes
/// its item's English description; nothing of another item.
#[tokio::test]
async fn a_recording_whose_work_has_no_item_takes_its_items_line() {
    let t = Recorded::new(Mode {
        work_without_item: true,
        ..Default::default()
    });
    let song = card(&web(t.clone(), true), GET_LUCKY).await;
    assert_eq!(song.image, None);
    assert_eq!(
        song.description.as_deref(),
        Some("vocal track by Daft Punk")
    );
    assert_eq!(credits(&song), json!([item_credit("Q139827657", None)]));
    assert!(song.unread_enriched.is_empty());
    assert_eq!(t.to("musicbrainz.org").len(), 2, "{:?}", t.seen());
    assert_eq!(t.to("en.wikipedia.org"), Vec::<String>::new());
}

const SEFA_IMAGE: &str =
    "https://thumb.wikimedia.org/wikipedia/commons/thumb/4/45/Sefa_Thumb_Wiki.jpg/960px-Sefa_Thumb_Wiki.jpg";
/// Wikidata's English description of an item, credited.
fn item_credit(qid: &str, subject: Option<&str>) -> Value {
    json!({
        "field": "description", "provider": "wikidata",
        "source_url": format!("https://www.wikidata.org/wiki/{qid}"),
        "author": null, "license": "CC0-1.0",
        "license_url": "https://creativecommons.org/publicdomain/zero/1.0/",
        "lang": "en", "excerpted": false, "modified": false, "subject": subject,
    })
}
fn sefa_image_credit() -> Value {
    json!({
        "field": "image", "provider": "wikimedia_commons",
        "source_url": "https://commons.wikimedia.org/wiki/File:Sefa_Thumb_Wiki.jpg",
        "author": "SjakePaak",
        "license": "CC-BY-SA-4.0", "license_url": "https://creativecommons.org/licenses/by-sa/4.0/",
        "lang": null, "excerpted": false, "modified": false, "subject": null,
    })
}
fn sefa_credits() -> Value {
    json!([sefa_image_credit(), item_credit("Q55825905", None)])
}
const SEFA_DESCRIPTION: &str = "deejay, producer and musician from the Netherlands";
/// The item's requests for a card of Sefa that the MBID search found, sorted.
fn sefa_requests() -> Vec<String> {
    let mut asked = vec![
        "musicbrainz.org/ws/2/artist/b5416a54-4941-4fcf-b2a0-7f98da6f9344?inc=url-rels+genres&fmt=json".to_owned(),
        "www.wikidata.org/w/api.php?action=query&list=search&srsearch=haswbstatement%3AP434%3Db5416a54-4941-4fcf-b2a0-7f98da6f9344&srlimit=5&format=json".to_owned(),
        "www.wikidata.org/w/api.php?action=wbgetentities&ids=Q55825905&props=claims%7Csitelinks%2Furls&format=json".to_owned(),
        "www.wikidata.org/w/api.php?action=wbgetentities&ids=Q55825905&props=sitelinks%2Furls%7Cdescriptions&languages=en&format=json".to_owned(),
        "www.wikidata.org/w/api.php?action=wbgetclaims&entity=Q55825905&property=P18&format=json".to_owned(),
        "commons.wikimedia.org/w/api.php?action=query&format=json&formatversion=2&prop=imageinfo&iiprop=url%7Cmime%7Cextmetadata&iiurlwidth=640&iiextmetadatafilter=Artist%7CLicenseShortName%7CLicense%7CLicenseUrl%7CNonFree&iiextmetadatalanguage=en&titles=File%3ASefa+Thumb+Wiki.jpg".to_owned(),
    ];
    asked.sort();
    asked
}
fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// An artist without a Wikidata relation takes the one item that carries its MBID (P434),
/// read back and checked: its Commons image, and, with no English article, its English
/// description, credited to Wikidata under CC0. Exactly these six requests: MusicBrainz 1,
/// Wikidata 4 (search, read-back, pages and description, image), Commons 1; no Wikipedia.
#[tokio::test]
async fn an_artist_without_a_relation_takes_the_one_item_that_names_it() {
    let t = Recorded::new(Mode::default());
    let sefa = card(&web(t.clone(), true), SEFA).await;
    assert_eq!(
        (sefa.type_name.as_str(), sefa.name.as_deref()),
        ("Person", Some("Sefa"))
    );
    assert_eq!(sefa.image.as_deref(), Some(SEFA_IMAGE));
    assert_eq!(sefa.description.as_deref(), Some(SEFA_DESCRIPTION));
    assert_eq!(credits(&sefa), sefa_credits());
    assert!(sefa.unread_enriched.is_empty());
    assert_eq!(sorted(t.seen()), sefa_requests());
    // MusicBrainz's own facts are on the card with or without enrichment.
    for (name, value) in [
        ("disambiguation", "frenchcore artist"),
        ("country_code", "NL"),
        ("area", "Netherlands"),
        ("genre", "frenchcore; uptempo hardcore"),
    ] {
        assert_eq!(
            sefa.attributes.get(name).map(String::as_str),
            Some(value),
            "{name}"
        );
    }
}

/// With neither an English article nor an English description, the first other Wikipedia the
/// item has, in a fixed order (German before Dutch here), gives its summary, credited with its
/// language.
#[tokio::test]
async fn another_wikipedia_speaks_when_english_has_nothing() {
    let t = Recorded::new(Mode {
        no_en_description: true,
        ..Default::default()
    });
    let sefa = card(&web(t.clone(), true), SEFA).await;
    assert_eq!(
        sefa.description.as_deref(),
        Some(
            fixture_json("dewiki-summary-sefa.json")["extract"]
                .as_str()
                .unwrap()
        )
    );
    assert_eq!(
        credits(&sefa),
        json!([
            sefa_image_credit(),
            {
                "field": "description", "provider": "wikipedia",
                "source_url": "https://de.wikipedia.org/wiki/Sefa_Vlaarkamp",
                "author": "Wikipedia contributors", "license": WIKIPEDIA.0, "license_url": WIKIPEDIA.1,
                "lang": "de", "excerpted": true, "modified": false, "subject": null,
            },
        ])
    );
    assert_eq!(
        t.to("de.wikipedia.org"),
        ["de.wikipedia.org/api/rest_v1/page/summary/Sefa_Vlaarkamp?"]
    );
    assert_eq!(t.to("en.wikipedia.org"), Vec::<String>::new());
}

/// A search with more items than it returns takes no item (and no Discogs lookup follows); a
/// hit whose item, read back, carries another MBID (a stale index) is no item either. Nothing is
/// taken, and nothing is unread.
#[tokio::test]
async fn an_unclear_or_stale_search_takes_no_item() {
    let crowded = Recorded::new(Mode {
        mbid_search: Search::Crowded,
        ..Default::default()
    });
    let sefa = card(&web(crowded.clone(), true), SEFA).await;
    assert_eq!((&sefa.image, &sefa.description), (&None, &None));
    assert_eq!(credits(&sefa), json!([]));
    assert!(sefa.unread_enriched.is_empty());
    assert_eq!(
        crowded.to("www.wikidata.org").len(),
        1,
        "{:?}",
        crowded.seen()
    );
    assert_eq!(crowded.hosts(), ["musicbrainz.org", "www.wikidata.org"]);

    let stale = Recorded::new(Mode {
        other_mbid: true,
        discogs_search: Search::Empty,
        ..Default::default()
    });
    let sefa = card(&web(stale.clone(), true), SEFA).await;
    assert_eq!((&sefa.image, &sefa.description), (&None, &None));
    assert_eq!(credits(&sefa), json!([]));
    assert!(sefa.unread_enriched.is_empty());
    let wikidata = stale.to("www.wikidata.org");
    assert!(
        wikidata
            .iter()
            .all(|s| !s.contains("descriptions") && !s.contains("P18")),
        "{wikidata:?}"
    );
}

/// An artist the MBID search finds no item for takes the item its Discogs artist pages
/// (MusicBrainz's own links, at most two) name, when every one that names an item names the
/// same and that item names no other artist's MBID. Otherwise nothing.
#[tokio::test]
async fn discogs_names_the_artist_when_its_mbid_does_not() {
    let t = Recorded::new(Mode {
        mbid_search: Search::Empty,
        ..Default::default()
    });
    let sefa = card(&web(t.clone(), true), SEFA).await;
    assert_eq!(sefa.image.as_deref(), Some(SEFA_IMAGE));
    assert_eq!(credits(&sefa), sefa_credits());
    let searches: Vec<String> = t
        .to("www.wikidata.org")
        .into_iter()
        .filter(|s| s.contains("action=query"))
        .collect();
    assert_eq!(searches.len(), 3, "{searches:?}");
    assert!(searches[1..].iter().all(|s| s.contains("P1953")));
    // The two Discogs lookups read the same item back once.
    assert_eq!(
        t.to("www.wikidata.org")
            .iter()
            .filter(|s| s.contains("props=claims"))
            .count(),
        1
    );

    // The item both name carries another artist's MBID: it is someone else.
    let other = Recorded::new(Mode {
        mbid_search: Search::Empty,
        other_mbid: true,
        ..Default::default()
    });
    let sefa = card(&web(other.clone(), true), SEFA).await;
    assert_eq!((&sefa.image, &sefa.description), (&None, &None));
    assert_eq!(credits(&sefa), json!([]));
    assert!(sefa.unread_enriched.is_empty());
    assert_eq!(other.hosts(), ["musicbrainz.org", "www.wikidata.org"]);
}

/// Wikidata busy (a 503, or its `maxlag` error) while looking the item up leaves both fields
/// unread, not missing.
#[tokio::test]
async fn a_busy_search_leaves_both_fields_unread() {
    for failure in [Search::Busy, Search::Maxlag] {
        let t = Recorded::new(Mode {
            mbid_search: failure,
            ..Default::default()
        });
        let sefa = card(&web(t.clone(), true), SEFA).await;
        assert_eq!((&sefa.image, &sefa.description), (&None, &None));
        assert_eq!(credits(&sefa), json!([]));
        assert_eq!(
            serde_json::to_value(&sefa.unread_enriched).unwrap(),
            json!(["image", "description"])
        );
        assert!(t.seen().iter().all(|s| !s.contains("P1953")));
    }
}

/// A sitelink to a redirect names another entity's article (a band member's leads to the band's):
/// it is no article, and the article is never asked for; the item's own English description
/// speaks instead. A summary is used only when it is of the sitelink's own article and a
/// standard one: neither a redirect's target nor a disambiguation page gives text.
#[tokio::test]
async fn a_redirect_or_a_disambiguation_page_gives_no_description() {
    let t = Recorded::new(Mode::default());
    let colburn = card(&web(t.clone(), true), COLBURN).await;
    assert_eq!(
        (colburn.type_name.as_str(), colburn.name.as_deref()),
        ("Person", Some("Richard Colburn"))
    );
    assert_eq!(colburn.image, None);
    assert_eq!(colburn.description.as_deref(), Some("British musician"));
    assert_eq!(credits(&colburn), json!([item_credit("Q3780426", None)]));
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
/// is asked, not even for an artist without a Wikidata relation, and MusicBrainz's own facts are
/// on it.
#[tokio::test]
async fn enrichment_off_asks_musicbrainz_alone() {
    for uri in [MILES, SEFA] {
        let t = Recorded::new(Mode::default());
        let doc = card(&web(t.clone(), false), uri).await;
        assert_eq!((doc.image, doc.description), (None, None));
        assert!(doc.credits.is_empty() && doc.unread_enriched.is_empty());
        assert_eq!(t.hosts(), ["musicbrainz.org"]);
        assert!(doc.attributes.contains_key("country_code"), "{uri}");
    }
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
    // The article is gone: its text and credit go, and the item's English description takes
    // their place (a definite miss falls through to the next text); the image stays.
    let gone = Mode {
        wikipedia: Some((404, 60)),
        ..Default::default()
    };
    let cleared = resolve_once(&pool, &web(Recorded::new(gone), true), MILES).await;
    assert_eq!(
        cleared.description.as_deref(),
        Some("American jazz trumpeter, bandleader, and composer (1926–1991)")
    );
    assert_eq!(cleared.image.as_deref(), Some(MILES_IMAGE));
    assert_eq!(
        credits(&cleared),
        json!([miles_credits()[0], item_credit("Q93341", None)])
    );
}

/// The item an artist's MBID names, through the queue: a busy Wikidata (503, maxlag) keeps the
/// stored image, description and credits; a search that definitely finds no item (and no
/// Discogs item) clears them.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_busy_search_keeps_stored_content_and_no_item_clears_it(pool: PgPool) {
    tag(&pool, &[SEFA]).await;
    let full = resolve_once(&pool, &web(Recorded::new(Mode::default()), true), SEFA).await;
    assert_eq!(full.image.as_deref(), Some(SEFA_IMAGE));
    assert_eq!(credits(&full), sefa_credits());
    for failure in [Search::Busy, Search::Maxlag] {
        let busy = Mode {
            mbid_search: failure,
            ..Default::default()
        };
        let kept = resolve_once(&pool, &web(Recorded::new(busy), true), SEFA).await;
        assert_eq!(kept.image, full.image);
        assert_eq!(kept.description, full.description);
        assert_eq!(credits(&kept), sefa_credits());
        assert_eq!(kept.attributes.get("genre"), full.attributes.get("genre"));
    }
    let none = Mode {
        mbid_search: Search::Empty,
        discogs_search: Search::Empty,
        ..Default::default()
    };
    let cleared = resolve_once(&pool, &web(Recorded::new(none), true), SEFA).await;
    assert_eq!((&cleared.image, &cleared.description), (&None, &None));
    assert_eq!(credits(&cleared), json!([]));
    assert_eq!(cleared.name.as_deref(), Some("Sefa"));
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
    let none = Mode {
        mbid_search: Search::Empty,
        discogs_search: Search::Empty,
        ..Default::default()
    };
    resolve_once(&pool, &web(Recorded::new(none), true), SEFA).await;
    let (sefa, _, _) = identity(SEFA).unwrap();
    let read = call(&app, "GET", &format!("/v1/resources/{sefa}"), None).await;
    assert_eq!(read["credits"], json!([]));
    assert_eq!(read["description"], Value::Null);
    // The item its MBID names gives it Wikidata's credit.
    resolve_once(&pool, &web(Recorded::new(Mode::default()), true), SEFA).await;
    let uri = percent_encoding::utf8_percent_encode(SEFA, percent_encoding::NON_ALPHANUMERIC);
    let by_uri = call(
        &app,
        "GET",
        &format!("/v1/resources/by-uri?uri={uri}"),
        None,
    )
    .await;
    let search = call(&app, "GET", "/v1/search?q=Sefa", None).await;
    for card in [&by_uri, &search["items"][0]] {
        assert_eq!(card["id"], sefa, "{card}");
        assert_eq!(card["credits"], sefa_credits(), "{card}");
        assert_eq!(card["description"], SEFA_DESCRIPTION);
    }
}

/// MusicBrainz's own facts about the entity are on the resource read, whether or not the card
/// is enriched: an artist's disambiguation, country (ISO 3166-1 code), area, top genres and
/// type; an album's type and first release date; a recording's first release date.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn musicbrainz_facts_show_on_the_resource_read(pool: PgPool) {
    tag(&pool, &[SEFA, KIND_OF_BLUE, GET_LUCKY]).await;
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    for enrich in [false, true] {
        let t = Recorded::new(Mode::default());
        for uri in [SEFA, KIND_OF_BLUE, GET_LUCKY] {
            resolve_once(&pool, &web(t.clone(), enrich), uri).await;
        }
        if !enrich {
            assert_eq!(t.hosts(), ["musicbrainz.org"]);
        }
        let read = |uri: &str| {
            let (id, _, _) = identity(uri).unwrap();
            let app = app.clone();
            async move { call(&app, "GET", &format!("/v1/resources/{id}"), None).await }
        };
        let sefa = read(SEFA).await;
        assert_eq!(
            sefa["facts"],
            json!({
                "area": "Netherlands",
                "artist_type": "Person",
                "country_code": "NL",
                "disambiguation": "frenchcore artist",
                "genre": "frenchcore; uptempo hardcore",
            }),
            "{sefa}"
        );
        let album = read(KIND_OF_BLUE).await;
        assert_eq!(album["facts"]["release_type"], "Album", "{album}");
        assert_eq!(album["facts"]["first_release_date"], "1959-08-17");
        assert_eq!(album["facts"]["genre"], "jazz; cool jazz; modal jazz");
        let song = read(GET_LUCKY).await;
        assert_eq!(song["facts"]["first_release_date"], "2013-05-17", "{song}");
        // A tie in count goes by name.
        assert_eq!(song["facts"]["genre"], "disco; funk; electronic");
        assert_eq!(song["facts"]["release_type"], Value::Null);
    }
}

/// A song whose description is an enriched one keeps MusicBrainz's length and artist credit as
/// facts of its own card: the resource read gives `duration` (in seconds, as the
/// `musicbrainz_entity` source gives it) and `artist` from the card itself, before any also-on
/// source has read the song (none has here).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_enriched_recording_keeps_its_length_as_a_fact(pool: PgPool) {
    tag(&pool, &[GET_LUCKY]).await;
    let doc = resolve_once(&pool, &web(Recorded::new(Mode::default()), true), GET_LUCKY).await;
    let summary = extract("enwiki-summary-get-lucky-song.json");
    assert_eq!(doc.description.as_deref(), Some(summary.as_str()));
    let entity = fixture_json("mb-recording-get-lucky.json");
    let secs = (entity["length"].as_u64().unwrap() + 500) / 1000;
    let artist: String = entity["artist-credit"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{}{}",
                c["name"].as_str().unwrap(),
                c["joinphrase"].as_str().unwrap_or_default()
            )
        })
        .collect();
    let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_fact")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(facts, 0, "no also-on source has read the song");
    let (id, _, _) = identity(GET_LUCKY).unwrap();
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let uri = percent_encoding::utf8_percent_encode(GET_LUCKY, percent_encoding::NON_ALPHANUMERIC);
    let by_uri = call(
        &app,
        "GET",
        &format!("/v1/resources/by-uri?uri={uri}"),
        None,
    )
    .await;
    let read = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    for (route, card) in [("by-uri", &by_uri), ("read", &read)] {
        assert_eq!(card["description"], json!(summary), "{route}");
        assert_eq!(
            card["facts"]["duration"],
            json!(secs.to_string()),
            "{route}: {card}"
        );
        assert_eq!(card["facts"]["artist"], json!(artist), "{route}: {card}");
    }
    // The `musicbrainz_entity` source's facts say the same: each value shows once. With the
    // source switched off, the card's own still show.
    for (name, value) in [("artist", artist.clone()), ("duration", secs.to_string())] {
        sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) VALUES($1,$2,$3,'musicbrainz_entity',now()+interval '1 day')")
            .bind(&id)
            .bind(name)
            .bind(value)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) VALUES($1,'isrc','USQX91300108','musicbrainz_entity',now()+interval '1 day')")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let read = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    assert_eq!(read["facts"]["duration"], json!(secs.to_string()), "{read}");
    assert_eq!(read["facts"]["artist"], json!(artist), "{read}");
    assert_eq!(read["facts"]["isrc"], "USQX91300108", "{read}");
    sqlx::query("INSERT INTO also_on_source_off(source) VALUES('musicbrainz_entity')")
        .execute(&pool)
        .await
        .unwrap();
    let read = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    assert_eq!(read["facts"]["duration"], json!(secs.to_string()), "{read}");
    assert_eq!(read["facts"]["artist"], json!(artist), "{read}");
    assert_eq!(read["facts"]["isrc"], Value::Null, "{read}");
}
