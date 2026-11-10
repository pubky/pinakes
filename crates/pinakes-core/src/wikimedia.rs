//! Files on Wikimedia Commons: their titles, their description pages, and whether a file's own
//! description page says it may be shown with a credit.
//!
//! A Wikidata item names its image by file name (`P18`), and neither the item nor the file name
//! licenses the file: each file has its own licence and author on Commons. [`free_file`] reads
//! them from the file's `extmetadata` and accepts a file only under a licence on its list, with
//! the author the licence asks to credit. [`credited_image`] asks Commons about one file and
//! gives the image with its credit: every card that shows a Wikidata item's image (a MusicBrainz
//! card's enrichment, a Wikidata item's own card) takes it from there.
use crate::{
    model::{Credit, CreditField, CreditProvider},
    resolver::egress::{Egress, FetchOptions},
    sources::kit::html_text,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::Value;

const API: &str = "https://commons.wikimedia.org/w/api.php";
/// How long Commons' answer about a file stays in the egress cache (seconds); a card is read
/// again at its own refresh.
const CACHE_SECS: u64 = 3600;

/// One field's outcome, read from a Wikimedia project (Wikidata, Commons, Wikipedia).
#[derive(Clone, Debug, PartialEq)]
pub enum Found {
    Value(String, Credit),
    /// Nothing to show: the card keeps its own source's value.
    Missing,
    /// Not read for a transient failure: the card keeps its stored value.
    Unread,
}

/// The image of the Commons file `name` (in normal form, [`file_name`]) with its credit, a
/// thumbnail `width` pixels wide, when Commons says the file is free to show ([`free_file`]).
/// `Missing` for a file Commons does not hold or refuses; `Unread` for a transient failure (a
/// timeout, a 429, a 5xx, any other status but 404 and 410, an answer that is not JSON). One
/// request, through the egress and its cache. `subject` names the card in logs.
pub async fn credited_image(egress: &Egress, name: &str, width: u32, subject: &str) -> Found {
    let options = FetchOptions {
        accept: Some("application/json".into()),
        ..Default::default()
    };
    let fetched = match egress
        .discover(&file_query(name, width), &options, CACHE_SECS)
        .await
    {
        Ok(fetched) if fetched.status == 200 => fetched,
        Ok(fetched) if matches!(fetched.status, 404 | 410) => return Found::Missing,
        // A wait, or a refusal of this client: no answer about the file.
        Ok(fetched) => {
            tracing::info!(subject, status = fetched.status, "Commons did not answer");
            return Found::Unread;
        }
        Err(error) => {
            unread(subject, "commons_file", &error);
            return Found::Unread;
        }
    };
    // The author is HTML: read off the runtime's threads, as every HTML parse is.
    let body = fetched.body;
    let file = name.to_owned();
    // `None`: not JSON; `Some(None)`: an answer about another file.
    let check = crate::resolver::html::blocking(move || {
        serde_json::from_slice::<Value>(&body)
            .ok()
            .map(|v| free_file(&v, &file))
    })
    .await
    .unwrap_or_else(|error| {
        unread(subject, "commons_parse", &error);
        None
    });
    match check {
        Some(Some(FileCheck::Free(file))) => {
            let credit = Credit {
                field: CreditField::Image,
                provider: CreditProvider::WikimediaCommons,
                source_url: file.page,
                author: file.author,
                license: file.license.id,
                license_url: file.license.url,
                lang: None,
                excerpted: false,
                modified: false,
                subject: None,
            };
            match credit.bounded() {
                Some(credit) => Found::Value(file.image, credit),
                None => Found::Missing,
            }
        }
        Some(Some(FileCheck::Refused(reason))) => {
            tracing::debug!(subject, reason, "Commons file refused");
            Found::Missing
        }
        Some(Some(FileCheck::Missing)) => Found::Missing,
        // Nothing verifies the file: no image.
        Some(None) => {
            tracing::info!(subject, "Commons answered for another file");
            Found::Missing
        }
        None => {
            tracing::info!(subject, "Commons' answer is not JSON");
            Found::Unread
        }
    }
}
/// Logs a transient failure by its class, with the card and the step; never a URL.
fn unread(subject: &str, step: &'static str, error: &anyhow::Error) {
    tracing::info!(
        subject,
        step,
        kind = crate::resolver::error_class(error),
        "Commons left an image unread"
    );
}

/// A file name in MediaWiki's normal form, with underscores: underscores and runs of spaces
/// become one, the ends are trimmed and the first letter is upper case. `None` for a name
/// MediaWiki rejects.
pub fn file_name(raw: &str) -> Option<String> {
    let name = raw.replace('_', " ");
    let name = name
        .split(' ')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    let name = name.join("_");
    let bad = |c: char| c.is_control() || "#<>[]|{}/".contains(c);
    let escaped = name
        .as_bytes()
        .windows(3)
        .any(|w| w[0] == b'%' && w[1].is_ascii_hexdigit() && w[2].is_ascii_hexdigit());
    if name.is_empty() || name.len() > 240 || name.contains(bad) || escaped {
        return None;
    }
    // The first letter in upper case, as MediaWiki's `ucfirst` writes it: a letter whose upper
    // case is two letters (`ß`) stays, and so do Georgian letters, which MediaWiki leaves alone.
    let mut chars = name.chars();
    let first = chars.next()?;
    if matches!(first, '\u{10D0}'..='\u{10FA}' | '\u{10FD}'..='\u{10FF}') {
        return Some(name);
    }
    let mut upper = first.to_uppercase();
    Some(match (upper.next(), upper.next()) {
        (Some(u), None) => format!("{u}{}", chars.as_str()),
        _ => name,
    })
}

/// What MediaWiki leaves unescaped in a page URL (`wfUrlencode`).
pub const TITLE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b';')
    .remove(b'@')
    .remove(b'$')
    .remove(b'!')
    .remove(b'*')
    .remove(b'(')
    .remove(b')')
    .remove(b',')
    .remove(b'/')
    .remove(b':');

