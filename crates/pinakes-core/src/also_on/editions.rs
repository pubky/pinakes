//! The `editions` also-on source: which release each link of a recording's or an album's card is
//! on, so the card shows, of each service, its link on the card's own release, else on the
//! original edition (`link_release`, read by `also_on()`).
//!
//! A recording's original release group is the first of those MusicBrainz lists its official
//! releases in by type (an album, then a single, then an EP, then any other; one with no secondary
//! type, such as a compilation or a soundtrack, before one with), then by first release. An
//! album's is its own. The original edition is that group's official releases that name no later
//! edition (in their disambiguation, or in their title past the group's), the earliest first
//! (digital, then worldwide ones first among those of one date). The album pages MusicBrainz
//! relates to the card's own release rank 0, then the original edition's 1, 2, … in that order:
//! a service's link lands on its earliest release with a page there. A track link is on such an
//! album when its own service says so: its track's album (Apple Music through iTunes, Spotify by
//! its Web API or, for the card's own track or where the operator allows, the track's public
//! page, TIDAL and Deezer by their APIs, each while its source runs). An album link is on the
//! release whose album pages name it. A link its service puts on another album of a service the
//! original edition has a page on is on another release (rank 1000). Only the links a card may
//! show are read, never an album page nobody linked.
//!
//! Each link ends a run placed, on another release, unplaced, or kept: a link whose place the run
//! cannot tell (its service failed or refused the credentials, the release list was cut short, a
//! cap was reached) keeps the place recorded before. Only MusicBrainz failing fails the run.
use super::{
    deezer_api, itunes_api, spotify_api, tidal_api, Context, Edition, Fact, Found, Link, Source,
    Subject,
};
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
/// Track links of one service whose album is asked; the others keep their places.
const TRACKS_PER_SERVICE: usize = 8;
/// The card's own releases a link may name (`musicbrainz`'s `MAX_EDITIONS`), and the original
/// edition's named as facts.
const OWN_RELEASES: usize = 4;
/// The rank of a link its service puts on another release.
const OTHER: i16 = 1000;
/// When a run comes back after a service failed for some of its links.
const RETRY: Duration = Duration::from_secs(300);

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
    fn reads_candidates(&self) -> bool {
        true
    }
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(own) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        // The links to place, in the candidates' order (tagged members first): at the card's
        // level, on another service than its own (whose other pages the card never shows); of
        // tracks, those on a service that says which album a track is on.
        let candidates: Vec<(String, music::Link)> = subject
            .candidates
            .iter()
            .filter_map(|uri| Some((uri.clone(), music::parse(uri)?)))
            .filter(|(_, l)| l.level == own.level && l.service != own.service)
            .filter(|(_, l)| l.level == Level::Album || placeable(cx, l.service))
            .collect();
        // An album card derives its own release's pages even with nothing else to place.
        if candidates.is_empty() && own.level != Level::Album {
            return Ok(Found::default());
        }
        let base = cx.config.also_on.musicbrainz_base();
        let mut found = Found::default();
        // The frame: the releases, the original edition, the card's own releases. MusicBrainz
        // failing fails the run.
        let (releases, complete, own_releases, own_unknown) = match own.level {
            Level::Track => {
                let recording: Vec<&str> = subject.keys_in("mb:recording").collect();
                let [recording] = recording.as_slice() else {
                    return Ok(found);
                };
                let (mut releases, mut complete) = musicbrainz::releases(
                    &cx.egress,
                    base,
                    &format!("recording={recording}&type=album"),
                    RECORDING_PAGES,
                )
                .await?;
                // No album of its own: its singles and EPs come before compilations.
                if !releases.iter().any(first_type) {
                    let (singles, all) = musicbrainz::releases(
                        &cx.egress,
                        base,
                        &format!("recording={recording}&type=single|ep"),
                        1,
                    )
                    .await?;
                    releases.extend(singles);
                    complete &= all;
                }
                // The card's own album, as its own service names it: its failing keeps every
                // place, as it is the rank all others count against.
                let mut own_unknown = false;
                let own_album = match album_of(cx, &own, true).await {
                    Ok(albums) => albums,
                    Err(error) => {
                        own_unknown = true;
                        if !super::client_credentials::refused(&error) {
                            found.retry_after = Some(RETRY);
                        }
                        vec![]
                    }
                };
                // In the service's order (TIDAL's first album first), at most four.
                let mut own_releases: Vec<String> = vec![];
                for album in own_album.iter().take(OWN_RELEASES) {
                    for release in releases_of_page(cx, base, &album.canonical).await? {
                        if !own_releases.contains(&release) {
                            own_releases.push(release);
                        }
                    }
                }
                own_releases.truncate(OWN_RELEASES);
                (releases, complete, own_releases, own_unknown)
            }
            Level::Album => {
                let mut own_releases: Vec<String> = subject
                    .keys_in("mb:release")
                    .filter_map(hubs::mbid)
                    .collect();
                own_releases.sort();
                own_releases.truncate(OWN_RELEASES);
                // A link MusicBrainz relates to several releases of one group is keyed by the
                // group: its own releases are those its URL is related to.
                if own_releases.is_empty() {
                    own_releases = releases_of_page(cx, base, &own.canonical).await?;
                }
                // The group: the one its key names, else its own release's.
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
                let (releases, complete) = match &group {
                    Some(group) => {
                        musicbrainz::releases(
                            &cx.egress,
                            base,
                            &format!("release-group={group}"),
                            GROUP_PAGES,
                        )
                        .await?
                    }
                    None => (vec![], false),
                };
                (releases, complete, own_releases, false)
            }
            _ => return Ok(found),
        };
        let edition = original_group(&releases)
            .map(|g| original_edition(&releases, &g))
            .unwrap_or_default();
        let mut pages = album_pages(cx, base, &releases, &ranked(&own_releases, &edition)).await?;
        // A list cut short may lack an earlier release: only the card's own release ranks.
        if !complete {
            pages.retain(|p| p.rank == 0);
        }
        // The card's own album failing keeps every place: its release is the rank all others
        // count against.
        let outcomes = match own.level {
            _ if own_unknown => candidates
                .iter()
                .map(|(uri, _)| (uri.clone(), Outcome::Keep { retry: false }))
                .collect(),
            Level::Track => place_tracks(cx, &candidates, &pages).await,
            _ => candidates
                .iter()
                .map(|(uri, link)| (uri.clone(), place_album(link, &pages)))
                .collect(),
        };
        for (uri, outcome) in outcomes {
            match outcome {
                // An unplaced link of a list cut short may be on a release it lacks.
                Outcome::Unplaced | Outcome::Other if !complete => found.keep.push(uri),
                Outcome::Placed { rank, release } => found.editions.push(Edition {
                    uri,
                    release: Some(release),
                    rank,
                }),
                Outcome::Other | Outcome::Gone => found.editions.push(Edition {
                    uri,
                    release: None,
                    rank: OTHER,
                }),
                Outcome::Unplaced => {}
                Outcome::Keep { retry } => {
                    found.keep.push(uri);
                    if retry {
                        found.retry_after = Some(RETRY);
                    }
                }
            }
        }
        // An album card's own release's pages on other services are links of its own, placed
        // first: `musicbrainz` derives none for a link it relates to several releases of a group
        // (it keys the group, which names no pages). Each is keyed by its release, as
        // `musicbrainz` keys a release's relations.
        if own.level == Level::Album {
            // Every run names them all (a link no run names goes), and places those no
            // candidate already stands for.
            let known: BTreeSet<&str> = candidates
                .iter()
                .map(|(_, l)| l.canonical.as_str())
                .collect();
            let mut added = BTreeSet::new();
            for page in pages
                .iter()
                .filter(|p| p.rank == 0 && p.album.service != own.service)
            {
                if !added.insert(page.album.canonical.clone()) {
                    continue;
                }
                found.links.push(
                    Link::new(
                        page.album.canonical.clone(),
                        page.album.service.name(),
                        "musicbrainz",
                    )
                    .keyed(format!("mb:release:{}", page.release)),
                );
                if !known.contains(page.album.canonical.as_str()) {
                    found.editions.push(Edition {
                        uri: page.album.canonical.clone(),
                        release: Some(page.release.clone()),
                        rank: 0,
                    });
                }
            }
        }
        // The releases it reads as the card's own and as the original edition, by MBID.
        found.facts.extend(
            own_releases
                .iter()
                .map(|r| Fact::new("release", r.as_str())),
        );
        let mut original: Vec<&str> = vec![];
        for page in pages.iter().filter(|p| p.rank > 0) {
            if !original.contains(&page.release.as_str()) && original.len() < OWN_RELEASES {
                original.push(&page.release);
            }
        }
        found.facts.extend(
            original
                .into_iter()
                .map(|r| Fact::new("original_release", r)),
        );
        Ok(found)
    }
}

