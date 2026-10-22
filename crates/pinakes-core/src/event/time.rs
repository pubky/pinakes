use chrono::{
    DateTime, Datelike, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, Offset,
    SubsecRound, TimeZone, Timelike, Utc,
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
/// later arithmetic at the edge of its range. The ninth byte of a compact value may also be `Z`,
/// so that `offset` can strip it from `YYYYMMDDZ` and read the date as UTC; `naive` has no
/// format for the `Z` and refuses it. `naive` and `offset` trim leading whitespace before
/// calling this, as chrono's `%Y` skipped it.
fn four_digit_year(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() >= 8
        && b[..4].iter().all(u8::is_ascii_digit)
        && (b[4] == b'-'
            || (b[4..8].iter().all(u8::is_ascii_digit)
                && b.get(8).is_none_or(|&c| c == b'T' || c == b'Z')))
}
/// The instant if its UTC year is 0001–9999: what Postgres reads back from the text form
/// (`to_rfc3339` writes year 0 as `0000`, which Postgres refuses, and years past 9999 with a
/// `+`). The string parsers accept year 0000 and local times that land in year 10000, so every
/// source instant that is stored or bound passes through this (`event::bound_instants`).
///
/// Postgres keeps microseconds and rounds anything finer, so 9999-12-31T23:59:59.9999995Z would
/// be stored as year 10000: the instant is truncated to microseconds first, and callers store the
/// returned value.
///
/// chrono keeps a leap second as nanoseconds ≥ 1e9 within the 59th second, so
/// 9999-12-31T23:59:60Z reads as year 9999 while Postgres rolls it into year 10000: a leap second
/// is first normalised into the next second.
pub fn bounded(t: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let t = if t.nanosecond() >= 1_000_000_000 {
        t.with_nanosecond(t.nanosecond() - 1_000_000_000)?
            .checked_add_signed(Duration::seconds(1))?
    } else {
        t
    };
    let t = t.trunc_subsecs(6);
    (1..=9999).contains(&t.year()).then_some(t)
}
/// The earliest storable instant (`bounded`), for horizons that would otherwise be unbounded.
pub fn earliest() -> DateTime<Utc> {
    DateTime::from_timestamp(-62135596800, 0).expect("0001-01-01T00:00:00Z")
}
/// The latest storable instant (`bounded`).
pub fn latest() -> DateTime<Utc> {
    DateTime::from_timestamp(253402300799, 999_999_000).expect("9999-12-31T23:59:59.999999Z")
}
/// A Unix timestamp in seconds, bounded like every instant taken from a source.
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
/// The most dates one period of a recurrence rule may hold.
pub const MAX_PERIOD_DATES: usize = 1_000;
/// Whether a parsed recurrence set is safe to expand: every rule and exception rule repeats
/// yearly, monthly, weekly, daily or hourly, and one period holds at most `MAX_PERIOD_DATES`
/// dates (the days in its period times its hours, minutes and seconds). The `rrule` crate builds a
/// whole period before it yields its first date, so a yearly rule listing every hour, minute and
/// second would build 31 million at once. It reads the parsed rule, so how the rule spells its
/// names (`BYHOUR`, `byhour`) does not matter.
pub fn rrule_bounded(set: &rrule::RRuleSet) -> bool {
    set.get_rrule().iter().chain(set.get_exrule()).all(|rule| {
        let listed = |values: usize| values.max(1);
        let (hours, minutes, seconds) = (
            listed(rule.get_by_hour().len()),
            listed(rule.get_by_minute().len()),
            listed(rule.get_by_second().len()),
        );
        let dates = match rule.get_freq() {
            rrule::Frequency::Yearly => 366 * hours * minutes * seconds,
            rrule::Frequency::Monthly => 31 * hours * minutes * seconds,
            rrule::Frequency::Weekly => 7 * hours * minutes * seconds,
            rrule::Frequency::Daily => hours * minutes * seconds,
            rrule::Frequency::Hourly => minutes * seconds,
            rrule::Frequency::Minutely | rrule::Frequency::Secondly => return false,
        };
        dates <= MAX_PERIOD_DATES
    })
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
