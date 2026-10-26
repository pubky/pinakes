//! The `musicbrainz` also-on source.
use crate::{
    also_on::{Context, Found, Link, Source, Subject},
    hubs::{self, musicbrainz, wikidata, MbKind},
    music::{self, discogs, Level},
};
use anyhow::Result;
use async_trait::async_trait;

use crate::music_sources::*;

/// At most this many releases of one link are read to find the release group they share.
const MAX_EDITIONS: usize = 4;
/// MusicBrainz relations of an exact link.
pub struct MusicBrainzSource;
#[async_trait]
impl Source for MusicBrainzSource {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn id(&self) -> &'static str {
        "musicbrainz"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| join_level(&l).is_some())
    }
    async fn prefetch(&self, cx: &Context, uris: &[String]) {
        let links: Vec<String> = uris
            .iter()
            .filter_map(|u| music::parse(u))
            .filter(|l| join_level(l).is_some())
            .map(|l| l.canonical)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if links.len() > 1 {
            let base = cx.config.also_on.musicbrainz_base();
            if let Err(error) = musicbrainz::prefetch_urls(&cx.egress, base, &links).await {
                tracing::debug!(
                    kind = crate::resolver::error_class(&error),
                    "batched MusicBrainz lookup failed; links ask alone"
                );
            }
        }
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, Found::default)
    }
}
impl MusicBrainzSource {
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let Some(level) = join_level(&link) else {
            return Ok(Found::default());
        };
        let base = cx.config.also_on.musicbrainz_base();
        let targets = musicbrainz::url(&cx.egress, base, &link.canonical)
            .await?
            .map(|t| t.targets)
            .unwrap_or_default();
        let at = |kind: MbKind| -> Vec<String> {
            targets
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, id)| id.clone())
                .collect()
        };
        // Exactly one entity at the link's level; several are ambiguous and join nothing,
        // except releases of one release group. A Discogs master is a release group and a
        // Discogs release its releases, never across.
        let discogs = discogs::of_link(&link).map(|(kind, _)| kind);
        let (mut kind, mut ids) = match level {
            Level::Track => (MbKind::Recording, at(MbKind::Recording)),
            Level::Artist => (MbKind::Artist, at(MbKind::Artist)),
            _ => match (discogs, at(MbKind::Release)) {
                (Some(discogs::Kind::Master), _) => {
                    (MbKind::ReleaseGroup, at(MbKind::ReleaseGroup))
                }
                (None, releases) if releases.is_empty() => {
                    (MbKind::ReleaseGroup, at(MbKind::ReleaseGroup))
                }
                (_, releases) => (MbKind::Release, releases),
            },
        };
        // A track MusicBrainz does not relate the URL to: the one recording its page's own ISRC
        // is on, or the one of several its length picks (`isrc_match`). A picked recording's
        // shared ISRC stays a fact: it names the other recordings too.
        let mut shared = vec![];
        if kind == MbKind::Recording && ids.is_empty() {
            if let Some(isrc) = own_isrc(subject) {
                match isrc_match(cx, &isrc, page_duration(subject)).await? {
                    IsrcMatch::One(mbid) => ids = vec![mbid],
                    IsrcMatch::Picked(mbid) => {
                        ids = vec![mbid];
                        shared.push(isrc);
                    }
                    IsrcMatch::Unknown | IsrcMatch::Shared => {}
                }
            }
        }
        // An album MusicBrainz does not relate the URL to: the releases carrying its barcode keys
        // (TIDAL's own record, or a service's from an earlier run), one or editions of one
        // release group, as a related URL would name them. Never a barcode this source keyed
        // itself: that would re-find a release after an editor removed its URL relation. Never
        // for a Discogs master either: it is a release group, and barcodes name releases.
        if level == Level::Album && ids.is_empty() && discogs != Some(discogs::Kind::Master) {
            let barcodes: Vec<&str> = subject
                .keys
                .iter()
                .filter(|(_, source)| !source.starts_with("musicbrainz"))
                .filter_map(|(key, _)| key.strip_prefix("upc:"))
                .take(2)
                .collect();
            let mut releases = std::collections::BTreeSet::new();
            for upc in barcodes {
                releases.extend(musicbrainz::barcode(&cx.egress, base, upc).await?);
            }
            if !releases.is_empty() {
                kind = MbKind::Release;
                ids = releases.into_iter().collect();
            }
        }
        if ids.is_empty() || ids.len() > 1 && (kind != MbKind::Release || ids.len() > MAX_EDITIONS)
        {
            return Ok(Found::default());
        }
        let mut found = entities_found(
            cx,
            base,
            kind,
            &ids,
            level,
            discogs == Some(discogs::Kind::Release),
            &shared,
        )
        .await?;
        // A Discogs master is a release group, whose MusicBrainz entry names no service pages
        // (its editions have them): the one Wikidata item MusicBrainz relates it to names the
        // album's, when the item names this release group back. The entity is the read above.
        if let ([group], Some(discogs::Kind::Master), false) =
            (ids.as_slice(), discogs, found.keys.is_empty())
        {
            if let Some(entity) = musicbrainz::entity(&cx.egress, base, kind, group).await? {
                let named = hubs::key::mb(MbKind::ReleaseGroup, &entity.mbid);
                let storefront = &cx.config.also_on.apple_storefront;
                for mut link in album_item_links(cx, &entity, storefront).await? {
                    if !found.links.iter().any(|l| l.uri == link.uri) {
                        link.key.clone_from(&named);
                        found.links.push(link);
                    }
                }
            }
        }
        Ok(found)
    }
}

/// The album pages of the one Wikidata item a release group's MusicBrainz entry relates it to,
/// when that item names the release group back (P436) and no other.
async fn album_item_links(
    cx: &Context,
    group: &musicbrainz::Entity,
    storefront: &str,
) -> Result<Vec<Link>> {
    let mut qids: Vec<String> = group
        .urls
        .iter()
        .filter_map(|u| crate::lookups::wikidata::wikidata_item(&u.url))
        .collect();
    qids.sort();
    qids.dedup();
    let [qid] = qids.as_slice() else {
        return Ok(vec![]);
    };
    let items = wikidata::items(&cx.egress, std::slice::from_ref(qid)).await?;
    let Some(item) = items.iter().find(|i| &i.qid == qid) else {
        return Ok(vec![]);
    };
    if !matches!(item.id("P436"), [mbid] if *mbid == group.mbid) {
        return Ok(vec![]);
    }
    Ok(found(item, Level::Album, storefront).links)
}
