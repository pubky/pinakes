use crate::model::{LocationSource, Point, ResourceMeta};
use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use std::time::Duration;

pub const ATTRIBUTES: &[&str] = &["precision"];
/// Resolves `geo:` URIs and, when `location_links` is on, map links that name a point.
pub struct GeoResolver {
    pub location_links: bool,
}
impl Default for GeoResolver {
    fn default() -> Self {
        Self {
            location_links: true,
        }
    }
}

fn number(s: &str, signed: bool, _integer_digits: usize) -> Result<f64> {
    let unsigned = if signed {
        s.strip_prefix('-').unwrap_or(s)
    } else {
        s
    };
    let (whole, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, None), |(a, b)| (a, Some(b)));
    ensure!(
        !whole.is_empty() && whole.bytes().all(|c| c.is_ascii_digit()),
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
    let name = format!("{}, {}", coords[0], coords[1]);
    point(uri, lat, lon, digits(&coords[..2]), name)
}
const PRECISION: [&str; 5] = ["~10 km", "~1 km", "~100 m", "~10 m", "~1 m"];
/// Index into `PRECISION` from the less precise axis's decimal digits (0–1 → ~10 km … 5+ → ~1 m).
fn digits(coords: &[&str]) -> usize {
    let fraction = coords
        .iter()
        .map(|c| c.split_once('.').map_or(0, |(_, f)| f.len()))
        .min()
        .unwrap_or(0);
    fraction.saturating_sub(1).min(4)
}
/// Index into `PRECISION` from a web-map zoom: one pixel is about 156 543 m / 2^zoom.
fn zoom_precision(zoom: f64) -> usize {
    let metres = 156_543.0 / 2f64.powf(zoom);
    [2000.0, 200.0, 20.0, 2.0]
        .iter()
        .filter(|limit| metres <= **limit)
        .count()
}
fn point(uri: &str, lat: f64, lon: f64, precision: usize, name: String) -> Result<ResourceMeta> {
    ensure!(
        (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon),
        "coordinate out of range"
    );
    let mut doc = ResourceMeta::new(uri, "geo", 0);
    doc.set_type("Place");
    doc.name = Some(name);
    doc.location = Some(Point { lat, lon });
    doc.location_source = Some(LocationSource::Uri);
    doc.attributes
        .insert("precision".into(), PRECISION[precision].into());
    doc.validate(ATTRIBUTES)?;
    Ok(doc)
}

