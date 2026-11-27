//! TIDAL's own record of its own album or track, from TIDAL's developer API: an album's barcode
//! (`barcodeId`) as the key `upc:…` and a track's ISRC (`isrc`) as `isrc:…`, so the lookups by
//! barcode and ISRC after it (Deezer, iTunes) join the rest. An ISRC MusicBrainz gives to several
//! recordings keys the one the track's duration picks, else is a fact only, as a track page's
//! own ISRC is (`music_sources::isrc_match`).
//!
//! Asked only with an operator's client ID and secret (`[tidal]`, or `PINAKES_TIDAL_CLIENT_ID`
//! and `PINAKES_TIDAL_CLIENT_SECRET`), through the client-credentials grant. A batch of links asks
//! for up to 20 albums or tracks at once (`filter[id]`).
use super::{
    client_credentials::{self, slot_key},
    tidal_api::{api_object, TidalConfig, API, JSON_API, TIDAL},
    Context, Fact, Found, Key, Source, Subject,
};
use crate::{
    music::{self, Level, Service},
    music_sources::Kind,
    resolver::egress::{FetchOptions, Fetched},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeSet;

/// The most IDs one `filter[id]` request takes.
const BATCH: usize = 20;

/// The kind of a TIDAL album or track link.
fn kind_of(link: &music::Link) -> Option<Kind> {
    match (link.service, link.level) {
        (Service::Tidal, Level::Album) => Some(Kind::Album),
        (Service::Tidal, Level::Track) => Some(Kind::Track),
        _ => None,
    }
}
/// The attribute of an album or a track that holds its barcode or its ISRC.
fn attribute(kind: Kind) -> &'static str {
    match kind {
        Kind::Album => "barcodeId",
        Kind::Track => "isrc",
    }
}

pub struct Tidal;
#[async_trait]
impl Source for Tidal {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn budgets(&self) -> &'static [(&'static str, f64)] {
        // TIDAL's developer API publishes no figure and answers 429 with a time to wait: a cautious
        // 2/s (a batch asks for 20 items).
        &[("openapi.tidal.com", 2.0)]
    }
    fn id(&self) -> &'static str {
        "tidal"
    }
    fn section(&self) -> Option<super::Section> {
        Some(super::Section::of::<TidalConfig>())
    }
    fn ready(&self, config: &crate::config::Config) -> bool {
        config.section::<TidalConfig>().pair().is_some()
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| kind_of(&l).is_some())
    }
    async fn prefetch(&self, cx: &Context, uris: &[String]) {
        let wanted: BTreeSet<(Kind, String)> = uris
            .iter()
            .filter_map(|u| music::parse(u))
            .filter_map(|l| Some((kind_of(&l)?, l.id)))
            .collect();
        for kind in [Kind::Album, Kind::Track] {
            let ids: Vec<&str> = wanted
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, id)| id.as_str())
                .collect();
            if ids.len() < 2 {
                continue;
            }
            for chunk in ids.chunks(BATCH) {
                if let Err(error) = prefetch(cx, kind, chunk).await {
                    tracing::debug!(
                        kind = crate::resolver::error_class(&error),
                        "batched TIDAL lookup failed; links ask alone"
                    );
                    break;
                }
            }
        }
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some((kind, link)) = music::parse(&subject.uri).and_then(|l| Some((kind_of(&l)?, l)))
        else {
            return Ok(Found::default());
        };
        let mut found = Found::default();
        let Some(attributes) = item(cx, kind, &link.id).await? else {
            return Ok(found);
        };
        let Some(value) = attributes[attribute(kind)]
            .as_str()
            .and_then(|v| kind.normal(v))
        else {
            return Ok(found);
        };
        let namespace = kind.namespace();
        match kind {
            Kind::Album => found.keys.push(Key {
                key: format!("{namespace}:{value}"),
                via: "tidal".into(),
            }),
            // A track's ISRC names its recording as the track's length tells (`isrc_match`).
            Kind::Track => {
                let duration = attributes["duration"]
                    .as_str()
                    .and_then(crate::music_sources::duration_secs);
                let named = crate::music_sources::isrc_match(cx, &value, duration).await?;
                found
                    .keys
                    .extend(crate::music_sources::isrc_key(&value, &named, "tidal"));
            }
        }
        found.facts.push(Fact::new(namespace, value));
        Ok(found)
    }
}

