//! The `wikidata` also-on source.
use crate::{
    also_on::{Context, Found, Source, Subject},
    hubs::{self, wikidata},
    music::{self, discogs},
};
use anyhow::Result;
use async_trait::async_trait;

use crate::music_sources::*;

/// The Wikidata property for a link's own service ID, if Wikidata has one.
fn property_of(link: &music::Link) -> Option<&'static str> {
    if discogs::of_link(link).is_some_and(|(kind, _)| kind == discogs::Kind::Release) {
        return Some(DISCOGS_RELEASE);
    }
    PROPERTIES
        .iter()
        .find(|(_, service, level, _)| *service == link.service && *level == link.level)
        .map(|(p, ..)| *p)
}
/// Wikidata's Discogs release ID.
const DISCOGS_RELEASE: &str = "P2206";

/// Wikidata's item for an exact service ID.
pub struct WikidataSource;
#[async_trait]
impl Source for WikidataSource {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn id(&self) -> &'static str {
        "wikidata"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| property_of(&l).is_some())
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, Found::default)
    }
}
impl WikidataSource {
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let (Some(property), Some(level)) = (property_of(&link), join_level(&link)) else {
            return Ok(Found::default());
        };
        // A Discogs ID is the number alone.
        let id = discogs::of_link(&link).map_or(link.id.clone(), |(_, id)| id.to_string());
        let item = match wikidata::item_with(&cx.egress, property, &id).await? {
            wikidata::Found::One(item) => item,
            _ => return Ok(Found::default()),
        };
        // An item with a Discogs release is an edition's only when it is not the album's.
        if property == DISCOGS_RELEASE && ["P1954", "P436"].iter().any(|p| !item.id(p).is_empty()) {
            return Ok(Found::default());
        }
        let mut found = found(&item, level, &cx.config.also_on.apple_storefront);
        // A recording item's MBID is keyed as the recording that stands for its duplicates
        // (`standing`), as every other path keys a recording.
        let base = cx.config.also_on.musicbrainz_base();
        for key in &mut found.keys {
            let Some(mbid) = key.key.strip_prefix("mb:recording:").map(str::to_owned) else {
                continue;
            };
            if let Standing::Of { standing, .. } = standing(&cx.egress, base, &mbid).await? {
                if let Some(stands) = hubs::key::mb(crate::hubs::MbKind::Recording, &standing) {
                    key.key = stands;
                }
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Discogs release's ID is Wikidata's P2206, a master's P1954 (`PROPERTIES`).
    #[test]
    fn a_discogs_link_has_its_wikidata_property() {
        let release = music::parse("https://www.discogs.com/release/83182").unwrap();
        assert_eq!(property_of(&release), Some("P2206"));
        let master = music::parse("https://www.discogs.com/master/21491").unwrap();
        assert_eq!(property_of(&master), Some("P1954"));
    }
}
