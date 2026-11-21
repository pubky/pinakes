//! Apps from a store link: Google Play, F-Droid, the App Store, Flathub, the Snap Store and the
//! Microsoft Store. Each store's own ID is a key (`android:` for Google Play and F-Droid, which
//! share the Android package), and the one Wikidata item carrying it bridges an app's stores:
//! its other store IDs become keys and links, so every store link of one app joins one group.
use super::{web_url, Key};
use serde_json::Value;

/// A store an app is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    GooglePlay,
    FDroid,
    AppStore,
    Flathub,
    Snap,
    MicrosoftStore,
}
impl Store {
    /// The store's name as shown to people.
    pub fn name(self) -> &'static str {
        match self {
            Self::GooglePlay => "Google Play",
            Self::FDroid => "F-Droid",
            Self::AppStore => "App Store",
            Self::Flathub => "Flathub",
            Self::Snap => "Snap Store",
            Self::MicrosoftStore => "Microsoft Store",
        }
    }
    /// The Wikidata property holding the app's ID on this store.
    pub fn property(self) -> &'static str {
        match self {
            Self::GooglePlay => "P3418",
            Self::FDroid => "P3597",
            Self::AppStore => "P3861",
            Self::Flathub => "P4655",
            Self::Snap => "P4435",
            Self::MicrosoftStore => "P5885",
        }
    }
    /// The key namespace of the store's ID. Google Play and F-Droid both name the Android
    /// package: the same app.
    pub fn namespace(self) -> &'static str {
        match self {
            Self::GooglePlay | Self::FDroid => "android",
            Self::AppStore => "appstore",
            Self::Flathub => "flathub",
            Self::Snap => "snap",
            Self::MicrosoftStore => "msstore",
        }
    }
    /// The store's ID in its own spelling, or `None` for a value that is no such ID.
    pub fn id(self, value: &str) -> Option<String> {
        use crate::app_ids;
        let valid = match self {
            Self::GooglePlay | Self::FDroid => app_ids::android_package(value),
            Self::AppStore => app_ids::app_store_id(value),
            Self::Flathub => app_ids::flathub_id(value),
            Self::Snap => app_ids::snap_name(value),
            Self::MicrosoftStore => return app_ids::msstore_id(value),
        };
        valid.then(|| value.to_owned())
    }
}

/// An app on a store, from its own page's link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub store: Store,
    pub id: String,
}
impl App {
    pub fn key(&self) -> Key {
        Key::new(self.store.namespace(), &self.id)
    }
}

/// An F-Droid site language (`en`, `fr`, `zh_Hant`, `pt_BR`).
fn fdroid_language(s: &str) -> bool {
    let (lang, region) = s.split_once(['_', '-']).unwrap_or((s, "AA"));
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && (2..=4).contains(&region.len())
        && region.bytes().all(|b| b.is_ascii_alphabetic())
}

/// The app an app's own store page names; never a page under it (reviews, screenshots, a
/// developer's page) or a store's other pages.
pub fn app_from_uri(uri: &str) -> Option<App> {
    let url = web_url(uri)?;
    let host = url.host_str()?.trim_start_matches("www.");
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let (store, value): (Store, String) = match (host, segments.as_slice()) {
        // `/{cc}/app/{name}/id{N}`, with or without the storefront and the name.
        ("apps.apple.com", rest) => {
            let country = |cc: &str| cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_lowercase());
            // The app's name in the link is any one segment (Japanese names included).
            let name = |n: &str| !n.is_empty() && n.len() <= 600;
            let id = match rest {
                [cc, "app", n, id] if country(cc) && name(n) => id,
                [cc, "app", id] if country(cc) => id,
                ["app", n, id] if name(n) => id,
                ["app", id] => id,
                _ => return None,
            };
            (Store::AppStore, id.strip_prefix("id")?.to_owned())
        }
        // `details?id=…`; `hl=`, `gl=` and the like are the page's language, not another app.
        ("play.google.com", ["store", "apps", "details"]) => (Store::GooglePlay, query("id")?),
        // The package's page, with or without a language path (`/en/`, `/zh_Hant/`).
        ("f-droid.org", ["packages", id]) => (Store::FDroid, (*id).to_owned()),
        ("f-droid.org", [lang, "packages", id]) if fdroid_language(lang) => {
            (Store::FDroid, (*id).to_owned())
        }
        ("flathub.org", ["apps", "details", id]) | ("flathub.org", ["apps", id]) => {
            (Store::Flathub, (*id).to_owned())
        }
        ("snapcraft.io", [id]) => (Store::Snap, (*id).to_owned()),
        // `apps.microsoft.com/detail/{id}`, `/store/detail/{name}/{id}`, and the older
        // `microsoft.com/store/apps/{id}` and `microsoft.com/{locale}/p/{name}/{id}`.
        ("apps.microsoft.com", ["detail", id])
        | ("apps.microsoft.com", ["store", "detail", _, id])
        | ("microsoft.com", ["store", "apps", id])
        | ("microsoft.com", ["store", "apps", _, id])
        | ("microsoft.com", [_, "p", _, id]) => (Store::MicrosoftStore, (*id).to_owned()),
        _ => return None,
    };
    let id = store.id(&value)?;
    Some(App { store, id })
}

