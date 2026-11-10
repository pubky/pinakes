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
const KIND_OF_BLUE_RELEASE: &str =
    "https://musicbrainz.org/release/4c8af71f-5746-4981-92d7-0d6c64db1155";
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
    /// Get Lucky's recording has a `wikidata` relation to its song's item (Q11510350).
    recording_names_its_song: bool,
    /// Daft Punk's `wikidata` relation names the "Get Lucky" disambiguation page's item.
    names_disambiguation: bool,
    /// That item's read-back has no P31 claims.
    disambiguation_unclassed: bool,
    /// Get Lucky's recording item (Q139827657) has an English sitelink to the "Get Lucky"
    /// disambiguation page.
    recording_item_links_disambiguation: bool,
    /// Kind of Blue's item (Q283221) read back also carries a work ID (P435), as a rock opera's
    /// or a single's song's item that is its release group's too does.
    album_is_a_work: bool,
    /// `Some((file, qid))`: the `wikidata` relation of MusicBrainz's answer `file` names item
    /// `qid` (an item of another kind of entity) instead of its own, or is added naming it.
    relation: Option<(&'static str, &'static str)>,
    /// A request whose host, path and query hold this answers 410 Gone.
    gone: Option<&'static str>,
    /// How the Cover Art Archive answers.
    cover_art: CoverArt,
    /// Kind of Blue's release says the archive has no front cover for it
    /// (`cover-art-archive.front` false).
    release_without_front: bool,
    /// Kind of Blue's item (Q283221) has an image claim: Miles Davis's recorded one.
    album_has_image: bool,
    /// Kind of Blue's release group's `wikidata` relation is a link to a property, no item.
    album_relation_not_an_item: bool,
    /// Kind of Blue's item (Q283221) read back as another kind of item.
    album_item: AlbumItem,
    /// Wikidata answers the read-back of Kind of Blue's item with 503 (Retry-After: 120).
    album_item_busy: bool,
    /// Wikidata answers each request this many milliseconds late, as a busy Wikidata behind the
    /// egress's pacing (one request at a time per host) does.
    wikidata_delay_ms: u64,
    /// The Cover Art Archive answers this many milliseconds late.
    cover_art_delay_ms: u64,
    /// Daft Punk's Commons file as Commons writes a file under `{{GPL}}`.
    daft_punk_gpl: GplFile,
}
/// Daft Punk's Commons answer under `{{GPL}}`: its License removed, LicenseShortName `GPL` and
/// LicenseUrl `http://www.gnu.org/licenses/gpl.html`.
#[derive(Clone, Copy, Default, PartialEq)]
enum GplFile {
    /// As recorded (CC BY 4.0).
    #[default]
    No,
    /// With its recorded author.
    Credited,
    /// Its Artist removed too.
    WithoutAuthor,
}
/// What Kind of Blue's item (Q283221) is when read back.
#[derive(Clone, Copy, Default, PartialEq)]
enum AlbumItem {
    /// As recorded: the album's (P436, P1954).
    #[default]
    Recorded,
    /// A song's that is its single's too, as Wikidata often makes them: class song (Q7366),
    /// the release group's P436 and one recording's P4404, no work ID.
    SongAndAlbum,
    /// An item with no MusicBrainz or Discogs ID yet: P436 and P1954 removed.
    Bare,
    /// The album's item (class album, its P436) that also names one recording (P4404) and a
    /// Spotify track (P2207), as a single's item that is its track's too may.
    AlbumWithRecording,
}
/// The recording ID [`AlbumItem::SongAndAlbum`] adds.
const SONG_RECORDING: &str = "0d0f1e2a-3b4c-4d5e-8f60-718293a4b5c6";
/// How the Cover Art Archive answers a `front-500` request.
#[derive(Clone, Copy, Default, PartialEq)]
enum CoverArt {
    /// As recorded: a redirect to the front cover's thumbnail.
    #[default]
    Recorded,
    /// The recorded 404 of a release group without a cover.
    None,
    /// 503 with Retry-After: 120.
    Busy,
}
const KIND_OF_BLUE_COVER: &str =
    "https://coverartarchive.org/release/e7ba3cb7-a074-45ee-870f-3baeb6d3e8bf/front-500";
const KIND_OF_BLUE_RELEASE_COVER: &str =
    "https://coverartarchive.org/release/4c8af71f-5746-4981-92d7-0d6c64db1155/front-500";
