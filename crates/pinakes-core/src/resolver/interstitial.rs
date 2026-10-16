//! Interstitials are observations of access failure, never publisher identity evidence.
use super::egress::Fetched;
use anyhow::{anyhow, ensure, Result};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, RwLock},
};
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
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WallOverride {
    Content,
    Blocked,
}
/// Wall heuristics are data: operators replace the file or override single hosts.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Walls {
    pub text: Vec<String>,
    pub captcha_selectors: Vec<String>,
    pub wall_host_prefixes: Vec<String>,
    pub wall_paths: Vec<String>,
    #[serde(default)]
    pub overrides: BTreeMap<String, WallOverride>,
}
static BUILTIN: LazyLock<Walls> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../data/interstitial-markers.json"))
        .expect("interstitial markers")
});
static INSTALLED: LazyLock<RwLock<Arc<Walls>>> =
    LazyLock::new(|| RwLock::new(Arc::new(BUILTIN.clone())));
impl Default for Walls {
    fn default() -> Self {
        BUILTIN.clone()
    }
}
impl Walls {
    /// The data file (or its configured replacement) plus per-host config overrides.
    pub fn load(path: Option<&str>, overrides: &BTreeMap<String, WallOverride>) -> Result<Self> {
        let mut walls: Self = match path {
            Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
            None => BUILTIN.clone(),
        };
        walls.overrides.extend(overrides.clone());
        walls.validate()?;
        Ok(walls)
    }
    pub fn validate(&self) -> Result<()> {
        let lists = [
            &self.text,
            &self.captcha_selectors,
            &self.wall_host_prefixes,
            &self.wall_paths,
        ];
        ensure!(
            lists
                .iter()
                .all(|l| l.len() <= 256 && l.iter().all(|v| !v.trim().is_empty() && v.len() <= 256))
                && self.overrides.len() <= 4096,
            "interstitial marker lists must be bounded and non-empty strings"
        );
        ensure!(
            self.text
                .iter()
                .chain(&self.wall_host_prefixes)
                .chain(&self.wall_paths)
                .all(|v| *v == v.to_lowercase()),
            "interstitial markers must be lower case"
        );
        ensure!(
            self.wall_paths.iter().all(|p| p.starts_with('/')),
            "wall paths start with /"
        );
        for s in &self.captcha_selectors {
            Selector::parse(s).map_err(|_| anyhow!("invalid captcha selector {s}"))?;
        }
        ensure!(
            self.overrides.keys().all(|h| !h.is_empty()
                && *h == h.to_ascii_lowercase()
                && url::Host::parse(h).is_ok()),
            "invalid wall override host"
        );
        Ok(())
    }
    /// Longest-suffix host match, so an override of example.com covers www.example.com.
    fn decision(&self, host: &str) -> Option<WallOverride> {
        self.overrides
            .iter()
            .filter(|(base, _)| host == base.as_str() || host.ends_with(&format!(".{base}")))
            .max_by_key(|(base, _)| base.len())
            .map(|(_, v)| *v)
    }
    fn marked(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.text
            .iter()
            .any(|marker| text.contains(marker.as_str()))
    }
}
/// The daemon installs its configured walls once at startup; tests use the built-in file.
pub fn install(walls: Walls) {
    *INSTALLED.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(walls);
}
fn installed() -> Arc<Walls> {
    INSTALLED.read().unwrap_or_else(|e| e.into_inner()).clone()
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
        if s.len() >= 3 && s[..3].eq_ignore_ascii_case("url") {
            s[3..]
                .trim_start()
                .strip_prefix('=')
                .unwrap_or(&s[3..])
                .trim()
        } else {
            s
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
pub fn inspect(requested: &Url, f: &Fetched) -> Page {
    inspect_with(requested, f, &installed())
}
pub fn inspect_with(requested: &Url, f: &Fetched, walls: &Walls) -> Page {
    let decision = f
        .url
        .host_str()
        .and_then(|h| walls.decision(h))
        .or_else(|| requested.host_str().and_then(|h| walls.decision(h)));
    if decision == Some(WallOverride::Blocked) {
        return Page::Blocked;
    }
    let walls_apply = decision != Some(WallOverride::Content);
    if let (Some(a), Some(b)) = (requested.host_str(), f.url.host_str()) {
        if walls_apply
            && psl::domain_str(a).is_some()
            && psl::domain_str(a) == psl::domain_str(b)
            && walls
                .wall_host_prefixes
                .iter()
                .any(|p| b.starts_with(p.as_str()))
        {
            return Page::Blocked;
        }
    }
    // A requested page redirected to a sign-in path (LinkedIn /authwall, Facebook /login/).
    if walls_apply
        && f.url.path().trim_end_matches('/') != requested.path().trim_end_matches('/')
        && wall_path(walls, f.url.path())
    {
        return Page::Blocked;
    }
    if !f.content_type.contains("html")
        && !(matches!(f.status, 403 | 429 | 503)
            && matches!(f.content_type.as_str(), "text/plain" | ""))
    {
        return Page::Content;
    }
    let text = String::from_utf8_lossy(&f.body);
    let html = Html::parse_document(&text);
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
                    !n.ancestors().any(|a| {
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
    let challenge = matches!(f.status, 403 | 429 | 503);
    let short = visible.trim().len() < 2000;
    // Titles and headings are only evidence on short or challenge pages: a film called
    // "Just a Moment" is not a wall. Captcha widgets in comment forms need the same context.
    let title_marked = walls.marked(&title);
    if walls_apply
        && (((challenge || short)
            && (title_marked || (heading.len() < 200 && walls.marked(&heading))))
            || (short && walls.marked(&visible))
            || ((challenge || short || title_marked)
                && walls
                    .captcha_selectors
                    .iter()
                    .any(|s| html.select(&select(s)).next().is_some())))
    {
        return Page::Blocked;
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
                    return Page::Redirect(target);
                }
            }
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
        if walls_apply {
            return Page::Blocked;
        }
    }
    Page::Content
}
