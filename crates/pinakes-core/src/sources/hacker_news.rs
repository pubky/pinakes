//! Hacker News stories, comments, jobs and polls, read from the official Firebase API by item id.
//! hnsignals.com names items by the same id. No key; no published limit.
use super::{text, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct HackerNews;

fn item(value: &str) -> Option<u64> {
    (!value.is_empty() && value.len() <= 12 && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
        .filter(|n| *n > 0)
}

/// The item of `news.ycombinator.com/item?id={n}`, `hnsignals.com/signal/{n}` or the API.
pub fn of_uri(uri: &str) -> Option<u64> {
    let url = web_url(uri)?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (url.host_str()?, segments.as_slice()) {
        ("news.ycombinator.com", ["item"]) => url
            .query_pairs()
            .find(|(k, _)| k == "id")
            .and_then(|(_, v)| item(&v)),
        ("hnsignals.com" | "www.hnsignals.com", ["signal", id, ..]) => item(id),
        ("hacker-news.firebaseio.com", ["v0", "item", file]) => item(file.strip_suffix(".json")?),
        _ => None,
    }
}

pub fn root(id: u64) -> String {
    format!("https://news.ycombinator.com/item?id={id}")
}

/// HN's HTML-ish comment text as plain text.
fn plain(html: &str) -> String {
    let text = crate::resolver::cards::plain(&html.replace("<p>", "\n"));
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut: String = text.chars().take(200).collect();
    if cut.len() < text.len() {
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
    anyhow::ensure!(status == 200, "Hacker News answered {status}");
    let v: Value = serde_json::from_slice(body)?;
    if v.is_null() || v["deleted"] == true {
        return Ok(None);
    }
    anyhow::ensure!(
        v["id"].as_u64() == Some(id),
        "Hacker News answered for another item"
    );
    let kind = text(&v["type"]).unwrap_or_default();
    let by = text(&v["by"]);
    if kind == "comment" {
        doc.set_type("Comment");
        doc.name = by.as_ref().map(|by| format!("Comment by {by}"));
    } else {
        doc.set_type("DiscussionForumPosting");
        doc.name = text(&v["title"]);
    }
    doc.description = text(&v["text"])
        .map(|t| plain(&t))
        .filter(|t| !t.is_empty());
    let published = v["time"]
        .as_i64()
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    for (key, value) in [
        ("author", by),
        ("item_type", Some(kind).filter(|k| !k.is_empty())),
        ("published", published),
        ("score", v["score"].as_u64().map(|n| n.to_string())),
        (
            "comment_count",
            v["descendants"].as_u64().map(|n| n.to_string()),
        ),
        ("link", text(&v["url"]).filter(|u| web_url(u).is_some())),
        ("parent", v["parent"].as_u64().map(root)),
        ("page_url", Some(root(id))),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for HackerNews {
    fn name(&self) -> &'static str {
        "hacker_news"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "hacker_news",
                uri: "https://news.ycombinator.com/item?id=8863",
                card_type: "DiscussionForumPosting",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "hacker-news.firebaseio.com",
                path: "/v0/item/8863.json",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "hn-item-8863.json",
            }],
        })
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
            "item_type",
            "published",
            "score",
            "comment_count",
            "link",
            "parent",
            "page_url",
        ]
    }
    fn social(&self, _doc: &ResourceMeta) -> bool {
        true
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://news.ycombinator.com/item?id=8863",
                card_type: "DiscussionForumPosting",
            },
            Example {
                uri: "https://hnsignals.com/signal/2921983",
                card_type: "Comment",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let id = of_uri(uri).context("not a Hacker News item")?;
        let f = cx
            .fetch(
                &format!("https://hacker-news.firebaseio.com/v0/item/{id}.json"),
                &FetchOptions {
                    accept: Some("application/json".into()),
                    ..Default::default()
                },
            )
            .await?;
        let doc = cx.card(uri, self);
        Ok(
            crate::resolver::html::blocking(move || card(doc, id, f.status, &f.body))
                .await??
                .unwrap_or_else(|| cx.missing(uri, self)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("hacker_news", file)
    }
    fn base(id: u64) -> ResourceMeta {
        ResourceMeta::new(&root(id), "hacker_news", 3600)
    }

    #[test]
    fn addresses() {
        for uri in [
            "https://news.ycombinator.com/item?id=8863&p=2",
            "https://hnsignals.com/signal/8863",
            "https://hacker-news.firebaseio.com/v0/item/8863.json",
        ] {
            assert_eq!(
                HackerNews.alias_root(uri).as_deref(),
                Some("https://news.ycombinator.com/item?id=8863"),
                "{uri}"
            );
        }
        assert_eq!(
            HackerNews.alias_root("https://news.ycombinator.com/item?id=8863"),
            None
        );
        for uri in [
            "https://news.ycombinator.com/user?id=pg",
            "https://news.ycombinator.com/item?id=x",
            "https://news.ycombinator.com/",
        ] {
            assert!(!HackerNews.reads(uri), "{uri}");
        }
    }

    #[test]
    fn stories_and_comments() {
        let doc = card(base(8863), 8863, 200, &read("hn-item-8863.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "DiscussionForumPosting");
        assert_eq!(
            doc.name.as_deref(),
            Some("My YC app: Dropbox - Throw away your USB drive")
        );
        let a = &doc.attributes;
        assert_eq!(a["author"], "dhouston");
        assert_eq!(a["published"], "2007-04-04T19:16:40Z");
        assert_eq!(a["comment_count"], "71");
        assert_eq!(a["link"], "http://www.getdropbox.com/u/2/screencast.html");

        let doc = card(base(2921983), 2921983, 200, &read("hn-item-2921983.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "Comment");
        assert_eq!(doc.name.as_deref(), Some("Comment by norvig"));
        assert!(doc
            .description
            .unwrap()
            .starts_with("Aw shucks, guys ... you make me blush"));
        assert_eq!(
            doc.attributes["parent"],
            "https://news.ycombinator.com/item?id=2921506"
        );
        assert!(card(
            base(999999999),
            999999999,
            200,
            &read("hn-item-missing.json")
        )
        .unwrap()
        .is_none());
        assert!(card(base(1), 1, 200, &read("hn-item-8863.json")).is_err());
    }
}