/// One album's or track's own request, which a batch answers in advance.
fn item_url(cx: &Context, kind: Kind, id: &str) -> String {
    format!(
        "{API}/{}/{id}?countryCode={}",
        kind.collection(),
        cx.config.section::<TidalConfig>().country
    )
}
/// How the API is asked: with the client-credentials token, its answers cached by the pair.
fn api_options(token: &str, slot: &[u8; 32]) -> FetchOptions {
    client_credentials::api_options(token, slot, JSON_API, &[JSON_API])
}
/// Lookups waiting for a pair's slot right now (`client_credentials::waiting`).
#[doc(hidden)]
pub fn lookups_waiting(id: &str, secret: &str) -> usize {
    client_credentials::waiting(&slot_key(&TIDAL, id, secret))
}
/// The configured pair and its slot key.
fn pair_of(cx: &Context) -> Result<(String, String, [u8; 32])> {
    let (id, secret) = cx
        .config
        .section::<TidalConfig>()
        .pair()
        .context("no TIDAL credentials")?;
    let slot = slot_key(&TIDAL, &id, &secret);
    Ok((id, secret, slot))
}

/// The attributes of one album or track; `None` when TIDAL has no such item in the country.
async fn item(cx: &Context, kind: Kind, id: &str) -> Result<Option<Value>> {
    let Some(v) = api_object(cx, &item_url(cx, kind, id)).await? else {
        return Ok(None);
    };
    let data = &v["data"];
    // Only the item asked for.
    Ok(
        (data["id"].as_str() == Some(id) && data["type"].as_str() == Some(kind.collection()))
            .then(|| data["attributes"].clone()),
    )
}

