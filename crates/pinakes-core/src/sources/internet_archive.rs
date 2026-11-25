//! Internet Archive items, read from the item metadata API of archive.org.
//!
//! Every link to an item (its details page, the reader's views of it such as `/page/n5/mode/2up`,
//! its embed player, and the metadata API) is an alias of one identifier, `ia:{identifier}`.
//! Identifiers are case-sensitive: `Bitcoin-Whitepaper` is not `bitcoin-whitepaper`. A file of an
//! item (`/download/…`, or its view at `/details/{identifier}/{file}`) and the Wayback Machine
//! (`web.archive.org`) are not the item.
//!
//! The request reads only the item's `metadata` record, which stays a few kilobytes even for an
//! item with thousands of files (the full record lists every file). An unknown identifier and a taken-down ("darked")
//! item both answer 200 with an error that names the identifier.
use super::kit::{
    fetch, host_and_segments, html_text, image_licence, image_url, json, plain, text, web_url,
    Card, ImageLicence, Key, Request,
};
use super::{Example, Source};
use crate::{model::ResourceMeta, resolver::cards};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

pub const SITE: &str = "Internet Archive";

/// An item identifier as archive.org writes it: letters, digits, `.`, `-` and `_`.
pub fn item_id(value: &str) -> Option<String> {
    (plain(value, 100, |c| {
        c.is_ascii_alphanumeric() || "._-".contains(c)
    }) && !value.starts_with('.'))
    .then(|| value.to_owned())
}

/// The item an archive.org link names.
pub fn from_uri(uri: &str) -> Option<String> {
    let (host, segments) = host_and_segments(uri)?;
    if host != "archive.org" {
        return None;
    }
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match s.as_slice() {
        ["details", id] | ["embed", id] | ["metadata", id] | ["metadata", id, "metadata"] => {
            item_id(id)
        }
        // The reader's views of the item; any other path after it is one of its files.
        ["details", id, "page" | "mode", ..] => item_id(id),
        _ => None,
    }
}

pub fn identifier(id: &str) -> String {
    format!("ia:{id}")
}

/// The item of a root (`ia:{identifier}`).
pub fn from_root(uri: &str) -> Option<String> {
    item_id(uri.strip_prefix("ia:")?)
}

/// The item of a root or an archive.org link.
pub fn target(uri: &str) -> Option<String> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// The item's details page.
pub fn page(id: &str) -> String {
    format!("https://archive.org/details/{id}")
}

/// The item's metadata record.
pub fn request(id: &str) -> Option<Request> {
    Request::get(&format!(
        "https://archive.org/metadata/{}/metadata",
        item_id(id)?
    ))
}

/// Every trimmed, non-empty string of a value that is a string or an array of strings.
fn values(v: &Value) -> Vec<String> {
    match v {
        Value::Array(_) => super::kit::texts(v),
        _ => text(v).into_iter().collect(),
    }
}

/// The value of a field that must have exactly one.
fn single(v: &Value) -> Option<String> {
    match values(v).as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// The schema.org type of an item by its `mediatype`.
fn type_name(mediatype: &str) -> &'static str {
    match mediatype {
        "texts" => "Book",
        "movies" => "VideoObject",
        "audio" | "etree" => "AudioObject",
        "image" => "ImageObject",
        "software" => "SoftwareApplication",
        "data" => "Dataset",
        "web" => "WebSite",
        "collection" => "Collection",
        _ => "CreativeWork",
    }
}

/// The short name of a Creative Commons licence or public-domain tool by its URL, e.g.
/// `CC BY-SA 4.0`, `CC0 1.0` or `Public Domain Mark 1.0`.
fn cc_name(url: &str) -> Option<String> {
    let (host, s) = host_and_segments(url)?;
    if host != "creativecommons.org" {
        return None;
    }
    let s: Vec<&str> = s.iter().map(String::as_str).collect();
    let version = |v: &str| plain(v, 5, |c| c.is_ascii_digit() || c == '.').then_some(());
    match s.as_slice() {
        ["licenses", code, v, ..] if plain(code, 12, |c| "bysancd-".contains(c)) => {
            version(v)?;
            Some(format!("CC {} {v}", code.to_ascii_uppercase()))
        }
        ["publicdomain", "zero", v, ..] => version(v).map(|_| format!("CC0 {v}")),
        ["publicdomain", "mark", v, ..] => version(v).map(|_| format!("Public Domain Mark {v}")),
        _ => None,
    }
}

