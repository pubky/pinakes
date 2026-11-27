//! Google Maps links: a place's name and point read from the link itself. Google's pages, embeds
//! and APIs are never asked. A short link (`maps.app.goo.gl/{code}`, `goo.gl/maps/{code}`) is the
//! one request: a GET whose redirect is not followed, its `Location` read and its body never; the
//! card is then the target link's, and the short link becomes an alias of the target.
use super::{Context, Example, Source};
use crate::{
    model::ResourceMeta,
    resolver::{
        egress::FetchOptions,
        geo::{self, LocationLink},
        ErrorClass,
    },
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct GoogleMaps;

const SITE: &str = "Google Maps";

/// What a Google Maps link says of its place: a name, a point, or both.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub name: Option<String>,
    pub point: Option<LocationLink>,
}

/// The place a Google Maps link names, on any of Google's domains (`geo::google_maps`):
/// - `/maps/place/{name}[/@{lat},{lon},{zoom}z][/data=…]`: the pin in `data`
///   (`!3d{lat}!4d{lon}`) before the map's centre (`@`);
/// - `/maps/search/{text}[/@…]` and `/maps/search/?api=1&query={text}` (`query_place_id` is not
///   read);
/// - `/maps/@{lat},{lon},{zoom}z`;
/// - `/maps?q={text}`, `/maps?ll={lat},{lon}[&z={zoom}]`, and `maps.google.{domain}/?q=…`.
///
/// A text is a name, or a point (`lat,lon`, `lat,lon(Name)`, `loc:lat,lon`); a name that is only
/// coordinates (a dropped pin's) is the point's. `None` for directions (`/maps/dir/…`, `saddr`,
/// `daddr`), a place ID or CID alone (`?cid=`, `?ftid=`, `q=place_id:…`), and any link that
/// names neither a name nor a point.
pub fn place(uri: &str) -> Option<Place> {
    let (url, segments) = geo::google_maps(uri)?;
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if url
        .query_pairs()
        .any(|(k, _)| matches!(k.as_ref(), "saddr" | "daddr" | "dirflg"))
    {
        return None;
    }
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    let (name, point) = match segments.as_slice() {
        [] => {
            let (name, point) = query("q").map(|q| geo::text_place(&q)).unwrap_or_default();
            let centre = query("ll").and_then(|ll| {
                let (lat, lon) = geo::coordinates(&ll)?;
                let zoom = query("z");
                LocationLink::from_text(lat, lon, zoom.as_deref())
                    .or_else(|| LocationLink::from_text(lat, lon, None))
            });
            (name, point.or(centre))
        }
        ["search"] if query("api").as_deref() == Some("1") => {
            query("query").map(|q| geo::text_place(&q))?
        }
        ["place" | "search", text, rest @ ..] => path(Some(*text), rest),
        [at, ..] if at.starts_with('@') => path(None, &segments),
        _ => return None,
    };
    (name.is_some() || point.is_some()).then_some(Place { name, point })
}

/// A place or search path: its text (`+` as a space, percent-decoded), then the pin in its
/// `data`, the text's own coordinates, or the map's centre (`@`), in that order.
fn path(text: Option<&str>, rest: &[&str]) -> (Option<String>, Option<LocationLink>) {
    let (name, text_point) = text
        .filter(|t| !t.starts_with('@') && !t.starts_with("data="))
        .and_then(|t| geo::decode(t, true))
        .map(|t| geo::text_place(&t))
        .unwrap_or_default();
    let pin = rest
        .iter()
        .filter_map(|s| s.strip_prefix("data="))
        .find_map(pin);
    let centre = rest.iter().find_map(|s| centre(s));
    (name, pin.or(text_point).or(centre))
}

/// The pin of a `data` segment: the first `!3d{lat}` followed by `!4d{lon}`.
fn pin(data: &str) -> Option<LocationLink> {
    let tokens: Vec<&str> = data.split('!').collect();
    tokens.windows(2).find_map(|pair| {
        let lat = pair[0].strip_prefix("3d")?;
        let lon = pair[1].strip_prefix("4d")?;
        LocationLink::from_text(lat, lon, None)
    })
}

