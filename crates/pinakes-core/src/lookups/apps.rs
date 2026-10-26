//! Apps from a store link: Google Play, F-Droid, the App Store, Flathub, the Snap Store and the
//! Microsoft Store. Each store's own ID is a key (`android:` for Google Play and F-Droid, which
//! share the Android package), and the one Wikidata item carrying it bridges an app's stores:
//! its other store IDs become keys and links, so every store link of one app joins one group.
use super::{plain, web_url, Key};
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
    pub const ALL: [Store; 6] = [
        Store::GooglePlay,
        Store::FDroid,
        Store::AppStore,
        Store::Flathub,
        Store::Snap,
        Store::MicrosoftStore,
    ];
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
        match self {
            Self::GooglePlay | Self::FDroid => package(value).then(|| value.to_owned()),
            Self::AppStore => (plain(value, 20, |c| c.is_ascii_digit()) && !value.starts_with('0'))
                .then(|| value.to_owned()),
            Self::Flathub => (plain(value, 255, |c| {
                c.is_ascii_alphanumeric() || "._-".contains(c)
            }) && value.split('.').count() >= 3)
                .then(|| value.to_owned()),
            Self::Snap => (plain(value, 40, |c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
            }) && !value.starts_with('-'))
            .then(|| value.to_owned()),
            Self::MicrosoftStore => (value.len() == 12
                && value.bytes().all(|b| b.is_ascii_alphanumeric()))
            .then(|| value.to_ascii_uppercase()),
        }
    }
    /// The app's own page on the store.
    pub fn page(self, id: &str) -> String {
        match self {
            Self::GooglePlay => format!("https://play.google.com/store/apps/details?id={id}"),
            Self::FDroid => format!("https://f-droid.org/packages/{id}"),
            Self::AppStore => format!("https://apps.apple.com/app/id{id}"),
            Self::Flathub => format!("https://flathub.org/apps/{id}"),
            Self::Snap => format!("https://snapcraft.io/{id}"),
            Self::MicrosoftStore => format!("https://apps.microsoft.com/detail/{id}"),
        }
    }
}

/// An Android package name: dot-separated segments of letters, digits and underscores.
fn package(value: &str) -> bool {
    plain(value, 200, |c| {
        c.is_ascii_alphanumeric() || "._".contains(c)
    }) && value.contains('.')
        && value
            .split('.')
            .all(|s| s.bytes().next().is_some_and(|b| b.is_ascii_alphabetic()))
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
            let name = |n: &str| plain(n, 200, |c| c.is_ascii_alphanumeric() || "-_.%".contains(c));
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

pub mod also_on {
    use super::{app_from_uri, item_keys, Store};
    use crate::{
        also_on::{self, Context, Source, Subject},
        hubs,
        lookups::{item, wikidata as items},
    };
    use anyhow::Result;
    use async_trait::async_trait;

    /// An app's own store page: its store key, and through the one Wikidata item carrying that
    /// store's ID (read back and compared exactly; an item its successor replaces gives way to
    /// it; several others are ambiguous and give nothing), the item's Q-id, its other store IDs
    /// as keys and its other stores as links.
    pub struct AppStores;

    #[async_trait]
    impl Source for AppStores {
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
            found.push_key(app.key());
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
                    found.push_key(items::key(&hub.qid));
                    for key in item_keys(&raw) {
                        found.push_key(key);
                    }
                    for link in items::links(&raw, items::APP) {
                        found.push_link(link);
                    }
                }
            }
            Ok(found.into_also_on("wikidata"))
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
            assert!(sites(&found).is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            "https://apps.microsoft.com/detail/short",
            "https://apps.apple.com/us/app/vlc/extra/id650377962",
            "https://apps.apple.com/us/story/id650377962",
            "https://f-droid.org/en/docs/packages/org.videolan.vlc",
            "https://f-droid.org/en/categories/packages/x",
            "https://example.org/app/1",
        ] {
            assert_eq!(app_from_uri(uri), None, "{uri}");
        }
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
