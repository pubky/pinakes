//! App posts: the kinds apps write as posts on the social post envelope under their own
//! namespace, `/pub/<namespace>/v1/posts/{id}/{editId}.json`, and the social specs' events and
//! rsvps (`/pub/social/v1/posts/…`). A post comes on the resource feed as a `post` line naming
//! its versionless URI; the resolver lists its versions and reads the latest
//! (`Kind::versions`), checks it with pubky-social-specs' `PostEnvelope<K>` (acde362, the pin
//! Nexus and the music node use), and reads it by its namespace and kind:
//!
//! - `social` `event`: an `Event` card from its JSCalendar time (its next or current occurrence
//!   when it recurs), status and first location; `social` `rsvp`: an `RsvpAction` card. Any other
//!   social post is Nexus's: no card.
//! - `maps` `route`: a `Trip` card, its box from its coordinate waypoints.
//! - `events` `availability`: a `Schedule` card from its first available block.
//! - `music` `listen`: a `ListenAction` card naming the track it embeds, at the post's time;
//!   `music` `session`: a `CreativeWork` card from its name.
use super::{Client, Kind, Object, Profile, Read};
use crate::{event::time, model::ResourceMeta};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use pubky_social_specs::{
    traits::Validatable, AllowedSchemes, ContentReference, PostEnvelope, PostKind, ValidationCtx,
    PUB_CTX, VALIDATION_LIMITS,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The posts of a namespace: one path, a folder of versions.
const fn posts(read: fn(&Object, &Value, &mut ResourceMeta) -> Read) -> [Kind; 1] {
    [Kind {
        path: "v1/posts/{id}",
        read,
        open_in: &[] as &[Client],
        versions: true,
    }]
}
const EVENTS_KINDS: [Kind; 1] = posts(events);
const MAPS_KINDS: [Kind; 1] = posts(maps);
const MUSIC_KINDS: [Kind; 1] = posts(music);
const SOCIAL_KINDS: [Kind; 1] = posts(social);
/// The facts the post cards publish.
const FACTS: &[&str] = &[
    "kind",
    "location",
    "location_uri",
    "status",
    "guests",
    "event",
    "activity",
    "distance_m",
    "duration_s",
    "elevation_gain_m",
    "elevation_loss_m",
    "waypoints",
    "horizon_days",
    "available_blocks",
    "busy_blocks",
    "for",
    "track",
];
pub static EVENTS: Profile = Profile {
    namespaces: &["events"],
    kinds: &EVENTS_KINDS,
    facts: FACTS,
    links_off: None,
};
pub static MAPS: Profile = Profile {
    namespaces: &["maps"],
    kinds: &MAPS_KINDS,
    facts: FACTS,
    links_off: None,
};
pub static MUSIC: Profile = Profile {
    namespaces: &["music"],
    kinds: &MUSIC_KINDS,
    facts: FACTS,
    links_off: None,
};
pub static SOCIAL: Profile = Profile {
    namespaces: &["social"],
    kinds: &SOCIAL_KINDS,
    facts: FACTS,
    links_off: None,
};

/// Any kind token, for the envelope's checks: the kind's content is the profile's to read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppKind(pub String);
impl PostKind for AppKind {
    fn is_known(&self) -> bool {
        (1..=32).contains(&self.0.len())
            && self
                .0
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    }
    fn validate_content(&self, _: &PostEnvelope<Self>, _: &ValidationCtx) -> Result<(), String> {
        Ok(())
    }
    /// The references inside the content the profiles read (an event's locations, a route's
    /// waypoints, an availability's `for`), so the envelope's gate refuses a private or
    /// non-canonical one as it does the envelope's own.
    fn content_references(&self, post: &PostEnvelope<Self>) -> Vec<ContentReference> {
        let Ok(content) = serde_json::from_str::<Value>(&post.content) else {
            return vec![];
        };
        let reference = |field: String, uri: &Value| {
            uri.as_str().map(|uri| ContentReference {
                field,
                uri: uri.to_owned(),
                schemes: AllowedSchemes::Universal,
                max_code_points: VALIDATION_LIMITS.reference_uri_max_length,
            })
        };
        let list = |key: &str| {
            content[key]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(i, item)| reference(format!("{key}[{i}].uri"), &item["uri"]))
                .collect::<Vec<_>>()
        };
        match self.0.as_str() {
            "event" => list("locations"),
            "route" => list("waypoints"),
            "availability" => reference("for".into(), &content["for"])
                .into_iter()
                .collect(),
            _ => vec![],
        }
    }
}

