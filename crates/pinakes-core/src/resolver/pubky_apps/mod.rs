//! Cards for the public objects of Pubky apps other than pubky.app (`docs/pubky-apps.md`).
//!
//! An app is one module, `pubky_apps/<app>.rs`, whose `PROFILE` names the app's namespaces under
//! `/pub/`, the kinds of object it publishes (a path pattern, how to read one, the clients that
//! open it) and the facts its cards publish; it is listed once in [`REGISTRY`], and its recorded
//! objects are in `tests/fixtures/pubky_apps/<app>/`. In a known app's namespaces only those kinds
//! are fetched: every other path (user state, private-by-intent data, files, tags) never is. An
//! app no profile knows gets the general card ([`general`]).
use crate::model::{OpenIn, ResourceMeta, Status};
use serde_json::Value;
use url::Url;

pub mod eventky;
pub mod loopky;
pub mod mapky;
pub mod marketplace;
pub mod rooms;

/// Every app profile, one line each, in alphabetical order of module.
#[rustfmt::skip]
pub static REGISTRY: &[&Profile] = &[
    &eventky::PROFILE,
    &loopky::PROFILE,
    &mapky::PROFILE,
    &marketplace::PROFILE,
    &rooms::PROFILE,
];

/// One Pubky app: where its objects live and how each kind becomes a card.
pub struct Profile {
    /// Its namespaces below `/pub/`, as path segments joined by `/`: `loopky`,
    /// `pubky.app/marketplace/v1`.
    pub namespaces: &'static [&'static str],
    /// The objects that get a card. Every other path in its namespaces is never fetched.
    pub kinds: &'static [Kind],
    /// The attribute keys its cards publish as `details.facts`.
    pub facts: &'static [&'static str],
}
/// One kind of object an app publishes.
pub struct Kind {
    /// The path below a namespace: literal segments, and `{name}` for any one segment.
    pub path: &'static str,
    /// Fills the card from the object's JSON. [`Read::NoCard`] when its content keeps it from
    /// having one (unlisted, withdrawn, expired, not the app's format).
    pub read: fn(&Object, &Value, &mut ResourceMeta) -> Read,
    /// The clients that open it: `url` is a template filled with `{owner}` and the path's names.
    pub open_in: &'static [Client],
}
pub struct Client {
    pub client: &'static str,
    pub url: &'static str,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Read {
    Card,
    NoCard,
}
/// One object of a known kind: its owner, URI and the values of its path's `{name}` segments.
pub struct Object<'a> {
    pub uri: &'a str,
    pub owner: &'a str,
    vars: Vec<(&'static str, String)>,
}
impl Object<'_> {
    /// The value of the path's `{name}` segment.
    pub fn var(&self, name: &str) -> &str {
        self.vars
            .iter()
            .find(|(n, _)| *n == name)
            .map_or("", |(_, v)| v.as_str())
    }
    /// A link the object names: absolute (`https:`, or `pubky:` in someone's public storage), or
    /// relative to the object's own folder.
    pub fn link(&self, value: &str) -> Option<String> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        Url::parse(self.uri)
            .ok()?
            .join(value)
            .ok()
            .filter(|u| {
                u.scheme() == "https" || u.scheme() == "pubky" && u.path().starts_with("/pub/")
            })
            .map(String::from)
    }
}

