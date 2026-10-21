//! Wikipedia articles: one canonical URL per article, and "also on" for the same article in
//! other languages and other encyclopedias.
//!
//! [`parse`] reads an article link from the URL alone: the mobile host, `index.php?title=…`
//! (also a revision with `oldid`), spaces and percent-encoding fold into
//! `https://{lang}.wikipedia.org/wiki/{Title}`, an alias (`alias::root`).
//!
//! An article's card comes from its REST summary ([`summary_card`]), so a long article whose page
//! exceeds the page cap still gets its title, first paragraph and image.
//!
//! [`WikipediaSource`] asks the article's own Wikipedia for its Wikidata item (following
//! redirects), then lists that item's other Wikipedia editions and its Britannica, Stanford
//! Encyclopedia of Philosophy, Internet Encyclopedia of Philosophy, Scholarpedia and Namuwiki
//! entries, with the Q-id as key. Nothing is matched by title across languages.
use crate::{
    also_on::{Context, Found, Key, Link, Source, Subject},
    hubs::{self, wikidata},
    model::ResourceMeta,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use url::Url;

/// An article: its language edition, its title (with `_` for spaces) and its canonical URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Article {
    pub lang: String,
    pub title: String,
    pub canonical: String,
}

/// The Wikipedia article a URL names, if it is one.
pub fn parse(uri: &str) -> Option<Article> {
    let url = Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let lang = host.strip_suffix(".wikipedia.org")?;
    let lang = lang.strip_suffix(".m").unwrap_or(lang);
    let lang_ok = (2..=16).contains(&lang.len())
        && lang.as_bytes()[0].is_ascii_lowercase()
        && lang
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !matches!(lang, "www" | "m");
    if !lang_ok {
        return None;
    }
    let raw = match url.path() {
        "/w/index.php" => url
            .query_pairs()
            .find(|(k, _)| k == "title")
            .map(|(_, v)| v.into_owned())?,
        path => {
            let encoded = path.strip_prefix("/wiki/")?;
            percent_encoding::percent_decode_str(encoded)
                .decode_utf8()
                .ok()?
                .into_owned()
        }
    };
    let title = raw.trim().replace(' ', "_");
    if title.is_empty()
        || title.len() > 512
        || title
            .chars()
            .any(|c| c.is_control() || matches!(c, '#' | '<' | '>' | '[' | ']' | '{' | '}' | '|'))
    {
        return None;
    }
    let mut canonical = Url::parse(&format!("https://{lang}.wikipedia.org/")).ok()?;
    canonical.set_path(&format!("/wiki/{title}"));
    // `?` in a title is part of it, not a query.
    let canonical = canonical.as_str().replace('?', "%3F");
    Some(Article {
        lang: lang.to_owned(),
        title,
        canonical,
    })
}

