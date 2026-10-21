//! Games from a Steam appid: ProtonDB (its summary is the existence check), PCGamingWiki (its
//! appid redirect names the page), SteamDB (a template; never fetched), and GOG through GOG
//! GamesDB, keeping only its `gog` and `steam` rows.
use super::{body, plain, web_url, Key, Link, Request};
use anyhow::{Context, Result};
use serde_json::Value;

/// A Steam appid: a positive number of up to ten digits.
pub fn appid(value: &str) -> Option<u64> {
    number(value, 10)
}
/// A positive number of up to `max` digits, without leading zeros.
fn number(value: &str, max: usize) -> Option<u64> {
    (plain(value, max, |c| c.is_ascii_digit()) && !value.starts_with('0'))
        .then(|| value.parse().ok())
        .flatten()
}

/// The appid of a game's own page: its Steam store page (`/app/{id}`, with or without its name),
/// its Steam Community hub, SteamDB or ProtonDB page (`/app/{id}`), or PCGamingWiki's appid
/// lookup. A page under the game (a discussion, a review, a guide, workshop or news items, a
/// SteamDB subpage) is another object and names no game.
pub fn appid_from_uri(uri: &str) -> Option<u64> {
    let url = web_url(uri)?;
    let host = url.host_str()?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let slug = |s: &str| plain(s, 200, |c| c.is_ascii_alphanumeric() || "_-.%".contains(c));
    match (host, segments.as_slice()) {
        ("store.steampowered.com", ["app", id]) => appid(id),
        ("store.steampowered.com", ["app", id, name]) if slug(name) => appid(id),
        (
            "steamcommunity.com" | "steamdb.info" | "www.protondb.com" | "protondb.com",
            ["app", id],
        ) => appid(id),
        ("www.pcgamingwiki.com" | "pcgamingwiki.com", ["api", "appid.php"]) => url
            .query_pairs()
            .find(|(k, _)| k == "appid")
            .and_then(|(_, v)| appid(&v)),
        _ => None,
    }
}

pub fn steam_key(appid: u64) -> Key {
    Key::new("steam", &appid.to_string())
}
pub fn gog_key(product: u64) -> Key {
    Key::new("gog", &product.to_string())
}

/// The links made from the appid alone: the Steam store page and SteamDB.
pub fn templates(appid: u64) -> Vec<Link> {
    vec![
        Link::new(
            "Steam",
            format!("https://store.steampowered.com/app/{appid}/"),
            "steam",
        ),
        Link::new(
            "SteamDB",
            format!("https://steamdb.info/app/{appid}/"),
            "steam",
        ),
    ]
}

pub fn protondb_request(appid: u64) -> Option<Request> {
    Request::get(&format!(
        "https://www.protondb.com/api/v1/reports/summaries/{appid}.json"
    ))
}

/// ProtonDB's page, when it has a summary for the game.
pub fn parse_protondb(status: u16, raw: &[u8], appid: u64) -> Result<Option<Link>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let summary: Value = serde_json::from_slice(raw)?;
    Ok(summary["tier"].as_str().map(|_| {
        Link::new(
            "ProtonDB",
            format!("https://www.protondb.com/app/{appid}"),
            "protondb",
        )
    }))
}

/// PCGamingWiki's appid lookup. Its redirect is the answer, so it is not followed.
pub fn pcgamingwiki_request(appid: u64) -> Option<Request> {
    let mut r = Request::get(&format!(
        "https://www.pcgamingwiki.com/api/appid.php?appid={appid}"
    ))?;
    r.follow_redirects = false;
    Some(r)
}

/// The wiki page the lookup redirects to; an unknown appid answers without a redirect.
pub fn parse_pcgamingwiki(status: u16, location: Option<&str>) -> Result<Option<Link>> {
    match status {
        301 | 302 | 303 | 307 | 308 => {
            let url = web_url(location.context("redirect without location")?)
                .context("invalid location")?;
            let ok = url.scheme() == "https"
                && url.host_str() == Some("www.pcgamingwiki.com")
                && url.path().len() > "/wiki/".len()
                && url.path().starts_with("/wiki/");
            Ok(ok.then(|| Link::new("PCGamingWiki", url.to_string(), "pcgamingwiki")))
        }
        200 | 404 => Ok(None),
        s => anyhow::bail!("unexpected status {s}"),
    }
}

/// GOG GamesDB's record for a Steam release.
pub fn gog_gamesdb_request(appid: u64) -> Option<Request> {
    Request::get(&format!(
        "https://gamesdb.gog.com/platforms/steam/external_releases/{appid}"
    ))
}

