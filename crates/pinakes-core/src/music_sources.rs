//! "Also on" for music: the same track, album or artist on other services, from exact hub
//! lookups only (never a title or name).
//!
//! Who may vouch for a key: a hub (MusicBrainz, Wikidata) or a known music service's own record
//! of its own object. Any other page's claim is a fact at most.
//!
//! The sources themselves are in `also_on/`, one module each, and each has its page in
//! `docs/sources`: `service` (the ISRC a service's own track page states), `tidal` and `spotify`
//! (a service's own record, by its API), `musicbrainz`, `musicbrainz_entity` and
//! `musicbrainz_containment` (what MusicBrainz relates a link or its own page to), `wikidata`
//! (the one item carrying the link's service ID), `deezer`, `spotify_isrc`, `tidal_isrc` and
//! `itunes` (a service asked by the ISRC or barcode another source keyed), `itunes_song` and
//! `youtube_match` (matches by metadata, never keys), `editions` (which link of a service a card
//! shows) and `performers` (an event page's artists). This module holds what they share.
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
/// A recognized MusicBrainz page's identity level. Its verified keys may be used by reverse
/// provider lookups too; an arbitrary page's claimed type or ISRC is not enough.
pub(crate) fn mb_level(uri: &str) -> Option<Level> {
    if musicbrainz::isrc_link(uri).is_some() {
        return Some(Level::Track);
    }
    musicbrainz::entity_link(uri).map(|(kind, _, _)| match kind {
        MbKind::Recording => Level::Track,
        MbKind::Release | MbKind::ReleaseGroup => Level::Album,
        MbKind::Artist => Level::Artist,
    })
}

/// What a lookup on `own`'s catalogue by ISRC or barcode (`deezer`, `itunes`, `spotify_isrc`,
/// `tidal_isrc`) asks for `subject`: the level and the key namespace it is asked by. Another
/// service's track or video is asked by its `isrc:` keys, its album or YouTube album playlist by
/// its `upc:` keys, and so is a MusicBrainz recording's or release's page, by the keys its hub
/// gave. `None` for `own`'s links (each service reads its own object's record instead), for any
/// other link or page, and for a subject without such a key. One rule, so the four cover the same
/// cards.
pub(crate) fn reverse_subject(subject: &Subject, own: Service) -> Option<(Level, &'static str)> {
    let level = music::parse(&subject.uri)
        .filter(|l| l.service != own)
        .and_then(|l| join_level(&l))
        .or_else(|| mb_level(&subject.uri))?;
    let namespace = match level {
        Level::Track => "isrc",
        Level::Album => "upc",
        _ => return None,
    };
    subject
        .keys_in(namespace)
        .next()
        .is_some()
        .then_some((level, namespace))
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

/// An album or a track of a service that records albums' barcodes and tracks' ISRCs: how its
/// API names the collection, the namespace of the key and the value's normal form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Album,
    Track,
}
impl Kind {
    pub(crate) fn collection(self) -> &'static str {
        match self {
            Self::Album => "albums",
            Self::Track => "tracks",
        }
    }
    pub(crate) fn namespace(self) -> &'static str {
        match self {
            Self::Album => "upc",
            Self::Track => "isrc",
        }
    }
    pub(crate) fn normal(self, value: &str) -> Option<String> {
        match self {
            Self::Album => crate::barcode::gtin13(value),
            Self::Track => music::isrc(value),
        }
    }
}