/// The credit of a cover from the Cover Art Archive: the release's cover art page on
/// MusicBrainz, no author, no licence.
fn cover_credit(release: &str) -> Value {
    json!({
        "field": "image", "provider": "cover_art_archive",
        "source_url": format!("https://musicbrainz.org/release/{release}/cover-art"),
        "author": null, "license": null, "license_url": null,
        "lang": null, "excerpted": false, "modified": false, "subject": null,
    })
}
fn album_cover_credit() -> Value {
    cover_credit("e7ba3cb7-a074-45ee-870f-3baeb6d3e8bf")
}
/// The archive's answer to `path` (its status, content type and Location as recorded), by
/// `mode`.
fn cover_art(path: &str, mode: CoverArt) -> Fetched {
    let file = match (path, mode) {
        (_, CoverArt::None) => "caa-front-500-unknown.json",
        ("/release-group/8e8a594f-2175-38c7-a871-abb68ec363e7/front-500", _) => {
            "caa-front-500-release-group-kind-of-blue.json"
        }
        ("/release/4c8af71f-5746-4981-92d7-0d6c64db1155/front-500", _) => {
            "caa-front-500-release-kind-of-blue.json"
        }
        (other, _) => panic!("unexpected Cover Art Archive request {other}"),
    };
    let answer = fixture_json(file);
    let busy = mode == CoverArt::Busy;
    Fetched {
        permanent_redirect: false,
        truncated: false,
        validators: Default::default(),
        url: url::Url::parse(&format!("https://coverartarchive.org{path}")).unwrap(),
        status: if busy {
            503
        } else {
            answer["status"].as_u64().unwrap() as u16
        },
        retry_after_secs: if busy { 120 } else { 60 },
        location: answer["location"]
            .as_str()
            .filter(|_| !busy)
            .map(str::to_owned),
        content_type: answer["content_type"].as_str().unwrap().to_owned(),
        body: vec![],
    }
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
/// A MusicBrainz answer with a `wikidata` URL relation to `qid` added, written as MusicBrainz
/// writes Daft Punk's.
fn with_wikidata_relation(name: &str, qid: &str) -> Vec<u8> {
    let relation = fixture_json("mb-artist-daft-punk.json")["relations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["type"] == "wikidata")
        .unwrap()
        .clone();
    let relation = serde_json::to_string(&relation)
        .unwrap()
        .replace("Q185828", qid);
    edited(name, |v| {
        v["relations"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::from_str(&relation).unwrap())
    })
}
/// MusicBrainz's answer `file` with its `wikidata` relation's item replaced by `qid`, or, when it
/// has none, such a relation added ([`with_wikidata_relation`]).
fn naming(file: &str, qid: &str) -> Vec<u8> {
    let text = String::from_utf8(fixture(file)).unwrap();
    let Some(at) = text.find("wikidata.org/wiki/Q") else {
        return with_wikidata_relation(file, qid);
    };
    let start = at + "wikidata.org/wiki/".len();
    let end = start
        + 1
        + text[start + 1..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap();
    assert!(!text[end..].contains("wikidata.org/wiki/Q"), "{file}");
    format!("{}{qid}{}", &text[..start], &text[end..]).into_bytes()
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
        if url.host_str() == Some("coverartarchive.org") {
            // A cover's redirect is its answer, never followed.
            assert_eq!(options.accept.as_deref(), Some("image/*"), "{url}");
            assert!(options.keep_redirect, "{url}");
            self.seen
                .lock()
                .unwrap()
                .push(format!("coverartarchive.org{}?", url.path()));
            let (mode, delay) = {
                let mode = self.mode.lock().unwrap();
                (mode.cover_art, mode.cover_art_delay_ms)
            };
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            return Ok(cover_art(url.path(), mode));
        }
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
        if host == "www.wikidata.org" && mode.wikidata_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(mode.wikidata_delay_ms)).await;
        }
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
                    "release/4c8af71f-5746-4981-92d7-0d6c64db1155" => {
                        "mb-release-kind-of-blue.json"
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
                let body = if let Some((_, qid)) = mode.relation.filter(|(f, _)| *f == file) {
                    naming(file, qid)
                } else if mode.album_relation_not_an_item
                    && file == "mb-release-group-kind-of-blue.json"
                {
                    String::from_utf8(fixture(file))
                        .unwrap()
                        .replace(
                            "wikidata.org/wiki/Q283221",
                            "wikidata.org/wiki/Property:P18",
                        )
                        .into_bytes()
                } else if file == "mb-release-kind-of-blue.json" {
                    // The release's relation names its album's item, the release group's.
                    let body = with_wikidata_relation(file, "Q283221");
                    if mode.release_without_front {
                        let mut v: Value = serde_json::from_slice(&body).unwrap();
                        v["cover-art-archive"]["front"] = json!(false);
                        serde_json::to_vec(&v).unwrap()
                    } else {
                        body
                    }
                } else if mode.recording_names_its_song && file == "mb-recording-get-lucky.json" {
                    with_wikidata_relation(file, "Q11510350")
                } else if mode.names_disambiguation && file == "mb-artist-daft-punk.json" {
                    String::from_utf8(fixture(file))
                        .unwrap()
                        .replace("wiki/Q185828", "wiki/Q5554322")
                        .into_bytes()
                } else if mode.work_without_item && file == "mb-recording-get-lucky-works.json" {
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
                // An item read back, however it was found: its claims and sitelinks.
                "wbgetentities" if query("props") == "claims|sitelinks/urls" => {
                    let ids = query("ids");
                    let file = format!("wd-entities-{ids}.json");
                    let body = if mode.disambiguation_unclassed && ids == "Q5554322" {
                        edited(&file, |v| {
                            v["entities"][&ids]["claims"]
                                .as_object_mut()
                                .unwrap()
                                .remove("P31");
                        })
                    } else if mode.album_item_busy && ids == "Q283221" {
                        return Ok(Fetched {
                            permanent_redirect: false,
                            truncated: false,
                            validators: Default::default(),
                            url: url.clone(),
                            status: 503,
                            retry_after_secs: 120,
                            location: None,
                            content_type: "application/json".into(),
                            body: b"{}".to_vec(),
                        });
                    } else if mode.album_item != AlbumItem::Recorded && ids == "Q283221" {
                        edited(&file, |v| {
                            let claims = &mut v["entities"][&ids]["claims"];
                            match mode.album_item {
                                AlbumItem::SongAndAlbum => {
                                    let p31 = serde_json::to_string(&claims["P31"])
                                        .unwrap()
                                        .replace("Q482994", "Q7366");
                                    claims["P31"] = serde_json::from_str(&p31).unwrap();
                                    let recording = serde_json::to_string(&claims["P436"])
                                        .unwrap()
                                        .replace("P436", "P4404")
                                        .replace(
                                            "8e8a594f-2175-38c7-a871-abb68ec363e7",
                                            SONG_RECORDING,
                                        );
                                    claims["P4404"] = serde_json::from_str(&recording).unwrap();
                                }
                                AlbumItem::AlbumWithRecording => {
                                    let p436 = serde_json::to_string(&claims["P436"]).unwrap();
                                    let recording = p436.replace("P436", "P4404").replace(
                                        "8e8a594f-2175-38c7-a871-abb68ec363e7",
                                        SONG_RECORDING,
                                    );
                                    claims["P4404"] = serde_json::from_str(&recording).unwrap();
                                    let track = p436.replace("P436", "P2207").replace(
                                        "8e8a594f-2175-38c7-a871-abb68ec363e7",
                                        "0000000000000000000001",
                                    );
                                    claims["P2207"] = serde_json::from_str(&track).unwrap();
                                }
                                AlbumItem::Bare => {
                                    let claims = claims.as_object_mut().unwrap();
                                    claims.remove("P436");
                                    claims.remove("P1954");
                                }
                                AlbumItem::Recorded => {}
                            }
                        })
                    } else if mode.album_is_a_work && ids == "Q283221" {
                        edited(&file, |v| {
                            let claims = &mut v["entities"][&ids]["claims"];
                            let work = serde_json::to_string(&claims["P436"])
                                .unwrap()
                                .replace("P436", "P435")
                                .replace(
                                    "8e8a594f-2175-38c7-a871-abb68ec363e7",
                                    "f4a8b9d2-3c1e-4f5a-9b6c-7d8e9f0a1b2c",
                                );
                            claims["P435"] = serde_json::from_str(&work).unwrap();
                        })
                    } else if mode.other_mbid && ids == "Q55825905" {
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
                    } else if mode.recording_item_links_disambiguation && ids == "Q139827657" {
                        edited(&file, |v| {
                            v["entities"][&ids]["sitelinks"]["enwiki"] = json!({
                                "site": "enwiki", "title": "Get Lucky", "badges": [],
                                "url": "https://en.wikipedia.org/wiki/Get_Lucky",
                            })
                        })
                    } else {
                        fixture(&file)
                    };
                    (200, 60, body)
                }
                "wbgetclaims" => {
                    assert_eq!(query("property"), "P18");
                    let entity = query("entity");
                    let mut body = if mode.album_has_image && entity == "Q283221" {
                        fixture("wd-p18-Q93341.json")
                    } else {
                        fixture(&format!("wd-p18-{entity}.json"))
                    };
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
                            ("en.wikipedia.org", "Get_Lucky") => {
                                "enwiki-summary-get-lucky-disambiguation.json"
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
                        let body = match mode.daft_punk_gpl {
                            GplFile::No => fixture(file),
                            _ if file != "commons-daft-punk.json" => fixture(file),
                            gpl => edited(file, |v| {
                                let meta = v["query"]["pages"][0]["imageinfo"][0]["extmetadata"]
                                    .as_object_mut()
                                    .unwrap();
                                meta.remove("License");
                                if gpl == GplFile::WithoutAuthor {
                                    meta.remove("Artist");
                                }
                                meta.insert(
                                    "LicenseShortName".into(),
                                    json!({"value": "GPL", "source": "commons-desc-page"}),
                                );
                                meta.insert(
                                    "LicenseUrl".into(),
                                    json!({"value": "http://www.gnu.org/licenses/gpl.html", "source": "commons-desc-page"}),
                                );
                            }),
                        };
                        (200, 60, body)
                    }
                }
            }
            (host, path) => panic!("unexpected request {host}{path}"),
        };
        let asked = format!("{host}{path}?{}", url.query().unwrap_or_default());
        let (status, body) = match mode.gone {
            Some(part) if asked.contains(part) => (410, b"{}".to_vec()),
            _ => (status, body),
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
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            transport,
        ),
        config: ResolverConfig {
            music_enrichment: enrich,
            ..Default::default()
        },
    }
}
/// [`web`] with music enrichment and its time budget (`limits.music_enrichment_timeout_ms`).
fn web_within(transport: Arc<Recorded>, timeout_ms: u64) -> WebResolver {
    let mut web = web(transport, true);
    web.config.music_enrichment_timeout_ms = timeout_ms;
    web
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
    // The article's cover is never the card's: with no free file on Commons, its cover is the
    // Cover Art Archive's.
    assert_eq!(album.image.as_deref(), Some(KIND_OF_BLUE_COVER));
    assert_eq!(
        album.description.as_deref(),
        Some(extract("enwiki-summary-kind-of-blue.json").as_str())
    );
    assert_eq!(
        credits(&album),
        json!([
            album_cover_credit(),
            article_credit("https://en.wikipedia.org/wiki/Kind_of_Blue", None)
        ])
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
            "coverartarchive.org",
            "en.wikipedia.org",
            "musicbrainz.org",
            "www.wikidata.org"
        ]
    );
    // The album alone asks the Cover Art Archive, once.
    assert_eq!(
        t.to("coverartarchive.org"),
        ["coverartarchive.org/release-group/8e8a594f-2175-38c7-a871-abb68ec363e7/front-500?"]
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
/// Get Lucky's card asks exactly: MusicBrainz 2 (the recording, its works), Wikidata 6 (search,
/// read-back, the recording item's pages and image claim, the work item's read-back and pages),
/// English Wikipedia 1 (the work's article; the recording item has none), nothing of Commons.
#[tokio::test]
async fn a_recordings_requests_are_bounded() {
    let t = Recorded::new(Mode::default());
    card(&web(t.clone(), true), GET_LUCKY).await;
    let wd = "www.wikidata.org/w/api.php?action=";
    let mut expected = vec![
        "musicbrainz.org/ws/2/recording/833f00e1-781f-4edd-90e4-e52712618862?inc=url-rels+isrcs+artist-credits+genres+ratings&fmt=json".to_owned(),
        "musicbrainz.org/ws/2/recording/833f00e1-781f-4edd-90e4-e52712618862?inc=work-rels+work-level-rels+url-rels&fmt=json".to_owned(),
        format!("{wd}query&list=search&srsearch=haswbstatement%3AP4404%3D833f00e1-781f-4edd-90e4-e52712618862&srlimit=5&format=json"),
        format!("{wd}wbgetentities&ids=Q139827657&props=claims%7Csitelinks%2Furls&format=json"),
        format!("{wd}wbgetentities&ids=Q139827657&props=sitelinks%2Furls%7Cdescriptions&languages=en&format=json"),
        format!("{wd}wbgetclaims&entity=Q139827657&property=P18&format=json"),
        format!("{wd}wbgetentities&ids=Q11510350&props=claims%7Csitelinks%2Furls&format=json"),
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
        "musicbrainz.org/ws/2/artist/b5416a54-4941-4fcf-b2a0-7f98da6f9344?inc=url-rels+genres+ratings&fmt=json".to_owned(),
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
    assert!(pinakes_core::music_enrichment::is_disambiguation(
        &disambiguation,
        "Get_Lucky"
    ));
    assert!(!pinakes_core::music_enrichment::is_disambiguation(
        &redirect,
        "Richard_Colburn"
    ));
}

/// The item an entity's own `wikidata` relation names is read back and checked like a found one
/// (`is_own`): a recording's relation to its song's item (a single, a work: no recording ID)
/// gives the recording nothing of that item, and the recording takes its work's article as when
/// it has no item, naming the work, never an image; a release's relation to its album's item
/// (the release group's, P436) gives the release nothing. Neither looks for another item.
#[tokio::test]
async fn an_item_its_own_relation_names_is_checked_like_a_found_one() {
    let t = Recorded::new(Mode {
        recording_names_its_song: true,
        ..Default::default()
    });
    let song = card(&web(t.clone(), true), GET_LUCKY).await;
    assert_eq!(song.image, None, "a work's image is never its recording's");
    assert_eq!(
        song.description.as_deref(),
        Some(extract("enwiki-summary-get-lucky-song.json").as_str())
    );
    assert_eq!(credits(&song), json!([song_credit()]));
    let wikidata = t.to("www.wikidata.org");
    assert!(
        wikidata
            .iter()
            .all(|s| !s.contains("action=query") && !s.contains("wbgetclaims")),
        "{wikidata:?}"
    );
    assert!(
        wikidata
            .iter()
            .any(|s| s.contains("ids=Q11510350&props=claims")),
        "{wikidata:?}"
    );

    let t = Recorded::new(Mode::default());
    let plain = card(
        &web(Recorded::new(Mode::default()), false),
        KIND_OF_BLUE_RELEASE,
    )
    .await;
    let release = card(&web(t.clone(), true), KIND_OF_BLUE_RELEASE).await;
    assert_eq!(release.name.as_deref(), Some("Kind of Blue"));
    // Nothing of the album's item; its own front cover from the Cover Art Archive.
    assert_eq!(
        (release.image.as_deref(), &release.description),
        (Some(KIND_OF_BLUE_RELEASE_COVER), &plain.description)
    );
    assert_eq!(
        credits(&release),
        json!([cover_credit("4c8af71f-5746-4981-92d7-0d6c64db1155")])
    );
    assert!(release.unread_enriched.is_empty());
    assert_eq!(
        sorted(t.seen()),
        [
            "coverartarchive.org/release/4c8af71f-5746-4981-92d7-0d6c64db1155/front-500?",
            "musicbrainz.org/ws/2/release/4c8af71f-5746-4981-92d7-0d6c64db1155?inc=url-rels+release-groups+artist-credits+genres&fmt=json",
            "www.wikidata.org/w/api.php?action=wbgetentities&ids=Q283221&props=claims%7Csitelinks%2Furls&format=json",
        ]
    );
}

/// The card of `uri`, whose answers are as `mode` says, takes nothing of item `qid` (an item of
/// another kind of entity) and asks nothing of it but its read-back: the card is as MusicBrainz
/// alone makes it, with no credit and nothing unread (an album's cover, by its MBID alone, is
/// none here).
async fn takes_nothing_of(uri: &str, mode: Mode, qid: &str) {
    let plain = card(&web(Recorded::new(Mode::default()), false), uri).await;
    let t = Recorded::new(Mode {
        cover_art: CoverArt::None,
        ..mode
    });
    let doc = card(&web(t.clone(), true), uri).await;
    assert_eq!(
        (&doc.image, &doc.description),
        (&plain.image, &plain.description),
        "{uri} took {qid}'s"
    );
    assert_eq!(credits(&doc), json!([]), "{uri}, {qid}");
    assert!(doc.unread_enriched.is_empty());
    let seen = t.seen();
    assert!(
        seen.iter().any(|s| s.contains(&format!(
            "action=wbgetentities&ids={qid}&props=claims%7Csitelinks%2Furls&"
        ))),
        "{seen:?}"
    );
    assert!(
        seen.iter().all(|s| !s.contains("%7Cdescriptions")
            && !s.contains("wbgetclaims")
            && !s.contains("wikipedia.org")
            && !s.starts_with("commons.")),
        "{seen:?}"
    );
}
/// An item of another kind of entity is no card's, by the identifiers it carries
/// (`music_enrichment::ItemKind`): an artist whose own relation names an album's item (Kind of
/// Blue's, P436) takes nothing of it.
#[tokio::test]
async fn an_artists_relation_to_an_albums_item_gives_nothing() {
    let mode = Mode {
        relation: Some(("mb-artist-daft-punk.json", "Q283221")),
        ..Default::default()
    };
    takes_nothing_of(DAFT_PUNK, mode, "Q283221").await;
}
/// A release group's item that is a work's too (a single's song's item, a rock opera's: a work
/// ID, P435, beside the release group's, P436) is still the release group's: Kind of Blue's item
/// with a work ID added gives the album its article as without.
#[tokio::test]
async fn a_release_groups_item_that_is_a_works_too_is_its_own() {
    let t = Recorded::new(Mode {
        album_is_a_work: true,
        ..Default::default()
    });
    let album = card(&web(t.clone(), true), KIND_OF_BLUE).await;
    assert_eq!(
        album.description.as_deref(),
        Some(extract("enwiki-summary-kind-of-blue.json").as_str())
    );
    assert_eq!(
        credits(&album),
        json!([
            album_cover_credit(),
            article_credit("https://en.wikipedia.org/wiki/Kind_of_Blue", None)
        ])
    );
}
/// A release group whose own relation names an artist's item (Miles Davis's, P434) takes nothing
/// of it.
#[tokio::test]
async fn a_release_groups_relation_to_an_artists_item_gives_nothing() {
    let mode = Mode {
        relation: Some(("mb-release-group-kind-of-blue.json", "Q93341")),
        ..Default::default()
    };
    takes_nothing_of(KIND_OF_BLUE, mode, "Q93341").await;
}
/// A release whose own relation names a recording's item (Get Lucky's, P4404) takes nothing of
/// it.
#[tokio::test]
async fn a_releases_relation_to_a_recordings_item_gives_nothing() {
    let mode = Mode {
        relation: Some(("mb-release-kind-of-blue.json", "Q139827657")),
        ..Default::default()
    };
    takes_nothing_of(KIND_OF_BLUE_RELEASE, mode, "Q139827657").await;
}
/// A recording takes its work's text only from the work's item: when the work's relation names
/// a recording's item (Get Lucky's own, P4404), an album's (Kind of Blue's, P436) or an
/// artist's (Miles Davis's, P434), the recording, which no item names by its MBID, takes
/// nothing. Get Lucky's real work item, its song's and single's (P436, class single), still
/// gives its article (`a_recording_without_an_item_takes_its_works_article`).
#[tokio::test]
async fn a_works_relation_to_an_item_of_another_kind_gives_nothing() {
    for qid in ["Q139827657", "Q283221", "Q93341"] {
        let mode = Mode {
            mbid_search: Search::Empty,
            relation: Some(("mb-recording-get-lucky-works.json", qid)),
            ..Default::default()
        };
        takes_nothing_of(GET_LUCKY, mode, qid).await;
    }
}

/// A 410 from Wikidata or MusicBrainz in the enrichment says the item or the work is gone for
/// good, as a 404 does: the field is missing, never unread (which would keep a stale stored value
/// and read the card again early, forever). Daft Punk's item read back, Sefa's MBID search, Get
/// Lucky's works and Sefa's image claim each answer 410 in turn.
#[tokio::test]
async fn a_hubs_410_is_missing_never_unread() {
    let plain = |uri: &'static str| async move {
        card(&web(Recorded::new(Mode::default()), false), uri).await
    };
    for (uri, gone) in [
        (DAFT_PUNK, "ids=Q185828&props=claims"),
        (SEFA, "srsearch=haswbstatement"),
        (GET_LUCKY, "inc=work-rels"),
    ] {
        let t = Recorded::new(Mode {
            gone: Some(gone),
            mbid_search: if uri == GET_LUCKY {
                Search::Empty
            } else {
                Search::Recorded
            },
            discogs_search: Search::Empty,
            ..Default::default()
        });
        let doc = card(&web(t.clone(), true), uri).await;
        let plain = plain(uri).await;
        assert!(
            doc.unread_enriched.is_empty(),
            "{gone}: {:?}",
            doc.unread_enriched
        );
        assert_eq!(
            doc.refresh_by.is_some(),
            plain.refresh_by.is_some(),
            "{gone}"
        );
        assert_eq!(
            (&doc.image, &doc.description, credits(&doc)),
            (&plain.image, &plain.description, json!([])),
            "{gone}"
        );
        assert!(t.seen().iter().any(|s| s.contains(gone)), "{gone}");
    }
    // The image claim gone: no image, the description as read.
    let t = Recorded::new(Mode {
        gone: Some("wbgetclaims&entity=Q55825905"),
        ..Default::default()
    });
    let sefa = card(&web(t.clone(), true), SEFA).await;
    assert!(sefa.unread_enriched.is_empty());
    assert_eq!(
        (sefa.image.as_deref(), sefa.description.as_deref()),
        (None, Some(SEFA_DESCRIPTION))
    );
    assert_eq!(credits(&sefa), json!([item_credit("Q55825905", None)]));
    // The item's pages gone: no description, the image as read.
    let t = Recorded::new(Mode {
        gone: Some("ids=Q55825905&props=sitelinks"),
        ..Default::default()
    });
    let sefa = card(&web(t.clone(), true), SEFA).await;
    let plain = plain(SEFA).await;
    assert!(sefa.unread_enriched.is_empty());
    assert_eq!(
        (sefa.image.as_deref(), &sefa.description),
        (Some(SEFA_IMAGE), &plain.description)
    );
    assert_eq!(credits(&sefa), json!([sefa_image_credit()]));
}

/// An item that is a disambiguation page's (P31 Q4167410; a list article's, Q13406463, alike) is
/// no entity's: no description (never Wikidata's "Wikimedia disambiguation page"), no image, no
/// other Wikipedia. Read back, it ends there; an item that does not say so but whose English
/// article is a disambiguation page gives nothing either.
#[tokio::test]
async fn a_disambiguation_item_gives_nothing() {
    let plain = card(&web(Recorded::new(Mode::default()), false), DAFT_PUNK).await;
    for unclassed in [false, true] {
        let t = Recorded::new(Mode {
            names_disambiguation: true,
            disambiguation_unclassed: unclassed,
            ..Default::default()
        });
        let band = card(&web(t.clone(), true), DAFT_PUNK).await;
        assert_eq!(band.name.as_deref(), Some("Daft Punk"));
        assert_eq!(
            (&band.image, &band.description),
            (&None, &plain.description),
            "unclassed: {unclassed}"
        );
        assert_eq!(credits(&band), json!([]));
        assert!(band.unread_enriched.is_empty());
        let seen = t.seen();
        assert!(seen.iter().all(|s| !s.contains("Q185828")), "{seen:?}");
        assert!(
            seen.iter()
                .all(|s| !s.starts_with("commons.") && !s.starts_with("de.wikipedia.org")),
            "{seen:?}"
        );
        assert_eq!(
            t.to("en.wikipedia.org"),
            if unclassed {
                vec!["en.wikipedia.org/api/rest_v1/page/summary/Get_Lucky?".to_owned()]
            } else {
                vec![]
            },
            "{seen:?}"
        );
    }
    // A recording whose own item's English article is a disambiguation page takes nothing of
    // that item (not its English description), and its work's article as without an item.
    let t = Recorded::new(Mode {
        recording_item_links_disambiguation: true,
        ..Default::default()
    });
    let song = card(&web(t.clone(), true), GET_LUCKY).await;
    assert_eq!(song.image, None);
    assert_eq!(
        song.description.as_deref(),
        Some(extract("enwiki-summary-get-lucky-song.json").as_str())
    );
    assert_eq!(credits(&song), json!([song_credit()]));
    assert!(t.seen().iter().any(|s| s.ends_with("/summary/Get_Lucky?")));
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

/// A file under the GNU GPL is shown on a MusicBrainz card as on a Wikidata item's, by the same
/// rule: credited to its author with the licence Commons' `{{GPL}}` grants (version 2 or
/// later) and its text; without its author it is refused.
#[tokio::test]
async fn a_gpl_file_is_shown_with_its_author_only() {
    let t = Recorded::new(Mode {
        daft_punk_gpl: GplFile::Credited,
        ..Default::default()
    });
    let band = card(&web(t, true), DAFT_PUNK).await;
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
                "license": "GPL-2.0-or-later",
                "license_url": "https://www.gnu.org/licenses/old-licenses/gpl-2.0.html",
                "lang": null, "excerpted": false, "modified": false, "subject": null,
            },
            article_credit("https://en.wikipedia.org/wiki/Daft_Punk", None),
        ])
    );
    let t = Recorded::new(Mode {
        daft_punk_gpl: GplFile::WithoutAuthor,
        ..Default::default()
    });
    let band = card(&web(t, true), DAFT_PUNK).await;
    assert_eq!(band.image, None);
    assert_eq!(
        credits(&band),
        json!([article_credit(
            "https://en.wikipedia.org/wiki/Daft_Punk",
            None
        )])
    );
    assert!(band.unread_enriched.is_empty());
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
    // MusicBrainz, the item's read-back, pages and image claim, Wikipedia, Commons.
    assert_eq!(seen.len(), 6, "{seen:?}");
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
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
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
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
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
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
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

