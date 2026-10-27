//! Stack Exchange questions (Stack Overflow, Super User, Server Fault, Ask Ubuntu, MathOverflow,
//! every `*.stackexchange.com` site and their metas), read from the Stack Exchange API 2.3 by
//! question id. Keyless: the API allows 300 requests a day per address without a key.
//!
//! Its also-on half ([`also_on::StackExchange`]) keys a question by its id on its site, and a
//! question closed as a duplicate by its original's instead: the two join one group, a hint.
use super::{text, web_url, Context, Example, Source};
use crate::{
    kinds::questions::{self, Question},
    model::ResourceMeta,
    resolver::egress::FetchOptions,
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct StackExchange;

/// The API filter: the default fields with a question's body and its closing details.
const FILTER: &str = "!)4meWwcO75JCcyGuJACpWc-xLmMU";

/// A question on a site: the site's host and its API name, and the question's id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub site: String,
    pub id: u64,
}

/// The API's name of the site at `host`, if it is a Stack Exchange site.
fn site(host: &str) -> Option<String> {
    let host = host.strip_prefix("www.").unwrap_or(host);
    let name = |s: &str| {
        !s.is_empty()
            && s.len() <= 40
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if let Some(sub) = host.strip_suffix(".stackexchange.com") {
        return match sub.split_once('.') {
            Some(("meta", s)) if name(s) => Some(format!("meta.{s}")),
            None if name(sub) && sub != "api" && sub != "meta" => Some(sub.to_owned()),
            _ => None,
        };
    }
    match host {
        "stackoverflow.com" => Some("stackoverflow".into()),
        "meta.stackoverflow.com" => Some("meta.stackoverflow".into()),
        "pt.stackoverflow.com"
        | "ru.stackoverflow.com"
        | "es.stackoverflow.com"
        | "ja.stackoverflow.com" => Some(host.trim_end_matches(".com").to_owned()),
        "superuser.com" | "serverfault.com" | "askubuntu.com" | "stackapps.com" => {
            Some(host.trim_end_matches(".com").to_owned())
        }
        "meta.superuser.com" | "meta.serverfault.com" | "meta.askubuntu.com" => {
            Some(host.trim_end_matches(".com").to_owned())
        }
        "mathoverflow.net" => Some("mathoverflow.net".into()),
        "meta.mathoverflow.net" => Some("meta.mathoverflow.net".into()),
        _ => None,
    }
}

/// The question of `https://{site}/questions/{id}[/{slug}]` or its short link `/q/{id}`.
pub fn of_uri(uri: &str) -> Option<Target> {
    let url = web_url(uri)?;
    let host = url.host_str()?.to_ascii_lowercase();
    let site = site(&host)?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let id = match segments.as_slice() {
        ["questions", id, ..] | ["q", id, ..] => *id,
        _ => return None,
    };
    let id = (id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.parse::<u64>().ok())
        .flatten()
        .filter(|n| *n > 0)?;
    Some(Target {
        host: host.strip_prefix("www.").unwrap_or(&host).to_owned(),
        site,
        id,
    })
}
impl Target {
    pub fn root(&self) -> String {
        format!("https://{}/questions/{}", self.host, self.id)
    }
    fn api(&self) -> String {
        format!(
            "https://api.stackexchange.com/2.3/questions/{}?site={}&filter={FILTER}",
            self.id, self.site
        )
    }
    /// The key of a question on this site, `stackexchange:{site}/{id}`.
    pub fn key(&self, id: u64) -> String {
        format!("stackexchange:{}/{id}", self.site)
    }
}

/// The API's text, its HTML entities decoded (titles and names come escaped).
fn plain(v: &Value) -> Option<String> {
    text(v).map(|t| crate::resolver::web::decoded(&t))
}

/// The card of a question, or `None` when the site has no such question (or it was deleted).
pub fn card(
    mut doc: ResourceMeta,
    target: &Target,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let v: Value = serde_json::from_slice(body).context("not the API's answer")?;
    if let Some(error) = v["error_id"].as_u64() {
        // 502: the address used up its quota; 503 (or a backoff): too many requests.
        let seconds = v["backoff"]
            .as_u64()
            .unwrap_or(if error == 502 { 3600 } else { 60 });
        return Err(crate::resolver::RetryLater {
            class: "stack_exchange_quota",
            seconds: seconds.clamp(60, 86400),
        }
        .into());
    }
    anyhow::ensure!(status == 200, "Stack Exchange answered {status}");
    let Some(q) = v["items"].as_array().and_then(|items| items.first()) else {
        return Ok(None);
    };
    anyhow::ensure!(
        q["question_id"].as_u64() == Some(target.id),
        "Stack Exchange answered for another question"
    );
    let originals: Vec<String> = q["closed_details"]["original_questions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["question_id"].as_u64())
        .take(8)
        .map(|id| format!("https://{}/questions/{id}", target.host))
        .collect();
    Question {
        title: plain(&q["title"]),
        description: text(&q["body"])
            .and_then(|b| crate::resolver::pubky_apps::read::html_text(&b))
            .map(|t| questions::excerpt(&t))
            .filter(|t| !t.is_empty()),
        answer_count: q["answer_count"].as_u64(),
        accepted_answer: q["accepted_answer_id"]
            .as_u64()
            .map(|id| format!("https://{}/a/{id}", target.host)),
        score: q["score"].as_i64(),
        author: plain(&q["owner"]["display_name"]),
        published: q["creation_date"]
            .as_i64()
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        closed_reason: plain(&q["closed_reason"]),
        duplicate_of: originals,
    }
    .apply(&mut doc);
    let tags: Vec<String> = super::texts(&q["tags"]);
    if let Some(tags) = super::list(&tags) {
        doc.attributes.insert("tags".into(), tags);
    }
    // Its text is its authors' under CC BY-SA.
    doc.attribution = Some(format!(
        "{}, by its authors ({})",
        target.host,
        text(&q["content_license"]).unwrap_or_else(|| "CC BY-SA".into())
    ));
    Ok(Some(doc))
}

#[async_trait]
impl Source for StackExchange {
    fn name(&self) -> &'static str {
        "stack_exchange"
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = of_uri(uri)?.root();
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "answer_count",
            "accepted_answer",
            "score",
            "closed_reason",
            "duplicate_of",
            "author",
            "published",
            "tags",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://stackoverflow.com/questions/11227809/why-is-conditional-processing-of-a-sorted-array-faster-than-of-an-unsorted-array",
                card_type: "Question",
            },
            Example {
                uri: "https://bitcoin.stackexchange.com/questions/336",
                card_type: "Question",
            },
        ]
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "stack_exchange",
                uri: "https://bitcoin.stackexchange.com/questions/336/what-is-a-good-way-to-concisely-explain-bitcoin",
                card_type: "Question",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "api.stackexchange.com",
                path: "/2.3/questions/336",
                query: &[("site", "bitcoin")],
                header: None,
                content_type: "application/json; charset=utf-8",
                file: "se-bitcoin-336.json",
            }],
        })
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = of_uri(uri).context("not a Stack Exchange question")?;
        let f = cx
            .fetch(
                &target.api(),
                &FetchOptions {
                    accept: Some("application/json".into()),
                    ..Default::default()
                },
            )
            .await?;
        let doc = cx.card(uri, self);
        let t = target.clone();
        Ok(
            crate::resolver::html::blocking(move || card(doc, &t, f.status, &f.body))
                .await??
                .unwrap_or_else(|| cx.missing(uri, self)),
        )
    }
}

