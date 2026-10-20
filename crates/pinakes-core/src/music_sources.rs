//! "Also on" for music: the same track, album or artist on other services, from exact hub
//! lookups only (never a title or name).
//!
//! Who may vouch for a key: a hub (MusicBrainz, Wikidata) or a known music service's own record
//! of its own object. Any other page's claim is a fact at most.
//!
//! - [`ServiceSource`]: the ISRC a music service's own track page states (its JSON-LD
//!   `isrcCode`) as the key `isrc:…` when the service vouches for it (TIDAL,
//!   `Service::vouches_isrc`), and as a fact with its artist, album and length.
//! - [`crate::also_on::tidal::Tidal`] (with an operator's TIDAL client ID and secret): a TIDAL
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
    also_on::{Context, Found, Key, Link, Source, Subject},
    hubs::{self, musicbrainz, wikidata, MbKind},
    music::{self, Level, Service},
    sources::discogs,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// The level a link is joined at: a video counts as its recording, a YouTube playlist as the
/// album it holds; other playlists are not joined.
fn join_level(link: &music::Link) -> Option<Level> {
    match (link.level, link.service) {
        (Level::Video, _) => Some(Level::Track),
        (Level::Playlist, Service::YouTube) => Some(Level::Album),
        (Level::Playlist, _) => None,
        (level, _) => Some(level),
    }
}
/// A link found on a hub, when it is a music link at `level`: its canonical URL and service.
fn derived(url: &str, level: Level, via: &str) -> Option<Link> {
    let found = music::parse(url)?;
    (join_level(&found)? == level).then(|| Link::new(found.canonical, found.service.name(), via))
}

/// At most this many releases of one link are read to find the release group they share.
const MAX_EDITIONS: usize = 4;
/// At most this many of a recording's ISRCs become keys (the MBID key always fits).
const MAX_ISRC_KEYS: usize = 8;

/// The ISRC a music service's track page states (its JSON-LD `isrcCode`), never another host's
/// claim: a fact.
fn page_isrc(subject: &Subject) -> Option<(Service, String)> {
    let link = music::parse(&subject.uri)?;
    (join_level(&link)? == Level::Track && link.level == Level::Track).then_some(())?;
    let isrc = music::isrc(subject.doc.as_ref()?.attributes.get("isrc")?)?;
    Some((link.service, isrc))
}
/// The page's ISRC when its service vouches for it (`Service::vouches_isrc`): a key.
fn own_isrc(subject: &Subject) -> Option<String> {
    page_isrc(subject)
        .filter(|(service, _)| service.vouches_isrc())
        .map(|(_, isrc)| isrc)
}

/// Whether MusicBrainz gives `isrc` to several recordings: such an ISRC names none of them, so
/// it is a fact, never a key.
pub(crate) async fn shared_isrc(cx: &Context, isrc: &str) -> Result<bool> {
    Ok(
        musicbrainz::isrc(&cx.egress, cx.config.also_on.musicbrainz_base(), isrc)
            .await?
            .is_some_and(|recordings| recordings.len() > 1),
    )
}

/// A music service's own record of its own object: the ISRC on its track page (a key only from
/// a service that vouches for it), its artist, album and length.
pub struct ServiceSource;
#[async_trait]
impl Source for ServiceSource {
    fn id(&self) -> &'static str {
        "service"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let mut found = Found::default();
        if let Some((service, isrc)) = page_isrc(subject) {
            let shared = service.vouches_isrc() && shared_isrc(cx, &isrc).await?;
            if service.vouches_isrc() && !shared {
                found.keys.push(Key {
                    key: format!("isrc:{isrc}"),
                    via: service.token().into(),
                });
            }
            found.facts.push(crate::also_on::Fact::new("isrc", isrc));
        }
        // What its page says: the artist, a track's album and length (seconds).
        let attributes = subject.doc.as_ref().map(|d| &d.attributes);
        let attribute = |name: &str| attributes.and_then(|a| a.get(name)).map(|v| v.trim());
        if let Some(artist) = attribute("artist") {
            found
                .facts
                .push(crate::also_on::Fact::new("artist", artist));
        }
        if link.level == Level::Track {
            if let Some(album) = attribute("album") {
                found.facts.push(crate::also_on::Fact::new("album", album));
            }
            let seconds = attribute("duration")
                .and_then(crate::event::time::duration)
                .map(|d| d.num_seconds())
                .filter(|s| (1..=86_400).contains(s));
            if let Some(seconds) = seconds {
                found
                    .facts
                    .push(crate::also_on::Fact::new("duration", seconds.to_string()));
            }
        }
        Ok(found)
    }
}

