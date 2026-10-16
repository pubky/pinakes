//! Platform declarations are data; all network paths share the production egress policy.
use crate::{
    adapter::{Manifest, Mode},
    config::EventConfig,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};
use url::Url;

pub mod cursor;
pub mod nostr;
pub mod service;
pub mod state;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdapterConfig {
    pub enabled: bool,
    pub websocket: bool,
    pub websocket_deadline_ms: u64,
    pub max_cursor_pages: usize,
    pub max_calendar_members: usize,
    pub max_linked_resources: usize,
    pub platforms: BTreeMap<String, Options>,
}
impl Default for AdapterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            websocket: true,
            websocket_deadline_ms: 2000,
            max_cursor_pages: 10,
            max_calendar_members: 50,
            max_linked_resources: 100,
            platforms: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub enabled: Option<bool>,
    // Never serialize adapter config into the public info response.
    pub key: Option<String>,
    pub canary: Option<Url>,
    pub ua_profile: Option<String>,
    pub cadence_floor: Option<u64>,
    pub months_back: Option<u32>,
    pub months_forward: Option<u32>,
    pub max_occurrences: Option<usize>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Definition {
    pub id: String,
    pub domains: Vec<String>,
    pub paths: Vec<String>,
    pub parser: String,
    pub mode: Mode,
    pub cadence_floor: u64,
    pub identity: String,
    pub cancellation: String,
    pub expected_fields: Vec<String>,
    pub canary: Option<Url>,
    pub canary_note: Option<String>,
    pub ua_profile: String,
    pub requires_key: bool,
    pub fragile: bool,
    pub enabled: bool,
}
pub fn definitions() -> &'static [Definition] {
    static ALL: OnceLock<Vec<Definition>> = OnceLock::new();
    ALL.get_or_init(|| {
        serde_json::from_str(include_str!("../../../../../data/event-adapters.json"))
            .expect("bundled adapter declarations")
    })
}
pub fn definition(id: &str) -> Option<&'static Definition> {
    definitions().iter().find(|d| d.id == id)
}
pub fn detect(uri: &str) -> Option<&'static Definition> {
    let u = Url::parse(uri).ok()?;
    let host = u.host_str().unwrap_or("").trim_end_matches('.');
    let path = u.path();
    if host == "api.nostr.wine" {
        return definition("nip52");
    }
    if u.scheme() == "nostr" {
        return definition("nip52");
    }
    if path.contains("/remote.php/dav/public-calendars/")
        || (path.contains("/apps/calendar/p/") || path.contains("/apps/calendar/embed/"))
    {
        return definition("nextcloud");
    }
    if (host == "icloud.com" || host.ends_with(".icloud.com")) && path.contains("/published/") {
        return definition("icloud");
    }
    if host == "www.googleapis.com" && path.starts_with("/calendar/v3/calendars/") {
        return definition("google_calendar_api");
    }
    if host == "api.billetto.com"
        || host == "billetto.dk" && path.starts_with("/api/v3/public/events/")
    {
        return definition("billetto_api");
    }
    if host == "api.eventfrog.net" {
        return definition("eventfrog_api");
    }
    if path == "/starticket/wwwroot/ticketing/feeds/starticketfeed_D.xml" {
        return definition("starticket");
    }
    if let Some(d) = definitions().iter().find(|d| {
        d.domains
            .iter()
            .any(|h| host == h || host.strip_suffix(h).is_some_and(|p| p.ends_with('.')))
            && (d.paths.is_empty() || d.paths.iter().any(|p| path.starts_with(p)))
    }) {
        return Some(d);
    }
    if crate::event::service::is_feed(uri) || matches!(u.scheme(), "webcal" | "webcals") {
        return definition("ics");
    }
    None
}
pub fn detected_html(uri: &str, html: &str) -> Option<&'static Definition> {
    detect(uri).or_else(|| {
        let s = html.to_ascii_lowercase();
        if s.contains("pretix") && s.contains("/ical/") {
            definition("pretix")
        } else if s.contains("eventyay") {
            definition("eventyay")
        } else if s.contains("gancio") {
            definition("gancio")
        } else if s.contains("gathio") || s.contains("/export/event/") {
            definition("gathio")
        } else if s.contains("mobilizon") {
            definition("mobilizon")
        } else if s.contains("hitobito") {
            definition("hitobito")
        } else {
            None
        }
    })
}
pub fn options<'a>(cfg: &'a EventConfig, id: &str) -> Option<&'a Options> {
    cfg.adapters.platforms.get(id)
}
pub fn manifest(d: &Definition, cfg: &EventConfig) -> Manifest {
    let opts = options(cfg, &d.id);
    let reason = if !cfg.enabled || !cfg.adapters.enabled {
        Some("event adapters disabled")
    } else if !opts.and_then(|o| o.enabled).unwrap_or(d.enabled) {
        Some("disabled by configuration")
    } else if d.requires_key
        && opts
            .and_then(|o| o.key.as_deref())
            .is_none_or(str::is_empty)
    {
        Some("missing API key")
    } else if d.mode == Mode::Container && !cfg.calendars {
        Some("calendars disabled")
    } else {
        None
    };
    Manifest {
        source: d.id.clone(),
        mode: d.mode,
        cadence_floor: opts
            .and_then(|o| o.cadence_floor)
            .unwrap_or(d.cadence_floor)
            .max(if d.mode == Mode::None {
                7 * 86400
            } else {
                3600
            }),
        identity: d.identity.clone(),
        cancellation: d.cancellation.clone(),
        expected_fields: d.expected_fields.clone(),
        canary: opts
            .and_then(|o| o.canary.clone())
            .or_else(|| d.canary.clone()),
        canary_note: d.canary_note.clone(),
        ua_profile: opts
            .and_then(|o| o.ua_profile.clone())
            .unwrap_or_else(|| d.ua_profile.clone()),
        requires_key: d.requires_key,
        enabled: reason.is_none(),
        disabled_reason: reason.map(str::to_owned),
        fragile: d.fragile,
    }
}
pub fn manifests(cfg: &EventConfig) -> Vec<Manifest> {
    definitions().iter().map(|d| manifest(d, cfg)).collect()
}
pub fn profile(name: &str) -> &'static str {
    match name {
        "browser" => "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        "firefox" => "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0",
        "plain" => "curl/8.10.1",
        _ => crate::resolver::egress::USER_AGENT,
    }
}
pub fn configured_event(cfg: &EventConfig, d: &Definition) -> EventConfig {
    let mut cfg = cfg.clone();
    if let Some(o) = cfg.adapters.platforms.get(&d.id) {
        if let Some(v) = o.months_back {
            cfg.months_back = v;
        }
        if let Some(v) = o.months_forward {
            cfg.months_forward = v;
        }
        if let Some(v) = o.max_occurrences {
            cfg.max_occurrences_per_feed = v;
        }
    }
    cfg
}
pub fn validate(cfg: &EventConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=100).contains(&cfg.adapters.max_cursor_pages)
            && (1..=500).contains(&cfg.adapters.max_calendar_members)
            && (1..=1000).contains(&cfg.adapters.max_linked_resources)
            && (100..=5000).contains(&cfg.adapters.websocket_deadline_ms),
        "invalid adapter limits"
    );
    for (id, o) in &cfg.adapters.platforms {
        anyhow::ensure!(definition(id).is_some(), "unknown event adapter {id}");
        anyhow::ensure!(
            o.ua_profile
                .as_deref()
                .is_none_or(|p| ["pinakes", "browser", "plain", "firefox"].contains(&p)),
            "unknown UA profile"
        );
        anyhow::ensure!(
            o.cadence_floor
                .is_none_or(|n| (3600..=31536000).contains(&n))
                && o.months_back.is_none_or(|n| n <= 120)
                && o.months_forward.is_none_or(|n| (1..=120).contains(&n))
                && o.max_occurrences.is_none_or(|n| (1..=10000).contains(&n)),
            "invalid platform horizon or cap"
        );
    }
    Ok(())
}
