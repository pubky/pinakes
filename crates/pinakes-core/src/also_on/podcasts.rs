//! Podcasts: a show or an episode, by its Apple Podcasts, Podcast Index, Pocket Casts or Castro
//! link or its feed, and where else it is.
//!
//! - **Apple Podcasts** (iTunes lookup) confirms an Apple ID is a show, and finds an episode by
//!   its track ID or by its feed item's GUID (among the latest 200 episodes).
//! - **Podcast Index** (with an operator's key and secret) vouches for a show's GUID: its answer
//!   for this feed URL, Apple ID or feed ID must name what was asked, and an Apple ID it names
//!   counts only once iTunes knows it. A Podcast Index failure (a wrong key, an outage) never
//!   fails the lookup: the rest still counts, and a retry is passed on only when nothing else
//!   was found.
//! - **The feed** names its own `podcast:guid`. Without Podcast Index's word that is a fact
//!   only, as is the GUID derived from the feed's URL (UUIDv5 of the URL without its scheme).
//! - **Wikidata** (the item with the show's Apple ID, P5842) adds Spotify, Podchaser, Pocket
//!   Casts and Castbox.
//!
//! Keys: `podcast:apple:{id}` and `podcast:guid:{uuid}` for a show; for an episode
//! `podcast:apple_episode:{id}` and `podcast:item:{show GUID}:{SHA-256 of the item GUID}`, which
//! every link to the episode shares; and the show's `wikidata:Q…`.
use super::{
    media::{self, digits, property, slug, Deferred, Property},
    Context, Fact, Found, Key, Link, Source, Subject,
};
use crate::{
    event::adapters::SecretKey,
    hubs::{self, wikidata},
    resolver::{egress::FetchOptions, RetryLater},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

/// The Podcast Index API's key and secret (`[podcast_index]`, or `PINAKES__PODCAST_INDEX__KEY`
/// and `__SECRET`). Without both, Podcast Index is never asked.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PodcastIndexConfig {
    pub key: Option<SecretKey>,
    pub secret: Option<SecretKey>,
}
impl PodcastIndexConfig {
    fn pair(&self) -> Option<(&str, &str)> {
        match (self.key.as_deref(), self.secret.as_deref()) {
            (Some(k), Some(s)) if !k.trim().is_empty() && !s.trim().is_empty() => {
                Some((k.trim(), s.trim()))
            }
            _ => None,
        }
    }
}

/// What a subject names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Podcast {
    /// A show by its Apple ID, or one of its episodes (`?i=`), on Apple Podcasts or a player
    /// that opens shows by Apple ID; `site` is the link's own service.
    Apple {
        show: u64,
        episode: Option<u64>,
        site: &'static str,
    },
    /// A Podcast Index feed page, or one of its episodes (`?episode=`).
    Index { feed: u64, episode: Option<u64> },
    /// A feed's show, or one of its items (by the item's GUID) on `page`.
    Feed {
        url: String,
        item: Option<String>,
        page: String,
    },
    /// Any other page whose own head names a feed (`feed`): perhaps one of its items.
    Page { feed: String, page: String },
}

/// An http(s) URL on a default port, without credentials, on one of `hosts`.
fn plain(uri: &str, hosts: &[&str]) -> Option<url::Url> {
    let url = url::Url::parse(uri).ok()?;
    (matches!(url.scheme(), "https" | "http")
        && hosts.contains(&url.host_str()?)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}
fn segments(url: &url::Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// The show (and episode) an Apple Podcasts link names:
/// `podcasts.apple.com/{cc}/podcast/{slug}/id{n}`, with or without the country and slug, the
/// same on `itunes.apple.com`, and `?i={episode}`.
pub fn apple(uri: &str) -> Option<(u64, Option<u64>)> {
    let url = plain(uri, &["podcasts.apple.com", "itunes.apple.com"])?;
    let s = segments(&url);
    let rest = match s.as_slice() {
        [cc, rest @ ..] if cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_lowercase()) => rest,
        rest => rest,
    };
    let id = match rest {
        ["podcast", id] | ["podcast", _, id] => id.strip_prefix("id")?,
        _ => return None,
    };
    let show = number(id)?;
    let episode = match url.query_pairs().find(|(k, _)| k == "i") {
        Some((_, i)) => Some(number(&i)?),
        None => None,
    };
    Some((show, episode))
}
/// The show a player link names by its Apple ID: `pca.st/itunes/{id}` (Pocket Casts) or
/// `castro.fm/itunes/{id}` (Castro).
pub fn player(uri: &str) -> Option<(u64, &'static str)> {
    let url = plain(uri, &["pca.st", "castro.fm", "www.castro.fm"])?;
    let site = if url.host_str()? == "pca.st" {
        "Pocket Casts"
    } else {
        "Castro"
    };
    match segments(&url).as_slice() {
        ["itunes", id] => Some((number(id)?, site)),
        _ => None,
    }
}
/// The feed (and episode) a Podcast Index page names: `podcastindex.org/podcast/{feed id}`,
/// with `?episode={episode id}`.
pub fn index_page(uri: &str) -> Option<(u64, Option<u64>)> {
    let url = plain(uri, &["podcastindex.org", "www.podcastindex.org"])?;
    let feed = match segments(&url).as_slice() {
        ["podcast", id] => number(id)?,
        _ => return None,
    };
    let episode = match url.query_pairs().find(|(k, _)| k == "episode") {
        Some((_, e)) => Some(number(&e)?),
        None => None,
    };
    Some((feed, episode))
}
fn number(v: &str) -> Option<u64> {
    digits(v).then(|| v.parse().ok()).flatten()
}

/// What a subject names: an Apple Podcasts, player or Podcast Index link, a resolved feed card
/// (`PodcastSeries`, or a `PodcastEpisode` with its item's GUID) and its feed, or a page whose
/// head names its RSS feed (the card's `feed`).
pub fn podcast(subject: &Subject) -> Option<Podcast> {
    if let Some((show, episode)) = apple(&subject.uri) {
        return Some(Podcast::Apple {
            show,
            episode,
            site: "Apple Podcasts",
        });
    }
    if let Some((show, site)) = player(&subject.uri) {
        return Some(Podcast::Apple {
            show,
            episode: None,
            site,
        });
    }
    if let Some((feed, episode)) = index_page(&subject.uri) {
        return Some(Podcast::Index { feed, episode });
    }
    let doc = subject.doc.as_ref().filter(|_| subject.status == "ok")?;
    let page = subject.uri.clone();
    let attribute = |name: &str| {
        doc.attributes
            .get(name)
            .filter(|u| feed_name(u).is_some())
            .cloned()
    };
    let kind = subject.type_name.as_deref();
    if let (Some(url), Some("PodcastSeries" | "PodcastEpisode")) = (attribute("podcast_feed"), kind)
    {
        if kind == Some("PodcastSeries") {
            return Some(Podcast::Feed {
                url,
                item: None,
                page,
            });
        }
        let item = doc
            .attributes
            .get("podcast_guid")
            .map(|g| g.trim())
            .filter(|g| !g.is_empty())?;
        return Some(Podcast::Feed {
            url,
            item: Some(item.to_owned()),
            page,
        });
    }
    attribute("feed").map(|feed| Podcast::Page { feed, page })
}

/// A feed URL as the podcast namespace names it: without its scheme and trailing slashes, the
/// host in lower case (`mp3s.nashownotes.com/pc20rss.xml`). Only http(s) URLs on the default
/// port, without credentials.
pub fn feed_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if !matches!(parsed.scheme(), "https" | "http")
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    let (_, rest) = url.trim().split_once("://")?;
    let path = rest.get(host.len()..).filter(|_| {
        rest.get(..host.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(host))
    })?;
    let name = format!("{host}{path}");
    let name = name.trim_end_matches('/');
    (!name.is_empty()).then(|| name.to_owned())
}
/// Whether two feed URLs are the same feed (only their scheme, the host's case and trailing
/// slashes may differ).
fn same_feed(a: &str, b: &str) -> bool {
    feed_name(a).is_some_and(|a| Some(a) == feed_name(b))
}

