//! The same recording on Deezer and TIDAL for a label's upload on YouTube, by its artist, title
//! and length: a match, never an exact ID, so its links are unkeyed (they join no group) and
//! nothing it finds becomes a key.
//!
//! A label upload is a video whose channel is an artist's "Topic" channel (`<Artist> - Topic`) or
//! whose description starts "Provided to YouTube by", as the stored card's oEmbed author and
//! description show. The source reads the video's watch page for its title, channel, length and
//! description (`ytInitialPlayerResponse.videoDetails`; a stored card has no length), then searches
//! Deezer (while `deezer` runs) and TIDAL (while `tidal_isrc` runs, with its credentials) for the
//! artist and title. Of each service's answers, exactly one track must have the same artist and
//! title, compared as `same_recording` compares them (`matching`), and a length within
//! [`MATCH_WINDOW_SECS`]; none or several give nothing for that service (of the first page of
//! answers). TIDAL failing fails the lookup (which keeps every earlier link) unless the run has a
//! Deezer match, which then stands, and the run comes back later; TIDAL refusing the credentials
//! brings no run back early and leaves the links an earlier run found. A page YouTube serves only after
//! a sign-in (`LOGIN_REQUIRED`, which it answers some hosts for some videos) is no
//! answer: the lookup is retried hours later; a page over the cap is nothing, and is not retried.
//! It runs after `deezer` and `tidal_isrc`, and before `editions`.
use super::{
    deezer_api,
    tidal_api::{self, TidalConfig, API as TIDAL_API},
    Context, Found, Source, Subject,
};
use crate::{
    matching::{artist_name, recording_title},
    music::{self, Level, Service},
    music_sources::{derived, duration_secs},
    resolver::{egress::FetchOptions, RetryLater},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// How far a track's length may be from the video's, in seconds.
pub const MATCH_WINDOW_SECS: i64 = 2;
/// The largest watch page read (they are about 1.4 MB); a larger one is refused.
const WATCH_CAP: usize = 3 * 1024 * 1024;
/// When a lookup YouTube answered with a sign-in page is tried again.
const LOGIN_WAIT_SECS: u64 = 6 * 3600;
/// The description YouTube gives a label's upload.
const PROVIDED: &str = "Provided to YouTube by";
/// The suffix of an artist's Topic channel.
const TOPIC: &str = " - Topic";

/// A video's watch page, as far as this source reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Watch {
    /// The player's details.
    Page(Page),
    /// YouTube asks to sign in instead (`playabilityStatus.status` `LOGIN_REQUIRED`).
    LoginRequired,
    /// No details (a removed video, another page).
    Nothing,
}
/// What a watch page says of its video.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    /// The channel's name.
    pub author: String,
    pub description: String,
    pub seconds: i64,
}
/// A label's upload: its artist, title and length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub artist: String,
    pub title: String,
    pub seconds: i64,
}
/// A track a service's search answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    /// Its title with its version (`Bohemian Rhapsody (Live Aid)`).
    pub title: String,
    /// Its first artist.
    pub artist: String,
    pub seconds: i64,
}

