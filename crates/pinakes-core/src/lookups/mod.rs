//! Exact lookups on external services, as pure request builders and response parsers, and the
//! "also on" sources that run them ([`sources`]).
//!
//! The family modules perform no I/O. A builder returns the `Request` to send; a source fetches
//! it through Egress and hands the status and body to the matching parser. Parsers return
//! `Ok(None)` (or an empty result) when the service answers that the object does not exist, and
//! an error for anything else they cannot read, so the caller retries later.
//!
//! Every join is exact: a key read from the service that owns it, compared byte for byte (after
//! the key's own normalisation). No title or name matching.
use anyhow::{bail, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::fmt;
use url::Url;

pub mod archive;
pub mod games;
pub mod packages;
pub mod papers;
pub mod places;
pub mod sources;
pub mod wikidata;

/// One HTTP GET to an external API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub url: Url,
    /// The `Accept` header, when the API needs a specific one.
    pub accept: Option<&'static str>,
    /// Whether the caller follows redirects. `false` where the redirect target is the answer.
    pub follow_redirects: bool,
}
impl Request {
    fn get(url: &str) -> Option<Self> {
        Some(Self {
            url: Url::parse(url).ok()?,
            accept: None,
            follow_redirects: true,
        })
    }
}

/// A link to the same object on another service, derived from an exact lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The service's name as shown to people, e.g. "Software Heritage".
    pub site: String,
    pub uri: String,
    /// The source the link was derived from, e.g. `wikidata` or `unpaywall`.
    pub via: &'static str,
    /// What the link is, when the service offers more than one kind, e.g. `pdf`.
    pub kind: Option<&'static str>,
    /// When the linked copy was made (RFC 3339), for archived copies.
    pub at: Option<String>,
}
impl Link {
    fn new(site: impl Into<String>, uri: impl Into<String>, via: &'static str) -> Self {
        Self {
            site: site.into(),
            uri: uri.into(),
            via,
            kind: None,
            at: None,
        }
    }
    fn kind(mut self, kind: &'static str) -> Self {
        self.kind = Some(kind);
        self
    }
}

/// An exact join key, `{namespace}:{value}`, e.g. `wikidata:Q7024` or `osm:relation/2888417`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(String);
impl Key {
    fn new(namespace: &str, value: &str) -> Self {
        Self(format!("{namespace}:{value}"))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one lookup found: the keys it proved and the links it derived.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub keys: Vec<Key>,
    pub links: Vec<Link>,
}
impl Found {
    fn push_key(&mut self, key: Key) {
        if !self.keys.contains(&key) {
            self.keys.push(key);
        }
    }
    fn push_link(&mut self, link: Link) {
        if !self.links.iter().any(|l| l.uri == link.uri) {
            self.links.push(link);
        }
    }
}

/// The plain name of a service these lookups read or link to, by its link: the name their derived
/// links carry ("OpenStreetMap", "Google Play", "crates.io", "DOI"), so a tagged member of a
/// group and a derived link to the same service read as one service.
pub fn service_name(uri: &str) -> Option<&'static str> {
    if uri.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("doi:")) {
        return Some("DOI");
    }
    let url = web_url(uri)?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    Some(match host {
        "openstreetmap.org" | "osm.org" if crate::resolver::osm::parse_id(uri).is_some() => {
            "OpenStreetMap"
        }
        "wikidata.org" | "m.wikidata.org" if sources::wikidata_item(uri).is_some() => "Wikidata",
        "doi.org" | "dx.doi.org" => "DOI",
        "store.steampowered.com" | "steamcommunity.com" => "Steam",
        "steamdb.info" => "SteamDB",
        "protondb.com" => "ProtonDB",
        "pcgamingwiki.com" => "PCGamingWiki",
        "gog.com" => "GOG",
        "apps.apple.com" => "App Store",
        "play.google.com" => "Google Play",
        "f-droid.org" => "F-Droid",
        "flathub.org" => "Flathub",
        "snapcraft.io" => "Snap Store",
        "crates.io" => "crates.io",
        "docs.rs" => "Docs.rs",
        "lib.rs" => "lib.rs",
        "npmjs.com" => "npm",
        "jsdelivr.com" => "jsDelivr",
        "unpkg.com" => "unpkg",
        "pypi.org" => "PyPI",
        "github.com" => "GitHub",
        "gitlab.com" => "GitLab",
        "archive.softwareheritage.org" => "Software Heritage",
        "arxiv.org" => "arXiv",
        "alphaxiv.org" => "alphaXiv",
        "pubmed.ncbi.nlm.nih.gov" => "PubMed",
        "pmc.ncbi.nlm.nih.gov" => "PubMed Central",
        "huggingface.co" if url.path().starts_with("/papers/") => "Hugging Face Papers",
        "btcmap.org" => "BTC Map",
        "web.archive.org" => "Wayback Machine",
        _ => return None,
    })
}

