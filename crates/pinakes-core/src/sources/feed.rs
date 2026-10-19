//! Cards for links behind an access wall, from the site's own feeds. A wall hook: when a page
//! answers with a challenge (or 401/403), the same site's RSS or Atom feed (`/feed`,
//! `/rss.xml`, `/feed.xml`, `/atom.xml`, `/index.xml`) and the Google News sitemaps its
//! `robots.txt` names are searched for an item whose link is exactly the page's address. Its
//! title, description, image, author and date make the card. Feeds list recent items only, so
//! older links are not found. Every feed is fetched through the shared discovery cache (an hour);
//! a host with no feed at all is remembered for a day. No key.
use super::{list, Context, Example, HostMemo, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::LazyLock;

pub struct Feed;

/// Hosts with no feed or news sitemap at all (`false`), so their walled links ask no more.
static FEEDLESS: LazyLock<HostMemo> = LazyLock::new(HostMemo::default);

/// The feed addresses tried on the page's own origin, in order.
pub const FEEDS: &[&str] = &["/feed", "/rss.xml", "/feed.xml", "/atom.xml", "/index.xml"];
/// The most news sitemaps read from one `robots.txt`.
const MAX_SITEMAPS: usize = 2;
/// How long fetched feeds are reused.
const FEED_TTL: u64 = 3600;

/// One item of a feed or news sitemap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub link: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub authors: Vec<String>,
    pub published: Option<String>,
    /// From a Google News sitemap.
    pub news: bool,
}

fn one_line(value: &str) -> String {
    crate::resolver::cards::plain(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn some(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}
fn date(value: &str) -> Option<String> {
    let value = value.trim();
    chrono::DateTime::parse_from_rfc2822(value)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
        .ok()
        .map(|d| d.to_rfc3339())
}

/// A child element by local name (namespaces aside: `news:title`, `dc:creator`).
fn named<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}
fn text_of(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    named(node, name)
        .and_then(|n| n.text())
        .map(str::trim)
        .map(str::to_owned)
}

/// The items of an RSS feed, an Atom feed or a Google News sitemap.
pub fn entries(body: &[u8]) -> Vec<Entry> {
    let Ok(text) = std::str::from_utf8(body) else {
        return vec![];
    };
    let Ok(xml) = roxmltree::Document::parse(text) else {
        return vec![];
    };
    let mut out = vec![];
    for node in xml.descendants().filter(|n| n.is_element()).take(5000) {
        match node.tag_name().name() {
            // RSS 2.0.
            "item" => {
                let Some(link) = text_of(node, "link") else {
                    continue;
                };
                let image = node
                    .children()
                    .filter(|c| c.is_element())
                    .find(|c| {
                        matches!(c.tag_name().name(), "content" | "thumbnail")
                            && c.attribute("medium").is_none_or(|m| m == "image")
                            || c.tag_name().name() == "enclosure"
                                && c.attribute("type").is_some_and(|t| t.starts_with("image/"))
                    })
                    .and_then(|c| c.attribute("url"))
                    .map(str::to_owned);
                let authors = node
                    .children()
                    .filter(|c| c.is_element() && c.tag_name().name() == "creator")
                    .filter_map(|c| c.text())
                    .map(one_line)
                    .filter(|a| !a.is_empty())
                    .collect();
                out.push(Entry {
                    link,
                    title: some(text_of(node, "title").map(|t| one_line(&t))),
                    description: some(text_of(node, "description").map(|d| one_line(&d))),
                    image,
                    authors,
                    published: text_of(node, "pubDate").and_then(|d| date(&d)),
                    news: false,
                });
            }
            // Atom.
            "entry" => {
                let link = node
                    .children()
                    .filter(|c| c.is_element() && c.tag_name().name() == "link")
                    .find(|c| c.attribute("rel").is_none_or(|r| r == "alternate"))
                    .and_then(|c| c.attribute("href"))
                    .map(str::to_owned);
                let Some(link) = link else {
                    continue;
                };
                let authors = node
                    .children()
                    .filter(|c| c.is_element() && c.tag_name().name() == "author")
                    .filter_map(|a| text_of(a, "name"))
                    .collect();
                out.push(Entry {
                    link,
                    title: some(text_of(node, "title").map(|t| one_line(&t))),
                    description: some(
                        text_of(node, "summary")
                            .or_else(|| text_of(node, "content"))
                            .map(|d| one_line(&d)),
                    ),
                    image: node
                        .children()
                        .find(|c| c.is_element() && c.tag_name().name() == "thumbnail")
                        .and_then(|c| c.attribute("url"))
                        .map(str::to_owned),
                    authors,
                    published: text_of(node, "published")
                        .or_else(|| text_of(node, "updated"))
                        .and_then(|d| date(&d)),
                    news: false,
                });
            }
            // A Google News sitemap entry.
            "url" if named(node, "news").is_some() => {
                let Some(link) = text_of(node, "loc") else {
                    continue;
                };
                let news = named(node, "news").expect("checked");
                out.push(Entry {
                    link,
                    title: some(text_of(news, "title").map(|t| one_line(&t))),
                    description: None,
                    image: named(node, "image").and_then(|i| text_of(i, "loc")),
                    authors: vec![],
                    published: text_of(news, "publication_date").and_then(|d| date(&d)),
                    news: true,
                });
            }
            _ => {}
        }
    }
    out
}

/// The news sitemaps a `robots.txt` names on the page's own origin (`Sitemap:` lines whose
/// address says news).
pub fn news_sitemaps(robots: &str, page: &url::Url) -> Vec<String> {
    robots
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("sitemap")
                .then(|| value.trim())
        })
        .filter_map(super::web_url)
        .filter(|url| {
            url.origin() == page.origin() && url.path().to_ascii_lowercase().contains("news")
        })
        .map(String::from)
        .take(MAX_SITEMAPS)
        .collect()
}

