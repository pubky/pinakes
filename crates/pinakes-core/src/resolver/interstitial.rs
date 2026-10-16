//! Interstitials are observations of access failure, never publisher identity evidence.
use super::egress::Fetched;
use scraper::{Html, Selector};
use url::Url;

#[derive(Debug)]
pub struct Blocked;
impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream access interstitial")
    }
}
impl std::error::Error for Blocked {}

#[derive(Debug, PartialEq)]
pub enum Page {
    Content,
    Redirect(String),
    Blocked,
}
fn marked(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "request access",
        "just a moment",
        "attention required",
        "access denied",
        "verify you are human",
        "enable javascript",
        "unusual traffic",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}
fn select(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}
fn quoted(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let quote = s.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    s[1..].split_once(quote).map(|(v, _)| v)
}
pub fn inspect(requested: &Url, f: &Fetched) -> Page {
    if let (Some(a), Some(b)) = (requested.host_str(), f.url.host_str()) {
        if psl::domain_str(a).is_some()
            && psl::domain_str(a) == psl::domain_str(b)
            && ["unblock.", "challenge.", "consent.", "login."]
                .iter()
                .any(|p| b.starts_with(p))
        {
            return Page::Blocked;
        }
    }
    if !f.content_type.contains("html") {
        return Page::Content;
    }
    let text = String::from_utf8_lossy(&f.body);
    let html = Html::parse_document(&text);
    let title = html
        .select(&select("title"))
        .next()
        .map(|v| v.text().collect::<String>())
        .unwrap_or_default();
    // Exclude script/style text; a real article merely discussing a challenge is not a wall.
    let visible = html
        .select(&select("body"))
        .next()
        .map(|el| {
            el.descendants()
                .filter_map(|n| n.value().as_text().map(|t| (n, t)))
                .filter(|(n, _)| {
                    !n.ancestors().any(|a| {
                        a.value()
                            .as_element()
                            .is_some_and(|e| matches!(e.name(), "script" | "style"))
                    })
                })
                .map(|(_, t)| t.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let heading = html
        .select(&select("h1"))
        .next()
        .map(|v| v.text().collect::<String>())
        .unwrap_or_default();
    if marked(&title)
        || (heading.len() < 200 && marked(&heading))
        || (visible.len() < 2000 && marked(&visible))
        || html
            .select(&select(
                ".g-recaptcha, .h-captcha, .cf-turnstile, #challenge-form, #cf-challenge-running",
            ))
            .next()
            .is_some()
    {
        return Page::Blocked;
    }
    for el in html.select(&select("meta[http-equiv]")) {
        if el
            .value()
            .attr("http-equiv")
            .is_some_and(|s| s.eq_ignore_ascii_case("refresh"))
        {
            if let Some(content) = el.value().attr("content") {
                if let Some((_, target)) = content
                    .split_once(';')
                    .and_then(|(_, s)| s.trim().split_once('='))
                {
                    return Page::Redirect(target.trim().trim_matches(['\'', '"']).to_owned());
                }
            }
            return Page::Blocked;
        }
    }
    for script in html
        .select(&select("script"))
        .filter(|_| visible.trim().len() < 500)
    {
        let body = script.inner_html();
        for marker in ["location.replace(", "location.assign("] {
            if let Some((_, tail)) = body.split_once(marker) {
                if let Some(target) = quoted(tail) {
                    return Page::Redirect(target.to_owned());
                }
            }
        }
    }
    let redirect_title = title.trim().to_ascii_lowercase().starts_with("redirecting");
    let redirect_body = visible
        .trim()
        .to_ascii_lowercase()
        .starts_with("redirecting");
    if redirect_title || (redirect_body && visible.len() < 500) {
        if let Some(target) = html
            .select(&select("a[href]"))
            .next()
            .and_then(|a| a.value().attr("href"))
        {
            return Page::Redirect(target.to_owned());
        }
        if let Some((_, target)) = title.split_once(':') {
            if !target.trim().is_empty() {
                return Page::Redirect(target.trim().into());
            }
        }
        return Page::Blocked;
    }
    Page::Content
}
