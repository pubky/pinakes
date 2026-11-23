//! Models, datasets and Spaces on the Hugging Face Hub, read from the Hub API by the repo id in
//! the link.
//!
//! Every link of one repo on huggingface.co or hf.co (its page and its tabs on the default
//! branch: files, commits, discussions, the dataset viewer) and its API URL are aliases of one
//! identifier:
//! `hf:model/{owner}/{name}`, `hf:dataset/{owner}/{name}` or `hf:space/{owner}/{name}`, in lower
//! case, because the Hub ignores case in repo ids (it redirects any other case to the canonical
//! one, which the card shows).
//!
//! The API redirects an old name of a renamed repo to its new one; the redirect is what proves
//! the rename, so the card's root is the repo the final API URL names (`moved_to`), and the old
//! links become its aliases.
//!
//! A name without an owner (`huggingface.co/gpt2`, `huggingface.co/datasets/gsm8k`) is either a
//! legacy repo id, which the API redirects to the namespaced repo it moved to, or a user's or
//! organisation's profile page; the link alone can't tell which, so the source does not read
//! these links (they get ordinary web cards).
use super::{
    kit::{fetch, host_and_segments, int, json, plain, text, texts, time, Card, Key, Request},
    Context, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub const SITE: &str = "Hugging Face";
/// The prefix of the DOIs the Hub mints for repos.
const HUB_DOI: &str = "10.57967/hf/";

/// What kind of repo a link names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Model,
    Dataset,
    Space,
}
impl Kind {
    fn api(self) -> &'static str {
        match self {
            Self::Model => "models",
            Self::Dataset => "datasets",
            Self::Space => "spaces",
        }
    }
    /// The path prefix of the repo's page.
    fn prefix(self) -> &'static str {
        match self {
            Self::Model => "",
            Self::Dataset => "datasets/",
            Self::Space => "spaces/",
        }
    }
    fn namespace(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Dataset => "dataset",
            Self::Space => "space",
        }
    }
    fn type_name(self) -> &'static str {
        match self {
            Self::Model => "SoftwareSourceCode",
            Self::Dataset => "Dataset",
            Self::Space => "WebApplication",
        }
    }
}

/// A repo, by its id as written: `{owner}/{name}`, or a legacy `{name}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub kind: Kind,
    pub id: String,
}
impl Repo {
    /// A legacy id without an owner, which only a redirect resolves.
    pub fn legacy(&self) -> bool {
        !self.id.contains('/')
    }
}

/// First path segments of huggingface.co that are site sections, not owners.
const RESERVED: &[&str] = &[
    "api",
    "autotrain",
    "avatars",
    "billing",
    "blog",
    "brand",
    "changelog",
    "chat",
    "code-of-conduct",
    "collections",
    "competitions",
    "content-guidelines",
    "datasets",
    "deploy",
    "docs",
    "enterprise",
    "front",
    "hardware",
    "huggingchat",
    "inference",
    "inference-api",
    "inference-endpoints",
    "jobs",
    "join",
    "languages",
    "leaderboards",
    "learn",
    "login",
    "logout",
    "mcp",
    "models",
    "new",
    "new-collection",
    "new-dataset",
    "new-space",
    "notifications",
    "oauth",
    "organizations",
    "papers",
    "posts",
    "pricing",
    "privacy",
    "pro",
    "search",
    "settings",
    "spaces",
    "static-proxy",
    "storage",
    "subscribe",
    "support",
    "tasks",
    "terms-of-service",
    "welcome",
];

/// The repo's own tabs, which show the repo itself: its files and history on the default branch,
/// its discussions and the dataset viewer. Anything deeper (a file, another revision, one commit,
/// one discussion, a split) is another object, not an alias.
const TABS: &[&[&str]] = &[
    &["tree", "main"],
    &["commits", "main"],
    &["discussions"],
    &["viewer"],
];

/// One part of a repo id: letters, digits, `-`, `_` and `.`, not starting or ending with `-` or
/// `.`, without `--` or `..` (the Hub's own rule).
fn part(s: &str) -> bool {
    plain(s, 96, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
    }) && !s.starts_with(['-', '.'])
        && !s.ends_with(['-', '.'])
        && !s.contains("--")
        && !s.contains("..")
}

