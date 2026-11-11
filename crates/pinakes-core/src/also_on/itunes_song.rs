//! Apple Music's song for another service's track: the track's album barcode and its place on
//! that album (disc and track number) name one song of the album iTunes answers for the barcode
//! (`lookup?upc=…&entity=song`); its length must agree within `LENGTH_WINDOW_SECS` and its title
//! must be the track's (as `matching::recording_title` compares them). The album,
//! its barcode and the track's place come from the track's own service record (Deezer's, Spotify's
//! or TIDAL's, each only while that service's own source runs), else from the Deezer track Deezer
//! names for the track's ISRC key (only while `deezer` runs).
//!
//! iTunes answers carry no ISRC, so the song is a match on the album, the place and the length,
//! not an exact ID: the link is unkeyed (it joins no group), never a key, and the source is off by
//! default. An answer with more than one collection (iTunes answers unrelated singles for some
//! codes), a place with no song or with several, or a length outside the window gives nothing.
//! It runs after the sources that key a track's ISRC and before `editions`.
use super::{
    client_credentials, deezer,
    spotify::Spotify,
    spotify_api::{SpotifyConfig, API as SPOTIFY_API, SPOTIFY},
    tidal::{self, Tidal, TidalConfig, API as TIDAL_API},
    Context, Found, Source, Subject,
};
use crate::{
    matching::recording_title,
    music::{self, Level, Service},
    music_sources::{derived, own_isrc, LENGTH_WINDOW_SECS},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Pages of a TIDAL album's tracks read for a track's place (20 tracks a page).
const TIDAL_ITEM_PAGES: usize = 5;
/// The most ISRC keys of one track asked of Deezer (its own first).
const MAX_ISRCS: usize = 2;
/// How Spotify's Web API is asked: for JSON.
const JSON: (&str, &[&str]) = ("application/json", &[]);

/// Where a track is: its album's barcode (GTIN-13), its disc and track number there, its length
/// in milliseconds and its title (with its version), as its own service records them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub upc: String,
    pub disc: u64,
    pub number: u64,
    pub length_ms: u64,
    pub title: String,
}
/// A TIDAL track as `tracks/{id}?include=albums` gives it: its one album, that album's barcode,
/// its length in milliseconds and its title with its version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TidalTrack {
    pub album: String,
    pub upc: String,
    pub length_ms: u64,
    pub title: String,
}

pub struct ItunesSong;
#[async_trait]
impl Source for ItunesSong {
    fn id(&self) -> &'static str {
        "itunes_song"
    }
    fn enabled_by_default(&self) -> bool {
        false
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| {
            l.level == Level::Track
                && l.service != Service::AppleMusic
                && (matches!(
                    l.service,
                    Service::Deezer | Service::Spotify | Service::Tidal
                ) || subject.keys_in("isrc").next().is_some())
        })
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let mut found = Found::default();
        // Apple is asked only while `itunes` runs, as `editions` asks it.
        if !super::runs(&cx.config, &super::itunes::ItunesSource) {
            return Ok(found);
        }
        let Some(place) = place(cx, subject).await? else {
            return Ok(found);
        };
        let storefront = &cx.config.also_on.apple_storefront;
        let url = format!(
            "https://itunes.apple.com/lookup?upc={}&entity=song&country={storefront}",
            place.upc
        );
        let Some(answer) = super::itunes::answer(cx, &url).await? else {
            return Ok(found);
        };
        if let Some(song) = song(&answer, &place) {
            found.links.extend(
                derived(
                    &format!("https://music.apple.com/song/{song}"),
                    Level::Track,
                    "itunes_song",
                )
                .map(super::Link::maybe),
            );
        }
        Ok(found)
    }
}