/// The podcast namespace's GUID for a feed that names none: UUIDv5 of its name.
pub fn derived_guid(feed_url: &str) -> Option<String> {
    const NAMESPACE: [u8; 16] = [
        0xea, 0xd4, 0xc2, 0x36, 0xbf, 0x58, 0x58, 0xc6, 0xa2, 0xc6, 0xa6, 0xb2, 0x8d, 0x12, 0x8c,
        0xb6,
    ];
    let name = feed_name(feed_url)?;
    let hash = Sha1::new()
        .chain_update(NAMESPACE)
        .chain_update(name.as_bytes())
        .finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&hash[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
/// A GUID in lower case, if it is one.
pub fn guid(v: &str) -> Option<String> {
    let v = v.trim().to_ascii_lowercase();
    media::uuid(&v).then_some(v)
}
/// The largest podcast feed this source reads (a show's whole back catalogue; the page cap is
/// 1 MiB).
pub const PODCAST_FEED_CAP: usize = 8 * 1024 * 1024;

/// Whether iTunes' feed for a show (`itunes`) is one of `feeds`.
fn apple_feed(itunes: Option<&str>, feeds: &[Option<&str>]) -> bool {
    itunes.is_some_and(|it| feeds.iter().flatten().any(|f| same_feed(f, it)))
}

/// An episode's key: its show's GUID and its item's GUID (hashed: item GUIDs are free text).
pub fn item_key(show_guid: &str, item_guid: &str) -> Option<String> {
    use sha2::Digest as _;
    let item = item_guid.trim();
    if item.is_empty() {
        return None;
    }
    let hash = sha2::Sha256::digest(item.as_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("podcast:item:{}:{hex}", guid(show_guid)?))
}

/// Which item of a feed is wanted: the one with this GUID, or the one whose `<link>` is this page
/// (compared as resource identities, so spellings of one page match).
#[derive(Clone, Copy, Debug)]
pub enum ItemBy<'a> {
    Guid(&'a str),
    Link(&'a str),
}
/// What a podcast feed says: its own `podcast:guid`, and the one wanted item's `<link>` and GUID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedRead {
    pub guid: Option<String>,
    pub item_link: Option<String>,
    pub item_guid: Option<String>,
    /// Whether the wanted item has an audio or video enclosure (an episode, not a post).
    pub item_media: bool,
}
/// Read an RSS feed (any encoding is read as UTF-8, invalid bytes replaced: the GUIDs and links
/// are ASCII). `Err` when the body is no RSS channel at all.
pub fn read_feed(body: &[u8], item: Option<ItemBy<'_>>) -> Result<FeedRead> {
    const NAMESPACES: &[&str] = &[
        "https://podcastindex.org/namespace/1.0",
        "http://podcastindex.org/namespace/1.0",
        "https://github.com/Podcastindex-org/podcast-namespace/blob/main/docs/1.0.md",
    ];
    let text = String::from_utf8_lossy(body);
    let xml = roxmltree::Document::parse(&text)?;
    let channel = xml
        .root_element()
        .children()
        .find(|n| n.has_tag_name("channel"))
        .filter(|_| xml.root_element().has_tag_name("rss"))
        .context("no RSS channel")?;
    let mut guids = channel.children().filter(|n| {
        n.tag_name().name() == "guid"
            && n.tag_name()
                .namespace()
                .is_some_and(|ns| NAMESPACES.contains(&ns.trim_end_matches('/')))
    });
    let own = match (guids.next(), guids.next()) {
        (Some(one), None) => one.text().and_then(guid),
        _ => None,
    };
    let field = |n: roxmltree::Node<'_, '_>, name: &str| {
        n.children()
            .find(|c| {
                c.is_element() && c.tag_name().name() == name && c.tag_name().namespace().is_none()
            })
            .and_then(|c| c.text())
            .map(|t| t.trim().to_owned())
    };
    let identity = |u: &str| crate::model::identity(u).ok().map(|v| v.0);
    // The one item wanted (several are ambiguous).
    let wanted = item.and_then(|by| {
        let page = match by {
            ItemBy::Link(page) => identity(page),
            ItemBy::Guid(_) => None,
        };
        let mut items = channel.children().filter(|n| {
            n.has_tag_name("item")
                && match by {
                    ItemBy::Guid(g) => field(*n, "guid").as_deref() == Some(g),
                    ItemBy::Link(_) => {
                        page.is_some() && field(*n, "link").and_then(|l| identity(&l)) == page
                    }
                }
        });
        match (items.next(), items.next()) {
            (Some(one), None) => Some(one),
            _ => None,
        }
    });
    Ok(FeedRead {
        guid: own,
        item_link: wanted.and_then(|n| field(n, "link")),
        item_guid: wanted
            .and_then(|n| field(n, "guid"))
            .filter(|g| !g.is_empty()),
        item_media: wanted.is_some_and(|n| {
            n.children().any(|c| {
                c.has_tag_name("enclosure")
                    && c.attribute("type")
                        .is_some_and(|t| t.starts_with("audio/") || t.starts_with("video/"))
            })
        }),
    })
}

/// iTunes' record of a show.
pub fn show_request(show: u64, storefront: &str) -> String {
    format!("https://itunes.apple.com/lookup?id={show}&entity=podcast&country={storefront}")
}
/// The show's latest 200 episodes.
pub fn episodes_request(show: u64, storefront: &str) -> String {
    format!(
        "https://itunes.apple.com/lookup?id={show}&entity=podcastEpisode&limit=200&country={storefront}"
    )
}
/// iTunes' record of `show` when it knows it as a podcast (its one result, of kind `podcast`).
pub fn parse_show(status: u16, raw: &[u8], show: u64) -> Result<Option<ItunesShow>> {
    let Some(v) = media::json(status, raw, &[404])? else {
        return Ok(None);
    };
    let results = v["results"].as_array().context("no results")?;
    Ok(match results.as_slice() {
        [one] if one["kind"] == "podcast" && one["collectionId"].as_u64() == Some(show) => {
            Some(ItunesShow {
                feed: one["feedUrl"]
                    .as_str()
                    .filter(|u| feed_name(u).is_some())
                    .map(str::to_owned),
            })
        }
        _ => None,
    })
}
/// A show as iTunes knows it: Apple's word for which feed its Apple ID is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItunesShow {
    pub feed: Option<String>,
}
/// An episode iTunes lists: its track ID and its feed item's GUID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Episode {
    pub track: u64,
    pub guid: Option<String>,
}
/// The episodes of `show` iTunes lists.
pub fn parse_episodes(status: u16, raw: &[u8], show: u64) -> Result<Vec<Episode>> {
    let Some(v) = media::json(status, raw, &[404])? else {
        return Ok(Vec::new());
    };
    Ok(v["results"]
        .as_array()
        .context("no results")?
        .iter()
        .filter(|e| {
            e["wrapperType"] == "podcastEpisode" && e["collectionId"].as_u64() == Some(show)
        })
        .filter_map(|e| {
            Some(Episode {
                track: e["trackId"].as_u64().filter(|t| *t > 0)?,
                guid: e["episodeGuid"]
                    .as_str()
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .map(str::to_owned),
            })
        })
        .collect())
}
/// The one episode with this feed item GUID (several mean the GUID is not unique: none).
fn by_guid<'a>(episodes: &'a [Episode], item: &str) -> Option<&'a Episode> {
    let mut found = episodes.iter().filter(|e| e.guid.as_deref() == Some(item));
    match (found.next(), found.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}
