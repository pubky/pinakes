//! The Nexus fork's app-post feed (`GET /v0/app-posts/changes`, `[nexus] app_posts`): posts apps
//! write on the social post envelope under their own namespace, and the social specs' events
//! and rsvps. Each line names a version file on its author's homeserver; Pinakes fetches it,
//! checks it with pubky-social-specs' `PostEnvelope<K>` (the pin Nexus and the music node use),
//! reads it by its namespace and kind (`profile`), and keeps what it says as references
//! (`reference.feed = 'app_posts'`) and, for a route or an availability, the post's own card.
//!
//! What each kind becomes:
//! - `social` `event`: one event reference per location and occurrence (its local start in its
//!   zone, its duration, its status, its series expanded under the spec's caps), as a tagged
//!   event page's are, at the location's resource (the post itself when it names none). An
//!   override of one occurrence (`recurrenceOf`, `recurrenceId`) replaces that occurrence.
//! - `social` `rsvp`: a reply on the page it answers, with its status as the label; an answer to
//!   a Pubky object is Nexus's and is not kept.
//! - `maps` `route`: the post's card (a `Trip`: its name, description, first waypoint and the
//!   box of its coordinate waypoints, and its measures as facts) and an embed on each waypoint.
//! - `events` `availability`: the post's card (a `Schedule`: its zone, horizon and blocks).
//! - `music` `listen`: a play (`listen`) on the track it embeds, at the line's time.
//!
//! Every other namespace and kind is skipped and counted (`unprofiled`). A page moves the cursor
//! past every line it held, read or skipped, under the resource feed's cursor and epoch rules.
use crate::{
    model::ResourceMeta,
    nexus::{AppPostLine, AppPostPage, Nexus, Op},
    store,
    telemetry::Metrics,
};
use anyhow::{bail, Result};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use pubky_social_specs::{traits::Validatable, PostEnvelope, PostKind, ValidationCtx, PUB_CTX};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

/// What became of a feed line, as `pinakes_app_posts_total{outcome}` counts it.
pub const OUTCOMES: &[&str] = &[
    "indexed",
    "deleted",
    "malformed",
    "unknown_kind",
    "author",
    "path",
    "unprofiled",
    "foreign",
    "gone",
    "invalid",
    "unreachable",
];
/// The namespaces and kinds this build reads.
pub const PROFILED: &[(&str, &str)] = &[
    ("social", "event"),
    ("social", "rsvp"),
    ("maps", "route"),
    ("events", "availability"),
    ("music", "listen"),
];
/// The outcome of a post its profile cannot read.
const INVALID: &str = "invalid";
/// Lines per page: each names a file to fetch.
const PAGE: i64 = 100;
/// Polls a page may fail on an unreachable homeserver before its failing lines are skipped
/// (`unreachable`), so one homeserver never stops the feed.
pub const MAX_ATTEMPTS: u32 = 5;
/// The references one post may make (occurrences times locations).
const MAX_ROWS: usize = 1000;
/// The occurrences a series is expanded to, and how far (the spec's generator caps).
const MAX_OCCURRENCES: usize = 1000;
const MAX_SPAN_DAYS: i64 = 3 * 366;

/// The app-post feed's row in `feed_cursor`, beside the resource feed's.
pub fn cursor_key(nexus_url: &str) -> String {
    format!("{nexus_url}#app-posts")
}

/// How a version file is fetched: the Pubky resolver's fetch (`PubkyResolver::fetch_object`),
/// or a test's.
#[async_trait::async_trait]
pub trait Versions: Send + Sync {
    async fn fetch(&self, object: &Url) -> Result<crate::resolver::egress::Fetched>;
}
#[async_trait::async_trait]
impl Versions for crate::resolver::pubky::PubkyResolver {
    async fn fetch(&self, object: &Url) -> Result<crate::resolver::egress::Fetched> {
        self.fetch_object(object).await
    }
}

