use super::{time, Attendance, EventStatus, Offers, Organizer, Venue};
use crate::{
    model::{LocationSource, Point, ResourceMeta},
    resolver::web::origin_url,
};
use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

pub fn string(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.get("@value").and_then(Value::as_str).map(str::to_owned))
}
fn first(v: &Value) -> &Value {
    v.as_array().and_then(|v| v.first()).unwrap_or(v)
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|v| v.parse().ok()))
        .filter(|n| n.is_finite())
}
pub fn point(v: &Value) -> Option<Point> {
    let v = v.get("geo").unwrap_or(v);
    let lat = number(&v["latitude"])?;
    let lon = number(&v["longitude"])?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some(Point { lat, lon })
}
pub fn image(v: &Value, base: &Url) -> Option<String> {
    string(v)
        .or_else(|| string(&v["url"]))
        .or_else(|| string(&v["contentUrl"]))
        .or_else(|| v.as_array()?.iter().find_map(|v| image(v, base)))
        .and_then(|s| origin_url(base, &s))
}
fn fill<T>(dst: &mut Option<T>, src: Option<T>) {
    if dst.is_none() {
        *dst = src;
    }
}
fn is_event(v: &Value) -> bool {
    let kind = &v["@type"];
    kind.as_str()
        .into_iter()
        .chain(
            kind.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .any(|s| crate::model::type_path(s).iter().any(|s| s == "Event"))
}
/// Repair raw control characters in strings and trailing commas only, without evaluating JS.
pub fn lenient_json(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Some(v);
    }
    let mut out = String::with_capacity(text.len());
    let mut quoted = false;
    let mut escaped = false;
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => quoted = false,
                c if c < ' ' => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                    continue;
                }
                _ => {}
            }
        } else if c == '"' {
            quoted = true;
        } else if c == ','
            && chars[i + 1..]
                .iter()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| matches!(c, '}' | ']'))
        {
            continue;
        }
        out.push(c);
    }
    serde_json::from_str(&out).ok()
}
/// Every event node in `v`, in document order (an event's own nested entities aside).
fn event_nodes<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    if let Some(values) = v.as_array() {
        values.iter().for_each(|v| event_nodes(v, out));
    } else if is_event(v) {
        out.push(v);
    } else {
        ["@graph", "mainEntity"]
            .iter()
            .filter_map(|k| v.get(k))
            .for_each(|v| event_nodes(v, out));
    }
}
pub fn jsonld(doc: &mut ResourceMeta, v: &Value, base: &Url) {
    // The page's own event (its url is the page) over another the page lists, else the first.
    let mut nodes = vec![];
    event_nodes(v, &mut nodes);
    let Some(v) = nodes
        .iter()
        .find(|n| owns_page(n, base))
        .or(nodes.first())
        .copied()
    else {
        return;
    };
    let kind = v["@type"]
        .as_str()
        .or_else(|| {
            v["@type"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .find(|s| crate::model::type_path(s).contains(&"Event".into()))
        })
        .unwrap_or("Event");
    if !doc.type_path.contains(&"Event".into()) {
        doc.set_type(kind);
        doc.source = "jsonld".into();
    }
    fields(doc, v, base, false);
}
pub fn activity(doc: &mut ResourceMeta, v: &Value, base: &Url) -> bool {
    if v["type"] != "Event" {
        return false;
    }
    if !doc.type_path.contains(&"Event".into()) {
        doc.set_type("Event");
        doc.source = "activitypub".into();
    }
    fields(doc, v, base, true);
    true
}
fn fields(doc: &mut ResourceMeta, v: &Value, base: &Url, ap: bool) {
    // A page script's JSON-LD arrives decoded (`web::decode_json_strings`); JSON fetched as a
    // body is the publisher's text as it is.
    let text = string;
    fill(&mut doc.name, text(&v["name"]));
    fill(
        &mut doc.description,
        text(&v["description"])
            .or_else(|| text(&v["summary"]))
            .or_else(|| {
                string(&v["content"])
                    .map(|s| Html::parse_fragment(&s).root_element().text().collect())
            }),
    );
    fill(
        &mut doc.image,
        image(&v["image"], base).or_else(|| {
            v["attachment"]
                .as_array()?
                .iter()
                .filter(|v| {
                    v["mediaType"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("image/"))
                        || v["name"] == "Banner"
                })
                .find_map(|v| image(v, base))
        }),
    );
    if ap {
        fill(
            &mut doc.event.tz,
            string(&v["mz:timezone"])
                .or_else(|| string(&v["timezone"]))
                .and_then(|s| time::zone(&s).map(|s| s.to_string())),
        );
        fill(&mut doc.event.join_mode, string(&v["joinMode"]));
    }
    for (key, end) in [
        (if ap { "startTime" } else { "startDate" }, false),
        (if ap { "endTime" } else { "endDate" }, true),
    ] {
        if let Some(s) = v[key].as_str() {
            set_time(doc, s, end);
        }
    }
    if doc.event.all_day == Some(false) && doc.event.tz.is_none() {
        if let Some(end) = v[if ap { "endTime" } else { "endDate" }]
            .as_str()
            .map(str::trim)
            .filter(|s| s.len() == 10 || s.len() == 8)
        {
            if let Some(start) = v[if ap { "startTime" } else { "startDate" }]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
            {
                use chrono::TimeZone;
                doc.end = time::naive(end)
                    .and_then(|d| d.checked_add_signed(chrono::Duration::days(1)))
                    .and_then(|d| start.offset().from_local_datetime(&d).single())
                    .map(|d| d.to_utc());
                doc.event.end_local = None;
            }
        }
    }
    fill(
        &mut doc.event.event_status,
        v["eventStatus"]
            .as_str()
            .or_else(|| v["ical:status"].as_str())
            .or_else(|| v["status"].as_str())
            .and_then(EventStatus::parse),
    );
    let loc = first(&v["location"]);
    let virtual_location = loc["@type"] == "VirtualLocation" || loc["type"] == "VirtualLocation";
    let mode = v["eventAttendanceMode"].as_str().unwrap_or("");
    fill(
        &mut doc.event.attendance,
        if mode.ends_with("MixedEventAttendanceMode") {
            Some(Attendance::Mixed)
        } else if virtual_location
            || mode.ends_with("OnlineEventAttendanceMode")
            || v["isOnline"] == true
        {
            Some(Attendance::Online)
        } else if mode.ends_with("OfflineEventAttendanceMode") || v["isOnline"] == false {
            Some(Attendance::Offline)
        } else {
            None
        },
    );
    if doc.event.attendance != Some(Attendance::Online) {
        let address = loc.get("address").unwrap_or(loc);
        let mut venue = Venue {
            name: string(&loc["name"]),
            ..Default::default()
        };
        for (k, dst) in [
            ("streetAddress", "street"),
            ("addressLocality", "city"),
            ("postalCode", "postalcode"),
            ("addressCountry", "country"),
            ("addressRegion", "region"),
        ] {
            if let Some(s) = string(&address[k])
                .or_else(|| string(&address[k]["name"]))
                .filter(|s| !s.trim().is_empty())
            {
                venue.address.insert(dst.into(), s);
            }
        }
        venue.text = string(loc).or_else(|| string(address)).unwrap_or_else(|| {
            venue
                .name
                .iter()
                .chain(venue.address.values())
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        });
        venue.location = point(loc);
        venue.location_source = venue.location.as_ref().map(|_| LocationSource::Page);
        if !venue.text.is_empty() || venue.location.is_some() {
            if let Some(existing) = &mut doc.event.venue {
                if existing.text.is_empty() {
                    existing.text = venue.text;
                }
                fill(&mut existing.name, venue.name);
                for (k, v) in venue.address {
                    existing.address.entry(k).or_insert(v);
                }
                fill(&mut existing.location, venue.location);
                fill(&mut existing.location_source, venue.location_source);
            } else {
                doc.event.venue = Some(venue);
            }
        }
        fill(
            &mut doc.location,
            doc.event.venue.as_ref().and_then(|v| v.location.clone()),
        );
        if doc.location.is_some() {
            fill(&mut doc.location_source, Some(LocationSource::Page));
        }
    }
    let organizer = first(
        v.get("organizer")
            .or_else(|| v.get("attributedTo"))
            .or_else(|| v.get("actor"))
            .unwrap_or(&Value::Null),
    );
    let name = string(&organizer["name"]).or_else(|| string(organizer));
    fill(
        &mut doc.event.organizer,
        name.map(|name| Organizer {
            url: string(&organizer["url"])
                .and_then(|s| origin_url(base, &s))
                .or_else(|| {
                    Url::parse(&name)
                        .ok()
                        .and_then(|u| origin_url(base, u.as_str()))
                }),
            name,
        }),
    );
    if doc.event.performers.is_empty() && owns_page(v, base) {
        doc.event.performers = performers(v, base);
    }
    if let Some(offer) = v.get("offers") {
        let values: Vec<&Value> = offer
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_else(|| vec![offer]);
        let prices: Vec<f64> = values
            .iter()
            .flat_map(|v| {
                [
                    number(&v["price"]),
                    number(&v["lowPrice"]),
                    number(&v["highPrice"]),
                ]
            })
            .flatten()
            .collect();
        fill(
            &mut doc.event.offers,
            Some(Offers {
                low_price: prices.iter().copied().reduce(f64::min),
                high_price: prices.iter().copied().reduce(f64::max),
                currency: values.iter().find_map(|v| string(&v["priceCurrency"])),
            }),
        );
    }
}
/// Set the start or end from a page value, trimmed at both ends first (a padded date-only value
/// is still recognised as a date by its length).
/// Whether an event node is the page's own: the page it names is this page (tracking
/// parameters and fragments aside). Another event a page describes beside its own (a venue's
/// next show, a related date) never donates to the page's event.
pub fn owns_page(v: &Value, base: &Url) -> bool {
    names_page(v, base) == Some(true)
}
/// Whether the page an event node names (its `url`, else its `@id`) is this page; `None` when
/// it names none. An explicit `url` decides: a local `@id` such as `#next-week` resolves to
/// this page whatever event it is, so it never outweighs a `url` naming another.
pub fn names_page(v: &Value, base: &Url) -> Option<bool> {
    fn identity(u: &str) -> Option<String> {
        let mut u = Url::parse(u).ok()?;
        u.set_fragment(None);
        let u = crate::alias::root(u.as_str()).map_or_else(|| u.to_string(), |a| a.root);
        crate::model::identity(&u).ok().map(|v| v.0)
    }
    let page = identity(base.as_str())?;
    ["url", "@id"]
        .into_iter()
        .filter_map(|f| v.get(f).and_then(Value::as_str))
        .filter_map(|u| origin_url(base, u))
        .find_map(|u| identity(&u))
        .map(|id| id == page)
}
/// How much of a line-up a page's parse looks at, so a hostile one costs little: the first 64
/// entries, of which 17 performers with a name, and of each the first 64 link values, of which 9
/// links. Each is one above what an event keeps (`EventFields::bound`: 16 performers, 8 links),
/// so trimming still has one to drop.
const LINEUP_ENTRIES: usize = 64;
const LINEUP_PERFORMERS: usize = 17;
const PERFORMER_LINK_VALUES: usize = 64;
const PERFORMER_LINKS: usize = 9;

/// An event node's performers (`performer`, one or several): a name each, with the links the
/// page gives for it (`url`, `sameAs`); a bare name is a performer without links, a nameless
/// entry nothing.
pub fn performers(v: &Value, base: &Url) -> Vec<super::Performer> {
    let nodes: Vec<&Value> = match v.get("performer") {
        Some(Value::Array(all)) => all.iter().take(LINEUP_ENTRIES).collect(),
        Some(one) if !one.is_null() => vec![one],
        _ => vec![],
    };
    nodes
        .into_iter()
        .filter_map(|p| {
            let name = string(&p["name"]).or_else(|| string(p))?;
            let name = name.trim().to_owned();
            if name.is_empty() {
                return None;
            }
            let mut links = vec![];
            let mut seen = std::collections::HashSet::new();
            let values = ["url", "sameAs"]
                .into_iter()
                .flat_map(|field| field_values(p, field))
                .take(PERFORMER_LINK_VALUES);
            for value in values {
                if let Some(url) = string(value).and_then(|s| origin_url(base, &s)) {
                    if seen.insert(url.clone()) {
                        links.push(url);
                    }
                }
            }
            // What keys the performer first (a page MusicBrainz can relate to its artist), then
            // a MusicBrainz artist page (a fact), so trimming a big line-up drops the other links
            // before them.
            links.sort_by_key(|l| match crate::music_sources::artist_link(l) {
                Some(crate::music_sources::ArtistLink::Lookup(_)) => 0,
                Some(crate::music_sources::ArtistLink::Mbid(_)) => 1,
                None => 2,
            });
            links.truncate(PERFORMER_LINKS);
            Some(super::Performer { name, links })
        })
        .take(LINEUP_PERFORMERS)
        .collect()
}
/// A field's values, one or several, read in place: an array is never copied, so a cap on the
/// values read bounds the work however long the array is.
fn field_values<'a>(node: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    let (all, one) = match node.get(field) {
        Some(Value::Array(all)) => (all.as_slice(), None),
        one => (&[][..], one),
    };
    all.iter().chain(one)
}
pub fn set_time(doc: &mut ResourceMeta, s: &str, end: bool) {
    let s = s.trim();
    if (end && doc.end.is_some()) || (!end && doc.start.is_some()) {
        return;
    }
    let Some(value) = time::parse(s, doc.event.tz.as_deref()) else {
        return;
    };
    if end && (s.len() == 10 || s.len() == 8) && doc.event.all_day == Some(false) {
        if let Some(next) =
            time::naive(s).and_then(|d| d.checked_add_signed(chrono::Duration::days(1)))
        {
            let local = next.to_string();
            doc.end = time::parse(&local, doc.event.tz.as_deref());
            doc.event.end_local = Some(local);
        }
        return;
    }
    if end {
        doc.end = Some(value);
        if time::offset(s).is_none() {
            doc.event.end_local = Some(s.into());
        }
    } else {
        doc.start = Some(value);
        doc.event.all_day = Some(s.len() == 10 || s.len() == 8);
        if time::offset(s).is_none() {
            doc.event.start_local = Some(s.into());
        }
    }
}
pub fn microformats(doc: &mut ResourceMeta, html: &Html, base: &Url) {
    let Some(root) = html.select(&Selector::parse(".h-event").unwrap()).next() else {
        return;
    };
    if !doc.type_path.contains(&"Event".into()) {
        doc.set_type("Event");
        doc.source = "microformats".into();
    }
    let value = |s: &str| {
        root.select(&Selector::parse(s).unwrap()).next().map(|el| {
            ["datetime", "content", "href", "src", "title", "value"]
                .iter()
                .find_map(|a| el.value().attr(a))
                .map(str::to_owned)
                .unwrap_or_else(|| el.text().collect::<String>().trim().into())
        })
    };
    fill(&mut doc.name, value(".p-name"));
    fill(&mut doc.description, value(".p-summary, .p-description"));
    fill(
        &mut doc.image,
        value(".u-photo").and_then(|s| origin_url(base, &s)),
    );
    if let Some(s) = value(".dt-start") {
        set_time(doc, &s, false);
    }
    if let Some(s) = value(".dt-end") {
        set_time(doc, &s, true);
    }
    fill(
        &mut doc.event.venue,
        value(".p-location").map(|text| Venue {
            text,
            ..Default::default()
        }),
    );
    if let Some((lat, lon)) = value(".p-latitude")
        .and_then(|s| s.parse::<f64>().ok())
        .zip(value(".p-longitude").and_then(|s| s.parse::<f64>().ok()))
    {
        let p = point(&serde_json::json!({"latitude":lat,"longitude":lon}));
        fill(&mut doc.location, p.clone());
        if let Some(v) = &mut doc.event.venue {
            fill(&mut v.location, p);
            v.location_source = Some(LocationSource::Page);
        }
    }
}
pub fn finalize(doc: &mut ResourceMeta, coordinates: bool) {
    if !doc.type_path.contains(&"Event".into()) {
        return;
    }
    if doc.event.attendance == Some(Attendance::Online) {
        doc.location = None;
        doc.location_source = None;
        doc.event.venue = None;
    }
    if coordinates && doc.event.tz.is_none() {
        doc.event.tz = doc.location.as_ref().and_then(time::coordinate_zone);
        if doc.event.tz.is_some() {
            doc.event.timezone_attribution = Some(time::ATTRIBUTION.into());
        }
    }
    if let Some(s) = &doc.event.start_local {
        doc.start = time::parse(s, doc.event.tz.as_deref());
    }
    if let Some(s) = &doc.event.end_local {
        doc.end = time::parse(s, doc.event.tz.as_deref());
    }
    if doc
        .end
        .zip(doc.start)
        .is_some_and(|(end, start)| end < start)
    {
        doc.end = None;
        doc.event.warnings.push("invalid_end".into());
    }
    doc.event.event_status.get_or_insert(EventStatus::Scheduled);
    doc.event.attendance.get_or_insert(Attendance::Offline);
    if doc.location.is_some() {
        doc.location_source.get_or_insert(LocationSource::Page);
    }
}