/// MusicBrainz relations of an exact link.
pub struct MusicBrainzSource;
#[async_trait]
impl Source for MusicBrainzSource {
    fn id(&self) -> &'static str {
        "musicbrainz"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| join_level(&l).is_some())
    }
    async fn prefetch(&self, cx: &Context, uris: &[String]) {
        let links: Vec<String> = uris
            .iter()
            .filter_map(|u| music::parse(u))
            .filter(|l| join_level(l).is_some())
            .map(|l| l.canonical)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if links.len() > 1 {
            let base = cx.config.also_on.musicbrainz_base();
            if let Err(error) = musicbrainz::prefetch_urls(&cx.egress, base, &links).await {
                tracing::debug!(error = %format!("{error:#}"), "batched MusicBrainz lookup failed; links ask alone");
            }
        }
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, Found::default)
    }
}
impl MusicBrainzSource {
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let Some(level) = join_level(&link) else {
            return Ok(Found::default());
        };
        let base = cx.config.also_on.musicbrainz_base();
        let targets = musicbrainz::url(&cx.egress, base, &link.canonical)
            .await?
            .map(|t| t.targets)
            .unwrap_or_default();
        let at = |kind: MbKind| -> Vec<String> {
            targets
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, id)| id.clone())
                .collect()
        };
        // Exactly one entity at the link's level; several are ambiguous and join nothing,
        // except releases of one release group. A Discogs master is a release group and a
        // Discogs release its releases, never across.
        let discogs = discogs::of_link(&link).map(|(kind, _)| kind);
        let (mut kind, mut ids) = match level {
            Level::Track => (MbKind::Recording, at(MbKind::Recording)),
            Level::Artist => (MbKind::Artist, at(MbKind::Artist)),
            _ => match (discogs, at(MbKind::Release)) {
                (Some(discogs::Kind::Master), _) => {
                    (MbKind::ReleaseGroup, at(MbKind::ReleaseGroup))
                }
                (None, releases) if releases.is_empty() => {
                    (MbKind::ReleaseGroup, at(MbKind::ReleaseGroup))
                }
                (_, releases) => (MbKind::Release, releases),
            },
        };
        // A track MusicBrainz does not relate the URL to: the one recording its page's own ISRC
        // is on.
        if kind == MbKind::Recording && ids.is_empty() {
            if let Some(isrc) = own_isrc(subject) {
                if let Some(recordings) = musicbrainz::isrc(&cx.egress, base, &isrc).await? {
                    if recordings.len() == 1 {
                        ids = recordings;
                    }
                }
            }
        }
        // An album MusicBrainz does not relate the URL to: the releases carrying its barcode keys
        // (TIDAL's own record, or a service's from an earlier run), one or editions of one
        // release group, as a related URL would name them. Never a barcode this source keyed
        // itself: that would re-find a release after an editor removed its URL relation. Never
        // for a Discogs master either: it is a release group, and barcodes name releases.
        if level == Level::Album && ids.is_empty() && discogs != Some(discogs::Kind::Master) {
            let barcodes: Vec<&str> = subject
                .keys
                .iter()
                .filter(|(_, source)| !source.starts_with("musicbrainz"))
                .filter_map(|(key, _)| key.strip_prefix("upc:"))
                .take(2)
                .collect();
            let mut releases = std::collections::BTreeSet::new();
            for upc in barcodes {
                releases.extend(musicbrainz::barcode(&cx.egress, base, upc).await?);
            }
            if !releases.is_empty() {
                kind = MbKind::Release;
                ids = releases.into_iter().collect();
            }
        }
        if ids.is_empty() || ids.len() > 1 && (kind != MbKind::Release || ids.len() > MAX_EDITIONS)
        {
            return Ok(Found::default());
        }
        entities_found(
            cx,
            base,
            kind,
            &ids,
            level,
            discogs == Some(discogs::Kind::Release),
        )
        .await
    }
}

