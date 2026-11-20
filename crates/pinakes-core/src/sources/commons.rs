//! Wikimedia Commons files, read from the MediaWiki API of commons.wikimedia.org.
//!
//! Every link to a file's description page (desktop or mobile, `/wiki/` or `index.php?title=`,
//! the `Image:` alias, any case of the namespace, spaces or underscores, percent-encoded or not)
//! is an alias of one identifier, `commons:File:{Name_with_underscores}`, with the title
//! normalised the MediaWiki way. A `?curid=` link names the file by page id; its identifier comes
//! from the answer, which names the title. A renamed file answers through its redirect: the card
//! then carries the new title's identifier, and the old title's page is an alias of it. The media
//! file itself (`upload.wikimedia.org`) and other wikis' `File:` pages are not these objects.
//!
//! Wikimedia asks for a descriptive User-Agent and, without an account, at most one request at a
//! time and well under five a second. Every file has its own licence and author, which the card
//! carries as its attribution.
use super::kit::{
    fetch, float, host_and_segments, html_text, json, number, text, web_url, Card, Point, Request,
};
use super::{Example, Source};
use crate::model::ResourceMeta;
use crate::wikimedia::{file_name, file_page};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::Value;

pub const SITE: &str = "Wikimedia Commons";
const API: &str = "https://commons.wikimedia.org/w/api.php";
/// The width of the image the card shows.
pub const THUMB_WIDTH: u32 = 1280;
/// The `extmetadata` fields the card reads.
const FIELDS: &str = "ObjectName|ImageDescription|Artist|Credit|Attribution|LicenseShortName|\
                      LicenseUrl|UsageTerms|AttributionRequired|DateTimeOriginal";

/// A file, by title or by page id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The file name without the namespace, normalised, with underscores.
    Title(String),
    /// A page id (`?curid=`), whose title the answer names.
    PageId(u64),
}

/// The file a title names (`File:…` or `Image:…`, any case).
fn title(raw: &str) -> Option<Target> {
    let (ns, name) = raw.split_once(':')?;
    let ns = ns.trim_matches([' ', '_']);
    (ns.eq_ignore_ascii_case("file") || ns.eq_ignore_ascii_case("image"))
        .then(|| file_name(name).map(Target::Title))
        .flatten()
}

fn page_id(raw: &str) -> Option<Target> {
    number(raw, 12).map(Target::PageId)
}

/// What a Commons link names: a file description page or the API's query for one file.
pub fn from_uri(uri: &str) -> Option<Target> {
    let (host, segments) = host_and_segments(uri)?;
    if !matches!(
        host.as_str(),
        "commons.wikimedia.org" | "commons.m.wikimedia.org"
    ) {
        return None;
    }
    // A parameter given twice names nothing exact.
    let query = |name: &str| {
        let url = web_url(uri)?;
        let mut values = url.query_pairs().filter(|(k, _)| k == name);
        match (values.next(), values.next()) {
            (Some((_, v)), None) => Some(v.into_owned()),
            _ => None,
        }
    };
    // A past revision of the page (`oldid`, `diff`) is a dated version, not the file.
    if query("oldid").is_some() || query("diff").is_some() {
        return None;
    }
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match s.as_slice() {
        ["wiki", t] => title(&percent_decode_str(t).decode_utf8().ok()?),
        ["w", "index.php"] => match query("curid") {
            Some(id) => page_id(&id),
            None => title(&query("title")?),
        },
        ["w", "api.php"] => match query("titles") {
            Some(t) => title(&t),
            None => page_id(&query("pageids")?),
        },
        _ => None,
    }
}

/// What URI normalisation escapes from a name's first `?` on, which it reads as a query.
const QUERY: &AsciiSet = &CONTROLS.add(b'"');

