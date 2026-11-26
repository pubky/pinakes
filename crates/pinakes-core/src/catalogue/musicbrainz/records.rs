//! The MusicBrainz JSON dumps' lines as the importer reads them, what it keeps of each (the
//! records it sorts, [`Rec`]), and the cards it makes of an item's records ([`Item`]).
//!
//! A dump line is one entity (an artist, a release group, a release with its media, tracks and
//! recordings, or a recording on no release), as MusicBrainz's web service writes it with every
//! `inc`. Only the fields a card needs are read; the rest is skipped. Every list the service may
//! write as `null` reads as empty.
use crate::{
    catalogue::{link_key, Card},
    hubs::{self, MbKind},
    music::{self, Level},
};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A field the service may write as `null`: read as its default.
fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
/// A list the service may write as `null`, of which at most `N` entries are kept: the rest are
/// skipped without being built, so a line's memory never grows with a list's length.
fn capped<'de, D: Deserializer<'de>, T: Deserialize<'de>, const N: usize>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    struct Capped<T, const N: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> serde::de::Visitor<'de> for Capped<T, N> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a list or null")
        }
        fn visit_unit<E>(self) -> Result<Vec<T>, E> {
            Ok(vec![])
        }
        fn visit_none<E>(self) -> Result<Vec<T>, E> {
            Ok(vec![])
        }
        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Vec<T>, D2::Error> {
            d.deserialize_seq(self)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::new();
            while out.len() < N {
                match seq.next_element()? {
                    Some(item) => out.push(item),
                    None => return Ok(out),
                }
            }
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(out)
        }
    }
    d.deserialize_option(Capped::<T, N>(std::marker::PhantomData))
}
/// The most entries kept of each list of a line: a release's media and a medium's tracks (a box
/// set holds hundreds), an entity's relations, aliases, genres, ISRCs and credited artists.
const MEDIA: usize = 512;
const TRACKS: usize = 4096;
const RELATIONS: usize = 2048;
const ALIASES: usize = 256;
const GENRE_LIST: usize = 256;
const ISRCS: usize = 64;
const CREDITS: usize = 64;
const SECONDARY: usize = 16;

