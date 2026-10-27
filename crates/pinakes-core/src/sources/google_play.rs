//! Android apps on Google Play, from the app's own page: Google Play has no public API, so this
//! source is a page hook on the web route. The page's `SoftwareApplication` JSON-LD names the
//! app, its icon, its developer (with the developer's website) and its category; without it, the
//! page's Open Graph title (less " - Apps on Google Play") and image.
//!
//! Every Google Play link of one app is an alias of
//! `https://play.google.com/store/apps/details?id={package}`: its language and country
//! parameters (`hl=`, `gl=`) and anything else are the page's, not another app.
use super::{text, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::Fetched};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

pub const SITE: &str = "Google Play";

/// An Android package name (`app_ids`).
fn package(value: &str) -> Option<String> {
    crate::app_ids::android_package(value).then(|| value.to_owned())
}

/// The app a Google Play link names: `/store/apps/details?id={package}`.
pub fn from_uri(uri: &str) -> Option<String> {
    let url = web_url(uri)?;
    if url.host_str()? != "play.google.com" {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segments != ["store", "apps", "details"] {
        return None;
    }
    let id = url.query_pairs().find(|(k, _)| k == "id")?.1;
    package(&id)
}

pub fn page(package: &str) -> String {
    format!("https://play.google.com/store/apps/details?id={package}")
}

/// What the page says about its app.
#[derive(Debug, Default, PartialEq)]
pub struct Listing {
    pub name: Option<String>,
    pub image: Option<String>,
    pub developer: Option<String>,
    pub website: Option<String>,
    pub category: Option<String>,
}

/// The listing of a Google Play page: its `SoftwareApplication` JSON-LD, else its Open Graph
/// title and image. `None` for a page that names no app.
pub fn listing(body: &[u8]) -> Result<Option<Listing>> {
    let html = crate::resolver::html::document(&String::from_utf8_lossy(body))?;
    let select = |css: &str| scraper::Selector::parse(css).expect("selector");
    let app = html
        .select(&select("script[type='application/ld+json']"))
        .filter_map(|s| serde_json::from_str::<Value>(&s.text().collect::<String>()).ok())
        .find(|v| v["@type"] == "SoftwareApplication");
    let meta = |property: &str| {
        html.select(&select(&format!("meta[property='{property}']")))
            .next()
            .and_then(|m| m.value().attr("content"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    // "Signal Private Messenger - Apps on Google Play", in the page's language.
    let og_name = meta("og:title").and_then(|t| {
        let (name, suffix) = t.rsplit_once(" - ").or_else(|| t.rsplit_once(" – "))?;
        suffix
            .contains("Google Play")
            .then(|| name.trim().to_owned())
    });
    let listing = match &app {
        Some(app) => Listing {
            name: text(&app["name"]).or(og_name),
            image: text(&app["image"]).or_else(|| meta("og:image")),
            developer: text(&app["author"]["name"]),
            website: text(&app["author"]["url"]),
            category: text(&app["applicationCategory"]).map(|c| category(&c)),
        },
        None => Listing {
            name: og_name,
            image: meta("og:image"),
            ..Default::default()
        },
    };
    Ok(listing.name.is_some().then_some(Listing {
        image: listing.image.filter(|u| web_url(u).is_some()),
        website: listing.website.filter(|u| web_url(u).is_some()),
        ..listing
    }))
}

/// Google Play's category constant as words: `MUSIC_AND_AUDIO` → "Music and audio".
fn category(constant: &str) -> String {
    let words = constant.to_ascii_lowercase().replace('_', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

/// The page's card improved from its listing.
pub fn card(doc: &ResourceMeta, package: &str, listing: Listing) -> ResourceMeta {
    let mut doc = doc.clone();
    doc.set_type("MobileApplication");
    doc.source = "google_play".into();
    doc.site = SITE.into();
    doc.name = listing.name;
    if let Some(image) = listing.image {
        doc.image = Some(image);
    }
    for (key, value) in [
        ("developer", listing.developer),
        ("category", listing.category),
        ("website", listing.website),
        ("package", Some(package.to_owned())),
        ("page_url", Some(page(package))),
    ] {
        match value {
            Some(value) => {
                doc.attributes.insert(key.into(), value);
            }
            None => {
                doc.attributes.remove(key);
            }
        }
    }
    doc
}

/// The source: a page hook for Google Play's app pages.
pub struct GooglePlay;

#[async_trait]
impl Source for GooglePlay {
    fn name(&self) -> &'static str {
        "google_play"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "google_play",
                uri: "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms",
                card_type: "MobileApplication",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "play.google.com",
                path: "/store/apps/details",
                query: &[("id", "org.thoughtcrime.securesms")],
                header: None,
                content_type: "text/html",
                file: "page-signal.html.gz",
            }],
        })
    }
    fn reads(&self, _uri: &str) -> bool {
        false
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = page(&from_uri(uri)?);
        (root != uri).then_some(root)
    }
    fn hooks_pages(&self) -> bool {
        true
    }
    fn facts(&self) -> &'static [&'static str] {
        &["page_url", "developer", "category", "website", "package"]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms",
            card_type: "MobileApplication",
        }]
    }
    async fn resolve(&self, _cx: &Context<'_>, _uri: &str) -> Result<ResourceMeta> {
        anyhow::bail!("Google Play pages are read by the web route")
    }
    async fn page(
        &self,
        _cx: &Context<'_>,
        uri: &str,
        page: &Fetched,
        doc: &ResourceMeta,
    ) -> Result<Option<ResourceMeta>> {
        let Some(package) = from_uri(uri).or_else(|| from_uri(page.url.as_str())) else {
            return Ok(None);
        };
        let body = page.body.clone();
        let Some(listing) = crate::resolver::html::blocking(move || listing(&body)).await?? else {
            return Ok(None);
        };
        Ok(Some(card(doc, &package, listing)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    const SIGNAL: &str = "org.thoughtcrime.securesms";

    fn read(file: &str) -> Vec<u8> {
        fixture::read("google_play", file)
    }

    #[test]
    fn every_link_form_of_one_app() {
        for uri in [
            "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms",
            "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms&hl=de",
            "https://play.google.com/store/apps/details?hl=en_US&gl=US&id=org.thoughtcrime.securesms",
            "https://play.google.com/store/apps/details/?id=org.thoughtcrime.securesms",
        ] {
            assert_eq!(from_uri(uri).as_deref(), Some(SIGNAL), "{uri}");
        }
        assert_eq!(
            GooglePlay
                .alias_root(
                    "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms&hl=de"
                )
                .as_deref(),
            Some("https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms")
        );
        assert_eq!(
            GooglePlay.alias_root(
                "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms"
            ),
            None
        );
        for uri in [
            "https://play.google.com/store/apps/details?hl=de",
            "https://play.google.com/store/apps/details?id=securesms",
            "https://play.google.com/store/apps/developer?id=Signal+Foundation",
            "https://play.google.com/store/apps/details/Signal?id=org.thoughtcrime.securesms",
            "https://play.google.com/store/books/details?id=org.thoughtcrime.securesms",
            "https://example.org/store/apps/details?id=org.thoughtcrime.securesms",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        assert!(!GooglePlay.reads(GooglePlay.examples()[0].uri));
        assert!(from_uri(GooglePlay.examples()[0].uri).is_some());
    }

    #[test]
    fn signal() {
        let listing = listing(&read("page-signal.html.gz")).unwrap().unwrap();
        assert_eq!(
            listing,
            Listing {
                name: Some("Signal Private Messenger".into()),
                image: Some("https://play-lh.googleusercontent.com/HXEs85Okatm4_lZMmCaLFk3iXQ6_lrr00i8nsxaqBRSy6VGnsP7F50dKUfrO37Sj0nxtzSjMpKbjgWh6d0PTyEA".into()),
                developer: Some("Signal Foundation".into()),
                website: Some("https://signal.org".into()),
                category: Some("Communication".into()),
            }
        );
        let uri = "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms";
        let mut web = ResourceMeta::new(uri, "web", 3600);
        web.set_type("SoftwareApplication");
        web.name = Some("Signal Private Messenger - Apps on Google Play".into());
        let doc = card(&web, SIGNAL, listing);
        assert_eq!(doc.type_name, "MobileApplication");
        assert_eq!(doc.source, "google_play");
        assert_eq!(doc.site, "Google Play");
        assert_eq!(doc.name.as_deref(), Some("Signal Private Messenger"));
        assert_eq!(doc.attributes["developer"], "Signal Foundation");
        assert_eq!(doc.attributes["website"], "https://signal.org");
        assert_eq!(doc.attributes["package"], SIGNAL);
        for key in doc.attributes.keys() {
            assert!(GooglePlay.facts().contains(&key.as_str()), "{key}");
        }
    }

    #[test]
    fn open_graph_alone_and_no_app() {
        let head = |title: &str| {
            format!(r#"<html><head><meta property="og:title" content="{title}"><meta property="og:image" content="https://play-lh.googleusercontent.com/icon"></head></html>"#)
                .into_bytes()
        };
        let listing = listing(&head("Signal Private Messenger – Apps bei Google Play"))
            .unwrap()
            .unwrap();
        assert_eq!(listing.name.as_deref(), Some("Signal Private Messenger"));
        assert_eq!(
            listing.image.as_deref(),
            Some("https://play-lh.googleusercontent.com/icon")
        );
        assert_eq!(listing.developer, None);
        assert_eq!(super::listing(&head("Google Play")).unwrap(), None);
        assert_eq!(super::listing(&read("page-missing.html")).unwrap(), None);
    }

    #[test]
    fn categories_read_as_words() {
        assert_eq!(category("COMMUNICATION"), "Communication");
        assert_eq!(category("MUSIC_AND_AUDIO"), "Music and audio");
    }
}
