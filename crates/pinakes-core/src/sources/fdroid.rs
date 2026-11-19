//! Android apps on F-Droid, by the package name in the link: F-Droid's package API says whether
//! the repository has the package (and its suggested version), and the package's page gives its
//! name, summary, icon and links. No key, no account.
//!
//! Every F-Droid link of one package (with or without a site language) is an alias of
//! `https://f-droid.org/packages/{package}`, as Wikidata's F-Droid links spell it.
use super::{
    kit::{fetch, json, text, web_url, Card, Request},
    Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;

pub const SITE: &str = "F-Droid";

/// An Android package name (`app_ids`).
fn package(value: &str) -> Option<String> {
    crate::app_ids::android_package(value).then(|| value.to_owned())
}

/// An F-Droid site language (`en`, `fr`, `zh_Hant`, `pt_BR`).
fn language(s: &str) -> bool {
    let (lang, region) = s.split_once(['_', '-']).unwrap_or((s, "AA"));
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && (2..=4).contains(&region.len())
        && region.bytes().all(|b| b.is_ascii_alphabetic())
}

/// The package an F-Droid link names: `/packages/{package}/`, with or without a site language.
pub fn from_uri(uri: &str) -> Option<String> {
    let url = web_url(uri)?;
    if !matches!(url.host_str()?, "f-droid.org" | "www.f-droid.org") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["packages", id] => package(id),
        [lang, "packages", id] if language(lang) => package(id),
        _ => None,
    }
}

pub fn page(package: &str) -> String {
    format!("https://f-droid.org/packages/{package}")
}

/// The package API's answer: whether the repository has the package.
pub fn api_request(package: &str) -> Option<Request> {
    Request::get(&format!(
        "https://f-droid.org/api/v1/packages/{}",
        self::package(package)?
    ))
}

/// The package's English page.
pub fn page_request(package: &str) -> Option<Request> {
    Request::get(&format!(
        "https://f-droid.org/en/packages/{}/",
        self::package(package)?
    ))
    .map(|r| r.accept("text/html"))
}

/// The suggested version of a package, `None` when the repository has none by this name (404).
pub fn parse_api(status: u16, raw: &[u8], package: &str) -> Result<Option<Option<String>>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        v["packageName"] == package,
        "F-Droid answered for another package"
    );
    let suggested = v["suggestedVersionCode"].as_i64();
    let version = v["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["versionCode"].as_i64() == suggested)
        .and_then(|p| text(&p["versionName"]));
    Ok(Some(version))
}

