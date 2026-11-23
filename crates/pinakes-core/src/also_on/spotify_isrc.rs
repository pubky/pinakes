//! Another service's track or album on Spotify, by its ISRC or barcode. For a track with an
//! `isrc:` key (TIDAL's, Deezer's or a vouching page's own record, or a hub's), the Spotify tracks
//! that carry exactly that ISRC (the Web API's search `isrc:` filter, an exact lookup, never a
//! title), as links keyed by the ISRC; several answers are the recording on several releases,
//! which `editions` places and a card shows one per service. For an album with a `upc:` key, the
//! one Spotify album that carries exactly that barcode: the search's `upc:` filter names up to
//! three candidates, and as its album objects carry no barcode, each is read back with
//! `/v1/albums/{id}` (`external_ids.upc`; the request `spotify` makes, shared with it: Spotify
//! answers no batch read to an app in development mode); a barcode several albums carry names
//! none of them, and a search that found more albums than it lists names none either. A 403 on
//! one album's read is that read's: the lookup is retried, the token slot stays open.
//!
//! It runs only with an app Spotify answers (its owner's Premium). Asked with the same app
//! credentials as `spotify` (`spotify_api`), through the same token slot: each answer is shared
//! for an hour by credential pair, a refusal holds five minutes, a rate limit is waited out. The
//! link is all this source stores.
use super::{
    client_credentials,
    spotify_api::{self, SpotifyConfig, API, SPOTIFY},
    Context, Found, Source, Subject,
};
use crate::{
    music::{self, Level, Service},
    music_sources::{derived, key_values, reverse_subject},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

/// The most Spotify tracks one ISRC gives a card (the recording on several releases).
const MAX_LINKS: usize = 5;
/// The most ISRC or barcode keys of one track or album asked (a track's own ISRC first).
const MAX_ISRCS: usize = 2;
/// The most albums one barcode search lists, each read back (one request each).
const ALBUM_CANDIDATES: usize = 3;

pub struct SpotifyIsrc;
#[async_trait]
impl Source for SpotifyIsrc {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn budgets(&self) -> &'static [(&'static str, f64)] {
        // The host's pace is every Spotify source's: one budget, shared.
        &[("api.spotify.com", 2.0)]
    }
    fn id(&self) -> &'static str {
        "spotify_isrc"
    }
    fn section(&self) -> Option<super::Section> {
        Some(super::Section::of::<SpotifyConfig>())
    }
    fn ready(&self, config: &crate::config::Config) -> bool {
        spotify_api::configured(config)
    }
    fn applies(&self, subject: &Subject) -> bool {
        reverse_subject(subject, Service::Spotify).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let (client_id, secret) = cx
            .config
            .section::<SpotifyConfig>()
            .pair()
            .context("no Spotify credentials")?;
        let pair = (client_id.as_str(), secret.as_str());
        let Some((level, _)) = reverse_subject(subject, Service::Spotify) else {
            return Ok(Found::default());
        };
        if level == Level::Album {
            return albums(cx, pair, subject).await;
        }
        // The track's own ISRC first, then the hubs'; only ISRCs that are keys (a shared one is
        // a fact, and Spotify would answer with any of its recordings).
        let mut found = Found::default();
        for isrc in key_values(subject, "isrc", true, MAX_ISRCS) {
            let Some(isrc) = music::isrc(&isrc) else {
                continue;
            };
            let mut url = url::Url::parse(&format!("{API}/search")).expect("a fixed URL");
            url.query_pairs_mut()
                .append_pair("q", &format!("isrc:{isrc}"))
                .append_pair("type", "track")
                .append_pair("limit", "10");
            let Some(answer) = api(cx, pair, url.as_str()).await? else {
                continue;
            };
            // Only tracks that carry the ISRC asked, in Spotify's order, each once.
            for item in answer["tracks"]["items"].as_array().into_iter().flatten() {
                if item["external_ids"]["isrc"]
                    .as_str()
                    .and_then(music::isrc)
                    .as_deref()
                    != Some(isrc.as_str())
                {
                    continue;
                }
                let Some(id) = item["id"].as_str() else {
                    continue;
                };
                let Some(link) = derived(
                    &format!("https://open.spotify.com/track/{id}"),
                    Level::Track,
                    "spotify_isrc",
                ) else {
                    continue;
                };
                if found.links.iter().any(|l| l.uri == link.uri) {
                    continue;
                }
                found.links.push(link.keyed(format!("isrc:{isrc}")));
                if found.links.len() >= MAX_LINKS {
                    return Ok(found);
                }
            }
        }
        Ok(found)
    }
}

/// One answer of the Web API with the operator's pair; `None` when Spotify has none.
async fn api(cx: &Context, pair: (&str, &str), url: &str) -> Result<Option<Value>> {
    client_credentials::object(&SPOTIFY, cx, pair, url, ("application/json", &[])).await
}