/// Where a `pubky://` object's path leads.
pub enum Route {
    /// A kind a profile reads, with its path's values.
    Kind(&'static Profile, &'static Kind, Vec<(&'static str, String)>),
    /// Any other path in a known app's namespaces: never fetched.
    Excluded,
    /// An app no profile knows: the general card.
    General,
}
/// The route of a `pubky://` object of another app, from its decoded path segments below
/// `/pub/` (`resolver::pubky_app`'s reading). `None` for pubky.app's own objects, which Nexus
/// describes, apart from its marketplace (a profile).
pub fn route(segments: &[String]) -> Option<Route> {
    for profile in REGISTRY {
        for namespace in profile.namespaces {
            let prefix: Vec<&str> = namespace.split('/').collect();
            if segments.len() < prefix.len() || !segments.iter().zip(&prefix).all(|(s, p)| s == p) {
                continue;
            }
            let rest = &segments[prefix.len()..];
            for kind in profile.kinds {
                if let Some(vars) = matches(kind.path, rest) {
                    return Some(Route::Kind(profile, kind, vars));
                }
            }
            return Some(Route::Excluded);
        }
    }
    match segments.first() {
        Some(app) if app == "pubky.app" => None,
        Some(app) if !app.is_empty() => Some(Route::General),
        _ => None,
    }
}
fn matches(pattern: &'static str, rest: &[String]) -> Option<Vec<(&'static str, String)>> {
    let parts: Vec<&'static str> = pattern.split('/').collect();
    if parts.len() != rest.len() {
        return None;
    }
    let mut vars = vec![];
    for (part, segment) in parts.into_iter().zip(rest) {
        match part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
            Some(name) if !segment.is_empty() => vars.push((name, segment.clone())),
            Some(_) => return None,
            None if part == segment => {}
            None => return None,
        }
    }
    Some(vars)
}
/// The decoded path segments of a `pubky://` object below `/pub/`.
fn segments(uri: &str) -> Option<(String, Vec<String>)> {
    let url = Url::parse(uri).ok()?;
    let owner = url.host_str()?.to_owned();
    let mut all = crate::resolver::path_segments(&url)?;
    (all.first().map(String::as_str) == Some("pub")).then(|| (owner, all.split_off(1)))
}

/// The card of a fetched object: its kind's, or the general card.
pub fn card(uri: &str, content_type: &str, body: &[u8], ttl: u64) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, "pubky", ttl);
    doc.set_type("CreativeWork");
    doc.attributes
        .insert("content_type".into(), content_type.into());
    doc.attributes
        .insert("content_length".into(), body.len().to_string());
    if let Some(app) = crate::api::site(uri, None, None) {
        doc.site = app;
    }
    let json = serde_json::from_slice::<Value>(body).ok();
    let Some((owner, segments)) = segments(uri) else {
        return doc;
    };
    match route(&segments) {
        Some(Route::Kind(_, kind, vars)) => {
            let object = Object {
                uri,
                owner: &owner,
                vars,
            };
            let read = match json.as_ref().filter(|v| v.is_object()) {
                Some(value) => (kind.read)(&object, value, &mut doc),
                None => Read::NoCard,
            };
            if read == Read::NoCard {
                // Kept as an object without a card, checked again as missing ones are.
                let mut empty = ResourceMeta::new(uri, "pubky", ttl);
                empty.site = doc.site;
                empty.status = Status::Missing;
                return empty;
            }
            let mut links = fill(&object, kind.open_in);
            links.append(&mut doc.open_in);
            doc.open_in = links;
        }
        _ => {
            if let Some(value) = &json {
                general(uri, value, &mut doc);
            }
        }
    }
    doc
}
/// The general card of an app no profile knows: the common names for a title, a text and an
/// image; a text-only object is a post.
pub fn general(uri: &str, v: &Value, doc: &mut ResourceMeta) {
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    doc.name = text(&["name", "title", "summary", "topic", "caption"]);
    doc.description = text(&["description", "bio"]);
    doc.image = text(&["image", "image_uri", "imageUrl", "cover", "avatarUrl"])
        .and_then(|s| Url::parse(uri).ok()?.join(&s).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https" | "pubky"))
        .map(String::from);
    // An object without a title or description (a post, a note) is described by its content:
    // social content (`cards::social`), so a short excerpt that refreshes within the hour and is
    // purged with its history once the object is gone.
    if doc.name.is_none() && doc.description.is_none() {
        doc.description = text(&["content", "text"]).map(|s| s.chars().take(200).collect());
        if doc.description.is_some() {
            doc.set_type("SocialMediaPosting");
        }
    }
}
/// The client links of a kind, filled from the object's owner and path.
fn fill(object: &Object, clients: &[Client]) -> Vec<OpenIn> {
    clients
        .iter()
        .filter_map(|c| {
            let mut url = c.url.to_owned();
            while let Some(start) = url.find('{') {
                let end = start + url[start..].find('}')?;
                let name = &url[start + 1..end];
                let value = if name == "owner" {
                    object.owner
                } else {
                    object.var(name)
                };
                if value.is_empty() {
                    return None;
                }
                url.replace_range(start..=end, &encode(value));
            }
            Some(OpenIn {
                client: c.client.into(),
                url,
            })
        })
        .collect()
}
/// A value for a client link's path or query: everything but letters, digits and `-._~`
/// percent-encoded.
pub(crate) fn encode(value: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, SET).collect()
}
/// The client links of an object by its URI alone: what a card shows before its object is read,
/// and on an event card for the event object it names.
pub fn open_in(uri: &str) -> Vec<OpenIn> {
    let Some((owner, segments)) = segments(uri) else {
        return vec![];
    };
    match route(&segments) {
        Some(Route::Kind(_, kind, vars)) => fill(
            &Object {
                uri,
                owner: &owner,
                vars,
            },
            kind.open_in,
        ),
        _ => vec![],
    }
}
/// The profile whose namespace holds a `pubky://` object.
pub fn profile(uri: &str) -> Option<&'static Profile> {
    let (_, segments) = segments(uri)?;
    match route(&segments)? {
        Route::Kind(profile, _, _) => Some(profile),
        _ => None,
    }
}
/// Every attribute a `pubky` card may store: the content type and length, and every profile's
/// facts.
pub fn attributes() -> &'static [&'static str] {
    static ALL: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut all: Vec<&str> = crate::resolver::pubky::ATTRIBUTES.to_vec();
        for key in REGISTRY.iter().flat_map(|p| p.facts.iter().copied()) {
            if !all.contains(&key) {
                all.push(key);
            }
        }
        all
    })
}
/// `details.facts` of a `pubky` card: its profile's facts.
pub fn facts(doc: &ResourceMeta) -> Option<std::collections::BTreeMap<String, String>> {
    if doc.source != "pubky" {
        return None;
    }
    let profile = profile(&doc.url)?;
    let facts: std::collections::BTreeMap<String, String> = doc
        .attributes
        .iter()
        .filter(|(k, v)| profile.facts.contains(&k.as_str()) && !v.trim().is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!facts.is_empty()).then_some(facts)
}

