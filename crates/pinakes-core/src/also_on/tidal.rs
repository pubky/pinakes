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
/// How the API is asked: with the client-credentials token, its answers cached by the pair.
fn api_options(token: &str, slot: &[u8; 32]) -> FetchOptions {
    FetchOptions {
        accept: Some(JSON_API.into()),
        extra_types: &[JSON_API],
        headers: vec![("authorization", format!("Bearer {token}"))],
        // Answers are cached by the credential pair, not the token: a new token keeps them; an
        // authentication failure is the token's, never cached.
        cache_tag: Some(blake3::Hash::from(*slot).to_hex().to_string()),
        uncached: &[401, 403],
        sensitive: true,
        ..Default::default()
    }
}
/// A refusal of the credentials: the lookup retries later and keeps what TIDAL gave before, as
/// nothing else answers for this source.
fn refused() -> anyhow::Error {
    RetryLater {
        class: "tidal_auth",
        seconds: 3600,
    }
    .into()
}
/// TIDAL asked to wait `seconds` (held to a minute to an hour).
fn busy(seconds: u64) -> anyhow::Error {
    RetryLater {
        class: "tidal_busy",
        seconds: seconds.clamp(60, 3600),
    }
    .into()
}
/// The wait TIDAL's API answered with, when `error` is one (the egress turns a 429 and its
/// `Retry-After` into `RetryLater`, class `http_4xx`); its own pacing is not TIDAL's.
fn asked_wait(error: &anyhow::Error) -> Option<u64> {
    error
        .downcast_ref::<RetryLater>()
        .filter(|r| r.class == "http_4xx")
        .map(|r| r.seconds)
}

/// What is kept per credential pair.
#[derive(Default)]
struct Slot {
    /// The token in hand.
    token: Option<Issued>,
    /// Until when a refusal of the credentials holds: no token request before.
    refused_until: Option<Instant>,
    /// Until when TIDAL asked to wait: no request before, batched or single.
    busy_until: Option<Instant>,
}
#[derive(Clone)]
struct Issued {
    token: String,
    at: Instant,
    until: Instant,
}
impl Slot {
    /// The credentials are refused (`class` says how): logged, counted, and kept five minutes.
    fn refuse(&mut self, class: &'static str, status: u16) {
        tracing::warn!(status, class, "TIDAL refused the client credentials");
        crate::telemetry::also_on_service_error("tidal", class);
        self.token = None;
        self.refused_until = Some(Instant::now() + Duration::from_secs(300));
    }
    /// TIDAL asked to wait: no request before, and a shorter wait never cuts a longer one.
    fn cool_down(&mut self, seconds: u64) {
        let until = Instant::now() + Duration::from_secs(seconds.clamp(60, 3600));
        self.busy_until = Some(self.busy_until.map_or(until, |b| b.max(until)));
    }
    /// `Err` while a wait TIDAL asked for holds.
    fn unless_busy(&self) -> Result<()> {
        let now = Instant::now();
        match self.busy_until {
            Some(until) if until > now => Err(busy(until.saturating_duration_since(now).as_secs())),
            _ => Ok(()),
        }
    }
    /// An error a TIDAL request answered with: a wait TIDAL asked for is kept for every request.
    fn answered(&mut self, error: anyhow::Error) -> anyhow::Error {
        match asked_wait(&error) {
            Some(seconds) => {
                self.cool_down(seconds);
                busy(seconds)
            }
            // A failure the cache replays, while TIDAL's wait holds, is that wait.
            None if error
                .downcast_ref::<RetryLater>()
                .is_some_and(|r| r.class == "discovery_cached") =>
            {
                self.unless_busy().err().unwrap_or(error)
            }
            None => error,
        }
    }
    /// TIDAL rejected `token` (401). It is dropped when it is still the one in hand (a lookup
    /// that held an older one changes nothing), and the credentials count as refused when it was
    /// issued under a minute ago, so a token TIDAL keeps rejecting costs no request per lookup.
    fn reject(&mut self, token: &str) -> anyhow::Error {
        if let Some(issued) = &self.token {
            if issued.token == token {
                let fresh = issued.at.elapsed() < Duration::from_secs(60);
                self.token = None;
                if fresh {
                    self.refuse("rejected", 401);
                    return refused();
                }
            }
        }
        RetryLater {
            class: "tidal_auth",
            seconds: 60,
        }
        .into()
    }
    /// The token in hand until a minute before it expires (TIDAL's `expires_in`), else a new one.
    async fn token(&mut self, cx: &Context, id: &str, secret: &str) -> Result<String> {
        let now = Instant::now();
        if self.refused_until.is_some_and(|until| until > now) {
            return Err(refused());
        }
        if let Some(issued) = &self.token {
            if issued.until > now {
                return Ok(issued.token.clone());
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
        let fetched = match cx.egress.fetch_with(AUTH, &options).await {
            Ok(fetched) => fetched,
            Err(error) => return Err(self.answered(error)),
        };
        match fetched.status {
            200 => {
                let v: Value = serde_json::from_slice(&fetched.body).context("TIDAL token")?;
                let token = v["access_token"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .context("TIDAL token without access_token")?
                    .to_owned();
                let lifetime = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
                self.token = Some(Issued {
                    token: token.clone(),
                    at: now,
                    until: now + Duration::from_secs(lifetime.max(60)),
                });
                Ok(token)
            }
            429 | 500..=599 => {
                self.cool_down(fetched.retry_after_secs);
                Err(busy(fetched.retry_after_secs))
            }
            status => {
                self.refuse("refused", status);
                Err(refused())
            }
        }
    }
}
/// One slot per credential pair, and one TIDAL request at a time per pair: a lookup holds its
/// slot from the cooldown check to the answer, so one admitted before a 429 lands waits here and
/// then sees the wait, instead of sending under host pacing alone.
static SLOTS: LazyLock<tokio::sync::Mutex<HashMap<[u8; 32], Slot>>> =
    LazyLock::new(Default::default);
/// Lookups waiting for a pair's slot right now: a test's TIDAL answers one of its own lookups
/// once the next is pending, and no other pair's lookup counts.
static WAITING: LazyLock<std::sync::Mutex<HashMap<[u8; 32], usize>>> =
    LazyLock::new(Default::default);
#[doc(hidden)]
pub fn lookups_waiting(id: &str, secret: &str) -> usize {
    WAITING
        .lock()
        .unwrap()
        .get(&slot_key(id, secret))
        .copied()
        .unwrap_or(0)
}
/// A lookup counted as pending at a slot for as long as it lives, whether its turn comes or its
/// future is dropped while it waits.
struct Pending([u8; 32]);
impl Pending {
    fn new(slot: &[u8; 32]) -> Self {
        *WAITING.lock().unwrap().entry(*slot).or_default() += 1;
        Self(*slot)
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        let mut waiting = WAITING.lock().unwrap();
        if let Some(n) = waiting.get_mut(&self.0) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                waiting.remove(&self.0);
            }
        }
    }
}
/// The slots, once this lookup's turn at `slot` comes.
async fn slots(slot: &[u8; 32]) -> tokio::sync::MutexGuard<'static, HashMap<[u8; 32], Slot>> {
    let _pending = Pending::new(slot);
    SLOTS.lock().await
}
fn slot_key(id: &str, secret: &str) -> [u8; 32] {
    *blake3::hash(format!("{id}\n{secret}").as_bytes()).as_bytes()
}
/// The configured pair and its slot key.
fn pair_of(cx: &Context) -> Result<(&str, &str, [u8; 32])> {
    let (id, secret) = cx.config.tidal.pair().context("no TIDAL credentials")?;
    Ok((id, secret, slot_key(id, secret)))
}
// The slot rules on their own, for the unit tests.
#[cfg(test)]
async fn unless_busy(slot: &[u8; 32]) -> Result<()> {
    SLOTS.lock().await.entry(*slot).or_default().unless_busy()
}
#[cfg(test)]
async fn cool_down(slot: &[u8; 32], seconds: u64) {
    SLOTS
        .lock()
        .await
        .entry(*slot)
        .or_default()
        .cool_down(seconds);
}
#[cfg(test)]
async fn reject(slot: &[u8; 32], token: &str) -> anyhow::Error {
    SLOTS.lock().await.entry(*slot).or_default().reject(token)
}

