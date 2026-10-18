use chrono::{
    DateTime, Datelike, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, Offset,
    TimeZone, Utc,
};
use chrono_tz::Tz;
use std::{collections::BTreeMap, sync::OnceLock};

pub const ATTRIBUTION: &str =
    "Timezone Boundary Builder / OpenStreetMap contributors (ODbL-1.0), tzf-rel 2025c";

pub fn zone(name: &str) -> Option<Tz> {
    let name = name.trim().trim_matches('"');
    if let Ok(t) = name.parse() {
        return Some(t);
    }
    // Outlook also emits /freeassociation.sourceforge.net/Tzfile/Europe/Zurich.
    for (i, _) in name.match_indices('/') {
        if let Ok(t) = name[i + 1..].parse() {
            return Some(t);
        }
    }
    static WINDOWS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    if let Some(s) = WINDOWS
        .get_or_init(|| {
            serde_json::from_str(include_str!("../../../../data/windows-zones.json"))
                .expect("pinned CLDR")
        })
        .get(name)
    {
        return s.parse().ok();
    }
    static RAILS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    RAILS
        .get_or_init(|| {
            serde_json::from_str(include_str!("../../../../data/rails-zones.json"))
                .expect("pinned Rails")
        })
        .get(name)
        .and_then(|name| name.parse().ok())
}
pub fn coordinate_zone(point: &crate::model::Point) -> Option<String> {
    static FINDER: OnceLock<tzf_rs::DefaultFinder> = OnceLock::new();
    let name = FINDER
        .get_or_init(tzf_rs::DefaultFinder::new)
        .get_tz_name(point.lon, point.lat);
    zone(name).map(|tz| tz.to_string())
}
/// RFC 5545: first ambiguous instant; for a gap use the offset before the gap.
pub fn local(tz: Tz, value: NaiveDateTime) -> Option<DateTime<Utc>> {
    match tz.from_local_datetime(&value) {
        LocalResult::Single(t) => Some(t.to_utc()),
        LocalResult::Ambiguous(a, b) => Some(a.min(b).to_utc()),
        LocalResult::None => (1..=48)
            .find_map(|h| {
                tz.from_local_datetime(&value.checked_sub_signed(Duration::hours(h))?)
                    .earliest()
            })
            .and_then(|prior| prior.offset().fix().from_local_datetime(&value).single())
            .map(|t| t.to_utc()),
    }
}
/// Whether a date value starts with exactly four ASCII year digits, as RFC 5545
/// (`date-fullyear = 4DIGIT`) and the ISO 8601 forms Pinakes reads require: `YYYY-…` or the
/// compact `YYYYMMDD[T…]`. chrono's `%Y` would also take signed years of any width, which put
/// later arithmetic at the edge of its range. A compact date may carry `Z` directly
/// (`YYYYMMDDZ`), which `offset` strips. Callers trim leading whitespace first, which chrono's
/// `%Y` used to skip.
fn four_digit_year(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() >= 8
        && b[..4].iter().all(u8::is_ascii_digit)
        && (b[4] == b'-'
            || (b[4..8].iter().all(u8::is_ascii_digit)
                && b.get(8).is_none_or(|&c| c == b'T' || c == b'Z')))
}
/// The instant if its year is 0000–9999, the range the string parsers accept.
pub fn bounded(t: DateTime<Utc>) -> Option<DateTime<Utc>> {
    (0..=9999).contains(&t.year()).then_some(t)
}
/// A Unix timestamp in seconds, bounded like the string forms.
pub fn from_timestamp(secs: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(secs, 0).and_then(bounded)
}
pub fn naive(value: &str) -> Option<NaiveDateTime> {
    let value = value.trim_start();
    if !four_digit_year(value) {
        return None;
    }
    for f in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y%m%dT%H%M%S",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(value, f) {
            return Some(d);
        }
    }
    for f in ["%Y-%m-%d", "%Y%m%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(value, f) {
            return d.and_hms_opt(0, 0, 0);
        }
    }
    None
}
pub fn offset(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim_start();
    if !four_digit_year(value) {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(value) {
        return Some(t.to_utc());
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S %z"] {
        if let Ok(d) = DateTime::parse_from_str(value, f) {
            return Some(d.to_utc());
        }
    }
    value.strip_suffix('Z').and_then(naive).map(|t| t.and_utc())
}
pub fn parse(value: &str, tz: Option<&str>) -> Option<DateTime<Utc>> {
    offset(value).or_else(|| {
        naive(value).and_then(|n| {
            tz.and_then(zone)
                .map_or(Some(n.and_utc()), |tz| local(tz, n))
        })
    })
}
pub fn fixed(value: &str) -> Option<FixedOffset> {
    let sign = if value.starts_with('-') { -1 } else { 1 };
    let v = value.trim_start_matches(['-', '+']);
    if v.len() != 4 && v.len() != 6 {
        return None;
    }
    let h: i32 = v.get(..2)?.parse().ok()?;
    let m: i32 = v.get(2..4)?.parse().ok()?;
    let s: i32 = if v.len() == 6 {
        v.get(4..)?.parse().ok()?
    } else {
        0
    };
    if h > 23 || m > 59 || s > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60 + s))
}
pub fn duration(value: &str) -> Option<Duration> {
    let (sign, v) = if let Some(v) = value.strip_prefix('-') {
        (-1, v)
    } else {
        (1, value.trim_start_matches('+'))
    };
    let mut n = 0i64;
    let mut total = 0i64;
    let mut time = false;
    for c in v.strip_prefix('P')?.chars() {
        match c {
            '0'..='9' => n = n.checked_mul(10)?.checked_add(c.to_digit(10)? as i64)?,
            'T' => {
                time = true;
            }
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let mul = match c {
                    'W' => 604800,
                    'D' => 86400,
                    'H' if time => 3600,
                    'M' if time => 60,
                    'S' if time => 1,
                    _ => return None,
                };
                total = total.checked_add(n.checked_mul(mul)?)?;
                n = 0;
            }
            _ => return None,
        }
    }
    (n == 0)
        .then(|| Duration::try_seconds(sign * total))
        .flatten()
}
