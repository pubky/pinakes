//! GitHub repositories, read from its REST API (`api.github.com/repos/{owner}/{repo}`) with the
//! operator's token (`[sources.github] token`, or `PINAKES_GITHUB_TOKEN`). Without a token the
//! source is off and repository links get ordinary web cards. The token goes only to
//! `api.github.com`, never through a redirect, and never appears in logs, errors or cards. A token
//! GitHub refuses (401, or a 403 saying "Bad credentials") turns the source off for an hour
//! (`Refusals`): the links get web cards meanwhile, and
//! `pinakes_source_token_refusals_total{source="github"}` counts it.
use super::{list, text, web_url, Budget, Context, Declined, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Github;

const API: &str = "https://api.github.com";

use super::kit::GITHUB_RESERVED as RESERVED;

/// GitHub's refusals of the configured token.
static REFUSALS: super::Refusals = super::Refusals::new("github");

fn owner_name(value: &str) -> bool {
    (1..=39).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !value.starts_with('-')
}
fn repo_name(value: &str) -> bool {
    (1..=100).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !matches!(value, "." | "..")
}

/// The owner and repository a repository page names (`github.com/{owner}/{repo}`, `.git` and a
/// trailing slash allowed).
pub fn repository(uri: &str) -> Option<(String, String)> {
    let url = super::kit::web_url(uri)?;
    if !matches!(url.host_str()?, "github.com" | "www.github.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let [owner, repo] = segments.as_slice() else {
        return None;
    };
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (owner_name(owner)
        && repo_name(repo)
        && !RESERVED.contains(&owner.to_ascii_lowercase().as_str()))
    .then(|| (owner.to_string(), repo.to_owned()))
}

/// What GitHub answered for one repository.
#[derive(Debug)]
pub enum Answer {
    Card(Box<ResourceMeta>),
    /// A repository the token reads but the public cannot: no public object.
    Missing,
    /// The token was refused (401, or a 403 saying "Bad credentials").
    Refused,
    /// Moved or renamed (a redirect, not followed with the token): its web card follows it.
    Moved,
}

/// The API request for a repository with `token`.
fn options(token: &str) -> FetchOptions {
    FetchOptions {
        accept: Some("application/vnd.github+json".into()),
        headers: vec![
            ("authorization", format!("Bearer {token}")),
            ("x-github-api-version", "2022-11-28".into()),
        ],
        // Errors and cache keys never carry the header; a redirect is answered, not followed.
        sensitive: true,
        keep_redirect: true,
        // Its rate limit answers 403 (`read`): a wait, which a confirmation replays.
        waits: &[403],
        // An error answer keeps its JSON body: a 403's `message` says whether it is the rate
        // limit. A 429 comes as it is, for `read` to class.
        error_json: true,
        // A refusal belongs to the token sent, not to the URL: never replayed from the cache.
        uncached: &[401],
        ..Default::default()
    }
}

/// Whether a 403 is GitHub's rate limit: its JSON `message` names one ("API rate limit
/// exceeded for …", "You have exceeded a secondary rate limit. …", "… an abuse detection
/// mechanism …"). Any other 403, and one with no message to read, is no wait.
fn rate_limit(body: &[u8]) -> bool {
    message_names(body, &["rate limit", "abuse detection"])
}

/// Whether an error answer's JSON `message` says the token itself is bad ("Bad credentials").
fn bad_credentials(body: &[u8]) -> bool {
    message_names(body, &["bad credentials"])
}

fn message_names(body: &[u8], words: &[&str]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| text(&v["message"]))
        .is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            words.iter().any(|w| message.contains(w))
        })
}

