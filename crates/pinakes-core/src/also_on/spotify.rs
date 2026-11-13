//! Spotify's own record of its own album or track, from Spotify's Web API: an album's barcode
//! (`external_ids.upc`) as the key `upc:…` and a track's ISRC (`external_ids.isrc`) as `isrc:…`,
//! so the lookups by barcode and ISRC after it (MusicBrainz, Deezer, iTunes) join the rest. An
//! ISRC MusicBrainz gives to several recordings that agree keys the one that stands for them,
//! one on recordings that disagree the one the track's duration picks, else is a fact only, as
//! every service's own ISRC is (`music_sources::isrc_match`).
//!
//! The API is asked only with an operator's app credentials (`[spotify]`, or
//! `PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`), through the
//! client-credentials grant (`client_credentials`): no user signs in, and nothing but the public
//! catalogue is read. While this source is on, `editions` asks it which album a Spotify track a
//! card may show is on; where the API does not answer, and only with `[also_on] spotify_pages`,
//! the track's public page (its `music:album`).
use super::{
    client_credentials,
    spotify_api::{API, SPOTIFY},
    Context, Fact, Found, Key, Source, Subject,
};
use crate::music::{self, Level, Service};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

/// The credentials and endpoints every source that asks Spotify shares (`spotify_api`).
pub use super::spotify_api::SpotifyConfig;

/// An album or a track, as the API names its collection, the external ID read and the key's
/// namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Album,
    Track,
}
impl Kind {
    fn of(link: &music::Link) -> Option<Self> {
        match (link.service, link.level) {
            (Service::Spotify, Level::Album) => Some(Self::Album),
            (Service::Spotify, Level::Track) => Some(Self::Track),
            _ => None,
        }
    }
    fn collection(self) -> &'static str {
        match self {
            Self::Album => "albums",
            Self::Track => "tracks",
        }
    }
    /// The type the API gives the object.
    fn type_name(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Track => "track",
        }
    }
    fn external_id(self) -> &'static str {
        match self {
            Self::Album => "upc",
            Self::Track => "isrc",
        }
    }
    fn namespace(self) -> &'static str {
        match self {
            Self::Album => "upc",
            Self::Track => "isrc",
        }
    }
    fn normal(self, value: &str) -> Option<String> {
        match self {
            Self::Album => crate::barcode::gtin13(value),
            Self::Track => music::isrc(value),
        }
    }
}

pub struct Spotify;
#[async_trait]
impl Source for Spotify {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn budgets(&self) -> &'static [(&'static str, f64)] {
        // Spotify publishes no figure (a rolling 30-second window) and answers 429 with a time
        // to wait: a cautious 2/s.
        &[("api.spotify.com", 2.0)]
    }
    fn id(&self) -> &'static str {
        "spotify"
    }
    fn section(&self) -> Option<super::Section> {
        Some(super::Section::of::<SpotifyConfig>())
    }
    fn ready(&self, config: &crate::config::Config) -> bool {
        config.section::<SpotifyConfig>().pair().is_some()
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| Kind::of(&l).is_some())
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some((kind, link)) = music::parse(&subject.uri).and_then(|l| Some((Kind::of(&l)?, l)))
        else {
            return Ok(Found::default());
        };
        let mut found = Found::default();
        let Some(record) = object(cx, kind, &link.id).await? else {
            return Ok(found);
        };
        let Some(value) = record["external_ids"][kind.external_id()]
            .as_str()
            .and_then(|v| kind.normal(v))
        else {
            return Ok(found);
        };
        let namespace = kind.namespace();
        match kind {
            Kind::Album => found.keys.push(Key {
                key: format!("{namespace}:{value}"),
                via: "spotify".into(),
            }),
            // A track's ISRC names its recording as the track's length tells (`isrc_match`).
            Kind::Track => {
                let duration = record["duration_ms"]
                    .as_u64()
                    .map(|ms| ((ms + 500) / 1000) as i64)
                    .filter(|s| (1..=86_400).contains(s));
                let named = crate::music_sources::isrc_match(cx, &value, duration).await?;
                found
                    .keys
                    .extend(crate::music_sources::isrc_key(&value, &named, "spotify"));
            }
        }
        found.facts.push(Fact::new(namespace, value));
        Ok(found)
    }
}

