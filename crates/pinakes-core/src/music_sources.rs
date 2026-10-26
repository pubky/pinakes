//! "Also on" for music: the same track, album or artist on other services, from exact hub
//! lookups only (never a title or name).
//!
//! Who may vouch for a key: a hub (MusicBrainz, Wikidata) or a known music service's own record
//! of its own object. Any other page's claim is a fact at most.
//!
//! - [`ServiceSource`]: the ISRC a music service's own track page states (its JSON-LD
//!   `isrcCode`) as the key `isrc:…` when the service vouches for it (TIDAL,
//!   `Service::vouches_isrc`), and as a fact with its artist, album and length.
//! - `tidal` (with an operator's TIDAL client ID and secret): a TIDAL
//!   album's barcode and a track's ISRC from TIDAL's own API, as keys.
//! - [`MusicBrainzSource`]: what MusicBrainz relates the tagged link to (one recording, release
//!   or artist; for a track it does not relate, the one recording its page's own ISRC is on; for
//!   an album it does not relate, the releases carrying the album's barcode key),
//!   that entity's current links on music services at the same level, and its MBIDs, ISRCs and
//!   barcode as keys. An album link related to several releases of one release group (editions
//!   MusicBrainz keeps apart) takes that release group: its key, and the links and facts every
//!   one of those releases has.
//! - [`DeezerSource`] (off by default: Deezer's API terms are for non-commercial use): a Deezer
//!   link's own ISRC or barcode from Deezer's API, as keys; for another service's track or album
//!   with an ISRC or barcode key, the one Deezer track or album the API names for it, keyed.
//! - [`ItunesSource`]: for an album with a barcode key, the Apple Music album iTunes'
//!   `lookup?upc=` answers when exactly one album answers, keyed by the barcode.
//! - [`WikidataSource`]: the one item carrying the link's service ID, that item's other music
//!   service IDs at the same level as links, and its Q-id and MusicBrainz IDs as keys.
//!
//! The level is exact: a track lists tracks (and videos of the recording), an album lists
//! albums (and YouTube's album playlists), an artist lists artist pages.
use crate::{
    also_on::{Context, Found, Key, Link, Subject},
    hubs::{self, musicbrainz, wikidata, MbKind},
    music::{self, discogs, Level, Service},
};
use anyhow::Result;

/// The level a link is joined at: a video counts as its recording, a YouTube playlist as the
/// album it holds; other playlists are not joined.
pub(crate) fn join_level(link: &music::Link) -> Option<Level> {
    match (link.level, link.service) {
        (Level::Video, _) => Some(Level::Track),
        (Level::Playlist, Service::YouTube) => Some(Level::Album),
        (Level::Playlist, _) => None,
        (level, _) => Some(level),
    }
}
/// A link found on a hub, when it is a music link at `level`: its canonical URL and service.
pub(crate) fn derived(url: &str, level: Level, via: &str) -> Option<Link> {
    let found = music::parse(url)?;
    (join_level(&found)? == level).then(|| Link::new(found.canonical, found.service.name(), via))
}

/// At most this many of a recording's ISRCs become keys (the MBID key always fits).
pub(crate) const MAX_ISRC_KEYS: usize = 8;

/// The ISRC a music service's track page states (its JSON-LD `isrcCode`), never another host's
/// claim: a fact.
pub(crate) fn page_isrc(subject: &Subject) -> Option<(Service, String)> {
    let link = music::parse(&subject.uri)?;
    (join_level(&link)? == Level::Track && link.level == Level::Track).then_some(())?;
    let isrc = music::isrc(subject.doc.as_ref()?.attributes.get("isrc")?)?;
    Some((link.service, isrc))
}
/// The page's ISRC when its service vouches for it (`Service::vouches_isrc`): a key.
pub(crate) fn own_isrc(subject: &Subject) -> Option<String> {
    page_isrc(subject)
        .filter(|(service, _)| service.vouches_isrc())
        .map(|(_, isrc)| isrc)
}