/// The entry whose link is exactly `uri`, as Pinakes normalizes addresses.
pub fn find<'a>(entries: &'a [Entry], uri: &str) -> Option<&'a Entry> {
    let wanted = crate::model::identity(uri).ok()?.1;
    entries.iter().find(|e| {
        crate::model::identity(&e.link)
            .ok()
            .is_some_and(|(_, normalized, _)| normalized == wanted)
    })
}

/// The card of `uri` from its feed entry.
pub fn card(uri: &str, ttl: u64, entry: &Entry, feed: &str) -> Option<ResourceMeta> {
    let title = entry.title.clone()?;
    let mut doc = ResourceMeta::new(uri, "feed", ttl);
    doc.set_type(if entry.news { "NewsArticle" } else { "Article" });
    doc.name = Some(title);
    doc.description = entry
        .description
        .as_ref()
        .map(|d| d.chars().take(300).collect());
    doc.image = entry
        .image
        .clone()
        .filter(|u| super::web_url(u).is_some_and(|u| u.scheme() == "https"));
    for (key, value) in [
        ("author", list(&entry.authors)),
        ("published", entry.published.clone()),
        ("feed", Some(feed.to_owned())),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Some(doc)
}

#[async_trait]
impl Source for Feed {
    fn name(&self) -> &'static str {
        "feed"
    }
    fn reads(&self, _uri: &str) -> bool {
        false
    }
    fn hooks_walls(&self) -> bool {
        true
    }
    fn facts(&self) -> &'static [&'static str] {
        &["author", "published", "feed"]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.theblock.co/news/business/2026-10-19-mesh-visa-partner-spend-crypto-stablecoins-421058",
            card_type: "Article",
        }]
    }
    async fn resolve(&self, _cx: &Context<'_>, _uri: &str) -> Result<ResourceMeta> {
        anyhow::bail!("feed cards come from the web route's wall hook")
    }
    async fn walled(&self, cx: &Context<'_>, uri: &str) -> Result<Option<ResourceMeta>> {
        let Some(url) = super::web_url(uri) else {
            return Ok(None);
        };
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return Ok(None);
        };
        if FEEDLESS.get(&host) == Some(false) {
            return Ok(None);
        }
        let origin = url.origin().ascii_serialization();
        let feed = FetchOptions {
            accept: Some(
                "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9"
                    .into(),
            ),
            ..Default::default()
        };
        // Whether some feed listed items, and whether some request failed (then nothing is
        // concluded about the host).
        let (mut any, mut failed) = (false, false);
        let mut look = |address: String, answer: Result<crate::resolver::egress::Fetched>| {
            let f = match answer {
                Ok(f) => f,
                Err(_) => {
                    failed = true;
                    return None;
                }
            };
            if f.status != 200 {
                return None;
            }
            let found = entries(&f.body);
            any |= !found.is_empty();
            find(&found, uri).and_then(|e| card(uri, cx.ttl, e, &address))
        };
        for path in FEEDS {
            let address = format!("{origin}{path}");
            let answer = cx.egress.discover(&address, &feed, FEED_TTL).await;
            if let Some(doc) = look(address, answer) {
                return Ok(Some(doc));
            }
        }
        let robots = FetchOptions {
            accept: Some("text/plain".into()),
            extra_types: &["text/plain"],
            ..Default::default()
        };
        let sitemaps = match cx
            .egress
            .discover(&format!("{origin}/robots.txt"), &robots, 86_400)
            .await
        {
            Ok(f) if f.status == 200 => news_sitemaps(&String::from_utf8_lossy(&f.body), &url),
            _ => vec![],
        };
        for address in sitemaps {
            let answer = cx.egress.discover(&address, &feed, FEED_TTL).await;
            if let Some(doc) = look(address, answer) {
                return Ok(Some(doc));
            }
        }
        if !any && !failed {
            FEEDLESS.set(&host, false);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    const ARTICLE: &str = "https://www.theblock.co/news/business/2026-10-19-mesh-visa-partner-spend-crypto-stablecoins-421058";
    const RSS: &str = "https://www.theblock.co/rss.xml";
    fn read(file: &str) -> Vec<u8> {
        fixture::read("feed", file)
    }

    #[test]
    fn rss_atom_and_news_sitemaps() {
        let rss = entries(&read("theblock-rss.xml"));
        assert_eq!(rss.len(), 20);
        let item = find(&rss, ARTICLE).unwrap();
        assert_eq!(
            item.title.as_deref(),
            Some("Mesh taps Visa to let users spend crypto without a separate card")
        );
        assert_eq!(item.authors, ["Jason Shubnell"]);
        assert_eq!(item.published.as_deref(), Some("2026-10-19T16:10:02+00:00"));
        assert!(item
            .image
            .as_deref()
            .unwrap()
            .contains("/wp/uploads/2026/10/visa.png"));
        // Exact links only: another article, or a near spelling, is not this one.
        assert!(find(&rss, &format!("{ARTICLE}-2")).is_none());
        assert!(find(&rss, "https://www.theblock.co/news/business").is_none());
        assert!(find(&rss, &format!("{ARTICLE}?page=2")).is_none());

        let news = entries(&read("theblock-news-sitemap.xml"));
        let entry = find(&news, ARTICLE).unwrap();
        assert!(entry.news);
        assert_eq!(
            entry.published.as_deref(),
            Some("2026-10-19T16:09:02+00:00")
        );
        assert_eq!(
            entry.image.as_deref(),
            Some("https://www.tbstat.com/wp/uploads/2026/10/visa.png")
        );

        let atom = br#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>An &lt;b&gt;entry&lt;/b&gt;</title>
            <link rel="self" href="https://a.example/self"/><link href="https://a.example/post/1"/>
            <author><name>Ann</name></author><summary>Short.</summary><published>2026-01-02T03:04:05Z</published></entry></feed>"#;
        let atom = entries(atom);
        let entry = find(&atom, "https://a.example/post/1").unwrap();
        assert_eq!(entry.title.as_deref(), Some("An entry"));
        assert_eq!(entry.authors, ["Ann"]);
        assert_eq!(entry.description.as_deref(), Some("Short."));
        assert!(entries(b"<html>not a feed</html>").is_empty());
        assert!(entries(b"\xff\xfe").is_empty());

        let robots = String::from_utf8(read("theblock-robots.txt")).unwrap();
        let page = url::Url::parse(ARTICLE).unwrap();
        assert_eq!(
            news_sitemaps(&robots, &page),
            ["https://www.theblock.co/sitemap_tbco_news.xml"]
        );
        for other in [
            "https://theblock.co/a",
            "http://www.theblock.co/a",
            "https://www.theblock.co:8443/a",
        ] {
            assert!(
                news_sitemaps(&robots, &url::Url::parse(other).unwrap()).is_empty(),
                "{other}"
            );
        }
    }

    #[test]
    fn the_card() {
        let rss = entries(&read("theblock-rss.xml"));
        let doc = card(ARTICLE, 3600, find(&rss, ARTICLE).unwrap(), RSS).unwrap();
        assert_eq!(doc.type_name, "Article");
        assert_eq!(doc.source, "feed");
        assert_eq!(doc.attributes["author"], "Jason Shubnell");
        assert_eq!(doc.attributes["feed"], RSS);
        assert!(doc
            .description
            .unwrap()
            .starts_with("Mesh and Visa will let users spend"));
        let untitled = Entry {
            link: ARTICLE.into(),
            ..Default::default()
        };
        assert!(card(ARTICLE, 3600, &untitled, RSS).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_walled_page_is_found_in_its_feed() {
        let (egress, recorded) = fixture::egress(vec![
            (
                ARTICLE,
                403,
                "text/html; charset=UTF-8",
                read("theblock-article.html"),
            ),
            (
                "https://www.theblock.co/feed",
                301,
                "text/html",
                RSS.as_bytes().to_vec(),
            ),
            (
                RSS,
                200,
                "text/xml; charset=UTF-8",
                read("theblock-rss.xml"),
            ),
        ]);
        // Through the web route: the page is a challenge; the feed has the article.
        let web = crate::resolver::web::WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            events: None,
            config: Default::default(),
            egress,
        };
        let doc = crate::resolver::Resolver::resolve(&web, ARTICLE)
            .await
            .unwrap();
        assert_eq!(doc.source, "feed");
        assert_eq!(
            doc.name.as_deref(),
            Some("Mesh taps Visa to let users spend crypto without a separate card")
        );
        assert_eq!(crate::sources::from(&doc), Some("feed"));
        assert_eq!(
            recorded.asked(),
            [ARTICLE, "https://www.theblock.co/feed", RSS]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn news_sitemaps_are_the_last_resort_and_feedless_hosts_are_remembered() {
        let (egress, recorded) = fixture::egress(vec![
            (
                "https://www.theblock.co/robots.txt",
                200,
                "text/plain; charset=utf-8",
                read("theblock-robots.txt"),
            ),
            (
                "https://www.theblock.co/sitemap_tbco_news.xml",
                200,
                "text/xml;charset=UTF-8",
                read("theblock-news-sitemap.xml"),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Feed.walled(&cx, ARTICLE).await.unwrap().unwrap();
        assert_eq!(doc.type_name, "NewsArticle");
        assert_eq!(
            doc.attributes["feed"],
            "https://www.theblock.co/sitemap_tbco_news.xml"
        );
        assert_eq!(recorded.asked().len(), FEEDS.len() + 2);

        // A host with no feed at all asks once a day.
        let page = "https://nofeed.example/article/1";
        assert!(Feed.walled(&cx, page).await.unwrap().is_none());
        let asked = recorded.asked().len();
        // Past the discovery cache's five minutes for failures, the host is still not asked.
        tokio::time::advance(std::time::Duration::from_secs(301)).await;
        assert!(Feed.walled(&cx, page).await.unwrap().is_none());
        assert_eq!(recorded.asked().len(), asked);
    }
}
