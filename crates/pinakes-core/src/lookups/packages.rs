//! Software packages: crates.io, npm and PyPI pages from a package name, and the source
//! repository only when signed build provenance names it (crates.io trusted publishing, npm
//! SLSA provenance, PyPI trusted publishers, deps.dev `SLSA_ATTESTATION`). A package's
//! `repository` field is the publisher's claim and never joins anything. Software Heritage
//! adds its archive page for a repository it holds.
use super::{body, encode, plain, web_url, Found, Key, Link, Request};
use anyhow::{Context, Result};
use base64::Engine;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registry {
    Crates,
    Npm,
    PyPI,
}

/// A package on one registry, by its name as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub registry: Registry,
    pub name: String,
}

fn crate_name(v: &str) -> bool {
    plain(v, 64, |c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && v.starts_with(|c: char| c.is_ascii_alphabetic())
}
fn npm_part(v: &str) -> bool {
    plain(v, 214, |c| c.is_ascii_alphanumeric() || "-._~".contains(c)) && !v.starts_with(['.', '_'])
}
fn npm_name(v: &str) -> bool {
    match v.strip_prefix('@').map(|s| s.split_once('/')) {
        Some(Some((scope, name))) => npm_part(scope) && npm_part(name),
        Some(None) => false,
        None => npm_part(v),
    }
}
fn pypi_name(v: &str) -> bool {
    plain(v, 200, |c| c.is_ascii_alphanumeric() || "-._".contains(c))
        && v.starts_with(|c: char| c.is_ascii_alphanumeric())
        && v.ends_with(|c: char| c.is_ascii_alphanumeric())
}

impl Package {
    pub fn new(registry: Registry, name: &str) -> Option<Self> {
        let ok = match registry {
            Registry::Crates => crate_name(name),
            Registry::Npm => npm_name(name),
            Registry::PyPI => pypi_name(name),
        };
        ok.then(|| Self {
            registry,
            name: name.to_owned(),
        })
    }

    /// The package of a registry or documentation page: `crates.io/crates/{n}`,
    /// `docs.rs/{n}`, `docs.rs/crate/{n}`, `lib.rs/crates/{n}`, `npmjs.com/package/{n}`,
    /// `jsdelivr.com/package/npm/{n}`, `pypi.org/project/{n}`.
    /// The package a registry or documentation page is about: the package's own page, or its
    /// page at one version (`/crates/rand/0.8.5`, `/package/semver/v/7.8.5`,
    /// `/project/urllib3/2.2.0/`, docs.rs's crate root at a version). A page under the package
    /// (a function's documentation, a source file, a dependency list) is another object and
    /// names no package.
    pub fn from_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        let host = url.host_str()?.trim_start_matches("www.");
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        fn decoded(s: &str) -> Option<String> {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .ok()
                .map(|s| s.into_owned())
        }
        // A scoped npm name takes two segments (`@scope/name`, the `@` percent-encoded or not).
        fn npm<'a>(rest: &'a [&'a str]) -> Option<(String, &'a [&'a str])> {
            match rest {
                [scope, name, after @ ..] if scope.starts_with('@') || scope.starts_with("%40") => {
                    Some((format!("{}/{}", decoded(scope)?, decoded(name)?), after))
                }
                [name, after @ ..] => Some((decoded(name)?, after)),
                [] => None,
            }
        }
        let at_version = |after: &[&str]| {
            after.is_empty() || matches!(after, [v] if *v == "latest" || version_number(v))
        };
        let (registry, name) = match (host, segments.as_slice()) {
            ("crates.io", ["crates", name, after @ ..]) if at_version(after) => {
                (Registry::Crates, (*name).to_owned())
            }
            ("lib.rs", ["crates", name]) => (Registry::Crates, (*name).to_owned()),
            ("docs.rs", ["crate", name, after @ ..]) if at_version(after) => {
                (Registry::Crates, (*name).to_owned())
            }
            // docs.rs: the crate's page, its page at a version (`latest` too), or the crate's own
            // documentation index there (`/rand/latest/rand/`), never an item's page.
            ("docs.rs", [name, after @ ..])
                if !matches!(*name, "about" | "releases" | "crate" | "sitemap.xml")
                    && match after {
                        [] => true,
                        [v] | [v, _] if *v == "latest" || version_number(v) => after
                            .get(1)
                            .is_none_or(|index| index.replace('-', "_") == name.replace('-', "_")),
                        _ => false,
                    } =>
            {
                (Registry::Crates, (*name).to_owned())
            }
            ("npmjs.com", ["package", rest @ ..]) => {
                let (name, after) = npm(rest)?;
                match after {
                    [] => (Registry::Npm, name),
                    ["v", v] if version_number(v) => (Registry::Npm, name),
                    _ => return None,
                }
            }
            ("jsdelivr.com", ["package", "npm", rest @ ..]) => {
                let (name, after) = npm(rest)?;
                if !after.is_empty() {
                    return None;
                }
                (Registry::Npm, name)
            }
            ("pypi.org", ["project", name, after @ ..]) if at_version(after) => {
                (Registry::PyPI, (*name).to_owned())
            }
            _ => return None,
        };
        Self::new(registry, &name)
    }

    /// The join key. crates.io treats `-` and `_` and case as the same name; PyPI normalises
    /// names as in PEP 503; npm names are exact.
    pub fn key(&self) -> Key {
        match self.registry {
            Registry::Crates => {
                Key::new("crate", &self.name.to_ascii_lowercase().replace('_', "-"))
            }
            Registry::Npm => Key::new("npm", &self.name),
            Registry::PyPI => {
                let mut out = String::new();
                for c in self.name.chars() {
                    if "-_.".contains(c) {
                        if !out.ends_with('-') {
                            out.push('-');
                        }
                    } else {
                        out.push(c.to_ascii_lowercase());
                    }
                }
                Key::new("pypi", &out)
            }
        }
    }

    /// The package's pages, from the name alone.
    pub fn links(&self) -> Vec<Link> {
        let n = &self.name;
        let via = match self.registry {
            Registry::Crates => "crates",
            Registry::Npm => "npm",
            Registry::PyPI => "pypi",
        };
        let pages: Vec<(&str, String)> = match self.registry {
            Registry::Crates => vec![
                ("crates.io", format!("https://crates.io/crates/{n}")),
                ("Docs.rs", format!("https://docs.rs/{n}")),
                ("lib.rs", format!("https://lib.rs/crates/{n}")),
            ],
            Registry::Npm => vec![
                ("npm", format!("https://www.npmjs.com/package/{n}")),
                (
                    "jsDelivr",
                    format!("https://www.jsdelivr.com/package/npm/{n}"),
                ),
                ("unpkg", format!("https://unpkg.com/{n}/")),
            ],
            Registry::PyPI => vec![("PyPI", format!("https://pypi.org/project/{n}/"))],
        };
        pages
            .into_iter()
            .map(|(site, uri)| Link::new(site, uri, via))
            .collect()
    }
}

/// A source repository on a forge that build provenance can name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    host: &'static str,
    path: String,
}
impl Repo {
    /// `owner/name` (GitLab: `group/…/name`) on a known forge.
    pub fn new(host: &str, path: &str) -> Option<Self> {
        let host = match host.to_ascii_lowercase().as_str() {
            "github.com" | "github" => "github.com",
            "gitlab.com" | "gitlab" => "gitlab.com",
            _ => return None,
        };
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let parts: Vec<&str> = path.split('/').collect();
        let depth_ok = if host == "github.com" {
            // GitHub's own pages (`orgs/…`, `topics/…`) are not repositories: the GitHub
            // source's rule.
            parts.len() == 2
                && !crate::sources::github::RESERVED
                    .contains(&parts[0].to_ascii_lowercase().as_str())
        } else {
            (2..=10).contains(&parts.len())
        };
        (depth_ok
            && parts.iter().all(|p| {
                plain(p, 100, |c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                    // `-` starts GitLab's pages under a project (`group/project/-/issues/12`),
                    // and these did before it: such a page is not the repository.
                    && !matches!(
                        *p,
                        "." | ".."
                            | "-"
                            | "tree"
                            | "blob"
                            | "raw"
                            | "commits"
                            | "commit"
                            | "issues"
                            | "merge_requests"
                            | "wikis"
                            | "pipelines"
                            | "jobs"
                            | "tags"
                            | "branches"
                            | "releases"
                            | "snippets"
                            | "network"
                            | "graphs"
                            | "compare"
                            | "activity"
                            | "settings"
                    )
            }))
        .then(|| Self {
            host,
            path: path.to_owned(),
        })
    }
    /// A repository named by its address, e.g. `https://github.com/npm/node-semver`.
    pub fn from_url(value: &str) -> Option<Self> {
        let url = web_url(value)?;
        if url.scheme() != "https" || url.query().is_some() || url.fragment().is_some() {
            return None;
        }
        Self::new(url.host_str()?, url.path())
    }
    pub fn url(&self) -> String {
        format!("https://{}/{}", self.host, self.path)
    }
    /// Forges treat repository names without regard to case.
    pub fn key(&self) -> Key {
        Key::new(
            "repo",
            &format!("{}/{}", self.host, self.path.to_ascii_lowercase()),
        )
    }
    pub fn link(&self, via: &'static str) -> Link {
        let site = if self.host == "github.com" {
            "GitHub"
        } else {
            "GitLab"
        };
        Link::new(site, self.url(), via).kind("source")
    }
}

/// One release of a package and the repository its provenance names, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub package: Package,
    pub version: String,
    pub repository: Option<Repo>,
}
impl Release {
    /// Keys and links for the package card: the package's own key, and its repository as a
    /// link only. Several packages come from one repository (a workspace, a monorepo), so the
    /// repository is never a key that would join them.
    pub fn found(&self) -> Found {
        let mut found = Found::default();
        found.push_key(self.package.key());
        for link in self.package.links() {
            found.push_link(link);
        }
        if let Some(repo) = &self.repository {
            found.push_link(repo.link(match self.package.registry {
                Registry::Crates => "crates",
                Registry::Npm => "npm",
                Registry::PyPI => "pypi",
            }));
        }
        found
    }
}

