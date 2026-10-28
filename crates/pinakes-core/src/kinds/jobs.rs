//! Job postings (`JobPosting`), from a page's own JSON-LD (job boards and career pages publish it
//! for search engines) or a job board's API (an official source in `sources`).
//!
//! A posting's `start` is its date posted and its `end` the end of its `validThrough`: search's
//! `from` and `to` select postings by the date posted, and a posting past its end is closed
//! (`job_status`) and left out of search unless it asks for `closed=true`. Its workplace is its
//! venue, placed on the map like an event's (`EventService::workplace`); a remote posting names
//! the regions it accepts applicants from.
use crate::{
    event::Venue,
    model::{LocationSource, ResourceMeta},
    resolver::web::origin_url,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use url::Url;

pub const TYPE: &str = "JobPosting";

/// The attributes a posting publishes in `facts`, beside `job_status` (`open`, or
/// `closed` once `valid_through` has passed at the time of the read).
pub const FACTS: &[&str] = &[
    // The employer's name and its own site.
    "hiring_organization",
    "hiring_organization_url",
    // `FULL_TIME`, `PART_TIME`, `CONTRACTOR`, `TEMPORARY`, `INTERN`, `VOLUNTEER`, `PER_DIEM`,
    // `OTHER`, or the posting's own word in that form; several joined with "; ".
    "employment_type",
    // "true" for a posting that may be done remotely.
    "remote",
    // Where a remote posting accepts applicants from, as it names them, joined with "; ".
    "remote_regions",
    // The base salary: a range, or one amount in both; its ISO 4217 currency; per `HOUR`, `DAY`,
    // `WEEK`, `MONTH` or `YEAR`.
    "salary_min",
    "salary_max",
    "salary_currency",
    "salary_unit",
    // As the posting gives them.
    "date_posted",
    "valid_through",
    "apply_url",
];

/// The longest a posting's description is kept, in characters.
const EXCERPT: usize = 300;
/// How many regions (or other listed values) a posting keeps, and how much of a place's text is
/// read for its regions: a page cannot make the split or the dedup cost more.
pub const LISTED: usize = 32;
const PLACE_TEXT: usize = 1024;

pub fn is_posting(doc: &ResourceMeta) -> bool {
    doc.type_path.iter().any(|t| t == TYPE)
}

/// `open`, or `closed` once the posting's end has passed at `now`.
pub fn status(doc: &ResourceMeta, now: DateTime<Utc>) -> &'static str {
    if doc.end.is_some_and(|end| end <= now) {
        "closed"
    } else {
        "open"
    }
}