/// The track's place: from its own service's record while that service's source runs, else from
/// the Deezer track of its ISRC while `deezer` runs.
async fn place(cx: &Context, subject: &Subject) -> Result<Option<Place>> {
    let Some(link) = music::parse(&subject.uri) else {
        return Ok(None);
    };
    let deezer_runs = super::runs(&cx.config, &deezer::DeezerSource);
    let own = match link.service {
        Service::Deezer if deezer_runs => Some(deezer_place(cx, &link.id).await),
        Service::Spotify if super::runs(&cx.config, &Spotify) => {
            Some(spotify_place(cx, &link.id).await)
        }
        Service::Tidal if super::runs(&cx.config, &Tidal) => Some(tidal_place(cx, &link.id).await),
        _ => None,
    };
    match own {
        Some(Ok(place)) => return Ok(place),
        // Credentials the service refuses: the track's ISRC may still name its place on Deezer.
        Some(Err(error)) if refused(&error) => {}
        Some(Err(error)) => return Err(error),
        None => {}
    }
    if !deezer_runs {
        return Ok(None);
    }
    // The track's own ISRC first, then the hubs'; only ISRCs that are keys.
    let own = own_isrc(subject).filter(|o| subject.keys_in("isrc").any(|k| k == o.as_str()));
    let mut isrcs: Vec<&str> = own.as_deref().into_iter().collect();
    isrcs.extend(
        subject
            .keys_in("isrc")
            .filter(|v| Some(*v) != own.as_deref()),
    );
    for isrc in isrcs.into_iter().take(MAX_ISRCS) {
        let Some(isrc) = music::isrc(isrc) else {
            continue;
        };
        let Some(track) = deezer::deezer(cx, &format!("track/isrc:{isrc}")).await? else {
            continue;
        };
        // Only the track that has the ISRC asked.
        if track["isrc"].as_str().and_then(music::isrc).as_deref() != Some(isrc.as_str()) {
            continue;
        }
        return deezer_album_place(cx, &track).await;
    }
    Ok(None)
}

/// Whether an error is a service refusing the operator's credentials (`spotify_auth`,
/// `tidal_auth`).
fn refused(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::resolver::RetryLater>()
        .is_some_and(|r| r.class.ends_with("_auth"))
}

/// A Deezer track's place (`track/{id}`, then its album's `upc`).
async fn deezer_place(cx: &Context, id: &str) -> Result<Option<Place>> {
    match deezer::deezer(cx, &format!("track/{id}")).await? {
        Some(track) if track["id"].as_u64().map(|i| i.to_string()).as_deref() == Some(id) => {
            deezer_album_place(cx, &track).await
        }
        _ => Ok(None),
    }
}
/// A Deezer track record's place: its album's barcode from the album's own record.
async fn deezer_album_place(cx: &Context, track: &Value) -> Result<Option<Place>> {
    let Some(album) = track["album"]["id"].as_u64() else {
        return Ok(None);
    };
    let Some(album) = deezer::deezer(cx, &format!("album/{album}")).await? else {
        return Ok(None);
    };
    Ok(deezer_parse(track, &album))
}
/// The place a Deezer track record (`disk_number`, `track_position`, `duration` in seconds) and
/// its album's record (`upc`) give.
pub fn deezer_parse(track: &Value, album: &Value) -> Option<Place> {
    (track["album"]["id"].as_u64()? == album["id"].as_u64()?).then_some(())?;
    Some(Place {
        upc: crate::barcode::gtin13(album["upc"].as_str()?)?,
        disc: track["disk_number"].as_u64().filter(|n| *n > 0)?,
        number: track["track_position"].as_u64().filter(|n| *n > 0)?,
        length_ms: track["duration"].as_u64().filter(|s| *s > 0)? * 1000,
        title: track["title"].as_str()?.to_owned(),
    })
}

/// A Spotify track's place (`/v1/tracks/{id}`, then `/v1/albums/{album}`: the same answers
/// `spotify` reads, shared for an hour).
async fn spotify_place(cx: &Context, id: &str) -> Result<Option<Place>> {
    let Some((client_id, secret)) = cx.config.section::<SpotifyConfig>().pair() else {
        return Ok(None);
    };
    let pair = (client_id.as_str(), secret.as_str());
    let url = format!("{SPOTIFY_API}/tracks/{id}");
    let Some(track) = client_credentials::object(&SPOTIFY, cx, pair, &url, JSON).await? else {
        return Ok(None);
    };
    if track["id"].as_str() != Some(id) {
        return Ok(None);
    }
    let Some(album) = track["album"]["id"].as_str() else {
        return Ok(None);
    };
    let url = format!("{SPOTIFY_API}/albums/{album}");
    let Some(album) = client_credentials::object(&SPOTIFY, cx, pair, &url, JSON).await? else {
        return Ok(None);
    };
    Ok(spotify_parse(&track, &album))
}
/// The place a Spotify track (`disc_number`, `track_number`, `duration_ms`) and its album
/// (`external_ids.upc`) give.
pub fn spotify_parse(track: &Value, album: &Value) -> Option<Place> {
    (track["album"]["id"].as_str()? == album["id"].as_str()?).then_some(())?;
    Some(Place {
        upc: crate::barcode::gtin13(album["external_ids"]["upc"].as_str()?)?,
        disc: track["disc_number"].as_u64().filter(|n| *n > 0)?,
        number: track["track_number"].as_u64().filter(|n| *n > 0)?,
        length_ms: track["duration_ms"].as_u64().filter(|ms| *ms > 0)?,
        title: track["name"].as_str()?.to_owned(),
    })
}