fn version(v: &str) -> bool {
    plain(v, 100, |c| c.is_ascii_alphanumeric() || "-.+_!".contains(c))
}
/// A version as a page names it: it starts with a digit (`0.8.5`, `7.8.5`, `2.2.0`), never a
/// child path's word (`files`, `reviews`).
fn version_number(v: &str) -> bool {
    v.starts_with(|c: char| c.is_ascii_digit()) && version(v)
}

/// The crate and its default version (with its trusted-publishing record).
pub fn crates_request(package: &Package) -> Option<Request> {
    (package.registry == Registry::Crates).then_some(())?;
    Request::get(&format!(
        "https://crates.io/api/v1/crates/{}?include=default_version",
        package.name
    ))
}

/// The crate's default version, with the repository crates.io's trusted publishing recorded
/// for it.
pub fn parse_crates(status: u16, raw: &[u8], package: &Package) -> Result<Option<Release>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let response: Value = serde_json::from_slice(raw)?;
    let krate = &response["crate"];
    let name = krate["name"].as_str().context("no crate name")?;
    let found = Package::new(Registry::Crates, name).context("invalid crate name")?;
    anyhow::ensure!(
        found.key() == package.key(),
        "crates.io answered for another crate"
    );
    let default = krate["default_version"].as_str().filter(|v| version(v));
    let Some(default) = default else {
        return Ok(None);
    };
    let record = response["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|v| v["num"] == default && v["crate"] == name);
    let repository = record.and_then(|v| {
        let t = &v["trustpub_data"];
        Repo::new(t["provider"].as_str()?, t["repository"].as_str()?)
    });
    Ok(Some(Release {
        package: found,
        version: default.to_owned(),
        repository,
    }))
}