/// How a run ends for one link.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// On a release of the card's own (rank 0) or of the original edition (1, 2, …).
    Placed { rank: i16, release: String },
    /// On another album of a service an own or original release has a page on.
    Other,
    /// On no release its service could name.
    Unplaced,
    /// Its service names no album for it (gone, delisted): ranked with links on another release.
    Gone,
    /// Not told: the place recorded before stays (`retry`: its service failed).
    Keep { retry: bool },
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
/// type before one with; then the earliest first release, then the ID. A group with secondary
/// types stays eligible: a song first out on a soundtrack or a live album has no other. Secondary
/// types come before the kind: by kind first, a greatest-hits compilation album would beat the
/// original non-album single.
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

/// The original edition of `group`: its official releases among `releases` that name no later
/// edition ([`later_edition`]), the earliest first; of one date, digital ones, then worldwide
/// (`XW`) ones, then by ID.
pub fn original_edition(releases: &[Value], group: &str) -> Vec<String> {
    let mut picked: Vec<((String, bool, bool), String)> = releases
        .iter()
        .filter(|r| {
            r["release-group"]["id"].as_str() == Some(group)
                && r["status"] == "Official"
                && !later_edition(r)
        })
        .filter_map(|r| {
            let id = hubs::mbid(r["id"].as_str()?)?;
            let digital = r["media"]
                .as_array()
                .is_some_and(|m| m.iter().any(|m| m["format"] == "Digital Media"));
            Some(((date(&r["date"]), !digital, r["country"] != "XW"), id))
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

/// Whether a release (MusicBrainz's release object) is a later edition: its disambiguation names
/// one ([`edition`]), or its title past its group's title does (`… (Deluxe Edition)`). The
/// group's own title is never read: an album may be called "Remastered".
pub fn later_edition(release: &Value) -> bool {
    let title = release["title"].as_str().unwrap_or_default();
    let group = release["release-group"]["title"]
        .as_str()
        .unwrap_or_default();
    edition(release["disambiguation"].as_str().unwrap_or_default())
        || title.strip_prefix(group).is_some_and(edition)
}

/// Whether a text names a later edition: deluxe, legacy, anniversary, remastered, expanded,
/// reissued, special, collector's, bonus tracks, a box set, or hi-res (`hi-res`, `high
/// resolution`, a bit depth over 16 or a sample rate over 48 kHz: `24-bit`, `96 kHz`); an
/// "edition" alone (standard, original, explicit) is none, nor is a CD's `16-bit / 44.1 kHz`.
/// "Apple Digital Master" and "Mastered for iTunes" are none either: they are a store's copy of
/// the original.
pub fn edition(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().any(|w| {
        matches!(
            *w,
            "deluxe"
                | "legacy"
                | "anniversary"
                | "remaster"
                | "remastered"
                | "expanded"
                | "reissue"
                | "reissued"
                | "collector"
                | "collectors"
                | "bonus"
                | "hires"
        )
    }) || words.windows(2).any(|p| {
        p == ["hi", "res"]
            || p == ["high", "resolution"]
            || p == ["box", "set"]
            || p == ["special", "edition"]
            || p == ["super", "deluxe"]
    }) || amounts(&lower, "bit").iter().any(|bits| *bits > 16.0)
        || amounts(&lower, "khz").iter().any(|khz| *khz > 48.0)
}

/// The numbers `text` writes before `unit` (`24-bit`, `24 bit`, `88.2kHz`, `96 khz`).
fn amounts(text: &str, unit: &str) -> Vec<f64> {
    text.match_indices(unit)
        .filter_map(|(at, _)| {
            let before = text[..at].trim_end_matches([' ', '-', '‐', '/']);
            let start = before
                .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map_or(0, |i| {
                    i + before[i..].chars().next().map_or(1, char::len_utf8)
                });
            before[start..].parse().ok()
        })
        .collect()
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
                .map(|(n, id)| (id.clone(), (n + 1).min(999) as i16)),
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

/// The album pages of the releases of `order`, at their ranks: as the browse of `releases`
/// lists them (with their URL relations), or read, for one of the card's own releases it leaves
/// out (at most [`OWN_RELEASES`]).
async fn album_pages(
    cx: &Context,
    base: &str,
    releases: &[Value],
    order: &[(String, i16)],
) -> Result<Vec<Page>> {
    let listed: BTreeMap<&str, &Value> = releases
        .iter()
        .filter_map(|r| Some((r["id"].as_str()?, r)))
        .collect();
    let mut pages: Vec<Page> = vec![];
    for (release, rank) in order {
        let entity = match listed.get(release.as_str()) {
            Some(r) => musicbrainz::parse_entity(MbKind::Release, r),
            None if *rank == 0 => {
                musicbrainz::entity(&cx.egress, base, MbKind::Release, release).await?
            }
            None => None,
        };
        if let Some(entity) = entity {
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

/// The releases MusicBrainz relates an album page to (at most [`OWN_RELEASES`]).
async fn releases_of_page(cx: &Context, base: &str, page: &str) -> Result<Vec<String>> {
    let mut releases: Vec<String> = musicbrainz::url(&cx.egress, base, page)
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

/// Each candidate track's outcome, in the candidates' order: placed by the album its own service
/// names, at that page's best rank; at most [`TRACKS_PER_SERVICE`] tracks of a service are asked,
/// and the rest keep their places.
async fn place_tracks(
    cx: &Context,
    candidates: &[(String, music::Link)],
    pages: &[Page],
) -> Vec<(String, Outcome)> {
    let mut outcomes = vec![];
    let mut asked: BTreeMap<Service, usize> = BTreeMap::new();
    for (uri, link) in candidates {
        // A service no page names has nothing to place its tracks on.
        if !pages.iter().any(|p| p.album.service == link.service) {
            outcomes.push((uri.clone(), Outcome::Unplaced));
            continue;
        }
        let n = asked.entry(link.service).or_default();
        if *n == TRACKS_PER_SERVICE {
            outcomes.push((uri.clone(), Outcome::Keep { retry: false }));
            continue;
        }
        *n += 1;
        let outcome = match album_of(cx, link, false).await {
            Ok(albums) => place(link.service, &albums, pages),
            Err(error) => {
                tracing::debug!(
                    kind = crate::resolver::error_class(&error),
                    "a track's album failed; its place stays"
                );
                Outcome::Keep {
                    retry: !super::client_credentials::refused(&error),
                }
            }
        };
        outcomes.push((uri.clone(), outcome));
    }
    outcomes
}

/// An album link's outcome: placed on the release whose album pages name it, at its best rank.
pub fn place_album(link: &music::Link, pages: &[Page]) -> Outcome {
    place(link.service, std::slice::from_ref(link), pages)
}

/// The outcome of a link on `service` that is on `albums`: placed at the best rank of their pages
/// in `pages`; on another release when an own or original release has a page on `service` and
/// none is among `albums`; gone when its service names no album for it; else unplaced. A Discogs
/// master is the release group itself, never another release.
pub fn place(service: Service, albums: &[music::Link], pages: &[Page]) -> Outcome {
    if albums.is_empty() {
        return Outcome::Gone;
    }
    let best = pages
        .iter()
        .filter(|p| albums.iter().any(|a| a.canonical == p.album.canonical))
        .min_by_key(|p| p.rank);
    match best {
        Some(page) => Outcome::Placed {
            rank: page.rank,
            release: page.release.clone(),
        },
        None if !albums.is_empty()
            && pages.iter().any(|p| p.album.service == service)
            && !albums.iter().any(|a| a.id.starts_with("master/")) =>
        {
            Outcome::Other
        }
        None => Outcome::Unplaced,
    }
}

/// The albums a track is on, as its own service says ([`placeable`]); none when it says none.
/// `own`: the card's own track, whose public Spotify page is read where the API does not answer.
async fn album_of(cx: &Context, track: &music::Link, own: bool) -> Result<Vec<music::Link>> {
    let own_spotify =
        own && track.service == Service::Spotify && super::enabled_by_id(&cx.config, "spotify");
    if !placeable(cx, track.service) && !own_spotify {
        return Ok(vec![]);
    }
    let albums: Vec<String> = match track.service {
        Service::AppleMusic => itunes_api::track_album(cx, &track.id)
            .await?
            .map(|id| format!("https://music.apple.com/album/{id}"))
            .into_iter()
            .collect(),
        Service::Spotify => {
            spotify_api::track_album(cx, &track.id, own || cx.config.also_on.spotify_pages)
                .await?
                .map(|id| format!("https://open.spotify.com/album/{id}"))
                .into_iter()
                .collect()
        }
        Service::Tidal => tidal_api::track_albums(cx, &track.id)
            .await?
            .into_iter()
            .map(|id| format!("https://tidal.com/album/{id}"))
            .collect(),
        Service::Deezer => deezer_api::track_album(cx, &track.id)
            .await?
            .map(|id| format!("https://www.deezer.com/album/{id}"))
            .into_iter()
            .collect(),
        _ => vec![],
    };
    Ok(albums.iter().filter_map(|a| music::parse(a)).collect())
}
/// Whether `service` says which album a track is on, while its source runs: Apple Music through
/// iTunes, Spotify by its Web API with the operator's credentials (or its track's public page,
/// with `[also_on] spotify_pages`), TIDAL with the operator's credentials, Deezer.
fn placeable(cx: &Context, service: Service) -> bool {
    // By their IDs: this source goes without each service's own source, and each without it.
    let on = |id: &str| super::enabled_by_id(&cx.config, id);
    match service {
        Service::AppleMusic => on("itunes"),
        Service::Spotify => on("spotify") && spotify_api::places(&cx.config),
        Service::Tidal => on("tidal") && tidal_api::configured(&cx.config),
        Service::Deezer => on("deezer"),
        _ => false,
    }
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
    /// not the single that came out first nor a compilation; its original edition is the releases
    /// of 2013 and 2014, the earliest first (Apple's Digital Master before the other worldwide
    /// digital release of the same day), never the 2015 hi-res one or the 2023 10th anniversary
    /// edition.
    #[test]
    fn get_luckys_original_edition_is_random_access_memories_of_2013() {
        let releases = releases(&["mb-browse-releases-833f00e1-album.json"]);
        let group = original_group(&releases).unwrap();
        assert_eq!(group, "aa997ea0-2936-40bd-884d-3af8a0e064dc");
        let edition = original_edition(&releases, &group);
        let at = |id: &str| edition.iter().position(|e| e == id);
        assert!(
            at("36e2aede-346d-4931-8565-78d810d167c7") < at("5000a285-b67e-4cfc-b54b-2b98f1810d2e")
        );
        assert!(at("36e2aede-346d-4931-8565-78d810d167c7").is_some());
        for later in [
            "ec116461-5b0d-4c98-bb44-a4de5de63076",
            "e65fcfb0-02dc-407c-ab28-8168dafcb999",
        ] {
            assert_eq!(at(later), None, "{later}");
        }
    }

    /// Bohemian Rhapsody (four captured pages, 304 releases): A Night at the Opera, the vinyl of
    /// 1975 first; of its digital releases the 2009 Mexican one before the 2011 worldwide one.
    #[test]
    fn bohemian_rhapsodys_original_edition_is_the_opera_earliest_first() {
        let releases = releases(&[
            "mb-browse-releases-b1a9c0e9-album.json",
            "mb-browse-releases-b1a9c0e9-album-100.json",
            "mb-browse-releases-b1a9c0e9-album-200.json",
            "mb-browse-releases-b1a9c0e9-album-300.json",
        ]);
        let group = original_group(&releases).unwrap();
        assert_eq!(group, "6b47c9a0-b9e1-3df9-a5e8-50a6ce0dbdbd");
        let edition = original_edition(&releases, &group);
        let first = releases
            .iter()
            .find(|r| r["id"] == edition[0].as_str())
            .unwrap();
        assert!(first["date"].as_str().unwrap().starts_with("1975"));
        let at = |id: &str| edition.iter().position(|e| e == id);
        assert!(
            at("9ebe5c48-9a91-4e55-9ac1-5040a5a22088") < at("59c466a7-d981-4078-bffb-10bab7b023e6")
        );
    }

    /// A song first out on a soundtrack has only groups with secondary types: the earliest of
    /// them is its original.
    #[test]
    fn a_soundtrack_only_recording_takes_its_first_soundtrack() {
        let release = |id: &str, group: &str, first: &str| {
            serde_json::json!({"id": id, "status": "Official", "date": first,
                "release-group": {"id": group, "primary-type": "Album",
                    "secondary-types": ["Soundtrack"], "first-release-date": first}})
        };
        let releases = [
            release(
                "11111111-1111-1111-1111-111111111111",
                "22222222-2222-2222-2222-222222222222",
                "2001-05-01",
            ),
            release(
                "33333333-3333-3333-3333-333333333333",
                "44444444-4444-4444-4444-444444444444",
                "1999-03-01",
            ),
        ];
        assert_eq!(
            original_group(&releases).as_deref(),
            Some("44444444-4444-4444-4444-444444444444")
        );
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
        let placed: Vec<Outcome> = candidates
            .iter()
            .map(|(_, link)| place_album(link, &pages))
            .collect();
        let release = "4c8af71f-5746-4981-92d7-0d6c64db1155".to_owned();
        let on = Outcome::Placed { rank: 0, release };
        // Deezer's other album: Deezer has a page here, so it is on another release.
        assert_eq!(placed, [on.clone(), Outcome::Other, on]);
    }
}
