//! A card's `embed` (docs/api.md, Embed): the page a client may load in a sandboxed frame to show
//! the link the way its own site does (a service's player, a social post, a map, a slide deck),
//! with what the frame needs to be sized and allowed. Only the frame's address is kept, never a
//! provider's HTML.
//!
//! Three sources, the first that answers wins:
//! 1. [`link`]: the services whose player address follows from the link alone (YouTube, Vimeo,
//!    Spotify, SoundCloud, Mixcloud, TIDAL, Deezer, Apple Music, Apple Podcasts) and X posts; read
//!    when a card is served, so every stored card has it. [`mastodon`] and [`map`] are the same for
//!    a Mastodon post's card and an OpenStreetMap card.
//! 2. [`from_oembed`]: the `<iframe>` of a provider's oEmbed answer, stored with the card.
//! 3. [`from_meta`]: the page's own player (`twitter:player`, an HTML `og:video`), stored with the
//!    card.
//!
//! Every address passes [`safe_src`]: `https` on a public name, nothing that carries a credential.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use url::Url;
use utoipa::ToSchema;

/// What a frame shows (docs/api.md, Embed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EmbedKind {
    /// A player of audio: a track, an album, a podcast or its episode, a mix.
    Audio,
    /// A player of video.
    Video,
    /// A social post, as its network shows it.
    Post,
    /// A map.
    Map,
    /// Anything else a provider embeds: slides, a document, code, a chart.
    Rich,
}

/// The frame to load for a link (docs/api.md, Embed).
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
pub struct Embed {
    /// Who serves the frame: a service Pinakes knows (`youtube`, `vimeo`, `spotify`,
    /// `soundcloud`, `mixcloud`, `tidal`, `deezer`, `apple-music`, `apple-podcasts`, `x`,
    /// `mastodon`, `openstreetmap`), else the oEmbed provider's name in lower case with hyphens
    /// (`speaker-deck`), else the frame's host.
    pub provider: String,
    pub kind: EmbedKind,
    /// The frame's address: `https`, on a public host name, without a credential. It never starts
    /// playing by itself (see `autoplay_param`).
    pub src: String,
    /// The frame's height in CSS pixels, for a frame of fixed height (a player strip, a post, a
    /// document); null when it scales with its width (`aspect`) or the provider gives none.
    pub height: Option<u32>,
    /// Width divided by height, for a frame that scales with its width (a video, a map, a slide
    /// deck); null otherwise.
    pub aspect: Option<f64>,
    /// The Permissions Policy features the frame may use, for its `allow` attribute (joined with
    /// `; `), from `accelerometer`, `autoplay`, `clipboard-write`, `encrypted-media`,
    /// `fullscreen`, `gyroscope`, `picture-in-picture` and `web-share`.
    pub allow: Vec<String>,
    /// A query parameter (`autoplay=1`, `auto_play=true`) that makes the player start once
    /// loaded: add it to `src` only when the viewer pressed play. Null when the player has none.
    pub autoplay_param: Option<String>,
}

/// The Permissions Policy features an embed may ask for; anything else is dropped.
const FEATURES: &[&str] = &[
    "accelerometer",
    "autoplay",
    "clipboard-write",
    "encrypted-media",
    "fullscreen",
    "gyroscope",
    "picture-in-picture",
    "web-share",
];
/// The features a video or audio player gets unless its service asks for others.
const PLAYER_ALLOW: &[&str] = &[
    "accelerometer",
    "autoplay",
    "clipboard-write",
    "encrypted-media",
    "gyroscope",
    "picture-in-picture",
    "web-share",
];
/// The longest frame address kept.
const SRC_MAX: usize = 2048;
/// The tallest frame kept, in CSS pixels.
const HEIGHT_MAX: u32 = 2000;
/// A video's or map's shape when its provider names none.
const WIDESCREEN: f64 = 16.0 / 9.0;

impl Embed {
    fn new(provider: &str, kind: EmbedKind, src: String, allow: &[&str]) -> Self {
        Self {
            provider: provider.into(),
            kind,
            src,
            height: None,
            aspect: None,
            allow: allow.iter().map(|s| (*s).to_owned()).collect(),
            autoplay_param: None,
        }
    }
    fn height(mut self, height: u32) -> Self {
        self.height = Some(height);
        self
    }
    fn aspect(mut self, aspect: f64) -> Self {
        self.aspect = Some(aspect);
        self
    }
    fn autoplay(mut self, param: &str) -> Self {
        self.autoplay_param = Some(param.into());
        self
    }
    /// The embed within its bounds, or `None`: a safe `src` ([`safe_src`]), a provider name of
    /// 1–64 bytes, a height of 1–2000, an aspect between 0.2 and 5, known features only, an
    /// autoplay parameter of the form `name=value` in letters, digits and `_`.
    pub fn bounded(mut self) -> Option<Self> {
        self.src = safe_src(&self.src)?;
        let provider_ok = (1..=64).contains(&self.provider.len())
            && self
                .provider
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.".contains(&b));
        self.height = self.height.filter(|h| (1..=HEIGHT_MAX).contains(h));
        self.aspect = self
            .aspect
            .filter(|a| a.is_finite() && (0.2..=5.0).contains(a))
            .map(|a| (a * 10_000.0).round() / 10_000.0);
        self.allow.retain(|f| FEATURES.contains(&f.as_str()));
        self.allow.sort();
        self.allow.dedup();
        self.autoplay_param = self.autoplay_param.filter(|p| {
            p.len() <= 32
                && p.split_once('=').is_some_and(|(k, v)| {
                    !k.is_empty()
                        && !v.is_empty()
                        && (k.bytes().chain(v.bytes()))
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
        });
        provider_ok.then_some(self)
    }
    /// The embed as a card of `type_path` shows it: a stored frame of fixed height on an audio
    /// card (a podcast's or a song's) is its player, one on a social post is the post; a player
    /// has a height and no aspect, a video or map an aspect (16:9 when none is known) and no
    /// height, any other frame one of them.
    pub fn settled(mut self, type_path: &[String]) -> Self {
        let is = |types: &[&str]| type_path.iter().any(|t| types.contains(&t.as_str()));
        if matches!(self.kind, EmbedKind::Rich | EmbedKind::Video)
            && self.height.is_some()
            && is(AUDIO)
        {
            self.kind = EmbedKind::Audio;
        } else if self.kind == EmbedKind::Rich && is(POSTS) {
            self.kind = EmbedKind::Post;
        }
        match self.kind {
            EmbedKind::Audio | EmbedKind::Post => self.aspect = None,
            EmbedKind::Video | EmbedKind::Map => {
                self.height = None;
                self.aspect = self.aspect.or(Some(WIDESCREEN));
            }
            EmbedKind::Rich => {
                if self.aspect.is_some() {
                    self.height = None;
                }
            }
        }
        if self.kind == EmbedKind::Audio && self.height.is_none() {
            self.height = Some(152);
        }
        self
    }
}
/// The card types whose player is audio.
const AUDIO: &[&str] = &[
    "AudioObject",
    "Audiobook",
    "MusicAlbum",
    "MusicGroup",
    "MusicPlaylist",
    "MusicRecording",
    "PodcastEpisode",
    "PodcastSeries",
    "RadioEpisode",
    "RadioSeries",
];
/// The card types that are someone's post.
const POSTS: &[&str] = &["SocialMediaPosting", "DiscussionForumPosting", "Comment"];

/// `value` as a frame's address: `https`, no user or password, no port, a host name with a dot
/// that is no IP address, `localhost` or a private suffix (`.local`, `.localdomain`, `.internal`,
/// `.lan`, `.home.arpa`) and is under a public suffix (the Public Suffix List: not `router.corp`),
/// no credential anywhere in it, a link in its query included
/// (`capability::names_capability`), at most 2,048 bytes. `javascript:`,
/// `data:` and every other scheme are refused.
pub fn safe_src(value: &str) -> Option<String> {
    let url = Url::parse(value.trim()).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || crate::capability::names_capability(&url)
    {
        return None;
    }
    let url::Host::Domain(host) = url.host()? else {
        return None;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if !host.contains('.')
        || host == "localhost"
        || [
            ".localhost",
            ".local",
            ".localdomain",
            ".internal",
            ".lan",
            ".home.arpa",
        ]
        .iter()
        .any(|s| host.ends_with(s))
        || host.parse::<std::net::IpAddr>().is_ok()
        // A name under no public suffix (`router.corp`, `printer.home`) is a network's own.
        || !psl::domain(host.as_bytes()).is_some_and(|d| d.suffix().is_known())
    {
        return None;
    }
    let src = String::from(url);
    (src.len() <= SRC_MAX).then_some(src)
}

/// `value` when it is an `https` link without a user, password or port on one of `hosts`.
fn https_on(value: &str, hosts: &[&str]) -> Option<Url> {
    let url = Url::parse(value.trim()).ok()?;
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && hosts.contains(&url.host_str()?.to_ascii_lowercase().as_str()))
    .then_some(url)
}
fn segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}
fn chars_in(s: &str, len: std::ops::RangeInclusive<usize>, ok: impl Fn(u8) -> bool) -> bool {
    len.contains(&s.len()) && s.bytes().all(ok)
}
fn digits(s: &str, max: usize) -> bool {
    chars_in(s, 1..=max, |b| b.is_ascii_digit())
}
fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}
/// `link` as a query value, as JavaScript's `encodeURIComponent` writes it.
fn component(link: &str) -> String {
    const KEEP: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    percent_encoding::utf8_percent_encode(link, KEEP).to_string()
}

