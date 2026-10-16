use crate::{
    adapter::{Manifest, Mode},
    config::RefreshConfig,
    model::ResourceMeta,
};
use chrono::{DateTime, Duration, Utc};

pub fn next(
    manifest: &Manifest,
    doc: &ResourceMeta,
    now: DateTime<Utc>,
    config: &RefreshConfig,
) -> Option<DateTime<Utc>> {
    if manifest.mode == Mode::None {
        return None;
    }
    let end = doc.end.or(doc.start);
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
    let until = doc.start.map(|start| start - now);
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
    documents: &[ResourceMeta],
    feed_ttl: Option<u64>,
    now: DateTime<Utc>,
    config: &RefreshConfig,
) -> Option<DateTime<Utc>> {
    let mut config = config.clone();
    if let Some(ttl) = feed_ttl {
        config.container_secs[0] = ttl;
        config.container_secs[1] = ttl;
        config.container_secs[2] = ttl.min(config.container_secs[2]);
    }
    documents
        .iter()
        .filter_map(|doc| next(manifest, doc, now, &config))
        .min()
        .or_else(|| next(manifest, &ResourceMeta::default(), now, &config))
}
#[cfg(test)]
mod tests {
    use super::*;
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
            container_next(&manifest, &[], Some(8 * 3600), now, &config),
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