/// The card of a package from its page, with the API's version. A page that is not the
/// package's (no package header) is an error.
pub fn parse_page(
    status: u16,
    raw: &[u8],
    package: &str,
    version: Option<String>,
) -> Result<Option<Card>> {
    if status == 404 {
        return Ok(None);
    }
    ensure!(status == 200, "unexpected status {status}");
    let html = crate::resolver::html::document(&String::from_utf8_lossy(raw))?;
    let select = |css: &str| scraper::Selector::parse(css).expect("selector");
    let one = |css: &str| {
        html.select(&select(css))
            .next()
            .map(|e| {
                e.text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
    };
    let link = |css: &str| {
        html.select(&select(css))
            .next()
            .and_then(|e| e.value().attr("href"))
            .filter(|u| web_url(u).is_some())
            .map(str::to_owned)
    };
    let name = one(".package-header .package-name");
    ensure!(name.is_some(), "no package header on the page");
    let mut card = Card::new("MobileApplication", page(package), page(package));
    card.name = name;
    card.description = one(".package-header .package-summary");
    card.image = html
        .select(&select(".package-header img.package-icon"))
        .next()
        .and_then(|e| e.value().attr("src"))
        .filter(|u| web_url(u).is_some())
        .map(str::to_owned);
    card.detail("website", link("#website a[href]"));
    card.detail("license", one("#license a"));
    card.detail("source_code", link("#source_code a[href]"));
    card.detail("version", version);
    card.detail("package", package);
    Ok(Some(card))
}

/// The source: F-Droid's package API and package pages.
pub struct FDroid;

#[async_trait]
impl Source for FDroid {
    fn name(&self) -> &'static str {
        "fdroid"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "fdroid",
                uri: "https://f-droid.org/en/packages/de.danoeh.antennapod/",
                card_type: "MobileApplication",
                group_with: None,
            }],
            answers: &[
                E2eAnswer {
                    host: "f-droid.org",
                    path: "/api/v1/packages/de.danoeh.antennapod",
                    query: &[],
                    header: None,
                    content_type: "application/json",
                    file: "api-antennapod.json",
                },
                E2eAnswer {
                    host: "f-droid.org",
                    path: "/en/packages/de.danoeh.antennapod/",
                    query: &[],
                    header: None,
                    content_type: "text/html",
                    file: "page-antennapod.html",
                },
            ],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        from_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = page(&from_uri(uri)?);
        (root != uri).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "website",
            "license",
            "source_code",
            "version",
            "package",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("f-droid.org", 1.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://f-droid.org/en/packages/de.danoeh.antennapod/",
            card_type: "MobileApplication",
        }]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(package) = from_uri(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &api_request(&package).context("no request")?).await?;
        let Some(version) = parse_api(f.status, &f.body, &package)? else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &page_request(&package).context("no request")?).await?;
        let card = crate::resolver::html::blocking(move || {
            parse_page(f.status, &f.body, &package, version)
        })
        .await??;
        Ok(match card {
            Some(card) => card.into_doc(cx.card(uri, self), SITE),
            None => cx.missing(uri, self),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    const ANTENNAPOD: &str = "de.danoeh.antennapod";

    fn read(name: &str) -> Vec<u8> {
        fixture::read("fdroid", name)
    }

    #[test]
    fn every_link_form_of_one_package() {
        for uri in [
            "https://f-droid.org/packages/de.danoeh.antennapod/",
            "https://f-droid.org/packages/de.danoeh.antennapod",
            "https://f-droid.org/en/packages/de.danoeh.antennapod/",
            "https://f-droid.org/zh_Hant/packages/de.danoeh.antennapod/",
            "https://www.f-droid.org/pt_BR/packages/de.danoeh.antennapod/",
        ] {
            assert_eq!(from_uri(uri).as_deref(), Some(ANTENNAPOD), "{uri}");
        }
        assert_eq!(
            FDroid
                .alias_root("https://f-droid.org/en/packages/de.danoeh.antennapod/")
                .as_deref(),
            Some("https://f-droid.org/packages/de.danoeh.antennapod")
        );
        assert_eq!(
            FDroid.alias_root("https://f-droid.org/packages/de.danoeh.antennapod"),
            None
        );
        for uri in [
            "https://f-droid.org/en/packages/",
            "https://f-droid.org/en/packages/antennapod/",
            "https://f-droid.org/en/packages/de.danoeh.1antennapod/",
            "https://f-droid.org/english/packages/de.danoeh.antennapod/",
            "https://f-droid.org/en/categories/internet/",
            "https://example.org/packages/de.danoeh.antennapod/",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        for example in FDroid.examples() {
            assert!(FDroid.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn antennapod() {
        let version = parse_api(200, &read("api-antennapod.json"), ANTENNAPOD)
            .unwrap()
            .unwrap();
        assert_eq!(version.as_deref(), Some("3.12.3"));
        let card = parse_page(200, &read("page-antennapod.html"), ANTENNAPOD, version)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "MobileApplication");
        assert_eq!(card.id, "https://f-droid.org/packages/de.danoeh.antennapod");
        assert_eq!(card.name.as_deref(), Some("AntennaPod - Podcast Player"));
        assert_eq!(
            card.description.as_deref(),
            Some("Easy-to-use, flexible and open-source podcast manager and player")
        );
        assert!(card
            .image
            .as_deref()
            .is_some_and(|i| i.starts_with("https://f-droid.org/repo/de.danoeh.antennapod/")));
        assert_eq!(card.details["website"], "https://antennapod.org");
        assert_eq!(
            card.details["license"],
            "GNU General Public License v3.0 only"
        );
        assert_eq!(
            card.details["source_code"],
            "https://github.com/AntennaPod/AntennaPod"
        );
        assert_eq!(card.details["version"], "3.12.3");
        assert_eq!(card.details["package"], ANTENNAPOD);
        crate::sources::kit::checked(&FDroid, &card);
    }

    #[test]
    fn missing_mismatched_and_failed() {
        let unlisted = "org.example.unlisted";
        assert_eq!(
            parse_api(404, &read("api-missing.json"), unlisted).unwrap(),
            None
        );
        assert_eq!(
            parse_page(404, &read("page-missing.html"), unlisted, None).unwrap(),
            None
        );
        assert!(parse_api(200, &read("api-antennapod.json"), unlisted).is_err());
        assert!(parse_page(
            200,
            b"<html><body>Not a package</body></html>",
            unlisted,
            None
        )
        .is_err());
        assert!(parse_api(500, b"", ANTENNAPOD).is_err());
    }
}
