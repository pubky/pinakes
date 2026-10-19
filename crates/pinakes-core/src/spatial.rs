use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};

/// RFC3339, Unix milliseconds, or a time relative to the request: `now`, `now+7d`, `now-12h`
/// (days or hours, at most 100 years; `now 7d` is `now+7d` as a raw query string decodes it).
/// `now` is the current minute, floored, so responses for the same relative query stay cacheable
/// within a minute. Absolute times must lie within the years Postgres can bind
/// (`time::bounded`).
pub fn timestamp(raw: &str) -> Result<DateTime<Utc>> {
    timestamp_at(raw, Utc::now())
}
pub fn timestamp_at(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    if let Some(offset) = raw.strip_prefix("now") {
        let minute = DateTime::from_timestamp(now.timestamp() - now.timestamp().rem_euclid(60), 0)
            .context("timestamp out of range")?;
        if offset.is_empty() {
            return Ok(minute);
        }
        // A raw `now+30d` in a query string arrives as `now 30d` (form decoding). Client input:
        // match ASCII bytes and slice only after one, never at an arbitrary byte index.
        let sign = match offset.as_bytes()[0] {
            b'+' | b' ' => 1,
            b'-' => -1,
            _ => anyhow::bail!("relative time is now, now+N(d|h) or now-N(d|h)"),
        };
        let amount = &offset[1..];
        let hours = match amount.as_bytes().last() {
            Some(b'd') => 24,
            Some(b'h') => 1,
            _ => anyhow::bail!("relative time unit is d or h"),
        };
        let digits = &amount[..amount.len() - 1];
        ensure!(
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && digits.len() <= 6,
            "relative time amount is a whole number"
        );
        let hours = digits.parse::<i64>()? * hours;
        ensure!(
            hours <= 100 * 366 * 24,
            "relative time is at most 100 years"
        );
        return Ok(minute + chrono::Duration::hours(sign * hours));
    }
    let at = if let Ok(ms) = raw.parse::<i64>() {
        DateTime::from_timestamp_millis(ms).context("timestamp out of range")?
    } else {
        DateTime::parse_from_rfc3339(raw)?.with_timezone(&Utc)
    };
    crate::event::time::bounded(at).context("timestamp out of range")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamps_are_bounded_and_relative() {
        assert_eq!(timestamp("0").unwrap().timestamp(), 0);
        let at = timestamp("2026-10-18T10:15:42.5Z").unwrap();
        for (raw, expected) in [
            ("now", "2026-10-18T10:15:00Z"),
            ("now+30d", "2026-11-17T10:15:00Z"),
            ("now-12h", "2026-10-17T22:15:00Z"),
            ("now+0h", "2026-10-18T10:15:00Z"),
            ("now 30d", "2026-11-17T10:15:00Z"),
        ] {
            assert_eq!(
                timestamp_at(raw, at).unwrap(),
                timestamp(expected).unwrap(),
                "{raw}"
            );
        }
        for bad in [
            "nowé",
            "now+é",
            "now+1é",
            "now-é",
            "now+1😀",
            "now😀",
            "now\u{FFFD}",
            "now+",
            "now+d",
            "now+3",
            "now+3w",
            "now*3d",
            "now+-3d",
            "now+ 3d",
            "nowx",
            "now+9999999d",
            "now+40000d",
        ] {
            assert!(timestamp_at(bad, at).is_err(), "{bad}");
        }
        assert_eq!(
            timestamp("2026-10-15T10:00:00+02:00").unwrap(),
            timestamp("2026-10-15T08:00:00Z").unwrap()
        );
    }
}