/// A TIDAL track's place: its one album and that album's barcode (`tracks/{id}?include=albums`,
/// the answer `editions` reads too), its length, and its volume and number among the album's
/// items (`albums/{album}/relationships/items`). A track TIDAL puts on several albums has no
/// one place.
async fn tidal_place(cx: &Context, id: &str) -> Result<Option<Place>> {
    let country = &cx.config.section::<TidalConfig>().country;
    let url = format!("{TIDAL_API}/tracks/{id}?countryCode={country}&include=albums");
    let Some(track) = tidal::api_object(cx, &url).await? else {
        return Ok(None);
    };
    let Some(track) = tidal_track(&track, id) else {
        return Ok(None);
    };
    let album = &track.album;
    let mut url = format!("{TIDAL_API}/albums/{album}/relationships/items?countryCode={country}");
    for _ in 0..TIDAL_ITEM_PAGES {
        let Some(items) = tidal::api_object(cx, &url).await? else {
            return Ok(None);
        };
        if let Some((disc, number)) = tidal_position(&items, id) {
            return Ok(Some(Place {
                upc: track.upc,
                disc,
                number,
                length_ms: track.length_ms,
                title: track.title,
            }));
        }
        // The next page's link is relative to the API's root (`/albums/…`).
        match items["links"]["next"].as_str() {
            Some(next) if next.starts_with('/') => url = format!("{TIDAL_API}{next}"),
            _ => return Ok(None),
        }
    }
    Ok(None)
}
/// A TIDAL track's one album, that album's barcode, the track's length and title, from
/// `tracks/{id}?include=albums`.
pub fn tidal_track(v: &Value, id: &str) -> Option<TidalTrack> {
    let data = &v["data"];
    (data["id"].as_str()? == id && data["type"] == "tracks").then_some(())?;
    let albums = tidal::items(&data["relationships"]["albums"], "albums");
    let [album] = albums.as_slice() else {
        return None;
    };
    let included = v["included"]
        .as_array()?
        .iter()
        .find(|i| i["type"] == "albums" && i["id"].as_str() == Some(album.as_str()))?;
    let upc = crate::barcode::gtin13(included["attributes"]["barcodeId"].as_str()?)?;
    let seconds = data["attributes"]["duration"]
        .as_str()
        .and_then(crate::music_sources::duration_secs)?;
    let title = data["attributes"]["title"].as_str()?;
    let title = match data["attributes"]["version"].as_str().map(str::trim) {
        Some(version) if !version.is_empty() => format!("{title} ({version})"),
        _ => title.to_owned(),
    };
    Some(TidalTrack {
        album: album.clone(),
        upc,
        length_ms: u64::try_from(seconds).ok()? * 1000,
        title,
    })
}
/// A track's volume and number among a TIDAL album's items.
pub fn tidal_position(items: &Value, id: &str) -> Option<(u64, u64)> {
    let item = items["data"]
        .as_array()?
        .iter()
        .find(|i| i["type"] == "tracks" && i["id"].as_str() == Some(id))?;
    Some((
        item["meta"]["volumeNumber"].as_u64().filter(|n| *n > 0)?,
        item["meta"]["trackNumber"].as_u64().filter(|n| *n > 0)?,
    ))
}