pub mod also_on {
    //! The `stack_exchange` also-on source: a question's own key, and its original's.
    use super::of_uri;
    use crate::also_on::{Context, Found, Key, Source, Subject};
    use anyhow::Result;
    use async_trait::async_trait;

    /// A question's id on its site as the key `stackexchange:{site}/{id}`; a question closed as
    /// a duplicate carries its original's key instead, so the two are one group. One closed as a
    /// duplicate of several carries several, and joins none (a group holds one question of this
    /// namespace).
    pub struct StackExchange;

    #[async_trait]
    impl Source for StackExchange {
        fn id(&self) -> &'static str {
            "stack_exchange"
        }
        fn applies(&self, subject: &Subject) -> bool {
            of_uri(&subject.uri).is_some()
                && subject
                    .doc
                    .as_ref()
                    .is_some_and(|d| d.source == "stack_exchange")
        }
        fn single_namespaces(&self) -> &'static [&'static str] {
            &["stackexchange"]
        }
        fn search_key(&self, namespace: &str, value: &str) -> Option<String> {
            let (site, id) = value
                .split_once('/')
                .filter(|_| namespace == "stackexchange")?;
            (!site.is_empty()
                && site.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'
                })
                && crate::lookups::digits(id))
            .then(|| format!("stackexchange:{value}"))
        }
        async fn lookup(&self, _cx: &Context, subject: &Subject) -> Result<Found> {
            let mut found = Found::default();
            let (Some(target), Some(doc)) = (of_uri(&subject.uri), subject.doc.as_ref()) else {
                return Ok(found);
            };
            // A duplicate stands for the question it repeats: its originals' keys, not its own.
            let originals: Vec<u64> = doc
                .attributes
                .get("duplicate_of")
                .into_iter()
                .flat_map(|d| d.split("; "))
                .filter_map(of_uri)
                .filter(|o| o.site == target.site)
                .map(|o| o.id)
                .collect();
            let ids = if originals.is_empty() {
                vec![target.id]
            } else {
                originals
            };
            for id in ids {
                found.keys.push(Key {
                    key: target.key(id),
                    via: "stack_exchange".into(),
                });
            }
            Ok(found)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("stack_exchange", file)
    }

    #[test]
    fn addresses() {
        for (uri, site, root) in [
            (
                "https://stackoverflow.com/questions/11227809/why-is-conditional",
                "stackoverflow",
                "https://stackoverflow.com/questions/11227809",
            ),
            (
                "https://stackoverflow.com/q/11227809/87234",
                "stackoverflow",
                "https://stackoverflow.com/questions/11227809",
            ),
            (
                "https://bitcoin.stackexchange.com/questions/336/what-is",
                "bitcoin",
                "https://bitcoin.stackexchange.com/questions/336",
            ),
            (
                "https://meta.bitcoin.stackexchange.com/questions/7",
                "meta.bitcoin",
                "https://meta.bitcoin.stackexchange.com/questions/7",
            ),
            (
                "https://pt.stackoverflow.com/questions/5",
                "pt.stackoverflow",
                "https://pt.stackoverflow.com/questions/5",
            ),
            (
                "https://mathoverflow.net/questions/9",
                "mathoverflow.net",
                "https://mathoverflow.net/questions/9",
            ),
            (
                "https://www.superuser.com/questions/4",
                "superuser",
                "https://superuser.com/questions/4",
            ),
        ] {
            let t = of_uri(uri).unwrap_or_else(|| panic!("{uri}"));
            assert_eq!((t.site.as_str(), t.root().as_str()), (site, root), "{uri}");
        }
        assert_eq!(
            StackExchange.alias_root("https://stackoverflow.com/questions/11227809"),
            None
        );
        for uri in [
            "https://stackoverflow.com/questions/tagged/rust",
            "https://stackoverflow.com/a/11227902",
            "https://stackoverflow.com/users/87234",
            "https://api.stackexchange.com/questions/1",
            "https://a.b.c.stackexchange.com/questions/1",
            "https://stackoverflow.com.example/questions/1",
        ] {
            assert!(!StackExchange.reads(uri), "{uri}");
        }
    }

    #[test]
    fn questions_duplicates_and_quota() {
        let so = |id| Target {
            host: "stackoverflow.com".into(),
            site: "stackoverflow".into(),
            id,
        };
        let base = |t: &Target| ResourceMeta::new(&t.root(), "stack_exchange", 3600);
        let t = so(11227809);
        let doc = card(base(&t), &t, 200, &read("se-stackoverflow-11227809.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "Question");
        assert_eq!(
            doc.name.as_deref(),
            Some(
                "Why is conditional processing of a sorted array faster than of an unsorted array?"
            )
        );
        assert!(doc.description.as_deref().unwrap().chars().count() <= 201);
        let a = &doc.attributes;
        assert_eq!(a["accepted_answer"], "https://stackoverflow.com/a/11227902");
        assert_eq!(a["answer_count"], "25");
        assert_eq!(a["score"], "27547");
        assert_eq!(a["author"], "GManNickG");
        assert_eq!(a["published"], "2012-06-27T13:51:36Z");
        assert_eq!(
            a["tags"],
            "java; c++; performance; cpu-architecture; branch-prediction"
        );
        assert!(!a.contains_key("duplicate_of"));
        assert_eq!(
            doc.attribution.as_deref(),
            Some("stackoverflow.com, by its authors (CC BY-SA 4.0)")
        );
        assert!(doc.clone().bounded(crate::sources::attributes()).is_ok());

        let t = so(663171);
        let doc = card(base(&t), &t, 200, &read("se-stackoverflow-663171.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.attributes["closed_reason"], "Duplicate");
        assert_eq!(
            doc.attributes["duplicate_of"],
            "https://stackoverflow.com/questions/509211"
        );

        let t = so(1);
        assert!(card(base(&t), &t, 200, &read("se-stackoverflow-1.json"))
            .unwrap()
            .is_none());
        assert!(card(base(&t), &t, 200, &read("se-stackoverflow-11227809.json")).is_err());
        let quota = br#"{"error_id":502,"error_message":"too many requests from this IP, more requests available in 82800 seconds","error_name":"throttle_violation"}"#;
        let error = card(base(&t), &t, 400, quota).unwrap_err();
        assert!(error
            .downcast_ref::<crate::resolver::RetryLater>()
            .is_some());
    }
}
