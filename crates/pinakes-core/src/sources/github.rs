//! GitHub repositories, read from its REST API (`api.github.com/repos/{owner}/{repo}`) with the
//! operator's token (`[sources.github] token`, or `PINAKES_GITHUB_TOKEN`). Without a token the
//! source is off and repository links get ordinary web cards. The token goes only to
//! `api.github.com`, never through a redirect, and never appears in logs, errors or cards. A token
//! GitHub refuses (401) turns the source off until the daemon restarts: the links get web cards
//! again, and `pinakes_source_token_refusals_total{source="github"}` counts it.
use super::{list, text, web_url, Budget, Context, Declined, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Github;

const API: &str = "https://api.github.com";

/// First path segments that are GitHub's own pages, never an owner.
const RESERVED: &[&str] = &[
    "about",
    "apps",
    "collections",
    "contact",
    "customer-stories",
    "enterprise",
    "events",
    "explore",
    "features",
    "login",
    "marketplace",
    "new",
    "notifications",
    "orgs",
    "organizations",
    "pricing",
    "pulls",
    "issues",
    "search",
    "security",
    "settings",
    "site",
    "sponsors",
    "topics",
    "trending",
    "users",
];

/// Whether GitHub has refused the configured token since the daemon started.
static REFUSED: AtomicBool = AtomicBool::new(false);

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
    let url = web_url(uri)?;
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
    Missing,
    /// The token was refused (401): read without it.
    Refused,
    /// Moved or renamed (a redirect, not followed with the token): its web card follows it.
    Moved,
}

/// A repository's card from `GET /repos/{owner}/{repo}` with `token`.
pub async fn read(
    cx: &Context<'_>,
    doc: ResourceMeta,
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<Answer> {
    let options = FetchOptions {
        accept: Some("application/vnd.github+json".into()),
        headers: vec![
            ("authorization", format!("Bearer {token}")),
            ("x-github-api-version", "2022-11-28".into()),
        ],
        // Errors and cache keys never carry the header; a redirect is answered, not followed.
        sensitive: true,
        keep_redirect: true,
        ..Default::default()
    };
    let f = cx
        .egress
        .discover(&format!("{API}/repos/{owner}/{repo}"), &options, 3600)
        .await?;
    match f.status {
        200 => Ok(Answer::Card(Box::new(card(doc, &f.body)?))),
        301 | 302 | 307 | 308 => Ok(Answer::Moved),
        401 => Ok(Answer::Refused),
        404 => Ok(Answer::Missing),
        // GitHub's rate limit answers 403 or 429.
        403 | 408 | 429 | 500..=599 => Err(crate::resolver::RetryLater {
            class: "rate_limited",
            seconds: f.retry_after_secs.clamp(60, 3600),
        }
        .into()),
        status => anyhow::bail!("GitHub answered {status}"),
    }
}

/// A repository's card from its API record.
pub fn card(mut doc: ResourceMeta, body: &[u8]) -> Result<ResourceMeta> {
    let v: Value = serde_json::from_slice(body).context("GitHub answered no JSON")?;
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
    Ok(doc)
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
        &[("api.github.com", 5000.0 / 3600.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://github.com/BurntSushi/ripgrep",
            card_type: "SoftwareSourceCode",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let (owner, repo) = repository(uri).context("not a GitHub repository")?;
        let Some(token) = super::token(self.name()).filter(|_| !REFUSED.load(Ordering::Relaxed))
        else {
            return Err(Declined.into());
        };
        match read(cx, cx.card(uri, self), &owner, &repo, &token).await? {
            Answer::Card(doc) => Ok(*doc),
            Answer::Missing => Ok(cx.missing(uri, self)),
            Answer::Moved => Err(Declined.into()),
            Answer::Refused => {
                super::token_refused(self.name());
                if !REFUSED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "GitHub refused the configured token; GitHub links get web cards until restart"
                    );
                }
                Err(Declined.into())
            }
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
                ("/repos/old/name", _) => (
                    301,
                    b"{}".to_vec(),
                    Some("https://elsewhere.example/repos/new/name".to_owned()),
                ),
                _ => (404, fixture::read("github", "repo-missing.json"), None),
            };
            Ok(crate::resolver::egress::Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Default::default(),
                url: url.clone(),
                content_type: "application/json".into(),
                status,
                retry_after_secs: 60,
                location,
                body,
            })
        }
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
        assert!(matches!(
            read(
                &cx,
                doc("https://github.com/a/missing"),
                "a",
                "missing",
                "good-token"
            )
            .await
            .unwrap(),
            Answer::Missing
        ));
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
        assert_eq!(seen.len(), 4, "{seen:?}");
    }

    /// Without a token the source is off: its links get ordinary web cards, as today.
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
                },
            )]
            .into(),
        );
        assert!(other.validate().is_err());
    }
}
