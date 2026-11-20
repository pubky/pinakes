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

pub mod loopky;
pub mod marketplace;
pub mod posts;
pub mod rooms;

/// Every app profile, one line each, in alphabetical order of module.
#[rustfmt::skip]
pub static REGISTRY: &[&Profile] = &[
    &loopky::PROFILE,
    &marketplace::PROFILE,
    &posts::MAPS,
    &posts::MUSIC,
    &rooms::PROFILE,
];

/// One Pubky app: where its objects live and how each kind becomes a card.
pub struct Profile {
    /// Its namespaces below `/pub/`, as path segments joined by `/`: `loopky`,
    /// `app.marketplace`.
    pub namespaces: &'static [&'static str],
    /// The objects that get a card. Every other path in its namespaces is never fetched.
    pub kinds: &'static [Kind],
    /// The attribute keys its cards publish as `details.facts`.
    pub facts: &'static [&'static str],
    /// Why its objects open in no client yet, while a client link is held back: logged at
    /// startup ([`log_links_off`]) so that switching it on is not forgotten.
    pub links_off: Option<&'static str>,
}
/// Logs each app whose client links are held back, and why: once, at startup.
pub fn log_links_off() {
    for profile in REGISTRY {
        if let Some(why) = profile.links_off {
            tracing::info!(app = profile.namespaces[0], "open_in links are off: {why}");
        }
    }
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
    /// Whether the path is a folder of versions (a post, `{id}/{editId}.json`) read in its
    /// latest one, rather than the object itself.
    pub versions: bool,
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
    /// When the object is read: what its expiry, end or next occurrence is measured against.
    pub now: chrono::DateTime<chrono::Utc>,
    /// The most dates one period of a recurrence rule may hold (`event::time::rrule_bounded`).
    pub max_period_dates: usize,
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
    /// A link the object names (`link`), relative to the object's own folder.
    pub fn link(&self, value: &str) -> Option<String> {
        link(self.uri, value)
    }
}
/// A link an object at `uri` names: `https:`, or `pubky:` in someone's public storage, with no
/// user name, password or port (as `kit::web_url` reads a service's links); absolute, or relative
/// to the object's own folder. Anything else is no link.
fn link(uri: &str, value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Url::parse(uri)
        .ok()?
        .join(value)
        .ok()
        .filter(|u| {
            (u.scheme() == "https" || u.scheme() == "pubky" && u.path().starts_with("/pub/"))
                && u.port().is_none()
                && u.username().is_empty()
                && u.password().is_none()
        })
        .map(String::from)
}