/// An album with no free file on Commons (Kind of Blue's item has no image) takes the front cover
/// the Cover Art Archive holds for it, by its MBID, in one request: a release group the
/// thumbnail of the release the archive's redirect names, credited to that release's cover art
/// page; a release whose MusicBrainz answer says the archive shows its front cover, its own. The
/// redirect is never followed (archive.org is never asked), and nothing else is asked.
#[tokio::test]
async fn an_album_without_a_free_file_takes_its_cover_from_the_cover_art_archive() {
    let t = Recorded::new(Mode::default());
    let album = card(&web(t.clone(), true), KIND_OF_BLUE).await;
    assert_eq!(album.image.as_deref(), Some(KIND_OF_BLUE_COVER));
    assert_eq!(credits(&album)[0], album_cover_credit());
    assert!(album.unread_enriched.is_empty());
    assert_eq!(
        t.to("coverartarchive.org"),
        ["coverartarchive.org/release-group/8e8a594f-2175-38c7-a871-abb68ec363e7/front-500?"]
    );
    assert!(t
        .hosts()
        .iter()
        .all(|h| !h.ends_with("archive.org") || h == "coverartarchive.org"));
    // The item's image claim was read, and named no file.
    assert!(t
        .seen()
        .iter()
        .any(|s| s.contains("wbgetclaims&entity=Q283221")));

    let t = Recorded::new(Mode::default());
    let release = card(&web(t.clone(), true), KIND_OF_BLUE_RELEASE).await;
    assert_eq!(release.image.as_deref(), Some(KIND_OF_BLUE_RELEASE_COVER));
    assert_eq!(
        credits(&release),
        json!([cover_credit("4c8af71f-5746-4981-92d7-0d6c64db1155")])
    );
    assert_eq!(
        t.to("coverartarchive.org"),
        ["coverartarchive.org/release/4c8af71f-5746-4981-92d7-0d6c64db1155/front-500?"]
    );
    // Artists and recordings have no cover: the archive is not asked.
    let t = Recorded::new(Mode::default());
    for uri in [MILES, DAFT_PUNK, GET_LUCKY] {
        card(&web(t.clone(), true), uri).await;
    }
    assert!(t.to("coverartarchive.org").is_empty(), "{:?}", t.seen());
}