/// The one episode with this track ID.
fn by_track(episodes: &[Episode], track: u64) -> Option<&Episode> {
    episodes.iter().find(|e| e.track == track)
}

const PODCAST_INDEX: &str = "https://api.podcastindex.org/api/1.0";
/// What Podcast Index is asked.
#[derive(Clone, Copy, Debug)]
pub enum Ask<'a> {
    Apple(u64),
    Feed(&'a str),
    Guid(&'a str),
    FeedId(u64),
}
pub fn pi_request(ask: Ask) -> String {
    let enc = |v: &str| url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
    match ask {
        Ask::Apple(id) => format!("{PODCAST_INDEX}/podcasts/byitunesid?id={id}"),
        Ask::Feed(url) => format!("{PODCAST_INDEX}/podcasts/byfeedurl?url={}", enc(url)),
        Ask::Guid(g) => format!("{PODCAST_INDEX}/podcasts/byguid?guid={}", enc(g)),
        Ask::FeedId(id) => format!("{PODCAST_INDEX}/podcasts/byfeedid?id={id}"),
    }
}
/// Podcast Index's episode record.
pub fn pi_episode_request(id: u64) -> String {
    format!("{PODCAST_INDEX}/episodes/byid?id={id}")
}
/// Podcast Index's signed headers: the key, the time, and SHA-1 of key, secret and time.
pub fn pi_headers(key: &str, secret: &str, now: i64) -> Vec<(&'static str, String)> {
    let hash = Sha1::digest(format!("{key}{secret}{now}").as_bytes());
    vec![
        ("X-Auth-Key", key.into()),
        ("X-Auth-Date", now.to_string()),
        (
            "Authorization",
            hash.iter().map(|b| format!("{b:02x}")).collect(),
        ),
    ]
}
/// A feed as Podcast Index records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexFeed {
    pub id: u64,
    pub guid: Option<String>,
    pub apple: Option<u64>,
    /// The feed's URL as Podcast Index reads it today.
    pub url: Option<String>,
}
/// The feed Podcast Index answers, or `None` when it has none or answers another feed than the
/// one asked (its `url` or `originalUrl` must be the feed asked, its `itunesId`, `podcastGuid`
/// or `id` the one asked).
pub fn parse_pi(status: u16, raw: &[u8], ask: Ask) -> Result<Option<IndexFeed>> {
    let Some(v) = media::json(status, raw, &[400, 404])? else {
        return Ok(None);
    };
    let feed = &v["feed"];
    if !feed.is_object() {
        return Ok(None);
    }
    let Some(id) = feed["id"].as_u64().filter(|id| *id > 0) else {
        return Ok(None);
    };
    let found = IndexFeed {
        id,
        guid: feed["podcastGuid"].as_str().and_then(guid),
        apple: feed["itunesId"].as_u64().filter(|id| *id > 0),
        url: feed["url"]
            .as_str()
            .filter(|u| feed_name(u).is_some())
            .map(str::to_owned),
    };
    let theirs = match ask {
        Ask::Apple(asked) => found.apple == Some(asked),
        Ask::Feed(asked) => [&feed["url"], &feed["originalUrl"]]
            .iter()
            .filter_map(|u| u.as_str())
            .any(|u| same_feed(u, asked)),
        Ask::Guid(asked) => found.guid.as_deref() == Some(asked),
        Ask::FeedId(asked) => id == asked,
    };
    Ok(theirs.then_some(found))
}
/// An episode as Podcast Index records it: its item GUID and its feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEpisode {
    pub guid: String,
    pub feed: u64,
}
/// The episode, or `None` when Podcast Index has none or answers another one.
pub fn parse_pi_episode(status: u16, raw: &[u8], id: u64) -> Result<Option<IndexEpisode>> {
    let Some(v) = media::json(status, raw, &[400, 404])? else {
        return Ok(None);
    };
    let e = &v["episode"];
    if !e.is_object() || e["id"].as_u64() != Some(id) {
        return Ok(None);
    }
    Ok(
        match (e["guid"].as_str().map(str::trim), e["feedId"].as_u64()) {
            (Some(g), Some(feed)) if !g.is_empty() && feed > 0 => Some(IndexEpisode {
                guid: g.to_owned(),
                feed,
            }),
            _ => None,
        },
    )
}

/// The show's other pages, from its Wikidata item (found by its Apple Podcasts ID).
pub const WIKIDATA: &[Property] = &[
    property(
        "P5916",
        "Spotify",
        "https://open.spotify.com/show/$1",
        |v| v.len() == 22 && v.bytes().all(|b| b.is_ascii_alphanumeric()),
    ),
    property(
        "P11740",
        "Podcast Index",
        "https://podcastindex.org/podcast/$1",
        digits,
    ),
    property(
        "P7998",
        "Podchaser",
        "https://www.podchaser.com/podcasts/$1",
        digits,
    ),
    property("P9006", "Pocket Casts", "https://pca.st/$1", |v| {
        slug(v) && !v.contains('.') && !v.contains('_')
    }),
    property(
        "P9005",
        "Castbox",
        "https://castbox.fm/channel/id$1",
        digits,
    ),
];

