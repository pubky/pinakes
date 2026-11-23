//! The `own_ids` also-on source: an official card's own ids as its join keys.
//!
//! An official source's card holds the ids its service's record states for the object (`doi`,
//! `wikidata`, `isbn`, …) as facts. The namespaces the source declares its own
//! (`sources::Source::own_ids`) are also keys: a DOI the service issued itself, under its own
//! prefix, or the Wikidata item its record names. So a record joins the group of the `doi:` card
//! of its own DOI, or of the Wikidata item's card. A DOI the depositor or the metadata typed (a
//! publisher's) and every id the source does not declare stay facts and join nothing
//! (`sources::own_keys`). A page hook's card makes no key. No I/O.
use crate::also_on::{Context, Found, Key, Source, Subject};
use anyhow::Result;
use async_trait::async_trait;

pub struct OwnIds;
#[async_trait]
impl Source for OwnIds {
    fn id(&self) -> &'static str {
        "own_ids"
    }
    fn applies(&self, subject: &Subject) -> bool {
        own_keys(subject).is_some()
    }
    // No `single_namespaces`: a group holds one DOI and one Wikidata item at most whatever the
    // source (`groups::SINGLE_OBJECT`).
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        &[("doi", "10.1234/5678"), ("wikidata", "Q42")]
    }
    async fn lookup(&self, _cx: &Context, subject: &Subject) -> Result<Found> {
        let mut found = Found::default();
        if let Some((via, keys)) = own_keys(subject) {
            found.keys = keys
                .into_iter()
                .map(|key| Key {
                    key,
                    via: via.into(),
                })
                .collect();
        }
        Ok(found)
    }
}

fn own_keys(subject: &Subject) -> Option<(&'static str, Vec<String>)> {
    crate::sources::own_keys(subject.doc.as_ref()?)
}
