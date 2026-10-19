//! Discourse forums (any site running Discourse): topics and posts, read from the forum's own
//! JSON (`/t/{id}.json`, `/t/{id}/{n}.json`, `/posts/{id}.json`). No key; Discourse's default
//! limit is 200 requests per minute per address. A site that does not answer as Discourse is
//! remembered, and its links get ordinary web cards.
use super::{list, text, web_url, Context, Declined, Example, HostMemo, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::LazyLock;

pub struct Discourse;

/// Which hosts answered as Discourse, and which did not.
static FORUMS: LazyLock<HostMemo> = LazyLock::new(HostMemo::default);

fn number(value: &str) -> Option<u64> {
    (!value.is_empty() && value.len() <= 10 && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
        .filter(|n| *n > 0)
}
fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().any(|b| !b.is_ascii_digit())
        && !value.contains(['/', '?', '#'])
}

/// What a forum link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A topic, or post `n` in it (`/t/{slug}/{id}/{n}`, `/t/{id}/{n}`).
    Topic {
        host: String,
        id: u64,
        post: Option<u64>,
    },
    /// A post by its own id (`/p/{id}`).
    Post { host: String, id: u64 },
}

impl Target {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if url.scheme() != "https" || url.port().is_some() {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let topic = |id: &str, post: Option<&&str>| {
            let post = match post {
                Some(n) => Some(number(n)?),
                None => None,
            };
            Some(Self::Topic {
                host: host.clone(),
                id: number(id)?,
                post: post.filter(|n| *n > 1),
            })
        };
        match segments.as_slice() {
            ["t", s, id] if slug(s) && number(id).is_some() => topic(id, None),
            ["t", s, id, n] if slug(s) => topic(id, Some(n)),
            ["t", id] => topic(id, None),
            ["t", id, n] if number(id).is_some() => topic(id, Some(n)),
            ["p", id] => Some(Self::Post {
                host: host.clone(),
                id: number(id)?,
            }),
            _ => None,
        }
    }
    fn host(&self) -> &str {
        match self {
            Self::Topic { host, .. } | Self::Post { host, .. } => host,
        }
    }
    pub fn api_url(&self) -> String {
        match self {
            Self::Topic {
                host,
                id,
                post: Some(n),
            } => format!("https://{host}/t/{id}/{n}.json"),
            Self::Topic {
                host,
                id,
                post: None,
            } => format!("https://{host}/t/{id}.json"),
            Self::Post { host, id } => format!("https://{host}/posts/{id}.json"),
        }
    }
}

fn root(host: &str, slug: &str, topic: u64, post: Option<u64>) -> String {
    match post.filter(|n| *n > 1) {
        Some(n) => format!("https://{host}/t/{slug}/{topic}/{n}"),
        None => format!("https://{host}/t/{slug}/{topic}"),
    }
}

fn excerpt(html: &str) -> String {
    let text = crate::resolver::cards::plain(html);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut: String = text.chars().take(200).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    cut
}

/// Discourse answers JSON; anything else (a 404 page, a challenge) is another site.
fn json(status: u16, body: &[u8]) -> Result<Option<Value>> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Err(Declined.into());
    };
    match status {
        200 => Ok(Some(v)),
        404 if v["error_type"] == "not_found" => Ok(None),
        401 | 403 | 404 | 405 => Err(Declined.into()),
        s => anyhow::bail!("the forum answered {s}"),
    }
}