/// The attributes of one album or track; `None` when TIDAL has no such item in the country.
async fn item(cx: &Context, kind: Kind, id: &str) -> Result<Option<Value>> {
    let (client_id, secret, slot) = pair_of(cx)?;
    let mut slots = slots(&slot).await;
    let s = slots.entry(slot).or_default();
    s.unless_busy()?;
    let token = s.token(cx, client_id, secret).await?;
    let url = item_url(cx, kind, id);
    let options = api_options(&token, &slot);
    let fetched = match cx.egress.discover(&url, &options, 3600).await {
        Ok(fetched) => fetched,
        Err(error) => return Err(s.answered(error)),
    };
    let shorten = |seconds: u64| cx.egress.shorten_discovery(&url, &options, seconds);
    match fetched.status {
        200 => {}
        // Never cached (`api_options`): the next request asks with its own token.
        401 => return Err(s.reject(&token)),
        // No catalogue scope, a restricted app: the credentials, not the item.
        403 => {
            s.refuse("forbidden", 403);
            return Err(refused());
        }
        404 => return Ok(None),
        429 | 500..=599 => {
            shorten(fetched.retry_after_secs.clamp(60, 3600)).await?;
            s.cool_down(fetched.retry_after_secs);
            return Err(busy(fetched.retry_after_secs));
        }
        status => {
            shorten(300).await?;
            anyhow::bail!("TIDAL answered {status}");
        }
    }
    drop(slots);
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
    let (client_id, secret, slot) = pair_of(cx)?;
    let mut slots = slots(&slot).await;
    let s = slots.entry(slot).or_default();
    s.unless_busy()?;
    let token = s.token(cx, client_id, secret).await?;
    // A batch asks only for what its items don't have cached (a retry's, within the hour).
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
        cx.config.tidal.country
    );
    let options = api_options(&token, &slot);
    let fetched = match cx.egress.fetch_with(&url, &options).await {
        Ok(fetched) => fetched,
        Err(error) => return Err(s.answered(error)),
    };
    match fetched.status {
        200 => {}
        401 => return Err(s.reject(&token)),
        403 => {
            s.refuse("forbidden", 403);
            return Err(refused());
        }
        429 | 500..=599 => {
            s.cool_down(fetched.retry_after_secs);
            return Err(busy(fetched.retry_after_secs));
        }
        status => anyhow::bail!("TIDAL batch answered {status}"),
    }
    drop(slots);
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

    fn class_and_seconds(error: &anyhow::Error) -> (&'static str, u64) {
        let r = error.downcast_ref::<RetryLater>().unwrap();
        (r.class, r.seconds)
    }
    #[tokio::test]
    async fn a_stale_rejection_keeps_a_fresh_token_and_a_fresh_one_counts_as_refused() {
        let slot = slot_key("unit-id", "unit-secret");
        let now = Instant::now();
        let issued = |token: &str, at: Instant| Issued {
            token: token.into(),
            at,
            until: now + Duration::from_secs(3600),
        };
        let two_minutes_ago = now.checked_sub(Duration::from_secs(120)).unwrap_or(now);
        SLOTS.lock().await.insert(
            slot,
            Slot {
                token: Some(issued("new", two_minutes_ago)),
                ..Default::default()
            },
        );
        // A lookup that held an older token: the new one stays.
        assert_eq!(
            class_and_seconds(&reject(&slot, "old").await),
            ("tidal_auth", 60)
        );
        assert_eq!(
            SLOTS.lock().await[&slot]
                .token
                .as_ref()
                .map(|i| i.token.as_str()),
            Some("new")
        );
        // The token in hand, issued two minutes ago: dropped, the next lookup asks for one.
        assert_eq!(
            class_and_seconds(&reject(&slot, "new").await),
            ("tidal_auth", 60)
        );
        {
            // A scope of its own: the guard would otherwise hold the lock past the next one.
            let slots = SLOTS.lock().await;
            let s = &slots[&slot];
            assert!(s.token.is_none() && s.refused_until.is_none());
        }
        // One issued just now and rejected: the credentials are refused for five minutes.
        SLOTS.lock().await.get_mut(&slot).unwrap().token = Some(issued("fresh", now));
        assert_eq!(
            class_and_seconds(&reject(&slot, "fresh").await),
            ("tidal_auth", 3600)
        );
        let slots = SLOTS.lock().await;
        let s = &slots[&slot];
        assert!(s.token.is_none() && s.refused_until.is_some_and(|u| u > now));
    }
    #[tokio::test]
    async fn a_wait_tidal_asked_for_holds_every_request() {
        let slot = slot_key("busy-id", "busy-secret");
        assert!(unless_busy(&slot).await.is_ok());
        cool_down(&slot, 3600).await;
        let (class, seconds) = class_and_seconds(&unless_busy(&slot).await.unwrap_err());
        assert_eq!(class, "tidal_busy");
        assert!((3500..=3600).contains(&seconds), "{seconds}");
        // A shorter wait never cuts a longer one; a wait is at least a minute.
        cool_down(&slot, 5).await;
        assert!(class_and_seconds(&unless_busy(&slot).await.unwrap_err()).1 > 3000);
        let other = slot_key("other-id", "other-secret");
        cool_down(&other, 5).await;
        assert_eq!(
            class_and_seconds(&unless_busy(&other).await.unwrap_err()).1,
            60
        );
        // The egress's own pacing is not TIDAL's wait; TIDAL's 429 is.
        let paced: anyhow::Error = RetryLater {
            class: "rate_limited",
            seconds: 7,
        }
        .into();
        assert_eq!(asked_wait(&paced), None);
        let limited: anyhow::Error = RetryLater {
            class: "http_4xx",
            seconds: 900,
        }
        .into();
        assert_eq!(asked_wait(&limited), Some(900));
    }
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
                let reply = |status: u16, body: &str| Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    validators: Default::default(),
                    url: url.clone(),
                    content_type: JSON_API.into(),
                    status,
                    retry_after_secs: 60,
                    location: None,
                    body: body.as_bytes().to_vec(),
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
        config.tidal.client_id = Some(SecretKey::from("rotate-id".to_owned()));
        config.tidal.client_secret = Some(SecretKey::from("rotate-secret".to_owned()));
        let cx = Context {
            egress: Egress::with_backends(
                Default::default(),
                std::sync::Arc::new(Public),
                transport.clone(),
            ),
            config: std::sync::Arc::new(config),
        };
        // A token issued two minutes ago, which TIDAL now rejects.
        let slot = slot_key("rotate-id", "rotate-secret");
        let now = Instant::now();
        SLOTS.lock().await.insert(
            slot,
            Slot {
                token: Some(Issued {
                    token: "old".into(),
                    at: now.checked_sub(Duration::from_secs(120)).unwrap_or(now),
                    until: now + Duration::from_secs(3600),
                }),
                ..Default::default()
            },
        );
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