/// The length of a track as its service's page or record states it, in seconds (`duration`,
/// an ISO 8601 duration in the document's attributes).
pub(crate) fn page_duration(subject: &Subject) -> Option<i64> {
    duration_secs(subject.doc.as_ref()?.attributes.get("duration")?)
}
/// An ISO 8601 duration as seconds, when it is a track's length (a second to a day).
pub(crate) fn duration_secs(iso: &str) -> Option<i64> {
    crate::event::time::duration(iso)
        .map(|d| d.num_seconds())
        .filter(|s| (1..=86_400).contains(s))
}
/// How far a service's duration may be from a recording's length for the recording to be the
/// track's, in seconds.
pub const LENGTH_WINDOW_SECS: u64 = 3;
/// What an ISRC a service states for its own track names on MusicBrainz.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IsrcMatch {
    /// MusicBrainz knows no recording with it: the ISRC is the track's own key.
    Unknown,
    /// Exactly one recording has it: the ISRC is a key, and names that recording.
    One(String),
    /// Several recordings have it, and the service's duration picks one: the recording whose
    /// length is within [`LENGTH_WINDOW_SECS`] of it while every other's is known and is not.
    /// The recording's MBID is the key; the ISRC stays a fact.
    Picked(String),
    /// Several recordings have it and nothing tells them apart: the ISRC is a fact only, as a
    /// key would join whichever of their groups took it first.
    Shared,
}
/// Which recording `isrc`, a service's own record of its track, names: see [`IsrcMatch`].
/// `duration` is the track's length by the service, in seconds.
pub(crate) async fn isrc_match(
    cx: &Context,
    isrc: &str,
    duration: Option<i64>,
) -> Result<IsrcMatch> {
    let recordings =
        musicbrainz::isrc(&cx.egress, cx.config.also_on.musicbrainz_base(), isrc).await?;
    Ok(match recordings.as_deref() {
        None | Some([]) => IsrcMatch::Unknown,
        Some([one]) => IsrcMatch::One(one.mbid.clone()),
        Some(several) => picked(several, duration).map_or(IsrcMatch::Shared, IsrcMatch::Picked),
    })
}
/// Of several recordings, the one a track's `duration` picks: its length is within
/// [`LENGTH_WINDOW_SECS`] of it, and every other recording's length is known and is not.
pub fn picked(recordings: &[musicbrainz::IsrcRecording], duration: Option<i64>) -> Option<String> {
    let secs = u64::try_from(duration?).ok()?;
    let close = |r: &musicbrainz::IsrcRecording| {
        r.length_ms
            .map(|ms| ms.abs_diff(secs * 1000) <= LENGTH_WINDOW_SECS * 1000)
    };
    let mut near = recordings.iter().filter(|r| close(r) == Some(true));
    let one = near.next()?;
    (near.next().is_none() && recordings.iter().all(|r| close(r).is_some()))
        .then(|| one.mbid.clone())
}
/// The key a service's own ISRC gives its track (`via` the service): the ISRC itself, or the
/// recording a shared ISRC's duration picked; none for a shared ISRC nothing tells apart.
pub(crate) fn isrc_key(isrc: &str, found: &IsrcMatch, via: &str) -> Option<crate::also_on::Key> {
    let key = match found {
        IsrcMatch::Unknown | IsrcMatch::One(_) => format!("isrc:{isrc}"),
        IsrcMatch::Picked(mbid) => hubs::key::mb(MbKind::Recording, mbid)?,
        IsrcMatch::Shared => return None,
    };
    Some(crate::also_on::Key {
        key,
        via: via.into(),
    })
}

