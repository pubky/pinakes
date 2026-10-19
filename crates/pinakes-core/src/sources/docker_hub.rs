//! Container image repositories on Docker Hub, read from its v2 API by the namespace and name in
//! the link.
//!
//! Every Docker Hub link of one repository is an alias of `docker:{namespace}/{name}`. Official
//! images live in the `library` namespace, which the site writes as `/_/{name}`
//! (`docker:library/nginx`). The API allows 180 anonymous requests a minute.
use super::{
    kit::{fetch, host_and_segments, int, json, plain, text, time, Card, Request},
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;

pub const SITE: &str = "Docker Hub";

const LIBRARY: &str = "library";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub namespace: String,
    pub name: String,
}

/// A namespace or repository name, in lower case: letters, digits and `.`, `_` or `-`, starting
/// and ending with a letter or digit.
fn component(value: &str) -> Option<String> {
    let value = value.to_ascii_lowercase();
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    (plain(&value, 255, |c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
    }) && edge(value.chars().next())
        && edge(value.chars().last()))
    .then_some(value)
}

fn repo(namespace: &str, name: &str) -> Option<Repo> {
    Some(Repo {
        namespace: component(namespace)?,
        name: component(name)?,
    })
}

/// The repository a Docker Hub link names: `/_/{name}` (official images), `/r/{ns}/{name}`, the
/// owner's `/repository/docker/{ns}/{name}` (each with any tab), and the v2 API's repository URLs.
pub fn from_uri(uri: &str) -> Option<Repo> {
    let (host, segments) = host_and_segments(uri)?;
    if host != "hub.docker.com" {
        return None;
    }
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match s.as_slice() {
        ["_", name, ..] => repo(LIBRARY, name),
        ["r", ns, name, ..] | ["repository", "docker", ns, name, ..] => repo(ns, name),
        ["v2", "repositories", ns, name] | ["v2", "namespaces", ns, "repositories", name] => {
            repo(ns, name)
        }
        _ => None,
    }
}

pub fn identifier(r: &Repo) -> String {
    format!("docker:{}/{}", r.namespace, r.name)
}

/// The repository of a root (`docker:{namespace}/{name}`, already in lower case).
pub fn from_root(uri: &str) -> Option<Repo> {
    let (ns, name) = uri.strip_prefix("docker:")?.split_once('/')?;
    repo(ns, name).filter(|r| identifier(r) == uri)
}

/// The repository of a root or a Docker Hub link.
pub fn target(uri: &str) -> Option<Repo> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// The repository page: `/_/{name}` for an official image, else `/r/{ns}/{name}`.
pub fn page(r: &Repo) -> String {
    if r.namespace == LIBRARY {
        format!("https://hub.docker.com/_/{}", r.name)
    } else {
        format!("https://hub.docker.com/r/{}/{}", r.namespace, r.name)
    }
}

pub fn request(r: &Repo) -> Option<Request> {
    let r = repo(&r.namespace, &r.name)?;
    Request::get(&format!(
        "https://hub.docker.com/v2/repositories/{}/{}/",
        r.namespace, r.name
    ))
}

/// A repository, or `None` when Docker Hub has none by this name (404, "object not found"; a
/// private repository answers the same).
pub fn parse(status: u16, raw: &[u8], r: &Repo) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let field = |f: &str| v[f].as_str().and_then(component);
    ensure!(
        field("namespace").as_ref() == Some(&r.namespace)
            && field("name").as_ref() == Some(&r.name),
        "the API answered for another repository"
    );
    let official = r.namespace == LIBRARY;
    let mut card = Card::new("SoftwareApplication", identifier(r), page(r));
    card.name = Some(if official {
        r.name.clone()
    } else {
        format!("{}/{}", r.namespace, r.name)
    });
    card.description = text(&v["description"]);
    card.detail(
        "registered",
        time(&v["date_registered"]).map(|t| t.to_rfc3339()),
    );
    card.detail("pull_count", int(&v["pull_count"]));
    card.detail("star_count", int(&v["star_count"]));
    card.detail(
        "last_updated",
        time(&v["last_updated"]).map(|t| t.to_rfc3339()),
    );
    card.detail("official", official);
    card.detail("status", text(&v["status_description"]));
    card.detail("archived", v["archived"].as_bool());
    card.detail(
        "categories",
        v["categories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| text(&c["name"]))
            .collect::<Vec<_>>(),
    );
    Ok(Some(card))
}