/// Any kind token, for the envelope's checks: the kind's content is the profile's to read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppKind(pub String);
impl PostKind for AppKind {
    fn is_known(&self) -> bool {
        kind_token(&self.0)
    }
    fn validate_content(&self, _: &PostEnvelope<Self>, _: &ValidationCtx) -> Result<(), String> {
        Ok(())
    }
}
/// A short lowercase kind or namespace token.
fn kind_token(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// One reference a post makes.
#[derive(Clone, Debug, PartialEq)]
pub struct Ref {
    pub uri: String,
    pub kind: &'static str,
    pub label: Option<String>,
    pub recurrence_id: Option<String>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub status: Option<&'static str>,
}
impl Ref {
    fn plain(uri: &str, kind: &'static str) -> Self {
        Self {
            uri: uri.into(),
            kind,
            label: None,
            recurrence_id: None,
            start: None,
            end: None,
            status: None,
        }
    }
}
/// What one post makes: its references and, for some kinds, its own card. An override of an
/// event's occurrence names the master and the occurrences it replaces.
#[derive(Debug, Default)]
pub struct Projection {
    pub refs: Vec<Ref>,
    pub card: Option<ResourceMeta>,
    pub replaces: Option<(String, Vec<String>)>,
}

/// A line's post, author and namespace checked: (post id, versionless object path).
fn checked_paths(line: &AppPostLine) -> Result<(String, String), &'static str> {
    if !crate::nexus::valid_author(&line.author) {
        return Err("author");
    }
    if !kind_token(&line.namespace) || !kind_token(&line.post_kind) {
        return Err("path");
    }
    let prefix = format!("/pub/{}/v1/posts/", line.namespace);
    let id = line.post.strip_prefix(&prefix).ok_or("path")?;
    let edit = line
        .version
        .strip_prefix(&format!("{}/", line.post))
        .and_then(|v| v.strip_suffix(".json"))
        .ok_or("path")?;
    let token =
        |s: &str| (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric());
    if !token(id) || !token(edit) {
        return Err("path");
    }
    Ok((id.into(), line.post.clone()))
}

/// What one line makes, or why it is skipped. `Err` is a failure to reach the homeserver,
/// which the page retries (`MAX_ATTEMPTS`).
pub async fn process(
    line: &AppPostLine,
    versions: &dyn Versions,
) -> Result<std::result::Result<Projection, &'static str>> {
    let (id, post) = match checked_paths(line) {
        Ok(paths) => paths,
        Err(reason) => return Ok(Err(reason)),
    };
    if !PROFILED.contains(&(line.namespace.as_str(), line.post_kind.as_str())) {
        return Ok(Err("unprofiled"));
    }
    if matches!(line.op, Op::Del) {
        return Ok(Ok(Projection::default()));
    }
    let object = Url::parse(&format!("pubky://{}{}", line.author, line.version))?;
    let fetched = versions.fetch(&object).await?;
    match fetched.status {
        200..=299 => {}
        401 | 403 | 404 | 410 => return Ok(Err("gone")),
        status => bail!("a homeserver answered {status} to an app post"),
    }
    let Ok(envelope) =
        <PostEnvelope<AppKind> as Validatable>::try_from(&fetched.body, &id, &PUB_CTX)
    else {
        return Ok(Err("invalid"));
    };
    if envelope.kind.0 != line.post_kind {
        return Ok(Err("invalid"));
    }
    let post_uri = format!("pubky://{}{post}", line.author);
    Ok(profile(line, &post_uri, &envelope))
}