/// What a posting says, whichever route read it.
#[derive(Clone, Debug, Default)]
pub struct Posting {
    pub title: Option<String>,
    /// Plain text.
    pub description: Option<String>,
    pub organization: Option<String>,
    pub organization_url: Option<String>,
    pub employment_types: Vec<String>,
    pub remote: bool,
    pub regions: Vec<String>,
    pub salary: Salary,
    pub posted: Option<String>,
    pub valid_through: Option<String>,
    pub apply_url: Option<String>,
    pub workplace: Option<Venue>,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Salary {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub currency: Option<String>,
    pub unit: Option<String>,
}

impl Posting {
    /// Make `doc` this posting's card, keeping what it already holds.
    pub fn apply(self, doc: &mut ResourceMeta) {
        if !is_posting(doc) {
            doc.set_type(TYPE);
        }
        fill(&mut doc.name, self.title);
        fill(&mut doc.description, self.description);
        if doc.start.is_none() {
            doc.start = self.posted.as_deref().and_then(posted_at);
        }
        if doc.end.is_none() {
            doc.end = self.valid_through.as_deref().and_then(closes_at);
        }
        let salary = self.salary;
        for (key, value) in [
            ("hiring_organization", self.organization),
            ("hiring_organization_url", self.organization_url),
            ("employment_type", joined(&self.employment_types)),
            ("remote", self.remote.then(|| "true".to_owned())),
            ("remote_regions", joined(&self.regions)),
            ("salary_min", salary.min.map(amount)),
            // A range needs its minimum: a maximum alone says too little.
            (
                "salary_max",
                salary.max.filter(|_| salary.min.is_some()).map(amount),
            ),
            (
                "salary_currency",
                salary.currency.filter(|_| salary.min.is_some()),
            ),
            ("salary_unit", salary.unit.filter(|_| salary.min.is_some())),
            ("date_posted", self.posted),
            ("valid_through", self.valid_through),
            ("apply_url", self.apply_url),
        ] {
            if let Some(value) = value {
                doc.attributes.entry(key.into()).or_insert(value);
            }
        }
        if doc.event.venue.is_none() {
            if let Some(workplace) = self.workplace {
                if doc.location.is_none() && workplace.location.is_some() {
                    doc.location = workplace.location.clone();
                    doc.location_source = Some(LocationSource::Page);
                }
                doc.event.venue = Some(workplace);
            }
        }
    }
}

/// The posting a JSON-LD `JobPosting` node describes; `base` is the page's address.
pub fn from_jsonld(v: &Value, base: &Url) -> Posting {
    let org = first(&v["hiringOrganization"]);
    let mut posting = Posting {
        title: text(&v["title"]).or_else(|| text(&v["name"])),
        description: text(&v["description"]).and_then(|d| excerpt(&d)),
        organization: text(org).or_else(|| text(&org["name"])),
        organization_url: [&org["url"], &org["sameAs"]]
            .into_iter()
            .find_map(|u| text(first(u)))
            .and_then(|u| origin_url(base, &u)),
        employment_types: texts(&v["employmentType"])
            .iter()
            .filter_map(|t| employment_type(t))
            .collect(),
        remote: texts(&v["jobLocationType"])
            .iter()
            .any(|t| t.eq_ignore_ascii_case("TELECOMMUTE")),
        regions: all(&v["applicantLocationRequirements"])
            .filter_map(|r| text(r).or_else(|| text(&r["name"])))
            .collect(),
        salary: salary(&v["baseSalary"], &v["salaryCurrency"]),
        posted: text(&v["datePosted"]),
        valid_through: text(&v["validThrough"]),
        apply_url: apply_url(v, base),
        workplace: None,
    };
    let place = first(&v["jobLocation"]);
    let address = place.get("address").unwrap_or(place);
    posting.workplace = workplace(address, place);
    // The town's text as the page gives it, untrimmed: remote_place cuts it before it trims.
    let town = [
        &address["addressLocality"],
        &address["addressLocality"]["name"],
        address,
    ]
    .into_iter()
    .find_map(|v| {
        crate::event::page::string(v).filter(|s| s.contains(|c: char| !c.is_whitespace()))
    });
    if let Some(rest) = posting
        .workplace
        .as_ref()
        .filter(|w| w.address.contains_key("city"))
        .and(town.as_deref())
        .and_then(remote_place)
    {
        // "Remote", "Remote - US": a remote posting, not a town.
        posting.remote = true;
        posting.regions.extend(rest);
        posting.workplace = None;
    }
    posting.regions = dedup(posting.regions);
    posting
}

/// The workplace at `address` (a PostalAddress node or its text) of the Place `place`: its
/// address and point, or `None` when it names no street, town, postcode or point (a remote
/// posting's country alone).
pub fn workplace(address: &Value, place: &Value) -> Option<Venue> {
    let mut venue = Venue {
        name: text(&place["name"]),
        ..Default::default()
    };
    if let Some(line) = text(address) {
        venue.address.insert("city".into(), line);
    }
    for (k, dst) in [
        ("streetAddress", "street"),
        ("addressLocality", "city"),
        ("postalCode", "postalcode"),
        ("addressRegion", "region"),
        ("addressCountry", "country"),
    ] {
        if let Some(s) = text(&address[k]).or_else(|| text(&address[k]["name"])) {
            venue.address.insert(dst.into(), s);
        }
    }
    venue.location = crate::event::page::point(place);
    venue.location_source = venue.location.as_ref().map(|_| LocationSource::Page);
    let placed = venue.location.is_some()
        || ["street", "city", "postalcode"]
            .iter()
            .any(|k| venue.address.contains_key(*k));
    if !placed {
        return None;
    }
    let a = &venue.address;
    let town = [a.get("postalcode"), a.get("city")]
        .into_iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    venue.text = [
        a.get("street").cloned(),
        Some(town),
        a.get("region").cloned(),
    ]
    .into_iter()
    .chain([a.get("country").cloned()])
    .flatten()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(", ");
    if venue.text.is_empty() {
        venue.text = venue.name.clone().unwrap_or_default();
    }
    Some(venue)
}

/// When a place's text says the work is remote ("Remote", "Remote - US", "Remote, Europe"),
/// the regions it names after that word.
pub fn remote_place(text: &str) -> Option<Vec<String>> {
    let mut end = text.len().min(PLACE_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = end < text.len();
    let text = text[..end].trim();
    let head = text.get(..6)?;
    if !head.eq_ignore_ascii_case("remote") {
        return None;
    }
    let mut rest = &text[6..];
    if rest.chars().next().is_some_and(char::is_alphanumeric) {
        return None;
    }
    // "Remote-Friendly", "Remote first".
    for word in ["friendly", "first"] {
        let after = rest.trim_start_matches(['-', ' ']);
        if after
            .get(..word.len())
            .is_some_and(|w| w.eq_ignore_ascii_case(word))
            && !after[word.len()..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
        {
            rest = &after[word.len()..];
        }
    }
    // A cut text ends at its last separator, so no region is cut short.
    if cut {
        rest = rest
            .rsplit_once([',', '/', ';', '|', '(', ')'])
            .map_or("", |(head, _)| head);
    }
    Some(
        rest.split([',', '/', ';', '|', '(', ')'])
            .map(|s| s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '–' | ':')))
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("remote"))
            // "Remote or onsite", "Remote (hybrid)": alternatives, not regions.
            .filter(|s| {
                let lower = s.to_lowercase();
                let first = lower.split_whitespace().next().unwrap_or_default();
                !matches!(first, "or" | "and" | "&" | "+")
                    && !["onsite", "on-site", "on site", "hybrid", "office"]
                        .iter()
                        .any(|w| lower.contains(w))
            })
            .take(LISTED)
            .map(str::to_owned)
            .collect(),
    )
}

