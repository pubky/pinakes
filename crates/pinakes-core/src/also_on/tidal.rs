//! TIDAL's own record of its own album or track, from TIDAL's developer API: an album's barcode
//! (`barcodeId`) as the key `upc:…` and a track's ISRC (`isrc`) as `isrc:…`, so the lookups by
//! barcode and ISRC after it (Deezer, iTunes) join the rest. An ISRC MusicBrainz gives to several
//! recordings is a fact only, as a track page's own ISRC is (`music_sources::shared_isrc`).
//!
//! Asked only with an operator's client ID and secret (`[tidal]`, or `PINAKES_TIDAL_CLIENT_ID`
//! and `PINAKES_TIDAL_CLIENT_SECRET`), through the client-credentials grant. A batch of links asks
//! for up to 20 albums or tracks at once (`filter[id]`).
use super::{Context, Fact, Found, Key, Source, Subject};
use crate::{
    event::adapters::SecretKey,
    music::{self, Level, Service},
    resolver::{
        egress::{FetchOptions, Fetched},
        RetryLater,
    },
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    sync::LazyLock,
    time::{Duration, Instant},
};

/// TIDAL's developer API credentials (`[tidal] client_id` and `client_secret`, or
/// `PINAKES_TIDAL_CLIENT_ID` and `PINAKES_TIDAL_CLIENT_SECRET`) and the country its catalogue is
/// read in. Without both, TIDAL is never asked.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TidalConfig {
    pub client_id: Option<SecretKey>,
    pub client_secret: Option<SecretKey>,
    /// An ISO 3166-1 alpha-2 code in upper case, `US` by default.
    pub country: String,
}
impl Default for TidalConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            country: "US".into(),
        }
    }
}
impl TidalConfig {
    fn pair(&self) -> Option<(&str, &str)> {
        match (self.client_id.as_deref(), self.client_secret.as_deref()) {
            (Some(id), Some(secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => {
                Some((id.trim(), secret.trim()))
            }
            _ => None,
        }
    }
}

const AUTH: &str = "https://auth.tidal.com/v1/oauth2/token";
const API: &str = "https://openapi.tidal.com/v2";
/// The most IDs one `filter[id]` request takes.
const BATCH: usize = 20;
/// TIDAL's JSON:API answers.
const JSON_API: &str = "application/vnd.api+json";

/// An album or a track, as the API names its collection, the attribute read and the key's
/// namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Album,
    Track,
}
impl Kind {
    fn of(link: &music::Link) -> Option<Self> {
        match (link.service, link.level) {
            (Service::Tidal, Level::Album) => Some(Self::Album),
            (Service::Tidal, Level::Track) => Some(Self::Track),
            _ => None,
        }
    }
    fn collection(self) -> &'static str {
        match self {
            Self::Album => "albums",
            Self::Track => "tracks",
        }
    }
    fn attribute(self) -> &'static str {
        match self {
            Self::Album => "barcodeId",
            Self::Track => "isrc",
        }
    }
    fn namespace(self) -> &'static str {
        match self {
            Self::Album => "upc",
            Self::Track => "isrc",
        }
    }
    fn normal(self, value: &str) -> Option<String> {
        match self {
            Self::Album => music::gtin13(value),
            Self::Track => music::isrc(value),
        }
    }
}

pub struct Tidal;
#[async_trait]
impl Source for Tidal {
    fn id(&self) -> &'static str {
        "tidal"
    }
    fn ready(&self, config: &crate::config::Config) -> bool {
        config.tidal.pair().is_some()
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| Kind::of(&l).is_some())
    }
    async fn prefetch(&self, cx: &Context, uris: &[String]) {
        let wanted: BTreeSet<(Kind, String)> = uris
            .iter()
            .filter_map(|u| music::parse(u))
            .filter_map(|l| Some((Kind::of(&l)?, l.id)))
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
                    tracing::debug!(error = %format!("{error:#}"), "batched TIDAL lookup failed; links ask alone");
                    break;
                }
            }
        }
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some((kind, link)) = music::parse(&subject.uri).and_then(|l| Some((Kind::of(&l)?, l)))
        else {
            return Ok(Found::default());
        };
        let mut found = Found::default();
        let Some(attributes) = item(cx, kind, &link.id).await? else {
            return Ok(found);
        };
        let Some(value) = attributes[kind.attribute()]
            .as_str()
            .and_then(|v| kind.normal(v))
        else {
            return Ok(found);
        };
        let namespace = kind.namespace();
        if kind == Kind::Album || !crate::music_sources::shared_isrc(cx, &value).await? {
            found.keys.push(Key {
                key: format!("{namespace}:{value}"),
                via: "tidal".into(),
            });
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
        cx.config.tidal.country
    )
}
/// How the API is asked: with the client-credentials token. The token's hash keys the cache, so
/// a new token asks again.
fn api_options(token: &str) -> FetchOptions {
    FetchOptions {
        accept: Some(JSON_API.into()),
        extra_types: &[JSON_API],
        headers: vec![("authorization", format!("Bearer {token}"))],
        sensitive: true,
        ..Default::default()
    }
}
fn busy(fetched: &Fetched) -> anyhow::Error {
    RetryLater {
        class: "tidal_busy",
        seconds: fetched.retry_after_secs.clamp(60, 3600),
    }
    .into()
}

