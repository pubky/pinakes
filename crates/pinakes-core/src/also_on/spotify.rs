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
use super::{spotify_api, Context, Fact, Found, Key, Source, Subject};
use crate::{
    music::{self, Level, Service},
    music_sources::Kind,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// The credentials and endpoints every source that asks Spotify shares (`spotify_api`).
pub use super::spotify_api::SpotifyConfig;

/// The kind of a Spotify album or track link.
fn kind_of(link: &music::Link) -> Option<Kind> {
    match (link.service, link.level) {
        (Service::Spotify, Level::Album) => Some(Kind::Album),
        (Service::Spotify, Level::Track) => Some(Kind::Track),
        _ => None,
    }
}
/// The type the API gives the object.
fn type_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Album => "album",
        Kind::Track => "track",
    }
}
/// The external ID read: an album's barcode or a track's ISRC.
fn external_id(kind: Kind) -> &'static str {
    match kind {
        Kind::Album => "upc",
        Kind::Track => "isrc",
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
        music::parse(&subject.uri).is_some_and(|l| kind_of(&l).is_some())
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some((kind, link)) = music::parse(&subject.uri).and_then(|l| Some((kind_of(&l)?, l)))
        else {
            return Ok(Found::default());
        };
        let mut found = Found::default();
        let Some(record) = object(cx, kind, &link.id).await? else {
            return Ok(found);
        };
        let Some(value) = record["external_ids"][external_id(kind)]
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

/// One album or track of the catalogue (`spotify_api::object`).
async fn object(cx: &Context, kind: Kind, id: &str) -> Result<Option<Value>> {
    spotify_api::object(cx, kind.collection(), type_name(kind), id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::also_on::{
        client_credentials,
        spotify_api::{API, SPOTIFY},
    };

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
        let kind = |uri: &str| music::parse(uri).and_then(|l| kind_of(&l));
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