/// A MusicBrainz page as its own hub: a recording's, release's, release group's or artist's page
/// carries that entity's keys and facts and lists its links on music services at its level, as
/// the `musicbrainz` source does for a service link MusicBrainz relates to the entity. An ISRC's
/// page keys the ISRC and the one recording MusicBrainz gives it to; an ISRC several recordings
/// have names none of them (as a service's page ISRC), so its page keeps it as a fact only.
pub struct MusicBrainzEntitySource;
#[async_trait]
impl Source for MusicBrainzEntitySource {
    fn id(&self) -> &'static str {
        "musicbrainz_entity"
    }
    fn applies(&self, subject: &Subject) -> bool {
        musicbrainz::entity_link(&subject.uri).is_some()
            || musicbrainz::isrc_link(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, || {
            Self::own_keys(&subject.uri)
        })
    }
}
impl MusicBrainzEntitySource {
    /// What the page's URL alone proves: the entity's MBID, or the ISRC (with its fact).
    fn own_keys(uri: &str) -> Found {
        let mut found = Found::default();
        if let Some((kind, mbid, _)) = musicbrainz::entity_link(uri) {
            found.keys.extend(hubs::key::mb(kind, &mbid).map(|key| Key {
                key,
                via: "musicbrainz".into(),
            }));
        } else if let Some((isrc, _)) = musicbrainz::isrc_link(uri) {
            found.keys.push(Key {
                key: format!("isrc:{isrc}"),
                via: "musicbrainz".into(),
            });
            found.facts.push(crate::also_on::Fact::new("isrc", isrc));
        }
        found
    }
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let base = cx.config.also_on.musicbrainz_base();
        if let Some((kind, mbid, _)) = musicbrainz::entity_link(&subject.uri) {
            let level = match kind {
                MbKind::Recording => Level::Track,
                MbKind::Release | MbKind::ReleaseGroup => Level::Album,
                MbKind::Artist => Level::Artist,
            };
            let mut found =
                entities_found(cx, base, kind, std::slice::from_ref(&mbid), level, false).await?;
            // A merged entity answers its old MBID with the one it was merged into: the page keys
            // that one, and keeps the old as a fact, so `key=mbid:{old}` still finds it.
            if found
                .facts
                .iter()
                .any(|f| f.name == "mbid" && f.value != mbid)
            {
                found
                    .facts
                    .push(crate::also_on::Fact::new("mbid_was", &mbid));
            }
            return Ok(found);
        }
        let Some((isrc, _)) = musicbrainz::isrc_link(&subject.uri) else {
            return Ok(Found::default());
        };
        let own = Key {
            key: format!("isrc:{isrc}"),
            via: "musicbrainz".into(),
        };
        let mut found = match musicbrainz::isrc(&cx.egress, base, &isrc).await? {
            Some(recordings) if recordings.len() == 1 => {
                let mut found = entities_found(
                    cx,
                    base,
                    MbKind::Recording,
                    &recordings,
                    Level::Track,
                    false,
                )
                .await?;
                // Its own ISRC first: a recording's other ISRCs are keys only up to a cap.
                found.keys.retain(|k| k.key != own.key);
                found.keys.insert(0, own);
                found
            }
            Some(recordings) if recordings.len() > 1 => Found::default(),
            // An ISRC MusicBrainz doesn't know is still its own key.
            _ => Found {
                keys: vec![own],
                ..Default::default()
            },
        };
        let fact = crate::also_on::Fact::new("isrc", &isrc);
        if !found.facts.contains(&fact) {
            found.facts.push(fact);
        }
        Ok(found)
    }
}

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