/// The identifier of a file by title; `None` for a page id until the answer names the title.
/// Letters outside ASCII are percent-encoded, and so is a `"` after the name's first `?`, as URI
/// normalisation writes them.
pub fn identifier(target: &Target) -> Option<String> {
    match target {
        Target::Title(name) => {
            let (path, query) = name.split_at(name.find('?').unwrap_or(name.len()));
            Some(format!(
                "commons:File:{}{}",
                utf8_percent_encode(path, CONTROLS),
                utf8_percent_encode(query, QUERY)
            ))
        }
        Target::PageId(_) => None,
    }
}

/// The file of a root (`commons:File:{Name}`), already in normal form with letters outside ASCII
/// percent-encoded (`identifier`).
pub fn from_root(uri: &str) -> Option<Target> {
    let encoded = uri.strip_prefix("commons:File:")?;
    let name = percent_decode_str(encoded).decode_utf8().ok()?;
    let target = file_name(&name).filter(|n| *n == name).map(Target::Title)?;
    (identifier(&target).as_deref() == Some(uri)).then_some(target)
}

/// The file of a root or a Commons link.
pub fn target(uri: &str) -> Option<Target> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// The file's description page, as MediaWiki writes its URL.
pub fn page(target: &Target) -> String {
    match target {
        Target::Title(name) => file_page(name),
        Target::PageId(id) => format!("https://commons.wikimedia.org/w/index.php?curid={id}"),
    }
}

fn api(params: &[(&str, &str)]) -> Option<Request> {
    let mut url = url::Url::parse(API).ok()?;
    url.query_pairs_mut().extend_pairs(params);
    Request::get(url.as_str())
}

/// The file's image information, licence and coordinates, following a renamed file's redirect.
pub fn request(target: &Target) -> Option<Request> {
    let (key, value) = match target {
        Target::Title(name) => ("titles", format!("File:{}", name.replace('_', " "))),
        Target::PageId(id) => ("pageids", id.to_string()),
    };
    let width = THUMB_WIDTH.to_string();
    api(&[
        ("action", "query"),
        ("format", "json"),
        ("formatversion", "2"),
        ("redirects", "1"),
        ("prop", "imageinfo|coordinates"),
        ("iiprop", "url|size|mime|extmetadata|timestamp"),
        ("iiurlwidth", &width),
        ("iiextmetadatafilter", FIELDS),
        ("iiextmetadatalanguage", "en"),
        (key, &value),
    ])
}

/// The URL without its `utm_*` tracking parameters.
fn untracked(raw: &str) -> Option<String> {
    let mut url = web_url(raw)?;
    if url.query_pairs().all(|(k, _)| k.starts_with("utm_")) {
        url.set_query(None);
    }
    Some(url.into())
}

/// The schema.org type of a file by its MIME type.
fn type_name(mime: &str) -> &'static str {
    match mime.split('/').next() {
        Some("image") => "ImageObject",
        Some("video") => "VideoObject",
        Some("audio") => "AudioObject",
        _ if mime == "application/pdf" => "DigitalDocument",
        _ => "MediaObject",
    }
}

/// The title the API answers for: the asked title through MediaWiki's own `normalized` and
/// `redirects` lists.
fn answered_title(query: &Value, asked: String) -> String {
    let follow = |list: &Value, from: String| {
        list.as_array()
            .into_iter()
            .flatten()
            .find(|m| m["from"].as_str() == Some(from.as_str()))
            .and_then(|m| text(&m["to"]))
            .unwrap_or(from)
    };
    let title = follow(&query["normalized"], asked);
    follow(&query["redirects"], title)
}