/// The latest npm release.
pub fn npm_latest_request(package: &Package) -> Option<Request> {
    (package.registry == Registry::Npm).then_some(())?;
    Request::get(&format!(
        "https://registry.npmjs.org/{}/latest",
        package.name
    ))
}

/// The latest npm release, and the address of its attestations when it has provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NpmLatest {
    pub version: String,
    pub attestations: Option<Request>,
}

pub fn parse_npm_latest(status: u16, raw: &[u8], package: &Package) -> Result<Option<NpmLatest>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let latest: Value = serde_json::from_slice(raw)?;
    anyhow::ensure!(
        latest["name"] == package.name.as_str(),
        "npm answered for another package"
    );
    let version = latest["version"]
        .as_str()
        .filter(|v| version(v))
        .context("no version")?;
    let attestations = latest["dist"]["attestations"]
        .as_object()
        .filter(|a| {
            a.get("provenance")
                .is_some_and(|p| p["predicateType"] == SLSA_V1)
        })
        .and_then(|a| a.get("url")?.as_str())
        .filter(|u| u.starts_with("https://registry.npmjs.org/-/npm/v1/attestations/"))
        .and_then(Request::get);
    Ok(Some(NpmLatest {
        version: version.to_owned(),
        attestations,
    }))
}

const SLSA_V1: &str = "https://slsa.dev/provenance/v1";