pub fn merge_missing(doc: &mut ResourceMeta, fallback: &ResourceMeta) -> anyhow::Result<()> {
    fn fill(a: &mut Value, b: &Value) {
        if a.is_null() {
            *a = b.clone();
        } else if let (Some(a), Some(b)) = (a.as_object_mut(), b.as_object()) {
            for (k, v) in b {
                fill(a.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    }
    // A document's performers keep their own provenance; ones taken from `fallback` bring its.
    let own = (!doc.event.performers.is_empty()).then(|| doc.event.performers_from.clone());
    let mut event = serde_json::to_value(&doc.event)?;
    fill(&mut event, &serde_json::to_value(&fallback.event)?);
    doc.event = serde_json::from_value(event)?;
    if let Some(from) = own {
        doc.event.performers_from = from;
    }
    if doc.start.is_none() {
        doc.start = fallback.start;
    }
    if doc.end.is_none() {
        doc.end = fallback.end;
    }
    if doc.name.is_none() {
        doc.name = fallback.name.clone();
    }
    if doc.description.is_none() {
        doc.description = fallback.description.clone();
    }
    if doc.image.is_none() {
        doc.image = fallback.image.clone();
    }
    if doc.location.is_none() {
        doc.location = fallback.location.clone();
        doc.location_source = fallback.location_source.clone();
    }
    Ok(())
}

#[cfg(test)]
mod performer_tests {
    use super::*;
    /// The event service reads performers from the page's own node only: a node whose URL is
    /// the page (tracking parameters aside), never another event the page describes.
    #[test]
    fn performers_come_from_the_pages_own_node() {
        let base = Url::parse("https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena")
            .unwrap();
        let performer = serde_json::json!([{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"}]);
        let own = serde_json::json!({"@type":"MusicEvent","name":"Radiohead @ RAC Arena","url":"https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena?utm_medium=organic&utm_source=microformat","startDate":"2027-05-03T19:30:00","performer":performer});
        let other = serde_json::json!({"@type":"MusicEvent","name":"Next","url":"https://www.songkick.com/concerts/43402211-radiohead-at-rac-arena","startDate":"2027-05-04T19:30:00","performer":performer});
        let anonymous = serde_json::json!({"@type":"MusicEvent","name":"Anonymous","startDate":"2027-05-04T19:30:00","performer":performer});
        assert!(owns_page(&own, &base));
        assert!(!owns_page(&other, &base));
        assert!(!owns_page(&anonymous, &base));
        assert_eq!(names_page(&anonymous, &base), None);
        let mut doc = ResourceMeta::new(base.as_str(), "web", 3600);
        jsonld(&mut doc, &other, &base);
        assert!(doc.event.performers.is_empty());
        jsonld(&mut doc, &own, &base);
        assert_eq!(doc.event.performers.len(), 1);
        assert_eq!(doc.event.performers[0].name, "Radiohead");
        // Listed after another event, the page's own is the one read.
        let mut doc = ResourceMeta::new(base.as_str(), "web", 3600);
        jsonld(
            &mut doc,
            &serde_json::json!({"@graph": [other, own]}),
            &base,
        );
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ RAC Arena"));
        assert_eq!(doc.event.performers.len(), 1);
    }
    /// An explicit `url` decides whose event a node is: a local `@id` resolves to the page it
    /// sits on, whichever event it names.
    #[test]
    fn a_fragment_id_never_outweighs_a_url_naming_another_page() {
        let base = Url::parse("https://venue.example/tonight").unwrap();
        let node = |fields: serde_json::Value| {
            let mut v = serde_json::json!({"@type":"MusicEvent","name":"Next week","startDate":"2027-05-10T19:30:00","performer":[{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"}]});
            v.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            v
        };
        let next_week = node(serde_json::json!({"url":"/next-week","@id":"#next-week"}));
        assert!(!owns_page(&next_week, &base));
        let mut doc = ResourceMeta::new(base.as_str(), "web", 3600);
        jsonld(&mut doc, &next_week, &base);
        assert!(doc.event.performers.is_empty());
        // The page's own: its url names it, tracking aside, whatever its fragment `@id`; a
        // fragment `@id` alone names the page too.
        for own in [
            serde_json::json!({"url":"/tonight?utm_source=feed","@id":"#event"}),
            serde_json::json!({"@id":"https://venue.example/tonight#event"}),
            serde_json::json!({"@id":"#event"}),
        ] {
            assert!(owns_page(&node(own.clone()), &base), "{own}");
        }
    }
    /// A performer's links come in the order that keys it: a page MusicBrainz relates, then its
    /// MusicBrainz page (a fact), then the rest; trimming a big line-up drops from the end, so
    /// the link MusicBrainz is asked about is the last to go.
    #[test]
    fn a_performers_keying_link_comes_first_and_survives_trimming() {
        let base = Url::parse("https://tickets.example/show").unwrap();
        let node = serde_json::json!({"@type":"MusicEvent","url":"https://tickets.example/show","performer":[{"name":"Radiohead","url":"https://tickets.example/artists/radiohead","sameAs":["https://www.instagram.com/radiohead","https://www.songkick.com/artists/253846-radiohead","https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"]}]});
        let found = performers(&node, &base);
        assert_eq!(
            found[0].links,
            [
                "https://www.songkick.com/artists/253846-radiohead",
                "https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711",
                "https://tickets.example/artists/radiohead",
                "https://www.instagram.com/radiohead",
            ]
        );
        let mut fields = super::super::EventFields {
            performers: found,
            ..Default::default()
        };
        while fields.performers[0].links.len() > 1 {
            assert!(fields.shrink_performers());
        }
        assert_eq!(
            fields.performers[0].links,
            ["https://www.songkick.com/artists/253846-radiohead"]
        );
    }

    /// A hostile line-up costs little: the parse looks at a bounded part of it and keeps one
    /// performer and one link more than an event stores, the keying link among them.
    #[test]
    fn a_hostile_line_up_is_read_in_bounds() {
        let base = Url::parse("https://tickets.example/show").unwrap();
        let mut same_as: Vec<String> = (0..1000)
            .map(|n| format!("https://tickets.example/fans/{n}"))
            .collect();
        same_as.insert(
            40,
            "https://www.songkick.com/artists/253846-radiohead".into(),
        );
        let performer = serde_json::json!({"name":"Radiohead","sameAs":same_as});
        let node = serde_json::json!({"@type":"MusicEvent","url":"https://tickets.example/show","performer":vec![performer; 1000]});
        let started = std::time::Instant::now();
        let found = performers(&node, &base);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(found.len(), 17);
        for p in &found {
            assert_eq!(p.links.len(), 9);
            assert_eq!(
                p.links[0],
                "https://www.songkick.com/artists/253846-radiohead"
            );
        }
    }

    /// The values cap counts `url` first, then `sameAs`, each one value or an array.
    #[test]
    fn performer_link_values_are_capped_across_fields() {
        let base = Url::parse("https://tickets.example/show").unwrap();
        let radiohead = "https://www.songkick.com/artists/253846-radiohead";
        let filler = |n: usize| -> Vec<String> {
            (0..n)
                .map(|i| format!("https://tickets.example/fans/{i}"))
                .collect()
        };
        // 63 `url` values leave room for one `sameAs` value; 64 leave none.
        for (urls, read) in [(63, true), (64, false)] {
            let node = serde_json::json!({"performer":{"name":"Radiohead","url":filler(urls),"sameAs":[radiohead,"https://tickets.example/more"]}});
            let links = &performers(&node, &base)[0].links;
            assert_eq!(links.len(), 9, "{urls}");
            assert_eq!(links[0] == radiohead, read, "{urls}: {links:?}");
        }
        let node = serde_json::json!({"performer":{"name":"Radiohead","url":"https://tickets.example/radiohead","sameAs":radiohead}});
        assert_eq!(
            performers(&node, &base)[0].links,
            [radiohead, "https://tickets.example/radiohead"]
        );
    }

    /// A merge never pairs one document's performers with another's provenance: performers keep
    /// their own (or none), and ones taken from the fallback bring the fallback's.
    #[test]
    fn performers_keep_their_own_provenance_in_a_merge() {
        let performer = super::super::Performer {
            name: "Radiohead".into(),
            links: vec!["https://www.songkick.com/artists/253846".into()],
        };
        let mut fallback = ResourceMeta::new("https://www.songkick.com/concerts/1", "web", 60);
        fallback.event.performers = vec![performer.clone()];
        fallback.event.performers_from = Some("https://www.songkick.com/concerts/1".into());
        let mut own = ResourceMeta::new("https://www.songkick.com/concerts/1", "web", 60);
        own.event.performers = vec![performer];
        merge_missing(&mut own, &fallback).unwrap();
        assert_eq!(own.event.performers_from(), None);
        let mut none = ResourceMeta::new("https://www.songkick.com/concerts/1", "web", 60);
        merge_missing(&mut none, &fallback).unwrap();
        assert_eq!(none.event.performers.len(), 1);
        assert_eq!(
            none.event.performers_from(),
            Some("https://www.songkick.com/concerts/1")
        );
    }

    /// The provenance is stored as `performers_fetched_from`; a card stored with another key for
    /// it (an address it was asked for, not one its fetch ended on) has none when read back.
    #[test]
    fn only_the_stored_provenance_of_a_fetch_is_read_back() {
        let mut card = ResourceMeta::new("https://www.songkick.com/concerts/11-stored", "web", 60);
        card.event.performers = vec![super::super::Performer {
            name: "Radiohead".into(),
            links: vec!["https://www.songkick.com/artists/253846".into()],
        }];
        let mut stored = serde_json::to_value(&card).unwrap();
        stored["performers_from"] = "https://www.songkick.com/concerts/11-stored".into();
        let doc: ResourceMeta = serde_json::from_value(stored).unwrap();
        assert_eq!(doc.event.performers.len(), 1);
        assert_eq!(doc.event.performers_from(), None);
        let mut doc = doc;
        doc.event.performers_from = Some("https://www.songkick.com/concerts/11-stored".into());
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            json["performers_fetched_from"],
            "https://www.songkick.com/concerts/11-stored"
        );
        assert!(json.get("performers_from").is_none());
        let back: ResourceMeta = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.event.performers_from(),
            Some("https://www.songkick.com/concerts/11-stored")
        );
    }
}
