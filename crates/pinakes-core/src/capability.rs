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
    "id_token",
    "refresh_token",
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
/// share path, or a key in [`CREDENTIAL_KEYS`] in its query (its parameters separated by `&` or
/// `;`, as in `?x=1;token=…`), or with a value in its form-encoded
/// fragment or in any part of it after a `?`, `#` or `;` (an OAuth callback's `#access_token=…`,
/// a hash router's `#/reset?token=…` or `#/reset;token=…`, an OAuth answer after a route,
/// `#/cb#access_token=…`; a key may start with an old hash router's `/` or `!/`, as in
/// `#/access_token=…`; a plain anchor such as `#key` is a place in the page).
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
        || url.query().is_some_and(|query| {
            query.split(';').any(|form| {
                url::form_urlencoded::parse(form.as_bytes()).any(|(key, _)| credential(&key))
            })
        })
        || url
            .fragment()
            .is_some_and(|fragment| fragment_credential(fragment, &credential))
}

/// Whether `fragment`, as a form and in each part of it after a `?`, `#` or `;`, has a key that
/// `credential` names with a value. The fragment is read once, in the parts between those and
/// `&`: a part's key ends at its `=`, is decoded (`%2F`, `+`) and may then start with a hash
/// router's `/` or `!`; it has a value when something other than `&` follows the `=`. A key never
/// holds one of those separators, so no credential key is missed.
fn fragment_credential(fragment: &str, credential: &impl Fn(&str) -> bool) -> bool {
    let bytes = fragment.as_bytes();
    let mut start = 0;
    while start <= bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|b| matches!(b, b'?' | b'#' | b';' | b'&'))
            .map_or(bytes.len(), |at| start + at);
        if let Some(equals) = bytes[start..end].iter().position(|&b| b == b'=') {
            let has_value = bytes.get(start + equals + 1).is_some_and(|&b| b != b'&');
            let key = percent_encoding::percent_decode_str(
                &fragment[start..start + equals].replace('+', " "),
            )
            .decode_utf8_lossy()
            .into_owned();
            if has_value && credential(key.trim_start_matches(&['/', '!'][..])) {
                return true;
            }
        }
        start = end + 1;
    }
    false
}