/// The store IDs an app's Wikidata item names, one value each at its best rank, as keys. An
/// item whose Google Play and F-Droid packages differ names no `android:` key: either could be
/// another build.
pub fn item_keys(item: &Value) -> Vec<Key> {
    let one = |store: Store| -> Option<String> {
        let mut values: Vec<&str> = super::wikidata::best_values(item, store.property());
        values.sort_unstable();
        values.dedup();
        match values.as_slice() {
            [value] => store.id(value),
            _ => None,
        }
    };
    let mut keys = vec![];
    let android = match (one(Store::GooglePlay), one(Store::FDroid)) {
        (Some(a), Some(b)) if a != b => None,
        (a, b) => a.or(b),
    };
    if let Some(package) = android {
        keys.push(Key::new("android", &package));
    }
    for store in [
        Store::AppStore,
        Store::Flathub,
        Store::Snap,
        Store::MicrosoftStore,
    ] {
        if let Some(id) = one(store) {
            keys.push(Key::new(store.namespace(), &id));
        }
    }
    keys
}

/// The fact an app's store card is compared by when no Wikidata item joins its stores
/// (`app_match`): its official website's host (without `www.`) and its name in lower case, with
/// every run of other characters than letters and digits as one space ("signal.org signal private
/// messenger"). Two store cards with the same one are possibly the same app (`maybe_same`): a
/// hint, never a join. `None` without a website or a name.
pub fn app_match(name: &str, website: &str) -> Option<String> {
    let host = web_url(website)?.host_str()?.to_ascii_lowercase();
    let host = host.trim_start_matches("www.").trim_end_matches('.');
    let name = name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let value = format!("{host} {name}");
    (!host.is_empty() && !name.is_empty() && value.len() <= 512).then_some(value)
}

/// This family in the lookups' table ([`super::FAMILIES`]).
pub struct Lookups;
impl super::Family for Lookups {
    fn name(&self) -> &'static str {
        "apps"
    }
    fn sources(&self) -> Vec<std::sync::Arc<dyn crate::also_on::Source>> {
        vec![std::sync::Arc::new(also_on::AppStores)]
    }
}

/// The forms of key the app stores source records (`also_on::Source::key_forms`).
pub const KEY_FORMS: &[(&str, &str)] = &[
    ("android", "org.thoughtcrime.securesms"),
    ("appstore", "874139669"),
    ("flathub", "org.signal.Signal"),
    ("snap", "signal-desktop"),
    ("msstore", "9NBLGGH4N8RD"),
    ("wikidata", "Q42"),
];
pub mod also_on {
    use super::{app_from_uri, app_match, item_keys, Store};
    use crate::{
        also_on::{self, Context, Source, Subject},
        hubs,
        lookups::{item, wikidata as items},
    };
    use anyhow::Result;
    use async_trait::async_trait;

    /// An app's own store page: its store key, and through the one Wikidata item carrying that
    /// store's ID (read back and compared exactly; an item its successor replaces gives way to
    /// it; several others are ambiguous and give nothing) its other stores as links, and, only
    /// when the item carries no other ID on that store, the item's Q-id and its other store IDs
    /// as keys.
    pub struct AppStores;

