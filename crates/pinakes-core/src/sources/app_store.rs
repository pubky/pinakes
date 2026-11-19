//! Apps on Apple's App Store, read from the iTunes Lookup API by the app's ID in the link. No
//! key, no account.
//!
//! Every App Store link of one app (any storefront, with or without its name) is an alias of
//! `https://apps.apple.com/app/id{id}`. The lookup answers for the United States storefront; an
//! app that storefront does not sell is missing. The app's `bundleId` is Apple's alone: it is a
//! fact, never the Android package.
use super::{
    kit::{fetch, float, int, json, text, time, web_url, Card, Request},
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;

pub const SITE: &str = "App Store";

/// An App Store ID: digits, no leading zero (`app_ids`).
fn app_id(value: &str) -> Option<String> {
    crate::app_ids::app_store_id(value).then(|| value.to_owned())
}

/// The app an App Store link names: `apps.apple.com/{cc}/app/{name}/id{id}`, with or without
/// the storefront and the name, and the older `itunes.apple.com` spelling of the same paths.
pub fn from_uri(uri: &str) -> Option<String> {
    let url = web_url(uri)?;
    if !matches!(url.host_str()?, "apps.apple.com" | "itunes.apple.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let country = |cc: &str| cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_lowercase());
    // The app's name in the link is any one segment (Japanese names included).
    let name = |n: &str| !n.is_empty() && n.len() <= 600;
    let id = match segments.as_slice() {
        [cc, "app", n, id] if country(cc) && name(n) => id,
        [cc, "app", id] if country(cc) => id,
        ["app", n, id] if name(n) => id,
        ["app", id] => id,
        _ => return None,
    };
    app_id(id.strip_prefix("id")?)
}

pub fn page(id: &str) -> String {
    format!("https://apps.apple.com/app/id{id}")
}

pub fn request(id: &str) -> Option<Request> {
    Request::get(&format!(
        "https://itunes.apple.com/lookup?id={}",
        app_id(id)?
    ))
    .map(|r| r.admit(&["text/javascript"]))
}

/// The app, or `None` when the lookup finds no app by this ID.
pub fn parse(status: u16, raw: &[u8], id: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let Some(app) = v["results"].as_array().and_then(|r| r.first()) else {
        return Ok(None);
    };
    ensure!(
        int(&app["trackId"]).map(|n| n.to_string()).as_deref() == Some(id),
        "the lookup answered for another app"
    );
    // `software` is an iPhone and iPad app, `mac-software` a Mac app.
    let type_name = match app["kind"].as_str() {
        Some("mac-software") => "SoftwareApplication",
        _ => "MobileApplication",
    };
    let mut card = Card::new(type_name, page(id), page(id));
    card.name = text(&app["trackName"]);
    card.description = text(&app["description"]);
    card.image = text(&app["artworkUrl512"]).or_else(|| text(&app["artworkUrl100"]));
    card.detail(
        "developer",
        text(&app["sellerName"]).or_else(|| text(&app["artistName"])),
    );
    card.detail("category", text(&app["primaryGenreName"]));
    card.detail(
        "website",
        text(&app["sellerUrl"]).filter(|u| web_url(u).is_some()),
    );
    card.detail("bundle_id", text(&app["bundleId"]));
    card.detail("version", text(&app["version"]));
    card.detail("price", text(&app["formattedPrice"]));
    card.detail(
        "rating",
        float(&app["averageUserRating"]).map(|r| (r * 100.0).round() / 100.0),
    );
    card.detail("rating_count", int(&app["userRatingCount"]));
    card.detail("content_rating", text(&app["trackContentRating"]));
    card.detail(
        "released",
        time(&app["releaseDate"]).map(|t| t.to_rfc3339()),
    );
    card.detail(
        "updated",
        time(&app["currentVersionReleaseDate"]).map(|t| t.to_rfc3339()),
    );
    Ok(Some(card))
}

/// The source: the iTunes Lookup API.
pub struct AppStore;

#[async_trait]
impl Source for AppStore {
    fn name(&self) -> &'static str {
        "app_store"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "app_store",
                uri: "https://apps.apple.com/us/app/signal-private-messenger/id874139669",
                card_type: "MobileApplication",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "itunes.apple.com",
                path: "/lookup",
                query: &[("id", "874139669")],
                header: None,
                content_type: "text/javascript",
                file: "lookup-874139669.json",
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
            "bundle_id",
            "version",
            "price",
            "rating",
            "rating_count",
            "content_rating",
            "released",
            "updated",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        // Apple asks for about 20 calls a minute.
        &[("itunes.apple.com", 0.33)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://apps.apple.com/us/app/signal-private-messenger/id874139669",
                card_type: "MobileApplication",
            },
            Example {
                uri: "https://apps.apple.com/app/id874139669",
                card_type: "MobileApplication",
            },
        ]
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
        fixture::read("app_store", name)
    }

    #[test]
    fn every_link_form_of_one_app() {
        for uri in [
            "https://apps.apple.com/us/app/signal-private-messenger/id874139669",
            "https://apps.apple.com/de/app/signal-sicherer-messenger/id874139669?l=en",
            "https://apps.apple.com/app/id874139669",
            "https://apps.apple.com/ch/app/id874139669",
            "https://itunes.apple.com/us/app/signal-private-messenger/id874139669?mt=8",
            // A name in another script.
            "https://apps.apple.com/jp/app/%E3%82%B7%E3%82%B0%E3%83%8A%E3%83%AB/id874139669",
        ] {
            assert_eq!(from_uri(uri).as_deref(), Some("874139669"), "{uri}");
            assert_eq!(
                AppStore.alias_root(uri).as_deref().unwrap_or(uri),
                "https://apps.apple.com/app/id874139669",
                "{uri}"
            );
        }
        assert_eq!(
            AppStore.alias_root("https://apps.apple.com/app/id874139669"),
            None
        );
        for uri in [
            "https://apps.apple.com/us/developer/signal-messenger-llc/id874139672",
            "https://apps.apple.com/us/app/signal/id0874139669",
            "https://apps.apple.com/us/app/signal/874139669",
            "https://apps.apple.com/USA/app/id874139669",
            "https://apps.apple.com/us/app/signal/id874139669/reviews",
            "https://apps.apple.com/",
            "https://example.com/app/id874139669",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        for example in AppStore.examples() {
            assert!(AppStore.reads(example.uri), "{}", example.uri);
        }
        assert_eq!(
            request("874139669").unwrap().url.as_str(),
            "https://itunes.apple.com/lookup?id=874139669"
        );
    }

    #[test]
    fn signal() {
        let card = parse(200, &read("lookup-874139669.json"), "874139669")
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "MobileApplication");
        assert_eq!(card.id, "https://apps.apple.com/app/id874139669");
        assert_eq!(card.name.as_deref(), Some("Signal - Private Messenger"));
        assert!(card
            .description
            .as_deref()
            .is_some_and(|d| d.starts_with("Signal is a messaging app")));
        assert!(card
            .image
            .as_deref()
            .is_some_and(|i| i.starts_with("https://is1-ssl.mzstatic.com/")));
        assert_eq!(card.details["developer"], "Signal Messenger, LLC");
        assert_eq!(card.details["category"], "Social Networking");
        assert_eq!(card.details["website"], "https://signal.org/");
        assert_eq!(card.details["bundle_id"], "org.whispersystems.signal");
        assert_eq!(card.details["price"], "Free");
        assert_eq!(card.details["rating"], 4.73);
        assert_eq!(card.details["released"], "2014-07-29T18:24:15+00:00");
        assert!(card.keys.is_empty(), "the bundle ID joins nothing");
        let doc = crate::sources::kit::checked(&AppStore, &card);
        assert_eq!(doc.attributes["website"], "https://signal.org/");
    }

    #[test]
    fn missing_mismatched_and_failed() {
        assert_eq!(parse(200, &read("lookup-missing.json"), "1").unwrap(), None);
        assert!(parse(200, &read("lookup-874139669.json"), "874139670").is_err());
        assert!(parse(200, b"<html>", "874139669").is_err());
        assert!(parse(500, b"", "874139669").is_err());
    }
}
