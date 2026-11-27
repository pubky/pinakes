use crate::model::{LocationSource, Point, ResourceMeta};
use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use std::time::Duration;

pub const ATTRIBUTES: &[&str] = &["precision"];
/// Resolves `geo:` URIs and map links that name a point.
#[derive(Default)]
pub struct GeoResolver;

fn number(s: &str, signed: bool) -> Result<f64> {
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
    let (rest, query) = rest
        .split_once('?')
        .map_or((rest, None), |(r, q)| (r, Some(q)));
    let mut pieces = rest.split(';');
    let coords: Vec<_> = pieces.next().unwrap_or_default().split(',').collect();
    ensure!(
        (2..=3).contains(&coords.len()),
        "geo requires latitude,longitude[,altitude]"
    );
    let lat = number(coords[0], true)?;
    let lon = number(coords[1], true)?;
    ensure!(
        (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon),
        "coordinate out of range"
    );
    if let Some(alt) = coords.get(2) {
        number(alt, true)?;
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
    if let Some(query) = query {
        let (q_lat, q_lon, place) = android_query(query)?;
        let (lat_q, lon_q) = (number(q_lat, true)?, number(q_lon, true)?);
        // `geo:0,0?q=…` puts the point in `q` alone; another point before `?` must be the same.
        ensure!(
            (lat == 0.0 && lon == 0.0) || (lat == lat_q && lon == lon_q),
            "geo q names another point"
        );
        let name = place.unwrap_or_else(|| format!("{q_lat}, {q_lon}"));
        return point(uri, lat_q, lon_q, digits(&[q_lat, q_lon]), name);
    }
    let name = format!("{}, {}", coords[0], coords[1]);
    point(uri, lat, lon, digits(&coords[..2]), name)
}
/// Android's query on a `geo:` URI, `q=lat,lon` or `q=lat,lon(Name)`, and nothing else: the
/// coordinates as written and the name, decoded (`+` as a space). The coordinates keep the
/// strict number grammar; any other query is refused.
fn android_query(query: &str) -> Result<(&str, &str, Option<String>)> {
    let value = query
        .strip_prefix("q=")
        .ok_or_else(|| anyhow::anyhow!("unsupported geo query"))?;
    ensure!(query_text(value), "invalid geo query");
    let (coords, place) = match value.split_once('(') {
        Some((coords, rest)) => {
            let place = rest
                .strip_suffix(')')
                .ok_or_else(|| anyhow::anyhow!("unclosed geo label"))?;
            (coords, decode(place, true).and_then(|p| place_name(&p)))
        }
        None => (value, None),
    };
    let (lat, lon) = coords
        .split_once(',')
        .ok_or_else(|| anyhow::anyhow!("geo q requires latitude,longitude"))?;
    Ok((lat, lon, place))
}
/// Whether `s` is one query parameter's value as RFC 3986 writes it: unreserved and
/// sub-delimiter characters but `&`, `:`, `@`, `/`, `?` and well-formed percent escapes.
fn query_text(s: &str) -> bool {
    let mut bytes = s.bytes();
    while let Some(c) = bytes.next() {
        if c == b'%' {
            if !bytes.next().is_some_and(|c| c.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|c| c.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !(c.is_ascii_alphanumeric() || b"-._~!$'()*+,;=:@/?".contains(&c)) {
            return false;
        }
    }
    true
}
/// The most bytes of a place name read from a link.
pub const NAME_BYTES: usize = 256;
/// A link's text percent-decoded (with `+` as a space when `plus` is set), or `None` when it is
/// not UTF-8.
pub fn decode(raw: &str, plus: bool) -> Option<String> {
    let raw = if plus {
        std::borrow::Cow::Owned(raw.replace('+', " "))
    } else {
        std::borrow::Cow::Borrowed(raw)
    };
    percent_encoding::percent_decode_str(&raw)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}
/// A place's name as a map link spells it, decoded: control, format and bidirectional
/// characters and runs of whitespace become one space, the ends are trimmed and the text is cut
/// to [`NAME_BYTES`]. `None` when nothing is left, or when the text is only coordinates (a
/// dropped pin's "48.85, 2.29" or "48°51'N 2°17'E"), which name no place.
pub fn place_name(decoded: &str) -> Option<String> {
    let hidden = |c: char| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
    };
    let mut name = String::new();
    for word in decoded.split(hidden).filter(|w| !w.is_empty()) {
        if name.len() + word.len() + 1 > NAME_BYTES {
            // The last word that fits, cut at a character boundary.
            let room = NAME_BYTES.saturating_sub(name.len() + usize::from(!name.is_empty()));
            let mut end = room.min(word.len());
            while !word.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                if !name.is_empty() {
                    name.push(' ');
                }
                name.push_str(&word[..end]);
            }
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    (!name.is_empty() && !coordinates_only(&name)).then_some(name)
}
/// Whether a name is only a point: `lat,lon` in decimal degrees, or degrees, minutes and
/// seconds with their hemisphere letters.
fn coordinates_only(name: &str) -> bool {
    coordinates(name).is_some()
        || (name.contains('°')
            && name
                .chars()
                .all(|c| c.is_ascii_digit() || " .,°'\"′″NSEWnsew+-".contains(c)))
}
/// `lat,lon` in decimal degrees (spaces around either number allowed), as written, when both
/// are numbers in range.
pub fn coordinates(text: &str) -> Option<(&str, &str)> {
    let (lat, lon) = text.split_once(',')?;
    let (lat, lon) = (lat.trim_matches(' '), lon.trim_matches(' '));
    LocationLink::from_text(lat, lon, None)?;
    Some((lat, lon))
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
    // The name is the coordinates as written, however many digits: cut like any card's.
    doc.bounded(ATTRIBUTES)
}

/// A point named by a map link. Precision comes from the link's zoom when it has one, else from
/// the decimal digits of its coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct LocationLink {
    pub lat: f64,
    pub lon: f64,
    pub zoom: Option<f64>,
    text: Option<(String, String)>,
    /// The place's name, where the link carries one (Organic Maps).
    place: Option<String>,
}
impl LocationLink {
    /// A point from its coordinates as a link writes them, in decimal degrees and in range, and
    /// a web-map zoom (at most 25) when the link gives one.
    pub fn from_text(lat: &str, lon: &str, zoom: Option<&str>) -> Option<Self> {
        text_point(lat, lon, zoom.map(str::to_owned))
    }
    /// The precision label of the point (`~10 km` … `~1 m`), from the zoom or the digits.
    pub fn precision_label(&self) -> &'static str {
        PRECISION[self.precision()]
    }
    fn precision(&self) -> usize {
        match (self.zoom, &self.text) {
            (Some(zoom), _) => zoom_precision(zoom),
            (None, Some((lat, lon))) => digits(&[lat, lon]),
            (None, None) => 0,
        }
    }
    /// The point's name: its coordinates as written, or, from a zoom, rounded to its precision.
    pub fn name(&self) -> String {
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
    let (lat_n, lon_n) = (number(lat, true).ok()?, number(lon, true).ok()?);
    if !(-90.0..=90.0).contains(&lat_n) || !(-180.0..=180.0).contains(&lon_n) {
        return None;
    }
    let zoom = match zoom {
        Some(z) => Some(number(&z, false).ok().filter(|z| *z <= 25.0)?),
        None => None,
    };
    Some(LocationLink {
        lat: lat_n,
        lon: lon_n,
        zoom,
        text: zoom.is_none().then(|| (lat.to_owned(), lon.to_owned())),
        place: None,
    })
}
/// OSM marker links (`/?mlat=&mlon=`) and shortlinks (`/go/<code>`, `?m` marks a marker; not
/// with `node`, `way`, `relation`, `changeset`, `note` or `query`), OsmAnd
/// `map?pin=lat,lon` and `go?lat=&lon=&z=`, and Organic Maps `omaps.app/<ge0 code>[/<name>]`
/// (the name names the card).
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
            // A link that names an object, changeset, note or query is about that, not about
            // its map centre: objects are OSM aliases, the rest stay refused.
            if url.query_pairs().any(|(k, _)| {
                matches!(
                    k.as_ref(),
                    "node" | "way" | "relation" | "changeset" | "note" | "query"
                )
            }) {
                return None;
            }
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
            let (code, name) = path.split_once('/').unwrap_or((path, ""));
            let mut link = ge0(code)?;
            link.place = organic_maps_name(name.trim_end_matches('/'));
            Some(link)
        }
        _ => None,
    }
}
/// The name after an Organic Maps code (`libs/ge0/parser.cpp`, `DecodeName`): percent-decoded,
/// then `_` read as a space and a space as `_`, as the app swapped them when it wrote the link.
fn organic_maps_name(raw: &str) -> Option<String> {
    let swapped: String = decode(raw, false)?
        .chars()
        .map(|c| match c {
            '_' => ' ',
            ' ' => '_',
            c => c,
        })
        .collect();
    place_name(&swapped)
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
        place: None,
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
        place: None,
    })
}
/// The `Place` card of a map link that names a place or a point (the map sources' cards):
/// named by the place, or by the point as [`LocationLink::name`] names it; at the point (from
/// the link, `location_source` `uri`) with its `precision`.
pub fn place_card(
    mut doc: ResourceMeta,
    site: &str,
    name: Option<String>,
    point: Option<&LocationLink>,
) -> ResourceMeta {
    doc.set_type("Place");
    doc.site = site.into();
    doc.name = name.or_else(|| point.map(LocationLink::name));
    if let Some(p) = point {
        doc.location = Some(Point {
            lat: p.lat,
            lon: p.lon,
        });
        doc.location_source = Some(LocationSource::Uri);
        doc.attributes
            .insert("precision".into(), p.precision_label().into());
    }
    doc
}
/// What a map link's text (a search, a place's name, `q=`) names: a name, or a point written
/// `lat,lon` or `lat,lon(Name)`, with `loc:` before it as Google allows. A place ID
/// (`place_id:…`) names nothing.
pub fn text_place(text: &str) -> (Option<String>, Option<LocationLink>) {
    let text = text.trim();
    let text = text
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case("loc:"))
        .map_or(text, |_| text[4..].trim_start());
    if text
        .get(..9)
        .is_some_and(|p| p.eq_ignore_ascii_case("place_id:"))
    {
        return (None, None);
    }
    if let Some((coords, label)) = text.strip_suffix(')').and_then(|t| t.split_once('(')) {
        if let Some((lat, lon)) = coordinates(coords) {
            return (place_name(label), LocationLink::from_text(lat, lon, None));
        }
    }
    match coordinates(text) {
        Some((lat, lon)) => (None, LocationLink::from_text(lat, lon, None)),
        None => (place_name(text), None),
    }
}
/// Google's own domains, after `google.`, as Google lists them
/// (`https://www.google.com/supported_domains`).
const GOOGLE_DOMAINS: &[&str] = &[
    "com", "ad", "ae", "com.af", "com.ag", "al", "am", "co.ao", "com.ar", "as", "at", "com.au",
    "az", "ba", "com.bd", "be", "bf", "bg", "com.bh", "bi", "bj", "com.bn", "com.bo", "com.br",
    "bs", "bt", "co.bw", "by", "com.bz", "ca", "cd", "cf", "cg", "ch", "ci", "co.ck", "cl", "cm",
    "cn", "com.co", "co.cr", "com.cu", "cv", "com.cy", "cz", "de", "dj", "dk", "dm", "com.do",
    "dz", "com.ec", "ee", "com.eg", "es", "com.et", "fi", "com.fj", "fm", "fr", "ga", "ge", "gg",
    "com.gh", "com.gi", "gl", "gm", "gr", "com.gt", "gy", "com.hk", "hn", "hr", "ht", "hu",
    "co.id", "ie", "co.il", "im", "co.in", "iq", "is", "it", "je", "com.jm", "jo", "co.jp",
    "co.ke", "com.kh", "ki", "kg", "co.kr", "com.kw", "kz", "la", "com.lb", "li", "lk", "co.ls",
    "lt", "lu", "lv", "com.ly", "co.ma", "md", "me", "mg", "mk", "ml", "com.mm", "mn", "com.mt",
    "mu", "mv", "mw", "com.mx", "com.my", "co.mz", "com.na", "com.ng", "com.ni", "ne", "nl", "no",
    "com.np", "nr", "nu", "co.nz", "com.om", "com.pa", "com.pe", "com.pg", "com.ph", "com.pk",
    "pl", "pn", "com.pr", "ps", "pt", "com.py", "com.qa", "ro", "ru", "rw", "com.sa", "com.sb",
    "sc", "se", "com.sg", "sh", "si", "sk", "com.sl", "sn", "so", "sm", "sr", "st", "com.sv", "td",
    "tg", "co.th", "com.tj", "tl", "tm", "tn", "to", "com.tr", "tt", "com.tw", "co.tz", "com.ua",
    "co.ug", "co.uk", "com.uy", "co.uz", "com.vc", "co.ve", "co.vi", "com.vn", "vu", "ws", "rs",
    "co.za", "co.zm", "co.zw", "cat",
];
/// Whether `host` is Google's: `google.{domain}`, `www.google.{domain}` or
/// `maps.google.{domain}` on one of [`GOOGLE_DOMAINS`]; `Some(true)` for `maps.`.
pub fn google_host(host: &str) -> Option<bool> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let (maps, rest) = match host.strip_prefix("maps.") {
        Some(rest) => (true, rest),
        None => (false, host.strip_prefix("www.").unwrap_or(&host)),
    };
    GOOGLE_DOMAINS
        .contains(&rest.strip_prefix("google.")?)
        .then_some(maps)
}
/// A Google Maps link's address: an `https` or `http` URL on Google's host (`google_host`)
/// without a port or user name, and the path segments after `/maps` (all of them on
/// `maps.google.{domain}`, where the path may also be `/`).
pub fn google_maps(uri: &str) -> Option<(url::Url, Vec<String>)> {
    let url = url::Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let maps_host = google_host(url.host_str()?)?;
    let mut segments: Vec<String> = url
        .path_segments()?
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if segments.first().is_some_and(|s| s == "maps") {
        segments.remove(0);
    } else if !(maps_host && segments.is_empty()) {
        return None;
    }
    Some((url, segments))
}
/// The CID of the place a Google Maps link names, as a decimal number: the second number of
/// the place's feature ID (`0x{hex}:0x{hex}`), in its `data` segment (`!1s0x…:0x…`) or its
/// `ftid` parameter. `None` when the link names no such ID, or two different ones.
pub fn google_cid(uri: &str) -> Option<String> {
    let (url, segments) = google_maps(uri)?;
    let mut ids: Vec<&str> = segments
        .iter()
        .filter_map(|s| s.strip_prefix("data="))
        .flat_map(|data| data.split('!'))
        .filter_map(|token| token.strip_prefix("1s"))
        .filter(|id| id.starts_with("0x"))
        .collect();
    let ftid: Vec<String> = url
        .query_pairs()
        .filter(|(k, _)| k == "ftid")
        .map(|(_, v)| v.into_owned())
        .collect();
    ids.extend(ftid.iter().map(String::as_str));
    let mut cids: Vec<u64> = ids
        .into_iter()
        .map(|id| feature_cid(&percent_encoding::percent_decode_str(id).decode_utf8_lossy()))
        .collect::<Option<_>>()?;
    cids.sort_unstable();
    cids.dedup();
    match cids.as_slice() {
        [cid] => Some(cid.to_string()),
        _ => None,
    }
}
/// The CID of a feature ID, `0x{cell}:0x{cid}` with 1 to 16 hex digits each; never 0.
fn feature_cid(id: &str) -> Option<u64> {
    let (cell, cid) = id.split_once(':')?;
    let hex = |s: &str| {
        let digits = s.strip_prefix("0x")?;
        ((1..=16).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| u64::from_str_radix(digits, 16).ok())
            .flatten()
    };
    hex(cell)?;
    hex(cid).filter(|cid| *cid != 0)
}
pub fn parse_link(uri: &str) -> Result<ResourceMeta> {
    let link = location_link(uri).ok_or_else(|| anyhow::anyhow!("not a location link"))?;
    let name = link.place.clone().unwrap_or_else(|| link.name());
    point(uri, link.lat, link.lon, link.precision(), name)
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
        parse_link(uri)
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::ZERO
    }
}
/// A small deterministic generator (xorshift) and the link mutations the map readers'
/// property tests share, so each run is the same.
#[cfg(test)]
pub(crate) mod fuzz {
    pub struct Gen(pub u64);
    impl Gen {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        pub fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        pub fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
            from[self.below(from.len())]
        }
        /// `base` with up to three random edits at character boundaries: a part inserted, a
        /// range cut out, or the rest replaced by a part.
        pub fn mutate(&mut self, base: &str, parts: &[&str]) -> String {
            let mut link = base.to_owned();
            for _ in 0..self.below(4) {
                let bounds: Vec<usize> = link
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain([link.len()])
                    .collect();
                let at = bounds[self.below(bounds.len())];
                match self.below(3) {
                    0 => link.insert_str(at, self.pick(parts)),
                    1 => {
                        let ends: Vec<usize> = bounds.into_iter().filter(|b| *b >= at).collect();
                        let end = ends[self.below(ends.len())];
                        link.replace_range(at..end, "");
                    }
                    _ => {
                        link.truncate(at);
                        link.push_str(self.pick(parts));
                    }
                }
            }
            link
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortlinks_decode_like_openstreetmap_org() {
        // Where openstreetmap.org/go/<code> redirects for these codes.
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
                "Somewhere",
                "~10 km",
            ),
            (
                "https://omaps.app/B4srhdHVVt/Tour_Eiffel/",
                "Tour Eiffel",
                "~10 km",
            ),
            // The app writes a name's `_` as a space (`%20`) and a space as `_`.
            (
                "https://omaps.app/B4srhdHVVt/Caf%C3%A9_du%20Lac",
                "Café du_Lac",
                "~10 km",
            ),
            // No name left, or only coordinates: the point names the card.
            (
                "https://omaps.app/B4srhdHVVt/%E2%80%AE_",
                "64.5, 12.1",
                "~10 km",
            ),
            (
                "https://omaps.app/B4srhdHVVt/64.5,12.1",
                "64.5, 12.1",
                "~10 km",
            ),
            ("https://omaps.app/B4srhdHVVt/%FF", "64.5, 12.1", "~10 km"),
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
            // Object, changeset, note and query links are not map points.
            "https://osm.org/go/0EEQjE--?way=50637691",
            "https://osm.org/go/0EEQjE--?node=1&m",
            "https://osm.org/go/0EEQjE--?relation=7",
            "https://osm.org/go/0EEQjE--?way=1&way=2",
            "https://osm.org/go/0EEQjE--?changeset=1",
            "https://osm.org/go/0EEQjE--?note=1",
            "https://osm.org/go/0EEQjE--?query=52.5,13.4",
            "https://www.openstreetmap.org/?mlat=52.5&mlon=13.4&note=1",
        ] {
            assert!(location_link(uri).is_none(), "{uri}");
        }
        for (uri, route) in [
            ("https://osm.org/go/0EEQjE--?way=50637691", Some("osm")),
            ("https://osm.org/go/0EEQjE--?way=1&way=2", Some("web")),
            ("https://osm.org/go/0EEQjE--?changeset=1", Some("web")),
            ("https://osm.org/go/0EEQjE--?note=1", Some("web")),
            ("https://osm.org/go/0EEQjE--?query=52.5,13.4", Some("web")),
        ] {
            assert_eq!(super::super::classify(uri), route, "{uri}");
        }
        assert_eq!(
            super::super::classify("https://www.openstreetmap.org/#map=12/52.5/13.4"),
            Some("web")
        );
    }
    /// Coordinates written with hundreds of digits name the point within a card's name cap.
    #[test]
    fn a_long_coordinate_names_the_point_within_the_cap() {
        let doc = parse(&format!("geo:46.{},8.5", "1".repeat(1000))).unwrap();
        assert!(doc.name.as_ref().unwrap().len() <= 512);
        assert!(doc.name.unwrap().starts_with("46.1111"));
        assert_eq!(doc.location.unwrap().lon, 8.5);
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
    /// Android's `q=lat,lon(Name)` on a `geo:` URI names the card; the coordinates keep the
    /// strict grammar, and any other query is refused.
    #[test]
    fn a_geo_query_names_the_point() {
        for (uri, name, point, precision) in [
            (
                "geo:0,0?q=48.8584,2.2945(Tour%20Eiffel)",
                "Tour Eiffel",
                (48.8584, 2.2945),
                "~10 m",
            ),
            (
                "geo:48.8584,2.2945?q=48.8584,2.2945(Tour+Eiffel)",
                "Tour Eiffel",
                (48.8584, 2.2945),
                "~10 m",
            ),
            (
                "geo:48.8584,2.2945;u=10?q=48.8584,2.2945(Caf%C3%A9%20(Nord))",
                "Café (Nord)",
                (48.8584, 2.2945),
                "~10 m",
            ),
            (
                "geo:0,0?q=-33.8568,151.2153",
                "-33.8568, 151.2153",
                (-33.8568, 151.2153),
                "~10 m",
            ),
            ("geo:0,0?q=1.5,2.5()", "1.5, 2.5", (1.5, 2.5), "~10 km"),
            (
                "geo:0,0?q=1.5,2.5(%E2%80%AE)",
                "1.5, 2.5",
                (1.5, 2.5),
                "~10 km",
            ),
            ("geo:0,0?q=1.5,2.5(3,4)", "1.5, 2.5", (1.5, 2.5), "~10 km"),
        ] {
            let doc = parse(uri).unwrap();
            assert_eq!(doc.name.as_deref(), Some(name), "{uri}");
            let location = doc.location.unwrap();
            assert_eq!((location.lat, location.lon), point, "{uri}");
            assert_eq!(doc.attributes["precision"], precision, "{uri}");
            assert_eq!(super::super::classify(uri), Some("geo"), "{uri}");
        }
        for uri in [
            "geo:1,2?q=3,4(Elsewhere)",
            "geo:0,0?q=Paris",
            "geo:0,0?q=91,0(x)",
            "geo:0,0?q=1,2(x",
            "geo:0,0?q=1,2(x)y",
            "geo:0,0?q=+1,2(x)",
            "geo:0,0?q=1.,2",
            "geo:0,0?q=1,2,3",
            "geo:0,0?q=1,2(%ZZ)",
            "geo:0,0?q=1,2(a b)",
            "geo:0,0?q=1,2&z=3",
            "geo:0,0?z=3",
            "geo:0,0?q=",
            "geo:0,0?",
            "geo:91,0?q=1,2(x)",
            "geo:x,0?q=1,2(x)",
            "geo:1,2;u=-1?q=1,2(x)",
        ] {
            assert!(parse(uri).is_err(), "{uri}");
        }
    }
    /// A link's name: hidden characters and whitespace runs are one space, the ends trimmed,
    /// the text cut to its bytes at a character, and coordinates are no name.
    #[test]
    fn place_names_are_clean_and_bounded() {
        assert_eq!(
            place_name(" Tour\t\nEiffel\u{200b} \u{202e}x ").as_deref(),
            Some("Tour Eiffel x")
        );
        for name in [
            "",
            " \u{feff} ",
            "48.85,2.29",
            "48.85, 2.29",
            "48°51'30.2\"N 2°17'40.2\"E",
            "48°51′N, 2°17′E",
        ] {
            assert_eq!(place_name(name), None, "{name:?}");
        }
        for name in ["Pier 39", "1,Infinite Loop", "91,0", "Platz 1°"] {
            assert_eq!(place_name(name).as_deref(), Some(name), "{name:?}");
        }
        let long = "é".repeat(300);
        let cut = place_name(&long).unwrap();
        assert!(cut.len() <= NAME_BYTES && cut.len() > NAME_BYTES - 2);
        let words = "word ".repeat(100);
        let cut = place_name(&words).unwrap();
        assert!(cut.len() <= NAME_BYTES && !cut.ends_with(' '), "{cut:?}");
    }
    /// Property: on 20,000 random and malformed `geo:` URIs and Organic Maps links, the readers
    /// never panic, and a card they make has a bounded name without control characters and a
    /// point in range.
    #[test]
    fn random_geo_links_never_panic_and_name_bounded_points() {
        const BASES: &[&str] = &[
            "geo:46.0051,8.9528",
            "geo:46.0051,8.9528,7;crs=wgs84;u=5",
            "geo:0,0?q=48.8584,2.2945(Tour%20Eiffel)",
            "geo:48.8584,2.2945?q=48.8584,2.2945(Tour+Eiffel)",
            "https://omaps.app/B4srhdHVVt/Tour_Eiffel",
            "https://omaps.app/8wAAAAAAAA/Caf%C3%A9_du%20Lac",
            "https://osmand.net/go?lat=52.5186&lon=13.3996&z=12",
            "https://osm.org/go/0GAjIv8h?m",
        ];
        const PARTS: &[&str] = &[
            "0",
            "0,0",
            "48.8584",
            "-2.29",
            "91",
            ",",
            ";",
            "u=",
            "crs=wgs84",
            "?",
            "q=",
            "&",
            "(",
            ")",
            "+",
            "%",
            "%2",
            "%20",
            "%C3%A9",
            "%FF",
            "%00",
            "%E2%80%AE",
            "%0A",
            "_",
            "Tour",
            "é",
            "東京",
            "😀",
            "\u{301}",
            " ",
            "#",
            ".",
            "/",
        ];
        let mut gen = fuzz::Gen(0x9e37_79b9_7f4a_7c15);
        let (mut made, mut refused) = (0, 0);
        for _ in 0..20_000 {
            let base = gen.pick(BASES);
            let uri = gen.mutate(base, PARTS);
            let doc = if uri.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("geo:")) {
                parse(&uri)
            } else {
                parse_link(&uri)
            };
            let Ok(doc) = doc else {
                refused += 1;
                continue;
            };
            made += 1;
            let name = doc.name.unwrap();
            assert!(
                !name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control),
                "{uri:?} -> {name:?}"
            );
            let p = doc.location.unwrap();
            assert!((-90.0..=90.0).contains(&p.lat) && (-180.0..=180.0).contains(&p.lon));
        }
        assert!(made > 3_000 && refused > 3_000, "{made} {refused}");
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

    /// openstreetmap-website `ShortLink.encode` for `digits` characters (no partial zoom).
    fn shortlink_code(x: u32, y: u32, digits: usize) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_~";
        let mut code = 0u64;
        for i in (0..32).rev() {
            code = (code << 1) | u64::from((x >> i) & 1);
            code = (code << 1) | u64::from((y >> i) & 1);
        }
        (0..digits)
            .map(|i| ALPHABET[((code >> (58 - 6 * i)) & 0x3f) as usize] as char)
            .collect()
    }
    proptest::proptest! {
        /// A shortlink decodes to the corner of the cell its point is in, at its zoom.
        #[test]
        fn shortlinks_round_trip(x in proptest::num::u32::ANY, y in proptest::num::u32::ANY, digits in 1usize..=10) {
            let code = shortlink_code(x, y, digits);
            let link = shortlink(&code).expect("a link");
            let (lon, lat) = (f64::from(x) * 360.0 / 4_294_967_296.0 - 180.0, f64::from(y) * 180.0 / 4_294_967_296.0 - 90.0);
            let cell = 2f64.powi(-(3 * digits as i32));
            let shown = format!("{code} {link:?} {lat} {lon}");
            proptest::prop_assert!(lon >= link.lon - 1e-9 && lon - link.lon <= 360.0 * cell + 1e-9, "{}", shown);
            proptest::prop_assert!(lat >= link.lat - 1e-9 && lat - link.lat <= 180.0 * cell + 1e-9, "{}", shown);
            proptest::prop_assert_eq!(link.zoom, Some(3.0 * digits as f64 - 8.0));
        }
        /// Any shortlink, ge0 or OsmAnd/Organic Maps link: no panic, and the point in range.
        #[test]
        fn any_map_link_reads_in_range(
            code in "[A-Za-z0-9_~@-]{0,12}",
            host in proptest::sample::select(vec!["www.openstreetmap.org/go/", "osm.org/go/", "omaps.app/", "osmand.net/go?lat=", "osmand.net/map?pin="]),
            tail in "[0-9.,%_A-Za-z-]{0,16}",
        ) {
            let uri = format!("https://{host}{code}{tail}");
            for link in [location_link(&uri), shortlink(&code), ge0(&code)].into_iter().flatten() {
                let shown = format!("{uri} {link:?}");
                proptest::prop_assert!((-90.0..=90.0).contains(&link.lat) && (-180.0..=180.0).contains(&link.lon), "{}", shown);
            }
        }
        /// `geo:lat,lon`: that point; anything else after `geo:` is no panic.
        #[test]
        fn geo_uris_give_their_point(lat in -90.0f64..=90.0, lon in -180.0f64..=180.0, junk in "[0-9.,;=?a-z()%+-]{0,24}") {
            let uri = format!("geo:{lat:.6},{lon:.6}");
            let doc = parse(&uri).expect("a point");
            let at = doc.location.expect("a location");
            proptest::prop_assert!((at.lat - lat).abs() < 1e-6 && (at.lon - lon).abs() < 1e-6);
            if let Ok(doc) = parse(&format!("geo:{junk}")) {
                if let Some(at) = doc.location {
                    proptest::prop_assert!((-90.0..=90.0).contains(&at.lat) && (-180.0..=180.0).contains(&at.lon));
                }
            }
        }
    }
}
