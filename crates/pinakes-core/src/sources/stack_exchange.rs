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
    resolver::egress::{Egress, FetchOptions, Fetched},
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

/// The site at `host`: its canonical host and the API's name for it (as the API's `/sites`
/// answer names them, `api_site_parameter`), if it is a Stack Exchange site.
fn site(host: &str) -> Option<(String, String)> {
    let host = host.strip_prefix("www.").unwrap_or(host);
    let name = |s: &str| {
        !s.is_empty()
            && s.len() <= 40
            && s != "meta"
            && s != "api"
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if let Some(sub) = host.strip_suffix(".stackexchange.com") {
        // A site's meta is `{site}.meta` (its older host `meta.{site}` redirects there);
        // Meta Stack Exchange itself is `meta`.
        let api = match sub.split_once('.') {
            Some((s, "meta")) | Some(("meta", s)) if name(s) => format!("{s}.meta"),
            None if sub == "meta" || name(sub) => sub.to_owned(),
            _ => return None,
        };
        return Some((format!("{api}.stackexchange.com"), api));
    }
    let api = match host {
        "stackoverflow.com" | "meta.stackoverflow.com" => host.trim_end_matches(".com"),
        "pt.stackoverflow.com"
        | "ru.stackoverflow.com"
        | "es.stackoverflow.com"
        | "ja.stackoverflow.com" => host.trim_end_matches(".com"),
        "pt.meta.stackoverflow.com"
        | "ru.meta.stackoverflow.com"
        | "es.meta.stackoverflow.com"
        | "ja.meta.stackoverflow.com"
        | "agents.meta.stackoverflow.com" => host.trim_end_matches(".com"),
        "superuser.com" | "serverfault.com" | "askubuntu.com" | "stackapps.com" => {
            host.trim_end_matches(".com")
        }
        "meta.superuser.com" | "meta.serverfault.com" | "meta.askubuntu.com" => {
            host.trim_end_matches(".com")
        }
        "mathoverflow.net" | "meta.mathoverflow.net" => host,
        _ => return None,
    };
    Some((host.to_owned(), api.to_owned()))
}

/// The question of `https://{site}/questions/{id}[/{slug}]` or its short link `/q/{id}`.
pub fn of_uri(uri: &str) -> Option<Target> {
    let url = web_url(uri)?;
    let (host, site) = site(&url.host_str()?.to_ascii_lowercase())?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let id = match segments.as_slice() {
        ["questions", id, ..] | ["q", id, ..] => *id,
        _ => return None,
    };
    let id = (id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.parse::<u64>().ok())
        .flatten()
        .filter(|n| *n > 0)?;
    Some(Target { host, site, id })
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

/// The pause, in seconds, that an answer of the API asks for before the next call to its method:
/// its `backoff` (on any answer), else an hour after its quota error (502) and a minute after
/// "too many requests" (503).
fn pause(v: &Value) -> Option<u64> {
    v["backoff"]
        .as_u64()
        .or(match v["error_id"].as_u64() {
            Some(502) => Some(3600),
            Some(503) => Some(60),
            _ => None,
        })
        .filter(|s| *s > 0)
        .map(|s| s.min(86400))
}

/// The API's pause: it holds for the method (every question on every site), so no call is made
/// before it ends. One for the process, kept by both halves of the source. Calls take turns, so
/// each is asked only after the answer before it, and its pause, is in.
pub struct Pause {
    until: std::sync::Mutex<Option<tokio::time::Instant>>,
    turn: tokio::sync::Mutex<()>,
}
static PAUSE: Pause = Pause::new();
impl Pause {
    pub const fn new() -> Self {
        Self {
            until: std::sync::Mutex::new(None),
            turn: tokio::sync::Mutex::const_new(()),
        }
    }
    /// `RetryLater` while a pause lasts.
    fn wait(&self) -> Result<()> {
        let until = *self.until.lock().unwrap_or_else(|e| e.into_inner());
        match until.map(|u| u.saturating_duration_since(tokio::time::Instant::now())) {
            Some(left) if !left.is_zero() => Err(crate::resolver::RetryLater {
                class: "stack_exchange_quota",
                seconds: left.as_secs().max(1),
            }
            .into()),
            _ => Ok(()),
        }
    }
    fn hold(&self, seconds: u64) {
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
        let mut held = self.until.lock().unwrap_or_else(|e| e.into_inner());
        if held.is_none_or(|h| h < until) {
            *held = Some(until);
        }
    }
    /// Ask the API for `url` (in its turn, not before the pause ends), keeping the pause its
    /// answer asks for: its `backoff` or error, else a 429's or 503's `Retry-After` (a minute at
    /// least). Then rate limits and server errors are `RetryLater`, as for any source.
    async fn ask(&self, egress: &Egress, url: &str) -> Result<Fetched> {
        let _turn = self.turn.lock().await;
        self.wait()?;
        let options = FetchOptions {
            accept: Some("application/json".into()),
            ..Default::default()
        };
        let f = match egress.fetch_with(url, &options).await {
            Ok(f) => f,
            Err(error) => {
                // A 429 comes as egress's own `RetryLater`, with the answer's wait.
                if let Some(busy) = error
                    .downcast_ref::<crate::resolver::RetryLater>()
                    .filter(|r| r.class == "http_4xx")
                {
                    self.hold(busy.seconds.max(60));
                }
                return Err(error);
            }
        };
        let busy = (f.status == 503).then(|| f.retry_after_secs.max(60));
        if let Some(seconds) = serde_json::from_slice::<Value>(&f.body)
            .ok()
            .and_then(|v| pause(&v))
            .or(busy)
        {
            self.hold(seconds);
        }
        super::retry(&f)?;
        Ok(f)
    }
}
impl Default for Pause {
    fn default() -> Self {
        Self::new()
    }
}

/// The error an answer of the API is, if it is one. Only the API's own trouble passes (500, its
/// 502 quota, its 503 too many requests): ask again after its pause. Any other error
/// (`bad_parameter` for a site there is not, `no_method`, …) is the request's: a failure, never
/// asked again sooner.
fn refusal(v: &Value) -> Option<anyhow::Error> {
    let error = v["error_id"].as_u64()?;
    Some(if matches!(error, 500 | 502 | 503) {
        crate::resolver::RetryLater {
            class: "stack_exchange_quota",
            seconds: pause(v).unwrap_or(60),
        }
        .into()
    } else {
        anyhow::Error::new(crate::resolver::ErrorClass("http_4xx")).context(format!(
            "Stack Exchange refused the request (error {error})"
        ))
    })
}

/// The ids of the questions a question repeats (closed as their duplicate).
fn originals(q: &Value) -> Vec<u64> {
    q["closed_details"]["original_questions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["question_id"].as_u64())
        .take(8)
        .collect()
}

/// The card of a question, or `None` when the site has no such question (or it was deleted).
pub fn card(
    mut doc: ResourceMeta,
    target: &Target,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let v: Value = serde_json::from_slice(body).context("not the API's answer")?;
    if let Some(error) = refusal(&v) {
        return Err(error);
    }
    anyhow::ensure!(status == 200, "Stack Exchange answered {status}");
    let Some(q) = v["items"].as_array().and_then(|items| items.first()) else {
        return Ok(None);
    };
    anyhow::ensure!(
        q["question_id"].as_u64() == Some(target.id),
        "Stack Exchange answered for another question"
    );
    let originals: Vec<String> = originals(q)
        .into_iter()
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
        resolve(&PAUSE, cx, uri).await
    }
}

/// The card of the question at `uri`, asked within `pause`.
pub async fn resolve(pause: &Pause, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
    let target = of_uri(uri).context("not a Stack Exchange question")?;
    let f = pause.ask(cx.egress, &target.api()).await?;
    let doc = cx.card(uri, &StackExchange);
    Ok(
        crate::resolver::html::blocking(move || card(doc, &target, f.status, &f.body))
            .await??
            .unwrap_or_else(|| cx.missing(uri, &StackExchange)),
    )
}

pub mod also_on {
    //! The `stack_exchange` also-on source: a question's own key, and its original's.
    use super::{of_uri, originals, refusal, Target, PAUSE};
    use crate::also_on::{Context, Found, Key, Source, Subject};
    use anyhow::{Context as _, Result};
    use async_trait::async_trait;
    use serde_json::Value;

    /// A question's id on its site as the key `stackexchange:{site}/{id}`; a question closed as
    /// a duplicate carries its original's key instead, so the two are one group, and a chain of
    /// duplicates (A of B, B of C) carries the key of its end (C), so all are one group. One
    /// closed as a duplicate of several carries several, and joins none (a group holds one
    /// question of this namespace).
    pub struct StackExchange;

    /// How many questions past a duplicate's original a chain is followed.
    const HOPS: usize = 3;

    /// The question a chain of duplicates from `original` ends at on `target`'s site: each
    /// question closed as a duplicate of exactly one other, asked of the API (within its pause),
    /// at most `HOPS` of them. A loop, or a question repeating several, ends it.
    async fn end(cx: &Context, target: &Target, original: u64) -> Result<u64> {
        let mut seen = vec![target.id, original];
        for _ in 0..HOPS {
            let at = Target {
                id: *seen.last().unwrap(),
                ..target.clone()
            };
            let f = PAUSE.ask(&cx.egress, &at.api()).await?;
            let v: Value = serde_json::from_slice(&f.body).context("not the API's answer")?;
            if let Some(error) = refusal(&v) {
                return Err(error);
            }
            anyhow::ensure!(f.status == 200, "Stack Exchange answered {}", f.status);
            let next = match v["items"].as_array().and_then(|items| items.first()) {
                Some(q) if q["question_id"].as_u64() == Some(at.id) => originals(q),
                _ => vec![],
            };
            match next.as_slice() {
                [next] if !seen.contains(next) => seen.push(*next),
                _ => break,
            }
        }
        Ok(*seen.last().unwrap())
    }

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
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            &[("stackexchange", "stackoverflow/11227809")]
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
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
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
            let ids = match originals.as_slice() {
                [] => vec![target.id],
                [original] => vec![end(cx, &target, *original).await?],
                several => several.to_vec(),
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
            // A meta's canonical host, its older spelling, Meta Stack Exchange itself, and the
            // metas of Stack Overflow, as the API's `/sites` names them.
            (
                "https://bitcoin.meta.stackexchange.com/questions/7",
                "bitcoin.meta",
                "https://bitcoin.meta.stackexchange.com/questions/7",
            ),
            (
                "https://meta.bitcoin.stackexchange.com/questions/7",
                "bitcoin.meta",
                "https://bitcoin.meta.stackexchange.com/questions/7",
            ),
            (
                "https://meta.stackexchange.com/questions/8",
                "meta",
                "https://meta.stackexchange.com/questions/8",
            ),
            (
                "https://meta.stackoverflow.com/questions/9",
                "meta.stackoverflow",
                "https://meta.stackoverflow.com/questions/9",
            ),
            (
                "https://pt.meta.stackoverflow.com/questions/10",
                "pt.meta.stackoverflow",
                "https://pt.meta.stackoverflow.com/questions/10",
            ),
            (
                "https://meta.superuser.com/questions/11",
                "meta.superuser",
                "https://meta.superuser.com/questions/11",
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
        assert_eq!(
            StackExchange
                .alias_root("https://meta.bitcoin.stackexchange.com/questions/7")
                .as_deref(),
            Some("https://bitcoin.meta.stackexchange.com/questions/7")
        );
        for uri in [
            "https://stackoverflow.com/questions/tagged/rust",
            "https://stackoverflow.com/a/11227902",
            "https://stackoverflow.com/users/87234",
            "https://api.stackexchange.com/questions/1",
            "https://a.b.c.stackexchange.com/questions/1",
            "https://meta.meta.stackexchange.com/questions/1",
            "https://bitcoin.other.stackexchange.com/questions/1",
            "https://stackoverflow.com.example/questions/1",
        ] {
            assert!(!StackExchange.reads(uri), "{uri}");
        }
    }

    #[test]
    fn every_site_the_api_lists() {
        // The API's `/sites` (recorded): each site's address gives its API name.
        let sites: Value = serde_json::from_slice(&read("se-sites.json")).unwrap();
        let sites = sites["items"].as_array().unwrap();
        assert!(sites.len() > 300);
        for site in sites {
            let uri = format!("{}/questions/1", site["site_url"].as_str().unwrap());
            assert_eq!(
                of_uri(&uri).map(|t| t.site),
                site["api_site_parameter"].as_str().map(str::to_owned),
                "{uri}"
            );
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
        assert_eq!(
            error
                .downcast_ref::<crate::resolver::RetryLater>()
                .map(|r| r.seconds),
            Some(3600)
        );
        // A site there is not is the request's error: a failure, not asked again sooner.
        let error = card(base(&t), &t, 400, &read("se-no-such-site.json")).unwrap_err();
        assert!(error
            .downcast_ref::<crate::resolver::RetryLater>()
            .is_none());
        assert_eq!(crate::resolver::error_class(&error), "http_4xx");
    }
}

#[cfg(test)]
mod pause_tests {
    use super::*;
    use crate::sources::fixture;

    const JSON: &str = "application/json";
    fn api(id: u64) -> String {
        format!("https://api.stackexchange.com/2.3/questions/{id}?site=bitcoin&filter={FILTER}")
    }
    fn uri(id: u64) -> String {
        format!("https://bitcoin.stackexchange.com/questions/{id}")
    }
    /// The recorded question 336 as question `id`, with the API's `backoff` if given.
    fn answer(id: u64, backoff: Option<u64>) -> Vec<u8> {
        let mut v: Value =
            serde_json::from_slice(&fixture::read("stack_exchange", "se-bitcoin-336.json"))
                .unwrap();
        v["items"][0]["question_id"] = id.into();
        if let Some(seconds) = backoff {
            v["backoff"] = seconds.into();
        }
        serde_json::to_vec(&v).unwrap()
    }
    fn retry(error: anyhow::Error) -> Option<u64> {
        error
            .downcast_ref::<crate::resolver::RetryLater>()
            .map(|r| r.seconds)
    }

    #[tokio::test(start_paused = true)]
    async fn a_backoff_holds_every_question() {
        // A 200 asking for 120 s: the next question, another one, is not asked before.
        let (egress, recorded) = fixture::egress(vec![
            (&api(336), 200, JSON, answer(336, Some(120))),
            (&api(1), 200, JSON, answer(1, None)),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let pause = Pause::new();
        let doc = resolve(&pause, &cx, &uri(336)).await.unwrap();
        assert_eq!(doc.type_name, "Question");
        let error = resolve(&pause, &cx, &uri(1)).await.unwrap_err();
        assert_eq!(retry(error), Some(120));
        assert_eq!(recorded.asked().len(), 1);
        tokio::time::advance(std::time::Duration::from_secs(121)).await;
        assert!(resolve(&pause, &cx, &uri(1)).await.is_ok());
        assert_eq!(recorded.asked().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_errors_backoff_holds_every_question() {
        let busy = br#"{"error_id":503,"error_message":"too many requests","error_name":"temporarily_unavailable","backoff":30}"#;
        let (egress, recorded) = fixture::egress(vec![
            (&api(336), 400, JSON, busy.to_vec()),
            (&api(1), 200, JSON, answer(1, None)),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let pause = Pause::new();
        let error = resolve(&pause, &cx, &uri(336)).await.unwrap_err();
        assert_eq!(retry(error), Some(30));
        let error = resolve(&pause, &cx, &uri(1)).await.unwrap_err();
        assert!(retry(error).is_some_and(|s| s <= 30));
        assert_eq!(recorded.asked().len(), 1);
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(resolve(&pause, &cx, &uri(1)).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_or_503_holds_every_question() {
        // An HTTP 503 with the API's backoff, and a bare 429 (its Retry-After, a minute at
        // least): both are retried later, and the next question is not asked before.
        let busy = br#"{"error_id":503,"error_message":"too many requests","error_name":"temporarily_unavailable","backoff":30}"#;
        for (status, body, seconds) in [(503, busy.to_vec(), 30), (429, vec![], 60)] {
            let (egress, recorded) = fixture::egress(vec![
                (&api(336), status, JSON, body),
                (&api(1), 200, JSON, answer(1, None)),
            ]);
            let cx = Context {
                egress: &egress,
                ttl: 3600,
            };
            let pause = Pause::new();
            let error = resolve(&pause, &cx, &uri(336)).await.unwrap_err();
            assert!(retry(error).is_some(), "{status}");
            let error = resolve(&pause, &cx, &uri(1))
                .await
                .expect_err(&format!("{status}"));
            assert!(retry(error).is_some_and(|s| s <= seconds), "{status}");
            assert_eq!(recorded.asked().len(), 1, "{status}");
            tokio::time::advance(std::time::Duration::from_secs(seconds + 1)).await;
            assert!(resolve(&pause, &cx, &uri(1)).await.is_ok(), "{status}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_questions_take_turns() {
        // Two questions at once: the second is asked only after the first's answer, whose
        // backoff then holds it.
        let (egress, recorded) = fixture::egress(vec![
            (&api(336), 200, JSON, answer(336, Some(120))),
            (&api(1), 200, JSON, answer(1, None)),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let pause = Pause::new();
        let (a, b) = (uri(336), uri(1));
        let (first, second) = tokio::join!(resolve(&pause, &cx, &a), resolve(&pause, &cx, &b));
        assert_eq!(first.unwrap().type_name, "Question");
        assert!(retry(second.unwrap_err()).is_some_and(|s| s > 100));
        assert_eq!(recorded.asked(), vec![api(336)]);
    }
}