/// A file's description page on Commons, as MediaWiki writes its URL, for a name in normal form
/// ([`file_name`]).
pub fn file_page(name: &str) -> String {
    format!(
        "https://commons.wikimedia.org/wiki/{}",
        utf8_percent_encode(&format!("File:{name}"), TITLE)
    )
}

/// The `extmetadata` fields [`free_file`] reads.
const FIELDS: &str = "Artist|LicenseShortName|License|LicenseUrl|NonFree";

/// The query for one file's image URL (a thumbnail `width` pixels wide), MIME type, licence and
/// author, for a name in normal form.
pub fn file_query(name: &str, width: u32) -> String {
    let mut url = url::Url::parse(API).expect("constant URL");
    url.query_pairs_mut()
        .append_pair("action", "query")
        .append_pair("format", "json")
        .append_pair("formatversion", "2")
        .append_pair("prop", "imageinfo")
        .append_pair("iiprop", "url|mime|extmetadata")
        .append_pair("iiurlwidth", &width.to_string())
        .append_pair("iiextmetadatafilter", FIELDS)
        .append_pair("iiextmetadatalanguage", "en")
        .append_pair("titles", &format!("File:{}", name.replace('_', " ")));
    url.into()
}

/// A file that may be shown with its credit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreeFile {
    /// The image: Commons' thumbnail, else the file itself when it is a JPEG, PNG, GIF or
    /// WebP; `https:` on Wikimedia's upload hosts, without tracking parameters.
    pub image: String,
    /// The file's description page.
    pub page: String,
    /// The author, plain text; `None` only under CC0 or in the public domain.
    pub author: Option<String>,
    pub license: License,
}
/// A licence on the list [`free_file`] accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct License {
    /// An SPDX ID (`CC-BY-SA-4.0`, `CC-BY-3.0-DE`, `CC0-1.0`, `GPL-3.0-only`,
    /// `LGPL-2.1-or-later`), `public-domain`, or `Attribution`: Commons' licence of that name,
    /// which SPDX has no ID for.
    pub id: String,
    /// The licence's text; `None` for the public domain without a mark.
    pub url: Option<String>,
    /// Whether showing the file needs its author named: under every licence but CC0 and the
    /// public domain.
    pub attribution: bool,
}
/// What Commons says about a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileCheck {
    Free(FreeFile),
    /// A file that may not be shown: the reason (`non_free`, `license`, `author`, `mime`,
    /// `url`).
    Refused(&'static str),
    /// No such file on Commons.
    Missing,
}