/// Drops from `value`, a document as it is stored, every URL that carries a credential but `own`,
/// the link the resource is read by, as written (in any spelling of it only when it carries a
/// credential itself: another spelling of a public link may add one, in its fragment or user
/// info): the backstop at each write, whichever path put a URL there. The URL goes, or else the
/// list entry or object around it when a `T` would not read back without the URL; where a `T`
/// cannot do without it, it names `own` instead. Whether anything went.
pub fn strip<T: serde::de::DeserializeOwned>(value: &mut serde_json::Value, own: &str) -> bool {
    let own_id = Url::parse(own)
        .is_ok_and(|url| is_capability_url(&url))
        .then(|| crate::model::identity(own).ok().map(|(id, _, _)| id))
        .flatten();
    let foreign = |s: &str| {
        s != own
            && names_credential(s)
            && (own_id.is_none() || crate::model::identity(s).ok().map(|(id, _, _)| id) != own_id)
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

/// [`strip`] over what is stored of `doc`, and over its episodes, stored beside it: the last step
/// before a document is written. What is never stored (its container, its alias candidates and
/// the like) stays as it was.
pub fn strip_document(doc: &mut crate::model::ResourceMeta, own: &str) -> anyhow::Result<()> {
    if let Some(episodes) = &mut doc.episodes {
        let mut value = serde_json::to_value(&*episodes)?;
        if strip::<Vec<crate::episodes::Episode>>(&mut value, own) {
            *episodes = serde_json::from_value(value)?;
        }
    }
    let mut value = serde_json::to_value(&*doc)?;
    if strip::<crate::model::ResourceMeta>(&mut value, own) {
        let stored: crate::model::ResourceMeta = serde_json::from_value(value)?;
        *doc = crate::model::ResourceMeta {
            container: doc.container.take(),
            alias_candidates: std::mem::take(&mut doc.alias_candidates),
            http_status: doc.http_status,
            withheld: doc.withheld,
            refresh_by: doc.refresh_by,
            source_root: doc.source_root.take(),
            episodes: doc.episodes.take(),
            ..stored
        };
    }
    Ok(())
}

/// Whether `s`, a string as a document or a row stores it, is a link that carries a credential:
/// read as written, a scheme-relative one (`//host/p`) on `https:`, and a `webcal:` one written
/// without its `//`. What a URL parser drops (control characters and spaces around it, a tab or
/// newline anywhere, in the scheme too) is dropped first.
pub fn names_credential(s: &str) -> bool {
    let s: String = s
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let s = s.as_str();
    let url = match s.strip_prefix("//") {
        Some(rest) => Url::parse(&format!("https://{rest}")),
        None => Url::parse(s).and_then(|url| match url.scheme() {
            "webcal" | "webcals" if !url.has_host() => Url::parse(&format!(
                "{}://{}",
                url.scheme(),
                &s[url.scheme().len() + 1..]
            )),
            _ => Ok(url),
        }),
    };
    url.is_ok_and(|url| url.has_host() && is_capability_url(&url))
}

/// The most link starts [`contains_credential`] reads in one string; a string with more is taken
/// to hold a credential (its UID gets a stand-in), so a hostile one costs no more than that.
const LINK_STARTS: usize = 16;

/// Whether `s` holds a link that carries a credential ([`names_credential`]): `s` as a whole, or
/// a link that starts inside it at `//` or an `http`, `https`, `webcal` or `webcals` scheme after a
/// character no scheme holds (a Nostr calendar's `kind:author:https://…`, a name such as
/// `Meetup https://…`), up to a space.
pub fn contains_credential(s: &str) -> bool {
    if names_credential(s) {
        return true;
    }
    let s: String = s
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let scheme = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.' | b'/');
    let mut read = 0;
    (0..bytes.len())
        .filter(|&at| {
            ["//", "http:", "https:", "webcal:", "webcals:"]
                .iter()
                .any(|start| bytes[at..].starts_with(start.as_bytes()))
                && (at == 0 || !scheme(bytes[at - 1]))
        })
        .any(|at| {
            read += 1;
            read > LINK_STARTS || names_credential(s[at..].split(' ').next().unwrap_or_default())
        })
}

/// Whether an HTTP `Link` header (`<…>; rel=…, <…>`) names a link that carries a credential; a
/// relative one is read against `base`.
pub fn link_header_names_capability(header: &str, base: &str) -> bool {
    let base = Url::parse(base).ok();
    header
        .split('<')
        .skip(1)
        .filter_map(|part| part.split_once('>').map(|(target, _)| target))
        .any(|target| {
            match &base {
                Some(base) => base.join(target),
                None => Url::parse(target),
            }
            .is_ok_and(|url| is_capability_url(&url))
        })
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
        // An OAuth answer read by an old hash router: its keys start with the route's `/` or `#!/`.
        assert!(is_capability(
            "https://app.example/#/access_token=x&token_type=Bearer"
        ));
        assert!(is_capability("https://app.example/#!/access_token=x"));
        assert!(!is_capability("https://app.example/#/home"));
        assert!(!is_capability("https://app.example/#!/home"));
        // OpenID Connect's sign-in answer and a refresh token.
        assert!(is_capability("https://app.example/cb#id_token=x"));
        assert!(is_capability("https://app.example/cb?refresh_token=x"));
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

    /// The store guard keeps only the resource's own link as it is written: another spelling of it
    /// that carries a credential (a fragment, a password) goes, and so does a URL written without
    /// its `//`.
    #[test]
    fn the_store_guard_keeps_only_the_own_link_as_written() {
        let mut value = serde_json::json!({
            "url": "https://s.example/p",
            "image": "https://s.example/p#access_token=SECRET",
            "logo": "https://u:SECRET@s.example/p",
            "author": "https:cdn.example/profile?token=SECRET",
            "kept": "https://s.example/p",
        });
        assert!(strip::<serde_json::Value>(
            &mut value,
            "https://s.example/p"
        ));
        assert_eq!(
            value,
            serde_json::json!({"url": "https://s.example/p", "kept": "https://s.example/p"})
        );
        // An own link that carries a credential is kept in any spelling of it.
        let own = "https://cal.example/feed.ics?token=OWN";
        let mut value =
            serde_json::json!({"url": own, "feed": "https://CAL.example/feed.ics?token=OWN"});
        assert!(!strip::<serde_json::Value>(&mut value, own));
    }
    /// A query read as `;`-separated parameters counts too (`?x=1;token=…`), and so do a
    /// scheme-relative link and a `webcal:` one written without `//` where a document stores them.
    #[test]
    fn semicolon_queries_and_links_without_a_scheme_or_slashes_count() {
        assert!(is_capability(
            "https://events.example/feed?x=1;token=SECRET"
        ));
        assert!(is_capability(
            "https://events.example/feed?Token=SECRET;x=1"
        ));
        assert!(!is_capability("https://events.example/feed?x=1;y=2"));
        let own = "https://own.example/p";
        for stored in [
            "//cdn.example/x?token=SECRET",
            "webcal:cal.example/feed.ics?token=SECRET",
        ] {
            let mut value = serde_json::json!({"url": own, "website": stored});
            assert!(strip::<serde_json::Value>(&mut value, own), "{stored}");
            assert!(!value.to_string().contains("SECRET"), "{value}");
        }
        let mut value = serde_json::json!({"url": own, "website": "//cdn.example/x?size=2"});
        assert!(!strip::<serde_json::Value>(&mut value, own));
    }
    /// A tab or newline a URL parser drops (inside the scheme, too) does not hide a link.
    #[test]
    fn a_tab_or_newline_inside_a_link_hides_nothing() {
        for stored in [
            "web\tcal:cal.example/feed.ics?token=SECRET",
            "webcal:\ncal.example/feed.ics?token=SECRET",
            "\u{1}https://cdn.example/x?token=SECRET",
        ] {
            assert!(names_credential(stored), "{stored:?}");
        }
        assert!(!names_credential("web\tcal:cal.example/feed.ics?x=1"));
    }
    /// A link that carries a credential counts wherever it starts in a string: a Nostr calendar
    /// UID (`kind:author:d-tag`) or a name made of a word and an id.
    #[test]
    fn a_link_inside_a_string_counts_wherever_it_starts() {
        for s in [
            "31923:abc123:https://cal.example/item?token=SECRET",
            "Meetup https://cal.example/e?token=SECRET",
            "31923:abc123://cal.example/item?token=SECRET",
        ] {
            assert!(contains_credential(s), "{s}");
        }
        for s in ["31923:abc123:plain-id", "event-7@cal.example", "Meetup 7"] {
            assert!(!contains_credential(s), "{s}");
        }
    }
    /// A link with a space in its path still counts as a whole string.
    #[test]
    fn a_spaced_link_counts_as_a_whole() {
        assert!(contains_credential(
            "https://cal.example/Team Calendar/e?token=SECRET"
        ));
    }
    /// A long hostile string (many link starts, a long fragment) is read in bounded time.
    #[test]
    fn hostile_strings_are_read_quickly() {
        let started = std::time::Instant::now();
        let many_starts = format!("{}x?token=SECRET", "://".repeat(21_000));
        assert!(contains_credential(&many_starts));
        let long_fragment = format!("https://h.example/#{}", "?".repeat(60_000));
        assert!(!contains_credential(&long_fragment));
        assert!(!is_capability(&long_fragment));
        // Sixteen link starts, each with a fragment of separators before an `=`.
        let separators = format!(
            "{}{}",
            "https://h.example/#?".repeat(8),
            format!("{}=x", "?".repeat(62)).repeat(15)
        );
        for _ in 0..1000 {
            assert!(!contains_credential(&separators));
        }
        // Linear: about a second in a test build, against tens of seconds for a scan per suffix.
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    }
    /// A fragment key's percent-encoded `/` or `!` prefix (`#%2Faccess_token=…`) is trimmed as a
    /// written one is.
    #[test]
    fn an_encoded_router_prefix_hides_no_fragment_key() {
        assert!(is_capability(
            "https://example.org/cb#%2Faccess_token=SECRET"
        ));
        assert!(is_capability(
            "https://example.org/cb#%21%2Faccess_token=SECRET"
        ));
        assert!(!is_capability("https://example.org/cb#%2Fsection"));
    }
}