/// A post read by its namespace and kind.
pub fn profile(
    line: &AppPostLine,
    post_uri: &str,
    envelope: &PostEnvelope<AppKind>,
) -> std::result::Result<Projection, &'static str> {
    let content: Value = serde_json::from_str(&envelope.content).map_err(|_| INVALID)?;
    match (line.namespace.as_str(), line.post_kind.as_str()) {
        ("social", "event") => event(&line.author, post_uri, &content),
        ("social", "rsvp") => rsvp(envelope, &content),
        ("maps", "route") => route(post_uri, &content),
        ("events", "availability") => availability(post_uri, &content),
        ("music", "listen") => listen(envelope),
        _ => Err("unprofiled"),
    }
}

/// A local date-time (`YYYY-MM-DDTHH:MM:SS`) in a zone, as an instant.
fn local(value: &str, zone: chrono_tz::Tz) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").ok()?;
    crate::event::time::local(zone, naive)
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
/// The occurrences of an event: its local start in its zone and, with a rule, the series the
/// rule gives from it (at most `MAX_OCCURRENCES`, at most `MAX_SPAN_DAYS` on), less its
/// excluded dates. Each as (local start, instant).
fn occurrences(content: &Value, zone: chrono_tz::Tz) -> Option<Vec<(String, DateTime<Utc>)>> {
    let start = content["start"].as_str()?;
    let first = local(start, zone)?;
    let rules = content
        .get("recurrenceRules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if rules.is_empty() {
        return Some(vec![(start.into(), first)]);
    }
    if rules.len() > 1 {
        return None;
    }
    let excluded: Vec<DateTime<Utc>> = content
        .get("excludedDates")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(|d| local(d.as_str()?, zone)).collect())
        .unwrap_or_default();
    let mut lines = vec![
        format!(
            "DTSTART;TZID={}:{}",
            zone.name(),
            first.with_timezone(&zone).format("%Y%m%dT%H%M%S")
        ),
        format!("RRULE:{}", rrule(&rules[0], zone)?),
    ];
    if !excluded.is_empty() {
        let values: Vec<String> = excluded
            .iter()
            .map(|at| at.format("%Y%m%dT%H%M%SZ").to_string())
            .collect();
        lines.push(format!("EXDATE:{}", values.join(",")));
    }
    let set = lines.join("\n").parse::<rrule::RRuleSet>().ok()?;
    if !crate::event::time::rrule_bounded(&set, MAX_OCCURRENCES) {
        return None;
    }
    let horizon = first + Duration::days(MAX_SPAN_DAYS);
    Some(
        set.all(MAX_OCCURRENCES as u16)
            .dates
            .into_iter()
            .map(|at| at.with_timezone(&Utc))
            .take_while(|at| *at <= horizon)
            .map(|at| {
                (
                    at.with_timezone(&zone)
                        .format("%Y-%m-%dT%H:%M:%S")
                        .to_string(),
                    at,
                )
            })
            .collect(),
    )
}
/// A social event: its occurrences at each location, or at the post itself.
fn event(
    author: &str,
    post_uri: &str,
    content: &Value,
) -> std::result::Result<Projection, &'static str> {
    let zone = content["timeZone"]
        .as_str()
        .and_then(crate::event::time::zone)
        .ok_or(INVALID)?;
    let all_day = content["showWithoutTime"].as_bool().unwrap_or(false);
    let length = match content.get("duration") {
        Some(d) => Some(crate::event::time::duration(d.as_str().ok_or(INVALID)?).ok_or(INVALID)?),
        None if all_day => Some(Duration::days(1)),
        None => return Err(INVALID),
    };
    let status = match content.get("status").map(|s| s.as_str()) {
        None | Some(Some("confirmed" | "tentative")) => "scheduled",
        Some(Some("cancelled")) => "cancelled",
        _ => return Err(INVALID),
    };
    let locations: Vec<String> = match content.get("locations") {
        None => vec![],
        Some(list) => list
            .as_array()
            .ok_or(INVALID)?
            .iter()
            .map(|l| l["uri"].as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or(INVALID)?,
    };
    if locations.len() > 10 {
        return Err(INVALID);
    }
    let venues = if locations.is_empty() {
        vec![post_uri.to_owned()]
    } else {
        locations
    };
    let mut projection = Projection::default();
    // An override of one occurrence of a master event by the same author.
    let override_of = match (content.get("recurrenceOf"), content.get("recurrenceId")) {
        (Some(master), Some(rid)) => {
            let master = master.as_str().ok_or(INVALID)?;
            let prefix = format!("pubky://{author}/pub/social/v1/posts/");
            if !master.starts_with(&prefix) {
                return Err(INVALID);
            }
            let rid = rid.as_str().ok_or(INVALID)?;
            local(rid, zone).ok_or(INVALID)?;
            let path = master
                .trim_start_matches(&format!("pubky://{author}"))
                .to_owned();
            projection.replaces = Some((path.clone(), vec![rid.to_owned()]));
            Some((path, rid.to_owned()))
        }
        (None, None) => None,
        _ => return Err(INVALID),
    };
    let list = if override_of.is_some() {
        let start = content["start"].as_str().ok_or(INVALID)?;
        vec![(start.to_owned(), local(start, zone).ok_or(INVALID)?)]
    } else {
        occurrences(content, zone).ok_or(INVALID)?
    };
    let recurring = override_of.is_some()
        || content
            .get("recurrenceRules")
            .and_then(Value::as_array)
            .is_some_and(|r| !r.is_empty());
    for (local_start, at) in list {
        for venue in &venues {
            if projection.refs.len() >= MAX_ROWS {
                break;
            }
            let rid = match &override_of {
                Some((_, rid)) => Some(rid.clone()),
                None => recurring.then(|| local_start.clone()),
            };
            projection.refs.push(Ref {
                uri: venue.clone(),
                kind: "event",
                label: override_of.as_ref().map(|(master, _)| master.clone()),
                recurrence_id: rid,
                start: crate::event::time::bounded(at),
                end: length
                    .and_then(|l| at.checked_add_signed(l))
                    .and_then(crate::event::time::bounded),
                status: Some(status),
            });
        }
    }
    Ok(projection)
}
/// An rsvp: a reply on the page it answers, its status the label.
fn rsvp(
    envelope: &PostEnvelope<AppKind>,
    content: &Value,
) -> std::result::Result<Projection, &'static str> {
    let status = match content["status"].as_str() {
        Some(s @ ("requested" | "accepted" | "declined" | "tentative")) => s,
        _ => return Err(INVALID),
    };
    let parent = envelope.parent.as_deref().ok_or(INVALID)?;
    // An answer to a Pubky object (a social event post) is Nexus's to count.
    if parent.starts_with("pubky://") {
        return Err("foreign");
    }
    let mut reply = Ref::plain(parent, "reply");
    reply.label = Some(status.into());
    Ok(Projection {
        refs: vec![reply],
        ..Default::default()
    })
}
/// A point from a `geo:` waypoint.
fn geo(uri: &str) -> Option<crate::model::Point> {
    let (lat, lon) = uri.strip_prefix("geo:")?.split_once(',')?;
    let (lat, lon) = (lat.parse::<f64>().ok()?, lon.parse::<f64>().ok()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
        .then_some(crate::model::Point { lat, lon })
}
fn fact(doc: &mut ResourceMeta, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        doc.attributes.insert(key.into(), value);
    }
}
/// A text member, trimmed, within `max` code points.
fn text(content: &Value, key: &str, max: usize) -> Option<String> {
    content[key]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.chars().count() <= max)
        .map(str::to_owned)
}
/// A route: its card, and an embed on each waypoint.
fn route(post_uri: &str, content: &Value) -> std::result::Result<Projection, &'static str> {
    let name = text(content, "name", 100).ok_or(INVALID)?;
    let waypoints: Vec<String> = content["waypoints"]
        .as_array()
        .ok_or(INVALID)?
        .iter()
        .map(|w| w["uri"].as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or(INVALID)?;
    if !(2..=100).contains(&waypoints.len()) {
        return Err(INVALID);
    }
    let mut doc = ResourceMeta::new(post_uri, "app_post", 86400);
    doc.set_type("Trip");
    doc.name = Some(name);
    doc.description = text(content, "description", 2000);
    let points: Vec<crate::model::Point> = waypoints.iter().filter_map(|w| geo(w)).collect();
    doc.location = waypoints.first().and_then(|w| geo(w));
    if !points.is_empty() {
        let (mut w, mut s, mut e, mut n) = (180.0f64, 90.0f64, -180.0f64, -90.0f64);
        for p in &points {
            (w, s, e, n) = (w.min(p.lon), s.min(p.lat), e.max(p.lon), n.max(p.lat));
        }
        doc.bbox = Some([w, s, e, n]);
    }
    fact(&mut doc, "activity", text(content, "activity", 20));
    for key in [
        "distance_m",
        "duration_s",
        "elevation_gain_m",
        "elevation_loss_m",
    ] {
        fact(&mut doc, key, content[key].as_u64().map(|n| n.to_string()));
    }
    fact(&mut doc, "waypoints", Some(waypoints.len().to_string()));
    let mut refs = vec![Ref::plain(post_uri, "post")];
    let mut seen = BTreeSet::new();
    for waypoint in &waypoints {
        if seen.insert(waypoint.clone()) {
            refs.push(Ref::plain(waypoint, "embed"));
        }
    }
    Ok(Projection {
        refs,
        card: Some(doc),
        replaces: None,
    })
}
/// An availability: its card, a `Schedule` from its first available block.
fn availability(post_uri: &str, content: &Value) -> std::result::Result<Projection, &'static str> {
    let zone_name = content["timeZone"].as_str().ok_or(INVALID)?;
    let zone = crate::event::time::zone(zone_name).ok_or(INVALID)?;
    let horizon = content["horizonDays"]
        .as_u64()
        .filter(|d| (1..=366).contains(d))
        .ok_or(INVALID)?;
    let blocks = |key: &str| -> std::result::Result<Vec<Value>, &'static str> {
        match content.get(key) {
            None => Ok(vec![]),
            Some(list) => {
                let list = list.as_array().ok_or(INVALID)?;
                (list.len() <= 100).then(|| list.clone()).ok_or(INVALID)
            }
        }
    };
    let (available, busy) = (blocks("available")?, blocks("busy")?);
    let mut doc = ResourceMeta::new(post_uri, "app_post", 86400);
    doc.set_type("Schedule");
    let first = available.first();
    doc.name = first
        .and_then(|b| text(b, "title", 100))
        .or_else(|| Some("Availability".into()));
    doc.start = first.and_then(|b| local(b["start"].as_str()?, zone));
    doc.end = first.and_then(|b| {
        let length = crate::event::time::duration(b["duration"].as_str()?)?;
        doc.start?.checked_add_signed(length)
    });
    doc.event.tz = Some(zone_name.into());
    fact(&mut doc, "horizon_days", Some(horizon.to_string()));
    fact(
        &mut doc,
        "available_blocks",
        Some(available.len().to_string()),
    );
    fact(&mut doc, "busy_blocks", Some(busy.len().to_string()));
    fact(&mut doc, "for", content["for"].as_str().map(str::to_owned));
    Ok(Projection {
        refs: vec![Ref::plain(post_uri, "post")],
        card: Some(doc),
        replaces: None,
    })
}
/// A listen: a play on the track it embeds.
fn listen(envelope: &PostEnvelope<AppKind>) -> std::result::Result<Projection, &'static str> {
    let track = envelope.embed.as_deref().ok_or(INVALID)?;
    Ok(Projection {
        refs: vec![Ref::plain(track, "listen")],
        ..Default::default()
    })
}
/// The facts an app post's card shows (`api`'s `facts`).
pub fn facts(doc: &ResourceMeta) -> Option<BTreeMap<String, String>> {
    if doc.source != "app_post" {
        return None;
    }
    let facts: BTreeMap<String, String> = doc
        .attributes
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!facts.is_empty()).then_some(facts)
}

