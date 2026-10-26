//! The `musicbrainz_entity` also-on source.
use crate::{
    also_on::{Context, Found, Key, Source, Subject},
    hubs::{self, musicbrainz, MbKind},
    music::Level,
};
use anyhow::Result;
use async_trait::async_trait;

use crate::music_sources::*;

/// A MusicBrainz page as its own hub: a recording's, release's, release group's or artist's page
/// carries that entity's keys and facts and lists its links on music services at its level, as
/// the `musicbrainz` source does for a service link MusicBrainz relates to the entity. An ISRC's
/// page keys the ISRC and the one recording MusicBrainz gives it to; an ISRC several recordings
/// have names none of them (as a service's page ISRC), so its page keeps it as a fact only.
pub struct MusicBrainzEntitySource;
#[async_trait]
impl Source for MusicBrainzEntitySource {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn id(&self) -> &'static str {
        "musicbrainz_entity"
    }
    fn applies(&self, subject: &Subject) -> bool {
        musicbrainz::entity_link(&subject.uri).is_some()
            || musicbrainz::isrc_link(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, || {
            Self::own_keys(&subject.uri)
        })
    }
}
impl MusicBrainzEntitySource {
    /// What the page's URL alone proves: the entity's MBID, or the ISRC (with its fact).
    fn own_keys(uri: &str) -> Found {
        let mut found = Found::default();
        if let Some((kind, mbid, _)) = musicbrainz::entity_link(uri) {
            found.keys.extend(hubs::key::mb(kind, &mbid).map(|key| Key {
                key,
                via: "musicbrainz".into(),
            }));
        } else if let Some((isrc, _)) = musicbrainz::isrc_link(uri) {
            found.keys.push(Key {
                key: format!("isrc:{isrc}"),
                via: "musicbrainz".into(),
            });
            found.facts.push(crate::also_on::Fact::new("isrc", isrc));
        }
        found
    }
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let base = cx.config.also_on.musicbrainz_base();
        if let Some((kind, mbid, _)) = musicbrainz::entity_link(&subject.uri) {
            let level = match kind {
                MbKind::Recording => Level::Track,
                MbKind::Release | MbKind::ReleaseGroup => Level::Album,
                MbKind::Artist => Level::Artist,
            };
            let mut found = entities_found(
                cx,
                base,
                kind,
                std::slice::from_ref(&mbid),
                level,
                false,
                &[],
            )
            .await?;
            // A merged entity answers its old MBID with the one it was merged into: the page keys
            // that one, and keeps the old as a fact, so `key=mbid:{old}` still finds it.
            if found
                .facts
                .iter()
                .any(|f| f.name == "mbid" && f.value != mbid)
            {
                found
                    .facts
                    .push(crate::also_on::Fact::new("mbid_was", &mbid));
            }
            return Ok(found);
        }
        let Some((isrc, _)) = musicbrainz::isrc_link(&subject.uri) else {
            return Ok(Found::default());
        };
        let own = Key {
            key: format!("isrc:{isrc}"),
            via: "musicbrainz".into(),
        };
        let mut found = match musicbrainz::isrc(&cx.egress, base, &isrc).await? {
            Some(recordings) if recordings.len() == 1 => {
                let mut found = entities_found(
                    cx,
                    base,
                    MbKind::Recording,
                    &[recordings[0].mbid.clone()],
                    Level::Track,
                    false,
                    &[],
                )
                .await?;
                // Its own ISRC first: a recording's other ISRCs are keys only up to a cap.
                found.keys.retain(|k| k.key != own.key);
                found.keys.insert(0, own);
                found
            }
            Some(recordings) if recordings.len() > 1 => Found::default(),
            // An ISRC MusicBrainz doesn't know is still its own key.
            _ => Found {
                keys: vec![own],
                ..Default::default()
            },
        };
        let fact = crate::also_on::Fact::new("isrc", &isrc);
        if !found.facts.contains(&fact) {
            found.facts.push(fact);
        }
        Ok(found)
    }
}