/// The article's REST summary: its title, short description, first paragraph and thumbnail, a
/// few kilobytes however long the article is (a long article's page exceeds the page cap
/// before its JSON-LD).
pub fn summary_url(article: &Article) -> String {
    let title =
        percent_encoding::utf8_percent_encode(&article.title, percent_encoding::NON_ALPHANUMERIC);
    format!(
        "https://{}.wikipedia.org/api/rest_v1/page/summary/{title}",
        article.lang
    )
}
/// An article's card from its REST summary: an `Article` named by its title, described by its
/// first paragraph (or its short description), with its thumbnail, on `Wikipedia ({lang})`.
pub fn summary_card(uri: &str, article: &Article, v: &Value, ttl: u64) -> Option<ResourceMeta> {
    let title = v["title"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())?;
    let mut doc = ResourceMeta::new(uri, "wikipedia", ttl);
    doc.set_type("Article");
    doc.name = Some(title.to_owned());
    let text = |field: &str| {
        v[field]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    doc.description = text("extract").or_else(|| text("description")).map(|d| {
        if d.chars().count() > 600 {
            format!("{}…", d.chars().take(599).collect::<String>().trim_end())
        } else {
            d
        }
    });
    doc.image = v["thumbnail"]["source"]
        .as_str()
        .filter(|s| s.starts_with("https://"))
        .map(str::to_owned);
    doc.site = format!("Wikipedia ({})", article.lang);
    doc.attributes
        .insert("language".into(), article.lang.clone());
    Some(doc)
}

/// One of Wikidata's other encyclopedias: the property, the service name, the URL of a value,
/// and which values are well formed (as the property's format says).
struct Encyclopedia {
    property: &'static str,
    site: &'static str,
    template: &'static str,
    valid: fn(&str) -> bool,
}
const ENCYCLOPEDIAS: &[Encyclopedia] = &[
    Encyclopedia {
        property: "P1417",
        site: "Britannica",
        template: "https://www.britannica.com/{}",
        valid: britannica,
    },
    Encyclopedia {
        property: "P3123",
        site: "Stanford Encyclopedia of Philosophy",
        template: "https://plato.stanford.edu/entries/{}/",
        valid: sep,
    },
    Encyclopedia {
        property: "P5088",
        site: "Internet Encyclopedia of Philosophy",
        template: "https://iep.utm.edu/{}/",
        valid: iep,
    },
    Encyclopedia {
        property: "P9526",
        site: "Scholarpedia",
        template: "http://www.scholarpedia.org/article/{}",
        valid: scholarpedia,
    },
    Encyclopedia {
        property: "P8885",
        site: "Namuwiki",
        template: "https://namu.wiki/w/{}",
        valid: namuwiki,
    },
];
fn britannica(v: &str) -> bool {
    let mut parts = v.split('/');
    matches!(
        parts.next(),
        Some(
            "topic"
                | "facts"
                | "place"
                | "event"
                | "science"
                | "technology"
                | "plant"
                | "animal"
                | "biography"
                | "sports"
                | "art"
                | "money"
        )
    ) && v.contains('/')
        && v.split('/').skip(1).all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_graphic() && b != b'?' && b != b'#')
        })
}
fn sep(v: &str) -> bool {
    !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn iep(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.split('-')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
}
fn scholarpedia(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 256
        && v.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}
fn namuwiki(v: &str) -> bool {
    !v.is_empty() && v.len() <= 512 && !v.chars().any(|c| c.is_control() || c == '#')
}
/// Language editions listed first when an article has more than the per-source cap.
const LANGUAGES: &[&str] = &[
    "en", "de", "fr", "es", "it", "ja", "ru", "pt", "zh", "pl", "nl", "ar", "sv", "uk", "fa", "ko",
    "tr", "id", "he", "cs", "vi", "fi", "no", "hu", "ca", "ro", "el", "da", "th", "bg", "sr", "hr",
];

/// The Wikidata item of an article from its own Wikipedia (`pageprops`, redirects followed).
pub fn parse_item(v: &Value) -> Option<String> {
    v["query"]["pages"]
        .as_object()?
        .values()
        .find_map(|p| p["pageprops"]["wikibase_item"].as_str().and_then(hubs::qid))
}
/// An item's keys and links, without `own` (the article itself).
pub fn found(item: &wikidata::Item, own: &Article) -> Found {
    let mut found = Found {
        keys: item_key(item),
        ..Default::default()
    };
    found.links = encyclopedias(item);
    found.links.extend(editions(item, Some(&own.lang)));
    found
}
/// An item's Q-id as a key.
fn item_key(item: &wikidata::Item) -> Vec<Key> {
    hubs::key::wikidata(&item.qid)
        .map(|key| Key {
            key,
            via: "wikidata".into(),
        })
        .into_iter()
        .collect()
}
/// A link an item's own claim or sitelink names: keyed by the item, so a tagged link that is it
/// joins the item's group.
fn named(item: &wikidata::Item, link: Link) -> Link {
    match hubs::key::wikidata(&item.qid) {
        Some(key) => link.keyed(key),
        None => link,
    }
}
/// An item's entries in the other encyclopedias, keyed by the item.
pub fn encyclopedias(item: &wikidata::Item) -> Vec<Link> {
    let mut links = vec![];
    for e in ENCYCLOPEDIAS {
        for value in item.id(e.property).iter().filter(|v| (e.valid)(v)).take(2) {
            let value = if e.property == "P8885" {
                percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
                    .to_string()
            } else {
                value.clone()
            };
            links.push(named(
                item,
                Link::new(e.template.replace("{}", &value), e.site, "wikidata"),
            ));
        }
    }
    links
}
/// An item's Wikipedia editions (its sitelinks), the major languages first, without `skip`,
/// keyed by the item.
pub fn editions(item: &wikidata::Item, skip: Option<&str>) -> Vec<Link> {
    let mut editions: Vec<Article> = item
        .sitelinks
        .values()
        .filter_map(|url| parse(url))
        .filter(|a| Some(a.lang.as_str()) != skip)
        .collect();
    let rank = |lang: &str| {
        LANGUAGES
            .iter()
            .position(|l| *l == lang)
            .unwrap_or(LANGUAGES.len())
    };
    editions.sort_by(|a, b| (rank(&a.lang), &a.lang).cmp(&(rank(&b.lang), &b.lang)));
    editions
        .into_iter()
        .map(|a| {
            named(
                item,
                Link::new(a.canonical, format!("Wikipedia ({})", a.lang), "wikidata"),
            )
        })
        .collect()
}
/// What a Wikidata item link lists: its Q-id as key; its encyclopedia entries, the music
/// services its best-rank IDs name (every level: the item is the thing itself, so these carry
/// no key: a song item's track is one of several recordings) and its Wikipedia editions. Only
/// the item's own claims and sitelinks, never a search.
pub fn item_found(item: &wikidata::Item, storefront: &str) -> Found {
    let mut found = Found {
        keys: item_key(item),
        ..Default::default()
    };
    found.links = encyclopedias(item);
    found
        .links
        .extend(crate::music_sources::service_links(item, storefront));
    found.links.extend(editions(item, None));
    found
}

/// A Wikidata item link: the item's own hub.
pub struct WikidataItemSource;
#[async_trait]
impl Source for WikidataItemSource {
    fn id(&self) -> &'static str {
        "wikidata_item"
    }
    fn applies(&self, subject: &Subject) -> bool {
        wikidata::item_link(&subject.uri).is_some()
    }
    async fn prefetch(&self, cx: &Context, uris: &[String]) {
        let qids: Vec<String> = uris
            .iter()
            .filter_map(|u| wikidata::item_link(u))
            .map(|(qid, _)| qid)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if qids.len() > 1 {
            if let Err(error) = wikidata::prefetch_items(&cx.egress, &qids).await {
                tracing::debug!(error = %format!("{error:#}"), "batched Wikidata read failed; items read alone");
            }
        }
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, || {
            Self::own_key(&subject.uri)
        })
    }
}
impl WikidataItemSource {
    /// What the item link alone proves: its Q-id.
    fn own_key(uri: &str) -> Found {
        Found {
            keys: wikidata::item_link(uri)
                .and_then(|(qid, _)| hubs::key::wikidata(&qid))
                .map(|key| Key {
                    key,
                    via: "wikidata".into(),
                })
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some((qid, _)) = wikidata::item_link(&subject.uri) else {
            return Ok(Found::default());
        };
        Ok(wikidata::items(&cx.egress, &[qid])
            .await?
            .first()
            .map(|item| item_found(item, &cx.config.also_on.apple_storefront))
            .unwrap_or_default())
    }
}

/// Language editions and other encyclopedias of a Wikipedia article.
pub struct WikipediaSource;
#[async_trait]
impl Source for WikipediaSource {
    fn id(&self) -> &'static str {
        "wikipedia"
    }
    fn applies(&self, subject: &Subject) -> bool {
        parse(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, Found::default)
    }
}
impl WikipediaSource {
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(article) = parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let mut url = Url::parse(&format!("https://{}.wikipedia.org/w/api.php", article.lang))?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("prop", "pageprops")
            .append_pair("ppprop", "wikibase_item")
            .append_pair("titles", &article.title)
            .append_pair("redirects", "1")
            .append_pair("format", "json");
        let options = crate::resolver::egress::FetchOptions {
            accept: Some("application/json".into()),
            ..Default::default()
        };
        let fetched = cx.egress.discover(url.as_str(), &options, 3600).await?;
        match fetched.status {
            200 => {}
            429 | 500..=599 => {
                return Err(crate::resolver::RetryLater {
                    class: "wikipedia_busy",
                    seconds: fetched.retry_after_secs.clamp(60, 3600),
                }
                .into())
            }
            _ => return Ok(Found::default()),
        }
        let Some(qid) = parse_item(&serde_json::from_slice(&fetched.body)?) else {
            return Ok(Found::default());
        };
        Ok(wikidata::items(&cx.egress, &[qid])
            .await?
            .first()
            .map(|item| found(item, &article))
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// A long article (Signal's page is over the page cap) gets its card from the summary.
    #[test]
    fn an_article_card_comes_from_its_summary() {
        let uri = "https://en.m.wikipedia.org/wiki/Signal_(software)";
        let article = parse(uri).unwrap();
        assert_eq!(
            summary_url(&article),
            "https://en.wikipedia.org/api/rest_v1/page/summary/Signal%5F%28software%29"
        );
        let doc = summary_card(uri, &article, &fixture("enwiki-summary-signal.json"), 3600)
            .unwrap()
            .bounded(crate::resolver::web::ATTRIBUTES)
            .unwrap();
        assert_eq!(
            (
                doc.type_name.as_str(),
                doc.name.as_deref(),
                doc.site.as_str()
            ),
            ("Article", Some("Signal (software)"), "Wikipedia (en)")
        );
        assert!(doc
            .description
            .as_deref()
            .is_some_and(|d| d.starts_with("Signal is an open-source, encrypted messaging")));
        assert!(doc
            .image
            .as_deref()
            .is_some_and(|i| i.starts_with("https://")));
        assert!(summary_card(uri, &article, &serde_json::json!({"title": " "}), 3600).is_none());
    }

    #[test]
    fn every_spelling_of_an_article_has_one_canonical_url() {
        for (uri, canonical) in [
            (
                "https://en.wikipedia.org/wiki/OK_Computer",
                "https://en.wikipedia.org/wiki/OK_Computer",
            ),
            (
                "https://en.m.wikipedia.org/wiki/OK_Computer#Release",
                "https://en.wikipedia.org/wiki/OK_Computer",
            ),
            (
                "https://en.wikipedia.org/wiki/OK%20Computer",
                "https://en.wikipedia.org/wiki/OK_Computer",
            ),
            (
                "https://en.wikipedia.org/w/index.php?title=OK_Computer&oldid=1234567",
                "https://en.wikipedia.org/wiki/OK_Computer",
            ),
            (
                "https://de.wikipedia.org/wiki/K%C3%B6ln",
                "https://de.wikipedia.org/wiki/K%C3%B6ln",
            ),
            (
                "https://de.wikipedia.org/wiki/Köln",
                "https://de.wikipedia.org/wiki/K%C3%B6ln",
            ),
            (
                "https://en.wikipedia.org/wiki/Who%3F_(album)",
                "https://en.wikipedia.org/wiki/Who%3F_(album)",
            ),
            (
                "https://zh-yue.wikipedia.org/wiki/X",
                "https://zh-yue.wikipedia.org/wiki/X",
            ),
        ] {
            let a = parse(uri).unwrap_or_else(|| panic!("{uri}"));
            assert_eq!(a.canonical, canonical, "{uri}");
            assert_eq!(parse(canonical).unwrap().canonical, canonical);
        }
        for uri in [
            "https://www.wikipedia.org/",
            "https://en.wikipedia.org/",
            "https://en.wikipedia.org/w/index.php?search=x",
            "https://en.wikipedia.org/wiki/",
            "https://en.wikipedia.org/wiki/A%7CB",
            "https://evil.wikipedia.org.example/wiki/X",
            "https://commons.wikimedia.org/wiki/X",
        ] {
            assert!(parse(uri).is_none(), "{uri}");
        }
    }

    #[test]
    fn an_item_lists_other_editions_and_encyclopedias() {
        assert_eq!(
            parse_item(&fixture("enwiki-ok-computer.json")).as_deref(),
            Some("Q202996")
        );
        assert_eq!(
            parse_item(&fixture("enwiki-kant-redirect.json")).as_deref(),
            Some("Q9312")
        );
        assert_eq!(parse_item(&fixture("enwiki-missing.json")), None);
        let items = wikidata::parse_entities(&fixture("wd-kant.json"));
        let own = parse("https://en.wikipedia.org/wiki/Immanuel_Kant").unwrap();
        let found = found(&items[0], &own);
        assert_eq!(found.keys[0].key, "wikidata:Q9312");
        let links: Vec<(&str, &str)> = found
            .links
            .iter()
            .map(|l| (l.site.as_str(), l.uri.as_str()))
            .collect();
        for expected in [
            (
                "Britannica",
                "https://www.britannica.com/biography/Immanuel-Kant",
            ),
            (
                "Stanford Encyclopedia of Philosophy",
                "https://plato.stanford.edu/entries/kant/",
            ),
            (
                "Internet Encyclopedia of Philosophy",
                "https://iep.utm.edu/kantview/",
            ),
            (
                "Wikipedia (de)",
                "https://de.wikipedia.org/wiki/Immanuel_Kant",
            ),
        ] {
            assert!(links.contains(&expected), "{expected:?}");
        }
        // Never itself; the major languages first.
        assert!(links.iter().all(|(site, _)| *site != "Wikipedia (en)"));
        let editions: Vec<&str> = links
            .iter()
            .filter(|(s, _)| s.starts_with("Wikipedia"))
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(
            &editions[..3],
            ["Wikipedia (de)", "Wikipedia (fr)", "Wikipedia (es)"]
        );
        // Wikiquote and Wikisource sitelinks are not Wikipedia editions.
        assert!(editions.len() > 100, "{}", editions.len());
    }

    #[test]
    fn a_wikidata_item_link_is_its_own_hub() {
        for uri in [
            "https://www.wikidata.org/wiki/Q90",
            "http://www.wikidata.org/entity/Q90",
            "https://m.wikidata.org/wiki/Q90#sitelinks",
            "https://www.wikidata.org/wiki/Special:EntityPage/Q90",
            "https://wikidata.org/wiki/Q90",
        ] {
            assert_eq!(
                wikidata::item_link(uri),
                Some(("Q90".into(), "https://www.wikidata.org/wiki/Q90".into())),
                "{uri}"
            );
        }
        for uri in [
            "https://www.wikidata.org/wiki/Property:P31",
            "https://www.wikidata.org/wiki/Q0",
            "https://www.wikidata.org/wiki/Special:EntityData/Q90.json",
            "https://evil.example/wiki/Q90",
        ] {
            assert!(wikidata::item_link(uri).is_none(), "{uri}");
        }
        // Kant's item: encyclopedias and every edition, English included.
        let kant = &wikidata::parse_entities(&fixture("wd-kant.json"))[0];
        let found = item_found(kant, "us");
        assert_eq!(found.keys[0].key, "wikidata:Q9312");
        let sites: Vec<&str> = found.links.iter().map(|l| l.site.as_str()).collect();
        assert!(sites.contains(&"Britannica") && sites.contains(&"Wikipedia (en)"));
        let editions: Vec<&&str> = sites
            .iter()
            .filter(|s| s.starts_with("Wikipedia"))
            .collect();
        assert_eq!(*editions[0], "Wikipedia (en)");
        // OK Computer's item: the music services its IDs name, then its editions.
        let album = wikidata::parse_entities(&fixture("wd-entities.json"))
            .into_iter()
            .find(|i| i.qid == "Q202996")
            .unwrap();
        let uris: Vec<String> = item_found(&album, "us")
            .links
            .into_iter()
            .map(|l| l.uri)
            .collect();
        assert!(uris.contains(&"https://open.spotify.com/album/2fGCAYUMssLKiUAoNdxGLx".into()));
        assert!(uris.contains(&"https://en.wikipedia.org/wiki/OK_Computer".into()));
    }

    #[test]
    fn encyclopedia_ids_must_be_well_formed() {
        assert!(britannica("biography/Immanuel-Kant"));
        assert!(!britannica("biography"));
        assert!(!britannica("people/x"));
        assert!(sep("kant") && !sep("a b"));
        assert!(iep("kant-ethics") && !iep("Kant"));
        assert!(scholarpedia("Neural_networks") && !scholarpedia("a/b"));
    }
}
