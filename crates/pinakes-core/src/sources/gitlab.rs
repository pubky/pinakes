//! GitLab (gitlab.com and any other instance): projects, branches, tags, releases and files,
//! read from the instance's API (`/api/v4/projects/…`). No key; gitlab.com allows 500
//! unauthenticated API requests per minute. Project pages are read on known instances; the
//! `/-/` shapes on any host, which is remembered for a day once it answers (or does not answer)
//! as GitLab.
use super::kit::GITLAB_RESERVED as RESERVED;
use super::{list, text, web_url, Budget, Context, Declined, Example, HostMemo, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::LazyLock;

pub struct Gitlab;

/// Which hosts answered as GitLab, and which did not.
static INSTANCES: LazyLock<HostMemo> = LazyLock::new(HostMemo::default);

/// Instances known without asking; their project pages are read too.
const KNOWN: &[&str] = &[
    "framagit.org",
    "gitlab.com",
    "gitlab.freedesktop.org",
    "gitlab.gnome.org",
    "invent.kde.org",
    "salsa.debian.org",
];

/// How many leading segments of `/-/tree/…` or `/-/blob/…` are tried as the ref name. A link
/// with more, none of them a ref, is declined: the ref may be a longer name.
const MAX_REF_SEGMENTS: usize = 3;
/// The deepest project path read (groups nest).
const MAX_DEPTH: usize = 8;

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with(['.', '-'])
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn segment(value: &str) -> bool {
    !value.is_empty() && value.len() <= 255 && value != "." && value != ".."
}
fn decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}
/// A path parameter: the decoded value with every byte outside `[A-Za-z0-9._-]` encoded (a
/// ref's `/` becomes `%2F`).
fn param(value: &str) -> String {
    const KEEP: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'.')
        .remove(b'_')
        .remove(b'-');
    percent_encoding::utf8_percent_encode(&decode(value), KEEP).to_string()
}

/// What a project link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    Project,
    /// `/-/tree/{ref}[/{path}]` or `/-/blob/{ref}/{path}`: the ref and path, not yet split.
    At {
        blob: bool,
        rest: Vec<String>,
    },
    Tag {
        tag: String,
    },
    Release {
        tag: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    /// The project's path with its namespace, e.g. `inkscape/inkscape`.
    pub project: String,
    pub object: Object,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
    Commit,
}

impl Target {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = super::kit::web_url(uri)?;
        if url.scheme() != "https" || url.port().is_some() {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let (project, rest) = match segments.iter().position(|s| *s == "-") {
            Some(i) => (&segments[..i], Some(&segments[i + 1..])),
            None => (segments.as_slice(), None),
        };
        let mut project: Vec<&str> = project.to_vec();
        if rest.is_none() {
            if let Some(last) = project.last_mut() {
                *last = last.strip_suffix(".git").unwrap_or(last);
            }
        }
        let first = project.first()?.to_ascii_lowercase();
        if !(2..=MAX_DEPTH).contains(&project.len())
            || RESERVED.contains(&first.as_str())
            || !project.iter().all(|s| name(s))
        {
            return None;
        }
        let object = match rest {
            None => Object::Project,
            Some([kind @ ("tree" | "blob"), rest @ ..])
                if rest.len() > usize::from(*kind == "blob")
                    && rest.len() <= 40
                    && rest.iter().all(|s| segment(s)) =>
            {
                Object::At {
                    blob: *kind == "blob",
                    rest: rest.iter().map(|s| (*s).to_owned()).collect(),
                }
            }
            Some(["tags", tag]) if segment(tag) => Object::Tag {
                tag: (*tag).to_owned(),
            },
            Some(["releases", tag]) if segment(tag) => Object::Release {
                tag: (*tag).to_owned(),
            },
            Some(_) => return None,
        };
        Some(Self {
            host,
            project: project.join("/"),
            object,
        })
    }
    pub fn page(&self) -> String {
        format!("https://{}/{}", self.host, self.project)
    }
    pub fn api(&self) -> String {
        format!(
            "https://{}/api/v4/projects/{}",
            self.host,
            self.project.replace('/', "%2F")
        )
    }
    /// The (ref, path) splits to try, shortest ref first. A blob keeps a non-empty path; a
    /// full commit id is the ref by itself.
    pub fn splits(&self) -> Vec<(String, String)> {
        let Object::At { blob, rest } = &self.object else {
            return vec![];
        };
        if sha(&rest[0]) {
            return vec![(rest[0].clone(), rest[1..].join("/"))];
        }
        let most = rest
            .len()
            .saturating_sub(usize::from(*blob))
            .min(MAX_REF_SEGMENTS);
        (1..=most)
            .map(|n| (rest[..n].join("/"), rest[n..].join("/")))
            .collect()
    }
    /// Whether the ref may be longer than any split `splits` tries: the link has more leading
    /// segments that could be the ref than `MAX_REF_SEGMENTS`.
    fn longer_ref(&self) -> bool {
        match &self.object {
            Object::At { blob, rest } => {
                !sha(&rest[0]) && rest.len().saturating_sub(usize::from(*blob)) > MAX_REF_SEGMENTS
            }
            _ => false,
        }
    }
    /// Whether only GitLab has links of this shape (`/-/…`).
    fn distinctive(&self) -> bool {
        self.object != Object::Project
    }
}

