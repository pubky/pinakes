//! Apps on Apple's App Store, read from the iTunes Lookup API by the app's ID in the link. No
//! key, no account.
//!
//! Every App Store link of one app (any storefront, with or without its name) is an alias of
//! `https://apps.apple.com/app/id{id}`. The lookup asks the United States storefront first, so
//! one root per app shows the US record whenever there is one. When the US storefront does not
//! sell the app, it asks the storefront that sold it at the last read (the card's `storefront`
//! fact, kept with the stored card), then the storefront a link of the app named (the latest
//! seen since the daemon started). A link's storefront counts only once its lookup returned the
//! app: one that does not sell it (an empty answer, or a 400 for a code that is no storefront)
//! is forgotten and never decides the card. Any other failed lookup is a retry. An app no asked
//! storefront sells is missing. The app's `bundleId` is Apple's alone: it is a fact, never the
//! Android package.
use super::{
    kit::{fetch, float, int, json, text, time, web_url, Card, Request},
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{bail, ensure, Context, Result};
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
    // The app's name in the link is any one segment (Japanese names included).
    let name = |n: &str| !n.is_empty() && n.len() <= 600;
    let (cc, id) = match segments.as_slice() {
        [cc, "app", n, id] if storefront(cc) && name(n) => (Some(*cc), id),
        [cc, "app", id] if storefront(cc) => (Some(*cc), id),
        ["app", n, id] if name(n) => (None, id),
        ["app", id] => (None, id),
        _ => return None,
    };
    Some((app_id(id.strip_prefix("id")?)?, cc.map(str::to_owned)))
}

/// The storefront other than the US one that a link of each app named (the latest one seen),
/// until its lookup did not return the app: what the lookup asks when neither the US storefront
/// nor the one the card was read from sells the app. Kept while the daemon runs, up to `LINKED_CAP` apps (at the cap, the
/// smallest app id makes room).
static LINKED: std::sync::Mutex<BTreeMap<String, String>> = std::sync::Mutex::new(BTreeMap::new());
const LINKED_CAP: usize = 10_000;

/// Remember the storefront `uri` names for its app, when it names one other than the US.
fn remember(uri: &str) {
    let Some((id, Some(cc))) = link(uri) else {
        return;
    };
    if cc == "us" {
        return;
    }
    let mut seen = LINKED.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() >= LINKED_CAP && !seen.contains_key(&id) {
        seen.pop_first();
    }
    seen.insert(id, cc);
}

/// The storefront a link of app `id` named, other than the US one.
fn linked(id: &str) -> Option<String> {
    LINKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .cloned()
}

/// Forget storefront `cc` of app `id` once its lookup did not return the app.
fn forget(id: &str, cc: &str) {
    let mut seen = LINKED.lock().unwrap_or_else(|e| e.into_inner());
    if seen.get(id).is_some_and(|c| c == cc) {
        seen.remove(id);
    }
}

/// A storefront code: two lower-case letters.
fn storefront(cc: &str) -> bool {
    cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_lowercase())
}

pub fn page(id: &str) -> String {
    format!("https://apps.apple.com/app/id{id}")
}

/// The lookup of app `id` in the US storefront, or in storefront `cc`.
pub fn request(id: &str, cc: Option<&str>) -> Option<Request> {
    let id = app_id(id)?;
    let url = match cc {
        Some(cc) => {
            if !storefront(cc) {
                return None;
            }
            format!("https://itunes.apple.com/lookup?id={id}&country={cc}")
        }
        None => format!("https://itunes.apple.com/lookup?id={id}"),
    };
    Request::get(&url).map(|r| r.admit(&["text/javascript"]))
}

/// The app in storefront `cc`'s lookup, or `None` when that storefront does not sell it: an
/// answer with no result, or the 400 Apple answers for a country code that is no storefront (the
/// ID was checked before asking, so the code is what it refuses). Any other failure (a body that
/// is not a lookup, a record of another app, another status) is an error, so the read is retried.
fn sold(status: u16, raw: &[u8], id: &str) -> Result<Option<Card>> {
    match status {
        200 => parse(status, raw, id),
        400 => Ok(None),
        s => bail!("unexpected status {s}"),
    }
}

