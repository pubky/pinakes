//! Forgejo and Gitea instances (Codeberg, gitea.com and any other): repositories, branches,
//! tags, releases and files, read from the instance's API (`/api/v1/repos/…`), never from its
//! pages (Codeberg answers deep pages with a bot challenge, which is never solved). No key;
//! Codeberg allows 2,000 requests per 10 minutes. Another host is asked once a day whether it
//! runs Forgejo or Gitea (`/api/v1/version`); one that does not gets ordinary web cards.
use super::{list, text, web_url, Budget, Context, Declined, Example, HostMemo, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::LazyLock;

pub struct Forgejo;

/// Which hosts answered as Forgejo or Gitea, and which did not.
static INSTANCES: LazyLock<HostMemo> = LazyLock::new(HostMemo::default);

/// Instances known without asking; their repository pages are read too.
const KNOWN: &[&str] = &["codeberg.org", "gitea.com"];

/// First path segments that are the instance's own pages, never an owner.
const RESERVED: &[&str] = &[
    "-",
    ".well-known",
    "admin",
    "api",
    "assets",
    "attachments",
    "avatars",
    "captcha",
    "explore",
    "issues",
    "login",
    "metrics",
    "milestones",
    "notifications",
    "org",
    "pulls",
    "repo",
    "user",
    "v2",
];

/// How many leading segments of `src/{branch|tag}/…` are tried as the ref name: a branch name
/// can contain `/`, so the split is asked of the API, shortest first.
const MAX_REF_SEGMENTS: usize = 3;

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}
/// One segment of a ref or a path, as it appears in the link (percent-encoded).
fn segment(value: &str) -> bool {
    !value.is_empty() && value.len() <= 255 && value != "." && value != ".."
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
    Commit,
}

impl RefKind {
    fn of(value: &str) -> Option<Self> {
        Some(match value {
            "branch" => Self::Branch,
            "tag" => Self::Tag,
            "commit" => Self::Commit,
            _ => return None,
        })
    }
    fn segment(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Tag => "tag",
            Self::Commit => "commit",
        }
    }
}

/// What a repository link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    Repo,
    /// `src/{branch|tag|commit}/{ref}[/{path}]`: the ref and the path, not yet split.
    At {
        kind: RefKind,
        rest: Vec<String>,
    },
    /// `releases/tag/{tag}`.
    Release {
        tag: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub object: Object,
}

impl Target {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if url.scheme() != "https" || url.port().is_some() {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let [owner, repo, rest @ ..] = segments.as_slice() else {
            return None;
        };
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if RESERVED.contains(&owner.to_ascii_lowercase().as_str()) || !name(owner) || !name(repo) {
            return None;
        }
        let object = match rest {
            [] => Object::Repo,
            ["src", kind, rest @ ..] if !rest.is_empty() && rest.len() <= 40 => {
                let kind = RefKind::of(kind)?;
                if !rest.iter().all(|s| segment(s)) || (kind == RefKind::Commit && !sha(rest[0])) {
                    return None;
                }
                Object::At {
                    kind,
                    rest: rest.iter().map(|s| (*s).to_owned()).collect(),
                }
            }
            ["releases", "tag", tag] if segment(tag) => Object::Release {
                tag: (*tag).to_owned(),
            },
            _ => return None,
        };
        Some(Self {
            host,
            owner: (*owner).to_owned(),
            repo: repo.to_owned(),
            object,
        })
    }
    /// The repository's page.
    pub fn page(&self) -> String {
        format!("https://{}/{}/{}", self.host, self.owner, self.repo)
    }
    /// The repository in the API.
    pub fn api(&self) -> String {
        format!(
            "https://{}/api/v1/repos/{}/{}",
            self.host, self.owner, self.repo
        )
    }
    /// The (ref, path) splits to try, shortest ref first; a commit's ref is its first segment.
    pub fn splits(&self) -> Vec<(String, String)> {
        let Object::At { kind, rest } = &self.object else {
            return vec![];
        };
        let most = if *kind == RefKind::Commit {
            1
        } else {
            rest.len().min(MAX_REF_SEGMENTS)
        };
        (1..=most)
            .map(|n| (rest[..n].join("/"), rest[n..].join("/")))
            .collect()
    }
    /// Whether only a Forgejo or Gitea instance has links of this shape. A bare `/{owner}/{repo}`
    /// is read on known instances only.
    fn distinctive(&self) -> bool {
        self.object != Object::Repo
    }
}