/// An employment type in schema.org's spelling (`FULL_TIME`): the posting's word upper-cased,
/// with `_` between words.
pub fn employment_type(s: &str) -> Option<String> {
    let upper: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let word = upper
        .split('_')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let word = match word.as_str() {
        "FULLTIME" => "FULL_TIME".to_owned(),
        "PARTTIME" => "PART_TIME".to_owned(),
        "CONTRACT" => "CONTRACTOR".to_owned(),
        "INTERNSHIP" => "INTERN".to_owned(),
        "PERDIEM" => "PER_DIEM".to_owned(),
        _ => word,
    };
    (!word.is_empty() && word.len() <= 32).then_some(word)
}

/// A `baseSalary`: a MonetaryAmount whose value is an amount or a QuantitativeValue (an amount,
/// or a minimum and maximum, and a unit), or an amount alone in `currency`.
fn salary(v: &Value, currency: &Value) -> Salary {
    let v = first(v);
    let value = v.get("value").unwrap_or(v);
    let one = number(value).or_else(|| number(&value["value"]));
    let mut out = Salary {
        min: number(&value["minValue"]).or(one),
        max: number(&value["maxValue"]).or(one),
        currency: text(&v["currency"])
            .or_else(|| text(currency))
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())),
        unit: text(&value["unitText"])
            .or_else(|| text(&v["unitText"]))
            .and_then(|u| salary_unit(&u)),
    };
    if out.min.is_none() {
        out.max = None;
    }
    if out.min.zip(out.max).is_some_and(|(min, max)| min > max) {
        std::mem::swap(&mut out.min, &mut out.max);
    }
    out
}
pub fn salary_unit(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_uppercase();
    let s = s.strip_prefix("PER ").unwrap_or(&s);
    let unit = match s.trim_end_matches('S') {
        "HOUR" | "HOURLY" => "HOUR",
        "DAY" | "DAILY" => "DAY",
        "WEEK" | "WEEKLY" => "WEEK",
        "MONTH" | "MONTHLY" => "MONTH",
        "YEAR" | "YEARLY" | "ANNUAL" | "ANNUALLY" | "ANNUM" => "YEAR",
        _ => return None,
    };
    Some(unit.into())
}

