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
/// One Deezer API object; `None` when Deezer has no data (its error 800) or no such ID. Never
/// while the source is off, whoever asks: the egress refuses `api.deezer.com` then too
/// (`also_on::off_hosts`).
pub(crate) async fn deezer(cx: &Context, path: &str) -> Result<Option<Value>> {
    Ok(answer(cx, path)
        .await?
        .filter(|v| v["id"].as_u64().is_some()))
}
/// One Deezer API answer (`path` under `https://api.deezer.com/`, its query escaped), shared for
/// an hour; `None` when Deezer answers an error (800: no data) or a client error. Its quota (error
/// 4, 429) and an outage are retried. Never while the source is off.
pub(crate) async fn answer(cx: &Context, path: &str) -> Result<Option<Value>> {
    if !cx.config.also_on.source_enabled(&DeezerSource) {
        anyhow::bail!("the deezer source is off: its API is not called");
    }
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
        None if v["error"].is_null() => Ok(Some(v)),
        Some(4) => Err(busy()),
        _ => Ok(None),
    }
}

/// The album a Deezer track is on. Never while the source is off.
pub(crate) async fn track_album(cx: &Context, track: &str) -> Result<Option<String>> {
    Ok(deezer(cx, &format!("track/{track}"))
        .await?
        .and_then(|v| v["album"]["id"].as_u64())
        .map(|id| id.to_string()))
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
    fn api_hosts(&self) -> &'static [&'static str] {
        &["api.deezer.com"]
    }
    fn id(&self) -> &'static str {
        "deezer"
    }
    fn applies(&self, subject: &Subject) -> bool {
        let link = music::parse(&subject.uri);
        if link.as_ref().is_some_and(|l| {
            l.service == Service::Deezer && matches!(l.level, Level::Track | Level::Album)
        }) {
            return true;
        }
        match link.map(|l| l.level).or_else(|| mb_level(&subject.uri)) {
            Some(Level::Track) => subject.keys_in("isrc").next().is_some(),
            Some(Level::Album) => subject.keys_in("upc").next().is_some(),
            _ => false,
        }
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let link = music::parse(&subject.uri);
        let Some(level) = link
            .as_ref()
            .map(|l| l.level)
            .or_else(|| mb_level(&subject.uri))
        else {
            return Ok(Found::default());
        };
        let (kind, field, namespace) = match level {
            Level::Track => ("track", "isrc", "isrc"),
            Level::Album => ("album", "upc", "upc"),
            _ => return Ok(Found::default()),
        };
        let normal = if kind == "track" {
            music::isrc
        } else {
            crate::barcode::gtin13
        };
        let mut found = Found::default();
        if let Some(link) = link.filter(|l| l.service == Service::Deezer) {
            // Its own record of its own object.
            let Some(v) = deezer(cx, &format!("{kind}/{}", link.id)).await? else {
                return Ok(found);
            };
            if let Some(value) = v[field].as_str().and_then(normal) {
                if kind == "track" {
                    // Its ISRC names its recording as the track's length tells (`isrc_match`).
                    let duration = v["duration"].as_i64();
                    let named = isrc_match(cx, &value, duration).await?;
                    found.keys.extend(isrc_key(&value, &named, "deezer"));
                } else {
                    found.keys.push(Key {
                        key: format!("{namespace}:{value}"),
                        via: "deezer".into(),
                    });
                }
                found
                    .facts
                    .push(crate::also_on::Fact::new(namespace, value));
            }
            return Ok(found);
        }
        // The link's own ISRC first, then the hubs'. Its own only when it is a key: a shared ISRC
        // (on several recordings) is a fact, and Deezer would answer with any of them.
        let own = own_isrc(subject)
            .filter(|o| kind == "track" && subject.keys_in(namespace).any(|k| k == o.as_str()));
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
            if let Some(found_link) = derived(&url, level, "deezer") {
                found
                    .links
                    .push(found_link.keyed(format!("{namespace}:{value}")));
            }
        }
        Ok(found)
    }
}