/// The values of a subject's keys in `namespace` to ask a service for, at most `max`. With
/// `own_first`, the track's own ISRC comes first when it is one of them (a shared ISRC is a fact,
/// not a key, and a service would answer with any of its recordings); the others follow in their
/// order.
pub(crate) fn key_values(
    subject: &Subject,
    namespace: &str,
    own_first: bool,
    max: usize,
) -> Vec<String> {
    let own = own_isrc(subject)
        .filter(|o| own_first && subject.keys_in(namespace).any(|k| k == o.as_str()));
    own.iter()
        .map(String::as_str)
        .chain(
            subject
                .keys_in(namespace)
                .filter(|v| Some(*v) != own.as_deref()),
        )
        .take(max)
        .map(str::to_owned)
        .collect()
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
/// track's, in seconds; and how far apart two recordings' lengths may be for them to agree.
pub const LENGTH_WINDOW_SECS: u64 = 3;
/// At most this many recordings of one ISRC are read to see whether they agree: more is a
/// mess, not a duplicate.
pub const MAX_DUPLICATES: usize = 8;
/// What an ISRC a service states for its own track names on MusicBrainz.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IsrcMatch {
    /// MusicBrainz knows no recording with it: the ISRC is the track's own key.
    Unknown,
    /// Exactly one recording has it: the ISRC is a key, and names that recording.
    One(String),
    /// Several recordings have it and they agree ([`agree`]): one recording MusicBrainz has not
    /// merged yet. The ISRC and `standing`, the recording that stands for the `members`
    /// ([`standing`]), are keys, so their groups are one; the other MBIDs are facts.
    Duplicates {
        standing: String,
        members: Vec<String>,
    },
    /// Several recordings have it that disagree, and the service's duration picks one: `pick`,
    /// the recording whose length is within [`LENGTH_WINDOW_SECS`] of it while every other's is
    /// known and is not. `standing` is the MBID it is keyed by ([`recording_key`]: the pick, or
    /// the recording that stands for its duplicates); the ISRC stays a fact. The pick itself is
    /// what MusicBrainz is read about (its links, its MBID as `mbid_was`).
    Picked { pick: String, standing: String },
    /// Several recordings have it that disagree, and nothing tells them apart: the ISRC is a
    /// fact only, as a key would join whichever of their groups took it first.
    Shared,
}
/// Which recording `isrc`, a service's own record of its track, names: see [`IsrcMatch`].
/// `duration` is the track's length by the service, in seconds.
pub(crate) async fn isrc_match(
    cx: &Context,
    isrc: &str,
    duration: Option<i64>,
) -> Result<IsrcMatch> {
    let base = cx.config.also_on.musicbrainz_base();
    let recordings = musicbrainz::isrc(&cx.egress, base, isrc).await?;
    Ok(match recordings.as_deref() {
        None | Some([]) => IsrcMatch::Unknown,
        Some([one]) => IsrcMatch::One(one.mbid.clone()),
        Some(several) => {
            // Recordings that agree are one; the whole connected set decides who stands for
            // them. A set that cannot be settled is read as recordings that disagree.
            let settled = match agreeing(&cx.egress, base, several).await? {
                Some(entities) => match entities.as_slice() {
                    [one] => Some(IsrcMatch::One(one.mbid.clone())),
                    [first, ..] => match standing(&cx.egress, base, &first.mbid).await? {
                        Standing::Of { standing, members } => {
                            Some(IsrcMatch::Duplicates { standing, members })
                        }
                        Standing::Alone => Some(IsrcMatch::One(first.mbid.clone())),
                        Standing::Unclear => None,
                    },
                    [] => None,
                },
                None => None,
            };
            match settled {
                Some(settled) => settled,
                // The duration's pick is keyed as every producer keys a recording.
                None => match picked(several, duration) {
                    Some(pick) => IsrcMatch::Picked {
                        standing: recording_key(&cx.egress, base, &pick).await?,
                        pick,
                    },
                    None => IsrcMatch::Shared,
                },
            }
        }
    })
}
/// The recording an ISRC alone names, keyed as every producer keys a recording: the one
/// recording MusicBrainz gives it (through [`recording_key`]), or the one that stands for the
/// duplicates it is on; `None` when MusicBrainz does not know it, its recordings disagree, or
/// its lookup is over MusicBrainz's cap.
pub(crate) async fn isrc_recording(cx: &Context, isrc: &str) -> Result<Option<String>> {
    let base = cx.config.also_on.musicbrainz_base();
    // A lookup over MusicBrainz's cap names no recording: the ISRC gives no key.
    let Some(found) = secondary(isrc_match(cx, isrc, None).await)? else {
        return Ok(None);
    };
    Ok(match found {
        IsrcMatch::One(mbid) => Some(recording_key(&cx.egress, base, &mbid).await?),
        IsrcMatch::Duplicates { standing, .. } => Some(standing),
        IsrcMatch::Unknown | IsrcMatch::Picked { .. } | IsrcMatch::Shared => None,
    })
}
/// Whether an ISRC that `mbid`'s own record carries may key that recording: MusicBrainz's lookup
/// of the ISRC names this recording alone, or knows no recording with it (the ISRC is then its
/// own key, as [`isrc_key`] keys a service's track). One on recordings that disagree, or one
/// that names another recording, is a fact: as a key it would join whichever recording's group
/// took it first.
pub(crate) async fn isrc_is_own(cx: &Context, isrc: &str, mbid: &str) -> Result<bool> {
    let base = cx.config.also_on.musicbrainz_base();
    // A lookup over MusicBrainz's cap tells nothing: the ISRC is not known to be its own.
    let Some(found) = secondary(isrc_match(cx, isrc, None).await)? else {
        return Ok(false);
    };
    Ok(match found {
        IsrcMatch::Unknown => true,
        IsrcMatch::One(one) => recording_key(&cx.egress, base, &one).await? == mbid,
        IsrcMatch::Duplicates { standing, .. } => standing == mbid,
        IsrcMatch::Picked { .. } | IsrcMatch::Shared => false,
    })
}
/// The MBID every producer keys a recording by (`mb:recording:…`): the recording that stands for
/// its duplicates ([`standing`]), else its own. The one function behind every recording key: a
/// service's own ISRC ([`isrc_match`]: duplicates and a duration's pick), a URL relation and a
/// recording page (`entities_found`), the duplicates' union (`duplicates_found`), and Wikidata's
/// recording item (P4404). The one exception is a recording page whose read is over
/// MusicBrainz's cap: it keys its own MBID, as the link alone proves (`musicbrainz_entity`).
pub(crate) async fn recording_key(
    egress: &crate::resolver::egress::Egress,
    base: &str,
    mbid: &str,
) -> Result<String> {
    Ok(match standing(egress, base, mbid).await? {
        Standing::Of { standing, .. } => standing,
        Standing::Alone | Standing::Unclear => mbid.to_owned(),
    })
}
/// The recordings of one ISRC when they are one recording MusicBrainz has not merged
/// ([`agree`]), read from MusicBrainz (each recording's own record, the read the sources make
/// anyway), each once; `None` when they disagree, when one is unknown, or when there are more
/// than [`MAX_DUPLICATES`]. Only MusicBrainz's own data for the exact ISRC decides: no search.
pub(crate) async fn agreeing(
    egress: &crate::resolver::egress::Egress,
    base: &str,
    recordings: &[musicbrainz::IsrcRecording],
) -> Result<Option<Vec<musicbrainz::Entity>>> {
    if recordings.len() < 2 || recordings.len() > MAX_DUPLICATES {
        return Ok(None);
    }
    let mut entities: Vec<musicbrainz::Entity> = vec![];
    for recording in recordings {
        // A record over MusicBrainz's cap tells nothing: the recordings are not known to agree.
        let Some(Some(entity)) =
            secondary(musicbrainz::entity(egress, base, MbKind::Recording, &recording.mbid).await)?
        else {
            return Ok(None);
        };
        // Two MBIDs merged into one recording meanwhile answer as that one recording.
        if !entities.iter().any(|e| e.mbid == entity.mbid) {
            entities.push(entity);
        }
    }
    Ok(agree(&entities).then_some(entities))
}
/// Whether several recordings are one: the same artists (their MBIDs), the same title once
/// the tags that keep a recording's identity are dropped (`matching::recording_title`: a
/// remaster or a version tag goes, a live, remix or edit tag stays and tells them apart), the
/// same disambiguation (MusicBrainz keeps a live recording's title bare and says "live, …"
/// there), all audio or all video, and every two known lengths within [`LENGTH_WINDOW_SECS`]
/// (an unknown length says nothing). One recording agrees with itself; none without artists or
/// a title.
pub fn agree(entities: &[musicbrainz::Entity]) -> bool {
    let Some(first) = entities.first() else {
        return false;
    };
    let artists = |e: &musicbrainz::Entity| {
        let mut ids = e.artist_ids.clone();
        ids.sort();
        ids
    };
    let title = |e: &musicbrainz::Entity| {
        e.title
            .as_deref()
            .map(crate::matching::recording_title)
            .filter(|t| !t.is_empty())
    };
    let comment = |e: &musicbrainz::Entity| e.disambiguation.as_deref().map(str::to_lowercase);
    let (ids, name) = (artists(first), title(first));
    if ids.is_empty() || name.is_none() {
        return false;
    }
    entities.iter().all(|e| {
        artists(e) == ids
            && title(e) == name
            && comment(e) == comment(first)
            && e.video == first.video
    }) && entities.iter().all(|a| {
        entities.iter().all(|b| match (a.length_ms, b.length_ms) {
            (Some(x), Some(y)) => x.abs_diff(y) <= LENGTH_WINDOW_SECS * 1000,
            _ => true,
        })
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
/// Of duplicate recordings, the one that stands for them all: the smallest MBID, the same
/// whichever source reads them and in whatever order MusicBrainz lists them. A group holds one
/// recording MBID (`groups::single_object`), so the duplicates are keyed by this one; the others
/// are facts.
pub fn primary(mbids: &[String]) -> Option<&String> {
    mbids.iter().min()
}
/// The most ISRC lookups one [`standing`] makes before it gives up: duplicates share their
/// ISRCs, so a few reads settle a set; more is a mess, not a duplicate.
pub const MAX_DUPLICATE_LOOKUPS: usize = 16;
/// The most recording records one [`standing`] reads (the start's and those its ISRCs name)
/// before it gives up.
pub const MAX_DUPLICATE_READS: usize = 32;
/// What a recording is among its duplicates ([`standing`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Standing {
    /// No recording agrees with it on any of its ISRCs: it stands for itself.
    Alone,
    /// One of several recordings that agree: `standing` (the smallest MBID of `members`, which
    /// hold it) stands for them all.
    Of {
        standing: String,
        members: Vec<String>,
    },
    /// The set could not be settled within [`MAX_DUPLICATES`] recordings,
    /// [`MAX_DUPLICATE_LOOKUPS`] ISRC lookups and [`MAX_DUPLICATE_READS`] recording reads, a
    /// member could not be read, or the whole set does not agree (lengths drifting past the
    /// window along a chain): the recording stands for itself (it keeps its own MBID, an ISRC on
    /// several recordings is a fact), which keys no second recording.
    Unclear,
}
/// The recording that stands for `mbid` and its duplicates: the connected set of recordings that
/// agree ([`agree`]), reached through their ISRCs (every ISRC of every member is looked up, and
/// an ISRC's recordings join the set when all of them agree with it), closed under that walk,
/// its smallest MBID standing for all, when the whole set agrees ([`agree`] over every member,
/// not only along the walk). The same answer from any member, in whatever order MusicBrainz lists
/// them, so every source keys one recording for them. Bounded: over [`MAX_DUPLICATES`] members,
/// [`MAX_DUPLICATE_LOOKUPS`] ISRC lookups or [`MAX_DUPLICATE_READS`] recording reads, or a member
/// MusicBrainz cannot serve, the set is [`Standing::Unclear`]. Cached reads (each recording's own
/// record, each ISRC's recordings).
pub(crate) async fn standing(
    egress: &crate::resolver::egress::Egress,
    base: &str,
    mbid: &str,
) -> Result<Standing> {
    let Some(Some(entity)) =
        secondary(musicbrainz::entity(egress, base, MbKind::Recording, mbid).await)?
    else {
        return Ok(Standing::Unclear);
    };
    standing_of(egress, base, entity).await
}
/// A read a run makes besides the subject's own (another recording's record, an ISRC's
/// recordings, a recording's original album): over MusicBrainz's cap it settles nothing (`None`:
/// the duplicate walk is [`Standing::Unclear`] and each recording stands for itself, an ISRC that
/// cannot be checked gives no key, the album fact is absent), never an error that would empty the
/// source's run. Any other error is the read's own.
fn secondary<T>(read: Result<T>) -> Result<Option<T>> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(error) if crate::resolver::egress::over_cap(&error) => Ok(None),
        Err(error) => Err(error),
    }
}
/// [`standing`] from a recording's record already read.
pub(crate) async fn standing_of(
    egress: &crate::resolver::egress::Egress,
    base: &str,
    start: musicbrainz::Entity,
) -> Result<Standing> {
    let mut members: Vec<musicbrainz::Entity> = vec![];
    let mut queue: std::collections::VecDeque<musicbrainz::Entity> =
        std::collections::VecDeque::new();
    let mut seen_isrcs = std::collections::BTreeSet::new();
    let mut lookups = 0usize;
    // The start's record was read by the caller.
    let mut reads = 1usize;
    queue.push_back(start);
    while let Some(entity) = queue.pop_front() {
        // A merged MBID answers as the recording it was merged into.
        if members.iter().any(|m| m.mbid == entity.mbid) {
            continue;
        }
        members.push(entity.clone());
        if members.len() > MAX_DUPLICATES {
            return Ok(Standing::Unclear);
        }
        for isrc in &entity.isrcs {
            if !seen_isrcs.insert(isrc.clone()) {
                continue;
            }
            lookups += 1;
            if lookups > MAX_DUPLICATE_LOOKUPS {
                return Ok(Standing::Unclear);
            }
            let Some(recordings) = secondary(musicbrainz::isrc(egress, base, isrc).await)? else {
                return Ok(Standing::Unclear);
            };
            let Some(recordings) = recordings else {
                continue;
            };
            if recordings.len() < 2 {
                continue;
            }
            if recordings.len() > MAX_DUPLICATES {
                return Ok(Standing::Unclear);
            }
            // The ISRC's recordings as a set, with what is known so far: all of them agree, or
            // this ISRC tells nothing (a wrongly attached ISRC, an edit, a live recording).
            let mut set: Vec<musicbrainz::Entity> = vec![];
            for recording in &recordings {
                let known = members.iter().find(|m| m.mbid == recording.mbid).cloned();
                let read = match known {
                    Some(e) => Some(e),
                    None => {
                        reads += 1;
                        if reads > MAX_DUPLICATE_READS {
                            return Ok(Standing::Unclear);
                        }
                        let Some(read) = secondary(
                            musicbrainz::entity(egress, base, MbKind::Recording, &recording.mbid)
                                .await,
                        )?
                        else {
                            return Ok(Standing::Unclear);
                        };
                        read
                    }
                };
                let Some(e) = read else {
                    return Ok(Standing::Unclear);
                };
                if !set.iter().any(|s| s.mbid == e.mbid) {
                    set.push(e);
                }
            }
            if !set.iter().any(|s| s.mbid == entity.mbid) {
                continue;
            }
            if agree(&set) {
                for e in set {
                    if !members.iter().any(|m| m.mbid == e.mbid)
                        && !queue.iter().any(|q| q.mbid == e.mbid)
                    {
                        queue.push_back(e);
                    }
                }
            }
        }
    }
    // The set agrees as a whole: lengths that drift by 3 s at each step of a chain are not one
    // recording.
    if members.len() > 1 && !agree(&members) {
        return Ok(Standing::Unclear);
    }
    let mut mbids: Vec<String> = members.into_iter().map(|e| e.mbid).collect();
    mbids.sort();
    mbids.dedup();
    if mbids.len() < 2 {
        return Ok(Standing::Alone);
    }
    let Some(standing) = primary(&mbids).cloned() else {
        return Ok(Standing::Alone);
    };
    Ok(Standing::Of {
        standing,
        members: mbids,
    })
}
/// The keys a service's own ISRC gives its track (`via` the service): the ISRC itself; for
/// duplicates, the ISRC and the recording that stands for them ([`standing`]); for a shared
/// ISRC, the recording its duration picked (as [`recording_key`] keys it), or none when nothing
/// tells them apart.
pub(crate) fn isrc_key(isrc: &str, found: &IsrcMatch, via: &str) -> Vec<crate::also_on::Key> {
    let key = |key: String| crate::also_on::Key {
        key,
        via: via.into(),
    };
    match found {
        IsrcMatch::Unknown | IsrcMatch::One(_) => vec![key(format!("isrc:{isrc}"))],
        IsrcMatch::Picked { standing, .. } => hubs::key::mb(MbKind::Recording, standing)
            .map(key)
            .into_iter()
            .collect(),
        IsrcMatch::Duplicates { standing, .. } => std::iter::once(format!("isrc:{isrc}"))
            .chain(hubs::key::mb(MbKind::Recording, standing))
            .map(key)
            .collect(),
        IsrcMatch::Shared => vec![],
    }
}

