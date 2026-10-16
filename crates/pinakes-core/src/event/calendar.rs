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
use std::{collections::BTreeMap, io::BufRead};
use url::Url;

#[derive(Clone, Debug, Default)]
struct Property {
    name: String,
    params: BTreeMap<String, String>,
    value: String,
}
#[derive(Clone, Debug, Default)]
struct Component {
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
fn components(bytes: &[u8], cap: usize) -> Result<Component> {
    let mut stack: Vec<Component> = vec![];
    let mut root = None;
    let mut pending = String::new();
    let mut count = 0usize;
    let mut consume = |line: &str| -> Result<()> {
        if line.trim().is_empty() {
            return Ok(());
        }
        let p = property(line)?;
        if p.name == "BEGIN" {
            ensure!(stack.len() < 8, "calendar nesting cap");
            count += 1;
            ensure!(count <= cap, "calendar component cap");
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
    // BufRead processes physical lines; unfold before parameter parsing and TEXT unescaping.
    for line in std::io::Cursor::new(bytes).lines() {
        let line = line?;
        let line = line.trim_end_matches('\r');
        if line.starts_with([' ', '\t']) {
            ensure!(!pending.is_empty(), "orphan folded line");
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
    unknown: bool,
}
struct Zones<'a> {
    calendar: Option<&'a str>,
    default: Option<&'a str>,
    embedded: BTreeMap<String, &'a Component>,
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
                    let wall = at.with_timezone(&offset).naive_local();
                    if self.at_wall(stamp, wall).is_ok_and(|v| v == at) {
                        return Ok(wall);
                    }
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
        let at = if zulu {
            wall.and_utc()
        } else if let Some(z) = zone {
            time::local(z, wall).context("local date outside zone")?
        } else if let Some(z) = custom {
            custom_time(z, wall)?
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
            unknown: !zulu && name.is_some() && zone.is_none() && custom.is_none(),
        })
    }
    fn at_wall(&self, stamp: &Stamp, wall: NaiveDateTime) -> Result<DateTime<Utc>> {
        if let Some(zone) = stamp.zone.as_deref() {
            if let Some(zone) = time::zone(zone) {
                return time::local(zone, wall).context("invalid wall time");
            }
            if let Some(c) = self.embedded.get(zone) {
                return custom_time(c, wall);
            }
        }
        Ok(wall.and_utc())
    }
}
fn custom_time(c: &Component, wall: NaiveDateTime) -> Result<DateTime<Utc>> {
    let mut transitions = vec![];
    for o in &c.children {
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
            let end = (wall + Duration::days(370))
                .and_utc()
                .with_timezone(&rrule::Tz::UTC);
            dates.extend(
                set.before(end)
                    .all(1000)
                    .dates
                    .into_iter()
                    .map(|d| d.naive_utc()),
            );
        }
        for p in o.props.iter().filter(|p| p.name == "RDATE") {
            dates.extend(p.value.split(',').filter_map(time::naive));
        }
        for d in dates {
            transitions.push((d, from, to));
        }
    }
    transitions.sort_by_key(|t| t.0);
    let mut chosen = transitions
        .first()
        .map(|t| t.1)
        .context("empty VTIMEZONE")?;
    for (at, from, to) in transitions {
        let delta = to.local_minus_utc() - from.local_minus_utc();
        if delta > 0 && wall >= at && wall < at + Duration::seconds(delta as i64) {
            chosen = from;
            break;
        }
        if delta < 0 && wall >= at + Duration::seconds(delta as i64) && wall < at {
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
    s.split_whitespace()
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
    doc.description = c.text("DESCRIPTION").or_else(|| c.text("X-ALT-DESC"));
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
        Some(zones.at_wall(&start, start.wall + Duration::days(1))?)
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
        doc.event.organizer = name.map(|name| Organizer {
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
        || doc.description.as_deref().is_some_and(conference);
    doc.event.attendance = Some(if online && coord.is_some() {
        Attendance::Mixed
    } else if online {
        Attendance::Online
    } else {
        Attendance::Offline
    });
    if doc.event.attendance != Some(Attendance::Online) {
        if location.is_some() || coord.is_some() {
            doc.event.venue = Some(Venue {
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
    Ok((doc, start))
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
    let mut wall_start = start.wall;
    // Google uses UTC DTSTART with a local recurring rule and X-WR-TIMEZONE.
    if c.value("DTSTART").is_some_and(|s| s.ends_with('Z')) {
        if let Some(z) = start.zone.as_deref().and_then(time::zone) {
            wall_start = start.at.with_timezone(&z).naive_local();
        }
    }
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
                let mut until = zones.stamp(&prop, v)?;
                if v.len() == 8 {
                    until.wall += Duration::days(1) - Duration::seconds(1);
                }
                let wall = if v.ends_with('Z') {
                    if let Some(z) = start.zone.as_deref().and_then(time::zone) {
                        until.at.with_timezone(&z).naive_local()
                    } else {
                        until.wall
                    }
                } else {
                    until.wall
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
        let excluded = c
            .props
            .iter()
            .filter(|p| p.name == "EXDATE")
            .map(|p| p.value.split(',').count())
            .sum::<usize>();
        let take = cfg
            .max_occurrences_per_series
            .saturating_add(excluded)
            .saturating_add(10);
        let mut included = 0;
        for (steps, d) in set.limit().into_iter().enumerate() {
            ensure!(
                steps < cfg.max_recurrence_steps && std::time::Instant::now() < deadline,
                "recurrence work budget"
            );
            let at = zones.at_wall(start, d.naive_utc())?;
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
    for p in c.props.iter().filter(|p| p.name == "RDATE") {
        for v in p.value.split(',') {
            let (s, end) = v.split_once('/').map_or((v, None), |(s, e)| (s, Some(e)));
            let at = zones.stamp(p, s)?.at;
            let duration = end
                .map(|e| {
                    if e.starts_with(['P', '+', '-']) {
                        time::duration(e).context("RDATE duration")
                    } else {
                        Ok(zones.stamp(p, e)?.at - at)
                    }
                })
                .transpose()?;
            out.insert(at, duration);
        }
    }
    for p in c.props.iter().filter(|p| p.name == "EXDATE") {
        for v in p.value.split(',') {
            let s = zones.stamp(p, v)?;
            if s.date {
                out.retain(|at, _| {
                    zones
                        .wall_at(start, *at)
                        .is_ok_and(|wall| wall.date() != s.wall.date())
                });
            } else {
                out.remove(&s.at);
            }
        }
    }
    out.retain(|at, _| *at >= lo && *at <= hi);
    Ok(out)
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
    let cal = components(bytes, cfg.max_components)?;
    let canonical = canonical_feed(uri);
    let base = Url::parse(&canonical)?;
    let zones = Zones {
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
        horizon_start: lo,
        horizon_end: hi,
        held_retry_secs: cfg.held_retry_secs,
        feed_ttl: cal
            .value("REFRESH-INTERVAL")
            .or_else(|| cal.value("X-PUBLISHED-TTL"))
            .and_then(time::duration)
            .map(|d| d.num_seconds().clamp(3600, 31536000) as u64),
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
    let mut events: BTreeMap<(String, String), EventRevision<'_>> = BTreeMap::new();
    for c in cal.children.iter().filter(|c| c.name == "VEVENT") {
        ensure!(
            std::time::Instant::now() < deadline,
            "calendar parse budget"
        );
        let (mut doc, mut start) = document(c, &cal, &base, &zones)?;
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
                let (dated, stamp) = document(&derived, &cal, &base, &zones)?;
                doc.start = dated.start;
                doc.end = dated.end;
                doc.event.start_local = None;
                start = stamp;
                start.zone = Some(tz.clone());
            }
        }
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
        ensure!(uid.len() <= 1024, "UID exceeds cap");
        let rid = c
            .get("RECURRENCE-ID")
            .map(|p| zones.stamp(p, &p.value).map(|s| recurrence_key(&s)))
            .transpose()?
            .unwrap_or_default();
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
        events.insert(key, (c, doc, start, sequence, modified, volatile));
    }
    let mut result: BTreeMap<(String, String), Occurrence> = BTreeMap::new();
    for ((uid, rid), (c, doc, start, seq, modified, volatile)) in &events {
        if !rid.is_empty() {
            continue;
        }
        let recurring = c.get("RRULE").is_some() || c.get("RDATE").is_some();
        ensure!(
            std::time::Instant::now() < deadline,
            "calendar parse budget"
        );
        let expanded = expand(c, start, &zones, lo, hi, cfg, deadline)?;
        for (at, duration) in expanded {
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
            d.end = Some(at + duration.unwrap_or_else(|| doc.end.unwrap_or(start.at) - start.at));
            if start.date && duration.is_none() {
                let days = if let Some(end) = c.get("DTEND") {
                    zones.stamp(end, &end.value)?.wall - start.wall
                } else {
                    c.value("DURATION")
                        .and_then(time::duration)
                        .unwrap_or(Duration::days(1))
                };
                let wall = zones.wall_at(start, at)?;
                d.end = Some(zones.at_wall(start, wall + days)?);
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
                let original = zones.stamp(original, &original.value)?;
                d = override_doc.clone();
                d.start = Some(at + (override_start.at - original.at));
                d.end = Some(
                    d.start.unwrap()
                        + (override_doc.end.unwrap_or(override_start.at) - override_start.at),
                );
                source = c;
                sequence = *s;
                changed = *m;
                if r == &key {
                    d.start = override_doc.start;
                    d.end = override_doc.end;
                }
            }
            result.insert(
                (uid.clone(), key.clone()),
                Occurrence {
                    uid: uid.clone(),
                    recurrence_id: key,
                    doc: d,
                    sequence,
                    modified: changed,
                    content_hash: content_hash(source),
                    volatile: *volatile,
                },
            );
        }
    }
    // Orphans and moved overrides whose original instance is outside the window remain addressable.
    for ((uid, rid), (c, doc, _, seq, modified, volatile)) in &events {
        if !rid.is_empty() && doc.start.is_some_and(|s| s >= lo && s <= hi) {
            result
                .entry((uid.clone(), rid.clone()))
                .or_insert_with(|| Occurrence {
                    uid: uid.clone(),
                    recurrence_id: rid.clone(),
                    doc: doc.clone(),
                    sequence: *seq,
                    modified: *modified,
                    content_hash: content_hash(c),
                    volatile: *volatile,
                });
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
        row.doc = row.doc.bounded(&[])?;
        container.occurrences.push(row);
    }
    container.warnings.sort();
    container.warnings.dedup();
    let mut doc = ResourceMeta::new(uri, "calendar", 86400);
    doc.set_type("DataFeed");
    doc.name = cal.text("NAME").or_else(|| cal.text("X-WR-CALNAME"));
    doc.description = cal.text("DESCRIPTION").or_else(|| cal.text("X-WR-CALDESC"));
    doc.event.warnings = container.warnings.clone();
    doc.container = Some(Box::new(container));
    doc.bounded(&[])
}

/// RSS/Atom event extensions and Gancio's JSON Feed are containers with the same lifecycle.
pub fn structured_feed(
    uri: &str,
    bytes: &[u8],
    cfg: &EventConfig,
    now: DateTime<Utc>,
) -> Result<Option<ResourceMeta>> {
    let mut events = vec![];
    let mut name = None;
    let mut items = 0usize;
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        if !v["items"].is_array() {
            return Ok(None);
        }
        name = page::string(&v["title"]);
        for v in v["items"].as_array().unwrap() {
            items += 1;
            let event = v.get("_gancio").or_else(|| v.get("event")).unwrap_or(v);
            let start = event
                .get("start_datetime")
                .or_else(|| event.get("start"))
                .or_else(|| event.get("startTime"));
            if let Some(start) = start {
                events.push((
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
                ));
            }
        }
    } else {
        let text = std::str::from_utf8(bytes)?;
        let Ok(xml) = roxmltree::Document::parse(text) else {
            return Ok(None);
        };
        if !matches!(xml.root_element().tag_name().name(), "rss" | "feed" | "RDF") {
            return Ok(None);
        }
        for node in xml
            .descendants()
            .filter(|n| n.has_tag_name("entry") || n.has_tag_name("item"))
        {
            items += 1;
            let text = |names: &[&str]| {
                node.descendants()
                    .find(|n| names.contains(&n.tag_name().name()))
                    .and_then(|n| n.text())
                    .map(str::to_owned)
            };
            let start = text(&["dtstart", "startdate", "startDate", "start", "dateStart"]);
            let start = start.or_else(|| {
                node.descendants()
                    .find(|n| n.tag_name().name() == "date")
                    .and_then(|n| n.attribute("start").or_else(|| n.attribute("startdate")))
                    .map(str::to_owned)
            });
            if let Some(start) = start {
                let url = node
                    .children()
                    .find(|n| n.has_tag_name("link"))
                    .and_then(|n| n.attribute("href").or_else(|| n.text()))
                    .map(str::to_owned);
                let location = node
                    .descendants()
                    .find(|n| n.tag_name().name() == "location");
                let place=location.map(|n|serde_json::json!({"name":n.descendants().find(|n|n.has_tag_name("name")).and_then(|n|n.text()),"latitude":n.descendants().find(|n|matches!(n.tag_name().name(),"latitude"|"lat")).and_then(|n|n.text()),"longitude":n.descendants().find(|n|matches!(n.tag_name().name(),"longitude"|"long"|"lon")).and_then(|n|n.text())}));
                events.push((
                    text(&["id", "guid"]),
                    text(&["title"]),
                    url,
                    Value::String(start),
                    text(&["dtend", "enddate", "endDate", "end", "dateEnd"]).map(Value::String),
                    place,
                    text(&["description", "summary"]),
                ));
            }
        }
    }
    // An ordinary RSS/JSON feed is not an authoritative empty event snapshot.
    if events.is_empty() && items > 0 {
        return Ok(None);
    }
    ensure!(
        events.len() == items,
        "feed contains undated entries; refusing partial snapshot"
    );
    if items == 0 && !super::service::is_feed(uri) {
        return Ok(None);
    }
    let base = Url::parse(uri)?;
    let lo = now
        .checked_sub_months(Months::new(cfg.months_back))
        .context("horizon")?;
    let hi = now
        .checked_add_months(Months::new(cfg.months_forward))
        .context("horizon")?;
    let mut container = Container {
        horizon_start: lo,
        horizon_end: hi,
        held_retry_secs: cfg.held_retry_secs,
        ..Default::default()
    };
    for (uid, title, url, start, end, place, description) in events {
        let parse = |v: &Value| {
            v.as_i64()
                .and_then(|n| DateTime::from_timestamp(n, 0))
                .or_else(|| {
                    v.as_str()
                        .and_then(|s| time::parse(s, cfg.default_timezone.as_deref()))
                })
        };
        let Some(start) = parse(&start) else {
            bail!("invalid feed event date")
        };
        if start < lo || start > hi {
            continue;
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
        container.occurrences.push(Occurrence {
            uid,
            recurrence_id: String::new(),
            doc: d,
            sequence: 0,
            modified: None,
            content_hash,
            volatile,
        });
    }
    let mut d = ResourceMeta::new(uri, "calendar", 86400);
    d.set_type("DataFeed");
    d.name = name;
    d.container = Some(Box::new(container));
    Ok(Some(d.bounded(&[])?))
}