/// The map's centre of an `@{lat},{lon}[,{zoom}z|,…]` segment, with the zoom when it has one.
fn centre(segment: &str) -> Option<LocationLink> {
    let mut parts = segment.strip_prefix('@')?.split(',');
    let (lat, lon) = (parts.next()?, parts.next()?);
    match parts.next().and_then(|z| z.strip_suffix('z')) {
        Some(zoom) => LocationLink::from_text(lat, lon, Some(zoom)),
        None => LocationLink::from_text(lat, lon, None),
    }
}

/// The address a short link is asked at, `https://maps.app.goo.gl/{code}` or
/// `https://goo.gl/maps/{code}` (its query, a share's `g_st`, dropped): a code of 1 to 64
/// letters, digits, `_` and `-`.
pub fn short_link(uri: &str) -> Option<String> {
    let url = super::kit::web_url(uri)?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let (base, code) = match (host.as_str(), segments.as_slice()) {
        ("maps.app.goo.gl", [code]) => ("https://maps.app.goo.gl/", *code),
        ("goo.gl" | "www.goo.gl", ["maps", code]) => ("https://goo.gl/maps/", *code),
        _ => return None,
    };
    ((1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then(|| format!("{base}{code}"))
}

/// The Google Maps link a short link's redirect names, when this source reads it: `location`
/// resolved against the short link, through Google's consent page (`consent.google.com`, whose
/// `continue` is the target) at most once.
pub fn target(short: &str, location: &str) -> Option<String> {
    let mut url = url::Url::parse(short).ok()?.join(location).ok()?;
    if url.host_str() == Some("consent.google.com") {
        let next = url
            .query_pairs()
            .find(|(k, _)| k == "continue")
            .map(|(_, v)| v.into_owned())?;
        url = url::Url::parse(&next).ok()?;
    }
    place(url.as_str()).is_some().then(|| url.to_string())
}

/// What a short link's answer makes of its card: the target's card, `missing`, or an error.
pub fn short_card(
    doc: ResourceMeta,
    missing: ResourceMeta,
    short: &str,
    status: u16,
    location: Option<&str>,
) -> Result<ResourceMeta> {
    match status {
        301 | 302 | 303 | 307 | 308 => {
            let Some(target) = location.and_then(|l| target(short, l)) else {
                // A redirect to anything but a place or a point this source reads (directions,
                // a list, a CID alone, another site) names nothing the link can say.
                return Ok(missing);
            };
            let place = place(&target).context("target")?;
            let mut doc = geo::place_card(doc, SITE, place.name, place.point.as_ref());
            doc.source_root = Some(target);
            Ok(doc)
        }
        400 | 404 | 410 => Ok(missing),
        status => {
            Err(
                anyhow::Error::new(ErrorClass(if status < 400 { "parse" } else { "http_4xx" }))
                    .context(format!("short link answered {status} without a redirect")),
            )
        }
    }
}

#[async_trait]
impl Source for GoogleMaps {
    fn name(&self) -> &'static str {
        "google_maps"
    }
    fn reads(&self, uri: &str) -> bool {
        place(uri).is_some() || short_link(uri).is_some()
    }
    fn facts(&self) -> &'static [&'static str] {
        geo::ATTRIBUTES
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.google.com/maps/place/Eiffel+Tower/@48.8583701,2.2919064,17z/data=!3m1!4b1!4m6!3m5!1s0x47e66e2964e34e2d:0x8ddca9ee380ef7e0!8m2!3d48.8583701!4d2.2944813!16zL20vMDJqODE",
                card_type: "Place",
            },
            Example {
                uri: "https://www.google.com/maps/search/?api=1&query=47.5951518,-122.3316393",
                card_type: "Place",
            },
            Example {
                uri: "https://maps.app.goo.gl/9ivZQ71CKMqRPDtQA",
                card_type: "Place",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        if let Some(place) = place(uri) {
            return Ok(geo::place_card(
                cx.card(uri, self),
                SITE,
                place.name,
                place.point.as_ref(),
            ));
        }
        let short = short_link(uri).context("not a Google Maps link")?;
        // The redirect alone: never followed, and no body is read (a body is refused at once).
        let options = FetchOptions {
            keep_redirect: true,
            body_cap: Some(0),
            ..FetchOptions::default()
        };
        let fetched = match cx.fetch(&short, &options).await {
            Ok(fetched) => fetched,
            Err(error) if crate::resolver::egress::over_cap(&error) => {
                return Err(anyhow::Error::new(ErrorClass("parse"))
                    .context("short link answered a page, not a redirect"))
            }
            Err(error) => return Err(error),
        };
        short_card(
            cx.card(uri, self),
            cx.missing(uri, self),
            &short,
            fetched.status,
            fetched.location.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LocationSource, Status};

    fn card(uri: &str) -> ResourceMeta {
        let place = place(uri).unwrap_or_else(|| panic!("{uri}"));
        geo::place_card(
            ResourceMeta::new(uri, "google_maps", 60),
            SITE,
            place.name,
            place.point.as_ref(),
        )
    }

    /// Real link forms, with the name and point each card takes from the link alone.
    #[test]
    fn place_links_name_their_place_and_point() {
        for (uri, name, point, precision) in [
            // A place link: the pin in `data` wins over the map's centre.
            (
                "https://www.google.com/maps/place/Eiffel+Tower/@48.8583701,2.2919064,17z/data=!3m1!4b1!4m6!3m5!1s0x47e66e2964e34e2d:0x8ddca9ee380ef7e0!8m2!3d48.8583701!4d2.2944813!16zL20vMDJqODE?entry=ttu",
                "Eiffel Tower",
                Some((48.8583701, 2.2944813)),
                Some("~1 m"),
            ),
            // Recorded short links' targets (tests/fixtures/sources/google_maps).
            (
                "https://www.google.com/maps/place/Falafel+M.+Sahyoun/@33.8904447,35.5066505,17z/data=!3m1!4b1!4m6!3m5!1s0x151f16e2123697fd:0x8e6626b678863990!8m2!3d33.8904447!4d35.5066505!16s%2Fg%2F1td7wffc?utm_source=mstt_1&coh=164777&entry=tt&shorturl=1",
                "Falafel M. Sahyoun",
                Some((33.8904447, 35.5066505)),
                Some("~1 m"),
            ),
            (
                "https://www.google.com/maps/place/Mollafenari,+Elizi,+Bab-%C4%B1+Ali+Cd.+No:27,+34120+Fatih%2F%C4%B0stanbul,+Turkey/data=!4m2!3m1!1s0x14cab996a54299cb:0x40acb197d028d115?utm_source=mstt_1&entry=gps",
                "Mollafenari, Elizi, Bab-ı Ali Cd. No:27, 34120 Fatih/İstanbul, Turkey",
                None,
                None,
            ),
            (
                "https://www.google.com/maps/search/5.811698,+-55.118891?entry=tts&g_ep=EgoyMDI1MDEyOS4xIPu8ASoASAFQAw%3D%3D",
                "5.811698, -55.118891",
                Some((5.811698, -55.118891)),
                Some("~1 m"),
            ),
            // Without a pin, the map's centre, at the zoom's precision.
            (
                "https://www.google.com/maps/place/Kunsthaus+Z%C3%BCrich/@47.3703,8.5481,15z",
                "Kunsthaus Zürich",
                Some((47.3703, 8.5481)),
                Some("~10 m"),
            ),
            // A dropped pin's name is its coordinates: the card is the point's.
            (
                "https://www.google.com/maps/place/48%C2%B051'30.2%22N+2%C2%B017'40.2%22E/@48.8583889,2.2945,17z/data=!3m1!4b1!4m4!3m3!8m2!3d48.8583889!4d2.2945",
                "48.8583889, 2.2945",
                Some((48.8583889, 2.2945)),
                Some("~10 m"),
            ),
            (
                "https://www.google.com/maps/@46.0051,8.9528,14z",
                "46.0051, 8.9528",
                Some((46.0051, 8.9528)),
                Some("~10 m"),
            ),
            (
                "https://www.google.com/maps/search/Eiffel+Tower/@48.85,2.29,13z",
                "Eiffel Tower",
                Some((48.85, 2.29)),
                Some("~10 m"),
            ),
            (
                "https://www.google.com/maps/search/?api=1&query=47.5951518,-122.3316393",
                "47.5951518, -122.3316393",
                Some((47.5951518, -122.3316393)),
                Some("~1 m"),
            ),
            (
                "https://www.google.com/maps/search/?api=1&query=centurylink+field&query_place_id=ChIJKxjxuaNqkFQR3CK6O1HNNqY",
                "centurylink field",
                None,
                None,
            ),
            (
                "https://www.google.com/maps?q=48.8584,2.2945",
                "48.8584, 2.2945",
                Some((48.8584, 2.2945)),
                Some("~10 m"),
            ),
            (
                "https://maps.google.com/?q=Tour+Eiffel",
                "Tour Eiffel",
                None,
                None,
            ),
            (
                "https://maps.google.com/maps?q=loc:48.8584,2.2945(Tour+Eiffel)",
                "Tour Eiffel",
                Some((48.8584, 2.2945)),
                Some("~10 m"),
            ),
            (
                "https://maps.google.com/maps?q=Tour+Eiffel&ftid=0x47e66e2964e34e2d:0x8ddca9ee380ef7e0&entry=gps",
                "Tour Eiffel",
                None,
                None,
            ),
            (
                "https://www.google.com/maps?ll=47.37,8.54&z=12",
                "47.370, 8.540",
                Some((47.37, 8.54)),
                Some("~100 m"),
            ),
            (
                "https://www.google.co.uk/maps/place/Big+Ben/@51.5007292,-0.1268141,17z",
                "Big Ben",
                Some((51.5007292, -0.1268141)),
                Some("~1 m"),
            ),
            (
                "https://google.de/maps/place/Brandenburger+Tor",
                "Brandenburger Tor",
                None,
                None,
            ),
            (
                "http://maps.google.com.au/maps?q=-33.8568,151.2153",
                "-33.8568, 151.2153",
                Some((-33.8568, 151.2153)),
                Some("~10 m"),
            ),
        ] {
            let doc = card(uri);
            assert_eq!(doc.type_name, "Place", "{uri}");
            assert_eq!(doc.site, "Google Maps");
            assert_eq!(doc.name.as_deref(), Some(name), "{uri}");
            assert_eq!(
                doc.location.as_ref().map(|p| (p.lat, p.lon)),
                point,
                "{uri}"
            );
            assert_eq!(
                doc.attributes.get("precision").map(String::as_str),
                precision,
                "{uri}"
            );
            assert_eq!(
                matches!(doc.location_source, Some(LocationSource::Uri)),
                point.is_some()
            );
            assert!(GoogleMaps.reads(uri));
            assert_eq!(crate::resolver::classify(uri), Some("official"), "{uri}");
            doc.bounded(crate::sources::attributes()).unwrap();
        }
    }

    /// Links that name no place or point stay on their route: a CID alone, directions, lists of
    /// places, other Google pages and other sites.
    #[test]
    fn other_links_are_not_read() {
        for uri in [
            "https://www.google.com/maps?cid=10222232094831998944",
            "https://maps.google.com/?cid=10222232094831998944",
            "https://maps.google.com/maps?ftid=0x47e66e2964e34e2d:0x8ddca9ee380ef7e0",
            "https://www.google.com/maps?q=place_id:ChIJLU7jZClu5kcR4PcOOO6p3I0",
            "https://www.google.com/maps/dir/Paris/Lyon/",
            "https://www.google.com/maps?saddr=Paris&daddr=Lyon",
            "https://www.google.com/maps/place/",
            "https://www.google.com/maps/place/@",
            "https://www.google.com/maps/@91,0,3z",
            "https://www.google.com/maps/@x,y,3z",
            "https://www.google.com/maps",
            "https://www.google.com/?q=Eiffel+Tower",
            "https://www.google.com/search?q=Eiffel+Tower",
            "https://www.google.com/mapsx/place/Eiffel+Tower",
            "https://www.google.evil.example/maps/place/Eiffel+Tower",
            "https://google.com.evil.example/maps/place/Eiffel+Tower",
            "https://evilgoogle.com/maps/place/Eiffel+Tower",
            "https://www.google.com:8443/maps/place/Eiffel+Tower",
            "https://user@www.google.com/maps/place/Eiffel+Tower",
            "ftp://www.google.com/maps/place/Eiffel+Tower",
            "https://www.google.com/maps/place/%20%0A%E2%80%8B",
            "https://www.google.com/maps/place/%FF",
            "https://maps.app.goo.gl/",
            "https://maps.app.goo.gl/a/b",
            "https://maps.app.goo.gl/a.b",
            "https://goo.gl/abc",
            "https://goo.gl/maps/",
        ] {
            assert!(!GoogleMaps.reads(uri), "{uri}");
        }
        // A CID alone keeps its route, and its Wikidata lookup (`lookups::places::place_page`).
        assert_eq!(
            crate::resolver::classify("https://www.google.com/maps?cid=10222232094831998944"),
            Some("web")
        );
    }

    #[test]
    fn short_links_are_asked_at_their_code() {
        for (uri, short) in [
            (
                "https://maps.app.goo.gl/9ivZQ71CKMqRPDtQA",
                "https://maps.app.goo.gl/9ivZQ71CKMqRPDtQA",
            ),
            (
                "http://maps.app.goo.gl/TvPZdQo9HRmnfb4d8?g_st=ic",
                "https://maps.app.goo.gl/TvPZdQo9HRmnfb4d8",
            ),
            (
                "https://goo.gl/maps/sXnse1erRvkA6aV99",
                "https://goo.gl/maps/sXnse1erRvkA6aV99",
            ),
        ] {
            assert_eq!(short_link(uri).as_deref(), Some(short), "{uri}");
            assert!(GoogleMaps.reads(uri));
            assert_eq!(crate::resolver::classify(uri), Some("official"), "{uri}");
        }
    }

    /// A target is read only when it is a place or point link; Google's consent page is
    /// passed through once.
    #[test]
    fn a_short_links_target_must_be_a_place_link() {
        let short = "https://maps.app.goo.gl/x";
        let place = "https://www.google.com/maps/place/Tour+Eiffel/data=!4m2!3m1!1s0x47e66e2964e34e2d:0x8ddca9ee380ef7e0";
        assert_eq!(target(short, place).as_deref(), Some(place));
        let consent = format!(
            "https://consent.google.com/m?continue={}&gl=CH",
            crate::sources::kit::encode(place)
        );
        assert_eq!(target(short, &consent).as_deref(), Some(place));
        let twice = format!(
            "https://consent.google.com/m?continue={}",
            crate::sources::kit::encode(&consent)
        );
        for location in [
            "https://www.google.com/maps?cid=10222232094831998944",
            "https://www.google.com/maps/dir/Paris/Lyon/",
            "https://example.com/maps/place/X",
            "/",
            "",
            twice.as_str(),
        ] {
            assert_eq!(target(short, location), None, "{location}");
        }
    }

    fn read(file: &str) -> serde_json::Value {
        serde_json::from_slice(&crate::sources::fixture::read("google_maps", file)).unwrap()
    }

    /// The recorded short links, answered as recorded (status and `Location` only): one GET each
    /// without following the redirect; the card is the target's, and the short link an alias
    /// of it. A missing code is `missing`; a redirect elsewhere is `missing` too.
    #[tokio::test(start_paused = true)]
    async fn short_links_follow_only_their_redirect() {
        let provenance = read("provenance.json");
        let mut routes = vec![];
        for entry in provenance.as_object().unwrap().values() {
            let url = entry["url"].as_str().unwrap();
            let status = entry["status"].as_u64().unwrap() as u16;
            let location = entry["location"].as_str().unwrap_or_default();
            routes.push((url, status, "text/html", location.as_bytes().to_vec()));
        }
        routes.push((
            "https://maps.app.goo.gl/Directions1",
            302,
            "text/html",
            b"https://www.google.com/maps/dir/Paris/Lyon/".to_vec(),
        ));
        routes.push((
            "https://maps.app.goo.gl/Page1",
            200,
            "text/html",
            b"<html></html>".to_vec(),
        ));
        let (egress, recorded) = crate::sources::fixture::egress(routes);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        for (uri, name, point, root) in [
            (
                "https://goo.gl/maps/sXnse1erRvkA6aV99",
                "Falafel M. Sahyoun",
                Some((33.8904447, 35.5066505)),
                "https://www.google.com/maps/place/Falafel+M.+Sahyoun/@33.8904447,35.5066505,17z/data=!3m1!4b1!4m6!3m5!1s0x151f16e2123697fd:0x8e6626b678863990!8m2!3d33.8904447!4d35.5066505!16s%2Fg%2F1td7wffc?utm_source=mstt_1&coh=164777&entry=tt&shorturl=1",
            ),
            (
                "https://maps.app.goo.gl/9ivZQ71CKMqRPDtQA?g_st=ic",
                "Mollafenari, Elizi, Bab-ı Ali Cd. No:27, 34120 Fatih/İstanbul, Turkey",
                None,
                "https://www.google.com/maps/place/Mollafenari,+Elizi,+Bab-%C4%B1+Ali+Cd.+No:27,+34120+Fatih%2F%C4%B0stanbul,+Turkey/data=!4m2!3m1!1s0x14cab996a54299cb:0x40acb197d028d115?utm_source=mstt_1&entry=gps",
            ),
            (
                "https://maps.app.goo.gl/TvPZdQo9HRmnfb4d8",
                "5.811698, -55.118891",
                Some((5.811698, -55.118891)),
                "https://www.google.com/maps/search/5.811698,+-55.118891?entry=tts&g_ep=EgoyMDI1MDEyOS4xIPu8ASoASAFQAw%3D%3D",
            ),
        ] {
            let doc = GoogleMaps.resolve(&cx, uri).await.unwrap();
            assert_eq!(doc.status, Status::Ok, "{uri}");
            assert_eq!(doc.url, uri);
            assert_eq!(doc.type_name, "Place");
            assert_eq!(doc.name.as_deref(), Some(name), "{uri}");
            assert_eq!(doc.location.as_ref().map(|p| (p.lat, p.lon)), point);
            assert_eq!(doc.source_root.as_deref(), Some(root), "{uri}");
            // The root is this source's, read from the link alone.
            assert!(GoogleMaps.reads(root));
        }
        for uri in [
            "https://maps.app.goo.gl/doesnotexist123",
            "https://maps.app.goo.gl/Directions1",
        ] {
            let doc = GoogleMaps.resolve(&cx, uri).await.unwrap();
            assert_eq!(doc.status, Status::Missing, "{uri}");
            assert_eq!(doc.source_root, None);
        }
        let error = GoogleMaps
            .resolve(&cx, "https://maps.app.goo.gl/Page1")
            .await
            .unwrap_err();
        assert_eq!(crate::resolver::error_class(&error), "parse");
        // A long link asks nothing; each short link was asked once, at its code alone.
        GoogleMaps
            .resolve(&cx, "https://www.google.com/maps/@46.0051,8.9528,14z")
            .await
            .unwrap();
        let mut asked = recorded.asked();
        asked.sort();
        assert_eq!(
            asked,
            [
                "https://goo.gl/maps/sXnse1erRvkA6aV99",
                "https://maps.app.goo.gl/9ivZQ71CKMqRPDtQA",
                "https://maps.app.goo.gl/Directions1",
                "https://maps.app.goo.gl/Page1",
                "https://maps.app.goo.gl/TvPZdQo9HRmnfb4d8",
                "https://maps.app.goo.gl/doesnotexist123",
            ]
        );
    }

    /// What each answer to a short link makes: a redirect to a link this source reads is its
    /// card, one without a target or a 400, 404 or 410 is `missing`, any other answer an error.
    #[test]
    fn short_card_by_status() {
        let doc = || ResourceMeta::new("https://maps.app.goo.gl/x", "google_maps", 60);
        let missing = || {
            let mut d = doc();
            d.status = Status::Missing;
            d
        };
        let short = "https://maps.app.goo.gl/x";
        let ok = short_card(
            doc(),
            missing(),
            short,
            301,
            Some("https://www.google.com/maps/@46.0051,8.9528,14z"),
        )
        .unwrap();
        assert_eq!(ok.name.as_deref(), Some("46.0051, 8.9528"));
        for status in [400, 404, 410] {
            let doc = short_card(doc(), missing(), short, status, None).unwrap();
            assert_eq!(doc.status, Status::Missing);
        }
        assert_eq!(
            short_card(doc(), missing(), short, 302, None)
                .unwrap()
                .status,
            Status::Missing
        );
        for (status, class) in [(200, "parse"), (204, "parse"), (403, "http_4xx")] {
            let error = short_card(doc(), missing(), short, status, None).unwrap_err();
            assert_eq!(crate::resolver::error_class(&error), class, "{status}");
        }
    }

    /// Property: on 20,000 random and malformed spellings of real Google Maps links, the
    /// readers never panic, and a link read gives a bounded name without control characters
    /// and a point in range.
    #[test]
    fn random_links_never_panic_and_read_bounded_places() {
        const BASES: &[&str] = &[
            "https://www.google.com/maps/place/Eiffel+Tower/@48.8583701,2.2919064,17z/data=!3m1!4b1!4m6!3m5!1s0x47e66e2964e34e2d:0x8ddca9ee380ef7e0!8m2!3d48.8583701!4d2.2944813!16zL20vMDJqODE?entry=ttu",
            "https://www.google.com/maps/place/Mollafenari,+Elizi,+Bab-%C4%B1+Ali+Cd.+No:27,+34120+Fatih%2F%C4%B0stanbul,+Turkey/data=!4m2!3m1!1s0x14cab996a54299cb:0x40acb197d028d115",
            "https://www.google.com/maps/search/5.811698,+-55.118891?entry=tts",
            "https://www.google.com/maps/@46.0051,8.9528,14z",
            "https://www.google.com/maps/search/?api=1&query=47.5951518,-122.3316393",
            "https://maps.google.com/maps?q=loc:48.8584,2.2945(Tour+Eiffel)",
            "https://www.google.com/maps?ll=47.37,8.54&z=12",
            "https://www.google.co.uk/maps/place/Big+Ben/@51.5007292,-0.1268141,17z",
            "https://maps.app.goo.gl/9ivZQ71CKMqRPDtQA",
            "https://goo.gl/maps/sXnse1erRvkA6aV99",
        ];
        const PARTS: &[&str] = &[
            "https://www.google.com/maps",
            "https://maps.google.com/",
            "https://google.de/maps",
            "https://maps.app.goo.gl/",
            "https://goo.gl/maps/",
            "/place/",
            "/search/",
            "/@",
            "/data=",
            "?q=",
            "?api=1&query=",
            "&ll=",
            "&z=",
            "!3d",
            "!4d",
            "!1s0x",
            ":0x",
            "48.8583",
            "-122.33",
            "91",
            "1e5",
            ",",
            ",17z",
            ",3a",
            "+",
            "%",
            "%2",
            "%C3%A9",
            "%FF",
            "%00",
            "%E2%80%AE",
            "%0A",
            "(",
            ")",
            "loc:",
            "place_id:",
            "Eiffel",
            "Tower",
            "é",
            "東京",
            "😀",
            "\u{301}",
            " ",
            "/",
            "?",
            "&",
            "#",
            "abc",
            "9ivZQ71CKMqRPDtQA",
        ];
        let mut gen = geo::fuzz::Gen(0x9e37_79b9_7f4a_7c15);
        let (mut read, mut unread) = (0, 0);
        for _ in 0..20_000 {
            let base = gen.pick(BASES);
            let uri = gen.mutate(base, PARTS);
            if short_link(&uri).is_some() {
                read += 1;
            }
            geo::google_cid(&uri);
            target("https://maps.app.goo.gl/x", &uri);
            let Some(place) = place(&uri) else {
                unread += 1;
                continue;
            };
            read += 1;
            if let Some(name) = &place.name {
                assert!(
                    !name.is_empty()
                        && name.len() <= geo::NAME_BYTES
                        && name.trim() == name
                        && !name.chars().any(char::is_control),
                    "{uri:?} -> {name:?}"
                );
            }
            if let Some(p) = &place.point {
                assert!(
                    (-90.0..=90.0).contains(&p.lat) && (-180.0..=180.0).contains(&p.lon),
                    "{uri:?}"
                );
            }
            assert!(place.name.is_some() || place.point.is_some());
        }
        assert!(read > 3_000 && unread > 3_000, "{read} {unread}");
    }

    fn in_range(p: &LocationLink) -> bool {
        (-90.0..=90.0).contains(&p.lat) && (-180.0..=180.0).contains(&p.lon)
    }

    proptest::proptest! {
        /// Any path and query on Google's map hosts: no panic, and any point in range.
        #[test]
        fn any_link_reads_without_panic(
            host in proptest::sample::select(vec!["www.google.com", "maps.google.com", "www.google.de", "google.com"]),
            path in "(/maps)?(/(place|search|dir|@|data=)?[!@,.+%0-9A-Za-z:_-]{0,24}){0,4}",
            query in "(\\?((q|ll|z|api|query|cid|saddr)=[!@,.+%0-9A-Za-z:()_-]{0,20}&?){0,3})?",
        ) {
            let uri = format!("https://{host}{path}{query}");
            if let Some(found) = place(&uri) {
                proptest::prop_assert!(found.name.is_some() || found.point.is_some());
                if let Some(p) = &found.point {
                    let shown = format!("{uri} {p:?}");
                    proptest::prop_assert!(in_range(p), "{}", shown);
                }
            }
        }
        /// A place link with a name, its map's centre and its pin: the name, and the pin's point
        /// before the centre's.
        #[test]
        fn a_place_link_gives_its_name_and_pin(
            words in proptest::collection::vec("[A-Za-z]{1,10}", 1..4),
            pin in (-89.9f64..89.9, -179.9f64..179.9),
            centre in (-89.9f64..89.9, -179.9f64..179.9),
            zoom in 1u8..21,
        ) {
            let name = words.join(" ");
            let uri = format!(
                "https://www.google.com/maps/place/{}/@{:.6},{:.6},{zoom}z/data=!4m6!3m5!1s0x0:0x1!8m2!3d{:.7}!4d{:.7}",
                words.join("+"), centre.0, centre.1, pin.0, pin.1
            );
            let found = place(&uri);
            let shown = format!("{uri} {found:?}");
            let found = found.expect("a place");
            proptest::prop_assert_eq!(found.name.as_deref(), Some(name.as_str()), "{}", shown);
            let p = found.point.expect("a point");
            proptest::prop_assert!((p.lat - pin.0).abs() < 1e-6 && (p.lon - pin.1).abs() < 1e-6, "{}", shown);
        }
        /// `?q=lat,lon`: that point, and no name.
        #[test]
        fn a_query_point_is_the_point(lat in -90.0f64..=90.0, lon in -180.0f64..=180.0) {
            let uri = format!("https://maps.google.com/maps?q={lat:.5},{lon:.5}");
            let found = place(&uri).expect("a point");
            let p = found.point.expect("a point");
            proptest::prop_assert!((p.lat - lat).abs() < 1e-5 && (p.lon - lon).abs() < 1e-5);
            proptest::prop_assert_eq!(found.name, None);
        }
    }
}