/// A MusicBrainz page's card, from the entity MusicBrainz serves (`musicbrainz_entity` reads the
/// same answer): its title, its type (a recording, an album, an artist that is a person or a
/// group), the artist credit and a recording's length. An ISRC's page is its one recording's card
/// under the ISRC's own link, or the ISRC's when several recordings or none have it.
/// The forms of key the music sources record (`also_on::Source::key_forms`): ISRCs, barcodes,
/// MusicBrainz IDs at each level, Wikidata items.
pub const KEY_FORMS: &[(&str, &str)] = &[
    ("isrc", "USIR10211559"),
    ("upc", "0602498688373"),
    ("mb:recording", "8f3471b5-7e6a-48da-86a9-c1c07a0f47ae"),
    ("mb:release", "8f3471b5-7e6a-48da-86a9-c1c07a0f47ae"),
    ("mb:release-group", "8f3471b5-7e6a-48da-86a9-c1c07a0f47ae"),
    ("mb:artist", "8f3471b5-7e6a-48da-86a9-c1c07a0f47ae"),
    ("wikidata", "Q42"),
];
pub fn musicbrainz_card(
    uri: &str,
    entity: Option<&musicbrainz::Entity>,
    isrc: Option<&str>,
    ttl: u64,
) -> crate::model::ResourceMeta {
    let mut doc = crate::model::ResourceMeta::new(uri, "musicbrainz", ttl);
    doc.site = "MusicBrainz".into();
    let Some(entity) = entity else {
        doc.set_type("MusicRecording");
        doc.name = isrc.map(|i| format!("ISRC {i}"));
        return doc;
    };
    match entity.kind {
        MbKind::Recording => doc.set_type("MusicRecording"),
        MbKind::Release | MbKind::ReleaseGroup => doc.set_type("MusicAlbum"),
        MbKind::Artist if entity.artist_type.as_deref() == Some("Person") => doc.set_type("Person"),
        MbKind::Artist => doc.set_type("MusicGroup"),
    }
    doc.name.clone_from(&entity.title);
    let mut about: Vec<String> = entity.artist.iter().cloned().collect();
    if let Some(ms) = entity.length_ms {
        let secs = (ms + 500) / 1000;
        about.push(if secs >= 3600 {
            format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
        } else {
            format!("{}:{:02}", secs / 60, secs % 60)
        });
    }
    doc.description = (!about.is_empty()).then(|| about.join(" · "));
    if let Some(artist) = &entity.artist {
        doc.attributes.insert("artist".into(), artist.clone());
    }
    doc
}

/// What a performer's link says about the artist: a MusicBrainz artist page names the MBID
/// outright; an artist's page on a known music service, on Songkick or on Bandsintown is looked
/// up on MusicBrainz by URL, as MusicBrainz stores it (Songkick and Bandsintown artists by
/// number, without the slug and parameters). The lookups of one event's performers go in one
/// request (`prefetch_urls`), at most two links per performer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtistLink {
    Mbid(String),
    Lookup(String),
}
pub fn artist_link(url: &str) -> Option<ArtistLink> {
    if let Some((MbKind::Artist, mbid, _)) = musicbrainz::entity_link(url) {
        return Some(ArtistLink::Mbid(mbid));
    }
    if let Some(link) = music::parse(url) {
        return (link.level == Level::Artist).then_some(ArtistLink::Lookup(link.canonical));
    }
    let u = url::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let segments: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let number = |s: &str| {
        s.split('-')
            .next()
            .filter(|n| (1..=12).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_owned)
    };
    match (u.host_str()?, segments.as_slice()) {
        ("www.songkick.com" | "songkick.com", ["artists", id]) => {
            number(id).map(|n| ArtistLink::Lookup(format!("https://www.songkick.com/artists/{n}")))
        }
        ("www.bandsintown.com" | "bandsintown.com", ["a", id]) => {
            number(id).map(|n| ArtistLink::Lookup(format!("https://www.bandsintown.com/a/{n}")))
        }
        _ => None,
    }
}

