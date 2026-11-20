//! Lobsters stories, read from the story's own JSON (`/s/{id}.json`). No key.
use super::{list, text, texts, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct Lobsters;

/// A story's short id: six lower-case letters and digits.
fn short_id(value: &str) -> Option<&str> {
    let value = value.strip_suffix(".json").unwrap_or(value);
    ((3..=12).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
    .then_some(value)
}

/// The story of `https://lobste.rs/s/{id}[/{slug}]` or `…/s/{id}.json`.
pub fn of_uri(uri: &str) -> Option<String> {
    let url = super::kit::web_url(uri)?;
    if !matches!(url.host_str()?, "lobste.rs" | "www.lobste.rs") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["s", id, ..] => short_id(id).map(str::to_owned),
        _ => None,
    }
}

pub fn root(id: &str) -> String {
    format!("https://lobste.rs/s/{id}")
}

/// The card of a story, or `None` when Lobsters has no such story.
pub fn card(
    mut doc: ResourceMeta,
    id: &str,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let Some(v) = super::kit::json(status, body, &[404])? else {
        return Ok(None);
    };
    anyhow::ensure!(v["short_id"] == id, "Lobsters answered for another story");
    doc.set_type("DiscussionForumPosting");
    doc.name = Some(text(&v["title"]).context("no title")?);
    doc.description = text(&v["description_plain"]).map(|d| super::excerpt(&d, 200));
    for (key, value) in [
        ("author", text(&v["submitter_user"])),
        ("published", text(&v["created_at"])),
        ("score", v["score"].as_i64().map(|n| n.to_string())),
        (
            "comment_count",
            v["comment_count"].as_u64().map(|n| n.to_string()),
        ),
        ("tags", list(&texts(&v["tags"]))),
        ("link", text(&v["url"]).filter(|u| web_url(u).is_some())),
        ("page_url", Some(root(id))),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for Lobsters {
    fn name(&self) -> &'static str {
        "lobsters"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "lobsters",
                uri: "https://lobste.rs/s/wtthju/a5_v1_0",
                card_type: "DiscussionForumPosting",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "lobste.rs",
                path: "/s/wtthju.json",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "lobsters-wtthju.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = root(&of_uri(uri)?);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "author",
            "published",
            "score",
            "comment_count",
            "tags",
            "link",
            "page_url",
        ]
    }
    fn social(&self, _doc: &ResourceMeta) -> bool {
        true
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://lobste.rs/s/wtthju/a5_v1_0",
            card_type: "DiscussionForumPosting",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let id = of_uri(uri).context("not a Lobsters story")?;
        let f = cx
            .fetch(
                &format!("https://lobste.rs/s/{id}.json"),
                &FetchOptions::json(),
            )
            .await?;
        Ok(card(cx.card(uri, self), &id, f.status, &f.body)?
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("lobsters", file)
    }

    #[test]
    fn addresses() {
        for uri in [
            "https://lobste.rs/s/wtthju/a5_v1_0",
            "https://lobste.rs/s/wtthju.json",
            "https://lobste.rs/s/wtthju/",
        ] {
            assert_eq!(
                Lobsters.alias_root(uri).as_deref(),
                Some("https://lobste.rs/s/wtthju"),
                "{uri}"
            );
        }
        assert_eq!(Lobsters.alias_root("https://lobste.rs/s/wtthju"), None);
        for uri in [
            "https://lobste.rs/t/rust",
            "https://lobste.rs/s/WTTHJU",
            "https://lobste.rs/",
        ] {
            assert!(!Lobsters.reads(uri), "{uri}");
        }
    }

    #[test]
    fn story() {
        let base = ResourceMeta::new(&root("wtthju"), "lobsters", 3600);
        let doc = card(base.clone(), "wtthju", 200, &read("lobsters-wtthju.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "DiscussionForumPosting");
        assert_eq!(doc.name.as_deref(), Some("A5 v1.0"));
        assert_eq!(doc.description, None);
        let a = &doc.attributes;
        assert_eq!(a["author"], "pheelicks");
        assert_eq!(a["tags"], "compsci; math; rust; visualization");
        assert_eq!(a["link"], "https://a5geo.org/blog/2026/10/14/a5-v1");
        assert!(
            card(base.clone(), "zzzzzz", 404, &read("lobsters-missing.html"))
                .unwrap()
                .is_none()
        );
        assert!(card(base.clone(), "abcdef", 200, &read("lobsters-wtthju.json")).is_err());
        // A long description is cut to 200 characters.
        let mut long: serde_json::Value =
            serde_json::from_slice(&read("lobsters-wtthju.json")).unwrap();
        long["description_plain"] = "word ".repeat(500).into();
        let doc = card(base, "wtthju", 200, &serde_json::to_vec(&long).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(doc.description.unwrap().chars().count(), 201);
    }
}
