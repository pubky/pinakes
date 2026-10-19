//! Models, datasets and Spaces on the Hugging Face Hub, read from the Hub API by the repo id in
//! the link.
//!
//! Every link of one repo on huggingface.co or hf.co (its page and its subpages: files, commits,
//! discussions, the dataset viewer) and its API URL are aliases of one identifier:
//! `hf:model/{owner}/{name}`, `hf:dataset/{owner}/{name}` or `hf:space/{owner}/{name}`, in lower
//! case, because the Hub ignores case in repo ids (it redirects any other case to the canonical
//! one, which the card shows).
//!
//! A name without an owner (`huggingface.co/gpt2`, `huggingface.co/datasets/gsm8k`) is a legacy
//! id and has no identifier of its own: the API answers it with a redirect to the namespaced repo
//! it moved to (`redirect`), and the legacy links are aliases of that repo's identifier. Any
//! other redirect works the same way: the Hub's redirect is what proves a rename.
use super::kit::{host_and_segments, int, json, plain, text, texts, time, Card, Key, Request};
use anyhow::{ensure, Result};
use serde_json::Value;

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

/// Subpages of a repo (files, history, discussions, the dataset viewer): aliases of the repo.
const SUBPAGES: &[&str] = &[
    "blob",
    "commit",
    "commits",
    "discussions",
    "edit",
    "raw",
    "resolve",
    "tree",
    "viewer",
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

/// A repo page with an optional subpage, after the kind's prefix.
fn page_path(kind: Kind, s: &[&str]) -> Option<Repo> {
    match s {
        [name] => repo(kind, None, name),
        // A legacy name followed by a subpage, e.g. `bert-base-uncased/blob/main/config.json`.
        [name, sub, ..] if SUBPAGES.contains(sub) => repo(kind, None, name),
        [owner, name] => repo(kind, Some(owner), name),
        [owner, name, sub, ..] if SUBPAGES.contains(sub) => repo(kind, Some(owner), name),
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
        [first, ..] if RESERVED.contains(first) => None,
        rest => page_path(Kind::Model, rest),
    }
}

/// The identifier every link form of the repo is an alias of, or `None` for a legacy id (its
/// repo comes from `redirect`).
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

/// The repo's API record. Redirects are not followed: a redirect names the repo the id now
/// belongs to (`redirect`).
pub fn request(repo: &Repo) -> Option<Request> {
    Request::get(&format!(
        "https://huggingface.co/api/{}/{}",
        repo.kind.api(),
        repo.id
    ))
}

/// The repo a redirect of the API (status and `Location` header) moves `repo` to: the canonical
/// case of its id, or the namespaced repo a legacy or renamed id now belongs to. The links of
/// `repo` are aliases of that repo's identifier (`Card::alias(page(repo))` records them).
pub fn redirect(status: u16, location: &str, repo: &Repo) -> Option<Repo> {
    if !matches!(status, 301 | 302 | 307 | 308) {
        return None;
    }
    let target = url::Url::parse("https://huggingface.co/api/")
        .ok()?
        .join(location)
        .ok()?;
    let to = from_uri(target.as_str())?;
    (to.kind == repo.kind && !to.legacy() && target.path().starts_with("/api/")).then_some(to)
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
    card.name = Some(answered.id.clone());
    card.description = match repo.kind {
        Kind::Dataset => summary(&v["description"]),
        Kind::Space => text(&v["cardData"]["short_description"]),
        Kind::Model => None,
    };
    card.start = time(&v["createdAt"]);
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
    // The Hub mints DOIs for repos under 10.57967/hf/; any other `doi:` tag names a paper.
    let mut papers = vec![];
    for doi in tags.iter().filter_map(|t| t.strip_prefix("doi:")) {
        let doi = doi.to_ascii_lowercase();
        if doi.starts_with("10.57967/hf/") {
            card.key(Key::new("doi", &doi));
        } else {
            papers.push(doi);
        }
    }
    card.detail("doi", papers);
    Ok(Some(card))
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
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/blob/main/config.json",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/resolve/main/README.md?download=true",
            "https://huggingface.co/meta-llama/Llama-2-7b-hf/discussions/12",
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
            "https://huggingface.co/datasets/openai/gsm8k/viewer/main/train",
            "https://hf.co/datasets/openai/gsm8k/tree/main/main",
            "https://huggingface.co/api/datasets/openai/gsm8k",
        ] {
            assert_eq!(id(uri).as_deref(), Some("hf:dataset/openai/gsm8k"), "{uri}");
        }
        for uri in [
            "https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat",
            "https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat/blob/main/app.py",
            "https://huggingface.co/api/spaces/huggingface-projects/llama-2-7b-chat",
        ] {
            assert_eq!(
                id(uri).as_deref(),
                Some("hf:space/huggingface-projects/llama-2-7b-chat"),
                "{uri}"
            );
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
    fn legacy_ids_wait_for_the_redirect() {
        for (uri, kind, name) in [
            ("https://huggingface.co/gpt2", Kind::Model, "gpt2"),
            (
                "https://huggingface.co/bert-base-uncased/blob/main/config.json",
                Kind::Model,
                "bert-base-uncased",
            ),
            ("https://hf.co/api/models/gpt2", Kind::Model, "gpt2"),
            (
                "https://huggingface.co/datasets/gsm8k",
                Kind::Dataset,
                "gsm8k",
            ),
        ] {
            let r = from_uri(uri).unwrap();
            assert_eq!(r, repo(kind, name), "{uri}");
            assert!(r.legacy());
            assert_eq!(identifier(&r), None);
        }
        let gpt2 = repo(Kind::Model, "gpt2");
        assert_eq!(
            request(&gpt2).unwrap().url.as_str(),
            "https://huggingface.co/api/models/gpt2"
        );
        // The recorded 307s (see provenance.json for their `Location` headers).
        assert_eq!(
            redirect(307, "/api/models/openai-community/gpt2", &gpt2),
            Some(repo(Kind::Model, "openai-community/gpt2"))
        );
        let gsm8k = repo(Kind::Dataset, "gsm8k");
        let to = redirect(307, "/api/datasets/openai/gsm8k", &gsm8k).unwrap();
        assert_eq!(identifier(&to).as_deref(), Some("hf:dataset/openai/gsm8k"));
        // A different case redirects to the canonical one: the same identifier.
        let upper = repo(Kind::Model, "META-LLAMA/llama-2-7b-hf");
        let to = redirect(307, "/api/models/meta-llama/Llama-2-7b-hf", &upper).unwrap();
        assert_eq!(to.id, "meta-llama/Llama-2-7b-hf");
        assert_eq!(identifier(&to), identifier(&upper));
        // Not a redirect, another kind, another legacy name, or a page.
        assert_eq!(redirect(200, "/api/models/a/b", &gpt2), None);
        assert_eq!(redirect(307, "/api/datasets/a/b", &gpt2), None);
        assert_eq!(redirect(307, "/api/models/gpt3", &gpt2), None);
        assert_eq!(redirect(307, "https://huggingface.co/a/b", &gpt2), None);
        assert_eq!(
            redirect(307, "https://huggingface.co/api/models/a/b", &gpt2),
            Some(repo(Kind::Model, "a/b"))
        );
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
            card.start.unwrap().to_rfc3339(),
            "2023-07-13T16:16:13+00:00"
        );
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
            card.start.unwrap().to_rfc3339(),
            "2022-04-12T10:22:10+00:00"
        );
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