#[derive(Deserialize, Default)]
#[serde(default)]
struct Credit {
    #[serde(deserialize_with = "nullable")]
    name: String,
    #[serde(deserialize_with = "nullable")]
    joinphrase: String,
    #[serde(deserialize_with = "nullable")]
    artist: CreditArtist,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct CreditArtist {
    #[serde(deserialize_with = "nullable")]
    id: String,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Named {
    #[serde(deserialize_with = "nullable")]
    name: String,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Genre {
    #[serde(deserialize_with = "nullable")]
    name: String,
    #[serde(deserialize_with = "nullable")]
    count: u64,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Relation {
    #[serde(rename = "target-type", deserialize_with = "nullable")]
    target_type: String,
    #[serde(rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(deserialize_with = "nullable")]
    ended: bool,
    #[serde(deserialize_with = "nullable")]
    url: UrlTarget,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct UrlTarget {
    #[serde(deserialize_with = "nullable")]
    resource: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ArtistLine {
    #[serde(deserialize_with = "nullable")]
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    name: String,
    #[serde(rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(deserialize_with = "nullable")]
    area: Named,
    #[serde(deserialize_with = "nullable")]
    disambiguation: String,
    #[serde(deserialize_with = "capped::<_, _, ALIASES>")]
    aliases: Vec<Named>,
    #[serde(deserialize_with = "capped::<_, _, GENRE_LIST>")]
    genres: Vec<Genre>,
    #[serde(deserialize_with = "capped::<_, _, RELATIONS>")]
    relations: Vec<Relation>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct GroupLine {
    #[serde(deserialize_with = "nullable")]
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    title: String,
    #[serde(rename = "primary-type", deserialize_with = "nullable")]
    primary_type: String,
    #[serde(
        rename = "secondary-types",
        deserialize_with = "capped::<_, _, SECONDARY>"
    )]
    secondary_types: Vec<String>,
    #[serde(rename = "artist-credit", deserialize_with = "capped::<_, _, CREDITS>")]
    credit: Vec<Credit>,
    #[serde(rename = "first-release-date", deserialize_with = "nullable")]
    first_release_date: String,
    #[serde(deserialize_with = "nullable")]
    disambiguation: String,
    #[serde(deserialize_with = "capped::<_, _, ALIASES>")]
    aliases: Vec<Named>,
    #[serde(deserialize_with = "capped::<_, _, GENRE_LIST>")]
    genres: Vec<Genre>,
    #[serde(deserialize_with = "capped::<_, _, RELATIONS>")]
    relations: Vec<Relation>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ReleaseLine {
    #[serde(deserialize_with = "nullable")]
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    date: String,
    #[serde(rename = "artist-credit", deserialize_with = "capped::<_, _, CREDITS>")]
    credit: Vec<Credit>,
    #[serde(rename = "release-group", deserialize_with = "nullable")]
    group: EmbeddedGroup,
    #[serde(deserialize_with = "capped::<_, _, RELATIONS>")]
    relations: Vec<Relation>,
    #[serde(deserialize_with = "capped::<_, _, MEDIA>")]
    media: Vec<Medium>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct EmbeddedGroup {
    #[serde(deserialize_with = "nullable")]
    id: String,
    #[serde(deserialize_with = "nullable")]
    title: String,
    #[serde(rename = "first-release-date", deserialize_with = "nullable")]
    first_release_date: String,
    #[serde(deserialize_with = "capped::<_, _, GENRE_LIST>")]
    genres: Vec<Genre>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Medium {
    #[serde(deserialize_with = "capped::<_, _, TRACKS>")]
    tracks: Vec<TrackLine>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct TrackLine {
    #[serde(deserialize_with = "nullable")]
    title: String,
    #[serde(deserialize_with = "nullable")]
    recording: RecordingLine,
}
/// A recording: inside a release's track, or a line of the dump of recordings on no release.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RecordingLine {
    #[serde(deserialize_with = "nullable")]
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    title: String,
    #[serde(deserialize_with = "nullable")]
    disambiguation: String,
    #[serde(deserialize_with = "nullable")]
    length: u64,
    #[serde(deserialize_with = "nullable")]
    video: bool,
    #[serde(deserialize_with = "capped::<_, _, ISRCS>")]
    isrcs: Vec<String>,
    #[serde(rename = "artist-credit", deserialize_with = "capped::<_, _, CREDITS>")]
    credit: Vec<Credit>,
    #[serde(deserialize_with = "capped::<_, _, ALIASES>")]
    aliases: Vec<Named>,
    #[serde(deserialize_with = "capped::<_, _, GENRE_LIST>")]
    genres: Vec<Genre>,
    #[serde(deserialize_with = "capped::<_, _, RELATIONS>")]
    relations: Vec<Relation>,
}

/// What the importer keeps of a dump line, sorted under the key of the item it belongs to
/// ([`key`]): an item's card is made from all the records under its key ([`Item`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Rec {
    /// The content hash of the card stored under the key (an update compares with it). First, so
    /// a bounded group never leaves it out ([`super::spill::Merge::next_group`] keeps the
    /// payloads that sort first).
    Stored(Vec<u8>),
    /// An artist (`artist/<mbid>`).
    Artist(ArtistRec),
    /// A release group (`release-group/<mbid>`).
    Group(GroupRec),
    /// A release crediting the artist (`artist/<mbid>`): one per release.
    Credited,
    /// A release of the release group (`release-group/<mbid>`): its MBID and the album pages
    /// its URL relations name.
    Release { release: String, links: Vec<String> },
    /// A recording on one release, or on none (`recording/<mbid>`): one per release.
    Track(TrackRec),
    /// A recording carrying the ISRC (`isrc/<ISRC>`): its MBID.
    Isrc(String),
}
/// The first byte of each kind of record's payload (its postcard variant index), which
/// [`super::spill::Group::counts`] counts by.
pub mod tag {
    pub const STORED: usize = 0;
    pub const ARTIST: usize = 1;
    pub const GROUP: usize = 2;
    pub const CREDITED: usize = 3;
    pub const RELEASE: usize = 4;
    pub const TRACK: usize = 5;
    pub const ISRC: usize = 6;
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArtistRec {
    pub name: String,
    pub kind: String,
    pub area: String,
    pub disambiguation: String,
    pub aliases: Vec<String>,
    pub genres: Vec<(String, u64)>,
    pub wikidata: Vec<String>,
    pub links: Vec<String>,
    pub homepages: Vec<String>,
    pub wikipedia: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupRec {
    pub title: String,
    pub primary_type: String,
    pub secondary_types: Vec<String>,
    pub credit: String,
    pub artists: Vec<String>,
    pub first_release_date: String,
    pub disambiguation: String,
    pub aliases: Vec<String>,
    pub genres: Vec<(String, u64)>,
    pub wikidata: Vec<String>,
    pub links: Vec<String>,
    pub wikipedia: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackRec {
    pub title: String,
    pub disambiguation: String,
    pub length_ms: u64,
    pub video: bool,
    pub isrcs: Vec<String>,
    pub credit: String,
    pub artists: Vec<String>,
    pub aliases: Vec<String>,
    pub genres: Vec<(String, u64)>,
    pub links: Vec<String>,
    /// The track's own title where it is not the recording's.
    pub track_title: String,
    /// The release it is on (empty for a recording on none), its date, and its release group.
    pub release: String,
    pub release_date: String,
    pub group: String,
    pub group_title: String,
    pub group_date: String,
    pub group_genres: Vec<(String, u64)>,
}

/// The key of an entity's records: `<entity>/<mbid>`, also its card's `source_id`. Keys sort as
/// artists, ISRCs, recordings, release groups, so an item's ISRCs are known before its recording.
pub fn key(kind: MbKind, mbid: &str) -> String {
    format!("{}/{mbid}", kind.as_str())
}
pub fn isrc_key(isrc: &str) -> String {
    format!("isrc/{isrc}")
}

/// At most this many other names per card.
const OTHER_NAMES: usize = 16;
/// At most this many genres per card, by votes.
const GENRES: usize = 10;
/// At most this many `url:` keys per card (the item's own keys come first).
const LINK_KEYS: usize = 48;
/// At most this many links kept per record, before they are turned into keys.
const LINKS_PER_RECORD: usize = 64;
/// At most this many artists named in a card's details.
const DETAIL_ARTISTS: usize = 4;
/// At most this many ISRC keys per recording (`music_sources::MAX_ISRC_KEYS`).
const ISRC_KEYS: usize = 8;
/// The release count at which fame is 1.
const FAME_RELEASES: f64 = 1000.0;
/// The longest text kept of one field (bytes): longer ones are cut by `Card::checked`'s caps,
/// this bounds what is sorted.
const TEXT: usize = 1024;

fn text(s: &str) -> String {
    let mut s = s.trim().to_owned();
    crate::model::truncate(&mut s, TEXT);
    s
}
fn credit(credits: &[Credit]) -> (String, Vec<String>) {
    let mut text_out = String::new();
    let mut artists = vec![];
    for c in credits.iter().take(32) {
        text_out.push_str(&c.name);
        text_out.push_str(&c.joinphrase);
        if let Some(id) = hubs::mbid(&c.artist.id) {
            if !artists.contains(&id) {
                artists.push(id);
            }
        }
    }
    (text(&text_out), artists)
}
fn genres(genres: &[Genre]) -> Vec<(String, u64)> {
    let mut by_name: BTreeMap<String, u64> = BTreeMap::new();
    for g in genres {
        let name = text(&g.name);
        if !name.is_empty() {
            let most = by_name.entry(name).or_default();
            *most = (*most).max(g.count);
        }
    }
    let mut out: Vec<(String, u64)> = by_name.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(GENRES);
    out
}
/// The distinct aliases in their sort order (the dump's order of aliases may change from one dump
/// to the next), at most [`OTHER_NAMES`].
fn aliases(aliases: &[Named]) -> Vec<String> {
    let all: BTreeSet<String> = aliases
        .iter()
        .map(|a| text(&a.name))
        .filter(|a| !a.is_empty())
        .collect();
    all.into_iter().take(OTHER_NAMES).collect()
}
/// The current URL relations.
fn urls(relations: &[Relation]) -> impl Iterator<Item = (&str, &str)> {
    relations
        .iter()
        .filter(|r| r.target_type == "url" && !r.ended && !r.url.resource.is_empty())
        .map(|r| (r.kind.as_str(), r.url.resource.as_str()))
}
/// The pages of music services among `relations` at `level` (a recording's tracks and videos, an
/// album's albums, an artist's artist pages), as `music::parse` makes them canonical: what
/// people share for the item. A Discogs page only at its own level (`discogs_fits`).
fn music_links(relations: &[Relation], level: Level, kind: MbKind) -> Vec<String> {
    let mut out = BTreeSet::new();
    for (_, url) in urls(relations) {
        let Some(link) = music::parse(url) else {
            continue;
        };
        if crate::music_sources::join_level(&link) != Some(level)
            || !crate::music_sources::discogs_fits(url, kind)
        {
            continue;
        }
        if link.canonical.len() <= 2048 {
            out.insert(link.canonical);
        }
        if out.len() >= LINKS_PER_RECORD {
            break;
        }
    }
    out.into_iter().collect()
}
/// The Wikidata items a `wikidata` relation names.
fn wikidata(relations: &[Relation]) -> Vec<String> {
    let mut out: Vec<String> = urls(relations)
        .filter(|(kind, _)| *kind == "wikidata")
        .filter_map(|(_, url)| hubs::wikidata::item_link(url).map(|(qid, _)| qid))
        .collect();
    out.sort();
    out.dedup();
    out.truncate(4);
    out
}
fn of_kind(relations: &[Relation], wanted: &str, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = urls(relations)
        .filter(|(kind, url)| *kind == wanted && url.len() <= 2048)
        .map(|(_, url)| url.to_owned())
        .collect();
    out.sort();
    out.dedup();
    out.truncate(cap);
    out
}

/// The records of an artist line.
pub fn artist(line: &ArtistLine) -> Option<(String, Rec)> {
    let id = hubs::mbid(&line.id)?;
    Some((
        key(MbKind::Artist, &id),
        Rec::Artist(ArtistRec {
            name: text(&line.name),
            kind: text(&line.kind),
            area: text(&line.area.name),
            disambiguation: text(&line.disambiguation),
            aliases: aliases(&line.aliases),
            genres: genres(&line.genres),
            wikidata: wikidata(&line.relations),
            links: music_links(&line.relations, Level::Artist, MbKind::Artist),
            homepages: of_kind(&line.relations, "official homepage", 2),
            wikipedia: of_kind(&line.relations, "wikipedia", 8),
        }),
    ))
}
/// The record of a release group line.
pub fn group(line: &GroupLine) -> Option<(String, Rec)> {
    let id = hubs::mbid(&line.id)?;
    let (credit, artists) = credit(&line.credit);
    Some((
        key(MbKind::ReleaseGroup, &id),
        Rec::Group(GroupRec {
            title: text(&line.title),
            primary_type: text(&line.primary_type),
            secondary_types: line
                .secondary_types
                .iter()
                .map(|s| text(s))
                .filter(|s| !s.is_empty())
                .take(4)
                .collect(),
            credit,
            artists,
            first_release_date: date(&line.first_release_date),
            disambiguation: text(&line.disambiguation),
            aliases: aliases(&line.aliases),
            genres: genres(&line.genres),
            wikidata: wikidata(&line.relations),
            links: music_links(&line.relations, Level::Album, MbKind::ReleaseGroup),
            wikipedia: of_kind(&line.relations, "wikipedia", 8),
        }),
    ))
}
/// A date as MusicBrainz writes it (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`), else empty.
fn date(s: &str) -> String {
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = s.split('-').collect();
    let ok = match parts.as_slice() {
        [y] => digits(y, 4),
        [y, m] => digits(y, 4) && digits(m, 2),
        [y, m, d] => digits(y, 4) && digits(m, 2) && digits(d, 2),
        _ => false,
    };
    if ok {
        s.to_owned()
    } else {
        String::new()
    }
}
fn track(
    recording: &RecordingLine,
    track_title: &str,
    release: Option<(&ReleaseLine, &str)>,
) -> Option<(String, TrackRec)> {
    let id = hubs::mbid(&recording.id)?;
    let (credit, artists) = credit(&recording.credit);
    let title = text(&recording.title);
    let track_title = text(track_title);
    let mut isrcs: Vec<String> = recording
        .isrcs
        .iter()
        .filter_map(|i| music::isrc(i))
        .collect();
    isrcs.sort();
    isrcs.dedup();
    isrcs.truncate(32);
    let mut rec = TrackRec {
        track_title: if track_title == title {
            String::new()
        } else {
            track_title
        },
        title,
        disambiguation: text(&recording.disambiguation),
        length_ms: recording.length,
        video: recording.video,
        isrcs,
        credit,
        artists,
        aliases: aliases(&recording.aliases),
        genres: genres(&recording.genres),
        links: music_links(&recording.relations, Level::Track, MbKind::Recording),
        ..TrackRec::default()
    };
    if let Some((release, release_id)) = release {
        rec.release = release_id.to_owned();
        rec.release_date = date(&release.date);
        if let Some(group) = hubs::mbid(&release.group.id) {
            rec.group = group;
            rec.group_title = text(&release.group.title);
            rec.group_date = date(&release.group.first_release_date);
            rec.group_genres = genres(&release.group.genres);
        }
    }
    Some((key(MbKind::Recording, &id), rec))
}
/// The records of a release line: one `Credited` per artist its credit names, one `Release` for
/// its release group, and one `Track` (and an `Isrc` per ISRC) per recording on it, each once.
pub fn release(line: &ReleaseLine, out: &mut Vec<(String, Rec)>) {
    let Some(id) = hubs::mbid(&line.id) else {
        return;
    };
    let (_, artists) = credit(&line.credit);
    for artist in artists {
        out.push((key(MbKind::Artist, &artist), Rec::Credited));
    }
    if let Some(group) = hubs::mbid(&line.group.id) {
        out.push((
            key(MbKind::ReleaseGroup, &group),
            Rec::Release {
                release: id.clone(),
                // A Discogs release is an edition: it stays out of the album's keys.
                links: music_links(&line.relations, Level::Album, MbKind::Release)
                    .into_iter()
                    .filter(|l| music::discogs::of_uri(l).is_none())
                    .collect(),
            },
        ));
    }
    let mut seen = BTreeSet::new();
    for medium in &line.media {
        for t in &medium.tracks {
            let Some((k, rec)) = track(&t.recording, &t.title, Some((line, &id))) else {
                continue;
            };
            if !seen.insert(k.clone()) {
                continue;
            }
            let mbid = k[k.find('/').map_or(0, |i| i + 1)..].to_owned();
            for isrc in &rec.isrcs {
                out.push((isrc_key(isrc), Rec::Isrc(mbid.clone())));
            }
            out.push((k, Rec::Track(rec)));
        }
    }
}
/// The records of a line of the dump of recordings on no release.
pub fn standalone(line: &RecordingLine, out: &mut Vec<(String, Rec)>) {
    let Some((k, rec)) = track(line, "", None) else {
        return;
    };
    let mbid = k[k.find('/').map_or(0, |i| i + 1)..].to_owned();
    for isrc in &rec.isrcs {
        out.push((isrc_key(isrc), Rec::Isrc(mbid.clone())));
    }
    out.push((k, Rec::Track(rec)));
}

/// What the records under one key make: the item's card, and the stored card's hash when an
/// update read it.
#[derive(Debug, Default)]
pub struct Item {
    pub card: Option<Card>,
    pub stored: Option<Vec<u8>>,
}
/// The ISRCs that name more than one recording, as [`Item::of`] reads them under their keys
/// (which sort before the recordings'), each as one number ([`isrc_number`]), at most
/// [`SHARED_CAP`]: past it, [`Shared::full`] says so and the run stops rather than key an ISRC
/// it could not check.
#[derive(Default)]
pub struct Shared {
    isrcs: std::collections::HashSet<u64>,
    full: bool,
}
/// The most ISRCs [`Shared`] holds (8 bytes each).
pub const SHARED_CAP: usize = 16_000_000;
impl Shared {
    pub fn len(&self) -> usize {
        self.isrcs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.isrcs.is_empty()
    }
    /// More shared ISRCs than it may hold were found.
    pub fn full(&self) -> bool {
        self.full
    }
    fn insert(&mut self, isrc: &str) {
        let Some(n) = isrc_number(isrc) else { return };
        if self.isrcs.len() >= SHARED_CAP {
            self.full = true;
        } else {
            self.isrcs.insert(n);
        }
    }
    fn contains(&self, isrc: &str) -> bool {
        isrc_number(isrc).is_some_and(|n| self.isrcs.contains(&n))
    }
}
/// An ISRC (12 letters and digits) as the number its characters make in base 36, one to one.
pub fn isrc_number(isrc: &str) -> Option<u64> {
    let b = isrc.as_bytes();
    if b.len() != 12 {
        return None;
    }
    b.iter().try_fold(0u64, |n, c| {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'A'..=b'Z' => c - b'A' + 10,
            _ => return None,
        };
        Some(n * 36 + u64::from(d))
    })
}

fn fame(releases: u64) -> f32 {
    ((1.0 + releases as f64).ln() / (1.0 + FAME_RELEASES).ln()).min(1.0) as f32
}
fn labels(genres: &[(String, u64)]) -> Vec<String> {
    genres.iter().map(|(g, _)| g.clone()).collect()
}
fn wikidata_key(items: &[String]) -> Option<String> {
    // Exactly one item: several relations name no one item.
    match items {
        [one] => hubs::key::wikidata(one),
        _ => None,
    }
}
/// The English article if there is one, else the first.
fn wikipedia(articles: &[String]) -> Option<String> {
    articles
        .iter()
        .find(|a| a.starts_with("https://en.wikipedia.org/"))
        .or_else(|| articles.first())
        .cloned()
}
fn link_keys(links: impl IntoIterator<Item = String>) -> Vec<String> {
    let set: BTreeSet<String> = links.into_iter().filter_map(|l| link_key(&l)).collect();
    set.into_iter().take(LINK_KEYS).collect()
}
fn length(ms: u64) -> Option<String> {
    if ms == 0 {
        return None;
    }
    let secs = ms.saturating_add(500) / 1000;
    Some(if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    })
}
fn year(date: &str) -> &str {
    date.get(..4).unwrap_or("")
}
fn joined(parts: impl IntoIterator<Item = String>) -> Option<String> {
    let parts: Vec<String> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

impl Item {
    /// The item under `key` from its records (those a bounded group kept) and `counts`, every
    /// record of the key by kind ([`tag`]); `shared` takes the ISRCs that name several
    /// recordings (an `isrc/` key makes no card).
    pub fn of(key: &str, recs: Vec<Rec>, counts: &[u64], shared: &mut Shared) -> Item {
        let mut item = Item::default();
        let mut rest = vec![];
        for rec in recs {
            match rec {
                Rec::Stored(hash) => item.stored = Some(hash),
                rec => rest.push(rec),
            }
        }
        let count = |t: usize| counts.get(t).copied().unwrap_or(0);
        let Some((entity, mbid)) = key.split_once('/') else {
            return item;
        };
        item.card = match entity {
            "artist" => artist_card(mbid, rest, count(tag::CREDITED)),
            "release-group" => group_card(mbid, rest, count(tag::RELEASE)),
            "recording" => recording_card(mbid, rest, count(tag::TRACK), shared),
            "isrc" => {
                let recordings: BTreeSet<String> = rest
                    .into_iter()
                    .filter_map(|r| match r {
                        Rec::Isrc(m) => Some(m),
                        _ => None,
                    })
                    .collect();
                if recordings.len() > 1 {
                    shared.insert(mbid);
                }
                None
            }
            _ => None,
        };
        item
    }
}

fn artist_card(mbid: &str, recs: Vec<Rec>, releases: u64) -> Option<Card> {
    let a = recs.into_iter().find_map(|r| match r {
        Rec::Artist(a) => Some(a),
        _ => None,
    })?;
    let kind = if a.kind.is_empty() {
        "Artist".to_owned()
    } else {
        a.kind.clone()
    };
    let what = if a.area.is_empty() {
        kind
    } else {
        format!("{kind} from {}", a.area)
    };
    let mut keys: Vec<String> = hubs::key::mb(MbKind::Artist, mbid).into_iter().collect();
    keys.extend(wikidata_key(&a.wikidata));
    keys.extend(link_keys(a.links));
    let mut details = serde_json::Map::new();
    if let Some(article) = wikipedia(&a.wikipedia) {
        details.insert("wikipedia".into(), article.into());
    }
    Some(Card {
        source_id: key(MbKind::Artist, mbid),
        uri: hubs::musicbrainz::entity_page(MbKind::Artist, mbid),
        r#type: if a.kind == "Person" {
            "Person"
        } else {
            "MusicGroup"
        }
        .into(),
        name: a.name,
        other_names: a.aliases,
        description: joined([what, a.disambiguation]),
        labels: labels(&a.genres),
        keys,
        related_uris: a.homepages,
        fame: fame(releases),
        details,
        ..Card::default()
    })
}

fn group_card(mbid: &str, recs: Vec<Rec>, releases: u64) -> Option<Card> {
    let mut group = None;
    let mut links = BTreeSet::new();
    for rec in recs {
        match rec {
            Rec::Group(g) => group = Some(g),
            Rec::Release { links: more, .. } => links.extend(more),
            _ => {}
        }
    }
    let g = group?;
    links.extend(g.links);
    let kind = match (g.primary_type.as_str(), g.secondary_types.as_slice()) {
        ("", []) => "Release".to_owned(),
        ("", more) => more.join(", "),
        (primary, []) => primary.to_owned(),
        (primary, more) => format!("{primary} ({})", more.join(", ")),
    };
    let what = if g.credit.is_empty() {
        kind
    } else {
        format!("{kind} by {}", g.credit)
    };
    let mut keys: Vec<String> = hubs::key::mb(MbKind::ReleaseGroup, mbid)
        .into_iter()
        .collect();
    keys.extend(wikidata_key(&g.wikidata));
    keys.extend(link_keys(links));
    let mut details = serde_json::Map::new();
    if !g.artists.is_empty() {
        details.insert(
            "artists".into(),
            g.artists
                .iter()
                .take(DETAIL_ARTISTS)
                .cloned()
                .collect::<Vec<_>>()
                .into(),
        );
    }
    if let Some(article) = wikipedia(&g.wikipedia) {
        details.insert("wikipedia".into(), article.into());
    }
    Some(Card {
        source_id: key(MbKind::ReleaseGroup, mbid),
        uri: hubs::musicbrainz::entity_page(MbKind::ReleaseGroup, mbid),
        r#type: "MusicAlbum".into(),
        name: g.title,
        other_names: g.aliases,
        description: joined([
            what,
            year(&g.first_release_date).to_owned(),
            g.disambiguation,
        ]),
        labels: labels(&g.genres),
        keys,
        fame: fame(releases),
        details,
        ..Card::default()
    })
}

fn recording_card(mbid: &str, recs: Vec<Rec>, tracks_seen: u64, shared: &Shared) -> Option<Card> {
    let mut tracks: Vec<TrackRec> = recs
        .into_iter()
        .filter_map(|r| match r {
            Rec::Track(t) => Some(t),
            _ => None,
        })
        .collect();
    // The earliest release first (a release without a date after those with one), then by the
    // release's MBID: the order the records came in never changes the card.
    tracks.sort_by(|a, b| {
        (
            a.release.is_empty(),
            a.release_date.is_empty(),
            &a.release_date,
            &a.release,
        )
            .cmp(&(
                b.release.is_empty(),
                b.release_date.is_empty(),
                &b.release_date,
                &b.release,
            ))
    });
    // One record per release it is on (and one for a recording on none).
    let standalone = tracks.iter().any(|t| t.release.is_empty());
    let releases = tracks_seen.saturating_sub(u64::from(standalone));
    let first = tracks.first()?.clone();
    let mut other_names = first.aliases.clone();
    let mut titles: BTreeSet<String> = tracks
        .iter()
        .map(|t| t.track_title.clone())
        .filter(|t| !t.is_empty())
        .collect();
    titles.remove(&first.title);
    for t in titles {
        if other_names.len() >= OTHER_NAMES {
            break;
        }
        if !other_names.contains(&t) {
            other_names.push(t);
        }
    }
    // Its own genres, else those of the first of its releases (earliest first) whose release
    // group has some.
    let genres = if first.genres.is_empty() {
        tracks
            .iter()
            .map(|t| &t.group_genres)
            .find(|g| !g.is_empty())
            .unwrap_or(&first.group_genres)
    } else {
        &first.genres
    };
    let mut keys: Vec<String> = hubs::key::mb(MbKind::Recording, mbid).into_iter().collect();
    keys.extend(
        first
            .isrcs
            .iter()
            .filter(|i| !shared.contains(i))
            .take(ISRC_KEYS)
            .map(|i| format!("isrc:{i}")),
    );
    keys.extend(link_keys(first.links.iter().cloned()));
    let album = match (first.group_title.as_str(), year(&first.group_date)) {
        ("", _) => String::new(),
        (title, "") => title.to_owned(),
        (title, y) => format!("{title} ({y})"),
    };
    let mut details = serde_json::Map::new();
    if !first.artists.is_empty() {
        details.insert(
            "artists".into(),
            first
                .artists
                .iter()
                .take(DETAIL_ARTISTS)
                .cloned()
                .collect::<Vec<_>>()
                .into(),
        );
    }
    if !first.group.is_empty() {
        details.insert("release_group".into(), first.group.clone().into());
    }
    Some(Card {
        source_id: key(MbKind::Recording, mbid),
        uri: hubs::musicbrainz::entity_page(MbKind::Recording, mbid),
        r#type: "MusicRecording".into(),
        name: first.title.clone(),
        other_names,
        description: joined([
            first.credit.clone(),
            album,
            length(first.length_ms).unwrap_or_default(),
            first.disambiguation.clone(),
        ]),
        labels: labels(genres),
        keys,
        fame: fame(releases),
        details,
        ..Card::default()
    })
}