/// Where to apply: the posting's ApplyAction target, else its own `url`, else the page when the
/// posting says one applies there (`directApply`).
fn apply_url(v: &Value, base: &Url) -> Option<String> {
    all(&v["potentialAction"])
        .filter(|a| texts(&a["@type"]).iter().any(|t| t == "ApplyAction"))
        .find_map(|a| {
            let target = first(&a["target"]);
            text(target)
                .or_else(|| text(&target["urlTemplate"]))
                .or_else(|| text(&target["url"]))
        })
        .or_else(|| text(first(&v["url"])))
        .and_then(|u| origin_url(base, &u))
        .or_else(|| {
            let direct = v["directApply"] == true
                || v["directApply"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("true"));
            direct.then(|| base.to_string())
        })
}

/// When a posting was posted: a date alone is its midnight UTC.
pub fn posted_at(s: &str) -> Option<DateTime<Utc>> {
    crate::event::time::parse(s.trim(), None).and_then(crate::event::time::bounded)
}
/// When a posting closes: a date alone (with or without a `Z`) is valid through its whole day
/// (UTC).
pub fn closes_at(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    let at = posted_at(s)?;
    let date = s.strip_suffix(['Z', 'z']).unwrap_or(s);
    let date_only = ["%Y-%m-%d", "%Y%m%d"]
        .iter()
        .any(|f| chrono::NaiveDate::parse_from_str(date, f).is_ok());
    if date_only {
        crate::event::time::bounded(at + chrono::Duration::days(1))
    } else {
        Some(at)
    }
}

/// A posting's HTML description as a short plain-text excerpt.
pub fn excerpt(html: &str) -> Option<String> {
    let text = crate::resolver::pubky_apps::read::html_text(html)?;
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut: String = text.chars().take(EXCERPT).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    (!cut.is_empty()).then_some(cut)
}

