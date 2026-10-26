//! Reddit posts, subreddits and users, from Reddit's public JSON (`.json` beside any page). No
//! key, no account. Reddit refuses many requests without a browser with its network-security
//! page (403): the plain card, marked `blocked`. Every spelling (old., np., a post's subreddit
//! and slug, redd.it) is one resource at `https://www.reddit.com/comments/{id}/`,
//! `https://www.reddit.com/r/{name}/` or `https://www.reddit.com/user/{name}/`.
use super::{
    social::{self, Shape},
    text, web_url, Context, Example, Source,
};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Reddit;

const SITE: &str = "reddit.com";

/// A post id: base-36, 2–12 characters.
fn post_id(value: &str) -> bool {
    (2..=12).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}
/// A subreddit or user name: 2–30 letters, digits, `_` and `-`.
fn name(value: &str) -> bool {
    (2..=30).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What a Reddit link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Post(String),
    Subreddit(String),
    User(String),
}

impl Target {
    /// `https://www.reddit.com/r/{sub}/comments/{id}[/slug]`, `/comments/{id}[/slug]`,
    /// `/r/{sub}`, `/user/{name}` (also `/u/`), on reddit.com with any of its hosts (www., old.,
    /// new., np., m., none), and `https://redd.it/{id}`. A comment's permalink (a segment after
    /// the slug, or a `comment` parameter, which selects one comment) is not read: a comment is
    /// its own thing, never its post.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if url.query_pairs().any(|(k, _)| k == "comment") {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match url.host_str()? {
            "redd.it" | "www.redd.it" => match segments.as_slice() {
                [id] if post_id(id) => Some(Self::Post((*id).to_owned())),
                _ => None,
            },
            "reddit.com" | "www.reddit.com" | "old.reddit.com" | "new.reddit.com"
            | "np.reddit.com" | "m.reddit.com" => match segments.as_slice() {
                ["r", sub, "comments", id] | ["r", sub, "comments", id, _]
                    if name(sub) && post_id(id) =>
                {
                    Some(Self::Post((*id).to_owned()))
                }
                ["comments", id] | ["comments", id, _] if post_id(id) => {
                    Some(Self::Post((*id).to_owned()))
                }
                ["r", sub] if name(sub) => Some(Self::Subreddit(sub.to_ascii_lowercase())),
                ["user" | "u", user] if name(user) => Some(Self::User(user.to_ascii_lowercase())),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Post(id) => format!("https://www.reddit.com/comments/{id}/"),
            Self::Subreddit(name) => format!("https://www.reddit.com/r/{name}/"),
            Self::User(name) => format!("https://www.reddit.com/user/{name}/"),
        }
    }
    /// The JSON to read.
    fn json(&self) -> String {
        match self {
            Self::Post(id) => format!("https://www.reddit.com/comments/{id}.json?raw_json=1"),
            Self::Subreddit(name) => {
                format!("https://www.reddit.com/r/{name}/about.json?raw_json=1")
            }
            Self::User(name) => format!("https://www.reddit.com/user/{name}/about.json?raw_json=1"),
        }
    }
    fn shape(&self) -> Shape {
        match self {
            Self::Post(_) => Shape::Post,
            _ => Shape::Profile,
        }
    }
}

fn count(v: &Value) -> Option<String> {
    v.as_i64().map(|n| n.to_string())
}
/// A time Reddit gives as Unix seconds, often with a fraction (`1776700800.0`).
fn published(v: &Value) -> Option<String> {
    use chrono::TimeZone;
    let seconds = super::kit::float(v)?;
    chrono::Utc
        .timestamp_opt(seconds as i64, 0)
        .single()
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}
/// A Reddit image URL (`thumbnail` is `self`, `default` or `nsfw` when there is none).
fn image(v: &Value) -> Option<String> {
    text(v)
        .filter(|u| web_url(u).is_some())
        .map(|u| u.split('?').next().unwrap_or(&u).to_owned())
}

/// The card of a post from its listing, or `None` when the listing holds no such post.
pub fn post_card(doc: ResourceMeta, id: &str, v: &Value) -> Result<Option<ResourceMeta>> {
    let Some(post) = v[0]["data"]["children"][0]["data"].as_object() else {
        return Ok(None);
    };
    let post = Value::Object(post.clone());
    anyhow::ensure!(post["id"] == id, "Reddit answered for another post");
    let mut doc = Shape::Post.card(doc, SITE);
    doc.set_type("DiscussionForumPosting");
    let title = text(&post["title"]).unwrap_or_default();
    let body = text(&post["selftext"]).unwrap_or_default();
    social::post_text(&mut doc, &title, text(&post["author"]).as_deref());
    doc.description = Some(social::excerpt(
        if body.is_empty() { &title } else { &body },
        200,
    ));
    if post["over_18"] != true {
        doc.image = image(&post["thumbnail"]).or_else(|| {
            text(&post["url"]).filter(|u| post["post_hint"] == "image" && web_url(u).is_some())
        });
    }
    social::fact(&mut doc, "author", text(&post["author"]));
    social::fact(&mut doc, "subreddit", text(&post["subreddit"]));
    social::fact(&mut doc, "score", count(&post["score"]));
    social::fact(&mut doc, "comments", count(&post["num_comments"]));
    social::fact(&mut doc, "published", published(&post["created_utc"]));
    Ok(Some(doc))
}

