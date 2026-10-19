//! Bounded data extraction. Embedded JavaScript is never evaluated.
use crate::{
    event::{page, time, Attendance, EventStatus, Organizer, Venue},
    model::{LocationSource, ResourceMeta},
};
use serde_json::{json, Value};
use url::Url;

fn at<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k).filter(|v| !v.is_null()))
}
fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.as_i64().map(|n| n.to_string()))
}
fn val(v: &Value, keys: &[&str]) -> Option<String> {
    at(v, keys).and_then(text)
}
fn date(v: &Value) -> Option<String> {
    if let Some(n) = v.as_i64() {
        return time::from_timestamp(if n > 100000000000 { n / 1000 } else { n })
            .map(|t| t.to_rfc3339());
    }
    v.as_str()
        .map(str::to_owned)
        .or_else(|| at(v, &["utc", "dateTime", "date", "local"]).and_then(text))
}
fn start(v: &Value) -> Option<String> {
    at(
        v,
        &[
            "beginsOn",
            "start_at",
            "startUtc",
            "start_date",
            "startdate",
            "starts_at",
            "start_iso",
            "start_time",
            "time_start",
            "start_datetime",
            "dateTime",
            "startDate",
            "startTime",
            "start",
            "startsAt",
            "startDateTime",
        ],
    )
    .and_then(date)
    .or_else(|| v.pointer("/dates/start/dateTime").and_then(date))
}
fn end(v: &Value) -> Option<String> {
    at(
        v,
        &[
            "endsOn",
            "end_at",
            "endUtc",
            "end_date",
            "enddate",
            "ends_at",
            "end_iso",
            "end_time",
            "time_end",
            "end_datetime",
            "endDate",
            "endTime",
            "end",
            "endsAt",
            "endDateTime",
        ],
    )
    .and_then(date)
    .or_else(|| v.pointer("/dates/end/dateTime").and_then(date))
}

pub fn object(id: &str, uri: &str, v: &Value) -> Option<ResourceMeta> {
    let base = Url::parse(uri).ok()?;
    let mut doc = ResourceMeta::new(uri, id, 3600);
    let begins = start(v)?;
    doc.set_type("Event");
    doc.name = val(v, &["name", "title", "event_name", "eventname"]);
    if id == "google_calendar_api" {
        doc.name = doc.name.or_else(|| val(v, &["summary"]));
        doc.description = val(v, &["description"]);
    }
    doc.description = val(
        v,
        &[
            "description",
            "summary",
            "short_desc",
            "long_desc",
            "description_text",
        ],
    )
    .map(|mut s| {
        crate::model::truncate(&mut s, 8192);
        scraper::Html::parse_fragment(&s)
            .root_element()
            .text()
            .collect()
    });
    doc.event.tz = at(
        v,
        &[
            "standard_timezone",
            "timezone",
            "timeZone",
            "tz",
            "start_tzid",
        ],
    )
    .and_then(|v| v.as_str())
    .or_else(|| v.pointer("/startDate/timezone").and_then(Value::as_str))
    .or_else(|| v.pointer("/dates/timezone").and_then(Value::as_str))
    .or_else(|| v.pointer("/options/timezone").and_then(Value::as_str))
    .or_else(|| v.pointer("/start/timeZone").and_then(Value::as_str))
    .or_else(|| {
        v.pointer("/venue/area/ianaTimeZone")
            .and_then(Value::as_str)
    })
    .or_else(|| v.pointer("/group/timezone").and_then(Value::as_str))
    .and_then(time::zone)
    .map(|z| z.to_string());
    let mut schema = json!({"@type":"Event","name":doc.name,"startDate":begins,"endDate":end(v)});
    let place = at(
        v,
        &[
            "venue",
            "place",
            "physicalAddress",
            "geo_address_info",
            "location",
        ],
    )
    .or_else(|| v.pointer("/_embedded/venues/0"));
    if let Some(p) = place {
        if p.is_string() {
            schema["location"] = json!({"@type":"Place","name":p});
        } else {
            let address = at(p, &["address", "address_1", "full_address", "addressLines"])
                .cloned()
                .unwrap_or(Value::Null);
            schema["location"] = json!({"@type":"Place","name":at(p,&["name","venue","description"]),"address":address,"geo":p.get("geo").unwrap_or(p)});
        }
    }
    page::jsonld(&mut doc, &schema, &base);
    let coords = at(v, &["coordinate", "coordinates", "geo", "location"])
        .filter(|v| v.is_object())
        .or(place)
        .unwrap_or(v);
    let p=page::point(coords).or_else(||page::point(&json!({"latitude":at(coords,&["lat","latitude"]),"longitude":at(coords,&["lon","lng","longitude"])})))
        .or_else(||page::point(&json!({"latitude":at(v,&["lat","latitude"]),"longitude":at(v,&["lon","longitude"])})))
        .or_else(||place.and_then(|p|p.get("address")).and_then(page::point))
        .or_else(||place.and_then(|p|p.get("location")).and_then(page::point))
        .or_else(|| place.and_then(|p|p.get("geom")).and_then(Value::as_str).and_then(|s|s.split_once(';')).and_then(|(lon,lat)|page::point(&json!({"latitude":lat,"longitude":lon}))));
    if let Some(p) = p {
        doc.location = Some(p.clone());
        doc.location_source = Some(LocationSource::Page);
        let venue = doc.event.venue.get_or_insert_with(Venue::default);
        venue.location = Some(p);
        venue.location_source = Some(LocationSource::Page);
    }
    let status = at(v, &["status", "eventStatus", "state"])
        .and_then(|v| v.as_str())
        .or_else(|| v.pointer("/dates/status/code").and_then(Value::as_str));
    doc.event.event_status = status.and_then(EventStatus::parse);
    if v["cancelled"] == true
        || v["canceled"] == true
        || v.get("deleted_at").is_some_and(|x| !x.is_null())
    {
        doc.event.event_status = Some(EventStatus::Cancelled);
    }
    let mode = val(
        v,
        &["venue_type", "location_type", "event_type", "eventType"],
    )
    .unwrap_or_default()
    .to_lowercase();
    if matches!(mode.as_str(), "virtual" | "online" | "webinar")
        || v["isOnline"] == true
        || id == "airmeet" && mode != "hybrid"
    {
        doc.event.attendance = Some(Attendance::Online);
    } else if mode == "hybrid" {
        doc.event.attendance = Some(Attendance::Mixed);
    }
    doc.image = at(
        v,
        &[
            "cover_url",
            "image",
            "imageUrl",
            "master_img_url",
            "poster",
            "thumbnail",
            "picture",
        ],
    )
    .and_then(|v| page::image(v, &base))
    .or_else(|| v.pointer("/images/0").and_then(|v| page::image(v, &base)));
    if id == "gancio" {
        doc.image = doc.image.or_else(|| {
            v.pointer("/media/0/url")
                .and_then(Value::as_str)
                .and_then(|s| base.join(&format!("/media/{s}")).ok())
                .map(|u| u.to_string())
        });
    }
    doc.event.organizer = val(v, &["organiser_name", "organizer_name"])
        .or_else(|| v.pointer("/organizer/name").and_then(text))
        .map(|name| Organizer { name, url: None });
    doc.source = id.into();
    Some(doc)
}

