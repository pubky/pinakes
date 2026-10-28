//! Kinds of card that carry facts of their own whatever route read them: a job posting from a
//! career page's JSON-LD or from a job board's API holds the same facts under the same names, and
//! so does a product from a shop's page or its own product record.
//! Each module names its type, its facts (`FACTS`, published in `facts`) and reads
//! them from JSON-LD.
use crate::model::ResourceMeta;
use std::collections::BTreeMap;

pub mod jobs;
pub mod products;

/// Every attribute a kind's card may store.
pub const ATTRIBUTES: &[&[&str]] = &[jobs::FACTS, products::FACTS];

/// `facts` for a card of one of these kinds: `own` (its source's facts, if any) with the
/// kind's facts and what follows from them at `now` (a job posting's `job_status`).
pub fn facts(
    own: Option<BTreeMap<String, String>>,
    doc: &ResourceMeta,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<BTreeMap<String, String>> {
    let kind = if jobs::is_posting(doc) {
        jobs::FACTS
    } else if products::is_product(doc) {
        products::FACTS
    } else {
        return own;
    };
    let mut facts = own.unwrap_or_default();
    for (k, v) in &doc.attributes {
        if kind.contains(&k.as_str()) && !v.trim().is_empty() {
            facts.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    if jobs::is_posting(doc) {
        facts.insert("job_status".into(), jobs::status(doc, now).into());
    }
    (!facts.is_empty()).then_some(facts)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_kinds_facts_are_stored_by_every_route() {
        for facts in super::ATTRIBUTES {
            for fact in *facts {
                assert!(
                    crate::resolver::web::ATTRIBUTES.contains(fact),
                    "{fact} is not a web attribute"
                );
            }
        }
    }
}
