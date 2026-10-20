use crate::config::EgressConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    sync::{Mutex, OwnedMutexGuard},
    time::Instant,
};
use url::{Host, Url};

#[derive(Debug)]
pub struct BodyTooLarge;
impl std::fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("body exceeds cap (pages 1 MB)")
    }
}
impl std::error::Error for BodyTooLarge {}

pub const BODY_CAP: usize = 1024 * 1024;
/// The longest a request waits for its host's next slot, queue included. It leaves the request
/// at least half of the ten-second fetch timeout.
pub const MAX_SLOT_WAIT: Duration = Duration::from_secs(5);
pub const USER_AGENT: &str = concat!(
    "pinakes/",
    env!("CARGO_PKG_VERSION"),
    " (+https://pubky.app)"
);
/// The User-Agent with the operator's contact (`http.contact`), or `USER_AGENT` without one.
/// It never names a code host: some services refuse such agents as undeclared tools.
pub fn user_agent(contact: &str) -> String {
    if contact.is_empty() {
        USER_AGENT.to_owned()
    } else {
        format!(
            "pinakes/{} (+https://pubky.app; {contact})",
            env!("CARGO_PKG_VERSION")
        )
    }
}
pub const ALLOWED_TYPES: &[&str] = &[
    "text/html",
    "application/xhtml+xml",
    "application/json",
    "application/ld+json",
    "application/activity+json",
    "application/vnd.citationstyles.csl+json",
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
    "image/avif",
    "application/pdf",
];