/// The API's commits touching `path` at `reference`, newest first, one only.
pub fn commits_url(target: &Target, reference: &str, path: &str) -> String {
    let mut url = url::Url::parse(&format!("{}/commits", target.api())).expect("valid base");
    let mut query = url.query_pairs_mut();
    query.append_pair("sha", &decode(reference));
    if !path.is_empty() {
        query.append_pair("path", &decode(path));
    }
    query
        .append_pair("limit", "1")
        .append_pair("stat", "false")
        .append_pair("files", "false")
        .append_pair("verification", "false");
    drop(query);
    url.into()
}
fn decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}

/// The first line of a commit or tag message.
fn headline(value: &Value) -> Option<String> {
    let line = text(value)?.lines().next()?.trim().to_owned();
    (!line.is_empty()).then(|| line.chars().take(200).collect())
}

fn json(body: &[u8]) -> Result<Value> {
    serde_json::from_slice(body).context("the instance answered no JSON")
}

fn insert(doc: &mut ResourceMeta, facts: Vec<(&str, Option<String>)>) {
    for (key, value) in facts {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
}

/// A repository's card from `/api/v1/repos/{owner}/{repo}`.
pub fn repo_card(mut doc: ResourceMeta, host: &str, body: &[u8]) -> Result<ResourceMeta> {
    let v = json(body)?;
    let full = text(&v["full_name"]).context("no repository name")?;
    let page = text(&v["html_url"])
        .filter(|u| web_url(u).is_some_and(|u| u.host_str() == Some(host)))
        .context("no repository page")?;
    doc.set_type("SoftwareSourceCode");
    doc.name = Some(full);
    doc.description = text(&v["description"]);
    doc.image = text(&v["avatar_url"]).filter(|u| web_url(u).is_some());
    let topics: Vec<String> = v["topics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(text)
        .collect();
    let count = |key: &str| v[key].as_u64().map(|n| n.to_string());
    insert(
        &mut doc,
        vec![
            ("stars", count("stars_count")),
            ("forks", count("forks_count")),
            ("language", text(&v["language"])),
            ("topics", list(&topics)),
            ("default_branch", text(&v["default_branch"])),
            ("updated", text(&v["updated_at"])),
            ("archived", (v["archived"] == true).then(|| "true".into())),
            (
                "mirror_of",
                text(&v["original_url"]).filter(|_| v["mirror"] == true),
            ),
            (
                "fork_of",
                text(&v["parent"]["full_name"]).filter(|_| v["fork"] == true),
            ),
            (
                "website",
                text(&v["website"]).filter(|u| web_url(u).is_some()),
            ),
            ("page_url", Some(page.clone())),
        ],
    );
    doc.source_root = Some(page);
    Ok(doc)
}

/// A branch's or tag's card, from `/branches/{name}` or `/tags/{name}`.
pub fn ref_card(
    mut doc: ResourceMeta,
    target: &Target,
    kind: RefKind,
    reference: &str,
    body: &[u8],
) -> Result<ResourceMeta> {
    let v = json(body)?;
    let commit = &v["commit"];
    let (sha, date, message) = match kind {
        RefKind::Tag => (
            text(&commit["sha"]),
            text(&commit["created"]),
            headline(&v["message"]),
        ),
        _ => (
            text(&commit["id"]),
            text(&commit["timestamp"]),
            headline(&commit["message"]),
        ),
    };
    anyhow::ensure!(sha.is_some(), "no commit for the {}", kind.segment());
    doc.set_type("SoftwareSourceCode");
    doc.name = Some(format!(
        "{}/{} · {}",
        target.owner,
        target.repo,
        decode(reference)
    ));
    doc.description = message;
    let root = format!("{}/src/{}/{reference}", target.page(), kind.segment());
    insert(
        &mut doc,
        vec![
            ("repository", Some(target.page())),
            (kind.segment(), Some(decode(reference))),
            ("commit", sha),
            ("committed", date),
            ("page_url", Some(root.clone())),
        ],
    );
    doc.source_root = Some(root);
    Ok(doc)
}

/// A file's or directory's card from its latest commit (`/commits?sha={ref}&path={path}`), or
/// `None` when nothing at that ref touches the path. With no path it is the tree at a commit.
pub fn file_card(
    mut doc: ResourceMeta,
    target: &Target,
    kind: RefKind,
    reference: &str,
    path: &str,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    match status {
        200 => {}
        404 => return Ok(None),
        s => anyhow::bail!("the instance answered {s}"),
    }
    let v = json(body)?;
    let Some(last) = v.as_array().and_then(|a| a.first()) else {
        return Ok(None);
    };
    doc.set_type("SoftwareSourceCode");
    let repo = format!("{}/{}", target.owner, target.repo);
    doc.name = Some(if path.is_empty() {
        format!(
            "{repo} · {}",
            reference.chars().take(10).collect::<String>()
        )
    } else {
        format!("{} · {repo}", decode(path))
    });
    doc.description = headline(&last["commit"]["message"]);
    let mut root = format!("{}/src/{}/{reference}", target.page(), kind.segment());
    if !path.is_empty() {
        root = format!("{root}/{path}");
    }
    insert(
        &mut doc,
        vec![
            ("repository", Some(target.page())),
            (kind.segment(), Some(decode(reference))),
            ("path", Some(decode(path)).filter(|p| !p.is_empty())),
            ("commit", text(&last["sha"])),
            ("committed", text(&last["created"])),
            ("page_url", Some(root.clone())),
        ],
    );
    doc.source_root = Some(root);
    Ok(Some(doc))
}

/// A release's card from `/releases/tags/{tag}`.
pub fn release_card(
    mut doc: ResourceMeta,
    target: &Target,
    tag: &str,
    body: &[u8],
) -> Result<ResourceMeta> {
    let v = json(body)?;
    let tag_name = text(&v["tag_name"]).context("no release tag")?;
    anyhow::ensure!(
        tag_name == decode(tag),
        "the instance answered for another release"
    );
    doc.set_type("SoftwareSourceCode");
    let title = text(&v["name"]).unwrap_or_else(|| tag_name.clone());
    doc.name = Some(format!("{}/{} {title}", target.owner, target.repo));
    doc.description = text(&v["body"])
        .map(|b| super::markdown_excerpt(&b))
        .filter(|b| !b.is_empty());
    let root = format!("{}/releases/tag/{tag}", target.page());
    insert(
        &mut doc,
        vec![
            ("repository", Some(target.page())),
            ("tag", Some(tag_name)),
            ("published", text(&v["published_at"])),
            (
                "prerelease",
                (v["prerelease"] == true).then(|| "true".into()),
            ),
            ("page_url", Some(root.clone())),
        ],
    );
    doc.source_root = Some(root);
    Ok(doc)
}

/// Whether `/api/v1/version` answered as Forgejo or Gitea.
pub fn is_instance(status: u16, body: &[u8]) -> bool {
    status == 200
        && serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| text(&v["version"]))
            .is_some()
}

#[async_trait]
impl Source for Forgejo {
    fn name(&self) -> &'static str {
        "forgejo"
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some_and(|t| match INSTANCES.get(&t.host) {
            Some(known) => known,
            None => KNOWN.contains(&t.host.as_str()) || t.distinctive(),
        })
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        // `{repo}.git` names the repository, on the instances known without asking.
        let t = Target::of_uri(uri)?;
        if t.object != Object::Repo || !KNOWN.contains(&t.host.as_str()) {
            return None;
        }
        let root = t.page();
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "stars",
            "forks",
            "language",
            "topics",
            "default_branch",
            "updated",
            "archived",
            "mirror_of",
            "fork_of",
            "website",
            "repository",
            "branch",
            "tag",
            "commit",
            "committed",
            "path",
            "published",
            "prerelease",
            "page_url",
        ]
    }
    fn budgets(&self) -> &'static [Budget] {
        // Codeberg: 2,000 requests per 10 minutes.
        &[("codeberg.org", 2.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://codeberg.org/forgejo/forgejo",
                card_type: "SoftwareSourceCode",
            },
            Example {
                uri: "https://codeberg.org/forgejo/forgejo/src/branch/forgejo/README.md",
                card_type: "SoftwareSourceCode",
            },
            Example {
                uri: "https://codeberg.org/forgejo/forgejo/releases/tag/v11.0.1",
                card_type: "SoftwareSourceCode",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a repository link")?;
        let host = target.host.clone();
        let json = FetchOptions {
            accept: Some("application/json".into()),
            ..Default::default()
        };
        match INSTANCES.get(&host) {
            Some(false) => return Err(Declined.into()),
            Some(true) => {}
            None if KNOWN.contains(&host.as_str()) => {}
            None => {
                let f = cx
                    .fetch(&format!("https://{host}/api/v1/version"), &json)
                    .await?;
                let is = is_instance(f.status, &f.body);
                INSTANCES.set(&host, is);
                if !is {
                    return Err(Declined.into());
                }
            }
        }
        let doc = match &target.object {
            Object::Repo => {
                let f = cx.fetch(&target.api(), &json).await?;
                match f.status {
                    200 => Some(repo_card(cx.card(uri, self), &host, &f.body)?),
                    404 => None,
                    s => anyhow::bail!("the instance answered {s}"),
                }
            }
            Object::Release { tag } => {
                let url = format!("{}/releases/tags/{tag}", target.api());
                let f = cx.fetch(&url, &json).await?;
                match f.status {
                    200 => Some(release_card(cx.card(uri, self), &target, tag, &f.body)?),
                    404 => None,
                    s => anyhow::bail!("the instance answered {s}"),
                }
            }
            Object::At { kind, .. } => {
                let mut found = None;
                for (reference, path) in target.splits() {
                    if *kind == RefKind::Commit {
                        found = Some((reference, path, None));
                        break;
                    }
                    let url = match kind {
                        RefKind::Tag => format!("{}/tags/{reference}", target.api()),
                        _ => format!("{}/branches/{reference}", target.api()),
                    };
                    let f = cx.fetch(&url, &json).await?;
                    match f.status {
                        200 => {
                            found = Some((reference, path, Some(f.body)));
                            break;
                        }
                        404 => continue,
                        s => anyhow::bail!("the instance answered {s}"),
                    }
                }
                match found {
                    None => None,
                    Some((reference, path, Some(body))) if path.is_empty() => Some(ref_card(
                        cx.card(uri, self),
                        &target,
                        *kind,
                        &reference,
                        &body,
                    )?),
                    Some((reference, path, _)) => {
                        let f = cx
                            .fetch(&commits_url(&target, &reference, &path), &json)
                            .await?;
                        file_card(
                            cx.card(uri, self),
                            &target,
                            *kind,
                            &reference,
                            &path,
                            f.status,
                            &f.body,
                        )?
                    }
                }
            }
        };
        let mut doc = doc.unwrap_or_else(|| cx.missing(uri, self));
        doc.site = host;
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("forgejo", file)
    }
    fn base(uri: &str) -> ResourceMeta {
        ResourceMeta::new(uri, "forgejo", 3600)
    }
    fn target(uri: &str) -> Target {
        Target::of_uri(uri).unwrap()
    }

    #[test]
    fn addresses() {
        let repo = target("https://codeberg.org/forgejo/forgejo.git");
        assert_eq!(
            (repo.owner.as_str(), repo.repo.as_str(), &repo.object),
            ("forgejo", "forgejo", &Object::Repo)
        );
        assert_eq!(
            repo.api(),
            "https://codeberg.org/api/v1/repos/forgejo/forgejo"
        );
        assert_eq!(
            Forgejo
                .alias_root("https://codeberg.org/forgejo/forgejo.git")
                .as_deref(),
            Some("https://codeberg.org/forgejo/forgejo")
        );
        assert_eq!(
            Forgejo.alias_root("https://codeberg.org/forgejo/forgejo"),
            None
        );
        // Another host's `.git` is not merged before it answers as an instance.
        assert_eq!(Forgejo.alias_root("https://git.example.org/a/b.git"), None);

        let file =
            target("https://codeberg.org/forgejo/forgejo/src/branch/v11.0/forgejo/README.md");
        assert_eq!(
            file.splits(),
            [
                ("v11.0".to_owned(), "forgejo/README.md".to_owned()),
                ("v11.0/forgejo".to_owned(), "README.md".to_owned()),
                ("v11.0/forgejo/README.md".to_owned(), String::new()),
            ]
        );
        let commit = target("https://codeberg.org/forgejo/forgejo/src/commit/cf64767a/docs/a.md");
        assert_eq!(
            commit.splits(),
            [("cf64767a".to_owned(), "docs/a.md".to_owned())]
        );
        assert_eq!(
            target("https://codeberg.org/forgejo/forgejo/releases/tag/v11.0.1").object,
            Object::Release {
                tag: "v11.0.1".into()
            }
        );

        // Repository pages are read on known instances; deeper shapes on any host.
        assert!(Forgejo.reads("https://codeberg.org/forgejo/forgejo"));
        assert!(Forgejo.reads("https://gitea.com/gitea/tea"));
        assert!(!Forgejo.reads("https://example.org/about/team"));
        assert!(Forgejo.reads("https://git.example.org/a/b/src/branch/main/x.rs"));
        for uri in [
            "https://codeberg.org/explore/repos",
            "https://codeberg.org/user/login",
            "https://codeberg.org/forgejo/forgejo/issues/1",
            "https://codeberg.org/forgejo/forgejo/src/commit/zzz/a",
            "https://codeberg.org/forgejo/forgejo/src/branch",
            "https://codeberg.org/forgejo/forgejo/src/blob/main/a",
            "http://codeberg.org/forgejo/forgejo",
            "https://codeberg.org:8443/forgejo/forgejo",
            "https://codeberg.org/.hidden/forgejo",
        ] {
            assert!(!Forgejo.reads(uri), "{uri}");
        }
    }

    #[test]
    fn detection() {
        assert!(is_instance(200, &read("codeberg-version.json")));
        assert!(is_instance(200, &read("gitea-version.json")));
        assert!(!is_instance(410, &read("not-forgejo-version.html")));
        assert!(!is_instance(200, b"<html>"));
        assert!(!is_instance(200, b"{\"version\":\"\"}"));
    }

    #[test]
    fn repository() {
        let uri = "https://codeberg.org/forgejo/forgejo";
        let doc = repo_card(base(uri), "codeberg.org", &read("repo-forgejo.json")).unwrap();
        assert_eq!(doc.type_name, "SoftwareSourceCode");
        assert_eq!(doc.name.as_deref(), Some("forgejo/forgejo"));
        assert_eq!(doc.description.as_deref(), Some("Beyond coding. We forge."));
        let a = &doc.attributes;
        assert_eq!(a["language"], "Go");
        assert_eq!(a["default_branch"], "forgejo");
        assert_eq!(a["topics"], "forge; forgejo; git; self-hosted");
        // A migrated repository that is not a mirror names no mirror source.
        assert!(!a.contains_key("mirror_of"));
        assert!(!a.contains_key("archived"));
        assert_eq!(doc.source_root.as_deref(), Some(uri));
        // The page must be on the host asked.
        assert!(repo_card(base(uri), "gitea.com", &read("repo-forgejo.json")).is_err());
        assert!(repo_card(base(uri), "codeberg.org", &read("repo-missing.json")).is_err());
    }

    #[test]
    fn branches_tags_and_files() {
        let t = target("https://codeberg.org/forgejo/forgejo/src/branch/forgejo");
        let doc = ref_card(
            base("x"),
            &t,
            RefKind::Branch,
            "forgejo",
            &read("branch-forgejo.json"),
        )
        .unwrap();
        assert_eq!(doc.name.as_deref(), Some("forgejo/forgejo · forgejo"));
        assert_eq!(doc.attributes["branch"], "forgejo");
        assert_eq!(doc.attributes["commit"].len(), 40);
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://codeberg.org/forgejo/forgejo/src/branch/forgejo")
        );

        let t = target("https://codeberg.org/forgejo/forgejo/src/tag/v11.0.1");
        let doc = ref_card(
            base("x"),
            &t,
            RefKind::Tag,
            "v11.0.1",
            &read("tag-v11.0.1.json"),
        )
        .unwrap();
        assert_eq!(doc.attributes["tag"], "v11.0.1");
        assert_eq!(doc.attributes["committed"], "2025-05-02T06:26:31Z");
        assert!(doc
            .description
            .unwrap()
            .starts_with("[v11.0/forgejo] fix(sec)"));

        let t = target("https://codeberg.org/forgejo/forgejo/src/branch/v11.0/forgejo/README.md");
        assert_eq!(
            commits_url(&t, "v11.0/forgejo", "README.md"),
            "https://codeberg.org/api/v1/repos/forgejo/forgejo/commits?sha=v11.0%2Fforgejo&path=README.md&limit=1&stat=false&files=false&verification=false"
        );
        let doc = file_card(
            base("x"),
            &t,
            RefKind::Branch,
            "v11.0/forgejo",
            "README.md",
            200,
            &read("commits-v11-readme.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.name.as_deref(), Some("README.md · forgejo/forgejo"));
        assert_eq!(doc.attributes["path"], "README.md");
        assert_eq!(doc.attributes["branch"], "v11.0/forgejo");
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://codeberg.org/forgejo/forgejo/src/branch/v11.0/forgejo/README.md")
        );
        // No author email reaches the card.
        assert!(!serde_json::to_string(&doc).unwrap().contains('@'));
        assert!(file_card(
            base("x"),
            &t,
            RefKind::Branch,
            "forgejo",
            "NOPE.md",
            404,
            &read("commits-missing.json")
        )
        .unwrap()
        .is_none());
        assert!(
            file_card(base("x"), &t, RefKind::Branch, "forgejo", "a", 200, b"[]")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn releases() {
        let t = target("https://codeberg.org/forgejo/forgejo/releases/tag/v11.0.1");
        let doc = release_card(base("x"), &t, "v11.0.1", &read("release-v11.0.1.json")).unwrap();
        assert_eq!(doc.name.as_deref(), Some("forgejo/forgejo v11.0.1"));
        assert_eq!(doc.attributes["tag"], "v11.0.1");
        assert!(doc.attributes.contains_key("published"));
        assert!(release_card(base("x"), &t, "v11.0.2", &read("release-v11.0.1.json")).is_err());
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::{model::Status, sources::fixture};

    const JSON: &str = "application/json";
    fn read(file: &str) -> Vec<u8> {
        fixture::read("forgejo", file)
    }
    const API: &str = "https://codeberg.org/api/v1/repos/forgejo/forgejo";

    #[tokio::test(start_paused = true)]
    async fn a_branch_with_a_slash_is_found_by_asking() {
        let (egress, recorded) = fixture::egress(vec![
            (&format!("{API}/branches/v11.0"), 404, JSON, read("branch-v11.0.json")),
            (
                &format!("{API}/branches/v11.0/forgejo"),
                200,
                JSON,
                read("branch-v11.0-forgejo.json"),
            ),
            (
                &format!("{API}/commits?sha=v11.0%2Fforgejo&path=README.md&limit=1&stat=false&files=false&verification=false"),
                200,
                JSON,
                read("commits-v11-readme.json"),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let uri = "https://codeberg.org/forgejo/forgejo/src/branch/v11.0/forgejo/README.md";
        let doc = Forgejo.resolve(&cx, uri).await.unwrap();
        assert_eq!(doc.name.as_deref(), Some("README.md · forgejo/forgejo"));
        assert_eq!(doc.attributes["branch"], "v11.0/forgejo");
        assert_eq!(doc.site, "codeberg.org");
        // Codeberg is known: no version question; two splits tried, then the commits.
        assert_eq!(recorded.asked().len(), 3);

        // A path nothing touches is missing.
        let doc = Forgejo
            .resolve(
                &cx,
                "https://codeberg.org/forgejo/forgejo/src/branch/v11.0/forgejo/NOPE.md",
            )
            .await
            .unwrap();
        assert_eq!(doc.status, Status::Missing);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_host_is_asked_once_and_remembered() {
        let (egress, recorded) = fixture::egress(vec![
            (
                "https://not-a-forge.example/api/v1/version",
                410,
                "text/html",
                read("not-forgejo-version.html"),
            ),
            (
                "https://forge.example/api/v1/version",
                200,
                JSON,
                read("codeberg-version.json"),
            ),
            (
                "https://forge.example/api/v1/repos/forgejo/forgejo/releases/tags/v11.0.1",
                200,
                JSON,
                read("release-v11.0.1.json"),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let other = "https://not-a-forge.example/a/b/src/branch/main/x.rs";
        assert!(Forgejo.reads(other));
        let error = Forgejo.resolve(&cx, other).await.unwrap_err();
        assert!(error.is::<Declined>());
        // Remembered: no second question, and the links are no longer claimed.
        assert!(!Forgejo.reads(other));
        assert!(Forgejo
            .resolve(&cx, other)
            .await
            .unwrap_err()
            .is::<Declined>());
        assert_eq!(recorded.asked().len(), 1);

        let release = "https://forge.example/forgejo/forgejo/releases/tag/v11.0.1";
        let doc = Forgejo.resolve(&cx, release).await.unwrap();
        assert_eq!(doc.attributes["tag"], "v11.0.1");
        assert_eq!(doc.site, "forge.example");
        // Its repository pages are read from now on.
        assert!(Forgejo.reads("https://forge.example/forgejo/forgejo"));
    }
}