/// The repository npm's SLSA provenance names for this release. The subject must be this
/// package and version.
pub fn parse_npm_attestations(
    status: u16,
    raw: &[u8],
    package: &Package,
    version: &str,
) -> Result<Option<Repo>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let response: Value = serde_json::from_slice(raw)?;
    let purl = format!("pkg:npm/{}@{version}", package.name.replacen('@', "%40", 1));
    let mut repos = Vec::new();
    for attestation in response["attestations"].as_array().into_iter().flatten() {
        if attestation["predicateType"] != SLSA_V1 {
            continue;
        }
        let envelope = &attestation["bundle"]["dsseEnvelope"];
        if envelope["payloadType"] != "application/vnd.in-toto+json" {
            continue;
        }
        let payload = base64::engine::general_purpose::STANDARD
            .decode(envelope["payload"].as_str().unwrap_or_default())?;
        let statement: Value = serde_json::from_slice(&payload)?;
        let subject_ok = statement["subject"]
            .as_array()
            .is_some_and(|s| s.iter().any(|s| s["name"] == purl.as_str()));
        if !subject_ok || statement["predicateType"] != SLSA_V1 {
            continue;
        }
        let workflow = &statement["predicate"]["buildDefinition"]["externalParameters"]["workflow"];
        if let Some(repo) = workflow["repository"].as_str().and_then(Repo::from_url) {
            repos.push(repo);
        }
    }
    repos.dedup();
    Ok(match repos.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    })
}

/// The latest PyPI release and its files.
pub fn pypi_request(package: &Package) -> Option<Request> {
    (package.registry == Registry::PyPI).then_some(())?;
    Request::get(&format!("https://pypi.org/pypi/{}/json", package.name))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PypiLatest {
    pub package: Package,
    pub version: String,
    /// The release's files, wheels first.
    pub files: Vec<String>,
}

pub fn parse_pypi(status: u16, raw: &[u8], package: &Package) -> Result<Option<PypiLatest>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let response: Value = serde_json::from_slice(raw)?;
    let info = &response["info"];
    let name = info["name"].as_str().context("no name")?;
    let found = Package::new(Registry::PyPI, name).context("invalid name")?;
    anyhow::ensure!(
        found.key() == package.key(),
        "PyPI answered for another project"
    );
    let version = info["version"]
        .as_str()
        .filter(|v| version(v))
        .context("no version")?;
    let mut files: Vec<(bool, String)> = response["urls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| {
            let file = u["filename"].as_str()?;
            plain(file, 255, |c| {
                c.is_ascii_alphanumeric() || "-._+!".contains(c)
            })
            .then(|| (u["packagetype"] != "bdist_wheel", file.to_owned()))
        })
        .collect();
    files.sort();
    Ok(Some(PypiLatest {
        package: found,
        version: version.to_owned(),
        files: files.into_iter().map(|(_, f)| f).collect(),
    }))
}

/// The PEP 740 provenance of one file of a release.
pub fn pypi_provenance_request(latest: &PypiLatest, file: &str) -> Option<Request> {
    let mut r = Request::get(&format!(
        "https://pypi.org/integrity/{}/{}/{}/provenance",
        latest.package.name,
        encode(&latest.version),
        encode(file)
    ))?;
    r.accept = Some("application/vnd.pypi.integrity.v1+json");
    Some(r)
}

/// The repository the file's trusted publisher names, when every attestation agrees.
pub fn parse_pypi_provenance(status: u16, raw: &[u8]) -> Result<Option<Repo>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let response: Value = serde_json::from_slice(raw)?;
    let mut repos: Vec<Option<Repo>> = response["attestation_bundles"]
        .as_array()
        .context("no attestation bundles")?
        .iter()
        .map(|b| {
            let p = &b["publisher"];
            Repo::new(p["kind"].as_str()?, p["repository"].as_str()?)
        })
        .collect();
    repos.dedup();
    Ok(match repos.as_slice() {
        [Some(one)] => Some(one.clone()),
        _ => None,
    })
}