/// What MusicBrainz says about duplicate recordings of one ISRC (`members`, which `standing`
/// stands for, [`standing`]), as one recording: the ISRC and the standing MBID as keys, the
/// other ISRCs they carry that name the same standing recording on their own ([`isrc_recording`];
/// up to [`MAX_ISRC_KEYS`]; any other is a fact, never a key, so no exact lookup by it can name
/// another recording's track), the union of their facts (the standing MBID
/// as `mbid`, the others as `mbid_was`, so `key=mbid:` finds the card by any of them; one
/// `duration`; the standing recording's original album), and the union of their current links on
/// music services at `level`, each keyed by the standing recording. Nothing when one is unknown.
pub(crate) async fn duplicates_found(
    cx: &Context,
    base: &str,
    standing: &str,
    members: &[String],
    isrc: &str,
    level: Level,
) -> Result<Found> {
    let mut entities: Vec<musicbrainz::Entity> = vec![];
    for id in std::iter::once(standing).chain(members.iter().map(String::as_str)) {
        let Some(entity) = musicbrainz::entity(&cx.egress, base, MbKind::Recording, id).await?
        else {
            return Ok(Found::default());
        };
        if !entities.iter().any(|e| e.mbid == entity.mbid) {
            entities.push(entity);
        }
    }
    let Some(first) = entities.first() else {
        return Ok(Found::default());
    };
    let key = |key: String| Key {
        key,
        via: "musicbrainz".into(),
    };
    let mut found = Found::default();
    found.keys.push(key(format!("isrc:{isrc}")));
    let named = hubs::key::mb(MbKind::Recording, standing);
    found.keys.extend(named.clone().map(key));
    let mut others: Vec<&str> = entities
        .iter()
        .flat_map(|e| e.isrcs.iter().map(String::as_str))
        .filter(|i| *i != isrc)
        .collect();
    others.sort_unstable();
    others.dedup();
    for other in others.into_iter().take(MAX_ISRC_KEYS) {
        if isrc_recording(cx, other).await?.as_deref() == Some(standing) {
            found.keys.push(key(format!("isrc:{other}")));
        }
    }
    found
        .facts
        .push(crate::also_on::Fact::new("mbid", standing));
    for entity in &entities {
        for fact in facts(entity) {
            let fact = match fact.name.as_str() {
                "mbid" if fact.value != standing => {
                    crate::also_on::Fact::new("mbid_was", &fact.value)
                }
                "mbid" => continue,
                // One length: the standing recording's (the first read), the rest within 3 s.
                "duration" if found.facts.iter().any(|f| f.name == "duration") => continue,
                _ => fact,
            };
            if !found.facts.contains(&fact) {
                found.facts.push(fact);
            }
        }
    }
    // Over MusicBrainz's cap, the original album is not known: no fact, the rest stands.
    if let Some(Some(album)) =
        secondary(musicbrainz::original_album(&cx.egress, base, &first.mbid).await)?
    {
        found
            .facts
            .push(crate::also_on::Fact::new("original_album", album));
    }
    for entity in &entities {
        for url in &entity.urls {
            let Some(mut link) = derived(&url.url, level, "musicbrainz") else {
                continue;
            };
            if found.links.iter().any(|l| l.uri == link.uri) {
                continue;
            }
            link.key.clone_from(&named);
            found.links.push(link);
        }
    }
    Ok(found)
}

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
/// A MusicBrainz page's card, from the entity MusicBrainz serves (`musicbrainz_entity` reads the
/// same answer): its title, its type (a recording, an album, an artist that is a person or a
/// group), the artist credit and a recording's length. An ISRC's page is its one recording's card
/// under the ISRC's own link, or the ISRC's when several recordings or none have it.
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
        // Its own attribute too (ISO 8601, as every page's `duration`): an enriched description
        // takes the place of this line, and the length stays a fact (`card_facts`).
        doc.attributes
            .insert("duration".into(), format!("PT{secs}S"));
        about.push(if secs >= 3600 {
            format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
        } else {
            format!("{}:{:02}", secs / 60, secs % 60)
        });
    }
    doc.description = (!about.is_empty()).then(|| about.join(" · "));
    // Only a whole credit: the document keeps 256 bytes of an attribute, and a cut one would be a
    // second, wrong `artist` fact beside the `musicbrainz_entity` source's full credit.
    if let Some(artist) = entity
        .artist
        .as_ref()
        .filter(|a| a.len() <= ARTIST_ATTRIBUTE_MAX)
    {
        doc.attributes.insert("artist".into(), artist.clone());
    }
    for (name, value) in about_facts(entity) {
        doc.attributes.insert(name.into(), value);
    }
    doc
}

