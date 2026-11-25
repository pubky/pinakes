use crate::{
    adapter::{Manifest, Mode},
    config::RefreshConfig,
    model::ResourceMeta,
};
use chrono::{DateTime, Duration, Utc};

/// The wait of an undated card that refreshes on the backoff rule: `base` (its route's undated
/// wait: `limits.refresh_resource_secs`' first tier, an official card's own lifetime) after a
/// read that changed it, twice the last wait after one that found it the same, up to `max`
/// (`limits.refresh_max_secs`), and `max` for a link found gone. Events, calendars and feeds, dated
/// cards, social content and a card that names when it changes are not on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub base: u64,
    pub max: u64,
}
impl Backoff {
    /// The wait after a read; `previous` is the wait before it (the stored `ttl_secs`).
    pub fn next(self, previous: u64, unchanged: bool) -> u64 {
        // A base above the cap (a first tier configured past `refresh_max_secs`) is the cap.
        let cap = self.max.max(self.base);
        let base = self.base.max(1);
        if unchanged {
            previous.max(base).saturating_mul(2).min(cap)
        } else {
            base
        }
    }
    /// The wait of a link two fresh reads found gone: the slowest rate.
    pub fn gone(self) -> u64 {
        self.max.max(self.base)
    }
}
pub fn next(
    manifest: &Manifest,
    doc: &ResourceMeta,
    now: DateTime<Utc>,
    config: &RefreshConfig,
) -> Option<DateTime<Utc>> {
    // A job posting's date posted is past from the start: only its closing ends its refreshes.
    let start = doc.start.filter(|_| !crate::kinds::jobs::is_posting(doc));
    next_times(manifest, start, doc.end, now, config)
}
fn next_times(
    manifest: &Manifest,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    config: &RefreshConfig,
) -> Option<DateTime<Utc>> {
    if manifest.mode == Mode::None {
        return None;
    }
    let end = end.or(start);
    let final_at = end.map(|end| {
        end.checked_add_signed(Duration::hours(24))
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
    });
    if final_at.is_some_and(|at| now >= at) {
        return None;
    }
    if end.is_some_and(|end| now >= end) {
        return final_at;
    }
    let until = start.map(|start| start - now);
    let row = match until {
        None => 0,
        Some(d) if d > Duration::days(30) => 0,
        Some(d) if d >= Duration::days(7) => 1,
        Some(d) if d >= Duration::hours(24) => 2,
        _ => 3,
    };
    let seconds = match manifest.mode {
        Mode::Resource => config.resource_secs[row],
        Mode::Container => config.container_secs[row],
        Mode::Cursor => config.cursor_secs,
        Mode::None => return None,
    }
    .max(manifest.cadence_floor)
    .max(if manifest.mode == Mode::Cursor {
        1
    } else {
        3600
    });
    let at = now
        .checked_add_signed(Duration::try_seconds(seconds.min(i64::MAX as u64) as i64)?)
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    Some(final_at.map_or(at, |end| end.min(at)))
}
/// Container TTL is meaningful only in the far/medium rows; near starts tighten it.
pub fn container_next(
    manifest: &Manifest,
    dates: impl IntoIterator<Item = (Option<DateTime<Utc>>, Option<DateTime<Utc>>)>,
    feed_ttl: Option<u64>,
    now: DateTime<Utc>,
    config: &RefreshConfig,
) -> Option<DateTime<Utc>> {
    if manifest.mode == Mode::Cursor {
        return next_times(manifest, None, None, now, config);
    }
    let mut config = config.clone();
    if let Some(ttl) = feed_ttl {
        config.container_secs[0] = ttl;
        config.container_secs[1] = ttl;
        config.container_secs[2] = ttl.min(config.container_secs[2]);
    }
    dates
        .into_iter()
        .filter_map(|(start, end)| next_times(manifest, start, end, now, &config))
        .min()
        .or_else(|| next_times(manifest, None, None, now, &config))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_unchanged_card_waits_twice_as_long_up_to_the_cap() {
        let day = 86400;
        let rule = Backoff {
            base: 7 * day,
            max: 90 * day,
        };
        let mut wait = rule.next(7 * day, false);
        assert_eq!(wait, 7 * day);
        let mut waits = vec![];
        for _ in 0..6 {
            wait = rule.next(wait, true);
            waits.push(wait / day);
        }
        assert_eq!(waits, [14, 28, 56, 90, 90, 90]);
        // A change starts over.
        assert_eq!(rule.next(90 * day, false), 7 * day);
        // A wait shorter than the base (an older rule's) doubles from the base.
        assert_eq!(rule.next(3600, true), 14 * day);
        assert_eq!(rule.gone(), 90 * day);
        // An official card's own lifetime is its base.
        let official = Backoff {
            base: day,
            max: 90 * day,
        };
        assert_eq!(official.next(day, true), 2 * day);
        assert_eq!(official.next(64 * day, true), 90 * day);
        // A base above the cap (a first tier configured past it) is the longest wait.
        let odd = Backoff {
            base: 100 * day,
            max: 90 * day,
        };
        assert_eq!(odd.next(0, false), 100 * day);
        assert_eq!(odd.next(100 * day, true), 100 * day);
        assert_eq!(odd.gone(), 100 * day);
    }
    #[test]
    fn cadence_table_floor_and_final() {
        let now: DateTime<Utc> = "2026-10-15T12:00:00Z".parse().unwrap();
        let config = RefreshConfig::default();
        let mut manifest = crate::adapter::manifest("web");
        for (hours, expected) in [
            (None, 168),
            (Some(31 * 24), 168),
            (Some(30 * 24), 48),
            (Some(7 * 24), 48),
            (Some(7 * 24 - 1), 6),
            (Some(24), 6),
            (Some(23), 1),
        ] {
            let doc = ResourceMeta {
                start: hours.map(|h| now + Duration::hours(h)),
                ..Default::default()
            };
            assert_eq!(
                next(&manifest, &doc, now, &config).unwrap() - now,
                Duration::hours(expected)
            );
        }
        let mut doc = ResourceMeta {
            end: Some(now - Duration::hours(1)),
            ..Default::default()
        };
        assert_eq!(
            next(&manifest, &doc, now, &config),
            Some(now + Duration::hours(23))
        );
        doc.end = Some(now - Duration::hours(24));
        assert_eq!(next(&manifest, &doc, now, &config), None);
        let start_only = ResourceMeta {
            start: Some(now - Duration::hours(1)),
            ..Default::default()
        };
        assert_eq!(
            next(&manifest, &start_only, now, &config),
            Some(now + Duration::hours(23))
        );
        let past = ResourceMeta {
            start: Some(now - Duration::hours(24)),
            ..Default::default()
        };
        assert_eq!(next(&manifest, &past, now, &config), None);
        // A job posting posted a month ago refreshes as a dateless page until it closes, and once
        // more a day after.
        let mut posting = ResourceMeta {
            start: Some(now - Duration::days(30)),
            ..Default::default()
        };
        posting.set_type("JobPosting");
        assert_eq!(
            next(&manifest, &posting, now, &config),
            Some(now + Duration::hours(168))
        );
        posting.end = Some(now + Duration::days(2));
        assert_eq!(
            next(&manifest, &posting, now, &config),
            Some(now + Duration::hours(72))
        );
        posting.end = Some(now - Duration::days(2));
        assert_eq!(next(&manifest, &posting, now, &config), None);
        let running = ResourceMeta {
            start: Some(now - Duration::days(2)),
            end: Some(now + Duration::days(10)),
            ..Default::default()
        };
        assert_eq!(
            next(&manifest, &running, now, &config),
            Some(now + Duration::hours(1))
        );
        assert_eq!(
            next(
                &crate::adapter::manifest("geo"),
                &ResourceMeta::default(),
                now,
                &config
            ),
            None
        );
        manifest.mode = Mode::Container;
        manifest.cadence_floor = 4 * 3600;
        let doc = ResourceMeta {
            start: Some(now + Duration::hours(1)),
            ..Default::default()
        };
        assert_eq!(
            next(&manifest, &doc, now, &config),
            Some(now + Duration::hours(4))
        );
        assert_eq!(
            container_next(&manifest, [], Some(8 * 3600), now, &config),
            Some(now + Duration::hours(8))
        );
        manifest.mode = Mode::Cursor;
        manifest.cadence_floor = 0;
        assert_eq!(
            next(&manifest, &doc, now, &config),
            Some(now + Duration::minutes(5))
        );
        manifest.mode = Mode::Resource;
        manifest.cadence_floor = 0;
        let config = RefreshConfig {
            resource_secs: [1; 4],
            ..config
        };
        assert_eq!(
            next(&manifest, &doc, now, &config),
            Some(now + Duration::hours(1))
        );
    }
}