/// The attributes of one album or track; `None` when TIDAL has no such item in the country.
async fn item(cx: &Context, kind: Kind, id: &str) -> Result<Option<Value>> {
    let token = token(cx).await?;
    let url = item_url(cx, kind, id);
    let options = api_options(&token);
    let fetched = cx.egress.discover(&url, &options, 3600).await?;
    match fetched.status {
        200 => {}
        401 => {
            // An expired or revoked token: the next lookup asks for a new one.
            forget_token(cx).await;
            cx.egress.shorten_discovery(&url, &options, 1).await?;
            return Err(RetryLater {
                class: "tidal_auth",
                seconds: 60,
            }
            .into());
        }
        429 | 500..=599 => {
            let error = busy(&fetched);
            cx.egress
                .shorten_discovery(&url, &options, fetched.retry_after_secs.clamp(60, 3600))
                .await?;
            return Err(error);
        }
        _ => return Ok(None),
    }
    let v: Value = serde_json::from_slice(&fetched.body).context("TIDAL answer")?;
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
    let token = token(cx).await?;
    let filter: String = ids
        .iter()
        .map(|id| format!("&filter%5Bid%5D={id}"))
        .collect();
    let url = format!(
        "{API}/{}?countryCode={}{filter}",
        kind.collection(),
        cx.config.tidal.country
    );
    let options = api_options(&token);
    let fetched = cx.egress.fetch_with(&url, &options).await?;
    if fetched.status != 200 {
        anyhow::bail!("TIDAL batch answered {}", fetched.status);
    }
    let v: Value = serde_json::from_slice(&fetched.body).context("TIDAL batch")?;
    let items = v["data"].as_array().context("TIDAL batch without data")?;
    // A paged answer may hold the missing items on a later page.
    let whole = v["links"]["next"].is_null();
    for id in ids {
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
        let seeded = Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url::Url::parse(&one)?,
            status: if answer.is_some() { 200 } else { 404 },
            retry_after_secs: 60,
            location: None,
            content_type: JSON_API.into(),
            body: serde_json::to_vec(&body)?,
        };
        cx.egress.seed(&one, &options, 3600, seeded).await?;
    }
    Ok(())
}

/// A client-credentials token per credential pair, kept until a minute before it expires; a
/// refusal is kept five minutes (`None`), so lookups meanwhile retry without asking.
static TOKENS: LazyLock<tokio::sync::Mutex<HashMap<[u8; 32], Token>>> =
    LazyLock::new(Default::default);
/// A token (or `None`, a refusal) and until when it holds.
type Token = (Option<String>, Instant);
fn token_slot(id: &str, secret: &str) -> [u8; 32] {
    *blake3::hash(format!("{id}\n{secret}").as_bytes()).as_bytes()
}
async fn forget_token(cx: &Context) {
    if let Some((id, secret)) = cx.config.tidal.pair() {
        TOKENS.lock().await.remove(&token_slot(id, secret));
    }
}
/// A refusal of the credentials (logged, and counted in
/// `pinakes_also_on_service_errors_total{service="tidal"}`): the lookup retries later and keeps
/// what TIDAL gave before, as nothing else answers for this source.
fn refused() -> anyhow::Error {
    RetryLater {
        class: "tidal_auth",
        seconds: 3600,
    }
    .into()
}
/// The token for the configured credentials.
async fn token(cx: &Context) -> Result<String> {
    let (id, secret) = cx.config.tidal.pair().context("no TIDAL credentials")?;
    let slot = token_slot(id, secret);
    // Held while asking: one token request at a time.
    let mut tokens = TOKENS.lock().await;
    if let Some((token, until)) = tokens.get(&slot) {
        if *until > Instant::now() {
            return token.clone().ok_or_else(refused);
        }
    }
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    let options = FetchOptions {
        accept: Some("application/json".into()),
        headers: vec![("authorization", format!("Basic {basic}"))],
        form_body: Some(vec![("grant_type", "client_credentials".into())]),
        sensitive: true,
        ..Default::default()
    };
    let fetched = cx.egress.fetch_with(AUTH, &options).await?;
    match fetched.status {
        200 => {
            let v: Value = serde_json::from_slice(&fetched.body).context("TIDAL token")?;
            let token = v["access_token"]
                .as_str()
                .filter(|t| !t.is_empty())
                .context("TIDAL token without access_token")?
                .to_owned();
            let lifetime = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
            tokens.insert(
                slot,
                (
                    Some(token.clone()),
                    Instant::now() + Duration::from_secs(lifetime.max(60)),
                ),
            );
            Ok(token)
        }
        429 | 500..=599 => Err(busy(&fetched)),
        status => {
            tracing::warn!(status, "TIDAL refused the client credentials");
            crate::telemetry::also_on_service_error("tidal", "refused");
            tokens.insert(slot, (None, Instant::now() + Duration::from_secs(300)));
            Err(refused())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_stay_secret_and_both_are_needed() {
        let config: TidalConfig = serde_json::from_value(
            serde_json::json!({"client_id": "ABCDEFID", "client_secret": "ABCDEFSECRET"}),
        )
        .unwrap();
        assert_eq!(config.pair(), Some(("ABCDEFID", "ABCDEFSECRET")));
        assert_eq!(config.country, "US");
        let shown = format!("{config:?} {}", serde_json::to_string(&config).unwrap());
        assert!(!shown.contains("ABCDEF"), "{shown}");
        let half: TidalConfig =
            serde_json::from_value(serde_json::json!({"client_id": "ABCDEFID"})).unwrap();
        assert_eq!(half.pair(), None);
    }

    #[test]
    fn only_tidal_albums_and_tracks_apply() {
        let kind = |uri: &str| music::parse(uri).and_then(|l| Kind::of(&l));
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