/// A point named by a map link. Precision comes from the link's zoom when it has one, else from
/// the decimal digits of its coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct LocationLink {
    pub lat: f64,
    pub lon: f64,
    pub zoom: Option<f64>,
    text: Option<(String, String)>,
}
impl LocationLink {
    fn precision(&self) -> usize {
        match (self.zoom, &self.text) {
            (Some(zoom), _) => zoom_precision(zoom),
            (None, Some((lat, lon))) => digits(&[lat, lon]),
            (None, None) => 0,
        }
    }
    fn name(&self) -> String {
        match (&self.text, self.zoom) {
            (Some((lat, lon)), None) => format!("{lat}, {lon}"),
            _ => {
                let places = self.precision() + 1;
                format!("{:.places$}, {:.places$}", self.lat, self.lon)
            }
        }
    }
}
fn text_point(lat: &str, lon: &str, zoom: Option<String>) -> Option<LocationLink> {
    let (lat_n, lon_n) = (number(lat, true, 2).ok()?, number(lon, true, 3).ok()?);
    if !(-90.0..=90.0).contains(&lat_n) || !(-180.0..=180.0).contains(&lon_n) {
        return None;
    }
    let zoom = match zoom {
        Some(z) => Some(number(&z, false, 2).ok().filter(|z| *z <= 25.0)?),
        None => None,
    };
    Some(LocationLink {
        lat: lat_n,
        lon: lon_n,
        zoom,
        text: zoom.is_none().then(|| (lat.to_owned(), lon.to_owned())),
    })
}
/// OSM marker links (`/?mlat=&mlon=`) and shortlinks (`/go/<code>`, `?m` marks a marker), OsmAnd
/// `map?pin=lat,lon` and `go?lat=&lon=&z=`, and Organic Maps `omaps.app/<ge0 code>[/<name>]`.
/// Fragments are ignored: the identity rule drops them before a link is stored.
pub fn location_link(uri: &str) -> Option<LocationLink> {
    let url = url::Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    match host.as_str() {
        "www.openstreetmap.org" | "openstreetmap.org" | "osm.org" | "www.osm.org" => {
            match url.path() {
                "/" => text_point(&query("mlat")?, &query("mlon")?, query("zoom")),
                path => shortlink(path.strip_prefix("/go/")?),
            }
        }
        "osmand.net" | "www.osmand.net" => match url.path() {
            "/map" => {
                let pin = query("pin")?;
                let (lat, lon) = pin.split_once(',')?;
                text_point(lat, lon, None)
            }
            "/go" => text_point(&query("lat")?, &query("lon")?, query("z")),
            _ => None,
        },
        "omaps.app" => {
            let path = url.path().strip_prefix('/')?;
            ge0(path.split_once('/').map_or(path, |(code, _name)| code))
        }
        _ => None,
    }
}
/// openstreetmap-website `ShortLink.decode`: Morton-interleaved x/y, 3 bits each per character,
/// `@` as the legacy `~`, and up to two trailing `-` for partial zoom levels.
pub fn shortlink(code: &str) -> Option<LocationLink> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_~";
    let digits = code.trim_end_matches('-');
    let dashes = code.len() - digits.len();
    if digits.is_empty() || digits.len() > 10 || dashes > 2 {
        return None;
    }
    let (mut x, mut y, mut z) = (0u64, 0u64, 0u32);
    for c in digits.bytes() {
        let mut t = ALPHABET
            .iter()
            .position(|a| *a == if c == b'@' { b'~' } else { c })? as u64;
        for _ in 0..3 {
            x = (x << 1) | u64::from(t & 32 != 0);
            t <<= 1;
            y = (y << 1) | u64::from(t & 32 != 0);
            t <<= 1;
        }
        z += 3;
    }
    x <<= 32 - z;
    y <<= 32 - z;
    let scale = 4_294_967_296.0;
    Some(LocationLink {
        lat: y as f64 * 180.0 / scale - 90.0,
        lon: x as f64 * 360.0 / scale - 180.0,
        zoom: Some(f64::from(z) - 8.0 - [0.0, 2.0, 1.0][dashes]),
        text: None,
    })
}
/// Organic Maps ge0 (`libs/ge0/parser.cpp`): one zoom character (zoom = value / 4 + 4) and nine
/// characters of interleaved latitude/longitude bits, decoded to the centre of their cell.
pub fn ge0(code: &str) -> Option<LocationLink> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    const BITS: u32 = 30;
    let value = |c: u8| ALPHABET.iter().position(|a| *a == c).map(|v| v as u32);
    let bytes = code.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    let zoom = f64::from(value(bytes[0])?) / 4.0 + 4.0;
    let (mut lat, mut lon, mut shift) = (0u32, 0u32, BITS - 3);
    for c in &bytes[1..] {
        let a = value(*c)?;
        lat |= (((a >> 5) & 1) << 2 | ((a >> 3) & 1) << 1 | ((a >> 1) & 1)) << shift;
        lon |= (((a >> 4) & 1) << 2 | ((a >> 2) & 1) << 1 | (a & 1)) << shift;
        shift = shift.saturating_sub(3);
    }
    // Nine of the ten encodable characters: add half of the last 3-bit cell to get its centre.
    let middle = 1u32 << 2;
    let max = f64::from((1u32 << BITS) - 1);
    Some(LocationLink {
        lat: f64::from(lat + middle) / max * 180.0 - 90.0,
        lon: f64::from(lon + middle) / (max + 1.0) * 360.0 - 180.0,
        zoom: Some(zoom),
        text: None,
    })
}
pub fn parse_link(uri: &str) -> Result<ResourceMeta> {
    let link = location_link(uri).ok_or_else(|| anyhow::anyhow!("not a location link"))?;
    point(uri, link.lat, link.lon, link.precision(), link.name())
}
#[async_trait]
impl super::Resolver for GeoResolver {
    fn name(&self) -> &'static str {
        "geo"
    }
    fn matches(&self, uri: &str) -> bool {
        uri.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("geo:")) || location_link(uri).is_some()
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
        if uri.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("geo:")) {
            return parse(uri);
        }
        if !self.location_links {
            return Err(super::RetryLater {
                class: "disabled",
                seconds: 3600,
            }
            .into());
        }
        parse_link(uri)
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::ZERO
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortlinks_decode_like_openstreetmap_org() {
        // The live server's redirects for these codes (openstreetmap.org/go/<code>, 2026-10-17).
        for (code, lat, lon, zoom) in [
            ("0EEQjE--", 51.510772705078125, 0.054931640625, 9.0),
            ("0GAjIv8h", 50.671530961990356, 6.09715461730957, 16.0),
            ("~v2juONc--", 78.10350179672241, 178.41148853302002, 15.0),
            ("D@hV--", -47.1533203125, -92.8125, 3.0),
        ] {
            let link = shortlink(code).unwrap();
            assert_eq!(
                (link.lat, link.lon, link.zoom),
                (lat, lon, Some(zoom)),
                "{code}"
            );
        }
        // Upstream's deprecated-character cases decode alike.
        for (new, old) in [
            ("~v2juONc--", "@v2juONc--"),
            ("as3I3GpG~-", "as3I3GpG@-"),
            ("D~hV--", "D@hV--"),
            ("CO0O~m8--", "CO0O@m8--"),
        ] {
            assert_eq!(shortlink(new), shortlink(old));
        }
        for bad in ["", "---", "abc---", "a.b", "AAAAAAAAAAA", "ab-c"] {
            assert!(shortlink(bad).is_none(), "{bad}");
        }
    }
    #[test]
    fn ge0_decodes_like_organic_maps() {
        // Vectors from organicmaps libs/ge0/ge0_tests/parser_tests.cpp, with its 9-byte tolerances.
        let lat_eps = 8.0 / f64::from((1u32 << 30) - 1) * 180.0 / 1.77;
        let lon_eps = 8.0 / f64::from((1u32 << 30) - 1) * 360.0 / 1.77;
        for (code, lat, lon, zoom) in [
            ("Byqqqqqqqq", 45.0, 0.0, 4.25),
            ("B6qqqqqqqq", 90.0, 0.0, 4.25),
            ("BVVVVVVVVV", -90.0, 179.999999, 4.25),
            ("BP________", -0.000001, -0.000001, 4.25),
            ("Bzqqqqqqqq", 45.0, 45.0, 4.25),
            ("BaF6F6F6F6", -20.0, 20.0, 4.25),
            ("B4srhdHVVt", 64.5234, 12.1234, 4.25),
            ("B_________", 90.0, 179.999999, 4.25),
            ("Bqqqqqqqqq", 90.0, -180.0, 4.25),
            ("BAAAAAAAAA", -90.0, -180.0, 4.25),
            ("8wAAAAAAAA", 0.0, 0.0, 19.0),
            ("AwAAAAAAAA", 0.0, 0.0, 4.0),
        ] {
            let link = ge0(code).unwrap();
            assert!((link.lat - lat).abs() <= lat_eps, "{code} lat {}", link.lat);
            assert!((link.lon - lon).abs() <= lon_eps, "{code} lon {}", link.lon);
            assert_eq!(link.zoom, Some(zoom), "{code}");
        }
        for bad in [
            "Byqqqqqqq",
            "B",
            "",
            "!wAAAAAAAA",
            "/wAAAAAAAA",
            "AwAAAAAAAAs",
        ] {
            assert!(ge0(bad).is_none(), "{bad}");
        }
    }
    #[test]
    fn location_links_are_points_with_link_precision() {
        for (uri, name, precision) in [
            (
                "https://www.openstreetmap.org/?mlat=52.5186&mlon=13.3996",
                "52.5186, 13.3996",
                "~10 m",
            ),
            (
                "https://osm.org/?mlat=52.5&mlon=13.4&zoom=17#map=17/52.5/13.4",
                "52.50000, 13.40000",
                "~1 m",
            ),
            ("https://osm.org/go/0GAjIv8h?m", "50.6715, 6.0972", "~10 m"),
            (
                "https://www.openstreetmap.org/go/0EEQjE--",
                "51.51, 0.05",
                "~1 km",
            ),
            (
                "https://osmand.net/map?pin=52.51869,13.39960",
                "52.51869, 13.39960",
                "~1 m",
            ),
            (
                "https://osmand.net/go?lat=52.5186&lon=13.3996&z=12",
                "52.519, 13.400",
                "~100 m",
            ),
            (
                "https://omaps.app/B4srhdHVVt/Somewhere",
                "64.5, 12.1",
                "~10 km",
            ),
            ("https://omaps.app/8wAAAAAAAA", "0.00000, 0.00000", "~1 m"),
        ] {
            let doc = parse_link(uri).unwrap();
            assert_eq!(doc.name.as_deref(), Some(name), "{uri}");
            assert_eq!(doc.attributes["precision"], precision, "{uri}");
            assert_eq!(doc.type_name, "Place");
            assert!(matches!(doc.location_source, Some(LocationSource::Uri)));
            assert_eq!(super::super::classify(uri), Some("geo"), "{uri}");
        }
        for uri in [
            "https://www.openstreetmap.org/?mlat=91&mlon=0",
            "https://www.openstreetmap.org/?mlat=1",
            "https://www.openstreetmap.org/?mlat=x&mlon=1",
            "https://www.openstreetmap.org/#map=12/52.5/13.4",
            "https://www.openstreetmap.org/user/someone",
            "https://osmand.net/map?pin=52.5",
            "https://osmand.net/",
            "https://omaps.app/Byqqqqqqq",
            "https://example.org/go/0EEQjE--",
            "https://u@osm.org/go/0EEQjE--",
        ] {
            assert!(location_link(uri).is_none(), "{uri}");
        }
        assert_eq!(
            super::super::classify("https://www.openstreetmap.org/#map=12/52.5/13.4"),
            Some("web")
        );
    }
    #[test]
    fn full_grammar_and_precision() {
        for uri in [
            "geo:46.0051,8.9528",
            "geo:007.5,0008.0",
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