/// The embed of a link on a service whose player follows from the link alone, or of an X post.
pub fn link(uri: &str) -> Option<Embed> {
    [
        youtube,
        vimeo,
        spotify,
        soundcloud,
        mixcloud,
        tidal,
        deezer,
        apple_music,
        apple_podcasts,
        x_post,
    ]
    .iter()
    .find_map(|adapter| adapter(uri))
    .and_then(Embed::bounded)
}

fn youtube_id(s: &str) -> bool {
    chars_in(s, 11..=11, |b| {
        b.is_ascii_alphanumeric() || b"_-".contains(&b)
    })
}
/// A video, a playlist (`/playlist?list=`) or a channel by its ID (`/channel/UC…`, its videos
/// or home tab: its uploads, playlist `UU…`), on youtube.com, its www., m. and music. hosts and
/// youtu.be.
fn youtube(uri: &str) -> Option<Embed> {
    let url = https_on(
        uri,
        &[
            "youtube.com",
            "www.youtube.com",
            "m.youtube.com",
            "music.youtube.com",
            "youtu.be",
        ],
    )?;
    let player = |path: String| {
        Embed::new(
            "youtube",
            EmbedKind::Video,
            format!("https://www.youtube-nocookie.com/embed/{path}"),
            PLAYER_ALLOW,
        )
        .aspect(WIDESCREEN)
        .autoplay("autoplay=1")
    };
    let path = segments(&url);
    if url.host_str() == Some("youtu.be") {
        return match path.as_slice() {
            [id] if youtube_id(id) => Some(player(format!("{id}?enablejsapi=1&rel=0"))),
            _ => None,
        };
    }
    let list = |id: &str| {
        chars_in(id, 10..=64, |b| {
            b.is_ascii_alphanumeric() || b"_-".contains(&b)
        })
    };
    match path.as_slice() {
        ["playlist"] => query(&url, "list")
            .filter(|l| list(l))
            .map(|l| player(format!("videoseries?list={l}&enablejsapi=1&rel=0"))),
        ["channel", channel] | ["channel", channel, "videos" | "featured"] => channel
            .strip_prefix("UC")
            .filter(|id| {
                chars_in(id, 22..=22, |b| {
                    b.is_ascii_alphanumeric() || b"_-".contains(&b)
                })
            })
            .map(|id| player(format!("videoseries?list=UU{id}&enablejsapi=1&rel=0"))),
        ["watch"] => query(&url, "v")
            .filter(|id| youtube_id(id))
            .map(|id| player(format!("{id}?enablejsapi=1&rel=0"))),
        ["embed" | "shorts" | "live" | "v", id] if youtube_id(id) => {
            Some(player(format!("{id}?enablejsapi=1&rel=0")))
        }
        _ => None,
    }
}
/// A video on vimeo.com (also in a channel, group or album) or its player.
fn vimeo(uri: &str) -> Option<Embed> {
    let url = https_on(uri, &["vimeo.com", "www.vimeo.com", "player.vimeo.com"])?;
    let name = |s: &str| {
        chars_in(s, 1..=100, |b| {
            b.is_ascii_alphanumeric() || b"_-".contains(&b)
        })
    };
    let player = url.host_str() == Some("player.vimeo.com");
    let id = match segments(&url).as_slice() {
        ["video", id] if player => *id,
        [id] if !player => *id,
        ["channels", c, id] if !player && name(c) => *id,
        ["groups", g, "videos", id] if !player && name(g) => *id,
        ["album", a, "video", id] if !player && digits(a, 12) => *id,
        _ => return None,
    };
    digits(id, 12).then(|| {
        Embed::new(
            "vimeo",
            EmbedKind::Video,
            format!("https://player.vimeo.com/video/{id}?dnt=1"),
            PLAYER_ALLOW,
        )
        .aspect(WIDESCREEN)
        .autoplay("autoplay=1")
    })
}
/// A track, album, artist, playlist, show or episode (also under an `intl-xx` prefix). Spotify's
/// documented heights: 152 for one track or episode, 352 for the rest.
fn spotify(uri: &str) -> Option<Embed> {
    let url = https_on(uri, &["open.spotify.com"])?;
    let path = segments(&url);
    let path = match path.split_first() {
        Some((first, rest)) if intl(first) => rest,
        _ => &path[..],
    };
    let [kind, id] = path else {
        return None;
    };
    let kind = kind.to_ascii_lowercase();
    if !["track", "album", "artist", "playlist", "show", "episode"].contains(&kind.as_str())
        || !chars_in(id, 22..=22, |b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    let height = if matches!(kind.as_str(), "track" | "episode") {
        152
    } else {
        352
    };
    Some(
        Embed::new(
            "spotify",
            EmbedKind::Audio,
            format!("https://open.spotify.com/embed/{kind}/{id}"),
            PLAYER_ALLOW,
        )
        .height(height),
    )
}
/// `intl-de`, `intl-pt-br`.
fn intl(segment: &str) -> bool {
    segment.strip_prefix("intl-").is_some_and(|code| {
        let code = code.to_ascii_lowercase();
        let parts: Vec<&str> = code.split('-').collect();
        matches!(parts.len(), 1 | 2)
            && parts
                .iter()
                .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_lowercase()))
    })
}
/// Top-level SoundCloud pages that are no profile.
const SOUNDCLOUD_RESERVED: &[&str] = &[
    "discover",
    "search",
    "stream",
    "you",
    "upload",
    "charts",
    "pages",
    "stations",
    "mobile",
    "settings",
    "notifications",
    "messages",
    "feed",
    "signin",
    "signup",
    "logout",
    "terms-of-use",
    "popular",
    "people",
    "pro",
    "jobs",
    "imprint",
    "premium",
    "artists",
    "creators",
];
/// A profile's tabs, never a track.
const SOUNDCLOUD_TABS: &[&str] = &[
    "tracks",
    "albums",
    "sets",
    "reposts",
    "likes",
    "followers",
    "following",
    "popular-tracks",
    "comments",
];
/// A track, a set or a profile (whose tracks the widget plays as a list): 166 px for a track,
/// 450 for a list.
fn soundcloud(uri: &str) -> Option<Embed> {
    let url = https_on(
        uri,
        &["soundcloud.com", "www.soundcloud.com", "m.soundcloud.com"],
    )?;
    let slug = |s: &str, max| {
        chars_in(s, 1..=max, |b| {
            b.is_ascii_alphanumeric() || b"_-".contains(&b)
        })
    };
    let path = segments(&url);
    let (user, sets, track) = match path.as_slice() {
        [user] => (*user, false, None),
        [user, "sets", set] => (*user, true, Some(*set)),
        [user, track] => (*user, false, Some(*track)),
        _ => return None,
    };
    if !(slug(user, 64) && user.len() >= 2)
        || SOUNDCLOUD_RESERVED.contains(&user.to_ascii_lowercase().as_str())
        || track.is_some_and(|t| !slug(t, 128))
        || (!sets
            && track.is_some_and(|t| SOUNDCLOUD_TABS.contains(&t.to_ascii_lowercase().as_str())))
    {
        return None;
    }
    let link = match track {
        Some(t) if sets => format!("https://soundcloud.com/{user}/sets/{t}"),
        Some(t) => format!("https://soundcloud.com/{user}/{t}"),
        None => format!("https://soundcloud.com/{user}"),
    };
    Some(
        Embed::new(
            "soundcloud",
            EmbedKind::Audio,
            format!(
                "https://w.soundcloud.com/player/?url={}&visual=false&show_teaser=false",
                component(&link)
            ),
            PLAYER_ALLOW,
        )
        .height(if sets || track.is_none() { 450 } else { 166 })
        .autoplay("auto_play=true"),
    )
}
/// Top-level Mixcloud pages that are no account.
const MIXCLOUD_RESERVED: &[&str] = &[
    "discover",
    "upload",
    "live",
    "select",
    "settings",
    "search",
    "categories",
    "widget",
];
/// A show (`/{account}/{show}/`). The widget stays paused until pressed inside, whatever its
/// `autoplay` says, so it has no autoplay parameter.
/// A Mixcloud profile's tabs (`/{account}/uploads/`), which are no show.
const MIXCLOUD_TABS: &[&str] = &[
    "activity",
    "favorites",
    "followers",
    "following",
    "groups",
    "hosts",
    "listens",
    "playlists",
    "reposts",
    "stream",
    "uploads",
];
fn mixcloud(uri: &str) -> Option<Embed> {
    let url = https_on(uri, &["www.mixcloud.com", "mixcloud.com", "m.mixcloud.com"])?;
    let part = |s: &str, max| {
        chars_in(s, 1..=max, |b| {
            b.is_ascii_alphanumeric() || b"_-".contains(&b)
        })
    };
    let [account, show] = segments(&url)[..] else {
        return None;
    };
    if !part(account, 64)
        || !part(show, 160)
        || MIXCLOUD_RESERVED.contains(&account.to_ascii_lowercase().as_str())
        || MIXCLOUD_TABS.contains(&show.to_ascii_lowercase().as_str())
    {
        return None;
    }
    let link = format!("https://www.mixcloud.com/{account}/{show}/");
    Some(
        Embed::new(
            "mixcloud",
            EmbedKind::Audio,
            format!(
                "https://www.mixcloud.com/widget/iframe/?feed={}&hide_cover=1",
                component(&link)
            ),
            PLAYER_ALLOW,
        )
        .height(120),
    )
}
/// A track, album, video or playlist (also `/browse/…` and a share link's `/u`). TIDAL's oEmbed
/// heights: 120 for a track, 275 for a list.
fn tidal(uri: &str) -> Option<Embed> {
    let url = https_on(uri, &["tidal.com", "www.tidal.com", "listen.tidal.com"])?;
    let path = segments(&url);
    let path = match path.as_slice() {
        ["browse", rest @ ..] => rest,
        all => all,
    };
    let (kind, id) = match path {
        [kind, id] | [kind, id, "u"] => (kind.to_ascii_lowercase(), id.to_ascii_lowercase()),
        _ => return None,
    };
    let id_ok = digits(&id, 12)
        || chars_in(&id, 36..=36, |b| {
            b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-'
        });
    if !["track", "album", "video", "playlist"].contains(&kind.as_str()) || !id_ok {
        return None;
    }
    let embed = Embed::new(
        "tidal",
        EmbedKind::Audio,
        format!("https://embed.tidal.com/{kind}s/{id}"),
        &[
            "autoplay",
            "clipboard-write",
            "encrypted-media",
            "fullscreen",
            "web-share",
        ],
    );
    Some(match kind.as_str() {
        "video" => Embed {
            kind: EmbedKind::Video,
            ..embed
        }
        .aspect(WIDESCREEN),
        "track" => embed.height(120),
        _ => embed.height(275),
    })
}
/// A track, album, playlist or artist (its top tracks), also under a language prefix. The
/// widget's own sizes: 150 for a track, 300 for a list.
fn deezer(uri: &str) -> Option<Embed> {
    let url = https_on(uri, &["www.deezer.com", "deezer.com"])?;
    let path = segments(&url);
    let path = match path.split_first() {
        Some((first, rest)) if language(first) => rest,
        _ => &path[..],
    };
    let [kind, id] = path else {
        return None;
    };
    let kind = kind.to_ascii_lowercase();
    if !["track", "album", "playlist", "artist"].contains(&kind.as_str()) || !digits(id, 15) {
        return None;
    }
    let top = if kind == "artist" { "/top_tracks" } else { "" };
    Some(
        Embed::new(
            "deezer",
            EmbedKind::Audio,
            format!("https://widget.deezer.com/widget/dark/{kind}/{id}{top}"),
            &["clipboard-write", "encrypted-media"],
        )
        .height(if kind == "track" { 150 } else { 300 }),
    )
}
/// `fr`, `pt-br`.
fn language(segment: &str) -> bool {
    let code = segment.to_ascii_lowercase();
    let parts: Vec<&str> = code.split('-').collect();
    matches!(parts.len(), 1 | 2)
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_lowercase()))
}
/// What Apple's embed code asks for.
const APPLE_ALLOW: &[&str] = &[
    "autoplay",
    "clipboard-write",
    "encrypted-media",
    "fullscreen",
];
fn apple_id(s: &str) -> bool {
    digits(s, 15)
}
/// A storefront (`us` when the link names none) and the rest of the path.
fn storefront<'a>(path: &'a [&'a str]) -> (String, &'a [&'a str]) {
    match path.split_first() {
        Some((first, rest))
            if first.len() == 2 && first.bytes().all(|b| b.is_ascii_alphabetic()) =>
        {
            (first.to_ascii_lowercase(), rest)
        }
        _ => ("us".into(), path),
    }
}
/// A song (also an album's song, `?i=`), an album or a playlist, with or without its name in the
/// path. 175 px for a song, 450 for a list; played by a press inside it.
fn apple_music(uri: &str) -> Option<Embed> {
    let url = https_on(uri, &["music.apple.com"])?;
    let path = segments(&url);
    let (store, rest) = storefront(&path);
    let (kind, id) = match rest {
        [kind, id] | [kind, _, id] => (kind.to_ascii_lowercase(), *id),
        _ => return None,
    };
    let playlist_id = |s: &str| {
        s.strip_prefix("pl.")
            .is_some_and(|p| chars_in(p, 1..=64, |b| b.is_ascii_alphanumeric() || b == b'-'))
    };
    if !["song", "album", "playlist"].contains(&kind.as_str()) || !(apple_id(id) || playlist_id(id))
    {
        return None;
    }
    let song = query(&url, "i").filter(|s| apple_id(s));
    let (kind, id) = match song {
        Some(song) if kind == "album" => ("song".to_owned(), song),
        _ => (kind, id.to_owned()),
    };
    if (kind == "playlist") != id.starts_with("pl.") {
        return None;
    }
    Some(
        Embed::new(
            "apple-music",
            EmbedKind::Audio,
            format!("https://embed.music.apple.com/{store}/{kind}/{id}"),
            APPLE_ALLOW,
        )
        .height(if kind == "song" { 175 } else { 450 }),
    )
}
/// A show (`/podcast/{name}/id{n}`), or one of its episodes (`?i=`); played by a press inside it.
/// 175 px for both: Apple's taller show layout leaves an empty band on a phone.
fn apple_podcasts(uri: &str) -> Option<Embed> {
    let url = https_on(uri, &["podcasts.apple.com"])?;
    let path = segments(&url);
    let (store, rest) = storefront(&path);
    let show = match rest {
        ["podcast", id] | ["podcast", _, id] => id.strip_prefix("id").filter(|s| apple_id(s))?,
        _ => return None,
    };
    let src = match query(&url, "i").filter(|s| apple_id(s)) {
        Some(episode) => format!("https://embed.podcasts.apple.com/{store}/episode/{episode}"),
        None => format!("https://embed.podcasts.apple.com/{store}/podcast/{show}"),
    };
    Some(Embed::new("apple-podcasts", EmbedKind::Audio, src, APPLE_ALLOW).height(175))
}
/// A post on x.com or twitter.com (their www. and mobile. hosts): X's own post frame.
fn x_post(uri: &str) -> Option<Embed> {
    let url = https_on(
        uri,
        &[
            "x.com",
            "www.x.com",
            "mobile.x.com",
            "twitter.com",
            "www.twitter.com",
            "mobile.twitter.com",
        ],
    )?;
    let id = match segments(&url).as_slice() {
        ["i", "web", "status", id, ..] => *id,
        [_, "status", id, ..] => *id,
        _ => return None,
    };
    digits(id, 20).then(|| {
        Embed::new(
            "x",
            EmbedKind::Post,
            format!("https://platform.twitter.com/embed/Tweet.html?id={id}&dnt=true"),
            &[],
        )
    })
}

/// A Mastodon post's own frame (`/@{account}/{id}/embed`) on the server it was posted on: for a
/// card the `mastodon` or `activitypub` source read as a post at `https://{host}/@{account}/{id}`
/// or `/users/{account}/statuses/{id}`. A post seen through another server
/// (`/@{account}@{server}/{id}`) has none.
pub fn mastodon(uri: &str) -> Option<Embed> {
    let url = Url::parse(uri).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let local = |a: &str| {
        chars_in(a, 1..=64, |b| {
            b.is_ascii_alphanumeric() || b"_.-".contains(&b)
        })
    };
    let (account, id) = match segments(&url).as_slice() {
        [at, id] => (at.strip_prefix('@').filter(|a| local(a))?, *id),
        ["users", account, "statuses", id] if local(account) => (*account, *id),
        _ => return None,
    };
    if url.scheme() != "https" || url.port().is_some() || !digits(id, 20) {
        return None;
    }
    Embed::new(
        "mastodon",
        EmbedKind::Post,
        format!("https://{host}/@{account}/{id}/embed"),
        &[],
    )
    .bounded()
}

/// OpenStreetMap's own embedded map of a place, with a marker at its point: its bounding box
/// (`[west, south, east, north]`) when it has one, else the streets around the point.
pub fn map(point: &crate::model::Point, bbox: Option<[f64; 4]>) -> Option<Embed> {
    let valid = |lat: f64, lon: f64| {
        lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0
    };
    if !valid(point.lat, point.lon) {
        return None;
    }
    let [w, s, e, n] = bbox
        .filter(|[w, s, e, n]| valid(*s, *w) && valid(*n, *e) && w < e && s < n)
        .unwrap_or([
            point.lon - 0.006,
            point.lat - 0.003,
            point.lon + 0.006,
            point.lat + 0.003,
        ]);
    let src = format!(
        "https://www.openstreetmap.org/export/embed.html?bbox={w:.5}%2C{s:.5}%2C{e:.5}%2C{n:.5}&layer=mapnik&marker={:.5}%2C{:.5}",
        point.lat, point.lon
    );
    Embed::new("openstreetmap", EmbedKind::Map, src, &[])
        .aspect(WIDESCREEN)
        .bounded()
}

/// A number of CSS pixels as a provider writes it (`"152"`, `152`, `"152px"`); none for
/// `"100%"` and the like.
fn pixels(v: Option<&Value>) -> Option<u32> {
    let n = match v? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().trim_end_matches("px").trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (n.is_finite() && n >= 1.0 && n <= f64::from(HEIGHT_MAX) * 4.0).then(|| n.round() as u32)
}
/// A provider's name as an embed names it: `Speaker Deck` is `speaker-deck`.
fn slug(name: &str) -> Option<String> {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if c == '.' && !out.is_empty() {
            out.push('.');
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches(['-', '.']).to_owned();
    (!out.is_empty() && out.len() <= 64).then_some(out)
}
/// The oEmbed answers' HTML this reads at most; a longer one is left alone.
const OEMBED_HTML_MAX: usize = 16 * 1024;

/// The frame of an oEmbed answer (`v`) of type `video` or `rich`: the `src` of the first
/// `<iframe>` in its `html`, or the `data-embed-url` of a Mastodon-style `<blockquote>`, resolved
/// against `base`; its size from the iframe's `width` and `height`, else the answer's; its
/// features from the iframe's `allow` (and `allowfullscreen`), and never started by itself
/// (`quiet`). The HTML itself is never kept.
pub fn from_oembed(v: &Value, base: &Url) -> Option<Embed> {
    let kind = match v.get("type").and_then(Value::as_str) {
        Some("video") => EmbedKind::Video,
        Some("rich") => EmbedKind::Rich,
        _ => return None,
    };
    let html = v.get("html").and_then(Value::as_str)?;
    if html.len() > OEMBED_HTML_MAX {
        return None;
    }
    let frame = first_frame(html)?;
    let src = frame_src(base, &frame.src)?;
    let width = frame
        .width
        .as_deref()
        .and_then(|w| pixels(Some(&Value::String(w.into()))))
        .or_else(|| pixels(v.get("width")));
    let height = frame
        .height
        .as_deref()
        .and_then(|h| pixels(Some(&Value::String(h.into()))))
        .or_else(|| pixels(v.get("height")));
    // A name Pinakes gives a service of its own (`KNOWN`) is that service's alone: an answer that
    // claims it for a frame on another host is named by its frame's host.
    let host = Url::parse(&src).ok()?.host_str()?.to_owned();
    let provider = v
        .get("provider_name")
        .and_then(Value::as_str)
        .and_then(slug)
        .filter(|p| {
            KNOWN
                .iter()
                .find(|(name, _)| *name == p.as_str())
                .is_none_or(|(_, hosts)| hosts.contains(&host.as_str()))
        })
        .unwrap_or(host);
    let mut embed = Embed::new(&provider, kind, src, &[]);
    embed.allow = frame.allow;
    match (width, height) {
        // A frame that names its width scales with it; one that fills its box keeps its height.
        (Some(w), Some(h)) => {
            embed.aspect = Some(f64::from(w) / f64::from(h));
            if kind == EmbedKind::Rich {
                embed.height = Some(h);
            }
        }
        (None, Some(h)) => embed.height = Some(h),
        _ => {}
    }
    if kind == EmbedKind::Video {
        embed
            .allow
            .extend(PLAYER_ALLOW.iter().map(|s| (*s).to_owned()));
    }
    quiet(embed).bounded()
}
/// The provider names of the services whose frames follow from their links (`link`, `mastodon`,
/// `map`), each with the hosts of its own frames: an answer's name is taken only for a frame on
/// one of them (SoundCloud's answer for a short link), never for another host. Any server can
/// be a Mastodon one, so that name is never taken.
const KNOWN: &[(&str, &[&str])] = &[
    (
        "youtube",
        &["www.youtube.com", "youtube.com", "www.youtube-nocookie.com"],
    ),
    ("vimeo", &["player.vimeo.com"]),
    ("spotify", &["open.spotify.com"]),
    ("soundcloud", &["w.soundcloud.com"]),
    (
        "mixcloud",
        &["www.mixcloud.com", "player-widget.mixcloud.com"],
    ),
    ("tidal", &["embed.tidal.com"]),
    ("deezer", &["widget.deezer.com"]),
    ("apple-music", &["embed.music.apple.com"]),
    ("apple-podcasts", &["embed.podcasts.apple.com"]),
    ("x", &["platform.twitter.com"]),
    ("mastodon", &[]),
    ("openstreetmap", &["www.openstreetmap.org"]),
];
/// A frame address a page or an oEmbed answer gives (`raw`), resolved against `base` and safe
/// ([`safe_src`]); never an empty or fragment-only one, which would name `base` itself.
fn frame_src(base: &Url, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let mut url = base.join(raw).ok()?;
    let mut page = base.clone();
    page.set_fragment(None);
    let fragment = url.fragment().map(str::to_owned);
    url.set_fragment(None);
    if url == page {
        return None;
    }
    url.set_fragment(fragment.as_deref());
    safe_src(url.as_str())
}
/// The query parameters that start a player once loaded.
const AUTOPLAY_PARAMS: &[&str] = &["autoplay", "auto_play", "autostart"];
/// A stored frame that never starts by itself: an autoplay parameter its address sets on
/// (`autoplay=1`, `autoplay=true`) is turned off there and becomes its `autoplay_param`, for a
/// press on play; without one, the frame may not autoplay at all (no `autoplay` in `allow`).
fn quiet(mut embed: Embed) -> Embed {
    let Ok(mut url) = Url::parse(&embed.src) else {
        return embed;
    };
    let mut started = None;
    let query = url.query().and_then(|q| quiet_pairs(q, &mut started));
    // A player that reads its fragment is turned off there too; Play sets only the query.
    let fragment = url.fragment().and_then(|f| quiet_pairs(f, &mut None));
    if query.is_some() || fragment.is_some() {
        if let Some(query) = query {
            url.set_query(Some(&query));
        }
        if let Some(fragment) = fragment {
            url.set_fragment(Some(&fragment));
        }
        embed.src = url.into();
    }
    embed.autoplay_param = started;
    if embed.autoplay_param.is_none() {
        embed.allow.retain(|f| f != "autoplay");
    }
    embed
}
/// `part` (a query or a fragment) with every autoplay parameter (`AUTOPLAY_PARAMS`, any case)
/// that is not already off (`0`, `false`, `no`, `off`) turned off, a bare flag included; pairs
/// split on `&` or `;`, and every other pair left as written (a signed query stays valid). The
/// first one turned off is kept in `started` as `name=value` (`name=true` for a bare flag).
/// `None` when nothing was on.
fn quiet_pairs(part: &str, started: &mut Option<String>) -> Option<String> {
    let mut out = String::with_capacity(part.len() + 8);
    let mut changed = false;
    for piece in part.split_inclusive(['&', ';']) {
        let (pair, end) = match piece.char_indices().last() {
            Some((i, '&' | ';')) => piece.split_at(i),
            _ => (piece, ""),
        };
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |s: &str| {
            url::form_urlencoded::parse(format!("x={s}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        let (key, value) = (decode(raw_key), decode(raw_value).to_ascii_lowercase());
        let on = AUTOPLAY_PARAMS.contains(&key.to_ascii_lowercase().as_str())
            && !matches!(value.as_str(), "0" | "false" | "no" | "off");
        if !on {
            out.push_str(piece);
            continue;
        }
        let off = match value.as_str() {
            "1" => "0",
            "on" => "off",
            "true" | "yes" | "" => "false",
            _ => "0",
        };
        started.get_or_insert_with(|| {
            let shown = decode(raw_value);
            format!("{key}={}", if shown.is_empty() { "true" } else { &shown })
        });
        out.push_str(raw_key);
        out.push('=');
        out.push_str(off);
        out.push_str(end);
        changed = true;
    }
    changed.then_some(out)
}
struct Frame {
    src: String,
    width: Option<String>,
    height: Option<String>,
    allow: Vec<String>,
}
/// The first `<iframe src>` of an oEmbed answer's HTML, else the first `data-embed-url` (a
/// Mastodon-style quote). Read by a scan of its start tags, not parsed: the answer is read on the
/// runtime, where no HTML is parsed (`resolver::html`), and what is taken is only an address that
/// [`safe_src`] still judges.
fn first_frame(html: &str) -> Option<Frame> {
    let mut quoted = None;
    for (name, attrs) in StartTags::new(html).take(256) {
        let attr = |key: &str| attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        if name == "iframe" {
            let Some(src) = attr("src") else { continue };
            let mut allow: Vec<String> = attr("allow")
                .unwrap_or_default()
                .split(';')
                .filter_map(|d| d.split_whitespace().next())
                .map(str::to_ascii_lowercase)
                .collect();
            if attr("allowfullscreen").is_some() {
                allow.push("fullscreen".into());
            }
            let mut seen = std::collections::BTreeSet::new();
            allow.retain(|f| seen.insert(f.clone()));
            return Some(Frame {
                src,
                width: attr("width"),
                height: attr("height"),
                allow,
            });
        }
        if quoted.is_none() {
            quoted = attr("data-embed-url");
        }
    }
    Some(Frame {
        src: quoted?,
        width: None,
        height: None,
        allow: vec![],
    })
}
/// The start tags of an HTML fragment, each its lowercase name and its attributes (lowercase
/// names, values with character references decoded); comments, end tags and declarations skipped.
struct StartTags<'a> {
    rest: &'a str,
}
impl<'a> StartTags<'a> {
    fn new(html: &'a str) -> Self {
        let mut end = html.len().min(16 * 1024);
        while !html.is_char_boundary(end) {
            end -= 1;
        }
        Self { rest: &html[..end] }
    }
}
impl Iterator for StartTags<'_> {
    type Item = (String, Vec<(String, String)>);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let at = self.rest.find('<')?;
            self.rest = &self.rest[at + 1..];
            if let Some(after) = self.rest.strip_prefix("!--") {
                self.rest = after.find("-->").map_or("", |e| &after[e + 3..]);
                continue;
            }
            if !self.rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
                continue;
            }
            let name_end = self
                .rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(self.rest.len());
            let name = self.rest[..name_end].to_ascii_lowercase();
            self.rest = &self.rest[name_end..];
            let mut attrs = vec![];
            loop {
                self.rest = self
                    .rest
                    .trim_start_matches(|c: char| c.is_whitespace() || c == '/');
                if self.rest.is_empty() {
                    return Some((name, attrs));
                }
                if let Some(after) = self.rest.strip_prefix('>') {
                    self.rest = after;
                    return Some((name, attrs));
                }
                let key_end = self
                    .rest
                    .find(|c: char| c.is_whitespace() || matches!(c, '=' | '>' | '/'))
                    .unwrap_or(self.rest.len())
                    .max(1);
                let key = self.rest[..key_end].to_ascii_lowercase();
                self.rest = self.rest[key_end..].trim_start();
                let mut value = String::new();
                if let Some(after) = self.rest.strip_prefix('=') {
                    let after = after.trim_start();
                    let (raw, rest) = match after.chars().next() {
                        Some(q @ ('"' | '\'')) => {
                            let body = &after[1..];
                            let end = body.find(q).unwrap_or(body.len());
                            (&body[..end], body.get(end + 1..).unwrap_or(""))
                        }
                        _ => {
                            let end = after
                                .find(|c: char| c.is_whitespace() || c == '>')
                                .unwrap_or(after.len());
                            (&after[..end], &after[end..])
                        }
                    };
                    value = decode_refs(raw);
                    self.rest = rest;
                }
                // The first `ATTRS` of a tag count: a duplicate check over more would cost a
                // hostile answer's tag a quadratic scan.
                if attrs.len() < ATTRS && !attrs.iter().any(|(k, _): &(String, String)| *k == key) {
                    attrs.push((key, value));
                }
            }
        }
    }
}
/// The attributes of one start tag `StartTags` keeps.
const ATTRS: usize = 64;
/// An attribute value with its character references decoded: the named ones an address carries
/// (`&amp;`, `&quot;`, `&apos;`, `&lt;`, `&gt;`) and numeric ones; any other is left as written.
/// A reference's `;` is looked for in its next 10 bytes only.
fn decode_refs(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.as_bytes()[1..rest.len().min(11)]
            .iter()
            .position(|b| *b == b';')
            .map(|e| e + 1)
        else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let ch = match name {
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "lt" => Some('<'),
            "gt" => Some('>'),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .map(|h| u32::from_str_radix(h, 16))
                .or_else(|| name.strip_prefix('#').map(str::parse::<u32>))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The page's own player from its head `<meta>` (`meta`, keys in lower case), resolved against
/// `base`: `twitter:player` with its `:width` and `:height`, else an `og:video:secure_url` or
/// `og:video:url` (or `og:video`) whose `og:video:type` is `text/html`, with its size.
pub fn from_meta(meta: &HashMap<String, String>, base: &Url) -> Option<Embed> {
    let (src, prefix) = if let Some(src) = meta.get("twitter:player") {
        (src, "twitter:player")
    } else {
        let html = meta
            .get("og:video:type")
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/html"));
        let src = ["og:video:secure_url", "og:video:url", "og:video"]
            .iter()
            .find_map(|k| meta.get(*k))
            .filter(|_| html)?;
        (src, "og:video")
    };
    let src = frame_src(base, src)?;
    let size = |name: &str| {
        pixels(
            meta.get(&format!("{prefix}:{name}"))
                .map(|s| Value::String(s.clone()))
                .as_ref(),
        )
    };
    let host = Url::parse(&src)
        .ok()?
        .host_str()?
        .trim_start_matches("www.")
        .to_owned();
    let mut embed = Embed::new(&host, EmbedKind::Video, src, PLAYER_ALLOW);
    if let (Some(w), Some(h)) = (size("width"), size("height")) {
        embed.aspect = Some(f64::from(w) / f64::from(h));
        embed.height = Some(h);
    }
    quiet(embed).bounded()
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn src(uri: &str) -> Option<String> {
        link(uri).map(|e| e.src)
    }

    /// The players the app built from links, same addresses, without what starts them.
    #[test]
    fn players_follow_from_the_link() {
        let cases = [
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ?enablejsapi=1&rel=0",
            ),
            (
                "https://youtu.be/dQw4w9WgXcQ",
                "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ?enablejsapi=1&rel=0",
            ),
            (
                "https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share",
                "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ?enablejsapi=1&rel=0",
            ),
            (
                "https://www.youtube.com/shorts/dQw4w9WgXcQ",
                "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ?enablejsapi=1&rel=0",
            ),
            (
                "https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
                "https://www.youtube-nocookie.com/embed/videoseries?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI&enablejsapi=1&rel=0",
            ),
            (
                "https://www.youtube.com/channel/UC_kRDKYrUlrbtrSiyu5Tflg",
                "https://www.youtube-nocookie.com/embed/videoseries?list=UU_kRDKYrUlrbtrSiyu5Tflg&enablejsapi=1&rel=0",
            ),
            (
                "https://vimeo.com/76979871",
                "https://player.vimeo.com/video/76979871?dnt=1",
            ),
            (
                "https://vimeo.com/channels/staffpicks/76979871",
                "https://player.vimeo.com/video/76979871?dnt=1",
            ),
            (
                "https://open.spotify.com/track/003vvx7Niy0yvhvHt4a68B",
                "https://open.spotify.com/embed/track/003vvx7Niy0yvhvHt4a68B",
            ),
            (
                "https://open.spotify.com/intl-de/album/4m2880jivSbbyEGAKfITCa",
                "https://open.spotify.com/embed/album/4m2880jivSbbyEGAKfITCa",
            ),
            (
                "https://soundcloud.com/radiohead",
                "https://w.soundcloud.com/player/?url=https%3A%2F%2Fsoundcloud.com%2Fradiohead&visual=false&show_teaser=false",
            ),
            (
                "https://soundcloud.com/user-1/sets/a-set",
                "https://w.soundcloud.com/player/?url=https%3A%2F%2Fsoundcloud.com%2Fuser-1%2Fsets%2Fa-set&visual=false&show_teaser=false",
            ),
            (
                "https://www.mixcloud.com/spartacus/party-time/",
                "https://www.mixcloud.com/widget/iframe/?feed=https%3A%2F%2Fwww.mixcloud.com%2Fspartacus%2Fparty-time%2F&hide_cover=1",
            ),
            (
                "https://tidal.com/track/551040646/u",
                "https://embed.tidal.com/tracks/551040646",
            ),
            (
                "https://listen.tidal.com/browse/album/199574241",
                "https://embed.tidal.com/albums/199574241",
            ),
            (
                "https://www.deezer.com/fr/track/3135556",
                "https://widget.deezer.com/widget/dark/track/3135556",
            ),
            (
                "https://www.deezer.com/artist/27",
                "https://widget.deezer.com/widget/dark/artist/27/top_tracks",
            ),
            (
                "https://music.apple.com/gb/album/ok-computer/1097861387?i=1097862062",
                "https://embed.music.apple.com/gb/song/1097862062",
            ),
            (
                "https://music.apple.com/song/617154366",
                "https://embed.music.apple.com/us/song/617154366",
            ),
            (
                "https://music.apple.com/us/playlist/todays-hits/pl.f4d106fed2bd41149aaacabb233eb5eb",
                "https://embed.music.apple.com/us/playlist/pl.f4d106fed2bd41149aaacabb233eb5eb",
            ),
            (
                "https://podcasts.apple.com/us/podcast/acquired/id1050462261",
                "https://embed.podcasts.apple.com/us/podcast/1050462261",
            ),
            (
                "https://podcasts.apple.com/us/podcast/the-home-depot/id1050462261?i=1000789467893",
                "https://embed.podcasts.apple.com/us/episode/1000789467893",
            ),
            (
                "https://x.com/jack/status/20",
                "https://platform.twitter.com/embed/Tweet.html?id=20&dnt=true",
            ),
            (
                "https://twitter.com/i/web/status/20",
                "https://platform.twitter.com/embed/Tweet.html?id=20&dnt=true",
            ),
        ];
        for (uri, want) in cases {
            assert_eq!(src(uri).as_deref(), Some(want), "{uri}");
        }
    }

    #[test]
    fn players_carry_their_size_features_and_start() {
        let track = link("https://open.spotify.com/track/003vvx7Niy0yvhvHt4a68B").unwrap();
        assert_eq!(
            (
                track.provider.as_str(),
                track.kind,
                track.height,
                track.aspect
            ),
            ("spotify", EmbedKind::Audio, Some(152), None)
        );
        assert_eq!(track.autoplay_param, None);
        let artist = link("https://open.spotify.com/artist/1dfeR4HaWDbWqFHLkxsg1d").unwrap();
        assert_eq!(artist.height, Some(352));
        let video = link("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(
            (
                video.kind,
                video.height,
                video.aspect,
                video.autoplay_param.as_deref()
            ),
            (EmbedKind::Video, None, Some(1.7778), Some("autoplay=1"))
        );
        assert!(
            video.allow.contains(&"autoplay".into()),
            "{:?}",
            video.allow
        );
        let cloud = link("https://soundcloud.com/radiohead/creep").unwrap();
        assert_eq!(
            (cloud.height, cloud.autoplay_param.as_deref()),
            (Some(166), Some("auto_play=true"))
        );
        assert_eq!(
            link("https://soundcloud.com/radiohead").unwrap().height,
            Some(450)
        );
        let deezer = link("https://www.deezer.com/track/3135556").unwrap();
        assert_eq!(deezer.allow, ["clipboard-write", "encrypted-media"]);
        assert_eq!(deezer.height, Some(150));
        let tidal = link("https://tidal.com/browse/video/12345").unwrap();
        assert_eq!((tidal.kind, tidal.aspect), (EmbedKind::Video, Some(1.7778)));
        let post = link("https://x.com/jack/status/20").unwrap();
        assert_eq!((post.provider.as_str(), post.kind), ("x", EmbedKind::Post));
        let episode = link(
            "https://podcasts.apple.com/us/podcast/the-home-depot/id1050462261?i=1000789467893",
        )
        .unwrap();
        assert_eq!(
            (episode.provider.as_str(), episode.height),
            ("apple-podcasts", Some(175))
        );
    }

    #[test]
    fn other_pages_on_those_services_have_no_player() {
        for uri in [
            "http://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/@handle",
            "https://www.youtube.com:8443/watch?v=dQw4w9WgXcQ",
            "https://user:pw@www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://vimeo.com/about",
            "https://open.spotify.com/user/someone",
            "https://open.spotify.com/track/short",
            "https://soundcloud.com/discover",
            "https://soundcloud.com/radiohead/likes",
            "https://www.mixcloud.com/discover/jazz/",
            "https://www.mixcloud.com/spartacus/uploads/",
            "https://www.mixcloud.com/spartacus/favorites/",
            "https://tidal.com/artist/7429906",
            "https://www.deezer.com/profile/1",
            "https://music.apple.com/us/artist/657515",
            "https://music.apple.com/us/playlist/x/123",
            "https://podcasts.apple.com/us/genre/podcasts/id26",
            "https://x.com/jack",
            "https://x.com/jack/status/notanumber",
            "https://example.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(src(uri), None, "{uri}");
        }
    }

    #[test]
    fn a_frame_address_is_https_on_a_public_name_without_a_credential() {
        for bad in [
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "http://player.example.com/embed",
            "https://user:pw@player.example.com/embed",
            "https://player.example.com:8443/embed",
            "https://127.0.0.1/embed",
            "https://[::1]/embed",
            "https://localhost/embed",
            "https://router.local/embed",
            "https://intranet/embed",
            "https://cal.example.com/embed?access_token=abc",
            "https://player.example.com/embed?url=https%3A%2F%2Fsite.example%2Ffeed%3Ftoken%3DSECRET",
            "https://localhost.localdomain/embed",
            "https://box.localdomain./embed",
        ] {
            assert_eq!(safe_src(bad), None, "{bad}");
        }
        assert_eq!(
            safe_src("https://Player.Example.com/embed/1"),
            Some("https://player.example.com/embed/1".into())
        );
        let long = format!("https://player.example.com/{}", "a".repeat(2100));
        assert_eq!(safe_src(&long), None);
    }

    #[test]
    fn a_mastodon_post_on_its_own_server() {
        assert_eq!(
            mastodon("https://mastodon.social/@Gargron/1").map(|e| e.src),
            Some("https://mastodon.social/@Gargron/1/embed".into())
        );
        assert_eq!(
            mastodon("https://mastodon.social/users/Gargron/statuses/109").map(|e| e.src),
            Some("https://mastodon.social/@Gargron/109/embed".into())
        );
        assert_eq!(
            mastodon("https://mastodon.social/@Gargron@example.org/1"),
            None
        );
        assert_eq!(mastodon("https://mastodon.social/@Gargron"), None);
        assert_eq!(mastodon("http://mastodon.social/@Gargron/1"), None);
    }

    #[test]
    fn a_place_gets_openstreetmaps_map() {
        let point = crate::model::Point {
            lat: 46.00371,
            lon: 8.95106,
        };
        let embed = map(&point, None).unwrap();
        assert_eq!(embed.provider, "openstreetmap");
        assert_eq!(embed.kind, EmbedKind::Map);
        assert_eq!(
            embed.src,
            "https://www.openstreetmap.org/export/embed.html?bbox=8.94506%2C46.00071%2C8.95706%2C46.00671&layer=mapnik&marker=46.00371%2C8.95106"
        );
        let boxed = map(&point, Some([8.9, 45.9, 9.0, 46.1])).unwrap();
        assert!(
            boxed
                .src
                .contains("bbox=8.90000%2C45.90000%2C9.00000%2C46.10000"),
            "{}",
            boxed.src
        );
        let nan = crate::model::Point {
            lat: f64::NAN,
            lon: 0.0,
        };
        assert_eq!(map(&nan, None), None);
    }

    #[test]
    fn an_oembed_answer_gives_its_iframe_never_its_html() {
        let base = Url::parse("https://speakerdeck.com/oembed.json").unwrap();
        let deck = json!({"type": "rich", "provider_name": "Speaker Deck", "width": 710, "height": 532,
            "html": "<iframe id=\"talk_frame_16816\" class=\"speakerdeck-iframe\" src=\"https://speakerdeck.com/player/4e79b461c9bdcb003f00331d\" width=\"710\" height=\"532\" allowfullscreen=\"allowfullscreen\"></iframe><script>alert(1)</script>"});
        let embed = from_oembed(&deck, &base).unwrap();
        assert_eq!(embed.provider, "speaker-deck");
        assert_eq!(
            embed.src,
            "https://speakerdeck.com/player/4e79b461c9bdcb003f00331d"
        );
        assert_eq!((embed.kind, embed.aspect), (EmbedKind::Rich, Some(1.3346)));
        assert_eq!(embed.allow, ["fullscreen"]);
        let settled = embed.settled(&["Thing".into(), "CreativeWork".into()]);
        assert_eq!((settled.height, settled.aspect), (None, Some(1.3346)));

        let podcast = json!({"type": "rich", "provider_name": "Podbean", "height": 150,
            "html": "<iframe src=\"https://www.podbean.com/player-v2/?i=abc\" width=\"100%\" height=\"150\" allow=\"autoplay; camera; microphone\"></iframe>"});
        let embed = from_oembed(
            &podcast,
            &Url::parse("https://api.podbean.com/v1/oembed").unwrap(),
        )
        .unwrap();
        assert_eq!((embed.height, embed.aspect), (Some(150), None));
        // Camera and microphone are never allowed, nor autoplay without a parameter to start it.
        assert!(embed.allow.is_empty(), "{:?}", embed.allow);
        let episode = embed.settled(&[
            "Thing".into(),
            "CreativeWork".into(),
            "PodcastEpisode".into(),
        ]);
        assert_eq!(
            (episode.kind, episode.height),
            (EmbedKind::Audio, Some(150))
        );

        let toot = json!({"type": "rich", "provider_name": "mastodon.social", "width": 400,
            "html": "<blockquote class=\"mastodon-embed\" data-embed-url=\"https://mastodon.social/@Mastodon/117466985005297414/embed\"><a href=\"https://mastodon.social/@Mastodon/117466985005297414\">Post</a></blockquote><script async src=\"https://mastodon.social/embed.js\"></script>"});
        let embed = from_oembed(
            &toot,
            &Url::parse("https://mastodon.social/api/oembed").unwrap(),
        )
        .unwrap();
        assert_eq!(
            embed.src,
            "https://mastodon.social/@Mastodon/117466985005297414/embed"
        );
        assert_eq!(embed.provider, "mastodon.social");
        assert_eq!(
            embed
                .settled(&[
                    "Thing".into(),
                    "CreativeWork".into(),
                    "SocialMediaPosting".into()
                ])
                .kind,
            EmbedKind::Post
        );

        let video = json!({"type": "video", "provider_name": "TED", "width": 560, "height": 315,
            "html": "<iframe src=\"https://embed.ted.com/talks/sir_ken_robinson\" width=\"560\" height=\"315\"></iframe>"});
        let embed = from_oembed(
            &video,
            &Url::parse("https://www.ted.com/services/v1/oembed.json").unwrap(),
        )
        .unwrap()
        .settled(&[]);
        assert_eq!(
            (embed.kind, embed.height, embed.aspect),
            (EmbedKind::Video, None, Some(1.7778))
        );

        for refused in [
            json!({"type": "rich", "html": "<iframe src=\"javascript:alert(1)\"></iframe>"}),
            json!({"type": "rich", "html": "<iframe src=\"http://player.example.com/x\"></iframe>"}),
            json!({"type": "rich", "html": "<iframe src=\"https://10.0.0.1/x\"></iframe>"}),
            json!({"type": "rich", "html": "<blockquote>No frame</blockquote>"}),
            json!({"type": "photo", "html": "<iframe src=\"https://player.example.com/x\"></iframe>"}),
            json!({"type": "link"}),
        ] {
            assert_eq!(from_oembed(&refused, &base), None, "{refused}");
        }
    }

    #[test]
    fn a_page_names_its_own_player() {
        let base = Url::parse("https://artist.example/album/a").unwrap();
        let meta: HashMap<String, String> = [
            (
                "twitter:player",
                "https://player.example.com/EmbeddedPlayer/album=1/",
            ),
            ("twitter:player:width", "350"),
            ("twitter:player:height", "467"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let embed = from_meta(&meta, &base).unwrap();
        assert_eq!(embed.provider, "player.example.com");
        assert_eq!(
            embed.src,
            "https://player.example.com/EmbeddedPlayer/album=1/"
        );
        let album =
            embed
                .clone()
                .settled(&["Thing".into(), "CreativeWork".into(), "MusicAlbum".into()]);
        assert_eq!(
            (album.kind, album.height, album.aspect),
            (EmbedKind::Audio, Some(467), None)
        );
        let video = embed.settled(&["Thing".into(), "CreativeWork".into(), "VideoObject".into()]);
        assert_eq!(
            (video.kind, video.height, video.aspect),
            (EmbedKind::Video, None, Some(0.7495))
        );

        let og: HashMap<String, String> = [
            (
                "og:video:url",
                "https://www.dailymotion.com/embed/video/x8abc",
            ),
            ("og:video:type", "text/html"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        assert_eq!(
            from_meta(&og, &base).map(|e| e.src),
            Some("https://www.dailymotion.com/embed/video/x8abc".into())
        );
        let file: HashMap<String, String> = [
            ("og:video:url", "https://cdn.example.com/clip.mp4"),
            ("og:video:type", "video/mp4"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        assert_eq!(from_meta(&file, &base), None, "a file is no frame");
        let unsafe_player: HashMap<String, String> = [(
            "twitter:player".to_owned(),
            "javascript:alert(1)".to_owned(),
        )]
        .into();
        assert_eq!(from_meta(&unsafe_player, &base), None);
    }

    /// The answer's HTML is scanned for its frame: comments, other tags, quoting and character
    /// references as a browser reads them.
    #[test]
    fn an_answers_frame_is_read_from_its_start_tags() {
        let html = r#"<!-- <iframe src="https://comment.example/x"> --><div class=a><IFRAME
            title='Deck' SRC='https://speakerdeck.com/player/abc?x=1&amp;y=2' width=710 height="399"
            allow="encrypted-media; fullscreen 'self'" allowfullscreen></iframe></div>"#;
        let f = first_frame(html).unwrap();
        assert_eq!(f.src, "https://speakerdeck.com/player/abc?x=1&y=2");
        assert_eq!(
            (f.width.as_deref(), f.height.as_deref()),
            (Some("710"), Some("399"))
        );
        assert_eq!(f.allow, ["encrypted-media", "fullscreen"]);
        let quote = r#"<blockquote class="mastodon-embed" data-embed-url="https://a.example/@b/1/embed">x</blockquote><script src="https://a.example/embed.js"></script>"#;
        assert_eq!(
            first_frame(quote).unwrap().src,
            "https://a.example/@b/1/embed"
        );
        for broken in [
            "",
            "<",
            "<iframe",
            "<iframe src",
            "<iframe src=\"https://a.example/x",
            "&#xZZ;<p a=&#1114112;>",
        ] {
            let _ = first_frame(broken);
        }
        assert_eq!(decode_refs("a&amp;b&#38;c&#x26;d&nope;&"), "a&b&c&d&nope;&");
        assert_eq!(decode_refs("&aaaaaaaaaaaa;&amp;"), "&aaaaaaaaaaaa;&");
        // Attributes past the first `ATTRS` of a tag are not read.
        let many: String = (0..ATTRS).map(|i| format!(" a{i}=1")).collect();
        assert!(first_frame(&format!("<iframe{many} src=\"https://a.example/1\">")).is_none());
        let few: String = (0..ATTRS - 1).map(|i| format!(" a{i}=1")).collect();
        assert_eq!(
            first_frame(&format!("<iframe{few} src=\"https://a.example/1\">"))
                .unwrap()
                .src,
            "https://a.example/1"
        );
    }

    /// A stored frame never starts by itself: an autoplay its address turns on is turned off and
    /// kept for a press; without one, the frame may not autoplay at all.
    #[test]
    fn a_stored_frame_never_starts_by_itself() {
        let base = Url::parse("https://www.twitch.tv/videos/1").unwrap();
        let meta: HashMap<String, String> = [
            (
                "og:video:secure_url",
                "https://player.twitch.tv/?video=1&parent=meta.tag&autoplay=true",
            ),
            ("og:video:type", "text/html"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let embed = from_meta(&meta, &base).unwrap();
        assert_eq!(
            embed.src,
            "https://player.twitch.tv/?video=1&parent=meta.tag&autoplay=false"
        );
        assert_eq!(embed.autoplay_param.as_deref(), Some("autoplay=true"));
        assert!(embed.allow.contains(&"autoplay".into()));
        // Only that pair is rewritten: the rest of the query stays as written.
        let signed = json!({"type": "video", "provider_name": "Clips",
            "html": "<iframe src=\"https://clips.example.com/v?t=a%20b+c&p=/x/y&Autoplay=1&r=AB%2FC\"></iframe>"});
        let embed = from_oembed(&signed, &base).unwrap();
        assert_eq!(
            embed.src,
            "https://clips.example.com/v?t=a%20b+c&p=/x/y&Autoplay=0&r=AB%2FC"
        );
        assert_eq!(embed.autoplay_param.as_deref(), Some("Autoplay=1"));
        // Every form is turned off: other values, a bare flag, `;` pairs, the fragment.
        for (raw, quiet, param) in [
            ("v?autoplay=on", "v?autoplay=off", Some("autoplay=on")),
            (
                "v?a=1;autostart",
                "v?a=1;autostart=false",
                Some("autostart=true"),
            ),
            (
                "v?auto_play=muted&x=1",
                "v?auto_play=0&x=1",
                Some("auto_play=muted"),
            ),
            (
                "v?autoplay=0#autoplay=true",
                "v?autoplay=0#autoplay=false",
                None,
            ),
            ("v?autoplay=false", "v?autoplay=false", None),
        ] {
            let v = json!({"type": "video", "provider_name": "Clips",
                "html": format!("<iframe src=\"https://clips.example.com/{raw}\"></iframe>")});
            let embed = from_oembed(&v, &base).unwrap();
            assert_eq!(
                embed.src,
                format!("https://clips.example.com/{quiet}"),
                "{raw}"
            );
            assert_eq!(embed.autoplay_param.as_deref(), param, "{raw}");
        }
        let deck = json!({"type": "video", "provider_name": "Clips",
            "html": "<iframe src=\"https://clips.example.com/embed/1\"></iframe>"});
        let embed = from_oembed(&deck, &base).unwrap();
        assert_eq!(embed.autoplay_param, None);
        assert!(
            !embed.allow.contains(&"autoplay".into()),
            "{:?}",
            embed.allow
        );
    }

    /// An answer cannot take a service's name, and an empty frame address is no frame.
    #[test]
    fn an_answer_names_no_service_of_pinakes_and_no_empty_frame() {
        let base = Url::parse("https://pages.example.com/oembed").unwrap();
        let spoof = json!({"type": "rich", "provider_name": "Spotify",
            "html": "<iframe src=\"https://player.pages.example.com/1\" height=\"152\"></iframe>"});
        assert_eq!(
            from_oembed(&spoof, &base).unwrap().provider,
            "player.pages.example.com"
        );
        // The service's own answer, for a frame on its own host, keeps its name.
        let own = json!({"type": "rich", "provider_name": "SoundCloud",
            "html": "<iframe src=\"https://w.soundcloud.com/player/?url=https%3A%2F%2Fapi.soundcloud.com%2Ftracks%2F1\" height=\"400\"></iframe>"});
        assert_eq!(from_oembed(&own, &base).unwrap().provider, "soundcloud");
        let toot = json!({"type": "rich", "provider_name": "Mastodon",
            "html": "<iframe src=\"https://social.example.com/@a/1/embed\"></iframe>"});
        assert_eq!(
            from_oembed(&toot, &base).unwrap().provider,
            "social.example.com"
        );
        for raw in ["", " ", "#", "#x", "?url=1&format=json"] {
            let v = json!({"type": "rich", "html": format!("<iframe src=\"{raw}\"></iframe>")});
            let page = Url::parse("https://pages.example.com/oembed?url=1&format=json").unwrap();
            assert_eq!(from_oembed(&v, &page), None, "{raw:?}");
        }
        let meta: HashMap<String, String> = [("twitter:player".to_owned(), String::new())].into();
        assert_eq!(from_meta(&meta, &base), None);
    }

    #[test]
    fn a_stored_embed_is_held_to_the_same_bounds() {
        let mut embed = link("https://open.spotify.com/track/003vvx7Niy0yvhvHt4a68B").unwrap();
        embed.allow.push("camera".into());
        embed.autoplay_param = Some("on=<script>".into());
        let embed = embed.bounded().unwrap();
        assert!(!embed.allow.contains(&"camera".into()));
        assert_eq!(embed.autoplay_param, None);
        let mut bad = link("https://open.spotify.com/track/003vvx7Niy0yvhvHt4a68B").unwrap();
        bad.src = "javascript:alert(1)".into();
        assert_eq!(bad.bounded(), None);
    }
}