pub fn public_ip(ip: IpAddr) -> bool {
    static DENIED: OnceLock<Vec<ipnet::IpNet>> = OnceLock::new();
    let denied = DENIED.get_or_init(|| {
        [
            "0.0.0.0/8",
            "10.0.0.0/8",
            "100.64.0.0/10",
            "127.0.0.0/8",
            "169.254.0.0/16",
            "172.16.0.0/12",
            "192.0.0.0/24",
            "192.0.2.0/24",
            "192.88.99.0/24",
            "192.168.0.0/16",
            "198.18.0.0/15",
            "198.51.100.0/24",
            "203.0.113.0/24",
            "224.0.0.0/4",
            "240.0.0.0/4",
            "168.63.129.16/32",
            "2001::/23",
            "2001:db8::/32",
            "2002::/16",
            "3fff::/20",
        ]
        .iter()
        .map(|s| s.parse().expect("CIDR"))
        .collect()
    });
    let ip = match ip {
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                IpAddr::V4(v4)
            } else {
                // Excludes local, multicast, unspecified, NAT64 and other transition ranges.
                if (v.segments()[0] & 0xe000) != 0x2000 {
                    return false;
                }
                IpAddr::V6(v)
            }
        }
        v => v,
    };
    !denied.iter().any(|net| net.contains(&ip))
}
#[derive(Clone, Debug)]
pub struct Fetched {
    /// True only when every redirect from the requested URI was HTTP 301/308.
    pub permanent_redirect: bool,
    pub truncated: bool,
    pub validators: Validators,
    pub url: Url,
    pub content_type: String,
    pub status: u16,
    pub retry_after_secs: u64,
    pub location: Option<String>,
    pub body: Vec<u8>,
}
#[async_trait]
pub trait Dns: Send + Sync {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>>;
}
pub struct SystemDns;
#[async_trait]
impl Dns for SystemDns {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Validators {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct FetchOptions {
    /// Extra request headers, by literal name.
    pub headers: Vec<(&'static str, String)>,
    pub json_body: Option<serde_json::Value>,
    pub sensitive: bool,
    pub accept: Option<String>,
    pub user_agent: Option<String>,
    pub feed_cap: Option<usize>,
    /// A cap for every body whatever its type, HTML too: a larger body is refused, never
    /// truncated (PKARR relay payloads).
    pub body_cap: Option<usize>,
    pub feed_hint: bool,
    /// Content types this fetch also admits, within the page cap (`""`: no content type). The
    /// caller parses them: Pubky objects (`pubky::OBJECT_TYPES`), PKARR relay payloads
    /// (`pkarr_relays::PAYLOAD_TYPES`).
    pub extra_types: &'static [&'static str],
    pub validators: Validators,
}
/// A refusal by the egress policy, classed `policy`: not broken content, and not transient.
fn refused(message: &'static str) -> anyhow::Error {
    anyhow::Error::new(super::ErrorClass("policy")).context(message)
}
#[async_trait]
pub trait Transport: Send + Sync {
    async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched>;
    async fn get_with(
        &self,
        url: &Url,
        addresses: &[SocketAddr],
        _options: &FetchOptions,
    ) -> Result<Fetched> {
        self.get(url, addresses).await
    }
}
pub struct PinnedTransport;
#[async_trait]
impl Transport for PinnedTransport {
    async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
        self.get_with(url, addresses, &FetchOptions::default())
            .await
    }
    async fn get_with(
        &self,
        url: &Url,
        addresses: &[SocketAddr],
        options: &FetchOptions,
    ) -> Result<Fetched> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(options.user_agent.as_deref().unwrap_or(USER_AGENT))
            .resolve_to_addrs(url.host_str().context("missing host")?, addresses)
            .build()?;
        let mut request = if let Some(body) = &options.json_body {
            client.post(url.clone()).json(body)
        } else {
            client.get(url.clone())
        }
        .header(
            "accept",
            options
                .accept
                .clone()
                .unwrap_or_else(|| ALLOWED_TYPES.join(", ")),
        );
        for (key, value) in &options.headers {
            request = request.header(*key, value);
        }
        if let Some(v) = &options.validators.etag {
            request = request.header("if-none-match", v);
        }
        if let Some(v) = &options.validators.last_modified {
            request = request.header("if-modified-since", v);
        }
        read_response_with(url.clone(), request.send().await?, options).await
    }
}
pub async fn read_response(url: Url, response: reqwest::Response) -> Result<Fetched> {
    read_response_with(url, response, &FetchOptions::default()).await
}
pub async fn read_response_with(
    url: Url,
    mut response: reqwest::Response,
    options: &FetchOptions,
) -> Result<Fetched> {
    let header = |key: &str| {
        response
            .headers()
            .get(key)
            .and_then(|v| v.to_str().ok())
            .filter(|s| s.len() <= 2048)
            .map(str::to_owned)
    };
    // Every `Link` header, as one comma-separated value (RFC 8288), up to 2 KB of whole headers.
    let mut link = String::new();
    for value in response.headers().get_all("link").iter() {
        let Ok(value) = value.to_str() else { continue };
        if link.len() + value.len() + 2 > 2048 {
            continue;
        }
        if !link.is_empty() {
            link.push_str(", ");
        }
        link.push_str(value);
    }
    let validators = Validators {
        link: Some(link).filter(|s| !s.is_empty()),
        etag: header("etag"),
        last_modified: header("last-modified"),
    };
    let status = response.status().as_u16();
    let retry_after_secs = super::osm::retry_after(response.headers());
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut body = vec![];
    let mut truncated = false;
    if !(300..400).contains(&status) && (200..300).contains(&status) {
        let feed_type = matches!(
            content_type.as_str(),
            "text/calendar"
                | "application/ics"
                | "text/xml"
                | "application/xml"
                | "application/rss+xml"
                | "application/atom+xml"
                | "application/feed+json"
        );
        let possible = feed_type
            || matches!(
                content_type.as_str(),
                "text/plain" | "application/octet-stream" | ""
            );
        let extra = options.extra_types.contains(&content_type.as_str());
        if !ALLOWED_TYPES.contains(&content_type.as_str()) && !possible && !extra {
            return Err(refused("content type is not allowed"));
        }
        let html = matches!(content_type.as_str(), "text/html" | "application/xhtml+xml");
        let (max, refuse) = match options.body_cap {
            Some(cap) => (cap, true),
            None if html => (BODY_CAP, false),
            None => (options.feed_cap.unwrap_or(BODY_CAP), true),
        };
        if refuse && response.content_length().is_some_and(|n| n > max as u64) {
            return Err(BodyTooLarge.into());
        }
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > max {
                if refuse {
                    return Err(BodyTooLarge.into());
                }
                body.extend_from_slice(&chunk[..max.saturating_sub(body.len())]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
            let sniff = String::from_utf8_lossy(&body[..body.len().min(256)])
                .trim_start_matches(['\u{feff}', ' ', '\r', '\n'])
                .starts_with("BEGIN:VCALENDAR");
            if body.len() > BODY_CAP
                && !(options.feed_cap.is_some() && (feed_type || options.feed_hint || sniff))
            {
                return Err(BodyTooLarge.into());
            }
        }
        let sniff = String::from_utf8_lossy(&body[..body.len().min(256)])
            .trim_start_matches(['\u{feff}', ' ', '\r', '\n'])
            .starts_with("BEGIN:VCALENDAR");
        if !ALLOWED_TYPES.contains(&content_type.as_str()) && !feed_type && !sniff && !extra {
            return Err(refused("content type is not allowed"));
        }
    } else if matches!(status, 403 | 429 | 503)
        && matches!(
            content_type.as_str(),
            "text/html" | "application/xhtml+xml" | "text/plain" | ""
        )
    {
        // An error page keeps a bounded prefix of its body, so the interstitial classifier sees a
        // challenge; the status is preserved.
        const ERROR_BODY_CAP: usize = 64 * 1024;
        while let Some(chunk) = response.chunk().await? {
            let remaining = ERROR_BODY_CAP.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if chunk.len() >= remaining {
                truncated = true;
                break;
            }
        }
    }
    Ok(Fetched {
        permanent_redirect: false,
        truncated,
        validators,
        url,
        status,
        retry_after_secs,
        location,
        content_type,
        body,
    })
}
type HostBudget = Arc<Mutex<Instant>>;
struct DiscoveryEntry {
    expires: Instant,
    accessed: Instant,
    value: Option<Fetched>,
}
type DiscoveryCache = Arc<Mutex<HashMap<String, DiscoveryEntry>>>;
#[derive(Clone)]
pub struct Egress {
    cfg: EgressConfig,
    network_relays: bool,
    dns: Arc<dyn Dns>,
    transport: Arc<dyn Transport>,
    hosts: Arc<Mutex<HashMap<String, HostBudget>>>,
    discovered: DiscoveryCache,
    discoveries: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}
impl Egress {
    /// One bounded Nostr subscription. DNS is checked once and the checked address is connected directly.
    pub async fn nostr_request(
        &self,
        uri: &str,
        filter: serde_json::Value,
        deadline_ms: u64,
    ) -> Result<Vec<serde_json::Value>> {
        let key = format!(
            "{uri}|REQ:{}",
            blake3::hash(&serde_json::to_vec(&filter)?).to_hex()
        );
        let fetched = self
            .cached(key, 3600, async {
                let values = self.nostr_request_inner(uri, filter, deadline_ms).await?;
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: Url::parse(uri)?,
                    validators: Default::default(),
                    content_type: "application/json".into(),
                    status: 200,
                    retry_after_secs: 60,
                    location: None,
                    body: serde_json::to_vec(&values)?,
                })
            })
            .await?;
        Ok(tokio::task::spawn_blocking(move || serde_json::from_slice(&fetched.body)).await??)
    }
    async fn nostr_request_inner(
        &self,
        uri: &str,
        filter: serde_json::Value,
        deadline_ms: u64,
    ) -> Result<Vec<serde_json::Value>> {
        use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
        tokio::time::timeout(Duration::from_millis(deadline_ms), async {
            let url = Url::parse(uri)?;
            if url.scheme() != "wss" || !url.username().is_empty() || url.password().is_some() {
                bail!("invalid relay URL");
            }
            let host = url
                .host_str()
                .context("relay host missing")?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if !self.allows_host(&host) {
                bail!("relay denied by egress policy");
            }
            let addresses = self
                .dns
                .lookup(&host, url.port_or_known_default().context("relay port")?)
                .await?;
            if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
                bail!("relay DNS resolved to non-public address");
            }
            anyhow::ensure!(
                self.network_relays,
                "relay network disabled for injected transports"
            );
            let _permit = self.host_permit(&host).await?;
            let stream = tokio::net::TcpStream::connect(addresses.as_slice()).await?;
            let config = WebSocketConfig::default()
                .max_message_size(Some(BODY_CAP))
                .max_frame_size(Some(BODY_CAP));
            let mut observation = crate::telemetry::FetchGuard::start();
            let (mut socket, _) =
                tokio_tungstenite::client_async_tls_with_config(uri, stream, Some(config), None)
                    .await?;
            observation.status(101);
            nostr_subscription(&mut socket, filter).await
        })
        .await?
    }
    pub fn allows(&self, uri: &str) -> bool {
        Url::parse(uri)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .is_some_and(|host| self.allows_host(&host))
    }
    fn allows_host(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        !super::osm::osm_host(&host)
            && !self.cfg.deny_hosts.iter().any(|h| host_matches(&host, h))
            && (self.cfg.allow_hosts.is_empty()
                || self.cfg.allow_hosts.iter().any(|h| host_matches(&host, h)))
    }
    /// The operator's contact address (`http.contact`), if set.
    pub fn contact(&self) -> Option<&str> {
        Some(self.cfg.contact.as_str()).filter(|c| !c.is_empty())
    }
    pub fn new(cfg: EgressConfig) -> Self {
        let mut e = Self::with_backends(cfg, Arc::new(SystemDns), Arc::new(PinnedTransport));
        e.network_relays = true;
        e
    }
    pub fn with_backends(
        cfg: EgressConfig,
        dns: Arc<dyn Dns>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            cfg,
            network_relays: false,
            dns,
            transport,
            hosts: Arc::default(),
            discovered: Arc::default(),
            discoveries: Arc::default(),
        }
    }
    pub async fn fetch(&self, uri: &str) -> Result<Fetched> {
        tokio::time::timeout(Duration::from_secs(10), self.fetch_inner(uri))
            .await
            .context("web resolution exceeded 10 seconds")?
    }
    pub async fn fetch_with(&self, uri: &str, options: &FetchOptions) -> Result<Fetched> {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            self.fetch_options_inner(uri, options),
        )
        .await?;
        if options.sensitive {
            result
                .map(|mut fetched| {
                    // Parsers may retain the final URL as an alias. Credentials from
                    // configuration must never enter documents, logs, or change feeds.
                    fetched.url.set_query(None);
                    fetched.url.set_fragment(None);
                    fetched
                })
                .map_err(|error| {
                    // A reqwest cause can embed the complete URL, including API credentials.
                    if error.is::<super::interstitial::Blocked>() {
                        anyhow::Error::new(super::interstitial::Blocked)
                    } else if error.is::<BodyTooLarge>() {
                        anyhow::Error::new(BodyTooLarge)
                    } else if let Some(retry) = error.downcast_ref::<super::RetryLater>() {
                        anyhow::Error::new(super::RetryLater {
                            class: retry.class,
                            seconds: retry.seconds,
                        })
                    } else {
                        anyhow::Error::new(super::ErrorClass(super::error_class(&error)))
                    }
                })
        } else {
            result
        }
    }

    /// Concurrent references share a flight. Successful responses use the requested TTL;
    /// failures use five minutes. The LRU cache retains at most 10,000 entries / 32 MiB.
    /// Capacity eviction can shorten a TTL; host budgets still apply to every fetch.
    pub async fn discover(&self, uri: &str, options: &FetchOptions, ttl: u64) -> Result<Fetched> {
        self.cached(
            Self::discovery_key(uri, options)?,
            ttl,
            self.fetch_with(uri, options),
        )
        .await
    }
    fn discovery_key(uri: &str, options: &FetchOptions) -> Result<String> {
        let mut url = Url::parse(uri)?;
        url.set_fragment(None);
        let mut key = url.to_string();
        if options.json_body.is_some() || !options.headers.is_empty() {
            let identity = serde_json::to_vec(&(&options.json_body, &options.headers))?;
            key.push_str(&format!("|request:{}", blake3::hash(&identity).to_hex()));
        }
        Ok(key)
    }
    /// A successful HTTP response can still contain a transient application error.
    /// Shorten only its existing cache entry, without extending older expiration times.
    pub async fn shorten_discovery(
        &self,
        uri: &str,
        options: &FetchOptions,
        seconds: u64,
    ) -> Result<()> {
        let key = Self::discovery_key(uri, options)?;
        if let Some(entry) = self.discovered.lock().await.get_mut(&key) {
            entry.expires = entry
                .expires
                .min(Instant::now() + Duration::from_secs(seconds.clamp(1, 3600)));
        }
        Ok(())
    }
    async fn cached(
        &self,
        key: String,
        ttl: u64,
        fetch: impl std::future::Future<Output = Result<Fetched>>,
    ) -> Result<Fetched> {
        let lock = {
            let mut flights = self.discoveries.lock().await;
            flights.retain(|_, lock| Arc::strong_count(lock) > 1);
            flights.entry(key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;
        {
            let mut cache = self.discovered.lock().await;
            let now = Instant::now();
            cache.retain(|_, entry| entry.expires > now);
            if let Some(entry) = cache.get_mut(&key) {
                entry.accessed = now;
                return entry.value.clone().ok_or_else(|| {
                    super::RetryLater {
                        class: "discovery_cached",
                        seconds: entry
                            .expires
                            .saturating_duration_since(now)
                            .as_secs()
                            .max(1),
                    }
                    .into()
                });
            }
        }
        let result = fetch.await;
        // Our own host budget is not the page's answer: ask again when the slot comes.
        if result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<super::RetryLater>())
            .is_some_and(|r| r.class == "rate_limited")
        {
            return result;
        }
        let mut cache = self.discovered.lock().await;
        let now = Instant::now();
        cache.retain(|_, entry| entry.expires > now);
        let retained = result
            .as_ref()
            .ok()
            .filter(|f| f.body.len() <= 32 * 1024 * 1024);
        let needed = retained.map_or(0, |f| f.body.len());
        let mut bytes: usize = cache
            .values()
            .filter_map(|entry| entry.value.as_ref())
            .map(|f| f.body.len())
            .sum();
        if cache.len() >= 10000 || bytes.saturating_add(needed) > 32 * 1024 * 1024 {
            let mut oldest: Vec<_> = cache
                .iter()
                .map(|(k, entry)| (entry.accessed, k.clone()))
                .collect();
            oldest.sort_unstable();
            for (_, key) in oldest {
                if cache.len() < 10000 && bytes.saturating_add(needed) <= 32 * 1024 * 1024 {
                    break;
                }
                if let Some(entry) = cache.remove(&key) {
                    bytes = bytes.saturating_sub(entry.value.as_ref().map_or(0, |f| f.body.len()));
                }
            }
        }
        let ttl = if retained.is_some_and(|f| (200..400).contains(&f.status)) {
            ttl.clamp(3600, 31536000)
        } else {
            300
        };
        cache.insert(
            key,
            DiscoveryEntry {
                expires: now + Duration::from_secs(ttl),
                accessed: now,
                value: retained.cloned(),
            },
        );
        result
    }
    async fn fetch_inner(&self, uri: &str) -> Result<Fetched> {
        self.fetch_options_inner(uri, &FetchOptions::default())
            .await
    }
    async fn fetch_options_inner(&self, uri: &str, options: &FetchOptions) -> Result<Fetched> {
        let requested = Url::parse(uri)?;
        let mut url = requested.clone();
        let mut permanent = true;
        // A redirect within the same host keeps the slot it has; a slow host's next slot is
        // further away than a fetch may wait, so a new one would always answer RetryLater.
        let mut permit: Option<(String, OwnedMutexGuard<Instant>)> = None;
        for redirects in 0..=3 {
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(refused(
                    "only public HTTP(S) URLs without credentials are allowed",
                ));
            }
            let host = url
                .host_str()
                .context("missing host")?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if !self.allows_host(&host) {
                return Err(anyhow::Error::new(super::ErrorClass("policy"))
                    .context("host denied by operator/OSM policy"));
            }
            let port = url.port_or_known_default().context("missing port")?;
            let addresses = match url.host().context("missing host")? {
                Host::Ipv4(v) => vec![SocketAddr::new(IpAddr::V4(v), port)],
                Host::Ipv6(v) => vec![SocketAddr::new(IpAddr::V6(v), port)],
                Host::Domain(_) => self
                    .dns
                    .lookup(&host, port)
                    .await
                    .context(super::ErrorClass("dns"))?,
            };
            if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
                return Err(anyhow::Error::new(super::ErrorClass("policy"))
                    .context("egress refused non-public address"));
            }
            // Pin checked addresses to the connection; a second DNS lookup could rebind to a private IP.
            match &mut permit {
                Some((held, guard)) if *held == host => {
                    **guard = Instant::now() + self.host_interval(&host);
                }
                _ => {
                    // Release the other host's slot before waiting for this one.
                    drop(permit.take());
                    permit = Some((host.clone(), self.host_permit(&host).await?));
                }
            }
            let mut observation = crate::telemetry::FetchGuard::start();
            let with_agent;
            let default_agent = options
                .user_agent
                .as_deref()
                .is_none_or(|ua| ua == USER_AGENT);
            let options = if default_agent && !self.cfg.contact.is_empty() {
                with_agent = FetchOptions {
                    user_agent: Some(user_agent(&self.cfg.contact)),
                    ..options.clone()
                };
                &with_agent
            } else {
                options
            };
            let mut response = self.transport.get_with(&url, &addresses, options).await?;
            observation.status(response.status);
            drop(observation);
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                if options.sensitive || options.json_body.is_some() || !options.headers.is_empty() {
                    return Err(refused("redirect of authenticated/API request refused"));
                }
                if redirects == 3 {
                    return Err(refused("more than 3 redirects"));
                }
                permanent &= matches!(response.status, 301 | 308);
                url = url.join(
                    response
                        .location
                        .as_deref()
                        .context("redirect omitted location")?,
                )?;
                continue;
            }
            let page = if response.content_type.contains("html") {
                let input = response.clone();
                let original = requested.clone();
                tokio::task::spawn_blocking(move || super::interstitial::inspect(&original, &input))
                    .await?
            } else {
                super::interstitial::inspect(&requested, &response)
            };
            if page == super::interstitial::Page::Blocked {
                crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "blocked");
                return Err(super::interstitial::Blocked.into());
            }
            if let super::interstitial::Page::Redirect(target) = page {
                if options.sensitive || options.json_body.is_some() || !options.headers.is_empty() {
                    return Err(refused("redirect of authenticated/API request refused"));
                }
                if redirects == 3 {
                    return Err(refused("more than 3 redirects"));
                }
                permanent = false;
                url = url.join(&target)?;
                continue;
            }
            if response.status == 429 {
                return Err(super::RetryLater {
                    class: "http_4xx",
                    seconds: response.retry_after_secs,
                }
                .into());
            }
            response.permanent_redirect = redirects > 0 && permanent;
            return Ok(response);
        }
        unreachable!()
    }
    /// The host's next slot, held for the whole request (and its redirects within the host): one
    /// request per host at a time. An
    /// operator's `host_rates` entry sets the rate; else a source's published rate lowers the
    /// default, never raises it. A slot further away than `MAX_SLOT_WAIT`, or a queue longer than
    /// that, answers `RetryLater` instead of a wait the fetch timeout would cut short. Only a
    /// request that gets its slot moves the next one, so a cancelled waiter holds nothing back.
    async fn host_permit(&self, host: &str) -> Result<OwnedMutexGuard<Instant>> {
        let rate = self.host_rate(host);
        let budget = {
            let mut hosts = self.hosts.lock().await;
            if hosts.len() >= 10000 {
                hosts.retain(|_, v| {
                    Arc::strong_count(v) > 1 || v.try_lock().map_or(true, |t| *t > Instant::now())
                });
            }
            hosts
                .entry(host.into())
                .or_insert_with(|| Arc::new(Mutex::new(Instant::now())))
                .clone()
        };
        let retry = |wait: Duration| {
            anyhow::Error::new(super::RetryLater {
                class: "rate_limited",
                seconds: wait.as_secs() + 1,
            })
        };
        let deadline = Instant::now() + MAX_SLOT_WAIT;
        let Ok(mut guard) = tokio::time::timeout_at(deadline, budget.lock_owned()).await else {
            return Err(retry(
                Duration::from_secs_f64(1.0 / rate).max(MAX_SLOT_WAIT),
            ));
        };
        if *guard > deadline {
            return Err(retry(guard.saturating_duration_since(Instant::now())));
        }
        tokio::time::sleep_until(*guard).await;
        *guard = Instant::now() + self.host_interval(host);
        Ok(guard)
    }
    /// The least time between two requests to a host: its rate plus a stable per-host jitter,
    /// which avoids synchronized requests without weakening the minimum interval.
    fn host_interval(&self, host: &str) -> Duration {
        let jitter = f64::from(blake3::hash(host.as_bytes()).as_bytes()[0]) / 2550.0;
        Duration::from_secs_f64(1.0 / self.host_rate(host) + jitter)
    }
    fn host_rate(&self, host: &str) -> f64 {
        let default = self.cfg.host_requests_per_second;
        // A source's limit (`source_rates`) or a service's published one (`PUBLISHED_RATES`)
        // lowers the default, never raises it.
        let mut rate = match self.cfg.host_rates.get(host) {
            Some(rate) => *rate,
            None => self
                .cfg
                .source_rates
                .get(host)
                .copied()
                .into_iter()
                .chain(published_rate(host))
                .fold(default, f64::min),
        };
        if matches!(host, "api.nostr.wine" | "portal.einundzwanzig.space") {
            rate = rate.min(1.0);
        }
        if host == "api.nostrarchives.com" {
            rate = rate.min(2.0);
        }
        rate
    }
}
/// Requests per second the hub services the also-on sources read publish as their limit (one
/// request at a time per host is the rule for every host); it lowers a host's default pace,
/// beside the sources' own limits (`sources::budgets`). A rule `*.example.org` covers the
/// subdomains.
pub const PUBLISHED_RATES: &[(&str, f64)] = &[
    // MusicBrainz: 1 request/s per IP, 503 above that.
    ("musicbrainz.org", 1.0),
    // Wikimedia: 200 requests/min with a User-Agent naming a contact (at most 3 at once).
    ("www.wikidata.org", 200.0 / 60.0),
    ("*.wikipedia.org", 200.0 / 60.0),
    ("commons.wikimedia.org", 200.0 / 60.0),
    // Open Library: 3 requests/s with a User-Agent and a contact.
    ("openlibrary.org", 3.0),
    // iTunes Search and Lookup: about 20 requests/min.
    ("itunes.apple.com", 20.0 / 60.0),
    // crates.io: 1 request/s.
    ("crates.io", 1.0),
    // Crossref: 10 requests/s on the polite pool.
    ("api.crossref.org", 10.0),
    // Software Heritage: 120 requests/hour anonymous.
    ("archive.softwareheritage.org", 120.0 / 3600.0),
];
/// The published rate of `host`, if it has one (the most specific rule).
pub fn published_rate(host: &str) -> Option<f64> {
    PUBLISHED_RATES
        .iter()
        .filter(|(rule, _)| host_matches(host, rule))
        .max_by_key(|(rule, _)| rule.len())
        .map(|(_, rate)| *rate)
}
fn host_matches(host: &str, rule: &str) -> bool {
    let rule = rule.to_ascii_lowercase();
    if let Some(suffix) = rule.strip_prefix("*.") {
        host == suffix || host.strip_suffix(suffix).is_some_and(|s| s.ends_with('.'))
    } else {
        host == rule.trim_end_matches('.')
    }
}