/// Whether a licence (as `cc_name` names it) lets anyone show the item's image with a credit:
/// CC0, the Public Domain Mark, or CC BY without further terms.
fn open_image(name: &str) -> bool {
    matches!(
        image_licence(name),
        Some(ImageLicence::Cc0 | ImageLicence::PublicDomainMark | ImageLicence::By)
    )
}

/// An Open Library id of the given kind (`M` edition, `W` work).
fn openlibrary(value: Option<String>, kind: char) -> Option<String> {
    let value = value?;
    let digits = value.strip_prefix("OL")?.strip_suffix(kind)?;
    plain(digits, 12, |c| c.is_ascii_digit()).then_some(value)
}

/// Whether the answer says the item does not exist: an empty full record, a darked full record,
/// or the error the metadata sub-path gives for an unknown or darked item.
fn missing(v: &Value, id: &str) -> Result<bool> {
    if v.as_object().is_some_and(|o| o.is_empty()) || v["is_dark"].as_bool() == Some(true) {
        return Ok(true);
    }
    match v.get("error") {
        None => Ok(false),
        Some(e) => {
            let e = e.as_str().unwrap_or_default();
            if e == format!("Couldn't locate item '{id}'")
                || e == format!("Couldn't get 'metadata' for item {id}")
            {
                Ok(true)
            } else {
                // Never the service's text: an error message may hold the link it names.
                bail!("the API answered an error")
            }
        }
    }
}

/// An item, or `None` when archive.org has no such item or took it down. Reads the metadata
/// sub-path's answer (`{"result": …}`) and the full record (`{"metadata": …}`) alike.
pub fn parse(status: u16, raw: &[u8], id: &str) -> Result<Option<Card>> {
    let v = json(status, raw, &[])?.context("no body")?;
    if missing(&v, id)? {
        return Ok(None);
    }
    let m = v
        .get("result")
        .or_else(|| v.get("metadata"))
        .context("no metadata")?;
    ensure!(
        m["identifier"].as_str() == Some(id),
        "the API answered for another item"
    );
    let mediatype = text(&m["mediatype"]);
    let mut card = Card::new(
        type_name(mediatype.as_deref().unwrap_or_default()),
        identifier(id),
        page(id),
    );
    card.name = values(&m["title"]).into_iter().next();
    let description: Vec<String> = values(&m["description"])
        .iter()
        .filter_map(|d| html_text(d))
        .collect();
    card.description = (!description.is_empty()).then(|| description.join("\n"));
    let date = text(&m["date"]);
    let license = text(&m["licenseurl"])
        .and_then(|u| web_url(&u))
        .map(String::from);
    // The item's own thumbnail, only when its licence lets anyone show it with a credit.
    if license
        .as_deref()
        .and_then(cc_name)
        .is_some_and(|n| open_image(&n))
    {
        card.image = image_url(&format!("https://archive.org/services/img/{id}"));
    }
    let rights = values(&m["rights"]).iter().find_map(|r| html_text(r));
    card.attribution = license
        .as_deref()
        .and_then(cc_name)
        .or_else(|| license.clone())
        .or_else(|| rights.clone());
    card.detail("creators", values(&m["creator"]));
    card.detail("date", date);
    card.detail("publishers", values(&m["publisher"]));
    card.detail("languages", values(&m["language"]));
    card.detail("collections", values(&m["collection"]));
    card.detail("subjects", values(&m["subject"]));
    card.detail("mediatype", mediatype);
    card.detail("license_url", license);
    card.detail("rights", rights);
    card.detail("public_date", text(&m["publicdate"]));
    card.detail("ark", single(&m["identifier-ark"]));

    let mut isbns: Vec<String> = values(&m["isbn"])
        .iter()
        .filter_map(|i| cards::isbn(&format!("isbn:{i}")))
        .map(|i| cards::isbn13(&i))
        .collect();
    isbns.sort();
    isbns.dedup();
    if let [isbn] = isbns.as_slice() {
        card.key(Key::new("isbn", isbn));
    }
    // The older `openlibrary` field sometimes names another edition: then neither is certain.
    let edition = openlibrary(single(&m["openlibrary_edition"]), 'M')
        .filter(|e| values(&m["openlibrary"]).iter().all(|o| o == e));
    if let Some(edition) = edition {
        card.key(Key::new("openlibrary", &edition));
    }
    // The work is every edition, not this item: a fact, not a key.
    card.detail(
        "openlibrary_work",
        openlibrary(single(&m["openlibrary_work"]), 'W'),
    );
    let doi = single(&m["doi"]).and_then(|d| {
        let d = d.trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{feff}')
        });
        cards::doi(d).or_else(|| cards::doi(&format!("doi:{d}")))
    });
    if let Some(doi) = doi {
        card.key(Key::new("doi", &doi));
    }
    Ok(Some(card))
}