fn repo(kind: Kind, owner: Option<&str>, name: &str) -> Option<Repo> {
    let id = match owner {
        Some(owner) => format!("{owner}/{name}"),
        None if kind == Kind::Space => return None,
        None => name.to_owned(),
    };
    (owner.is_none_or(part) && part(name) && !name.ends_with(".git") && id.len() <= 96)
        .then_some(Repo { kind, id })
}

/// A repo page or one of its tabs, after the kind's prefix.
fn page_path(kind: Kind, s: &[&str]) -> Option<Repo> {
    match s {
        [name] => repo(kind, None, name),
        // A legacy name followed by a tab, e.g. `bert-base-uncased/tree/main`.
        [name, tab @ ..] if TABS.contains(&tab) => repo(kind, None, name),
        [owner, name] => repo(kind, Some(owner), name),
        [owner, name, tab @ ..] if TABS.contains(&tab) => repo(kind, Some(owner), name),
        _ => None,
    }
}

/// What a huggingface.co or hf.co link names: a repo page or subpage, or its API record.
pub fn from_uri(uri: &str) -> Option<Repo> {
    let (host, segments) = host_and_segments(uri)?;
    if host != "huggingface.co" && host != "hf.co" {
        return None;
    }
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match s.as_slice() {
        ["api", kind, rest @ ..] => {
            let kind = match *kind {
                "models" => Kind::Model,
                "datasets" => Kind::Dataset,
                "spaces" => Kind::Space,
                _ => return None,
            };
            match rest {
                [name] => repo(kind, None, name),
                [owner, name] => repo(kind, Some(owner), name),
                _ => None,
            }
        }
        ["datasets", rest @ ..] => page_path(Kind::Dataset, rest),
        ["spaces", rest @ ..] => page_path(Kind::Space, rest),
        [first, ..] if RESERVED.contains(&first.to_ascii_lowercase().as_str()) => None,
        rest => page_path(Kind::Model, rest),
    }
}

/// The identifier (root) every link form of the repo is an alias of, or `None` for a legacy id.
pub fn identifier(repo: &Repo) -> Option<String> {
    (!repo.legacy()).then(|| {
        format!(
            "hf:{}/{}",
            repo.kind.namespace(),
            repo.id.to_ascii_lowercase()
        )
    })
}

/// The repo's page on huggingface.co.
pub fn page(repo: &Repo) -> String {
    format!("https://huggingface.co/{}{}", repo.kind.prefix(), repo.id)
}

/// The repo's API record. Egress follows the API's redirect of another case or an old name.
pub fn request(repo: &Repo) -> Option<Request> {
    Request::get(&format!(
        "https://huggingface.co/api/{}/{}",
        repo.kind.api(),
        repo.id
    ))
}

/// The repo the final URL of an API request (after Egress followed the Hub's redirects) names,
/// when it is an API URL of a namespaced repo of the same kind: the canonical case of the id, or
/// the repo an old or legacy name now belongs to.
pub fn moved_to(final_url: &str, repo: &Repo) -> Option<Repo> {
    let path = url::Url::parse(final_url).ok()?.path().to_owned();
    let to = from_uri(final_url)?;
    (to.kind == repo.kind && !to.legacy() && path.starts_with("/api/")).then_some(to)
}

/// The repo of a root (`hf:model/{owner}/{name}`, in lower case).
pub fn from_root(uri: &str) -> Option<Repo> {
    let (kind, id) = uri.strip_prefix("hf:")?.split_once('/')?;
    let kind = match kind {
        "model" => Kind::Model,
        "dataset" => Kind::Dataset,
        "space" => Kind::Space,
        _ => return None,
    };
    let (owner, name) = id.split_once('/')?;
    let r = repo(kind, Some(owner), name)?;
    (r.id == r.id.to_ascii_lowercase()).then_some(r)
}

/// The namespaced repo of a root or a Hugging Face link (never a legacy name).
pub fn target(uri: &str) -> Option<Repo> {
    from_root(uri).or_else(|| from_uri(uri).filter(|r| !r.legacy()))
}