pub struct YoutubeMatch;
#[async_trait]
impl Source for YoutubeMatch {
    fn id(&self) -> &'static str {
        "youtube_match"
    }
    fn applies(&self, subject: &Subject) -> bool {
        let video = music::parse(&subject.uri)
            .is_some_and(|l| l.service == Service::YouTube && l.level == Level::Video);
        video
            && subject.doc.as_ref().is_some_and(|doc| {
                doc.attributes
                    .get("author")
                    .is_some_and(|a| a.trim_end().ends_with(TOPIC))
                    || doc
                        .description
                        .as_deref()
                        .is_some_and(|d| d.trim_start().starts_with(PROVIDED))
            })
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let mut found = Found::default();
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(found);
        };
        // By their IDs: this source goes without `deezer` and `tidal_isrc`, and each without it.
        let on_deezer = super::runs_by_id(&cx.config, "deezer");
        let on_tidal = super::runs_by_id(&cx.config, "tidal_isrc");
        if !on_deezer && !on_tidal {
            return Ok(found);
        }
        let upload = match watch(cx, &link.id).await? {
            Watch::Page(page) => upload(&page),
            Watch::LoginRequired => {
                return Err(RetryLater {
                    class: "youtube_login",
                    seconds: LOGIN_WAIT_SECS,
                }
                .into())
            }
            Watch::Nothing => None,
        };
        let Some(upload) = upload else {
            return Ok(found);
        };
        let query = format!(
            "{} {}",
            artist_name(&upload.artist),
            recording_title(&upload.title)
        );
        if on_deezer {
            let path = format!(
                "search?{}",
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("q", &query)
                    .finish()
            );
            if let Some(answer) = deezer_api::answer(cx, &path).await? {
                if let Some(id) = pick(&deezer_candidates(&answer), &upload) {
                    found.links.extend(
                        derived(
                            &format!("https://www.deezer.com/track/{id}"),
                            Level::Track,
                            "youtube_match",
                        )
                        .map(super::Link::maybe),
                    );
                }
            }
        }
        if on_tidal {
            let mut url = url::Url::parse(&format!("{TIDAL_API}/searchResults")).expect("a URL");
            url.query_pairs_mut()
                .append_pair("countryCode", &cx.config.section::<TidalConfig>().country)
                .append_pair("filter[query]", &query)
                .append_pair("include", "tracks.artists");
            match tidal_api::api_object(cx, url.as_str()).await {
                Ok(Some(answer)) => {
                    if let Some(id) = pick(&tidal_candidates(&answer), &upload) {
                        found.links.extend(
                            derived(
                                &format!("https://tidal.com/track/{id}"),
                                Level::Track,
                                "youtube_match",
                            )
                            .map(super::Link::maybe),
                        );
                    }
                }
                Ok(None) => {}
                // TIDAL failing (refused credentials, a wait, an unexpected answer) with no
                // Deezer match in this run fails the lookup, which keeps every link an earlier
                // run found (a TIDAL one too): until the retry, or for refused credentials until
                // the card's next run.
                Err(error) if found.links.is_empty() => return Err(error),
                // With a Deezer match, that stands (a TIDAL failure that lasts never holds it
                // back); the run comes back after the wait TIDAL named, else after five minutes,
                // and a TIDAL link an earlier run found is out until then. Refused credentials
                // last: the run comes back at its lifetime, and the links an earlier run found
                // stay beside the Deezer match (a TIDAL one too).
                Err(error) => {
                    tracing::debug!(
                        kind = crate::resolver::error_class(&error),
                        "TIDAL's search failed; the Deezer match stands"
                    );
                    if super::client_credentials::refused(&error) {
                        found.keep_stored = true;
                    } else {
                        let seconds = error
                            .downcast_ref::<RetryLater>()
                            .map_or(super::client_credentials::REFUSAL_HOLD, |r| r.seconds);
                        found.retry_after = Some(std::time::Duration::from_secs(seconds));
                    }
                }
            }
        }
        Ok(found)
    }
}