/// The GOG product of a Steam game: the record must list this appid among its Steam rows and
/// exactly one numeric GOG row. Every other platform row is ignored.
pub fn parse_gog_gamesdb(status: u16, raw: &[u8], appid: u64) -> Result<Option<u64>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let record: Value = serde_json::from_slice(raw)?;
    let releases = record["game"]["releases"]
        .as_array()
        .context("no releases")?;
    let ids = |platform: &str| -> Vec<u64> {
        let mut ids: Vec<u64> = releases
            .iter()
            .filter(|r| r["platform_id"] == platform)
            .filter_map(|r| number(r["external_id"].as_str()?, 15))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    if !ids("steam").contains(&appid) {
        return Ok(None);
    }
    Ok(match ids("gog").as_slice() {
        [one] => Some(*one),
        _ => None,
    })
}

/// GOG's product record, which names the store page.
pub fn gog_product_request(product: u64) -> Option<Request> {
    Request::get(&format!("https://api.gog.com/products/{product}"))
}

/// The GOG store page of a game product.
pub fn parse_gog_product(status: u16, raw: &[u8], product: u64) -> Result<Option<Link>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let record: Value = serde_json::from_slice(raw)?;
    anyhow::ensure!(
        record["id"].as_u64() == Some(product),
        "GOG answered for another product"
    );
    if record["game_type"] != "game" || record["is_secret"] == true {
        return Ok(None);
    }
    Ok(record["links"]["product_card"]
        .as_str()
        .and_then(web_url)
        .filter(|u| {
            u.scheme() == "https"
                && u.host_str() == Some("www.gog.com")
                && u.path().contains("/game/")
        })
        .map(|u| Link::new("GOG", u.to_string(), "gog")))
}

/// This family in the lookups' table ([`super::FAMILIES`]).
pub struct Lookups;
impl super::Family for Lookups {
    fn name(&self) -> &'static str {
        "games"
    }
    fn sources(&self) -> Vec<std::sync::Arc<dyn crate::also_on::Source>> {
        vec![
            std::sync::Arc::new(also_on::Steam),
            std::sync::Arc::new(also_on::WikidataStores),
        ]
    }
}

/// The also-on sources that run these lookups: each fetches through the egress (its policy,
/// budgets and shared discovery cache), hands the answer to this module's parsers, and reports
/// keys and derived links.
/// Whether a MediaWiki title is a namespace's page (`Category:Games`, `Special:Search`,
/// `User_talk:Someone`) or the wiki's main page rather than an article.
fn wiki_namespace(title: &str) -> bool {
    let title = percent_encoding::percent_decode_str(title).decode_utf8_lossy();
    let Some((namespace, _)) = title.split_once(':') else {
        return title.eq_ignore_ascii_case("Main_Page");
    };
    let namespace = namespace.replace(' ', "_").to_ascii_lowercase();
    namespace.ends_with("_talk")
        || [
            "special",
            "talk",
            "user",
            "project",
            "pcgamingwiki",
            "file",
            "image",
            "media",
            "mediawiki",
            "template",
            "help",
            "category",
            "module",
            "property",
            "form",
            "concept",
            "company",
            "engine",
            "series",
            "glossary",
        ]
        .contains(&namespace.as_str())
}