/// The source: Internet Archive items by identifier.
pub struct InternetArchive;

#[async_trait]
impl Source for InternetArchive {
    fn name(&self) -> &'static str {
        "internet_archive"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "internet_archive",
                uri: "https://archive.org/details/bitcoin-whitepaper",
                card_type: "Book",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "archive.org",
                path: "/metadata/bitcoin-whitepaper/metadata",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "bitcoin-whitepaper.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?);
        (root != uri).then_some(root)
    }
    fn root_page(&self, root: &str) -> Option<String> {
        from_root(root).map(|id| page(&id))
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("ia:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "openlibrary_work",
            "creators",
            "date",
            "publishers",
            "languages",
            "collections",
            "subjects",
            "mediatype",
            "license_url",
            "rights",
            "public_date",
            "ark",
            // Keys
            "isbn",
            "openlibrary",
            "doi",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("archive.org", 0.5)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://archive.org/details/bitcoin-whitepaper",
            card_type: "Book",
        }]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(id) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = fetch(cx, &request(&id).context("item identifier")?).await?;
        let read = id.clone();
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

    const DARK: &str =
        "afewsuggestionsastothebestwayofmakingandutilizingindianmanuscriptsburnella.c.1870_191_g";

    fn read(name: &str) -> Vec<u8> {
        fixture::read("internet_archive", name)
    }
    fn card(name: &str, id: &str) -> Card {
        parse(200, &read(&format!("{name}.json")), id)
            .unwrap()
            .unwrap()
    }
    fn keys(card: &Card) -> Vec<&str> {
        card.keys.iter().map(Key::as_str).collect()
    }

    #[test]
    fn every_link_form_of_one_item() {
        for uri in [
            "https://archive.org/details/goodytwoshoes00newyiala",
            "https://www.archive.org/details/goodytwoshoes00newyiala/",
            "http://archive.org/details/goodytwoshoes00newyiala?tab=about#reviews",
            "https://archive.org/details/goodytwoshoes00newyiala/page/n5/mode/2up",
            "https://archive.org/embed/goodytwoshoes00newyiala",
            "https://archive.org/metadata/goodytwoshoes00newyiala",
            "https://archive.org/metadata/goodytwoshoes00newyiala/metadata",
        ] {
            assert_eq!(
                from_uri(uri).map(|i| identifier(&i)).as_deref(),
                Some("ia:goodytwoshoes00newyiala"),
                "{uri}"
            );
        }
        // Identifiers keep their case.
        assert_eq!(
            from_uri("https://archive.org/details/Bitcoin-Whitepaper").as_deref(),
            Some("Bitcoin-Whitepaper")
        );
        assert_eq!(from_uri(&page(DARK)).as_deref(), Some(DARK));
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://web.archive.org/web/2020/https://bitcoin.org/bitcoin.pdf",
            "https://archive.org/download/bitcoin-whitepaper/bitcoin.pdf",
            "https://archive.org/details/@headywook",
            "https://archive.org/details/",
            "https://archive.org/search?query=bitcoin",
            "https://archive.org/metadata/bitcoin-whitepaper/files",
            "https://ia801905.us.archive.org/26/items/bitcoin-whitepaper/bitcoin.pdf",
            "https://archive.org/details/a%20b",
            "https://archive.org/details/.hidden",
            "https://archive.org/details/bitcoin-whitepaper/bitcoin.pdf",
            "https://archive.org/details/goodytwoshoes00newyiala/goodytwoshoes00newyiala.pdf",
            "https://archive.org/embed/bitcoin-whitepaper/bitcoin.pdf",
            "https://openlibrary.org/books/OL7095326M",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn requests() {
        assert_eq!(
            request("bitcoin-whitepaper").unwrap().url.as_str(),
            "https://archive.org/metadata/bitcoin-whitepaper/metadata"
        );
        assert_eq!(request("a/b"), None);
    }

    #[test]
    fn scanned_book() {
        let card = card("goodytwoshoes00newyiala", "goodytwoshoes00newyiala");
        assert_eq!(card.type_name, "Book");
        assert_eq!(card.id, "ia:goodytwoshoes00newyiala");
        assert_eq!(
            card.url,
            "https://archive.org/details/goodytwoshoes00newyiala"
        );
        assert_eq!(card.name.as_deref(), Some("Goody Two-Shoes"));
        assert_eq!(
            card.description.as_deref(),
            Some("Publisher's chromolithographed pictorial wrappers\nEx libris Elvah Karshner")
        );
        assert_eq!(card.image, None, "the item names no licence");
        // "c1888" is not a full date.
        assert_eq!(card.start, None);
        assert_eq!(card.details["date"], "c1888");
        assert_eq!(
            card.details["publishers"],
            serde_json::json!(["New-York : McLoughlin Bro's"])
        );
        assert_eq!(
            card.details["collections"],
            serde_json::json!(["cdl", "yrlsc", "iacl", "americana"])
        );
        assert_eq!(card.details["languages"], serde_json::json!(["eng"]));
        assert_eq!(card.details["ark"], "ark:/13960/t53f4ms96");
        assert_eq!(card.attribution, None);
        assert_eq!(keys(&card), ["openlibrary:OL7095326M"]);
        assert_eq!(card.details["openlibrary_work"], "OL16077419W");
    }

    #[test]
    fn whitepaper() {
        let card = card("bitcoin-whitepaper", "bitcoin-whitepaper");
        assert_eq!(card.name.as_deref(), Some("bitcoin"));
        assert_eq!(
            card.description.as_deref(),
            Some("The Bitcoin white paper by Satoshi Nakamoto")
        );
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(
            card.details["creators"],
            serde_json::json!(["Satoshi Nakamoto"])
        );
        assert_eq!(card.details["public_date"], "2020-10-07 17:57:24");
        assert_eq!(card.attribution.as_deref(), Some("Public Domain Mark 1.0"));
        assert_eq!(
            card.image.as_deref(),
            Some("https://archive.org/services/img/bitcoin-whitepaper")
        );
        assert_eq!(
            card.details["license_url"],
            "http://creativecommons.org/publicdomain/mark/1.0/"
        );
        assert!(card.keys.is_empty());
    }

    #[test]
    fn isbn_doi_and_editions() {
        // An ISBN-10 and ISBN-13 of one edition; the older `openlibrary` field names another
        // edition, so no edition key.
        let crime = card("crimepunishment00dost_0", "crimepunishment00dost_0");
        assert_eq!(keys(&crime), ["isbn:9780140449136"]);
        // The work is every edition: a fact, not a key.
        assert_eq!(crime.details["openlibrary_work"], "OL166894W");
        // The ISBNs of two editions.
        let algo = card(
            "introductiontoal0000unse_t3a6",
            "introductiontoal0000unse_t3a6",
        );
        assert_eq!(keys(&algo), ["openlibrary:OL23170657M"]);
        assert_eq!(algo.details["openlibrary_work"], "OL16921783W");
        let paper = card("aside-cardiovascular-319", "aside-cardiovascular-319");
        assert_eq!(keys(&paper), ["doi:10.71079/aside.cv.042826319"]);
        assert_eq!(paper.attribution.as_deref(), Some("CC BY 4.0"));
        assert_eq!(
            paper.image.as_deref(),
            Some("https://archive.org/services/img/aside-cardiovascular-319")
        );
        assert_eq!(paper.details["rights"], "Authors retain copyright.");
        assert_eq!(
            paper.start, None,
            "a date of a thing, not of an event, is a fact"
        );
    }

    #[test]
    fn missing_and_failures() {
        let unknown = "thisdoesnotexist-pinakes-xyz";
        assert_eq!(parse(200, &read("missing.json"), unknown).unwrap(), None);
        assert_eq!(
            parse(200, &read("missing-full.json"), unknown).unwrap(),
            None
        );
        assert_eq!(
            parse(200, &read("case-variant.json"), "Bitcoin-Whitepaper").unwrap(),
            None
        );
        assert_eq!(parse(200, &read("dark.json"), DARK).unwrap(), None);
        assert_eq!(parse(200, &read("dark-full.json"), DARK).unwrap(), None);
        // An answer for another item, including an error that names another one.
        assert!(parse(
            200,
            &read("bitcoin-whitepaper.json"),
            "goodytwoshoes00newyiala"
        )
        .is_err());
        assert!(parse(200, &read("missing.json"), "bitcoin-whitepaper").is_err());
        assert!(parse(200, br#"{"error":"Internal error"}"#, unknown).is_err());
        // The service's error text never becomes ours: it may name a link.
        let error = parse(
            200,
            br#"{"error":"failed [https://first.example/a,HTTPS://user:CANARY@second.example/b]"}"#,
            unknown,
        )
        .unwrap_err();
        assert!(!format!("{error:#}").contains("CANARY"), "{error:#}");
        assert!(!format!("{error:#}").contains("http"), "{error:#}");
        assert!(parse(429, b"", unknown).is_err());
        assert!(parse(502, b"<html>", unknown).is_err());
        assert!(parse(200, b"[", unknown).is_err());
    }

    #[test]
    fn types_and_licences() {
        assert_eq!(type_name("movies"), "VideoObject");
        assert_eq!(type_name("etree"), "AudioObject");
        assert_eq!(type_name("collection"), "Collection");
        assert_eq!(type_name("account"), "CreativeWork");
        assert_eq!(
            cc_name("https://creativecommons.org/licenses/by-nc-sa/3.0/us/").as_deref(),
            Some("CC BY-NC-SA 3.0")
        );
        assert_eq!(
            cc_name("http://creativecommons.org/publicdomain/zero/1.0/").as_deref(),
            Some("CC0 1.0")
        );
        assert_eq!(cc_name("https://example.org/licenses/by/4.0/"), None);
        for (licence, open) in [
            ("https://creativecommons.org/publicdomain/zero/1.0/", true),
            ("http://creativecommons.org/publicdomain/mark/1.0/", true),
            ("https://creativecommons.org/licenses/by/4.0/", true),
            ("https://creativecommons.org/licenses/by/3.0/us/", true),
            ("https://creativecommons.org/licenses/by-nc/4.0/", false),
            ("https://creativecommons.org/licenses/by-sa/4.0/", false),
            ("https://creativecommons.org/licenses/by-nc-nd/3.0/", false),
            ("https://creativecommons.org/licenses/sa/1.0/", false),
        ] {
            assert_eq!(open_image(&cc_name(licence).unwrap()), open, "{licence}");
        }
    }

    #[test]
    fn roots_and_aliases() {
        let ia = InternetArchive;
        for root in [
            "ia:bitcoin-whitepaper",
            "ia:Bitcoin-Whitepaper",
            &format!("ia:{DARK}"),
        ] {
            assert!(ia.reads(root), "{root}");
            assert_eq!(ia.alias_root(root), None, "{root}");
            assert!(!ia.invalid(root), "{root}");
            assert_eq!(crate::model::identity(root).unwrap().1, root);
        }
        assert_eq!(
            ia.alias_root("https://archive.org/details/bitcoin-whitepaper/page/n1/mode/2up")
                .as_deref(),
            Some("ia:bitcoin-whitepaper")
        );
        for uri in ["ia:", "ia:a/b", "ia:.x", "ia:bitcoin whitepaper"] {
            assert!(ia.invalid(uri), "{uri}");
            assert!(!ia.reads(uri), "{uri}");
        }
        assert!(!ia.invalid("https://archive.org/download/x/y.pdf"));
        assert!(!ia.reads("https://archive.org/download/x/y.pdf"));
    }

    /// An item's ISBN, Open Library edition and DOI are what its uploader typed: facts that
    /// join no group.
    #[test]
    fn its_ids_join_nothing() {
        use crate::sources::kit::{checked, own_keys};
        for (name, fact) in [
            ("crimepunishment00dost_0", "isbn"),
            ("introductiontoal0000unse_t3a6", "openlibrary"),
            ("aside-cardiovascular-319", "doi"),
        ] {
            let card = card(name, name);
            assert!(checked(&InternetArchive, &card)
                .attributes
                .contains_key(fact));
            assert_eq!(
                own_keys(&InternetArchive, &card),
                Vec::<String>::new(),
                "{name}"
            );
        }
    }

    #[test]
    fn documents() {
        let ia = InternetArchive;
        for (name, id) in [
            ("goodytwoshoes00newyiala", "goodytwoshoes00newyiala"),
            ("bitcoin-whitepaper", "bitcoin-whitepaper"),
            ("crimepunishment00dost_0", "crimepunishment00dost_0"),
            (
                "introductiontoal0000unse_t3a6",
                "introductiontoal0000unse_t3a6",
            ),
            ("aside-cardiovascular-319", "aside-cardiovascular-319"),
        ] {
            let card = card(name, id);
            let doc = crate::sources::kit::checked(&ia, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
        let doc = card("goodytwoshoes00newyiala", "goodytwoshoes00newyiala")
            .into_doc(ResourceMeta::new("x", "internet_archive", 60), SITE);
        assert_eq!(doc.site, "Internet Archive");
        assert_eq!(doc.attributes["openlibrary"], "OL7095326M");
        assert_eq!(doc.attributes["openlibrary_work"], "OL16077419W");
        assert_eq!(doc.attributes["collections"], "cdl; yrlsc; iacl; americana");
        let doc = card("crimepunishment00dost_0", "crimepunishment00dost_0")
            .into_doc(ResourceMeta::new("x", "internet_archive", 60), SITE);
        assert_eq!(doc.attributes["isbn"], "9780140449136");
    }
}