/// The card of a subreddit from its `about`, or `None` when the answer describes no subreddit.
pub fn subreddit_card(doc: ResourceMeta, name: &str, v: &Value) -> Result<Option<ResourceMeta>> {
    let about = &v["data"];
    let Some(display) = text(&about["display_name"]) else {
        return Ok(None);
    };
    anyhow::ensure!(
        display.eq_ignore_ascii_case(name),
        "Reddit answered for another subreddit"
    );
    let mut doc = Shape::Profile.card(doc, SITE);
    doc.set_type("Organization");
    doc.name = Some(format!("r/{display}"));
    doc.description = text(&about["public_description"])
        .or_else(|| text(&about["title"]))
        .map(|d| social::excerpt(&d, 200));
    if about["over18"] != true {
        doc.image = image(&about["community_icon"]).or_else(|| image(&about["icon_img"]));
    }
    social::fact(&mut doc, "handle", Some(display.to_ascii_lowercase()));
    social::fact(&mut doc, "subscribers", count(&about["subscribers"]));
    social::fact(&mut doc, "published", published(&about["created_utc"]));
    Ok(Some(doc))
}

/// The card of a user from their `about`, or `None` when the answer describes no user.
pub fn user_card(doc: ResourceMeta, name: &str, v: &Value) -> Result<Option<ResourceMeta>> {
    let about = &v["data"];
    let Some(user) = text(&about["name"]) else {
        return Ok(None);
    };
    anyhow::ensure!(
        user.eq_ignore_ascii_case(name),
        "Reddit answered for another user"
    );
    let mut doc = Shape::Profile.card(doc, SITE);
    doc.name = Some(format!("u/{user}"));
    doc.description =
        text(&about["subreddit"]["public_description"]).map(|d| social::excerpt(&d, 200));
    doc.image = image(&about["icon_img"]);
    social::fact(&mut doc, "handle", Some(user.to_ascii_lowercase()));
    social::fact(&mut doc, "karma", count(&about["total_karma"]));
    social::fact(&mut doc, "published", published(&about["created_utc"]));
    Ok(Some(doc))
}

/// The card of a link Reddit refused: its shape alone, marked `blocked`, a post keeping the type
/// its readable card has.
pub fn blocked(doc: ResourceMeta, target: &Target) -> ResourceMeta {
    let mut doc = social::blocked(doc, target.shape(), SITE);
    if matches!(target, Target::Post(_)) {
        doc.set_type("DiscussionForumPosting");
    }
    doc
}

#[async_trait]
impl Source for Reddit {
    fn name(&self) -> &'static str {
        "reddit"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "reddit",
                    uri: "https://www.reddit.com/r/announcements/comments/1k3d0pk/",
                    card_type: "DiscussionForumPosting",
                    group_with: None,
                },
                E2eSample {
                    kind: "reddit_subreddit",
                    uri: "https://old.reddit.com/r/announcements",
                    card_type: "Organization",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "www.reddit.com",
                    path: "/comments/1k3d0pk.json",
                    query: &[],
                    header: None,
                    content_type: "application/json",
                    file: "post-1k3d0pk.json",
                },
                E2eAnswer {
                    host: "www.reddit.com",
                    path: "/r/announcements/about.json",
                    query: &[],
                    header: None,
                    content_type: "application/json",
                    file: "subreddit-announcements.json",
                },
            ],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = Target::of_uri(uri)?.root();
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "author",
            "subreddit",
            "score",
            "comments",
            "published",
            "handle",
            "subscribers",
            "karma",
            "blocked",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        // Reddit's published limit for requests without an account: 10 per minute.
        &[("www.reddit.com", 10.0 / 60.0, None)]
    }
    /// A post's card, and its blocked card (a post Reddit refused to show), whose type is the
    /// same: storage asks the stored card whether to purge the post's history once it is gone.
    fn social(&self, doc: &ResourceMeta) -> bool {
        matches!(
            doc.type_name.as_str(),
            "DiscussionForumPosting" | "SocialMediaPosting"
        )
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.reddit.com/r/announcements/comments/1k3d0pk/",
                card_type: "DiscussionForumPosting",
            },
            Example {
                uri: "https://www.reddit.com/r/announcements/",
                card_type: "Organization",
            },
            Example {
                uri: "https://www.reddit.com/user/spez/",
                card_type: "Person",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Reddit link")?;
        let doc = cx.card(uri, self);
        let options = FetchOptions {
            accept: Some("application/json".into()),
            ..Default::default()
        };
        let fetched = match cx.fetch(&target.json(), &options).await {
            Ok(fetched) => fetched,
            Err(e) if e.is::<crate::resolver::interstitial::Blocked>() => {
                return Ok(blocked(doc, &target).with_root(target.root()))
            }
            Err(e) => return Err(e),
        };
        let mut doc = match fetched.status {
            200 => {
                let v: Value =
                    serde_json::from_slice(&fetched.body).context("Reddit's answer is not JSON")?;
                match &target {
                    Target::Post(id) => post_card(doc, id, &v)?,
                    Target::Subreddit(name) => subreddit_card(doc, name, &v)?,
                    Target::User(name) => user_card(doc, name, &v)?,
                }
                .unwrap_or_else(|| cx.missing(uri, self))
            }
            404 => cx.missing(uri, self),
            401 | 403 => blocked(doc, &target),
            s => anyhow::bail!("Reddit answered {s}"),
        };
        doc.source_root = Some(target.root());
        Ok(doc)
    }
}