/// A video's watch page through the egress, shared for an hour; YouTube's sign-in page, which the
/// egress refuses as a wall, is [`Watch::LoginRequired`].
async fn watch(cx: &Context, id: &str) -> Result<Watch> {
    let url = format!("https://www.youtube.com/watch?v={id}");
    let options = FetchOptions {
        accept: Some("text/html".into()),
        body_cap: Some(WATCH_CAP),
        ..Default::default()
    };
    let fetched = match cx.egress.discover(&url, &options, 3600).await {
        Ok(fetched) => fetched,
        // The sign-in page ("confirm you're not a bot") is a wall to the egress: no answer.
        Err(error) if error.is::<crate::resolver::interstitial::Blocked>() => {
            return Ok(Watch::LoginRequired)
        }
        // A page over the cap stays over it: nothing to read, and nothing to retry.
        Err(error) if crate::resolver::egress::over_cap(&error) => return Ok(Watch::Nothing),
        Err(error) => return Err(error),
    };
    match fetched.status {
        200 => {}
        429 | 500..=599 => return Err(super::media::busy("youtube_busy", &fetched)),
        // YouTube's answer to a host it blocks: no answer either.
        403 => return Ok(Watch::LoginRequired),
        _ => return Ok(Watch::Nothing),
    }
    let html = String::from_utf8_lossy(&fetched.body);
    Ok(parse_watch(&html))
}

/// What a watch page's player response (`ytInitialPlayerResponse`) says: its `videoDetails`
/// (title, channel, description, `lengthSeconds`, else the page's `<meta itemprop="duration">`), a
/// sign-in page, or nothing.
pub fn parse_watch(html: &str) -> Watch {
    let Some(response) = player_response(html) else {
        return Watch::Nothing;
    };
    let details = &response["videoDetails"];
    let text = |field: &str| details[field].as_str().map(str::to_owned);
    let seconds = details["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|s| (1..=86_400).contains(s))
        .or_else(|| meta_duration(html));
    match (text("title"), text("author"), seconds) {
        (Some(title), Some(author), Some(seconds)) => Watch::Page(Page {
            title,
            author,
            description: text("shortDescription").unwrap_or_default(),
            seconds,
        }),
        _ if response["playabilityStatus"]["status"] == "LOGIN_REQUIRED" => Watch::LoginRequired,
        _ => Watch::Nothing,
    }
}
/// The page's player response: the JSON object after `ytInitialPlayerResponse = `.
fn player_response(html: &str) -> Option<Value> {
    let marker = "ytInitialPlayerResponse = {";
    let at = html.find(marker)? + marker.len() - 1;
    serde_json::Deserializer::from_str(&html[at..])
        .into_iter::<Value>()
        .next()?
        .ok()
        .filter(Value::is_object)
}
/// The video's length from `<meta itemprop="duration" content="PT…">`.
fn meta_duration(html: &str) -> Option<i64> {
    let marker = "<meta itemprop=\"duration\" content=\"";
    let rest = &html[html.find(marker)? + marker.len()..];
    duration_secs(&rest[..rest.find('"')?])
}

/// The artist, title and length of a label's upload: the artist is its Topic channel's name, else
/// the first artist of the description's "Title · Artist" line. `None` for any other video.
pub fn upload(page: &Page) -> Option<Upload> {
    let provided = page.description.trim_start().starts_with(PROVIDED);
    let artist = match page.author.trim_end().strip_suffix(TOPIC) {
        Some(artist) => artist.trim().to_owned(),
        None if provided => {
            // "Provided to YouTube by …\n\nTitle · Artist · Artist\n\nAlbum\n\n…"
            let line = page.description.split("\n\n").nth(1)?;
            line.split(" · ").nth(1)?.trim().to_owned()
        }
        None => return None,
    };
    (!artist.is_empty() && !page.title.trim().is_empty()).then(|| Upload {
        artist,
        title: page.title.trim().to_owned(),
        seconds: page.seconds,
    })
}

/// The one candidate with the upload's artist and title (as `matching` compares them) and a length
/// within [`MATCH_WINDOW_SECS`]: its ID. None or several give `None`.
pub fn pick<'a>(candidates: &'a [Candidate], upload: &Upload) -> Option<&'a str> {
    let title = recording_title(&upload.title);
    let artist = artist_name(&upload.artist);
    if title.is_empty() || artist.is_empty() {
        return None;
    }
    let mut ids: Vec<&str> = candidates
        .iter()
        .filter(|c| {
            recording_title(&c.title) == title
                && artist_name(&c.artist) == artist
                && (c.seconds - upload.seconds).abs() <= MATCH_WINDOW_SECS
        })
        .map(|c| c.id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    match ids.as_slice() {
        [id] => Some(id),
        _ => None,
    }
}

