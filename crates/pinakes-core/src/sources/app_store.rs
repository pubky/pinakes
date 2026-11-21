//! Apps on Apple's App Store, read from the iTunes Lookup API by the app's ID in the link. No
//! key, no account.
//!
//! Every App Store link of one app (any storefront, with or without its name) is an alias of
//! `https://apps.apple.com/app/id{id}`. The lookup asks the United States storefront first; when
//! that storefront does not sell the app, it asks the storefront a link of the app named (the
//! country in the link, remembered since the daemon started), so one root per app shows the US
//! record whenever there is one. An app no asked storefront sells is missing. The app's
//! `bundleId` is Apple's alone: it is a fact, never the Android package.
use super::{
    kit::{fetch, float, int, json, text, time, web_url, Card, Request},
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;

pub const SITE: &str = "App Store";

/// An App Store ID: digits, no leading zero (`app_ids`).
fn app_id(value: &str) -> Option<String> {
    crate::app_ids::app_store_id(value).then(|| value.to_owned())
}

/// The app an App Store link names: `apps.apple.com/{cc}/app/{name}/id{id}`, with or without
/// the storefront and the name, and the older `itunes.apple.com` spelling of the same paths.
pub fn from_uri(uri: &str) -> Option<String> {
    Some(link(uri)?.0)
}

/// The app's ID and the storefront (`cc`) the link names, if it names one.
fn link(uri: &str) -> Option<(String, Option<String>)> {
    let url = web_url(uri)?;
    if !matches!(url.host_str()?, "apps.apple.com" | "itunes.apple.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let country = |cc: &str| cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_lowercase());
    // The app's name in the link is any one segment (Japanese names included).
    let name = |n: &str| !n.is_empty() && n.len() <= 600;
    let (cc, id) = match segments.as_slice() {
        [cc, "app", n, id] if country(cc) && name(n) => (Some(*cc), id),
        [cc, "app", id] if country(cc) => (Some(*cc), id),
        ["app", n, id] if name(n) => (None, id),
        ["app", id] => (None, id),
        _ => return None,
    };
    Some((app_id(id.strip_prefix("id")?)?, cc.map(str::to_owned)))
}

/// The storefront other than the US one that a link of each app named (the latest one seen): what
/// the lookup asks when the US storefront does not sell the app. Kept while the daemon runs, up
/// to `STOREFRONTS_CAP` apps (at the cap, the smallest app id makes room).
static STOREFRONTS: std::sync::Mutex<BTreeMap<String, String>> =
    std::sync::Mutex::new(BTreeMap::new());
const STOREFRONTS_CAP: usize = 10_000;

/// Remember the storefront `uri` names for its app, when it names one other than the US.
fn remember(uri: &str) {
    let Some((id, Some(cc))) = link(uri) else {
        return;
    };
    if cc == "us" {
        return;
    }
    let mut seen = STOREFRONTS.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() >= STOREFRONTS_CAP && !seen.contains_key(&id) {
        seen.pop_first();
    }
    seen.insert(id, cc);
}

/// The storefront a link of app `id` named, other than the US one.
fn storefront(id: &str) -> Option<String> {
    let seen = STOREFRONTS.lock().unwrap_or_else(|e| e.into_inner());
    seen.get(id).cloned()
}

pub fn page(id: &str) -> String {
    format!("https://apps.apple.com/app/id{id}")
}

/// The lookup of app `id` in the US storefront, or in storefront `cc`.
pub fn request(id: &str, cc: Option<&str>) -> Option<Request> {
    let id = app_id(id)?;
    let url = match cc {
        Some(cc) => {
            if cc.len() != 2 || !cc.bytes().all(|b| b.is_ascii_lowercase()) {
                return None;
            }
            format!("https://itunes.apple.com/lookup?id={id}&country={cc}")
        }
        None => format!("https://itunes.apple.com/lookup?id={id}"),
    };
    Request::get(&url).map(|r| r.admit(&["text/javascript"]))
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
        remember(uri);
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
        let Some((id, cc)) = link(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&id, None).context("no request")?).await?;
        let mut card = parse(f.status, &f.body, &id)?;
        // The US storefront does not sell it: ask the one a link of the app named.
        if card.is_none() {
            if let Some(cc) = cc.or_else(|| storefront(&id)).filter(|cc| cc != "us") {
                let f = fetch(cx, &request(&id, Some(&cc)).context("no request")?).await?;
                card = parse(f.status, &f.body, &id)?;
            }
        }
        Ok(match card {
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
            request("874139669", None).unwrap().url.as_str(),
            "https://itunes.apple.com/lookup?id=874139669"
        );
        assert_eq!(
            request("874139669", Some("ch")).unwrap().url.as_str(),
            "https://itunes.apple.com/lookup?id=874139669&country=ch"
        );
        assert!(request("874139669", Some("CH&x=1")).is_none());
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

    fn answers(routes: &[(&'static str, &str)]) -> Vec<(&'static str, u16, &'static str, Vec<u8>)> {
        routes
            .iter()
            .map(|(url, file)| (*url, 200, "text/javascript", read(file)))
            .collect()
    }

    /// An app the US storefront does not sell is read from the storefront its link names, under
    /// the one root of the app.
    #[tokio::test(start_paused = true)]
    async fn an_app_the_us_storefront_does_not_sell_is_read_from_the_links_storefront() {
        let (egress, recorded) = fixture::egress(answers(&[
            (
                "https://itunes.apple.com/lookup?id=1225135057",
                "lookup-1225135057.json",
            ),
            (
                "https://itunes.apple.com/lookup?id=1225135057&country=ch",
                "lookup-1225135057-ch.json",
            ),
        ]));
        let cx = crate::sources::Context {
            egress: &egress,
            ttl: 3600,
        };
        let link = "https://apps.apple.com/ch/app/postfinance-twint/id1225135057";
        let root = AppStore.alias_root(link).unwrap();
        assert_eq!(root, "https://apps.apple.com/app/id1225135057");
        // The root is read (an alias never is): the storefront comes from the link seen.
        let doc = AppStore.resolve(&cx, &root).await.unwrap();
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.name.as_deref(), Some("PostFinance TWINT"));
        assert_eq!(
            recorded.asked(),
            [
                "https://itunes.apple.com/lookup?id=1225135057",
                "https://itunes.apple.com/lookup?id=1225135057&country=ch"
            ]
        );
    }

    /// When the US storefront sells the app, its record is the card, whichever storefront the
    /// link names; an app no storefront sells is missing.
    #[tokio::test(start_paused = true)]
    async fn the_us_record_wins_and_an_app_nobody_sells_is_missing() {
        let (egress, recorded) = fixture::egress(answers(&[
            (
                "https://itunes.apple.com/lookup?id=874139669",
                "lookup-874139669.json",
            ),
            (
                "https://itunes.apple.com/lookup?id=1",
                "lookup-missing.json",
            ),
        ]));
        let cx = crate::sources::Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = AppStore
            .resolve(
                &cx,
                "https://apps.apple.com/de/app/signal-sicherer-messenger/id874139669",
            )
            .await
            .unwrap();
        assert_eq!(doc.name.as_deref(), Some("Signal - Private Messenger"));
        assert_eq!(
            recorded.asked(),
            ["https://itunes.apple.com/lookup?id=874139669"]
        );
        let doc = AppStore
            .resolve(&cx, "https://apps.apple.com/app/id1")
            .await
            .unwrap();
        assert_eq!(doc.status, crate::model::Status::Missing);
    }

    #[test]
    fn missing_mismatched_and_failed() {
        assert_eq!(parse(200, &read("lookup-missing.json"), "1").unwrap(), None);
        assert!(parse(200, &read("lookup-874139669.json"), "874139670").is_err());
        assert!(parse(200, b"<html>", "874139669").is_err());
        assert!(parse(500, b"", "874139669").is_err());
    }
}
