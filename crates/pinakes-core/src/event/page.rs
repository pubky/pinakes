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
pub fn event_node(v: &Value) -> Option<&Value> {
    if let Some(values) = v.as_array() {
        return values.iter().find_map(event_node);
    }
    let kind = &v["@type"];
    if kind
        .as_str()
        .into_iter()
        .chain(
            kind.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .any(|s| crate::model::type_path(s).iter().any(|s| s == "Event"))
    {
        return Some(v);
    }
    ["@graph", "mainEntity"]
        .iter()
        .filter_map(|k| v.get(k))
        .find_map(event_node)
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
pub fn jsonld(doc: &mut ResourceMeta, v: &Value, base: &Url) {
    let Some(v) = event_node(v) else { return };
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
    if doc.event.performers.is_empty() {
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
/// An event node's performers (`performer`, one or several): a name each, with the links the
/// page gives for it (`url`, `sameAs`); a bare name is a performer without links, a nameless
/// entry nothing.
pub fn performers(v: &Value, base: &Url) -> Vec<super::Performer> {
    let nodes: Vec<&Value> = match v.get("performer") {
        Some(Value::Array(all)) => all.iter().collect(),
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
            for field in ["url", "sameAs"] {
                let values: Vec<&Value> = match p.get(field) {
                    Some(Value::Array(all)) => all.iter().collect(),
                    Some(one) => vec![one],
                    None => vec![],
                };
                for value in values {
                    if let Some(url) = string(value).and_then(|s| origin_url(base, &s)) {
                        if !links.contains(&url) {
                            links.push(url);
                        }
                    }
                }
            }
            Some(super::Performer { name, links })
        })
        .collect()
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
    let mut event = serde_json::to_value(&doc.event)?;
    fill(&mut event, &serde_json::to_value(&fallback.event)?);
    doc.event = serde_json::from_value(event)?;
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