/// The post's envelope, checked as pubky-social-specs checks it (its id the path's), and its
/// content as JSON.
fn envelope(object: &Object, v: &Value) -> Option<(PostEnvelope<AppKind>, Value)> {
    let bytes = serde_json::to_vec(v).ok()?;
    let envelope =
        <PostEnvelope<AppKind> as Validatable>::try_from(&bytes, object.var("id"), &PUB_CTX)
            .ok()?;
    let content = serde_json::from_str(&envelope.content).ok()?;
    Some((envelope, content))
}
/// When a post was written: its id is a timestamp (microseconds, as the specs decode it).
fn written(id: &str) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_micros(pubky_social_specs::validate_timestamp_id_format(id).ok()?)
}
fn fact(doc: &mut ResourceMeta, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        doc.attributes.insert(key.into(), value);
    }
}
/// A text member, trimmed, within `max` code points; none with a NUL, which no stored text
/// may hold.
fn text(v: &Value, key: &str, max: usize) -> Option<String> {
    v[key]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.chars().count() <= max && !t.contains('\0'))
        .map(str::to_owned)
}

fn social(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let Some((envelope, content)) = envelope(object, v) else {
        return Read::NoCard;
    };
    match envelope.kind.0.as_str() {
        "event" => event(object, &content, doc),
        "rsvp" => rsvp(&envelope, &content, doc),
        // Every other social post is Nexus's.
        _ => Read::NoCard,
    }
}
fn maps(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    match envelope(object, v) {
        Some((e, content)) if e.kind.0 == "route" => route(&content, doc),
        _ => Read::NoCard,
    }
}
fn events(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    match envelope(object, v) {
        Some((e, content)) if e.kind.0 == "availability" => availability(&content, doc),
        _ => Read::NoCard,
    }
}
fn music(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    match envelope(object, v) {
        Some((e, _)) if e.kind.0 == "listen" => listen(object, &e, doc),
        Some((e, content)) if e.kind.0 == "session" => session(&content, doc),
        _ => Read::NoCard,
    }
}