/// Save an app post's card (or mark it gone, `doc = None`): its resource made if new, never
/// routed to a resolver (`classify` leaves `/pub/<ns>/v1/posts/` alone), its change journaled.
async fn save_card(pool: &PgPool, uri: &str, doc: Option<&ResourceMeta>) -> Result<()> {
    let Ok((id, uri, _)) = crate::model::identity(uri) else {
        return Ok(());
    };
    let generation: Option<i64> = match doc {
        Some(_) => {
            let mut tx = pool.begin().await?;
            store::insert_plain(&mut tx, &id, &uri).await?;
            tx.commit().await?;
            sqlx::query_scalar("SELECT generation FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_optional(pool)
                .await?
        }
        None => {
            sqlx::query_scalar("SELECT generation FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_optional(pool)
                .await?
        }
    };
    let Some(generation) = generation else {
        return Ok(());
    };
    let doc = match doc {
        Some(doc) => doc.clone(),
        None => {
            // Deleted by its author: gone at once, as an object whose content withholds it is.
            let mut gone = ResourceMeta::new(&uri, "app_post", 86400);
            gone.status = crate::model::Status::Missing;
            gone.withheld = true;
            gone
        }
    };
    crate::changes::save(
        pool,
        &id,
        generation,
        &doc,
        &crate::config::QueueConfig::default(),
    )
    .await
}

/// The polls a page has failed for, to skip unreachable lines after `MAX_ATTEMPTS`.
#[derive(Debug, Default)]
pub struct Attempts {
    cursor: Option<i64>,
    failed: u32,
}

/// One page of the app-post feed: the number of lines it held, or 0 when there was nothing to
/// read or a reset began.
pub async fn poll_once(
    pool: &PgPool,
    nexus_url: &str,
    nexus: &Nexus,
    versions: &dyn Versions,
    metrics: &Metrics,
    attempts: &mut Attempts,
) -> Result<usize> {
    let key = cursor_key(nexus_url);
    let requested = crate::ingest::cursor(pool, &key).await?;
    let feed = nexus.app_post_changes(requested.cursor, PAGE).await?;
    apply(pool, &key, &requested, &feed, versions, metrics, attempts).await
}

/// One page applied: every line fetched and read, the post cards saved, then the references
/// and the cursor in one transaction. A page that fails to reach a homeserver is retried, its
/// failing lines skipped after `MAX_ATTEMPTS` polls.
pub async fn apply(
    pool: &PgPool,
    key: &str,
    requested: &crate::ingest::Cursor,
    feed: &AppPostPage,
    versions: &dyn Versions,
    metrics: &Metrics,
    attempts: &mut Attempts,
) -> Result<usize> {
    let changed = matches!((&requested.epoch, &feed.epoch), (Some(a), Some(b)) if a != b);
    if changed || requested.cursor > feed.length {
        reset(pool, key, feed).await?;
        return Ok(0);
    }
    let read = feed.page.read();
    if read > PAGE as usize
        || feed.page.next_cursor != requested.cursor + read as i64
        || feed.page.next_cursor > feed.length
    {
        bail!("invalid app-post feed cursor progression");
    }
    if attempts.cursor != Some(requested.cursor) {
        *attempts = Attempts {
            cursor: Some(requested.cursor),
            failed: 0,
        };
    }
    let mut outcomes: BTreeMap<&'static str, u64> = feed.page.skipped.clone();
    let mut read_lines = Vec::with_capacity(feed.page.lines.len());
    for line in &feed.page.lines {
        match process(line, versions).await {
            Ok(Ok(projection)) => read_lines.push((line, projection)),
            Ok(Err(reason)) => *outcomes.entry(reason).or_default() += 1,
            Err(error) if attempts.failed + 1 < MAX_ATTEMPTS => {
                attempts.failed += 1;
                return Err(error.context("an app post's homeserver failed; the page is retried"));
            }
            Err(_) => *outcomes.entry("unreachable").or_default() += 1,
        }
    }
    // The cards first: a page replayed after a crash saves them again.
    for (line, projection) in &read_lines {
        let post_uri = format!("pubky://{}{}", line.author, line.post);
        match (&line.op, &projection.card) {
            (Op::Put, Some(card)) => save_card(pool, &post_uri, Some(card)).await?,
            (Op::Del, _) => save_card(pool, &post_uri, None).await?,
            _ => {}
        }
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::INGEST)
        .execute(&mut *tx)
        .await?;
    let saved: crate::ingest::Cursor =
        sqlx::query_as("SELECT cursor, epoch FROM feed_cursor WHERE nexus=$1 FOR UPDATE")
            .bind(key)
            .fetch_one(&mut *tx)
            .await?;
    if saved.cursor != requested.cursor || saved.epoch != requested.epoch {
        return Ok(0);
    }
    store::lock_writers(&mut tx, store::WriteOrder::Shared).await?;
    crate::alias::lock(&mut tx).await?;
    let mut affected = BTreeSet::new();
    let mut aliased = vec![];
    for (line, projection) in &read_lines {
        let at = DateTime::<Utc>::from_timestamp_millis(line.at)
            .and_then(crate::event::time::bounded)
            .unwrap_or_else(Utc::now);
        write_post(
            &mut tx,
            &line.author,
            &line.post,
            projection,
            at,
            &mut affected,
            &mut aliased,
        )
        .await?;
        *outcomes
            .entry(match line.op {
                Op::Put => "indexed",
                Op::Del => "deleted",
            })
            .or_default() += 1;
    }
    store::recompute_scores(&mut tx, &affected.into_iter().collect::<Vec<_>>()).await?;
    sqlx::query("UPDATE feed_cursor SET cursor=$2, epoch=$3, feed_length=$4, updated_at=now() WHERE nexus=$1")
        .bind(key)
        .bind(feed.page.next_cursor)
        .bind(feed.epoch.as_ref().or(saved.epoch.as_ref()))
        .bind(feed.length)
        .execute(&mut *tx)
        .await?;
    crate::alias::record(&mut tx, aliased).await?;
    tx.commit().await?;
    for (outcome, count) in &outcomes {
        metrics
            .app_posts
            .with_label_values(&[outcome])
            .inc_by(*count);
    }
    let skipped: u64 = outcomes
        .iter()
        .filter(|(o, _)| !matches!(**o, "indexed" | "deleted"))
        .map(|(_, n)| n)
        .sum();
    if skipped > 0 {
        tracing::warn!(skipped, "skipped app-post feed lines");
    }
    Ok(read)
}

/// One post's references written as the feed writes them: its previous version's go, an
/// override's replaced occurrences of its master go, and its own (but a master's occurrence an
/// override by the same author has replaced) are written. A deletion is an empty projection.
async fn write_post(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    author: &str,
    post: &str,
    projection: &Projection,
    at: DateTime<Utc>,
    affected: &mut BTreeSet<String>,
    aliased: &mut Vec<crate::alias::Line>,
) -> Result<()> {
    let old: Vec<String> = sqlx::query_scalar(
        "DELETE FROM reference WHERE author=$1 AND object_path=$2 AND feed='app_posts' RETURNING resource_id",
    )
    .bind(author)
    .bind(post)
    .fetch_all(&mut **tx)
    .await?;
    affected.extend(old);
    if let Some((master, rids)) = &projection.replaces {
        let gone: Vec<String> = sqlx::query_scalar(
            "DELETE FROM reference WHERE author=$1 AND object_path=$2 AND feed='app_posts' AND recurrence_id=ANY($3) RETURNING resource_id",
        )
        .bind(author)
        .bind(master)
        .bind(rids)
        .fetch_all(&mut **tx)
        .await?;
        affected.extend(gone);
    }
    let overridden: Vec<String> = sqlx::query_scalar(
        "SELECT recurrence_id FROM reference WHERE author=$1 AND label=$2 AND kind='event' AND feed='app_posts' AND recurrence_id IS NOT NULL",
    )
    .bind(author)
    .bind(post)
    .fetch_all(&mut **tx)
    .await?;
    for r in &projection.refs {
        if r.kind == "event"
            && projection.replaces.is_none()
            && r.recurrence_id
                .as_ref()
                .is_some_and(|rid| overridden.contains(rid))
        {
            continue;
        }
        let Ok((id, uri, _)) = crate::model::identity(&r.uri) else {
            continue;
        };
        aliased.extend(store::insert_resource(tx, &id, &uri).await?);
        let unit_key = format!(
            "{id}:{}:{}",
            r.kind,
            r.recurrence_id.as_deref().unwrap_or("")
        );
        sqlx::query(r#"INSERT INTO reference(resource_id,kind,author,label,object_path,recurrence_id,start_at,end_at,created_at,unit_key,event_status,feed)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'app_posts')
            ON CONFLICT(author,object_path,unit_key) DO UPDATE SET resource_id=excluded.resource_id,kind=excluded.kind,label=excluded.label,
            recurrence_id=excluded.recurrence_id,start_at=excluded.start_at,end_at=excluded.end_at,created_at=excluded.created_at,
            event_status=excluded.event_status,feed=excluded.feed"#)
            .bind(&id).bind(r.kind).bind(author).bind(&r.label).bind(post).bind(&r.recurrence_id)
            .bind(r.start).bind(r.end).bind(at).bind(&unit_key).bind(r.status)
            .execute(&mut **tx).await?;
        affected.insert(id);
    }
    Ok(())
}

/// One post's references written as the app-post feed writes them (`refs` empty: the post
/// deleted), its scores recomputed: what a post becomes, for tests that start from it.
pub async fn write(
    pool: &PgPool,
    author: &str,
    post: &str,
    refs: Vec<Ref>,
    at: DateTime<Utc>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    store::lock_writers(&mut tx, store::WriteOrder::Shared).await?;
    crate::alias::lock(&mut tx).await?;
    let (mut affected, mut aliased) = (BTreeSet::new(), vec![]);
    let projection = Projection {
        refs,
        ..Default::default()
    };
    write_post(
        &mut tx,
        author,
        post,
        &projection,
        at,
        &mut affected,
        &mut aliased,
    )
    .await?;
    store::recompute_scores(&mut tx, &affected.into_iter().collect::<Vec<_>>()).await?;
    crate::alias::record(&mut tx, aliased).await?;
    tx.commit().await?;
    Ok(())
}

/// A new epoch, or a cursor past the feed's end: the app-post feed's references go (the cards
/// they keep lapse with them), and the feed is read again from the start.
async fn reset(pool: &PgPool, key: &str, feed: &AppPostPage) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::INGEST)
        .execute(&mut *tx)
        .await?;
    store::lock_writers(&mut tx, store::WriteOrder::Exclusive).await?;
    let gone: Vec<String> =
        sqlx::query_scalar("DELETE FROM reference WHERE feed='app_posts' RETURNING resource_id")
            .fetch_all(&mut *tx)
            .await?;
    let gone: Vec<String> = gone
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    store::recompute_scores(&mut tx, &gone).await?;
    sqlx::query("UPDATE feed_cursor SET cursor=0, epoch=$2, feed_length=$3, updated_at=now(), resets=resets+1 WHERE nexus=$1")
        .bind(key)
        .bind(&feed.epoch)
        .bind(feed.length)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::warn!("Nexus app-post feed reset; reading it again");
    Ok(())
}