/// The source: Docker Hub's v2 API.
pub struct DockerHub;

#[async_trait]
impl Source for DockerHub {
    fn name(&self) -> &'static str {
        "docker_hub"
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?);
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("docker:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "registered",
            "pull_count",
            "star_count",
            "last_updated",
            "official",
            "status",
            "archived",
            "categories",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("hub.docker.com", 3.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://hub.docker.com/_/nginx",
                card_type: "SoftwareApplication",
            },
            Example {
                uri: "https://hub.docker.com/r/btcpayserver/btcpayserver",
                card_type: "SoftwareApplication",
            },
            Example {
                uri: "https://hub.docker.com/r/lncm/bitcoind/tags",
                card_type: "SoftwareApplication",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(repo) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&repo).context("no request")?).await?;
        Ok(match parse(f.status, &f.body, &repo)? {
            Some(card) => card.into_doc(cx.card(uri, self), SITE),
            None => cx.missing(uri, self),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("docker_hub", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).as_ref().map(identifier)
    }
    fn lncm() -> Repo {
        repo("lncm", "bitcoind").unwrap()
    }

    #[test]
    fn every_link_form_of_one_repository() {
        for uri in [
            "https://hub.docker.com/_/nginx",
            "https://hub.docker.com/_/nginx/tags?name=stable",
            "https://hub.docker.com/r/library/nginx",
            "https://hub.docker.com/r/library/NGINX/",
            "https://hub.docker.com/v2/repositories/library/nginx/",
            "https://hub.docker.com/v2/namespaces/library/repositories/nginx",
        ] {
            assert_eq!(id(uri).as_deref(), Some("docker:library/nginx"), "{uri}");
        }
        for uri in [
            "https://hub.docker.com/r/lncm/bitcoind",
            "https://hub.docker.com/r/lncm/bitcoind/tags",
            "http://hub.docker.com/r/LNCM/bitcoind#readme",
            "https://hub.docker.com/repository/docker/lncm/bitcoind/general",
            "https://hub.docker.com/v2/repositories/lncm/bitcoind",
        ] {
            assert_eq!(id(uri).as_deref(), Some("docker:lncm/bitcoind"), "{uri}");
        }
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://hub.docker.com/_/",
            "https://hub.docker.com/r/lncm",
            "https://hub.docker.com/u/lncm",
            "https://hub.docker.com/r/-lncm/bitcoind",
            "https://hub.docker.com/r/lncm/bit%20coind",
            "https://hub.docker.com/layers/library/nginx/latest/images/sha256-abc",
            "https://hub.docker.com/v2/repositories/nginx/",
            "https://hub.docker.com/v2/repositories/library/nginx/tags",
            "https://hub.docker.com/search?q=nginx",
            "https://ghcr.io/lncm/bitcoind",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn pages_and_requests() {
        let nginx = from_uri("https://hub.docker.com/_/nginx").unwrap();
        assert_eq!(page(&nginx), "https://hub.docker.com/_/nginx");
        assert_eq!(page(&lncm()), "https://hub.docker.com/r/lncm/bitcoind");
        assert_eq!(
            request(&nginx).unwrap().url.as_str(),
            "https://hub.docker.com/v2/repositories/library/nginx/"
        );
    }

    #[test]
    fn official_image() {
        let nginx = repo(LIBRARY, "nginx").unwrap();
        let card = parse(200, &read("repo-library-nginx.json"), &nginx)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "SoftwareApplication");
        assert_eq!(card.id, "docker:library/nginx");
        assert_eq!(card.url, "https://hub.docker.com/_/nginx");
        assert_eq!(card.name.as_deref(), Some("nginx"));
        assert_eq!(
            card.description.as_deref(),
            Some("Official build of Nginx.")
        );
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["registered"], "2014-06-05T19:14:45+00:00");
        assert_eq!(card.details["pull_count"], 13485840765i64);
        assert_eq!(card.details["star_count"], 21412);
        assert_eq!(card.details["official"], true);
        assert_eq!(card.details["status"], "active");
        assert_eq!(card.details["categories"][0], "Web servers");
        assert_eq!(
            card.details["last_updated"],
            "2026-10-19T01:52:21.514629+00:00"
        );
    }

    #[test]
    fn community_images() {
        let card = parse(200, &read("repo-lncm-bitcoind.json"), &lncm())
            .unwrap()
            .unwrap();
        assert_eq!(card.name.as_deref(), Some("lncm/bitcoind"));
        assert_eq!(
            card.description.as_deref(),
            Some("Flexible multi-arch (amd & amd64) Bitcoin Core Docker image")
        );
        assert_eq!(card.details["official"], false);
        let btcpay = repo("btcpayserver", "btcpayserver").unwrap();
        let card = parse(200, &read("repo-btcpayserver-btcpayserver.json"), &btcpay)
            .unwrap()
            .unwrap();
        assert_eq!(
            card.url,
            "https://hub.docker.com/r/btcpayserver/btcpayserver"
        );
        assert_eq!(card.details["pull_count"], 4117458);
        assert_eq!(card.details["categories"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn roots_and_aliases() {
        let d = DockerHub;
        for uri in ["docker:library/nginx", "docker:lncm/bitcoind"] {
            assert!(d.reads(uri), "{uri}");
            assert_eq!(d.alias_root(uri), None, "{uri}");
            assert!(!d.invalid(uri), "{uri}");
        }
        assert_eq!(
            d.alias_root("https://hub.docker.com/_/nginx").as_deref(),
            Some("docker:library/nginx")
        );
        assert_eq!(
            d.alias_root("https://hub.docker.com/r/LNCM/bitcoind/tags")
                .as_deref(),
            Some("docker:lncm/bitcoind")
        );
        for uri in [
            "docker:nginx",
            "docker:LNCM/bitcoind",
            "docker:a/b/c",
            "docker:-a/b",
        ] {
            assert!(d.invalid(uri), "{uri}");
            assert!(!d.reads(uri), "{uri}");
        }
        assert!(!d.invalid("https://example.org/docker:x"));
        for example in d.examples() {
            assert!(d.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, r) in [
            ("repo-library-nginx.json", repo(LIBRARY, "nginx").unwrap()),
            (
                "repo-btcpayserver-btcpayserver.json",
                repo("btcpayserver", "btcpayserver").unwrap(),
            ),
            ("repo-lncm-bitcoind.json", lncm()),
        ] {
            let card = parse(200, &read(file), &r).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&DockerHub, &card);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
            assert_eq!(doc.attributes["page_url"], card.url);
        }
    }

    #[test]
    fn missing_mismatched_and_failed() {
        let missing = repo("btcpayserver", "no-such-repo-pinakes").unwrap();
        assert_eq!(
            parse(404, &read("repo-missing.json"), &missing).unwrap(),
            None
        );
        assert!(parse(200, &read("repo-lncm-bitcoind.json"), &missing).is_err());
        let nginx = repo("lncm", "nginx").unwrap();
        assert!(parse(200, &read("repo-library-nginx.json"), &nginx).is_err());
        assert!(parse(429, b"", &lncm()).is_err());
        assert!(parse(500, b"", &lncm()).is_err());
    }
}
