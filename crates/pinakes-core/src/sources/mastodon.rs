//! Mastodon posts, read from the instance's own API (`/api/v1/statuses/{id}`) on any instance.
//! A post is keyed on its ActivityPub id: every address of it, on its own server or seen through
//! another, becomes one resource. No key; Mastodon allows 300 requests per 5 minutes per address.
//! A server that is not Mastodon (or refuses unauthenticated reads) is remembered, and its links
//! get ordinary web cards.
use super::{text, web_url, Context, Declined, Example, HostMemo, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::LazyLock;

pub struct Mastodon;

/// Which hosts answered as Mastodon, and which did not.
static INSTANCES: LazyLock<HostMemo> = LazyLock::new(HostMemo::default);

fn status_id(value: &str) -> Option<&str> {
    (!value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit()))
        .then_some(value)
}
fn account(value: &str) -> bool {
    let name = value.split('@').next().unwrap_or_default();
    !name.is_empty()
        && value.len() <= 300
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-@".contains(&b))
}

/// A post on an instance: `https://{host}/@{account}/{id}` or `/users/{account}/statuses/{id}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub id: String,
}

impl Target {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if url.scheme() != "https" || url.port().is_some() {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.collect();
        let id = match segments.as_slice() {
            [user, id] if user.strip_prefix('@').is_some_and(account) => status_id(id)?,
            ["users", user, "statuses", id] if account(user) => status_id(id)?,
            _ => return None,
        };
        Some(Self {
            host: url.host_str()?.to_ascii_lowercase(),
            id: id.to_owned(),
        })
    }
}

/// HTML post content as plain text.
fn plain(html: &str) -> String {
    let text = crate::resolver::cards::plain(&html.replace("</p>", "</p> "));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The card of a status, or `None` when the instance says it does not exist. A body that is not
/// a Mastodon status is `Declined`.
pub fn card(
    mut doc: ResourceMeta,
    target: &Target,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Err(Declined.into()),
    };
    match status {
        200 => {}
        404 | 410 if v["error"].is_string() => return Ok(None),
        401 | 403 | 404 | 405 => return Err(Declined.into()),
        s => anyhow::bail!("the instance answered {s}"),
    }
    let (Some(id), Some(ap)) = (text(&v["id"]), text(&v["uri"])) else {
        return Err(Declined.into());
    };
    anyhow::ensure!(id == target.id, "the instance answered for another post");
    let ap = web_url(&ap).context("no ActivityPub id")?;
    let account = &v["account"];
    let content = text(&v["content"]).map(|c| plain(&c)).unwrap_or_default();
    let warning = text(&v["spoiler_text"]);
    doc.set_type("SocialMediaPosting");
    let by = text(&account["display_name"]).or_else(|| text(&account["acct"]));
    // A content warning stands in for the text it hides.
    let shown = warning.clone().unwrap_or(content);
    doc.name = if shown.is_empty() {
        by.as_ref().map(|by| format!("Post by {by}"))
    } else {
        Some(super::cut(&shown, 120))
    };
    doc.description = (!shown.is_empty()).then(|| super::cut(&shown, 200));
    doc.image = v["media_attachments"][0]["preview_url"]
        .as_str()
        .filter(|_| v["sensitive"] != true)
        .or(account["avatar_static"].as_str())
        .and_then(web_url)
        .map(String::from);
    for (key, value) in [
        ("author", text(&account["acct"])),
        ("author_name", text(&account["display_name"])),
        ("published", text(&v["created_at"])),
        (
            "replies",
            v["replies_count"].as_u64().map(|n| n.to_string()),
        ),
        (
            "reblogs",
            v["reblogs_count"].as_u64().map(|n| n.to_string()),
        ),
        (
            "favourites",
            v["favourites_count"].as_u64().map(|n| n.to_string()),
        ),
        ("content_warning", warning),
        // An author who opted out of search indexing: the card stays a link preview, as
        // Mastodon shows one, but its text is never searchable nor kept in the change history.
        ("opted_out", opted_out(account).then(|| "true".into())),
        ("activitypub_id", Some(ap.to_string())),
        (
            "page_url",
            text(&v["url"]).filter(|u| web_url(u).is_some_and(|u| same_host(&u, &target.host))),
        ),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    // Every address of the post on its own server becomes an alias of its ActivityPub id. A
    // server answering with another host's id (a copy, or a claim) keeps its own card: only the
    // id's own server could say that the two are one post.
    if same_host(&ap, &target.host) {
        doc.source_root = Some(ap.to_string());
    }
    Ok(Some(doc))
}

/// Whether an account opted out of search indexing (`indexable: false`, or `noindex`, which
/// the profile's `robots` meta tag also carries).
fn opted_out(account: &Value) -> bool {
    account["indexable"] == false || account["noindex"] == true
}

/// Whether `url` is on the link's server: the same host, the default HTTPS port and no user
/// part.
fn same_host(url: &url::Url, host: &str) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|h| h.trim_end_matches('.').eq_ignore_ascii_case(host))
}