/// A repository's card from `GET /repos/{owner}/{repo}` with `token`. A path GitHub has no
/// repository for (404) is declined: it may be another page of the site (an advisory, a
/// two-part site page), so the web route reads it and says whether it is gone. A 403 that is
/// not the rate limit (access to the repository is blocked) is declined too. The rate limit is
/// a wait of class `http_4xx`, a timeout or a server error one of class `upstream`.
pub async fn read(
    cx: &Context<'_>,
    doc: ResourceMeta,
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<Answer> {
    let options = options(token);
    let url = format!("{API}/repos/{owner}/{repo}");
    let f = cx.egress.discover(&url, &options, 3600).await?;
    let wait = |class: &'static str| -> Result<Answer> {
        Err(crate::resolver::RetryLater {
            class,
            seconds: f.retry_after_secs.clamp(60, 3600),
        }
        .into())
    };
    match f.status {
        200 => match card(doc, &f.body)? {
            Some(card) => Ok(Answer::Card(Box::new(card))),
            // A repository the token can read but the public can't is no public object: no
            // card, no key, and its record leaves the discovery cache now instead of resting
            // there for the hour.
            None => {
                cx.egress.forget_discovery(&url, &options).await?;
                Ok(Answer::Missing)
            }
        },
        301 | 302 | 307 | 308 => Ok(Answer::Moved),
        401 => Ok(Answer::Refused),
        404 => Err(Declined.into()),
        // GitHub's rate limit answers 429, or 403 with a message that names it.
        429 => wait("http_4xx"),
        403 if rate_limit(&f.body) => wait("http_4xx"),
        403 if bad_credentials(&f.body) => Ok(Answer::Refused),
        403 => Err(Declined.into()),
        408 | 500..=599 => wait("upstream"),
        status => anyhow::bail!("GitHub answered {status}"),
    }
}

/// Whether a repository record is public: not `private`, and its `visibility` (when GitHub
/// states it) `public`. Anything else (`private`, `internal`, an unknown value) is not public.
pub fn public(v: &Value) -> bool {
    v["private"] == false
        && v["visibility"]
            .as_str()
            .is_none_or(|visibility| visibility == "public")
}

/// A repository's card from its API record, or `None` for a repository that is not public.
pub fn card(mut doc: ResourceMeta, body: &[u8]) -> Result<Option<ResourceMeta>> {
    let v: Value = serde_json::from_slice(body).context("GitHub answered no JSON")?;
    if !public(&v) {
        return Ok(None);
    }
    let name = text(&v["full_name"]).context("no repository name")?;
    let page = text(&v["html_url"])
        .filter(|u| web_url(u).is_some_and(|u| u.host_str() == Some("github.com")))
        .context("no repository page")?;
    doc.set_type("SoftwareSourceCode");
    doc.name = Some(name);
    doc.site = "GitHub".into();
    doc.description = text(&v["description"]).map(|d| super::without_addresses(&d));
    doc.image = text(&v["owner"]["avatar_url"]).filter(|u| web_url(u).is_some());
    let topics: Vec<String> = v["topics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(text)
        .collect();
    let count = |key: &str| v[key].as_u64().map(|n| n.to_string());
    for (key, value) in [
        ("stars", count("stargazers_count")),
        ("forks", count("forks_count")),
        ("topics", list(&topics)),
        ("language", text(&v["language"])),
        (
            "license",
            text(&v["license"]["spdx_id"]).filter(|l| l != "NOASSERTION"),
        ),
        ("default_branch", text(&v["default_branch"])),
        ("updated", text(&v["pushed_at"])),
        ("archived", (v["archived"] == true).then(|| "true".into())),
        ("fork_of", text(&v["parent"]["full_name"])),
        ("page_url", Some(page.clone())),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    doc.source_root = Some(page);
    Ok(Some(doc))
}

#[async_trait]
impl Source for Github {
    fn name(&self) -> &'static str {
        "github"
    }
    fn reads(&self, uri: &str) -> bool {
        repository(uri).is_some()
    }
    fn takes_token(&self) -> bool {
        true
    }
    fn token_variable(&self) -> Option<&'static str> {
        Some("PINAKES_GITHUB_TOKEN")
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "stars",
            "forks",
            "topics",
            "language",
            "license",
            "default_branch",
            "updated",
            "archived",
            "fork_of",
            "page_url",
        ]
    }
    fn budgets(&self) -> &'static [Budget] {
        // 5,000 requests an hour with a token; the source never asks without one.
        &[("api.github.com", 5000.0 / 3600.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://github.com/BurntSushi/ripgrep",
            card_type: "SoftwareSourceCode",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let (owner, repo) = repository(uri).context("not a GitHub repository")?;
        let Some(token) = super::token(self.name()) else {
            return Err(Declined.into());
        };
        resolve_with(cx, uri, &owner, &repo, &token, &REFUSALS).await
    }
}

