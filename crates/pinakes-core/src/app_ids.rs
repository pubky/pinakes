//! The IDs app stores give their apps, as their links spell them: one reading for the store
//! sources (`sources::{app_store, google_play, fdroid, flathub, snap_store}`) and the `apps`
//! lookup family, so a link that is no app on one is no app on the other (a store's own pages
//! never become apps).

/// A plain value: non-empty, at most `max` bytes, every character allowed by `ok`.
fn plain(value: &str, max: usize, ok: impl Fn(char) -> bool) -> bool {
    !value.is_empty() && value.len() <= max && value.chars().all(ok)
}

/// An Android package (Google Play and F-Droid): dot-separated segments of letters, digits and
/// `_`, each starting with a letter.
pub fn android_package(value: &str) -> bool {
    plain(value, 200, |c| {
        c.is_ascii_alphanumeric() || "._".contains(c)
    }) && value.contains('.')
        && value
            .split('.')
            .all(|s| s.bytes().next().is_some_and(|b| b.is_ascii_alphabetic()))
}

/// An App Store ID: digits, no leading zero.
pub fn app_store_id(value: &str) -> bool {
    plain(value, 20, |c| c.is_ascii_digit()) && !value.starts_with('0')
}

/// A Flathub app ID: a reverse-DNS name of at least three non-empty parts of letters, digits,
/// `_` and `-` (`org.signal.Signal`), in its own case.
pub fn flathub_id(value: &str) -> bool {
    plain(value, 255, |c| {
        c.is_ascii_alphanumeric() || "._-".contains(c)
    }) && value.split('.').count() >= 3
        && value.split('.').all(|part| !part.is_empty())
}

/// snapcraft.io's own pages at its root, which are no snaps.
pub const SNAPCRAFT_PAGES: &[&str] = &[
    "about",
    "account",
    "blog",
    "docs",
    "first-snap",
    "iot",
    "login",
    "logout",
    "publisher",
    "search",
    "snaps",
    "store",
    "tutorials",
];

/// A snap name: lower-case letters, digits and `-`, at most 40, with a letter, not starting or
/// ending with `-`, and none of snapcraft.io's own pages.
pub fn snap_name(value: &str) -> bool {
    plain(value, 40, |c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
    }) && !value.starts_with('-')
        && !value.ends_with('-')
        && value.bytes().any(|b| b.is_ascii_lowercase())
        && !SNAPCRAFT_PAGES.contains(&value)
}

/// A Microsoft Store ID: 12 letters and digits, in upper case.
pub fn msstore_id(value: &str) -> Option<String> {
    (value.len() == 12 && value.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| value.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert!(android_package("org.thoughtcrime.securesms"));
        assert!(!android_package("securesms") && !android_package("org.1x"));
        assert!(app_store_id("874139669") && !app_store_id("0874139669"));
        assert!(flathub_id("org.signal.Signal"));
        assert!(!flathub_id("org..Signal") && !flathub_id("signal.Signal"));
        assert!(snap_name("signal-desktop") && snap_name("1password"));
        for page in [
            "store", "blog", "docs", "123", "-signal", "signal-", "Signal",
        ] {
            assert!(!snap_name(page), "{page}");
        }
        assert_eq!(msstore_id("9nksqgp7f2nh").as_deref(), Some("9NKSQGP7F2NH"));
        assert_eq!(msstore_id("short"), None);
    }
}