/// A file, or `None` when Commons has no such file (or the page id is not a file's page).
pub fn parse(status: u16, raw: &[u8], target: &Target) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[])? else {
        return Ok(None);
    };
    if let Some(error) = v.get("error") {
        // The API is busy or read-only for a while: the file is read later.
        if matches!(
            error["code"].as_str(),
            Some("ratelimited" | "maxlag" | "readonly")
        ) {
            return Err(crate::resolver::RetryLater {
                class: "upstream",
                seconds: 60,
            }
            .into());
        }
        bail!("the API answered an error");
    }
    let query = &v["query"];
    let pages = query["pages"].as_array().context("no pages")?;
    let [p] = pages.as_slice() else {
        bail!("expected one page, got {}", pages.len());
    };
    let title = text(&p["title"]);
    match target {
        Target::Title(name) => {
            let asked = format!("File:{}", name.replace('_', " "));
            ensure!(
                title.as_deref() == Some(answered_title(query, asked).as_str()),
                "the API answered for another file"
            );
        }
        Target::PageId(id) => {
            let redirected = query["redirects"].as_array().is_some_and(|r| !r.is_empty());
            ensure!(
                redirected || p["pageid"].as_u64() == Some(*id),
                "the API answered for another page"
            );
        }
    }
    if p["missing"].as_bool() == Some(true) || p["invalid"].as_bool() == Some(true) {
        return Ok(None);
    }
    if p["ns"].as_i64() != Some(6) {
        return Ok(None);
    }
    let Some(info) = p["imageinfo"].as_array().and_then(|i| i.first()) else {
        return Ok(None);
    };
    let name = title
        .as_deref()
        .and_then(|t| t.strip_prefix("File:"))
        .and_then(file_name)
        .context("not a file title")?;
    let t = Target::Title(name.clone());
    let mut card = Card::new("ImageObject", identifier(&t).expect("a title"), page(&t));
    let meta = |field: &str| info["extmetadata"][field]["value"].as_str();
    let plain = |field: &str| meta(field).and_then(html_text);
    let mime = text(&info["mime"]).unwrap_or_default();
    card.type_name = type_name(&mime);
    card.name = plain("ObjectName").or_else(|| {
        let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
        Some(stem.replace('_', " "))
    });
    card.description = plain("ImageDescription");
    let (width, height) = (info["width"].as_u64(), info["height"].as_u64());
    let small_raster = matches!(
        mime.as_str(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) && width.is_some_and(|w| w <= u64::from(THUMB_WIDTH));
    card.image = if small_raster {
        info["url"].as_str()
    } else {
        info["thumburl"].as_str()
    }
    .and_then(untracked);
    card.point = p["coordinates"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["primary"].as_bool() == Some(true) && c["globe"].as_str() == Some("earth"))
        .and_then(|c| Point::new(float(&c["lat"])?, float(&c["lon"])?));
    let date = plain("DateTimeOriginal");
    let author = plain("Artist");
    let license = plain("LicenseShortName");
    let credit = plain("Attribution").or_else(|| author.clone());
    card.attribution = match (credit, license.clone()) {
        (Some(c), Some(l)) => Some(format!("{c} / {l}")),
        (c, l) => c.or(l),
    };
    card.detail("author", author);
    card.detail("credit", plain("Credit"));
    card.detail("license_short_name", license);
    card.detail(
        "license_url",
        meta("LicenseUrl").and_then(web_url).map(String::from),
    );
    card.detail("usage_terms", plain("UsageTerms"));
    card.detail(
        "attribution_required",
        match meta("AttributionRequired") {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        },
    );
    card.detail("date_original", date);
    card.detail("mime", mime);
    card.detail("width", width);
    card.detail("height", height);
    card.detail("size", info["size"].as_u64());
    card.detail("page_id", p["pageid"].as_u64());
    Ok(Some(card))
}

/// The source: Wikimedia Commons files by title (or page id).
pub struct Commons;

