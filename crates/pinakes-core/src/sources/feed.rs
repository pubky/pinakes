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

/// A field's text on one line, as the web reader reads the same formats (`web::feed_words`):
/// markup taken out only where the format makes the field HTML (`html`: an RSS description, an
/// Atom `type="html"` text); anywhere else what looks like markup is the literal text it is
/// (`Using <title> in HTML`).
fn one_line(value: &str, html: bool) -> String {
    crate::resolver::web::feed_words(value, html)
}
fn some(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}
/// A feed's date as podcast feeds' are read (`episodes::date`: RFC 2822, RFC 3339, a zone name
/// RFC 2822 does not list, within the stored range), in RFC 3339: with the item's own offset when
/// it gives one (`+0200` stays `+02:00`), in UTC when it names a zone RFC 2822 does not list.
fn date(value: &str) -> Option<String> {
    let instant = crate::episodes::date(value)?;
    let value = value.trim();
    Some(
        chrono::DateTime::parse_from_rfc2822(value)
            .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
            .map_or_else(|_| instant.to_rfc3339(), |d| d.to_rfc3339()),
    )
}
/// An Atom text construct (`title`, `summary`, `content`) on one line, by its `type`: `text`
/// (the default) is plain, `html` (or a content's `text/html`) is HTML escaped as text, and
/// `xhtml` (or `application/xhtml+xml`) is the XHTML it holds, all of it from its first child to
/// its last, read as HTML.
fn atom_text(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    let field = named(node, name)?;
    let text = match field.attribute("type") {
        Some("html" | "text/html") => one_line(field.text()?, true),
        Some("xhtml" | "application/xhtml+xml") => {
            let (first, last) = (field.first_child()?, field.last_child()?);
            let source = field.document().input_text();
            one_line(&source[first.range().start..last.range().end], true)
        }
        _ => one_line(field.text()?, false),
    };
    some(Some(text))
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

/// The most items read from one feed or sitemap.
const ENTRIES: usize = 5000;

/// The items of a feed answer: of the whole body, or of one the read cut at its cap up to the
/// last whole item (`cards::feed_prefix`; a sitemap up to its last whole `url`). `None` when a
/// cut body holds no whole item: nothing is known about the feed then.
pub fn entries_of(body: &[u8], truncated: bool) -> Option<Vec<Entry>> {
    if !truncated {
        return Some(entries(body));
    }
    // A cut inside a character leaves the whole characters before it.
    let text = String::from_utf8_lossy(body);
    let completed = match crate::resolver::cards::feed_prefix(&text, ENTRIES) {
        Some(feed) => feed,
        None => {
            let end = text.rfind("</url>")? + "</url>".len();
            format!("{}</urlset>", &text[..end])
        }
    };
    let found = entries(completed.as_bytes());
    (!found.is_empty()).then_some(found)
}

/// The items of an RSS feed, an Atom feed or a Google News sitemap.
pub fn entries(body: &[u8]) -> Vec<Entry> {
    let Ok(text) = std::str::from_utf8(body) else {
        return vec![];
    };
    let Ok(xml) = crate::resolver::xml::parse(text) else {
        return vec![];
    };
    let mut out = vec![];
    let listed = |n: &roxmltree::Node<'_, '_>| {
        n.is_element() && matches!(n.tag_name().name(), "item" | "entry" | "url")
    };
    for node in xml.descendants().filter(listed).take(ENTRIES) {
        match node.tag_name().name() {
            // RSS 2.0.
            "item" => {
                let Some(link) = text_of(node, "link") else {
                    continue;
                };
                let image_type = |c: &roxmltree::Node<'_, '_>| {
                    c.attribute("type").map(|t| t.starts_with("image/"))
                };
                let image = node
                    .children()
                    .filter(|c| c.is_element())
                    .find(|c| match (c.tag_name().name(), c.attribute("medium")) {
                        ("content" | "thumbnail", Some(medium)) => medium == "image",
                        ("thumbnail", None) => true,
                        // `media:content` that names no medium: a video or a sound by its
                        // type is not the picture.
                        ("content", None) => image_type(c).unwrap_or(true),
                        ("enclosure", _) => image_type(c).unwrap_or(false),
                        _ => false,
                    })
                    .and_then(|c| c.attribute("url"))
                    .map(str::to_owned);
                let authors = node
                    .children()
                    .filter(|c| c.is_element() && c.tag_name().name() == "creator")
                    .filter_map(|c| c.text())
                    .map(|a| one_line(a, false))
                    .filter(|a| !a.is_empty())
                    .collect();
                out.push(Entry {
                    link,
                    title: some(text_of(node, "title").map(|t| one_line(&t, false))),
                    description: some(text_of(node, "description").map(|d| one_line(&d, true))),
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
                    title: atom_text(node, "title"),
                    description: atom_text(node, "summary").or_else(|| atom_text(node, "content")),
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
                    title: some(text_of(news, "title").map(|t| one_line(&t, false))),
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
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "feed",
                    uri: "https://www.theblock.co/news/business/2026-10-19-mesh-visa-partner-spend-crypto-stablecoins-421058",
                    card_type: "Article",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "www.theblock.co",
                    path: "/feed",
                    query: &[],
                    header: None,
                    content_type: "text/xml; charset=UTF-8",
                    file: "theblock-rss.xml",
                },
                E2eAnswer {
                    host: "www.theblock.co",
                    path: "/news/business/2026-10-19-mesh-visa-partner-spend-crypto-stablecoins-421058",
                    query: &[],
                    header: None,
                    content_type: "text/html; charset=UTF-8",
                    file: "theblock-article.html",
                },
            ],
        })
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
        let mut look = |address: String, read: Result<Option<Vec<Entry>>, ()>| {
            let found = match read {
                Ok(Some(found)) => found,
                Ok(None) => return None,
                Err(()) => {
                    failed = true;
                    return None;
                }
            };
            any |= !found.is_empty();
            find(&found, uri).and_then(|e| card(uri, cx.ttl, e, &address))
        };
        for path in FEEDS {
            let address = format!("{origin}{path}");
            let answer = cx.egress.discover(&address, &feed, FEED_TTL).await;
            if let Some(doc) = look(address, feed_entries(answer).await) {
                return Ok(Some(doc));
            }
        }
        let robots = FetchOptions {
            accept: Some("text/plain".into()),
            extra_types: &["text/plain"],
            ..Default::default()
        };
        // A `robots.txt` that failed (a fetch error, a rate limit, a timeout or a server error)
        // says nothing about the host's sitemaps: nothing is concluded then either.
        let mut robots_failed = false;
        let sitemaps = match cx
            .egress
            .discover(&format!("{origin}/robots.txt"), &robots, 86_400)
            .await
        {
            Ok(f) if f.status == 200 => news_sitemaps(&String::from_utf8_lossy(&f.body), &url),
            Ok(f) if !crate::resolver::busy_status(f.status) => vec![],
            _ => {
                robots_failed = true;
                vec![]
            }
        };
        for address in sitemaps {
            let answer = cx.egress.discover(&address, &feed, FEED_TTL).await;
            if let Some(doc) = look(address, feed_entries(answer).await) {
                return Ok(Some(doc));
            }
        }
        if !any && !failed && !robots_failed {
            FEEDLESS.set(&host, false);
        }
        Ok(None)
    }
}

/// A feed answer's entries, parsed in a parse slot: `Err` when nothing can be said about the
/// feed (the request failed, the host answered a rate limit, a timeout or a server error, or the
/// body was cut before its first whole item), `None` when the host has no feed there.
async fn feed_entries(
    answer: Result<crate::resolver::egress::Fetched>,
) -> std::result::Result<Option<Vec<Entry>>, ()> {
    let f = answer.map_err(|_| ())?;
    match f.status {
        200 => {}
        status if crate::resolver::busy_status(status) => return Err(()),
        _ => return Ok(None),
    }
    crate::resolver::html::blocking(move || entries_of(&f.body, f.truncated))
        .await
        .map_err(|_| ())?
        .map(Some)
        .ok_or(())
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

    /// An RSS title, an Atom `text` title (the default) and a news sitemap's title are plain
    /// text: what looks like markup is the literal text. An RSS description and an Atom `html` or
    /// `xhtml` text are HTML: their markup goes, their words stay whole.
    #[test]
    fn a_plain_feed_title_keeps_text_that_looks_like_markup() {
        let rss = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>T</title><item><title>Using &lt;title&gt; in HTML</title><link>https://example.org/a</link><description>&lt;p&gt;Why H&lt;sub&gt;2&lt;/sub&gt;O &lt;b&gt;boils&lt;/b&gt;&lt;/p&gt;</description></item></channel></rss>"#;
        let atom = br#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>T</title><entry><title type="text">Using &lt;title&gt; in HTML</title><link href="https://example.org/b"/><summary type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><p>Why H<sub>2</sub>O</p><p>boils</p></div></summary></entry><entry><title>Using &lt;title&gt; in HTML</title><link href="https://example.org/c"/><content type="html">&lt;p&gt;An &amp;amp; B&lt;/p&gt;</content></entry></feed>"#;
        let news = br#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:news="http://www.google.com/schemas/sitemap-news/0.9"><url><loc>https://example.org/d</loc><news:news><news:title>Using &lt;title&gt; in HTML</news:title></news:news></url></urlset>"#;
        let read = |body: &[u8]| -> Vec<(Option<String>, Option<String>)> {
            entries(body)
                .into_iter()
                .map(|e| (e.title, e.description))
                .collect()
        };
        let title = Some("Using <title> in HTML".to_owned());
        assert_eq!(
            read(rss),
            [(title.clone(), Some("Why H2O boils".to_owned()))]
        );
        assert_eq!(
            read(atom),
            [
                (title.clone(), Some("Why H2O boils".to_owned())),
                (title.clone(), Some("An & B".to_owned()))
            ]
        );
        assert_eq!(read(news), [(title, None)]);
        // XHTML without the one wrapping `div` is read whole; MIME types name the same kinds.
        let kinds = br#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><title>A</title><link href="https://example.org/e"/><content type="xhtml"><p xmlns="http://www.w3.org/1999/xhtml">a</p><p xmlns="http://www.w3.org/1999/xhtml">b &amp; c</p></content></entry><entry><title>B</title><link href="https://example.org/f"/><content type="text/html">&lt;p&gt;H&lt;sub&gt;2&lt;/sub&gt;O&lt;/p&gt;</content></entry><entry><title>C</title><link href="https://example.org/g"/><content type="application/xhtml+xml"><div xmlns="http://www.w3.org/1999/xhtml"><b>bold</b> text</div></content></entry></feed>"#;
        assert_eq!(
            read(kinds),
            [
                (Some("A".to_owned()), Some("a b & c".to_owned())),
                (Some("B".to_owned()), Some("H2O".to_owned())),
                (Some("C".to_owned()), Some("bold text".to_owned()))
            ]
        );
    }

    /// An item dated with a zone name RFC 2822 does not list is dated all the same, as podcast
    /// episodes are (`episodes::date`): `UTC` is `+0000`, any other name `-0000`, stored in UTC;
    /// GMT and EST are RFC 2822's own. A date with its own offset keeps it. A date past the stored
    /// range dates nothing.
    #[test]
    fn a_date_with_a_zone_name_is_read() {
        let published = |date: &str| {
            let rss = format!(
                r#"<?xml version="1.0"?><rss version="2.0"><channel><title>T</title><item><title>A</title><link>https://example.org/a</link><pubDate>{date}</pubDate></item></channel></rss>"#
            );
            entries(rss.as_bytes())
                .first()
                .and_then(|e| e.published.clone())
        };
        for (date, read) in [
            (
                "Tue, 10 Nov 2026 10:00:00 UTC",
                Some("2026-11-10T10:00:00+00:00"),
            ),
            (
                "Tue, 20 Oct 2026 10:00:00 CEST",
                Some("2026-10-20T10:00:00+00:00"),
            ),
            (
                "Tue, 20 Oct 2026 10:00:00 GMT",
                Some("2026-10-20T10:00:00+00:00"),
            ),
            (
                "Tue, 20 Oct 2026 12:00:00 +0200",
                Some("2026-10-20T12:00:00+02:00"),
            ),
            (
                "2026-10-20T12:00:00+02:00",
                Some("2026-10-20T12:00:00+02:00"),
            ),
            (
                "Tue, 20 Oct 2026 10:00:00 EST",
                Some("2026-10-20T10:00:00-05:00"),
            ),
            ("2026-10-20T10:00:00Z", Some("2026-10-20T10:00:00+00:00")),
            ("Tue, 20 Oct 2026 10:00:00 PM", None),
            ("Fri, 31 Dec 9999 23:59:60 +0000", None),
        ] {
            assert_eq!(published(date).as_deref(), read, "{date}");
        }
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

        let atom = br#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title type="html">An &lt;b&gt;entry&lt;/b&gt;</title>
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

    /// An item's `media:content` is its image by its `medium`, else by its `type`: a video is
    /// not the picture, and the thumbnail after it is.
    #[test]
    fn a_video_is_not_an_items_image() {
        let image = |media: &str| {
            let rss = format!(
                r#"<rss xmlns:media="http://search.yahoo.com/mrss/"><channel><item>
                <link>https://a.example/post/1</link><title>A post</title>{media}</item></channel></rss>"#
            );
            entries(rss.as_bytes()).remove(0).image
        };
        let video = r#"<media:content url="https://a.example/v.mp4" type="video/mp4"/>"#;
        let thumbnail = r#"<media:thumbnail url="https://a.example/t.jpg"/>"#;
        assert_eq!(image(video), None);
        assert_eq!(
            image(&format!("{video}{thumbnail}")).as_deref(),
            Some("https://a.example/t.jpg")
        );
        assert_eq!(
            image(r#"<media:content url="https://a.example/s.mp3" type="audio/mpeg"/>"#),
            None
        );
        for picture in [
            r#"<media:content url="https://a.example/p.jpg"/>"#,
            r#"<media:content url="https://a.example/p.jpg" type="image/jpeg"/>"#,
            r#"<media:content url="https://a.example/p.jpg" medium="image"/>"#,
            r#"<enclosure url="https://a.example/p.jpg" type="image/jpeg"/>"#,
        ] {
            assert_eq!(
                image(picture).as_deref(),
                Some("https://a.example/p.jpg"),
                "{picture}"
            );
        }
        assert_eq!(
            image(r#"<media:content url="https://a.example/v.mp4" medium="video"/>"#),
            None
        );
    }

    /// A sitemap's entries are counted, not its elements: a link far down a long news sitemap
    /// is found.
    #[test]
    fn a_long_sitemap_is_read_by_its_entries() {
        let mut sitemap = String::from(
            r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:news="http://www.google.com/schemas/sitemap-news/0.9">"#,
        );
        for n in 0..1500 {
            sitemap.push_str(&format!(
                "<url><loc>https://n.example/a/{n}</loc><news:news><news:publication>\
                 <news:name>N</news:name><news:language>en</news:language></news:publication>\
                 <news:publication_date>2026-10-19T16:09:02+00:00</news:publication_date>\
                 <news:title>Article {n}</news:title></news:news></url>"
            ));
        }
        sitemap.push_str("</urlset>");
        let found = entries(sitemap.as_bytes());
        assert_eq!(found.len(), 1500);
        assert!(find(&found, "https://n.example/a/1499").is_some());
    }

    /// A feed the read cut at its cap is read up to its last whole item, and a sitemap up to
    /// its last whole `url`.
    #[test]
    fn a_cut_feed_keeps_its_whole_items() {
        let rss = read("theblock-rss.xml");
        let all = entries(&rss);
        let at = String::from_utf8_lossy(&rss).rfind("<item>").unwrap();
        let cut = entries_of(&rss[..at + 40], true).unwrap();
        assert_eq!(cut.len(), all.len() - 1);
        assert!(find(&cut, ARTICLE).is_some());
        let sitemap = read("theblock-news-sitemap.xml");
        let at = String::from_utf8_lossy(&sitemap).rfind("<url>").unwrap();
        let cut = entries_of(&sitemap[..at + 20], true).unwrap();
        assert!(find(&cut, ARTICLE).is_some());
        assert_eq!(
            entries_of(b"<rss><channel><item><link>https://a.exa", true),
            None
        );
        assert_eq!(entries_of(b"<html>", false), Some(vec![]));
    }

    /// A feed over the read's cap still names its article, and neither it nor a server error
    /// marks the host as having no feed.
    #[tokio::test(start_paused = true)]
    async fn a_cut_feed_or_a_server_error_does_not_mark_a_host_feedless() {
        let rss = String::from_utf8(read("theblock-rss.xml"))
            .unwrap()
            .replace("www.theblock.co", "big.example");
        let cut = rss.as_bytes()[..rss.rfind("<item>").unwrap() + 40].to_vec();
        let (egress, recorded) = fixture::egress(vec![
            ("https://big.example/feed", 200, "!large-xml", cut),
            ("https://busy.example/feed", 503, "text/html", vec![]),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let article = ARTICLE.replace("www.theblock.co", "big.example");
        let doc = Feed.walled(&cx, &article).await.unwrap().unwrap();
        assert_eq!(doc.source, "feed");
        // The busy host is asked again once the discovery cache lets it.
        let page = "https://busy.example/article/1";
        assert!(Feed.walled(&cx, page).await.unwrap().is_none());
        let asked = recorded.asked().len();
        tokio::time::advance(std::time::Duration::from_secs(301)).await;
        assert!(Feed.walled(&cx, page).await.unwrap().is_none());
        assert!(recorded.asked().len() > asked, "the host was memoised");
    }

    /// A `robots.txt` that answers a server error says nothing about the host's news sitemaps: a
    /// host whose feeds list nothing is not remembered as feedless then, and is asked again.
    #[tokio::test(start_paused = true)]
    async fn a_failed_robots_txt_does_not_mark_a_host_feedless() {
        let (egress, recorded) = fixture::egress(vec![(
            "https://robots-down.example/robots.txt",
            503,
            "text/plain",
            vec![],
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let page = "https://robots-down.example/article/1";
        assert!(Feed.walled(&cx, page).await.unwrap().is_none());
        let asked = recorded.asked().len();
        // Past the discovery cache's five minutes for failures, the host is asked again.
        tokio::time::advance(std::time::Duration::from_secs(301)).await;
        assert!(Feed.walled(&cx, page).await.unwrap().is_none());
        assert!(recorded.asked().len() > asked, "the host was memoised");
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
