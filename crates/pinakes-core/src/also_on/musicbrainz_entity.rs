//! The `musicbrainz_entity` also-on source.
use crate::{
    also_on::{Context, Found, Key, Source, Subject},
    hubs::{self, musicbrainz, wikidata, MbKind},
    music::Level,
};
use anyhow::Result;
use async_trait::async_trait;

use crate::music_sources::*;

/// A MusicBrainz page as its own hub: a recording's, release's, release group's or artist's page
/// carries that entity's keys and facts and lists its links on music services at its level, as
/// the `musicbrainz` source does for a service link MusicBrainz relates to the entity. An
/// artist's or release group's page also keys the Wikidata item its own relations name, when
/// that item, read back, is exactly its own (`own_wikidata_key`). An ISRC's
/// page keys the ISRC and the one recording MusicBrainz gives it to, or the duplicates that
/// agree as one recording (`music_sources::agree`); an ISRC on recordings that disagree names
/// none of them (as a service's page ISRC), so its page keeps it as a fact only.
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
    /// What the page's URL alone proves: the entity's MBID, or the ISRC (with its fact). A
    /// recording page keys its own MBID here too, though it may be one of several duplicates
    /// another recording stands for (only the read, `standing`, could tell): at worst its group
    /// stays apart from theirs until a read succeeds, never two recording keys on one resource.
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
            // An artist's or release group's own Wikidata item (the entity is the same answer,
            // from the egress's cache): a key like its MBID. A Wikidata that fails for now leaves
            // MusicBrainz's part standing, keeps the item key this source recorded before, and
            // brings the run back after its wait.
            if !found.keys.is_empty() {
                if let Some(entity) = musicbrainz::entity(&cx.egress, base, kind, &mbid).await? {
                    match own_wikidata_key(&cx.egress, &entity).await {
                        Ok(key) => found.keys.extend(key),
                        Err(error) => {
                            let wait = crate::also_on::media::retry_wait(&error);
                            found.retry_after = Some(std::time::Duration::from_secs(wait));
                            found.keys.extend(
                                subject
                                    .keys
                                    .iter()
                                    .filter(|(key, source)| {
                                        key.starts_with("wikidata:")
                                            && source == "musicbrainz_entity"
                                    })
                                    .map(|(key, _)| Key {
                                        key: key.clone(),
                                        via: "musicbrainz".into(),
                                    }),
                            );
                        }
                    }
                }
            }
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
            // Duplicates that agree are one recording, keyed by the one that stands for them;
            // recordings that disagree, or a set that cannot be settled, name none.
            Some(recordings) if recordings.len() > 1 => {
                let settled = match agreeing(&cx.egress, base, &recordings).await? {
                    Some(entities) => match entities.first() {
                        Some(first) => standing(&cx.egress, base, &first.mbid).await?,
                        None => Standing::Unclear,
                    },
                    None => Standing::Unclear,
                };
                match settled {
                    Standing::Of { standing, members } => {
                        let mut found =
                            duplicates_found(cx, base, &standing, &members, &isrc, Level::Track)
                                .await?;
                        found.keys.retain(|k| k.key != own.key);
                        found.keys.insert(0, own);
                        found
                    }
                    Standing::Alone | Standing::Unclear => Found::default(),
                }
            }
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