/// The card of a topic, or of post `n` in it, from `/t/{id}[/{n}].json`.
pub fn topic_card(
    mut doc: ResourceMeta,
    host: &str,
    id: u64,
    post: Option<u64>,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let Some(v) = json(status, body)? else {
        return Ok(None);
    };
    let (Some(title), Some(slug)) = (text(&v["title"]), text(&v["slug"])) else {
        return Err(Declined.into());
    };
    anyhow::ensure!(
        v["id"].as_u64() == Some(id),
        "the forum answered for another topic"
    );
    let posts = v["post_stream"]["posts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let n = post.unwrap_or(1);
    let shown = posts.iter().find(|p| p["post_number"].as_u64() == Some(n));
    if post.is_some() && shown.is_none() {
        return Ok(None);
    }
    let tags: Vec<String> = v["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| text(t).or_else(|| text(&t["name"])))
        .collect();
    if post.is_some() {
        doc.set_type("Comment");
        doc.name = Some(format!("Comment on “{title}”"));
    } else {
        doc.set_type("DiscussionForumPosting");
        doc.name = Some(title);
        doc.image = text(&v["image_url"]).filter(|u| web_url(u).is_some());
    }
    doc.description = shown
        .and_then(|p| text(&p["cooked"]))
        .map(|c| excerpt(&c))
        .filter(|c| !c.is_empty());
    for (key, value) in [
        ("author", shown.and_then(|p| text(&p["username"]))),
        ("published", shown.and_then(|p| text(&p["created_at"]))),
        (
            "post_count",
            v["posts_count"]
                .as_u64()
                .map(|n| n.to_string())
                .filter(|_| post.is_none()),
        ),
        ("tags", list(&tags).filter(|_| post.is_none())),
        ("thread", post.map(|_| root(host, &slug, id, None))),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    doc.source_root = Some(root(host, &slug, id, post));
    Ok(Some(doc))
}

/// A post by its own id (`/posts/{id}.json`): named after its author; the link becomes an alias
/// of the post's address in its topic.
pub fn post_card(
    mut doc: ResourceMeta,
    host: &str,
    id: u64,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let Some(v) = json(status, body)? else {
        return Ok(None);
    };
    let (Some(topic), Some(n), Some(slug)) = (
        v["topic_id"].as_u64(),
        v["post_number"].as_u64(),
        text(&v["topic_slug"]),
    ) else {
        return Err(Declined.into());
    };
    anyhow::ensure!(
        v["id"].as_u64() == Some(id),
        "the forum answered for another post"
    );
    doc.set_type(if n > 1 {
        "Comment"
    } else {
        "DiscussionForumPosting"
    });
    let by = text(&v["name"]).or_else(|| text(&v["username"]));
    doc.name = by.map(|by| format!("Post by {by}"));
    doc.description = text(&v["cooked"]).map(|c| excerpt(&c));
    for (key, value) in [
        ("author", text(&v["username"])),
        ("published", text(&v["created_at"])),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    doc.source_root = Some(root(host, &slug, topic, Some(n)));
    Ok(Some(doc))
}

#[async_trait]
impl Source for Discourse {
    fn name(&self) -> &'static str {
        "discourse"
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some_and(|t| FORUMS.get(t.host()) != Some(false))
    }
    fn facts(&self) -> &'static [&'static str] {
        &["author", "published", "post_count", "tags", "thread"]
    }
    fn social(&self, _doc: &ResourceMeta) -> bool {
        true
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304",
                card_type: "DiscussionForumPosting",
            },
            Example {
                uri: "https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304/2",
                card_type: "Comment",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a forum link")?;
        let host = target.host().to_owned();
        if FORUMS.get(&host) == Some(false) {
            return Err(Declined.into());
        }
        let f = cx
            .fetch(
                &target.api_url(),
                &FetchOptions {
                    accept: Some("application/json".into()),
                    ..Default::default()
                },
            )
            .await?;
        let result = match &target {
            Target::Topic { id, post, .. } => {
                topic_card(cx.card(uri, self), &host, *id, *post, f.status, &f.body)
            }
            Target::Post { id, .. } => post_card(cx.card(uri, self), &host, *id, f.status, &f.body),
        };
        match &result {
            Err(e) if e.is::<Declined>() => FORUMS.set(&host, false),
            Ok(_) => FORUMS.set(&host, true),
            _ => {}
        }
        let mut doc = result?.unwrap_or_else(|| cx.missing(uri, self));
        doc.site = host;
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("discourse", file)
    }
    const HOST: &str = "delvingbitcoin.org";
    fn base() -> ResourceMeta {
        ResourceMeta::new("https://delvingbitcoin.org/t/304", "discourse", 3600)
    }

    #[test]
    fn addresses() {
        let topic = |post| Target::Topic {
            host: HOST.into(),
            id: 304,
            post,
        };
        for (uri, target) in [
            (
                "https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304",
                topic(None),
            ),
            (
                "https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304?u=someone",
                topic(None),
            ),
            ("https://delvingbitcoin.org/t/304", topic(None)),
            (
                "https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304/1",
                topic(None),
            ),
            (
                "https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304/2",
                topic(Some(2)),
            ),
            ("https://delvingbitcoin.org/t/304/2", topic(Some(2))),
            (
                "https://delvingbitcoin.org/p/790",
                Target::Post {
                    host: HOST.into(),
                    id: 790,
                },
            ),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            "https://delvingbitcoin.org/c/protocol-design/8",
            "https://delvingbitcoin.org/t/",
            "https://delvingbitcoin.org/t/slug/x",
            "http://delvingbitcoin.org/t/304",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn topics_and_posts() {
        let doc = topic_card(base(), HOST, 304, None, 200, &read("topic-304.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "DiscussionForumPosting");
        assert_eq!(doc.name.as_deref(), Some("Unspendable keys in descriptors"));
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .starts_with("With miniscript on taproot"));
        assert_eq!(doc.attributes["author"], "salvatoshi");
        assert!(doc.attributes["tags"].contains("taproot"));
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304")
        );

        let doc = topic_card(base(), HOST, 304, Some(2), 200, &read("topic-304-2.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "Comment");
        assert_eq!(
            doc.name.as_deref(),
            Some("Comment on “Unspendable keys in descriptors”")
        );
        assert_eq!(doc.attributes["author"], "sipa");
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304/2")
        );

        let doc = post_card(base(), HOST, 790, 200, &read("post-790.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.name.as_deref(), Some("Post by Pieter Wuille"));
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304/2")
        );

        assert!(
            topic_card(base(), HOST, 999999, None, 404, &read("topic-missing.json"))
                .unwrap()
                .is_none()
        );
        let other =
            topic_card(base(), HOST, 304, None, 404, &read("not-discourse.html")).unwrap_err();
        assert!(other.is::<Declined>());
        assert!(topic_card(base(), HOST, 305, None, 200, &read("topic-304.json")).is_err());
    }
}
