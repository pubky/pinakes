//! The `deezer` also-on source.
use crate::{
    also_on::{deezer_api, Context, Found, Key, Source, Subject},
    music::{self, Level, Service},
};
use anyhow::Result;
use async_trait::async_trait;

use crate::music_sources::*;

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
        own_object(&subject.uri).is_some()
            || reverse_subject(subject, Service::Deezer).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let link = own_object(&subject.uri);
        let Some(level) = link
            .as_ref()
            .map(|l| l.level)
            .or_else(|| reverse_subject(subject, Service::Deezer).map(|(level, _)| level))
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
        if let Some(link) = link {
            // Its own record of its own object.
            let Some(v) = deezer_api::object(cx, &format!("{kind}/{}", link.id)).await? else {
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
        for value in key_values(subject, namespace, kind == "track", 2) {
            let value = value.as_str();
            // Deezer finds a UPC-A by its 12 digits, without GTIN-13's leading zero.
            let asked = match value.strip_prefix('0') {
                Some(upc) if kind == "album" => upc,
                _ => value,
            };
            let Some(v) = deezer_api::object(cx, &format!("{kind}/{namespace}:{asked}")).await?
            else {
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

/// A Deezer track or album link: its own record names its ISRC or barcode.
fn own_object(uri: &str) -> Option<music::Link> {
    music::parse(uri)
        .filter(|l| l.service == Service::Deezer && matches!(l.level, Level::Track | Level::Album))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A YouTube video with an `isrc:` key is asked as its recording, as every lookup by ISRC
    /// asks it (`reverse_subject`): Deezer's track with that ISRC (its recorded answer), read
    /// back and keyed by it.
    #[tokio::test]
    async fn a_video_with_an_isrc_key_gets_deezers_track() {
        let body = std::fs::read(format!(
            "{}/tests/fixtures/hubs/deezer-track-isrc-GBUM71029604.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let (egress, _) = crate::sources::fixture::egress(vec![(
            "https://api.deezer.com/track/isrc:GBUM71029604",
            200,
            "application/json",
            body,
        )]);
        let cx = Context {
            egress,
            config: Default::default(),
        };
        let subject = Subject {
            id: "x".into(),
            uri: "https://www.youtube.com/watch?v=fJ9rUzIMcZQ".into(),
            status: "ok".into(),
            type_name: Some("VideoObject".into()),
            doc: None,
            keys: vec![("isrc:GBUM71029604".into(), "musicbrainz".into())],
            first_seen_at: None,
            candidates: vec![],
        };
        assert!(DeezerSource.applies(&subject));
        let found = DeezerSource.lookup(&cx, &subject).await.unwrap();
        let links: Vec<(&str, Option<&str>)> = found
            .links
            .iter()
            .map(|l| (l.uri.as_str(), l.key.as_deref()))
            .collect();
        assert_eq!(
            links,
            [(
                "https://www.deezer.com/track/3541552051",
                Some("isrc:GBUM71029604")
            )]
        );
    }
}

