//! Eventky (events and calendars, eventky.app): an event and a calendar. Attendance answers
//! (`attendees/`) and tags are never fetched. Events also reach Pinakes as Nexus event
//! references, whose cards carry the same client link.
use super::{read, Client, Kind, Object, Profile, Read};
use crate::{event::time, model::ResourceMeta};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

pub static PROFILE: Profile = Profile {
    namespaces: &["eventky.app"],
    kinds: &[
        Kind {
            path: "events/{id}",
            read: event,
            open_in: &[Client {
                client: "Eventky",
                url: "https://eventky.app/event/{owner}/{id}",
            }],
        },
        Kind {
            path: "calendars/{id}",
            read: calendar,
            open_in: &[Client {
                client: "Eventky",
                url: "https://eventky.app/calendar/{owner}/{id}",
            }],
        },
    ],
    facts: &["location", "timezone"],
};

/// The plain text of a description: `description`, else the styled one (HTML, Markdown or plain
/// text) with its markup dropped.
fn description(v: &Value) -> Option<String> {
    read::text(v, &["description"]).or_else(|| {
        let styled = &v["styled_description"];
        let content = read::text(styled, &["content"])?;
        match styled.get("format").and_then(Value::as_str) {
            Some("html") => read::html_text(&content),
            _ => Some(content),
        }
    })
}

/// The start of a recurring event's (`rrule`) occurrence that ends after `now`: the series' next
/// or current one. `None` for a one-off event, a finished series, or a rule that does not read
/// (it must be RFC 5545 characters only, at most 512 of them, and give an occurrence within
/// 10,000 steps).
fn next_occurrence(
    v: &Value,
    zone: &str,
    first: DateTime<Utc>,
    length: Duration,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let rule = read::text(v, &["rrule"])?;
    if rule.len() > 512
        || !rule
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | ';' | ',' | '-' | '+'))
    {
        return None;
    }
    let wall = first.with_timezone(&time::zone(zone)?);
    let set: rrule::RRuleSet = format!(
        "DTSTART;TZID={zone}:{}\nRRULE:{rule}",
        wall.format("%Y%m%dT%H%M%S")
    )
    .parse()
    .ok()?;
    let after = now.checked_sub_signed(length)?;
    // DTSTART is the first occurrence even where the rule would not give it (RFC 5545).
    if first > after {
        return Some(first);
    }
    set.limit()
        .into_iter()
        .take(10_000)
        .map(|at| at.with_timezone(&Utc))
        .find(|at| *at > after)
}

/// An event: its summary, description, times (local times in their time zone; a time without a
/// zone is left out; a recurring event's are its next or current occurrence's) and first
/// location's label.
fn event(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(summary) = read::text(v, &["summary"]) else {
        return Read::NoCard;
    };
    doc.set_type("Event");
    doc.name = Some(summary);
    doc.description = description(v);
    let zone = read::text(v, &["dtstart_tzid"]).filter(|z| time::zone(z).is_some());
    if let Some(zone) = &zone {
        doc.start = read::text(v, &["dtstart"]).and_then(|t| time::parse(&t, Some(zone)));
        let end_zone = read::text(v, &["dtend_tzid"]).unwrap_or_else(|| zone.clone());
        doc.end = read::text(v, &["dtend"])
            .and_then(|t| time::parse(&t, Some(&end_zone)))
            .or_else(|| {
                let duration = time::duration(&read::text(v, &["duration"])?)?;
                doc.start
                    .and_then(|start| start.checked_add_signed(duration))
            })
            .filter(|end| doc.start.is_some_and(|start| *end >= start));
        // A series is shown, and fetched again, by its next occurrence: its first one may be
        // long past while the series goes on.
        if let Some(first) = doc.start {
            let length = doc.end.map_or(Duration::zero(), |end| end - first);
            if let Some(next) = next_occurrence(v, zone, first, length, object.now) {
                doc.start = Some(next);
                doc.end = doc.end.and_then(|_| next.checked_add_signed(length));
            }
        }
        doc.event.tz = Some(zone.clone());
    }
    if v.get("status").and_then(Value::as_str) == Some("CANCELLED") {
        doc.event.event_status = Some(crate::event::EventStatus::Cancelled);
    }
    // The first location: `label` (or `name` in older events).
    if let Some(label) = v["locations"]
        .get(0)
        .and_then(|l| read::text(l, &["label", "name"]))
    {
        doc.attributes.insert("location".into(), label);
    }
    Read::Card
}

/// A calendar: its name, description and time zone.
fn calendar(_: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(name) = read::text(v, &["name"]) else {
        return Read::NoCard;
    };
    doc.set_type("Collection");
    doc.name = Some(name);
    doc.description = read::text(v, &["description"]);
    if let Some(zone) = read::text(v, &["timezone"]).filter(|z| time::zone(z).is_some()) {
        doc.attributes.insert("timezone".into(), zone);
    }
    Read::Card
}