/// The app, or `None` when the lookup finds no app by this ID.
pub fn parse(status: u16, raw: &[u8], id: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let results = v["results"]
        .as_array()
        .context("the answer is not a lookup")?;
    let Some(app) = results.first() else {
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
            "storefront",
        ]
    }
    fn reads_back(&self) -> &'static [&'static str] {
        &["storefront"]
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
        let mut from = None;
        // The US storefront does not sell it: ask the one the card was read from, which decides
        // the card when it answers (`sold`).
        let known = cx
            .stored(uri, "storefront")
            .filter(|cc| storefront(cc) && cc != "us");
        if let (None, Some(cc)) = (&card, &known) {
            let f = fetch(cx, &request(&id, Some(cc)).context("no request")?).await?;
            card = sold(f.status, &f.body, &id)?;
            from = known.clone();
        }
        // Then the one a link named, which counts only once it returned the app: one that does
        // not sell it (`sold`) is forgotten, and a lookup that fails otherwise is a retry that
        // keeps it.
        let linked = cc
            .or_else(|| linked(&id))
            .filter(|cc| cc != "us" && Some(cc) != known.as_ref());
        if let (None, Some(cc)) = (&card, linked) {
            let f = fetch(cx, &request(&id, Some(&cc)).context("no request")?).await?;
            match sold(f.status, &f.body, &id)? {
                Some(found) => {
                    card = Some(found);
                    from = Some(cc);
                }
                None => forget(&id, &cc),
            }
        }
        Ok(match card {
            Some(mut card) => {
                card.detail("storefront", from);
                card.into_doc(cx.card(uri, self), SITE)
            }
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

    /// The stored card's facts of `uri` that a queue job hands its read (`sources::stored`).
    fn stored(uri: &str, cc: &str) -> BTreeMap<String, BTreeMap<String, String>> {
        BTreeMap::from([(
            uri.to_owned(),
            BTreeMap::from([("storefront".to_owned(), cc.to_owned())]),
        )])
    }

    /// An app the US storefront does not sell is read from the storefront a link of it names,
    /// under the one root of the app, and the card keeps that storefront: a later read (after a
    /// restart, with nothing in memory) asks it again. A link's storefront that does not sell the
    /// app, or a code that is no storefront, never decides the card.
    #[tokio::test(start_paused = true)]
    async fn a_storefront_counts_once_it_returned_the_app_and_stays_with_the_card() {
        use crate::model::Status;
        let us = "https://itunes.apple.com/lookup?id=1225135057";
        let ch = "https://itunes.apple.com/lookup?id=1225135057&country=ch";
        let de = "https://itunes.apple.com/lookup?id=1225135057&country=de";
        let zz = "https://itunes.apple.com/lookup?id=1225135057&country=zz";
        let mut routes = answers(&[
            (us, "lookup-1225135057.json"),
            (ch, "lookup-1225135057-ch.json"),
            (de, "lookup-1225135057-de.json"),
        ]);
        routes.push((
            zz,
            400,
            "text/javascript",
            read("lookup-1225135057-zz.json"),
        ));
        let (egress, recorded) = fixture::egress(routes);
        let cx = crate::sources::Context {
            egress: &egress,
            ttl: 3600,
        };
        let root = "https://apps.apple.com/app/id1225135057";
        let asked = |from: usize| recorded.asked()[from..].to_vec();

        // A made-up code in a link: asked once, it does not sell the app (no card yet).
        assert_eq!(
            AppStore
                .alias_root("https://apps.apple.com/zz/app/postfinance-twint/id1225135057")
                .as_deref(),
            Some(root)
        );
        let doc = AppStore.resolve(&cx, root).await.unwrap();
        assert_eq!(doc.status, Status::Missing);
        assert_eq!(asked(0), [us, zz]);
        assert_eq!(linked("1225135057"), None, "forgotten once asked");

        // The link's storefront sells it: the card is its record and keeps the storefront.
        AppStore.alias_root("https://apps.apple.com/ch/app/postfinance-twint/id1225135057");
        let doc = AppStore.resolve(&cx, root).await.unwrap();
        assert_eq!(doc.status, Status::Ok);
        assert_eq!(doc.name.as_deref(), Some("PostFinance TWINT"));
        assert_eq!(doc.attributes["storefront"], "ch");
        assert_eq!(asked(2), [us, ch]);
        // Kept until the card that names it is stored.
        assert_eq!(linked("1225135057").as_deref(), Some("ch"));

        // A fresh process: no link seen, only the stored card's storefront.
        forget("1225135057", "ch");
        let doc = crate::sources::with_stored(stored(root, "ch"), AppStore.resolve(&cx, root))
            .await
            .unwrap();
        assert_eq!(doc.status, Status::Ok);
        assert_eq!(doc.attributes["storefront"], "ch");
        assert_eq!(asked(4), [us, ch]);

        // Links tagged with storefronts that do not sell it leave the card as it is.
        AppStore.alias_root("https://apps.apple.com/zz/app/id1225135057");
        AppStore.alias_root("https://apps.apple.com/de/app/id1225135057");
        let doc = crate::sources::with_stored(stored(root, "ch"), AppStore.resolve(&cx, root))
            .await
            .unwrap();
        assert_eq!(doc.status, Status::Ok);
        assert_eq!(doc.name.as_deref(), Some("PostFinance TWINT"));
        assert_eq!(asked(6), [us, ch]);
    }

    /// When the US storefront sells the app, its record is the card, whichever storefront the
    /// link names; an app no storefront sells is missing, once the US storefront and the one the
    /// card was read from were both asked.
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
            (
                "https://itunes.apple.com/lookup?id=1&country=ch",
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
        assert!(!doc.attributes.contains_key("storefront"));
        // The US record wins over the storefront the card was read from, too.
        let signal = "https://apps.apple.com/app/id874139669";
        let doc = crate::sources::with_stored(stored(signal, "ch"), AppStore.resolve(&cx, signal))
            .await
            .unwrap();
        assert_eq!(doc.name.as_deref(), Some("Signal - Private Messenger"));
        assert!(!doc.attributes.contains_key("storefront"));
        assert_eq!(
            recorded.asked(),
            [
                "https://itunes.apple.com/lookup?id=874139669",
                "https://itunes.apple.com/lookup?id=874139669"
            ]
        );
        let gone = "https://apps.apple.com/app/id1";
        let doc = crate::sources::with_stored(stored(gone, "ch"), AppStore.resolve(&cx, gone))
            .await
            .unwrap();
        assert_eq!(doc.status, crate::model::Status::Missing);
        assert_eq!(
            recorded.asked()[2..],
            [
                "https://itunes.apple.com/lookup?id=1",
                "https://itunes.apple.com/lookup?id=1&country=ch"
            ]
        );
    }

    /// A storefront lookup says "does not sell it" only with an empty answer or the 400 of a code
    /// that is no storefront; a body that is not a lookup, a record of another app or any other
    /// status is a retry, and the link's storefront is kept for it. The stored storefront counts
    /// the same way. Each case is its own app (made-up IDs), so the links seen do not mix.
    #[tokio::test(start_paused = true)]
    async fn only_an_empty_answer_or_an_unknown_storefront_does_not_sell_it() {
        use crate::model::Status;
        const JS: &str = "text/javascript";
        let us = |id: &str| format!("https://itunes.apple.com/lookup?id={id}");
        let de = |id: &str| format!("https://itunes.apple.com/lookup?id={id}&country=de");
        let empty = read("lookup-missing.json");
        let cases: [(&str, u16, Vec<u8>, bool); 8] = [
            ("900000001", 200, empty.clone(), true),
            ("900000002", 400, read("lookup-1225135057-zz.json"), true),
            ("900000003", 200, b"<html>busy</html>".to_vec(), false),
            ("900000004", 200, read("lookup-874139669.json"), false),
            ("900000005", 503, Vec::new(), false),
            ("900000006", 403, Vec::new(), false),
            // JSON that is no lookup, and a 404, which the lookup never answers.
            ("900000007", 200, b"{}".to_vec(), false),
            ("900000008", 404, empty.clone(), false),
        ];
        let urls: Vec<(String, String)> = cases.iter().map(|(id, ..)| (us(id), de(id))).collect();
        let mut routes = Vec::new();
        for ((_, status, body, _), (us, de)) in cases.iter().zip(&urls) {
            routes.push((us.as_str(), 200, JS, empty.clone()));
            routes.push((de.as_str(), *status, JS, body.clone()));
        }
        let (egress, _) = fixture::egress(routes);
        let cx = crate::sources::Context {
            egress: &egress,
            ttl: 3600,
        };
        for (id, status, _, sells_not) in &cases {
            AppStore.alias_root(&format!("https://apps.apple.com/de/app/id{id}"));
            let root = page(id);
            let read = AppStore.resolve(&cx, &root).await;
            if *sells_not {
                assert_eq!(read.unwrap().status, Status::Missing, "{id} {status}");
                assert_eq!(linked(id), None, "{id}: forgotten");
            } else {
                assert!(read.is_err(), "{id} {status}: a retry");
                assert_eq!(linked(id).as_deref(), Some("de"), "{id}: kept");
                // The storefront the card was read from: the same failure is a retry too.
                forget(id, "de");
                let read =
                    crate::sources::with_stored(stored(&root, "de"), AppStore.resolve(&cx, &root))
                        .await;
                assert!(read.is_err(), "{id} {status}: stored storefront, a retry");
            }
        }
        // A stored storefront Apple no longer knows (400) is one that does not sell the app.
        let root = page("900000002");
        let doc = crate::sources::with_stored(stored(&root, "de"), AppStore.resolve(&cx, &root))
            .await
            .unwrap();
        assert_eq!(doc.status, Status::Missing);
    }

    #[test]
    fn missing_mismatched_and_failed() {
        assert_eq!(parse(200, &read("lookup-missing.json"), "1").unwrap(), None);
        assert!(parse(200, &read("lookup-874139669.json"), "874139670").is_err());
        assert!(parse(200, b"<html>", "874139669").is_err());
        assert!(parse(200, b"{}", "874139669").is_err());
        assert!(parse(500, b"", "874139669").is_err());
    }
}