/// The latest commit touching `path` at `reference`.
pub fn commits_url(target: &Target, reference: &str, path: &str) -> String {
    let mut url =
        url::Url::parse(&format!("{}/repository/commits", target.api())).expect("valid base");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("ref_name", &decode(reference));
        if !path.is_empty() {
            query.append_pair("path", &decode(path));
        }
        query.append_pair("per_page", "1");
    }
    url.into()
}

/// Whether an API answer comes from GitLab: JSON naming a project, or GitLab's own error shape
/// (`{"message": "404 Project Not Found"}`).
pub fn is_gitlab(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    v["path_with_namespace"].is_string()
        || text(&v["message"]).is_some_and(|m| {
            let b = m.as_bytes();
            b.len() > 4 && b[..3].iter().all(u8::is_ascii_digit) && b[3] == b' '
        })
}

/// The string a 200 answer holds when it is the object asked for, as a JSON pointer: a project's
/// path, a branch's or tag's commit, a release's tag, the first commit of a list.
const PROJECT: &str = "/path_with_namespace";
const REF: &str = "/commit/id";
const RELEASE: &str = "/tag_name";
const COMMITS: &str = "/0/id";

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
fn line(value: &Value) -> Option<String> {
    let line = super::without_addresses(text(value)?.lines().next()?.trim());
    (!line.is_empty()).then(|| line.chars().take(200).collect())
}

/// A project's card from `/api/v4/projects/{path}`.
pub fn project_card(mut doc: ResourceMeta, host: &str, body: &[u8]) -> Result<ResourceMeta> {
    let v = json(body)?;
    let path = text(&v["path_with_namespace"]).context("no project path")?;
    let page = text(&v["web_url"])
        .filter(|u| web_url(u).is_some_and(|u| u.host_str() == Some(host)))
        .context("no project page")?;
    doc.set_type("SoftwareSourceCode");
    doc.name = Some(path);
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
            ("stars", count("star_count")),
            ("forks", count("forks_count")),
            ("topics", list(&topics)),
            ("default_branch", text(&v["default_branch"])),
            ("updated", text(&v["last_activity_at"])),
            ("archived", (v["archived"] == true).then(|| "true".into())),
            (
                "fork_of",
                text(&v["forked_from_project"]["path_with_namespace"]),
            ),
            ("page_url", Some(page.clone())),
        ],
    );
    doc.source_root = Some(page);
    Ok(doc)
}

fn kind_name(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Branch => "branch",
        RefKind::Tag => "tag",
        RefKind::Commit => "commit",
    }
}

/// A branch's or tag's card from `/repository/branches/{name}` or `/repository/tags/{name}`,
/// for its tree (`/-/tree/{ref}`) or, with `tag_page`, the tag's page (`/-/tags/{tag}`).
pub fn ref_card(
    mut doc: ResourceMeta,
    target: &Target,
    kind: RefKind,
    reference: &str,
    tag_page: bool,
    body: &[u8],
) -> Result<ResourceMeta> {
    let v = json(body)?;
    let commit = &v["commit"];
    let id = text(&commit["id"]).context("no commit")?;
    doc.set_type("SoftwareSourceCode");
    doc.name = Some(format!("{} · {}", target.project, decode(reference)));
    doc.description = match kind {
        RefKind::Tag => line(&v["message"]).or_else(|| line(&commit["title"])),
        _ => line(&commit["title"]),
    };
    let shape = if tag_page { "tags" } else { "tree" };
    let root = format!("{}/-/{shape}/{reference}", target.page());
    insert(
        &mut doc,
        vec![
            ("repository", Some(target.page())),
            (kind_name(kind), Some(decode(reference))),
            ("commit", Some(id)),
            ("committed", text(&commit["committed_date"])),
            ("page_url", Some(root.clone())),
        ],
    );
    doc.source_root = Some(root);
    Ok(doc)
}