/// The Wikidata item a MusicBrainz artist or release group may be keyed by: the one item its own
/// current `wikidata` URL relations name (`musicbrainz::own_item`, the item link parsed by
/// `wikidata::item_link`). Relations that name several items name none. A recording or a
/// release has none: MusicBrainz relates Wikidata items to artists and release groups (and
/// works), and an item a recording's or release's relation named would be its song's or its
/// album's, another level's (a key never crosses levels).
fn own_wikidata_item(entity: &musicbrainz::Entity) -> Option<String> {
    if !matches!(entity.kind, MbKind::Artist | MbKind::ReleaseGroup) {
        return None;
    }
    match musicbrainz::own_item(&entity.urls) {
        musicbrainz::OwnItem::Item(qid) => Some(qid),
        musicbrainz::OwnItem::None | musicbrainz::OwnItem::Unclear => None,
    }
}
/// The `wikidata:` key of a MusicBrainz artist or release group ([`own_wikidata_item`], made by
/// `hubs::key::wikidata` as every producer makes it), when that item, read back, is exactly an
/// item of the entity's kind (`music_enrichment::ItemKind::of`: an artist's for an artist, a
/// release group's for a release group) that names this MBID and no other for its level (P434,
/// P436). A key is stricter than the enrichment's text and image: an item that is a work's too
/// (a single's song's) or carries no
/// identifier yet keys nothing, so a key never joins an album to a song or an artist to anything
/// but itself. An item that is unknown, gone or too large to read keys nothing; a busy Wikidata
/// is an error.
async fn own_wikidata_key(
    egress: &crate::resolver::egress::Egress,
    entity: &musicbrainz::Entity,
) -> Result<Option<Key>> {
    let Some(qid) = own_wikidata_item(entity) else {
        return Ok(None);
    };
    let item = match wikidata::items(egress, std::slice::from_ref(&qid)).await {
        Ok(items) => items.into_iter().find(|i| i.qid == qid),
        Err(e)
            if crate::resolver::egress::over_cap(&e)
                || e.downcast_ref::<crate::hubs::Gone>().is_some() =>
        {
            None
        }
        Err(e) => return Err(e),
    };
    Ok(item
        .filter(|item| keys_entity(entity, item))
        .and_then(|item| hubs::key::wikidata(&item.qid))
        .map(|key| Key {
            key,
            via: "musicbrainz".into(),
        }))
}
/// Whether `item` may key `entity` ([`own_wikidata_key`]): exactly of its kind, naming this one
/// MBID at its level; for a release group, naming no recording either (P4404): an album's item
/// that also carries a recording's ID is a single's and its track's at once, and the page's own
/// key takes only an item that is the album's alone.
fn keys_entity(entity: &musicbrainz::Entity, item: &wikidata::Item) -> bool {
    use crate::music_enrichment::{mbid_property, ItemKind};
    ItemKind::of(item) == ItemKind::of_entity(entity.kind)
        && item.id(mbid_property(entity.kind)) == [entity.mbid.as_str()]
        && (entity.kind != MbKind::ReleaseGroup || item.id("P4404").is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An artist's or release group's own `wikidata` relation names the item it may be keyed
    /// by; a recording's or a release's never does, and relations that name no item, or
    /// several, name none.
    #[test]
    fn own_wikidata_items_stay_at_their_level() {
        let entity = |kind: MbKind, urls: &[&str]| musicbrainz::Entity {
            kind,
            mbid: "8e8a594f-2175-38c7-a871-abb68ec363e7".into(),
            title: None,
            urls: urls
                .iter()
                .map(|u| musicbrainz::UrlRelation {
                    kind: "wikidata".into(),
                    url: (*u).into(),
                })
                .collect(),
            isrcs: vec![],
            barcode: None,
            release_group: None,
            artist: None,
            artist_ids: vec![],
            disambiguation: None,
            video: false,
            length_ms: None,
            artist_type: None,
            about: Default::default(),
        };
        let item = "https://www.wikidata.org/wiki/Q283221";
        for kind in [MbKind::Artist, MbKind::ReleaseGroup] {
            assert_eq!(
                own_wikidata_item(&entity(kind, &[item])).as_deref(),
                Some("Q283221")
            );
        }
        for kind in [MbKind::Recording, MbKind::Release] {
            assert!(own_wikidata_item(&entity(kind, &[item])).is_none());
        }
        for urls in [
            &["https://www.wikidata.org/wiki/Property:P18"][..],
            &[item, "https://www.wikidata.org/wiki/Q1"][..],
            &[][..],
        ] {
            assert!(own_wikidata_item(&entity(MbKind::ReleaseGroup, urls)).is_none());
        }
    }
}
