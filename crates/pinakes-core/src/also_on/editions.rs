//! The `editions` also-on source: which release each link of a recording's or an album's card is
//! on, so the card shows, of each service, its link on the card's own release, else on the
//! original edition (`link_release`, read by `also_on()`).
//!
//! A recording's original release group is the first of those MusicBrainz lists its official
//! releases in by type (an album, then a single, then an EP, then any other; one with no secondary
//! type, such as a compilation or a soundtrack, before one with), then by first release. An
//! album's is its own. The original edition is that group's official releases whose
//! disambiguation names no edition (deluxe, anniversary, remastered, hi-res, …): digital ones
//! first, then worldwide ones, then the earliest. The source reads the album pages MusicBrainz
//! relates to the card's own release (rank 0) and to the original edition's (rank 1, 2, … in that
//! order), and places a track by its service's list of that album's tracks (Apple Music through
//! iTunes, Spotify by its album page, TIDAL and Deezer by their APIs while their sources run), an
//! album link by the album page it is. A link no list names stays unplaced.
use super::{deezer, itunes, spotify, tidal, Context, Edition, Found, Source, Subject};
use crate::{
    hubs::{self, musicbrainz, MbKind},
    music::{self, Level, Service},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Pages of 100 of a recording's album releases read for its original release group.
const RECORDING_PAGES: usize = 4;
/// Pages of 100 of a release group's releases read for its original edition.
const GROUP_PAGES: usize = 2;
/// Releases whose album pages are read: the card's own, then the original edition's.
const RELEASES_READ: usize = 6;
/// Albums of one service whose tracks are listed.
const ALBUMS_PER_SERVICE: usize = 2;
/// The card's own releases a link may name (`musicbrainz`'s `MAX_EDITIONS`).
const OWN_RELEASES: usize = 4;

/// Where a recording's or an album's links sit among its releases.
pub struct Editions;
#[async_trait]
impl Source for Editions {
    fn id(&self) -> &'static str {
        "editions"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| match l.level {
            Level::Track => subject.keys_in("mb:recording").count() == 1,
            Level::Album => {
                subject.keys_in("mb:release").next().is_some()
                    || subject.keys_in("mb:release-group").next().is_some()
            }
            _ => false,
        })
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(own) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        // The links to place: at the card's level, on another service than its own (whose other
        // pages the card never shows).
        let candidates: Vec<(String, music::Link)> = subject
            .candidates
            .iter()
            .filter_map(|uri| Some((uri.clone(), music::parse(uri)?)))
            .filter(|(_, l)| l.level == own.level && l.service != own.service)
            .collect();
        if candidates.is_empty() {
            return Ok(Found::default());
        }
        let base = cx.config.also_on.musicbrainz_base();
        let mut found = Found::default();
        match own.level {
            Level::Track => {
                let recording: Vec<&str> = subject.keys_in("mb:recording").collect();
                let [recording] = recording.as_slice() else {
                    return Ok(found);
                };
                let mut releases = musicbrainz::releases(
                    &cx.egress,
                    base,
                    &format!("recording={recording}&type=album"),
                    RECORDING_PAGES,
                )
                .await?;
                // No album of its own: its singles and EPs come before compilations.
                if !releases.iter().any(first_type) {
                    releases.extend(
                        musicbrainz::releases(
                            &cx.egress,
                            base,
                            &format!("recording={recording}&type=single|ep"),
                            1,
                        )
                        .await?,
                    );
                }
                let edition = original_group(&releases)
                    .map(|g| original_edition(&releases, &g))
                    .unwrap_or_default();
                let own_releases = match own_track_releases(cx, base, &own).await {
                    Ok(releases) => releases,
                    Err(error) => {
                        partial(&mut found, &error);
                        vec![]
                    }
                };
                // Read releases until each service whose tracks can be listed has an album.
                let services: BTreeSet<Service> = candidates
                    .iter()
                    .map(|(_, l)| l.service)
                    .filter(|s| listable(cx, *s))
                    .collect();
                let pages = album_pages(
                    cx,
                    base,
                    &ranked(&own_releases, &edition),
                    |pages: &[Page]| {
                        services
                            .iter()
                            .all(|s| pages.iter().any(|p| p.album.service == *s))
                    },
                )
                .await?;
                found.editions =
                    place_tracks(cx, &candidates, &pages, &mut found.retry_after).await;
            }
            Level::Album => {
                let mut own_releases: Vec<String> = subject
                    .keys_in("mb:release")
                    .filter_map(hubs::mbid)
                    .collect();
                own_releases.sort();
                own_releases.truncate(OWN_RELEASES);
                // The group: the one its key names, else its own release's (read first below).
                let mut group: Option<String> = match subject
                    .keys_in("mb:release-group")
                    .collect::<Vec<_>>()
                    .as_slice()
                {
                    [one] => hubs::mbid(one),
                    _ => None,
                };
                if group.is_none() {
                    if let Some(first) = own_releases.first() {
                        group = musicbrainz::entity(&cx.egress, base, MbKind::Release, first)
                            .await?
                            .and_then(|e| e.release_group);
                    }
                }
                let edition = match &group {
                    Some(group) => {
                        let releases = musicbrainz::releases(
                            &cx.egress,
                            base,
                            &format!("release-group={group}"),
                            GROUP_PAGES,
                        )
                        .await?;
                        original_edition(&releases, group)
                    }
                    None => vec![],
                };
                let wanted: BTreeSet<String> = candidates
                    .iter()
                    .map(|(_, l)| l.canonical.clone())
                    .collect();
                let pages = album_pages(cx, base, &ranked(&own_releases, &edition), |pages| {
                    wanted
                        .iter()
                        .all(|c| pages.iter().any(|p| &p.album.canonical == c))
                })
                .await?;
                found.editions = place_albums(&candidates, &pages);
            }
            _ => {}
        }
        Ok(found)
    }
}