async fn nostr_subscription<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    filter: serde_json::Value,
) -> Result<Vec<serde_json::Value>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    socket
        .send(Message::Text(
            serde_json::json!(["REQ", "pinakes", filter])
                .to_string()
                .into(),
        ))
        .await?;
    let mut events = vec![];
    let mut bytes = 0;
    let mut complete = false;
    for _ in 0..256 {
        let Some(message) = socket.next().await else {
            break;
        };
        let message = message?;
        bytes += message.len();
        if bytes > BODY_CAP {
            bail!("relay response exceeds cap");
        }
        if let Message::Text(text) = message {
            let value: serde_json::Value =
                tokio::task::spawn_blocking(move || serde_json::from_str(&text)).await??;
            if value[1] != "pinakes" {
                continue;
            }
            if value[0] == "EOSE" {
                complete = true;
                break;
            }
            if value[0] == "EVENT" {
                events.push(value[2].clone());
            }
            if value[0] == "CLOSED" {
                break;
            }
        }
    }
    socket
        .send(Message::Text("[\"CLOSE\",\"pinakes\"]".into()))
        .await?;
    let _ = socket.close(None).await;
    if !complete {
        bail!("relay did not finish subscription");
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test(start_paused = true)]
    async fn slow_hosts_answer_retry_later_instead_of_a_doomed_wait() {
        struct Ok200;
        #[async_trait]
        impl Transport for Ok200 {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: url.clone(),
                    status: 200,
                    content_type: "application/json".into(),
                    validators: Default::default(),
                    location: None,
                    retry_after_secs: 60,
                    body: b"{}".to_vec(),
                })
            }
        }
        let egress = Arc::new(Egress::with_backends(
            EgressConfig {
                source_rates: [
                    ("slow.example".to_owned(), 5.0 / 60.0),
                    ("fast.example".to_owned(), 50.0),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            Arc::new(FakeDns),
            Arc::new(Ok200),
        ));
        // Three links of a host with a 12-second interval claimed together: one goes, the others
        // learn when to come back, at once.
        let tasks: Vec<_> = (0..3)
            .map(|n| {
                let egress = egress.clone();
                tokio::spawn(
                    async move { egress.fetch(&format!("https://slow.example/{n}")).await },
                )
            })
            .collect();
        let mut results = vec![];
        for task in tasks {
            results.push(task.await.unwrap());
        }
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        for error in results.into_iter().filter_map(Result::err) {
            let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
            assert_eq!(retry.class, "rate_limited");
            assert!((9..=14).contains(&retry.seconds), "{}", retry.seconds);
        }
        tokio::time::advance(Duration::from_secs(13)).await;
        assert!(egress.fetch("https://slow.example/again").await.is_ok());
        // A source's rate never raises the default of one request per second.
        let start = Instant::now();
        egress.fetch("https://fast.example/1").await.unwrap();
        egress.fetch("https://fast.example/2").await.unwrap();
        assert!(Instant::now() - start >= Duration::from_secs(1));
    }
    #[tokio::test(start_paused = true)]
    async fn a_queue_behind_a_slow_request_answers_retry_later() {
        struct Slow;
        #[async_trait]
        impl Transport for Slow {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                if url.path() == "/slow" {
                    tokio::time::sleep(Duration::from_secs(7)).await;
                }
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: url.clone(),
                    status: 200,
                    content_type: "application/json".into(),
                    validators: Default::default(),
                    location: None,
                    retry_after_secs: 60,
                    body: b"{}".to_vec(),
                })
            }
        }
        let egress = Arc::new(Egress::with_backends(
            EgressConfig::default(),
            Arc::new(FakeDns),
            Arc::new(Slow),
        ));
        let slow = {
            let egress = egress.clone();
            tokio::spawn(async move { egress.fetch("https://host.example/slow").await })
        };
        tokio::task::yield_now().await;
        // A waiter that gives up holds nothing back.
        let cancelled = {
            let egress = egress.clone();
            tokio::spawn(async move { egress.fetch("https://host.example/cancelled").await })
        };
        tokio::task::yield_now().await;
        cancelled.abort();
        // The next one gives up before the fetch timeout, with a time to come back.
        let error = egress
            .fetch("https://host.example/queued")
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
        assert_eq!((retry.class, retry.seconds), ("rate_limited", 6));
        assert!(slow.await.unwrap().is_ok());
        let start = Instant::now();
        assert!(egress.fetch("https://host.example/after").await.is_ok());
        assert!(Instant::now() - start <= Duration::from_secs(2));
    }
    #[tokio::test(start_paused = true)]
    async fn a_redirect_within_a_slow_host_keeps_its_slot() {
        struct Redirects;
        #[async_trait]
        impl Transport for Redirects {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                let redirect = url.path() == "/old";
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: url.clone(),
                    status: if redirect { 301 } else { 200 },
                    content_type: "application/json".into(),
                    validators: Default::default(),
                    location: redirect.then(|| "/new".to_owned()),
                    retry_after_secs: 60,
                    body: b"{}".to_vec(),
                })
            }
        }
        let egress = Egress::with_backends(
            EgressConfig {
                source_rates: [("slow.example".to_owned(), 5.0 / 60.0)]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            Arc::new(FakeDns),
            Arc::new(Redirects),
        );
        // With a 12-second interval a second slot is always too far away: the hop keeps the first.
        let fetched = egress.fetch("https://slow.example/old").await.unwrap();
        assert_eq!(fetched.url.path(), "/new");
        assert!(fetched.permanent_redirect);
        // The next request still waits a full interval after the last hop.
        let error = egress
            .fetch("https://slow.example/other")
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
        assert_eq!(retry.class, "rate_limited");

        // Our own budget is not the page's answer: discovery asks again once the slot comes.
        let options = FetchOptions::default();
        let error = egress
            .discover("https://slow.example/page", &options, 3600)
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
        assert_eq!(retry.class, "rate_limited");
        tokio::time::advance(Duration::from_secs(13)).await;
        assert!(egress
            .discover("https://slow.example/page", &options, 3600)
            .await
            .is_ok());
    }
    #[tokio::test]
    async fn body_cap_refuses_any_type_html_too() {
        use axum::{body::Body, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let html = |n: usize| ([("content-type", "text/html")], "x".repeat(n));
        let app = Router::new()
            .route("/small", get(move || async move { html(8 * 1024) }))
            .route("/large", get(move || async move { html(8 * 1024 + 1) }))
            .route(
                "/chunked",
                get(|| async {
                    let chunks = (0..3).map(|_| Ok::<_, std::io::Error>("x".repeat(4096)));
                    (
                        [("content-type", "text/html")],
                        Body::from_stream(futures_util::stream::iter(chunks)),
                    )
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let options = FetchOptions {
            body_cap: Some(8 * 1024),
            ..Default::default()
        };
        let fetch = |path: &str| {
            let url: Url = format!("http://{addr}{path}").parse().unwrap();
            let request = client.get(url.clone());
            let options = &options;
            async move { read_response_with(url, request.send().await.unwrap(), options).await }
        };
        assert_eq!(fetch("/small").await.unwrap().body.len(), 8 * 1024);
        for path in ["/large", "/chunked"] {
            let error = fetch(path).await.unwrap_err();
            assert!(
                error.downcast_ref::<BodyTooLarge>().is_some(),
                "{path}: {error:#}"
            );
        }
        server.abort();
    }
    #[tokio::test]
    async fn the_default_agent_carries_the_operator_contact() {
        #[derive(Default)]
        struct Agents(std::sync::Mutex<Vec<Option<String>>>);
        #[async_trait]
        impl Transport for Agents {
            async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
                self.get_with(url, addresses, &FetchOptions::default())
                    .await
            }
            async fn get_with(
                &self,
                url: &Url,
                _: &[SocketAddr],
                options: &FetchOptions,
            ) -> Result<Fetched> {
                self.0.lock().unwrap().push(options.user_agent.clone());
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: url.clone(),
                    status: 200,
                    content_type: "application/json".into(),
                    validators: Default::default(),
                    location: None,
                    retry_after_secs: 60,
                    body: b"{}".to_vec(),
                })
            }
        }
        let with_contact = format!(
            "pinakes/{} (+https://pubky.app; info@synonym.to)",
            env!("CARGO_PKG_VERSION")
        );
        for (contact, agent, sent) in [
            ("info@synonym.to", None, Some(with_contact.as_str())),
            (
                "info@synonym.to",
                Some(USER_AGENT),
                Some(with_contact.as_str()),
            ),
            ("info@synonym.to", Some("Browser/1.0"), Some("Browser/1.0")),
            ("", None, None),
        ] {
            let transport = Arc::new(Agents::default());
            let egress = Egress::with_backends(
                EgressConfig {
                    contact: contact.into(),
                    ..Default::default()
                },
                Arc::new(FakeDns),
                transport.clone(),
            );
            assert_eq!(egress.contact(), Some(contact).filter(|c| !c.is_empty()));
            let options = FetchOptions {
                user_agent: agent.map(str::to_owned),
                ..Default::default()
            };
            egress
                .fetch_with("https://public.example/api", &options)
                .await
                .unwrap();
            assert_eq!(transport.0.lock().unwrap()[0].as_deref(), sent);
        }
    }
    #[tokio::test(start_paused = true)]
    async fn graphql_error_expires_before_the_next_retry() {
        struct Graphql(AtomicUsize);
        #[async_trait]
        impl Transport for Graphql {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: url.clone(),
                    status: 200,
                    content_type: "application/json".into(),
                    validators: Default::default(),
                    location: None,
                    retry_after_secs: 60,
                    body: if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                        br#"{"errors":[{"message":"temporary"}]}"#.to_vec()
                    } else {
                        br#"{"data":{"event":null}}"#.to_vec()
                    },
                })
            }
        }
        let transport = Arc::new(Graphql(AtomicUsize::new(0)));
        let egress =
            Egress::with_backends(Default::default(), Arc::new(FakeDns), transport.clone());
        let options = FetchOptions {
            json_body: Some(serde_json::json!({"query":"fixture"})),
            ..Default::default()
        };
        let uri = "https://public.example/api";
        let definition = crate::event::adapters::definition("mobilizon").unwrap();
        for advance in [0, 299] {
            tokio::time::advance(Duration::from_secs(advance)).await;
            let error = crate::event::adapters::service::discover_enrichment(
                &egress, uri, &options, 86400, definition,
            )
            .await
            .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<super::super::RetryLater>()
                    .unwrap()
                    .class,
                "graphql"
            );
        }
        assert_eq!(transport.0.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        crate::event::adapters::service::discover_enrichment(
            &egress, uri, &options, 86400, definition,
        )
        .await
        .unwrap();
        assert_eq!(transport.0.load(Ordering::SeqCst), 2);
    }
    #[tokio::test(start_paused = true)]
    async fn discovery_capacity_evicts_lru_and_failures_expire_after_five_minutes() {
        let egress = Egress::new(Default::default());
        let now = Instant::now();
        {
            let mut cache = egress.discovered.lock().await;
            for i in 0..10000 {
                cache.insert(
                    i.to_string(),
                    DiscoveryEntry {
                        expires: now + Duration::from_secs(86400),
                        accessed: now - Duration::from_secs(10000 - i),
                        value: None,
                    },
                );
            }
        }
        assert!(egress
            .cached("0".into(), 86400, async { panic!("cached") })
            .await
            .is_err());
        let response = Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: Url::parse("https://example.org/").unwrap(),
            content_type: "text/html".into(),
            status: 200,
            retry_after_secs: 60,
            location: None,
            body: vec![1],
        };
        egress
            .cached("fresh".into(), 86400, async { Ok(response.clone()) })
            .await
            .unwrap();
        {
            let cache = egress.discovered.lock().await;
            assert_eq!(cache.len(), 10000);
            assert!(cache.contains_key("0"));
            assert!(!cache.contains_key("1"));
        }
        egress
            .cached("failure".into(), 86400, async { bail!("transport") })
            .await
            .unwrap_err();
        tokio::time::advance(Duration::from_secs(299)).await;
        let error = egress
            .cached("failure".into(), 86400, async { panic!("negative cache") })
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<super::super::RetryLater>()
                .unwrap()
                .seconds,
            1
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        egress
            .cached("failure".into(), 86400, async { Ok(response.clone()) })
            .await
            .unwrap();
        egress
            .cached("fresh".into(), 86400, async {
                panic!("positive TTL remains")
            })
            .await
            .unwrap();
        // HTTP errors also expire quickly, and a body replaces old bodies within the byte cap.
        let mut failure = response.clone();
        failure.status = 503;
        egress
            .cached("http-error".into(), 86400, async { Ok(failure) })
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(301)).await;
        egress
            .cached("http-error".into(), 86400, async { Ok(response) })
            .await
            .unwrap();
        for key in ["large-a", "large-b"] {
            egress
                .cached(key.into(), 3600, async {
                    Ok(Fetched {
                        permanent_redirect: false,
                        truncated: false,
                        validators: Default::default(),
                        url: Url::parse("https://example.org/").unwrap(),
                        content_type: "text/calendar".into(),
                        status: 200,
                        retry_after_secs: 60,
                        location: None,
                        body: vec![0; 20 * 1024 * 1024],
                    })
                })
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        let cache = egress.discovered.lock().await;
        assert!(!cache.contains_key("large-a"));
        assert_eq!(
            cache
                .values()
                .filter_map(|e| e.value.as_ref())
                .map(|f| f.body.len())
                .sum::<usize>(),
            20 * 1024 * 1024
        );
    }
    /// A hub's published limit lowers its host's default pace; an operator's entry sets it.
    #[tokio::test(start_paused = true)]
    async fn a_published_limit_paces_its_host() {
        assert_eq!(published_rate("musicbrainz.org"), Some(1.0));
        assert_eq!(published_rate("de.wikipedia.org"), Some(200.0 / 60.0));
        assert_eq!(published_rate("wikipedia.org"), Some(200.0 / 60.0));
        assert_eq!(published_rate("evilwikipedia.org"), None);
        assert_eq!(published_rate("example.org"), None);
        let gap = |cfg: EgressConfig, host: &'static str| async move {
            let egress = Egress::new(cfg);
            drop(egress.host_permit(host).await.unwrap());
            let start = Instant::now();
            drop(egress.host_permit(host).await.unwrap());
            start.elapsed()
        };
        // iTunes allows about 20 requests a minute: three seconds apart, not the default one.
        let itunes = gap(EgressConfig::default(), "itunes.apple.com").await;
        assert!(itunes >= Duration::from_secs(3), "{itunes:?}");
        // A published limit above the default never raises it.
        let crossref = gap(EgressConfig::default(), "api.crossref.org").await;
        assert!(crossref >= Duration::from_secs(1), "{crossref:?}");
        let mut slow = EgressConfig::default();
        slow.host_rates.insert("www.wikidata.org".into(), 0.5);
        let wd = gap(slow, "www.wikidata.org").await;
        assert!(wd >= Duration::from_secs(2), "{wd:?}");
    }
    #[test]
    fn rejects_every_special_range() {
        for ip in [
            "0.1.2.3",
            "10.1.2.3",
            "100.64.0.1",
            "100.100.100.200",
            "127.255.1.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.1.1",
            "240.0.0.1",
            "255.255.255.255",
            "168.63.129.16",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "100::1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "2002:7f00:1::",
            "3fff::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }
    struct FakeDns;
    #[async_trait]
    impl Dns for FakeDns {
        async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
            Ok(vec![SocketAddr::new(
                if host == "private.example" {
                    "127.0.0.1"
                } else {
                    "8.8.8.8"
                }
                .parse()?,
                port,
            )])
        }
    }
    struct Redirect {
        calls: AtomicUsize,
        target: &'static str,
    }
    #[async_trait]
    impl Transport for Redirect {
        async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
            assert!(addresses.iter().all(|s| public_ip(s.ip())));
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Validators::default(),
                url: url.clone(),
                status: 302,
                retry_after_secs: 60,
                location: Some(self.target.into()),
                content_type: "text/html".into(),
                body: vec![],
            })
        }
    }
    #[tokio::test]
    async fn redirects_recheck_dns_and_stop_at_three() {
        let cfg = EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        };
        let transport = Arc::new(Redirect {
            calls: AtomicUsize::new(0),
            target: "http://private.example/",
        });
        let egress = Egress::with_backends(cfg.clone(), Arc::new(FakeDns), transport.clone());
        assert!(egress
            .fetch("https://public.example/")
            .await
            .unwrap_err()
            .to_string()
            .contains("non-public"));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        let transport = Arc::new(Redirect {
            calls: AtomicUsize::new(0),
            target: "https://public.example/",
        });
        let egress = Egress::with_backends(cfg, Arc::new(FakeDns), transport.clone());
        assert!(egress
            .fetch("https://public.example/")
            .await
            .unwrap_err()
            .to_string()
            .contains("3 redirects"));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 4);
    }
    #[test]
    fn allowlist_never_matches_suffix_impostors() {
        assert!(host_matches("a.example.org", "*.example.org"));
        assert!(!host_matches("evilexample.org", "*.example.org"));
    }
    #[tokio::test]
    async fn body_cap_catches_streams_and_content_length() {
        use axum::{
            body::Body,
            http::{header, Response},
            routing::get,
            Router,
        };
        for length_header in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/",
                get(move || async move {
                    let mut response =
                        Response::builder().header(header::CONTENT_TYPE, "text/html");
                    if length_header {
                        response =
                            response.header(header::CONTENT_LENGTH, (BODY_CAP + 1).to_string());
                    }
                    let chunks = futures_util::stream::iter(
                        (0..1025).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024])),
                    );
                    response.body(Body::from_stream(chunks)).unwrap()
                }),
            );
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let url = Url::parse(&format!("http://{addr}/")).unwrap();
            let response = reqwest::get(url.clone()).await.unwrap();
            let f = read_response(url, response).await.unwrap();
            assert!(f.truncated);
            assert_eq!(f.body.len(), BODY_CAP);
            task.abort();
        }
    }
    #[tokio::test]
    async fn mixed_dns_answers_literals_and_allowlists_cannot_bypass_ip_policy() {
        struct MixedDns;
        #[async_trait]
        impl Dns for MixedDns {
            async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
                Ok(vec![
                    SocketAddr::new("8.8.8.8".parse()?, port),
                    SocketAddr::new("10.0.0.1".parse()?, port),
                ])
            }
        }
        let transport = Arc::new(Redirect {
            calls: AtomicUsize::new(0),
            target: "/",
        });
        let cfg = EgressConfig {
            allow_hosts: vec!["*.example.org".into()],
            ..Default::default()
        };
        let egress = Egress::with_backends(cfg, Arc::new(MixedDns), transport.clone());
        assert!(egress
            .fetch("https://a.example.org")
            .await
            .unwrap_err()
            .to_string()
            .contains("non-public"));
        assert!(egress
            .fetch("https://example.net")
            .await
            .unwrap_err()
            .to_string()
            .contains("operator"));
        let egress = Egress::with_backends(
            EgressConfig::default(),
            Arc::new(FakeDns),
            transport.clone(),
        );
        for uri in [
            "http://127.1/",
            "http://0x7f000001/",
            "http://2130706433/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::1]/",
            "file:///etc/passwd",
        ] {
            assert!(egress.fetch(uri).await.is_err(), "{uri}");
        }
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn transport_pins_dns_sends_fixed_agent_and_does_not_follow_or_store_cookies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = vec![0; 4096];
            let n = socket.read(&mut data).await.unwrap();
            let request = String::from_utf8_lossy(&data[..n]).to_lowercase();
            assert!(request.contains(&format!("user-agent: {}", USER_AGENT.to_lowercase())));
            assert!(!request.contains("cookie:"));
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/forbidden\r\nSet-Cookie: test=secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let url = Url::parse(&format!("http://never-resolve.invalid:{}/", addr.port())).unwrap();
        let response = PinnedTransport.get(&url, &[addr]).await.unwrap();
        assert_eq!(response.status, 302);
        task.await.unwrap();
    }
    #[tokio::test]
    async fn every_link_header_is_kept() {
        use axum::{http::HeaderMap, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    get(|| async {
                        let mut headers = HeaderMap::new();
                        headers.insert("content-type", "text/html".parse().unwrap());
                        for value in [
                            "<https://w.example/wp-json/>; rel=\"https://api.w.org/\"",
                            "<https://w.example/wp-json/wp/v2/posts/7>; rel=\"alternate\"; type=\"application/json\"",
                        ] {
                            headers.append("link", value.parse().unwrap());
                        }
                        headers.append("link", "x".repeat(3000).parse().unwrap());
                        (headers, "<html></html>")
                    }),
                ),
            )
            .await
            .unwrap()
        });
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let fetched = read_response(url.clone(), reqwest::get(url).await.unwrap())
            .await
            .unwrap();
        // Whole headers only, up to 2 KB.
        assert_eq!(
            fetched.validators.link.as_deref(),
            Some("<https://w.example/wp-json/>; rel=\"https://api.w.org/\", <https://w.example/wp-json/wp/v2/posts/7>; rel=\"alternate\"; type=\"application/json\"")
        );
        task.abort();
    }
    #[tokio::test]
    async fn non_allowlisted_content_type_is_refused_before_reading() {
        use axum::{routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    get(|| async { ([("content-type", "application/octet-stream")], "binary") }),
                ),
            )
            .await
            .unwrap()
        });
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let error = read_response(url.clone(), reqwest::get(url.clone()).await.unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("content type"));
        // A refusal by policy, not broken content.
        assert_eq!(super::super::error_class(&error), "policy");
        // Pubky object fetches accept it.
        let options = FetchOptions {
            extra_types: &["application/octet-stream"],
            ..Default::default()
        };
        let fetched = read_response_with(url.clone(), reqwest::get(url).await.unwrap(), &options)
            .await
            .unwrap();
        assert_eq!(fetched.body, b"binary");
        task.abort();
    }
}