/// A free file on Commons wins: an album whose item names one shows it. The Cover Art Archive,
/// asked at the same time (at most once), is not used, nor waited for.
#[tokio::test]
async fn a_free_commons_file_wins_over_the_cover() {
    let t = Recorded::new(Mode {
        album_has_image: true,
        ..Default::default()
    });
    let album = card(&web(t.clone(), true), KIND_OF_BLUE).await;
    assert_eq!(album.image.as_deref(), Some(MILES_IMAGE));
    assert_eq!(credits(&album)[0], miles_credits()[0]);
    assert!(album.unread_enriched.is_empty());
    assert!(t.to("coverartarchive.org").len() <= 1, "{:?}", t.seen());
    // A cover that comes late does not hold the card: the Commons file is there at once, long
    // before the budget ends (the archive's answer would come after it).
    let t = Recorded::new(Mode {
        album_has_image: true,
        cover_art_delay_ms: 30_000,
        ..Default::default()
    });
    let started = std::time::Instant::now();
    let album = card(&web_within(t.clone(), 12_000), KIND_OF_BLUE).await;
    assert_eq!(album.image.as_deref(), Some(MILES_IMAGE));
    assert!(album.unread_enriched.is_empty());
    // Well under the egress's own 10 s cap on a request, which the join would wait for.
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
}