/// What MusicBrainz says about one entity, or about up to four releases of one release group
/// (`kind`, `ids`): their MBIDs (the release group's alone for several releases), the recording's
/// ISRCs and the release's barcode as keys, their facts, and their current links on music
/// services at `level`, each keyed by the entity MusicBrainz names it on. Nothing when an entity
/// is unknown, or when several releases are of different release groups.
async fn entities_found(
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
    // An edition's link (a Discogs release) is not the album: no release group's key.
    match &entity.release_group {
        Some(_) if edition => {}
        Some(group)
            if entities
                .iter()
                .all(|e| e.release_group.as_ref() == Some(group)) =>
        {
            key(MbKind::ReleaseGroup, group)
        }
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
    if let Some(upc) = barcode.and_then(|b| music::gtin13(b)) {
        found.keys.push(Key {
            key: format!("upc:{upc}"),
            via: "musicbrainz".into(),
        });
    }
    Ok(found)
}

/// Deezer's public API: its own record of a Deezer link, and its lookups by ISRC and barcode.
pub struct DeezerSource;
const DEEZER_API: &str = "https://api.deezer.com";
#[async_trait]
impl Source for DeezerSource {
    fn id(&self) -> &'static str {
        "deezer"
    }
    fn enabled_by_default(&self) -> bool {
        false
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| match (l.service, l.level) {
            (Service::Deezer, Level::Track | Level::Album) => true,
            (_, Level::Track) => subject.keys_in("isrc").next().is_some(),
            (_, Level::Album) => subject.keys_in("upc").next().is_some(),
            _ => false,
        })
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let (kind, field, namespace) = match link.level {
            Level::Track => ("track", "isrc", "isrc"),
            Level::Album => ("album", "upc", "upc"),
            _ => return Ok(Found::default()),
        };
        let normal = if kind == "track" {
            music::isrc
        } else {
            music::gtin13
        };
        let mut found = Found::default();
        if link.service == Service::Deezer {
            // Its own record of its own object.
            let Some(v) = deezer(cx, &format!("{kind}/{}", link.id)).await? else {
                return Ok(found);
            };
            if let Some(value) = v[field].as_str().and_then(normal) {
                found.keys.push(Key {
                    key: format!("{namespace}:{value}"),
                    via: "deezer".into(),
                });
                found
                    .facts
                    .push(crate::also_on::Fact::new(namespace, value));
            }
            return Ok(found);
        }
        // The link's own ISRC first, then the hubs'.
        let own = own_isrc(subject).filter(|_| kind == "track");
        let mut values: Vec<&str> = own.as_deref().into_iter().collect();
        values.extend(
            subject
                .keys_in(namespace)
                .filter(|v| Some(*v) != own.as_deref()),
        );
        for value in values.into_iter().take(2) {
            // Deezer finds a UPC-A by its 12 digits, without GTIN-13's leading zero.
            let asked = match value.strip_prefix('0') {
                Some(upc) if kind == "album" => upc,
                _ => value,
            };
            let Some(v) = deezer(cx, &format!("{kind}/{namespace}:{asked}")).await? else {
                continue;
            };
            // Only the object that has the value asked.
            if v[field].as_str().and_then(normal).as_deref() != Some(value) {
                continue;
            }
            let url = format!(
                "https://www.deezer.com/{kind}/{}",
                v["id"].as_u64().unwrap_or(0)
            );
            if let Some(found_link) = derived(&url, link.level, "deezer") {
                found
                    .links
                    .push(found_link.keyed(format!("{namespace}:{value}")));
            }
        }
        Ok(found)
    }
}
/// One Deezer API object; `None` when Deezer has no data (its error 800) or no such ID.
async fn deezer(cx: &Context, path: &str) -> Result<Option<Value>> {
    let options = crate::resolver::egress::FetchOptions {
        accept: Some("application/json".into()),
        ..Default::default()
    };
    let fetched = cx
        .egress
        .discover(&format!("{DEEZER_API}/{path}"), &options, 3600)
        .await?;
    let busy = || -> anyhow::Error {
        crate::resolver::RetryLater {
            class: "deezer_busy",
            seconds: fetched.retry_after_secs.clamp(60, 3600),
        }
        .into()
    };
    match fetched.status {
        200 => {}
        429 | 500..=599 => return Err(busy()),
        _ => return Ok(None),
    }
    let v: Value = serde_json::from_slice(&fetched.body)?;
    // Deezer answers errors with 200: 4 is its quota, 800 "no data".
    match v["error"]["code"].as_u64() {
        None if v["id"].as_u64().is_some() => Ok(Some(v)),
        Some(4) => Err(busy()),
        _ => Ok(None),
    }
}