/// Helpers for the profiles' readers.
pub mod read {
    use serde_json::Value;

    /// The first of `keys` that holds a non-empty string, trimmed.
    pub fn text(v: &Value, keys: &[&str]) -> Option<String> {
        keys.iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
    /// A number as a fact: an integer without a fraction, else as written.
    pub fn number(v: &Value) -> Option<String> {
        let n = v.as_f64().filter(|n| n.is_finite())?;
        Some(if n.fract() == 0.0 && n.abs() < 1e15 {
            format!("{}", n as i64)
        } else {
            n.to_string()
        })
    }
    /// A point from `lat` and `lon` members, when both are valid coordinates.
    pub fn point(v: &Value) -> Option<crate::model::Point> {
        let (lat, lon) = (v.get("lat")?.as_f64()?, v.get("lon")?.as_f64()?);
        ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
            .then_some(crate::model::Point { lat, lon })
    }
    /// An amount of money from its minor units and exponent: `{100, "USD", 2}` is `1.00 USD`.
    pub fn money(v: &Value) -> Option<String> {
        let amount = v.get("amountMinor")?.as_i64()?;
        let currency = v.get("currency")?.as_str()?;
        let exponent = v.get("exponent")?.as_u64().filter(|e| *e <= 18)? as u32;
        if currency.is_empty()
            || currency.len() > 12
            || !currency.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return None;
        }
        let sign = if amount < 0 { "-" } else { "" };
        let digits = amount.unsigned_abs().to_string();
        let width = exponent as usize + 1;
        let digits = format!("{digits:0>width$}");
        let (whole, fraction) = digits.split_at(digits.len() - exponent as usize);
        Some(if fraction.is_empty() {
            format!("{sign}{whole} {currency}")
        } else {
            format!("{sign}{whole}.{fraction} {currency}")
        })
    }
    /// An OpenStreetMap element from its canonical URL, `https://www.openstreetmap.org/node/1`.
    pub fn osm_element(url: &str) -> Option<(&'static str, u64)> {
        let path = url.strip_prefix("https://www.openstreetmap.org/")?;
        let (kind, id) = path.split_once('/')?;
        let kind = ["node", "way", "relation"]
            .into_iter()
            .find(|k| *k == kind)?;
        let id = id.parse::<u64>().ok().filter(|id| *id > 0)?;
        Some((kind, id))
    }
    /// Unix time in microseconds, as a time.
    pub fn micros(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::from_timestamp_micros(v.as_i64()?)
    }
}