/// Whether a release is on an album with no secondary type: a recording's first kind of group.
fn first_type(release: &Value) -> bool {
    let group = &release["release-group"];
    group["primary-type"] == "Album"
        && group["secondary-types"]
            .as_array()
            .is_none_or(Vec::is_empty)
}

/// The original release group among `releases` (MusicBrainz's release objects, each with its
/// `release-group`): an album, then a single, then an EP, then any other, one with no secondary
/// type before one with; then the earliest first release, then the ID.
pub fn original_group(releases: &[Value]) -> Option<String> {
    releases
        .iter()
        .filter_map(|r| {
            let group = &r["release-group"];
            let id = hubs::mbid(group["id"].as_str()?)?;
            let kind = match group["primary-type"].as_str() {
                Some("Album") => 0,
                Some("Single") => 1,
                Some("EP") => 2,
                _ => 3,
            };
            let secondary = group["secondary-types"]
                .as_array()
                .is_some_and(|t| !t.is_empty());
            let first = date(&group["first-release-date"]);
            Some(((secondary, kind, first, id.clone()), id))
        })
        .min()
        .map(|(_, id)| id)
}

/// The original edition of `group`: its official releases among `releases` whose disambiguation
/// names no edition ([`edition`]), digital ones first, then worldwide (`XW`) ones, then by date
/// and ID.
pub fn original_edition(releases: &[Value], group: &str) -> Vec<String> {
    let mut picked: Vec<((bool, bool, String), String)> = releases
        .iter()
        .filter(|r| {
            r["release-group"]["id"].as_str() == Some(group)
                && r["status"] == "Official"
                && !edition(r["disambiguation"].as_str().unwrap_or_default())
        })
        .filter_map(|r| {
            let id = hubs::mbid(r["id"].as_str()?)?;
            let digital = r["media"]
                .as_array()
                .is_some_and(|m| m.iter().any(|m| m["format"] == "Digital Media"));
            Some(((!digital, r["country"] != "XW", date(&r["date"])), id))
        })
        .collect();
    picked.sort();
    let mut seen = BTreeSet::new();
    picked
        .into_iter()
        .map(|(_, id)| id)
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

/// A MusicBrainz date for ordering: an unknown one last.
fn date(v: &Value) -> String {
    v.as_str()
        .filter(|d| !d.is_empty())
        .unwrap_or("9999")
        .to_owned()
}

/// Whether a release's disambiguation names a later edition: deluxe, anniversary, remastered,
/// expanded, reissued, an edition, bonus tracks, a box set, or hi-res (`hi-res`, `24-bit`,
/// `96 kHz`). "Apple Digital Master" and "Mastered for iTunes" are no edition: they are a store's
/// copy of the original.
pub fn edition(disambiguation: &str) -> bool {
    let lower = disambiguation.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().any(|w| {
        matches!(
            *w,
            "deluxe"
                | "anniversary"
                | "remaster"
                | "remastered"
                | "expanded"
                | "reissue"
                | "reissued"
                | "edition"
                | "bonus"
                | "hires"
                | "bit"
        ) || w.ends_with("khz")
            || (w.ends_with("bit") && w.starts_with(|c: char| c.is_ascii_digit()))
    }) || words
        .windows(2)
        .any(|p| p == ["hi", "res"] || p == ["box", "set"])
}

/// The releases to read, with their ranks: the card's own (0), then the original edition's
/// (1, 2, …), each once.
fn ranked(own: &[String], edition: &[String]) -> Vec<(String, i16)> {
    let mut seen = BTreeSet::new();
    own.iter()
        .map(|id| (id.clone(), 0))
        .chain(
            edition
                .iter()
                .enumerate()
                .map(|(n, id)| (id.clone(), (n + 1).min(1000) as i16)),
        )
        .filter(|(id, _)| seen.insert(id.clone()))
        .collect()
}

/// An album page MusicBrainz relates a release to.
#[derive(Clone, Debug)]
pub struct Page {
    pub rank: i16,
    pub release: String,
    pub album: music::Link,
}

/// The album pages of the releases of `order`, best first: at most [`RELEASES_READ`] releases are
/// read, and no more once `enough` holds.
async fn album_pages(
    cx: &Context,
    base: &str,
    order: &[(String, i16)],
    enough: impl Fn(&[Page]) -> bool,
) -> Result<Vec<Page>> {
    let mut pages: Vec<Page> = vec![];
    for (release, rank) in order.iter().take(RELEASES_READ) {
        if enough(&pages) {
            break;
        }
        if let Some(entity) =
            musicbrainz::entity(&cx.egress, base, MbKind::Release, release).await?
        {
            pages.extend(release_pages(&entity, *rank));
        }
    }
    Ok(pages)
}

/// The album pages of one release, at `rank`, each once.
pub fn release_pages(entity: &musicbrainz::Entity, rank: i16) -> Vec<Page> {
    let mut seen = BTreeSet::new();
    entity
        .urls
        .iter()
        .filter_map(|u| music::parse(&u.url))
        .filter(|l| l.level == Level::Album && seen.insert(l.canonical.clone()))
        .map(|album| Page {
            rank,
            release: entity.mbid.clone(),
            album,
        })
        .collect()
}

/// The release(s) the card's own track's album is a page of, when its service names that album.
async fn own_track_releases(cx: &Context, base: &str, own: &music::Link) -> Result<Vec<String>> {
    let album = match own.service {
        Service::AppleMusic if itunes_on(cx) => itunes::track_album(cx, &own.id)
            .await?
            .map(|id| format!("https://music.apple.com/album/{id}")),
        Service::Spotify => spotify::track_album(cx, &own.id)
            .await?
            .map(|id| format!("https://open.spotify.com/album/{id}")),
        Service::Tidal if tidal_on(cx) => tidal::track_album(cx, &own.id)
            .await?
            .map(|id| format!("https://tidal.com/album/{id}")),
        Service::Deezer if deezer_on(cx) => deezer::track_album(cx, &own.id)
            .await?
            .map(|id| format!("https://www.deezer.com/album/{id}")),
        _ => None,
    };
    let Some(album) = album.as_deref().and_then(music::parse) else {
        return Ok(vec![]);
    };
    let mut releases: Vec<String> = musicbrainz::url(&cx.egress, base, &album.canonical)
        .await?
        .map(|t| t.targets)
        .unwrap_or_default()
        .into_iter()
        .filter(|(kind, _)| *kind == MbKind::Release)
        .map(|(_, id)| id)
        .collect();
    releases.truncate(OWN_RELEASES);
    Ok(releases)
}

/// Each candidate track on an album of `pages` its service lists the tracks of, at that album's
/// rank: at most [`ALBUMS_PER_SERVICE`] albums of a service are listed, best first. A list that
/// fails leaves its tracks unplaced, and the run comes back sooner (`retry`).
async fn place_tracks(
    cx: &Context,
    candidates: &[(String, music::Link)],
    pages: &[Page],
    retry: &mut Option<Duration>,
) -> Vec<Edition> {
    let mut placed: BTreeMap<String, Edition> = BTreeMap::new();
    let services: BTreeSet<Service> = candidates.iter().map(|(_, l)| l.service).collect();
    for service in services {
        let albums = pages
            .iter()
            .filter(|p| p.album.service == service)
            .take(ALBUMS_PER_SERVICE);
        for page in albums {
            let tracks = match tracks(cx, service, &page.album.id).await {
                Ok(Some(tracks)) => tracks,
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(
                        kind = crate::resolver::error_class(&error),
                        "an album's track list failed; its tracks stay unplaced"
                    );
                    *retry = Some(Duration::from_secs(300));
                    continue;
                }
            };
            for (uri, link) in candidates.iter().filter(|(_, l)| l.service == service) {
                if tracks.contains(&link.id) && !placed.contains_key(uri) {
                    placed.insert(
                        uri.clone(),
                        Edition {
                            uri: uri.clone(),
                            release: page.release.clone(),
                            rank: page.rank,
                        },
                    );
                }
            }
        }
    }
    placed.into_values().collect()
}