/// The facts a MusicBrainz card takes from what MusicBrainz says about the entity besides its
/// title, artist credit and length ([`about_facts`]); each is also an attribute the web route
/// keeps (`resolver::web::ATTRIBUTES`).
pub(crate) const ABOUT_FACTS: &[&str] = &[
    "disambiguation",
    "artist_type",
    "country_code",
    "area",
    "begin_date",
    "end_date",
    "release_type",
    "first_release_date",
    "genre",
    "rating",
    "rating_count",
];
/// At most this many genres in the `genre` fact.
const GENRES: usize = 3;
/// What MusicBrainz says about `entity` as facts, each value whole and at most
/// [`ARTIST_ATTRIBUTE_MAX`] bytes (a longer one is left out, never cut): its disambiguation;
/// an artist's type (`artist_type`, as the `musicbrainz_entity` source names it), country
/// (`country_code`, ISO 3166-1), area and life span (`begin_date`, `end_date`); a release
/// group's primary type (`release_type`); a release group's or recording's
/// `first_release_date` (dates as MusicBrainz gives them: `YYYY`, `YYYY-MM` or `YYYY-MM-DD`);
/// its top genres (`genre`: at most three, most votes first, a tie by name, joined with "; ");
/// and a release group's, recording's or artist's rating on MusicBrainz (`rating`, 0 to 5 with
/// one decimal, and `rating_count`, the number of votes; neither when nobody voted).
pub(crate) fn about_facts(entity: &musicbrainz::Entity) -> Vec<(&'static str, String)> {
    let about = &entity.about;
    let mut genres: Vec<&(String, u64)> = about.genres.iter().filter(|(_, n)| *n >= 1).collect();
    genres.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut genre = String::new();
    for (name, _) in genres.into_iter().take(GENRES) {
        let next = if genre.is_empty() {
            name.clone()
        } else {
            format!("{genre}; {name}")
        };
        if next.len() > ARTIST_ATTRIBUTE_MAX {
            break;
        }
        genre = next;
    }
    let artist_type = (entity.kind == MbKind::Artist)
        .then(|| entity.artist_type.clone())
        .flatten();
    [
        ("disambiguation", about.disambiguation.clone()),
        ("artist_type", artist_type),
        ("country_code", about.country.clone()),
        ("area", about.area.clone()),
        ("begin_date", about.begin.clone()),
        ("end_date", about.end.clone()),
        ("release_type", about.release_type.clone()),
        ("first_release_date", about.first_release_date.clone()),
        ("genre", Some(genre)),
        ("rating", about.rating.map(|r| rating_text(r.value))),
        ("rating_count", about.rating.map(|r| r.votes.to_string())),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        value
            .filter(|v| !v.is_empty() && v.len() <= ARTIST_ATTRIBUTE_MAX)
            .map(|v| (name, v))
    })
    .collect()
}