/// Up to 20 albums or tracks in one request: each item's own request finds its answer cached,
/// and one TIDAL leaves out finds that it has none.
async fn prefetch(cx: &Context, kind: Kind, ids: &[&str]) -> Result<()> {
    let (client_id, secret, slot) = pair_of(cx)?;
    let mut s = client_credentials::slot(&slot, cx.config.also_on.refusal_hold_secs).await;
    s.unless_busy(&TIDAL)?;
    let token = s.token(&TIDAL, cx, &client_id, &secret).await?;
    // A batch asks only for what its items don't have cached (a retry's, within the discovery cache's lifetime).
    let mut ids: Vec<&str> = ids.to_vec();
    for id in ids.clone() {
        if cx
            .egress
            .discovered(&item_url(cx, kind, id), &api_options(&token, &slot))
            .await?
        {
            ids.retain(|i| *i != id);
        }
    }
    if ids.len() < 2 {
        return Ok(());
    }
    let filter: String = ids
        .iter()
        .map(|id| format!("&filter%5Bid%5D={id}"))
        .collect();
    let url = format!(
        "{API}/{}?countryCode={}{filter}",
        kind.collection(),
        cx.config.section::<TidalConfig>().country
    );
    let options = api_options(&token, &slot);
    let fetched = match cx.egress.fetch_with(&url, &options).await {
        Ok(fetched) => fetched,
        Err(error) => return Err(s.answered(&TIDAL, error)),
    };
    match fetched.status {
        200 => {}
        401 => return Err(s.reject(&TIDAL, &token)),
        403 => {
            s.refuse(&TIDAL, "forbidden", 403);
            return Err(TIDAL.refused(s.hold));
        }
        status if crate::resolver::busy_status(status) => {
            s.cool_down(fetched.retry_after_secs);
            return Err(TIDAL.busy(fetched.retry_after_secs));
        }
        status => anyhow::bail!("TIDAL batch answered {status}"),
    }
    drop(s);
    let v: Value = serde_json::from_slice(&fetched.body).context("TIDAL batch")?;
    let items = v["data"].as_array().context("TIDAL batch without data")?;
    // A paged answer may hold the missing items on a later page.
    let whole = v["links"]["next"].is_null();
    for id in &ids {
        let answer = items.iter().find(|i| {
            i["id"].as_str() == Some(*id) && i["type"].as_str() == Some(kind.collection())
        });
        if answer.is_none() && !whole {
            continue;
        }
        let body = match answer {
            Some(answer) => serde_json::json!({ "data": answer }),
            None => serde_json::json!({"errors": [{"code": "NOT_FOUND"}]}),
        };
        let one = item_url(cx, kind, id);
        let status = if answer.is_some() { 200 } else { 404 };
        let seeded = Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url::Url::parse(&one)?,
            status,
            retry_after_secs: 60,
            location: None,
            content_type: JSON_API.into(),
            body: serde_json::to_vec(&body)?,
            charset: None,
        };
        cx.egress
            .seed(&one, &options, cx.egress.config().discovery_secs, seeded)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::RetryLater;
    use client_credentials::{Issued, Slot};
    use std::time::{Duration, Instant};

    /// A token TIDAL rejects (401) is dropped, and the next lookup asks TIDAL again with a new
    /// one: the 401 was never cached for the pair.
    #[tokio::test]
    async fn a_new_token_after_a_401_asks_again() {
        use crate::resolver::egress::{Dns, Egress, Transport};
        use std::net::SocketAddr;
        struct Public;
        #[async_trait]
        impl Dns for Public {
            async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
                Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
            }
        }
        #[derive(Default)]
        struct Tidal {
            items: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl Transport for Tidal {
            async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> Result<Fetched> {
                self.get_with(url, a, &FetchOptions::default()).await
            }
            async fn get_with(
                &self,
                url: &url::Url,
                _: &[SocketAddr],
                options: &FetchOptions,
            ) -> Result<Fetched> {
                let reply = |status: u16, body: &str| {
                    Fetched::answer(
                        url.clone(),
                        status,
                        JSON_API.into(),
                        body.as_bytes().to_vec(),
                    )
                };
                if url.host_str() == Some("auth.tidal.com") {
                    return Ok(reply(200, r#"{"access_token":"new","expires_in":14400}"#));
                }
                let bearer = options
                    .headers
                    .iter()
                    .find(|(k, _)| *k == "authorization")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                self.items.lock().unwrap().push(bearer.clone());
                Ok(if bearer == "Bearer old" {
                    reply(401, "{}")
                } else {
                    reply(
                        200,
                        r#"{"data":{"id":"7","type":"tracks","attributes":{"isrc":"DEQ121641300"}}}"#,
                    )
                })
            }
        }
        let transport = std::sync::Arc::new(Tidal::default());
        let mut config = crate::config::Config::default();
        config.sections.set(
            "tidal",
            serde_json::json!({"client_id": "rotate-id", "client_secret": "rotate-secret"}),
        );
        let cx = Context {
            egress: Egress::with_backends(
                Default::default(),
                std::sync::Arc::new(Public),
                transport.clone(),
            ),
            config: std::sync::Arc::new(config),
        };
        // A token issued two minutes ago, which TIDAL now rejects.
        let slot = slot_key(&TIDAL, "rotate-id", "rotate-secret");
        let now = Instant::now();
        *client_credentials::slot(&slot, 300).await = Slot {
            token: Some(Issued {
                token: "old".into(),
                at: now.checked_sub(Duration::from_secs(120)).unwrap_or(now),
                until: now + Duration::from_secs(3600),
            }),
            ..Default::default()
        };
        let error = item(&cx, Kind::Track, "7").await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<RetryLater>()
                .map(|r| (r.class, r.seconds)),
            Some(("tidal_auth", 60))
        );
        let attributes = item(&cx, Kind::Track, "7").await.unwrap().unwrap();
        assert_eq!(attributes["isrc"], "DEQ121641300");
        assert_eq!(
            *transport.items.lock().unwrap(),
            ["Bearer old", "Bearer new"]
        );
    }
    #[test]
    fn only_tidal_albums_and_tracks_apply() {
        let kind = |uri: &str| music::parse(uri).and_then(|l| kind_of(&l));
        assert_eq!(kind("https://tidal.com/album/199574241"), Some(Kind::Album));
        assert_eq!(
            kind("https://listen.tidal.com/track/199574242"),
            Some(Kind::Track)
        );
        assert_eq!(kind("https://tidal.com/artist/8847"), None);
        assert_eq!(kind("https://tidal.com/video/1"), None);
        assert_eq!(kind("https://www.deezer.com/album/320429237"), None);
        assert_eq!(
            Kind::Album.normal("196874054246").as_deref(),
            Some("0196874054246")
        );
        assert_eq!(
            Kind::Track.normal("deq121641300").as_deref(),
            Some("DEQ121641300")
        );
    }
}
