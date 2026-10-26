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
    links_off: None,
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

/// What a recurring event's rule (`rrule`) says about its occurrence that ends after `now`.
#[derive(Debug, PartialEq)]
enum Series {
    /// The series' next or current occurrence starts then.
    At(DateTime<Utc>),
    /// The series has ended (its rule's `COUNT` or `UNTIL`, or its last listed date, is past):
    /// its first occurrence, the first date its set gives (an excluded DTSTART is not one);
    /// `None` when every date is excluded.
    Over(Option<DateTime<Utc>>),
    /// Not known here: the rule is not read (not RFC 5545 characters, over 512 of them, or not
    /// one `time::rrule_bounded` lets expand), a date list is not read (over `MAX_DATES` values,
    /// or one that is no time), or the occurrence lies past the expansion's budget.
    Unknown,
}
/// The occurrences, and the time, one expansion may take.
const MAX_OCCURRENCES: usize = 10_000;
const MAX_EXPANSION: std::time::Duration = std::time::Duration::from_millis(100);
/// The most dates an event's `rdate` or `exdate` list may hold.
const MAX_DATES: usize = 128;

/// An event's `rdate` or `exdate` list as instants, read in the event's zone (a value with an
/// offset keeps its own). `None` when the list is over `MAX_DATES` or holds a value that is no
/// time: an exclusion that cannot be read must not be dropped.
fn dates(v: &Value, key: &str, zone: &str) -> Option<Vec<DateTime<Utc>>> {
    let Some(list) = v.get(key).filter(|l| !l.is_null()) else {
        return Some(vec![]);
    };
    let list = list.as_array()?;
    if list.len() > MAX_DATES {
        return None;
    }
    list.iter()
        .map(|d| {
            let text = d.as_str()?.trim();
            (text.len() <= 64).then(|| time::parse(text, Some(zone)))?
        })
        .collect()
}

/// The series of an event, from its first occurrence and length: its dates are DTSTART, its
/// rule's (if any) and its `rdate` list, less its `exdate` list. `None` for a one-off event, one
/// with no rule and no date list.
fn series(
    v: &Value,
    zone: &str,
    first: DateTime<Utc>,
    length: Duration,
    now: DateTime<Utc>,
    max_period_dates: usize,
) -> Option<Series> {
    let rule = read::text(v, &["rrule"]);
    let listed = |key: &str| v.get(key).is_some_and(|l| !l.is_null());
    if rule.is_none() && !listed("rdate") && !listed("exdate") {
        return None;
    }
    if rule.as_ref().is_some_and(|rule| {
        rule.len() > 512
            || !rule
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | ';' | ',' | '-' | '+'))
    }) {
        return Some(Series::Unknown);
    }
    let (Some(rdates), Some(exdates)) = (dates(v, "rdate", zone), dates(v, "exdate", zone)) else {
        return Some(Series::Unknown);
    };
    // The set: DTSTART in the zone's canonical name and the rule from it, if there is one;
    // DTSTART itself as a date (RFC 5545: the first occurrence, even where the rule would not
    // give it); the event's added and excluded dates as instants (an exclusion applies to
    // DTSTART too).
    let instant = |at: &DateTime<Utc>| at.format("%Y%m%dT%H%M%SZ").to_string();
    let Some(set) = time::zone(zone).and_then(|tz| {
        let mut lines = vec![format!(
            "DTSTART;TZID={}:{}",
            tz.name(),
            first.with_timezone(&tz).format("%Y%m%dT%H%M%S")
        )];
        lines.extend(rule.iter().map(|rule| format!("RRULE:{rule}")));
        lines.push(format!("RDATE:{}", instant(&first)));
        for (name, list) in [("RDATE", &rdates), ("EXDATE", &exdates)] {
            if !list.is_empty() {
                let values: Vec<String> = list.iter().map(instant).collect();
                lines.push(format!("{name}:{}", values.join(",")));
            }
        }
        lines
            .join("\n")
            .parse::<rrule::RRuleSet>()
            .ok()
            // Its periods must stay small (`time::rrule_bounded`), however its names are spelled.
            .filter(|set| time::rrule_bounded(set, max_period_dates))
    }) else {
        return Some(Series::Unknown);
    };
    let Some(after) = now.checked_sub_signed(length) else {
        return Some(Series::Unknown);
    };
    // Whether the set has an end: no rule (its dates are listed), or a rule with `COUNT` or
    // `UNTIL`, from the parsed rule.
    let ends = set.get_rrule().is_empty()
        || set
            .get_rrule()
            .iter()
            .any(|r| r.get_count().is_some() || r.get_until().is_some());
    let deadline = std::time::Instant::now() + MAX_EXPANSION;
    let mut dates = set.limit().into_iter();
    let mut earliest = None;
    for _ in 0..MAX_OCCURRENCES {
        match dates.next().map(|at| at.with_timezone(&Utc)) {
            Some(at) if at > after => return Some(Series::At(at)),
            Some(at) if std::time::Instant::now() < deadline => {
                earliest = earliest.or(Some(at));
            }
            Some(_) => return Some(Series::Unknown),
            // Only a set with an end has run out; a rule without one (the crate gives up on a
            // rule that matches too rarely) is not over.
            None if ends => return Some(Series::Over(earliest)),
            None => return Some(Series::Unknown),
        }
    }
    Some(Series::Unknown)
}

/// An event: its summary, description, times (local times in their time zone; a time without a
/// zone is left out; a recurring event's are its next or current occurrence's, and none when
/// that is not known) and first location's label.
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
        // long past while the series goes on. When that is not known the card has no time, so
        // it is fetched again on the usual cadence and never made final. A series that has ended
        // shows its first occurrence (final), and one whose every date is excluded has no time.
        if let Some(first) = doc.start {
            let length = doc.end.map_or(Duration::zero(), |end| end - first);
            match series(v, zone, first, length, object.now, object.max_period_dates) {
                Some(Series::At(next)) => {
                    doc.start = Some(next);
                    doc.end = doc.end.and_then(|_| next.checked_add_signed(length));
                }
                Some(Series::Over(Some(earliest))) => {
                    doc.start = Some(earliest);
                    doc.end = doc.end.and_then(|_| earliest.checked_add_signed(length));
                }
                Some(Series::Unknown | Series::Over(None)) => (doc.start, doc.end) = (None, None),
                None => {}
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
