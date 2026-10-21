//! The `wikidata` also-on source.
use crate::{
    also_on::{Context, Found, Source, Subject},
    hubs::wikidata,
    music::{self, discogs},
};
use anyhow::Result;
use async_trait::async_trait;

use crate::music_sources::*;

/// Wikidata's item for an exact service ID.
pub struct WikidataSource;
#[async_trait]
impl Source for WikidataSource {
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
        Ok(found(&item, level, &cx.config.also_on.apple_storefront))
    }
}
