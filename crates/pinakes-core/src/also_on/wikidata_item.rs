//! The `wikidata_item` also-on source.
use crate::{
    also_on::{Context, Found, Key, Source, Subject},
    hubs::{self, wikidata},
};
use anyhow::Result;
use async_trait::async_trait;

use crate::wikipedia::*;

/// What a Wikidata item link lists: its Q-id as key; its encyclopedia entries, the music
/// services its best-rank IDs name (every level: the item is the thing itself, so these carry
/// no key: a song item's track is one of several recordings) and its Wikipedia editions. Only
/// the item's own claims and sitelinks, never a search.
pub fn item_found(item: &wikidata::Item, storefront: &str) -> Found {
    let mut found = Found {
        keys: item_key(item),
        ..Default::default()
    };
    found.links = encyclopedias(item);
    found
        .links
        .extend(crate::music_sources::service_links(item, storefront));
    found.links.extend(editions(item, None));
    found
}

/// A Wikidata item link: the item's own hub.
pub struct WikidataItemSource;
#[async_trait]
impl Source for WikidataItemSource {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        &[("wikidata", "Q42")]
    }
    fn id(&self) -> &'static str {
        "wikidata_item"
    }
    fn applies(&self, subject: &Subject) -> bool {
        wikidata::item_link(&subject.uri).is_some()
    }
    async fn prefetch(&self, cx: &Context, uris: &[String]) {
        let qids: Vec<String> = uris
            .iter()
            .filter_map(|u| wikidata::item_link(u))
            .map(|(qid, _)| qid)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if qids.len() > 1 {
            if let Err(error) = wikidata::prefetch_items(&cx.egress, &qids).await {
                tracing::debug!(
                    kind = crate::resolver::error_class(&error),
                    "batched Wikidata read failed; items read alone"
                );
            }
        }
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, || {
            Self::own_key(&subject.uri)
        })
    }
}
impl WikidataItemSource {
    /// What the item link alone proves: its Q-id.
    fn own_key(uri: &str) -> Found {
        Found {
            keys: wikidata::item_link(uri)
                .and_then(|(qid, _)| hubs::key::wikidata(&qid))
                .map(|key| Key {
                    key,
                    via: "wikidata".into(),
                })
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some((qid, _)) = wikidata::item_link(&subject.uri) else {
            return Ok(Found::default());
        };
        Ok(wikidata::items(&cx.egress, &[qid])
            .await?
            .first()
            .map(|item| item_found(item, &cx.config.also_on.apple_storefront))
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> serde_json::Value {
        let path = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn a_wikidata_item_link_is_its_own_hub() {
        for uri in [
            "https://www.wikidata.org/wiki/Q90",
            "http://www.wikidata.org/entity/Q90",
            "https://m.wikidata.org/wiki/Q90#sitelinks",
            "https://www.wikidata.org/wiki/Special:EntityPage/Q90",
            "https://wikidata.org/wiki/Q90",
        ] {
            assert_eq!(
                wikidata::item_link(uri),
                Some(("Q90".into(), "https://www.wikidata.org/wiki/Q90".into())),
                "{uri}"
            );
        }
        for uri in [
            "https://www.wikidata.org/wiki/Property:P31",
            "https://www.wikidata.org/wiki/Q0",
            "https://www.wikidata.org/wiki/Special:EntityData/Q90.json",
            "https://evil.example/wiki/Q90",
        ] {
            assert!(wikidata::item_link(uri).is_none(), "{uri}");
        }
        // Kant's item: encyclopedias and every edition, English included.
        let kant = &wikidata::parse_entities(&fixture("wd-kant.json"))[0];
        let found = item_found(kant, "us");
        assert_eq!(found.keys[0].key, "wikidata:Q9312");
        let sites: Vec<&str> = found.links.iter().map(|l| l.site.as_str()).collect();
        assert!(sites.contains(&"Britannica") && sites.contains(&"Wikipedia (en)"));
        let editions: Vec<&&str> = sites
            .iter()
            .filter(|s| s.starts_with("Wikipedia"))
            .collect();
        assert_eq!(*editions[0], "Wikipedia (en)");
        // OK Computer's item: the music services its IDs name, then its editions.
        let album = wikidata::parse_entities(&fixture("wd-entities.json"))
            .into_iter()
            .find(|i| i.qid == "Q202996")
            .unwrap();
        let uris: Vec<String> = item_found(&album, "us")
            .links
            .into_iter()
            .map(|l| l.uri)
            .collect();
        assert!(uris.contains(&"https://open.spotify.com/album/2fGCAYUMssLKiUAoNdxGLx".into()));
        assert!(uris.contains(&"https://en.wikipedia.org/wiki/OK_Computer".into()));
    }
}