/// deps.dev's record of one release.
pub fn depsdev_request(package: &Package, version: &str) -> Option<Request> {
    Request::get(&format!(
        "https://api.deps.dev/v3/systems/{}/packages/{}/versions/{}",
        depsdev_system(package.registry),
        encode(&package.name),
        encode(version)
    ))
}
fn depsdev_system(registry: Registry) -> &'static str {
    match registry {
        Registry::Crates => "cargo",
        Registry::Npm => "npm",
        Registry::PyPI => "pypi",
    }
}

/// The source repository deps.dev links through a verified SLSA attestation, when there is
/// exactly one, from its answer for exactly this package and version.
pub fn parse_depsdev(
    status: u16,
    raw: &[u8],
    package: &Package,
    version: &str,
) -> Result<Option<Repo>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let response: Value = serde_json::from_slice(raw)?;
    let answered = &response["versionKey"];
    let same = answered["system"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(depsdev_system(package.registry)))
        && answered["name"].as_str().is_some_and(|name| {
            Package {
                registry: package.registry,
                name: name.to_owned(),
            }
            .key()
                == package.key()
        })
        && answered["version"] == version;
    anyhow::ensure!(same, "deps.dev answered for another package or version");
    let mut repos: Vec<Repo> = response["relatedProjects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| {
            p["relationProvenance"] == "SLSA_ATTESTATION" && p["relationType"] == "SOURCE_REPO"
        })
        .filter_map(|p| {
            let (host, path) = p["projectKey"]["id"].as_str()?.split_once('/')?;
            Repo::new(host, path)
        })
        .collect();
    repos.dedup();
    Ok(match repos.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    })
}