/// The join key a search names (`key={namespace}:{value}`), normalized as the sources record
/// it, or `None` when it is not one of these lookups' keys.
pub fn search_key(namespace: &str, value: &str) -> Option<String> {
    let digits = |v: &str| plain(v, 20, |c| c.is_ascii_digit()) && !v.starts_with('0');
    let key = match namespace {
        "osm" => {
            let (kind, id) = value.split_once('/')?;
            (matches!(kind, "node" | "way" | "relation") && digits(id))
                .then(|| Key::new("osm", value))?
        }
        "doi" => papers::doi_key(&crate::resolver::cards::doi(&format!("doi:{value}"))?),
        "arxiv" => papers::arxiv_key(&papers::arxiv_id(value)?),
        "pmid" => digits(value).then(|| Key::new("pmid", value))?,
        "pmcid" => {
            let n = value.strip_prefix("PMC")?;
            digits(n).then(|| Key::new("pmcid", value))?
        }
        "crate" => packages::Package::new(packages::Registry::Crates, value)?.key(),
        "npm" => packages::Package::new(packages::Registry::Npm, value)?.key(),
        "pypi" => packages::Package::new(packages::Registry::PyPI, value)?.key(),
        "repo" => {
            let (host, path) = value.split_once('/')?;
            packages::Repo::new(host, path)?.key()
        }
        "steam" => games::steam_key(games::appid(value)?),
        "gog" => games::gog_key(value.parse().ok().filter(|_| digits(value))?),
        "fr" => Key::new(
            "fr",
            &crate::sources::federal_register::document_number(value)?,
        ),
        "discogs" => {
            let (kind, id) = value.split_once(':')?;
            (matches!(kind, "artist" | "master" | "release") && digits(id))
                .then(|| Key::new("discogs", value))?
        }
        _ => return None,
    };
    Some(key.0)
}

/// The body of a response, `None` for a status that means "no such object", or an error.
fn body<'a>(status: u16, body: &'a [u8], missing: &[u16]) -> Result<Option<&'a [u8]>> {
    match status {
        200 => Ok(Some(body)),
        s if missing.contains(&s) => Ok(None),
        s => bail!("unexpected status {s}"),
    }
}

/// Characters kept as they are in a path segment or query value.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
/// As `COMPONENT`, keeping `/` (a DOI or a repository path).
const PATH: &AsciiSet = &COMPONENT.remove(b'/');

fn encode(value: &str) -> String {
    utf8_percent_encode(value, COMPONENT).to_string()
}
fn encode_path(value: &str) -> String {
    utf8_percent_encode(value, PATH).to_string()
}

/// The contact for APIs that ask for one (`email`, `mailto`), or `None` when it is not set.
fn contact(contact: &str) -> Option<&str> {
    let contact = contact.trim();
    (!contact.is_empty() && contact.contains('@') && !contact.contains(char::is_whitespace))
        .then_some(contact)
}

/// A plain identifier: non-empty, at most `max` bytes, and only the characters `ok` allows.
fn plain(value: &str, max: usize, ok: impl Fn(char) -> bool) -> bool {
    !value.is_empty() && value.len() <= max && value.chars().all(ok)
}