/// What MusicBrainz says about one entity, or about up to four releases of one release group
/// (`kind`, `ids`): their MBIDs (the release group's alone for several releases), the recording's
/// ISRCs and the release's barcode as keys, their facts, and their current links on music
/// services at `level`, each keyed by the entity MusicBrainz names it on. Nothing when an entity
/// is unknown, or when several releases are of different release groups.
pub(crate) async fn entities_found(
    cx: &Context,
    base: &str,
    kind: MbKind,
    ids: &[String],
    level: Level,
    edition: bool,
) -> Result<Found> {
    let mut entities = vec![];
    for id in ids {
        let Some(entity) = musicbrainz::entity(&cx.egress, base, kind, id).await? else {
            return Ok(Found::default());
        };
        entities.push(entity);
    }
    let mut found = Found::default();
    let mut key = |kind: MbKind, id: &str| {
        if let Some(key) = hubs::key::mb(kind, id) {
            found.keys.push(Key {
                key,
                via: "musicbrainz".into(),
            });
        }
    };
    let entity = &entities[0];
    // Several releases: the release group's key alone, so editions stay apart.
    if entities.len() == 1 {
        key(kind, &entity.mbid);
    }
    match &entity.release_group {
        Some(group)
            if entities
                .iter()
                .all(|e| e.release_group.as_ref() == Some(group)) =>
        {
            // An edition's link (a Discogs release) is not the album: no release group's key.
            if !edition {
                key(MbKind::ReleaseGroup, group)
            }
        }
        // Releases of different release groups: ambiguous, an edition's link too.
        _ if entities.len() > 1 => return Ok(Found::default()),
        _ => {}
    }
    for isrc in entity.isrcs.iter().take(MAX_ISRC_KEYS) {
        found.keys.push(Key {
            key: format!("isrc:{isrc}"),
            via: "musicbrainz".into(),
        });
    }
    // What every entity has (one entity: all of it).
    found.facts = facts(entity);
    // A recording's original album ranks the card's links of one service (the track there).
    if kind == MbKind::Recording {
        if let Some(album) = musicbrainz::original_album(&cx.egress, base, &entity.mbid).await? {
            found
                .facts
                .push(crate::also_on::Fact::new("original_album", album));
        }
    }
    found
        .facts
        .retain(|f| entities[1..].iter().all(|e| facts(e).contains(f)));
    let links = |e: &musicbrainz::Entity| -> Vec<Link> {
        e.urls
            .iter()
            .filter(|u| discogs_fits(&u.url, kind))
            .filter_map(|u| derived(&u.url, level, "musicbrainz"))
            .collect()
    };
    found.links = links(entity);
    found.links.retain(|l| {
        entities[1..]
            .iter()
            .all(|e| links(e).iter().any(|o| o.uri == l.uri))
    });
    // Each link is keyed by the entity MusicBrainz names it on (the release group for
    // editions), so a tagged link that is it joins even when its own lookup is ambiguous.
    let named = if entities.len() == 1 {
        hubs::key::mb(kind, &entity.mbid)
    } else {
        entity
            .release_group
            .as_deref()
            .and_then(|g| hubs::key::mb(MbKind::ReleaseGroup, g))
    };
    for link in &mut found.links {
        link.key.clone_from(&named);
    }
    let barcode = entity
        .barcode
        .as_ref()
        .filter(|b| entities.iter().all(|e| e.barcode.as_ref() == Some(*b)));
    // A barcode is a key (the services that look albums up by it start from it).
    if let Some(upc) = barcode.and_then(|b| crate::barcode::gtin13(b)) {
        found.keys.push(Key {
            key: format!("upc:{upc}"),
            via: "musicbrainz".into(),
        });
    }
    Ok(found)
}

/// Whether a Discogs page MusicBrainz relates to a `kind` entity is at its level: an artist's
/// artist, a release group's master, a release's release (any other link: yes).
pub(crate) fn discogs_fits(url: &str, kind: MbKind) -> bool {
    discogs::of_uri(url).is_none_or(|(discogs, _)| {
        matches!(
            (discogs, kind),
            (discogs::Kind::Artist, MbKind::Artist)
                | (discogs::Kind::Master, MbKind::ReleaseGroup)
                | (discogs::Kind::Release, MbKind::Release)
        )
    })
}

/// What a MusicBrainz entity says about the object: its MBID, ISRCs, barcode (`upc`), artist
/// credit and length (`duration`, in seconds).
pub(crate) fn facts(entity: &musicbrainz::Entity) -> Vec<crate::also_on::Fact> {
    use crate::also_on::Fact;
    let mut facts = vec![Fact::new("mbid", &entity.mbid)];
    facts.extend(entity.isrcs.iter().map(|isrc| Fact::new("isrc", isrc)));
    facts.extend(entity.barcode.iter().map(|upc| Fact::new("upc", upc)));
    facts.extend(
        entity
            .artist
            .iter()
            .map(|artist| Fact::new("artist", artist)),
    );
    facts.extend(
        entity
            .length_ms
            .map(|ms| Fact::new("duration", ((ms + 500) / 1000).to_string())),
    );
    facts.extend(
        entity
            .artist_type
            .iter()
            .map(|t| Fact::new("artist_type", t)),
    );
    facts
}