/// The repository the provenance of a release's files names, when they agree: files that name
/// two repositories name none.
pub fn one_repository(mut named: Vec<Repo>) -> Option<Repo> {
    named.sort_by_key(Repo::key);
    named.dedup();
    match named.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// Whether Software Heritage archives a repository.
pub fn software_heritage_request(repo: &Repo) -> Option<Request> {
    Request::get(&format!(
        "https://archive.softwareheritage.org/api/1/origin/{}/get/",
        repo.url()
    ))
}

/// Software Heritage's browse page for the repository, when it is archived.
pub fn parse_software_heritage(status: u16, raw: &[u8], repo: &Repo) -> Result<Option<Link>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let origin: Value = serde_json::from_slice(raw)?;
    Ok((origin["url"] == repo.url().as_str()).then(|| {
        Link::new(
            "Software Heritage",
            format!(
                "https://archive.softwareheritage.org/browse/origin/?origin_url={}",
                encode(&repo.url())
            ),
            "softwareheritage",
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture;

    fn pkg(registry: Registry, name: &str) -> Package {
        Package::new(registry, name).unwrap()
    }

    #[test]
    fn names_from_uris() {
        for (uri, registry, name) in [
            (
                "https://crates.io/crates/serde_json",
                Registry::Crates,
                "serde_json",
            ),
            (
                "https://docs.rs/rand/latest/rand/",
                Registry::Crates,
                "rand",
            ),
            (
                "https://docs.rs/crate/rand/0.10.3",
                Registry::Crates,
                "rand",
            ),
            ("https://lib.rs/crates/tokio", Registry::Crates, "tokio"),
            (
                "https://www.npmjs.com/package/semver",
                Registry::Npm,
                "semver",
            ),
            (
                "https://www.npmjs.com/package/@sigstore/sign",
                Registry::Npm,
                "@sigstore/sign",
            ),
            (
                "https://www.jsdelivr.com/package/npm/vite",
                Registry::Npm,
                "vite",
            ),
            (
                "https://pypi.org/project/urllib3/",
                Registry::PyPI,
                "urllib3",
            ),
        ] {
            assert_eq!(Package::from_uri(uri), Some(pkg(registry, name)), "{uri}");
        }
        for uri in [
            "https://docs.rs/about",
            "https://crates.io/search?q=x",
            "https://www.npmjs.com/package/",
            "https://www.npmjs.com/package/@scope",
            "https://pypi.org/project/-bad-/",
            "https://example.com/crates/rand",
        ] {
            assert_eq!(Package::from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn keys() {
        assert_eq!(
            pkg(Registry::Crates, "Serde_JSON").key().as_str(),
            "crate:serde-json"
        );
        assert_eq!(
            pkg(Registry::PyPI, "Zope.Interface__x").key().as_str(),
            "pypi:zope-interface-x"
        );
        assert_eq!(
            pkg(Registry::Npm, "@sigstore/sign").key().as_str(),
            "npm:@sigstore/sign"
        );
        let repo = Repo::from_url("https://github.com/NPM/node-semver.git").unwrap();
        assert_eq!(repo.url(), "https://github.com/NPM/node-semver");
        assert_eq!(repo.key().as_str(), "repo:github.com/npm/node-semver");
        for bad in [
            "https://github.com/npm",
            "https://github.com/a/b/c",
            "https://example.com/a/b",
            "http://github.com/a/b",
            "https://github.com/a/..",
        ] {
            assert_eq!(Repo::from_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn templates() {
        let links = pkg(Registry::Npm, "@sigstore/sign").links();
        assert_eq!(
            links.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>(),
            [
                "https://www.npmjs.com/package/@sigstore/sign",
                "https://www.jsdelivr.com/package/npm/@sigstore/sign",
                "https://unpkg.com/@sigstore/sign/"
            ]
        );
        assert_eq!(
            pkg(Registry::Crates, "rand").links()[1].uri,
            "https://docs.rs/rand"
        );
    }

    #[test]
    fn crates_trusted_publishing() {
        let rand = pkg(Registry::Crates, "rand");
        assert_eq!(
            crates_request(&rand).unwrap().url.as_str(),
            "https://crates.io/api/v1/crates/rand?include=default_version"
        );
        assert!(crates_request(&pkg(Registry::Npm, "rand")).is_none());
        let release = parse_crates(200, &fixture::read("crates-rand.json"), &rand)
            .unwrap()
            .unwrap();
        assert_eq!(release.version, "0.10.3");
        let found = release.found();
        assert_eq!(
            found.keys.iter().map(Key::as_str).collect::<Vec<_>>(),
            ["crate:rand"]
        );
        let repo = found.links.last().unwrap();
        assert_eq!(
            (repo.site.as_str(), repo.uri.as_str(), repo.kind),
            (
                "GitHub",
                "https://github.com/rust-random/rand",
                Some("source")
            )
        );
        // serde declares a repository but has no trusted-publishing record: no join.
        let serde = pkg(Registry::Crates, "serde");
        let release = parse_crates(200, &fixture::read("crates-serde.json"), &serde)
            .unwrap()
            .unwrap();
        assert_eq!(release.repository, None);
        assert_eq!(release.found().keys, [serde.key()]);
        assert_eq!(parse_crates(404, b"{}", &serde).unwrap(), None);
    }

    #[test]
    fn npm_provenance() {
        let semver = pkg(Registry::Npm, "semver");
        assert_eq!(
            npm_latest_request(&semver).unwrap().url.as_str(),
            "https://registry.npmjs.org/semver/latest"
        );
        let latest = parse_npm_latest(200, &fixture::read("npm-semver-latest.json"), &semver)
            .unwrap()
            .unwrap();
        assert_eq!(latest.version, "7.8.5");
        assert_eq!(
            latest.attestations.as_ref().unwrap().url.as_str(),
            "https://registry.npmjs.org/-/npm/v1/attestations/semver@7.8.5"
        );
        let attestations = fixture::read("npm-semver-attestations.json");
        let repo = parse_npm_attestations(200, &attestations, &semver, "7.8.5")
            .unwrap()
            .unwrap();
        assert_eq!(repo.url(), "https://github.com/npm/node-semver");
        // The statement's subject must be this release.
        assert_eq!(
            parse_npm_attestations(200, &attestations, &semver, "7.8.4").unwrap(),
            None
        );
        let other = pkg(Registry::Npm, "semver2");
        assert_eq!(
            parse_npm_attestations(200, &attestations, &other, "7.8.5").unwrap(),
            None
        );

        let left_pad = pkg(Registry::Npm, "left-pad");
        let latest = parse_npm_latest(200, &fixture::read("npm-left-pad-latest.json"), &left_pad)
            .unwrap()
            .unwrap();
        assert_eq!(latest.attestations, None);
        assert!(
            parse_npm_latest(200, &fixture::read("npm-left-pad-latest.json"), &semver).is_err()
        );
    }

    #[test]
    fn pypi_trusted_publisher() {
        let urllib3 = pkg(Registry::PyPI, "urllib3");
        let latest = parse_pypi(200, &fixture::read("pypi-urllib3.json.gz"), &urllib3)
            .unwrap()
            .unwrap();
        assert_eq!(latest.version, "2.8.0");
        assert_eq!(
            latest.files,
            ["urllib3-2.8.0-py3-none-any.whl", "urllib3-2.8.0.tar.gz"]
        );
        let r = pypi_provenance_request(&latest, &latest.files[0]).unwrap();
        assert_eq!(
            r.url.as_str(),
            "https://pypi.org/integrity/urllib3/2.8.0/urllib3-2.8.0-py3-none-any.whl/provenance"
        );
        assert_eq!(r.accept, Some("application/vnd.pypi.integrity.v1+json"));
        let repo = parse_pypi_provenance(200, &fixture::read("pypi-urllib3-provenance.json"))
            .unwrap()
            .unwrap();
        assert_eq!(repo.url(), "https://github.com/urllib3/urllib3");
        assert_eq!(
            parse_pypi_provenance(404, &fixture::read("pypi-six-provenance.json")).unwrap(),
            None
        );
    }

    #[test]
    fn deps_dev() {
        let semver = pkg(Registry::Npm, "semver");
        assert_eq!(
            depsdev_request(&semver, "7.8.5").unwrap().url.as_str(),
            "https://api.deps.dev/v3/systems/npm/packages/semver/versions/7.8.5"
        );
        assert!(
            depsdev_request(&pkg(Registry::Npm, "@sigstore/sign"), "1.0.0")
                .unwrap()
                .url
                .as_str()
                .contains("/packages/%40sigstore%2Fsign/")
        );
        let semver = fixture::read("depsdev-npm-semver.json");
        let npm = pkg(Registry::Npm, "semver");
        let repo = parse_depsdev(200, &semver, &npm, "7.8.5").unwrap().unwrap();
        assert_eq!(repo.url(), "https://github.com/npm/node-semver");
        // Only the answer for the package and version asked counts.
        assert!(parse_depsdev(200, &semver, &npm, "7.8.4").is_err());
        assert!(parse_depsdev(200, &semver, &pkg(Registry::Npm, "semver2"), "7.8.5").is_err());
        assert!(parse_depsdev(200, &semver, &pkg(Registry::PyPI, "semver"), "7.8.5").is_err());
        // rand's repository is unverified metadata there.
        assert_eq!(
            parse_depsdev(
                200,
                &fixture::read("depsdev-cargo-rand.json"),
                &pkg(Registry::Crates, "rand"),
                "0.10.3"
            )
            .unwrap(),
            None
        );
    }

    /// A registry or documentation page is the package only at the package's own page, or its
    /// page at one version; a function, a source file, a dependency list or any other child
    /// path names no package.
    #[test]
    fn a_page_under_a_package_names_no_package() {
        let key = |uri: &str| Package::from_uri(uri).map(|p| p.key().as_str().to_owned());
        for (uri, want) in [
            ("https://crates.io/crates/rand", Some("crate:rand")),
            ("https://crates.io/crates/rand/0.8.5", Some("crate:rand")),
            ("https://crates.io/crates/rand/versions", None),
            ("https://crates.io/crates/rand/0.8.5/dependencies", None),
            ("https://lib.rs/crates/rand", Some("crate:rand")),
            ("https://lib.rs/crates/rand/features", None),
            ("https://docs.rs/rand", Some("crate:rand")),
            ("https://docs.rs/rand/latest", Some("crate:rand")),
            ("https://docs.rs/rand/0.8.5", Some("crate:rand")),
            ("https://docs.rs/rand/latest/rand/", Some("crate:rand")),
            (
                "https://docs.rs/rand_core/latest/rand_core/",
                Some("crate:rand-core"),
            ),
            ("https://docs.rs/crate/rand/latest", Some("crate:rand")),
            ("https://docs.rs/rand/latest/rand/fn.random.html", None),
            ("https://docs.rs/rand/latest/src/rand/lib.rs.html", None),
            ("https://docs.rs/rand/latest/rand/rngs/index.html", None),
            ("https://docs.rs/crate/rand/latest/source/", None),
            ("https://docs.rs/crate/rand/latest/features", None),
            ("https://docs.rs/releases", None),
            ("https://www.npmjs.com/package/semver", Some("npm:semver")),
            (
                "https://www.npmjs.com/package/semver/v/7.8.5",
                Some("npm:semver"),
            ),
            (
                "https://www.npmjs.com/package/@babel/core",
                Some("npm:@babel/core"),
            ),
            (
                "https://www.npmjs.com/package/@babel/core/v/7.0.0",
                Some("npm:@babel/core"),
            ),
            (
                "https://www.npmjs.com/package/semver?activeTab=versions",
                Some("npm:semver"),
            ),
            ("https://www.npmjs.com/package/semver/v/latest", None),
            ("https://www.npmjs.com/package/semver/access", None),
            ("https://www.npmjs.com/package/@babel/core/access", None),
            (
                "https://www.jsdelivr.com/package/npm/semver",
                Some("npm:semver"),
            ),
            ("https://www.jsdelivr.com/package/npm/semver/files", None),
            ("https://pypi.org/project/urllib3/", Some("pypi:urllib3")),
            (
                "https://pypi.org/project/urllib3/2.2.0/",
                Some("pypi:urllib3"),
            ),
            ("https://pypi.org/project/urllib3/files/", None),
            ("https://pypi.org/project/urllib3/2.2.0/files/", None),
            ("https://pypi.org/user/urllib3/", None),
        ] {
            assert_eq!(key(uri).as_deref(), want, "{uri}");
        }
    }

    #[test]
    fn a_page_under_a_gitlab_project_is_not_its_repository() {
        for uri in [
            "https://github.com/orgs/rust-lang",
            "https://github.com/topics/rust",
            "https://gitlab.com/group/project/tree/master",
            "https://gitlab.com/group/project/issues",
            "https://gitlab.com/group/project/-/merge_requests/1",
        ] {
            assert_eq!(Repo::from_url(uri), None, "{uri}");
        }
        assert!(Repo::from_url("https://gitlab.com/inkscape/inkscape").is_some());
        assert!(Repo::from_url("https://github.com/rust-random/rand").is_some());
        assert_eq!(Repo::new("gitlab.com", "group/project/-/issues/12"), None);
        assert_eq!(Repo::new("gitlab.com", "group/sub/project/-"), None);
        assert!(Repo::new("gitlab.com", "group/sub/project").is_some());
        assert!(Repo::new("gitlab.com", "group/my-project").is_some());
    }

    #[test]
    fn files_that_name_two_repositories_name_none() {
        let repo = |path: &str| Repo::new("github.com", path).unwrap();
        assert_eq!(one_repository(vec![]), None);
        assert_eq!(
            one_repository(vec![repo("a/b"), repo("a/b")]),
            Some(repo("a/b"))
        );
        assert_eq!(one_repository(vec![repo("a/b"), repo("a/c")]), None);
    }

    #[test]
    fn software_heritage() {
        let repo = Repo::from_url("https://github.com/rust-random/rand").unwrap();
        assert_eq!(
            software_heritage_request(&repo).unwrap().url.as_str(),
            "https://archive.softwareheritage.org/api/1/origin/https://github.com/rust-random/rand/get/"
        );
        let link = parse_software_heritage(200, &fixture::read("swh-origin-rand.json"), &repo)
            .unwrap()
            .unwrap();
        assert_eq!(
            link.uri,
            "https://archive.softwareheritage.org/browse/origin/\
             ?origin_url=https%3A%2F%2Fgithub.com%2Frust-random%2Frand"
        );
        let missing = Repo::from_url("https://github.com/pubky/pinakes-no-such-repo").unwrap();
        let raw = fixture::read("swh-origin-missing.json");
        assert_eq!(parse_software_heritage(404, &raw, &missing).unwrap(), None);
        assert!(parse_software_heritage(429, b"", &repo).is_err());
    }

    /// Answers of the wrong shape are errors or absent provenance, never a panic.
    #[test]
    fn a_malformed_answer_never_panics() {
        let probe = pkg(Registry::Npm, "probe");
        for raw in [
            &br#"{"name":"probe","version":"1.0.0","dist":{"attestations":{}}}"#[..],
            br#"{"name":"probe","version":"1.0.0","dist":{"attestations":{"provenance":7}}}"#,
            br#"{"name":"probe","version":"1.0.0","dist":{"attestations":[]}}"#,
            br#"{"name":"probe","version":"1.0.0","dist":false}"#,
        ] {
            let latest = parse_npm_latest(200, raw, &probe).unwrap().unwrap();
            assert!(latest.attestations.is_none());
        }
        for raw in [&b"[]"[..], b"false", b"{}"] {
            assert!(parse_npm_latest(200, raw, &probe).is_err());
        }
    }
}
