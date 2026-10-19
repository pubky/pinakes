//! Podcasts: a show or an episode, by its Apple Podcasts link or its feed, and where else it is.
//!
//! - **Apple Podcasts** (iTunes lookup) confirms an Apple ID is a show, and finds an episode by
//!   its track ID or by its feed item's GUID (among the latest 200 episodes).
//! - **The feed** names its own `podcast:guid`. Only a feed that names none gets the GUID
//!   derived from its URL (UUIDv5 of the URL without its scheme, as the podcast namespace says).
//! - **Podcast Index** (with an operator's key and secret) joins a feed URL or an Apple ID to the
//!   other: its answer must name the URL, ID or GUID asked, and an Apple ID it names counts only
//!   once iTunes knows it as a show.
//! - **Wikidata** (the item with the show's Apple ID, P5842) adds Spotify, Podchaser, Pocket
//!   Casts and Castbox.
//!
//! Keys: `podcast:apple:{id}` and `podcast:guid:{uuid}` for a show, `podcast:apple_episode:{id}`
//! for an episode, and the show's `wikidata:Q…`.
use super::{
    media::{self, digits, property, slug, Property},
    Context, Fact, Found, Key, Link, Source, Subject,
};
use crate::{
    event::adapters::SecretKey,
    hubs::{self, wikidata},
    resolver::egress::FetchOptions,
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
    /// An Apple Podcasts show, or one of its episodes (`?i=`).
    Apple { show: u64, episode: Option<u64> },
    /// A feed's show, or one of its items (by the item's GUID).
    Feed { url: String, item: Option<String> },
}

