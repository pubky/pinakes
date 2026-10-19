//! Generic events and calendar instances. Fetch health and event lifecycle are distinct.
use crate::model::{LocationSource, Point, ResourceMeta};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

pub mod adapters;
pub mod calendar;
pub mod page;
pub mod service;
pub mod storage;
pub mod time;

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct EventFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone_attribution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_day: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_status: Option<EventStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attendance: Option<Attendance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue: Option<Venue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organizer: Option<Organizer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offers: Option<Offers>,
    /// A local time without a resolvable zone is retained; UTC columns are only a sort proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_local: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_mode: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Scheduled,
    Cancelled,
    Postponed,
    Rescheduled,
    MovedOnline,
}
impl EventStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Cancelled => "cancelled",
            Self::Postponed => "postponed",
            Self::Rescheduled => "rescheduled",
            Self::MovedOnline => "moved_online",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value
            .rsplit(['/', '#', ':'])
            .next()?
            .to_ascii_lowercase()
            .as_str()
        {
            "eventscheduled" | "scheduled" | "confirmed" | "tentative" => Some(Self::Scheduled),
            "eventcancelled" | "cancelled" | "canceled" => Some(Self::Cancelled),
            "eventpostponed" | "postponed" => Some(Self::Postponed),
            "eventrescheduled" | "rescheduled" => Some(Self::Rescheduled),
            "eventmovedonline" | "moved_online" => Some(Self::MovedOnline),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Attendance {
    Offline,
    Online,
    Mixed,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct Venue {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub address: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osm_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osm_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_source: Option<LocationSource>,
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Organizer {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct Offers {
    pub low_price: Option<f64>,
    pub high_price: Option<f64>,
    pub currency: Option<String>,
}
impl EventFields {
    pub fn bound(&mut self) {
        use crate::model::truncate;
        for value in [&mut self.tz, &mut self.start_local, &mut self.end_local]
            .into_iter()
            .flatten()
        {
            truncate(value, 128);
        }
        if let Some(v) = &mut self.venue {
            truncate(&mut v.text, 1024);
            if let Some(n) = &mut v.name {
                truncate(n, 256);
            }
            for v in v.address.values_mut() {
                truncate(v, 256);
            }
        }
        if let Some(o) = &mut self.organizer {
            truncate(&mut o.name, 256);
            if o.url.as_ref().is_some_and(|s| s.len() > 2048) {
                o.url = None;
            }
        }
        if let Some(o) = &mut self.offers {
            if let Some(c) = &mut o.currency {
                truncate(c, 16);
            }
        }
        if let Some(v) = &mut self.join_mode {
            truncate(v, 64);
        }
        self.warnings.sort();
        self.warnings.dedup();
        self.warnings.truncate(16);
        self.categories.sort();
        self.categories.dedup();
        self.categories.truncate(32);
        for category in &mut self.categories {
            truncate(category, 128);
        }
    }
}
#[derive(Clone, Debug)]
pub struct Occurrence {
    pub uid: String,
    pub recurrence_id: String,
    pub doc: ResourceMeta,
    pub sequence: i64,
    pub modified: Option<DateTime<Utc>>,
    pub content_hash: String,
}
impl Occurrence {
    pub fn key(&self) -> String {
        serde_json::to_string(&(&self.uid, &self.recurrence_id)).expect("strings")
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContainerLimits {
    pub max_changes: usize,
    pub max_rows: usize,
    pub max_new_keys: usize,
    pub retention_days: u32,
    pub max_holds: u32,
    pub cadence_floor: u64,
}
impl Default for ContainerLimits {
    fn default() -> Self {
        Self {
            max_changes: 100,
            max_rows: 10000,
            max_new_keys: 1000,
            retention_days: 30,
            max_holds: 5,
            cadence_floor: 3600,
        }
    }
}
/// Instants Postgres can store (`time::bounded`), applied to every document before it is saved:
/// an occurrence whose start is out of range is dropped with the feed's
/// `occurrence_start_out_of_range`; an out-of-range occurrence end or modified time is dropped
/// with the item's `end_out_of_range`/`modified_out_of_range`, as is a document's own start or
/// end. One bad item therefore never fails the feed's apply. `None` when nothing was out of
/// range, so the common case does not copy the document. This is the early pass:
/// `storage::prepare` bounds each upsert row again (`bound_item`), after anything that may
/// re-derive times, and `changes::save` clears the document again (`clear_instants`) after
/// `preserve_venue`.
pub fn bound_instants(doc: &ResourceMeta) -> Option<ResourceMeta> {
    let out = |t: &Option<DateTime<Utc>>| t.is_some_and(|t| time::bounded(t).is_none());
    let item = |d: &ResourceMeta| out(&d.start) || out(&d.end);
    if !item(doc)
        && !doc.container.as_ref().is_some_and(|c| {
            c.occurrences
                .iter()
                .any(|o| item(&o.doc) || out(&o.modified))
        })
    {
        return None;
    }
    let mut doc = doc.clone();
    clear_instants(&mut doc);
    if let Some(c) = &mut doc.container {
        let before = c.occurrences.len();
        c.occurrences
            .retain(|o| o.doc.start.is_none_or(|t| time::bounded(t).is_some()));
        if c.occurrences.len() < before {
            c.warnings.push("occurrence_start_out_of_range".into());
            doc.event
                .warnings
                .push("occurrence_start_out_of_range".into());
            doc.event.warnings.sort();
            doc.event.warnings.dedup();
            doc.event.warnings.truncate(16);
        }
        for o in &mut c.occurrences {
            bound_item(&mut o.doc, &mut o.modified);
        }
    }
    Some(doc)
}
/// Bound a document's start and end (`time::bounded`, which also truncates to the microseconds
/// Postgres keeps): an out-of-range value is dropped with its `start_out_of_range` or
/// `end_out_of_range` warning.
pub fn clear_instants(d: &mut ResourceMeta) {
    let mut changed = false;
    for (value, warning) in [
        (&mut d.start, "start_out_of_range"),
        (&mut d.end, "end_out_of_range"),
    ] {
        if let Some(t) = *value {
            *value = time::bounded(t);
            if value.is_none() {
                d.event.warnings.push(warning.into());
                changed = true;
            }
        }
    }
    if changed {
        tidy_warnings(d);
    }
}
fn tidy_warnings(d: &mut ResourceMeta) {
    d.event.warnings.sort();
    d.event.warnings.dedup();
    d.event.warnings.truncate(16);
}
/// One occurrence's instants: `false` when its start is out of range (the row must be dropped);
/// otherwise each instant is truncated to microseconds and an out-of-range end or modified time
/// is dropped with the item's warning.
pub fn bound_item(d: &mut ResourceMeta, modified: &mut Option<DateTime<Utc>>) -> bool {
    if d.start.is_some_and(|t| time::bounded(t).is_none()) {
        return false;
    }
    if let Some(t) = *modified {
        *modified = time::bounded(t);
        if modified.is_none() {
            d.event.warnings.push("modified_out_of_range".into());
            tidy_warnings(d);
        }
    }
    clear_instants(d);
    true
}
#[derive(Clone, Debug, Default)]
pub struct Container {
    /// Committed with the snapshot, never before parsing or a generation fence.
    pub validators: Option<crate::resolver::egress::Validators>,
    pub limits: ContainerLimits,
    pub delta: bool,
    pub deleted_uids: Vec<String>,
    pub cursor: Option<adapters::cursor::Checkpoint>,
    pub occurrences: Vec<Occurrence>,
    pub feed_ttl: Option<u64>,
    pub warnings: Vec<String>,
    pub horizon_start: DateTime<Utc>,
    pub horizon_end: DateTime<Utc>,
    pub held_retry_secs: u64,
    /// The adapter's per-parent and global derived-link caps: a lapsed link comes back only
    /// within them (`store::EXTEND_PARENT_DERIVATIONS`). `(0, 0)` revives none.
    pub link_caps: (usize, usize),
}