/// A file's or directory's card from its latest commit, or `None` when nothing at that ref
/// touches the path.
#[allow(clippy::too_many_arguments)]
pub fn file_card(
    mut doc: ResourceMeta,
    target: &Target,
    kind: RefKind,
    blob: bool,
    reference: &str,
    path: &str,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let Some(body) = super::kit::body(status, body, &[404])? else {
        return Ok(None);
    };
    let v = json(body)?;
    let Some(last) = v.as_array().and_then(|a| a.first()) else {
        return Ok(None);
    };
    doc.set_type("SoftwareSourceCode");
    doc.name = Some(if path.is_empty() {
        format!(
            "{} · {}",
            target.project,
            reference.chars().take(10).collect::<String>()
        )
    } else {
        format!("{} · {}", decode(path), target.project)
    });
    doc.description = line(&last["title"]);
    let shape = if blob { "blob" } else { "tree" };
    let mut root = format!("{}/-/{shape}/{reference}", target.page());
    if !path.is_empty() {
        root = format!("{root}/{path}");
    }
    insert(
        &mut doc,
        vec![
            ("repository", Some(target.page())),
            (kind_name(kind), Some(decode(reference))),
            ("path", Some(decode(path)).filter(|p| !p.is_empty())),
            ("commit", text(&last["id"])),
            ("committed", text(&last["committed_date"])),
            ("page_url", Some(root.clone())),
        ],
    );
    doc.source_root = Some(root);
    Ok(Some(doc))
}

/// A release's card from `/releases/{tag}`.
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
    doc.name = Some(format!("{} {title}", target.project));
    doc.description = text(&v["description"])
        .map(|b| super::markdown_excerpt(&b))
        .filter(|b| !b.is_empty());
    let root = format!("{}/-/releases/{tag}", target.page());
    insert(
        &mut doc,
        vec![
            ("repository", Some(target.page())),
            ("tag", Some(tag_name)),
            ("published", text(&v["released_at"])),
            (
                "upcoming",
                (v["upcoming_release"] == true).then(|| "true".into()),
            ),
            ("page_url", Some(root.clone())),
        ],
    );
    doc.source_root = Some(root);
    Ok(doc)
}