/// Each part of an album's enrichment ends on its own. A cover that comes after the budget
/// leaves the image unread and the article found stays; a Wikidata that refuses the album's
/// item for now (503) leaves the album its cover, the image and description unread.
#[tokio::test]
async fn each_part_of_an_album_reading_ends_on_its_own() {
    let t = Recorded::new(Mode {
        cover_art_delay_ms: 30_000,
        ..Default::default()
    });
    let album = card(&web_within(t.clone(), 2500), KIND_OF_BLUE).await;
    assert_eq!(album.image, None);
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
    assert_eq!(
        serde_json::to_value(&album.unread_enriched).unwrap(),
        json!(["image"])
    );
    let t = Recorded::new(Mode {
        album_item_busy: true,
        ..Default::default()
    });
    let album = card(&web(t.clone(), true), KIND_OF_BLUE).await;
    assert_eq!(album.image.as_deref(), Some(KIND_OF_BLUE_COVER));
    assert_eq!(credits(&album), json!([album_cover_credit()]));
    assert_eq!(
        serde_json::to_value(&album.unread_enriched).unwrap(),
        json!(["image", "description"])
    );
}

/// A busy Wikidata leaves an album its cover. Every host is paced (one request at a time), so
/// the cover is asked at the same time as the album's item, not behind it: here the item's
/// answers come after the card's enrichment budget, the cover's in time. The card shows the
/// cover and its credit; image and description stay unread (a stored card's come back), and the
/// card is read again early.
#[tokio::test]
async fn a_busy_wikidata_leaves_the_album_its_cover() {
    let t = Recorded::new(Mode {
        wikidata_delay_ms: 3000,
        ..Default::default()
    });
    let album = card(&web_within(t.clone(), 1000), KIND_OF_BLUE).await;
    assert_eq!(
        album.image.as_deref(),
        Some(KIND_OF_BLUE_COVER),
        "{:?}",
        t.seen()
    );
    assert_eq!(credits(&album), json!([album_cover_credit()]));
    assert_eq!(
        serde_json::to_value(&album.unread_enriched).unwrap(),
        json!(["image", "description"])
    );
    assert!(album.refresh_by.is_some());
    assert_eq!(t.to("coverartarchive.org").len(), 1, "{:?}", t.seen());
    // Artists and recordings have no cover: a busy Wikidata leaves them unread, as before.
    let t = Recorded::new(Mode {
        wikidata_delay_ms: 3000,
        ..Default::default()
    });
    let artist = card(&web_within(t.clone(), 1000), MILES).await;
    assert_eq!((&artist.image, credits(&artist)), (&None, json!([])));
    assert_eq!(
        serde_json::to_value(&artist.unread_enriched).unwrap(),
        json!(["image", "description"])
    );
    assert!(t.to("coverartarchive.org").is_empty(), "{:?}", t.seen());
}

