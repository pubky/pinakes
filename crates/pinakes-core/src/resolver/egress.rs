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

pub const BODY_CAP: usize = 1024 * 1024;
pub const USER_AGENT: &str = concat!(
    "pinakes/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/pubky/pinakes)"
);
pub const ALLOWED_TYPES: &[&str] = &[
    "text/html",
    "application/xhtml+xml",
    "application/json",
    "application/ld+json",
    "application/activity+json",
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
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct FetchOptions {
    pub accept: Option<String>,
    pub user_agent: Option<String>,
    pub feed_cap: Option<usize>,
    pub feed_hint: bool,
    pub validators: Validators,
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
        let mut request = client.get(url.clone()).header(
            "accept",
            options
                .accept
                .clone()
                .unwrap_or_else(|| ALLOWED_TYPES.join(", ")),
        );
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
    let validators = Validators {
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
        let possible = options.feed_cap.is_some()
            && (feed_type
                || matches!(
                    content_type.as_str(),
                    "text/plain" | "application/octet-stream" | ""
                ));
        if !ALLOWED_TYPES.contains(&content_type.as_str()) && !possible {
            bail!("content type is not allowed");
        }
        let max = options.feed_cap.unwrap_or(BODY_CAP);
        if response.content_length().is_some_and(|n| n > max as u64) {
            bail!("body exceeds cap (pages 1 MB)");
        }
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > max {
                bail!("body exceeds cap (pages 1 MB)");
            }
            body.extend_from_slice(&chunk);
            let sniff = String::from_utf8_lossy(&body[..body.len().min(256)])
                .trim_start_matches(['\u{feff}', ' ', '\r', '\n'])
                .starts_with("BEGIN:VCALENDAR");
            if body.len() > BODY_CAP
                && !(options.feed_cap.is_some() && (feed_type || options.feed_hint || sniff))
            {
                bail!("body exceeds 1 MB");
            }
        }
        let sniff = String::from_utf8_lossy(&body[..body.len().min(256)])
            .trim_start_matches(['\u{feff}', ' ', '\r', '\n'])
            .starts_with("BEGIN:VCALENDAR");
        if !ALLOWED_TYPES.contains(&content_type.as_str()) && !feed_type && !sniff {
            bail!("content type is not allowed");
        }
    }
    Ok(Fetched {
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
#[derive(Clone)]
pub struct Egress {
    cfg: EgressConfig,
    dns: Arc<dyn Dns>,
    transport: Arc<dyn Transport>,
    hosts: Arc<Mutex<HashMap<String, HostBudget>>>,
}
impl Egress {
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
    pub fn new(cfg: EgressConfig) -> Self {
        Self::with_backends(cfg, Arc::new(SystemDns), Arc::new(PinnedTransport))
    }
    pub fn with_backends(
        cfg: EgressConfig,
        dns: Arc<dyn Dns>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            cfg,
            dns,
            transport,
            hosts: Arc::default(),
        }
    }
    pub async fn fetch(&self, uri: &str) -> Result<Fetched> {
        tokio::time::timeout(Duration::from_secs(10), self.fetch_inner(uri))
            .await
            .context("web resolution exceeded 10 seconds")?
    }
    pub async fn fetch_with(&self, uri: &str, options: &FetchOptions) -> Result<Fetched> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.fetch_options_inner(uri, options),
        )
        .await?
    }
    async fn fetch_inner(&self, uri: &str) -> Result<Fetched> {
        self.fetch_options_inner(uri, &FetchOptions::default())
            .await
    }
    async fn fetch_options_inner(&self, uri: &str, options: &FetchOptions) -> Result<Fetched> {
        let mut url = Url::parse(uri)?;
        for redirects in 0..=3 {
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                bail!("only public HTTP(S) URLs without credentials are allowed");
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
            let _permit = self.host_permit(&host).await;
            let mut observation = crate::telemetry::FetchGuard::start();
            let response = self.transport.get_with(&url, &addresses, options).await?;
            observation.status(response.status);
            drop(observation);
            if response.status == 429 {
                return Err(super::RetryLater {
                    class: "http_4xx",
                    seconds: response.retry_after_secs,
                }
                .into());
            }
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                if redirects == 3 {
                    bail!("more than 3 redirects");
                }
                url = url.join(
                    response
                        .location
                        .as_deref()
                        .context("redirect omitted location")?,
                )?;
                continue;
            }
            return Ok(response);
        }
        unreachable!()
    }
    async fn host_permit(&self, host: &str) -> OwnedMutexGuard<Instant> {
        let rate = self
            .cfg
            .host_rates
            .get(host)
            .copied()
            .unwrap_or(self.cfg.host_requests_per_second);
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
        let mut guard = budget.lock_owned().await;
        tokio::time::sleep_until(*guard).await;
        // Stable per-host jitter avoids synchronized requests without weakening the minimum interval.
        let jitter = f64::from(blake3::hash(host.as_bytes()).as_bytes()[0]) / 2550.0;
        *guard = Instant::now() + Duration::from_secs_f64(1.0 / rate + jitter);
        guard
    }
}
fn host_matches(host: &str, rule: &str) -> bool {
    let rule = rule.to_ascii_lowercase();
    if let Some(suffix) = rule.strip_prefix("*.") {
        host == suffix || host.strip_suffix(suffix).is_some_and(|s| s.ends_with('.'))
    } else {
        host == rule.trim_end_matches('.')
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
            assert!(read_response(url, response)
                .await
                .unwrap_err()
                .to_string()
                .contains("1 MB"));
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
        assert!(read_response(url.clone(), reqwest::get(url).await.unwrap())
            .await
            .unwrap_err()
            .to_string()
            .contains("content type"));
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
