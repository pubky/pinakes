//! The Podcast Index API, which the also-on `podcasts` source and the official `podcast_index`
//! source both read: the operator's key and secret (`[podcast_index]`), its requests and their
//! signed headers, and the Podcast Index page a link names.
use crate::event::adapters::SecretKey;
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
    pub(crate) fn pair(&self) -> Option<(&str, &str)> {
        match (self.key.as_deref(), self.secret.as_deref()) {
            (Some(k), Some(s)) if !k.trim().is_empty() && !s.trim().is_empty() => {
                Some((k.trim(), s.trim()))
            }
            _ => None,
        }
    }
}

/// The feed (and episode) a Podcast Index page names: `podcastindex.org/podcast/{feed id}`,
/// with `?episode={episode id}`.
pub fn index_page(uri: &str) -> Option<(u64, Option<u64>)> {
    let url = url::Url::parse(uri).ok()?;
    let page = matches!(url.scheme(), "https" | "http")
        && matches!(url.host_str()?, "podcastindex.org" | "www.podcastindex.org")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if !page {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let feed = match segments.as_slice() {
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
    crate::also_on::media::digits(v)
        .then(|| v.parse().ok())
        .flatten()
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
/// The request for `ask`. None for a feed whose link carries a credential: Podcast Index is never
/// sent one.
pub fn pi_request(ask: Ask) -> Option<String> {
    let enc = |v: &str| url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
    Some(match ask {
        Ask::Apple(id) => format!("{PODCAST_INDEX}/podcasts/byitunesid?id={id}"),
        Ask::Feed(url) if crate::capability::is_capability(url) => return None,
        Ask::Feed(url) => format!("{PODCAST_INDEX}/podcasts/byfeedurl?url={}", enc(url)),
        Ask::Guid(g) => format!("{PODCAST_INDEX}/podcasts/byguid?guid={}", enc(g)),
        Ask::FeedId(id) => format!("{PODCAST_INDEX}/podcasts/byfeedid?id={id}"),
    })
}
/// Podcast Index's episode record.
pub fn pi_episode_request(id: u64) -> String {
    format!("{PODCAST_INDEX}/episodes/byid?id={id}")
}
/// Podcast Index's episode with this item GUID in its feed `feed`.
pub fn pi_episode_by_guid(guid: &str, feed: u64) -> String {
    let guid = url::form_urlencoded::byte_serialize(guid.as_bytes()).collect::<String>();
    format!("{PODCAST_INDEX}/episodes/byguid?guid={guid}&feedid={feed}")
}
/// An episode's enclosure as Podcast Index records it (`enclosureUrl`, `enclosureType`,
/// `duration`), when it is a public stream.
pub fn pi_stream(e: &serde_json::Value) -> Option<crate::model::Stream> {
    let duration = match &e["duration"] {
        serde_json::Value::String(s) => s.trim().parse().ok(),
        v => v.as_u64(),
    };
    crate::model::Stream::public(
        e["enclosureUrl"].as_str()?,
        e["enclosureType"].as_str()?,
        duration,
        false,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Podcast Index is never asked about a feed whose link carries a credential.
    #[test]
    fn a_feed_that_carries_a_credential_is_never_asked_about() {
        assert!(pi_request(Ask::Feed("https://feeds.example/rss?auth=x")).is_none());
        assert!(pi_request(Ask::Feed("https://feeds.example/rss")).is_some());
    }
    #[test]
    fn podcast_index_requests_are_signed() {
        assert_eq!(
            pi_request(Ask::Feed("https://feeds.simplecast.com/54nAGcIl")).unwrap(),
            "https://api.podcastindex.org/api/1.0/podcasts/byfeedurl?url=https%3A%2F%2Ffeeds.simplecast.com%2F54nAGcIl"
        );
        assert_eq!(
            pi_request(Ask::Apple(1200361736)).unwrap(),
            "https://api.podcastindex.org/api/1.0/podcasts/byitunesid?id=1200361736"
        );
        assert_eq!(
            pi_request(Ask::Guid("304b84f0-07b0-5265-b6b7-da5cf5aeb56e")).unwrap(),
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
}
