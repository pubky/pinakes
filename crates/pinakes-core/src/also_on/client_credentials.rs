//! An API asked with an operator's app credentials through the OAuth client-credentials grant
//! (TIDAL's, Spotify's): one slot per service and credential pair, holding its token, a refusal
//! of the pair (five minutes) and a wait the service asked for; one request at a time per slot.
//! Every request is `sensitive` (no credential in an error or a log), answers are cached by the
//! pair (never by the token), and a 401 or 403 is never cached.
use super::Context;
use crate::resolver::{egress::FetchOptions, RetryLater};
use anyhow::{Context as _, Result};
use base64::Engine;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

/// One service's API: how it is named in logs and metrics, where its tokens come from, and the
/// classes a refusal and a wait are reported as.
pub(crate) struct Api {
    /// The source's ID (`tidal`), for its metrics.
    pub service: &'static str,
    /// Its name in logs (`TIDAL`).
    pub label: &'static str,
    /// Its token endpoint.
    pub auth: &'static str,
    /// The class a refusal of the credentials is retried as (`tidal_auth`).
    pub auth_class: &'static str,
    /// The class a wait the service asked for is retried as (`tidal_busy`).
    pub busy_class: &'static str,
}
impl Api {
    /// A refusal of the credentials: the lookup retries later and keeps what the service gave
    /// before, as nothing else answers for its source.
    pub(crate) fn refused(&self) -> anyhow::Error {
        RetryLater {
            class: self.auth_class,
            seconds: REFUSAL_HOLD,
        }
        .into()
    }
    /// The service asked to wait `seconds` (held to a minute to an hour).
    pub(crate) fn busy(&self, seconds: u64) -> anyhow::Error {
        RetryLater {
            class: self.busy_class,
            seconds: seconds.clamp(60, 3600),
        }
        .into()
    }
}
/// How long refused credentials are held before the service is asked again, and the wait a
/// refused lookup reports: one value, so the retry comes when the hold ends.
pub(crate) const REFUSAL_HOLD: u64 = 300;
/// The wait the service's API answered with, when `error` is one (the egress turns a 429 and its
/// `Retry-After` into `RetryLater`, class `http_4xx`); the egress's own pacing is not the
/// service's.
pub(crate) fn asked_wait(error: &anyhow::Error) -> Option<u64> {
    error
        .downcast_ref::<RetryLater>()
        .filter(|r| r.class == "http_4xx")
        .map(|r| r.seconds)
}

