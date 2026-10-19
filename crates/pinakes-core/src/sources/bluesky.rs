//! Bluesky posts and profiles, read from the public AppView (`public.api.bsky.app`). No key. A
//! post's identity is its at-URI: every address of it, by handle or by DID, becomes one resource
//! named by the DID.
use super::{text, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Bluesky;

const APPVIEW: &str = "https://public.api.bsky.app/xrpc";

/// A DID (`did:plc:…`, `did:web:…`).
fn did(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("did:") else {
        return false;
    };
    let Some((method, id)) = rest.split_once(':') else {
        return false;
    };
    matches!(method, "plc" | "web")
        && !id.is_empty()
        && value.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:%".contains(&b))
}
/// A handle: a domain name.
fn handle(value: &str) -> bool {
    value.len() <= 253
        && value.contains('.')
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}
/// A record key: letters, digits and `.-_:~`.
fn rkey(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:~".contains(&b))
}

/// What a Bluesky link names: an actor (a DID or a handle) and, for a post, its record key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub actor: String,
    pub post: Option<String>,
}

impl Target {
    /// `https://bsky.app/profile/{actor}[/post/{rkey}]` or `at://{actor}[/app.bsky.feed.post/{rkey}]`.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let (actor, rest): (&str, Vec<&str>) = if let Some(at) = uri.strip_prefix("at://") {
            let mut parts = at.split('/');
            let actor = parts.next()?;
            (actor, parts.collect())
        } else {
            let url = web_url(uri)?;
            if !matches!(url.host_str()?, "bsky.app" | "www.bsky.app") {
                return None;
            }
            let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
            let ["profile", actor, rest @ ..] = segments.as_slice() else {
                return None;
            };
            let rest = match rest {
                [] => vec![],
                ["post", key] => vec!["app.bsky.feed.post", key],
                _ => return None,
            };
            let actor = percent_encoding::percent_decode_str(actor)
                .decode_utf8()
                .ok()?;
            return Self::new(&actor, &rest);
        };
        Self::new(actor, &rest)
    }
    fn new(actor: &str, rest: &[&str]) -> Option<Self> {
        let actor = actor.to_ascii_lowercase();
        if !(did(&actor) || handle(&actor)) {
            return None;
        }
        let post = match rest {
            [] => None,
            ["app.bsky.feed.post", key] if rkey(key) => Some((*key).to_owned()),
            _ => return None,
        };
        Some(Self { actor, post })
    }
    fn by_did(&self) -> bool {
        did(&self.actor)
    }
    /// The address by DID: the root of every spelling.
    pub fn root_with(&self, did: &str) -> String {
        match &self.post {
            Some(key) => format!("https://bsky.app/profile/{did}/post/{key}"),
            None => format!("https://bsky.app/profile/{did}"),
        }
    }
}

/// The DID a `resolveHandle` answer names.
pub fn resolved_did(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v["did"].as_str().filter(|d| did(d)).map(str::to_owned)
}

fn excerpt(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut: String = flat.chars().take(max).collect();
    if cut.len() < flat.len() {
        cut.push('…');
    }
    cut
}

/// Whether an account carries the `!no-unauthenticated` self-label.
fn no_unauthenticated(labels: &Value) -> bool {
    labels
        .as_array()
        .is_some_and(|l| l.iter().any(|l| l["val"] == "!no-unauthenticated"))
}

