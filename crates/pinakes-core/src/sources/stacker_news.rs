//! Stacker News posts and comments, read from its GraphQL API by item id. No key.
use super::{text, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct StackerNews;

const API: &str = "https://stacker.news/api/graphql";

fn item(value: &str) -> Option<u64> {
    (!value.is_empty() && value.len() <= 12 && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
        .filter(|n| *n > 0)
}

/// The item of `https://stacker.news/items/{id}[/…]`; with `?commentId={c}` the comment `c`.
pub fn of_uri(uri: &str) -> Option<u64> {
    let url = web_url(uri)?;
    if !matches!(url.host_str()?, "stacker.news" | "www.stacker.news") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let ["items", id, ..] = segments.as_slice() else {
        return None;
    };
    let comment = url
        .query_pairs()
        .find(|(k, _)| k == "commentId")
        .and_then(|(_, v)| item(&v));
    comment.or_else(|| item(id))
}

pub fn root(id: u64) -> String {
    format!("https://stacker.news/items/{id}")
}

pub fn query(id: u64) -> Value {
    json!({
        "query": format!(
            "{{ item(id: \"{id}\") {{ id title url text createdAt user {{ name }} sub {{ name }} \
             root {{ id title }} parentId noteId ncomments sats deletedAt }} }}"
        )
    })
}

/// Markdown as plain text for an excerpt: images and heading marks dropped, links kept as text.
fn excerpt(markdown: &str) -> String {
    let mut out = String::new();
    for line in markdown.lines() {
        let line = line.trim().trim_start_matches(['#', '>']).trim();
        if line.is_empty() || line.starts_with("![") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
        if out.len() > 400 {
            break;
        }
    }
    let mut cut = out.chars().take(200).collect::<String>();
    if cut.len() < out.len() {
        cut.push('…');
    }
    cut
}

/// The card of an item, or `None` when there is no such item or it was deleted.
pub fn card(
    mut doc: ResourceMeta,
    id: u64,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    anyhow::ensure!(status == 200, "Stacker News answered {status}");
    let v: Value = serde_json::from_slice(body)?;
    if let Some(errors) = v["errors"].as_array().filter(|e| !e.is_empty()) {
        anyhow::bail!("Stacker News error: {}", errors[0]["message"]);
    }
    let node = &v["data"]["item"];
    if node.is_null() || !node["deletedAt"].is_null() {
        return Ok(None);
    }
    anyhow::ensure!(
        node["id"] == id.to_string().as_str(),
        "Stacker News answered for another item"
    );
    let root_title = text(&node["root"]["title"]);
    match text(&node["title"]) {
        Some(title) => {
            doc.set_type("DiscussionForumPosting");
            doc.name = Some(title);
        }
        // A comment has no title of its own: it takes its thread's.
        None => {
            doc.set_type("Comment");
            doc.name = root_title.map(|t| format!("Comment on “{t}”"));
        }
    }
    doc.description = text(&node["text"])
        .map(|t| excerpt(&t))
        .filter(|t| !t.is_empty());
    doc.image = Some(format!("https://capture.stacker.news/items/{id}"));
    for (key, value) in [
        ("author", text(&node["user"]["name"])),
        ("territory", text(&node["sub"]["name"])),
        ("published", text(&node["createdAt"])),
        (
            "comment_count",
            node["ncomments"].as_u64().map(|n| n.to_string()),
        ),
        ("sats", node["sats"].as_u64().map(|n| n.to_string())),
        ("link", text(&node["url"]).filter(|u| web_url(u).is_some())),
        (
            "thread",
            text(&node["root"]["id"]).and_then(|r| item(&r)).map(root),
        ),
        ("nostr", text(&node["noteId"]).map(|n| format!("nostr:{n}"))),
        ("page_url", Some(root(id))),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for StackerNews {
    fn name(&self) -> &'static str {
        "stacker_news"
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = root(of_uri(uri)?);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "author",
            "territory",
            "published",
            "comment_count",
            "sats",
            "link",
            "thread",
            "nostr",
            "page_url",
        ]
    }
    fn social(&self, _doc: &ResourceMeta) -> bool {
        true
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://stacker.news/items/1596515",
                card_type: "DiscussionForumPosting",
            },
            Example {
                uri: "https://stacker.news/items/1596515?commentId=1596517",
                card_type: "Comment",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let id = of_uri(uri).context("not a Stacker News item")?;
        let f = cx
            .fetch(
                API,
                &FetchOptions {
                    accept: Some("application/json".into()),
                    json_body: Some(query(id)),
                    ..Default::default()
                },
            )
            .await?;
        Ok(card(cx.card(uri, self), id, f.status, &f.body)?
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("stacker_news", file)
    }
    fn base(id: u64) -> ResourceMeta {
        ResourceMeta::new(&root(id), "stacker_news", 3600)
    }

    #[test]
    fn addresses() {
        for (uri, root) in [
            (
                "https://stacker.news/items/1596515/r/someone",
                "https://stacker.news/items/1596515",
            ),
            (
                "https://stacker.news/items/1596515?r=someone",
                "https://stacker.news/items/1596515",
            ),
            (
                "https://stacker.news/items/1596515?commentId=1596517",
                "https://stacker.news/items/1596517",
            ),
            (
                "https://stacker.news/items/1596515/related",
                "https://stacker.news/items/1596515",
            ),
        ] {
            assert_eq!(StackerNews.alias_root(uri).as_deref(), Some(root), "{uri}");
        }
        assert_eq!(
            StackerNews.alias_root("https://stacker.news/items/1596515"),
            None
        );
        for uri in [
            "https://stacker.news/~bitcoin",
            "https://stacker.news/items/x",
            "https://stacker.news/",
        ] {
            assert!(!StackerNews.reads(uri), "{uri}");
        }
        let recorded: Value = serde_json::from_slice(&read("provenance.json")).unwrap();
        assert_eq!(recorded["sn-item-1"]["body"], query(1));
    }

    #[test]
    fn posts_and_comments() {
        let doc = card(base(1596515), 1596515, 200, &read("sn-item-1596515.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "DiscussionForumPosting");
        assert_eq!(
            doc.name.as_deref(),
            Some("Sword < Pen < Home Mining < High Hashrate Home Mining")
        );
        let text = doc.description.as_deref().unwrap();
        assert!(text.starts_with("I really hope they don't have CCTV at Microcenter."));
        assert!(text.len() <= 204 && !text.contains("!["));
        assert_eq!(
            doc.image.as_deref(),
            Some("https://capture.stacker.news/items/1596515")
        );
        let a = &doc.attributes;
        assert_eq!(a["author"], "jasonb");
        assert_eq!(a["territory"], "DIY");
        assert!(a["nostr"].starts_with("nostr:naddr1"));

        let doc = card(base(1596517), 1596517, 200, &read("sn-item-1596517.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "Comment");
        assert_eq!(
            doc.name.as_deref(),
            Some("Comment on “Sword < Pen < Home Mining < High Hashrate Home Mining”")
        );
        assert_eq!(
            doc.attributes["thread"],
            "https://stacker.news/items/1596515"
        );

        let doc = card(base(1), 1, 200, &read("sn-item-1.json"))
            .unwrap()
            .unwrap();
        assert_eq!(
            doc.attributes["link"],
            "https://bitcoinmagazine.com/business/el-salvador-approves-bitcoin-legal-tender"
        );
        assert!(card(
            base(999999999),
            999999999,
            200,
            &read("sn-item-missing.json")
        )
        .unwrap()
        .is_none());
        assert!(card(base(2), 2, 200, &read("sn-item-1.json")).is_err());
    }
}