/// The album that carries an album's barcode key, for at most two of its keys: the search's
/// candidates, read back. A barcode no album or several albums carry gives nothing.
async fn albums(cx: &Context, pair: (&str, &str), subject: &Subject) -> Result<Found> {
    let mut found = Found::default();
    for upc in subject.keys_in("upc").take(MAX_ISRCS) {
        let Some(upc) = crate::barcode::gtin13(upc) else {
            continue;
        };
        let mut url = url::Url::parse(&format!("{API}/search")).expect("a fixed URL");
        url.query_pairs_mut()
            .append_pair("q", &format!("upc:{}", searched_upc(&upc)))
            .append_pair("type", "album")
            .append_pair("limit", &ALBUM_CANDIDATES.to_string());
        let Some(answer) = api(cx, pair, url.as_str()).await? else {
            continue;
        };
        let candidates = album_ids(&answer);
        // A search with more albums than it listed may hide a second album with the barcode.
        if candidates.is_empty() || lists_less(&answer) {
            continue;
        }
        let mut carried = Vec::new();
        // Every candidate must be read: one that gives no album (a 404) may be the other album
        // with the barcode.
        let mut unread = false;
        for id in &candidates {
            let url = format!("{API}/albums/{id}");
            let album = client_credentials::object_as(
                &SPOTIFY,
                cx,
                pair,
                &url,
                ("application/json", &[]),
                false,
            )
            .await?;
            match album {
                Some(album) if carries(&album, id, &upc) => carried.push(id),
                Some(_) => {}
                None => unread = true,
            }
        }
        let [id] = carried.as_slice() else {
            continue;
        };
        if unread {
            continue;
        }
        if let Some(link) = derived(
            &format!("https://open.spotify.com/album/{id}"),
            Level::Album,
            "spotify_isrc",
        ) {
            if !found.links.iter().any(|l| l.uri == link.uri) {
                found.links.push(link.keyed(format!("upc:{upc}")));
            }
        }
    }
    Ok(found)
}
/// A barcode as Spotify's search takes it: a UPC-A (a GTIN-13 with a leading zero) by its 12
/// digits, as Spotify keeps it, an EAN-13 whole.
pub fn searched_upc(gtin13: &str) -> &str {
    match gtin13.strip_prefix('0') {
        Some(upc) if gtin13.len() == 13 => upc,
        _ => gtin13,
    }
}
/// The album IDs a search answer lists, in its order, each once.
pub fn album_ids(answer: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for item in answer["albums"]["items"].as_array().into_iter().flatten() {
        let Some(id) = item["id"].as_str() else {
            continue;
        };
        if id.len() == 22
            && id.bytes().all(|b| b.is_ascii_alphanumeric())
            && !ids.iter().any(|i| i == id)
        {
            ids.push(id.to_owned());
        }
    }
    ids.truncate(ALBUM_CANDIDATES);
    ids
}
/// Whether an album search lists fewer albums than it found: a `next` page, or a `total` above
/// the items it lists.
pub fn lists_less(answer: &Value) -> bool {
    let albums = &answer["albums"];
    let listed = albums["items"].as_array().map_or(0, Vec::len) as u64;
    !albums["next"].is_null() || albums["total"].as_u64().is_some_and(|t| t > listed)
}
/// Whether a `/v1/albums/{id}` answer is album `id` and its own barcode (`external_ids.upc`) is
/// `upc` (GTIN-13).
pub fn carries(album: &Value, id: &str, upc: &str) -> bool {
    album["type"].as_str() == Some("album")
        && album["id"].as_str() == Some(id)
        && album["external_ids"]["upc"]
            .as_str()
            .and_then(crate::barcode::gtin13)
            .as_deref()
            == Some(upc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Random Access Memories on Spotify, and a made-up album the search lists beside it.
    const RAM: &str = "4m2880jivSbbyEGAKfITCa";
    const OTHER: &str = "0otherUpcAlbum00000000";

    /// The shapes of Spotify's Web API reference (Search, Get Several Albums): a search's
    /// simplified albums carry no `external_ids`; the full albums do. Spotify answers 403 to this
    /// host's app (its owner has no Premium), so no answer is recorded.
    fn search(ids: &[&str]) -> Value {
        json!({"albums": {"limit": 10, "offset": 0, "total": ids.len(), "next": null,
            "items": ids.iter().map(|id| json!({"album_type": "album", "id": id, "type": "album",
                "name": "Random Access Memories", "uri": format!("spotify:album:{id}")})).collect::<Vec<_>>()}})
    }
    fn album(id: &str, upc: &str) -> Value {
        json!({"album_type": "album", "id": id, "type": "album", "name": "Random Access Memories",
            "external_ids": {"upc": upc}, "total_tracks": 13, "uri": format!("spotify:album:{id}")})
    }

    #[test]
    fn a_upc_a_is_searched_by_its_12_digits_an_ean_whole() {
        assert_eq!(searched_upc("0886443927087"), "886443927087");
        assert_eq!(searched_upc("4018939306052"), "4018939306052");
    }

    #[test]
    fn only_an_album_whose_own_barcode_is_the_key_counts() {
        // At most three candidates, each once, in the search's order.
        let candidates = album_ids(&search(&[
            RAM,
            OTHER,
            RAM,
            "short",
            "1fourthAlbum0000000000",
        ]));
        assert_eq!(candidates, [RAM, OTHER, "1fourthAlbum0000000000"]);
        let candidates = album_ids(&search(&[
            RAM,
            OTHER,
            "1third00Album000000000",
            "1fourthAlbum0000000000",
        ]));
        assert_eq!(candidates.len(), 3);
        // Read back: the album with the barcode, in either spelling Spotify keeps.
        for upc in ["886443927087", "0886443927087"] {
            assert!(carries(&album(RAM, upc), RAM, "0886443927087"));
        }
        // Another barcode, or another album than the one asked, counts for nothing.
        assert!(!carries(&album(RAM, "4018939306052"), RAM, "0886443927087"));
        assert!(!carries(
            &album(OTHER, "886443927087"),
            RAM,
            "0886443927087"
        ));
        assert!(album_ids(&json!({"albums": {"items": []}})).is_empty());
        // A search that found more albums than it lists: refused before any read.
        let all = search(&[RAM, OTHER]);
        assert!(!lists_less(&all));
        let mut more = all.clone();
        more["albums"]["total"] = 4.into();
        assert!(lists_less(&more));
        let mut paged = all.clone();
        paged["albums"]["next"] =
            "https://api.spotify.com/v1/search?offset=3&limit=3&query=upc%3A886443927087&type=album"
                .into();
        assert!(lists_less(&paged));
    }
}
