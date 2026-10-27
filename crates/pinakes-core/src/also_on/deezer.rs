//! The `deezer` also-on source.
use crate::{
    also_on::{Context, Found, Key, Source, Subject},
    music::{self, Level, Service},
};
use anyhow::Result;
use async_trait::async_trait;

use crate::music_sources::*;
use serde_json::Value;

const DEEZER_API: &str = "https://api.deezer.com";
/// One Deezer API object; `None` when Deezer has no data (its error 800) or no such ID.
async fn deezer(cx: &Context, path: &str) -> Result<Option<Value>> {
    let options = crate::resolver::egress::FetchOptions {
        accept: Some("application/json".into()),
        ..Default::default()
    };
    let fetched = cx
        .egress
        .discover(&format!("{DEEZER_API}/{path}"), &options, 3600)
        .await?;
    let busy = || -> anyhow::Error {
        crate::resolver::RetryLater {
            class: "deezer_busy",
            seconds: fetched.retry_after_secs.clamp(60, 3600),
        }
        .into()
    };
    match fetched.status {
        200 => {}
        429 | 500..=599 => return Err(busy()),
        _ => return Ok(None),
    }
    let v: Value = serde_json::from_slice(&fetched.body)?;
    // Deezer answers errors with 200: 4 is its quota, 800 "no data".
    match v["error"]["code"].as_u64() {
        None if v["id"].as_u64().is_some() => Ok(Some(v)),
        Some(4) => Err(busy()),
        _ => Ok(None),
    }
}

/// Deezer's public API: its own record of a Deezer link, and its lookups by ISRC and barcode.
pub struct DeezerSource;
#[async_trait]
impl Source for DeezerSource {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn budgets(&self) -> &'static [(&'static str, f64)] {
        // Deezer's public API: 50 requests per 5 seconds.
        &[("api.deezer.com", 10.0)]
    }
    fn id(&self) -> &'static str {
        "deezer"
    }
    fn enabled_by_default(&self) -> bool {
        false
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| match (l.service, l.level) {
            (Service::Deezer, Level::Track | Level::Album) => true,
            (_, Level::Track) => subject.keys_in("isrc").next().is_some(),
            (_, Level::Album) => subject.keys_in("upc").next().is_some(),
            _ => false,
        })
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(link) = music::parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let (kind, field, namespace) = match link.level {
            Level::Track => ("track", "isrc", "isrc"),
            Level::Album => ("album", "upc", "upc"),
            _ => return Ok(Found::default()),
        };
        let normal = if kind == "track" {
            music::isrc
        } else {
            music::gtin13
        };
        let mut found = Found::default();
        if link.service == Service::Deezer {
            // Its own record of its own object.
            let Some(v) = deezer(cx, &format!("{kind}/{}", link.id)).await? else {
                return Ok(found);
            };
            if let Some(value) = v[field].as_str().and_then(normal) {
                found.keys.push(Key {
                    key: format!("{namespace}:{value}"),
                    via: "deezer".into(),
                });
                found
                    .facts
                    .push(crate::also_on::Fact::new(namespace, value));
            }
            return Ok(found);
        }
        // The link's own ISRC first, then the hubs'.
        let own = own_isrc(subject).filter(|_| kind == "track");
        let mut values: Vec<&str> = own.as_deref().into_iter().collect();
        values.extend(
            subject
                .keys_in(namespace)
                .filter(|v| Some(*v) != own.as_deref()),
        );
        for value in values.into_iter().take(2) {
            // Deezer finds a UPC-A by its 12 digits, without GTIN-13's leading zero.
            let asked = match value.strip_prefix('0') {
                Some(upc) if kind == "album" => upc,
                _ => value,
            };
            let Some(v) = deezer(cx, &format!("{kind}/{namespace}:{asked}")).await? else {
                continue;
            };
            // Only the object that has the value asked.
            if v[field].as_str().and_then(normal).as_deref() != Some(value) {
                continue;
            }
            let url = format!(
                "https://www.deezer.com/{kind}/{}",
                v["id"].as_u64().unwrap_or(0)
            );
            if let Some(found_link) = derived(&url, link.level, "deezer") {
                found
                    .links
                    .push(found_link.keyed(format!("{namespace}:{value}")));
            }
        }
        Ok(found)
    }
}
