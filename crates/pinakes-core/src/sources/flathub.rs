//! Desktop apps on Flathub, read from Flathub's v2 API (the app's AppStream data) by the app ID
//! in the link. No key, no account.
//!
//! Every Flathub link of one app is an alias of `https://flathub.org/apps/{id}`. An app ID is a
//! reverse-DNS name (`org.signal.Signal`) and keeps its case.
use super::{
    kit::{fetch, json, plain, text, texts, web_url, Card, Request},
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;

pub const SITE: &str = "Flathub";

/// A Flathub app ID: at least three dot-separated parts of letters, digits, `_` and `-`.
fn app_id(value: &str) -> Option<String> {
    (plain(value, 255, |c| {
        c.is_ascii_alphanumeric() || "._-".contains(c)
    }) && value.split('.').count() >= 3
        && value.split('.').all(|part| !part.is_empty()))
    .then(|| value.to_owned())
}

/// A Flathub site language (`de`, `pt-BR`, `zh-Hans`).
fn language(s: &str) -> bool {
    let (lang, region) = s.split_once('-').unwrap_or((s, "AA"));
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && (2..=4).contains(&region.len())
        && region.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The app a Flathub link names: `/apps/{id}`, `/apps/details/{id}` and either under a site
/// language (`/de/apps/{id}`).
pub fn from_uri(uri: &str) -> Option<String> {
    let url = web_url(uri)?;
    if !matches!(url.host_str()?, "flathub.org" | "www.flathub.org") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let rest = match segments.as_slice() {
        [lang, rest @ ..] if language(lang) => rest,
        rest => rest,
    };
    match rest {
        ["apps", "details", id] | ["apps", id] => app_id(id),
        _ => None,
    }
}

pub fn page(id: &str) -> String {
    format!("https://flathub.org/apps/{id}")
}

pub fn request(id: &str) -> Option<Request> {
    Request::get(&format!(
        "https://flathub.org/api/v2/appstream/{}",
        app_id(id)?
    ))
}

/// The app, or `None` when Flathub has none by this ID (404).
pub fn parse(status: u16, raw: &[u8], id: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(v["id"] == id, "Flathub answered for another app");
    let mut card = Card::new("SoftwareApplication", page(id), page(id));
    card.name = text(&v["name"]);
    card.description = text(&v["summary"]);
    card.image = text(&v["icon"]).filter(|u| web_url(u).is_some());
    card.detail("developer", text(&v["developer_name"]));
    card.detail("category", texts(&v["categories"]).into_iter().next());
    card.detail(
        "website",
        text(&v["urls"]["homepage"]).filter(|u| web_url(u).is_some()),
    );
    card.detail("license", text(&v["project_license"]));
    card.detail(
        "source_code",
        text(&v["urls"]["vcs_browser"]).filter(|u| web_url(u).is_some()),
    );
    Ok(Some(card))
}

/// The source: Flathub's v2 API.
pub struct Flathub;

#[async_trait]
impl Source for Flathub {
    fn name(&self) -> &'static str {
        "flathub"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "flathub",
                uri: "https://flathub.org/apps/org.signal.Signal",
                card_type: "SoftwareApplication",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "flathub.org",
                path: "/api/v2/appstream/org.signal.Signal",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "appstream-org.signal.Signal.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        from_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = page(&from_uri(uri)?);
        (root != uri).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "developer",
            "category",
            "website",
            "license",
            "source_code",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("flathub.org", 1.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://flathub.org/apps/org.signal.Signal",
            card_type: "SoftwareApplication",
        }]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = from_uri(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&id).context("no request")?).await?;
        Ok(match parse(f.status, &f.body, &id)? {
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
        fixture::read("flathub", name)
    }

    #[test]
    fn every_link_form_of_one_app() {
        for uri in [
            "https://flathub.org/apps/org.signal.Signal",
            "https://flathub.org/apps/details/org.signal.Signal",
            "https://flathub.org/de/apps/org.signal.Signal",
            "https://flathub.org/pt-BR/apps/org.signal.Signal/",
            "https://www.flathub.org/apps/org.signal.Signal",
        ] {
            assert_eq!(from_uri(uri).as_deref(), Some("org.signal.Signal"), "{uri}");
        }
        assert_eq!(
            Flathub
                .alias_root("https://flathub.org/apps/details/org.signal.Signal")
                .as_deref(),
            Some("https://flathub.org/apps/org.signal.Signal")
        );
        assert_eq!(
            Flathub.alias_root("https://flathub.org/apps/org.signal.Signal"),
            None
        );
        for uri in [
            "https://flathub.org/apps/collection/popular",
            "https://flathub.org/apps/signal",
            "https://flathub.org/apps/org..Signal",
            "https://flathub.org/apps/org.signal.Signal/reviews",
            "https://flathub.org/",
            "https://example.org/apps/org.signal.Signal",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        for example in Flathub.examples() {
            assert!(Flathub.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn signal_desktop() {
        let card = parse(
            200,
            &read("appstream-org.signal.Signal.json"),
            "org.signal.Signal",
        )
        .unwrap()
        .unwrap();
        assert_eq!(card.id, "https://flathub.org/apps/org.signal.Signal");
        assert_eq!(card.name.as_deref(), Some("Signal Desktop"));
        assert_eq!(card.description.as_deref(), Some("Private messenger"));
        assert!(card
            .image
            .as_deref()
            .is_some_and(|i| i.starts_with("https://dl.flathub.org/media/")));
        assert_eq!(card.details["developer"], "Signal Foundation");
        assert_eq!(card.details["category"], "Network");
        assert_eq!(card.details["website"], "https://signal.org/");
        assert_eq!(card.details["license"], "AGPL-3.0-only");
        assert_eq!(
            card.details["source_code"],
            "https://github.com/signalapp/Signal-Desktop"
        );
        crate::sources::kit::checked(&Flathub, &card);
    }

    #[test]
    fn missing_mismatched_and_failed() {
        assert_eq!(
            parse(
                404,
                &read("appstream-missing.json"),
                "org.example.NoSuchApp"
            )
            .unwrap(),
            None
        );
        assert!(parse(
            200,
            &read("appstream-org.signal.Signal.json"),
            "org.signal.Other"
        )
        .is_err());
        assert!(parse(500, b"", "org.signal.Signal").is_err());
    }
}