fn key(key: String, via: &str) -> Key {
    Key {
        key,
        via: via.into(),
    }
}
fn add(found: &mut Found, link: Link) {
    if !found
        .links
        .iter()
        .any(|l| l.uri == link.uri || l.site == link.site)
    {
        found.links.push(link);
    }
}
/// A show's Apple ID: its key and the pages that open it by that ID, keyed by it.
pub fn apple_show(found: &mut Found, show: u64, storefront: &str) {
    let apple = format!("podcast:apple:{show}");
    found.keys.push(key(apple.clone(), "itunes"));
    for (uri, site) in [
        (
            format!("https://podcasts.apple.com/{storefront}/podcast/id{show}"),
            "Apple Podcasts",
        ),
        (format!("https://pca.st/itunes/{show}"), "Pocket Casts"),
        (format!("https://castro.fm/itunes/{show}"), "Castro"),
    ] {
        add(found, Link::new(uri, site, "itunes").keyed(&apple));
    }
}
/// The exact key a show's derived link is named by: the GUID Podcast Index vouches for, else the
/// Apple ID iTunes confirmed.
fn show_key(vouched: Option<&str>, apple: Option<u64>) -> Option<String> {
    vouched
        .map(|g| format!("podcast:guid:{g}"))
        .or_else(|| apple.map(|a| format!("podcast:apple:{a}")))
}
/// A show's Podcast Index feed: its page, and the GUID Podcast Index vouches for, as a key.
/// `apple`: the show's Apple ID, when Podcast Index's feed is the one iTunes names for it.
fn index_show(found: &mut Found, feed: &IndexFeed, vouched: Option<&str>, apple: Option<u64>) {
    let mut page = Link::new(
        format!("https://podcastindex.org/podcast/{}", feed.id),
        "Podcast Index",
        "podcastindex",
    );
    page.key = show_key(vouched, apple);
    add(found, page);
    if let Some(guid) = vouched {
        found
            .keys
            .push(key(format!("podcast:guid:{guid}"), "podcastindex"));
    }
}
/// The GUID Podcast Index vouches for: the feed's GUID, when Podcast Index's feed for that GUID
/// (`byguid`) is this same feed. A copy of a show's feed carries the show's GUID too, but the
/// GUID is not its own.
pub fn vouched<'a>(feed: &'a IndexFeed, owner: Option<&IndexFeed>) -> Option<&'a str> {
    feed.guid
        .as_deref()
        .filter(|_| owner.is_some_and(|o| o.id == feed.id))
}
/// An episode's Apple track: its key and page, keyed by it.
pub fn apple_episode(found: &mut Found, show: u64, track: u64, storefront: &str) {
    let apple = format!("podcast:apple_episode:{track}");
    found.keys.push(key(apple.clone(), "itunes"));
    add(
        found,
        Link::new(
            format!("https://podcasts.apple.com/{storefront}/podcast/id{show}?i={track}"),
            "Apple Podcasts",
            "itunes",
        )
        .keyed(apple),
    );
}

/// The source: iTunes, Podcast Index, the feed and Wikidata.
pub struct Podcasts;

impl Podcasts {
    async fn itunes(cx: &Context, url: &str) -> Result<crate::resolver::egress::Fetched> {
        let options = FetchOptions {
            accept: Some("application/json".into()),
            extra_types: &["text/javascript"],
            ..Default::default()
        };
        cx.egress.discover(url, &options, 3600).await
    }
    /// iTunes' record of the show, if it knows it.
    async fn itunes_show(cx: &Context, show: u64) -> Result<Option<ItunesShow>> {
        let sf = &cx.config.also_on.apple_storefront;
        let f = Self::itunes(cx, &show_request(show, sf)).await?;
        parse_show(f.status, &f.body, show)
    }
    /// The Apple ID Podcast Index names for a feed, when iTunes' own record of that show names
    /// one of `feeds` as its feed. Podcast Index takes any feed's claim to an Apple ID; Apple is
    /// the authority for its own IDs.
    async fn apple_of(
        cx: &Context,
        deferred: &mut Deferred,
        claimed: Option<u64>,
        feeds: &[Option<&str>],
    ) -> Option<u64> {
        let show = claimed?;
        let record = deferred.take("itunes", Self::itunes_show(cx, show).await)?;
        apple_feed(record.feed.as_deref(), feeds).then_some(show)
    }
    async fn episodes(cx: &Context, show: u64) -> Result<Vec<Episode>> {
        let sf = &cx.config.also_on.apple_storefront;
        let f = Self::itunes(cx, &episodes_request(show, sf)).await?;
        parse_episodes(f.status, &f.body, show)
    }
    /// A signed Podcast Index request, when the operator configured its key.
    async fn index_get(
        cx: &Context,
        url: &str,
    ) -> Result<Option<crate::resolver::egress::Fetched>> {
        let Some((key, secret)) = cx.config.podcast_index.pair() else {
            return Ok(None);
        };
        let options = FetchOptions {
            accept: Some("application/json".into()),
            headers: pi_headers(key, secret, chrono::Utc::now().timestamp()),
            sensitive: true,
            ..Default::default()
        };
        Ok(Some(cx.egress.fetch_with(url, &options).await?))
    }
    /// Podcast Index's feed for `ask`.
    async fn index(cx: &Context, ask: Ask<'_>) -> Result<Option<IndexFeed>> {
        match Self::index_get(cx, &pi_request(ask)).await? {
            Some(f) => parse_pi(f.status, &f.body, ask),
            None => Ok(None),
        }
    }
    /// Podcast Index's episode `id`.
    async fn index_episode(cx: &Context, id: u64) -> Result<Option<IndexEpisode>> {
        match Self::index_get(cx, &pi_episode_request(id)).await? {
            Some(f) => parse_pi_episode(f.status, &f.body, id),
            None => Ok(None),
        }
    }
    /// The feed read at exactly this URL (a redirect may be another publisher's feed). `None`
    /// when it can't be read as one (e.g. a body over the page cap); a retry is passed on.
    async fn feed(cx: &Context, url: &str, item: Option<&str>) -> Result<Option<FeedRead>> {
        Self::read(cx, url, item.map(ItemBy::Guid), true)
            .await
            .map(|r| r.map(|(read, _)| read))
    }
    /// A feed read with what is wanted of it, and the URL it was read at (bodies up to
    /// `PODCAST_FEED_CAP`). `exact`: only at this URL, never through a redirect.
    async fn read(
        cx: &Context,
        url: &str,
        item: Option<ItemBy<'_>>,
        exact: bool,
    ) -> Result<Option<(FeedRead, String)>> {
        let options = FetchOptions {
            feed_cap: Some(PODCAST_FEED_CAP),
            ..Default::default()
        };
        let f = match cx.egress.discover(url, &options, 3600).await {
            Ok(f) => f,
            Err(e) if e.is::<RetryLater>() => return Err(e),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "podcast feed unreadable");
                return Ok(None);
            }
        };
        if f.status != 200 || (exact && !same_feed(f.url.as_str(), url)) {
            return Ok(None);
        }
        let body = f.body.clone();
        let owned = item.map(|by| match by {
            ItemBy::Guid(g) => (true, g.to_owned()),
            ItemBy::Link(l) => (false, l.to_owned()),
        });
        let read = tokio::task::spawn_blocking(move || {
            let by = owned.as_ref().map(|(guid, v)| {
                if *guid {
                    ItemBy::Guid(v)
                } else {
                    ItemBy::Link(v)
                }
            });
            read_feed(&body, by)
        })
        .await?
        .ok();
        Ok(read.map(|r| (r, f.url.to_string())))
    }
    /// The GUID Podcast Index vouches for this feed (asking `byguid` who owns it).
    async fn vouch(cx: &Context, deferred: &mut Deferred, feed: &IndexFeed) -> Option<String> {
        let g = feed.guid.as_deref()?;
        let owner = deferred.take("podcastindex", Self::index(cx, Ask::Guid(g)).await);
        vouched(feed, owner.as_ref()).map(str::to_owned)
    }
    /// The show's Wikidata item, by its Apple ID: its pages are keyed by that ID.
    async fn wikidata(cx: &Context, found: &mut Found, show: u64) -> Result<Option<()>> {
        if let wikidata::Found::One(item) =
            wikidata::item_with(&cx.egress, "P5842", &show.to_string()).await?
        {
            found
                .keys
                .extend(hubs::key::wikidata(&item.qid).map(|k| key(k, "wikidata")));
            for l in media::links(&item, WIKIDATA) {
                add(found, l.keyed(format!("podcast:apple:{show}")));
            }
        }
        Ok(Some(()))
    }
    /// Podcast Index's feed for an Apple ID, when it is the feed iTunes names for that ID
    /// (`itunes_feed`), with the GUID Podcast Index vouches for it.
    async fn index_for_apple(
        cx: &Context,
        deferred: &mut Deferred,
        show: u64,
        itunes_feed: Option<&str>,
    ) -> Option<(IndexFeed, Option<String>)> {
        let feed = deferred
            .take("podcastindex", Self::index(cx, Ask::Apple(show)).await)
            .filter(|f| apple_feed(itunes_feed, &[f.url.as_deref()]))?;
        let vouched = Self::vouch(cx, deferred, &feed).await;
        Some((feed, vouched))
    }
    /// Everything a show confirmed by iTunes gets: its Apple ID, Podcast Index's feed (the one
    /// for the Apple ID that is iTunes' feed, unless given) and its Wikidata item. Returns
    /// Podcast Index's GUID for the show (a fact).
    async fn show(
        cx: &Context,
        found: &mut Found,
        deferred: &mut Deferred,
        show: u64,
        itunes_feed: Option<&str>,
        indexed: Option<(IndexFeed, Option<String>)>,
    ) -> Result<Option<String>> {
        apple_show(found, show, &cx.config.also_on.apple_storefront);
        let indexed = match indexed {
            Some(given) => Some(given),
            None => Self::index_for_apple(cx, deferred, show, itunes_feed).await,
        };
        if let Some((feed, vouched)) = &indexed {
            index_show(found, feed, vouched.as_deref(), Some(show));
        }
        deferred.take("wikidata", Self::wikidata(cx, found, show).await);
        Ok(indexed.and_then(|(f, _)| f.guid))
    }
    /// An episode's keys from its show's vouched GUID and its item GUID, and its Apple track
    /// (found by track ID or by item GUID among the show's latest episodes).
    async fn episode(
        cx: &Context,
        found: &mut Found,
        deferred: &mut Deferred,
        show: Option<u64>,
        show_guid: Option<&str>,
        item: Option<&str>,
        track: Option<u64>,
    ) -> Option<String> {
        let sf = cx.config.also_on.apple_storefront.clone();
        let mut item = item.map(str::to_owned);
        if let Some(show) = show {
            if let Some(episodes) =
                deferred.take("itunes", Self::episodes(cx, show).await.map(Some))
            {
                let e = match (track, &item) {
                    (Some(t), _) => by_track(&episodes, t),
                    (None, Some(g)) => by_guid(&episodes, g),
                    _ => None,
                };
                if let Some(e) = e {
                    apple_episode(found, show, e.track, &sf);
                    if item.is_none() {
                        item = e.guid.clone();
                    }
                }
            }
        }
        if let (Some(show_guid), Some(item)) = (show_guid, &item) {
            found
                .keys
                .extend(item_key(show_guid, item).map(|k| key(k, "podcastindex")));
        }
        item
    }
}