#[async_trait]
impl Source for Commons {
    fn name(&self) -> &'static str {
        "commons"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "commons",
                    uri: "https://commons.wikimedia.org/wiki/File:Golden_Gate_Bridge_as_seen_from_Battery_East.jpg",
                    card_type: "ImageObject",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "commons.wikimedia.org",
                    path: "/w/api.php",
                    query: &[("titles", "File:Golden Gate Bridge as seen from Battery East.jpg")],
                    header: None,
                    content_type: "application/json",
                    file: "golden-gate.json",
                },
            ],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    /// A `?curid=` link has no root until the answer names the title.
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?)?;
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("commons:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "author",
            "credit",
            "license_short_name",
            "license_url",
            "usage_terms",
            "attribution_required",
            "date_original",
            "mime",
            "width",
            "height",
            "size",
            "page_id",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://commons.wikimedia.org/wiki/File:Golden_Gate_Bridge_as_seen_from_Battery_East.jpg",
                card_type: "ImageObject",
            },
            Example {
                uri: "https://commons.wikimedia.org/wiki/File:Bitcoin.svg",
                card_type: "ImageObject",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(target) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&target).context("file title")?).await?;
        let read = target.clone();
        Ok(
            match crate::resolver::html::blocking(move || parse(f.status, &f.body, &read)).await?? {
                Some(card) => card.into_doc(cx.card(uri, self), SITE),
                None => cx.missing(uri, self),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    const GATE: &str = "Golden_Gate_Bridge_as_seen_from_Battery_East.jpg";

    fn read(name: &str) -> Vec<u8> {
        fixture::read("commons", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).as_ref().and_then(identifier)
    }
    fn title(name: &str) -> Target {
        Target::Title(name.into())
    }
    /// The URL a fixture was recorded from.
    fn recorded(file: &str) -> String {
        let p: Value = serde_json::from_slice(&read("provenance.json")).unwrap();
        p.as_object()
            .unwrap()
            .values()
            .find(|e| e["file"] == file)
            .and_then(|e| e["url"].as_str())
            .unwrap()
            .into()
    }

    #[test]
    fn every_link_form_of_one_file() {
        let gate = format!("commons:File:{GATE}");
        for uri in [
            format!("https://commons.wikimedia.org/wiki/File:{GATE}"),
            format!("https://commons.m.wikimedia.org/wiki/File:{GATE}"),
            format!("http://commons.wikimedia.org/wiki/Image:{GATE}#Licensing"),
            "https://commons.wikimedia.org/wiki/file:Golden%20Gate%20Bridge%20as%20seen%20from%20Battery%20East.jpg".into(),
            "https://commons.wikimedia.org/wiki/File:golden__Gate_Bridge_as_seen_from_Battery_East.jpg?uselang=de".into(),
            "https://commons.wikimedia.org/w/index.php?title=File:Golden+Gate+Bridge+as+seen+from+Battery+East.jpg&action=history".into(),
            "https://commons.wikimedia.org/w/index.php?title=File%3AGolden_Gate_Bridge_as_seen_from_Battery_East.jpg".into(),
            "https://commons.wikimedia.org/w/api.php?action=query&titles=File:Golden%20Gate%20Bridge%20as%20seen%20from%20Battery%20East.jpg".into(),
        ] {
            assert_eq!(id(&uri).as_deref(), Some(gate.as_str()), "{uri}");
        }
        assert_eq!(
            id("https://commons.wikimedia.org/wiki/File:%C3%A9t%C3%A9_%C3%A0_Paris.jpg").as_deref(),
            Some("commons:File:%C3%89t%C3%A9_%C3%A0_Paris.jpg")
        );
        for uri in [
            "https://commons.wikimedia.org/w/index.php?curid=30202095",
            "https://commons.wikimedia.org/w/index.php?title=File:X.jpg&curid=30202095",
            "https://commons.wikimedia.org/w/api.php?action=query&pageids=30202095",
        ] {
            assert_eq!(from_uri(uri), Some(Target::PageId(30202095)), "{uri}");
            assert_eq!(id(uri), None);
        }
    }

    #[test]
    fn first_letters_as_mediawiki_writes_them() {
        assert_eq!(file_name("été.jpg").as_deref(), Some("Été.jpg"));
        // Georgian letters and a letter whose upper case is two letters stay as they are.
        assert_eq!(file_name("ქალაქი.jpg").as_deref(), Some("ქალაქი.jpg"));
        assert_eq!(file_name("ßtraße.jpg").as_deref(), Some("ßtraße.jpg"));
    }

    #[test]
    fn invalid_links() {
        for uri in [
            // A parameter given twice names nothing exact.
            "https://commons.wikimedia.org/w/index.php?title=File:A.jpg&title=File:B.jpg",
            "https://commons.wikimedia.org/w/index.php?curid=1&curid=2",
            "https://upload.wikimedia.org/wikipedia/commons/4/46/Bitcoin.svg",
            "https://en.wikipedia.org/wiki/File:Bitcoin.svg",
            "https://commons.wikimedia.org/wiki/Category:Bitcoin",
            "https://commons.wikimedia.org/wiki/Bitcoin.svg",
            "https://commons.wikimedia.org/wiki/File:",
            "https://commons.wikimedia.org/wiki/File:a%7Cb.jpg",
            "https://commons.wikimedia.org/wiki/File:a/b.jpg",
            "https://commons.wikimedia.org/wiki/Special:FilePath/Bitcoin.svg",
            "https://commons.wikimedia.org/w/index.php?title=File:Bitcoin.svg&oldid=123456789",
            "https://commons.wikimedia.org/wiki/File:Bitcoin.svg?oldid=123456789",
            "https://commons.wikimedia.org/w/index.php?title=File:Bitcoin.svg&diff=prev&oldid=1",
            "https://commons.wikimedia.org/w/index.php?curid=0",
            "https://commons.wikimedia.org/w/index.php?curid=abc",
            // A page id is digits only.
            "https://commons.wikimedia.org/w/index.php?curid=%2B30202095",
            "https://commons.wikimedia.org/w/api.php?action=query&pageids=%2B30202095",
            "https://commons.wikimedia.org/w/api.php?titles=File:A.jpg%7CFile:B.jpg",
            "https://commons.wikimedia.org/wiki/File:a%2541.jpg",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn requests() {
        let gate = title(GATE);
        assert_eq!(
            request(&gate).unwrap().url.as_str(),
            recorded("golden-gate.json")
        );
        assert_eq!(
            request(&Target::PageId(30202095)).unwrap().url.as_str(),
            recorded("curid-30202095.json")
        );
        assert_eq!(
            page(&title("Été_(1).jpg")),
            "https://commons.wikimedia.org/wiki/File:%C3%89t%C3%A9_(1).jpg"
        );
    }

    #[test]
    fn photo_with_location() {
        let gate = title(GATE);
        let card = parse(200, &read("golden-gate.json"), &gate)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "ImageObject");
        assert_eq!(card.id, format!("commons:File:{GATE}"));
        assert_eq!(
            card.url,
            format!("https://commons.wikimedia.org/wiki/File:{GATE}")
        );
        assert_eq!(
            card.name.as_deref(),
            Some("Golden Gate Bridge as seen from Battery East")
        );
        assert_eq!(
            card.description.as_deref(),
            Some("The Golden Gate Bridge in San Francisco, California, as seen from Battery East")
        );
        assert_eq!(
            card.image.as_deref(),
            Some(format!(
                "https://thumb.wikimedia.org/wikipedia/commons/thumb/b/bf/{GATE}/1280px-{GATE}"
            ))
            .as_deref()
        );
        assert_eq!(card.point, Point::new(37.808838, -122.47525));
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(
            card.attribution.as_deref(),
            Some("© Frank Schulenburg / CC BY-SA 4.0")
        );
        assert_eq!(card.details["author"], "Frank Schulenburg");
        assert_eq!(card.details["credit"], "Own work");
        assert_eq!(card.details["date_original"], "2017-06-24 08:45:10");
        assert_eq!(
            card.details["license_url"],
            "https://creativecommons.org/licenses/by-sa/4.0"
        );
        assert_eq!(card.details["attribution_required"], true);
        assert_eq!(card.details["mime"], "image/jpeg");
        assert_eq!(
            (
                &card.details["width"],
                &card.details["height"],
                &card.details["size"]
            ),
            (&5388.into(), &3368.into(), &13848964.into())
        );
        assert_eq!(card.details["page_id"], 60353189);
        assert!(card.keys.is_empty());
    }

    #[test]
    fn logo_by_title_and_by_page_id() {
        let card = parse(200, &read("bitcoin-svg.json"), &title("Bitcoin.svg"))
            .unwrap()
            .unwrap();
        assert_eq!(card.id, "commons:File:Bitcoin.svg");
        assert_eq!(card.name.as_deref(), Some("Bitcoin"));
        assert_eq!(card.description.as_deref(), Some("Logo of Bitcoin"));
        assert_eq!(
            card.image.as_deref(),
            Some("https://thumb.wikimedia.org/wikipedia/commons/thumb/4/46/Bitcoin.svg/1280px-Bitcoin.svg.png")
        );
        assert_eq!(
            card.attribution.as_deref(),
            Some("Grayliptrot / Public domain")
        );
        assert_eq!(card.details["attribution_required"], false);
        assert_eq!(
            card.details["credit"],
            "This file was derived from: Bitcoin logo.svg"
        );
        assert_eq!((card.point, card.start), (None, None));

        let by_id = parse(200, &read("curid-30202095.json"), &Target::PageId(30202095))
            .unwrap()
            .unwrap();
        assert_eq!(by_id, card);
    }

    #[test]
    fn renamed_file() {
        let old = title("Bitcoin_BTC_symbol.svg");
        let card = parse(200, &read("redirect.json"), &old).unwrap().unwrap();
        assert_eq!(card.id, "commons:File:BitcoinSign.svg");
        assert_eq!(
            card.url,
            "https://commons.wikimedia.org/wiki/File:BitcoinSign.svg"
        );
        // The old title's link becomes an alias of this root once the card is stored
        // (`source_root`).
        assert_eq!(card.id, "commons:File:BitcoinSign.svg");
    }

    #[test]
    fn missing_and_failures() {
        let missing = title("Pinakes_does_not_exist_0000.jpg");
        assert_eq!(parse(200, &read("missing.json"), &missing).unwrap(), None);
        assert_eq!(
            parse(200, &read("curid-missing.json"), &Target::PageId(999999999)).unwrap(),
            None
        );
        // An answer for another file or page.
        assert!(parse(200, &read("bitcoin-svg.json"), &title(GATE)).is_err());
        assert!(parse(200, &read("bitcoin-svg.json"), &Target::PageId(1)).is_err());
        assert!(parse(200, &read("missing.json"), &title("Bitcoin.svg")).is_err());
        assert!(parse(429, b"", &title(GATE)).is_err());
        assert!(parse(503, b"", &title(GATE)).is_err());
        assert!(parse(200, b"<html>", &title(GATE)).is_err());
        let gallery = br#"{"query":{"pages":[{"pageid":5,"ns":0,"title":"Bitcoin"}]}}"#;
        assert_eq!(parse(200, gallery, &Target::PageId(5)).unwrap(), None);
    }

    #[test]
    fn api_errors_sent_with_status_200() {
        let answer = |code: &str| {
            let body = format!(r#"{{"error":{{"code":"{code}","info":"Details of {code}"}}}}"#);
            parse(200, body.as_bytes(), &title(GATE)).unwrap_err()
        };
        // A busy or read-only API: the file is read later.
        for code in ["ratelimited", "maxlag", "readonly"] {
            let error = answer(code);
            let retry = error
                .downcast_ref::<crate::resolver::RetryLater>()
                .unwrap_or_else(|| panic!("{code} is a failure: {error}"));
            assert_eq!(retry.class, "upstream", "{code}");
            assert!(retry.seconds >= 60, "{code}");
            assert!(!format!("{error:#}").contains(code), "{error:#}");
        }
        // Any other error is a failure, whose message leaves the answer's text out.
        let error = answer("badvalue");
        assert!(error
            .downcast_ref::<crate::resolver::RetryLater>()
            .is_none());
        assert!(!format!("{error:#}").contains("badvalue"), "{error:#}");
    }

    #[test]
    fn types_by_mime() {
        assert_eq!(type_name("image/svg+xml"), "ImageObject");
        assert_eq!(type_name("video/webm"), "VideoObject");
        assert_eq!(type_name("audio/ogg"), "AudioObject");
        assert_eq!(type_name("application/pdf"), "DigitalDocument");
        assert_eq!(type_name("application/ogg"), "MediaObject");
    }

    #[test]
    fn roots_and_aliases() {
        let c = Commons;
        for root in [
            format!("commons:File:{GATE}"),
            "commons:File:Bitcoin.svg".into(),
            "commons:File:BitcoinSign.svg".into(),
            "commons:File:%C3%89t%C3%A9_%C3%A0_Paris.jpg".into(),
            "commons:File:100%_pure.jpg".into(),
        ] {
            assert!(c.reads(&root), "{root}");
            assert_eq!(c.alias_root(&root), None, "{root}");
            assert!(!c.invalid(&root), "{root}");
            assert_eq!(crate::model::identity(&root).unwrap().1, root);
        }
        assert_eq!(
            c.alias_root(&format!(
                "https://commons.m.wikimedia.org/wiki/Image:{GATE}"
            )),
            Some(format!("commons:File:{GATE}"))
        );
        // A renamed file's old title is its own static root until the fetch names the new one.
        assert_eq!(
            c.alias_root("https://commons.wikimedia.org/wiki/File:Bitcoin_BTC_symbol.svg")
                .as_deref(),
            Some("commons:File:Bitcoin_BTC_symbol.svg")
        );
        // A page id names a file only after the fetch.
        let curid = "https://commons.wikimedia.org/w/index.php?curid=30202095";
        assert!(c.reads(curid));
        assert_eq!(c.alias_root(curid), None);
        for uri in [
            "commons:File:",
            "commons:File:bitcoin.svg",
            "commons:File:Bitcoin BTC symbol.svg",
            "commons:File:A__b.jpg",
            "commons:Image:Bitcoin.svg",
            "commons:Bitcoin.svg",
            "commons:File:Été_à_Paris.jpg",
            "commons:File:%C3%A9t%C3%A9.jpg",
            "commons:File:%42itcoin.svg",
        ] {
            assert!(c.invalid(uri), "{uri}");
            assert!(!c.reads(uri), "{uri}");
        }
        assert!(!c.invalid("https://upload.wikimedia.org/wikipedia/commons/4/46/Bitcoin.svg"));
    }

    #[test]
    fn a_quote_in_a_name_keeps_its_root() {
        let c = Commons;
        for (link, root, name) in [
            // URI normalisation leaves a `"` as it is before a name's first `?`,
            (
                "https://commons.wikimedia.org/wiki/File:%22Me%22.jpg",
                "commons:File:\"Me\".jpg",
                "\"Me\".jpg",
            ),
            // and percent-encodes it after one.
            (
                "https://commons.wikimedia.org/wiki/File:Who%3F_%22Me%22.jpg",
                "commons:File:Who?_%22Me%22.jpg",
                "Who?_\"Me\".jpg",
            ),
        ] {
            assert_eq!(id(link).as_deref(), Some(root), "{link}");
            assert_eq!(crate::model::identity(root).unwrap().1, root);
            assert_eq!(from_root(root), Some(title(name)), "{root}");
            assert!(c.reads(root) && !c.invalid(root), "{root}");
            assert_eq!(c.alias_root(root), None, "{root}");
        }
    }

    #[test]
    fn documents() {
        let c = Commons;
        for (file, target) in [
            ("golden-gate.json", title(GATE)),
            ("bitcoin-svg.json", title("Bitcoin.svg")),
            ("curid-30202095.json", Target::PageId(30202095)),
            ("redirect.json", title("Bitcoin_BTC_symbol.svg")),
        ] {
            let card = parse(200, &read(file), &target).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&c, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let card = parse(
            200,
            &read("redirect.json"),
            &title("Bitcoin_BTC_symbol.svg"),
        )
        .unwrap()
        .unwrap();
        let doc = card.into_doc(ResourceMeta::new("x", "commons", 60), SITE);
        assert_eq!(doc.site, "Wikimedia Commons");
        assert_eq!(
            doc.source_root.as_deref(),
            Some("commons:File:BitcoinSign.svg")
        );
    }
}
