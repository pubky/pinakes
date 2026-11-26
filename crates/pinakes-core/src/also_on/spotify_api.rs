//! What the sources that ask Spotify share (`spotify`, `spotify_isrc`, and `itunes_song` and
//! `editions` for a Spotify track): the operator's app credentials (`[spotify]`, or
//! `PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`), the Web API's base, its
//! token endpoint (`client_credentials`), one object of the catalogue and the album a track is
//! on. Each of those sources goes without the others.
use super::{
    client_credentials::{self, Api},
    Context,
};
use crate::{
    event::adapters::SecretKey,
    music::{self, Level, Service},
};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
        client_credentials::pair(self.client_id.as_deref(), self.client_secret.as_deref())
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

/// One object of the catalogue (`/v1/{collection}/{id}`, of the type the API names `type_name`);
/// `None` when Spotify has none. Its answer is shared for an hour.
pub(crate) async fn object(
    cx: &Context,
    collection: &str,
    type_name: &str,
    id: &str,
) -> Result<Option<Value>> {
    let (client_id, secret) = cx
        .config
        .section::<SpotifyConfig>()
        .pair()
        .context("no Spotify credentials")?;
    let url = format!("{API}/{collection}/{id}");
    let record = client_credentials::object(
        &SPOTIFY,
        cx,
        (&client_id, &secret),
        &url,
        ("application/json", &[]),
    )
    .await?;
    // Only the object asked for.
    Ok(record.filter(|v| v["id"].as_str() == Some(id) && v["type"].as_str() == Some(type_name)))
}

/// The album a Spotify track is on: the Web API's record of the track (`/v1/tracks/{id}`) with
/// the operator's credentials, else, where `page` allows (a card's own track, a link someone
/// referenced, or any with `[also_on] spotify_pages`), the album its public page names
/// (`music:album`). Spotify refusing the credentials, with no page to read, is an error
/// ([`refused`]): the caller keeps what it knew.
pub(crate) async fn track_album(cx: &Context, track: &str, page: bool) -> Result<Option<String>> {
    let mut refusal = None;
    if configured(&cx.config) {
        match object(cx, "tracks", "track", track).await {
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
    configured(config) || config.also_on.spotify_pages
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
            cx.egress.config().discovery_secs,
        )
        .await?;
    crate::resolver::busy(&fetched, "spotify_busy")?;
    if fetched.status != 200 {
        return Ok(None);
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
