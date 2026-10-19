//! The Electronic Code of Federal Regulations: CFR sections and parts, read from the eCFR
//! versioner API (the HTML is bot-walled). Every eCFR address of a section or part, current or
//! at a date, is an alias of its current address. Public domain; no key.
use super::{web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Ecfr;

/// A CFR section (`17`, `240.10b-5`) or part (`14`, `39`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Section { title: u16, section: String },
    Part { title: u16, part: String },
}

fn title(value: &str) -> Option<u16> {
    (!value.is_empty() && value.len() <= 2 && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
        .filter(|t| (1..=50).contains(t))
}
fn part(value: &str) -> Option<&str> {
    (!value.is_empty() && value.len() <= 12 && value.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then_some(value)
}
/// `240.10b-5`: the part, a dot, and letters, digits and hyphens.
fn section(value: &str) -> Option<&str> {
    let (p, rest) = value.split_once('.')?;
    (part(p).is_some()
        && !rest.is_empty()
        && value.len() <= 40
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'))
    .then_some(value)
}

impl Target {
    /// `https://www.ecfr.gov/{current|on/YYYY-MM-DD}/title-{t}/…/section-{s}` or `…/part-{p}`.
    /// The chapter, subchapter, subpart and subject-group segments never matter.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if !matches!(url.host_str()?, "www.ecfr.gov" | "ecfr.gov") {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let rest = match segments.as_slice() {
            ["current", rest @ ..] => rest,
            ["on", date, rest @ ..]
                if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok() =>
            {
                rest
            }
            _ => return None,
        };
        let title = title(rest.first()?.strip_prefix("title-")?)?;
        let mut found_part = None;
        let mut found_section = None;
        for segment in &rest[1..] {
            if let Some(p) = segment.strip_prefix("part-") {
                found_part = Some(part(p)?);
            } else if let Some(s) = segment.strip_prefix("section-") {
                found_section = Some(section(s)?);
            } else if ![
                "chapter-",
                "subchapter-",
                "subpart-",
                "subject-group-",
                "appendix-",
            ]
            .iter()
            .any(|p| segment.starts_with(p))
            {
                return None;
            }
        }
        match (found_part, found_section) {
            (part, Some(section)) => {
                // A part named next to the section must be the section's own.
                let own = section.split_once('.').map(|(p, _)| p);
                (part.is_none() || part == own).then(|| Self::Section {
                    title,
                    section: section.to_owned(),
                })
            }
            (Some(part), None) => Some(Self::Part {
                title,
                part: part.to_owned(),
            }),
            (None, None) => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Section { title, section } => {
                format!("https://www.ecfr.gov/current/title-{title}/section-{section}")
            }
            Self::Part { title, part } => {
                format!("https://www.ecfr.gov/current/title-{title}/part-{part}")
            }
        }
    }
    /// The ancestry of the section or part today: its own heading and every level above it.
    pub fn api_url(&self) -> String {
        match self {
            Self::Section { title, section } => format!(
                "https://www.ecfr.gov/api/versioner/v1/ancestry/current/title-{title}.json?section={section}"
            ),
            Self::Part { title, part } => format!(
                "https://www.ecfr.gov/api/versioner/v1/ancestry/current/title-{title}.json?part={part}"
            ),
        }
    }
    /// The citation, e.g. `17 CFR 240.10b-5` or `14 CFR 39`.
    pub fn citation(&self) -> String {
        match self {
            Self::Section { title, section } => format!("{title} CFR {section}"),
            Self::Part { title, part } => format!("{title} CFR {part}"),
        }
    }
}

fn level(node: &Value) -> Option<(String, String)> {
    let level = node["label_level"].as_str()?.trim().to_owned();
    let description = node["label_description"].as_str()?.trim().to_owned();
    Some((level, description))
}

/// The card of a section or part from its ancestry, or `None` when eCFR has no such object.
pub fn card(
    mut doc: ResourceMeta,
    target: &Target,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    match status {
        200 => {}
        404 => return Ok(None),
        s => anyhow::bail!("eCFR answered {s}"),
    }
    let v: Value = serde_json::from_slice(body)?;
    let ancestors = v["ancestors"].as_array().context("no ancestors")?;
    let own = ancestors.last().context("empty ancestry")?;
    let (kind, id) = match target {
        Target::Section { section, .. } => ("section", section),
        Target::Part { part, .. } => ("part", part),
    };
    anyhow::ensure!(
        own["type"] == kind && own["identifier"] == id.as_str(),
        "eCFR answered for another object"
    );
    let (label, heading) = level(own).context("no heading")?;
    doc.set_type("Legislation");
    doc.name = Some(format!("{label} {heading}").trim().to_owned());
    let path: Vec<String> = ancestors[..ancestors.len() - 1]
        .iter()
        .filter(|a| matches!(a["type"].as_str(), Some("title" | "chapter" | "part")))
        .filter_map(level)
        .map(|(l, d)| format!("{l} — {d}"))
        .collect();
    doc.description = (!path.is_empty()).then(|| path.join(" · "));
    let named = |t: &str| {
        ancestors
            .iter()
            .find(|a| a["type"] == t)
            .and_then(level)
            .map(|(_, d)| d)
    };
    for (key, value) in [
        ("citation", Some(target.citation())),
        ("cfr_title", named("title")),
        ("chapter", named("chapter")),
        ("cfr_part", named("part")),
        (
            "reserved",
            (own["reserved"] == true).then(|| "true".to_owned()),
        ),
        ("page_url", Some(target.root())),
    ] {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for Ecfr {
    fn name(&self) -> &'static str {
        "ecfr"
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = Target::of_uri(uri)?.root();
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "citation",
            "cfr_title",
            "chapter",
            "cfr_part",
            "reserved",
            "page_url",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.ecfr.gov/current/title-17/section-240.10b-5",
                card_type: "Legislation",
            },
            Example {
                uri: "https://www.ecfr.gov/current/title-14/chapter-I/subchapter-C/part-39",
                card_type: "Legislation",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not an eCFR link")?;
        let f = cx
            .fetch(
                &target.api_url(),
                &FetchOptions {
                    accept: Some("application/json".into()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(card(cx.card(uri, self), &target, f.status, &f.body)?
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("ecfr", file)
    }
    fn section(t: u16, s: &str) -> Target {
        Target::Section {
            title: t,
            section: s.into(),
        }
    }

    #[test]
    fn addresses() {
        let root = "https://www.ecfr.gov/current/title-17/section-240.10b-5";
        for uri in [
            "https://www.ecfr.gov/current/title-17/chapter-II/part-240/subpart-A/subject-group-ECFRbda83517ce4377f/section-240.10b-5",
            "https://www.ecfr.gov/current/title-17/part-240/section-240.10b-5",
            "https://www.ecfr.gov/on/2020-01-01/title-17/section-240.10b-5",
            "https://ecfr.gov/current/title-17/section-240.10b-5",
        ] {
            assert_eq!(Target::of_uri(uri), Some(section(17, "240.10b-5")), "{uri}");
            assert_eq!(Ecfr.alias_root(uri).as_deref(), Some(root), "{uri}");
        }
        assert_eq!(Ecfr.alias_root(root), None);
        // A fragment is dropped by normalization: the link is the root itself.
        let anchored = "https://www.ecfr.gov/current/title-17/section-240.10b-5#p-240.10b-5(a)";
        assert_eq!(Target::of_uri(anchored), Some(section(17, "240.10b-5")));
        assert_eq!(Ecfr.alias_root(anchored), None);
        assert_eq!(
            Ecfr.alias_root("https://www.ecfr.gov/current/title-14/chapter-I/subchapter-C/part-39")
                .as_deref(),
            Some("https://www.ecfr.gov/current/title-14/part-39")
        );
        for uri in [
            "https://www.ecfr.gov/current/title-17",
            "https://www.ecfr.gov/current/title-17/part-241/section-240.10b-5",
            "https://www.ecfr.gov/current/title-99/part-1",
            "https://www.ecfr.gov/on/yesterday/title-17/part-240",
            "https://www.ecfr.gov/cgi-bin/text-idx?node=se17.4.240_110b_65",
            "https://www.ecfr.gov/current/title-17/section-240",
            "https://www.ecfr.gov/current/title-17/search/part-240",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            section(17, "240.10b-5").api_url(),
            "https://www.ecfr.gov/api/versioner/v1/ancestry/current/title-17.json?section=240.10b-5"
        );
    }

    #[test]
    fn section_and_part_cards() {
        let target = section(17, "240.10b-5");
        let base = ResourceMeta::new(&target.root(), "ecfr", 3600);
        let raw = read("ancestry_current_title-17_section-240.10b-5.json");
        let doc = card(base.clone(), &target, 200, &raw).unwrap().unwrap();
        assert_eq!(doc.type_name, "Legislation");
        assert_eq!(
            doc.name.as_deref(),
            Some("§ 240.10b-5 Employment of manipulative and deceptive devices.")
        );
        assert_eq!(
            doc.description.as_deref(),
            Some(
                "Title 17 — Commodity and Securities Exchanges · Chapter II — Securities and \
                 Exchange Commission · Part 240 — General Rules and Regulations, Securities \
                 Exchange Act of 1934"
            )
        );
        let a = &doc.attributes;
        assert_eq!(a["citation"], "17 CFR 240.10b-5");
        assert_eq!(a["cfr_title"], "Commodity and Securities Exchanges");
        assert_eq!(a["chapter"], "Securities and Exchange Commission");
        assert!(!a.contains_key("reserved"));
        // The ancestry must end at the object asked for.
        assert!(card(base.clone(), &section(17, "240.10b-6"), 200, &raw).is_err());

        let part = Target::Part {
            title: 14,
            part: "39".into(),
        };
        let raw = read("ancestry_current_title-14_part-39.json");
        let doc = card(base.clone(), &part, 200, &raw).unwrap().unwrap();
        assert_eq!(
            doc.name.as_deref(),
            Some("Part 39 Airworthiness Directives")
        );
        assert_eq!(doc.attributes["citation"], "14 CFR 39");
        assert_eq!(doc.attributes["cfr_part"], "Airworthiness Directives");

        let missing = read("ancestry_current_title-17_section-240.99zz-missing.json");
        assert!(card(base.clone(), &section(17, "240.99zz"), 404, &missing)
            .unwrap()
            .is_none());
        assert!(card(base, &part, 503, b"").is_err());
    }
}
