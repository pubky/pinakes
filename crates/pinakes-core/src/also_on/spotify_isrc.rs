//! Another service's track on Spotify, by its ISRC: for a track with an `isrc:` key (TIDAL's,
//! Deezer's or a vouching page's own record, or a hub's), the Spotify tracks that carry exactly
//! that ISRC (the Web API's search `isrc:` filter, an exact lookup, never a title), as links
//! keyed by the ISRC. Several answers are the recording on several releases; `editions` places
//! them and a card shows one per service.
//!
//! Asked with the same app credentials as `spotify` (`[spotify]`, or `PINAKES_SPOTIFY_CLIENT_ID`
//! and `PINAKES_SPOTIFY_CLIENT_SECRET`), through the same token slot: each answer is shared for
//! an hour by credential pair, a refusal holds five minutes, a rate limit is waited out. The
//! Spotify Developer Policy asks that metadata shown be accompanied by a link back to Spotify;
//! the link is all this source stores.
use super::{
    client_credentials,
    spotify::{Spotify, SpotifyConfig, SPOTIFY},
    Context, Found, Source, Subject,
};
use crate::{
    music::{self, Level, Service},
    music_sources::{derived, join_level, own_isrc},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

const API: &str = "https://api.spotify.com/v1";
/// The most Spotify tracks one ISRC gives a card (the recording on several releases).
const MAX_LINKS: usize = 5;
/// The most ISRC keys of one track asked (its own first).
const MAX_ISRCS: usize = 2;

pub struct SpotifyIsrc;
#[async_trait]
impl Source for SpotifyIsrc {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn budgets(&self) -> &'static [(&'static str, f64)] {
        // The host's pace is `spotify`'s: one budget, shared.
        &[("api.spotify.com", 2.0)]
    }
    fn id(&self) -> &'static str {
        "spotify_isrc"
    }
    fn page(&self) -> &'static str {
        "spotify"
    }
    fn ready(&self, config: &crate::config::Config) -> bool {
        Spotify.ready(config)
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri)
            .is_some_and(|l| join_level(&l) == Some(Level::Track) && l.service != Service::Spotify)
            && subject.keys_in("isrc").next().is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let (client_id, secret) = cx
            .config
            .section::<SpotifyConfig>()
            .pair()
            .context("no Spotify credentials")?;
        // The track's own ISRC first, then the hubs'; only ISRCs that are keys (a shared one is
        // a fact, and Spotify would answer with any of its recordings).
        let own = own_isrc(subject).filter(|o| subject.keys_in("isrc").any(|k| k == o.as_str()));
        let mut isrcs: Vec<&str> = own.as_deref().into_iter().collect();
        isrcs.extend(
            subject
                .keys_in("isrc")
                .filter(|v| Some(*v) != own.as_deref()),
        );
        let mut found = Found::default();
        for isrc in isrcs.into_iter().take(MAX_ISRCS) {
            let Some(isrc) = music::isrc(isrc) else {
                continue;
            };
            let mut url = url::Url::parse(&format!("{API}/search")).expect("a fixed URL");
            url.query_pairs_mut()
                .append_pair("q", &format!("isrc:{isrc}"))
                .append_pair("type", "track")
                .append_pair("limit", "10");
            let Some(answer) = client_credentials::object(
                &SPOTIFY,
                cx,
                (&client_id, &secret),
                url.as_str(),
                ("application/json", &[]),
            )
            .await?
            else {
                continue;
            };
            // Only tracks that carry the ISRC asked, in Spotify's order, each once.
            for item in answer["tracks"]["items"].as_array().into_iter().flatten() {
                if item["external_ids"]["isrc"]
                    .as_str()
                    .and_then(music::isrc)
                    .as_deref()
                    != Some(isrc.as_str())
                {
                    continue;
                }
                let Some(id) = item["id"].as_str() else {
                    continue;
                };
                let Some(link) = derived(
                    &format!("https://open.spotify.com/track/{id}"),
                    Level::Track,
                    "spotify_isrc",
                ) else {
                    continue;
                };
                if found.links.iter().any(|l| l.uri == link.uri) {
                    continue;
                }
                found.links.push(link.keyed(format!("isrc:{isrc}")));
                if found.links.len() >= MAX_LINKS {
                    return Ok(found);
                }
            }
        }
        Ok(found)
    }
}