/// The album a Spotify track is on: the Web API's record of the track (`/v1/tracks/{id}`) with
/// the operator's credentials, else, where `page` allows (a card's own track, a link someone
/// referenced, or any with `[also_on] spotify_pages`), the album its public page names
/// (`music:album`). Spotify refusing the credentials, with no page to read, is an error
/// ([`refused`]): the caller keeps what it knew.
pub(crate) async fn track_album(cx: &Context, track: &str, page: bool) -> Result<Option<String>> {
    let mut refusal = None;
    if Spotify.ready(&cx.config) {
        match object(cx, Kind::Track, track).await {
            Ok(record) => {
                return Ok(record.and_then(|v| v["album"]["id"].as_str().map(str::to_owned)))
            }
            Err(error) if refused(&error) => refusal = Some(error),
            Err(error) => return Err(error),
        }
    }
    if !page {
        return refusal.map_or(Ok(None), Err);
    }
    Ok(page_links(cx, "track", track, "music:album", Level::Album)
        .await?
        .and_then(|albums| albums.into_iter().next()))
}
/// Whether Spotify can say which album a track is on: its credentials, or its pages allowed.
pub(crate) fn places(config: &crate::config::Config) -> bool {
    Spotify.ready(config) || config.also_on.spotify_pages
}
/// Whether an error is Spotify refusing the credentials (its token or its API).
pub(crate) fn refused(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::resolver::RetryLater>()
        .is_some_and(|r| r.class == SPOTIFY.auth_class)
}
/// The Spotify objects at `level` a public page names in its `<meta name="{meta}">` tags.
async fn page_links(
    cx: &Context,
    kind: &str,
    id: &str,
    meta: &'static str,
    level: Level,
) -> Result<Option<Vec<String>>> {
    let url = format!("https://open.spotify.com/{kind}/{id}");
    let fetched = cx
        .egress
        .discover(
            &url,
            &crate::resolver::egress::FetchOptions::default(),
            3600,
        )
        .await?;
    match fetched.status {
        200 => {}
        429 | 500..=599 => {
            return Err(crate::resolver::RetryLater {
                class: "spotify_busy",
                seconds: fetched.retry_after_secs.clamp(60, 3600),
            }
            .into())
        }
        _ => return Ok(None),
    }
    let body = String::from_utf8_lossy(&fetched.body).into_owned();
    Ok(Some(
        crate::resolver::html::blocking(move || meta_links(&body, meta, level)).await?,
    ))
}
/// The IDs of the Spotify objects at `level` a page's `<meta name="{meta}">` tags name.
pub fn meta_links(html: &str, meta: &str, level: Level) -> Vec<String> {
    let Ok(document) = crate::resolver::html::document(html) else {
        return vec![];
    };
    let selector = scraper::Selector::parse(&format!("meta[name='{meta}'][content]"))
        .expect("a fixed selector");
    document
        .select(&selector)
        .filter_map(|m| music::parse(m.value().attr("content")?))
        .filter(|l| l.service == Service::Spotify && l.level == level)
        .map(|l| l.id)
        .collect()
}

