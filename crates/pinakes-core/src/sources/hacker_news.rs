//! Hacker News stories, comments, jobs and polls, read from the official Firebase API by item id.
//! hnsignals.com names items by the same id. No key; no published limit. A job item, and a
//! top-level comment of a monthly "Ask HN: Who is hiring?" thread, is a job posting.
use super::{text, web_url, Context, Example, Source};
use crate::{
    kinds::jobs::{self, Posting},
    model::ResourceMeta,
    resolver::egress::FetchOptions,
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct HackerNews;

fn item(value: &str) -> Option<u64> {
    super::kit::number(value, 12)
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
    super::excerpt(
        &crate::resolver::cards::plain(&html.replace("<p>", "\n")),
        200,
    )
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
        ("item_type", Some(kind.clone()).filter(|k| !k.is_empty())),
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
    if kind == "job" {
        // "9 Mothers (YC P26) Is Hiring in Austin, TX".
        let title = text(&v["title"]).unwrap_or_default();
        Posting {
            organization: title
                .split_once(" (YC ")
                .map(|(org, _)| org.trim().to_owned())
                .filter(|org| !org.is_empty()),
            posted: doc.attributes.get("published").cloned(),
            apply_url: doc.attributes.get("link").cloned(),
            ..Default::default()
        }
        .apply(&mut doc);
    }
    Ok(Some(doc))
}

/// Whether `thread` is a monthly "Ask HN: Who is hiring?" story, whose top-level comments are
/// job postings.
pub fn hiring_thread(thread: &Value) -> bool {
    thread["type"] == "story"
        && thread["by"] == "whoishiring"
        && text(&thread["title"]).is_some_and(|t| t.starts_with("Ask HN: Who is hiring?"))
}

/// A top-level comment of a hiring thread as the job posting it is, when its first line is a
/// header in the thread's convention ("Company | Role | Place | REMOTE (US) | Full Time", parts
/// separated by `|`, `—` or `–`; an ASCII `-` is part of a word such as "Full-time"): it is
/// named by that line; the employer is its first part; a part saying REMOTE makes it remote (with
/// the regions in its parentheses), and one naming an employment type gives it.
pub fn hiring_comment(doc: &mut ResourceMeta, comment: &Value) {
    let Some(html) = text(&comment["text"]) else {
        return;
    };
    let first = html.split("<p>").next().unwrap_or_default();
    let Some(header) = crate::resolver::html::scan_text(first) else {
        return;
    };
    let header = header.split_whitespace().collect::<Vec<_>>().join(" ");
    let parts: Vec<&str> = header
        .split(['|', '—', '–'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 || header.chars().count() > 300 {
        return;
    }
    // Each part, its parenthesised asides, and its alternatives ("REMOTE or ONSITE").
    let mut fragments: Vec<String> = vec![];
    for part in &parts {
        let (outside, asides) = parenthesised(part);
        for piece in std::iter::once(outside).chain(asides) {
            fragments.extend(
                piece
                    .split(" or ")
                    .flat_map(|p| p.split(" OR "))
                    .flat_map(|p| p.split('/'))
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty()),
            );
        }
    }
    // "REMOTE(Worldwide) or ONSITE": a part saying REMOTE, with the regions it names.
    let regions: Vec<Vec<String>> = parts
        .iter()
        .copied()
        .chain(fragments.iter().map(String::as_str))
        .filter_map(jobs::remote_place)
        .collect();
    let employment_types = fragments
        .iter()
        .filter_map(|f| jobs::employment_type(f))
        .filter(|t| {
            matches!(
                t.as_str(),
                "FULL_TIME" | "PART_TIME" | "CONTRACTOR" | "INTERN" | "TEMPORARY"
            )
        })
        .collect();
    let organization = Some(parenthesised(parts[0]).0)
        .map(|o| o.trim().to_owned())
        .filter(|o| !o.is_empty());
    doc.name = None;
    Posting {
        title: Some(header.chars().take(200).collect()),
        organization,
        remote: !regions.is_empty(),
        regions: regions.into_iter().flatten().collect(),
        employment_types,
        posted: doc.attributes.get("published").cloned(),
        apply_url: doc.attributes.get("page_url").cloned(),
        ..Default::default()
    }
    .apply(doc);
}
/// `text` without its parenthesised asides, and the asides.
fn parenthesised(text: &str) -> (String, Vec<String>) {
    let (mut outside, mut asides, mut depth, mut aside) =
        (String::new(), vec![], 0usize, String::new());
    for c in text.chars() {
        match c {
            '(' => {
                depth += 1;
                if depth > 1 {
                    aside.push(c);
                }
            }
            ')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    asides.push(std::mem::take(&mut aside).trim().to_owned());
                } else {
                    aside.push(c);
                }
            }
            c if depth > 0 => aside.push(c),
            c => outside.push(c),
        }
    }
    (
        outside.split_whitespace().collect::<Vec<_>>().join(" "),
        asides.into_iter().filter(|a| !a.is_empty()).collect(),
    )
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
            // A job item's and a hiring thread's posting (`kinds::jobs`).
            "hiring_organization",
            "employment_type",
            "remote",
            "remote_regions",
            "date_posted",
            "apply_url",
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
            Example {
                uri: "https://news.ycombinator.com/item?id=49926618",
                card_type: "JobPosting",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let id = of_uri(uri).context("not a Hacker News item")?;
        let f = cx
            .fetch(
                &format!("https://hacker-news.firebaseio.com/v0/item/{id}.json"),
                &FetchOptions::json(),
            )
            .await?;
        let doc = cx.card(uri, self);
        let body = f.body.clone();
        let Some(mut doc) =
            crate::resolver::html::blocking(move || card(doc, id, f.status, &body)).await??
        else {
            return Ok(cx.missing(uri, self));
        };
        // A comment's thread, when its parent is one: a failed read leaves it a comment.
        let item: Value = serde_json::from_slice(&f.body)?;
        if let Some(parent) = item["parent"]
            .as_u64()
            .filter(|_| item["type"] == "comment")
        {
            let thread = cx
                .fetch(
                    &format!("https://hacker-news.firebaseio.com/v0/item/{parent}.json"),
                    &FetchOptions::json(),
                )
                .await
                .ok()
                .filter(|t| t.status == 200)
                .and_then(|t| serde_json::from_slice::<Value>(&t.body).ok());
            if thread.as_ref().is_some_and(hiring_thread) {
                hiring_comment(&mut doc, &item);
            }
        }
        Ok(doc)
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

    #[test]
    fn job_items_and_hiring_threads_are_postings() {
        let doc = card(
            base(50179455),
            50179455,
            200,
            &read("hn-item-50179455.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "JobPosting");
        assert_eq!(
            doc.name.as_deref(),
            Some("9 Mothers (YC P26) Is Hiring in Austin, TX")
        );
        let a = &doc.attributes;
        assert_eq!(a["hiring_organization"], "9 Mothers");
        assert_eq!(a["apply_url"], "https://9mothers.com/careers");
        assert_eq!(
            doc.start.map(|t| t.to_rfc3339()),
            a.get("published").map(|p| {
                p.parse::<chrono::DateTime<chrono::Utc>>()
                    .unwrap()
                    .to_rfc3339()
            })
        );

        let item = |id: u64| -> Value {
            serde_json::from_slice(&read(&format!("hn-item-{id}.json"))).unwrap()
        };
        let thread = item(49922569);
        assert!(hiring_thread(&thread));
        assert!(!hiring_thread(&item(8863)));
        let comment = |id: u64| {
            let mut doc = card(base(id), id, 200, &read(&format!("hn-item-{id}.json")))
                .unwrap()
                .unwrap();
            hiring_comment(&mut doc, &item(id));
            doc
        };
        let doc = comment(49926618);
        assert_eq!(doc.type_name, "JobPosting");
        assert_eq!(
            doc.name.as_deref(),
            Some(
                "FUTO | https://futo.tech/ | Austin, TX | REMOTE(Worldwide) or ONSITE | Full Time"
            )
        );
        let a = &doc.attributes;
        assert_eq!(a["hiring_organization"], "FUTO");
        assert_eq!(a["remote"], "true");
        assert_eq!(a["remote_regions"], "Worldwide");
        assert_eq!(a["employment_type"], "FULL_TIME");
        assert_eq!(a["apply_url"], root(49926618));
        assert_eq!(a["author"], "futohq");

        let doc = comment(49922584);
        assert_eq!(doc.attributes["hiring_organization"], "PrairieLearn");
        assert_eq!(doc.attributes["remote_regions"], "US");
        assert!(!doc.attributes.contains_key("employment_type"));
        // A comment that is no header stays a comment.
        let doc = comment(49925643);
        assert_eq!(doc.type_name, "Comment");
        assert!(!doc.attributes.contains_key("hiring_organization"));
        assert!(doc.bounded(crate::sources::attributes()).is_ok());
    }
}
