//! The `service` also-on source.
use crate::{
    also_on::{Context, Found, Key, Source, Subject},
    music::{self, Level},
};
use anyhow::Result;
use async_trait::async_trait;

use crate::music_sources::*;

/// A music service's own record of its own object: the ISRC on its track page (a key only from
/// a service that vouches for it), its artist, album and length.
pub struct ServiceSource;
#[async_trait]
impl Source for ServiceSource {
    fn id(&self) -> &'static str {
        "service"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let mut found = Found::default();
        if let Some((service, isrc)) = page_isrc(subject) {
            let shared = service.vouches_isrc() && shared_isrc(cx, &isrc).await?;
            if service.vouches_isrc() && !shared {
                found.keys.push(Key {
                    key: format!("isrc:{isrc}"),
                    via: service.token().into(),
                });
            }
            found.facts.push(crate::also_on::Fact::new("isrc", isrc));
        }
        // What its page says: the artist, a track's album and length (seconds).
        let attributes = subject.doc.as_ref().map(|d| &d.attributes);
        let attribute = |name: &str| attributes.and_then(|a| a.get(name)).map(|v| v.trim());
        if let Some(artist) = attribute("artist") {
            found
                .facts
                .push(crate::also_on::Fact::new("artist", artist));
        }
        if link.level == Level::Track {
            if let Some(album) = attribute("album") {
                found.facts.push(crate::also_on::Fact::new("album", album));
            }
            let seconds = attribute("duration")
                .and_then(crate::event::time::duration)
                .map(|d| d.num_seconds())
                .filter(|s| (1..=86_400).contains(s));
            if let Some(seconds) = seconds {
                found
                    .facts
                    .push(crate::also_on::Fact::new("duration", seconds.to_string()));
            }
        }
        Ok(found)
    }
}
