//! Bounded streaming content-line parser and RFC 5545 occurrence expansion.
use super::{page, time, Attendance, Container, EventStatus, Occurrence, Organizer, Venue};
use crate::{
    config::EventConfig,
    model::{LocationSource, ResourceMeta},
    resolver::web::origin_url,
};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Duration, Months, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};
use url::Url;

#[derive(Clone, Debug, Default)]
struct Property {
    name: String,
    params: BTreeMap<String, String>,
    value: String,
}
#[derive(Clone, Debug, Default)]
struct Component {
    invalid: bool,
    name: String,
    props: Vec<Property>,
    children: Vec<Component>,
}
impl Component {
    fn get(&self, name: &str) -> Option<&Property> {
        self.props.iter().find(|p| p.name == name)
    }
    fn value(&self, name: &str) -> Option<&str> {
        self.get(name).map(|p| p.value.as_str())
    }
    fn text(&self, name: &str) -> Option<String> {
        self.value(name).map(unescape)
    }
}
fn split_quoted(s: &str, sep: char) -> Vec<&str> {
    let mut quoted = false;
    let mut start = 0;
    let mut parts = vec![];
    for (i, c) in s.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c == sep && !quoted {
            parts.push(&s[start..i]);
            start = i + 1;
        }
    }
    parts.push(&s[start..]);
    parts
}
fn property(line: &str) -> Result<Property> {
    let mut quoted = false;
    let i = line
        .char_indices()
        .find_map(|(i, c)| {
            if c == '"' {
                quoted = !quoted;
            }
            (c == ':' && !quoted).then_some(i)
        })
        .context("content line missing colon")?;
    let fields = split_quoted(&line[..i], ';');
    let mut params = BTreeMap::new();
    for p in &fields[1..] {
        let (k, v) = p.split_once('=').context("invalid parameter")?;
        params.insert(k.to_ascii_uppercase(), caret(v.trim_matches('"')));
    }
    Ok(Property {
        name: fields[0].to_ascii_uppercase(),
        params,
        value: line[i + 1..].into(),
    })
}
fn caret(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '^' {
            match it.next() {
                Some('n' | 'N') => out.push('\n'),
                Some('^') => out.push('^'),
                Some('\'') => out.push('"'),
                Some(c) => {
                    out.push('^');
                    out.push(c);
                }
                None => out.push('^'),
            }
        } else {
            out.push(c);
        }
    }
    out
}
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(c) => out.push(c),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}
fn components(bytes: &[u8], cfg: &EventConfig, deadline: std::time::Instant) -> Result<Component> {
    let mut stack: Vec<Component> = vec![];
    let mut root = None;
    let mut pending = String::new();
    let mut count = 0usize;
    let mut consume = |line: &str| -> Result<()> {
        if line.trim().is_empty() {
            return Ok(());
        }
        let p = match property(line) {
            Ok(p) => p,
            Err(e) => {
                if let Some(event) = stack.iter_mut().find(|c| c.name == "VEVENT") {
                    event.invalid = true;
                    return Ok(());
                }
                return Err(e);
            }
        };
        if p.name == "BEGIN" {
            ensure!(stack.len() < 8, "calendar nesting cap");
            count += 1;
            ensure!(count <= cfg.max_components, "calendar component cap");
            stack.push(Component {
                name: p.value.to_ascii_uppercase(),
                ..Default::default()
            });
        } else if p.name == "END" {
            let node = stack.pop().context("unexpected END")?;
            ensure!(node.name.eq_ignore_ascii_case(&p.value), "mismatched END");
            if let Some(parent) = stack.last_mut() {
                parent.children.push(node);
            } else {
                ensure!(root.is_none(), "multiple calendars");
                root = Some(node);
            }
        } else {
            stack
                .last_mut()
                .context("property outside calendar")?
                .props
                .push(p);
        }
        Ok(())
    };
    // Borrow physical byte slices before decoding: a hostile line cannot allocate past its cap.
    for line in bytes.split(|b| *b == b'\n') {
        ensure!(
            std::time::Instant::now() < deadline,
            "calendar parse budget"
        );
        ensure!(
            line.len() <= cfg.max_line_bytes,
            "calendar physical line cap"
        );
        let line = std::str::from_utf8(line)
            .unwrap_or("INVALID_CONTENT_LINE")
            .trim_end_matches('\r');
        if line.starts_with([' ', '\t']) {
            ensure!(!pending.is_empty(), "orphan folded line");
            ensure!(
                pending.len() + line.len() <= cfg.max_line_bytes,
                "calendar unfolded line cap"
            );
            pending.push_str(&line[1..]);
        } else {
            if !pending.is_empty() {
                consume(&pending)?;
            }
            pending = line.trim_start_matches('\u{feff}').into();
        }
    }
    if !pending.is_empty() {
        consume(&pending)?;
    }
    ensure!(stack.is_empty(), "truncated calendar");
    let root = root.context("empty calendar")?;
    ensure!(root.name == "VCALENDAR", "expected VCALENDAR");
    Ok(root)
}
#[derive(Clone, Debug)]
struct Stamp {
    at: DateTime<Utc>,
    wall: NaiveDateTime,
    date: bool,
    zone: Option<String>,
    /// Where `zone` came from; decides wall-clock expansion of UTC series.
    source: ZoneSource,
    unknown: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneSource {
    None,
    Tzid,
    Calendar,
    Default,
    Coordinates,
}
struct Zones<'a> {
    deadline: std::time::Instant,
    max_date_values: usize,
    calendar: Option<&'a str>,
    default: Option<&'a str>,
    embedded: BTreeMap<String, &'a Component>,
    transitions: RefCell<BTreeMap<(String, i32), Vec<Transition>>>,
}
impl Zones<'_> {
    fn wall_at(&self, stamp: &Stamp, at: DateTime<Utc>) -> Result<NaiveDateTime> {
        if let Some(name) = stamp.zone.as_deref() {
            if let Some(zone) = time::zone(name) {
                return Ok(at.with_timezone(&zone).naive_local());
            }
            if let Some(zone) = self.embedded.get(name) {
                for offset in zone
                    .children
                    .iter()
                    .flat_map(|c| {
                        [&c.value("TZOFFSETFROM"), &c.value("TZOFFSETTO")]
                            .map(|v| v.and_then(time::fixed))
                    })
                    .flatten()
                {
                    ensure!(
                        std::time::Instant::now() < self.deadline,
                        "VTIMEZONE work budget"
                    );
                    let wall = at.with_timezone(&offset).naive_local();
                    if self.at_wall(stamp, wall).is_ok_and(|v| v == at) {
                        return Ok(wall);
                    }
                }
                // The second pass of a repeated hour: its wall time maps forward to the first
                // pass, so accept the wall that lies in a transition's repeated window under
                // that transition's later offset.
                if let Some(wall) = self.second_pass(zone, at)? {
                    return Ok(wall);
                }
                bail!("custom zone reverse conversion");
            }
        }
        Ok(at.naive_utc())
    }
    fn stamp(&self, p: &Property, value: &str) -> Result<Stamp> {
        let date = p.params.get("VALUE").is_some_and(|v| v == "DATE") || value.len() == 8;
        let zulu = value.ends_with('Z');
        let wall = time::naive(value.trim_end_matches('Z')).context("invalid calendar date")?;
        let name = p
            .params
            .get("TZID")
            .map(String::as_str)
            .or(self.calendar)
            .or(self.default);
        let zone = name.and_then(time::zone).or_else(|| {
            // An unrecognized TZID falls back to the advertised calendar zone.
            if name.is_some_and(|n| self.embedded.contains_key(n)) {
                None
            } else {
                self.calendar
                    .and_then(time::zone)
                    .or_else(|| self.default.and_then(time::zone))
            }
        });
        let custom = name.and_then(|n| self.embedded.get(n)).copied();
        let tzid = p.params.get("TZID").map(String::as_str);
        let source = if zone.is_none() && custom.is_none() {
            ZoneSource::None
        } else if tzid.is_some_and(|n| time::zone(n).is_some() || self.embedded.contains_key(n)) {
            ZoneSource::Tzid
        } else if self.calendar.and_then(time::zone).is_some() {
            ZoneSource::Calendar
        } else {
            ZoneSource::Default
        };
        let at = if zulu {
            wall.and_utc()
        } else if let Some(z) = zone {
            time::local(z, wall).context("local date outside zone")?
        } else if let Some(z) = custom {
            self.custom_time(z, wall)?
        } else {
            wall.and_utc()
        };
        Ok(Stamp {
            at,
            wall,
            date,
            zone: zone
                .map(|z| z.to_string())
                .or_else(|| custom.and(name).map(str::to_owned)),
            source,
            unknown: !zulu && name.is_some() && zone.is_none() && custom.is_none(),
        })
    }
    /// A series value (RDATE, EXDATE, RECURRENCE-ID, UNTIL): floating values take the series zone,
    /// and in a series without one (a UTC series) they are UTC, never the calendar's
    /// X-WR-TIMEZONE or the configured default.
    fn series_stamp(&self, p: &Property, value: &str, start: &Stamp) -> Result<Stamp> {
        if !value.ends_with('Z') && !p.params.contains_key("TZID") {
            let mut p = p.clone();
            p.params.insert(
                "TZID".into(),
                start.zone.clone().unwrap_or_else(|| "UTC".into()),
            );
            let mut stamp = self.stamp(&p, value)?;
            if start.zone.is_none() {
                stamp.zone = None;
                stamp.source = ZoneSource::None;
            }
            return Ok(stamp);
        }
        self.stamp(p, value)
    }
    fn at_wall(&self, stamp: &Stamp, wall: NaiveDateTime) -> Result<DateTime<Utc>> {
        if let Some(zone) = stamp.zone.as_deref() {
            if let Some(zone) = time::zone(zone) {
                return time::local(zone, wall).context("invalid wall time");
            }
            if let Some(c) = self.embedded.get(zone) {
                return self.custom_time(c, wall);
            }
        }
        Ok(wall.and_utc())
    }
}
type Transition = (NaiveDateTime, chrono::FixedOffset, chrono::FixedOffset);
fn transitions(
    c: &Component,
    wall: NaiveDateTime,
    deadline: std::time::Instant,
    max_date_values: usize,
) -> Result<Vec<Transition>> {
    let mut transitions = vec![];
    let mut values = 0usize;
    let mut budget = || -> Result<()> {
        values += 1;
        ensure!(
            values <= max_date_values && std::time::Instant::now() < deadline,
            "VTIMEZONE work budget"
        );
        Ok(())
    };
    for o in &c.children {
        budget()?;
        let start = time::naive(o.value("DTSTART").context("VTIMEZONE DTSTART")?)
            .context("VTIMEZONE date")?;
        let from = time::fixed(o.value("TZOFFSETFROM").context("VTIMEZONE FROM")?)
            .context("VTIMEZONE offset")?;
        let to = time::fixed(o.value("TZOFFSETTO").context("VTIMEZONE TO")?)
            .context("VTIMEZONE offset")?;
        let mut dates = vec![start];
        if let Some(rule) = o.value("RRULE") {
            let set: rrule::RRuleSet =
                format!("DTSTART:{}Z\nRRULE:{}", start.format("%Y%m%dT%H%M%S"), rule).parse()?;
            let end = wall
                .checked_add_signed(Duration::days(370))
                .context("VTIMEZONE horizon overflow")?
                .and_utc()
                .with_timezone(&rrule::Tz::UTC);
            for date in set.limit().into_iter() {
                budget()?;
                if date > end {
                    break;
                }
                dates.push(date.naive_utc());
            }
        }
        for p in o.props.iter().filter(|p| p.name == "RDATE") {
            for value in p.value.split(',') {
                budget()?;
                if let Some(date) = time::naive(value) {
                    dates.push(date);
                }
            }
        }
        for d in dates {
            transitions.push((d, from, to));
        }
    }
    transitions.sort_by_key(|t| t.0);
    Ok(transitions)
}
impl Zones<'_> {
    /// The wall time of `at` when it is the second pass of an embedded zone's repeated hour.
    fn second_pass(&self, c: &Component, at: DateTime<Utc>) -> Result<Option<NaiveDateTime>> {
        use chrono::Datelike;
        // Load the year's transitions as `custom_time` does.
        let year_wall = at.naive_utc();
        let key = (c.value("TZID").unwrap_or("").to_owned(), year_wall.year());
        let mut cache = self.transitions.borrow_mut();
        if !cache.contains_key(&key) {
            cache.insert(
                key.clone(),
                transitions(c, year_wall, self.deadline, self.max_date_values)?,
            );
        }
        for &(start, from, to) in &cache[&key] {
            let delta = to.local_minus_utc() - from.local_minus_utc();
            if delta >= 0 {
                continue;
            }
            let wall = at.with_timezone(&to).naive_local();
            if wall >= start + Duration::seconds(delta as i64) && wall < start {
                return Ok(Some(wall));
            }
        }
        Ok(None)
    }
    fn custom_time(&self, c: &Component, wall: NaiveDateTime) -> Result<DateTime<Utc>> {
        use chrono::Datelike;
        ensure!(
            std::time::Instant::now() < self.deadline,
            "VTIMEZONE work budget"
        );
        let key = (c.value("TZID").unwrap_or("").to_owned(), wall.year());
        let mut cache = self.transitions.borrow_mut();
        if !cache.contains_key(&key) {
            cache.insert(
                key.clone(),
                transitions(c, wall, self.deadline, self.max_date_values)?,
            );
        }
        let transitions = &cache[&key];
        let mut chosen = transitions
            .first()
            .map(|t| t.1)
            .context("empty VTIMEZONE")?;
        for &(at, from, to) in transitions {
            ensure!(
                std::time::Instant::now() < self.deadline,
                "VTIMEZONE work budget"
            );
            let delta = to.local_minus_utc() - from.local_minus_utc();
            if delta > 0
                && wall >= at
                && wall
                    < at.checked_add_signed(Duration::seconds(delta as i64))
                        .context("VTIMEZONE transition overflow")?
            {
                chosen = from;
                break;
            }
            if delta < 0
                && wall
                    >= at
                        .checked_add_signed(Duration::seconds(delta as i64))
                        .context("VTIMEZONE transition overflow")?
                && wall < at
            {
                chosen = from;
                break;
            }
            if wall >= at {
                chosen = to;
            } else {
                break;
            }
        }
        chosen
            .from_local_datetime(&wall)
            .single()
            .map(|d| d.to_utc())
            .context("VTIMEZONE conversion")
    }
}
fn recurrence_key(s: &Stamp) -> String {
    if s.date {
        s.wall.date().to_string()
    } else {
        s.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn canonical_feed(uri: &str) -> String {
    let normalized = uri
        .replacen("webcals://", "https://", 1)
        .replacen("webcal://", "https://", 1);
    let Ok(mut u) = Url::parse(&normalized) else {
        return normalized;
    };
    u.set_fragment(None);
    if u.path().contains("/apps/calendar/")
        || u.path().contains("/remote.php/dav/public-calendars/")
    {
        let p = u.path().replace("/index.php/", "/");
        u.set_path(p.trim_end_matches('/'));
    }
    // Percent encodings are canonicalized as bytes; unreserved spelling differences share feed keys.
    if u.host_str() == Some("calendar.google.com") {
        let p = percent_encoding::percent_decode_str(u.path())
            .decode_utf8_lossy()
            .into_owned();
        u.set_path(&p);
    }
    u.to_string()
}
fn platform_id(uri: &str) -> Option<String> {
    let u = Url::parse(uri).ok()?;
    let h = u.host_str()?;
    if h.starts_with("eventfrog.") {
        let id = u
            .path()
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| s.len() >= 15)
            .max_by_key(|s| s.len())?;
        return Some(format!("eventfrog:{id}"));
    }
    if h == "ti.to" {
        let s: Vec<_> = u
            .path_segments()?
            .filter(|s| !s.is_empty())
            .take(2)
            .collect();
        if s.len() == 2 {
            return Some(format!("tito:{}/{}", s[0], s[1].trim_end_matches(".ics")));
        }
    }
    if h.ends_with("petzi.ch") {
        let s = u
            .path()
            .split('/')
            .find(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
        return Some(format!("petzi:{}", s.split('-').next()?));
    }
    if h.ends_with("weezevent.com") {
        let s = u
            .path_segments()?
            .find(|s| s.starts_with('E') && s.chars().nth(1).is_some_and(|c| c.is_ascii_digit()))?;
        return Some(format!("weezevent:{}", s.trim_end_matches(".ics")));
    }
    None
}
fn content_hash(c: &Component) -> String {
    let mut props: Vec<_> = c
        .props
        .iter()
        .filter(|p| {
            !matches!(p.name.as_str(), "DTSTAMP" | "X-LIC-ERROR" | "UID")
                && (!p.name.starts_with("X-")
                    || matches!(
                        p.name.as_str(),
                        "X-ALT-DESC"
                            | "X-APPLE-STRUCTURED-LOCATION"
                            | "X-GOOGLE-CONFERENCE"
                            | "X-MICROSOFT-SKYPETEAMSMEETINGURL"
                    ))
        })
        .map(|p| format!("{};{:?}:{}", p.name, p.params, unescape(&p.value)))
        .collect();
    props.sort();
    hash(props.join("\n").as_bytes())
}
fn conference(s: &str) -> bool {
    let html = scraper::Html::parse_fragment(s);
    let hrefs: Vec<_> = html
        .select(&scraper::Selector::parse("a[href]").unwrap())
        .filter_map(|n| n.value().attr("href").map(str::to_owned))
        .collect();
    s.split_whitespace()
        .chain(hrefs.iter().map(String::as_str))
        .filter_map(|s| Url::parse(s.trim_matches(['<', '>', '(', ')', ','])).ok())
        .filter_map(|u| u.host_str().map(str::to_owned))
        .any(|h| {
            h == "meet.google.com"
                || h == "teams.microsoft.com"
                || h == "teams.live.com"
                || h == "zoom.us"
                || h.ends_with(".zoom.us")
                || h == "zoom.com"
                || h.ends_with(".zoom.com")
        })
}
fn structured_address(text: &str) -> BTreeMap<String, String> {
    let parts: Vec<_> = text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = BTreeMap::new();
    for (i, part) in parts.iter().enumerate().skip(1) {
        let mut words = part.split_whitespace();
        let Some(postcode) = words.next() else {
            continue;
        };
        let city = words.collect::<Vec<_>>().join(" ");
        if (4..=10).contains(&postcode.len())
            && postcode.chars().any(|c| c.is_ascii_digit())
            && !city.is_empty()
            && parts[i - 1].chars().any(|c| c.is_ascii_digit())
        {
            out.insert("street".into(), parts[i - 1].to_string());
            out.insert("postalcode".into(), postcode.into());
            out.insert("city".into(), city);
            if let Some(country) = parts.get(i + 1) {
                out.insert("country".into(), country.to_string());
            }
            break;
        }
    }
    for value in out.values_mut() {
        crate::model::truncate(value, 256);
    }
    out
}
fn document(
    c: &Component,
    cal: &Component,
    base: &Url,
    zones: &Zones<'_>,
) -> Result<(ResourceMeta, Stamp)> {
    let start = zones.stamp(
        c.get("DTSTART").context("VEVENT missing DTSTART")?,
        c.value("DTSTART").unwrap(),
    )?;
    let mut doc = ResourceMeta::new(base.as_str(), "ics", 86400);
    doc.set_type("Event");
    doc.name = c.text("SUMMARY");
    doc.description = c.text("DESCRIPTION").or_else(|| {
        c.text("X-ALT-DESC").map(|s| {
            scraper::Html::parse_fragment(&s)
                .root_element()
                .text()
                .collect()
        })
    });
    for p in c.props.iter().filter(|p| p.name == "CATEGORIES") {
        let mut escaped = false;
        let mut start = 0;
        for (i, ch) in p.value.char_indices() {
            if ch == ',' && !escaped {
                doc.event.categories.push(unescape(&p.value[start..i]));
                start = i + 1;
            }
            escaped = ch == '\\' && !escaped;
        }
        doc.event.categories.push(unescape(&p.value[start..]));
    }
    doc.start = Some(start.at);
    doc.event.all_day = Some(start.date);
    doc.event.tz = start.zone.clone();
    if start.unknown {
        doc.event.warnings.push("unknown_tzid_utc_fallback".into());
    }
    if start.zone.is_none() && !c.value("DTSTART").unwrap().ends_with('Z') {
        doc.event.start_local = Some(start.wall.to_string());
    }
    doc.end = if let Some(p) = c.get("DTEND") {
        Some(zones.stamp(p, &p.value)?.at)
    } else if let Some(v) = c.value("DURATION") {
        Some(
            start
                .at
                .checked_add_signed(time::duration(v).context("invalid duration")?)
                .context("duration overflow")?,
        )
    } else if start.date {
        Some(
            zones.at_wall(
                &start,
                start
                    .wall
                    .checked_add_signed(Duration::days(1))
                    .context("all-day end overflow")?,
            )?,
        )
    } else {
        Some(start.at)
    };
    ensure!(doc.end >= doc.start, "end before start");
    doc.event.event_status = Some(if cal.value("METHOD") == Some("CANCEL") {
        EventStatus::Cancelled
    } else {
        c.value("STATUS")
            .and_then(EventStatus::parse)
            .unwrap_or(EventStatus::Scheduled)
    });
    if c.value("STATUS") == Some("TENTATIVE") {
        doc.event.warnings.push("tentative".into());
    }
    if let Some(p) = c.get("ORGANIZER") {
        let name = p
            .params
            .get("CN")
            .cloned()
            .or_else(|| (!p.value.starts_with("mailto:")).then(|| p.value.clone()))
            .or_else(|| cal.text("X-WR-CALNAME").or_else(|| cal.text("NAME")));
        doc.event.organizer = name.filter(|s| !s.contains('@')).map(|name| Organizer {
            name,
            url: origin_url(base, &p.value),
        });
    }
    if doc.event.organizer.is_none() {
        doc.event.organizer = cal
            .text("X-WR-CALNAME")
            .or_else(|| cal.text("NAME"))
            .map(|name| Organizer { name, url: None });
    }
    let apple = c.get("X-APPLE-STRUCTURED-LOCATION");
    let vlocation = c.children.iter().find(|c| c.name == "VLOCATION");
    let location = c
        .text("LOCATION")
        .or_else(|| apple.and_then(|a| a.params.get("X-TITLE").cloned()))
        .or_else(|| vlocation.and_then(|c| c.text("NAME")));
    let coord = c
        .value("GEO")
        .and_then(|s| s.split_once(';'))
        .or_else(|| apple?.value.strip_prefix("geo:")?.split_once(','))
        .or_else(|| vlocation?.value("GEO")?.split_once(';'))
        .and_then(|(lat, lon)| Some((lat.parse::<f64>().ok()?, lon.parse::<f64>().ok()?)))
        .and_then(|(lat, lon)| page::point(&serde_json::json!({"latitude":lat,"longitude":lon})));
    let online = [
        "CONFERENCE",
        "X-GOOGLE-CONFERENCE",
        "X-MICROSOFT-SKYPETEAMSMEETINGURL",
    ]
    .iter()
    .any(|k| c.value(k).is_some_and(|s| !s.is_empty()))
        || location.as_deref().is_some_and(conference)
        || doc.description.as_deref().is_some_and(conference)
        || c.value("X-ALT-DESC").is_some_and(conference);
    doc.event.attendance = Some(
        if online
            && (coord.is_some()
                || location
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty() && !conference(s)))
        {
            Attendance::Mixed
        } else if online {
            Attendance::Online
        } else {
            Attendance::Offline
        },
    );
    if doc.event.attendance != Some(Attendance::Online) {
        if location.is_some() || coord.is_some() {
            doc.event.venue = Some(Venue {
                address: structured_address(
                    apple
                        .and_then(|p| p.params.get("X-ADDRESS"))
                        .map(String::as_str)
                        .or(location.as_deref())
                        .unwrap_or(""),
                ),
                text: location.unwrap_or_default(),
                name: apple.and_then(|p| p.params.get("X-TITLE").cloned()),
                location: coord.clone(),
                location_source: coord.as_ref().map(|_| LocationSource::Page),
                ..Default::default()
            });
        }
        doc.location = coord;
        doc.location_source = doc.location.as_ref().map(|_| LocationSource::Page);
    }
    doc.image = c.props.iter().find_map(|p| {
        if p.name == "IMAGE"
            || (p.name == "ATTACH"
                && p.params
                    .get("FMTTYPE")
                    .is_some_and(|t| t.starts_with("image/")))
        {
            Url::parse(&p.value)
                .ok()
                .and_then(|_| origin_url(base, &p.value))
        } else {
            None
        }
    });
    if doc.image.is_none() {
        if let Some(html) = c.value("X-ALT-DESC") {
            let html = scraper::Html::parse_fragment(html);
            doc.image = html
                .select(&scraper::Selector::parse("img[src]").unwrap())
                .find_map(|el| origin_url(base, el.value().attr("src")?));
        }
    }
    if let Some(url) = c
        .value("URL")
        .filter(|s| !s.is_empty())
        .and_then(|s| origin_url(base, s))
    {
        doc.url = url;
    }
    doc = doc.bounded(&[])?;
    Ok((doc, start))
}
/// The stamp a series expands and resolves its floating RECURRENCE-ID, RDATE and EXDATE values
/// with. A UTC DTSTART expands in UTC (RFC 5545) unless its zone source is configured for
/// wall-clock expansion (Google exports a UTC DTSTART with a local rule and X-WR-TIMEZONE); then
/// its wall time is DTSTART in that zone, IANA or embedded VTIMEZONE alike.
fn series_start(
    c: &Component,
    start: &Stamp,
    zones: &Zones<'_>,
    cfg: &EventConfig,
) -> Result<Stamp> {
    let utc = c.value("DTSTART").is_some_and(|s| s.ends_with('Z'));
    if !utc || start.zone.is_none() {
        return Ok(start.clone());
    }
    let wall_clock = match cfg.utc_series_wall_clock {
        crate::config::UtcSeriesWallClock::Never => false,
        crate::config::UtcSeriesWallClock::XWrTimezone => {
            matches!(start.source, ZoneSource::Tzid | ZoneSource::Calendar)
        }
        crate::config::UtcSeriesWallClock::Coordinates => true,
    };
    if wall_clock {
        Ok(Stamp {
            wall: zones.wall_at(start, start.at)?,
            ..start.clone()
        })
    } else {
        Ok(Stamp {
            zone: None,
            source: ZoneSource::None,
            wall: start.at.naive_utc(),
            ..start.clone()
        })
    }
}
fn expand(
    c: &Component,
    start: &Stamp,
    zones: &Zones<'_>,
    lo: DateTime<Utc>,
    hi: DateTime<Utc>,
    cfg: &EventConfig,
    deadline: std::time::Instant,
) -> Result<BTreeMap<DateTime<Utc>, Option<Duration>>> {
    let series = series_start(c, start, zones, cfg).unwrap_or_else(|_| start.clone());
    let start = &series;
    let wall_start = start.wall;
    // A UNTIL instant is compared as an instant: its wall time may fall in a repeated or skipped
    // hour, so the rule runs an hour past it and instances after it are dropped.
    let mut until_at = None;
    let mut out = BTreeMap::new();
    out.insert(start.at, None);
    if let Some(rule) = c.value("RRULE") {
        ensure!(
            !(rule.split(';').any(|p| p.starts_with("COUNT="))
                && rule.split(';').any(|p| p.starts_with("UNTIL="))),
            "RRULE COUNT and UNTIL are exclusive"
        );
        let freq = rule
            .split(';')
            .find_map(|p| p.strip_prefix("FREQ="))
            .unwrap_or("");
        if !matches!(freq, "MONTHLY" | "YEARLY") {
            ensure!(
                !rule
                    .split(';')
                    .filter_map(|p| p.strip_prefix("BYDAY="))
                    .flat_map(|p| p.split(','))
                    .any(|p| p.chars().any(|c| c.is_ascii_digit())),
                "numeric BYDAY needs monthly/yearly frequency"
            );
        }
        let mut parts = vec![];
        for p in rule.split(';') {
            if let Some(v) = p.strip_prefix("UNTIL=") {
                let prop = Property {
                    name: "UNTIL".into(),
                    params: c
                        .get("DTSTART")
                        .map(|p| p.params.clone())
                        .unwrap_or_default(),
                    value: v.into(),
                };
                let mut until = zones.series_stamp(&prop, v, start)?;
                let wall = if v.len() == 8 {
                    until.wall = until
                        .wall
                        .checked_add_signed(Duration::days(1) - Duration::seconds(1))
                        .context("UNTIL overflow")?;
                    until.wall
                } else {
                    until_at = Some(until.at);
                    zones
                        .wall_at(start, until.at)?
                        .checked_add_signed(Duration::hours(1))
                        .context("UNTIL overflow")?
                };
                parts.push(format!("UNTIL={}Z", wall.format("%Y%m%dT%H%M%S")));
            } else {
                parts.push(p.into());
            }
        }
        let set: rrule::RRuleSet = format!(
            "DTSTART:{}Z\nRRULE:{}",
            wall_start.format("%Y%m%dT%H%M%S"),
            parts.join(";")
        )
        .parse()?;
        // Generate floating wall values with the bounded RRULE iterator, then apply RFC gap rules.
        let take = cfg
            .max_occurrences_per_series
            .saturating_mul(2)
            .saturating_add(10);
        let mut included = 0;
        for (steps, d) in set.limit().into_iter().enumerate() {
            ensure!(
                steps < cfg.max_recurrence_steps && std::time::Instant::now() < deadline,
                "recurrence work budget"
            );
            // The first instance is DTSTART itself: its wall time may be ambiguous (the repeated
            // autumn hour), and mapping it back could name the other instant as a second date.
            let at = if d.naive_utc() == wall_start {
                start.at
            } else {
                zones.at_wall(start, d.naive_utc())?
            };
            if until_at.is_some_and(|until| at > until) {
                continue;
            }
            if at > hi {
                break;
            }
            if at >= lo {
                out.insert(at, None);
                included += 1;
                if included >= take {
                    break;
                }
            }
        }
    }
    let mut values = 0usize;
    for p in c.props.iter().filter(|p| p.name == "RDATE") {
        for v in p.value.split(',') {
            values += 1;
            ensure!(
                values <= cfg.max_date_values && std::time::Instant::now() < deadline,
                "RDATE work budget"
            );
            let (s, end) = v.split_once('/').map_or((v, None), |(s, e)| (s, Some(e)));
            let at = zones.series_stamp(p, s, start)?.at;
            let duration = end
                .map(|e| {
                    if e.starts_with(['P', '+', '-']) {
                        time::duration(e).context("RDATE duration")
                    } else {
                        Ok(zones.series_stamp(p, e, start)?.at - at)
                    }
                })
                .transpose()?;
            ensure!(
                duration
                    .is_none_or(|d| d >= Duration::zero() && at.checked_add_signed(d).is_some()),
                "RDATE end overflow"
            );
            if at >= lo && at <= hi {
                out.insert(at, duration);
            }
        }
    }
    let mut dates = BTreeSet::new();
    let mut instants = BTreeSet::new();
    for p in c.props.iter().filter(|p| p.name == "EXDATE") {
        for v in p.value.split(',') {
            values += 1;
            ensure!(
                values <= cfg.max_date_values && std::time::Instant::now() < deadline,
                "EXDATE work budget"
            );
            let s = zones.series_stamp(p, v, start)?;
            if s.date {
                dates.insert(s.wall.date());
            } else {
                instants.insert(s.at);
            }
        }
    }
    out.retain(|at, _| {
        !instants.contains(at)
            && (dates.is_empty()
                || zones
                    .wall_at(start, *at)
                    .is_ok_and(|w| !dates.contains(&w.date())))
    });
    out.retain(|at, _| *at >= lo && *at <= hi);
    Ok(out)
}
/// The calendar expansion signature: the calendar extractor version and the configured
/// `utc_series_wall_clock` mode, the two things that re-key unchanged feeds.
pub fn expansion(cfg: &EventConfig) -> String {
    let version = crate::resolver::extractor_versions()["event"]
        .as_i64()
        .unwrap_or(0);
    let mode = serde_json::to_value(cfg.utc_series_wall_clock)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    format!("v{version}:{mode}")
}
pub fn parse(
    uri: &str,
    bytes: &[u8],
    cfg: &EventConfig,
    now: DateTime<Utc>,
) -> Result<ResourceMeta> {
    ensure!(bytes.len() <= cfg.feed_body_bytes, "calendar body cap");
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(cfg.parse_timeout_ms);
    let cal = components(bytes, cfg, deadline)?;
    let canonical = canonical_feed(uri);
    let base = Url::parse(&canonical)?;
    let zones = Zones {
        deadline,
        max_date_values: cfg.max_date_values,
        transitions: RefCell::default(),
        calendar: cal.value("X-WR-TIMEZONE"),
        default: cfg.default_timezone.as_deref(),
        embedded: cal
            .children
            .iter()
            .filter(|c| c.name == "VTIMEZONE")
            .filter_map(|c| Some((c.value("TZID")?.to_owned(), c)))
            .collect(),
    };
    let lo = now
        .checked_sub_months(Months::new(cfg.months_back))
        .context("horizon underflow")?;
    let hi = now
        .checked_add_months(Months::new(cfg.months_forward))
        .context("horizon overflow")?;
    let mut container = Container {
        limits: cfg.container.clone(),
        horizon_start: lo,
        horizon_end: hi,
        held_retry_secs: cfg.held_retry_secs,
        expansion: Some(expansion(cfg)),
        feed_ttl: cal
            .value("REFRESH-INTERVAL")
            .or_else(|| cal.value("X-PUBLISHED-TTL"))
            .and_then(time::duration)
            .map(|d| d.num_seconds().clamp(3600, 30 * 86400) as u64),
        ..Default::default()
    };
    type EventRevision<'a> = (
        &'a Component,
        ResourceMeta,
        Stamp,
        i64,
        Option<DateTime<Utc>>,
        bool,
    );
    let mut retained_bytes = 0usize;
    let mut events: BTreeMap<(String, String), EventRevision<'_>> = BTreeMap::new();
    for c in cal.children.iter().filter(|c| c.name == "VEVENT") {
        ensure!(
            std::time::Instant::now() < deadline,
            "calendar parse budget"
        );
        if c.invalid {
            container.warnings.push("invalid_vevent_line".into());
            continue;
        }
        let (mut doc, mut start) = match document(c, &cal, &base, &zones) {
            Ok(d) => d,
            Err(_) => {
                container.warnings.push("invalid_vevent_document".into());
                continue;
            }
        };
        if cfg.coordinate_timezone && doc.event.tz.is_none() {
            doc.event.tz = doc.location.as_ref().and_then(time::coordinate_zone);
            if let Some(tz) = &doc.event.tz {
                doc.event.timezone_attribution = Some(time::ATTRIBUTION.into());
                // Coordinates supply a zone for floating DTSTART/DTEND as well as
                // recurrence expansion; offset/UTC values remain exact instants.
                let mut derived = c.clone();
                for p in &mut derived.props {
                    if matches!(p.name.as_str(), "DTSTART" | "DTEND") && !p.value.ends_with('Z') {
                        p.params.insert("TZID".into(), tz.clone());
                    }
                }
                let (dated, stamp) = match document(&derived, &cal, &base, &zones) {
                    Ok(d) => d,
                    Err(_) => {
                        container.warnings.push("invalid_vevent_zone".into());
                        continue;
                    }
                };
                doc.start = dated.start;
                doc.end = dated.end;
                doc.event.start_local = None;
                start = stamp;
                start.zone = Some(tz.clone());
                start.source = ZoneSource::Coordinates;
            }
        }
        // From here on the series stamp: RECURRENCE-ID keys, expansion and overrides agree.
        // A zone that cannot convert keeps the plain stamp; its occurrences fail one by one below.
        let start = series_start(c, &start, &zones, cfg).unwrap_or(start);
        let uid = c.text("UID").filter(|s| !s.is_empty());
        let volatile = uid.is_none() && platform_id(&doc.url).is_none();
        let uid = if base.host_str().is_some_and(|s| s.starts_with("eventfrog."))
            || doc.url.contains("eventfrog.")
        {
            platform_id(&doc.url).or_else(|| platform_id(uri)).or(uid)
        } else {
            uid.or_else(|| platform_id(&doc.url))
        }
        .unwrap_or_else(|| {
            format!(
                "volatile:{}",
                hash(
                    format!(
                        "{canonical}\n{}\n{}",
                        start.at,
                        doc.name.as_deref().unwrap_or("")
                    )
                    .as_bytes()
                )
            )
        });
        if uid.len() > 1024 {
            container.warnings.push("invalid_vevent_uid".into());
            continue;
        }
        let rid = c
            .get("RECURRENCE-ID")
            .map(|p| {
                zones
                    .series_stamp(p, &p.value, &start)
                    .map(|s| recurrence_key(&s))
            })
            .transpose();
        let rid = match rid {
            Ok(r) => r.unwrap_or_default(),
            Err(_) => {
                container.warnings.push("invalid_recurrence_id".into());
                continue;
            }
        };
        let sequence = c
            .value("SEQUENCE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let modified = c
            .value("LAST-MODIFIED")
            .and_then(|s| time::parse(s, Some("UTC")));
        let key = (uid, rid);
        if let Some(old) = events.get(&key) {
            container.warnings.push("duplicate_uid".into());
            if (old.3, old.4) > (sequence, modified) {
                continue;
            }
        }
        let prior_bytes = events
            .get(&key)
            .map(|old| serde_json::to_vec(&old.1).map(|v| v.len()))
            .transpose()?
            .unwrap_or(0);
        let new_bytes = serde_json::to_vec(&doc)?.len();
        retained_bytes = retained_bytes
            .saturating_sub(prior_bytes)
            .saturating_add(new_bytes);
        ensure!(
            events.len() < cfg.max_event_revisions || events.contains_key(&key),
            "event revision cap"
        );
        ensure!(
            retained_bytes <= cfg.max_event_document_bytes,
            "event document memory cap"
        );
        events.insert(key, (c, doc, start, sequence, modified, volatile));
    }
    let mut hashes: BTreeMap<usize, String> = BTreeMap::new();
    let mut result: BTreeMap<(String, String), Occurrence> = BTreeMap::new();
    let mut expanded_bytes = 0usize;
    for ((uid, rid), (c, doc, start, seq, modified, volatile)) in &events {
        if !rid.is_empty() {
            continue;
        }
        let recurring = c.get("RRULE").is_some() || c.get("RDATE").is_some();
        ensure!(
            std::time::Instant::now() < deadline,
            "calendar parse budget"
        );
        let expanded = match expand(c, start, &zones, lo, hi, cfg, deadline) {
            Ok(v) => v,
            Err(e) if e.to_string().contains("budget") => return Err(e),
            Err(_) => {
                container.warnings.push("invalid_vevent_recurrence".into());
                continue;
            }
        };
        for (series_count, (at, duration)) in expanded.into_iter().enumerate() {
            ensure!(
                std::time::Instant::now() < deadline,
                "occurrence work budget"
            );
            if series_count >= cfg.max_occurrences_per_series {
                container.warnings.push("series_cap".into());
                break;
            }
            ensure!(
                result.len() < cfg.max_occurrences_per_feed,
                "calendar occurrence cap"
            );
            let occurrence = (|| -> Result<Option<Occurrence>> {
                let key = if recurring {
                    if start.date {
                        zones.wall_at(start, at)?.date().to_string()
                    } else {
                        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    }
                } else {
                    String::new()
                };
                let mut d = doc.clone();
                d.container = None;
                d.start = Some(at);
                d.end = at.checked_add_signed(
                    duration.unwrap_or_else(|| doc.end.unwrap_or(start.at) - start.at),
                );
                if d.end.is_none() {
                    container.warnings.push("occurrence_end_overflow".into());
                    return Ok(None);
                }
                if start.date && duration.is_none() {
                    let days = if let Some(end) = c.get("DTEND") {
                        zones.stamp(end, &end.value)?.wall - start.wall
                    } else {
                        c.value("DURATION")
                            .and_then(time::duration)
                            .unwrap_or(Duration::days(1))
                    };
                    let wall = zones.wall_at(start, at)?;
                    d.end = Some(
                        zones.at_wall(
                            start,
                            wall.checked_add_signed(days)
                                .context("all-day occurrence overflow")?,
                        )?,
                    );
                }
                let mut source = *c;
                let mut sequence = *seq;
                let mut changed = *modified;
                // RANGE applies the shift and new fields to subsequent original instance keys.
                let override_ = events.iter().rfind(|((u, r), (c, _, _, _, _, _))| {
                    u == uid
                        && !r.is_empty()
                        && (r == &key
                            || (r <= &key
                                && c.get("RECURRENCE-ID")
                                    .and_then(|p| p.params.get("RANGE"))
                                    .is_some_and(|s| s == "THISANDFUTURE")))
                });
                if let Some(((_, r), (c, override_doc, override_start, s, m, _))) = override_ {
                    let original = c.get("RECURRENCE-ID").context("override key")?;
                    let original = zones.series_stamp(original, &original.value, start)?;
                    d = override_doc.clone();
                    d.start = at.checked_add_signed(override_start.at - original.at);
                    if d.start.is_none() {
                        container.warnings.push("override_start_overflow".into());
                        return Ok(None);
                    }
                    d.end = d.start.unwrap().checked_add_signed(
                        override_doc.end.unwrap_or(override_start.at) - override_start.at,
                    );
                    if d.end.is_none() {
                        container.warnings.push("override_end_overflow".into());
                        return Ok(None);
                    }
                    source = c;
                    sequence = *s;
                    changed = *m;
                    if r == &key {
                        d.start = override_doc.start;
                        d.end = override_doc.end;
                    }
                }
                Ok(Some(Occurrence {
                    uid: uid.clone(),
                    recurrence_id: key,
                    doc: d,
                    sequence,
                    modified: changed,
                    content_hash: hashes
                        .entry(source as *const Component as usize)
                        .or_insert_with(|| content_hash(source))
                        .clone(),
                    volatile: *volatile,
                }))
            })();
            match occurrence {
                Ok(Some(row)) => {
                    retain_occurrence(
                        &mut result,
                        row,
                        &mut expanded_bytes,
                        cfg.max_event_document_bytes,
                    )?;
                }
                Ok(None) => (),
                Err(_) => container.warnings.push("invalid_vevent_occurrence".into()),
            }
        }
    }
    // Orphans and moved overrides whose original instance is outside the window remain addressable.
    for ((uid, rid), (c, doc, _, seq, modified, volatile)) in &events {
        if !rid.is_empty() && doc.start.is_some_and(|s| s >= lo && s <= hi) {
            ensure!(std::time::Instant::now() < deadline, "override work budget");
            ensure!(
                result.len() < cfg.max_occurrences_per_feed
                    || result.contains_key(&(uid.clone(), rid.clone())),
                "calendar occurrence cap"
            );
            if !result.contains_key(&(uid.clone(), rid.clone())) {
                let row = Occurrence {
                    uid: uid.clone(),
                    recurrence_id: rid.clone(),
                    doc: doc.clone(),
                    sequence: *seq,
                    modified: *modified,
                    content_hash: hashes
                        .entry(*c as *const Component as usize)
                        .or_insert_with(|| content_hash(c))
                        .clone(),
                    volatile: *volatile,
                };
                retain_occurrence(
                    &mut result,
                    row,
                    &mut expanded_bytes,
                    cfg.max_event_document_bytes,
                )?;
            }
        }
    }
    let mut counts = BTreeMap::new();
    let mut rows: Vec<_> = result.into_values().collect();
    rows.sort_by_key(|o| o.doc.start);
    for mut row in rows {
        let count = counts.entry(row.uid.clone()).or_insert(0);
        if *count >= cfg.max_occurrences_per_series {
            container.warnings.push("series_cap".into());
            continue;
        }
        *count += 1;
        ensure!(
            container.occurrences.len() < cfg.max_occurrences_per_feed,
            "calendar occurrence cap"
        );
        row.doc = match row.doc.bounded(&[]) {
            Ok(doc) => doc,
            Err(_) => {
                container.warnings.push("invalid_vevent_occurrence".into());
                continue;
            }
        };
        container.occurrences.push(row);
    }
    container.warnings.sort();
    container.warnings.dedup();
    let mut doc = ResourceMeta::new(uri, "event", 86400);
    doc.set_type("DataFeed");
    doc.name = cal.text("NAME").or_else(|| cal.text("X-WR-CALNAME"));
    doc.description = cal.text("DESCRIPTION").or_else(|| cal.text("X-WR-CALDESC"));
    doc.event.warnings = container.warnings.clone();
    doc.container = Some(Box::new(container));
    doc.bounded(&[])
}

// Account replacements as well as new keys; a small revision map can otherwise amplify
// into tens of MB when every recurrence clones a long description.
fn retain_occurrence(
    rows: &mut BTreeMap<(String, String), Occurrence>,
    row: Occurrence,
    retained: &mut usize,
    cap: usize,
) -> Result<()> {
    let key = (row.uid.clone(), row.recurrence_id.clone());
    let old = rows
        .get(&key)
        .map(|o| serde_json::to_vec(&o.doc).map(|v| v.len()))
        .transpose()?
        .unwrap_or(0);
    *retained = retained
        .saturating_sub(old)
        .saturating_add(serde_json::to_vec(&row.doc)?.len());
    ensure!(*retained <= cap, "expanded event document memory cap");
    rows.insert(key, row);
    Ok(())
}

/// RSS/Atom event extensions and Gancio's JSON Feed are containers with the same lifecycle.
pub fn structured_feed(
    uri: &str,
    bytes: &[u8],
    cfg: &EventConfig,
    now: DateTime<Utc>,
) -> Result<Option<ResourceMeta>> {
    ensure!(bytes.len() <= cfg.feed_body_bytes, "feed body cap");
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(cfg.parse_timeout_ms);
    let check_budget = || -> Result<()> {
        ensure!(
            std::time::Instant::now() < deadline,
            "structured feed parse budget"
        );
        Ok(())
    };
    let name;
    let mut items = 0usize;
    let mut dated = 0usize;
    let mut retained_bytes = 0usize;
    let base = Url::parse(uri)?;
    let lo = now
        .checked_sub_months(Months::new(cfg.months_back))
        .context("horizon")?;
    let hi = now
        .checked_add_months(Months::new(cfg.months_forward))
        .context("horizon")?;
    let mut container = Container {
        limits: cfg.container.clone(),
        horizon_start: lo,
        horizon_end: hi,
        held_retry_secs: cfg.held_retry_secs,
        ..Default::default()
    };
    let mut append = |uid: Option<String>,
                      title: Option<String>,
                      url: Option<String>,
                      start: Value,
                      end: Option<Value>,
                      place: Option<Value>,
                      description: Option<String>|
     -> Result<()> {
        check_budget()?;
        dated += 1;
        let parse = |v: &Value| {
            v.as_i64()
                .and_then(|n| DateTime::from_timestamp(n, 0))
                .or_else(|| {
                    v.as_str()
                        .and_then(|s| time::parse(s, cfg.default_timezone.as_deref()))
                })
        };
        let Some(start) = parse(&start) else {
            dated -= 1;
            container.warnings.push("invalid_feed_item_date".into());
            return Ok(());
        };
        if start < lo || start > hi {
            return Ok(());
        }
        let mut d = ResourceMeta::new(uri, "event_feed", 86400);
        d.set_type("Event");
        d.start = Some(start);
        d.end = end.as_ref().and_then(parse);
        d.name = title;
        d.description = description;
        if let Some(url) = url.and_then(|s| origin_url(&base, &s)) {
            d.url = url;
        }
        if let Some(place) = place {
            d.location = page::point(&place).or_else(|| {
                page::point(&serde_json::json!({"latitude":place["lat"],"longitude":place["lon"]}))
            });
            d.event.venue = Some(Venue {
                text: page::string(&place["name"]).unwrap_or_default(),
                location: d.location.clone(),
                location_source: Some(LocationSource::Page),
                ..Default::default()
            });
        }
        page::finalize(&mut d, cfg.coordinate_timezone);
        let volatile = uid.is_none();
        let uid = uid.unwrap_or_else(|| {
            format!(
                "volatile:{}",
                hash(format!("{}\n{start}\n{:?}", canonical_feed(uri), d.name).as_bytes())
            )
        });
        ensure!(uid.len() <= 1024, "UID cap");
        d = d.bounded(&[])?;
        let content_hash = hash(serde_json::to_string(&crate::changes::normalize(&d))?.as_bytes());
        ensure!(
            container.occurrences.len() < cfg.max_occurrences_per_feed,
            "feed occurrence cap"
        );
        retained_bytes = retained_bytes.saturating_add(serde_json::to_vec(&d)?.len());
        ensure!(
            retained_bytes <= cfg.max_event_document_bytes,
            "feed document memory cap"
        );
        container.occurrences.push(Occurrence {
            uid,
            recurrence_id: String::new(),
            doc: d,
            sequence: 0,
            modified: None,
            content_hash,
            volatile,
        });
        Ok(())
    };
    // JSON decoding materialises one DOM before deadline/item checks. Its input is
    // bounded by feed_body_bytes (32 MB by default); retained occurrence docs have a
    // separate byte cap above. Parsing runs on the blocking pool.
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        if !v["items"].is_array() {
            return Ok(None);
        }
        name = page::string(&v["title"]);
        for v in v["items"].as_array().unwrap() {
            check_budget()?;
            items += 1;
            ensure!(items <= cfg.max_components, "structured feed item cap");
            let event = v.get("_gancio").or_else(|| v.get("event")).unwrap_or(v);
            let start = event
                .get("start_datetime")
                .or_else(|| event.get("start"))
                .or_else(|| event.get("startTime"))
                .filter(|v| !v.is_null() && v.as_str().is_none_or(|s| !s.trim().is_empty()));
            if let Some(start) = start {
                append(
                    page::string(&v["id"]),
                    page::string(&v["title"]).or_else(|| page::string(&event["title"])),
                    page::string(&v["url"]),
                    start.clone(),
                    event
                        .get("end_datetime")
                        .or_else(|| event.get("end"))
                        .cloned(),
                    event.get("place").cloned(),
                    page::string(&v["content_text"]),
                )?;
            }
        }
    } else {
        let text = std::str::from_utf8(bytes)?;
        let options = roxmltree::ParsingOptions {
            nodes_limit: cfg.max_components.saturating_mul(16).min(u32::MAX as usize) as u32,
            ..Default::default()
        };
        let Ok(xml) = roxmltree::Document::parse_with_options(text, options) else {
            return Ok(None);
        };
        if !matches!(xml.root_element().tag_name().name(), "rss" | "feed" | "RDF") {
            return Ok(None);
        }
        name = xml
            .root_element()
            .children()
            .find(|n| n.tag_name().name() == "title")
            .and_then(|n| n.text())
            .map(str::to_owned);
        for node in xml
            .descendants()
            .filter(|n| matches!(n.tag_name().name(), "entry" | "item"))
        {
            check_budget()?;
            items += 1;
            ensure!(items <= cfg.max_components, "structured feed item cap");
            let text = |names: &[&str]| {
                node.descendants()
                    .find(|n| names.contains(&n.tag_name().name()))
                    .and_then(|n| n.text())
                    .map(str::to_owned)
            };
            let start = text(&["dtstart", "startdate", "startDate", "start", "dateStart"]);
            let start = start.or_else(|| {
                node.descendants()
                    .find(|n| matches!(n.tag_name().name(), "date" | "dates"))
                    .and_then(|n| {
                        n.attribute("start")
                            .or_else(|| n.attribute("startdate"))
                            .or_else(|| n.attribute("fromdate"))
                    })
                    .map(str::to_owned)
            });
            if let Some(start) = start {
                let url = node
                    .children()
                    .find(|n| n.tag_name().name() == "link")
                    .and_then(|n| n.attribute("href").or_else(|| n.text()))
                    .map(str::to_owned);
                let location = node
                    .descendants()
                    .find(|n| n.tag_name().name() == "location");
                let place=location.map(|n|serde_json::json!({"name":n.attribute("name").or_else(|| n.descendants().find(|n|n.tag_name().name()=="name").and_then(|n|n.text())),"latitude":n.descendants().find(|n|matches!(n.tag_name().name(),"latitude"|"lat")).and_then(|n|n.text()),"longitude":n.descendants().find(|n|matches!(n.tag_name().name(),"longitude"|"long"|"lon")).and_then(|n|n.text())}));
                append(
                    text(&["id", "guid"]),
                    text(&["title"]),
                    url,
                    Value::String(start),
                    text(&["dtend", "enddate", "endDate", "end", "dateEnd"])
                        .or_else(|| {
                            node.descendants()
                                .find(|n| n.tag_name().name() == "dates")
                                .and_then(|n| n.attribute("todate"))
                                .map(str::to_owned)
                        })
                        .map(Value::String),
                    place,
                    text(&["description", "summary"]),
                )?;
            }
        }
    }
    // Without usable dates this is not a new event feed. A previously accepted
    // feed is handled by disappeared(), with empty-snapshot confirmation.
    check_budget()?;
    if dated == 0 && items > 0 {
        return Ok(None);
    }
    if dated < items {
        container.warnings.push("undated_feed_item".into());
    }
    if items == 0 && !super::service::is_feed(uri) {
        return Ok(None);
    }
    let mut d = ResourceMeta::new(uri, "event", 86400);
    d.set_type("DataFeed");
    d.name = name;
    d.event.warnings = container.warnings.clone();
    d.container = Some(Box::new(container));
    Ok(Some(d.bounded(&[])?))
}