/// The Wikidata properties of music services, by service and level, and how each value makes
/// a URL (`{sf}` is the Apple storefront).
pub(crate) const PROPERTIES: &[(&str, Service, Level, &str)] = &[
    (
        "P2207",
        Service::Spotify,
        Level::Track,
        "https://open.spotify.com/track/{}",
    ),
    (
        "P2205",
        Service::Spotify,
        Level::Album,
        "https://open.spotify.com/album/{}",
    ),
    (
        "P1902",
        Service::Spotify,
        Level::Artist,
        "https://open.spotify.com/artist/{}",
    ),
    (
        "P10110",
        Service::AppleMusic,
        Level::Track,
        "https://music.apple.com/{sf}/song/{}",
    ),
    (
        "P2281",
        Service::AppleMusic,
        Level::Album,
        "https://music.apple.com/{sf}/album/{}",
    ),
    (
        "P2850",
        Service::AppleMusic,
        Level::Artist,
        "https://music.apple.com/{sf}/artist/{}",
    ),
    (
        "P2724",
        Service::Deezer,
        Level::Track,
        "https://www.deezer.com/track/{}",
    ),
    (
        "P2723",
        Service::Deezer,
        Level::Album,
        "https://www.deezer.com/album/{}",
    ),
    (
        "P2722",
        Service::Deezer,
        Level::Artist,
        "https://www.deezer.com/artist/{}",
    ),
    (
        "P4578",
        Service::Tidal,
        Level::Track,
        "https://tidal.com/track/{}",
    ),
    (
        "P4577",
        Service::Tidal,
        Level::Album,
        "https://tidal.com/album/{}",
    ),
    (
        "P4576",
        Service::Tidal,
        Level::Artist,
        "https://tidal.com/artist/{}",
    ),
    (
        "P11770",
        Service::Qobuz,
        Level::Album,
        "https://open.qobuz.com/album/{}",
    ),
    (
        "P11578",
        Service::Qobuz,
        Level::Artist,
        "https://open.qobuz.com/artist/{}",
    ),
    (
        "P1651",
        Service::YouTube,
        Level::Video,
        "https://www.youtube.com/watch?v={}",
    ),
    (
        "P4300",
        Service::YouTube,
        Level::Playlist,
        "https://www.youtube.com/playlist?list={}",
    ),
    (
        "P2397",
        Service::YouTube,
        Level::Artist,
        "https://www.youtube.com/channel/{}",
    ),
    (
        "P3040",
        Service::SoundCloud,
        Level::Artist,
        "https://soundcloud.com/{}",
    ),
    (
        "P13398",
        Service::AmazonMusic,
        Level::Track,
        "https://music.amazon.com/tracks/{}",
    ),
    (
        "P6276",
        Service::AmazonMusic,
        Level::Artist,
        "https://music.amazon.com/artists/{}",
    ),
    (
        "P3283",
        Service::Bandcamp,
        Level::Artist,
        "https://{}.bandcamp.com/",
    ),
    // An album item names its master; a release (P2206) is an edition's (`property_of`).
    (
        "P1953",
        Service::Discogs,
        Level::Artist,
        "https://www.discogs.com/artist/{}",
    ),
    (
        "P1954",
        Service::Discogs,
        Level::Album,
        "https://www.discogs.com/master/{}",
    ),
];
/// The MusicBrainz property of each level, whose values become keys.
pub(crate) fn mb_property(level: Level) -> (&'static str, MbKind) {
    match level {
        Level::Track | Level::Video => ("P4404", MbKind::Recording),
        Level::Artist => ("P434", MbKind::Artist),
        _ => ("P436", MbKind::ReleaseGroup),
    }
}
pub(crate) fn property_url(template: &str, value: &str, storefront: &str) -> String {
    let value: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
    template.replace("{sf}", storefront).replace("{}", &value)
}

/// The music-service pages an item's best-rank IDs name, at any level.
pub fn service_links(item: &wikidata::Item, storefront: &str) -> Vec<Link> {
    let mut links = vec![];
    for (property, _, _, template) in PROPERTIES {
        for value in item.id(property).iter().take(4) {
            let url = property_url(template, value, storefront);
            if let Some(found) = music::parse(&url) {
                links.push(Link::new(found.canonical, found.service.name(), "wikidata"));
            }
        }
    }
    links
}