/// Where a `pubky://` object's path leads.
pub enum Route {
    /// A kind a profile reads, with its path's values.
    Kind(&'static Profile, &'static Kind, Vec<(&'static str, String)>),
    /// Any other path in a known app's namespaces, and any path in a namespace Pinakes never
    /// fetches (`NEVER_FETCHED`): never fetched, no card (`no_card`).
    Excluded,
    /// An app no profile knows: the general card.
    General,
}
/// The namespaces whose objects Pinakes never fetches: those Nexus describes (pubky.app's own, and
/// the social specs', `/pub/social/…` in any version, their posts, events and rsvps included:
/// social events are Nexus's), and the old Mapky and Eventky layouts (`mapky.app`,
/// `eventky.app`), whose data is disregarded: Maps writes posts (`posts`), and events and
/// availability are social posts.
const NEVER_FETCHED: &[&str] = &["pubky.app", "social", "mapky.app", "eventky.app"];
/// Whether Pinakes never fetches the objects of this namespace (decoded): ingest drops their post
/// lines, so they create no resource, and one stored before the rule keeps no card (`no_card`).
pub fn never_fetched(namespace: &str) -> bool {
    NEVER_FETCHED.contains(&namespace)
}
/// The route of a `pubky://` object of another app, from its decoded path segments below
/// `/pub/` (`resolver::pubky_app`'s reading). `Excluded` for every path in a namespace Pinakes
/// never fetches (`NEVER_FETCHED`); `None` for any path inside a post of a namespace no profile
/// reads, whose post itself is `posts::ANY`'s.
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
    if segments.first().is_some_and(|app| never_fetched(app)) {
        return Some(Route::Excluded);
    }
    // A post in a namespace no profile reads (`/pub/<ns>/v1/posts/{id}`) has the general card,
    // with its namespace and kind; a path inside one (a version
    // file) is the resolver's to fetch, never an object of its own.
    if matches!(segments, [_, version, posts, ..] if version == "v1" && posts == "posts") {
        let any = &posts::ANY.kinds[0];
        return matches(any.path, segments).map(|vars| Route::Kind(&posts::ANY, any, vars));
    }
    match segments.first() {
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

/// The card of an object fetched at `now`: its kind's, or the general card.
pub fn card_at(
    uri: &str,
    content_type: &str,
    body: &[u8],
    ttl: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> ResourceMeta {
    card_with(
        uri,
        content_type,
        body,
        ttl,
        now,
        crate::event::time::MAX_PERIOD_DATES,
    )
}
/// The card of an object fetched at `now`, its recurrence rules bounded by `max_period_dates`
/// (`limits.event_max_period_dates`).
pub fn card_with(
    uri: &str,
    content_type: &str,
    body: &[u8],
    ttl: u64,
    now: chrono::DateTime<chrono::Utc>,
    max_period_dates: usize,
) -> ResourceMeta {
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
                now,
                max_period_dates,
                vars,
            };
            let read = match json.as_ref().filter(|v| v.is_object()) {
                Some(value) => (kind.read)(&object, value, &mut doc),
                None => Read::NoCard,
            };
            if read == Read::NoCard {
                // Kept as an object without a card, checked again as missing ones are.
                let mut empty = ResourceMeta::new(uri, "pubky", ttl);
                empty.set_type("CreativeWork");
                empty.site = doc.site;
                empty.status = Status::Missing;
                empty.withheld = true;
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
/// image; a text-only object is a post. A field no stored text may hold (a NUL) or longer than
/// its cap is left out, not the card.
pub fn general(uri: &str, v: &Value, doc: &mut ResourceMeta) {
    let text = |keys: &[&str], max: usize| {
        keys.iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.chars().count() <= max && !s.contains('\0'))
            .map(str::to_owned)
    };
    doc.name = text(&["name", "title", "summary", "topic", "caption"], 200);
    doc.description = text(&["description", "bio"], 2000);
    doc.image = text(
        &["image", "image_uri", "imageUrl", "cover", "avatarUrl"],
        2048,
    )
    .and_then(|s| link(uri, &s))
    .filter(|l| l.len() <= 2048);
    // An object without a title or description (a post, a note) is described by its content:
    // social content (`cards::social`), so a short excerpt that refreshes within the hour and is
    // purged with its history once the object is gone.
    if doc.name.is_none() && doc.description.is_none() {
        doc.description =
            text(&["content", "text"], usize::MAX).map(|s| s.chars().take(200).collect());
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
/// The client links of an object by its URI alone: what a card shows before its object is read.
pub fn open_in(uri: &str) -> Vec<OpenIn> {
    let Some((owner, segments)) = segments(uri) else {
        return vec![];
    };
    match route(&segments) {
        Some(Route::Kind(_, kind, vars)) => fill(
            &Object {
                uri,
                owner: &owner,
                now: chrono::Utc::now(),
                max_period_dates: crate::event::time::MAX_PERIOD_DATES,
                vars,
            },
            kind.open_in,
        ),
        _ => vec![],
    }
}
/// Whether a `pubky://` object is in a known app's namespace, of a kind its profile reads or not,
/// or in a namespace Pinakes never fetches.
pub fn known(uri: &str) -> bool {
    segments(uri).is_some_and(|(_, s)| matches!(route(&s), Some(Route::Kind(..) | Route::Excluded)))
}
/// Whether a `pubky://` object is a folder of versions (`Kind::versions`).
pub fn versioned(uri: &str) -> bool {
    segments(uri)
        .is_some_and(|(_, s)| matches!(route(&s), Some(Route::Kind(_, kind, _)) if kind.versions))
}
/// Whether a `pubky://` object is never fetched: in a known app's namespace but of no kind its
/// profile reads, or in a namespace Pinakes never fetches.
pub fn excluded(uri: &str) -> bool {
    segments(uri).is_some_and(|(_, s)| matches!(route(&s), Some(Route::Excluded)))
}
/// The card of an object that is never fetched (`excluded`): a missing `CreativeWork` with
/// nothing of the object, confirmed at once (`withheld`).
pub fn no_card(uri: &str, ttl_secs: u64) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, "pubky", ttl_secs);
    doc.set_type("CreativeWork");
    doc.status = Status::Missing;
    doc.withheld = true;
    doc
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

    /// The first of `keys` that holds a non-empty string, trimmed, without the NUL characters
    /// no stored text may hold.
    pub fn text(v: &Value, keys: &[&str]) -> Option<String> {
        keys.iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .map(|s| s.replace('\0', ""))
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
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
}