/// Through the queue, while Wikidata is busy: a card with nothing stored keeps the cover; a stored
/// free Commons image comes back over it with its credit, as does the stored description.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_stored_commons_image_stays_over_the_cover_while_wikidata_is_busy(pool: PgPool) {
    tag(&pool, &[KIND_OF_BLUE]).await;
    let busy = |album_has_image| Mode {
        album_has_image,
        wikidata_delay_ms: 3000,
        ..Default::default()
    };
    let first = resolve_once(
        &pool,
        &web_within(Recorded::new(busy(true)), 1000),
        KIND_OF_BLUE,
    )
    .await;
    assert_eq!(first.image.as_deref(), Some(KIND_OF_BLUE_COVER));
    assert_eq!(credits(&first), json!([album_cover_credit()]));
    let free = Mode {
        album_has_image: true,
        ..Default::default()
    };
    let stored = resolve_once(&pool, &web(Recorded::new(free), true), KIND_OF_BLUE).await;
    assert_eq!(stored.image.as_deref(), Some(MILES_IMAGE));
    let kept = resolve_once(
        &pool,
        &web_within(Recorded::new(busy(true)), 1000),
        KIND_OF_BLUE,
    )
    .await;
    assert_eq!(kept.image.as_deref(), Some(MILES_IMAGE));
    assert_eq!(kept.description, stored.description);
    assert_eq!(credits(&kept), credits(&stored));
}

/// A release MusicBrainz says has no front cover in the archive asks the archive nothing. A
/// release group the archive has no cover for (404) has no image and no image credit, and is
/// not read again early; a busy archive (503) leaves the image unread, so a stored cover stays.
#[tokio::test]
async fn no_cover_is_none_and_a_busy_archive_is_unread() {
    let t = Recorded::new(Mode {
        release_without_front: true,
        ..Default::default()
    });
    let release = card(&web(t.clone(), true), KIND_OF_BLUE_RELEASE).await;
    assert_eq!((&release.image, credits(&release)), (&None, json!([])));
    assert!(release.unread_enriched.is_empty());
    assert!(t.to("coverartarchive.org").is_empty(), "{:?}", t.seen());

    let t = Recorded::new(Mode {
        cover_art: CoverArt::None,
        ..Default::default()
    });
    let album = card(&web(t.clone(), true), KIND_OF_BLUE).await;
    assert_eq!(album.image, None);
    assert_eq!(
        credits(&album),
        json!([article_credit(
            "https://en.wikipedia.org/wiki/Kind_of_Blue",
            None
        )])
    );
    assert!(album.unread_enriched.is_empty() && album.refresh_by.is_none());
    assert_eq!(t.to("coverartarchive.org").len(), 1);

    let t = Recorded::new(Mode {
        cover_art: CoverArt::Busy,
        ..Default::default()
    });
    let web = web(t.clone(), true);
    for _ in 0..2 {
        let album = card(&web, KIND_OF_BLUE).await;
        assert_eq!(album.image, None);
        assert_eq!(
            serde_json::to_value(&album.unread_enriched).unwrap(),
            json!(["image"])
        );
        assert!(album.refresh_by.is_some());
    }
    // The second card, right after the first, takes the answer the egress cached a moment.
    assert_eq!(t.to("coverartarchive.org").len(), 1, "{:?}", t.seen());
}

/// A stored cover stays through a busy archive and goes when the archive says there is none.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_stored_cover_stays_while_the_archive_is_busy(pool: PgPool) {
    tag(&pool, &[KIND_OF_BLUE]).await;
    let stored = resolve_once(
        &pool,
        &web(Recorded::new(Mode::default()), true),
        KIND_OF_BLUE,
    )
    .await;
    assert_eq!(stored.image.as_deref(), Some(KIND_OF_BLUE_COVER));
    let busy = Mode {
        cover_art: CoverArt::Busy,
        ..Default::default()
    };
    let kept = resolve_once(&pool, &web(Recorded::new(busy), true), KIND_OF_BLUE).await;
    assert_eq!(kept.image.as_deref(), Some(KIND_OF_BLUE_COVER));
    assert_eq!(
        serde_json::to_value(&kept.credits).unwrap()[0],
        album_cover_credit()
    );
    let none = Mode {
        cover_art: CoverArt::None,
        ..Default::default()
    };
    let gone = resolve_once(&pool, &web(Recorded::new(none), true), KIND_OF_BLUE).await;
    assert_eq!(gone.image, None);
    assert!(gone
        .credits
        .iter()
        .all(|c| serde_json::to_value(c).unwrap()["field"] != "image"));
}