/// The one song of an iTunes `lookup?upc=…&entity=song` answer at the place, when the answer is
/// one collection and the song's length is within the window of the track's and its title is the
/// track's: its track ID.
pub fn song(answer: &Value, place: &Place) -> Option<u64> {
    let results = answer["results"].as_array()?;
    let mut collections: Vec<u64> = results
        .iter()
        .filter_map(|r| r["collectionId"].as_u64())
        .collect();
    collections.sort_unstable();
    collections.dedup();
    let [_] = collections.as_slice() else {
        return None;
    };
    let at_place: Vec<&Value> = results
        .iter()
        .filter(|r| r["wrapperType"] == "track" && r["kind"] == "song")
        .filter(|r| {
            r["discNumber"].as_u64() == Some(place.disc)
                && r["trackNumber"].as_u64() == Some(place.number)
        })
        .collect();
    let [song] = at_place.as_slice() else {
        return None;
    };
    let length = song["trackTimeMillis"].as_u64()?;
    (length.abs_diff(place.length_ms) <= LENGTH_WINDOW_SECS * 1000).then_some(())?;
    // The same title, as `same_recording` compares titles: a credit (`feat. …`) and the tags that
    // keep a recording's identity go, a version tag (`Live`, `Remix`) stays. One wrong collection
    // for a barcode (a single answered with its album) puts another song at the place.
    let title = recording_title(&place.title);
    (!title.is_empty() && recording_title(song["trackName"].as_str()?) == title).then_some(())?;
    song["trackId"].as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/media/itunes_song/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_slice(std::fs::read(path).unwrap().trim_ascii()).unwrap()
    }

    /// Get Lucky: Deezer says 367 s (it rounds), disc 1, track 8 of Random Access Memories;
    /// Apple's song is 369.629 s, within the window.
    #[test]
    fn get_lucky_on_deezer_is_apple_s_song_617154366() {
        let track = serde_json::from_slice(
            &std::fs::read(format!(
                "{}/tests/fixtures/hubs/deezer-track-67238735.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
        .unwrap();
        let place = deezer_parse(&track, &fixture("deezer-album-6575789.json")).unwrap();
        assert_eq!(
            place,
            Place {
                upc: "0886443927087".into(),
                disc: 1,
                number: 8,
                length_ms: 367_000,
                title: "Get Lucky (feat. Pharrell Williams and Nile Rodgers)".into()
            }
        );
        let answer = fixture("itunes-upc-0886443927087-songs.json");
        assert_eq!(song(&answer, &place), Some(617154366));
        // Another album's record is not the track's.
        assert_eq!(
            deezer_parse(&track, &fixture("deezer-album-125431.json")),
            None
        );
        // A length outside the window, another place: nothing.
        let off = Place {
            length_ms: 365_000,
            ..place.clone()
        };
        assert_eq!(song(&answer, &off), None);
        let elsewhere = Place {
            number: 14,
            ..place.clone()
        };
        assert_eq!(song(&answer, &elsewhere), None);
        // The same place and length with another title (the album a barcode wrongly names):
        // nothing.
        let other = Place {
            title: "Lose Yourself to Dance (feat. Pharrell Williams)".into(),
            ..place.clone()
        };
        assert_eq!(song(&answer, &other), None);
        // A version tag tells recordings apart; a remaster tag does not.
        let live = Place {
            title: "Get Lucky (Live)".into(),
            ..place.clone()
        };
        assert_eq!(song(&answer, &live), None);
        let remastered = Place {
            title: "Get Lucky - Remastered".into(),
            ..place.clone()
        };
        assert_eq!(song(&answer, &remastered), Some(617154366));
    }

    /// Walking Disaster: Underclass Hero's barcode answers four collections (the album and
    /// singles of its title track), so no song is taken, whatever its place and length.
    #[test]
    fn walking_disaster_s_barcode_answers_several_collections() {
        let track = fixture("deezer-track-isrc-USUM70740306.json");
        let place = deezer_parse(&track, &fixture("deezer-album-125431.json")).unwrap();
        assert_eq!(
            place,
            Place {
                upc: "0602517416987".into(),
                disc: 1,
                number: 2,
                length_ms: 286_000,
                title: "Walking Disaster".into()
            }
        );
        let answer = fixture("itunes-upc-0602517416987-songs.json");
        assert_eq!(song(&answer, &place), None);
        // Its album's own songs alone would answer: the collections are the refusal.
        let mut one = answer.clone();
        let album = 1440766058;
        one["results"]
            .as_array_mut()
            .unwrap()
            .retain(|r| r["collectionId"] == album);
        assert!(song(&one, &place).is_some());
    }

    /// TIDAL's Get Lucky (370 s, track 8 of Random Access Memories) and Walking Disaster
    /// (track 2 of Underclass Hero).
    #[test]
    fn a_tidal_track_s_place_is_read_from_its_album() {
        let track = tidal_track(&fixture("tidal-track-20115564-albums.json"), "20115564").unwrap();
        assert_eq!(
            track,
            TidalTrack {
                album: "20115556".into(),
                upc: "0886443927087".into(),
                length_ms: 370_000,
                title: "Get Lucky (feat. Pharrell Williams and Nile Rodgers)".into(),
            }
        );
        let items = fixture("tidal-album-20115556-items.json");
        assert_eq!(tidal_position(&items, "20115564"), Some((1, 8)));
        assert_eq!(tidal_position(&items, "3524506"), None);
        let place = Place {
            upc: track.upc,
            disc: 1,
            number: 8,
            length_ms: track.length_ms,
            title: track.title,
        };
        assert_eq!(
            song(&fixture("itunes-upc-0886443927087-songs.json"), &place),
            Some(617154366)
        );
        let track = tidal_track(&fixture("tidal-track-3524506-albums.json"), "3524506").unwrap();
        assert_eq!(
            (
                track.album.as_str(),
                track.upc.as_str(),
                track.title.as_str()
            ),
            ("3524504", "0602517416987", "Walking Disaster")
        );
        assert_eq!(
            tidal_position(&fixture("tidal-album-3524504-items.json"), "3524506"),
            Some((1, 2))
        );
        assert_eq!(
            tidal_track(&fixture("tidal-track-3524506-albums.json"), "1"),
            None
        );
    }

    /// Spotify answers this host's app 403 (no Premium): the shapes of its Web API reference
    /// (Get Track, Get Album), with Get Lucky's values.
    #[test]
    fn a_spotify_track_s_place_is_its_own_and_its_album_s() {
        let track = serde_json::json!({"id": "69kOkLUCkxIZYexIgSG8rq", "type": "track",
            "name": "Get Lucky (feat. Pharrell Williams and Nile Rodgers)", "disc_number": 1,
            "track_number": 8, "duration_ms": 369_626,
            "album": {"id": "4m2880jivSbbyEGAKfITCa", "type": "album"}});
        let album = serde_json::json!({"id": "4m2880jivSbbyEGAKfITCa", "type": "album",
            "external_ids": {"upc": "886443927087"}});
        let place = spotify_parse(&track, &album).unwrap();
        assert_eq!(place.upc, "0886443927087");
        assert_eq!((place.disc, place.number, place.length_ms), (1, 8, 369_626));
        assert_eq!(
            song(&fixture("itunes-upc-0886443927087-songs.json"), &place),
            Some(617154366)
        );
        let mut other = album.clone();
        other["id"] = "0otherUpcAlbum00000000".into();
        assert_eq!(spotify_parse(&track, &other), None);
    }

    #[test]
    fn only_other_services_tracks_apply() {
        let subject = |uri: &str, keys: &[&str]| Subject {
            id: "r".into(),
            uri: uri.into(),
            status: "ok".into(),
            type_name: None,
            doc: None,
            keys: keys.iter().map(|k| ((*k).into(), "x".into())).collect(),
            first_seen_at: None,
            candidates: vec![],
        };
        let applies = |uri: &str, keys: &[&str]| ItunesSong.applies(&subject(uri, keys));
        assert!(applies("https://www.deezer.com/track/67238735", &[]));
        assert!(applies(
            "https://open.spotify.com/track/69kOkLUCkxIZYexIgSG8rq",
            &[]
        ));
        assert!(applies("https://tidal.com/track/20115564", &[]));
        assert!(!applies("https://soundcloud.com/a/b", &[]));
        assert!(applies(
            "https://soundcloud.com/a/b",
            &["isrc:USQX91300108"]
        ));
        assert!(!applies(
            "https://music.apple.com/song/617154366",
            &["isrc:USQX91300108"]
        ));
        assert!(!applies(
            "https://www.deezer.com/album/6575789",
            &["upc:0886443927087"]
        ));
        // A video is no track of an album.
        assert!(!applies(
            "https://www.youtube.com/watch?v=5NV6Rdv1a3I",
            &["isrc:USQX91300108"]
        ));
    }
}
