use crate::{api::SearchQuery, config::SearchConfig};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{Postgres, QueryBuilder};

#[derive(Debug, PartialEq)]
pub struct Bbox(pub [f64; 4]);
impl Bbox {
    pub fn parse(raw: &str) -> Result<Self> {
        let values = coordinates(raw, 4)?;
        ensure!(
            (-180.0..=180.0).contains(&values[0])
                && (-180.0..=180.0).contains(&values[2])
                && (-90.0..=90.0).contains(&values[1])
                && (-90.0..=90.0).contains(&values[3])
                && values[1] <= values[3],
            "invalid bbox"
        );
        Ok(Self(values.try_into().unwrap()))
    }
    pub fn split(&self) -> Vec<[f64; 4]> {
        let [w, s, e, n] = self.0;
        if w > e {
            vec![[w, s, 180.0, n], [-180.0, s, e, n]]
        } else {
            vec![self.0]
        }
    }
}
fn coordinates(raw: &str, count: usize) -> Result<Vec<f64>> {
    ensure!(raw.len() <= 256, "coordinate filter too long");
    let values = raw
        .split(',')
        .map(str::parse::<f64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        values.len() == count && values.iter().all(|v| v.is_finite()),
        "invalid coordinates"
    );
    Ok(values)
}
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
        // A raw `now+30d` in a query string arrives as `now 30d` (form decoding).
        let (sign, amount) = match offset.split_at(1) {
            ("+" | " ", rest) => (1, rest),
            ("-", rest) => (-1, rest),
            _ => anyhow::bail!("relative time is now, now+N(d|h) or now-N(d|h)"),
        };
        let (digits, hours) = match amount.split_at(amount.len().saturating_sub(1)) {
            (digits, "d") => (digits, 24),
            (digits, "h") => (digits, 1),
            _ => anyhow::bail!("relative time unit is d or h"),
        };
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
pub fn resolution(zoom: f64, config: &SearchConfig) -> Result<h3o::Resolution> {
    ensure!(zoom.is_finite(), "zoom must be finite");
    let zoom = (zoom.floor().max(0.0) as usize).min(config.h3_resolutions.len().saturating_sub(1));
    let res = *config
        .h3_resolutions
        .get(zoom)
        .context("zoom outside configured H3 table")?;
    Ok(res.try_into()?)
}
#[derive(Default)]
pub struct Filters {
    pub bbox: Option<Bbox>,
    pub near: Option<(f64, f64, f64)>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub resolution: Option<h3o::Resolution>,
}
impl Filters {
    pub fn parse(q: &SearchQuery, config: &SearchConfig) -> Result<Self> {
        ensure!(
            q.bbox.is_none() || q.near.is_none(),
            "bbox and near are mutually exclusive"
        );
        ensure!(
            q.radius.is_none() || q.near.is_some(),
            "radius requires near"
        );
        ensure!(
            q.sort.as_deref() != Some("distance") || q.near.is_some(),
            "sort=distance requires near"
        );
        ensure!(
            q.cluster.is_none() || q.bbox.is_some(),
            "cluster requires bbox"
        );
        ensure!(
            q.cluster.is_none() || q.cursor.is_none(),
            "cluster does not accept a cursor"
        );
        let mut out = Self {
            bbox: q.bbox.as_deref().map(Bbox::parse).transpose()?,
            from: q.from.as_deref().map(timestamp).transpose()?,
            to: q.to.as_deref().map(timestamp).transpose()?,
            ..Default::default()
        };
        if let Some(raw) = &q.near {
            let p = coordinates(raw, 2)?;
            ensure!(
                (-90.0..=90.0).contains(&p[0]) && (-180.0..=180.0).contains(&p[1]),
                "invalid near point"
            );
            let radius = q
                .radius
                .as_deref()
                .context("near requires radius in metres")?
                .parse::<f64>()?;
            ensure!(
                radius.is_finite() && radius > 0.0 && radius <= config.max_radius_m,
                "radius exceeds configured metre range"
            );
            out.near = Some((p[0], p[1], radius));
        }
        if let Some(zoom) = &q.cluster {
            let mut res = resolution(zoom.parse()?, config)?;
            let [w, s, e, n] = out.bbox.as_ref().expect("required bbox").0;
            let width = if e < w { e + 360.0 - w } else { e - w };
            let fraction = width / 360.0 * (n.to_radians().sin() - s.to_radians().sin()) / 2.0;
            // Conservative area and edge allowance; SQL also checks the actual group count.
            while u8::from(res) > 0 {
                let cells = res.cell_count() as f64;
                let estimate = (cells * fraction * 4.0
                    + cells.sqrt() * (width / 360.0 + (n - s) / 180.0) * 8.0
                    + 16.0)
                    .min(cells);
                if estimate <= config.max_cluster_cells as f64 {
                    break;
                }
                res = (u8::from(res) - 1).try_into()?;
            }
            out.resolution = Some(res);
        }
        ensure!(
            out.from.zip(out.to).is_none_or(|(f, t)| f <= t),
            "from must precede to"
        );
        Ok(out)
    }
    pub fn is_spatial(&self) -> bool {
        self.bbox.is_some() || self.near.is_some()
    }
    pub fn distance(&self, sql: &mut QueryBuilder<'_, Postgres>) {
        let (lat, lon, _) = self.near.expect("validated near");
        sql.push("ST_Distance(r.point::geography,ST_SetSRID(ST_MakePoint(")
            .push_bind(lon)
            .push(",")
            .push_bind(lat)
            .push("),4326)::geography)");
    }
    pub fn apply(&self, sql: &mut QueryBuilder<'_, Postgres>) {
        if let Some(bbox) = &self.bbox {
            sql.push(" AND (");
            for (i, b) in bbox.split().iter().enumerate() {
                if i > 0 {
                    sql.push(" OR ");
                }
                sql.push("r.point && ST_MakeEnvelope(");
                for (j, n) in b.iter().enumerate() {
                    if j > 0 {
                        sql.push(",");
                    }
                    sql.push_bind(*n);
                }
                sql.push(",4326)");
            }
            sql.push(")");
        }
        if let Some((lat, lon, radius)) = self.near {
            sql.push(" AND ST_DWithin(r.point::geography,ST_SetSRID(ST_MakePoint(")
                .push_bind(lon)
                .push(",")
                .push_bind(lat)
                .push("),4326)::geography,")
                .push_bind(radius)
                .push(")");
        }
        if let Some(from) = self.from {
            sql.push(" AND r.start_at>=").push_bind(from);
        }
        if let Some(to) = self.to {
            sql.push(" AND r.start_at<=").push_bind(to);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn antimeridian_and_h3_table() {
        assert_eq!(
            Bbox::parse("170,-10,-170,10").unwrap().split(),
            vec![[170.0, -10.0, 180.0, 10.0], [-180.0, -10.0, -170.0, 10.0]]
        );
        assert_eq!(Bbox::parse("-180,-90,180,90").unwrap().split().len(), 1);
        for raw in ["1,2,3", "1,3,2,0", "NaN,0,2,3", "181,0,0,0"] {
            assert!(Bbox::parse(raw).is_err());
        }
        let config = SearchConfig::default();
        for zoom in 0usize..=22 {
            assert_eq!(
                u8::from(resolution(zoom as f64, &config).unwrap()),
                zoom.saturating_sub(4).min(11) as u8
            );
        }
        assert_eq!(
            resolution(23.0, &config).unwrap(),
            resolution(22.0, &config).unwrap()
        );
        assert_eq!(
            resolution(8.4, &config).unwrap(),
            resolution(8.0, &config).unwrap()
        );
        assert!(resolution(f64::NAN, &config).is_err());
        let world = Filters::parse(
            &SearchQuery {
                bbox: Some("-180,-90,180,90".into()),
                cluster: Some("1000.5".into()),
                ..Default::default()
            },
            &config,
        )
        .unwrap();
        assert!(world.resolution.unwrap().cell_count() <= config.max_cluster_cells as u64);
        let tiny = Filters::parse(
            &SearchQuery {
                bbox: Some("8,46,8.00001,46.00001".into()),
                cluster: Some("8.4".into()),
                ..Default::default()
            },
            &config,
        )
        .unwrap();
        assert_eq!(u8::from(tiny.resolution.unwrap()), 4);
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