/// Each candidate album that is a page of a release of `pages`, at its best rank.
pub fn place_albums(candidates: &[(String, music::Link)], pages: &[Page]) -> Vec<Edition> {
    candidates
        .iter()
        .filter_map(|(uri, link)| {
            let page = pages
                .iter()
                .filter(|p| p.album.canonical == link.canonical)
                .min_by_key(|p| p.rank)?;
            Some(Edition {
                uri: uri.clone(),
                release: page.release.clone(),
                rank: page.rank,
            })
        })
        .collect()
}

/// The tracks of one album of `service`, by ID, where this build can list them and the service's
/// source runs ([`listable`]); `None` otherwise.
async fn tracks(cx: &Context, service: Service, album: &str) -> Result<Option<Vec<String>>> {
    if !listable(cx, service) {
        return Ok(None);
    }
    match service {
        Service::AppleMusic => itunes::album_songs(cx, album).await,
        Service::Spotify => spotify::album_tracks(cx, album).await,
        Service::Tidal => tidal::album_tracks(cx, album).await,
        Service::Deezer => deezer::album_tracks(cx, album).await,
        _ => Ok(None),
    }
}
/// Whether this build lists an album's tracks on `service`: Apple Music while `itunes` runs,
/// Spotify, TIDAL while its source runs with credentials, Deezer while its source is on.
fn listable(cx: &Context, service: Service) -> bool {
    match service {
        Service::AppleMusic => itunes_on(cx),
        Service::Spotify => true,
        Service::Tidal => tidal_on(cx),
        Service::Deezer => deezer_on(cx),
        _ => false,
    }
}
fn itunes_on(cx: &Context) -> bool {
    cx.config.also_on.source_enabled(&itunes::ItunesSource)
}
fn tidal_on(cx: &Context) -> bool {
    cx.config.also_on.source_enabled(&tidal::Tidal) && tidal::Tidal.ready(&cx.config)
}
fn deezer_on(cx: &Context) -> bool {
    cx.config.also_on.source_enabled(&deezer::DeezerSource)
}
/// A secondary lookup failed: the run stands and comes back sooner.
fn partial(found: &mut Found, error: &anyhow::Error) {
    tracing::debug!(
        kind = crate::resolver::error_class(error),
        "a card's own album failed; its own release stays unknown"
    );
    found.retry_after = Some(Duration::from_secs(300));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(std::fs::read(path).unwrap().trim_ascii()).unwrap()
    }
    fn releases(names: &[&str]) -> Vec<Value> {
        names
            .iter()
            .flat_map(|n| fixture(n)["releases"].as_array().unwrap().clone())
            .collect()
    }

    /// Get Lucky (MusicBrainz, captured): its original release group is Random Access Memories,
    /// not the single that came out first nor a compilation; the original edition is the 2013
    /// worldwide digital releases (Apple's Digital Master, then the other), never the 2015
    /// hi-res one or the 2023 10th anniversary edition.
    #[test]
    fn get_luckys_original_edition_is_random_access_memories_of_2013() {
        let releases = releases(&["mb-browse-releases-833f00e1-album.json"]);
        let group = original_group(&releases).unwrap();
        assert_eq!(group, "aa997ea0-2936-40bd-884d-3af8a0e064dc");
        let edition = original_edition(&releases, &group);
        assert_eq!(
            &edition[..2],
            [
                "36e2aede-346d-4931-8565-78d810d167c7",
                "5000a285-b67e-4cfc-b54b-2b98f1810d2e"
            ]
        );
        for later in [
            "ec116461-5b0d-4c98-bb44-a4de5de63076",
            "e65fcfb0-02dc-407c-ab28-8168dafcb999",
        ] {
            assert!(!edition.iter().any(|id| id == later), "{later}");
        }
    }

    /// Bohemian Rhapsody (four captured pages, 304 releases): A Night at the Opera, and of its
    /// releases the 2011 worldwide digital one before the 2009 Mexican one and the vinyl of 1975.
    #[test]
    fn bohemian_rhapsodys_original_edition_is_the_worldwide_digital_opera() {
        let releases = releases(&[
            "mb-browse-releases-b1a9c0e9-album.json",
            "mb-browse-releases-b1a9c0e9-album-100.json",
            "mb-browse-releases-b1a9c0e9-album-200.json",
            "mb-browse-releases-b1a9c0e9-album-300.json",
        ]);
        let group = original_group(&releases).unwrap();
        assert_eq!(group, "6b47c9a0-b9e1-3df9-a5e8-50a6ce0dbdbd");
        let edition = original_edition(&releases, &group);
        assert_eq!(edition[0], "59c466a7-d981-4078-bffb-10bab7b023e6");
    }

    #[test]
    fn an_edition_is_named_in_the_disambiguation_alone() {
        for later in [
            "10th anniversary edition",
            "88.2kHz/24‐bit",
            "USB FLAC, 24-bit / 88.2kHz",
            "limited box set edition",
            "deluxe remastered version",
            "2011 Remaster Hi-Res",
            "Legacy edition",
        ] {
            assert!(edition(later), "{later}");
        }
        for original in [
            "",
            "Apple Digital Master",
            "Mastered for iTunes",
            "stereo",
            "made in UK",
        ] {
            assert!(!edition(original), "{original}");
        }
    }

    /// An album link is placed by the release whose album pages name it: Kind of Blue's 2011
    /// worldwide release names Deezer, Apple Music, Qobuz and TIDAL.
    #[test]
    fn an_album_is_placed_by_the_release_it_is_a_page_of() {
        let release =
            musicbrainz::parse_entity(MbKind::Release, &fixture("mb-release-4c8af71f.json"))
                .unwrap();
        let pages = release_pages(&release, 0);
        let candidates: Vec<(String, music::Link)> = [
            "https://www.deezer.com/album/1441464",
            "https://www.deezer.com/album/7824595",
            "https://tidal.com/browse/album/655910",
        ]
        .iter()
        .map(|u| (u.to_string(), music::parse(u).unwrap()))
        .collect();
        let placed = place_albums(&candidates, &pages);
        let uris: Vec<&str> = placed.iter().map(|e| e.uri.as_str()).collect();
        assert_eq!(
            uris,
            [
                "https://www.deezer.com/album/1441464",
                "https://tidal.com/browse/album/655910"
            ]
        );
        assert!(placed
            .iter()
            .all(|e| e.rank == 0 && e.release == "4c8af71f-5746-4981-92d7-0d6c64db1155"));
    }
}
