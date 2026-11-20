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
        // An item with a Discogs release is an edition's only when it is not the album's (nor a
        // recording's, a work's or an artist's): the release takes an edition's item as it is.
        let mut found = if property == DISCOGS_RELEASE {
            if !edition_item(&item) {
                return Ok(Found::default());
            }
            item_found(&item, level)
        } else {
            found(&item, level)
        };
        // A recording item's MBID is keyed as every producer keys a recording (`recording_key`:
        // the one that stands for its duplicates); a replaced MBID stays as `mbid_was`, so
        // `key=mbid:` still finds the track by it.
        let base = cx.config.also_on.musicbrainz_base();
        let mut replaced = vec![];
        for key in &mut found.keys {
            let Some(mbid) = key.key.strip_prefix("mb:recording:").map(str::to_owned) else {
                continue;
            };
            let stands = recording_key(&cx.egress, base, &mbid).await?;
            if stands != mbid {
                if let Some(stands) = hubs::key::mb(crate::hubs::MbKind::Recording, &stands) {
                    key.key = stands;
                    replaced.push(mbid);
                }
            }
        }
        for mbid in replaced {
            found
                .facts
                .push(crate::also_on::Fact::new("mbid_was", mbid));
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