/// An `https` or `http` URL with a host, as given (no rewriting).
fn web_url(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "https" | "http") && url.host_str().is_some()).then_some(url)
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Recorded responses (see `tests/fixtures/lookups/provenance.json`).
    use std::io::Read;

    pub fn read(name: &str) -> Vec<u8> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lookups/");
        let raw = std::fs::read(format!("{dir}{name}")).expect("fixture");
        if name.ends_with(".gz") {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw.as_slice())
                .read_to_end(&mut out)
                .expect("gzip fixture");
            out
        } else {
            raw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Link-derived text may hold any character: no link or id check may slice a string inside
    /// one.
    #[test]
    fn a_multibyte_character_anywhere_in_a_link_never_panics() {
        let links = [
            "https://arxiv.org/abs/1304.1068v2",
            "https://arxiv.org/pdf/2101.00001v1.pdf",
            "http://arxiv.org/abs/hep-th/9901001",
            "https://doi.org/10.48550/arXiv.1304.1068",
            "https://www.alphaxiv.org/abs/1706.03762",
            "https://crates.io/crates/serde_json",
            "https://docs.rs/crate/rand/0.10.3",
            "https://www.npmjs.com/package/@sigstore/sign",
            "https://www.jsdelivr.com/package/npm/vite",
            "https://pypi.org/project/urllib3/",
            "https://github.com/npm/node-semver.git",
            "https://store.steampowered.com/app/620/Portal_2/",
            "https://www.pcgamingwiki.com/api/appid.php?appid=620",
            "https://example.com/",
            "Q7024",
        ];
        for link in links {
            for at in (0..=link.len()).filter(|i| link.is_char_boundary(*i)) {
                for insert in ["é", "€", "😀", "\u{301}"] {
                    let s = format!("{}{insert}{}", &link[..at], &link[at..]);
                    papers::arxiv_from_uri(&s);
                    papers::arxiv_id(&s);
                    packages::Package::from_uri(&s);
                    packages::Repo::from_url(&s);
                    games::appid_from_uri(&s);
                    games::appid(&s);
                    places::wikidata_tag(&s);
                    wikidata::qid(&s);
                    archive::wayback_request(&s, chrono::Utc::now());
                }
            }
        }
    }

    #[test]
    fn search_keys() {
        for (namespace, value, key) in [
            ("osm", "node/240109189", "osm:node/240109189"),
            ("doi", "10.1038/NATURE12373", "doi:10.1038/nature12373"),
            ("arxiv", "1304.1068v2", "arxiv:1304.1068"),
            ("pmcid", "PMC3672233", "pmcid:PMC3672233"),
            ("crate", "Serde_JSON", "crate:serde-json"),
            ("npm", "@sigstore/sign", "npm:@sigstore/sign"),
            ("pypi", "Zope.Interface", "pypi:zope-interface"),
            (
                "repo",
                "github.com/Rust-Random/rand",
                "repo:github.com/rust-random/rand",
            ),
            ("steam", "620", "steam:620"),
            ("fr", "2026-18291", "fr:2026-18291"),
            ("discogs", "master:21491", "discogs:master:21491"),
        ] {
            assert_eq!(
                search_key(namespace, value).as_deref(),
                Some(key),
                "{namespace}:{value}"
            );
        }
        for (namespace, value) in [
            ("osm", "area/1"),
            ("osm", "node/01"),
            ("pmcid", "123"),
            ("steam", "x"),
            ("isbn", "1"),
            ("repo", "example.org/a/b"),
            ("discogs", "label:1"),
            ("discogs", "release:083182"),
        ] {
            assert_eq!(search_key(namespace, value), None, "{namespace}:{value}");
        }
    }

    #[test]
    fn statuses() {
        assert_eq!(body(200, b"x", &[404]).unwrap(), Some(&b"x"[..]));
        assert_eq!(body(404, b"x", &[404]).unwrap(), None);
        assert!(body(429, b"x", &[404]).is_err());
        assert!(body(500, b"x", &[404]).is_err());
        assert!(body(404, b"x", &[]).is_err());
    }

    #[test]
    fn contact_setting() {
        assert_eq!(contact(" info@synonym.to "), Some("info@synonym.to"));
        assert_eq!(contact(""), None);
        assert_eq!(contact("nobody"), None);
        assert_eq!(contact("a b@c"), None);
    }

    #[test]
    fn encoding() {
        assert_eq!(encode("a/b c?d"), "a%2Fb%20c%3Fd");
        assert_eq!(encode_path("10.1000/x<y>"), "10.1000/x%3Cy%3E");
        assert_eq!(encode("Stardew_Valley-1.5~"), "Stardew_Valley-1.5~");
    }

    #[test]
    fn found_deduplicates() {
        let mut f = Found::default();
        f.push_key(Key::new("doi", "10.1/x"));
        f.push_key(Key::new("doi", "10.1/x"));
        f.push_link(Link::new("A", "https://a/", "x"));
        f.push_link(Link::new("B", "https://a/", "y"));
        assert_eq!((f.keys.len(), f.links.len()), (1, 1));
        assert_eq!(f.keys[0].to_string(), "doi:10.1/x");
    }

    #[test]
    fn a_service_card_has_its_plain_name() {
        for (uri, name) in [
            (
                "https://www.openstreetmap.org/node/240109189",
                Some("OpenStreetMap"),
            ),
            ("https://www.openstreetmap.org/about", None),
            ("https://www.wikidata.org/wiki/Q64", Some("Wikidata")),
            (
                "https://play.google.com/store/apps/details?id=io.anuke.mindustry",
                Some("Google Play"),
            ),
            (
                "https://f-droid.org/en/packages/io.anuke.mindustry/",
                Some("F-Droid"),
            ),
            ("https://store.steampowered.com/app/1127400/", Some("Steam")),
            ("https://crates.io/crates/rand", Some("crates.io")),
            ("https://github.com/rust-random/rand", Some("GitHub")),
            ("https://arxiv.org/abs/1706.03762", Some("arXiv")),
            ("doi:10.1038/nature12373", Some("DOI")),
            ("https://doi.org/10.1038/nature12373", Some("DOI")),
            ("https://btcmap.org/merchant/way:259338174", Some("BTC Map")),
            ("https://huggingface.co/models", None),
            ("https://example.org/", None),
        ] {
            assert_eq!(service_name(uri), name, "{uri}");
        }
    }
}