/// MusicBrainz's rating comes with the entity read (`inc=ratings`), no other request: a release
/// group's, an artist's and a recording's, with one decimal (Kind of Blue's 4.45 is 4.5) and the
/// number of votes. An artist nobody rated (Sefa: a null value, 0 votes) and a release (no
/// rating of its own) have neither fact, whether or not the card is enriched.
#[tokio::test]
async fn musicbrainz_ratings_are_facts_of_the_card() {
    for enrich in [false, true] {
        let t = Recorded::new(Mode::default());
        let web = web(t.clone(), enrich);
        for (uri, rating) in [
            (KIND_OF_BLUE, Some(("4.5", "43"))),
            (MILES, Some(("4.8", "21"))),
            (GET_LUCKY, Some(("3.9", "25"))),
            (SEFA, None),
            (KIND_OF_BLUE_RELEASE, None),
        ] {
            let doc = card(&web, uri).await;
            let facts = (
                doc.attributes.get("rating").map(String::as_str),
                doc.attributes.get("rating_count").map(String::as_str),
            );
            assert_eq!(
                facts,
                rating.map_or((None, None), |(r, n)| (Some(r), Some(n))),
                "{uri}"
            );
        }
        // One entity read each: the rating asks nothing more.
        let entity_reads: Vec<String> = t
            .to("musicbrainz.org")
            .into_iter()
            .filter(|s| !s.contains("work-rels"))
            .collect();
        assert_eq!(entity_reads.len(), 5, "{entity_reads:?}");
        assert!(entity_reads
            .iter()
            .filter(|s| !s.contains("/release/"))
            .all(|s| s.contains("+ratings&")));
        assert!(entity_reads
            .iter()
            .filter(|s| s.contains("/release/"))
            .all(|s| !s.contains("ratings")));
    }
}

/// A test source that answers the keys it is given, per link.
struct Keys(std::collections::BTreeMap<String, Vec<String>>);
#[async_trait::async_trait]
impl pinakes_core::also_on::Source for Keys {
    fn id(&self) -> &'static str {
        "test_keys"
    }
    fn applies(&self, subject: &pinakes_core::also_on::Subject) -> bool {
        self.0.contains_key(&subject.uri)
    }
    async fn lookup(
        &self,
        _: &pinakes_core::also_on::Context,
        subject: &pinakes_core::also_on::Subject,
    ) -> anyhow::Result<pinakes_core::also_on::Found> {
        Ok(pinakes_core::also_on::Found {
            keys: self.0[&subject.uri]
                .iter()
                .map(|key| pinakes_core::also_on::Key {
                    key: key.clone(),
                    via: "wikidata".into(),
                })
                .collect(),
            ..Default::default()
        })
    }
}

/// A MusicBrainz artist's or release group's page keys the Wikidata item its own `wikidata`
/// relation names (`wikidata:{Q-id}`), as its MBID, once the item, read back, is its own; a
/// release's page never does, even when its relation names its album's item (a key never
/// crosses levels); a relation that names no item, or an item of another kind (the album's
/// relation to its artist's item), keys nothing. In groups the key is one more single-object
/// key: a page that names another item for the same release group stays apart from the album's
/// group (a conflict, counted), and a page with only the item's key joins it.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_musicbrainz_page_keys_its_own_wikidata_item_and_a_disagreeing_one_stays_apart(
    pool: PgPool,
) {
    let Some(entity_source) = pinakes_core::also_on::registry()
        .into_iter()
        .find(|s| s.id() == "musicbrainz_entity")
    else {
        return;
    };
    let other = "https://open.spotify.com/album/0000000000000000000001";
    let item_only = "https://open.spotify.com/album/0000000000000000000003";
    let group = "mb:release-group:8e8a594f-2175-38c7-a871-abb68ec363e7";
    let keys = Keys(
        [
            (other, vec![group, "wikidata:Q999999"]),
            (item_only, vec!["wikidata:Q283221"]),
        ]
        .into_iter()
        .map(|(uri, keys)| {
            (
                uri.to_owned(),
                keys.into_iter().map(str::to_owned).collect(),
            )
        })
        .collect(),
    );
    let mut ids = std::collections::BTreeMap::new();
    for (uri, ty) in [
        (KIND_OF_BLUE, "MusicAlbum"),
        (KIND_OF_BLUE_RELEASE, "MusicAlbum"),
        (DAFT_PUNK, "MusicGroup"),
        (other, "MusicAlbum"),
        (item_only, "MusicAlbum"),
    ] {
        let id = pinakes_core::store::ensure_resource(&pool, uri)
            .await
            .unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.set_type(ty);
        d.name = Some("Kind of Blue".into());
        pinakes_core::store::save_document(&pool, &id, 0, &d)
            .await
            .unwrap();
        ids.insert(uri, id);
    }
    let t = Recorded::new(Mode::default());
    let cx = pinakes_core::also_on::Context {
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            t.clone(),
        ),
        config: Arc::new(Config::default()),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let sources: Vec<Arc<dyn pinakes_core::also_on::Source>> = vec![entity_source, Arc::new(keys)];
    pinakes_core::also_on::drain(&pool, &cx, &sources, 100)
        .await
        .unwrap();
    let keys_of = |uri: &str| {
        let (pool, id) = (pool.clone(), ids[uri].clone());
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT key FROM resource_key WHERE resource_id=$1 AND source='musicbrainz_entity' ORDER BY key COLLATE \"C\"",
            )
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(keys_of(KIND_OF_BLUE).await, [group, "wikidata:Q283221"]);
    assert_eq!(
        keys_of(DAFT_PUNK).await,
        [
            "mb:artist:056e4f3e-d505-4dad-8ec1-d04f521cbb56",
            "wikidata:Q185828"
        ]
    );
    let release = keys_of(KIND_OF_BLUE_RELEASE).await;
    assert!(release.contains(&group.to_owned()), "{release:?}");
    assert!(
        release.iter().all(|k| !k.starts_with("wikidata:")),
        "{release:?}"
    );
    // The entity is read once a page: the key comes from the same answer.
    let reads = t.to("musicbrainz.org");
    assert_eq!(reads.len(), 3, "{reads:?}");

    common::drain_groups(&pool, &Default::default()).await;
    let group_of = |uri: &str| {
        let (pool, id) = (pool.clone(), ids[uri].clone());
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT group_id FROM group_unit WHERE resource_id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let album = group_of(KIND_OF_BLUE).await;
    assert!(album.is_some());
    assert_eq!(group_of(item_only).await, album, "the item alone joins");
    assert_eq!(
        group_of(KIND_OF_BLUE_RELEASE).await,
        album,
        "its release joins"
    );
    assert_ne!(group_of(other).await, album, "another item stays apart");
    let refused: i64 = sqlx::query_scalar("SELECT count(*) FROM group_input WHERE key_conflicts>0")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(refused > 0);

    // A relation that is no item link keys nothing.
    let t = Recorded::new(Mode {
        album_relation_not_an_item: true,
        ..Default::default()
    });
    let cx = pinakes_core::also_on::Context {
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            t.clone(),
        ),
        config: Arc::new(Config::default()),
    };
    let entity_source = pinakes_core::also_on::registry()
        .into_iter()
        .find(|s| s.id() == "musicbrainz_entity")
        .unwrap();
    let subject = pinakes_core::also_on::Subject {
        id: ids[KIND_OF_BLUE].clone(),
        uri: KIND_OF_BLUE.into(),
        status: "ok".into(),
        type_name: Some("MusicAlbum".into()),
        doc: None,
        keys: vec![],
        first_seen_at: None,
        candidates: vec![],
    };
    let found = entity_source.lookup(&cx, &subject).await.unwrap();
    let keys: Vec<&str> = found.keys.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, [group]);
    // A relation to another kind's item (Miles Davis's, an artist's) keys nothing: it is read
    // back and is not the album's.
    let t = Recorded::new(Mode {
        relation: Some(("mb-release-group-kind-of-blue.json", "Q93341")),
        ..Default::default()
    });
    let cx = pinakes_core::also_on::Context {
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            t.clone(),
        ),
        config: Arc::new(Config::default()),
    };
    let found = entity_source.lookup(&cx, &subject).await.unwrap();
    let keys: Vec<&str> = found.keys.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, [group]);
    assert!(
        t.seen()
            .iter()
            .any(|s| s.contains("ids=Q93341&props=claims")),
        "{:?}",
        t.seen()
    );
}