/// Whether an item is one recording: exactly one MusicBrainz recording ID and no work ID. A song
/// item is a work (its studio, live and remastered recordings share it), so a track takes
/// nothing from it, neither its Q-id nor its tracks.
pub(crate) fn recording_item(item: &wikidata::Item) -> bool {
    item.id("P4404").len() == 1 && item.id("P435").is_empty()
}
/// An item's keys and same-level links; at track level only a recording item's.
pub fn found(item: &wikidata::Item, level: Level, storefront: &str) -> Found {
    let mut found = Found::default();
    if level == Level::Track && !recording_item(item) {
        return found;
    }
    if let Some(key) = hubs::key::wikidata(&item.qid) {
        found.keys.push(Key {
            key,
            via: "wikidata".into(),
        });
    }
    // One MusicBrainz ID at this level is a key; several mean the item spans entities.
    let (property, kind) = mb_property(level);
    if let [mbid] = item.id(property) {
        if let Some(key) = hubs::key::mb(kind, mbid) {
            found.keys.push(Key {
                key,
                via: "wikidata".into(),
            });
        }
    }
    // Keyed by the item (one recording at track level), so a tagged link that is one joins.
    let named = hubs::key::wikidata(&item.qid);
    for (property, _, _, template) in PROPERTIES {
        for value in item.id(property).iter().take(4) {
            if let Some(mut link) = derived(
                &property_url(template, value, storefront),
                level,
                "wikidata",
            ) {
                link.key.clone_from(&named);
                found.links.push(link);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(std::fs::read(path).unwrap().trim_ascii()).unwrap()
    }

    #[test]
    fn hub_links_keep_the_level() {
        // A track takes tracks and videos of the recording, never an album.
        assert!(derived("https://www.deezer.com/track/1", Level::Track, "x").is_some());
        assert!(derived("https://youtu.be/dQw4w9WgXcQ", Level::Track, "x").is_some());
        assert!(derived("https://www.deezer.com/album/1", Level::Track, "x").is_none());
        // An album takes albums and YouTube's album playlists, never a Spotify playlist.
        assert!(derived(
            "https://www.youtube.com/playlist?list=OLAK5uy_nc6afxSnmK8OFML8dF9q_0D1MBMXsGnL4",
            Level::Album,
            "x"
        )
        .is_some());
        assert!(derived(
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            Level::Album,
            "x"
        )
        .is_none());
        // Not a music service: no link.
        assert!(derived(
            "https://www.allmusic.com/album/mw0000024289",
            Level::Album,
            "x"
        )
        .is_none());
        // Discogs at its level: a release group's master, a release's release, never across.
        let (release, master) = (
            "https://www.discogs.com/release/11993641",
            "https://www.discogs.com/master/21491",
        );
        assert!(derived(release, Level::Album, "x").is_some());
        assert!(derived(release, Level::Artist, "x").is_none());
        assert!(discogs_fits(release, MbKind::Release));
        assert!(!discogs_fits(release, MbKind::ReleaseGroup));
        assert!(discogs_fits(master, MbKind::ReleaseGroup));
        assert!(!discogs_fits(master, MbKind::Release));
        assert!(discogs_fits(
            "https://www.discogs.com/artist/3840",
            MbKind::Artist
        ));
        assert!(discogs_fits("https://tidal.com/album/1", MbKind::Release));
        let link = derived(
            "https://music.apple.com/gb/album/ok-computer/1097861387",
            Level::Album,
            "musicbrainz",
        )
        .unwrap();
        assert_eq!(
            (link.uri.as_str(), link.site.as_str()),
            ("https://music.apple.com/gb/album/1097861387", "Apple Music")
        );
    }

    #[test]
    fn a_recording_says_its_isrcs_artist_and_length() {
        let recording =
            musicbrainz::parse_entity(MbKind::Recording, &fixture("mb-recording.json")).unwrap();
        let facts: Vec<(String, String)> = facts(&recording)
            .into_iter()
            .map(|f| (f.name, f.value))
            .collect();
        let has = |n: &str, v: &str| facts.iter().any(|(a, b)| a == n && b == v);
        assert!(
            has("mbid", "9f9cf187-d6f9-437f-9d98-d59cdbd52757"),
            "{facts:?}"
        );
        assert!(
            has("isrc", "GBAYE9701376") && has("isrc", "GBBKS1700108"),
            "{facts:?}"
        );
        assert!(has("artist", "Radiohead"), "{facts:?}");
        assert!(has("duration", "384"), "{facts:?}");
        let release =
            musicbrainz::parse_entity(MbKind::Release, &fixture("mb-release.json")).unwrap();
        assert!(facts_of(&release).contains(&("upc".into(), "634904078164".into())));
    }

    fn facts_of(e: &musicbrainz::Entity) -> Vec<(String, String)> {
        facts(e).into_iter().map(|f| (f.name, f.value)).collect()
    }

    #[test]
    fn a_wikidata_item_gives_its_ids_at_the_level() {
        let items = wikidata::parse_entities(&fixture("wd-entities.json"));
        let item = items.iter().find(|i| i.qid == "Q202996").unwrap();
        let found = found(item, Level::Album, "us");
        let keys: Vec<&str> = found.keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "wikidata:Q202996",
                "mb:release-group:b1392450-e666-3926-a536-22c65f834433"
            ]
        );
        let uris: Vec<&str> = found.links.iter().map(|l| l.uri.as_str()).collect();
        assert!(
            uris.contains(&"https://open.spotify.com/album/2fGCAYUMssLKiUAoNdxGLx"),
            "{uris:?}"
        );
        assert!(found.links.iter().all(|l| l.via == "wikidata"));
        // The same item read as an artist gives no album links.
        assert!(super::found(item, Level::Artist, "us")
            .links
            .iter()
            .all(|l| { music::parse(&l.uri).is_some_and(|m| m.level == Level::Artist) }));
        // A track takes a recording item only: never a song (a work) or an item of unknown level.
        let item = |ids: &[(&str, &str)]| wikidata::Item {
            qid: "Q1".into(),
            ids: ids
                .iter()
                .map(|(p, v)| (p.to_string(), vec![v.to_string()]))
                .collect(),
            sitelinks: Default::default(),
            ..Default::default()
        };
        let spotify = ("P2207", "6LgJvl0Xdtc73RJ1mmpotq");
        let recording = ("P4404", "b1a9c0e9-d987-4042-ae91-78d6a3267d69");
        let work = ("P435", "ebd75d2a-0c3e-3b32-b4a5-d9c0eb0e5f8f");
        let keys = |ids: &[(&str, &str)]| super::found(&item(ids), Level::Track, "us").keys.len();
        assert_eq!(keys(&[spotify, recording]), 2);
        assert_eq!(keys(&[spotify, recording, work]), 0);
        assert_eq!(keys(&[spotify, work]), 0);
        assert_eq!(keys(&[spotify]), 0);
        assert!(super::found(&item(&[spotify, work]), Level::Track, "us")
            .links
            .is_empty());
        // Property values become valid URLs: the Apple storefront fills its place.
        assert_eq!(
            property_url("https://music.apple.com/{sf}/album/{}", "1097861387", "gb"),
            "https://music.apple.com/gb/album/1097861387"
        );
    }
    /// A performer's links name the artist only where a hub or MusicBrainz can: a MusicBrainz
    /// artist page outright; a music service's, Songkick's or Bandsintown's artist page as the
    /// URL MusicBrainz stores (no slug, no parameters); nothing else.
    #[test]
    fn performer_links_name_the_artist_as_musicbrainz_stores_them() {
        let lookup = |u: &str| Some(ArtistLink::Lookup(u.to_owned()));
        assert_eq!(
            artist_link("https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"),
            Some(ArtistLink::Mbid(
                "a74b1b7f-71a5-4011-9441-d0b5e4122711".into()
            ))
        );
        assert_eq!(
            artist_link("https://www.songkick.com/artists/253846-radiohead?utm_medium=organic&utm_source=microformat"),
            lookup("https://www.songkick.com/artists/253846")
        );
        assert_eq!(
            artist_link("https://www.bandsintown.com/a/25874-radiohead"),
            lookup("https://www.bandsintown.com/a/25874")
        );
        assert_eq!(
            artist_link("https://open.spotify.com/intl-de/artist/4Z8W4fKeB5YxbusRsdQVPb?si=x"),
            lookup("https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb")
        );
        for other in [
            "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69",
            "https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena",
            "https://www.songkick.com/artists/radiohead",
            "https://example.org/artists/253846",
        ] {
            assert_eq!(artist_link(other), None, "{other}");
        }
    }
}