/// The image types a card shows: raster images and SVG (shown through its PNG thumbnail).
const IMAGE_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/tiff",
    "image/svg+xml",
];
/// The image types whose original file a card may show when Commons gives no thumbnail.
const SHOWN_AS_IS: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// The file in a `file_query` answer for `name`: accepted only when Commons holds it, it is an
/// image, it is not marked non-free, its licence is on the list by its licence name and URL
/// ([`license`]: CC0, the public domain, CC BY or CC BY-SA 1.0 to 4.0 with ported versions, the
/// GNU GPL, LGPL or AGPL, Commons' `Attribution`), and, under every licence but CC0 and the
/// public domain, it names its author. `None` for an answer that is not about this file.
pub fn free_file(v: &Value, name: &str) -> Option<FileCheck> {
    let pages = v["query"]["pages"].as_array()?;
    let [page] = pages.as_slice() else {
        return None;
    };
    let asked = format!("File:{}", name.replace('_', " "));
    // MediaWiki's own normal form of the asked title, which `file_name` already wrote.
    if page["title"].as_str() != Some(asked.as_str()) {
        return None;
    }
    if page["missing"].as_bool() == Some(true)
        || page["invalid"].as_bool() == Some(true)
        || page["ns"].as_i64() != Some(6)
    {
        return Some(FileCheck::Missing);
    }
    let Some(info) = page["imageinfo"].as_array().and_then(|i| i.first()) else {
        return Some(FileCheck::Missing);
    };
    let meta = |field: &str| {
        info["extmetadata"][field]["value"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if meta("NonFree").is_some_and(|v| !v.eq_ignore_ascii_case("false")) {
        return Some(FileCheck::Refused("non_free"));
    }
    if !info["mime"]
        .as_str()
        .is_some_and(|m| IMAGE_TYPES.contains(&m))
    {
        return Some(FileCheck::Refused("mime"));
    }
    let Some(license) = license(
        meta("License"),
        meta("LicenseShortName"),
        meta("LicenseUrl"),
    ) else {
        return Some(FileCheck::Refused("license"));
    };
    let author = meta("Artist").and_then(html_text).map(|mut a| {
        crate::model::truncate(&mut a, 256);
        a.trim().to_owned()
    });
    // Commons' placeholders (`{{Unknown|author}}`, `{{Unknown photographer}}`, …) name no one;
    // matched whole, so a name that starts with one ("Unknown Pleasures …") is a name.
    let author = author.filter(|a| {
        !a.is_empty()
            && ![
                "unknown",
                "unknown author",
                "author unknown",
                "anonymous",
                "unknown photographer",
                "unknown artist",
                "unknown creator",
            ]
            .iter()
            .any(|p| a.eq_ignore_ascii_case(p))
    });
    if license.attribution && author.is_none() {
        return Some(FileCheck::Refused("author"));
    }
    // The original only when a browser shows it as it is: never a TIFF or an SVG's source.
    let original = || {
        info["mime"]
            .as_str()
            .is_some_and(|m| SHOWN_AS_IS.contains(&m))
            .then(|| info["url"].as_str().and_then(upload_url))
            .flatten()
    };
    let Some(image) = info["thumburl"]
        .as_str()
        .and_then(upload_url)
        .or_else(original)
    else {
        return Some(FileCheck::Refused("url"));
    };
    Some(FileCheck::Free(FreeFile {
        image,
        page: file_page(name),
        author,
        license,
    }))
}

/// An `https:` URL on Wikimedia's upload hosts, without its `utm_*` tracking parameters, of at
/// most 1,024 bytes.
fn upload_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || !matches!(
            url.host_str(),
            Some("upload.wikimedia.org" | "thumb.wikimedia.org")
        )
        || url.port().is_some()
        || !url.username().is_empty()
    {
        return None;
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    let url = String::from(url);
    (url.len() <= 1024).then_some(url)
}

/// The licence a file's `License` (its template, `cc-by-sa-4.0`), `LicenseShortName`
/// (`CC BY-SA 4.0`, `GPLv3`) and `LicenseUrl` name, when it is on the list: CC0 1.0, the public
/// domain, CC BY and CC BY-SA 1.0 to 4.0 (ported versions too), the GNU GPL (1.0 to 3.0), LGPL
/// (2.0 to 3.0) and AGPL (3.0), and Commons' `Attribution` licence (`gnu` and `attribution`
/// give their IDs). A Creative Commons licence needs its URL, and its template or name must
/// name the same licence. A GNU licence or `Attribution` needs its name; a URL, when there is
/// one, must name the same licence, and so must every name.
pub fn license(template: Option<&str>, short: Option<&str>, url: Option<&str>) -> Option<License> {
    let template = template.map(str::to_ascii_lowercase);
    let short = short.map(|s| s.trim().to_ascii_lowercase());
    let from_url = url.and_then(cc_url);
    let cc0 = template.as_deref() == Some("cc0")
        || short
            .as_deref()
            .is_some_and(|s| s == "cc0" || s == "cc0 1.0");
    if cc0 || matches!(&from_url, Some(Cc::Zero)) {
        // Every name must agree: a CC0 URL beside another licence's name is no CC0.
        let agrees = from_url.as_ref().is_none_or(|c| matches!(c, Cc::Zero))
            && (cc0 || (template.is_none() && short.is_none()));
        return agrees.then(|| License {
            id: "CC0-1.0".into(),
            url: Some("https://creativecommons.org/publicdomain/zero/1.0/".into()),
            attribution: false,
        });
    }
    let public_domain = template
        .as_deref()
        .is_some_and(|t| t == "pd" || t.starts_with("pd-"))
        || short
            .as_deref()
            .is_some_and(|s| s == "public domain" || s.starts_with("pd-"));
    if public_domain || matches!(&from_url, Some(Cc::Mark)) {
        let agrees = from_url.as_ref().is_none_or(|c| matches!(c, Cc::Mark))
            && (public_domain || (template.is_none() && short.is_none()));
        return agrees.then(|| License {
            id: "public-domain".into(),
            url: matches!(&from_url, Some(Cc::Mark))
                .then(|| "https://creativecommons.org/publicdomain/mark/1.0/".into()),
            attribution: false,
        });
    }
    let Some(Cc::By {
        share_alike,
        version,
        port,
    }) = from_url
    else {
        // No Creative Commons URL: a GNU licence or `Attribution`, or none on the list.
        let (template, short) = (template.as_deref(), short.as_deref());
        return gnu(template, short, url).or_else(|| attribution(template, short, url));
    };
    let kind = if share_alike { "by-sa" } else { "by" };
    let port_suffix = port.as_deref().map(|p| format!("-{p}")).unwrap_or_default();
    let named_template = format!("cc-{kind}-{version}{port_suffix}");
    let named_short = format!(
        "cc {kind} {version}{}",
        port.as_deref().map(|p| format!(" {p}")).unwrap_or_default()
    );
    let named = template.as_deref() == Some(named_template.as_str())
        || short.as_deref() == Some(named_short.as_str());
    named.then(|| License {
        id: format!(
            "CC-{}-{version}{}",
            kind.to_ascii_uppercase(),
            port_suffix.to_ascii_uppercase()
        ),
        url: Some(format!(
            "https://creativecommons.org/licenses/{kind}/{version}/{}",
            port.as_deref().map(|p| format!("{p}/")).unwrap_or_default()
        )),
        attribution: true,
    })
}

/// A GNU licence family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gnu {
    Gpl,
    Lgpl,
    Agpl,
}
impl Gnu {
    fn name(self) -> &'static str {
        match self {
            Gnu::Gpl => "GPL",
            Gnu::Lgpl => "LGPL",
            Gnu::Agpl => "AGPL",
        }
    }
    /// The version (as SPDX writes it) that `number` (`3`, `3.0`, `2.1`) names in this family;
    /// `None` for one the family never had.
    fn version(self, number: &str) -> Option<&'static str> {
        Some(match (self, number) {
            (Gnu::Gpl, "1" | "1.0") => "1.0",
            (Gnu::Gpl | Gnu::Lgpl, "2" | "2.0") => "2.0",
            (Gnu::Lgpl, "2.1") => "2.1",
            (_, "3" | "3.0") => "3.0",
            _ => return None,
        })
    }
    /// The text of `version` on gnu.org.
    fn text(self, version: &str) -> String {
        let old = if version == "3.0" {
            ""
        } else {
            "old-licenses/"
        };
        format!(
            "https://www.gnu.org/licenses/{old}{}-{version}.html",
            self.name().to_ascii_lowercase()
        )
    }
}
/// A GNU licence as a template or short name writes it (`GPLv3`, `GPLv2+`, `LGPL 3`, `AGPL`,
/// `GPL-3.0-or-later`; lower case): its family, its version when it names one, and `Some(true)`
/// when it says "or later", `Some(false)` when it says "only".
struct GnuName {
    family: Gnu,
    version: Option<&'static str>,
    later: Option<bool>,
}
fn gnu_name(name: &str) -> Option<GnuName> {
    let name = name.replace('_', " ");
    let name = name
        .strip_prefix("gnu")
        .map_or(name.as_str(), |s| s.trim_start_matches([' ', '-']));
    let (family, rest) = [("agpl", Gnu::Agpl), ("lgpl", Gnu::Lgpl), ("gpl", Gnu::Gpl)]
        .into_iter()
        .find_map(|(prefix, family)| name.strip_prefix(prefix).map(|rest| (family, rest)))?;
    let rest = rest.trim_start_matches([' ', '-']);
    let (marked, rest) = match rest
        .strip_prefix("version")
        .or_else(|| rest.strip_prefix('v'))
    {
        Some(rest) => (true, rest.trim_start()),
        None => (false, rest),
    };
    let (number, suffix) = rest.split_at(
        rest.find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len()),
    );
    let later = match suffix.replace('-', " ").trim() {
        "" => None,
        "+" | "or later" | "or any later" | "or any later version" => Some(true),
        "only" => Some(false),
        _ => return None,
    };
    let version = match number {
        "" => None,
        number => Some(family.version(number)?),
    };
    // "v" and "or later" or "only" go with a version.
    if version.is_none() && (marked || later.is_some()) {
        return None;
    }
    Some(GnuName {
        family,
        version,
        later,
    })
}
/// The GNU licence a `gnu.org` URL names (http or https, `www.` or not, under `licenses/`,
/// `licenses/old-licenses/` or `copyleft/`, with `.html`, `.en.html` or neither, a trailing
/// slash allowed): its family, its version unless the page is the family's own (`gpl.html`,
/// `lgpl.html`, `agpl.html`), and whether it is under `copyleft/`.
fn gnu_url(raw: &str) -> Option<(Gnu, Option<&'static str>, bool)> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str()?.trim_start_matches("www.") != "gnu.org"
    {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let (page, copyleft) = match segments.as_slice() {
        ["copyleft", page] => (*page, true),
        ["licenses", page] | ["licenses", "old-licenses", page] => (*page, false),
        _ => return None,
    };
    let page = page.strip_suffix(".html").unwrap_or(page);
    let page = page.strip_suffix(".en").unwrap_or(page);
    let (family, version) = match page.split_once('-') {
        Some((family, version)) => (family, Some(version)),
        None => (page, None),
    };
    let family = match family {
        "gpl" => Gnu::Gpl,
        "lgpl" => Gnu::Lgpl,
        "agpl" => Gnu::Agpl,
        _ => return None,
    };
    let version = match version {
        Some(version) => Some(family.version(version)?),
        None => None,
    };
    Some((family, version, copyleft))
}
/// The GNU GPL, LGPL or AGPL a file's template and short name (lower case) and URL name: every
/// name given is one, of one family, and the URL, when there is one, a `gnu.org` page of that
/// family; two versions given are the same, and so are two "or later"s or "only"s. The ID is
/// the licence every Commons template that writes those values grants:
/// - a name or URL with a version: that version, "or later" only when a name says so
///   (`GPLv3`, written by both `{{GPLv3 only}}` and `{{GPLv3+}}`, is `GPL-3.0-only`);
/// - `GPL` (`{{GPL}}`, version 2 or later): `GPL-2.0-or-later`;
/// - `LGPL` with the family's page: `LGPL-2.1-or-later` under `licenses/` (`{{LGPL}}`, 2.1 or
///   later), `LGPL-3.0-or-later` under `copyleft/` (`{{LGPLv3}}`, 3 or later); without a URL
///   (`{{LGPLv2.1}}`, 2.1 only, whose URL Commons does not read): `LGPL-2.1-only`;
/// - `AGPL` (`{{AGPL}}`, 3 or later, and `{{AGPLv3 only}}`): `AGPL-3.0-only`.
///
/// The URL credited is the gnu.org text of the version the ID names.
fn gnu(template: Option<&str>, short: Option<&str>, url: Option<&str>) -> Option<License> {
    let names = [template, short]
        .into_iter()
        .flatten()
        .map(gnu_name)
        .collect::<Option<Vec<_>>>()?;
    let family = names.first()?.family;
    let from_url = match url {
        Some(url) => Some(gnu_url(url)?),
        None => None,
    };
    if names.iter().any(|n| n.family != family) || from_url.is_some_and(|(f, _, _)| f != family) {
        return None;
    }
    let mut versions = names
        .iter()
        .filter_map(|n| n.version)
        .chain(from_url.and_then(|(_, v, _)| v));
    let version = versions.next();
    let mut laters = names.iter().filter_map(|n| n.later);
    let later = laters.next();
    if versions.any(|v| Some(v) != version) || laters.any(|l| Some(l) != later) {
        return None;
    }
    let (version, later) = match (version, family) {
        (Some(version), _) => (version, later == Some(true)),
        (None, Gnu::Gpl) => ("2.0", true),
        (None, Gnu::Lgpl) => match from_url {
            Some((_, _, true)) => ("3.0", true),
            Some((_, _, false)) => ("2.1", true),
            None => ("2.1", false),
        },
        (None, Gnu::Agpl) => ("3.0", false),
    };
    Some(License {
        id: format!(
            "{}-{version}-{}",
            family.name(),
            if later { "or-later" } else { "only" }
        ),
        url: Some(family.text(version)),
        attribution: true,
    })
}
/// Commons' own text of its `Attribution` licence.
const ATTRIBUTION: &str = "https://commons.wikimedia.org/wiki/Template:Attribution";
/// Commons' `Attribution` licence (`{{Attribution}}`: any use, the author credited), when every
/// name given (lower case) is `attribution` and the URL, when there is one, is its template's
/// page. Its ID is Commons' name for it, which is no SPDX or Creative Commons ID.
fn attribution(template: Option<&str>, short: Option<&str>, url: Option<&str>) -> Option<License> {
    let mut names = [template, short].into_iter().flatten().peekable();
    if names.peek().is_none() || !names.all(|n| n == "attribution") {
        return None;
    }
    let page = |raw: &str| {
        url::Url::parse(raw.trim()).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host_str() == Some("commons.wikimedia.org")
                && u.path() == "/wiki/Template:Attribution"
        })
    };
    url.is_none_or(page).then(|| License {
        id: "Attribution".into(),
        url: Some(ATTRIBUTION.into()),
        attribution: true,
    })
}