/// The `musicbrainz_entity` source's answer for `uri`, with answers as `mode` says and the keys
/// it recorded before (`stored`).
async fn entity_lookup(
    uri: &str,
    mode: Mode,
    stored: &[&str],
) -> Option<pinakes_core::also_on::Found> {
    let source = pinakes_core::also_on::registry()
        .into_iter()
        .find(|s| s.id() == "musicbrainz_entity")?;
    let cx = pinakes_core::also_on::Context {
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            Recorded::new(mode),
        ),
        config: Arc::new(Config::default()),
    };
    let subject = pinakes_core::also_on::Subject {
        id: identity(uri).unwrap().0,
        uri: uri.into(),
        status: "ok".into(),
        type_name: None,
        doc: None,
        keys: stored
            .iter()
            .map(|k| ((*k).to_owned(), "musicbrainz_entity".to_owned()))
            .collect(),
        first_seen_at: None,
        candidates: vec![],
    };
    Some(source.lookup(&cx, &subject).await.unwrap())
}
fn keys_of(found: &pinakes_core::also_on::Found) -> Vec<&str> {
    found.keys.iter().map(|k| k.key.as_str()).collect()
}

/// A key is stricter than the enrichment: an album's relation keys only an item that is exactly
/// a release group's and names this one release group (P436), never a song's item that is its
/// single's too (Wikidata often keys a single's track by it through its streaming IDs, so the
/// track would join the album) nor an item with no identifier yet. Their text still counts for
/// the card's description, as before.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_album_is_keyed_only_by_an_item_that_is_exactly_its_own(pool: PgPool) {
    let group = "mb:release-group:8e8a594f-2175-38c7-a871-abb68ec363e7";
    let Some(own) = entity_lookup(KIND_OF_BLUE, Mode::default(), &[]).await else {
        return;
    };
    assert_eq!(keys_of(&own), [group, "wikidata:Q283221"]);
    for album_item in [AlbumItem::SongAndAlbum, AlbumItem::Bare] {
        let mode = Mode {
            album_item,
            ..Default::default()
        };
        let found = entity_lookup(KIND_OF_BLUE, mode, &[]).await.unwrap();
        assert_eq!(keys_of(&found), [group]);
        // The enrichment still takes the item's article.
        let album = card(&web(Recorded::new(mode), true), KIND_OF_BLUE).await;
        assert_eq!(
            album.description.as_deref(),
            Some(extract("enwiki-summary-kind-of-blue.json").as_str())
        );
    }
    // A track keyed by the song's item (as the `wikidata` source keys a Spotify track whose
    // P2207 the item holds) and by its recording does not join the album's group.
    a_track_of_the_item_stays_apart(&pool, AlbumItem::SongAndAlbum).await;
}

/// With Kind of Blue's item read back as `album_item`, a Spotify track keyed by that item and by
/// the recording it names (as the `wikidata` source keys a track whose P2207 the item holds: one
/// P4404, no P435) is not in the album's group.
async fn a_track_of_the_item_stays_apart(pool: &PgPool, album_item: AlbumItem) {
    let track = "https://open.spotify.com/track/0000000000000000000001";
    let recording = format!("mb:recording:{SONG_RECORDING}");
    let keys = Keys(
        [(
            track.to_owned(),
            vec!["wikidata:Q283221".to_owned(), recording],
        )]
        .into_iter()
        .collect(),
    );
    let mut ids = std::collections::BTreeMap::new();
    for (uri, ty) in [(KIND_OF_BLUE, "MusicAlbum"), (track, "MusicRecording")] {
        let id = pinakes_core::store::ensure_resource(pool, uri)
            .await
            .unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.set_type(ty);
        d.name = Some("Kind of Blue".into());
        pinakes_core::store::save_document(pool, &id, 0, &d)
            .await
            .unwrap();
        ids.insert(uri, id);
    }
    let cx = pinakes_core::also_on::Context {
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            Recorded::new(Mode {
                album_item,
                ..Default::default()
            }),
        ),
        config: Arc::new(Config::default()),
    };
    let entity_source = pinakes_core::also_on::registry()
        .into_iter()
        .find(|s| s.id() == "musicbrainz_entity")
        .unwrap();
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(pool)
        .await
        .unwrap();
    let sources: Vec<Arc<dyn pinakes_core::also_on::Source>> = vec![entity_source, Arc::new(keys)];
    pinakes_core::also_on::drain(pool, &cx, &sources, 100)
        .await
        .unwrap();
    common::drain_groups(pool, &Default::default()).await;
    let group_of = |uri: &str| {
        let (pool, id) = (pool.clone(), ids[uri].clone());
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT group_id FROM group_unit WHERE resource_id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (album, song) = (group_of(KIND_OF_BLUE).await, group_of(track).await);
    assert!(album.is_none() || album != song, "{album:?} {song:?}");
}

/// An album's own item that also names one recording (and that recording's Spotify track) is a
/// single's track's too: it keys no album, so the track the `wikidata` source keys by it stays
/// out of the album's group.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_album_item_that_names_a_recording_keys_no_album(pool: PgPool) {
    let group = "mb:release-group:8e8a594f-2175-38c7-a871-abb68ec363e7";
    let mode = Mode {
        album_item: AlbumItem::AlbumWithRecording,
        ..Default::default()
    };
    let Some(found) = entity_lookup(KIND_OF_BLUE, mode, &[]).await else {
        return;
    };
    assert_eq!(keys_of(&found), [group]);
    a_track_of_the_item_stays_apart(&pool, AlbumItem::AlbumWithRecording).await;
}

/// A Wikidata that fails for now (503) never costs a MusicBrainz page its MusicBrainz keys and
/// links: they stand, the item key this source recorded before is kept, and the run comes back
/// after the wait Wikidata named.
#[tokio::test]
async fn a_busy_wikidata_keeps_the_musicbrainz_keys_and_the_stored_item_key() {
    let group = "mb:release-group:8e8a594f-2175-38c7-a871-abb68ec363e7";
    let busy = Mode {
        album_item_busy: true,
        ..Default::default()
    };
    let Some(found) = entity_lookup(KIND_OF_BLUE, busy, &[group, "wikidata:Q283221"]).await else {
        return;
    };
    assert_eq!(keys_of(&found), [group, "wikidata:Q283221"]);
    assert_eq!(found.retry_after, Some(std::time::Duration::from_secs(120)));
    let calm = entity_lookup(KIND_OF_BLUE, Mode::default(), &[])
        .await
        .unwrap();
    assert_eq!(found.links, calm.links);
    assert_eq!(found.facts, calm.facts);
    // A page whose item key was never recorded has none yet.
    let fresh = entity_lookup(KIND_OF_BLUE, busy, &[]).await.unwrap();
    assert_eq!(keys_of(&fresh), [group]);
    assert!(fresh.retry_after.is_some());
}
