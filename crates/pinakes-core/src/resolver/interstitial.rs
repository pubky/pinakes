//! Interstitials are observations of access failure, never publisher identity evidence.
use super::egress::Fetched;
use scraper::Selector;
use serde::Deserialize;
use std::sync::LazyLock;
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
/// The challenge-page markers in `data/interstitial-markers.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Walls {
    text: Vec<String>,
    /// Challenge vendors' wording that a real work could also carry as its name ("Pardon Our
    /// Interruption"): evidence in the title or first heading on a challenge status. On a short
    /// page it counts only with a sign that the page runs a challenge (`challenge_signatures`,
    /// or a `noscript` asking for JavaScript on a page without links) and describes no object
    /// (no `og:title`, `twitter:title`, JSON-LD or microformat name).
    challenge_text: Vec<String>,
    /// Lower-case strings in the page source that only a challenge page carries: the vendors'
    /// challenge script paths and ids.
    challenge_signatures: Vec<String>,
    /// Whole page titles that name only the site, never an object, by the site's domain: the
    /// page a service shows in place of the object (YouTube's regional or consent page is titled
    /// "- YouTube"). A title counts only on its domain and the domain's subdomains.
    placeholder_titles: std::collections::BTreeMap<String, Vec<String>>,
    captcha_selectors: Vec<String>,
    wall_host_prefixes: Vec<String>,
    wall_paths: Vec<String>,
}
static WALLS: LazyLock<Walls> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../data/interstitial-markers.json"))
        .expect("interstitial markers")
});
impl Walls {
    fn marked(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.text
            .iter()
            .any(|marker| text.contains(marker.as_str()))
    }
    fn challenge_marked(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.challenge_text
            .iter()
            .any(|marker| text.contains(marker.as_str()))
    }
}
fn select(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}
fn quoted(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let quote = s.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    s[1..].split_once(quote).map(|(v, _)| v)
}
/// `N`, `N; url=X`, `N;URL='X'` or `N, X`: the delay and an optional target.
fn refresh(content: &str) -> (f64, Option<String>) {
    let (delay, rest) = match content.find([';', ',']) {
        Some(i) => (&content[..i], Some(&content[i + 1..])),
        None => (content, None),
    };
    let delay = delay.trim().parse().unwrap_or(0.0);
    let target = rest.map(str::trim).map(|s| {
        // Page content: byte 3 may fall inside a multi-byte character.
        match s.get(..3).zip(s.get(3..)) {
            Some((p, rest)) if p.eq_ignore_ascii_case("url") => {
                rest.trim_start().strip_prefix('=').unwrap_or(rest).trim()
            }
            _ => s,
        }
    });
    let target = target
        .map(|s| s.trim_matches(['\'', '"']).trim())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    (delay, target)
}
fn wall_path(walls: &Walls, path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    walls.wall_paths.iter().any(|p| {
        path == *p
            || path
                .strip_prefix(p.as_str())
                .is_some_and(|rest| rest.starts_with(['/', '.']))
    })
}
/// A challenge page (bot wall, captcha, sign-in redirect) is `Blocked`, a meta refresh or
/// script redirect stub is a `Redirect` hop, anything else is content.
/// `Err` when the answer is too complex to parse.
pub fn inspect(requested: &Url, f: &Fetched) -> anyhow::Result<Page> {
    let walls = &*WALLS;
    // A redirect from the requested host to a challenge or sign-in host of the same site. A
    // page referenced on such a host itself (`login.example.org/help`) is content.
    if let (Some(a), Some(b)) = (requested.host_str(), f.url.host_str()) {
        if a != b
            && psl::domain_str(a).is_some()
            && psl::domain_str(a) == psl::domain_str(b)
            && walls
                .wall_host_prefixes
                .iter()
                .any(|p| b.starts_with(p.as_str()))
        {
            return Ok(Page::Blocked);
        }
    }
    // A requested page redirected to a sign-in path (LinkedIn /authwall, Facebook /login/).
    // Case or slash normalisation is not a redirect, and a sign-in page moving within its
    // own flow (/signin to /signin/callback) is the page that was asked for.
    let normal = |p: &str| p.trim_end_matches('/').to_ascii_lowercase();
    if normal(f.url.path()) != normal(requested.path())
        && !wall_path(walls, requested.path())
        && wall_path(walls, f.url.path())
    {
        return Ok(Page::Blocked);
    }
    if !f.content_type.contains("html")
        && !(matches!(f.status, 403 | 429 | 503)
            && matches!(f.content_type.as_str(), "text/plain" | ""))
    {
        return Ok(Page::Content);
    }
    // "Accepted" with nothing in it is a challenge (AWS WAF answers bots so), never a page; an
    // API's empty 202 (not HTML) is its own answer.
    if f.status == 202 && f.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Page::Blocked);
    }
    let text = String::from_utf8_lossy(&f.body);
    let html = crate::resolver::html::document(&text)?;
    let title = html
        .select(&select("title"))
        .next()
        .map(|v| v.text().collect::<String>())
        .unwrap_or_default();
    // Exclude script/style/noscript text: client-rendered shells say "enable JavaScript"
    // there, and a real article merely discussing a challenge is not a wall.
    let visible = html
        .select(&select("body"))
        .next()
        .map(|el| {
            el.descendants()
                .filter_map(|n| n.value().as_text().map(|t| (n, t)))
                .filter(|(n, _)| {
                    !n.ancestors().take(super::html::MAX_ANCESTORS).any(|a| {
                        a.value().as_element().is_some_and(|e| {
                            matches!(e.name(), "script" | "style" | "noscript" | "template")
                        })
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
    // "Accepted" with nothing to read is a challenge (AWS WAF answers bots so: a script that
    // reloads the page), never the page.
    if f.status == 202 && visible.trim().is_empty() {
        return Ok(Page::Blocked);
    }
    let challenge = matches!(f.status, 403 | 429 | 503);
    let short = visible.trim().len() < 2000;
    // Titles and headings are only evidence on short or challenge pages: a film called
    // "Just a Moment" is not a wall. Captcha widgets in comment forms need the same context.
    // A page titled only with its site's name, below the site's root, is the service's
    // stand-in for the object. Where the page is is where the fetch ended (a short link that
    // redirected to YouTube lands there), not where it began.
    let landed = &f.url;
    let host = landed.host_str().unwrap_or_default().to_ascii_lowercase();
    let placeholder = walls
        .placeholder_titles
        .iter()
        .filter(|(domain, _)| host == **domain || host.ends_with(&format!(".{domain}")))
        .flat_map(|(_, titles)| titles)
        .any(|t| t.eq_ignore_ascii_case(title.trim()));
    if placeholder && !landed.path().trim_end_matches('/').is_empty() {
        return Ok(Page::Blocked);
    }
    let title_marked = walls.marked(&title);
    let vendor_marked =
        walls.challenge_marked(&title) || (heading.len() < 200 && walls.challenge_marked(&heading));
    // What the page reader names a card from: `og:title` or `twitter:title` (by `property` or
    // `name`, any case), JSON-LD, or a microformat name.
    let describes_object = || {
        html.select(&select("meta")).any(|el| {
            let v = el.value();
            v.attr("property")
                .or_else(|| v.attr("name"))
                .is_some_and(|k| {
                    k.eq_ignore_ascii_case("og:title") || k.eq_ignore_ascii_case("twitter:title")
                })
                && v.attr("content").is_some_and(|c| !c.trim().is_empty())
        }) || html.select(&select("script[type]")).any(|el| {
            el.value()
                .attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/ld+json"))
        }) || html
            .select(&select(
                ".h-entry .p-name, .h-card .p-name, .h-event .p-name",
            ))
            .next()
            .is_some()
    };
    // Positive signs of a challenge: a vendor's challenge script in the source, or a page whose
    // only other content asks for JavaScript (MusicBrainz' proof-of-work gate) and links nowhere.
    let runs_challenge = || {
        let source = text.to_lowercase();
        walls
            .challenge_signatures
            .iter()
            .any(|sig| source.contains(sig.as_str()))
            || (html.select(&select("noscript")).any(|n| {
                // The parser keeps a noscript's body as raw text (markup included).
                n.text()
                    .collect::<String>()
                    .to_lowercase()
                    .contains("javascript")
            }) && html.select(&select("body a[href]")).next().is_none())
    };
    if ((challenge || short) && (title_marked || (heading.len() < 200 && walls.marked(&heading))))
        || (short && walls.marked(&visible))
        || (challenge && vendor_marked)
        || (short
            && (vendor_marked || walls.challenge_marked(&visible))
            && runs_challenge()
            && !describes_object())
        || ((challenge || short || title_marked)
            && walls
                .captcha_selectors
                .iter()
                .any(|s| html.select(&select(s)).next().is_some()))
    {
        return Ok(Page::Blocked);
    }
    for el in html.select(&select("meta[http-equiv]")) {
        if el
            .value()
            .attr("http-equiv")
            .is_some_and(|s| s.eq_ignore_ascii_case("refresh"))
        {
            // A refresh without a target is an auto-reload, and a slow refresh of a full page
            // or of itself is content. Immediate refreshes (a self-refresh too: a cookie
            // challenge, bounded by the hop limit) and short stubs are redirect hops.
            let (delay, target) = refresh(el.value().attr("content").unwrap_or(""));
            if let Some(target) = target {
                let itself = f.url.join(&target).is_ok_and(|t| t == f.url);
                if delay <= 10.0 || (short && !itself) {
                    return Ok(Page::Redirect(target));
                }
            }
        }
    }
    for script in html
        .select(&select("script"))
        .filter(|_| visible.trim().len() < 500)
    {
        let body: String = script.text().collect();
        for marker in ["location.replace(", "location.assign("] {
            if let Some((_, tail)) = body.split_once(marker) {
                if let Some(target) = quoted(tail) {
                    return Ok(Page::Redirect(target.to_owned()));
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
            return Ok(Page::Redirect(target.to_owned()));
        }
        if let Some((_, target)) = title.split_once(':') {
            if !target.trim().is_empty() {
                return Ok(Page::Redirect(target.trim().into()));
            }
        }
        return Ok(Page::Blocked);
    }
    Ok(Page::Content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_file_is_bounded_lower_case_and_parses() {
        let w = &*WALLS;
        for list in [
            &w.text,
            &w.challenge_text,
            &w.challenge_signatures,
            &w.captcha_selectors,
            &w.wall_host_prefixes,
            &w.wall_paths,
        ] {
            assert!(!list.is_empty() && list.len() <= 256);
            assert!(list.iter().all(|v| !v.trim().is_empty() && v.len() <= 256));
        }
        for v in w
            .text
            .iter()
            .chain(&w.challenge_text)
            .chain(&w.challenge_signatures)
            .chain(&w.wall_host_prefixes)
            .chain(&w.wall_paths)
        {
            assert_eq!(*v, v.to_lowercase());
        }
        assert!(w.wall_paths.iter().all(|p| p.starts_with('/')));
        for s in &w.captcha_selectors {
            assert!(Selector::parse(s).is_ok(), "{s}");
        }
    }
    #[test]
    fn an_empty_accepted_answer_is_a_challenge() {
        let url = Url::parse("https://www.imdb.com/title/tt0133093/").unwrap();
        let fetched = |status: u16, body: &[u8]| Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            body: body.to_vec(),
            content_type: "text/html".into(),
            status,
            location: None,
            retry_after_secs: 60,
            validators: Default::default(),
        };
        assert_eq!(inspect(&url, &fetched(202, b"")).unwrap(), Page::Blocked);
        assert_eq!(inspect(&url, &fetched(202, b" \n")).unwrap(), Page::Blocked);
        // AWS WAF's challenge page: scripts and a <noscript> notice, nothing to read.
        let waf = b"<html><head><title></title><script>window.awsWafCookieDomainList = ['imdb.com'];</script><script src=\"https://x.token.awswaf.com/challenge.js\"></script></head><body><div id=\"challenge-container\"></div><noscript><h1>JavaScript is disabled</h1>In order to continue, we need to verify that you're not a robot.</noscript></body></html>";
        assert_eq!(inspect(&url, &fetched(202, waf)).unwrap(), Page::Blocked);
        assert_eq!(
            inspect(
                &url,
                &fetched(
                    202,
                    b"<title>Queued</title><p>Your export is being prepared.</p>"
                )
            )
            .unwrap(),
            Page::Content
        );
        assert_eq!(inspect(&url, &fetched(200, b"")).unwrap(), Page::Content);
        let mut api = fetched(202, b"");
        api.content_type = "application/json".into();
        assert_eq!(
            inspect(&url, &api).unwrap(),
            Page::Content,
            "an API's empty 202"
        );
    }
    #[test]
    fn verification_pages_are_walls_never_names() {
        let url =
            Url::parse("https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862")
                .unwrap();
        let fetched = |status: u16, body: &str| Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            body: body.as_bytes().to_vec(),
            content_type: "text/html; charset=utf-8".into(),
            status,
            location: None,
            retry_after_secs: 60,
            validators: Default::default(),
        };
        // MusicBrainz' proof-of-work gate (2026-11-10), answered with 200 to a plain client.
        let musicbrainz = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\n<title>Verifying your browser</title>\n<style>body{display:flex}</style>\n</head><body><div class=\"box\"><div class=\"spinner\"></div>\n<p>Verifying your browser, please wait…</p>\n<noscript><p><b>JavaScript is required to access this page.</b></p></noscript>\n</div><script>\n(async()=>{const f=document.createElement(\"form\");f.method=\"POST\";f.action=\"/__meb_verify\";})()\n</script></body></html>";
        assert_eq!(
            inspect(&url, &fetched(200, musicbrainz)).unwrap(),
            Page::Blocked
        );
        // Anubis, Cloudflare's newer wording, Imperva and Vercel, each with its challenge script:
        // by title or by heading.
        for page in [
            "<html><head><title>Making sure you&#39;re not a bot!</title><script id=\"anubis_challenge\" type=\"application/json\">{}</script></head><body><h1>Making sure you&#39;re not a bot!</h1><p>Loading...</p><p>Protected by <a href=\"https://github.com/TecharoHQ/anubis\">Anubis</a></p><script src=\"/.within.website/x/cmd/anubis/static/js/main.mjs\"></script></body></html>",
            "<html><head><title>Making sure you’re not a bot!</title></head><body><p>Loading...</p><script src=\"/.within.website/x/cmd/anubis/static/js/main.mjs\"></script></body></html>",
            "<html><head><title>example.org</title></head><body><h1>Verifying you are human. This may take a few seconds.</h1><script src=\"/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1?ray=1\"></script></body></html>",
            "<html><head><title>Pardon Our Interruption</title><script src=\"/_Incapsula_Resource?SWJIYLWA=719d34d31c8e3a6e\"></script></head><body><p>As you were browsing something about your browser made us think you were a bot.</p></body></html>",
            "<html><head><title>Vercel Security Checkpoint</title></head><body><p>We're verifying your browser</p><script src=\"/.well-known/vercel/security/static/challenge.v2.min.js\"></script></body></html>",
        ] {
            assert_eq!(inspect(&url, &fetched(200, page)).unwrap(), Page::Blocked, "{page}");
            assert_eq!(inspect(&url, &fetched(429, page)).unwrap(), Page::Blocked, "{page}");
        }
        // A real page that only carries such words in its title (a long article) is content.
        let article = format!(
            "<html><head><title>Verifying your browser: how bot walls work</title></head><body><p>{}</p></body></html>",
            "A long article about challenge pages. ".repeat(80)
        );
        assert_eq!(
            inspect(&url, &fetched(200, &article)).unwrap(),
            Page::Content
        );
        // Short real pages that carry the wording describe an object: a film named "Pardon Our
        // Interruption", a news brief about MusicBrainz' new gate. They are content, unless the
        // answer is itself a challenge status.
        for page in [
            "<html><head><title>Pardon Our Interruption (2019)</title><meta property=\"og:title\" content=\"Pardon Our Interruption\"></head><body><h1>Pardon Our Interruption</h1><p>A short film.</p></body></html>",
            "<html><head><title>MusicBrainz now shows Verifying your browser</title><meta name=\"twitter:title\" content=\"MusicBrainz gate\"></head><body><p>Plain clients now see a page saying verifying your browser.</p></body></html>",
            "<html><head><title>Making sure you're not a bot</title><script type=\"application/ld+json\">{\"@type\":\"MusicRecording\"}</script></head><body><p>A song.</p></body></html>",
            "<html><head><title>Pardon Our Interruption</title><meta name=\"OG:Title\" content=\"Pardon Our Interruption\"></head><body><p>A short film.</p></body></html>",
            "<html><head><title>Pardon Our Interruption</title></head><body><article class=\"h-entry\"><h1 class=\"p-name\">Pardon Our Interruption</h1></article></body></html>",
        ] {
            assert_eq!(inspect(&url, &fetched(200, page)).unwrap(), Page::Content, "{page}");
            assert_eq!(inspect(&url, &fetched(429, page)).unwrap(), Page::Blocked, "{page}");
        }
        let empty = "<html><head><title>Verifying your browser</title><meta property=\"og:title\" content=\" \"></head><body><p>Please wait</p><noscript>JavaScript is required.</noscript></body></html>";
        assert_eq!(inspect(&url, &fetched(200, empty)).unwrap(), Page::Blocked);
        // Without a sign of a challenge the wording alone never makes a wall at 200: a plain film
        // page, a microdata page, a page that only runs its own script, a page that asks for
        // JavaScript but links on.
        for page in [
            "<html><head><title>Pardon Our Interruption</title></head><body><p>A short film.</p></body></html>",
            "<html><head><title>Pardon Our Interruption</title></head><body><div itemscope itemtype=\"https://schema.org/Movie\"><h1 itemprop=\"name\">Pardon Our Interruption</h1></div></body></html>",
            "<html><head><title>Verifying your browser</title><script src=\"/app.js\"></script></head><body><p>A song.</p></body></html>",
            "<html><head><title>Making sure you're not a bot</title></head><body><noscript>Enable JavaScript for the player.</noscript><a href=\"/album\">Album</a></body></html>",
        ] {
            assert_eq!(inspect(&url, &fetched(200, page)).unwrap(), Page::Content, "{page}");
            assert_eq!(inspect(&url, &fetched(503, page)).unwrap(), Page::Blocked, "{page}");
        }
    }
    #[test]
    fn refresh_parsing_survives_multi_byte_content() {
        let tokens = [
            "0", "5", ";", ",", " ", "url", "URL", "=", "'", "\"", "€", "é", "😀", "a", "/x", "ü",
        ];
        // Every sequence of up to four tokens, so each byte offset meets a multi-byte char.
        let mut inputs = vec![String::new()];
        for _ in 0..4 {
            let next: Vec<String> = inputs
                .iter()
                .flat_map(|s| tokens.iter().map(move |t| format!("{s}{t}")))
                .collect();
            inputs.extend(next);
            inputs.sort();
            inputs.dedup();
        }
        assert!(inputs.len() > 60_000);
        for s in &inputs {
            let (_, target) = refresh(s);
            assert!(target.is_none_or(|t| !t.is_empty()), "{s}");
        }
        assert_eq!(refresh("5; a€b"), (5.0, Some("a€b".into())));
        assert_eq!(refresh("0;url€=x"), (0.0, Some("€=x".into())));
        assert_eq!(refresh("1; URL='/next'"), (1.0, Some("/next".into())));
        assert_eq!(refresh("1, ur"), (1.0, Some("ur".into())));
    }
}
