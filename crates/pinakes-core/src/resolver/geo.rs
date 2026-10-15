use crate::model::{LocationSource, Point, ResourceMeta};
use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use std::time::Duration;

pub const ATTRIBUTES: &[&str] = &["precision"];
pub struct GeoResolver;

fn number(s: &str, signed: bool, integer_digits: usize) -> Result<f64> {
    let unsigned = if signed {
        s.strip_prefix('-').unwrap_or(s)
    } else {
        s
    };
    let (whole, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, None), |(a, b)| (a, Some(b)));
    ensure!(
        !whole.is_empty()
            && whole.len() <= integer_digits
            && whole.bytes().all(|c| c.is_ascii_digit()),
        "invalid geo number"
    );
    ensure!(
        fraction.is_none_or(|f| !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit())),
        "invalid geo fraction"
    );
    let value: f64 = s.parse()?;
    ensure!(value.is_finite(), "non-finite coordinate");
    Ok(value)
}
fn parameter_value(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.bytes();
    while let Some(c) = chars.next() {
        if c == b'%' {
            if !chars.next().is_some_and(|c| c.is_ascii_hexdigit())
                || !chars.next().is_some_and(|c| c.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !(c.is_ascii_alphanumeric() || b"-_.!~*'()[]:&+$".contains(&c)) {
            return false;
        }
    }
    true
}
pub fn parse(uri: &str) -> Result<ResourceMeta> {
    let (scheme, rest) = uri
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("missing geo scheme"))?;
    ensure!(scheme.eq_ignore_ascii_case("geo"), "not a geo URI");
    let mut pieces = rest.split(';');
    let coords: Vec<_> = pieces.next().unwrap_or_default().split(',').collect();
    ensure!(
        (2..=3).contains(&coords.len()),
        "geo requires latitude,longitude[,altitude]"
    );
    let lat = number(coords[0], true, 2)?;
    let lon = number(coords[1], true, 3)?;
    ensure!(
        (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon),
        "coordinate out of range"
    );
    if let Some(alt) = coords.get(2) {
        number(alt, true, usize::MAX)?;
    }
    let (mut crs, mut uncertainty, mut extra) = (false, false, false);
    for parameter in pieces {
        let (key, value) = parameter
            .split_once('=')
            .map_or((parameter, None), |(k, v)| (k, Some(v)));
        ensure!(
            !key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "invalid geo parameter name"
        );
        match key.to_ascii_lowercase().as_str() {
            "crs" => {
                ensure!(
                    !crs && !uncertainty && !extra,
                    "crs must occur once and first"
                );
                ensure!(
                    value.is_some_and(|v| v.eq_ignore_ascii_case("wgs84")),
                    "unsupported coordinate reference system"
                );
                crs = true;
            }
            "u" => {
                ensure!(
                    !uncertainty && !extra,
                    "u must occur once, before extensions"
                );
                number(
                    value.ok_or_else(|| anyhow::anyhow!("missing uncertainty"))?,
                    false,
                    usize::MAX,
                )?;
                uncertainty = true;
            }
            _ => {
                if value.is_some_and(|v| !parameter_value(v)) {
                    bail!("invalid geo parameter value");
                }
                extra = true;
            }
        }
    }
    let digits = coords[..2]
        .iter()
        .map(|c| c.split_once('.').map_or(0, |(_, f)| f.len()))
        .min()
        .unwrap();
    let precision = match digits {
        0 | 1 => "~10 km",
        2 => "~1 km",
        3 => "~100 m",
        4 => "~10 m",
        _ => "~1 m",
    };
    let mut doc = ResourceMeta::new(uri, "geo", 0);
    doc.set_type("Place");
    doc.name = Some(format!("{}, {}", coords[0], coords[1]));
    doc.location = Some(Point { lat, lon });
    doc.location_source = Some(LocationSource::Uri);
    doc.attributes.insert("precision".into(), precision.into());
    doc.validate(ATTRIBUTES)?;
    Ok(doc)
}
#[async_trait]
impl super::Resolver for GeoResolver {
    fn name(&self) -> &'static str {
        "geo"
    }
    fn matches(&self, uri: &str) -> bool {
        uri.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("geo:"))
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
        parse(uri)
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::ZERO
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_grammar_and_precision() {
        for uri in [
            "geo:46.0051,8.9528",
            "GEO:-90,180,-10.5;CRS=WGS84;U=0;foo=a%20b;flag",
            "geo:-0,0;u=2.0",
            "geo:1,2;foo=[:&+$]",
        ] {
            parse(uri).unwrap();
        }
        let doc = parse("geo:46.0051,8.9528,7;crs=wgs84;u=5").unwrap();
        assert_eq!(doc.name.as_deref(), Some("46.0051, 8.9528"));
        assert_eq!(doc.attributes["precision"], "~10 m");
        assert!(serde_json::to_value(doc).unwrap().get("ttl_secs").is_none());
        for (coords, label) in [
            ("1,2", "~10 km"),
            ("1.12,2.34", "~1 km"),
            ("1.123,2.345", "~100 m"),
            ("1.12345,2.34567", "~1 m"),
        ] {
            assert_eq!(
                parse(&format!("geo:{coords}")).unwrap().attributes["precision"],
                label
            );
        }
    }
    #[test]
    fn rejects_invalid_coordinates_and_parameters() {
        for tail in [
            "91,0",
            "0,-181",
            "NaN,0",
            "1e1,0",
            "+1,0",
            "1.,2",
            ".1,2",
            "001,2",
            "1,2,3,4",
            "1,2;u=-1",
            "1,2;u=+1",
            "1,2;u=NaN",
            "1,2;crs=other",
            "1,2;u=1;crs=wgs84",
            "1,2;u=1;u=2",
            "1,2;x=y;u=1",
            "1,2;crs=wgs84;crs=wgs84",
            "1,2;foo=%0Z",
            "1,2;foo=",
            "1,2;",
            "1,2#x",
            "1,2?x",
        ] {
            assert!(parse(&format!("geo:{tail}")).is_err(), "{tail}");
        }
    }
}