#[cfg(test)]
mod osm_policy_tests {
    use super::*;
    #[test]
    fn osm_hosts_and_disabled_canaries_are_refused() {
        let egress = Egress::new(EgressConfig::default());
        for uri in [
            "https://www.openstreetmap.org/user/alice",
            "https://osm.org/",
            "https://wiki.openstreetmap.org./node/1",
        ] {
            assert!(!egress.allows(uri));
        }
        assert!(egress.allows("https://openstreetmap.org.example.net/"));
        let web = crate::resolver::web::WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            events: None,
            egress: Egress::new(EgressConfig {
                allow_hosts: vec!["only.example".into()],
                ..Default::default()
            }),
            config: Default::default(),
        };
        assert!(!crate::resolver::Resolver::canary_allowed(&web));
        assert!(crate::adapter::manifest("web").expected_fields.is_empty());
    }
}

#[cfg(test)]
mod relay_protocol_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        tungstenite::{protocol::Role, Message},
        WebSocketStream,
    };
    #[tokio::test]
    async fn eose_closes_subscription_incomplete_and_stalled_relays_fail() {
        for complete in [true, false] {
            let (a, b) = tokio::io::duplex(4096);
            let mut client = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
            let task = tokio::spawn(async move {
                let req = server.next().await.unwrap().unwrap().into_text().unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&req).unwrap()[0],
                    "REQ"
                );
                server
                    .send(Message::Text(r#"["EVENT","other",{}]"#.into()))
                    .await
                    .unwrap();
                server
                    .send(Message::Text(
                        r#"["EVENT","pinakes",{"id":"fixture"}]"#.into(),
                    ))
                    .await
                    .unwrap();
                server
                    .send(Message::Text(
                        if complete {
                            r#"["EOSE","pinakes"]"#
                        } else {
                            r#"["CLOSED","pinakes","denied"]"#
                        }
                        .into(),
                    ))
                    .await
                    .unwrap();
                let close = server.next().await.unwrap().unwrap().into_text().unwrap();
                assert_eq!(close, r#"["CLOSE","pinakes"]"#);
            });
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                nostr_subscription(&mut client, serde_json::json!({"kinds":[31923]})),
            )
            .await
            .unwrap();
            if complete {
                assert_eq!(result.unwrap().len(), 1)
            } else {
                assert!(result.is_err())
            }
            task.await.unwrap();
        }
        let (a, _b) = tokio::io::duplex(4096);
        let mut socket = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            nostr_subscription(&mut socket, serde_json::json!({}))
        )
        .await
        .is_err());
    }
}
