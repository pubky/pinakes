//! Linux apps on the Snap Store, read from the snapcraft.io v2 info API by the snap's name in the
//! link. No key, no account; the API asks for its `Snap-Device-Series: 16` header.
//!
//! Every Snap Store link of one snap is an alias of `https://snapcraft.io/{name}`.
use super::{
    kit::{fetch, json, text, web_url, Card, Request},
    Declined, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;

pub const SITE: &str = "Snap Store";

/// A snap name, never one of snapcraft.io's own pages (`app_ids`).
fn snap_name(value: &str) -> Option<String> {
    crate::app_ids::snap_name(value).then(|| value.to_owned())
}

/// The snap a Snap Store link names: `snapcraft.io/{name}` and `snapcraft.io/install/{name}/…`.
pub fn from_uri(uri: &str) -> Option<String> {
    let url = web_url(uri)?;
    if !matches!(url.host_str()?, "snapcraft.io" | "www.snapcraft.io") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [name] | ["install", name, _] => snap_name(name),
        _ => None,
    }
}

pub fn page(name: &str) -> String {
    format!("https://snapcraft.io/{name}")
}

pub fn request(name: &str) -> Option<Request> {
    Request::get(&format!(
        "https://api.snapcraft.io/v2/snaps/info/{}?fields=title,summary,publisher,media,website,license,categories",
        snap_name(name)?
    ))
    .map(|r| r.header("Snap-Device-Series", "16"))
}

/// The snap, or `None` when the store has none by this name (404).
pub fn parse(status: u16, raw: &[u8], name: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(v["name"] == name, "the store answered for another snap");
    let snap = &v["snap"];
    let mut card = Card::new("SoftwareApplication", page(name), page(name));
    card.name = text(&snap["title"]).or_else(|| Some(name.to_owned()));
    card.description = text(&snap["summary"]);
    card.image = snap["media"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["type"] == "icon")
        .and_then(|m| text(&m["url"]))
        .filter(|u| web_url(u).is_some());
    card.detail("developer", text(&snap["publisher"]["display-name"]));
    card.detail(
        "category",
        snap["categories"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["featured"] == true)
            .or_else(|| snap["categories"].get(0))
            .and_then(|c| text(&c["name"])),
    );
    card.detail(
        "website",
        text(&snap["website"]).filter(|u| web_url(u).is_some()),
    );
    card.detail("license", text(&snap["license"]));
    Ok(Some(card))
}

/// The source: the snapcraft.io v2 info API.
pub struct SnapStore;

#[async_trait]
impl Source for SnapStore {
    fn name(&self) -> &'static str {
        "snap_store"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "snap_store",
                uri: "https://snapcraft.io/signal-desktop",
                card_type: "SoftwareApplication",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "api.snapcraft.io",
                path: "/v2/snaps/info/signal-desktop",
                query: &[],
                header: Some("Snap-Device-Series"),
                content_type: "application/json",
                file: "info-signal-desktop.json",
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
        &["page_url", "developer", "category", "website", "license"]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("api.snapcraft.io", 1.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://snapcraft.io/signal-desktop",
            card_type: "SoftwareApplication",
        }]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(name) = from_uri(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&name).context("no request")?).await?;
        match parse(f.status, &f.body, &name)? {
            Some(card) => Ok(card.into_doc(cx.card(uri, self), SITE)),
            // No snap by this name: the link may be another page of the site, so the web route
            // reads it and says whether it is gone.
            None => Err(Declined.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("snap_store", name)
    }

    #[test]
    fn every_link_form_of_one_snap() {
        for uri in [
            "https://snapcraft.io/signal-desktop",
            "https://snapcraft.io/signal-desktop/",
            "https://snapcraft.io/install/signal-desktop/ubuntu",
            "https://www.snapcraft.io/signal-desktop?utm_source=x",
        ] {
            assert_eq!(from_uri(uri).as_deref(), Some("signal-desktop"), "{uri}");
        }
        assert_eq!(
            SnapStore
                .alias_root("https://snapcraft.io/install/signal-desktop/ubuntu")
                .as_deref(),
            Some("https://snapcraft.io/signal-desktop")
        );
        assert_eq!(
            SnapStore.alias_root("https://snapcraft.io/signal-desktop"),
            None
        );
        for uri in [
            "https://snapcraft.io/store",
            "https://snapcraft.io/docs",
            "https://snapcraft.io/publisher/snapcrafters",
            "https://snapcraft.io/Signal-Desktop",
            "https://snapcraft.io/-signal",
            "https://snapcraft.io/123",
            "https://snapcraft.io/",
            "https://example.org/signal-desktop",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        for example in SnapStore.examples() {
            assert!(SnapStore.reads(example.uri), "{}", example.uri);
        }
        let request = request("signal-desktop").unwrap();
        assert!(request
            .url
            .as_str()
            .starts_with("https://api.snapcraft.io/v2/snaps/info/signal-desktop?fields="));
        assert_eq!(
            request.headers,
            vec![("Snap-Device-Series", "16".to_owned())]
        );
    }

    #[test]
    fn signal_desktop() {
        let card = parse(200, &read("info-signal-desktop.json"), "signal-desktop")
            .unwrap()
            .unwrap();
        assert_eq!(card.id, "https://snapcraft.io/signal-desktop");
        assert_eq!(card.name.as_deref(), Some("Signal Desktop"));
        assert_eq!(
            card.description.as_deref(),
            Some("Speak Freely - Private Messenger")
        );
        assert!(card
            .image
            .as_deref()
            .is_some_and(|i| i.starts_with("https://dashboard.snapcraft.io/")));
        assert_eq!(card.details["developer"], "Snapcrafters");
        assert_eq!(card.details["category"], "social");
        assert_eq!(card.details["website"], "https://signal.org/");
        assert_eq!(card.details["license"], "AGPL-3.0-only");
        crate::sources::kit::checked(&SnapStore, &card);
    }

    #[test]
    fn missing_mismatched_and_failed() {
        assert_eq!(
            parse(404, &read("info-missing.json"), "no-such-snap-pinakes").unwrap(),
            None
        );
        assert!(parse(200, &read("info-signal-desktop.json"), "signal").is_err());
        assert!(parse(500, b"", "signal-desktop").is_err());
    }

    /// A name the store does not know is no `missing` card: the page gets a web card.
    #[tokio::test(start_paused = true)]
    async fn an_unknown_name_is_declined() {
        let (egress, _) = fixture::egress(vec![(
            "https://api.snapcraft.io/v2/snaps/info/no-such-snap-pinakes?fields=title,summary,publisher,media,website,license,categories",
            404,
            "application/json",
            read("info-missing.json"),
        )]);
        let cx = crate::sources::Context {
            egress: &egress,
            ttl: 3600,
        };
        let answer = SnapStore
            .resolve(&cx, "https://snapcraft.io/no-such-snap-pinakes")
            .await;
        assert!(answer.is_err_and(|e| e.is::<crate::sources::Declined>()));
    }
}