/// Apple Music's album for an album's barcode keys, from iTunes.
pub struct ItunesSource;
#[async_trait]
impl Source for ItunesSource {
    fn id(&self) -> &'static str {
        "itunes"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| {
            join_level(&l) == Some(Level::Album) && l.service != Service::AppleMusic
        }) && subject.keys_in("upc").next().is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let mut found = Found::default();
        for upc in subject.keys_in("upc").take(2) {
            if let Some(apple) = itunes_album(cx, upc).await? {
                found.links.push(apple.keyed(format!("upc:{upc}")));
            }
        }
        Ok(found)
    }
}

/// Whether a Discogs page MusicBrainz relates to a `kind` entity is at its level: an artist's
/// artist, a release group's master, a release's release (any other link: yes).
fn discogs_fits(url: &str, kind: MbKind) -> bool {
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
fn facts(entity: &musicbrainz::Entity) -> Vec<crate::also_on::Fact> {
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

/// Apple Music's album for a barcode: iTunes `lookup?upc=` in the configured storefront, when
/// exactly one album answers (an unknown code can return unrelated albums). Its results carry no
/// UPC, so unlike Deezer's the answer can't be read back against the code asked: exactly one
/// album is the only check, and `upc` is no single-object namespace that would catch a wrong one.
async fn itunes_album(cx: &Context, barcode: &str) -> Result<Option<Link>> {
    let storefront = &cx.config.also_on.apple_storefront;
    let url =
        format!("https://itunes.apple.com/lookup?upc={barcode}&entity=album&country={storefront}");
    let fetched = cx.egress.discover(&url, &itunes_options(), 3600).await?;
    match fetched.status {
        200 => {}
        429 | 500..=599 => {
            return Err(crate::resolver::RetryLater {
                class: "itunes_busy",
                seconds: fetched.retry_after_secs.clamp(60, 3600),
            }
            .into())
        }
        _ => return Ok(None),
    }
    // iTunes answers `text/javascript` with leading newlines.
    let v: Value = serde_json::from_slice(fetched.body.trim_ascii())?;
    Ok(itunes_parse(&v))
}
/// How iTunes is asked: for JSON, which it serves as `text/javascript`.
fn itunes_options() -> crate::resolver::egress::FetchOptions {
    crate::resolver::egress::FetchOptions {
        accept: Some("application/json".into()),
        extra_types: &["text/javascript"],
        ..Default::default()
    }
}
/// The one album of an iTunes lookup, as an Apple Music link.
pub fn itunes_parse(v: &Value) -> Option<Link> {
    let albums: Vec<&Value> = v["results"]
        .as_array()?
        .iter()
        .filter(|r| r["wrapperType"] == "collection" && r["collectionType"] == "Album")
        .collect();
    let [album] = albums.as_slice() else {
        return None;
    };
    derived(album["collectionViewUrl"].as_str()?, Level::Album, "itunes")
}

/// The Wikidata properties of music services, by service and level, and how each value makes
/// a URL (`{sf}` is the Apple storefront).
const PROPERTIES: &[(&str, Service, Level, &str)] = &[
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
fn mb_property(level: Level) -> (&'static str, MbKind) {
    match level {
        Level::Track | Level::Video => ("P4404", MbKind::Recording),
        Level::Artist => ("P434", MbKind::Artist),
        _ => ("P436", MbKind::ReleaseGroup),
    }
}
fn property_url(template: &str, value: &str, storefront: &str) -> String {
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

/// Wikidata's item for an exact service ID.
pub struct WikidataSource;
#[async_trait]
impl Source for WikidataSource {
    fn id(&self) -> &'static str {
        "wikidata"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| property_of(&l).is_some())
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, Found::default)
    }
}
impl WikidataSource {
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let (Some(property), Some(level)) = (property_of(&link), join_level(&link)) else {
            return Ok(Found::default());
        };
        // A Discogs ID is the number alone.
        let id = discogs::of_link(&link).map_or(link.id.clone(), |(_, id)| id.to_string());
        let item = match wikidata::item_with(&cx.egress, property, &id).await? {
            wikidata::Found::One(item) => item,
            _ => return Ok(Found::default()),
        };
        // An item with a Discogs release is an edition's only when it is not the album's.
        if property == DISCOGS_RELEASE && ["P1954", "P436"].iter().any(|p| !item.id(p).is_empty()) {
            return Ok(Found::default());
        }
        Ok(found(&item, level, &cx.config.also_on.apple_storefront))
    }
}
/// The Wikidata property for a link's own service ID, if Wikidata has one.
fn property_of(link: &music::Link) -> Option<&'static str> {
    if discogs::of_link(link).is_some_and(|(kind, _)| kind == discogs::Kind::Release) {
        return Some(DISCOGS_RELEASE);
    }
    PROPERTIES
        .iter()
        .find(|(_, service, level, _)| *service == link.service && *level == link.level)
        .map(|(p, ..)| *p)
}
/// Whether an item is one recording: exactly one MusicBrainz recording ID and no work ID. A song
/// item is a work (its studio, live and remastered recordings share it), so a track takes
/// nothing from it, neither its Q-id nor its tracks.
fn recording_item(item: &wikidata::Item) -> bool {
    item.id("P4404").len() == 1 && item.id("P435").is_empty()
}
/// Wikidata's Discogs release ID.
const DISCOGS_RELEASE: &str = "P2206";
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
        let release = music::parse(release).unwrap();
        assert_eq!(property_of(&release), Some("P2206"));
        let master = music::parse(master).unwrap();
        assert_eq!(property_of(&master), Some("P1954"));
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

    /// iTunes' real answer (`text/javascript`) passes the egress checks; without admitting it,
    /// the lookup was refused as `policy` and took MusicBrainz's answer down with it.
    #[tokio::test]
    async fn itunes_javascript_json_is_admitted() {
        use axum::{routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/lookup",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    "\n\n\n{\"resultCount\":0,\"results\":[]}",
                )
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url: url::Url = format!("http://{addr}/lookup").parse().unwrap();
        let client = reqwest::Client::new();
        let read = |options: crate::resolver::egress::FetchOptions| {
            let (url, request) = (url.clone(), client.get(url.clone()));
            async move {
                crate::resolver::egress::read_response_with(
                    url,
                    request.send().await.unwrap(),
                    &options,
                )
                .await
            }
        };
        let fetched = read(itunes_options()).await.unwrap();
        assert_eq!(fetched.status, 200);
        let v: Value = serde_json::from_slice(fetched.body.trim_ascii()).unwrap();
        assert!(itunes_parse(&v).is_none());
        assert!(
            read(Default::default()).await.is_err(),
            "refused without the admission"
        );
        server.abort();
    }

    #[test]
    fn itunes_counts_only_one_album() {
        let link = itunes_parse(&fixture("itunes-upc.json")).unwrap();
        assert_eq!(link.uri, "https://music.apple.com/us/album/1097861387");
        assert_eq!(link.via, "itunes");
        assert!(itunes_parse(&fixture("itunes-upc-many.json")).is_none());
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
}