/// The show (and episode) an Apple Podcasts link names:
/// `podcasts.apple.com/{cc}/podcast/{slug}/id{n}`, with or without the country and slug, the
/// same on `itunes.apple.com`, and `?i={episode}`.
pub fn apple(uri: &str) -> Option<(u64, Option<u64>)> {
    let url = url::Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || !matches!(url.host_str()?, "podcasts.apple.com" | "itunes.apple.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let s: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
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
fn number(v: &str) -> Option<u64> {
    digits(v).then(|| v.parse().ok()).flatten()
}

/// What a subject names: an Apple Podcasts link, or a resolved feed card (`PodcastSeries`, or a
/// `PodcastEpisode` with its item's GUID) and its feed.
pub fn podcast(subject: &Subject) -> Option<Podcast> {
    if let Some((show, episode)) = apple(&subject.uri) {
        return Some(Podcast::Apple { show, episode });
    }
    let doc = subject.doc.as_ref().filter(|_| subject.status == "ok")?;
    let url = doc
        .attributes
        .get("podcast_feed")
        .filter(|u| feed_name(u).is_some())?
        .clone();
    match subject.type_name.as_deref()? {
        "PodcastSeries" => Some(Podcast::Feed { url, item: None }),
        "PodcastEpisode" => {
            let item = doc
                .attributes
                .get("podcast_guid")
                .map(|g| g.trim())
                .filter(|g| !g.is_empty())?;
            Some(Podcast::Feed {
                url,
                item: Some(item.to_owned()),
            })
        }
        _ => None,
    }
}

/// A feed URL as the podcast namespace names it: without its scheme and trailing slashes
/// (`mp3s.nashownotes.com/pc20rss.xml`). Only plain http(s) URLs on the default port.
pub fn feed_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "https" | "http")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    let (_, rest) = url.trim().split_once("://")?;
    let rest = rest.trim_end_matches('/');
    (!rest.is_empty()).then(|| rest.to_owned())
}
/// Whether two feed URLs are the same feed (only their scheme, the host's case and trailing
/// slashes may differ).
fn same_feed(a: &str, b: &str) -> bool {
    let norm = |u: &str| {
        let name = feed_name(u)?;
        let (host, path) = name.split_once('/').unwrap_or((&name, ""));
        Some(format!("{}/{path}", host.to_ascii_lowercase()))
    };
    norm(a).is_some_and(|a| Some(a) == norm(b))
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

/// The feed's own `podcast:guid`, if it is a podcast feed (an RSS channel) that names one;
/// `Err` when the body is no RSS channel at all.
pub fn feed_guid(body: &[u8]) -> Result<Option<String>> {
    const NAMESPACES: &[&str] = &[
        "https://podcastindex.org/namespace/1.0",
        "http://podcastindex.org/namespace/1.0",
        "https://github.com/Podcastindex-org/podcast-namespace/blob/main/docs/1.0.md",
    ];
    let xml = roxmltree::Document::parse(std::str::from_utf8(body)?)?;
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
    Ok(match (guids.next(), guids.next()) {
        (Some(one), None) => one.text().and_then(guid),
        _ => None,
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

const PODCAST_INDEX: &str = "https://api.podcastindex.org/api/1.0";
/// What Podcast Index is asked.
#[derive(Clone, Copy, Debug)]
pub enum Ask<'a> {
    Apple(u64),
    Feed(&'a str),
    Guid(&'a str),
}
pub fn pi_request(ask: Ask) -> String {
    let enc = |v: &str| url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
    match ask {
        Ask::Apple(id) => format!("{PODCAST_INDEX}/podcasts/byitunesid?id={id}"),
        Ask::Feed(url) => format!("{PODCAST_INDEX}/podcasts/byfeedurl?url={}", enc(url)),
        Ask::Guid(g) => format!("{PODCAST_INDEX}/podcasts/byguid?guid={}", enc(g)),
    }
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
}
/// The feed Podcast Index answers, or `None` when it has none or answers another feed than the
/// one asked (its `url` or `originalUrl` must be the feed asked, its `itunesId` or
/// `podcastGuid` the one asked).
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
    };
    let theirs = match ask {
        Ask::Apple(asked) => found.apple == Some(asked),
        Ask::Feed(asked) => [&feed["url"], &feed["originalUrl"]]
            .iter()
            .filter_map(|u| u.as_str())
            .any(|u| same_feed(u, asked)),
        Ask::Guid(asked) => found.guid.as_deref() == Some(asked),
    };
    Ok(theirs.then_some(found))
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
/// A show's GUID: its key and fact.
pub fn show_guid(found: &mut Found, guid: &str, via: &str) {
    found.keys.push(key(format!("podcast:guid:{guid}"), via));
    found.facts.push(Fact::new("podcast_guid", guid));
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

/// The source: iTunes, the feed, Podcast Index and Wikidata.
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
    /// Podcast Index's answer, when the operator configured its key.
    async fn index(cx: &Context, ask: Ask<'_>) -> Result<Option<IndexFeed>> {
        let Some((key, secret)) = cx.config.podcast_index.pair() else {
            return Ok(None);
        };
        let options = FetchOptions {
            accept: Some("application/json".into()),
            headers: pi_headers(key, secret, chrono::Utc::now().timestamp()),
            sensitive: true,
            ..Default::default()
        };
        let f = cx.egress.fetch_with(&pi_request(ask), &options).await?;
        parse_pi(f.status, &f.body, ask)
    }
    /// The feed's own GUID, read from the feed at exactly this URL (a redirect may be another
    /// publisher's feed), or the GUID derived from its URL when it names none. `None` when the
    /// feed can't be read as one (e.g. a body over the page cap); a retry is passed on.
    async fn feed(cx: &Context, url: &str) -> Result<Option<(String, &'static str)>> {
        let f = match cx.egress.discover(url, &Default::default(), 3600).await {
            Ok(f) => f,
            Err(e) if e.is::<crate::resolver::RetryLater>() => return Err(e),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "podcast feed unreadable");
                return Ok(None);
            }
        };
        if f.status != 200 || !same_feed(f.url.as_str(), url) {
            return Ok(None);
        }
        let body = f.body.clone();
        let own = tokio::task::spawn_blocking(move || feed_guid(&body)).await?;
        Ok(match own {
            Ok(Some(guid)) => Some((guid, "feed")),
            Ok(None) => derived_guid(url).map(|g| (g, "feed")),
            Err(_) => None,
        })
    }
    /// The show's Wikidata item, by its Apple ID.
    async fn wikidata(cx: &Context, found: &mut Found, show: u64) -> Result<()> {
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
        Ok(())
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
        let sf = cx.config.also_on.apple_storefront.clone();
        let mut found = Found::default();
        match podcast(subject) {
            None => {}
            Some(Podcast::Apple {
                show,
                episode: Some(track),
            }) => {
                if Self::episodes(cx, show)
                    .await?
                    .iter()
                    .any(|e| e.track == track)
                {
                    apple_episode(&mut found, show, track, &sf);
                }
            }
            Some(Podcast::Apple {
                show,
                episode: None,
            }) => {
                if !Self::show_known(cx, show).await? {
                    return Ok(found);
                }
                apple_show(&mut found, show, &sf);
                if let Some(feed) = Self::index(cx, Ask::Apple(show)).await? {
                    add(
                        &mut found,
                        Link::new(
                            format!("https://podcastindex.org/podcast/{}", feed.id),
                            "Podcast Index",
                            "podcastindex",
                        ),
                    );
                    if let Some(guid) = &feed.guid {
                        show_guid(&mut found, guid, "podcastindex");
                    }
                }
                Self::wikidata(cx, &mut found, show).await?;
            }
            Some(Podcast::Feed { url, item }) => {
                let indexed = Self::index(cx, Ask::Feed(&url)).await?;
                // An Apple ID Podcast Index names counts once iTunes knows it.
                let apple = match indexed.as_ref().and_then(|f| f.apple) {
                    Some(show) if Self::show_known(cx, show).await? => Some(show),
                    _ => None,
                };
                if let Some(item) = item {
                    if let Some(show) = apple {
                        if let Some(e) = by_guid(&Self::episodes(cx, show).await?, &item) {
                            apple_episode(&mut found, show, e.track, &sf);
                        }
                    }
                    return Ok(found);
                }
                // The feed's own GUID first; Podcast Index's for a feed that can't be read; the
                // derived one last.
                let own = Self::feed(cx, &url).await?;
                let guid = match (&own, &indexed) {
                    (Some((g, via)), _) => Some((g.clone(), *via)),
                    (None, Some(IndexFeed { guid: Some(g), .. })) => {
                        Some((g.clone(), "podcastindex"))
                    }
                    _ => None,
                };
                if let Some((guid, via)) = guid {
                    show_guid(&mut found, &guid, via);
                }
                if let Some(feed) = &indexed {
                    add(
                        &mut found,
                        Link::new(
                            format!("https://podcastindex.org/podcast/{}", feed.id),
                            "Podcast Index",
                            "podcastindex",
                        ),
                    );
                }
                if let Some(show) = apple {
                    apple_show(&mut found, show, &sf);
                    Self::wikidata(cx, &mut found, show).await?;
                }
            }
        }
        Ok(found)
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
                item: None
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
                item: Some("ep-1".into())
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
    }

    #[test]
    fn a_feed_names_its_own_guid() {
        // Podcasting 2.0 moved to feeds.podcastindex.org but keeps the GUID of its first URL:
        // the GUID derived from where it lives now would be another show's.
        let own = feed_guid(&read("feed-pc20.xml.gz")).unwrap();
        assert_eq!(own.as_deref(), Some("917393e3-1b1e-5cef-ace4-edaa54e1f810"));
        assert_ne!(derived_guid("https://feeds.podcastindex.org/pc20.xml"), own);
        let plain = br#"<rss version="2.0"><channel><title>T</title><item><guid>a</guid></item></channel></rss>"#;
        assert_eq!(feed_guid(plain).unwrap(), None);
        let other = br#"<rss xmlns:x="https://example.org/ns"><channel><x:guid>917393e3-1b1e-5cef-ace4-edaa54e1f810</x:guid></channel></rss>"#;
        assert_eq!(feed_guid(other).unwrap(), None, "another namespace's guid");
        assert!(feed_guid(b"<html><body>no feed</body></html>").is_err());
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
            Some(daily)
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
        assert!(parse_pi(401, b"{}", Ask::Apple(DAILY)).is_err());
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