/// One `license` value, or all of them when the repo names several.
fn licence(v: &Value, tags: &[String]) -> Value {
    let mut ids: Vec<String> = tags
        .iter()
        .filter_map(|t| t.strip_prefix("license:"))
        .map(str::to_owned)
        .collect();
    if ids.is_empty() {
        let card = &v["cardData"]["license"];
        ids = text(card).map(|l| vec![l]).unwrap_or_else(|| texts(card));
    }
    match ids.len() {
        0 => Value::Null,
        1 => ids.remove(0).into(),
        _ => ids.into(),
    }
}

/// The text the Hub extracts from a dataset card, in one line, without the trailing pointer to
/// the full card.
fn summary(v: &Value) -> Option<String> {
    let s = text(v)?;
    let s = s
        .split_once("See the full description on the dataset page:")
        .map_or(s.as_str(), |(head, _)| head);
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!s.is_empty()).then_some(s)
}

/// A repo, or `None` when the Hub has no public repo with this id. To a request without a token
/// the Hub answers 401 for a missing repo and for a private one alike; both mean there is no
/// public repo, so both read as missing.
pub fn parse(status: u16, raw: &[u8], repo: &Repo) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[401, 404])? else {
        return Ok(None);
    };
    let answered = Repo {
        kind: repo.kind,
        id: text(&v["id"]).unwrap_or_default(),
    };
    ensure!(
        answered.id.eq_ignore_ascii_case(&repo.id) && !answered.legacy(),
        "the API answered for another repo"
    );
    let id = identifier(&answered).expect("a namespaced repo");
    let mut card = Card::new(repo.kind.type_name(), id, page(&answered));
    card.name = Some(answered.id);
    card.description = match repo.kind {
        Kind::Dataset => summary(&v["description"]),
        Kind::Space => text(&v["cardData"]["short_description"]),
        Kind::Model => None,
    };
    card.detail("created", time(&v["createdAt"]).map(|t| t.to_rfc3339()));
    let tags = texts(&v["tags"]);
    card.detail("author", text(&v["author"]));
    card.detail(
        "title",
        text(&v["cardData"]["title"]).or_else(|| text(&v["cardData"]["pretty_name"])),
    );
    card.detail("downloads", int(&v["downloads"]));
    card.detail("likes", int(&v["likes"]));
    card.detail("pipeline_tag", text(&v["pipeline_tag"]));
    card.detail("library_name", text(&v["library_name"]));
    card.detail("license", licence(&v, &tags));
    card.detail("gated", v["gated"].clone());
    card.detail("sdk", text(&v["sdk"]));
    card.detail("app_url", text(&v["host"]));
    card.detail(
        "last_modified",
        time(&v["lastModified"]).map(|t| t.to_rfc3339()),
    );
    let arxiv: Vec<&str> = tags
        .iter()
        .filter_map(|t| t.strip_prefix("arxiv:"))
        .collect();
    card.detail("arxiv", arxiv);
    // The Hub mints DOIs for repos under `HUB_DOI`; any other `doi:` tag names a paper.
    let mut papers = vec![];
    for doi in tags.iter().filter_map(|t| t.strip_prefix("doi:")) {
        let doi = doi.to_ascii_lowercase();
        if doi.starts_with(HUB_DOI) {
            card.key(Key::new("doi", &doi));
        } else {
            papers.push(doi);
        }
    }
    card.detail("paper_doi", papers);
    Ok(Some(card))
}

/// The source: the Hugging Face Hub API.
pub struct HuggingFace;

