//! Archived copies: when a resource is gone or unreachable, the Wayback Machine capture
//! (HTTP 200 only) closest to the time it was first referenced. archive.today is never used.
use super::{body, encode, web_url, Link, Request};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

/// How many captures around the time to ask for; the parser picks the closest itself.
const CAPTURES: u32 = 5;

/// A Wayback timestamp, `YYYYMMDDhhmmss` in UTC.
fn timestamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%d%H%M%S").to_string()
}
fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    (value.len() == 14 && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M%S").ok())
        .flatten()
        .map(|t| t.and_utc())
}

/// Captures of `url` that answered 200, closest to `near` first.
pub fn wayback_request(url: &str, near: DateTime<Utc>) -> Option<Request> {
    web_url(url)?;
    Request::get(&format!(
        "https://web.archive.org/cdx/search/cdx?url={}&output=json&fl=timestamp,original\
         &filter=statuscode:200&closest={}&sort=closest&limit={CAPTURES}",
        encode(url),
        timestamp(near)
    ))
}

/// "Archived copy" of `url`: the capture closest to `near` (the earlier one on a tie), with
/// its time in `at`. `None` when the archive has no capture.
pub fn parse_wayback(
    status: u16,
    raw: &[u8],
    url: &str,
    near: DateTime<Utc>,
) -> Result<Option<Link>> {
    let raw = body(status, raw, &[])?.context("no body")?;
    let rows: Vec<Vec<Value>> = if raw.iter().all(u8::is_ascii_whitespace) {
        Vec::new()
    } else {
        serde_json::from_slice(raw)?
    };
    let mut rows = rows.iter();
    if let Some(header) = rows.next() {
        anyhow::ensure!(
            header.first() == Some(&Value::from("timestamp")),
            "unexpected header"
        );
    }
    let closest = rows
        .filter_map(|row| parse_timestamp(row.first()?.as_str()?))
        .min_by_key(|at| ((*at - near).num_seconds().abs(), *at));
    Ok(closest.map(|at| Link {
        site: "Wayback Machine".into(),
        uri: format!("https://web.archive.org/web/{}/{url}", timestamp(at)),
        via: "wayback",
        kind: Some("archived"),
        at: Some(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture;
    use chrono::TimeZone;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn request() {
        let r = wayback_request("https://example.com/", at("2015-06-01T00:00:00Z")).unwrap();
        assert_eq!(
            r.url.as_str(),
            "https://web.archive.org/cdx/search/cdx?url=https%3A%2F%2Fexample.com%2F&output=json\
             &fl=timestamp,original&filter=statuscode:200&closest=20150601000000&sort=closest&limit=5"
        );
        assert!(wayback_request("mailto:a@b", Utc::now()).is_none());
    }

    #[test]
    fn closest_capture() {
        let near = at("2015-06-01T00:00:00Z");
        let raw = fixture::read("wayback-cdx-example.json");
        let link = parse_wayback(200, &raw, "http://example.com/", near)
            .unwrap()
            .unwrap();
        assert_eq!(
            link.uri,
            "https://web.archive.org/web/20150601000827/http://example.com/"
        );
        assert_eq!(link.at.as_deref(), Some("2015-06-01T00:08:27Z"));
        assert_eq!(
            (link.site.as_str(), link.via, link.kind),
            ("Wayback Machine", "wayback", Some("archived"))
        );
        // The parser chooses by itself, whatever order the server used.
        let later = at("2015-06-01T03:00:00Z");
        let link = parse_wayback(200, &raw, "http://example.com/", later)
            .unwrap()
            .unwrap();
        assert_eq!(link.at.as_deref(), Some("2015-06-01T03:04:15Z"));
        // On a tie the earlier capture wins.
        let rows = br#"[["timestamp","original"],["20200101000010","x"],["20200101000000","x"]]"#;
        let mid = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 5).unwrap();
        let link = parse_wayback(200, rows, "https://a.example/", mid)
            .unwrap()
            .unwrap();
        assert_eq!(link.at.as_deref(), Some("2020-01-01T00:00:00Z"));
    }

    #[test]
    fn no_capture() {
        let near = at("2015-06-01T00:00:00Z");
        let raw = fixture::read("wayback-cdx-none.json");
        assert_eq!(
            parse_wayback(200, &raw, "https://a.example/", near).unwrap(),
            None
        );
        assert_eq!(
            parse_wayback(200, b"", "https://a.example/", near).unwrap(),
            None
        );
        let bad = br#"[["timestamp"],["2015"],["notatimestamp00"]]"#;
        assert_eq!(
            parse_wayback(200, bad, "https://a.example/", near).unwrap(),
            None
        );
        assert!(parse_wayback(429, b"", "https://a.example/", near).is_err());
        assert!(parse_wayback(200, br#"[["original"]]"#, "https://a.example/", near).is_err());
    }
}