/// Read JSON values at balanced braces, including JSON strings in React flight payloads.
/// The work is bounded by the transport cap and a fixed number of candidates.
pub fn values(text: &str) -> Vec<Value> {
    values_before(
        text,
        std::time::Instant::now() + std::time::Duration::from_millis(100),
    )
}
fn values_before(text: &str, deadline: std::time::Instant) -> Vec<Value> {
    let mut bytes_scanned = 0usize;
    let mut out = vec![];
    let bytes = text.as_bytes();
    let mut pos = 0;
    let mut attempts = 0;
    while pos < bytes.len()
        && out.len() < 256
        && attempts < 512
        && std::time::Instant::now() < deadline
    {
        if !matches!(bytes[pos], b'{' | b'[' | b'"') {
            pos += 1;
            continue;
        }
        attempts += 1;
        bytes_scanned = bytes_scanned.saturating_add(bytes.len() - pos);
        if bytes_scanned > 16 * 1024 * 1024 {
            break;
        }
        let mut stream = serde_json::Deserializer::from_str(&text[pos..]).into_iter::<Value>();
        if let Some(Ok(v)) = stream.next() {
            pos += stream.byte_offset();
            out.push(v);
        } else {
            pos += 1;
        }
        while pos < bytes.len() && !text.is_char_boundary(pos) {
            pos += 1;
        }
    }
    out
}
fn candidate<'a>(
    v: &'a Value,
    depth: usize,
    remaining: &mut usize,
    deadline: std::time::Instant,
) -> Option<&'a Value> {
    if depth > 24 || *remaining == 0 || std::time::Instant::now() >= deadline {
        return None;
    }
    *remaining -= 1;
    if v.is_object()
        && start(v).is_some()
        && at(v, &["name", "title", "event_name", "eventname"]).is_some()
    {
        return Some(v);
    }
    // Prefer the page's primary event over recommendation/analytics arrays.
    for key in [
        "event",
        "eventDetails",
        "eventData",
        "pageProps",
        "props",
        "data",
        "result",
        "initialState",
    ] {
        if let Some(found) = v
            .get(key)
            .and_then(|v| candidate(v, depth + 1, remaining, deadline))
        {
            return Some(found);
        }
    }
    match v {
        Value::Object(m) => m
            .iter()
            .filter(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "recommendations" | "relatedEvents" | "similarEvents" | "analytics"
                )
            })
            .find_map(|(_, v)| candidate(v, depth + 1, remaining, deadline)),
        Value::Array(a) => a
            .iter()
            .find_map(|v| candidate(v, depth + 1, remaining, deadline)),
        _ => None,
    }
}
pub fn parse(id: &str, uri: &str, body: &[u8]) -> Option<ResourceMeta> {
    if body.len() > crate::resolver::egress::BODY_CAP {
        return None;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let text = String::from_utf8_lossy(body);
    let base = Url::parse(uri).ok()?;
    let mut roots = vec![];
    // State embedded in a page script is HTML-escaped like JSON-LD; an API's JSON body is not.
    let from_scripts = match serde_json::from_str::<Value>(&text) {
        Ok(v) => {
            roots.push(v);
            false
        }
        Err(_) => true,
    };
    if from_scripts {
        let html = scraper::Html::parse_document(&text);
        for node in html.select(&scraper::Selector::parse("script").unwrap()) {
            if roots.len() >= 256 || std::time::Instant::now() >= deadline {
                break;
            }
            let content = node.text().collect::<String>();
            roots.extend(values_before(&content, deadline));
        }
        if id == "ticketino" {
            let get = |s: &str| {
                html.select(&scraper::Selector::parse(s).ok()?)
                    .next()
                    .map(|n| n.text().collect::<String>())
            };
            let v = json!({"name":get("title"),"startDate":get(".atc_date_start"),"endDate":get(".atc_date_end"),"timezone":get(".atc_timezone"),"location":get(".atc_location")});
            return object(id, uri, &v);
        }
    }
    // Decode JSON string payloads without executing scripts or accepting unbounded recursion.
    for _ in 0..3 {
        let mut extra = vec![];
        fn strings(v: &Value, depth: usize, out: &mut Vec<Value>, deadline: std::time::Instant) {
            if depth > 16 || out.len() >= 256 || std::time::Instant::now() >= deadline {
                return;
            }
            match v {
                Value::String(s) if s.contains('{') => out.extend(values_before(s, deadline)),
                Value::Array(a) => {
                    for v in a {
                        strings(v, depth + 1, out, deadline)
                    }
                }
                Value::Object(m) => {
                    for v in m.values() {
                        strings(v, depth + 1, out, deadline)
                    }
                }
                _ => (),
            }
        }
        for root in &roots {
            if extra.len() >= 256 || std::time::Instant::now() >= deadline {
                break;
            }
            strings(root, 0, &mut extra, deadline);
        }
        if extra.is_empty() {
            break;
        }
        roots.extend(extra);
        if roots.len() > 1024 {
            roots.truncate(1024);
            break;
        }
    }
    // Decoded once, after the JSON-in-string payloads are parsed: an escaped quote inside a
    // payload would otherwise end its string early.
    if from_scripts {
        for root in &mut roots {
            crate::resolver::web::decode_json_strings(root);
        }
    }
    if id == "ents24" {
        for root in &roots {
            if std::time::Instant::now() >= deadline {
                return None;
            }
            if let Some(a) = root.as_array() {
                let expanded = nuxt(&Value::from(0), a, 0, &mut 10000, deadline);
                if let Some(v) = candidate(&expanded, 0, &mut 10000, deadline) {
                    if let Some(d) = object(id, uri, v) {
                        return Some(d);
                    }
                }
            }
        }
    }
    let mut doc = ResourceMeta::new(uri, id, 3600);
    for v in &roots {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        page::jsonld(&mut doc, v, &base);
    }
    let mut nodes = 50000;
    for root in &roots {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        if let Some(v) = candidate(root, 0, &mut nodes, deadline) {
            if let Some(fallback) = object(id, uri, v) {
                if !doc.type_path.contains(&"Event".into()) {
                    doc.set_type("Event");
                }
                page::merge_missing(&mut doc, &fallback).ok()?;
                if id == "dice" {
                    doc.event.event_status = root
                        .pointer("/initialState/status")
                        .and_then(Value::as_str)
                        .and_then(EventStatus::parse)
                        .or(fallback.event.event_status)
                        .or(doc.event.event_status);
                }
            }
        }
    }
    doc.start?;
    doc.source = id.into();
    Some(doc)
}
fn nuxt(
    v: &Value,
    table: &[Value],
    depth: usize,
    budget: &mut usize,
    deadline: std::time::Instant,
) -> Value {
    if depth > 20 || *budget == 0 || std::time::Instant::now() >= deadline {
        return Value::Null;
    }
    *budget -= 1;
    match v {
        Value::Number(n) if n.as_u64().is_some_and(|i| (i as usize) < table.len()) => nuxt(
            &table[n.as_u64().unwrap() as usize],
            table,
            depth + 1,
            budget,
            deadline,
        ),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), nuxt(v, table, depth + 1, budget, deadline)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|v| nuxt(v, table, depth + 1, budget, deadline))
                .collect(),
        ),
        _ => v.clone(),
    }
}