/// What is kept per service and credential pair.
#[derive(Default)]
pub(crate) struct Slot {
    /// The token in hand.
    pub(crate) token: Option<Issued>,
    /// Until when a refusal of the credentials holds: no token request before.
    pub(crate) refused_until: Option<Instant>,
    /// Until when the service asked to wait: no request before, batched or single.
    pub(crate) busy_until: Option<Instant>,
}
#[derive(Clone)]
pub(crate) struct Issued {
    pub(crate) token: String,
    pub(crate) at: Instant,
    pub(crate) until: Instant,
}
impl Slot {
    /// The credentials are refused (`class` says how): logged, counted, and kept five minutes.
    pub(crate) fn refuse(&mut self, api: &Api, class: &'static str, status: u16) {
        tracing::warn!(
            status,
            class,
            "{} refused the client credentials",
            api.label
        );
        crate::telemetry::also_on_service_error(api.service, class);
        self.token = None;
        self.refused_until = Some(Instant::now() + Duration::from_secs(REFUSAL_HOLD));
    }
    /// The service asked to wait: no request before, and a shorter wait never cuts a longer one.
    pub(crate) fn cool_down(&mut self, seconds: u64) {
        let until = Instant::now() + Duration::from_secs(seconds.clamp(60, 3600));
        self.busy_until = Some(self.busy_until.map_or(until, |b| b.max(until)));
    }
    /// `Err` while a wait the service asked for holds.
    pub(crate) fn unless_busy(&self, api: &Api) -> Result<()> {
        let now = Instant::now();
        match self.busy_until {
            Some(until) if until > now => {
                Err(api.busy(until.saturating_duration_since(now).as_secs()))
            }
            _ => Ok(()),
        }
    }
    /// An error a request answered with: a wait the service asked for is kept for every request.
    pub(crate) fn answered(&mut self, api: &Api, error: anyhow::Error) -> anyhow::Error {
        match asked_wait(&error) {
            Some(seconds) => {
                self.cool_down(seconds);
                api.busy(seconds)
            }
            // A failure the cache replays, while the service's wait holds, is that wait.
            None if error
                .downcast_ref::<RetryLater>()
                .is_some_and(|r| r.class == "discovery_cached") =>
            {
                self.unless_busy(api).err().unwrap_or(error)
            }
            None => error,
        }
    }
    /// The service rejected `token` (401). It is dropped when it is still the one in hand (a
    /// lookup that held an older one changes nothing), and the credentials count as refused when
    /// it was issued under a minute ago, so a token the service keeps rejecting costs no request
    /// per lookup.
    pub(crate) fn reject(&mut self, api: &Api, token: &str) -> anyhow::Error {
        if let Some(issued) = &self.token {
            if issued.token == token {
                let fresh = issued.at.elapsed() < Duration::from_secs(60);
                self.token = None;
                if fresh {
                    self.refuse(api, "rejected", 401);
                    return api.refused();
                }
            }
        }
        RetryLater {
            class: api.auth_class,
            seconds: 60,
        }
        .into()
    }
    /// The token in hand until a minute before it expires (the service's `expires_in`), else a
    /// new one.
    pub(crate) async fn token(
        &mut self,
        api: &Api,
        cx: &Context,
        id: &str,
        secret: &str,
    ) -> Result<String> {
        let now = Instant::now();
        if self.refused_until.is_some_and(|until| until > now) {
            return Err(api.refused());
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
        let fetched = match cx.egress.fetch_with(api.auth, &options).await {
            Ok(fetched) => fetched,
            Err(error) => return Err(self.answered(api, error)),
        };
        match fetched.status {
            200 => {
                let v: Value = serde_json::from_slice(&fetched.body).context("token answer")?;
                let token = v["access_token"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .context("token answer without access_token")?
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
                Err(api.busy(fetched.retry_after_secs))
            }
            status => {
                self.refuse(api, "refused", status);
                Err(api.refused())
            }
        }
    }
}
/// One slot per service and credential pair, each behind its own lock, and one request at a time
/// per slot: a lookup holds its slot from the cooldown check to the answer, so one admitted
/// before a 429 lands waits here and then sees the wait, instead of sending under host pacing
/// alone. Another slot (another service, another pair) is never held up by it: the map's own
/// lock is held only to find or create a slot.
static SLOTS: LazyLock<std::sync::Mutex<HashMap<[u8; 32], SlotCell>>> =
    LazyLock::new(Default::default);
/// One slot's own lock.
type SlotCell = Arc<tokio::sync::Mutex<Slot>>;
/// Lookups waiting for a slot right now: a test's service answers one of its own lookups once
/// the next is pending, and no other slot's lookup counts.
static WAITING: LazyLock<std::sync::Mutex<HashMap<[u8; 32], usize>>> =
    LazyLock::new(Default::default);
/// How many lookups wait for `slot` now: a test hook (`tidal::lookups_waiting`).
#[doc(hidden)]
pub fn waiting(slot: &[u8; 32]) -> usize {
    WAITING.lock().unwrap().get(slot).copied().unwrap_or(0)
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
/// The slot `key` names, once this lookup's turn at it comes.
pub(crate) async fn slot(key: &[u8; 32]) -> tokio::sync::OwnedMutexGuard<Slot> {
    let _pending = Pending::new(key);
    let cell = SLOTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(*key)
        .or_default()
        .clone();
    cell.lock_owned().await
}
/// A key of this process's own, so a cache tag is never the pair's plain hash (an offline check
/// of a guessed secret).
static TAG_KEY: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let mut key = [0; 32];
    getrandom::fill(&mut key).expect("entropy for the client-credentials cache tag");
    key
});
/// The cache tag of a slot: keyed by `TAG_KEY`, stable for the process.
pub(crate) fn cache_tag(slot: &[u8; 32]) -> String {
    blake3::keyed_hash(&TAG_KEY, slot).to_hex().to_string()
}
/// The slot of a service's credential pair.
pub(crate) fn slot_key(api: &Api, id: &str, secret: &str) -> [u8; 32] {
    *blake3::hash(format!("{}\n{id}\n{secret}", api.service).as_bytes()).as_bytes()
}
/// How the API is asked: with the token, its answers (typed `accept`) cached by the pair.
pub(crate) fn api_options(
    token: &str,
    slot: &[u8; 32],
    accept: &'static str,
    extra_types: &'static [&'static str],
) -> FetchOptions {
    FetchOptions {
        accept: Some(accept.into()),
        extra_types,
        headers: vec![("authorization", format!("Bearer {token}"))],
        // Answers are cached by the credential pair, not the token: a new token keeps them; an
        // authentication failure is the token's, never cached.
        cache_tag: Some(cache_tag(slot)),
        uncached: &[401, 403],
        sensitive: true,
        ..Default::default()
    }
}
/// One API object's JSON, its answer shared for an hour; `None` when the service has none
/// (404). A rejected token, refused credentials, a wait asked for and an outage are errors the
/// lookup retries.
pub(crate) async fn object(
    api: &Api,
    cx: &Context,
    (id, secret): (&str, &str),
    url: &str,
    (accept, extra_types): (&'static str, &'static [&'static str]),
) -> Result<Option<Value>> {
    let slot = slot_key(api, id, secret);
    let mut s = self::slot(&slot).await;
    s.unless_busy(api)?;
    let token = s.token(api, cx, id, secret).await?;
    let options = api_options(&token, &slot, accept, extra_types);
    let fetched = match cx.egress.discover(url, &options, 3600).await {
        Ok(fetched) => fetched,
        Err(error) => return Err(s.answered(api, error)),
    };
    let shorten = |seconds: u64| cx.egress.shorten_discovery(url, &options, seconds);
    match fetched.status {
        200 => {}
        // Never cached (`api_options`): the next request asks with its own token.
        401 => return Err(s.reject(api, &token)),
        // No catalogue scope, a restricted app: the credentials, not the object.
        403 => {
            s.refuse(api, "forbidden", 403);
            return Err(api.refused());
        }
        404 => return Ok(None),
        429 | 500..=599 => {
            shorten(fetched.retry_after_secs.clamp(60, 3600)).await?;
            s.cool_down(fetched.retry_after_secs);
            return Err(api.busy(fetched.retry_after_secs));
        }
        status => {
            shorten(300).await?;
            anyhow::bail!("{} answered {status}", api.label);
        }
    }
    drop(s);
    Ok(Some(
        serde_json::from_slice(&fetched.body).with_context(|| format!("{} answer", api.label))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const API: Api = Api {
        service: "unit",
        label: "Unit",
        auth: "https://auth.unit.example/token",
        auth_class: "unit_auth",
        busy_class: "unit_busy",
    };
    fn class_and_seconds(error: &anyhow::Error) -> (&'static str, u64) {
        let r = error.downcast_ref::<RetryLater>().unwrap();
        (r.class, r.seconds)
    }
    async fn unless_busy(key: &[u8; 32]) -> Result<()> {
        slot(key).await.unless_busy(&API)
    }
    async fn cool_down(key: &[u8; 32], seconds: u64) {
        slot(key).await.cool_down(seconds);
    }
    async fn reject(key: &[u8; 32], token: &str) -> anyhow::Error {
        slot(key).await.reject(&API, token)
    }

    #[tokio::test]
    async fn a_stale_rejection_keeps_a_fresh_token_and_a_fresh_one_counts_as_refused() {
        let slot = slot_key(&API, "unit-id", "unit-secret");
        let now = Instant::now();
        let issued = |token: &str, at: Instant| Issued {
            token: token.into(),
            at,
            until: now + Duration::from_secs(3600),
        };
        let two_minutes_ago = now.checked_sub(Duration::from_secs(120)).unwrap_or(now);
        *self::slot(&slot).await = Slot {
            token: Some(issued("new", two_minutes_ago)),
            ..Default::default()
        };
        // A lookup that held an older token: the new one stays.
        assert_eq!(
            class_and_seconds(&reject(&slot, "old").await),
            ("unit_auth", 60)
        );
        assert_eq!(
            self::slot(&slot)
                .await
                .token
                .as_ref()
                .map(|i| i.token.as_str()),
            Some("new")
        );
        // The token in hand, issued two minutes ago: dropped, the next lookup asks for one.
        assert_eq!(
            class_and_seconds(&reject(&slot, "new").await),
            ("unit_auth", 60)
        );
        {
            // A scope of its own: the guard would otherwise hold the lock past the next one.
            let s = self::slot(&slot).await;
            assert!(s.token.is_none() && s.refused_until.is_none());
        }
        // One issued just now and rejected: the credentials are refused for five minutes.
        self::slot(&slot).await.token = Some(issued("fresh", now));
        assert_eq!(
            class_and_seconds(&reject(&slot, "fresh").await),
            ("unit_auth", REFUSAL_HOLD)
        );
        let s = self::slot(&slot).await;
        assert!(s.token.is_none() && s.refused_until.is_some_and(|u| u > now));
    }
    #[tokio::test]
    async fn a_wait_the_service_asked_for_holds_every_request() {
        let slot = slot_key(&API, "busy-id", "busy-secret");
        assert!(unless_busy(&slot).await.is_ok());
        cool_down(&slot, 3600).await;
        let (class, seconds) = class_and_seconds(&unless_busy(&slot).await.unwrap_err());
        assert_eq!(class, "unit_busy");
        assert!((3500..=3600).contains(&seconds), "{seconds}");
        // A shorter wait never cuts a longer one; a wait is at least a minute.
        cool_down(&slot, 5).await;
        assert!(class_and_seconds(&unless_busy(&slot).await.unwrap_err()).1 > 3000);
        let other = slot_key(&API, "other-id", "other-secret");
        cool_down(&other, 5).await;
        assert_eq!(
            class_and_seconds(&unless_busy(&other).await.unwrap_err()).1,
            60
        );
        // The egress's own pacing is not the service's wait; the service's 429 is.
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
    /// A request held open at one service (Spotify, slow to answer) holds its own slot only:
    /// another service's lookup (TIDAL) completes meanwhile (review of 18e57ab: one lock over
    /// every slot stopped TIDAL while a Spotify request was pending).
    #[tokio::test]
    async fn a_slow_request_holds_only_its_own_slot() {
        use crate::resolver::egress::{Dns, Egress, Fetched, Transport};
        use std::net::SocketAddr;
        struct Public;
        #[async_trait::async_trait]
        impl Dns for Public {
            async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
                Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
            }
        }
        /// Answers every token and API request at once, but `slow.example`'s API request only
        /// once released.
        #[derive(Default)]
        struct Held {
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
        }
        #[async_trait::async_trait]
        impl Transport for Held {
            async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> Result<Fetched> {
                self.get_with(url, a, &FetchOptions::default()).await
            }
            async fn get_with(
                &self,
                url: &url::Url,
                _: &[SocketAddr],
                _: &FetchOptions,
            ) -> Result<Fetched> {
                let token = url.path() == "/token";
                if url.host_str() == Some("slow.example") && !token {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                let body = if token {
                    r#"{"access_token":"t","expires_in":3600}"#
                } else {
                    r#"{"id":"1"}"#
                };
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    validators: Default::default(),
                    url: url.clone(),
                    content_type: "application/json".into(),
                    status: 200,
                    retry_after_secs: 60,
                    location: None,
                    body: body.as_bytes().to_vec(),
                })
            }
        }
        static SLOW: Api = Api {
            service: "slow",
            label: "Slow",
            auth: "https://slow.example/token",
            auth_class: "slow_auth",
            busy_class: "slow_busy",
        };
        static FAST: Api = Api {
            service: "fast",
            label: "Fast",
            auth: "https://fast.example/token",
            auth_class: "fast_auth",
            busy_class: "fast_busy",
        };
        let held = Arc::new(Held::default());
        let cx = Arc::new(Context {
            egress: Egress::with_backends(Default::default(), Arc::new(Public), held.clone()),
            config: Arc::new(crate::config::Config::default()),
        });
        let json = ("application/json", &[] as &[&str]);
        let pending = {
            let cx = cx.clone();
            tokio::spawn(async move {
                object(
                    &SLOW,
                    &cx,
                    ("slow-id", "slow-secret"),
                    "https://slow.example/api/1",
                    json,
                )
                .await
            })
        };
        // The slow service's request is in flight, its slot held.
        held.entered.notified().await;
        let fast = tokio::time::timeout(
            Duration::from_secs(5),
            object(
                &FAST,
                &cx,
                ("fast-id", "fast-secret"),
                "https://fast.example/api/1",
                json,
            ),
        )
        .await
        .expect("a lookup at another service waited for the slow one's slot")
        .unwrap();
        assert_eq!(fast.unwrap()["id"], "1");
        held.release.notify_one();
        assert!(pending.await.unwrap().unwrap().is_some());
    }
    #[test]
    fn a_cache_tag_is_keyed_and_stable_and_slots_are_per_service() {
        let slot = slot_key(&API, "tag-id", "tag-secret");
        let tag = cache_tag(&slot);
        assert_eq!(tag, cache_tag(&slot));
        assert_ne!(tag, blake3::Hash::from(slot).to_hex().to_string());
        assert_ne!(tag, cache_tag(&slot_key(&API, "tag-id", "other-secret")));
        // The same pair at another service is another slot.
        let other = Api {
            service: "other",
            ..API
        };
        assert_ne!(slot, slot_key(&other, "tag-id", "tag-secret"));
        // The refusal's reported wait is the hold.
        assert_eq!(
            API.refused()
                .downcast_ref::<RetryLater>()
                .map(|r| (r.class, r.seconds)),
            Some(("unit_auth", REFUSAL_HOLD))
        );
    }
}
