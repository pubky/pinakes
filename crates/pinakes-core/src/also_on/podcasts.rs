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
    media::{self, digits, property, slug, Property},
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

/// What a subject names: an Apple Podcasts, player or Podcast Index link, or a resolved feed
/// card (`PodcastSeries`, or a `PodcastEpisode` with its item's GUID) and its feed.
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
    let url = doc
        .attributes
        .get("podcast_feed")
        .filter(|u| feed_name(u).is_some())?
        .clone();
    let page = subject.uri.clone();
    match subject.type_name.as_deref()? {
        "PodcastSeries" => Some(Podcast::Feed {
            url,
            item: None,
            page,
        }),
        "PodcastEpisode" => {
            let item = doc
                .attributes
                .get("podcast_guid")
                .map(|g| g.trim())
                .filter(|g| !g.is_empty())?;
            Some(Podcast::Feed {
                url,
                item: Some(item.to_owned()),
                page,
            })
        }
        _ => None,
    }
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

/// What a podcast feed says: its own `podcast:guid`, and the `<link>` of the item with a given
/// GUID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedRead {
    pub guid: Option<String>,
    pub item_link: Option<String>,
}
/// Read an RSS feed (any encoding is read as UTF-8, invalid bytes replaced: the GUIDs and links
/// are ASCII). `Err` when the body is no RSS channel at all.
pub fn read_feed(body: &[u8], item: Option<&str>) -> Result<FeedRead> {
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
    // The one item with this GUID (several are ambiguous).
    let item_link = item.and_then(|wanted| {
        let mut items = channel
            .children()
            .filter(|n| n.has_tag_name("item") && field(*n, "guid").as_deref() == Some(wanted));
        match (items.next(), items.next()) {
            (Some(one), None) => field(one, "link"),
            _ => None,
        }
    });
    Ok(FeedRead {
        guid: own,
        item_link,
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
/// Whether iTunes knows `show` as a podcast (its one result, of kind `podcast`).
pub fn parse_show(status: u16, raw: &[u8], show: u64) -> Result<bool> {
    let Some(v) = media::json(status, raw, &[404])? else {
        return Ok(false);
    };
    let results = v["results"].as_array().context("no results")?;
    Ok(matches!(results.as_slice(),
        [one] if one["kind"] == "podcast" && one["collectionId"].as_u64() == Some(show)))
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
/// A show's Apple ID: its key and the pages that open it by that ID.
pub fn apple_show(found: &mut Found, show: u64, storefront: &str) {
    found
        .keys
        .push(key(format!("podcast:apple:{show}"), "itunes"));
    for (uri, site) in [
        (
            format!("https://podcasts.apple.com/{storefront}/podcast/id{show}"),
            "Apple Podcasts",
        ),
        (format!("https://pca.st/itunes/{show}"), "Pocket Casts"),
        (format!("https://castro.fm/itunes/{show}"), "Castro"),
    ] {
        add(found, Link::new(uri, site, "itunes"));
    }
}
/// A show's Podcast Index feed: its page, and the GUID Podcast Index vouches for as a key.
fn index_show(found: &mut Found, feed: &IndexFeed) {
    add(
        found,
        Link::new(
            format!("https://podcastindex.org/podcast/{}", feed.id),
            "Podcast Index",
            "podcastindex",
        ),
    );
    if let Some(guid) = &feed.guid {
        found
            .keys
            .push(key(format!("podcast:guid:{guid}"), "podcastindex"));
    }
}
/// An episode's Apple track: its key and page.
pub fn apple_episode(found: &mut Found, show: u64, track: u64, storefront: &str) {
    found
        .keys
        .push(key(format!("podcast:apple_episode:{track}"), "itunes"));
    add(
        found,
        Link::new(
            format!("https://podcasts.apple.com/{storefront}/podcast/id{show}?i={track}"),
            "Apple Podcasts",
            "itunes",
        ),
    );
}

/// What a failed secondary request leaves: a retry to pass on when nothing else was found.
#[derive(Default)]
struct Deferred(Option<anyhow::Error>);
impl Deferred {
    /// The answer, or `None` after counting the failure; a retry is kept for later.
    fn take<T>(&mut self, service: &'static str, result: Result<Option<T>>) -> Option<T> {
        match result {
            Ok(v) => v,
            Err(error) => {
                let class = crate::resolver::error_class(&error);
                crate::telemetry::also_on_service_error(service, class);
                tracing::debug!(service, class, "also-on service failed; the lookup goes on");
                if error.is::<RetryLater>() && self.0.is_none() {
                    self.0 = Some(error);
                }
                None
            }
        }
    }
    /// The lookup's answer: what was found, or the retry when nothing was.
    fn finish(self, found: Found) -> Result<Found> {
        match self.0 {
            Some(retry) if found == Found::default() => Err(retry),
            _ => Ok(found),
        }
    }
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
    /// Whether iTunes knows the show.
    async fn show_known(cx: &Context, show: u64) -> Result<bool> {
        let sf = &cx.config.also_on.apple_storefront;
        let f = Self::itunes(cx, &show_request(show, sf)).await?;
        parse_show(f.status, &f.body, show)
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
        let f = match cx.egress.discover(url, &Default::default(), 3600).await {
            Ok(f) => f,
            Err(e) if e.is::<RetryLater>() => return Err(e),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "podcast feed unreadable");
                return Ok(None);
            }
        };
        if f.status != 200 || !same_feed(f.url.as_str(), url) {
            return Ok(None);
        }
        let (body, item) = (f.body.clone(), item.map(str::to_owned));
        Ok(
            tokio::task::spawn_blocking(move || read_feed(&body, item.as_deref()))
                .await?
                .ok(),
        )
    }
    /// The show's Wikidata item, by its Apple ID.
    async fn wikidata(cx: &Context, found: &mut Found, show: u64) -> Result<Option<()>> {
        if let wikidata::Found::One(item) =
            wikidata::item_with(&cx.egress, "P5842", &show.to_string()).await?
        {
            found
                .keys
                .extend(hubs::key::wikidata(&item.qid).map(|k| key(k, "wikidata")));
            for l in media::links(&item, WIKIDATA) {
                add(found, l);
            }
        }
        Ok(Some(()))
    }
    /// Everything a show confirmed by iTunes gets: its Apple ID, Podcast Index's feed (the one
    /// that names the Apple ID, unless given) and its Wikidata item. Returns the vouched GUID.
    async fn show(
        cx: &Context,
        found: &mut Found,
        deferred: &mut Deferred,
        show: u64,
        indexed: Option<IndexFeed>,
    ) -> Result<Option<String>> {
        apple_show(found, show, &cx.config.also_on.apple_storefront);
        let indexed = match indexed {
            Some(feed) => Some(feed),
            None => deferred.take("podcastindex", Self::index(cx, Ask::Apple(show)).await),
        };
        if let Some(feed) = &indexed {
            index_show(found, feed);
        }
        deferred.take("wikidata", Self::wikidata(cx, found, show).await);
        Ok(indexed.and_then(|f| f.guid))
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
                if deferred.take("itunes", Self::show_known(cx, *show).await.map(Some))
                    != Some(true)
                {
                    // A player link names its Apple ID in the URL itself.
                    if *site != "Apple Podcasts" {
                        found.keys.push(key(format!("podcast:apple:{show}"), "url"));
                    }
                    return deferred.finish(found);
                }
                match episode {
                    None => {
                        let guid = Self::show(cx, &mut found, &mut deferred, *show, None).await?;
                        if let Some(g) = guid {
                            found.facts.push(Fact::new("podcast_guid", g));
                        }
                    }
                    Some(track) => {
                        let indexed =
                            deferred.take("podcastindex", Self::index(cx, Ask::Apple(*show)).await);
                        let show_guid = indexed.and_then(|f| f.guid);
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
                let apple = match indexed.apple {
                    Some(show) => deferred
                        .take("itunes", Self::show_known(cx, show).await.map(Some))
                        .filter(|known| *known)
                        .map(|_| show),
                    None => None,
                };
                match episode {
                    None => {
                        let guid = indexed.guid.clone();
                        if let Some(url) = &indexed.url {
                            add(
                                &mut found,
                                Link::new(url.clone(), "Podcast feed", "podcastindex"),
                            );
                        }
                        match apple {
                            Some(show) => {
                                Self::show(cx, &mut found, &mut deferred, show, Some(indexed))
                                    .await?;
                            }
                            None => index_show(&mut found, &indexed),
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
                                indexed.guid.as_deref(),
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
                // An Apple ID Podcast Index names counts once iTunes knows it.
                let apple = match indexed.as_ref().and_then(|f| f.apple) {
                    Some(show) => deferred
                        .take("itunes", Self::show_known(cx, show).await.map(Some))
                        .filter(|known| *known)
                        .map(|_| show),
                    None => None,
                };
                let vouched = indexed.as_ref().and_then(|f| f.guid.clone());
                match item {
                    None => {
                        let own = deferred
                            .take("feed", Self::feed(cx, url, None).await)
                            .and_then(|r| r.guid);
                        match (apple, indexed) {
                            (Some(show), Some(feed)) => {
                                Self::show(cx, &mut found, &mut deferred, show, Some(feed)).await?;
                            }
                            (None, Some(feed)) => index_show(&mut found, &feed),
                            _ => {}
                        }
                        // The fact: the feed's own GUID, then Podcast Index's, then the derived one.
                        if let Some(g) = own.or(vouched).or_else(|| derived_guid(url)) {
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
            read_feed(&pc20, Some("PC20-275"))
                .unwrap()
                .item_link
                .as_deref(),
            Some("https://www.podpage.com/podcasting-20/off-the-rails/")
        );
        assert_eq!(read_feed(&pc20, Some("PC20-0")).unwrap().item_link, None);
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
        assert!(parse_show(200, &read("itunes-podcast-1200361736.json"), DAILY).unwrap());
        assert!(!parse_show(200, &read("itunes-podcast-1200361736.json"), 1).unwrap());
        assert!(!parse_show(200, &read("itunes-podcast-missing.json"), 1).unwrap());
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
