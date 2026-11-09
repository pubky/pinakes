//! What the sources that ask Spotify's Web API share (`spotify`, `spotify_isrc`): the operator's
//! app credentials (`[spotify]`, or `PINAKES_SPOTIFY_CLIENT_ID` and
//! `PINAKES_SPOTIFY_CLIENT_SECRET`), the API's base and its token endpoint
//! (`client_credentials`). Either source goes without the other.
use super::client_credentials::Api;
use crate::event::adapters::SecretKey;
use serde::{Deserialize, Serialize};

/// Spotify's app credentials (`[spotify] client_id` and `client_secret`, or
/// `PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`). Without both, Spotify is
/// never asked.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpotifyConfig {
    pub client_id: Option<SecretKey>,
    pub client_secret: Option<SecretKey>,
}
impl SpotifyConfig {
    pub fn pair(&self) -> Option<(String, String)> {
        match (self.client_id.as_deref(), self.client_secret.as_deref()) {
            (Some(id), Some(secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => {
                Some((id.trim().to_owned(), secret.trim().to_owned()))
            }
            _ => None,
        }
    }
}
impl super::OwnSection for SpotifyConfig {
    const NAME: &'static str = "spotify";
    const VARIABLES: &'static [(&'static str, &'static str)] = &[
        ("PINAKES_SPOTIFY_CLIENT_ID", "client_id"),
        ("PINAKES_SPOTIFY_CLIENT_SECRET", "client_secret"),
    ];
}

/// Spotify's Web API.
pub const API: &str = "https://api.spotify.com/v1";
/// Spotify's Web API and its token endpoint.
pub(crate) const SPOTIFY: Api = Api {
    service: "spotify",
    label: "Spotify",
    auth: "https://accounts.spotify.com/api/token",
    auth_class: "spotify_auth",
    busy_class: "spotify_busy",
};
/// Whether the operator's Spotify pair is configured: the sources that ask Spotify run only then.
pub fn configured(config: &crate::config::Config) -> bool {
    config.section::<SpotifyConfig>().pair().is_some()
}
