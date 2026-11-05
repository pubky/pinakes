//! Links that carry a credential: a calendar provider's secret share link, or a query parameter
//! that names a token, key or signature. Pinakes never previews one on request and never passes
//! one to another service (an archive, an oEmbed provider, a hub); a link someone posted is still
//! read from its own site. The provider patterns are those `scripts/verify-no-capability-urls`
//! keeps out of the repository; `tests/fixtures/capability_urls.json` holds the probes both check.
use url::Url;

/// Query keys that carry a credential, compared after percent-decoding and without case.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "token",
    "access_token",
    "auth",
    "key",
    "api_key",
    "apikey",
    "secret",
    "sig",
    "signature",
    "password",
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "x-goog-signature",
    "x-goog-credential",
];

/// Whether `uri` carries a credential ([`is_capability_url`]); a URI that is not a URL does not.
pub fn is_capability(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|url| is_capability_url(&url))
}

/// Whether `url` carries a credential: a password in its user info, a calendar provider's secret
/// share path, or a key in [`CREDENTIAL_KEYS`] in its query, or with a value in its form-encoded
/// fragment or in any part of it after a `?`, `#` or `;` (an OAuth callback's `#access_token=…`,
/// a hash router's `#/reset?token=…` or `#/reset;token=…`, an OAuth answer after a route,
/// `#/cb#access_token=…`; a plain anchor such as `#key` is a place in the page).
pub fn is_capability_url(url: &Url) -> bool {
    if url.password().is_some() {
        return true;
    }
    if !matches!(url.scheme(), "http" | "https" | "webcal" | "webcals") {
        return false;
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    // `/%6Fwa/calendar/` is `/owa/calendar/` to the provider.
    let path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8_lossy()
        .to_ascii_lowercase();
    let credential = |key: &str| CREDENTIAL_KEYS.contains(&key.to_ascii_lowercase().as_str());
    secret_share(&host, &path)
        || url.query_pairs().any(|(key, _)| credential(&key))
        || url.fragment().is_some_and(|fragment| {
            // The fragment as a form, and each part of it after a `?`, `#` or `;`.
            std::iter::once(fragment)
                .chain(
                    fragment
                        .match_indices(&['?', '#', ';'][..])
                        .map(|(at, _)| &fragment[at + 1..]),
                )
                .any(|form| {
                    url::form_urlencoded::parse(form.as_bytes())
                        .any(|(key, value)| !value.is_empty() && credential(&key))
                })
        })
}

/// Drops from `value`, a document as it is stored, every URL that carries a credential but `own`
/// (the link the resource is read by, in any spelling of it): the backstop before anything is
/// stored, whichever path put it there. The URL goes, or else the list entry or object around it
/// when a `T` would not read back without the URL; where a `T` cannot do without it, it names `own`
/// instead. Whether anything went.
pub fn strip<T: serde::de::DeserializeOwned>(value: &mut serde_json::Value, own: &str) -> bool {
    let own_id = crate::model::identity(own).ok().map(|(id, _, _)| id);
    let foreign = |s: &str| {
        s != own
            && s.contains("://")
            && Url::parse(s).is_ok_and(|url| url.has_host() && is_capability_url(&url))
            && crate::model::identity(s).ok().map(|(id, _, _)| id) != own_id
    };
    let mut stripped = false;
    while let Some(path) = find(value, &foreign) {
        stripped = true;
        let kept = (1..=path.len()).rev().find_map(|depth| {
            let mut candidate = value.clone();
            remove(&mut candidate, &path[..depth]);
            serde_json::from_value::<T>(candidate.clone())
                .is_ok()
                .then_some(candidate)
        });
        match kept {
            Some(candidate) => *value = candidate,
            None => {
                if let Some(at) = path.iter().try_fold(&mut *value, |v, step| match step {
                    Step::Key(key) => v.get_mut(key.as_str()),
                    Step::Index(i) => v.get_mut(*i),
                }) {
                    *at = own.into();
                }
            }
        }
    }
    stripped
}

/// A step into a JSON value.
enum Step {
    Key(String),
    Index(usize),
}

/// The path to the first string in `value` that `matches`.
fn find(value: &serde_json::Value, matches: &impl Fn(&str) -> bool) -> Option<Vec<Step>> {
    use serde_json::Value;
    match value {
        Value::String(s) => matches(s).then(Vec::new),
        Value::Array(items) => items.iter().enumerate().find_map(|(i, item)| {
            find(item, matches).map(|mut path| {
                path.insert(0, Step::Index(i));
                path
            })
        }),
        Value::Object(fields) => fields.iter().find_map(|(key, field)| {
            find(field, matches).map(|mut path| {
                path.insert(0, Step::Key(key.clone()));
                path
            })
        }),
        _ => None,
    }
}

/// Remove what `path` leads to from the object or list that holds it.
fn remove(value: &mut serde_json::Value, path: &[Step]) {
    let Some((last, parent)) = path.split_last() else {
        return;
    };
    let parent = parent.iter().try_fold(value, |v, step| match step {
        Step::Key(key) => v.get_mut(key.as_str()),
        Step::Index(i) => v.get_mut(*i),
    });
    match (parent, last) {
        (Some(serde_json::Value::Object(fields)), Step::Key(key)) => {
            fields.remove(key);
        }
        (Some(serde_json::Value::Array(items)), Step::Index(i)) if *i < items.len() => {
            items.remove(*i);
        }
        _ => {}
    }
}

/// Whether `url` carries a credential, or names a link that does in one of its query values (an
/// oEmbed endpoint's `url=`).
pub fn names_capability(url: &Url) -> bool {
    is_capability_url(url)
        || url
            .query_pairs()
            .any(|(_, value)| Url::parse(&value).is_ok_and(|named| is_capability_url(&named)))
}

/// A calendar provider's share path whose own segment is the secret. Nextcloud's may sit below a
/// prefix (`/index.php/apps/…`, an install in a sub-path).
fn secret_share(host: &str, path: &str) -> bool {
    let on = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    // What follows `marker` in `path`: the secret, when not empty.
    let after = |marker: &str| {
        path.find(marker)
            .is_some_and(|at| !path[at + marker.len()..].is_empty())
    };
    ((on("outlook.office365.com") || on("outlook.live.com"))
        && (path.starts_with("/owa/calendar/") || path.starts_with("/owa/0/calendar/")))
        || (on("icloud.com") && path.contains("/published/"))
        || ((host == "calendar.google.com" || host == "www.google.com")
            && path.starts_with("/calendar/ical/")
            && path.contains("/private-"))
        || after("/remote.php/dav/public-calendars/")
        || after("/apps/calendar/p/")
        || after("/apps/calendar/embed/")
        || (on("proton.me") && path.starts_with("/api/calendar/v1/url/"))
        || (host == "calendar.proton.me" && path.starts_with("/share/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential key in a form-encoded fragment (an OAuth callback's `#access_token=`) counts too.
    #[test]
    fn a_credential_key_in_the_fragment_is_one_too() {
        assert!(is_capability(
            "https://example.org/cb#access_token=x&state=y"
        ));
        assert!(is_capability("https://example.org/cb#Token=x"));
        assert!(!is_capability("https://example.org/page#section-token"));
        assert!(!is_capability("https://example.org/page#t=10"));
        // A hash router's route keeps its own query after the `?`.
        assert!(is_capability("https://app.example/#/reset?token=x"));
        assert!(!is_capability("https://app.example/#/reset?step=2"));
        // An OAuth answer after a route, and a route's matrix parameters.
        assert!(is_capability("https://app.example/#/cb#access_token=x"));
        assert!(is_capability("https://app.example/#/reset;token=x"));
        assert!(!is_capability("https://app.example/#/reset;step=2"));
        // A plain anchor is a place in the page, not a credential.
        for anchor in ["#Key", "#token", "#auth", "#key="] {
            let url = format!("https://en.wikipedia.org/wiki/Lock_and_key{anchor}");
            assert!(!is_capability(&url), "{url}");
        }
    }

    #[test]
    fn a_credential_key_is_matched_whole_after_decoding() {
        for flagged in [
            "https://example.org/feed.ics?TOKEN=x",
            "https://example.org/feed.ics?a=1&api%5Fkey=x",
            "https://example.org/feed.ics?X-Amz-Signature=x",
            "webcal://example.org/feed.ics?sig=x",
            "https://user:pass@example.org/feed.ics",
        ] {
            assert!(is_capability(flagged), "{flagged}");
        }
        for plain in [
            "https://example.org/feed.ics?key_art=1",
            "https://example.org/feed.ics?monkey=1",
            "https://example.org/token/feed.ics",
            "https://example.org/feed.ics?q=token",
            "https://user@example.org/feed.ics",
            "https://example.org/apps/calendar/p/",
            "nostr:npub1example",
            "not a url",
        ] {
            assert!(!is_capability(plain), "{plain}");
        }
    }
}
