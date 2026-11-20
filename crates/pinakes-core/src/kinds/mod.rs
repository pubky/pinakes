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
/// A finite number of zero or more, given as a number or as a string (`decimal`).
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(decimal))
        .filter(|n| n.is_finite() && *n >= 0.0)
}
/// A number as a page writes it. Commas that group thousands are dropped (`1,299`,
/// `1,299.00`, and the Indian `12,50,000`); a comma before one or two last digits is a decimal
/// comma, with points grouping thousands or none (`12,99`, `1,5`, `1.299,00`). Any other use of
/// a comma is no number: a guess would store a price a hundred times too high.
fn decimal(s: &str) -> Option<f64> {
    let s = s.trim();
    if !s.contains(',') {
        return s.parse().ok();
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    // One to three digits, then groups of three after each `separator`.
    let grouped = |s: &str, separator: char| {
        let mut groups = s.split(separator);
        let first = groups.next().unwrap_or_default();
        digits(first)
            && first.len() <= 3
            && !first.starts_with('0')
            && groups.all(|group| group.len() == 3 && digits(group))
    };
    // Lakhs and crores: one or two digits, groups of two, then the last three.
    let indian = |s: &str| {
        let Some((head, thousands)) = s.rsplit_once(',') else {
            return false;
        };
        let mut groups = head.split(',');
        let first = groups.next().unwrap_or_default();
        thousands.len() == 3
            && digits(thousands)
            && digits(first)
            && first.len() <= 2
            && !first.starts_with('0')
            && groups.all(|group| group.len() == 2 && digits(group))
    };
    let (whole, fraction) = s.split_once('.').map_or((s, None), |(w, f)| (w, Some(f)));
    if whole.contains(',') && (grouped(whole, ',') || indian(whole)) && fraction.is_none_or(digits)
    {
        let whole = whole.replace(',', "");
        return format!("{whole}.{}", fraction.unwrap_or("0")).parse().ok();
    }
    let (whole, fraction) = s.rsplit_once(',')?;
    let whole_number = digits(whole) || (whole.contains('.') && grouped(whole, '.'));
    if whole_number && fraction.len() <= 2 && digits(fraction) {
        return format!("{}.{fraction}", whole.replace('.', ""))
            .parse()
            .ok();
    }
    None
}
/// A link kept as a fact only whole: one longer than an attribute holds would be cut to a link
/// that opens something else, or nothing.
fn link(url: Option<String>) -> Option<String> {
    url.filter(|url| url.len() <= crate::model::ATTRIBUTE_VALUE_CAP)
}
/// Links joined with "; ", as many whole ones as an attribute holds.
fn links(urls: &[String]) -> Option<String> {
    let mut out = String::new();
    for url in urls {
        let separator = if out.is_empty() { "" } else { "; " };
        if out.len() + separator.len() + url.len() > crate::model::ATTRIBUTE_VALUE_CAP {
            break;
        }
        out.push_str(separator);
        out.push_str(url);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_reads_grouping_commas_and_a_decimal_comma() {
        let read = |s: &str| number(&Value::String(s.into()));
        for (written, value) in [
            ("1299", Some(1299.0)),
            ("12.99", Some(12.99)),
            ("1,299", Some(1299.0)),
            ("1,299.00", Some(1299.0)),
            ("1,234,567.5", Some(1234567.5)),
            ("50,000", Some(50000.0)),
            ("1,00,000", Some(100000.0)),
            ("12,50,000.50", Some(1250000.5)),
            ("12,99", Some(12.99)),
            ("1,5", Some(1.5)),
            ("0,5", Some(0.5)),
            ("1.299,00", Some(1299.0)),
            ("1.234.567,8", Some(1234567.8)),
            (" 12,99 ", Some(12.99)),
            // Neither a grouping nor a decimal comma.
            ("1,2345", None),
            ("0,500", None),
            ("1,29,9", None),
            ("1,00,00", None),
            ("12,", None),
            (",5", None),
            ("1,299.", None),
            ("12,99 EUR", None),
            ("-12,99", None),
        ] {
            assert_eq!(read(written), value, "{written}");
        }
        assert_eq!(number(&serde_json::json!(12.5)), Some(12.5));
        assert_eq!(number(&serde_json::json!(-1)), None);
    }

    #[test]
    fn a_link_is_a_fact_only_whole() {
        let long = format!("https://jobs.example/apply?{}", "x".repeat(300));
        assert_eq!(link(Some(long.clone())), None);
        let short = "https://jobs.example/apply".to_owned();
        assert_eq!(link(Some(short.clone())), Some(short.clone()));
        let several = [
            short.clone(),
            format!("https://qa.example/q/{}", "1".repeat(200)),
            "https://qa.example/q/3".to_owned(),
        ];
        assert_eq!(links(&several), Some(format!("{short}; {}", several[1])));
        assert_eq!(links(&[long]), None);
    }

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