    #[async_trait]
    impl Source for AppStores {
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            crate::lookups::apps::KEY_FORMS
        }
        fn single_namespaces(&self) -> &'static [&'static str] {
            &["android", "appstore", "flathub", "snap", "msstore"]
        }
        fn search_key(&self, namespace: &str, value: &str) -> Option<String> {
            let store = match namespace {
                "android" => Store::GooglePlay,
                "appstore" => Store::AppStore,
                "flathub" => Store::Flathub,
                "snap" => Store::Snap,
                "msstore" => Store::MicrosoftStore,
                _ => return None,
            };
            Some(format!("{namespace}:{}", store.id(value)?))
        }
        /// An app's own store page only.
        fn service_name(&self, _host: &str, url: &url::Url) -> Option<&'static str> {
            app_from_uri(url.as_str()).map(|app| app.store.name())
        }
        fn id(&self) -> &'static str {
            "app_stores"
        }
        fn applies(&self, subject: &Subject) -> bool {
            app_from_uri(&subject.uri).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            let Some(app) = app_from_uri(&subject.uri) else {
                return Ok(also_on::Found::default());
            };
            let mut found = crate::lookups::Found::default();
            let pid = app.store.property();
            // An answer over its read's cap is no answer: asking again would read the same
            // megabytes.
            let hub = match hubs::wikidata::item_with(&cx.egress, pid, &app.id).await {
                Err(error) if crate::resolver::egress::over_cap(&error) => None,
                Ok(hubs::wikidata::Found::One(hub)) => Some(hub),
                Ok(_) => None,
                Err(error) => return Err(error),
            };
            if let Some(hub) = hub {
                if let Some((raw, _)) = item(cx, &hub.qid).await? {
                    // An item naming several IDs on this store is not this one page's: links
                    // only.
                    if hubs::wikidata::only_value(&hub, pid, &app.id) {
                        found.push_key(items::key(&hub.qid));
                        for key in item_keys(&raw) {
                            found.push_key(key);
                        }
                    }
                    for link in items::links(&raw, items::APP) {
                        found.push_link(link);
                    }
                }
            }
            let mut found = found.into_also_on("wikidata");
            // The store's own key comes from its link, not from Wikidata.
            let own = app.key();
            found.keys.retain(|k| k.key != own.as_str());
            found.keys.insert(
                0,
                also_on::Key {
                    key: own.as_str().to_owned(),
                    via: "app_stores".into(),
                },
            );
            // The card's own name and website, for the possibly-same-app hint.
            if let Some(doc) = &subject.doc {
                let website = doc.attributes.get("website");
                if let Some(value) = doc
                    .name
                    .as_deref()
                    .zip(website)
                    .and_then(|(n, w)| app_match(n, w))
                {
                    found.facts.push(also_on::Fact::new("app_match", value));
                }
            }
            Ok(found)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::lookups::fixture::{at, keys, recorded, sites};

        /// Signal's Google Play page finds Signal's item although TextSecure's carries the same
        /// package (Signal replaces it): the item's other stores become keys and links. Its
        /// App Store page finds the same item from its own ID.
        #[tokio::test(start_paused = true)]
        async fn a_store_link_bridges_to_the_apps_other_stores() {
            let cx = recorded(vec![]);
            let play = at(
                "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms&hl=en",
            );
            assert!(AppStores.applies(&play));
            let found = AppStores.lookup(&cx, &play).await.unwrap();
            let mut k = keys(&found);
            k.sort_unstable();
            assert_eq!(
                k,
                [
                    "android:org.thoughtcrime.securesms",
                    "appstore:874139669",
                    "flathub:org.signal.Signal",
                    "snap:signal-desktop",
                    "wikidata:Q19718090",
                ]
            );
            let s = sites(&found);
            for site in [
                "App Store https://apps.apple.com/app/id874139669",
                "Flathub https://flathub.org/apps/org.signal.Signal",
                "Snap Store https://snapcraft.io/signal-desktop",
            ] {
                assert!(s.contains(&site.to_owned()), "{site} in {s:?}");
            }
            let apple = at("https://apps.apple.com/us/app/signal-private-messenger/id874139669");
            let found = AppStores.lookup(&cx, &apple).await.unwrap();
            assert!(
                keys(&found).contains(&"wikidata:Q19718090"),
                "{:?}",
                keys(&found)
            );
            assert!(keys(&found).contains(&"android:org.thoughtcrime.securesms"));
            let via = |key: &str| {
                found
                    .keys
                    .iter()
                    .find(|k| k.key == key)
                    .map(|k| k.via.clone())
            };
            // The App Store link's own key is the link's; the package is Wikidata's word.
            assert_eq!(via("appstore:874139669").as_deref(), Some("app_stores"));
            assert_eq!(
                via("android:org.thoughtcrime.securesms").as_deref(),
                Some("wikidata")
            );
        }

        /// AntennaPod's Google Play page finds its item, whose F-Droid page is a link.
        #[tokio::test(start_paused = true)]
        async fn a_play_link_finds_the_apps_f_droid_page() {
            let cx = recorded(vec![]);
            let found = AppStores
                .lookup(
                    &cx,
                    &at("https://play.google.com/store/apps/details?id=de.danoeh.antennapod&hl=en"),
                )
                .await
                .unwrap();
            assert!(
                keys(&found).contains(&"wikidata:Q16927599"),
                "{:?}",
                keys(&found)
            );
            assert!(keys(&found).contains(&"android:de.danoeh.antennapod"));
            assert!(
                sites(&found).contains(
                    &"F-Droid https://f-droid.org/packages/de.danoeh.antennapod".to_owned()
                ),
                "{:?}",
                sites(&found)
            );
        }

        /// A package Wikidata does not know keeps its own key alone.
        #[tokio::test(start_paused = true)]
        async fn a_store_link_wikidata_lacks_keeps_its_own_key() {
            let cx = recorded(vec![]);
            let found = AppStores
                .lookup(
                    &cx,
                    &at("https://f-droid.org/en/packages/org.example.unlisted/"),
                )
                .await
                .unwrap();
            assert_eq!(keys(&found), ["android:org.example.unlisted"]);
            assert_eq!(
                found.keys[0].via, "app_stores",
                "the link's own key, not Wikidata's"
            );
            assert!(sites(&found).is_empty());
        }

        /// An item that names two Google Play packages is not either page's: neither its Q-id nor
        /// its other store IDs are keys of the page, only its own store key. The same item with
        /// the one package keys it.
        #[tokio::test(start_paused = true)]
        async fn an_item_with_two_packages_on_the_store_gives_no_item_key() {
            use crate::lookups::fixture::{context, read, JSON};
            let search = hubs::wikidata::search_lookup("P3418", "de.danoeh.antennapod").unwrap();
            let entity = hubs::wikidata::entities_lookup(&["Q16927599".to_owned()]);
            let item = |second: bool| {
                let mut v: serde_json::Value =
                    serde_json::from_slice(&read("wikidata-q16927599.json.gz")).unwrap();
                // A Flathub ID: a key of the item's other store.
                v["entities"]["Q16927599"]["claims"]["P4655"] = serde_json::json!([{"mainsnak": {"snaktype": "value",
                    "property": "P4655",
                    "datavalue": {"value": "de.danoeh.antennapod", "type": "string"},
                    "datatype": "external-id"}, "type": "statement", "rank": "normal"}]);
                if second {
                    v["entities"]["Q16927599"]["claims"]["P3418"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"mainsnak": {"snaktype": "value",
                            "property": "P3418",
                            "datavalue": {"value": "de.danoeh.antennapod.debug", "type": "string"},
                            "datatype": "external-id"}, "type": "statement", "rank": "normal"}));
                }
                serde_json::to_vec(&v).unwrap()
            };
            let page = at("https://play.google.com/store/apps/details?id=de.danoeh.antennapod");
            for (second, keyed) in [(false, true), (true, false)] {
                let cx = context(vec![
                    (
                        search.as_str(),
                        200,
                        JSON,
                        read("wikidata-search-p3418-antennapod.json"),
                    ),
                    (entity.as_str(), 200, JSON, item(second)),
                ]);
                let found = AppStores.lookup(&cx, &page).await.unwrap();
                let k = keys(&found);
                assert_eq!(k[0], "android:de.danoeh.antennapod", "{k:?}");
                assert_eq!(k.contains(&"wikidata:Q16927599"), keyed, "{second}: {k:?}");
                assert_eq!(
                    k.contains(&"flathub:de.danoeh.antennapod"),
                    keyed,
                    "{second}: {k:?}"
                );
                if !keyed {
                    assert_eq!(k, ["android:de.danoeh.antennapod"]);
                }
                // Its other stores stay links either way.
                assert!(
                    sites(&found).contains(
                        &"F-Droid https://f-droid.org/packages/de.danoeh.antennapod".to_owned()
                    ),
                    "{second}: {:?}",
                    sites(&found)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn app_match_is_the_website_host_and_the_plain_name() {
        let play = app_match("Signal Private Messenger", "https://signal.org");
        assert_eq!(play.as_deref(), Some("signal.org signal private messenger"));
        assert_eq!(
            app_match("Signal - Private Messenger", "https://www.Signal.org/"),
            play
        );
        assert_eq!(
            app_match("Über·App", "https://example.org/app").as_deref(),
            Some("example.org über app")
        );
        assert_eq!(app_match("Signal", "mailto:a@signal.org"), None);
        assert_eq!(app_match(" - ", "https://signal.org"), None);
    }

    #[test]
    fn store_links() {
        for (uri, store, id) in [
            (
                "https://apps.apple.com/us/app/signal-private-messenger/id874139669",
                Store::AppStore,
                "874139669",
            ),
            ("https://apps.apple.com/app/id650377962", Store::AppStore, "650377962"),
            (
                "https://apps.apple.com/us/app/vlc-media-player/id650377962?see-all=reviews",
                Store::AppStore,
                "650377962",
            ),
            (
                "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms",
                Store::GooglePlay,
                "org.thoughtcrime.securesms",
            ),
            // The page's language and country are the same app.
            (
                "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms&hl=de&gl=CH",
                Store::GooglePlay,
                "org.thoughtcrime.securesms",
            ),
            (
                "https://f-droid.org/en/packages/de.danoeh.antennapod/",
                Store::FDroid,
                "de.danoeh.antennapod",
            ),
            (
                "https://f-droid.org/zh_Hant/packages/org.videolan.vlc",
                Store::FDroid,
                "org.videolan.vlc",
            ),
            (
                "https://flathub.org/apps/org.mozilla.firefox",
                Store::Flathub,
                "org.mozilla.firefox",
            ),
            (
                "https://flathub.org/apps/details/org.signal.Signal",
                Store::Flathub,
                "org.signal.Signal",
            ),
            ("https://snapcraft.io/firefox", Store::Snap, "firefox"),
            (
                "https://apps.microsoft.com/detail/9nblggh4nns1",
                Store::MicrosoftStore,
                "9NBLGGH4NNS1",
            ),
            (
                "https://www.microsoft.com/en-us/p/whatsapp/9nksqgp7f2nh",
                Store::MicrosoftStore,
                "9NKSQGP7F2NH",
            ),
        ] {
            assert_eq!(
                app_from_uri(uri),
                Some(App {
                    store,
                    id: id.into()
                }),
                "{uri}"
            );
        }
        for uri in [
            "https://play.google.com/store/apps/details",
            "https://play.google.com/store/apps/details?id=a%20b",
            "https://play.google.com/store/apps/developer?id=Signal",
            "https://apps.apple.com/us/app/x/idabc",
            "https://apps.apple.com/us/developer/videolan/id1234",
            "https://apps.apple.com/us/app/vlc-media-player/id650377962/reviews",
            "https://f-droid.org/en/packages/org.videolan.vlc/screenshots",
            "https://f-droid.org/repo/packages/org.videolan.vlc",
            "https://flathub.org/apps/collection/popular",
            "https://snapcraft.io/docs/x",
            "https://snapcraft.io/Firefox",
            // snapcraft.io's own pages and names no snap can have.
            "https://snapcraft.io/store",
            "https://snapcraft.io/blog",
            "https://snapcraft.io/docs",
            "https://snapcraft.io/123",
            "https://snapcraft.io/signal-",
            "https://flathub.org/apps/org..Signal",
            "https://apps.microsoft.com/detail/short",
            "https://apps.apple.com/us/app/vlc/extra/id650377962",
            "https://apps.apple.com/us/story/id650377962",
            "https://f-droid.org/en/docs/packages/org.videolan.vlc",
            "https://f-droid.org/en/categories/packages/x",
            "https://example.org/app/1",
        ] {
            assert_eq!(app_from_uri(uri), None, "{uri}");
        }
        // An App Store link names its app in any script: the ID is the app.
        assert_eq!(
            app_from_uri(
                "https://apps.apple.com/jp/app/%E3%83%A2%E3%83%B3%E3%82%B9%E3%83%88/id1015862326"
            )
            .map(|a| a.key()),
            Some(Key::new("appstore", "1015862326"))
        );
        // Google Play and F-Droid name one Android package: the same key.
        assert_eq!(
            app_from_uri("https://f-droid.org/packages/de.danoeh.antennapod").map(|a| a.key()),
            app_from_uri("https://play.google.com/store/apps/details?id=de.danoeh.antennapod")
                .map(|a| a.key())
        );
    }

    /// Only an app's own store page is named after its store; the store's other pages are not.
    #[test]
    fn only_an_apps_own_page_is_named_after_its_store() {
        for (uri, name) in [
            (
                "https://apps.apple.com/us/app/vlc-media-player/id650377962",
                Some("App Store"),
            ),
            ("https://flathub.org/apps/org.videolan.VLC", Some("Flathub")),
            ("https://snapcraft.io/vlc", Some("Snap Store")),
            (
                "https://play.google.com/store/apps/details?id=io.anuke.mindustry",
                Some("Google Play"),
            ),
            (
                "https://f-droid.org/en/packages/io.anuke.mindustry/",
                Some("F-Droid"),
            ),
            (
                "https://apps.microsoft.com/detail/9nblggh4nns1",
                Some("Microsoft Store"),
            ),
            ("https://play.google.com/store/apps", None),
            ("https://play.google.com/store/apps/dev?id=1", None),
            ("https://f-droid.org/en/docs/", None),
            ("https://apps.apple.com/us/developer/videolan/id1234", None),
        ] {
            assert_eq!(crate::lookups::service_name(uri), name, "{uri}");
        }
        use crate::lookups::search_key;
        assert_eq!(
            search_key("android", "org.thoughtcrime.securesms").as_deref(),
            Some("android:org.thoughtcrime.securesms")
        );
        assert_eq!(
            search_key("msstore", "9nblggh4nns1").as_deref(),
            Some("msstore:9NBLGGH4NNS1")
        );
        assert_eq!(search_key("appstore", "x"), None);
    }

    #[test]
    fn an_items_store_ids_are_keys_one_value_each() {
        let claim = |v: &str| json!({"rank": "normal", "mainsnak": {"snaktype": "value", "datatype": "external-id", "datavalue": {"value": v}}});
        let item = json!({"claims": {
            "P3418": [claim("org.thoughtcrime.securesms")],
            "P3861": [claim("874139669")],
            "P4655": [claim("org.signal.Signal")],
            "P4435": [claim("signal-desktop"), claim("signal-beta")],
        }});
        let keys: Vec<String> = item_keys(&item).iter().map(|k| k.to_string()).collect();
        // Two snap names are ambiguous: no snap key.
        assert_eq!(
            keys,
            [
                "android:org.thoughtcrime.securesms",
                "appstore:874139669",
                "flathub:org.signal.Signal",
            ]
        );
        // Google Play and F-Droid naming different packages: neither is the app's.
        let split = json!({"claims": {"P3418": [claim("a.b.c")], "P3597": [claim("a.b.d")]}});
        assert!(item_keys(&split).is_empty());
        let agree = json!({"claims": {"P3418": [claim("a.b.c")], "P3597": [claim("a.b.c")]}});
        assert_eq!(item_keys(&agree).len(), 1);
    }
}
