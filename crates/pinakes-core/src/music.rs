//! Music links: which service, which level (track, album, artist, playlist) and the one
//! canonical URL each object has, read from the URL alone (no network).
//!
//! Every spelling of one object on one service (a country or language path, `intl-xx`, the
//! `spotify:` URI, `youtu.be`, `music.youtube.com`, `listen.tidal.com`, an Apple Music
//! storefront or an album URL with `?i=` for one of its tracks, tracking parameters) folds into
//! its canonical URL, an alias (`alias::root`). Short links that need a request to expand (`spotify.link`,
//! `deezer.page.link`, `on.soundcloud.com`) are not folded.
use url::Url;

/// A music service a link can be on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Service {
    Spotify,
    AppleMusic,
    YouTube,
    Deezer,
    Tidal,
    Qobuz,
    SoundCloud,
    AmazonMusic,
    Bandcamp,
    /// The Discogs database ([`discogs`]): an artist, a master or a release.
    Discogs,
}
impl Service {
    /// The service's name as a card shows it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Spotify => "Spotify",
            Self::AppleMusic => "Apple Music",
            Self::YouTube => "YouTube",
            Self::Deezer => "Deezer",
            Self::Tidal => "TIDAL",
            Self::Qobuz => "Qobuz",
            Self::SoundCloud => "SoundCloud",
            Self::AmazonMusic => "Amazon Music",
            Self::Bandcamp => "Bandcamp",
            Self::Discogs => "Discogs",
        }
    }
    /// Whether the ISRC its own track page states (JSON-LD `isrcCode`) is a key: only a service
    /// whose catalogue comes from labels and whose page was checked to state it (TIDAL). On
    /// Bandcamp and SoundCloud an uploader types it; elsewhere it is a fact.
    pub fn vouches_isrc(self) -> bool {
        matches!(self, Self::Tidal)
    }
    /// The service as a row's `via` (`apple_music`).
    pub fn token(self) -> &'static str {
        match self {
            Self::Spotify => "spotify",
            Self::AppleMusic => "apple_music",
            Self::YouTube => "youtube",
            Self::Deezer => "deezer",
            Self::Tidal => "tidal",
            Self::Qobuz => "qobuz",
            Self::SoundCloud => "soundcloud",
            Self::AmazonMusic => "amazon_music",
            Self::Bandcamp => "bandcamp",
            Self::Discogs => "discogs",
        }
    }
}
/// What a link names. A video on YouTube is a `Track` only when a hub says it is a recording;
/// the URL alone makes it a `Video`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Track,
    Album,
    Artist,
    Playlist,
    Video,
}
impl Level {
    /// The schema.org type of a card at this level.
    pub fn type_name(self) -> &'static str {
        match self {
            Self::Track => "MusicRecording",
            Self::Album => "MusicAlbum",
            Self::Artist => "MusicGroup",
            Self::Playlist => "MusicPlaylist",
            Self::Video => "VideoObject",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub service: Service,
    pub level: Level,
    /// The service's ID for the object, as it appears in the canonical URL.
    pub id: String,
    pub canonical: String,
}
/// An ISRC as keys and facts store it: upper case without hyphens or spaces, two letters (the
/// country), three letters or digits (the registrant) and seven digits.
pub fn isrc(code: &str) -> Option<String> {
    let code: String = code
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .collect::<String>()
        .to_ascii_uppercase();
    let b = code.as_bytes();
    (b.len() == 12
        && b[..2].iter().all(u8::is_ascii_uppercase)
        && b[2..5].iter().all(u8::is_ascii_alphanumeric)
        && b[5..].iter().all(u8::is_ascii_digit))
    .then_some(code)
}
fn link(service: Service, level: Level, id: &str, canonical: String) -> Option<Link> {
    Some(Link {
        service,
        level,
        id: id.to_owned(),
        canonical,
    })
}

/// The music link a URL names, if it is one.
pub fn parse(uri: &str) -> Option<Link> {
    if let Some(rest) = uri.strip_prefix("spotify:") {
        let (kind, id) = rest.split_once(':')?;
        return spotify(kind, id);
    }
    let url = Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    match host {
        "discogs.com" => {
            let (kind, id) = discogs::of_uri(uri)?;
            let level = match kind {
                discogs::Kind::Artist => Level::Artist,
                _ => Level::Album,
            };
            link(
                Service::Discogs,
                level,
                &format!("{}/{id}", kind.as_str()),
                discogs::root(kind, id),
            )
        }
        "open.spotify.com" | "play.spotify.com" => {
            // `intl-xx` and the old `user/{name}/playlist/{id}` forms.
            let s: Vec<&str> = segments
                .iter()
                .copied()
                .skip_while(|p| p.starts_with("intl-"))
                .collect();
            match s.as_slice() {
                [kind, id] => spotify(kind, id),
                ["user", _, "playlist", id] => spotify("playlist", id),
                _ => None,
            }
        }
        "music.apple.com" => apple(&segments, query("i")),
        // The iTunes Store's URLs for the same catalogue (`…/album/{slug}/id{N}`).
        "itunes.apple.com" => {
            let mut s = segments.clone();
            let last = s.pop()?.strip_prefix("id")?;
            s.push(last);
            apple(&s, query("i"))
        }
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => match segments.as_slice() {
            ["watch"] => youtube_video(&query("v")?),
            ["shorts" | "embed" | "live", id] => youtube_video(id),
            // A channel: the artist's own page on YouTube.
            ["channel", id] if id.len() == 24 && id.starts_with("UC") && token(id, 24, 24) => link(
                Service::YouTube,
                Level::Artist,
                id,
                format!("https://www.youtube.com/channel/{id}"),
            ),
            ["playlist"] => {
                let list = query("list")?;
                if !token(&list, 8, 64) {
                    return None;
                }
                link(
                    Service::YouTube,
                    Level::Playlist,
                    &list,
                    format!("https://www.youtube.com/playlist?list={list}"),
                )
            }
            _ => None,
        },
        "youtu.be" => match segments.as_slice() {
            [id] => youtube_video(id),
            _ => None,
        },
        "deezer.com" => {
            let s = strip_locale(&segments);
            match s {
                [kind @ ("track" | "album" | "artist" | "playlist"), id] if digits(id) => link(
                    Service::Deezer,
                    level(kind)?,
                    id,
                    format!("https://www.deezer.com/{kind}/{id}"),
                ),
                _ => None,
            }
        }
        "tidal.com" | "listen.tidal.com" => {
            let s: &[&str] = match segments.as_slice() {
                ["browse", rest @ ..] => rest,
                rest => rest,
            };
            // The share form ends in `/u`.
            let s = match s {
                [rest @ .., "u"] => rest,
                rest => rest,
            };
            match s {
                [kind @ ("track" | "album" | "artist"), id] if digits(id) => link(
                    Service::Tidal,
                    level(kind)?,
                    id,
                    format!("https://tidal.com/{kind}/{id}"),
                ),
                ["video", id] if digits(id) => link(
                    Service::Tidal,
                    Level::Video,
                    id,
                    format!("https://tidal.com/video/{id}"),
                ),
                ["playlist", id] if uuid(id) => link(
                    Service::Tidal,
                    Level::Playlist,
                    id,
                    format!("https://tidal.com/playlist/{id}"),
                ),
                _ => None,
            }
        }
        "open.qobuz.com" | "play.qobuz.com" => match segments.as_slice() {
            [kind @ ("track" | "album" | "artist" | "playlist"), id] if token(id, 1, 64) => link(
                Service::Qobuz,
                level(kind)?,
                id,
                format!("https://open.qobuz.com/{kind}/{id}"),
            ),
            _ => None,
        },
        "qobuz.com" => match segments.as_slice() {
            // `/{country-language}/album/{slug}/{id}`: the store page of an album.
            [_, "album", _, id] if token(id, 1, 64) => link(
                Service::Qobuz,
                Level::Album,
                id,
                format!("https://open.qobuz.com/album/{id}"),
            ),
            _ => None,
        },
        "soundcloud.com" | "m.soundcloud.com" => match segments.as_slice() {
            [user] if soundcloud_user(user) => link(
                Service::SoundCloud,
                Level::Artist,
                &user.to_ascii_lowercase(),
                format!("https://soundcloud.com/{}", user.to_ascii_lowercase()),
            ),
            [user, "sets", set] if soundcloud_user(user) && soundcloud_name(set) => {
                let id = format!(
                    "{}/sets/{}",
                    user.to_ascii_lowercase(),
                    set.to_ascii_lowercase()
                );
                link(
                    Service::SoundCloud,
                    Level::Album,
                    &id,
                    format!("https://soundcloud.com/{id}"),
                )
            }
            [user, track]
                if soundcloud_user(user)
                    && soundcloud_name(track)
                    && !SOUNDCLOUD_PAGES.contains(track) =>
            {
                let id = format!(
                    "{}/{}",
                    user.to_ascii_lowercase(),
                    track.to_ascii_lowercase()
                );
                link(
                    Service::SoundCloud,
                    Level::Track,
                    &id,
                    format!("https://soundcloud.com/{id}"),
                )
            }
            _ => None,
        },
        _ if host
            .strip_prefix("music.amazon.")
            .is_some_and(|tld| AMAZON_MUSIC_TLDS.contains(&tld)) =>
        {
            let tld = &host["music.amazon.".len()..];
            let (kind, id) = match segments.as_slice() {
                ["albums", id] => match query("trackAsin") {
                    Some(track) => ("tracks", track),
                    None => ("albums", (*id).to_owned()),
                },
                [kind @ ("tracks" | "artists" | "playlists" | "user-playlists"), id] => {
                    (*kind, (*id).to_owned())
                }
                _ => return None,
            };
            if !token(&id, 4, 64) {
                return None;
            }
            let level = match kind {
                "tracks" => Level::Track,
                "albums" => Level::Album,
                "artists" => Level::Artist,
                _ => Level::Playlist,
            };
            link(
                Service::AmazonMusic,
                level,
                &id,
                format!("https://music.amazon.{tld}/{kind}/{id}"),
            )
        }
        _ if host.ends_with(".bandcamp.com") => {
            let artist = &host[..host.len() - ".bandcamp.com".len()];
            if artist.is_empty() || artist.contains('.') || !token(artist, 1, 64) {
                return None;
            }
            match segments.as_slice() {
                [kind @ ("track" | "album"), slug] if token(slug, 1, 128) => link(
                    Service::Bandcamp,
                    level(kind)?,
                    &format!("{artist}/{kind}/{slug}"),
                    format!("https://{artist}.bandcamp.com/{kind}/{slug}"),
                ),
                [] | ["music"] => link(
                    Service::Bandcamp,
                    Level::Artist,
                    artist,
                    format!("https://{artist}.bandcamp.com/"),
                ),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Type and name a resolved card by what its link is, whatever the service's page or oEmbed
/// says: a track is a `MusicRecording`, an album a `MusicAlbum`, an artist a `MusicGroup` (a
/// `Person` when MusicBrainz says the artist is one: `artist_type`), a playlist a
/// `MusicPlaylist`; a video stays as resolved. The card's site is the service's name ("Apple
/// Music", "TIDAL", "YouTube"), as derived links show it. A Bandcamp root (an artist page that
/// features an album) is named after the artist. A card that did not resolve is left alone.
pub fn type_card(uri: &str, artist_type: Option<&str>, doc: &mut crate::model::ResourceMeta) {
    if doc.status != crate::model::Status::Ok {
        return;
    }
    let Some(link) = parse(uri) else {
        return;
    };
    let page_site = std::mem::replace(&mut doc.site, link.service.name().into());
    // A Bandcamp root shows the artist's featured album: the card is the artist, named as the
    // page's site name says, without the album's text.
    if link.service == Service::Bandcamp && link.level == Level::Artist {
        if !page_site.trim().is_empty() {
            doc.name = Some(page_site.trim().to_owned());
        }
        doc.description = None;
    }
    match link.level {
        Level::Video => {}
        Level::Artist if artist_type == Some("Person") => doc.set_type("Person"),
        level => doc.set_type(level.type_name()),
    }
    // What the service's own page says about the object, in place of its share blurb: the
    // artist, then a track's album and length or an album's year.
    let attribute = |name: &str| {
        doc.attributes
            .get(name)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    };
    if let Some(artist) = attribute("artist") {
        let mut parts = vec![artist.to_owned()];
        match link.level {
            Level::Track => {
                parts.extend(attribute("album").map(str::to_owned));
                parts.extend(attribute("duration").and_then(length));
            }
            Level::Album => parts.extend(
                attribute("published")
                    .and_then(|d| d.get(..4))
                    .filter(|y| y.bytes().all(|b| b.is_ascii_digit()))
                    .map(str::to_owned),
            ),
            _ => {}
        }
        doc.description = Some(parts.join(" · "));
    } else if link.service == Service::Tidal
        && doc.description.as_deref().is_some_and(|d| {
            (d.starts_with("Listen to ") || d.starts_with("See the releases of "))
                && (d.ends_with(" on TIDAL") || d.contains(" on TIDAL in "))
        })
    {
        // TIDAL's share text says nothing about the object.
        doc.description = None;
    }
}
/// An ISO 8601 duration (`PT5M55S`) as a track length: `5:55`, `1:02:03`.
fn length(iso: &str) -> Option<String> {
    let secs = crate::event::time::duration(iso)?.num_seconds();
    if !(1..=86_400).contains(&secs) {
        return None;
    }
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    Some(if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    })
}

/// Amazon Music's own hosts, `music.amazon.{tld}`: any other `music.amazon.` host is someone
/// else's.
const AMAZON_MUSIC_TLDS: &[&str] = &[
    "com", "co.uk", "de", "fr", "it", "es", "ca", "com.au", "co.jp", "in", "com.br", "com.mx",
];

/// SoundCloud pages that are not tracks.
const SOUNDCLOUD_PAGES: &[&str] = &[
    "tracks",
    "albums",
    "sets",
    "reposts",
    "likes",
    "followers",
    "following",
    "popular-tracks",
    "comments",
    "spotlight",
];

/// SoundCloud's own pages at the root, never a user.
const SOUNDCLOUD_ROOTS: &[&str] = &[
    "search",
    "charts",
    "discover",
    "upload",
    "stream",
    "you",
    "settings",
    "messages",
    "notifications",
    "people",
    "pages",
    "tags",
    "stations",
    "feed",
    "signin",
    "logout",
    "connect",
    "pro",
    "jobs",
    "imprint",
    "terms-of-use",
    "mobile",
    "popular",
    "artists",
    "creators",
];

fn level(kind: &str) -> Option<Level> {
    Some(match kind {
        "track" | "song" => Level::Track,
        "album" => Level::Album,
        "artist" => Level::Artist,
        "playlist" => Level::Playlist,
        _ => return None,
    })
}
fn digits(s: &str) -> bool {
    (1..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}
fn token(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn uuid(s: &str) -> bool {
    crate::hubs::mbid(s).as_deref() == Some(s)
}
fn soundcloud_name(s: &str) -> bool {
    token(s, 1, 128)
}
/// A SoundCloud user name: not one of the site's own pages.
fn soundcloud_user(s: &str) -> bool {
    soundcloud_name(s) && !SOUNDCLOUD_ROOTS.contains(&s.to_ascii_lowercase().as_str())
}
/// Deezer's `/{lang}/track/…`: the language segment goes.
fn strip_locale<'a, 'b>(segments: &'a [&'b str]) -> &'a [&'b str] {
    match segments {
        [lang, rest @ ..]
            if rest.len() == 2
                && (2..=5).contains(&lang.len())
                && lang.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') =>
        {
            rest
        }
        _ => segments,
    }
}
fn spotify(kind: &str, id: &str) -> Option<Link> {
    // Base-62 IDs of 22 characters.
    if id.len() != 22 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    link(
        Service::Spotify,
        level(kind).filter(|_| kind != "song")?,
        id,
        format!("https://open.spotify.com/{kind}/{id}"),
    )
}
fn youtube_video(id: &str) -> Option<Link> {
    if id.len() != 11
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    link(
        Service::YouTube,
        Level::Video,
        id,
        format!("https://www.youtube.com/watch?v={id}"),
    )
}
/// The storefronts a hub may store an Apple Music link under (MusicBrainz keeps the storefront
/// an editor browsed), asked together ([`hub_spellings`]).
pub const APPLE_STOREFRONTS: &[&str] = &[
    "us", "gb", "de", "fr", "jp", "au", "ca", "es", "it", "nl", "se", "ch", "at", "br", "mx", "kr",
    "pl", "ie", "nz", "be",
];
/// An Apple Music link as a storefront spells it (`https://music.apple.com/{storefront}/{kind}/{id}`);
/// `None` for another service or a video.
pub fn apple_spelling(link: &Link, storefront: &str) -> Option<String> {
    if link.service != Service::AppleMusic {
        return None;
    }
    let kind = match link.level {
        Level::Track => "song",
        Level::Album => "album",
        Level::Artist => "artist",
        Level::Playlist => "playlist",
        Level::Video => return None,
    };
    Some(format!(
        "https://music.apple.com/{storefront}/{kind}/{}",
        link.id
    ))
}
/// The URL forms a hub may store for `uri`: an Apple Music link's storefront spellings
/// ([`APPLE_STOREFRONTS`]), any other URL itself.
pub fn hub_spellings(uri: &str) -> Vec<String> {
    parse(uri)
        .filter(|l| l.service == Service::AppleMusic)
        .map(|l| {
            APPLE_STOREFRONTS
                .iter()
                .filter_map(|sf| apple_spelling(&l, sf))
                .collect()
        })
        .filter(|s: &Vec<String>| !s.is_empty())
        .unwrap_or_else(|| vec![uri.to_owned()])
}
/// Apple Music: `/{storefront}/{kind}/{slug?}/{id}` or `/{kind}/{slug?}/{id}`, and an album URL
/// with `?i=` names one track. The catalogue ID is global, so every storefront's page is one
/// object: the canonical URL has no storefront (Apple answers it with the viewer's).
fn apple(segments: &[&str], track: Option<String>) -> Option<Link> {
    let segments = match segments {
        [country, rest @ ..]
            if country.len() == 2 && country.bytes().all(|b| b.is_ascii_lowercase()) =>
        {
            rest
        }
        rest => rest,
    };
    let [kind, rest @ ..] = segments else {
        return None;
    };
    let id = *rest.last()?;
    let id_ok = |id: &str| {
        if *kind == "playlist" {
            id.starts_with("pl.") && token(&id[3..], 1, 64)
        } else {
            digits(id)
        }
    };
    if !id_ok(id) || rest.len() > 2 {
        return None;
    }
    if let (&"album", Some(track)) = (kind, track.as_deref()) {
        if !digits(track) {
            return None;
        }
        return link(
            Service::AppleMusic,
            Level::Track,
            track,
            format!("https://music.apple.com/song/{track}"),
        );
    }
    let level = match *kind {
        "song" => Level::Track,
        "album" => Level::Album,
        "artist" => Level::Artist,
        "playlist" => Level::Playlist,
        _ => return None,
    };
    link(
        Service::AppleMusic,
        level,
        id,
        format!("https://music.apple.com/{kind}/{id}"),
    )
}

/// Discogs links (artists, masters, releases), read here as every music link is: the Discogs
/// card source and the music hubs both name them.
pub mod discogs {
    use url::Url;

    fn web_url(value: &str) -> Option<Url> {
        let url = Url::parse(value).ok()?;
        (matches!(url.scheme(), "https" | "http") && url.host_str().is_some()).then_some(url)
    }

    /// What a Discogs link names.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Kind {
        Artist,
        /// An album: every edition of it (a MusicBrainz release group).
        Master,
        /// One edition (a MusicBrainz release).
        Release,
    }
    impl Kind {
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Artist => "artist",
                Self::Master => "master",
                Self::Release => "release",
            }
        }
    }

    /// The kind and ID of `www.discogs.com/{lang/}?{artist,master,release}/{id}[-slug]`.
    pub fn of_uri(uri: &str) -> Option<(Kind, u64)> {
        let url = web_url(uri)?;
        if !matches!(url.host_str()?, "www.discogs.com" | "discogs.com") {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        // A language path (`de`, `pt_BR`) before the kind.
        let language = |s: &str| {
            let (lang, region) = s.split_once('_').unwrap_or((s, "AA"));
            lang.len() == 2
                && lang.bytes().all(|b| b.is_ascii_lowercase())
                && region.len() == 2
                && region.bytes().all(|b| b.is_ascii_uppercase())
        };
        let segments = match segments.as_slice() {
            [lang, rest @ ..] if language(lang) => rest,
            all => all,
        };
        let [kind, id] = segments else {
            return None;
        };
        let kind = match *kind {
            "artist" => Kind::Artist,
            "master" => Kind::Master,
            "release" => Kind::Release,
            _ => return None,
        };
        let id = id.split('-').next()?;
        if id.is_empty()
            || id.len() > 12
            || id.starts_with('0')
            || !id.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Some((kind, id.parse().ok()?))
    }

    /// The one address of a Discogs object.
    pub fn root(kind: Kind, id: u64) -> String {
        format!("https://www.discogs.com/{}/{id}", kind.as_str())
    }

    /// The kind and ID of a music link on Discogs.
    pub fn of_link(link: &super::Link) -> Option<(Kind, u64)> {
        (link.service == super::Service::Discogs)
            .then(|| of_uri(&link.canonical))
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isrcs_have_one_form() {
        assert_eq!(isrc("gb-um7-10-29604").as_deref(), Some("GBUM71029604"));
        assert_eq!(isrc("USIR20400274").as_deref(), Some("USIR20400274"));
        for bad in [
            "12UM71029604",
            "GBUM7102960",
            "GBUM7102960X",
            "GBUM710296045",
        ] {
            assert_eq!(isrc(bad), None, "{bad}");
        }
    }
    fn canonical(uri: &str) -> Option<String> {
        parse(uri).map(|l| l.canonical)
    }

    #[test]
    fn every_spelling_of_one_object_has_one_canonical_url() {
        let cases: &[(&str, &str, Level)] = &[
            (
                "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq?si=abc",
                "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
                Level::Track,
            ),
            (
                "https://open.spotify.com/intl-de/album/6dVIqQ8qmQ5GBnJ9shOYGE",
                "https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE",
                Level::Album,
            ),
            (
                "spotify:artist:4Z8W4fKeB5YxbusRsdQVPb",
                "https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb",
                Level::Artist,
            ),
            (
                "https://open.spotify.com/user/someone/playlist/37i9dQZF1DXcBWIGoYBM5M",
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
                Level::Playlist,
            ),
            (
                "https://music.apple.com/gb/album/ok-computer/1097861387?i=1097861770&ls=1",
                "https://music.apple.com/song/1097861770",
                Level::Track,
            ),
            (
                "https://music.apple.com/gb/album/ok-computer/1097861387",
                "https://music.apple.com/album/1097861387",
                Level::Album,
            ),
            (
                "https://music.apple.com/us/song/paranoid-android/1097861770",
                "https://music.apple.com/song/1097861770",
                Level::Track,
            ),
            (
                "https://music.apple.com/us/artist/radiohead/657515",
                "https://music.apple.com/artist/657515",
                Level::Artist,
            ),
            // Any storefront, and none: one object.
            (
                "https://music.apple.com/ch/song/bohemian-rhapsody/6781024437",
                "https://music.apple.com/song/6781024437",
                Level::Track,
            ),
            (
                "https://music.apple.com/album/1097861387?i=1097861770",
                "https://music.apple.com/song/1097861770",
                Level::Track,
            ),
            (
                "https://music.apple.com/playlist/pl.u-8aAVZaJcb3Lvj8",
                "https://music.apple.com/playlist/pl.u-8aAVZaJcb3Lvj8",
                Level::Playlist,
            ),
            (
                "https://youtu.be/dQw4w9WgXcQ?si=x",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                Level::Video,
            ),
            (
                "https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                Level::Video,
            ),
            (
                "https://m.youtube.com/watch?v=dQw4w9WgXcQ&t=42",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                Level::Video,
            ),
            (
                "https://music.youtube.com/playlist?list=OLAK5uy_nc6afxSnmK8OFML8dF9q_0D1MBMXsGnL4",
                "https://www.youtube.com/playlist?list=OLAK5uy_nc6afxSnmK8OFML8dF9q_0D1MBMXsGnL4",
                Level::Playlist,
            ),
            (
                "https://www.deezer.com/fr/track/138539973",
                "https://www.deezer.com/track/138539973",
                Level::Track,
            ),
            (
                "https://deezer.com/album/14879699",
                "https://www.deezer.com/album/14879699",
                Level::Album,
            ),
            (
                "https://listen.tidal.com/track/58990512",
                "https://tidal.com/track/58990512",
                Level::Track,
            ),
            (
                "https://tidal.com/browse/album/58990511?u",
                "https://tidal.com/album/58990511",
                Level::Album,
            ),
            (
                "https://tidal.com/track/534050211/u",
                "https://tidal.com/track/534050211",
                Level::Track,
            ),
            (
                "https://tidal.com/browse/artist/64518/u",
                "https://tidal.com/artist/64518",
                Level::Artist,
            ),
            (
                "https://tidal.com/video/155006578/u",
                "https://tidal.com/video/155006578",
                Level::Video,
            ),
            (
                "https://itunes.apple.com/gb/album/id1237917085",
                "https://music.apple.com/album/1237917085",
                Level::Album,
            ),
            (
                "https://itunes.apple.com/us/album/cut-to-the-feeling/id1237917085?i=1237917090&uo=4",
                "https://music.apple.com/song/1237917090",
                Level::Track,
            ),
            (
                "https://www.qobuz.com/gb-en/album/ok-computer-radiohead/0634904078164",
                "https://open.qobuz.com/album/0634904078164",
                Level::Album,
            ),
            (
                "https://open.qobuz.com/track/12345",
                "https://open.qobuz.com/track/12345",
                Level::Track,
            ),
            (
                "https://m.soundcloud.com/Radiohead/paranoid-android?in=x",
                "https://soundcloud.com/radiohead/paranoid-android",
                Level::Track,
            ),
            (
                "https://soundcloud.com/radiohead/sets/ok-computer",
                "https://soundcloud.com/radiohead/sets/ok-computer",
                Level::Album,
            ),
            (
                "https://music.amazon.co.uk/albums/B01DPSNU78?trackAsin=B01DPSS5LY&do=play",
                "https://music.amazon.co.uk/tracks/B01DPSS5LY",
                Level::Track,
            ),
            (
                "https://music.amazon.com/albums/B01DPSNU78",
                "https://music.amazon.com/albums/B01DPSNU78",
                Level::Album,
            ),
            (
                "https://radiohead.bandcamp.com/album/ok-computer?from=x",
                "https://radiohead.bandcamp.com/album/ok-computer",
                Level::Album,
            ),
        ];
        for (uri, expected, level) in cases {
            let l = parse(uri).unwrap_or_else(|| panic!("{uri}"));
            assert_eq!(
                (l.canonical.as_str(), l.level),
                (*expected, *level),
                "{uri}"
            );
            // The canonical URL is its own canonical form.
            assert_eq!(
                canonical(expected).as_deref(),
                Some(*expected),
                "{expected}"
            );
        }
    }

    /// A hub stores an Apple Music link under a storefront: the spellings asked are one per
    /// storefront, each a spelling of the one object; any other link is asked as it is.
    #[test]
    fn an_apple_link_has_a_spelling_per_storefront() {
        let spellings = hub_spellings("https://music.apple.com/song/1097861770");
        assert_eq!(spellings.len(), APPLE_STOREFRONTS.len());
        assert!(spellings.contains(&"https://music.apple.com/gb/song/1097861770".to_owned()));
        assert!(spellings.contains(&"https://music.apple.com/us/song/1097861770".to_owned()));
        assert!(spellings
            .iter()
            .all(|s| canonical(s).as_deref() == Some("https://music.apple.com/song/1097861770")));
        assert_eq!(
            hub_spellings("https://music.apple.com/ch/album/x/1097861387"),
            hub_spellings("https://music.apple.com/album/1097861387")
        );
        assert_eq!(
            hub_spellings("https://tidal.com/track/58990512"),
            ["https://tidal.com/track/58990512"]
        );
        assert_eq!(
            hub_spellings("https://example.org/x"),
            ["https://example.org/x"]
        );
    }

    #[test]
    fn a_youtube_channel_is_an_artist_page() {
        let l = parse("https://music.youtube.com/channel/UCq19-LqvG35A-30oyAiPiqA").unwrap();
        assert_eq!(
            (l.level, l.canonical.as_str()),
            (
                Level::Artist,
                "https://www.youtube.com/channel/UCq19-LqvG35A-30oyAiPiqA"
            )
        );
        assert!(parse("https://www.youtube.com/channel/short").is_none());
    }

    #[test]
    fn cards_are_typed_by_their_level() {
        use crate::model::{ResourceMeta, Status};
        let card = |uri: &str, kind: &str, status: Status| {
            let mut d = ResourceMeta::new(uri, "web", 3600);
            d.set_type(kind);
            d.status = status;
            type_card(uri, None, &mut d);
            d.type_name
        };
        // Apple Music's JSON-LD calls a song a MusicComposition; Spotify's oEmbed a video.
        assert_eq!(
            card(
                "https://music.apple.com/us/song/1097861770",
                "MusicComposition",
                Status::Ok
            ),
            "MusicRecording"
        );
        assert_eq!(
            card(
                "https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE",
                "VideoObject",
                Status::Ok
            ),
            "MusicAlbum"
        );
        assert_eq!(
            card("https://www.deezer.com/artist/399", "WebPage", Status::Ok),
            "MusicGroup"
        );
        assert_eq!(
            card(
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
                "WebPage",
                Status::Ok
            ),
            "MusicPlaylist"
        );
        assert_eq!(
            card(
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                "VideoObject",
                Status::Ok
            ),
            "VideoObject"
        );
        assert_eq!(
            card(
                "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
                "WebPage",
                Status::Missing
            ),
            "WebPage"
        );
        assert_eq!(
            card("https://example.org/", "WebPage", Status::Ok),
            "WebPage"
        );
        // The service's name is the card's site, a video's too.
        let mut d = ResourceMeta::new("https://www.youtube.com/watch?v=dQw4w9WgXcQ", "media", 3600);
        d.site = "www.youtube.com".into();
        d.set_type("VideoObject");
        type_card("https://www.youtube.com/watch?v=dQw4w9WgXcQ", None, &mut d);
        assert_eq!(
            (d.type_name.as_str(), d.site.as_str()),
            ("VideoObject", "YouTube")
        );
        // A Bandcamp root features an album: the card is the artist.
        let mut d = ResourceMeta::new("https://radiohead.bandcamp.com/", "web", 3600);
        d.set_type("MusicAlbum");
        d.name = Some("OK Computer".into());
        d.description = Some("The album's notes".into());
        d.site = "Radiohead".into();
        type_card("https://radiohead.bandcamp.com/", None, &mut d);
        assert_eq!(
            (d.type_name.as_str(), d.name.as_deref(), d.site.as_str()),
            ("MusicGroup", Some("Radiohead"), "Bandcamp")
        );
        assert!(d.description.is_none());
        // A solo artist, as MusicBrainz says.
        let mut d = ResourceMeta::new("https://www.deezer.com/artist/1", "web", 3600);
        type_card("https://www.deezer.com/artist/1", Some("Person"), &mut d);
        assert_eq!(d.type_name, "Person");
    }

    #[test]
    fn other_pages_and_malformed_ids_are_not_music_links() {
        for uri in [
            "https://open.spotify.com/track/short",
            "https://open.spotify.com/episode/6LgJvl0Xdtc73RJ1mmpotq",
            "https://open.spotify.com/",
            "spotify:track:../../x",
            "https://spotify.link/abc",
            "https://music.apple.com/gb/browse",
            "https://music.apple.com/gbr/album/x/1",
            "https://music.apple.com/album/x/1/2/3",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/@radiohead",
            "https://www.deezer.com/en/track/abc",
            "https://deezer.page.link/xyz",
            "https://on.soundcloud.com/abc",
            "https://soundcloud.com/radiohead/likes",
            "https://soundcloud.com/radiohead/a/b/c",
            "https://soundcloud.com/search/sounds?q=radiohead",
            "https://soundcloud.com/charts/top",
            "https://soundcloud.com/discover",
            "https://soundcloud.com/Upload",
            "https://evil.bandcamp.com.example/album/x",
            "https://a.b.bandcamp.com/album/x",
            "https://music.amazon.com/search/x",
            "https://music.amazon.evil.com/tracks/B0ABCDEFGH",
            "https://music.amazon.com.evil.example/tracks/B0ABCDEFGH",
            "https://music.amazon.co/tracks/B0ABCDEFGH",
            "ftp://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
        ] {
            assert!(parse(uri).is_none(), "{uri}");
        }
    }
}