trait WithRoot {
    fn with_root(self, root: String) -> Self;
}
impl WithRoot for ResourceMeta {
    fn with_root(mut self, root: String) -> Self {
        self.source_root = Some(root);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Value {
        serde_json::from_slice(&crate::sources::fixture::read("reddit", file)).unwrap()
    }

    #[test]
    fn addresses() {
        for (uri, target) in [
            (
                "https://www.reddit.com/r/announcements/comments/1k3d0pk/a_title/",
                Target::Post("1k3d0pk".into()),
            ),
            (
                "https://old.reddit.com/comments/1k3d0pk",
                Target::Post("1k3d0pk".into()),
            ),
            ("https://redd.it/1k3d0pk", Target::Post("1k3d0pk".into())),
            (
                "https://reddit.com/r/Announcements",
                Target::Subreddit("announcements".into()),
            ),
            ("https://www.reddit.com/u/spez", Target::User("spez".into())),
        ] {
            assert_eq!(Target::of_uri(uri), Some(target), "{uri}");
        }
        for uri in [
            // A comment's permalink, with and without its post's slug, is not its post.
            "https://www.reddit.com/r/announcements/comments/1k3d0pk/a_title/abcdefg/",
            "https://old.reddit.com/r/announcements/comments/1k3d0pk/a_title/abcdefg",
            "https://www.reddit.com/comments/1k3d0pk/a_title/abcdefg/",
            // So is a post's link that selects one comment.
            "https://www.reddit.com/r/announcements/comments/1k3d0pk/a_title/?comment=abcdefg",
            "https://www.reddit.com/comments/1k3d0pk/?context=3&comment=abcdefg",
            "https://redd.it/1k3d0pk?comment=abcdefg",
            "https://www.reddit.com/r/announcements/s/AbCdEf",
            "https://www.reddit.com/r/announcements/wiki/index",
            "https://www.reddit.com/comments/",
            "https://www.reddit.com/search?q=x",
            "https://example.com/r/announcements",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
        assert_eq!(
            Reddit
                .alias_root("https://old.reddit.com/r/announcements/comments/1k3d0pk/a_title/")
                .as_deref(),
            Some("https://www.reddit.com/comments/1k3d0pk/")
        );
        assert_eq!(
            Reddit.alias_root("https://www.reddit.com/comments/1k3d0pk/"),
            None
        );
        assert_eq!(
            Reddit.alias_root("https://www.reddit.com/comments/1k3d0pk/a_title/?comment=abcdefg"),
            None
        );
        // Other parameters (a share's tracking, the comment sort) leave the post.
        assert_eq!(
            Target::of_uri("https://www.reddit.com/comments/1k3d0pk/?sort=new&share_id=x"),
            Some(Target::Post("1k3d0pk".into()))
        );
    }

    #[test]
    fn post_subreddit_and_user_cards() {
        let base = ResourceMeta::new("https://www.reddit.com/comments/1k3d0pk/", "reddit", 3600);
        let doc = post_card(base.clone(), "1k3d0pk", &read("post-1k3d0pk.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "DiscussionForumPosting");
        assert!(doc
            .name
            .as_deref()
            .unwrap()
            .starts_with("Reddit's 2025 Transparency Report"));
        assert_eq!(doc.attributes["author"], "reddit_admin");
        assert_eq!(doc.attributes["subreddit"], "announcements");
        assert_eq!(doc.attributes["score"], "1234");
        assert_eq!(doc.attributes["comments"], "567");
        assert_eq!(doc.attributes["published"], "2026-04-20T16:00:00Z");
        assert!(post_card(base, "other", &read("post-1k3d0pk.json")).is_err());

        let base = ResourceMeta::new("https://www.reddit.com/r/announcements/", "reddit", 3600);
        let doc = subreddit_card(
            base.clone(),
            "announcements",
            &read("subreddit-announcements.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "Organization");
        assert_eq!(doc.name.as_deref(), Some("r/announcements"));
        assert_eq!(doc.attributes["subscribers"], "300000000");
        assert!(doc.image.as_deref().unwrap().starts_with("https://"));
        assert!(subreddit_card(base, "other", &read("subreddit-announcements.json")).is_err());

        let base = ResourceMeta::new("https://www.reddit.com/user/spez/", "reddit", 3600);
        let doc = user_card(base, "spez", &read("user-spez.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "Person");
        assert_eq!(doc.name.as_deref(), Some("u/spez"));
        assert_eq!(doc.attributes["karma"], "1000000");
    }
}