/// The tracks of a Deezer search answer (`data`: `title` with its version, `artist.name`,
/// `duration` in seconds).
pub fn deezer_candidates(answer: &Value) -> Vec<Candidate> {
    answer["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["type"] == "track")
        .filter_map(|t| {
            Some(Candidate {
                id: t["id"].as_u64()?.to_string(),
                title: t["title"].as_str()?.to_owned(),
                artist: t["artist"]["name"].as_str()?.to_owned(),
                seconds: t["duration"].as_i64()?,
            })
        })
        .collect()
}
/// The tracks of a TIDAL search answer (`searchResults` with `include=tracks.artists`): each
/// included track, its title with its `version`, its first artist's name, its ISO 8601 length.
pub fn tidal_candidates(answer: &Value) -> Vec<Candidate> {
    let included = answer["included"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let artist = |id: &str| {
        included
            .iter()
            .find(|i| i["type"] == "artists" && i["id"].as_str() == Some(id))
            .and_then(|i| i["attributes"]["name"].as_str())
    };
    included
        .iter()
        .filter(|t| t["type"] == "tracks")
        .filter_map(|t| {
            let a = &t["attributes"];
            let title = tidal_api::title(a)?;
            let first = t["relationships"]["artists"]["data"][0]["id"].as_str()?;
            Some(Candidate {
                id: t["id"].as_str()?.to_owned(),
                title,
                artist: artist(first)?.to_owned(),
                seconds: a["duration"].as_str().and_then(duration_secs)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/media/youtube_match/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(path).unwrap()
    }
    fn gunzip(bytes: &[u8]) -> String {
        use std::io::Read;
        let mut html = String::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_string(&mut html)
            .unwrap();
        html
    }
    fn json(name: &str) -> Value {
        serde_json::from_slice(&fixture(name)).unwrap()
    }
    fn queen() -> Upload {
        let Watch::Page(page) = parse_watch(&gunzip(&fixture("watch-bSnlKl_PoQU.html.gz"))) else {
            panic!("no page");
        };
        upload(&page).unwrap()
    }

    /// Queen's Topic upload of Bohemian Rhapsody: its details, though YouTube calls it
    /// unplayable from here.
    #[test]
    fn a_topic_upload_names_its_artist_title_and_length() {
        let Watch::Page(page) = parse_watch(&gunzip(&fixture("watch-bSnlKl_PoQU.html.gz"))) else {
            panic!("no page");
        };
        assert_eq!(page.title, "Bohemian Rhapsody (Remastered 2011)");
        assert_eq!(page.author, "Queen - Topic");
        assert_eq!(page.seconds, 354);
        assert!(page.description.starts_with(PROVIDED));
        assert_eq!(
            upload(&page),
            Some(Upload {
                artist: "Queen".into(),
                title: "Bohemian Rhapsody (Remastered 2011)".into(),
                seconds: 354
            })
        );
        // Without the Topic channel, the description's line names the artist.
        let renamed = Page {
            author: "Queen".into(),
            ..page.clone()
        };
        assert_eq!(upload(&renamed).unwrap().artist, "Queen");
        // Without either, it is no label's upload.
        let other = Page {
            author: "Queen".into(),
            description: "Official audio".into(),
            ..page
        };
        assert_eq!(upload(&other), None);
    }

    /// A page YouTube serves only after a sign-in is no answer, and no details.
    #[test]
    fn a_sign_in_page_is_login_required() {
        assert_eq!(
            parse_watch(&gunzip(&fixture("watch-5NV6Rdv1a3I.html.gz"))),
            Watch::LoginRequired
        );
        assert_eq!(parse_watch("<html></html>"), Watch::Nothing);
    }

    /// An official video on the artist's own channel (Rick Astley's) is no label's upload.
    #[test]
    fn an_artist_s_own_video_is_no_label_upload() {
        let bytes = std::fs::read(format!(
            "{}/tests/fixtures/media/youtube-watch.html.gz",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let Watch::Page(page) = parse_watch(&gunzip(&bytes)) else {
            panic!("no page");
        };
        assert_eq!(page.author, "Rick Astley");
        assert_eq!(page.seconds, 213);
        assert_eq!(upload(&page), None);
    }

    /// Deezer's plain search: one track is Queen's "Bohemian Rhapsody" within 2 s (354 s); its
    /// live versions differ by title, another of its name by length.
    #[test]
    fn deezer_s_one_match_is_taken() {
        let candidates = deezer_candidates(&json("deezer-search-queen-bohemian-rhapsody.json"));
        assert_eq!(candidates.len(), 25);
        let upload = queen();
        assert_eq!(pick(&candidates, &upload), Some("4091937401"));
        // Outside the window: nothing.
        let longer = Upload {
            seconds: 357,
            ..upload.clone()
        };
        assert_eq!(pick(&candidates, &longer), None);
        // Another artist: nothing.
        let other = Upload {
            artist: "Queens".into(),
            ..upload.clone()
        };
        assert_eq!(pick(&candidates, &other), None);
        assert!(deezer_candidates(&json("deezer-search-none.json")).is_empty());
    }

    /// TIDAL's search: two of Queen's "Bohemian Rhapsody" tracks are within 2 s (355 s and
    /// 356 s): neither is taken.
    #[test]
    fn tidal_s_two_matches_give_nothing() {
        let candidates = tidal_candidates(&json("tidal-search-queen-bohemian-rhapsody.json"));
        assert_eq!(candidates.len(), 20);
        let live = candidates.iter().find(|c| c.id == "534049340").unwrap();
        assert_eq!(live.title, "Bohemian Rhapsody (Live Aid)");
        assert_eq!(live.artist, "Queen");
        let upload = queen();
        assert_eq!(pick(&candidates, &upload), None);
        // One of them alone would be taken.
        let one: Vec<Candidate> = candidates
            .iter()
            .filter(|c| c.id != "551040646")
            .cloned()
            .collect();
        assert_eq!(pick(&one, &upload), Some("534050211"));
    }

    #[test]
    fn only_a_label_upload_s_stored_card_applies() {
        let subject = |uri: &str, author: &str, description: Option<&str>| {
            let mut doc = crate::model::ResourceMeta::new(uri, "oembed", 86400);
            doc.attributes.insert("author".into(), author.into());
            doc.description = description.map(str::to_owned);
            Subject {
                id: "r".into(),
                uri: uri.into(),
                status: "ok".into(),
                type_name: None,
                doc: Some(doc),
                keys: vec![],
                first_seen_at: None,
                candidates: vec![],
            }
        };
        let topic = "https://www.youtube.com/watch?v=bSnlKl_PoQU";
        assert!(YoutubeMatch.applies(&subject(topic, "Queen - Topic", None)));
        assert!(YoutubeMatch.applies(&subject(
            "https://music.youtube.com/watch?v=bSnlKl_PoQU",
            "Queen - Topic",
            None
        )));
        assert!(YoutubeMatch.applies(&subject(
            topic,
            "Queen",
            Some("Provided to YouTube by Universal Music Group")
        )));
        // Daft Punk's "Official Audio" on its VEVO channel (its oEmbed): no label upload.
        assert!(!YoutubeMatch.applies(&subject(
            "https://www.youtube.com/watch?v=5NV6Rdv1a3I",
            "DaftPunkVEVO",
            None
        )));
        assert!(!YoutubeMatch.applies(&subject(
            "https://www.deezer.com/track/4091937401",
            "Queen - Topic",
            None
        )));
    }
}