#[async_trait]
impl Source for Gitlab {
    fn name(&self) -> &'static str {
        "gitlab"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "gitlab",
                uri: "https://gitlab.com/inkscape/inkscape",
                card_type: "SoftwareSourceCode",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "gitlab.com",
                path: "/api/v4/projects/inkscape%2Finkscape",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "project-inkscape.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some_and(|t| match INSTANCES.get(&t.host) {
            Some(known) => known,
            None => KNOWN.contains(&t.host.as_str()) || t.distinctive(),
        })
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        // `{project}.git` names the project, on the instances known without asking.
        let t = Target::of_uri(uri)?;
        if t.object != Object::Project || !KNOWN.contains(&t.host.as_str()) {
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
            "topics",
            "default_branch",
            "updated",
            "archived",
            "fork_of",
            "repository",
            "branch",
            "tag",
            "commit",
            "committed",
            "path",
            "published",
            "upcoming",
            "page_url",
        ]
    }
    fn budgets(&self) -> &'static [Budget] {
        // gitlab.com: 500 unauthenticated API requests per minute.
        &[("gitlab.com", 2.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://gitlab.com/inkscape/inkscape",
                card_type: "SoftwareSourceCode",
            },
            Example {
                uri: "https://gitlab.com/inkscape/inkscape/-/blob/master/README.md",
                card_type: "SoftwareSourceCode",
            },
            Example {
                uri: "https://gitlab.com/inkscape/inkscape/-/releases/INKSCAPE_1_4",
                card_type: "SoftwareSourceCode",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a GitLab link")?;
        let host = target.host.clone();
        if INSTANCES.get(&host) == Some(false) {
            return Err(Declined.into());
        }
        let json = FetchOptions::json();
        // The first answer from an unknown host says whether it is GitLab: GitLab's own error
        // shape, or a 200 that holds `field`, the string the object asked for always has.
        let get = |url: String, field: &'static str| {
            let host = host.clone();
            let json = json.clone();
            async move {
                // Only a host not yet known either way is judged by this answer; a known
                // instance is never marked as not one.
                let unknown = INSTANCES.get(&host).is_none() && !KNOWN.contains(&host.as_str());
                let not_gitlab = || {
                    if unknown {
                        INSTANCES.set(&host, false);
                    }
                    Err(Declined.into())
                };
                let f = match cx.fetch(&url, &json).await {
                    Ok(f) => f,
                    Err(e) if unknown && !super::own_rate_limit(&e) => return not_gitlab(),
                    Err(e) => return Err(e),
                };
                // On 200: whether the JSON answer is the object asked for.
                let object = match f.status {
                    200 => serde_json::from_slice::<Value>(&f.body)
                        .ok()
                        .map(|v| v.pointer(field).is_some_and(Value::is_string)),
                    _ => None,
                };
                match object {
                    Some(true) => INSTANCES.set(&host, true),
                    // An instance's answer without it (no commit touches a path) is read as
                    // it is; from an unknown host it does not show GitLab.
                    Some(false) if !unknown => {}
                    None if f.status != 200 && is_gitlab(&f.body) => INSTANCES.set(&host, true),
                    _ => return not_gitlab(),
                }
                Ok::<_, anyhow::Error>(f)
            }
        };
        let doc = match &target.object {
            Object::Project => {
                let f = get(target.api(), PROJECT).await?;
                match f.status {
                    200 => match project_card(cx.card(uri, self), &host, &f.body) {
                        Ok(doc) => Some(doc),
                        // On a learned host, a page shaped like a project may be any page.
                        Err(_) if !KNOWN.contains(&host.as_str()) => return Err(Declined.into()),
                        Err(e) => return Err(e),
                    },
                    // A namespace or another page shaped like a project: an ordinary web card.
                    404 => return Err(Declined.into()),
                    s => anyhow::bail!("GitLab answered {s}"),
                }
            }
            Object::Tag { tag } => {
                let url = format!("{}/repository/tags/{}", target.api(), param(tag));
                let f = get(url, REF).await?;
                match f.status {
                    200 => Some(ref_card(
                        cx.card(uri, self),
                        &target,
                        RefKind::Tag,
                        tag,
                        true,
                        &f.body,
                    )?),
                    404 => None,
                    s => anyhow::bail!("GitLab answered {s}"),
                }
            }
            Object::Release { tag } => {
                let f = get(format!("{}/releases/{}", target.api(), param(tag)), RELEASE).await?;
                match f.status {
                    200 => Some(release_card(cx.card(uri, self), &target, tag, &f.body)?),
                    404 => None,
                    s => anyhow::bail!("GitLab answered {s}"),
                }
            }
            Object::At { blob, .. } => {
                let mut found = None;
                'splits: for (reference, path) in target.splits() {
                    if sha(&reference) {
                        found = Some((RefKind::Commit, reference, path, None));
                        break;
                    }
                    for (kind, list) in [(RefKind::Branch, "branches"), (RefKind::Tag, "tags")] {
                        let url =
                            format!("{}/repository/{list}/{}", target.api(), param(&reference));
                        let f = get(url, REF).await?;
                        match f.status {
                            200 => {
                                found = Some((kind, reference, path, Some(f.body)));
                                break 'splits;
                            }
                            404 => continue,
                            s => anyhow::bail!("GitLab answered {s}"),
                        }
                    }
                }
                match found {
                    // The ref may be one of the longer names not tried: the page's web card.
                    None if target.longer_ref() => return Err(Declined.into()),
                    None => None,
                    Some((kind, reference, path, Some(body))) if path.is_empty() => Some(ref_card(
                        cx.card(uri, self),
                        &target,
                        kind,
                        &reference,
                        false,
                        &body,
                    )?),
                    Some((kind, reference, path, _)) => {
                        let f = get(commits_url(&target, &reference, &path), COMMITS).await?;
                        file_card(
                            cx.card(uri, self),
                            &target,
                            kind,
                            *blob,
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
    use crate::{model::Status, sources::fixture};

    const JSON: &str = "application/json";
    const API: &str = "https://gitlab.com/api/v4/projects/inkscape%2Finkscape";
    fn read(file: &str) -> Vec<u8> {
        fixture::read("gitlab", file)
    }
    fn base() -> ResourceMeta {
        ResourceMeta::new("https://gitlab.com/x/y", "gitlab", 3600)
    }
    fn target(uri: &str) -> Target {
        Target::of_uri(uri).unwrap()
    }

    #[test]
    fn addresses() {
        let t = target("https://gitlab.com/gitlab-org/security/gitlab.git");
        assert_eq!(
            (t.project.as_str(), &t.object),
            ("gitlab-org/security/gitlab", &Object::Project)
        );
        assert_eq!(
            t.api(),
            "https://gitlab.com/api/v4/projects/gitlab-org%2Fsecurity%2Fgitlab"
        );
        assert_eq!(
            Gitlab
                .alias_root("https://gitlab.com/inkscape/inkscape.git")
                .as_deref(),
            Some("https://gitlab.com/inkscape/inkscape")
        );
        assert_eq!(Gitlab.alias_root("https://git.example.org/a/b.git"), None);

        let blob = target("https://gitlab.com/inkscape/inkscape/-/blob/1.4.x/share/README.md");
        assert_eq!(
            blob.splits(),
            [
                ("1.4.x".to_owned(), "share/README.md".to_owned()),
                ("1.4.x/share".to_owned(), "README.md".to_owned()),
            ]
        );
        let tree = target("https://gitlab.com/inkscape/inkscape/-/tree/master");
        assert_eq!(tree.splits(), [("master".to_owned(), String::new())]);
        let at = "66521dc6553f24c91df190476dd5c24c1daef8ca";
        let commit = target(&format!(
            "https://gitlab.com/inkscape/inkscape/-/tree/{at}/src"
        ));
        assert_eq!(commit.splits(), [(at.to_owned(), "src".to_owned())]);
        assert_eq!(
            target("https://gitlab.com/inkscape/inkscape/-/releases/INKSCAPE_1_4").object,
            Object::Release {
                tag: "INKSCAPE_1_4".into()
            }
        );

        assert!(Gitlab.reads("https://gitlab.com/inkscape/inkscape"));
        assert!(Gitlab.reads("https://salsa.debian.org/debian/devscripts"));
        assert!(!Gitlab.reads("https://example.org/about/team"));
        assert!(Gitlab.reads("https://git.example.org/a/b/-/tree/main"));
        for uri in [
            "https://gitlab.com/explore/projects",
            "https://gitlab.com/users/sign_in",
            "https://gitlab.com/inkscape",
            "https://gitlab.com/inkscape/inkscape/-/issues/1",
            "https://gitlab.com/inkscape/inkscape/-/blob/master",
            "https://gitlab.com/-/ide/project/inkscape/inkscape",
            "https://gitlab.com/inkscape/-weird/x",
            "http://gitlab.com/inkscape/inkscape",
        ] {
            assert!(!Gitlab.reads(uri), "{uri}");
        }
    }

    #[test]
    fn detection() {
        assert!(is_gitlab(&read("project-inkscape.json")));
        assert!(is_gitlab(&read("project-missing.json")));
        assert!(is_gitlab(&read("branch-missing.json")));
        assert!(!is_gitlab(&read("not-gitlab.html")));
        assert!(!is_gitlab(b"{\"message\":\"hello\"}"));
        assert!(!is_gitlab("{\"message\":\"404é\"}".as_bytes()));
    }

    #[test]
    fn cards() {
        let doc = project_card(base(), "gitlab.com", &read("project-inkscape.json")).unwrap();
        assert_eq!(doc.type_name, "SoftwareSourceCode");
        assert_eq!(doc.name.as_deref(), Some("inkscape/inkscape"));
        assert_eq!(doc.attributes["default_branch"], "master");
        assert!(doc.attributes["topics"].contains("svg"));
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://gitlab.com/inkscape/inkscape")
        );
        assert!(project_card(base(), "framagit.org", &read("project-inkscape.json")).is_err());

        let t = target("https://gitlab.com/inkscape/inkscape/-/tags/INKSCAPE_1_4");
        let doc = ref_card(
            base(),
            &t,
            RefKind::Tag,
            "INKSCAPE_1_4",
            true,
            &read("tag-inkscape-1-4.json"),
        )
        .unwrap();
        assert_eq!(
            doc.name.as_deref(),
            Some("inkscape/inkscape · INKSCAPE_1_4")
        );
        assert_eq!(doc.description.as_deref(), Some("Inkscape 1.4"));
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://gitlab.com/inkscape/inkscape/-/tags/INKSCAPE_1_4")
        );

        let t = target("https://gitlab.com/inkscape/inkscape/-/blob/master/README.md");
        let doc = file_card(
            base(),
            &t,
            RefKind::Branch,
            true,
            "master",
            "README.md",
            200,
            &read("commits-readme.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.name.as_deref(), Some("README.md · inkscape/inkscape"));
        assert_eq!(doc.attributes["branch"], "master");
        assert!(!serde_json::to_string(&doc).unwrap().contains('@'));
        assert!(file_card(
            base(),
            &t,
            RefKind::Branch,
            true,
            "master",
            "NOPE.md",
            200,
            &read("commits-none.json")
        )
        .unwrap()
        .is_none());

        let t = target("https://gitlab.com/inkscape/inkscape/-/releases/INKSCAPE_1_4");
        let doc = release_card(
            base(),
            &t,
            "INKSCAPE_1_4",
            &read("release-inkscape-1-4.json"),
        )
        .unwrap();
        assert_eq!(doc.name.as_deref(), Some("inkscape/inkscape Inkscape 1.4"));
        assert_eq!(doc.attributes["published"], "2024-10-12T22:00:00.000Z");
        assert!(!doc.description.unwrap().contains("**"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_ref_with_a_slash_is_found_by_asking() {
        let api = "https://gitlab.com/api/v4/projects/gitlab-org%2Fgitlab";
        let (egress, recorded) = fixture::egress(vec![
            (&format!("{api}/repository/branches/17-7"), 404, JSON, read("branch-17-7.json")),
            (&format!("{api}/repository/tags/17-7"), 404, JSON, read("tag-17-7.json")),
            (
                &format!("{api}/repository/branches/17-7%2Fcreated-at-for-pipeline-configuration-full-path"),
                200,
                JSON,
                read("branch-17-7-slash.json"),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let uri = "https://gitlab.com/gitlab-org/gitlab/-/tree/17-7/created-at-for-pipeline-configuration-full-path";
        let doc = Gitlab.resolve(&cx, uri).await.unwrap();
        assert_eq!(
            doc.attributes["branch"],
            "17-7/created-at-for-pipeline-configuration-full-path"
        );
        assert_eq!(doc.source_root.as_deref(), Some(uri));
        assert_eq!(recorded.asked().len(), 3);
    }

    /// A ref no tried split finds is `missing` only when every split was tried. A link with more
    /// segments may name a longer ref, so its page gets a web card.
    #[tokio::test(start_paused = true)]
    async fn a_ref_longer_than_the_splits_tried_is_declined() {
        let api = "https://gitlab.com/api/v4/projects/g%2Fp/repository";
        let routes: Vec<String> = [
            "dependabot",
            "dependabot%2Fnpm_and_yarn",
            "dependabot%2Fnpm_and_yarn%2Fbabel",
        ]
        .iter()
        .flat_map(|name| ["branches", "tags"].map(|list| format!("{api}/{list}/{name}")))
        .collect();
        let (egress, recorded) = fixture::egress(
            routes
                .iter()
                .map(|url| (url.as_str(), 404, JSON, read("branch-missing.json")))
                .collect(),
        );
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let page = "https://gitlab.com/g/p/-/tree/dependabot/npm_and_yarn/babel";
        let answer = Gitlab.resolve(&cx, &format!("{page}/core-7.26.0")).await;
        assert_eq!(recorded.asked(), routes);
        assert!(
            answer.is_err_and(|e| e.is::<Declined>()),
            "a ref of four segments was not tried, yet the link is not declined"
        );
        // Every split of three segments was tried: GitLab has no such ref.
        let doc = Gitlab.resolve(&cx, page).await.unwrap();
        assert_eq!(doc.status, Status::Missing);
    }

    #[tokio::test(start_paused = true)]
    async fn files_namespaces_and_other_hosts() {
        let (egress, recorded) = fixture::egress(vec![
            (
                &format!("{API}/repository/branches/master"),
                200,
                JSON,
                read("branch-master.json"),
            ),
            (
                &format!("{API}/repository/commits?ref_name=master&path=NOPE.md&per_page=1"),
                200,
                JSON,
                read("commits-none.json"),
            ),
            (
                "https://gitlab.com/api/v4/projects/nosuch%2Fnosuch",
                404,
                JSON,
                read("project-missing.json"),
            ),
            (
                "https://not-gitlab.example/api/v4/projects/a%2Fb/repository/branches/main",
                404,
                "text/html",
                read("not-gitlab.html"),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Gitlab
            .resolve(
                &cx,
                "https://gitlab.com/inkscape/inkscape/-/blob/master/NOPE.md",
            )
            .await
            .unwrap();
        assert_eq!(doc.status, Status::Missing);
        // A page shaped like a project that is not one (a group, a user) gets a web card.
        let error = Gitlab
            .resolve(&cx, "https://gitlab.com/nosuch/nosuch")
            .await
            .unwrap_err();
        assert!(error.is::<Declined>());
        let other = "https://not-gitlab.example/a/b/-/tree/main";
        assert!(Gitlab
            .resolve(&cx, other)
            .await
            .unwrap_err()
            .is::<Declined>());
        assert!(!Gitlab.reads(other));
        let asked = recorded.asked().len();
        assert!(Gitlab
            .resolve(&cx, other)
            .await
            .unwrap_err()
            .is::<Declined>());
        assert_eq!(recorded.asked().len(), asked);
    }

    /// An unknown host is GitLab when its 200 answer is the object asked for. Any other JSON
    /// leaves the host's links to the web route.
    #[tokio::test(start_paused = true)]
    async fn a_200_that_is_not_the_object_asked_for_is_not_gitlab() {
        let (egress, _) = fixture::egress(vec![(
            "https://json.example/api/v4/projects/a%2Fb/repository/branches/main",
            200,
            JSON,
            b"{}".to_vec(),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let link = "https://json.example/a/b/-/tree/main";
        let error = Gitlab.resolve(&cx, link).await.unwrap_err();
        assert!(error.is::<Declined>(), "{error}");
        assert!(!Gitlab.reads(link));
        assert!(!Gitlab.reads("https://json.example/a/b"));
    }

    /// On a host learned as GitLab, a page shaped like a project whose answer is no project of
    /// that host gets a web card.
    #[tokio::test(start_paused = true)]
    async fn a_learned_host_s_page_that_is_no_project_is_declined() {
        let api = "https://learned.example/api/v4/projects";
        let (egress, _) = fixture::egress(vec![
            (
                &format!("{api}/a%2Fb/repository/branches/master"),
                200,
                JSON,
                read("branch-master.json"),
            ),
            (&format!("{api}/team%2Fhandbook"), 200, JSON, b"{}".to_vec()),
            // A project whose page is on another host.
            (
                &format!("{api}/inkscape%2Finkscape"),
                200,
                JSON,
                read("project-inkscape.json"),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Gitlab
            .resolve(&cx, "https://learned.example/a/b/-/tree/master")
            .await
            .unwrap();
        assert_eq!(doc.attributes["branch"], "master");
        for page in [
            "https://learned.example/team/handbook",
            "https://learned.example/inkscape/inkscape",
        ] {
            assert!(Gitlab.reads(page), "{page}");
            let error = Gitlab.resolve(&cx, page).await.unwrap_err();
            assert!(error.is::<Declined>(), "{page}: {error}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failures_judge_unknown_hosts_only() {
        let (egress, recorded) = fixture::egress(vec![
            (
                "https://busy.example/api/v4/projects/a%2Fb/repository/branches/main",
                503,
                "text/html",
                b"<html>busy</html>".to_vec(),
            ),
            (
                "https://framagit.org/api/v4/projects/a%2Fb",
                200,
                "text/html",
                b"<html>a wall</html>".to_vec(),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        // An unknown host that fails is not GitLab for a day.
        let busy = "https://busy.example/a/b/-/tree/main";
        assert!(Gitlab
            .resolve(&cx, busy)
            .await
            .unwrap_err()
            .is::<Declined>());
        assert!(!Gitlab.reads(busy));
        let asked = recorded.asked().len();
        assert!(Gitlab
            .resolve(&cx, busy)
            .await
            .unwrap_err()
            .is::<Declined>());
        assert_eq!(recorded.asked().len(), asked);
        // A known instance answering something else once stays an instance.
        let known = "https://framagit.org/a/b";
        assert!(Gitlab
            .resolve(&cx, known)
            .await
            .unwrap_err()
            .is::<Declined>());
        assert!(Gitlab.reads(known));
    }
}
