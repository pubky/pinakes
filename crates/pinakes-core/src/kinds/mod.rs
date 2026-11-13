//! Kinds of card that carry facts of their own whatever route read them: a job posting from a
//! career page's JSON-LD or from a job board's API holds the same facts under the same names, and
//! so does a product from a shop's page or its own product record, a question from a Q&A
//! page or a Q&A site's API, and a recipe (`cooking`: `crate::recipes` is the API's examples).
//! Each module names its type, its facts (`FACTS`, published in `facts`) and reads
//! them from JSON-LD.
use crate::model::ResourceMeta;
use serde_json::Value;
use std::collections::BTreeMap;

pub mod cooking;
pub mod jobs;
pub mod products;
pub mod questions;

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
    } else if questions::is_question(doc) {
        questions::FACTS
    } else if cooking::is_recipe(doc) {
        cooking::FACTS
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

// What the kinds' JSON-LD readers share.

/// A value's first entry when it is an array, else the value.
fn first(v: &Value) -> &Value {
    v.as_array().and_then(|a| a.first()).unwrap_or(v)
}
/// A value's entries, the first `max` of an array or the value itself, nulls left out.
fn all(v: &Value, max: usize) -> impl Iterator<Item = &Value> {
    v.as_array()
        .map_or_else(|| vec![v], |a| a.iter().take(max).collect())
        .into_iter()
        .filter(|v| !v.is_null())
}
/// A string value, trimmed; none when empty.
fn text(v: &Value) -> Option<String> {
    crate::event::page::string(v)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
/// As `text`, and a whole number as its digits.
fn text_or_number(v: &Value) -> Option<String> {
    crate::event::page::string(v)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
/// A finite number of zero or more, given as a number or as a string (its commas ignored).
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| {
            v.as_str()
                .and_then(|s| s.trim().replace(',', "").parse().ok())
        })
        .filter(|n| n.is_finite() && *n >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kinds_facts_are_stored_by_every_route() {
        for facts in [
            jobs::FACTS,
            products::FACTS,
            questions::FACTS,
            cooking::FACTS,
        ] {
            for fact in facts {
                assert!(
                    crate::resolver::web::ATTRIBUTES.contains(fact),
                    "{fact} is not a web attribute"
                );
            }
        }
    }
}