/// A Creative Commons licence by its URL.
enum Cc {
    Zero,
    Mark,
    By {
        share_alike: bool,
        version: &'static str,
        port: Option<String>,
    },
}
/// The Creative Commons licence a `creativecommons.org` URL names (http or https, a trailing
/// `deed…` page or slash allowed): CC0 1.0, the Public Domain Mark 1.0, CC BY or CC BY-SA 1.0 to
/// 4.0 with an optional two-letter port.
fn cc_url(raw: &str) -> Option<Cc> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str()?.trim_start_matches("www.") != "creativecommons.org"
    {
        return None;
    }
    let mut segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segments.last().is_some_and(|s| s.starts_with("deed")) {
        segments.pop();
    }
    match segments.as_slice() {
        ["publicdomain", "zero", "1.0"] => Some(Cc::Zero),
        ["publicdomain", "mark", "1.0"] => Some(Cc::Mark),
        ["licenses", kind, version, rest @ ..] => {
            let share_alike = match *kind {
                "by" => false,
                "by-sa" => true,
                _ => return None,
            };
            let version = ["1.0", "2.0", "2.5", "3.0", "4.0"]
                .into_iter()
                .find(|v| v == version)?;
            let port = match rest {
                [] => None,
                [port]
                    if port.len() == 2
                        && port.bytes().all(|b| b.is_ascii_lowercase())
                        && version != "4.0" =>
                {
                    Some((*port).to_owned())
                }
                _ => return None,
            };
            Some(Cc::By {
                share_alike,
                version,
                port,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn licences_on_the_list_by_name_and_url() {
        let by_sa = license(
            Some("cc-by-sa-2.0"),
            Some("CC BY-SA 2.0"),
            Some("https://creativecommons.org/licenses/by-sa/2.0"),
        )
        .unwrap();
        assert_eq!(
            (by_sa.id.as_str(), by_sa.url.as_deref(), by_sa.attribution),
            (
                "CC-BY-SA-2.0",
                Some("https://creativecommons.org/licenses/by-sa/2.0/"),
                true
            )
        );
        let ported = license(
            None,
            Some("CC BY-SA 3.0 de"),
            Some("http://creativecommons.org/licenses/by-sa/3.0/de/deed.en"),
        )
        .unwrap();
        assert_eq!(ported.id, "CC-BY-SA-3.0-DE");
        assert_eq!(
            ported.url.as_deref(),
            Some("https://creativecommons.org/licenses/by-sa/3.0/de/")
        );
        assert_eq!(
            license(Some("cc0"), Some("CC0"), None).unwrap().id,
            "CC0-1.0"
        );
        let pd = license(Some("pd"), Some("Public domain"), None).unwrap();
        assert_eq!(
            (pd.id.as_str(), pd.url, pd.attribution),
            ("public-domain", None, false)
        );
        // Off the list, or names that disagree.
        for (template, short, url) in [
            (Some("gfdl"), Some("GFDL"), None),
            (
                Some("fal"),
                Some("FAL"),
                Some("http://artlibre.org/licence/lal/en"),
            ),
            (
                Some("cc-by-nc-2.0"),
                Some("CC BY-NC 2.0"),
                Some("https://creativecommons.org/licenses/by-nc/2.0/"),
            ),
            (Some("cc-by-sa-4.0"), Some("CC BY-SA 4.0"), None),
            (
                Some("cc-by-4.0"),
                Some("CC BY 4.0"),
                Some("https://creativecommons.org/licenses/by-sa/4.0/"),
            ),
            (
                Some("gfdl"),
                None,
                Some("https://creativecommons.org/publicdomain/zero/1.0/"),
            ),
            (
                None,
                None,
                Some("https://creativecommons.org/licenses/by/9.0/"),
            ),
            (None, None, None),
        ] {
            assert_eq!(
                license(template, short, url),
                None,
                "{template:?} {short:?} {url:?}"
            );
        }
    }
    /// Commons' placeholder for an unknown author names no one: refused under CC BY, shown
    /// without an author under CC0.
    #[test]
    fn a_placeholder_author_is_no_author() {
        let answer = |artist: &str, license: &str, url: &str| {
            serde_json::json!({"query": {"pages": [{"title": "File:A.jpg", "ns": 6, "imageinfo": [{
                "mime": "image/jpeg",
                "url": "https://upload.wikimedia.org/wikipedia/commons/a/aa/A.jpg",
                "extmetadata": {
                    "Artist": {"value": artist},
                    "License": {"value": license},
                    "LicenseUrl": {"value": url},
                },
            }]}]}})
        };
        let by = |artist: &str| {
            free_file(
                &answer(
                    artist,
                    "cc-by-4.0",
                    "https://creativecommons.org/licenses/by/4.0",
                ),
                "A.jpg",
            )
        };
        for placeholder in [
            "Unknown author",
            "<span class=\"fn\">unknown</span>",
            "Anonymous",
            " Author unknown ",
            "Unknown photographer",
            "<span>Unknown artist</span>",
            "unknown creator",
        ] {
            assert_eq!(
                by(placeholder),
                Some(FileCheck::Refused("author")),
                "{placeholder}"
            );
        }
        let Some(FileCheck::Free(file)) = by("Unknown Pleasures Photography Ltd") else {
            panic!("a named author");
        };
        // Exact: a placeholder within a name is part of the name.
        assert!(matches!(
            by("Unknown photographer, colourised by Jane Doe"),
            Some(FileCheck::Free(_))
        ));
        assert_eq!(
            file.author.as_deref(),
            Some("Unknown Pleasures Photography Ltd")
        );
        let cc0 = free_file(
            &answer(
                "Unknown author",
                "cc0",
                "https://creativecommons.org/publicdomain/zero/1.0/",
            ),
            "A.jpg",
        );
        let Some(FileCheck::Free(file)) = cc0 else {
            panic!("CC0 needs no author: {cc0:?}");
        };
        assert_eq!(file.author, None);
    }
    /// Without a thumbnail, a JPEG is shown as its original file; a TIFF or an SVG is not.
    #[test]
    fn an_original_file_only_when_a_browser_shows_it() {
        let check = |mime: &str| {
            free_file(
                &serde_json::json!({"query": {"pages": [{"title": "File:A.x", "ns": 6, "imageinfo": [{
                    "mime": mime,
                    "url": "https://upload.wikimedia.org/wikipedia/commons/a/aa/A.x",
                    "extmetadata": {"License": {"value": "cc0"}, "LicenseUrl": {"value": "https://creativecommons.org/publicdomain/zero/1.0/"}},
                }]}]}}),
                "A.x",
            )
        };
        let Some(FileCheck::Free(file)) = check("image/jpeg") else {
            panic!("a JPEG's original");
        };
        assert_eq!(
            file.image,
            "https://upload.wikimedia.org/wikipedia/commons/a/aa/A.x"
        );
        for mime in ["image/tiff", "image/svg+xml"] {
            assert_eq!(check(mime), Some(FileCheck::Refused("url")), "{mime}");
        }
    }
    #[test]
    fn file_pages_and_queries() {
        assert_eq!(
            file_name("miles davis by_Palumbo  cropped.jpg").as_deref(),
            Some("Miles_davis_by_Palumbo_cropped.jpg")
        );
        assert_eq!(
            file_page("Miles_Davis_by_Palumbo_cropped.jpg"),
            "https://commons.wikimedia.org/wiki/File:Miles_Davis_by_Palumbo_cropped.jpg"
        );
        let query = file_query("Daft_Punk_in_2013.jpg", 640);
        assert!(
            query.contains("titles=File%3ADaft+Punk+in+2013.jpg"),
            "{query}"
        );
        assert!(query.contains("iiurlwidth=640"), "{query}");
        assert_eq!(
            upload_url("https://thumb.wikimedia.org/wikipedia/commons/thumb/d/db/x.jpg/960px-x.jpg?utm_source=commons.wikimedia.org&utm_campaign=imageinfo").as_deref(),
            Some("https://thumb.wikimedia.org/wikipedia/commons/thumb/d/db/x.jpg/960px-x.jpg")
        );
        assert_eq!(upload_url("http://upload.wikimedia.org/x.jpg"), None);
        assert_eq!(upload_url("https://example.org/x.jpg"), None);
    }

    /// The GNU licences and `Attribution` as Commons writes them (`LicenseShortName` and
    /// `LicenseUrl` as its templates give them; Commons gives no `License` for these), each
    /// with the SPDX ID the templates grant and the text of that licence; and names that
    /// disagree, or name no licence on the list, refused.
    #[test]
    fn gnu_licences_and_attribution_by_name_and_url() {
        let gnu = "https://www.gnu.org/licenses/";
        for (template, short, url, id, text) in [
            // {{GPL}}, {{GPLv2+}}: version 2 or later.
            (
                None,
                "GPL",
                Some("http://www.gnu.org/licenses/gpl.html"),
                "GPL-2.0-or-later",
                "old-licenses/gpl-2.0.html",
            ),
            // {{GPLv2}}, {{GPLv2 only}}.
            (
                None,
                "GPLv2",
                Some("https://www.gnu.org/licenses/old-licenses/gpl-2.0.html"),
                "GPL-2.0-only",
                "old-licenses/gpl-2.0.html",
            ),
            // {{GPLv3}} (that is, {{GPLv3 only}}) and {{GPLv3+}} both write this: version 3 is
            // what both grant.
            (
                None,
                "GPLv3",
                Some("http://www.gnu.org/licenses/gpl-3.0.html"),
                "GPL-3.0-only",
                "gpl-3.0.html",
            ),
            (
                None,
                "GPLv3+",
                Some("https://www.gnu.org/licenses/gpl-3.0.en.html"),
                "GPL-3.0-or-later",
                "gpl-3.0.html",
            ),
            (
                None,
                "GPLv2 or later",
                Some("https://gnu.org/licenses/old-licenses/gpl-2.0/"),
                "GPL-2.0-or-later",
                "old-licenses/gpl-2.0.html",
            ),
            (
                None,
                "GPLv2+",
                None,
                "GPL-2.0-or-later",
                "old-licenses/gpl-2.0.html",
            ),
            (
                Some("gpl-3.0-or-later"),
                "GPL v3+",
                Some("https://www.gnu.org/licenses/gpl.html"),
                "GPL-3.0-or-later",
                "gpl-3.0.html",
            ),
            (
                None,
                "GNU GPL version 1",
                None,
                "GPL-1.0-only",
                "old-licenses/gpl-1.0.html",
            ),
            // {{LGPL}} (2.1 or later) and {{LGPLv3}} (3 or later) with the family's page.
            (
                None,
                "LGPL",
                Some("http://www.gnu.org/licenses/lgpl.html"),
                "LGPL-2.1-or-later",
                "old-licenses/lgpl-2.1.html",
            ),
            (
                None,
                "LGPL",
                Some("https://www.gnu.org/copyleft/lgpl.html"),
                "LGPL-3.0-or-later",
                "lgpl-3.0.html",
            ),
            // {{LGPLv2.1}}, {{LGPLv2.1 only}}, whose URL Commons does not read.
            (
                None,
                "LGPL",
                None,
                "LGPL-2.1-only",
                "old-licenses/lgpl-2.1.html",
            ),
            (
                None,
                "LGPLv2.1",
                Some("http://www.gnu.org/licenses/old-licenses/lgpl-2.1.html"),
                "LGPL-2.1-only",
                "old-licenses/lgpl-2.1.html",
            ),
            // {{LGPLv3 only}}.
            (
                None,
                "LGPL 3",
                Some("https://www.gnu.org/copyleft/lgpl.html"),
                "LGPL-3.0-only",
                "lgpl-3.0.html",
            ),
            (
                None,
                "LGPLv2",
                None,
                "LGPL-2.0-only",
                "old-licenses/lgpl-2.0.html",
            ),
            // {{AGPL}} (3 or later) and {{AGPLv3 only}} both write this.
            (
                None,
                "AGPL",
                Some("http://www.gnu.org/licenses/agpl.html"),
                "AGPL-3.0-only",
                "agpl-3.0.html",
            ),
            (
                None,
                "AGPLv3",
                Some("https://www.gnu.org/licenses/agpl-3.0.html"),
                "AGPL-3.0-only",
                "agpl-3.0.html",
            ),
            (
                None,
                "AGPL-3.0-or-later",
                None,
                "AGPL-3.0-or-later",
                "agpl-3.0.html",
            ),
        ] {
            let found = license(template, Some(short), url);
            let expected = License {
                id: id.into(),
                url: Some(format!("{gnu}{text}")),
                attribution: true,
            };
            assert_eq!(found, Some(expected), "{template:?} {short} {url:?}");
        }
        for (template, url) in [
            (None, None),
            (
                None,
                Some("https://commons.wikimedia.org/wiki/Template:Attribution"),
            ),
            (
                Some("attribution"),
                Some("http://commons.wikimedia.org/wiki/Template:Attribution"),
            ),
        ] {
            assert_eq!(
                license(template, Some("Attribution"), url),
                Some(License {
                    id: "Attribution".into(),
                    url: Some("https://commons.wikimedia.org/wiki/Template:Attribution".into()),
                    attribution: true,
                }),
                "{template:?} {url:?}"
            );
        }
        for (template, short, url) in [
            // The URL names another family, another version, or no GNU page.
            (
                None,
                Some("GPLv3"),
                Some("https://www.gnu.org/licenses/lgpl-3.0.html"),
            ),
            (
                None,
                Some("AGPL"),
                Some("https://www.gnu.org/licenses/old-licenses/gpl-2.0.html"),
            ),
            (
                None,
                Some("LGPL"),
                Some("http://www.gnu.org/licenses/gpl.html"),
            ),
            (
                None,
                Some("GPLv3"),
                Some("https://www.gnu.org/licenses/old-licenses/gpl-2.0.html"),
            ),
            (
                None,
                Some("GPLv3"),
                Some("https://www.gnu.org/licenses/fdl-1.3.html"),
            ),
            (
                None,
                Some("GPLv3"),
                Some("https://example.org/licenses/gpl-3.0.html"),
            ),
            (
                None,
                Some("GPL"),
                Some("https://creativecommons.org/licenses/by/4.0/"),
            ),
            (
                None,
                Some("GPL"),
                Some("https://creativecommons.org/publicdomain/zero/1.0/"),
            ),
            // The names disagree.
            (Some("gpl-3.0-only"), Some("GPLv3+"), None),
            (
                Some("cc-by-sa-4.0"),
                Some("GPLv3"),
                Some("http://www.gnu.org/licenses/gpl-3.0.html"),
            ),
            (Some("attribution"), Some("GPL"), None),
            (Some("gfdl"), Some("Attribution"), None),
            // An "Attribution" name beside another licence's URL.
            (
                None,
                Some("Attribution"),
                Some("https://creativecommons.org/licenses/by-nc/4.0/"),
            ),
            (
                None,
                Some("Attribution"),
                Some("https://creativecommons.org/licenses/by/4.0/"),
            ),
            (
                None,
                Some("Attribution"),
                Some("http://www.gnu.org/licenses/gpl.html"),
            ),
            // No version the family had, or a name that is not a licence's.
            (None, Some("GPL 2.1"), None),
            (None, Some("AGPLv1"), None),
            (None, Some("GPLv"), None),
            (None, Some("GPL+"), None),
            (None, Some("GPL linking exception"), None),
            (None, Some("Attribution-ShareAlike"), None),
            // A URL alone names no licence.
            (
                None,
                None,
                Some("https://www.gnu.org/licenses/gpl-3.0.html"),
            ),
            // Off the list.
            (None, Some("GFDL"), None),
            (
                Some("fal"),
                Some("FAL"),
                Some("http://artlibre.org/licence/lal/en"),
            ),
            (
                None,
                Some("CC BY-NC 4.0"),
                Some("https://creativecommons.org/licenses/by-nc/4.0/"),
            ),
        ] {
            assert_eq!(
                license(template, short, url),
                None,
                "{template:?} {short:?} {url:?}"
            );
        }
    }
    /// Under a GNU licence or `Attribution`, a file is shown only with its author: without one,
    /// or with Commons' placeholder, it is refused, and a file marked non-free is refused
    /// whatever its licence.
    #[test]
    fn a_gnu_or_attribution_file_needs_its_author() {
        let check = |artist: Option<&str>, short: &str, url: Option<&str>, non_free: bool| {
            let mut meta = serde_json::json!({"LicenseShortName": {"value": short}});
            if let Some(artist) = artist {
                meta["Artist"] = serde_json::json!({"value": artist});
            }
            if let Some(url) = url {
                meta["LicenseUrl"] = serde_json::json!({"value": url});
            }
            if non_free {
                meta["NonFree"] = serde_json::json!({"value": "true"});
            }
            free_file(
                &serde_json::json!({"query": {"pages": [{"title": "File:A.png", "ns": 6, "imageinfo": [{
                    "mime": "image/png",
                    "url": "https://upload.wikimedia.org/wikipedia/commons/a/aa/A.png",
                    "extmetadata": meta,
                }]}]}}),
                "A.png",
            )
        };
        let agpl = Some("http://www.gnu.org/licenses/agpl.html");
        let gplv3 = Some("http://www.gnu.org/licenses/gpl-3.0.html");
        for (short, url, id) in [
            ("AGPL", agpl, "AGPL-3.0-only"),
            ("GPLv3", gplv3, "GPL-3.0-only"),
            ("Attribution", None, "Attribution"),
        ] {
            let artist =
                "<a href=\"//commons.wikimedia.org/wiki/User:A\">Jane Doe</a> (screenshot)";
            let Some(FileCheck::Free(file)) = check(Some(artist), short, url, false) else {
                panic!("{short} with its author");
            };
            assert_eq!(
                (file.author.as_deref(), file.license.id.as_str()),
                (Some("Jane Doe (screenshot)"), id)
            );
            assert_eq!(file.page, "https://commons.wikimedia.org/wiki/File:A.png");
            for artist in [None, Some("Unknown author"), Some("<span>anonymous</span>")] {
                assert_eq!(
                    check(artist, short, url, false),
                    Some(FileCheck::Refused("author")),
                    "{short} {artist:?}"
                );
            }
            assert_eq!(
                check(Some("Jane Doe"), short, url, true),
                Some(FileCheck::Refused("non_free")),
                "{short}"
            );
        }
        assert_eq!(
            check(
                Some("Jane Doe"),
                "GPLv3",
                Some("https://www.gnu.org/licenses/lgpl.html"),
                false
            ),
            Some(FileCheck::Refused("license"))
        );
    }
}