/// One album or track of the catalogue (`/v1/{albums|tracks}/{id}`); `None` when Spotify has
/// none. Its answer is shared for an hour.
async fn object(cx: &Context, kind: Kind, id: &str) -> Result<Option<Value>> {
    let (client_id, secret) = cx
        .config
        .section::<SpotifyConfig>()
        .pair()
        .context("no Spotify credentials")?;
    let url = format!("{API}/{}/{id}", kind.collection());
    let record = client_credentials::object(
        &SPOTIFY,
        cx,
        (&client_id, &secret),
        &url,
        ("application/json", &[]),
    )
    .await?;
    // Only the object asked for.
    Ok(record
        .filter(|v| v["id"].as_str() == Some(id) && v["type"].as_str() == Some(kind.type_name())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[spotify]` is Spotify's own section: its pair from the file or its own variables, both
    /// needed, secret in every form the config shows.
    #[test]
    fn the_spotify_section_takes_its_own_variables_and_stays_secret() {
        use crate::config::{apply_env, apply_named_env, Config};
        let mut value = Config::defaults().unwrap();
        let vars = [
            ("PINAKES_SPOTIFY_CLIENT_ID", "ABCDEFID"),
            ("PINAKES_SPOTIFY_CLIENT_SECRET", "ABCDEFSECRET"),
            ("PINAKES_OTHER", "x"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        apply_env(&mut value, vars.clone()).unwrap();
        apply_named_env(&mut value, vars).unwrap();
        let cfg = Config::from_value(value).unwrap();
        assert_eq!(
            cfg.section::<SpotifyConfig>().pair(),
            Some(("ABCDEFID".into(), "ABCDEFSECRET".into()))
        );
        assert!(Spotify.ready(&cfg));
        let shown = format!("{cfg:?} {}", serde_json::to_string(&cfg).unwrap());
        assert!(!shown.contains("ABCDEF"), "{shown}");
        // One half is no pair, and Spotify is never asked.
        let half: SpotifyConfig =
            serde_json::from_value(serde_json::json!({"client_id": "ABCDEFID"})).unwrap();
        assert_eq!(half.pair(), None);
        assert!(!Spotify.ready(&Config::default()));
    }
    /// A 401 is the token's, never the object's: it is not cached for the pair, so the next
    /// request asks Spotify again (with a new token, after the hold a fresh token's rejection
    /// starts).
    #[tokio::test]
    async fn a_401_is_never_cached() {
        use crate::resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport};
        use std::net::SocketAddr;
        struct Public;
        #[async_trait]
        impl Dns for Public {
            async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
                Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
            }
        }
        struct Rejecting;
        #[async_trait]
        impl Transport for Rejecting {
            async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> Result<Fetched> {
                self.get_with(url, a, &FetchOptions::default()).await
            }
            async fn get_with(
                &self,
                url: &url::Url,
                _: &[SocketAddr],
                _: &FetchOptions,
            ) -> Result<Fetched> {
                let (status, body) = if url.host_str() == Some("accounts.spotify.com") {
                    (200, r#"{"access_token":"t1","expires_in":3600}"#)
                } else {
                    (
                        401,
                        r#"{"error":{"status":401,"message":"The access token expired"}}"#,
                    )
                };
                Ok(Fetched::answer(
                    url.clone(),
                    status,
                    "application/json".into(),
                    body.as_bytes().to_vec(),
                ))
            }
        }
        let mut config = crate::config::Config::default();
        config.sections.set(
            "spotify",
            serde_json::json!({"client_id": "reject-id", "client_secret": "reject-secret"}),
        );
        let cx = Context {
            egress: Egress::with_backends(
                Default::default(),
                std::sync::Arc::new(Public),
                std::sync::Arc::new(Rejecting),
            ),
            config: std::sync::Arc::new(config),
        };
        let error = object(&cx, Kind::Track, "7tFiyTwD0nx5a1eklYtX2J")
            .await
            .unwrap_err();
        // A token rejected within a minute of its issue: the credentials are held as refused.
        assert_eq!(
            error
                .downcast_ref::<crate::resolver::RetryLater>()
                .map(|r| r.class),
            Some("spotify_auth")
        );
        let slot = client_credentials::slot_key(&SPOTIFY, "reject-id", "reject-secret");
        let options = client_credentials::api_options("t1", &slot, "application/json", &[]);
        assert!(!cx
            .egress
            .discovered(&format!("{API}/tracks/7tFiyTwD0nx5a1eklYtX2J"), &options)
            .await
            .unwrap());
    }
    #[test]
    fn only_spotify_albums_and_tracks_apply() {
        let kind = |uri: &str| music::parse(uri).and_then(|l| Kind::of(&l));
        assert_eq!(
            kind("https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE"),
            Some(Kind::Album)
        );
        assert_eq!(
            kind("spotify:track:7tFiyTwD0nx5a1eklYtX2J"),
            Some(Kind::Track)
        );
        assert_eq!(
            kind("https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb"),
            None
        );
        assert_eq!(
            kind("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"),
            None
        );
        assert_eq!(kind("https://tidal.com/album/199574241"), None);
        assert_eq!(
            Kind::Album.normal("602547288233").as_deref(),
            Some("0602547288233")
        );
    }
}