/// A rating of 0 to 5 with one decimal, rounded half away from zero (`4.45` is `4.5`).
fn rating_text(value: f64) -> String {
    let tenths = (value * 10.0).round() as u64;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// The longest artist credit (or other fact) a MusicBrainz card stores as an attribute: what
/// the document keeps of an attribute (`ResourceMeta::bounded`).
const ARTIST_ATTRIBUTE_MAX: usize = 256;

/// The facts a MusicBrainz page's own card states (`musicbrainz_card`): its artist credit and
/// its length (`duration`, in seconds, as the `musicbrainz_entity` source gives it), so a
/// resource read keeps them when an enriched description takes the place of MusicBrainz's
/// "artist · length" line, also before (or without) any also-on source reading the page; and
/// what MusicBrainz says about the entity ([`about_facts`]). Enrichment plays no part: these are
/// MusicBrainz's own.
pub(crate) fn card_facts(
    doc: &crate::model::ResourceMeta,
) -> Option<std::collections::BTreeMap<String, String>> {
    if doc.source != "musicbrainz" {
        return None;
    }
    let mut facts = std::collections::BTreeMap::new();
    for name in ["artist", "duration"].iter().chain(ABOUT_FACTS) {
        let Some(value) = doc.attributes.get(*name).filter(|v| !v.trim().is_empty()) else {
            continue;
        };
        let value = match *name {
            "duration" => match duration_secs(value) {
                Some(secs) => secs.to_string(),
                None => continue,
            },
            _ => value.clone(),
        };
        facts.insert((*name).to_owned(), value);
    }
    (!facts.is_empty()).then_some(facts)
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
/// is unknown, or when several releases are of different release groups. An ISRC is a key only
/// when it is this recording's alone ([`isrc_is_own`]; for a duplicate, when it names the one
/// that stands for it, [`isrc_recording`]). `shared` are ISRCs already known to be on several
/// recordings (the one a track's length picked among them): they stay facts without a lookup.
pub(crate) async fn entities_found(
    cx: &Context,
    base: &str,
    kind: MbKind,
    ids: &[String],
    level: Level,
    edition: bool,
    shared: &[String],
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
    // A recording that is one of several agreeing duplicates is keyed by the one that stands
    // for them (`standing`, as `recording_key` keys every recording); its own MBID becomes
    // `mbid_was`. An unsettled set keeps its own.
    let duplicates = match (kind, entities.len()) {
        (MbKind::Recording, 1) => match standing_of(&cx.egress, base, entity.clone()).await? {
            Standing::Of { standing, .. } => Some(standing),
            Standing::Alone | Standing::Unclear => None,
        },
        _ => None,
    };
    let stands = duplicates.clone().filter(|s| *s != entity.mbid);
    let own = stands.as_deref().unwrap_or(&entity.mbid);
    // A recording's ISRCs are keys only when each names this recording on its own (for a
    // duplicate, the one that stands for it): one it shares with a recording that disagrees
    // stays a fact, whether the recording stands alone, in a set, or in one that cannot be
    // settled.
    let mut isrcs: Vec<&String> = vec![];
    for isrc in entity
        .isrcs
        .iter()
        .filter(|i| !shared.contains(i))
        .take(MAX_ISRC_KEYS)
    {
        let keyed = match &duplicates {
            Some(standing) => isrc_recording(cx, isrc).await?.as_ref() == Some(standing),
            None => isrc_is_own(cx, isrc, &entity.mbid).await?,
        };
        if keyed {
            isrcs.push(isrc);
        }
    }
    // Several releases: the release group's key alone, so editions stay apart.
    if entities.len() == 1 {
        key(kind, own);
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
    for isrc in isrcs {
        found.keys.push(Key {
            key: format!("isrc:{isrc}"),
            via: "musicbrainz".into(),
        });
    }
    // What every entity has (one entity: all of it).
    found.facts = facts(entity);
    if let Some(standing) = &stands {
        for fact in &mut found.facts {
            if fact.name == "mbid" {
                fact.name = "mbid_was".into();
            }
        }
        found
            .facts
            .insert(0, crate::also_on::Fact::new("mbid", standing));
    }
    // A recording's original album ranks the card's links of one service (the track there).
    if kind == MbKind::Recording {
        // Over MusicBrainz's cap, the original album is not known: no fact, the rest stands.
        if let Some(Some(album)) =
            secondary(musicbrainz::original_album(&cx.egress, base, &entity.mbid).await)?
        {
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
        hubs::key::mb(kind, own)
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
/// a URL (an Apple Music link has no storefront).
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
        "https://music.apple.com/song/{}",
    ),
    (
        "P2281",
        Service::AppleMusic,
        Level::Album,
        "https://music.apple.com/album/{}",
    ),
    (
        "P2850",
        Service::AppleMusic,
        Level::Artist,
        "https://music.apple.com/artist/{}",
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
pub(crate) fn property_url(template: &str, value: &str) -> String {
    let value: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
    template.replace("{}", &value)
}

/// The music-service pages an item's best-rank IDs name, at any level.
pub fn service_links(item: &wikidata::Item) -> Vec<Link> {
    let mut links = vec![];
    for (property, _, _, template) in PROPERTIES {
        for value in item.id(property).iter().take(4) {
            let url = property_url(template, value);
            if let Some(found) = music::parse(&url) {
                links.push(Link::new(found.canonical, found.service.name(), "wikidata"));
            }
        }
    }
    links
}

/// Whether an item is one recording: a recording's item by the one classifier
/// (`music_enrichment::ItemKind::of`: a recording ID, P4404, and no work's, release's, release
/// group's or artist's ID, of no song's class) that names exactly one recording. A song item is
/// a work (its studio, live and remastered recordings share it), so a track takes nothing from
/// it, neither its Q-id nor its tracks.
pub(crate) fn recording_item(item: &wikidata::Item) -> bool {
    use crate::music_enrichment::ItemKind;
    ItemKind::of(item) == ItemKind::Recording && item.id("P4404").len() == 1
}
/// Whether an item is one album: a release group's item by the same classifier (one that is its
/// single's song's too counts). Such an item is never a recording's ([`recording_item`]), so no
/// track is keyed by it. An item with no MusicBrainz or Discogs album ID is of unknown level and
/// keys nothing.
pub(crate) fn album_item(item: &wikidata::Item) -> bool {
    use crate::music_enrichment::ItemKind;
    matches!(
        ItemKind::of(item),
        ItemKind::ReleaseGroup | ItemKind::WorkAndGroup
    )
}
/// Whether an item is one edition of an album: a release's item by the same classifier, or one
/// with no MusicBrainz identifier yet; never an album's, a recording's, a work's or an artist's.
/// The item a Discogs release's ID (P2206) names is taken only when it is one.
pub(crate) fn edition_item(item: &wikidata::Item) -> bool {
    use crate::music_enrichment::ItemKind;
    matches!(ItemKind::of(item), ItemKind::Release | ItemKind::Bare)
}
/// The music-service pages an item's best-rank IDs name at `level`, unkeyed.
pub(crate) fn item_links(item: &wikidata::Item, level: Level) -> Vec<Link> {
    let mut links = vec![];
    for (property, _, _, template) in PROPERTIES {
        for value in item.id(property).iter().take(4) {
            links.extend(derived(&property_url(template, value), level, "wikidata"));
        }
    }
    links
}
/// An item's keys and same-level links: at track level only a recording item's
/// ([`recording_item`]), at album level only an album item's ([`album_item`]).
pub fn found(item: &wikidata::Item, level: Level) -> Found {
    let of_level = match level {
        Level::Track => recording_item(item),
        Level::Album => album_item(item),
        _ => true,
    };
    if of_level {
        item_found(item, level)
    } else {
        Found::default()
    }
}
/// An item's keys and same-level links, for an item the caller knows to be of `level`: its Q-id,
/// its one MusicBrainz ID at the level, and its service pages at the level, each keyed by the
/// Q-id so a tagged link that is one joins.
pub(crate) fn item_found(item: &wikidata::Item, level: Level) -> Found {
    let mut found = Found::default();
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
    let named = hubs::key::wikidata(&item.qid);
    found.links = item_links(item, level);
    for link in &mut found.links {
        link.key.clone_from(&named);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read a run makes besides the subject's own, over MusicBrainz's cap, leaves out only its
    /// own part: a duplicate set's other ISRC that cannot be checked gives no key and its
    /// original album no fact, while the set's ISRC, its standing MBID and its facts stand; an
    /// ISRC whose lookup is over the cap names no recording.
    #[tokio::test(start_paused = true)]
    async fn a_secondary_read_over_the_cap_leaves_out_its_own_part() {
        let base = "https://musicbrainz.org/ws/2";
        let (standing, member) = (
            "00000000-0000-4000-8000-000000000081",
            "00000000-0000-4000-8000-000000000082",
        );
        let record = |id: &str, isrcs: &[&str]| {
            serde_json::to_vec(&serde_json::json!({
                "id": id, "title": "Capped", "length": 200000, "isrcs": isrcs, "video": false,
                "artist-credit": [{"name": "Someone", "joinphrase": "",
                    "artist": {"id": "00000000-0000-4000-8000-0000000000a1", "name": "Someone"}}],
                "relations": [],
            }))
            .unwrap()
        };
        let entity = |id: &str| hubs::musicbrainz::entity_lookup(base, MbKind::Recording, id);
        let (standing_url, member_url) = (entity(standing), entity(member));
        let album = format!("{base}/release?recording={standing}&type=album&status=official&inc=release-groups&limit=100&fmt=json");
        let other = format!("{base}/isrc/ZZCAP2600082?fmt=json");
        let (egress, recorded) = crate::sources::fixture::egress(vec![
            (
                &standing_url,
                200,
                "application/json",
                record(standing, &["ZZCAP2600081"]),
            ),
            (
                &member_url,
                200,
                "application/json",
                record(member, &["ZZCAP2600081", "ZZCAP2600082"]),
            ),
            (&album, 200, "!too-large", vec![]),
            (&other, 200, "!too-large", vec![]),
        ]);
        let cx = Context {
            egress,
            config: Default::default(),
        };
        assert_eq!(isrc_recording(&cx, "ZZCAP2600082").await.unwrap(), None);
        let found = duplicates_found(
            &cx,
            base,
            standing,
            &[member.to_owned()],
            "ZZCAP2600081",
            Level::Track,
        )
        .await
        .unwrap();
        let keys: Vec<&str> = found.keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(
            keys,
            ["isrc:ZZCAP2600081", &format!("mb:recording:{standing}")]
        );
        assert!(found
            .facts
            .iter()
            .any(|f| f.name == "mbid" && f.value == standing));
        assert!(!found.facts.iter().any(|f| f.name == "original_album"));
        let asked = recorded.asked();
        assert!(
            asked.contains(&album) && asked.contains(&other),
            "{asked:?}"
        );
    }

    /// The four lookups by ISRC or barcode cover the same cards: another service's track or
    /// video by its ISRC, its album or YouTube album playlist by its barcode, a MusicBrainz
    /// recording or release page by its hub's keys; never the service's own links, an artist,
    /// another playlist, or a card without the key.
    #[test]
    fn the_lookups_by_isrc_and_barcode_cover_the_same_cards() {
        let subject = |uri: &str, keys: &[&str]| Subject {
            id: String::new(),
            uri: uri.into(),
            status: "ok".into(),
            type_name: None,
            doc: None,
            keys: keys.iter().map(|k| ((*k).into(), "test".into())).collect(),
            first_seen_at: None,
            candidates: vec![],
        };
        let isrc = ["isrc:GBUM71029604"];
        let upc = ["upc:0602517416987"];
        let both = ["isrc:GBUM71029604", "upc:0602517416987"];
        for (uri, keys, asked) in [
            (
                "https://www.youtube.com/watch?v=bSnlKl_PoQU",
                &isrc[..],
                Some((Level::Track, "isrc")),
            ),
            (
                "https://www.deezer.com/track/1171872",
                &isrc[..],
                Some((Level::Track, "isrc")),
            ),
            (
                "https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa",
                &upc[..],
                Some((Level::Album, "upc")),
            ),
            (
                "https://www.youtube.com/playlist?list=OLAK5uy_kHcT8rC7yXBgYsSvrqrCN8i4KmDrhHl8s",
                &upc[..],
                Some((Level::Album, "upc")),
            ),
            (
                "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69",
                &isrc[..],
                Some((Level::Track, "isrc")),
            ),
            (
                "https://musicbrainz.org/release/30702389-5c67-4438-9ea0-2351c8de0f1d",
                &upc[..],
                Some((Level::Album, "upc")),
            ),
            // Without the key of its level: nothing to ask.
            (
                "https://www.youtube.com/watch?v=bSnlKl_PoQU",
                &upc[..],
                None,
            ),
            (
                "https://musicbrainz.org/release/30702389-5c67-4438-9ea0-2351c8de0f1d",
                &isrc[..],
                None,
            ),
            // No track or album.
            ("https://www.deezer.com/artist/27", &both[..], None),
            (
                "https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711",
                &both[..],
                None,
            ),
            (
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
                &both[..],
                None,
            ),
            ("https://example.org/track", &both[..], None),
        ] {
            // Each service but the link's own (below).
            let on = music::parse(uri).map(|l| l.service);
            for own in [
                Service::Deezer,
                Service::AppleMusic,
                Service::Spotify,
                Service::Tidal,
            ]
            .into_iter()
            .filter(|s| Some(*s) != on)
            {
                assert_eq!(
                    reverse_subject(&subject(uri, keys), own),
                    asked,
                    "{uri} on {own:?}"
                );
            }
        }
        // A service's own links are never asked by value.
        for (uri, own) in [
            ("https://www.deezer.com/track/1171872", Service::Deezer),
            (
                "https://music.apple.com/album/1097861387",
                Service::AppleMusic,
            ),
            (
                "https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa",
                Service::Spotify,
            ),
            ("https://tidal.com/album/3524504", Service::Tidal),
        ] {
            assert_eq!(reverse_subject(&subject(uri, &both), own), None, "{uri}");
        }
    }
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
            ("https://music.apple.com/album/1097861387", "Apple Music")
        );
    }

    /// A MusicBrainz card's own facts are the `musicbrainz_entity` source's artist and
    /// duration, same names and units, so a resource read shows each value once; another
    /// source's document gives none.
    #[test]
    fn a_musicbrainz_card_states_its_artist_and_length() {
        let recording =
            musicbrainz::parse_entity(MbKind::Recording, &fixture("mb-recording.json")).unwrap();
        let card = musicbrainz_card(
            "https://musicbrainz.org/recording/9f9cf187-d6f9-437f-9d98-d59cdbd52757",
            Some(&recording),
            None,
            86400,
        );
        let mut own = card_facts(&card).unwrap();
        // Besides what MusicBrainz says about the recording (`about_facts`).
        own.retain(|name, _| !ABOUT_FACTS.contains(&name.as_str()));
        let source: std::collections::BTreeMap<String, String> = facts(&recording)
            .into_iter()
            .filter(|f| f.name == "artist" || f.name == "duration")
            .map(|f| (f.name, f.value))
            .collect();
        assert_eq!(own, source);
        assert_eq!(own.len(), 2, "{own:?}");
        // The same-recording pass reads the card's own length when no source has read it.
        let unit = crate::same_recording::Unit::new(
            card.name.as_deref().unwrap(),
            card.attributes.get("artist").map(String::as_str),
            None,
            card.attributes.get("duration").map(String::as_str),
        );
        assert_eq!(unit.length_secs, own["duration"].parse::<i64>().ok());
        let mut page = card.clone();
        page.source = "jsonld".into();
        assert_eq!(card_facts(&page), None);
        // A credit longer than the document keeps is no attribute (and no fact) at all.
        let mut long = recording.clone();
        long.artist = Some("Artist feat. ".repeat(30));
        let card = musicbrainz_card(
            "https://musicbrainz.org/recording/9f9cf187-d6f9-437f-9d98-d59cdbd52757",
            Some(&long),
            None,
            86400,
        )
        .bounded(crate::resolver::web::ATTRIBUTES)
        .unwrap();
        assert_eq!(card.attributes.get("artist"), None);
        assert_eq!(
            card_facts(&card).unwrap().keys().collect::<Vec<_>>(),
            ["duration", "first_release_date"]
        );
    }

    fn enrichment_fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/shared/music_enrichment/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
    /// What MusicBrainz says about an artist, an album and a recording, as the card's facts:
    /// raw codes, dates as MusicBrainz writes them, the top three genres (a tie by name), the
    /// rating when people voted (Sefa: nobody did).
    #[test]
    fn a_musicbrainz_card_states_what_musicbrainz_says_about_it() {
        let facts = |kind: MbKind, file: &str| {
            let entity = musicbrainz::parse_entity(kind, &enrichment_fixture(file)).unwrap();
            let uri = format!("https://musicbrainz.org/{}/{}", kind.as_str(), entity.mbid);
            let card = musicbrainz_card(&uri, Some(&entity), None, 86400)
                .bounded(crate::resolver::web::ATTRIBUTES)
                .unwrap();
            let own: Vec<String> = about_facts(&entity).iter().map(|f| f.0.into()).collect();
            assert!(
                own.iter().all(|n| ABOUT_FACTS.contains(&n.as_str())),
                "{own:?}"
            );
            card_facts(&card).unwrap()
        };
        let map = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_eq!(
            facts(MbKind::Artist, "mb-artist-sefa.json"),
            map(&[
                ("disambiguation", "frenchcore artist"),
                ("artist_type", "Person"),
                ("country_code", "NL"),
                ("area", "Netherlands"),
                ("genre", "frenchcore; uptempo hardcore"),
            ])
        );
        assert_eq!(
            facts(MbKind::Artist, "mb-artist-daft-punk.json"),
            map(&[
                ("disambiguation", "French electronic duo"),
                ("artist_type", "Group"),
                ("country_code", "FR"),
                ("area", "France"),
                ("begin_date", "1993"),
                ("end_date", "2021-02-22"),
                ("genre", "electronic; house; french house"),
            ])
        );
        assert_eq!(
            facts(MbKind::ReleaseGroup, "mb-release-group-kind-of-blue.json"),
            map(&[
                ("artist", "Miles Davis"),
                ("release_type", "Album"),
                ("first_release_date", "1959-08-17"),
                ("genre", "jazz; cool jazz; modal jazz"),
                ("rating", "4.5"),
                ("rating_count", "43"),
            ])
        );
        let song = facts(MbKind::Recording, "mb-recording-get-lucky.json");
        assert_eq!(song["first_release_date"], "2013-05-17");
        assert_eq!(song["genre"], "disco; funk; electronic", "a tie by name");
        assert_eq!(song.get("disambiguation"), None, "MusicBrainz's is empty");
        assert_eq!((&*song["rating"], &*song["rating_count"]), ("3.9", "25"));
        // Every fact is an attribute the web route keeps.
        for name in ABOUT_FACTS {
            assert!(crate::resolver::web::ATTRIBUTES.contains(name), "{name}");
        }
    }
    /// A rating is a fact with one decimal, rounded half away from zero, beside its count.
    #[test]
    fn a_rating_has_one_decimal() {
        for (value, text) in [
            (4.45, "4.5"),
            (4.75, "4.8"),
            (3.9, "3.9"),
            (5.0, "5.0"),
            (0.0, "0.0"),
            (2.04, "2.0"),
        ] {
            assert_eq!(rating_text(value), text, "{value}");
        }
    }
    /// Values out of form are left out; a value over the bound is left out whole, and the genre
    /// list stops before the bound; a genre without a vote is none.
    #[test]
    fn musicbrainz_facts_are_bounded_and_checked() {
        let v = serde_json::json!({
            "id": "b5416a54-4941-4fcf-b2a0-7f98da6f9344",
            "name": "X",
            "type": "Person",
            "country": "nl",
            "disambiguation": "d".repeat(300),
            "area": {"name": "  "},
            "life-span": {"begin": "1990-1", "end": "2001-02-03"},
            "genres": [
                {"name": "a".repeat(200), "count": 9},
                {"name": "b".repeat(100), "count": 8},
                {"name": "c", "count": 0},
            ],
        });
        let entity = musicbrainz::parse_entity(MbKind::Artist, &v).unwrap();
        let facts: std::collections::BTreeMap<&str, String> =
            about_facts(&entity).into_iter().collect();
        assert_eq!(
            facts.keys().copied().collect::<Vec<_>>(),
            ["artist_type", "end_date", "genre"]
        );
        assert_eq!(facts["genre"], "a".repeat(200));
        // The same fields on a release group are not an artist's.
        let group = musicbrainz::parse_entity(MbKind::ReleaseGroup, &v).unwrap();
        assert!(about_facts(&group)
            .iter()
            .all(|(n, _)| !matches!(*n, "artist_type" | "country_code" | "end_date")));
    }

    /// A genre MusicBrainz lists twice counts once, with its higher count.
    #[test]
    fn a_genre_listed_twice_counts_once() {
        let v = serde_json::json!({
            "id": "b5416a54-4941-4fcf-b2a0-7f98da6f9344",
            "genres": [
                {"name": "b", "count": 2},
                {"name": "c", "count": 3},
                {"name": "b", "count": 5},
                {"name": "a", "count": 1},
            ],
        });
        let entity = musicbrainz::parse_entity(MbKind::Artist, &v).unwrap();
        let facts: std::collections::BTreeMap<&str, String> =
            about_facts(&entity).into_iter().collect();
        assert_eq!(facts["genre"], "b; c; a");
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
        let found = found(item, Level::Album);
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
        assert!(super::found(item, Level::Artist)
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
        let keys = |ids: &[(&str, &str)]| super::found(&item(ids), Level::Track).keys.len();
        assert_eq!(keys(&[spotify, recording]), 2);
        assert_eq!(keys(&[spotify, recording, work]), 0);
        assert_eq!(keys(&[spotify, work]), 0);
        assert_eq!(keys(&[spotify]), 0);
        assert!(super::found(&item(&[spotify, work]), Level::Track)
            .links
            .is_empty());
        // The one classifier decides (`ItemKind::of`): a song's item (its class) that carries a
        // recording's ID is a work, and a track takes nothing from it.
        let mut song = item(&[spotify, recording]);
        song.instance_of = vec!["Q7366".into()];
        assert_eq!(super::found(&song, Level::Track), Found::default());
        // Two recording IDs: the item spans recordings, and names none.
        let mut two = item(&[spotify]);
        two.ids.insert(
            "P4404".into(),
            vec![
                recording.1.into(),
                "8f3471b5-7e6a-48da-86a9-c1c07a0f47ae".into(),
            ],
        );
        assert_eq!(super::found(&two, Level::Track), Found::default());
        // An album takes an album's item only. A single's item that names its track's recording
        // too is the album's and never the track's: no Q-id keys both.
        let album = ("P2205", "6dVIqQ8qmQ5GBnJ9shOYGE");
        let group = ("P436", "b1392450-e666-3926-a536-22c65f834433");
        let at_album = |ids: &[(&str, &str)]| super::found(&item(ids), Level::Album);
        let keys_of =
            |found: &Found| -> Vec<String> { found.keys.iter().map(|k| k.key.clone()).collect() };
        assert_eq!(
            keys_of(&at_album(&[album, group])),
            [
                "wikidata:Q1",
                "mb:release-group:b1392450-e666-3926-a536-22c65f834433"
            ]
        );
        assert!(at_album(&[album, group])
            .links
            .iter()
            .all(|l| l.key.as_deref() == Some("wikidata:Q1")));
        let single_and_track = [album, group, spotify, recording];
        assert_eq!(keys_of(&at_album(&single_and_track)).len(), 2);
        assert_eq!(
            super::found(&item(&single_and_track), Level::Track),
            Found::default()
        );
        // An item of unknown level (no MusicBrainz or Discogs album ID) keys nothing either.
        assert_eq!(at_album(&[album]), Found::default());
        assert_eq!(at_album(&[album, work]), Found::default());
        // An edition's item (a release's, or one with no MusicBrainz ID yet) is what a Discogs
        // release takes; an album's, a recording's or a work's is not one.
        assert!(edition_item(&item(&[album])));
        assert!(edition_item(&item(&[album, ("P5813", group.1)])));
        assert!(!edition_item(&item(&[album, group])));
        assert!(!edition_item(&item(&[album, recording])));
        assert!(!edition_item(&item(&[album, work])));
        assert_eq!(
            keys_of(&item_found(&item(&[album]), Level::Album)),
            ["wikidata:Q1"]
        );
        // A single's song's item that is its release group's too is the album's.
        let mut single = item(&[album, group]);
        single.instance_of = vec!["Q134556".into()];
        assert_eq!(keys_of(&super::found(&single, Level::Album)).len(), 2);
        // Two release group IDs: the item's own key alone, no MusicBrainz key.
        let mut groups = item(&[album]);
        groups.ids.insert(
            "P436".into(),
            vec![
                group.1.into(),
                "00000000-0000-4000-8000-000000000001".into(),
            ],
        );
        assert_eq!(
            keys_of(&super::found(&groups, Level::Album)),
            ["wikidata:Q1"]
        );
        // Property values become valid URLs.
        assert_eq!(
            property_url("https://music.apple.com/album/{}", "1097861387"),
            "https://music.apple.com/album/1097861387"
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

    /// Duplicate recordings agree on their artists' MBIDs, their title without identity-keeping
    /// tags, and lengths within the window; a live, remix or edit tag, another artist, a length
    /// 4 s off, no artists or no title tell them apart. The smallest MBID stands for them.
    #[test]
    fn duplicates_agree_on_artists_title_and_length() {
        let entity =
            |mbid: &str, title: &str, artist: &str, length: Option<u64>| musicbrainz::Entity {
                kind: MbKind::Recording,
                mbid: mbid.into(),
                title: Some(title.into()),
                urls: vec![],
                isrcs: vec!["USUM70740306".into()],
                barcode: None,
                release_group: None,
                artist: Some("Sum 41".into()),
                artist_ids: if artist.is_empty() {
                    vec![]
                } else {
                    vec![artist.into()]
                },
                disambiguation: None,
                video: false,
                length_ms: length,
                artist_type: None,
                about: Default::default(),
            };
        let a = "f2eef649-0000-4000-8000-000000000001";
        let b = "f2eef649-0000-4000-8000-000000000002";
        let one = entity(
            "4da0e503-0cef-4dd7-a5b7-b08bea3db145",
            "Walking Disaster",
            a,
            Some(286_507),
        );
        let same = entity(
            "6a5c430a-8035-4217-97ac-ffa395f55bf9",
            "Walking Disaster",
            a,
            Some(286_373),
        );
        assert!(agree(&[one.clone(), same.clone()]));
        // Identity-keeping tags go; an unknown length says nothing.
        assert!(agree(&[
            one.clone(),
            entity(b, "Walking Disaster - 2007 Remaster", a, None)
        ]));
        assert!(agree(&[
            one.clone(),
            entity(b, "Walking Disaster (Album Version)", a, Some(288_000))
        ]));
        // A tag that names another recording, another artist, a length off the window.
        assert!(!agree(&[
            one.clone(),
            entity(b, "Walking Disaster (Live)", a, Some(286_400))
        ]));
        assert!(!agree(&[
            one.clone(),
            entity(b, "Walking Disaster (Radio Edit)", a, Some(286_400))
        ]));
        assert!(!agree(&[
            one.clone(),
            entity(b, "Walking Disaster", b, Some(286_400))
        ]));
        assert!(!agree(&[
            one.clone(),
            entity(b, "Walking Disaster", a, Some(290_600))
        ]));
        // Three recordings: every pair within the window, not only neighbours.
        assert!(!agree(&[
            one.clone(),
            entity(b, "Walking Disaster", a, Some(288_500)),
            entity(
                "f2eef649-0000-4000-8000-000000000003",
                "Walking Disaster",
                a,
                Some(291_000)
            ),
        ]));
        // A live recording keeps its bare title and says so in its disambiguation; a video is
        // not the audio recording.
        let live = musicbrainz::Entity {
            disambiguation: Some("live, 2008-06-01: Wembley".into()),
            ..entity(b, "Walking Disaster", a, Some(286_400))
        };
        assert!(!agree(&[one.clone(), live]));
        let video = musicbrainz::Entity {
            video: true,
            ..entity(b, "Walking Disaster", a, Some(286_400))
        };
        assert!(!agree(&[one.clone(), video]));
        // No artists or no title: never duplicates; one recording agrees with itself.
        assert!(!agree(&[
            one.clone(),
            entity(b, "Walking Disaster", "", Some(286_400))
        ]));
        assert!(!agree(&[entity(b, "", a, None), entity(a, "", a, None)]));
        assert!(agree(std::slice::from_ref(&one)));
        assert!(!agree(&[]));
        assert_eq!(
            primary(&[same.mbid.clone(), one.mbid.clone()]).map(String::as_str),
            Some("4da0e503-0cef-4dd7-a5b7-b08bea3db145")
        );
    }
}