/// The card of a post from `getPostThread`, or `None` when the post does not exist.
pub fn post_card(
    mut doc: ResourceMeta,
    at_uri: &str,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let v: Value = serde_json::from_slice(body).unwrap_or_default();
    match status {
        200 => {}
        400 if v["error"] == "NotFound" => return Ok(None),
        s => anyhow::bail!("Bluesky answered {s}"),
    }
    let thread = &v["thread"];
    if thread["$type"] != "app.bsky.feed.defs#threadViewPost" {
        // A blocked or deleted post (`#notFoundPost`, `#blockedPost`).
        return Ok(None);
    }
    let post = &thread["post"];
    anyhow::ensure!(post["uri"] == at_uri, "Bluesky answered for another post");
    let author = &post["author"];
    doc.set_type("SocialMediaPosting");
    doc.site = "bsky.app".into();
    // An author who asked apps not to show their content to logged-out viewers: the post
    // resolves and groups, but its card shows no text, image or name.
    if no_unauthenticated(&author["labels"]) {
        doc.name = Some("Post on bsky.app".into());
        doc.attributes.insert("at_uri".into(), at_uri.to_owned());
        return Ok(Some(doc));
    }
    let body_text = text(&post["record"]["text"]).unwrap_or_default();
    let by = text(&author["displayName"]).or_else(|| text(&author["handle"]));
    doc.name = if body_text.is_empty() {
        by.as_ref().map(|by| format!("Post by {by}"))
    } else {
        Some(excerpt(&body_text, 120))
    };
    doc.description = (!body_text.is_empty()).then(|| excerpt(&body_text, 200));
    doc.image = post["embed"]["images"][0]["thumb"]
        .as_str()
        .or(author["avatar"].as_str())
        .and_then(web_url)
        .map(String::from);
    for (key, value) in [
        ("author", text(&author["handle"])),
        ("author_name", text(&author["displayName"])),
        ("published", text(&post["record"]["createdAt"])),
        ("likes", post["likeCount"].as_u64().map(|n| n.to_string())),
        (
            "reposts",
            post["repostCount"].as_u64().map(|n| n.to_string()),
        ),
        (
            "replies",
            post["replyCount"].as_u64().map(|n| n.to_string()),
        ),
        ("at_uri", Some(at_uri.to_owned())),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

/// The card of a profile from `getProfile`, or `None` when there is no such actor.
pub fn profile_card(
    mut doc: ResourceMeta,
    did_: &str,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let v: Value = serde_json::from_slice(body).unwrap_or_default();
    match status {
        200 => {}
        400 if matches!(
            v["error"].as_str(),
            Some("InvalidRequest" | "NotFound" | "AccountTakedown")
        ) =>
        {
            return Ok(None)
        }
        s => anyhow::bail!("Bluesky answered {s}"),
    }
    anyhow::ensure!(v["did"] == did_, "Bluesky answered for another actor");
    doc.set_type("Person");
    doc.site = "bsky.app".into();
    if no_unauthenticated(&v["labels"]) {
        doc.name = Some("Profile on bsky.app".into());
        doc.attributes.insert("did".into(), did_.to_owned());
        return Ok(Some(doc));
    }
    doc.name = text(&v["displayName"]).or_else(|| text(&v["handle"]));
    doc.description = text(&v["description"]).map(|d| excerpt(&d, 200));
    doc.image = text(&v["avatar"]).filter(|a| web_url(a).is_some());
    for (key, value) in [
        ("handle", text(&v["handle"])),
        ("did", Some(did_.to_owned())),
        (
            "followers",
            v["followersCount"].as_u64().map(|n| n.to_string()),
        ),
        ("posts", v["postsCount"].as_u64().map(|n| n.to_string())),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for Bluesky {
    fn name(&self) -> &'static str {
        "bluesky"
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some()
    }
    /// Only spellings by DID are known from the link alone; a handle is resolved first.
    fn alias_root(&self, uri: &str) -> Option<String> {
        let target = Target::of_uri(uri)?;
        if !target.by_did() {
            return None;
        }
        let root = target.root_with(&target.actor);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "author",
            "author_name",
            "published",
            "likes",
            "reposts",
            "replies",
            "at_uri",
            "handle",
            "did",
            "followers",
            "posts",
        ]
    }
    fn social(&self, doc: &ResourceMeta) -> bool {
        doc.type_name == "SocialMediaPosting"
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://bsky.app/profile/bsky.app/post/3mxzez2m7wk2r",
                card_type: "SocialMediaPosting",
            },
            Example {
                uri: "https://bsky.app/profile/bsky.app",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Bluesky link")?;
        let json = FetchOptions {
            accept: Some("application/json".into()),
            ..Default::default()
        };
        let did = if target.by_did() {
            target.actor.clone()
        } else {
            let f = cx
                .fetch(
                    &format!(
                        "{APPVIEW}/com.atproto.identity.resolveHandle?handle={}",
                        target.actor
                    ),
                    &json,
                )
                .await?;
            match f.status {
                200 => resolved_did(&f.body).context("no DID")?,
                400 => return Ok(cx.missing(uri, self)),
                s => anyhow::bail!("Bluesky answered {s}"),
            }
        };
        let mut doc = match &target.post {
            Some(key) => {
                let at_uri = format!("at://{did}/app.bsky.feed.post/{key}");
                let mut url = url::Url::parse(&format!("{APPVIEW}/app.bsky.feed.getPostThread"))?;
                url.query_pairs_mut()
                    .append_pair("uri", &at_uri)
                    .append_pair("depth", "0")
                    .append_pair("parentHeight", "0");
                let f = cx.fetch(url.as_str(), &json).await?;
                post_card(cx.card(uri, self), &at_uri, f.status, &f.body)?
            }
            None => {
                let f = cx
                    .fetch(
                        &format!("{APPVIEW}/app.bsky.actor.getProfile?actor={did}"),
                        &json,
                    )
                    .await?;
                profile_card(cx.card(uri, self), &did, f.status, &f.body)?
            }
        }
        .unwrap_or_else(|| cx.missing(uri, self));
        // A link by handle becomes an alias of the address by DID.
        doc.source_root = Some(target.root_with(&did));
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("bluesky", file)
    }
    const DID: &str = "did:plc:z72i7hdynmk6r22z27h6tvur";

    #[test]
    fn addresses() {
        let post = Target::of_uri("https://bsky.app/profile/bsky.app/post/3mxzez2m7wk2r").unwrap();
        assert_eq!(
            post,
            Target {
                actor: "bsky.app".into(),
                post: Some("3mxzez2m7wk2r".into())
            }
        );
        assert_eq!(
            Bluesky.alias_root("https://bsky.app/profile/bsky.app/post/3mxzez2m7wk2r"),
            None
        );
        let root = format!("https://bsky.app/profile/{DID}/post/3mxzez2m7wk2r");
        assert_eq!(post.root_with(DID), root);
        assert_eq!(
            Bluesky
                .alias_root(&format!("at://{DID}/app.bsky.feed.post/3mxzez2m7wk2r"))
                .as_deref(),
            Some(root.as_str())
        );
        assert_eq!(Bluesky.alias_root(&root), None);
        assert!(Bluesky.reads(&format!("https://bsky.app/profile/{DID}")));
        for uri in [
            "https://bsky.app/profile/bsky.app/lists/x",
            "https://bsky.app/search?q=x",
            "https://bsky.app/profile/not_a_handle",
            "at://bsky.app/app.bsky.feed.like/x",
        ] {
            assert!(!Bluesky.reads(uri), "{uri}");
        }
    }

    #[test]
    fn an_author_who_opted_out_shows_nothing_of_theirs() {
        let at = format!("at://{DID}/app.bsky.feed.post/3mxzez2m7wk2r");
        let base = ResourceMeta::new(
            "https://bsky.app/profile/bsky.app/post/3mxzez2m7wk2r",
            "bluesky",
            3600,
        );
        let doc = post_card(
            base,
            &at,
            200,
            &read("bsky-thread-3mxzez2m7wk2r-no-unauthenticated.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert_eq!(doc.name.as_deref(), Some("Post on bsky.app"));
        assert_eq!((doc.description, doc.image), (None, None));
        assert_eq!(doc.attributes.keys().collect::<Vec<_>>(), ["at_uri"]);
        let mut profile: Value =
            serde_json::from_slice(&read("bsky-profile-bsky-app.json")).unwrap();
        profile["labels"] = serde_json::json!([{"val": "!no-unauthenticated"}]);
        let base = ResourceMeta::new("https://bsky.app/profile/bsky.app", "bluesky", 3600);
        let doc = profile_card(base, DID, 200, &serde_json::to_vec(&profile).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(doc.name.as_deref(), Some("Profile on bsky.app"));
        assert_eq!((doc.description, doc.image), (None, None));
    }

    #[test]
    fn post_and_profile_cards() {
        assert_eq!(
            resolved_did(&read("bsky-resolve-bsky-app.json")).as_deref(),
            Some(DID)
        );
        let at = format!("at://{DID}/app.bsky.feed.post/3mxzez2m7wk2r");
        let base = ResourceMeta::new(
            "https://bsky.app/profile/bsky.app/post/3mxzez2m7wk2r",
            "bluesky",
            3600,
        );
        let doc = post_card(
            base.clone(),
            &at,
            200,
            &read("bsky-thread-3mxzez2m7wk2r.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert!(doc
            .name
            .as_deref()
            .unwrap()
            .starts_with("“Pro wrestling conversations"));
        let a = &doc.attributes;
        assert_eq!(a["author"], "bsky.app");
        assert_eq!(a["likes"], "1008");
        assert_eq!(a["at_uri"], at);
        let missing = format!("at://{DID}/app.bsky.feed.post/3aaaaaaaaaaaa");
        assert!(post_card(
            base.clone(),
            &missing,
            400,
            &read("bsky-thread-missing.json")
        )
        .unwrap()
        .is_none());
        assert!(post_card(
            base.clone(),
            &missing,
            200,
            &read("bsky-thread-3mxzez2m7wk2r.json")
        )
        .is_err());

        let doc = profile_card(base, DID, 200, &read("bsky-profile-bsky-app.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("Bluesky"));
        assert_eq!(doc.attributes["handle"], "bsky.app");
        assert_eq!(doc.attributes["followers"], "35309351");
    }
}