#[async_trait]
impl Source for HuggingFace {
    fn name(&self) -> &'static str {
        "huggingface"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "huggingface",
                uri: "https://huggingface.co/datasets/openai/gsm8k",
                card_type: "Dataset",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "huggingface.co",
                path: "/api/datasets/openai/gsm8k",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "dataset-gsm8k.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?)?;
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("hf:") && from_root(uri).is_none()
    }
    /// The DOIs the Hub mints for a repo. A paper's DOI stays the fact `paper_doi`.
    fn own_ids(&self) -> &'static [super::kit::OwnId] {
        &[super::kit::OwnId::Doi(HUB_DOI)]
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "created",
            "doi",
            "author",
            "title",
            "downloads",
            "likes",
            "pipeline_tag",
            "library_name",
            "license",
            "gated",
            "sdk",
            "app_url",
            "last_modified",
            "arxiv",
            "paper_doi",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("huggingface.co", 1.6)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://huggingface.co/meta-llama/Llama-2-7b-hf",
                card_type: "SoftwareSourceCode",
            },
            Example {
                uri: "https://huggingface.co/datasets/openai/gsm8k",
                card_type: "Dataset",
            },
            Example {
                uri: "https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat",
                card_type: "WebApplication",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(repo) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&repo).context("repo id")?).await?;
        let repo = moved_to(f.url.as_str(), &repo).unwrap_or(repo);
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
        fixture::read("huggingface", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).as_ref().and_then(identifier)
    }
    fn repo(kind: Kind, id: &str) -> Repo {
        Repo {
            kind,
            id: id.into(),
        }
    }

    #[test]
    fn every_link_form_of_one_repo() {
        for uri in [
            "https://huggingface.co/meta-llama/Llama-2-7b-hf",
            "https://www.huggingface.co/meta-llama/Llama-2-7b-hf",
            "https://hf.co/meta-llama/Llama-2-7b-hf",
            "https://huggingface.co/META-LLAMA/llama-2-7b-hf",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/tree/main",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/discussions",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/commits/main",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf#model-details",
            "https://huggingface.co/api/models/meta-llama/Llama-2-7b-hf",
        ] {
            assert_eq!(
                id(uri).as_deref(),
                Some("hf:model/meta-llama/llama-2-7b-hf"),
                "{uri}"
            );
        }
        for uri in [
            "https://huggingface.co/datasets/openai/gsm8k",
            "https://huggingface.co/datasets/openai/gsm8k/viewer",
            "https://hf.co/datasets/openai/gsm8k/tree/main",
            "https://huggingface.co/api/datasets/openai/gsm8k",
        ] {
            assert_eq!(id(uri).as_deref(), Some("hf:dataset/openai/gsm8k"), "{uri}");
        }
        for uri in [
            "https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat",
            "https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat/tree/main",
            "https://huggingface.co/api/spaces/huggingface-projects/llama-2-7b-chat",
        ] {
            assert_eq!(
                id(uri).as_deref(),
                Some("hf:space/huggingface-projects/llama-2-7b-chat"),
                "{uri}"
            );
        }
        // Files, other revisions, single commits and discussions, and dataset splits are other
        // objects.
        for uri in [
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/blob/main/config.json",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/resolve/main/README.md?download=true",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/raw/main/config.json",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/tree/v1.0",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/tree/main/subdir",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/commit/01c7f73d771dfac7d292323805ebc428287df4f9",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/commits/v1.0",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/discussions/12",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/edit/main/README.md",
            "https://huggingface.co/datasets/openai/gsm8k/viewer/main/train",
            "https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat/blob/main/app.py",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
            assert!(!HuggingFace.reads(uri), "{uri}");
        }
        // A site section is no owner, in any letter case.
        for uri in [
            "https://huggingface.co/docs/hub",
            "https://huggingface.co/Docs/hub",
            "https://huggingface.co/BLOG/gemma",
        ] {
            assert!(!HuggingFace.reads(uri), "{uri}");
        }
        let r = from_uri("https://hf.co/meta-llama/Llama-2-7b-hf/tree/main").unwrap();
        assert_eq!(page(&r), "https://huggingface.co/meta-llama/Llama-2-7b-hf");
        let req = request(&r).unwrap();
        assert_eq!(
            req.url.as_str(),
            "https://huggingface.co/api/models/meta-llama/Llama-2-7b-hf"
        );
        assert_eq!(
            request(&repo(Kind::Space, "a/b")).unwrap().url.as_str(),
            "https://huggingface.co/api/spaces/a/b"
        );
    }

    #[test]
    fn legacy_ids_are_not_read() {
        let h = HuggingFace;
        for (uri, kind, name) in [
            ("https://huggingface.co/gpt2", Kind::Model, "gpt2"),
            (
                "https://huggingface.co/bert-base-uncased/tree/main",
                Kind::Model,
                "bert-base-uncased",
            ),
            ("https://hf.co/api/models/gpt2", Kind::Model, "gpt2"),
            (
                "https://huggingface.co/datasets/gsm8k",
                Kind::Dataset,
                "gsm8k",
            ),
            (
                "https://huggingface.co/meta-llama",
                Kind::Model,
                "meta-llama",
            ),
        ] {
            let r = from_uri(uri).unwrap();
            assert_eq!(r, repo(kind, name), "{uri}");
            assert!(r.legacy());
            assert_eq!(identifier(&r), None);
            assert!(!h.reads(uri), "{uri}");
            assert_eq!(h.alias_root(uri), None, "{uri}");
        }
    }

    #[test]
    fn redirects_name_the_repo() {
        // The recorded 307s (see provenance.json): Egress follows them, and the final URL names
        // the repo.
        let gpt2 = repo(Kind::Model, "gpt2");
        assert_eq!(
            moved_to(
                "https://huggingface.co/api/models/openai-community/gpt2",
                &gpt2
            ),
            Some(repo(Kind::Model, "openai-community/gpt2"))
        );
        let gsm8k = repo(Kind::Dataset, "gsm8k");
        let to = moved_to("https://huggingface.co/api/datasets/openai/gsm8k", &gsm8k).unwrap();
        assert_eq!(identifier(&to).as_deref(), Some("hf:dataset/openai/gsm8k"));
        // A different case redirects to the canonical one: the same identifier.
        let upper = repo(Kind::Model, "META-LLAMA/llama-2-7b-hf");
        let to = moved_to(
            "https://huggingface.co/api/models/meta-llama/Llama-2-7b-hf",
            &upper,
        )
        .unwrap();
        assert_eq!(to.id, "meta-llama/Llama-2-7b-hf");
        assert_eq!(identifier(&to), identifier(&upper));
        let card = parse(200, &read("model-llama-2-7b-hf.json"), &to)
            .unwrap()
            .unwrap();
        assert_eq!(card.id, "hf:model/meta-llama/llama-2-7b-hf");
        // A renamed repo: the answer for the new name is that repo's card.
        let old = repo(Kind::Model, "someone/gpt2");
        let to = moved_to(
            "https://huggingface.co/api/models/openai-community/gpt2",
            &old,
        )
        .unwrap();
        let card = parse(200, &read("model-gpt2.json"), &to).unwrap().unwrap();
        assert_eq!(card.id, "hf:model/openai-community/gpt2");
        assert!(parse(200, &read("model-gpt2.json"), &old).is_err());
        // Another kind, a legacy name, or a page.
        assert_eq!(
            moved_to("https://huggingface.co/api/datasets/a/b", &gpt2),
            None
        );
        assert_eq!(
            moved_to("https://huggingface.co/api/models/gpt3", &gpt2),
            None
        );
        assert_eq!(moved_to("https://huggingface.co/a/b", &gpt2), None);
    }

    #[test]
    fn roots_and_aliases() {
        let h = HuggingFace;
        for uri in [
            "hf:model/meta-llama/llama-2-7b-hf",
            "hf:dataset/openai/gsm8k",
            "hf:space/huggingface-projects/llama-2-7b-chat",
        ] {
            assert!(h.reads(uri), "{uri}");
            assert_eq!(h.alias_root(uri), None, "{uri}");
            assert!(!h.invalid(uri), "{uri}");
            assert_eq!(
                target(uri).as_ref().and_then(identifier).as_deref(),
                Some(uri)
            );
        }
        assert_eq!(
            h.alias_root("https://hf.co/META-LLAMA/Llama-2-7b-hf/tree/main")
                .as_deref(),
            Some("hf:model/meta-llama/llama-2-7b-hf")
        );
        assert_eq!(
            request(&target("hf:dataset/openai/gsm8k").unwrap())
                .unwrap()
                .url
                .as_str(),
            "https://huggingface.co/api/datasets/openai/gsm8k"
        );
        for uri in [
            "hf:model/Meta-Llama/Llama-2-7b-hf",
            "hf:model/gpt2",
            "hf:space/a",
            "hf:org/a/b",
            "hf:model/a/b/c",
            "hf:model/a--b/c",
        ] {
            assert!(h.invalid(uri), "{uri}");
            assert!(!h.reads(uri), "{uri}");
        }
        assert!(!h.invalid("https://huggingface.co/hf:model/x"));
        for example in h.examples() {
            assert!(h.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, r) in [
            (
                "model-llama-2-7b-hf.json",
                repo(Kind::Model, "meta-llama/Llama-2-7b-hf"),
            ),
            (
                "model-gpt2.json",
                repo(Kind::Model, "openai-community/gpt2"),
            ),
            ("dataset-gsm8k.json", repo(Kind::Dataset, "openai/gsm8k")),
            (
                "space-llama-2-7b-chat.json",
                repo(Kind::Space, "huggingface-projects/llama-2-7b-chat"),
            ),
        ] {
            let card = parse(200, &read(file), &r).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&HuggingFace, &card);
            assert_eq!(doc.attributes["page_url"], card.url, "{file}");
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let card = parse(
            200,
            &read("model-gpt2.json"),
            &repo(Kind::Model, "openai-community/gpt2"),
        )
        .unwrap()
        .unwrap();
        let doc = crate::sources::kit::checked(&HuggingFace, &card);
        assert_eq!(doc.attributes["doi"], "10.57967/hf/0039");
        assert_eq!(doc.attributes["gated"], "false");
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://huggingface.co/",
            "https://huggingface.co/models",
            "https://huggingface.co/models?search=llama",
            "https://huggingface.co/docs/transformers",
            "https://huggingface.co/docs/transformers/index",
            "https://huggingface.co/blog/llama2",
            "https://huggingface.co/papers/2307.09288",
            "https://huggingface.co/learn/nlp-course",
            "https://huggingface.co/organizations/meta-llama",
            "https://huggingface.co/settings/tokens",
            "https://huggingface.co/tasks/text-generation",
            "https://huggingface.co/collections/meta-llama/llama-2-family",
            "https://huggingface.co/posts/user/123",
            "https://huggingface.co/join",
            "https://huggingface.co/pricing",
            "https://huggingface.co/spaces",
            "https://huggingface.co/spaces/gradio",
            "https://huggingface.co/datasets",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/unknown",
            "https://huggingface.co/a/b/c",
            "https://huggingface.co/meta--llama/x",
            "https://huggingface.co/-x/y",
            "https://huggingface.co/x/y.",
            "https://huggingface.co/x/y.git",
            "https://huggingface.co/x/y%20z",
            "https://huggingface.co/api/models/a/b/tree/main",
            "https://huggingface.co/api/whoami-v2",
            "https://huggingface.co/api/spaces/gradio",
            "https://huggingface-projects-llama-2-7b-chat.hf.space/",
            "https://example.org/meta-llama/Llama-2-7b-hf",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        let long = format!(
            "https://huggingface.co/{}/{}",
            "a".repeat(50),
            "b".repeat(50)
        );
        assert_eq!(from_uri(&long), None);
    }

    #[test]
    fn gated_model() {
        let r = repo(Kind::Model, "meta-llama/Llama-2-7b-hf");
        let card = parse(200, &read("model-llama-2-7b-hf.json"), &r)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "SoftwareSourceCode");
        assert_eq!(card.id, "hf:model/meta-llama/llama-2-7b-hf");
        assert_eq!(card.url, "https://huggingface.co/meta-llama/Llama-2-7b-hf");
        assert_eq!(card.name.as_deref(), Some("meta-llama/Llama-2-7b-hf"));
        assert_eq!(card.description, None);
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["created"], "2023-07-13T16:16:13+00:00");
        assert_eq!(card.details["author"], "meta-llama");
        assert_eq!(card.details["gated"], "manual");
        assert_eq!(card.details["license"], "llama2");
        assert_eq!(card.details["pipeline_tag"], "text-generation");
        assert_eq!(card.details["library_name"], "transformers");
        assert_eq!(card.details["downloads"], 596457);
        assert_eq!(card.details["likes"], 2469);
        assert_eq!(card.details["arxiv"], serde_json::json!(["2307.09288"]));
        assert_eq!(card.details["last_modified"], "2024-04-17T08:40:16+00:00");
        assert!(card.keys.is_empty(), "a paper is not the model");
        // Another case of the same id is the same repo; another id is not.
        let upper = repo(Kind::Model, "META-LLAMA/llama-2-7b-hf");
        let same = parse(200, &read("model-llama-2-7b-hf.json"), &upper)
            .unwrap()
            .unwrap();
        assert_eq!(same.id, card.id);
        assert_eq!(same.url, card.url);
        let other = repo(Kind::Model, "meta-llama/Llama-2-13b-hf");
        assert!(parse(200, &read("model-llama-2-7b-hf.json"), &other).is_err());
        assert!(parse(429, b"", &r).is_err());
        assert!(parse(500, b"", &r).is_err());
    }

    /// The DOI the Hub minted for a repo joins groups; a paper's DOI among its tags stays the
    /// fact `paper_doi`.
    #[test]
    fn only_the_hubs_own_doi_joins() {
        use crate::sources::kit::{checked, own_keys};
        let r = repo(Kind::Model, "openai-community/gpt2");
        let mut v: Value = serde_json::from_slice(&read("model-gpt2.json")).unwrap();
        v["tags"]
            .as_array_mut()
            .unwrap()
            .push("doi:10.48550/arXiv.1706.03762".into());
        let card = parse(200, &serde_json::to_vec(&v).unwrap(), &r)
            .unwrap()
            .unwrap();
        assert_eq!(own_keys(&HuggingFace, &card), ["doi:10.57967/hf/0039"]);
        let doc = checked(&HuggingFace, &card);
        assert_eq!(doc.attributes["paper_doi"], "10.48550/arxiv.1706.03762");
    }

    #[test]
    fn model_with_the_hubs_doi() {
        let r = repo(Kind::Model, "openai-community/gpt2");
        let card = parse(200, &read("model-gpt2.json"), &r).unwrap().unwrap();
        assert_eq!(card.id, "hf:model/openai-community/gpt2");
        assert_eq!(card.keys, [Key::new("doi", "10.57967/hf/0039")]);
        assert_eq!(card.details["license"], "mit");
        assert_eq!(card.details["gated"], false);
        // The legacy request is answered with a redirect, never a card.
        assert!(parse(
            307,
            &read("model-gpt2-legacy.txt"),
            &repo(Kind::Model, "gpt2")
        )
        .is_err());
    }

    #[test]
    fn dataset() {
        let r = repo(Kind::Dataset, "openai/gsm8k");
        let card = parse(200, &read("dataset-gsm8k.json"), &r)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "Dataset");
        assert_eq!(card.id, "hf:dataset/openai/gsm8k");
        assert_eq!(card.url, "https://huggingface.co/datasets/openai/gsm8k");
        let description = card.description.unwrap();
        assert!(
            description.starts_with("Dataset Card for GSM8K Dataset Summary GSM8K (Grade School"),
            "{description}"
        );
        assert!(description.ends_with("basic arithmetic operations (+ − ×÷) to…"));
        assert_eq!(card.details["title"], "Grade School Math 8K");
        assert_eq!(card.details["license"], "mit");
        assert_eq!(card.details["arxiv"], serde_json::json!(["2110.14168"]));
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["created"], "2022-04-12T10:22:10+00:00");
    }

    #[test]
    fn space() {
        let r = repo(Kind::Space, "huggingface-projects/llama-2-7b-chat");
        let card = parse(200, &read("space-llama-2-7b-chat.json"), &r)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "WebApplication");
        assert_eq!(card.id, "hf:space/huggingface-projects/llama-2-7b-chat");
        assert_eq!(
            card.url,
            "https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat"
        );
        assert_eq!(card.details["sdk"], "gradio");
        assert_eq!(card.details["title"], "Llama 2 7B Chat");
        assert_eq!(
            card.details["app_url"],
            "https://huggingface-projects-llama-2-7b-chat.hf.space"
        );
        assert_eq!(card.details["likes"], 484);
        assert!(!card.details.contains_key("license"));
    }

    #[test]
    fn missing_repo() {
        let r = repo(Kind::Model, "openai-community/no-such-repo-pinakes");
        assert_eq!(parse(401, &read("model-missing.json"), &r).unwrap(), None);
        assert_eq!(parse(404, b"", &r).unwrap(), None);
    }
}