/// The card of repository `owner/repo` at `uri` with `token`, `Declined` while `refusals` holds
/// the token off.
async fn resolve_with(
    cx: &Context<'_>,
    uri: &str,
    owner: &str,
    repo: &str,
    token: &str,
    refusals: &super::Refusals,
) -> Result<ResourceMeta> {
    refusals.check()?;
    match read(cx, cx.card(uri, &Github), owner, repo, token).await? {
        Answer::Card(doc) => Ok(*doc),
        Answer::Missing => Ok(cx.missing(uri, &Github)),
        Answer::Moved => Err(Declined.into()),
        Answer::Refused => {
            if refusals.refused() {
                tracing::warn!(
                    "GitHub refused the configured token; GitHub links get web cards for an hour"
                );
            }
            Err(Declined.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    #[test]
    fn repository_links() {
        for (uri, want) in [
            (
                "https://github.com/BurntSushi/ripgrep",
                Some(("BurntSushi", "ripgrep")),
            ),
            (
                "https://github.com/BurntSushi/ripgrep/",
                Some(("BurntSushi", "ripgrep")),
            ),
            (
                "https://www.github.com/rust-random/rand.git",
                Some(("rust-random", "rand")),
            ),
            ("https://github.com/BurntSushi/ripgrep/issues/1", None),
            ("https://github.com/BurntSushi", None),
            ("https://github.com/orgs/rust-lang", None),
            ("https://github.com/topics/rust", None),
            ("https://github.com/advisories/GHSA-jfh8-c2jp-5v3q", None),
            ("https://github.com/Solutions/ci-cd", None),
            ("https://github.com/-x/y", None),
            ("https://gist.github.com/a/b", None),
            ("https://example.org/a/b", None),
        ] {
            let got = repository(uri);
            assert_eq!(
                got.as_ref().map(|(o, r)| (o.as_str(), r.as_str())),
                want,
                "{uri}"
            );
        }
    }

    #[test]
    fn a_card_says_what_the_record_says() {
        let doc = card(
            ResourceMeta::new("https://github.com/BurntSushi/ripgrep", "github", 3600),
            &fixture::read("github", "repo-ripgrep.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "SoftwareSourceCode");
        assert_eq!(doc.name.as_deref(), Some("BurntSushi/ripgrep"));
        assert_eq!(doc.site, "GitHub");
        assert!(doc.description.as_deref().unwrap().contains("ripgrep"));
        assert_eq!(doc.attributes["language"], "Rust");
        assert_eq!(doc.attributes["license"], "Unlicense");
        assert!(doc.attributes["stars"].parse::<u64>().unwrap() > 10_000);
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://github.com/BurntSushi/ripgrep")
        );
    }

    /// A token that can read private repositories must never make one a public card: a record
    /// marked `private`, or with a `visibility` other than `public`, gives no card at all.
    #[test]
    fn a_non_public_repository_gets_no_card() {
        let doc = || ResourceMeta::new("https://github.com/acme/secret", "github", 3600);
        let mut record: serde_json::Value =
            serde_json::from_slice(&fixture::read("github", "repo-ripgrep.json")).unwrap();
        assert!(public(&record));
        for (field, value) in [
            ("private", serde_json::json!(true)),
            ("visibility", serde_json::json!("private")),
            ("visibility", serde_json::json!("internal")),
            ("visibility", serde_json::json!("")),
        ] {
            let mut v = record.clone();
            v[field] = value.clone();
            assert!(!public(&v), "{field}={value}");
            assert!(
                card(doc(), &serde_json::to_vec(&v).unwrap())
                    .unwrap()
                    .is_none(),
                "{field}={value}"
            );
        }
        // A record without `visibility` is public only when `private` says so.
        record.as_object_mut().unwrap().remove("visibility");
        assert!(public(&record));
        record["private"] = serde_json::json!(true);
        assert!(!public(&record));
    }

    /// A transport that answers as GitHub does: the record with the right token, 401 with any
    /// other, and remembers each request's Authorization header.
    struct Api {
        seen: std::sync::Mutex<Vec<(String, Option<String>)>>,
    }
    #[async_trait]
    impl crate::resolver::egress::Transport for Api {
        async fn get(
            &self,
            url: &url::Url,
            addresses: &[std::net::SocketAddr],
        ) -> Result<crate::resolver::egress::Fetched> {
            self.get_with(url, addresses, &FetchOptions::default())
                .await
        }
        async fn get_with(
            &self,
            url: &url::Url,
            _: &[std::net::SocketAddr],
            options: &FetchOptions,
        ) -> Result<crate::resolver::egress::Fetched> {
            let authorization = options
                .headers
                .iter()
                .find(|(name, _)| *name == "authorization")
                .map(|(_, v)| v.clone());
            self.seen
                .lock()
                .unwrap()
                .push((url.to_string(), authorization.clone()));
            let (status, body, location) = match (url.path(), authorization.as_deref()) {
                (_, Some(a)) if a != "Bearer good-token" => (401, b"{}".to_vec(), None),
                ("/repos/BurntSushi/ripgrep", _) => {
                    (200, fixture::read("github", "repo-ripgrep.json"), None)
                }
                // The public record marked private: what a token that reads private repositories
                // would get.
                ("/repos/acme/secret", _) => {
                    let mut v: serde_json::Value =
                        serde_json::from_slice(&fixture::read("github", "repo-ripgrep.json"))
                            .unwrap();
                    v["private"] = serde_json::json!(true);
                    v["visibility"] = serde_json::json!("private");
                    (200, serde_json::to_vec(&v).unwrap(), None)
                }
                ("/repos/old/name", _) => (
                    301,
                    b"{}".to_vec(),
                    Some("https://elsewhere.example/repos/new/name".to_owned()),
                ),
                // GitHub's rate limit.
                ("/repos/busy/repo", _) => (
                    403,
                    br#"{"message":"API rate limit exceeded"}"#.to_vec(),
                    None,
                ),
                // Its secondary rate limit.
                ("/repos/slow/repo", _) => (
                    403,
                    br#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}"#.to_vec(),
                    None,
                ),
                // A 403 that is not a rate limit.
                ("/repos/blocked/repo", _) => (
                    403,
                    br#"{"message":"Repository access blocked"}"#.to_vec(),
                    None,
                ),
                ("/repos/down/repo", _) => (503, b"{}".to_vec(), None),
                // A 403 with no message to read.
                ("/repos/mute/repo", _) => (403, b"{}".to_vec(), None),
                // A 403 that says the token itself is bad.
                ("/repos/bad/credentials", _) => (
                    403,
                    br#"{"message":"Bad credentials","documentation_url":"https://docs.github.com/rest"}"#.to_vec(),
                    None,
                ),
                _ => (404, fixture::read("github", "repo-missing.json"), None),
            };
            Ok(crate::resolver::egress::Fetched {
                location,
                ..crate::resolver::egress::Fetched::answer(
                    url.clone(),
                    status,
                    "application/json".into(),
                    body,
                )
            })
        }
    }

    /// A private repository's record gives no card and leaves the discovery cache at once: the
    /// next read asks GitHub again, where a public record is answered from the cache.
    #[tokio::test(start_paused = true)]
    async fn a_private_record_is_not_kept_in_the_discovery_cache() {
        let api = std::sync::Arc::new(Api {
            seen: Default::default(),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(fixture::PublicDns),
            api.clone(),
        );
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = |uri: &str| cx.card(uri, &Github);
        let asked = |path: &str| {
            api.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(url, _)| url.ends_with(path))
                .count()
        };
        for _ in 0..2 {
            let answer = read(
                &cx,
                doc("https://github.com/acme/secret"),
                "acme",
                "secret",
                "good-token",
            )
            .await
            .unwrap();
            assert!(matches!(answer, Answer::Missing));
            read(
                &cx,
                doc("https://github.com/BurntSushi/ripgrep"),
                "BurntSushi",
                "ripgrep",
                "good-token",
            )
            .await
            .unwrap();
        }
        assert_eq!(asked("/repos/acme/secret"), 2);
        assert_eq!(asked("/repos/BurntSushi/ripgrep"), 1);
        let options = options("good-token");
        assert!(!egress
            .discovered(&format!("{API}/repos/acme/secret"), &options)
            .await
            .unwrap());
        assert!(egress
            .discovered(&format!("{API}/repos/BurntSushi/ripgrep"), &options)
            .await
            .unwrap());
    }

    /// A confirming fetch asks GitHub again for a cached 404, but replays a
    /// cached 403: GitHub's rate limit, a wait (`FetchOptions::waits`) it must not cut short.
    #[tokio::test(start_paused = true)]
    async fn a_confirmation_keeps_github_s_rate_limit() {
        use crate::resolver::egress::confirming;
        let api = std::sync::Arc::new(Api {
            seen: Default::default(),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(fixture::PublicDns),
            api.clone(),
        );
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let asked = |path: &str| {
            api.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(url, _)| url.ends_with(path))
                .count()
        };
        for (owner, repo) in [("busy", "repo"), ("a", "missing")] {
            let doc = || cx.card(&format!("https://github.com/{owner}/{repo}"), &Github);
            let first = read(&cx, doc(), owner, repo, "good-token").await;
            let again = confirming(read(&cx, doc(), owner, repo, "good-token")).await;
            if owner == "busy" {
                for answer in [first, again] {
                    assert!(answer
                        .err()
                        .and_then(|e| e.downcast::<crate::resolver::RetryLater>().ok())
                        .is_some());
                }
            } else {
                assert!(first.unwrap_err().is::<Declined>());
                assert!(again.unwrap_err().is::<Declined>());
            }
        }
        assert_eq!(asked("/repos/busy/repo"), 1, "the rate limit was cut short");
        assert_eq!(asked("/repos/a/missing"), 2);
    }

    /// A 403 is a wait only when its message names a rate limit: any other 403 (access to the
    /// repository is blocked) is the web route's page. A server error is a wait of its own class.
    #[tokio::test(start_paused = true)]
    async fn only_a_rate_limit_403_is_a_wait() {
        let api = std::sync::Arc::new(Api {
            seen: Default::default(),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(fixture::PublicDns),
            api.clone(),
        );
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let mut answers = Vec::new();
        for owner in ["busy", "slow", "blocked", "down", "mute"] {
            let doc = cx.card(&format!("https://github.com/{owner}/repo"), &Github);
            let answer = read(&cx, doc, owner, "repo", "good-token").await;
            let error = answer.expect_err("no card");
            let class = error
                .downcast_ref::<crate::resolver::RetryLater>()
                .map(|retry| retry.class);
            answers.push((owner, class, error.is::<Declined>()));
        }
        assert_eq!(
            answers,
            [
                ("busy", Some("http_4xx"), false),
                ("slow", Some("http_4xx"), false),
                ("blocked", None, true),
                ("down", Some("upstream"), false),
                ("mute", None, true),
            ]
        );
    }

    /// The request asks for an error answer's JSON body: a 403's message says whether it is the
    /// rate limit, and the egress drops an error's JSON body unless asked.
    #[test]
    fn an_error_answer_keeps_its_json_body() {
        assert!(options("good-token").error_json);
    }

    #[tokio::test(start_paused = true)]
    async fn the_token_goes_only_to_the_api_and_a_refused_one_reads_without_it() {
        let api = std::sync::Arc::new(Api {
            seen: Default::default(),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(fixture::PublicDns),
            api.clone(),
        );
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = |uri: &str| cx.card(uri, &Github);
        let answer = read(
            &cx,
            doc("https://github.com/BurntSushi/ripgrep"),
            "BurntSushi",
            "ripgrep",
            "good-token",
        )
        .await
        .unwrap();
        assert!(
            matches!(answer, Answer::Card(ref d) if d.name.as_deref() == Some("BurntSushi/ripgrep"))
        );
        // A path GitHub has no repository for (a site page shaped like one, a deleted
        // repository) is the web route's to read, never `missing` on the API's word.
        for (owner, repo) in [("a", "missing"), ("advisories", "GHSA-jfh8-c2jp-5v3q")] {
            let answer = read(
                &cx,
                doc(&format!("https://github.com/{owner}/{repo}")),
                owner,
                repo,
                "good-token",
            )
            .await;
            assert!(
                answer.is_err_and(|e| e.is::<Declined>()),
                "{owner}/{repo} is not declined"
            );
        }
        // A renamed repository's redirect is answered, never followed with the token.
        assert!(matches!(
            read(
                &cx,
                doc("https://github.com/old/name"),
                "old",
                "name",
                "good-token"
            )
            .await
            .unwrap(),
            Answer::Moved
        ));
        // An expired token: refused, and the caller reads the page without it.
        let refused = read(
            &cx,
            doc("https://github.com/BurntSushi/ripgrep"),
            "BurntSushi",
            "ripgrep",
            "expired-s3cret",
        )
        .await
        .unwrap();
        assert!(matches!(refused, Answer::Refused));
        assert!(!format!("{refused:?}").contains("s3cret"));
        let seen = api.seen.lock().unwrap().clone();
        assert!(seen
            .iter()
            .all(|(url, auth)| url.starts_with("https://api.github.com/") && auth.is_some()));
        assert_eq!(seen.len(), 5, "{seen:?}");
    }

    /// Without a token the source is off: its links get ordinary web cards.
    #[test]
    fn the_source_is_on_only_with_a_token() {
        use crate::sources::{SourcesConfig, Switch};
        let mut config = crate::config::Config::default();
        let uri = "https://github.com/BurntSushi/ripgrep";
        assert!(!config.sources.enabled("github"));
        assert_eq!(crate::resolver::classify_config(uri, &config), Some("web"));
        config.sources = SourcesConfig(
            [(
                "github".to_owned(),
                Switch {
                    enabled: true,
                    token: Some("github_pat_x".into()),
                    key: None,
                },
            )]
            .into(),
        );
        assert!(config.sources.enabled("github"));
        assert_eq!(
            crate::resolver::classify_config(uri, &config),
            Some("official")
        );
        assert!(!format!("{:?}", config.sources).contains("github_pat_x"));
        assert!(!serde_json::to_string(&config.sources)
            .unwrap()
            .contains("github_pat_x"));
        // Only a source that takes a token may have one.
        let other = SourcesConfig(
            [(
                "gitlab".to_owned(),
                Switch {
                    enabled: true,
                    token: Some("x".into()),
                    key: None,
                },
            )]
            .into(),
        );
        assert!(other.validate().is_err());
    }

    /// A 401, or a 403 saying "Bad credentials", switches the token off for an hour: links are
    /// declined (web cards) and nothing is asked meanwhile; after the hour it is sent again.
    /// Another 403 (access to one repository blocked) declines that link alone.
    #[tokio::test(start_paused = true)]
    async fn a_refused_token_is_off_for_an_hour() {
        let api = std::sync::Arc::new(Api {
            seen: Default::default(),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(fixture::PublicDns),
            api.clone(),
        );
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let declined = |r: Result<ResourceMeta>| r.is_err_and(|e| e.is::<Declined>());
        let ripgrep = "https://github.com/BurntSushi/ripgrep";
        let refusals = crate::sources::Refusals::new("github");
        assert!(declined(
            resolve_with(
                &cx,
                "https://github.com/blocked/repo",
                "blocked",
                "repo",
                "good-token",
                &refusals
            )
            .await
        ));
        assert!(!refusals.latched());
        assert!(declined(
            resolve_with(
                &cx,
                "https://github.com/bad/credentials",
                "bad",
                "credentials",
                "good-token",
                &refusals
            )
            .await
        ));
        assert!(refusals.latched());
        let asked = api.seen.lock().unwrap().len();
        tokio::time::advance(std::time::Duration::from_secs(3599)).await;
        assert!(declined(
            resolve_with(
                &cx,
                ripgrep,
                "BurntSushi",
                "ripgrep",
                "good-token",
                &refusals
            )
            .await
        ));
        assert_eq!(api.seen.lock().unwrap().len(), asked);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        let doc = resolve_with(
            &cx,
            ripgrep,
            "BurntSushi",
            "ripgrep",
            "good-token",
            &refusals,
        )
        .await
        .unwrap();
        assert_eq!(doc.status, crate::model::Status::Ok);
        // A 401 latches too.
        let refusals = crate::sources::Refusals::new("github");
        assert!(declined(
            resolve_with(
                &cx,
                ripgrep,
                "BurntSushi",
                "ripgrep",
                "stale-token",
                &refusals
            )
            .await
        ));
        assert!(refusals.latched());
    }
}