fn fill(dst: &mut Option<String>, src: Option<String>) {
    if dst.is_none() {
        *dst = src;
    }
}
fn first(v: &Value) -> &Value {
    v.as_array().and_then(|a| a.first()).unwrap_or(v)
}
fn all(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array()
        .map_or_else(|| vec![v], |a| a.iter().take(32).collect())
        .into_iter()
        .filter(|v| !v.is_null())
}
fn text(v: &Value) -> Option<String> {
    crate::event::page::string(v)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn texts(v: &Value) -> Vec<String> {
    all(v).filter_map(text).collect()
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| {
            v.as_str()
                .and_then(|s| s.trim().replace(',', "").parse().ok())
        })
        .filter(|n| n.is_finite() && *n >= 0.0)
}
fn amount(n: f64) -> String {
    if n.fract() == 0.0 && n < 1e15 {
        format!("{}", n as u64)
    } else {
        format!("{n}")
    }
}
/// The first `LISTED` distinct values (ignoring ASCII case), in order.
fn dedup(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values
        .into_iter()
        .filter(|v| seen.insert(v.to_ascii_lowercase()))
        .take(LISTED)
        .collect()
}
/// Values joined with "; ", as many whole ones as an attribute holds (256 bytes).
fn joined(values: &[String]) -> Option<String> {
    let mut out = String::new();
    for value in dedup(values.to_vec()) {
        let sep = if out.is_empty() { "" } else { "; " };
        if out.len() + sep.len() + value.len() > 256 {
            break;
        }
        out.push_str(sep);
        out.push_str(&value);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Url {
        "https://jobs.example/postings/1".parse().unwrap()
    }

    #[test]
    fn a_posting_from_its_json_ld() {
        let p = from_jsonld(
            &json!({
                "@type": "JobPosting",
                "title": "Engineering Manager - EU",
                "description": "<p>Hi 👋 I’m <a href=\"x\">Colin</a>.</p><p>Read on.</p>",
                "datePosted": "2024-03-04",
                "validThrough": "2026-10-29",
                "hiringOrganization": {"@type": "Organization", "name": "Ashby", "sameAs": "https://www.ashbyhq.com"},
                "jobLocation": {"@type": "Place", "address": {"@type": "PostalAddress", "addressLocality": "", "addressRegion": "", "addressCountry": "European Union"}},
                "employmentType": "FULL_TIME",
                "baseSalary": {"@type": "MonetaryAmount", "currency": "EUR", "value": {"@type": "QuantitativeValue", "minValue": 150000, "maxValue": 250000, "unitText": "YEAR"}},
                "jobLocationType": "TELECOMMUTE",
                "applicantLocationRequirements": [{"@type": "Country", "name": "Spain"}, {"@type": "Country", "name": "Italy"}, {"@type": "Country", "name": "spain"}],
                "directApply": true
            }),
            &base(),
        );
        assert_eq!(p.title.as_deref(), Some("Engineering Manager - EU"));
        assert_eq!(p.description.as_deref(), Some("Hi 👋 I’m Colin. Read on."));
        assert_eq!(p.organization.as_deref(), Some("Ashby"));
        assert_eq!(
            p.organization_url.as_deref(),
            Some("https://www.ashbyhq.com/")
        );
        assert_eq!(p.employment_types, ["FULL_TIME"]);
        assert!(p.remote);
        assert_eq!(p.regions, ["Spain", "Italy"]);
        assert_eq!(
            p.salary,
            Salary {
                min: Some(150000.0),
                max: Some(250000.0),
                currency: Some("EUR".into()),
                unit: Some("YEAR".into())
            }
        );
        // A country alone is no workplace.
        assert!(p.workplace.is_none());
        assert_eq!(
            p.apply_url.as_deref(),
            Some("https://jobs.example/postings/1")
        );

        let mut doc = ResourceMeta::new("https://jobs.example/postings/1", "jsonld", 3600);
        p.apply(&mut doc);
        assert_eq!(doc.type_name, "JobPosting");
        assert_eq!(doc.start.unwrap().to_rfc3339(), "2024-03-04T00:00:00+00:00");
        // Valid through the whole of its last day.
        assert_eq!(doc.end.unwrap().to_rfc3339(), "2026-10-30T00:00:00+00:00");
        let a = &doc.attributes;
        assert_eq!(a["remote"], "true");
        assert_eq!(a["remote_regions"], "Spain; Italy");
        assert_eq!(a["salary_min"], "150000");
        assert_eq!(a["salary_max"], "250000");
        assert_eq!(a["salary_currency"], "EUR");
        assert_eq!(a["salary_unit"], "YEAR");
        assert_eq!(a["date_posted"], "2024-03-04");
        assert_eq!(a["valid_through"], "2026-10-29");
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(status(&doc, at("2026-10-29T23:00:00Z")), "open");
        assert_eq!(status(&doc, at("2026-10-30T00:00:00Z")), "closed");
    }

    #[test]
    fn a_workplace_and_its_spellings() {
        let p = from_jsonld(
            &json!({
                "@type": "JobPosting",
                "title": "Administrative Business Partner",
                "hiringOrganization": {"@type": "Organization", "name": "Palantir Technologies"},
                "jobLocation": {"@type": "Place", "address": {"@type": "PostalAddress", "addressLocality": "Singapore, Singapore", "addressRegion": null, "addressCountry": null, "postalCode": null}},
                "employmentType": "Full-time",
                "datePosted": "2026-08-11"
            }),
            &base(),
        );
        let w = p.workplace.unwrap();
        assert_eq!(w.address["city"], "Singapore, Singapore");
        assert_eq!(w.text, "Singapore, Singapore");
        assert!(!p.remote);
        assert_eq!(p.employment_types, ["FULL_TIME"]);
        assert_eq!(p.apply_url, None);

        let p = from_jsonld(
            &json!({"@type": "JobPosting", "jobLocation": [{"@type": "Place", "geo": {"latitude": 47.37, "longitude": 8.54}, "address": {"streetAddress": "Bahnhofstrasse 1", "addressLocality": "Zürich", "postalCode": "8001", "addressCountry": {"@type": "Country", "name": "CH"}}}],
                "potentialAction": {"@type": "ApplyAction", "target": {"@type": "EntryPoint", "urlTemplate": "/apply?id=1"}},
                "employmentType": ["Part time", "contract"],
                "baseSalary": {"currency": "chf", "value": {"value": "45.50", "unitText": "per hour"}}}),
            &base(),
        );
        let w = p.workplace.clone().unwrap();
        assert_eq!(w.text, "Bahnhofstrasse 1, 8001 Zürich, CH");
        assert_eq!(w.location.as_ref().unwrap().lat, 47.37);
        assert_eq!(
            p.apply_url.as_deref(),
            Some("https://jobs.example/apply?id=1")
        );
        assert_eq!(p.employment_types, ["PART_TIME", "CONTRACTOR"]);
        assert_eq!(p.salary.min, Some(45.5));
        assert_eq!(p.salary.max, Some(45.5));
        assert_eq!(p.salary.currency.as_deref(), Some("CHF"));
        assert_eq!(p.salary.unit.as_deref(), Some("HOUR"));
        let mut doc = ResourceMeta::new("https://jobs.example/postings/1", "jsonld", 3600);
        p.apply(&mut doc);
        assert_eq!(doc.location.as_ref().unwrap().lon, 8.54);
        assert_eq!(doc.attributes["salary_min"], "45.5");
        assert_eq!(doc.attributes["employment_type"], "PART_TIME; CONTRACTOR");
        assert_eq!(doc.end, None);
        assert_eq!(status(&doc, Utc::now()), "open");

        // A place that says the work is remote is no town.
        let p = from_jsonld(
            &json!({"@type": "JobPosting", "jobLocation": {"address": {"addressLocality": "Remote - US"}}}),
            &base(),
        );
        assert!(p.remote);
        assert_eq!(p.regions, ["US"]);
        assert!(p.workplace.is_none());
    }

    #[test]
    fn a_long_list_of_regions_costs_little() {
        // "Remote,R00000,R00001,…": 40,000 regions in one locality (280 KB) give the first 32,
        // read from the start of the text only, whole, and deduplicated in one pass.
        let many = (0..40_000)
            .map(|i| format!("R{i:05}"))
            .collect::<Vec<_>>()
            .join(",");
        let regions = remote_place(&format!("Remote,{many}")).unwrap();
        assert_eq!(regions.len(), LISTED);
        assert_eq!((&*regions[0], &*regions[31]), ("R00000", "R00031"));
        let p = from_jsonld(
            &json!({"@type": "JobPosting", "title": "Role",
                "jobLocation": {"address": {"addressLocality": format!("Remote,{many}")}}}),
            &base(),
        );
        assert!(p.remote);
        assert_eq!(p.regions.len(), LISTED);
        // A cut ends at a separator: no region is a part of one.
        let long = format!("Remote, {}", ["Region"; 400].join(", "));
        assert_eq!(
            remote_place(&long).unwrap(),
            vec!["Region".to_owned(); LISTED]
        );
        let words = format!("Remote, {}", "Somewhere ".repeat(200));
        assert_eq!(remote_place(&words), Some(vec![]));
        // The region the cut falls in is dropped, also when no separator comes before it.
        let token = format!("Remote-{}", "A".repeat(2000));
        assert_eq!(remote_place(&token), Some(vec![]));
        // A text is cut before it is trimmed, so its trailing spaces are not read to the end:
        // the cut falls in them, and the region they follow goes with it.
        let spaced = format!("Remote, Europe{}", " ".repeat(2000));
        assert_eq!(remote_place(&spaced), Some(vec![]));
        let short = format!("Remote, Europe{}", " ".repeat(500));
        assert_eq!(remote_place(&short), Some(vec!["Europe".to_owned()]));
        // A posting's place reaches remote_place as the page gives it, not trimmed first.
        for locality in [json!(spaced), json!({"name": spaced})] {
            let p = from_jsonld(
                &json!({"@type": "JobPosting", "title": "Role",
                    "jobLocation": {"address": {"addressLocality": locality}}}),
                &base(),
            );
            assert!(p.remote && p.workplace.is_none(), "{locality}");
            assert_eq!(p.regions, Vec::<String>::new(), "{locality}");
        }
        let p = from_jsonld(
            &json!({"@type": "JobPosting", "title": "Role", "jobLocation": {"address": short}}),
            &base(),
        );
        assert_eq!(p.regions, vec!["Europe".to_owned()]);
        // The dedup keeps the first of each value, ignoring case, and stops at 32.
        let values: Vec<String> = (0..40_000).map(|i| format!("v{}", i % 50)).collect();
        let kept = dedup([vec!["V1".to_owned()], values].concat());
        assert_eq!(kept.len(), LISTED);
        assert_eq!((&*kept[0], &*kept[1], &*kept[2]), ("V1", "v0", "v2"));
    }

    #[test]
    fn remote_places_and_words() {
        assert_eq!(remote_place("Remote"), Some(vec![]));
        assert_eq!(
            remote_place("Remote, United States"),
            Some(vec!["United States".into()])
        );
        assert_eq!(
            remote_place("REMOTE (EU / UK)"),
            Some(vec!["EU".into(), "UK".into()])
        );
        assert_eq!(remote_place("Remoteville"), None);
        assert_eq!(
            remote_place("Remote-Friendly, United States"),
            Some(vec!["United States".into()])
        );
        assert_eq!(remote_place("Remote First"), Some(vec![]));
        assert_eq!(
            remote_place("REMOTE(Worldwide) or ONSITE"),
            Some(vec!["Worldwide".into()])
        );
        assert_eq!(remote_place("Zürich"), None);
        assert_eq!(employment_type("volunteer").as_deref(), Some("VOLUNTEER"));
        assert_eq!(employment_type(" - "), None);
        assert_eq!(salary_unit("Annually").as_deref(), Some("YEAR"));
        assert_eq!(salary_unit("piece"), None);
        assert_eq!(
            closes_at("2026-11-01T12:00:00+01:00").unwrap().to_rfc3339(),
            "2026-11-01T11:00:00+00:00"
        );
        assert_eq!(closes_at("soon"), None);
        // Every date-only form is valid through its whole day.
        for date in ["2026-11-07", "20261107", "2026-11-07Z", "20261107Z"] {
            assert_eq!(
                closes_at(date).unwrap().to_rfc3339(),
                "2026-11-08T00:00:00+00:00",
                "{date}"
            );
        }
        assert_eq!(
            closes_at("2026-11-07T00:00:00Z").unwrap().to_rfc3339(),
            "2026-11-07T00:00:00+00:00"
        );
        // A salary maximum alone, from any route, is no salary.
        let mut doc = ResourceMeta::new("https://jobs.example/p", "greenhouse", 60);
        Posting {
            salary: Salary {
                min: None,
                max: Some(235200.0),
                currency: Some("USD".into()),
                unit: None,
            },
            ..Default::default()
        }
        .apply(&mut doc);
        assert!(!doc.attributes.keys().any(|k| k.starts_with("salary_")));
    }
}