pub mod also_on {
    use crate::{
        also_on::{self, Context, Source, Subject},
        hubs,
        lookups::{item, secondary, send, wikidata as items, with_retry},
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::Value;

    /// A Steam game: SteamDB (from the appid alone), ProtonDB (when its summary exists),
    /// PCGamingWiki (where its appid lookup leads) and GOG (through GOG GamesDB).
    pub struct Steam;

    #[async_trait]
    impl Source for Steam {
        fn single_namespaces(&self) -> &'static [&'static str] {
            &["steam", "gog"]
        }
        fn search_key(&self, namespace: &str, value: &str) -> Option<String> {
            let digits = |v: &str| crate::lookups::digits(v);
            Some(
                match namespace {
                    "steam" => {
                        crate::lookups::games::steam_key(crate::lookups::games::appid(value)?)
                    }
                    "gog" => crate::lookups::games::gog_key(
                        value.parse().ok().filter(|_| digits(value))?,
                    ),
                    _ => return None,
                }
                .to_string(),
            )
        }
        /// A game's own page only, never a page under it, a search or a user's page.
        fn service_name(&self, host: &str, url: &url::Url) -> Option<&'static str> {
            use crate::lookups::games::appid_from_uri;
            let uri = url.as_str();
            let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
            let (name, object) = match host {
                "store.steampowered.com" | "steamcommunity.com" => {
                    ("Steam", appid_from_uri(uri).is_some())
                }
                "steamdb.info" => ("SteamDB", appid_from_uri(uri).is_some()),
                "protondb.com" => ("ProtonDB", appid_from_uri(uri).is_some()),
                // A game's article: `/wiki/{title}`, no namespace's page (`Special:Search`,
                // `Category:Games`) and no other view of it (`?action=history`).
                "pcgamingwiki.com" => (
                    "PCGamingWiki",
                    appid_from_uri(uri).is_some()
                        || matches!(
                            segments.as_slice(),
                            ["wiki", title] if crate::lookups::harmless_query(url)
                                && !super::wiki_namespace(title)
                        ),
                ),
                "gog.com" => ("GOG", store_property(uri).is_some()),
                _ => return None,
            };
            object.then_some(name)
        }
        fn id(&self) -> &'static str {
            "steam"
        }
        fn applies(&self, subject: &Subject) -> bool {
            crate::lookups::games::appid_from_uri(&subject.uri).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            use crate::lookups::games;
            let Some(appid) = games::appid_from_uri(&subject.uri) else {
                return Ok(also_on::Found::default());
            };
            let mut found = crate::lookups::Found::default();
            found.push_key(games::steam_key(appid));
            for link in games::templates(appid) {
                found.push_link(link);
            }
            // The key and the templates are the appid's own; each service below adds its page, and
            // one that fails (busy, a bot wall) costs only that.
            let mut retry = None;
            if let Some(request) = games::protondb_request(appid) {
                let link = secondary(
                    "protondb",
                    &mut retry,
                    async {
                        let f = send(cx, &request, "protondb_busy").await?;
                        games::parse_protondb(f.status, &f.body, appid)
                    }
                    .await,
                );
                if let Some(link) = link.flatten() {
                    found.push_link(link);
                }
            }
            if let Some(request) = games::pcgamingwiki_request(appid) {
                // Where the lookup redirects is the page the appid names.
                let link = secondary(
                    "pcgamingwiki",
                    &mut retry,
                    async {
                        let f = send(cx, &request, "pcgamingwiki_busy").await?;
                        games::parse_pcgamingwiki(f.status, f.location.as_deref())
                    }
                    .await,
                );
                if let Some(link) = link.flatten() {
                    found.push_link(link);
                }
            }
            if let Some(request) = games::gog_gamesdb_request(appid) {
                let product = secondary(
                    "gog",
                    &mut retry,
                    async {
                        let f = send(cx, &request, "gog_busy").await?;
                        games::parse_gog_gamesdb(f.status, &f.body, appid)
                    }
                    .await,
                );
                if let Some(product) = product.flatten() {
                    found.push_key(games::gog_key(product));
                    if let Some(request) = games::gog_product_request(product) {
                        let link = secondary(
                            "gog",
                            &mut retry,
                            async {
                                let f = send(cx, &request, "gog_busy").await?;
                                games::parse_gog_product(f.status, &f.body, product)
                            }
                            .await,
                        );
                        if let Some(link) = link.flatten() {
                            found.push_link(link);
                        }
                    }
                }
            }
            Ok(with_retry(found.into_also_on("steam"), retry))
        }
    }

    /// The Wikidata property a store link names, its value, and the stores to list for the item.
    /// An F-Droid site language (`en`, `fr`, `zh_Hant`, `pt_BR`).
    fn fdroid_language(s: &str) -> bool {
        let (lang, region) = s.split_once(['_', '-']).unwrap_or((s, "AA"));
        (2..=3).contains(&lang.len())
            && lang.bytes().all(|b| b.is_ascii_lowercase())
            && (2..=4).contains(&region.len())
            && region.bytes().all(|b| b.is_ascii_alphabetic())
    }

    pub fn store_property(uri: &str) -> Option<(&'static str, String, &'static [items::Property])> {
        let url = crate::lookups::web_url(uri)?;
        let host = url.host_str()?.trim_start_matches("www.");
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let query = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        let package = |v: &str| {
            crate::lookups::plain(v, 200, |c| c.is_ascii_alphanumeric() || "._-".contains(c))
        };
        let (pid, value, set) = match (host, segments.as_slice()) {
            // The game's own store page only (`games::appid_from_uri`), never a page under it.
            ("store.steampowered.com", ["app", ..]) => (
                "P1733",
                crate::lookups::games::appid_from_uri(uri)?.to_string(),
                items::GAME,
            ),
            ("gog.com", [lang, "game", slug]) if lang.len() == 2 => {
                ("P2725", format!("game/{slug}"), items::GAME)
            }
            ("gog.com", ["game", slug]) => ("P2725", format!("game/{slug}"), items::GAME),
            // The app's own page: `/{cc}/app/{name}/id{N}`, with or without the storefront and the
            // name. Nothing under it.
            ("apps.apple.com", rest) => {
                let country =
                    |cc: &str| cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_lowercase());
                let name = |n: &str| {
                    crate::lookups::plain(n, 200, |c| {
                        c.is_ascii_alphanumeric() || "-_.%".contains(c)
                    })
                };
                let id = match rest {
                    [cc, "app", n, id] if country(cc) && name(n) => id,
                    [cc, "app", id] if country(cc) => id,
                    ["app", n, id] if name(n) => id,
                    ["app", id] => id,
                    _ => return None,
                };
                let id = id.strip_prefix("id")?;
                crate::lookups::plain(id, 20, |c| c.is_ascii_digit()).then_some(())?;
                ("P3861", id.to_owned(), items::APP)
            }
            ("play.google.com", ["store", "apps", "details"]) => {
                ("P3418", query("id")?, items::APP)
            }
            // The package's page, with or without a language path (`/en/`, `/zh_Hant/`).
            ("f-droid.org", ["packages", id]) => ("P3597", (*id).to_owned(), items::APP),
            ("f-droid.org", [lang, "packages", id]) if fdroid_language(lang) => {
                ("P3597", (*id).to_owned(), items::APP)
            }
            ("flathub.org", ["apps", "details", id]) | ("flathub.org", ["apps", id]) => {
                ("P4655", (*id).to_owned(), items::APP)
            }
            ("snapcraft.io", [id]) => ("P4435", (*id).to_owned(), items::APP),
            _ => return None,
        };
        (pid == "P2725" || pid == "P3861" || pid == "P1733" || package(&value))
            .then_some((pid, value, set))
    }

    /// Whether `value` is the item's only best-rank value of `pid`: an item with two appids is not
    /// one Steam page's.
    fn only_value(item: &Value, pid: &str, value: &str) -> bool {
        let mut values = items::best_values(item, pid);
        values.sort_unstable();
        values.dedup();
        values == [value]
    }

    /// An app or game on a store: the one Wikidata item carrying that store's ID (read back and
    /// compared exactly; several are ambiguous and give nothing), its other stores and game
    /// databases, and its Q-id as key when the item carries no other ID on that store.
    pub struct WikidataStores;

    #[async_trait]
    impl Source for WikidataStores {
        /// An app's own store page only (`store_property`).
        fn service_name(&self, host: &str, url: &url::Url) -> Option<&'static str> {
            let name = match host {
                "apps.apple.com" => "App Store",
                "play.google.com" => "Google Play",
                "f-droid.org" => "F-Droid",
                "flathub.org" => "Flathub",
                "snapcraft.io" => "Snap Store",
                _ => return None,
            };
            store_property(url.as_str()).map(|_| name)
        }
        fn id(&self) -> &'static str {
            "wikidata_stores"
        }
        fn applies(&self, subject: &Subject) -> bool {
            store_property(&subject.uri).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            let Some((pid, value, set)) = store_property(&subject.uri) else {
                return Ok(also_on::Found::default());
            };
            // An answer over its read's cap is no answer: asking again would read the same megabytes.
            let found = match hubs::wikidata::item_with(&cx.egress, pid, &value).await {
                Err(error) if crate::resolver::egress::over_cap(&error) => {
                    return Ok(also_on::Found::default())
                }
                found => found?,
            };
            let hubs::wikidata::Found::One(hub) = found else {
                return Ok(also_on::Found::default());
            };
            let Some((raw, _)) = item(cx, &hub.qid).await? else {
                return Ok(also_on::Found::default());
            };
            let mut found = crate::lookups::Found::default();
            if only_value(&raw, pid, &value) {
                found.push_key(items::key(&hub.qid));
            }
            for link in items::links(&raw, set) {
                found.push_link(link);
            }
            Ok(found.into_also_on("wikidata"))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::lookups::fixture::{at, keys, recorded, recorded_but_busy, sites};

        #[tokio::test(start_paused = true)]
        async fn games_and_stores() {
            let cx = recorded(vec![]);
            let portal = at("https://store.steampowered.com/app/620/Portal_2/");
            let found = Steam.lookup(&cx, &portal).await.unwrap();
            assert!(keys(&found).contains(&"steam:620"));
            let s = sites(&found);
            assert!(s.iter().any(|s| s.starts_with("SteamDB ")), "{s:?}");
            assert!(s.iter().any(|s| s.starts_with("ProtonDB ")), "{s:?}");
            assert!(
                s.contains(&"PCGamingWiki https://www.pcgamingwiki.com/wiki/Portal_2".to_owned()),
                "{s:?}"
            );
            // A busy wiki costs only its own link.
            let busy = recorded_but_busy(
                vec![],
                &["https://www.pcgamingwiki.com/api/appid.php?appid=620"],
            );
            let without = sites(&Steam.lookup(&busy, &portal).await.unwrap());
            assert!(
                without.iter().any(|s| s.starts_with("ProtonDB ")),
                "{without:?}"
            );
            assert!(!without.iter().any(|s| s.starts_with("PCGamingWiki ")));
            // An appid nobody knows: the templates only.
            let none = Steam
                .lookup(&cx, &at("https://store.steampowered.com/app/1/"))
                .await
                .unwrap();
            assert!(
                sites(&none).iter().all(|s| s.starts_with("Steam")),
                "{:?}",
                sites(&none)
            );

            // Through Wikidata: the one item with the store's ID, its other stores and databases.
            assert!(WikidataStores.applies(&portal));
            let item = WikidataStores.lookup(&cx, &portal).await.unwrap();
            assert_eq!(keys(&item), ["wikidata:Q279446"]);
            let s = sites(&item);
            assert!(
                s.contains(&"PCGamingWiki https://www.pcgamingwiki.com/wiki/Portal_2".to_owned()),
                "{s:?}"
            );
            assert!(
                s.contains(&"IGDB https://www.igdb.com/games/portal-2".to_owned()),
                "{s:?}"
            );
            // MobyGames names two games: ambiguous, left out.
            assert!(!s.iter().any(|s| s.starts_with("MobyGames")), "{s:?}");
            let app =
                at("https://play.google.com/store/apps/details?id=de.danoeh.antennapod&hl=en");
            let item = WikidataStores.lookup(&cx, &app).await.unwrap();
            assert_eq!(keys(&item), ["wikidata:Q16927599"]);
            assert!(
                sites(&item).contains(
                    &"F-Droid https://f-droid.org/packages/de.danoeh.antennapod".to_owned()
                ),
                "{:?}",
                sites(&item)
            );
        }

        #[test]
        fn store_links() {
            for (uri, pid, value) in [
                (
                    "https://store.steampowered.com/app/620/Portal_2/",
                    "P1733",
                    "620",
                ),
                (
                    "https://www.gog.com/en/game/the_witcher_3_wild_hunt",
                    "P2725",
                    "game/the_witcher_3_wild_hunt",
                ),
                (
                    "https://apps.apple.com/us/app/signal-private-messenger/id874139669",
                    "P3861",
                    "874139669",
                ),
                (
                    "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms",
                    "P3418",
                    "org.thoughtcrime.securesms",
                ),
                (
                    "https://f-droid.org/en/packages/de.danoeh.antennapod/",
                    "P3597",
                    "de.danoeh.antennapod",
                ),
                (
                    "https://flathub.org/apps/org.mozilla.firefox",
                    "P4655",
                    "org.mozilla.firefox",
                ),
                ("https://snapcraft.io/firefox", "P4435", "firefox"),
            ] {
                let (p, v, _) = store_property(uri).unwrap_or_else(|| panic!("{uri}"));
                assert_eq!((p, v.as_str()), (pid, value), "{uri}");
            }
            for uri in [
                "https://play.google.com/store/apps/details",
                "https://play.google.com/store/apps/details?id=a%20b",
                "https://apps.apple.com/us/app/x/idabc",
                "https://snapcraft.io/docs/x",
                "https://example.org/app/1",
            ] {
                assert!(store_property(uri).is_none(), "{uri}");
            }
        }

        #[test]
        fn a_page_under_an_app_names_no_store_item() {
            for (uri, want) in [
                (
                    "https://apps.apple.com/us/app/vlc-media-player/id650377962",
                    Some("650377962"),
                ),
                (
                    "https://apps.apple.com/us/app/id650377962",
                    Some("650377962"),
                ),
                (
                    "https://apps.apple.com/app/vlc-media-player/id650377962",
                    Some("650377962"),
                ),
                ("https://apps.apple.com/app/id650377962", Some("650377962")),
                (
                    "https://apps.apple.com/us/app/vlc-media-player/id650377962?see-all=reviews",
                    Some("650377962"),
                ),
                ("https://apps.apple.com/us/developer/videolan/id1234", None),
                ("https://apps.apple.com/us/app/vlc/extra/id650377962", None),
                ("https://apps.apple.com/us/story/id650377962", None),
                (
                    "https://apps.apple.com/us/app/vlc-media-player/id650377962/reviews",
                    None,
                ),
                (
                    "https://f-droid.org/packages/org.videolan.vlc",
                    Some("org.videolan.vlc"),
                ),
                (
                    "https://f-droid.org/en/packages/org.videolan.vlc/",
                    Some("org.videolan.vlc"),
                ),
                (
                    "https://f-droid.org/zh_Hant/packages/org.videolan.vlc",
                    Some("org.videolan.vlc"),
                ),
                (
                    "https://f-droid.org/en/packages/org.videolan.vlc/screenshots",
                    None,
                ),
                ("https://f-droid.org/repo/packages/org.videolan.vlc", None),
                (
                    "https://f-droid.org/en/docs/packages/org.videolan.vlc",
                    None,
                ),
                ("https://f-droid.org/en/categories/packages/x", None),
            ] {
                let got = store_property(uri).map(|(_, v, _)| v);
                assert_eq!(got.as_deref(), want, "{uri}");
            }
        }

        #[test]
        fn a_page_under_a_game_names_no_store_item() {
            assert_eq!(
                store_property("https://store.steampowered.com/app/620/Portal_2/")
                    .map(|(p, v, _)| (p, v)),
                Some(("P1733", "620".to_owned()))
            );
            for uri in [
                "https://store.steampowered.com/app/620/Portal_2/reviews/",
                "https://store.steampowered.com/app/620/Portal_2/news/1",
            ] {
                assert!(store_property(uri).is_none(), "{uri}");
                assert!(!Steam.applies(&at(uri)), "{uri}");
            }
            assert!(!Steam.applies(&at(
                "https://steamcommunity.com/app/620/discussions/0/123456789/"
            )));
        }

        #[test]
        fn an_item_with_two_appids_is_neither_page() {
            use serde_json::json;
            let claim = |v: &str| json!({"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": v}}});
            let one = json!({"claims": {"P1733": [claim("400"), claim("400")]}});
            let two = json!({"claims": {"P1733": [claim("400"), claim("620")]}});
            assert!(only_value(&one, "P1733", "400"));
            assert!(!only_value(&two, "P1733", "400"));
            assert!(!only_value(&two, "P1733", "620"));
            assert!(!only_value(&one, "P1733", "620"));
        }

        /// A secondary store service that fails costs its own part only: the game keeps its key and
        /// the other services' answers, and the run asks again after the service's retry time.
        #[tokio::test(start_paused = true)]
        async fn a_failing_store_service_keeps_the_games_own_key() {
            use crate::lookups::games;
            // GOG busy: the game keeps its key, its templates and ProtonDB.
            let gog = games::gog_gamesdb_request(620).unwrap().url.to_string();
            let busy = recorded_but_busy(vec![], &[gog.as_str()]);
            let portal = at("https://store.steampowered.com/app/620/Portal_2/");
            let found = Steam.lookup(&busy, &portal).await.unwrap();
            assert_eq!(keys(&found), ["steam:620"]);
            assert!(sites(&found).iter().any(|s| s.starts_with("ProtonDB ")));
            assert!(found.retry_after.is_some());
            // Nothing failed: the source runs again after its lifetime.
            let found = Steam.lookup(&recorded(vec![]), &portal).await.unwrap();
            assert!(found.retry_after.is_none());
        }

        /// No link or ID check of this family slices a string inside a multibyte character, and no
        /// parser panics on an answer it doesn't expect.
        #[test]
        fn spellings_and_answers_never_panic() {
            use crate::lookups::{fixture, games};
            for s in fixture::multibyte_spellings(&[
                "https://store.steampowered.com/app/620/Portal_2/",
                "https://www.pcgamingwiki.com/api/appid.php?appid=620",
            ]) {
                games::appid_from_uri(&s);
                games::appid(&s);
            }
            for (status, raw) in fixture::malformed_answers() {
                let _ = games::parse_protondb(status, raw, 620);
                let _ = games::parse_gog_gamesdb(status, raw, 620);
                let _ = games::parse_gog_product(status, raw, 1);
            }
        }

        /// Its key namespaces and services, as searches and cards name them.
        #[test]
        fn keys_and_service_names() {
            use crate::lookups::{search_key, service_name};
            assert_eq!(search_key("steam", "620").as_deref(), Some("steam:620"));
            assert_eq!(search_key("steam", "x"), None);
            for (uri, name) in [
                (
                    "https://play.google.com/store/apps/details?id=io.anuke.mindustry",
                    "Google Play",
                ),
                (
                    "https://f-droid.org/en/packages/io.anuke.mindustry/",
                    "F-Droid",
                ),
                ("https://store.steampowered.com/app/1127400/", "Steam"),
            ] {
                assert_eq!(service_name(uri), Some(name), "{uri}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture;

    /// Only an object's own page on this family's services is named after the service; any
    /// other page there (a topic list, a search, a user's page, a page under an object) is not.
    #[test]
    fn only_an_objects_own_page_is_named_after_its_service() {
        for (uri, name) in [
            ("https://steamcommunity.com/app/620", Some("Steam")),
            ("https://steamdb.info/app/620/", Some("SteamDB")),
            ("https://www.protondb.com/app/620", Some("ProtonDB")),
            (
                "https://www.pcgamingwiki.com/wiki/Portal_2",
                Some("PCGamingWiki"),
            ),
            ("https://www.gog.com/en/game/stardew_valley", Some("GOG")),
            (
                "https://apps.apple.com/us/app/vlc-media-player/id650377962",
                Some("App Store"),
            ),
            ("https://flathub.org/apps/org.videolan.VLC", Some("Flathub")),
            ("https://snapcraft.io/vlc", Some("Snap Store")),
            (
                "https://www.pcgamingwiki.com/wiki/Half-Life:_Alyx",
                Some("PCGamingWiki"),
            ),
            (
                "https://www.pcgamingwiki.com/wiki/Portal_2?utm_source=x",
                Some("PCGamingWiki"),
            ),
            (
                "https://www.pcgamingwiki.com/wiki/Special:Search?search=Portal",
                None,
            ),
            ("https://www.pcgamingwiki.com/wiki/Category:Games", None),
            ("https://www.pcgamingwiki.com/wiki/User_talk:Someone", None),
            ("https://www.pcgamingwiki.com/wiki/Main_Page", None),
            (
                "https://www.pcgamingwiki.com/wiki/Portal_2?action=history",
                None,
            ),
            ("https://steamcommunity.com/app/620/discussions/", None),
            ("https://steamcommunity.com/id/someone", None),
            ("https://store.steampowered.com/search/?term=portal", None),
            ("https://steamdb.info/app/620/charts/", None),
            ("https://www.pcgamingwiki.com/wiki/Portal_2/Talk", None),
            ("https://www.gog.com/en/games", None),
            ("https://play.google.com/store/apps", None),
            ("https://play.google.com/store/apps/dev?id=1", None),
            ("https://f-droid.org/en/docs/", None),
            ("https://apps.apple.com/us/developer/videolan/id1234", None),
        ] {
            assert_eq!(crate::lookups::service_name(uri), name, "{uri}");
        }
    }

    /// Every link this family derives is named, by its URL, as the link names its service: the
    /// object-page rule never unnames the family's own links.
    #[test]
    fn every_derived_link_is_named_as_its_service() {
        let links = templates(620);
        assert!(!links.is_empty());
        for link in &links {
            assert_eq!(
                crate::lookups::service_name(&link.uri),
                Some(link.site.as_str()),
                "{}",
                link.uri
            );
        }
    }

    #[test]
    fn appids() {
        for (uri, id) in [
            ("https://store.steampowered.com/app/620/Portal_2/", 620),
            ("https://steamcommunity.com/app/413150", 413150),
            ("https://steamdb.info/app/620/", 620),
            ("https://www.protondb.com/app/620", 620),
            ("https://www.pcgamingwiki.com/api/appid.php?appid=620", 620),
        ] {
            assert_eq!(appid_from_uri(uri), Some(id), "{uri}");
        }
        for uri in [
            "https://store.steampowered.com/app/0620/",
            "https://store.steampowered.com/bundle/620/",
            "https://example.com/app/620",
            "https://store.steampowered.com/app/12345678901/",
            // Objects under a game are not the game. The store has no child object of the shape
            // `/app/{id}/{name}`: that is the game's page with its name; its reviews, news and
            // age check live elsewhere (`/app/{id}/{name}/reviews`, `/news/app/{id}`,
            // `/agecheck/app/{id}`).
            "https://steamcommunity.com/app/620/discussions/0/123456789/",
            "https://steamcommunity.com/app/620/reviews/",
            "https://steamcommunity.com/app/620/workshop/",
            "https://steamcommunity.com/app/620/allnews/",
            "https://steamcommunity.com/app/620/guides/",
            "https://store.steampowered.com/app/620/Portal_2/reviews/",
            "https://steamdb.info/app/620/charts/",
            "https://www.protondb.com/app/620/reports",
        ] {
            assert_eq!(appid_from_uri(uri), None, "{uri}");
        }
        for uri in [
            "https://store.steampowered.com/app/620",
            "https://steamcommunity.com/app/620/",
        ] {
            assert_eq!(appid_from_uri(uri), Some(620), "{uri}");
        }
        assert_eq!(steam_key(620).as_str(), "steam:620");
        assert_eq!(
            templates(620)
                .iter()
                .map(|l| l.uri.as_str())
                .collect::<Vec<_>>(),
            [
                "https://store.steampowered.com/app/620/",
                "https://steamdb.info/app/620/"
            ]
        );
    }

    #[test]
    fn protondb() {
        assert_eq!(
            protondb_request(620).unwrap().url.as_str(),
            "https://www.protondb.com/api/v1/reports/summaries/620.json"
        );
        let link = parse_protondb(200, &fixture::read("protondb-620.json"), 620)
            .unwrap()
            .unwrap();
        assert_eq!(link.uri, "https://www.protondb.com/app/620");
        assert_eq!(
            parse_protondb(404, &fixture::read("protondb-1.html"), 1).unwrap(),
            None
        );
        assert!(parse_protondb(503, b"", 1).is_err());
    }

    #[test]
    fn pcgamingwiki() {
        let r = pcgamingwiki_request(620).unwrap();
        assert!(!r.follow_redirects);
        // Recorded: 302 to the wiki page for 620, 200 without a redirect for 1.
        let link = parse_pcgamingwiki(302, Some("https://www.pcgamingwiki.com/wiki/Portal_2"))
            .unwrap()
            .unwrap();
        assert_eq!(link.uri, "https://www.pcgamingwiki.com/wiki/Portal_2");
        assert_eq!(parse_pcgamingwiki(200, None).unwrap(), None);
        assert_eq!(
            parse_pcgamingwiki(302, Some("https://example.com/wiki/X")).unwrap(),
            None
        );
        assert_eq!(
            parse_pcgamingwiki(302, Some("https://www.pcgamingwiki.com/")).unwrap(),
            None
        );
        assert!(parse_pcgamingwiki(302, None).is_err());
        assert!(parse_pcgamingwiki(429, None).is_err());
    }

    #[test]
    fn gog() {
        assert_eq!(
            gog_gamesdb_request(413150).unwrap().url.as_str(),
            "https://gamesdb.gog.com/platforms/steam/external_releases/413150"
        );
        let stardew = fixture::read("gog-gamesdb-steam-413150.json");
        assert_eq!(
            parse_gog_gamesdb(200, &stardew, 413150).unwrap(),
            Some(1453375253)
        );
        // The record must list the asked appid as a Steam row.
        assert_eq!(parse_gog_gamesdb(200, &stardew, 620).unwrap(), None);
        // The Witcher 3 has several GOG rows (editions, clients): ambiguous.
        let witcher = fixture::read("gog-gamesdb-steam-292030.json");
        assert_eq!(parse_gog_gamesdb(200, &witcher, 292030).unwrap(), None);
        let unknown = fixture::read("gog-gamesdb-steam-1.json");
        assert_eq!(parse_gog_gamesdb(404, &unknown, 1).unwrap(), None);

        let product = fixture::read("gog-product-1453375253.json");
        let link = parse_gog_product(200, &product, 1453375253)
            .unwrap()
            .unwrap();
        assert_eq!(
            (link.site.as_str(), link.uri.as_str()),
            ("GOG", "https://www.gog.com/game/stardew_valley")
        );
        assert!(parse_gog_product(200, &product, 1).is_err());
        assert_eq!(gog_key(1453375253).as_str(), "gog:1453375253");
    }
}