#[async_trait]
impl Source for Podcasts {
    fn id(&self) -> &'static str {
        "podcasts"
    }
    fn applies(&self, subject: &Subject) -> bool {
        podcast(subject).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let mut found = Found::default();
        let mut deferred = Deferred::default();
        let Some(podcast) = podcast(subject) else {
            return Ok(found);
        };
        match &podcast {
            Podcast::Apple {
                show,
                episode,
                site,
            } => {
                let Some(record) = deferred.take("itunes", Self::itunes_show(cx, *show).await)
                else {
                    // A player link names its Apple ID in the URL itself.
                    if *site != "Apple Podcasts" {
                        found.keys.push(key(format!("podcast:apple:{show}"), "url"));
                    }
                    return deferred.finish(found);
                };
                let itunes_feed = record.feed.as_deref();
                match episode {
                    None => {
                        let guid =
                            Self::show(cx, &mut found, &mut deferred, *show, itunes_feed, None)
                                .await?;
                        if let Some(g) = guid {
                            found.facts.push(Fact::new("podcast_guid", g));
                        }
                    }
                    Some(track) => {
                        let show_guid =
                            Self::index_for_apple(cx, &mut deferred, *show, itunes_feed)
                                .await
                                .and_then(|(_, vouched)| vouched);
                        Self::episode(
                            cx,
                            &mut found,
                            &mut deferred,
                            Some(*show),
                            show_guid.as_deref(),
                            None,
                            Some(*track),
                        )
                        .await;
                    }
                }
                // The link's own service is the card itself.
                found.links.retain(|l| l.site != *site);
            }
            Podcast::Index { feed, episode } => {
                let indexed =
                    deferred.take("podcastindex", Self::index(cx, Ask::FeedId(*feed)).await);
                let Some(indexed) = indexed else {
                    return deferred.finish(found);
                };
                let apple =
                    Self::apple_of(cx, &mut deferred, indexed.apple, &[indexed.url.as_deref()])
                        .await;
                let vouched = Self::vouch(cx, &mut deferred, &indexed).await;
                match episode {
                    None => {
                        let guid = indexed.guid.clone();
                        if let Some(url) = &indexed.url {
                            let mut feed = Link::new(url.clone(), "Podcast feed", "podcastindex");
                            feed.key = show_key(vouched.as_deref(), apple);
                            add(&mut found, feed);
                        }
                        match apple {
                            Some(show) => {
                                let given = Some((indexed.clone(), vouched.clone()));
                                Self::show(cx, &mut found, &mut deferred, show, None, given)
                                    .await?;
                            }
                            None => index_show(&mut found, &indexed, vouched.as_deref(), None),
                        }
                        if let Some(g) = guid {
                            found.facts.push(Fact::new("podcast_guid", g));
                        }
                        found.links.retain(|l| l.site != "Podcast Index");
                    }
                    Some(id) => {
                        let e = deferred.take("podcastindex", Self::index_episode(cx, *id).await);
                        if let Some(e) = e.filter(|e| e.feed == *feed) {
                            Self::episode(
                                cx,
                                &mut found,
                                &mut deferred,
                                apple,
                                vouched.as_deref(),
                                Some(&e.guid),
                                None,
                            )
                            .await;
                        }
                    }
                }
            }
            Podcast::Feed { url, item, page } => {
                let indexed = deferred.take("podcastindex", Self::index(cx, Ask::Feed(url)).await);
                // An Apple ID Podcast Index names counts only when iTunes names this feed for it.
                let apple = Self::apple_of(
                    cx,
                    &mut deferred,
                    indexed.as_ref().and_then(|f| f.apple),
                    &[Some(url), indexed.as_ref().and_then(|f| f.url.as_deref())],
                )
                .await;
                let vouched = match &indexed {
                    Some(feed) => Self::vouch(cx, &mut deferred, feed).await,
                    None => None,
                };
                let pi_guid = indexed.as_ref().and_then(|f| f.guid.clone());
                match item {
                    None => {
                        let own = deferred
                            .take("feed", Self::feed(cx, url, None).await)
                            .and_then(|r| r.guid);
                        match (apple, indexed) {
                            (Some(show), Some(feed)) => {
                                let given = Some((feed, vouched.clone()));
                                Self::show(cx, &mut found, &mut deferred, show, None, given)
                                    .await?;
                            }
                            (None, Some(feed)) => {
                                index_show(&mut found, &feed, vouched.as_deref(), None)
                            }
                            _ => {}
                        }
                        // The fact: the feed's own GUID, then Podcast Index's, then the derived one.
                        if let Some(g) = own.or(pi_guid).or_else(|| derived_guid(url)) {
                            found.facts.push(Fact::new("podcast_guid", g));
                        }
                    }
                    Some(item) => {
                        // The item key only for the page the vouched feed names as the item's
                        // `<link>`.
                        let show_guid = match &vouched {
                            Some(_) => {
                                let read =
                                    deferred.take("feed", Self::feed(cx, url, Some(item)).await);
                                let listed = read.and_then(|r| r.item_link).is_some_and(|link| {
                                    crate::model::identity(&link).ok().map(|v| v.0)
                                        == crate::model::identity(page).ok().map(|v| v.0)
                                });
                                listed.then(|| vouched.clone()).flatten()
                            }
                            None => None,
                        };
                        Self::episode(
                            cx,
                            &mut found,
                            &mut deferred,
                            apple,
                            show_guid.as_deref(),
                            Some(item),
                            None,
                        )
                        .await;
                    }
                }
            }
            Podcast::Page { feed, page } => {
                // The page's own head names its feed, so the feed is read where that leads; the
                // one item whose `<link>` is this page makes it an episode of a vouched show.
                let read = deferred.take(
                    "feed",
                    Self::read(cx, feed, Some(ItemBy::Link(page)), false).await,
                );
                let Some((read, at)) = read else {
                    return deferred.finish(found);
                };
                // Only an episode (an item with audio or video) is worth asking Podcast Index.
                let Some(item) = read.item_guid.filter(|_| read.item_media) else {
                    return deferred.finish(found);
                };
                let indexed = deferred.take("podcastindex", Self::index(cx, Ask::Feed(&at)).await);
                let Some(indexed) = indexed else {
                    return deferred.finish(found);
                };
                let vouched = Self::vouch(cx, &mut deferred, &indexed).await;
                let apple = Self::apple_of(
                    cx,
                    &mut deferred,
                    indexed.apple,
                    &[Some(&at), indexed.url.as_deref()],
                )
                .await;
                Self::episode(
                    cx,
                    &mut found,
                    &mut deferred,
                    apple,
                    vouched.as_deref(),
                    Some(&item),
                    None,
                )
                .await;
            }
        }
        deferred.finish(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{also_on::media::fixture, model::ResourceMeta};

    fn read(name: &str) -> Vec<u8> {
        fixture::read("podcasts", name)
    }
    const DAILY: u64 = 1200361736;
    const GUID_DAILY: &str = "304b84f0-07b0-5265-b6b7-da5cf5aeb56e";

    #[test]
    fn links_that_name_a_show_or_an_episode() {
        for uri in [
            "https://podcasts.apple.com/us/podcast/the-daily/id1200361736",
            "https://podcasts.apple.com/podcast/id1200361736",
            "https://podcasts.apple.com/gb/podcast/id1200361736?uo=4",
            "http://itunes.apple.com/us/podcast/the-daily/id1200361736?mt=2",
        ] {
            assert_eq!(apple(uri), Some((DAILY, None)), "{uri}");
        }
        assert_eq!(
            apple("https://podcasts.apple.com/us/podcast/inside-putins-plan/id1200361736?i=1000797051580&uo=4"),
            Some((DAILY, Some(1000797051580)))
        );
        for uri in [
            "https://podcasts.apple.com/us/podcast/the-daily/id0",
            "https://podcasts.apple.com/us/podcast/the-daily/1200361736",
            "https://podcasts.apple.com/us/album/x/id1200361736",
            "https://podcasts.apple.com/us/podcast/the-daily/id1200361736?i=x",
            "https://podcasts.apple.com:8443/us/podcast/id1200361736",
            "https://music.apple.com/us/podcast/id1200361736",
        ] {
            assert_eq!(apple(uri), None, "{uri}");
        }
        // Players that open a show by its Apple ID, and Podcast Index pages.
        assert_eq!(
            player("https://pca.st/itunes/1200361736"),
            Some((DAILY, "Pocket Casts"))
        );
        assert_eq!(
            player("https://castro.fm/itunes/1200361736"),
            Some((DAILY, "Castro"))
        );
        assert_eq!(player("https://pca.st/thedaily"), None);
        assert_eq!(player("https://pca.st:8443/itunes/1200361736"), None);
        assert_eq!(
            index_page("https://podcastindex.org/podcast/743229"),
            Some((743229, None))
        );
        assert_eq!(
            index_page("https://podcastindex.org/podcast/743229?episode=61953304662"),
            Some((743229, Some(61953304662)))
        );
        assert_eq!(index_page("https://podcastindex.org/podcast/x"), None);
        assert_eq!(index_page("https://podcastindex.org/search?q=daily"), None);
    }

    #[test]
    fn feed_cards_name_their_feed() {
        let mut doc = ResourceMeta::new("https://example.org/feed.xml", "media", 60);
        doc.attributes
            .insert("podcast_feed".into(), "https://example.org/feed.xml".into());
        let mut s = Subject {
            id: "r".into(),
            uri: "https://example.org/feed.xml".into(),
            status: "ok".into(),
            type_name: Some("PodcastSeries".into()),
            doc: Some(doc.clone()),
            keys: Vec::new(),
            first_seen_at: None,
        };
        assert_eq!(
            podcast(&s),
            Some(Podcast::Feed {
                url: "https://example.org/feed.xml".into(),
                item: None,
                page: "https://example.org/feed.xml".into(),
            })
        );
        s.type_name = Some("PodcastEpisode".into());
        assert_eq!(podcast(&s), None, "an episode needs its item's GUID");
        doc.attributes.insert("podcast_guid".into(), "ep-1".into());
        s.doc = Some(doc);
        assert_eq!(
            podcast(&s),
            Some(Podcast::Feed {
                url: "https://example.org/feed.xml".into(),
                item: Some("ep-1".into()),
                page: "https://example.org/feed.xml".into(),
            })
        );
        s.status = "missing".into();
        assert_eq!(podcast(&s), None);
    }

    #[test]
    fn the_guid_a_feed_without_one_gets() {
        // The podcast namespace's own example, and The Daily's feed.
        assert_eq!(
            derived_guid("https://mp3s.nashownotes.com/pc20rss.xml").as_deref(),
            Some("917393e3-1b1e-5cef-ace4-edaa54e1f810")
        );
        assert_eq!(
            derived_guid("http://mp3s.nashownotes.com/pc20rss.xml/").as_deref(),
            Some("917393e3-1b1e-5cef-ace4-edaa54e1f810"),
            "the scheme and trailing slashes are left out"
        );
        assert_eq!(
            derived_guid("https://feeds.simplecast.com/54nAGcIl").as_deref(),
            Some("304b84f0-07b0-5265-b6b7-da5cf5aeb56e")
        );
        assert_eq!(derived_guid("ftp://example.org/feed.xml"), None);
        assert_eq!(
            derived_guid("https://MP3S.nashownotes.com/pc20rss.xml").as_deref(),
            Some("917393e3-1b1e-5cef-ace4-edaa54e1f810"),
            "the host's case is not the feed's"
        );
        assert_eq!(derived_guid("https://example.org:8443/feed.xml"), None);
        assert_eq!(
            feed_name("HTTPS://Feeds.Example.org/Show.xml/").as_deref(),
            Some("feeds.example.org/Show.xml"),
            "the path keeps its case"
        );
    }

    #[test]
    fn an_episode_key_is_its_shows_guid_and_its_item_guid() {
        let daily = "304b84f0-07b0-5265-b6b7-da5cf5aeb56e";
        let key = item_key(daily, "1897475d-9b60-447d-ba9b-79aa09f59551").unwrap();
        assert!(key.starts_with("podcast:item:304b84f0-07b0-5265-b6b7-da5cf5aeb56e:"));
        assert_eq!(key.len(), "podcast:item:".len() + 36 + 1 + 64);
        assert!(crate::also_on::valid_key(&key), "{key}");
        assert_eq!(
            item_key(
                &daily.to_uppercase(),
                " 1897475d-9b60-447d-ba9b-79aa09f59551 "
            ),
            Some(key)
        );
        assert_eq!(item_key(daily, " "), None);
        assert_eq!(item_key("not-a-guid", "x"), None);
    }

    #[test]
    fn a_feed_names_its_own_guid() {
        // Podcasting 2.0 moved to feeds.podcastindex.org but keeps the GUID of its first URL:
        // the GUID derived from where it lives now would be another show's.
        let pc20 = read("feed-pc20.xml.gz");
        let own = read_feed(&pc20, None).unwrap().guid;
        assert_eq!(own.as_deref(), Some("917393e3-1b1e-5cef-ace4-edaa54e1f810"));
        assert_ne!(derived_guid("https://feeds.podcastindex.org/pc20.xml"), own);
        // An item's own page, by its GUID.
        assert_eq!(
            read_feed(&pc20, Some(ItemBy::Guid("PC20-275")))
                .unwrap()
                .item_link
                .as_deref(),
            Some("https://www.podpage.com/podcasting-20/off-the-rails/")
        );
        assert_eq!(
            read_feed(&pc20, Some(ItemBy::Guid("PC20-0")))
                .unwrap()
                .item_link,
            None
        );
        // An item by its page.
        let dd = read("feed-darknetdiaries.xml.gz");
        assert_eq!(
            read_feed(
                &dd,
                Some(ItemBy::Link("https://darknetdiaries.com/episode/180/"))
            )
            .unwrap()
            .item_guid
            .as_deref(),
            Some("prx_7057_141e0c43-f935-41de-be94-708448052c4d"),
            "the page"
        );
        assert_eq!(
            read_feed(&dd, Some(ItemBy::Link("https://darknetdiaries.com/")))
                .unwrap()
                .item_guid,
            None
        );
        let plain = br#"<rss version="2.0"><channel><title>T</title><item><guid>a</guid></item></channel></rss>"#;
        assert_eq!(read_feed(plain, None).unwrap().guid, None);
        let other = br#"<rss xmlns:x="https://example.org/ns"><channel><x:guid>917393e3-1b1e-5cef-ace4-edaa54e1f810</x:guid></channel></rss>"#;
        assert_eq!(
            read_feed(other, None).unwrap().guid,
            None,
            "another namespace's guid"
        );
        // A Latin-1 feed is read: its GUID and links are ASCII.
        let latin1 = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><rss xmlns:podcast=\"https://podcastindex.org/namespace/1.0\"><channel><title>Caf\xe9</title><podcast:guid>917393e3-1b1e-5cef-ace4-edaa54e1f810</podcast:guid></channel></rss>";
        assert_eq!(
            read_feed(latin1, None).unwrap().guid.as_deref(),
            Some("917393e3-1b1e-5cef-ace4-edaa54e1f810")
        );
        assert!(read_feed(b"<html><body>no feed</body></html>", None).is_err());
    }

    #[test]
    fn itunes_shows_and_episodes() {
        assert_eq!(
            show_request(DAILY, "us"),
            "https://itunes.apple.com/lookup?id=1200361736&entity=podcast&country=us"
        );
        assert_eq!(
            episodes_request(DAILY, "us"),
            "https://itunes.apple.com/lookup?id=1200361736&entity=podcastEpisode&limit=200&country=us"
        );
        assert_eq!(
            parse_show(200, &read("itunes-podcast-1200361736.json"), DAILY).unwrap(),
            Some(ItunesShow {
                feed: Some("https://feeds.simplecast.com/Sl5CSM3S".into())
            }),
            "Apple's feed for The Daily is not the one Podcast Index reads"
        );
        assert_eq!(
            parse_show(200, &read("itunes-podcast-1200361736.json"), 1).unwrap(),
            None
        );
        assert_eq!(
            parse_show(200, &read("itunes-podcast-missing.json"), 1).unwrap(),
            None
        );
        assert!(apple_feed(
            Some("https://podcast.darknetdiaries.com"),
            &[None, Some("https://podcast.darknetdiaries.com/")]
        ));
        assert!(!apple_feed(
            Some("https://feeds.simplecast.com/Sl5CSM3S"),
            &[Some("https://feeds.simplecast.com/54nAGcIl")]
        ));
        assert!(!apple_feed(
            None,
            &[Some("https://feeds.simplecast.com/54nAGcIl")]
        ));
        assert!(parse_show(503, b"", DAILY).is_err());
        let episodes =
            parse_episodes(200, &read("itunes-episodes-1200361736.json.gz"), DAILY).unwrap();
        assert_eq!(episodes.len(), 43, "the show's own record is no episode");
        assert_eq!(
            by_guid(&episodes, "1897475d-9b60-447d-ba9b-79aa09f59551"),
            Some(&Episode {
                track: 1000797051580,
                guid: Some("1897475d-9b60-447d-ba9b-79aa09f59551".into())
            })
        );
        assert_eq!(by_guid(&episodes, "not-there"), None);
        let twice = [episodes[0].clone(), episodes[0].clone()];
        assert_eq!(by_guid(&twice, episodes[0].guid.as_deref().unwrap()), None);
        assert!(
            parse_episodes(200, &read("itunes-episodes-1200361736.json.gz"), 1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn podcast_index_requests_are_signed() {
        assert_eq!(
            pi_request(Ask::Feed("https://feeds.simplecast.com/54nAGcIl")),
            "https://api.podcastindex.org/api/1.0/podcasts/byfeedurl?url=https%3A%2F%2Ffeeds.simplecast.com%2F54nAGcIl"
        );
        assert_eq!(
            pi_request(Ask::Apple(DAILY)),
            "https://api.podcastindex.org/api/1.0/podcasts/byitunesid?id=1200361736"
        );
        assert_eq!(
            pi_request(Ask::Guid("304b84f0-07b0-5265-b6b7-da5cf5aeb56e")),
            "https://api.podcastindex.org/api/1.0/podcasts/byguid?guid=304b84f0-07b0-5265-b6b7-da5cf5aeb56e"
        );
        assert_eq!(
            pi_headers("KEY", "SECRET", 1792440000),
            [
                ("X-Auth-Key", "KEY".to_string()),
                ("X-Auth-Date", "1792440000".into()),
                (
                    "Authorization",
                    "2d50ebc58cd40f3e713415c20e206d6948fcff1d".into()
                ),
            ]
        );
        let config: PodcastIndexConfig =
            serde_json::from_str(r#"{"key":"KEY","secret":" "}"#).unwrap();
        assert_eq!(config.pair(), None, "an empty secret is none");
        assert!(!format!("{config:?}").contains("KEY"));
        assert!(!serde_json::to_string(&config).unwrap().contains("KEY"));
    }

    #[test]
    fn podcast_index_must_answer_what_was_asked() {
        let daily = IndexFeed {
            id: 743229,
            guid: Some("304b84f0-07b0-5265-b6b7-da5cf5aeb56e".into()),
            apple: Some(DAILY),
            url: Some("https://feeds.simplecast.com/54nAGcIl".into()),
        };
        let byid = read("pi-byitunesid-1200361736.json");
        assert_eq!(
            parse_pi(200, &byid, Ask::Apple(DAILY)).unwrap(),
            Some(daily.clone())
        );
        assert_eq!(parse_pi(200, &byid, Ask::Apple(1)).unwrap(), None);
        // Asked for Apple ID 1, Podcast Index answers a show that claims it: only iTunes can
        // say whether the ID is a show, so it is asked first.
        let claimed = parse_pi(200, &read("pi-byitunesid-missing.json"), Ask::Apple(1)).unwrap();
        assert_eq!(claimed.and_then(|f| f.apple), Some(1));
        let byguid = read("pi-byguid-304b84f0.json");
        assert_eq!(
            parse_pi(
                200,
                &byguid,
                Ask::Guid("304b84f0-07b0-5265-b6b7-da5cf5aeb56e")
            )
            .unwrap(),
            Some(daily.clone())
        );
        assert_eq!(
            parse_pi(
                200,
                &read("pi-byguid-missing.json"),
                Ask::Guid("00000000-0000-5000-8000-000000000000")
            )
            .unwrap(),
            None
        );
        let byurl = read("pi-byfeedurl-54nAGcIl.json");
        assert_eq!(
            parse_pi(
                200,
                &byurl,
                Ask::Feed("https://feeds.simplecast.com/54nAGcIl/")
            )
            .unwrap(),
            Some(daily.clone())
        );
        assert_eq!(
            parse_pi(
                200,
                &byurl,
                Ask::Feed("https://feeds.simplecast.com/Sl5CSM3S")
            )
            .unwrap(),
            None
        );
        // The feed moved; its first URL is still its own (`originalUrl`, over http).
        let moved = parse_pi(
            200,
            &read("pi-byfeedurl-pc20.json"),
            Ask::Feed("https://mp3s.nashownotes.com/pc20rss.xml"),
        )
        .unwrap()
        .unwrap();
        assert_eq!((moved.id, moved.apple), (920666, Some(1584274529)));
        assert_eq!(
            parse_pi(
                400,
                &read("pi-byfeedurl-missing.json"),
                Ask::Feed("https://example.org/no-such-feed.xml")
            )
            .unwrap(),
            None
        );
        assert!(parse_pi(401, &read("pi-byitunesid-401.json"), Ask::Apple(DAILY)).is_err());
        // By feed ID.
        assert_eq!(
            pi_request(Ask::FeedId(743229)),
            "https://api.podcastindex.org/api/1.0/podcasts/byfeedid?id=743229"
        );
        assert_eq!(
            parse_pi(200, &read("pi-byfeedid-743229.json"), Ask::FeedId(743229)).unwrap(),
            Some(daily)
        );
        assert_eq!(
            parse_pi(200, &read("pi-byfeedid-743229.json"), Ask::FeedId(1)).unwrap(),
            None
        );
        assert_eq!(
            parse_pi(
                200,
                &read("pi-byfeedid-missing.json"),
                Ask::FeedId(999999999)
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn podcast_index_vouches_only_for_a_guids_own_feed() {
        let daily = parse_pi(
            200,
            &read("pi-byfeedurl-54nAGcIl.json"),
            Ask::Feed("https://feeds.simplecast.com/54nAGcIl"),
        )
        .unwrap()
        .unwrap();
        let guid = Ask::Guid("304b84f0-07b0-5265-b6b7-da5cf5aeb56e");
        let owner = parse_pi(200, &read("pi-byguid-304b84f0.json"), guid).unwrap();
        assert_eq!(vouched(&daily, owner.as_ref()), Some(GUID_DAILY));
        // A copy Podcast Index indexed carries The Daily's GUID; the GUID's own feed is The
        // Daily's, so the copy is not vouched for (and The Daily is not, seen from the copy).
        let copy_owner = parse_pi(200, &read("pi-byguid-304b84f0-copy.json"), guid).unwrap();
        assert_eq!(vouched(&daily, copy_owner.as_ref()), None);
        assert_eq!(vouched(&daily, None), None, "no owner, no vouch");
    }

    #[test]
    fn a_page_that_names_its_feed() {
        let mut doc = ResourceMeta::new("https://darknetdiaries.com/episode/180/", "web", 60);
        doc.set_type("Article");
        doc.attributes.insert(
            "feed".into(),
            "https://feeds.megaphone.fm/darknetdiaries".into(),
        );
        let s = Subject {
            id: "r".into(),
            uri: "https://darknetdiaries.com/episode/180/".into(),
            status: "ok".into(),
            type_name: Some("Article".into()),
            doc: Some(doc),
            keys: Vec::new(),
            first_seen_at: None,
        };
        assert_eq!(
            podcast(&s),
            Some(Podcast::Page {
                feed: "https://feeds.megaphone.fm/darknetdiaries".into(),
                page: "https://darknetdiaries.com/episode/180/".into(),
            })
        );
    }

    #[test]
    fn podcast_index_episodes() {
        assert_eq!(
            pi_episode_request(61953304662),
            "https://api.podcastindex.org/api/1.0/episodes/byid?id=61953304662"
        );
        let raw = read("pi-episode-61953304662.json");
        assert_eq!(
            parse_pi_episode(200, &raw, 61953304662).unwrap(),
            Some(IndexEpisode {
                guid: "1897475d-9b60-447d-ba9b-79aa09f59551".into(),
                feed: 743229
            })
        );
        assert_eq!(parse_pi_episode(200, &raw, 1).unwrap(), None);
        assert_eq!(
            parse_pi_episode(200, &read("pi-episode-missing.json"), 1).unwrap(),
            None
        );
    }

    #[test]
    fn the_shows_wikidata_item() {
        let links = media::links(&fixture::item("Q48741148"), WIKIDATA);
        let got: Vec<(&str, &str)> = links
            .iter()
            .map(|l| (l.site.as_str(), l.uri.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "Spotify",
                    "https://open.spotify.com/show/3IM0lmZxpFAY7CwMuv9H4g"
                ),
                ("Podcast Index", "https://podcastindex.org/podcast/743229"),
                ("Podchaser", "https://www.podchaser.com/podcasts/243479"),
                ("Pocket Casts", "https://pca.st/thedaily"),
                ("Castbox", "https://castbox.fm/channel/id463650"),
            ]
        );
    }
}
