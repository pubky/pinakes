//! WordPress posts, on any site running WordPress. A page hook: when a fetched page names its
//! own REST record (a `Link` header or `<link rel="alternate" type="application/json">` to
//! `…/wp-json/wp/v2/posts/{id}`, or the `?p={id}` shortlink), its card takes the post's editorial
//! title, excerpt, dates and featured image from that record. Links are read by the web route,
//! never claimed by their shape. No key.
use super::{text, web_url, Context, Example, Source};
use crate::{
    model::ResourceMeta,
    resolver::egress::{FetchOptions, Fetched},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;
use url::Url;

pub struct Wordpress;

fn post_id(value: &str) -> Option<u64> {
    super::kit::number(value, 12)
}

/// The post a REST address names (`…/wp-json/wp/v2/posts/{id}` or
/// `?rest_route=/wp/v2/posts/{id}`), on the page's origin, and the record's address rebuilt from
/// the id alone: no other path or query of the page's choosing is ever requested.
fn rest_post(url: &Url, page: &Url) -> Option<(Url, u64)> {
    if url.origin() != page.origin() {
        return None;
    }
    let origin = page.origin().ascii_serialization();
    if let Some((_, route)) = url.query_pairs().find(|(k, _)| k == "rest_route") {
        let id = post_id(route.strip_prefix("/wp/v2/posts/")?.trim_end_matches('/'))?;
        let record = Url::parse(&format!("{origin}/?rest_route=/wp/v2/posts/{id}")).ok()?;
        return Some((record, id));
    }
    let path = url.path().trim_end_matches('/');
    let (prefix, id) = path.rsplit_once("/wp-json/wp/v2/posts/")?;
    let id = post_id(id)?;
    // A WordPress installed in a subdirectory (`/blog/wp-json/…`) keeps that directory.
    let clean = prefix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/-_".contains(&b))
        && prefix.len() <= 100;
    clean.then_some(())?;
    let record = Url::parse(&format!("{origin}{prefix}/wp-json/wp/v2/posts/{id}")).ok()?;
    Some((record, id))
}

/// Whether the page's text holds what a post's API link is written with: without it the markup
/// names none, and is not parsed.
fn names_api(page: &Fetched) -> bool {
    let body = String::from_utf8_lossy(&page.body);
    body.contains("wp-json") || body.contains("?p=")
}
/// The site's REST record of the post a page names about itself: (record address, post id).
pub fn post_api(page: &Fetched) -> Option<(Url, u64)> {
    let base = &page.url;
    // `Link: <…/wp-json/wp/v2/posts/7>; rel="alternate"; type="application/json"`.
    let from_header = page.validators.link.as_deref().and_then(|links| {
        links.split('<').skip(1).find_map(|entry| {
            let (href, params) = entry.split_once('>')?;
            let alternate =
                params.contains("rel=\"alternate\"") || params.contains("rel=alternate");
            if !alternate || !params.contains("application/json") {
                return None;
            }
            let url = base.join(href).ok()?;
            rest_post(&url, base)
        })
    });
    if from_header.is_some() {
        return from_header;
    }
    if !names_api(page) {
        return None;
    }
    let body = String::from_utf8_lossy(&page.body);
    let html = crate::resolver::html::document(&body).ok()?;
    let links = scraper::Selector::parse("head link[href]").ok()?;
    let mut shortlink = None;
    for link in html.select(&links).take(64) {
        let el = link.value();
        let (Some(rel), Some(href)) = (el.attr("rel"), el.attr("href")) else {
            continue;
        };
        let Ok(url) = base.join(href) else {
            continue;
        };
        if rel.eq_ignore_ascii_case("alternate") && el.attr("type") == Some("application/json") {
            if let Some(found) = rest_post(&url, base) {
                return Some(found);
            }
        }
        if rel.eq_ignore_ascii_case("shortlink") && url.origin() == base.origin() {
            shortlink = url
                .query_pairs()
                .find(|(k, _)| k == "p")
                .and_then(|(_, v)| post_id(&v));
        }
    }
    // `?rest_route=` works with and without pretty permalinks.
    let id = shortlink?;
    let record = Url::parse(&format!(
        "{}/?rest_route=/wp/v2/posts/{id}",
        base.origin().ascii_serialization()
    ))
    .ok()?;
    Some((record, id))
}

/// The record's address with the featured image embedded and only the fields a card needs.
pub fn record_url(api: &Url) -> String {
    let mut url = api.clone();
    url.query_pairs_mut()
        .append_pair("_embed", "wp:featuredmedia")
        .append_pair(
            "_fields",
            "id,link,title,excerpt,date_gmt,modified_gmt,_links,_embedded",
        );
    url.into()
}

/// HTML (a rendered title or excerpt) as one line of plain text.
fn plain(html: &str) -> String {
    crate::resolver::cards::plain(html)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// WordPress's dates are UTC without an offset (`date_gmt`).
fn utc(value: &Value) -> Option<String> {
    let v = text(value)?;
    chrono::NaiveDateTime::parse_from_str(&v, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|d| {
            d.and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
}

/// The page's card with the post's record, or `None` when the record is not this page's.
pub fn card(doc: &ResourceMeta, page: &Url, id: u64, body: &[u8]) -> Result<Option<ResourceMeta>> {
    let v: Value = serde_json::from_slice(body).context("WordPress answered no JSON")?;
    if v["id"].as_u64() != Some(id) {
        return Ok(None);
    }
    let link = text(&v["link"]).context("no link")?;
    let same = |a: &str, b: &str| {
        let (a, b) = (crate::model::identity(a), crate::model::identity(b));
        matches!((a, b), (Ok(a), Ok(b)) if a.1 == b.1)
    };
    if !same(&link, page.as_str()) {
        return Ok(None);
    }
    let Some(title) = text(&v["title"]["rendered"])
        .map(|t| plain(&t))
        .filter(|t| !t.is_empty())
    else {
        return Ok(None);
    };
    let mut doc = doc.clone();
    doc.name = Some(title);
    let excerpt = text(&v["excerpt"]["rendered"]).map(|e| {
        let e = plain(&e);
        let e = e
            .trim_end_matches("[…]")
            .trim_end_matches("[&hellip;]")
            .trim_end();
        e.chars().take(300).collect::<String>()
    });
    if let Some(excerpt) = excerpt.filter(|e| !e.is_empty()) {
        doc.description = Some(excerpt);
    }
    let image = v["_embedded"]["wp:featuredmedia"][0]["source_url"]
        .as_str()
        .filter(|u| web_url(u).is_some_and(|u| u.scheme() == "https"));
    if let Some(image) = image {
        doc.image = Some(image.to_owned());
    }
    if !doc.type_path.iter().any(|t| t == "Article") {
        doc.set_type("BlogPosting");
    }
    doc.source = "wordpress".into();
    for (key, value) in [
        ("published", utc(&v["date_gmt"])),
        ("modified", utc(&v["modified_gmt"])),
        ("wordpress_post", Some(id.to_string())),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for Wordpress {
    fn name(&self) -> &'static str {
        "wordpress"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "wordpress",
                uri: "https://bitcoinmagazine.com/markets/no-uptober-yet-as-bitcoin-slips",
                card_type: "Article",
                group_with: None,
            }],
            answers: &[
                E2eAnswer {
                    host: "bitcoinmagazine.com",
                    path: "/markets/no-uptober-yet-as-bitcoin-slips",
                    query: &[],
                    header: None,
                    content_type: "text/html; charset=UTF-8",
                    file: "page-no-uptober.html.gz",
                },
                E2eAnswer {
                    host: "bitcoinmagazine.com",
                    path: "/wp-json/wp/v2/posts/55992",
                    query: &[],
                    header: None,
                    content_type: "application/json",
                    file: "post-55992.json",
                },
            ],
        })
    }
    fn reads(&self, _uri: &str) -> bool {
        false
    }
    fn hooks_pages(&self) -> bool {
        true
    }
    fn facts(&self) -> &'static [&'static str] {
        &["published", "modified", "wordpress_post"]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://bitcoinmagazine.com/markets/no-uptober-yet-as-bitcoin-slips",
            card_type: "Article",
        }]
    }
    async fn resolve(&self, _cx: &Context<'_>, _uri: &str) -> Result<ResourceMeta> {
        anyhow::bail!("WordPress pages are read by the web route")
    }
    async fn page(
        &self,
        cx: &Context<'_>,
        _uri: &str,
        page: &Fetched,
        doc: &ResourceMeta,
    ) -> Result<Option<ResourceMeta>> {
        // The cheap test first: only a page with a `Link` header or the marker in its text is
        // copied and waits for a parse.
        if page.validators.link.is_none() && !names_api(page) {
            return Ok(None);
        }
        let found = page.clone();
        let Some((api, id)) = crate::resolver::html::blocking(move || post_api(&found)).await?
        else {
            return Ok(None);
        };
        let f = cx
            .fetch(
                &record_url(&api),
                &FetchOptions {
                    accept: Some("application/json".into()),
                    // No redirect to another host (nor any other): the record is on the page's
                    // origin or not read.
                    sensitive: true,
                    ..Default::default()
                },
            )
            .await?;
        if f.status != 200 {
            return Ok(None);
        }
        let (doc, url) = (doc.clone(), page.url.clone());
        crate::resolver::html::blocking(move || card(&doc, &url, id, &f.body)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    const PAGE: &str = "https://bitcoinmagazine.com/markets/no-uptober-yet-as-bitcoin-slips";
    fn read(file: &str) -> Vec<u8> {
        fixture::read("wordpress", file)
    }
    fn page(link: Option<&str>) -> Fetched {
        fixture::page(
            PAGE,
            "text/html; charset=UTF-8",
            read("page-no-uptober.html.gz"),
            link,
        )
    }
    fn web_card() -> ResourceMeta {
        let mut doc = ResourceMeta::new(PAGE, "opengraph", 3600);
        doc.set_type("NewsArticle");
        doc.name = Some("No 'Uptober' Yet: Bitcoin Slips 2%".into());
        doc
    }

    #[test]
    fn the_page_names_its_record() {
        let api = "https://bitcoinmagazine.com/wp-json/wp/v2/posts/55992";
        // From the `Link` header, as recorded.
        let link = fixture::link("wordpress", "page-no-uptober");
        let (url, id) = post_api(&page(Some(&link))).unwrap();
        assert_eq!((url.as_str(), id), (api, 55992));
        // From the HTML head when no header says it.
        let (url, id) = post_api(&page(None)).unwrap();
        assert_eq!((url.as_str(), id), (api, 55992));
        assert_eq!(
            record_url(&url),
            "https://bitcoinmagazine.com/wp-json/wp/v2/posts/55992?_embed=wp%3Afeaturedmedia&_fields=id%2Clink%2Ctitle%2Cexcerpt%2Cdate_gmt%2Cmodified_gmt%2C_links%2C_embedded"
        );
        // From the shortlink alone, through `rest_route` (with or without pretty permalinks).
        let html = |head: &str| {
            fixture::page(
                "https://w.example/2026/a-post/",
                "text/html",
                format!("<html><head>{head}</head><body></body></html>").into_bytes(),
                None,
            )
        };
        let (url, id) = post_api(&html(
            r#"<link rel="shortlink" href="https://w.example/?p=7">"#,
        ))
        .unwrap();
        assert_eq!(
            (url.as_str(), id),
            ("https://w.example/?rest_route=/wp/v2/posts/7", 7)
        );
        // Another origin's record, a page's (not a post's) and a non-number are not this post.
        for head in [
            r#"<link rel="alternate" type="application/json" href="https://other.example/wp-json/wp/v2/posts/7">"#,
            r#"<link rel="alternate" type="application/json" href="https://w.example/wp-json/wp/v2/pages/7">"#,
            r#"<link rel="shortlink" href="https://other.example/?p=7">"#,
            r#"<link rel="shortlink" href="https://w.example/?p=x">"#,
        ] {
            assert_eq!(post_api(&html(head)), None, "{head}");
        }
        // The record's address is rebuilt from the id: no other path or query of the page's.
        let hostile = Some("<https://w.example/wp-login.php?action=x&rest_route=/wp/v2/posts/1>; rel=\"alternate\"; type=\"application/json\"");
        let (url, _) = post_api(&fixture::page(
            "https://w.example/a",
            "text/html",
            vec![],
            hostile,
        ))
        .unwrap();
        assert_eq!(url.as_str(), "https://w.example/?rest_route=/wp/v2/posts/1");
        let blog = Some("<https://w.example/blog/wp-json/wp/v2/posts/3?x=1>; rel=\"alternate\"; type=\"application/json\"");
        let (url, _) = post_api(&fixture::page(
            "https://w.example/a",
            "text/html",
            vec![],
            blog,
        ))
        .unwrap();
        assert_eq!(url.as_str(), "https://w.example/blog/wp-json/wp/v2/posts/3");
        let header = Some("<https://other.example/wp-json/wp/v2/posts/7>; rel=\"alternate\"; type=\"application/json\"");
        assert_eq!(
            post_api(&fixture::page(
                "https://w.example/a",
                "text/html",
                vec![],
                header
            )),
            None
        );
    }

    #[test]
    fn the_record_makes_the_card() {
        let url = Url::parse(PAGE).unwrap();
        let doc = card(&web_card(), &url, 55992, &read("post-55992.json"))
            .unwrap()
            .unwrap();
        // The editorial title, not the shorter social one.
        assert_eq!(
            doc.name.as_deref(),
            Some("No ‘Uptober’ Yet: Bitcoin Slips 2% in a Month That’s Historically Bullish")
        );
        assert_eq!(doc.type_name, "NewsArticle");
        assert_eq!(doc.source, "wordpress");
        assert_eq!(
            doc.image.as_deref(),
            Some("https://bitcoinmagazine.com/wp-content/uploads/2026/10/Pics-17.jpg")
        );
        assert!(doc
            .description
            .unwrap()
            .starts_with("Bitcoin’s price is currently down"));
        assert_eq!(doc.attributes["published"], "2026-10-19T16:18:41Z");
        assert_eq!(doc.attributes["wordpress_post"], "55992");
        // A record of another post, or of another address, leaves the page's card.
        assert!(card(&web_card(), &url, 55993, &read("post-55992.json"))
            .unwrap()
            .is_none());
        let other = Url::parse("https://bitcoinmagazine.com/markets/another").unwrap();
        assert!(card(&web_card(), &other, 55992, &read("post-55992.json"))
            .unwrap()
            .is_none());
        // A plain web page becomes a blog post.
        let plain = ResourceMeta::new(PAGE, "web", 3600);
        let doc = card(&plain, &url, 55992, &read("post-55992.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.type_name, "BlogPosting");
    }

    #[tokio::test(start_paused = true)]
    async fn the_hook_fetches_the_record_once() {
        let record = "https://bitcoinmagazine.com/wp-json/wp/v2/posts/55992?_embed=wp%3Afeaturedmedia&_fields=id%2Clink%2Ctitle%2Cexcerpt%2Cdate_gmt%2Cmodified_gmt%2C_links%2C_embedded";
        let (egress, recorded) = fixture::egress(vec![(
            record,
            200,
            "application/json",
            read("post-55992.json"),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let link = fixture::link("wordpress", "page-no-uptober");
        let doc = Wordpress
            .page(&cx, PAGE, &page(Some(&link)), &web_card())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.attributes["wordpress_post"], "55992");
        assert_eq!(recorded.asked(), [record]);
    }
}
