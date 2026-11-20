//! Stacker News posts and comments, read from its GraphQL API by item id. No key.
use super::{text, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct StackerNews;

const API: &str = "https://stacker.news/api/graphql";

fn item(value: &str) -> Option<u64> {
    super::kit::number(value, 12)
}

/// The item of `https://stacker.news/items/{id}[/…]`; with `?commentId={c}` the comment `c`.
pub fn of_uri(uri: &str) -> Option<u64> {
    let (item, comment) = parts(uri)?;
    Some(comment.unwrap_or(item))
}

/// The item a link's path names and the comment its `?commentId=` names, if any. The comment
/// counts only once its own record says it is in that item's thread.
fn parts(uri: &str) -> Option<(u64, Option<u64>)> {
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
    Some((item(id)?, comment.filter(|c| Some(*c) != item(id))))
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

/// The card of an item, or `None` when there is no such item or it was deleted.
pub fn card(
    mut doc: ResourceMeta,
    id: u64,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    anyhow::ensure!(status == 200, "Stacker News answered {status}");
    let v: Value = serde_json::from_slice(body)?;
    if v["errors"].as_array().is_some_and(|e| !e.is_empty()) {
        // Their text may repeat the request, link and all: only that there was an error.
        anyhow::bail!("Stacker News answered an error");
    }
    anyhow::ensure!(v["data"].is_object(), "Stacker News answered no data");
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
        .map(|t| super::markdown_excerpt(&t))
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
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "stacker_news",
                uri: "https://stacker.news/items/1596515",
                card_type: "DiscussionForumPosting",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "stacker.news",
                path: "/api/graphql",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "sn-item-1596515.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    /// A link naming a comment is an alias only after its fetch (the comment must be in the
    /// thread its path names).
    fn alias_root(&self, uri: &str) -> Option<String> {
        let (item, None) = parts(uri)? else {
            return None;
        };
        let root = root(item);
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
        let (item, comment) = parts(uri).context("not a Stacker News item")?;
        let get = |id: u64| async move {
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
            card(cx.card(uri, self), id, f.status, &f.body)
        };
        if let Some(comment) = comment {
            // The comment, when its record puts it in the path's thread; else the thread.
            if let Some(mut doc) = get(comment).await? {
                if doc.attributes.get("thread") == Some(&root(item)) {
                    doc.source_root = Some(root(comment));
                    return Ok(doc);
                }
            }
        }
        let mut doc = get(item).await?.unwrap_or_else(|| cx.missing(uri, self));
        if comment.is_some() {
            doc.source_root = Some(root(item));
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("stacker_news", file)
    }

    /// An error answer gives an error without Stacker News' text, which can repeat the request
    /// (a canary token in a link stays out of it).
    #[test]
    fn an_error_answer_repeats_none_of_its_text() {
        let body = serde_json::json!({"errors": [{"message": "bad item https://stacker.news/items/1?token=CANARY"}]});
        let doc = ResourceMeta::new("https://stacker.news/items/1", "stacker_news", 3600);
        let error = card(doc, 1, 200, &serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(!format!("{error:#}").contains("CANARY"), "{error:#}");
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
        // A comment named by `?commentId=` is not trusted from the link alone.
        assert_eq!(
            StackerNews.alias_root("https://stacker.news/items/1596515?commentId=1596517"),
            None
        );
        for uri in [
            "https://stacker.news/~bitcoin",
            "https://stacker.news/items/x",
            "https://stacker.news/",
        ] {
            assert!(!StackerNews.reads(uri), "{uri}");
        }
        // The comment's own record names its thread, which the link's path must match.
        let comment = card(base(1596517), 1596517, 200, &read("sn-item-1596517.json"))
            .unwrap()
            .unwrap();
        assert_eq!(comment.attributes["thread"], root(1596515));
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
        // An answer without `data` says nothing about the item: an error, not "gone".
        assert!(card(base(1), 1, 200, b"{}").is_err());
        assert!(card(base(1), 1, 200, br#"{"data":null}"#).is_err());
    }
}
