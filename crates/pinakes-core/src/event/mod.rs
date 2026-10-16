//! Generic events and calendar instances. Fetch health and event lifecycle are distinct.
use crate::model::{LocationSource, Point, ResourceMeta};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

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
    pub volatile: bool,
}
impl Occurrence {
    pub fn key(&self) -> String {
        serde_json::to_string(&(&self.uid, &self.recurrence_id)).expect("strings")
    }
}
#[derive(Clone, Debug, Default)]
pub struct Container {
    pub occurrences: Vec<Occurrence>,
    pub feed_ttl: Option<u64>,
    pub warnings: Vec<String>,
    pub horizon_start: DateTime<Utc>,
    pub horizon_end: DateTime<Utc>,
    pub held_retry_secs: u64,
}