#[async_trait]
impl Source for Mastodon {
    fn name(&self) -> &'static str {
        "mastodon"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "mastodon",
                uri: "https://mastodon.social/@Gargron/1",
                card_type: "SocialMediaPosting",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "mastodon.social",
                path: "/api/v1/statuses/1",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "status-1.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some_and(|t| INSTANCES.get(&t.host) != Some(false))
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "author",
            "author_name",
            "published",
            "replies",
            "reblogs",
            "favourites",
            "content_warning",
            "activitypub_id",
            "page_url",
            "opted_out",
        ]
    }
    fn social(&self, doc: &ResourceMeta) -> bool {
        doc.type_name == "SocialMediaPosting"
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://mastodon.social/@Gargron/1",
            card_type: "SocialMediaPosting",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a Mastodon post")?;
        if INSTANCES.get(&target.host) == Some(false) {
            return Err(Declined.into());
        }
        let f = cx
            .fetch(
                &format!("https://{}/api/v1/statuses/{}", target.host, target.id),
                &FetchOptions::json(),
            )
            .await?;
        let (doc, read, status, body) =
            (cx.card(uri, self), target.clone(), f.status, f.body.clone());
        let result =
            crate::resolver::html::blocking(move || card(doc, &read, status, &body)).await?;
        // Only an answer that is not JSON at all says the server is not Mastodon; a JSON
        // refusal (a private post, one server's rule) is about this post alone.
        let json = serde_json::from_slice::<Value>(&f.body).is_ok();
        match &result {
            Err(e) if e.is::<Declined>() && !json => INSTANCES.set(&target.host, false),
            Ok(Some(_)) => INSTANCES.set(&target.host, true),
            _ => {}
        }
        Ok(result?.unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("mastodon", file)
    }

    #[test]
    fn addresses() {
        for (uri, host, id) in [
            ("https://mastodon.social/@Gargron/1", "mastodon.social", "1"),
            (
                "https://mastodon.social/users/Gargron/statuses/1",
                "mastodon.social",
                "1",
            ),
            (
                "https://fosstodon.org/@someone@mastodon.social/113000000000000000",
                "fosstodon.org",
                "113000000000000000",
            ),
        ] {
            assert_eq!(
                Target::of_uri(uri),
                Some(Target {
                    host: host.into(),
                    id: id.into()
                }),
                "{uri}"
            );
        }
        for uri in [
            "https://mastodon.social/@Gargron",
            "https://medium.com/@someone/a-title-1a2b3c",
            "https://www.tiktok.com/@someone/video/123",
            "https://mastodon.social/@Gargron/1/reblogs",
            "http://mastodon.social/@Gargron/1",
        ] {
            assert_eq!(Target::of_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn statuses() {
        let target = Target {
            host: "mastodon.social".into(),
            id: "1".into(),
        };
        let base = ResourceMeta::new("https://mastodon.social/@Gargron/1", "mastodon", 3600);
        let doc = card(base.clone(), &target, 200, &read("status-1.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "SocialMediaPosting");
        assert_eq!(doc.attributes["author"], "Gargron");
        assert_eq!(
            doc.attributes["activitypub_id"],
            "https://mastodon.social/users/Gargron/statuses/1"
        );
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://mastodon.social/users/Gargron/statuses/1")
        );
        assert!(doc.image.is_some());
        let media = Target {
            host: "mastodon.social".into(),
            id: "116929144390213579".into(),
        };
        let doc = card(
            base.clone(),
            &media,
            200,
            &read("status-media-116929144390213579.json"),
        )
        .unwrap()
        .unwrap();
        assert!(doc.image.as_deref().unwrap().starts_with("https://"));
        assert!(
            card(base.clone(), &target, 404, &read("status-missing.json"))
                .unwrap()
                .is_none()
        );
        // Another server's 404 page is not Mastodon's: the link goes to the web fetch.
        let error = card(base.clone(), &target, 404, &read("not-mastodon.html")).unwrap_err();
        assert!(error.is::<Declined>());
        assert!(card(base, &media, 200, &read("status-1.json")).is_err());
    }

    #[test]
    fn an_author_who_opted_out_keeps_a_card_marked_opted_out() {
        let target = Target {
            host: "mastodon.social".into(),
            id: "1".into(),
        };
        let base = ResourceMeta::new("https://mastodon.social/@Gargron/1", "mastodon", 3600);
        let doc = card(base.clone(), &target, 200, &read("status-1-opted-out.json"))
            .unwrap()
            .unwrap();
        // A link preview, as Mastodon shows one; search and history read the mark.
        assert_eq!(doc.name.as_deref(), Some("Hello world"));
        assert!(doc.image.is_some());
        assert_eq!(doc.attributes["opted_out"], "true");
        assert_eq!(
            doc.source_root.as_deref(),
            Some("https://mastodon.social/users/Gargron/statuses/1")
        );
        let doc = card(base, &target, 200, &read("status-1.json"))
            .unwrap()
            .unwrap();
        assert!(!doc.attributes.contains_key("opted_out"));
    }

    #[test]
    fn a_user_part_or_port_is_another_server() {
        for id in [
            "https://someone@mastodon.social/users/Gargron/statuses/1",
            "https://mastodon.social:8443/users/Gargron/statuses/1",
            "http://mastodon.social/users/Gargron/statuses/1",
        ] {
            let url = url::Url::parse(id).unwrap();
            assert!(!same_host(&url, "mastodon.social"), "{id}");
        }
        let url = url::Url::parse("https://Mastodon.Social./users/a/statuses/1").unwrap();
        assert!(same_host(&url, "mastodon.social"));
    }

    #[test]
    fn another_hosts_id_roots_nothing() {
        // A server answering with another host's ActivityPub id (here someone else's post, as a
        // hostile server could) keeps its own card: no alias of that post, no page elsewhere.
        let evil = Target {
            host: "evil.example".into(),
            id: "1".into(),
        };
        let base = ResourceMeta::new("https://evil.example/@a/1", "mastodon", 3600);
        let doc = card(base.clone(), &evil, 200, &read("status-1.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.source_root, None);
        assert!(!doc.attributes.contains_key("page_url"));
        let mut hostile: Value = serde_json::from_slice(&read("status-1.json")).unwrap();
        hostile["uri"] = "https://www.paypal.com/signin".into();
        let ours = Target {
            host: "mastodon.social".into(),
            id: "1".into(),
        };
        let doc = card(base, &ours, 200, &serde_json::to_vec(&hostile).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(doc.source_root, None);
    }
}