/// A local date-time (`YYYY-MM-DDTHH:MM:SS`) in a zone, as an instant.
fn local(value: &str, zone: chrono_tz::Tz) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").ok()?;
    time::local(zone, naive)
}
/// A JSCalendar `RecurrenceRule` as an RFC 5545 RRULE, its `until` an instant in the zone.
fn rrule(rule: &Value, zone: chrono_tz::Tz) -> Option<String> {
    let mut parts = vec![format!(
        "FREQ={}",
        match rule["frequency"].as_str()? {
            f @ ("yearly" | "monthly" | "weekly" | "daily") => f.to_uppercase(),
            _ => return None,
        }
    )];
    let day = |d: &str| {
        matches!(d, "mo" | "tu" | "we" | "th" | "fr" | "sa" | "su").then(|| d.to_uppercase())
    };
    if let Some(n) = rule.get("interval") {
        parts.push(format!(
            "INTERVAL={}",
            n.as_u64().filter(|n| (1..=1000).contains(n))?
        ));
    }
    if let Some(d) = rule.get("firstDayOfWeek") {
        parts.push(format!("WKST={}", day(d.as_str()?)?));
    }
    if let Some(days) = rule.get("byDay") {
        let days: Option<Vec<String>> = days
            .as_array()?
            .iter()
            .map(|d| {
                let name = day(d["day"].as_str()?)?;
                Some(match d.get("nthOfPeriod") {
                    Some(n) => {
                        format!("{}{name}", n.as_i64().filter(|n| *n != 0 && n.abs() <= 53)?)
                    }
                    None => name,
                })
            })
            .collect();
        parts.push(format!("BYDAY={}", days?.join(",")));
    }
    for (member, name, range) in [
        ("byMonthDay", "BYMONTHDAY", 31),
        ("bySetPosition", "BYSETPOS", 366),
    ] {
        if let Some(list) = rule.get(member) {
            let values: Option<Vec<String>> = list
                .as_array()?
                .iter()
                .map(|v| {
                    v.as_i64()
                        .filter(|v| *v != 0 && v.abs() <= range)
                        .map(|v| v.to_string())
                })
                .collect();
            parts.push(format!("{name}={}", values?.join(",")));
        }
    }
    if let Some(list) = rule.get("byMonth") {
        let values: Option<Vec<String>> = list
            .as_array()?
            .iter()
            .map(|v| {
                v.as_str()?
                    .parse::<u8>()
                    .ok()
                    .filter(|m| (1..=12).contains(m))
                    .map(|m| m.to_string())
            })
            .collect();
        parts.push(format!("BYMONTH={}", values?.join(",")));
    }
    if let Some(n) = rule.get("count") {
        parts.push(format!(
            "COUNT={}",
            n.as_u64().filter(|n| (1..=1000).contains(n))?
        ));
    }
    if let Some(until) = rule.get("until") {
        let at = local(until.as_str()?, zone)?;
        parts.push(format!("UNTIL={}", at.format("%Y%m%dT%H%M%SZ")));
    }
    Some(parts.join(";"))
}
/// The occurrence a series shows: its next or current one (ending after `now`), else its last
/// when it has ended. `None` when its rule cannot be read, or every date is excluded.
fn shown(
    content: &Value,
    zone: chrono_tz::Tz,
    first: DateTime<Utc>,
    length: Duration,
    now: DateTime<Utc>,
    max_period_dates: usize,
) -> Option<DateTime<Utc>> {
    let rules = content["recurrenceRules"].as_array()?;
    if rules.len() != 1 {
        return None;
    }
    let mut lines = vec![
        format!(
            "DTSTART;TZID={}:{}",
            zone.name(),
            first.with_timezone(&zone).format("%Y%m%dT%H%M%S")
        ),
        format!("RRULE:{}", rrule(&rules[0], zone)?),
    ];
    let excluded: Vec<String> = content["excludedDates"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|d| local(d.as_str()?, zone))
                .map(|at| at.format("%Y%m%dT%H%M%SZ").to_string())
                .collect()
        })
        .unwrap_or_default();
    if !excluded.is_empty() {
        lines.push(format!("EXDATE:{}", excluded.join(",")));
    }
    let set = lines.join("\n").parse::<rrule::RRuleSet>().ok()?;
    if !time::rrule_bounded(&set, max_period_dates) {
        return None;
    }
    // The spec's caps: at most 1,000 occurrences, at most three years on.
    let horizon = first + Duration::days(3 * 366);
    let mut last = None;
    for at in set.all(1000).dates {
        let at = at.with_timezone(&Utc);
        if at > horizon {
            break;
        }
        last = Some(at);
        if at + length > now {
            return Some(at);
        }
    }
    last
}
/// A social event: its title, description, its time (a recurring one's next or current
/// occurrence, else its last), zone, status and first location as its venue.
fn event(object: &Object, content: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(title) = text(content, "title", 100) else {
        return Read::NoCard;
    };
    let Some(zone_name) = content["timeZone"].as_str() else {
        return Read::NoCard;
    };
    let Some(zone) = time::zone(zone_name) else {
        return Read::NoCard;
    };
    let Some(first) = content["start"].as_str().and_then(|s| local(s, zone)) else {
        return Read::NoCard;
    };
    let all_day = content["showWithoutTime"].as_bool().unwrap_or(false);
    let length = match content["duration"].as_str() {
        Some(d) => match time::duration(d).filter(|d| *d > Duration::zero()) {
            Some(length) => length,
            None => return Read::NoCard,
        },
        None if all_day => Duration::days(1),
        None => return Read::NoCard,
    };
    let start = if content
        .get("recurrenceRules")
        .and_then(Value::as_array)
        .is_some_and(|r| !r.is_empty())
    {
        match shown(
            content,
            zone,
            first,
            length,
            object.now,
            object.max_period_dates,
        ) {
            Some(at) => at,
            None => return Read::NoCard,
        }
    } else {
        first
    };
    doc.set_type("Event");
    doc.name = Some(title);
    doc.description = text(content, "description", 50_000);
    doc.start = time::bounded(start);
    doc.end = start.checked_add_signed(length).and_then(time::bounded);
    doc.event.tz = Some(zone_name.into());
    doc.event.all_day = all_day.then_some(true);
    match content.get("status").map(Value::as_str) {
        None | Some(Some("confirmed" | "tentative")) => {}
        Some(Some("cancelled")) => {
            doc.event.event_status = Some(crate::event::EventStatus::Cancelled);
        }
        _ => return Read::NoCard,
    }
    // The venue: the first location, by its name, its `geo:` point, its link.
    if let Some(first) = content["locations"].get(0) {
        let uri = first["uri"].as_str().map(str::trim).unwrap_or_default();
        let name = text(first, "name", 100);
        let point = geo(uri);
        let link = object.link(uri);
        doc.location = point.clone();
        if let Some(label) = name.clone().or_else(|| link.clone()) {
            doc.event.venue = Some(crate::event::Venue {
                text: label.clone(),
                name,
                location_source: point.as_ref().map(|_| crate::model::LocationSource::Uri),
                location: point,
                ..Default::default()
            });
            fact(doc, "location", Some(label));
        }
        fact(doc, "location_uri", link);
    }
    fact(doc, "kind", Some("event".into()));
    Read::Card
}
/// An rsvp: its status, guests and the event it answers.
fn rsvp(envelope: &PostEnvelope<AppKind>, content: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(status) = content["status"]
        .as_str()
        .filter(|s| matches!(*s, "requested" | "accepted" | "declined" | "tentative"))
    else {
        return Read::NoCard;
    };
    let Some(parent) = envelope.parent.as_deref() else {
        return Read::NoCard;
    };
    doc.set_type("RsvpAction");
    doc.name = Some(format!("RSVP: {status}"));
    fact(doc, "status", Some(status.into()));
    fact(
        doc,
        "guests",
        content["guests"].as_u64().map(|n| n.to_string()),
    );
    fact(doc, "event", Some(parent.into()));
    fact(doc, "kind", Some("rsvp".into()));
    Read::Card
}
/// A point from a `geo:` URI.
fn geo(uri: &str) -> Option<crate::model::Point> {
    let (lat, lon) = uri.strip_prefix("geo:")?.split_once(',')?;
    let (lat, lon) = (lat.parse::<f64>().ok()?, lon.parse::<f64>().ok()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
        .then_some(crate::model::Point { lat, lon })
}
/// A route: its name, description, first waypoint, the box of its coordinate waypoints, and its
/// measures.
fn route(content: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(name) = text(content, "name", 100) else {
        return Read::NoCard;
    };
    let Some(waypoints) = content["waypoints"].as_array().and_then(|w| {
        w.iter()
            .map(|w| w["uri"].as_str().map(str::to_owned))
            .collect::<Option<Vec<String>>>()
    }) else {
        return Read::NoCard;
    };
    if !(2..=100).contains(&waypoints.len()) {
        return Read::NoCard;
    }
    doc.set_type("Trip");
    doc.name = Some(name);
    doc.description = text(content, "description", 2000);
    doc.location = waypoints.first().and_then(|w| geo(w));
    let points: Vec<_> = waypoints.iter().filter_map(|w| geo(w)).collect();
    if !points.is_empty() {
        let (mut w, mut s, mut e, mut n) = (180.0f64, 90.0f64, -180.0f64, -90.0f64);
        for p in &points {
            (w, s, e, n) = (w.min(p.lon), s.min(p.lat), e.max(p.lon), n.max(p.lat));
        }
        doc.bbox = Some([w, s, e, n]);
    }
    fact(doc, "activity", text(content, "activity", 20));
    for key in [
        "distance_m",
        "duration_s",
        "elevation_gain_m",
        "elevation_loss_m",
    ] {
        fact(doc, key, content[key].as_u64().map(|n| n.to_string()));
    }
    fact(doc, "waypoints", Some(waypoints.len().to_string()));
    fact(doc, "kind", Some("route".into()));
    Read::Card
}
/// An availability: a schedule named and timed by its first available block.
fn availability(content: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(zone_name) = content["timeZone"].as_str() else {
        return Read::NoCard;
    };
    let Some(zone) = time::zone(zone_name) else {
        return Read::NoCard;
    };
    let Some(horizon) = content["horizonDays"]
        .as_u64()
        .filter(|d| (1..=366).contains(d))
    else {
        return Read::NoCard;
    };
    let count = |key: &str| content[key].as_array().map_or(0, Vec::len);
    if count("available") > 100 || count("busy") > 100 {
        return Read::NoCard;
    }
    let first = content["available"].get(0);
    doc.set_type("Schedule");
    doc.name = first
        .and_then(|b| text(b, "title", 100))
        .or_else(|| Some("Availability".into()));
    doc.start = first.and_then(|b| local(b["start"].as_str()?, zone));
    doc.end = first.and_then(|b| {
        let length = time::duration(b["duration"].as_str()?)?;
        doc.start?.checked_add_signed(length)
    });
    doc.event.tz = Some(zone_name.into());
    fact(doc, "horizon_days", Some(horizon.to_string()));
    fact(
        doc,
        "available_blocks",
        Some(count("available").to_string()),
    );
    fact(doc, "busy_blocks", Some(count("busy").to_string()));
    fact(doc, "for", text(content, "for", 1024));
    fact(doc, "kind", Some("availability".into()));
    Read::Card
}
/// A listen: who (the post's author), what (the track it embeds) and when (the post's time).
fn listen(object: &Object, envelope: &PostEnvelope<AppKind>, doc: &mut ResourceMeta) -> Read {
    let Some(track) = envelope.embed.as_deref() else {
        return Read::NoCard;
    };
    doc.set_type("ListenAction");
    doc.name = Some("Listen".into());
    doc.start = written(object.var("id"));
    fact(doc, "track", Some(track.into()));
    fact(doc, "kind", Some("listen".into()));
    Read::Card
}
/// A session: a work named by its `name` or `title`.
fn session(content: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(name) = text(content, "name", 100).or_else(|| text(content, "title", 100)) else {
        return Read::NoCard;
    };
    doc.set_type("CreativeWork");
    doc.name = Some(name);
    doc.description = text(content, "description", 2000);
    fact(doc, "kind", Some("session".into()));
    Read::Card
}
