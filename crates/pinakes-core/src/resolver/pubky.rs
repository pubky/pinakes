use super::{
    egress::{Egress, FetchOptions},
    pkarr_relays::Relays,
    ErrorClass, Resolver,
};
use crate::{
    config::ResolverConfig,
    model::{ResourceMeta, Status},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use pkarr::PublicKey;
use std::time::Duration;
use url::Url;

pub const ATTRIBUTES: &[&str] = &["content_type", "content_length"];
/// What homeservers label objects with beside the general allowlist: they type a body by its
/// magic bytes or file extension, so JSON without an extension arrives as octet-stream.
pub const OBJECT_TYPES: &[&str] = &["application/octet-stream", "text/plain", ""];
/// Pubky objects are fetched through `Egress` from the homeserver's public HTTPS (ICANN)
/// endpoint, which the owner's PKARR record names; the record comes from the PKARR relays
/// (`[pubky] pkarr_relays`), also through `Egress`, and its signature is checked locally
/// (`pkarr_relays`).
pub struct PubkyResolver {
    egress: Egress,
    relays: Relays,
    config: ResolverConfig,
}
impl PubkyResolver {
    pub fn new(config: ResolverConfig, egress: Egress) -> Result<Self> {
        Ok(Self {
            relays: Relays::new(&config.pkarr_relays, egress.clone())?,
            egress,
            config,
        })
    }
}

#[async_trait]
impl Resolver for PubkyResolver {
    fn name(&self) -> &'static str {
        "pubky"
    }
    fn matches(&self, uri: &str) -> bool {
        super::classify(uri) == Some("pubky")
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let object = Url::parse(uri)?;
            // A path an app's profile does not read (one stored before the profile excluded it)
            // is never fetched: it keeps no card.
            if super::pubky_apps::excluded(uri) {
                let mut doc = ResourceMeta::new(uri, "pubky", self.config.ttl_secs);
                doc.set_type("CreativeWork");
                doc.status = Status::Missing;
                doc.withheld = true;
                return Ok(doc);
            }
            // Only objects of other apps, by their decoded path (`classify`).
            if super::classify(uri) != Some("pubky") {
                return Err(anyhow::Error::new(ErrorClass("policy"))
                    .context("not a Pubky object of another app"));
            }
            let host = object.host_str().context("missing Pubky owner")?;
            // One spelling per object: the owner's key as the whole host, in z32.
            let owner = host
                .parse::<PublicKey>()
                .ok()
                .filter(|owner| owner.to_z32() == host)
                .ok_or_else(|| {
                    anyhow::Error::new(ErrorClass("policy"))
                        .context("the Pubky owner is not a canonical z32 key")
                })?;
            let (domain, port) = self.relays.homeserver(&owner).await?;
            // The `pubky-host` form the SDK uses for ICANN endpoints. Egress pins the checked
            // addresses, refuses non-public ones and, because of the header, every redirect.
            let mut url = Url::parse(&format!("https://{domain}/"))
                .map_err(|_| ErrorClass("policy"))
                .context("homeserver domain")?;
            // A domain only: an IP literal as the target names no homeserver.
            if !matches!(url.host(), Some(url::Host::Domain(_))) {
                return Err(anyhow::Error::new(ErrorClass("policy"))
                    .context("the homeserver endpoint is an IP address, not a domain"));
            }
            url.set_port(port)
                .map_err(|_| anyhow::Error::new(ErrorClass("policy")))?;
            url.set_path(object.path());
            // In a `pubky:` URL a backslash or a dot segment is literal, in an `https:` one it is
            // not: the path fetched must be the path classified, or the namespace check is moot.
            if url.path() != object.path() {
                return Err(anyhow::Error::new(ErrorClass("policy"))
                    .context("the Pubky object path changes as an HTTPS path"));
            }
            // Homeservers serve many objects as `application/octet-stream`; `card` reads JSON
            // whatever the declared type.
            let options = FetchOptions {
                headers: vec![("pubky-host", owner.to_z32())],
                accept: Some(format!(
                    "{}, application/octet-stream",
                    super::egress::ALLOWED_TYPES.join(", ")
                )),
                extra_types: OBJECT_TYPES,
                ..Default::default()
            };
            let fetched = self.egress.fetch_with(url.as_str(), &options).await?;
            let mut doc = ResourceMeta::new(uri, "pubky", self.config.ttl_secs);
            doc.set_type("CreativeWork");
            match fetched.status {
                200..=299 => {}
                404 | 410 => {
                    doc.status = Status::Missing;
                    return Ok(doc);
                }
                401 | 403 => {
                    doc.status = Status::Unreachable;
                    return Ok(doc);
                }
                status => {
                    return Err(anyhow::Error::new(ErrorClass(if status >= 500 {
                        "http_5xx"
                    } else {
                        "http_4xx"
                    }))
                    .context(crate::resolver::Upstream(status)))
                }
            }
            let doc = super::pubky_apps::card_with(
                uri,
                &fetched.content_type,
                &fetched.body,
                self.config.ttl_secs,
                chrono::Utc::now(),
                self.config.max_period_dates,
            )
            .bounded(super::pubky_apps::attributes())?;
            crate::telemetry::document(crate::telemetry::last_fetch(), &self.manifest(), &doc);
            Ok(doc)
        })
        .await?
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        Duration::from_secs(doc.ttl_secs)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::egress::{Dns, Fetched, Transport, Validators};
    use pkarr::{dns::rdata::SVCB, Keypair, SignedPacket};
    use std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    #[test]
    fn conventional_card_and_namespace_matching() {
        let uri =
            "pubky://ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi/pub/other.app/item";
        let doc=super::super::pubky_apps::card(uri,"application/json",br#"{"title":"A deck","description":"Cards","image":"https://example.org/cover.png","password":"never keep"}"#,10);
        assert_eq!(doc.name.as_deref(), Some("A deck"));
        assert!(!serde_json::to_string(&doc).unwrap().contains("password"));
        assert_eq!(crate::resolver::classify(uri), Some("pubky"));
        assert_eq!(
            crate::resolver::classify(&uri.replace("other.app", "pubky.app")),
            None
        );
        assert_eq!(crate::resolver::classify("geo:1,2"), Some("geo"));
        assert_eq!(
            crate::resolver::classify("https://www.openstreetmap.org/node/1"),
            Some("osm")
        );
    }

    /// Public addresses for the relays, `homeserver` for the homeserver's domain.
    struct FixedDns {
        homeserver: Vec<SocketAddr>,
        relay: Vec<SocketAddr>,
    }
    #[async_trait]
    impl Dns for FixedDns {
        async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
            match (host, port) {
                ("homeserver.example", 9443 | 443) => Ok(self.homeserver.clone()),
                ("relay.example" | "relay2.example", 443) => Ok(self.relay.clone()),
                other => panic!("unexpected lookup {other:?}"),
            }
        }
    }
    /// The PKARR relays (each key's packet, as relays serve it) and the homeserver.
    struct Net {
        packets: HashMap<String, Vec<u8>>,
        /// Relay hosts that flip a byte of every payload.
        tampering: Vec<&'static str>,
        /// Relay hosts that answer 404.
        empty: Mutex<Vec<&'static str>>,
        /// What one relay host serves for one key instead.
        serve: Mutex<HashMap<(&'static str, String), Vec<u8>>>,
        relay_requests: Mutex<Vec<String>>,
        homeserver: Arc<dyn Transport>,
    }
    #[async_trait]
    impl Transport for Net {
        async fn get(&self, _: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            unreachable!("Egress sends its options")
        }
        async fn get_with(
            &self,
            url: &Url,
            addresses: &[SocketAddr],
            options: &FetchOptions,
        ) -> Result<Fetched> {
            let host = url.host_str().unwrap();
            if !host.starts_with("relay") {
                return self.homeserver.get_with(url, addresses, options).await;
            }
            self.relay_requests.lock().unwrap().push(url.to_string());
            let key = url.path().trim_start_matches('/');
            let served = self
                .serve
                .lock()
                .unwrap()
                .iter()
                .find(|((h, k), _)| *h == host && k == key)
                .map(|(_, payload)| payload.clone());
            let empty = self.empty.lock().unwrap().contains(&host);
            let response = match served
                .or_else(|| self.packets.get(key).cloned())
                .filter(|_| !empty)
            {
                Some(mut payload) => {
                    if self.tampering.contains(&host) {
                        *payload.last_mut().unwrap() ^= 1;
                    }
                    axum::http::Response::builder()
                        .status(200)
                        .header("content-type", "application/pkarr.org/relays#payload")
                        .body(payload)
                }
                None => axum::http::Response::builder().status(404).body(vec![]),
            };
            let response = reqwest::Response::from(response.unwrap());
            crate::resolver::egress::read_response_with(url.clone(), response, options).await
        }
    }
    /// A request's URL and headers.
    type Request = (String, Vec<(&'static str, String)>);
    /// Answers every request with `status` (and a `Location` for a redirect), recording it.
    #[derive(Default)]
    struct Homeserver {
        status: u16,
        body: &'static str,
        requests: Mutex<Vec<Request>>,
    }
    #[async_trait]
    impl Transport for Homeserver {
        async fn get(&self, _: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            unreachable!("Pubky fetches carry the pubky-host header")
        }
        async fn get_with(
            &self,
            url: &Url,
            _: &[SocketAddr],
            options: &FetchOptions,
        ) -> Result<Fetched> {
            self.requests
                .lock()
                .unwrap()
                .push((url.to_string(), options.headers.clone()));
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                validators: Validators::default(),
                url: url.clone(),
                // As homeservers serve JSON without an extension.
                content_type: "application/octet-stream".into(),
                status: self.status,
                retry_after_secs: 0,
                location: Some("https://elsewhere.example/pub/app/item".into()),
                body: self.body.as_bytes().to_vec(),
            })
        }
    }
    /// `name`'s HTTPS record points at `target` (`.` for the packet's own addresses).
    fn https(
        key: &Keypair,
        name: &str,
        target: &str,
        port: u16,
        addresses: &[&str],
    ) -> SignedPacket {
        let mut service = SVCB::new(1, target.try_into().unwrap());
        if port > 0 {
            service.set_port(port);
        }
        let mut packet = SignedPacket::builder().https(name.try_into().unwrap(), service, 3600);
        for address in addresses {
            packet = packet.address(".".try_into().unwrap(), address.parse().unwrap(), 3600);
        }
        packet.sign(key).unwrap()
    }
    fn network(packets: &[SignedPacket], homeserver: Arc<dyn Transport>) -> Arc<Net> {
        Arc::new(Net {
            packets: packets
                .iter()
                .map(|p| (p.public_key().to_z32(), p.to_relay_payload().to_vec()))
                .collect(),
            tampering: vec![],
            empty: Mutex::default(),
            serve: Mutex::default(),
            relay_requests: Mutex::default(),
            homeserver,
        })
    }
    fn resolver_on(net: Arc<Net>, dns: &[&str], relay: &str, relays: &[&str]) -> PubkyResolver {
        let addresses = |list: &[&str], port| {
            list.iter()
                .map(|a| SocketAddr::new(a.parse().unwrap(), port))
                .collect()
        };
        let dns = FixedDns {
            homeserver: addresses(dns, 9443),
            relay: addresses(&[relay], 443),
        };
        // No per-host pacing in tests: the relays and the homeserver answer at once.
        let cfg = crate::config::EgressConfig {
            host_requests_per_second: 1_000_000.0,
            ..Default::default()
        };
        let egress = Egress::with_backends(cfg, Arc::new(dns), net);
        let config = ResolverConfig {
            pkarr_relays: relays.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        };
        PubkyResolver::new(config, egress).unwrap()
    }
    fn resolver(
        packets: &[SignedPacket],
        dns: &[&str],
        homeserver: Arc<dyn Transport>,
    ) -> PubkyResolver {
        resolver_on(
            network(packets, homeserver),
            dns,
            "8.8.4.4",
            &["https://relay.example"],
        )
    }
    fn object(owner: &Keypair) -> String {
        format!("pubky://{}/pub/other.app/item", owner.public_key())
    }

    #[tokio::test]
    async fn objects_come_through_egress_from_the_homeservers_https_domain() {
        // The owner delegates to its homeserver's key, which names its ICANN domain.
        let (owner, server) = (Keypair::random(), Keypair::random());
        let delegation = https(&owner, "_pubky", &server.public_key().to_string(), 0, &[]);
        let endpoint = https(&server, ".", "homeserver.example", 9443, &[]);
        let homeserver = Arc::new(Homeserver {
            status: 200,
            body: r#"{"name":"A deck"}"#,
            ..Default::default()
        });
        let doc = resolver(&[delegation, endpoint], &["8.8.8.8"], homeserver.clone())
            .resolve(&object(&owner))
            .await
            .unwrap();
        assert_eq!(doc.name.as_deref(), Some("A deck"));
        assert_eq!(doc.url, object(&owner));
        assert_eq!(
            *homeserver.requests.lock().unwrap(),
            [(
                "https://homeserver.example:9443/pub/other.app/item".to_owned(),
                vec![("pubky-host", owner.public_key().to_z32())]
            )]
        );
    }

    #[tokio::test]
    async fn a_homeserver_domain_on_a_private_address_is_refused() {
        let owner = Keypair::random();
        for address in ["127.0.0.1", "10.0.0.1", "169.254.169.254", "::1"] {
            let homeserver = Arc::new(Homeserver::default());
            let packet = https(&owner, "_pubky", "homeserver.example", 9443, &[]);
            let error = resolver(&[packet], &["8.8.8.8", address], homeserver.clone())
                .resolve(&object(&owner))
                .await
                .unwrap_err();
            assert_eq!(crate::resolver::error_class(&error), "policy", "{address}");
            assert!(homeserver.requests.lock().unwrap().is_empty(), "{address}");
        }
    }

    #[tokio::test]
    async fn an_ip_literal_endpoint_is_refused() {
        let owner = Keypair::random();
        for literal in ["127.0.0.1", "8.8.8.8"] {
            let homeserver = Arc::new(Homeserver::default());
            let packet = https(&owner, "_pubky", literal, 9443, &[]);
            let error = resolver(&[packet], &[], homeserver.clone())
                .resolve(&object(&owner))
                .await
                .unwrap_err();
            assert_eq!(crate::resolver::error_class(&error), "policy", "{literal}");
            assert!(format!("{error:#}").contains("not a domain"), "{error:#}");
            assert!(homeserver.requests.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn classify_reads_the_path_as_the_homeserver_does() {
        let key = Keypair::random().public_key().to_z32();
        for uri in [
            format!("pubky://{key}/pub/a.app/x%2F..%2F..%2Fpubky.app%2Fposts%2F0034A0X7NJ52G"),
            format!("pubky://{key}/pub/%70ubky.app/posts/0034A0X7NJ52G"),
            format!("pubky://{key}/pub/a.app/x%2F..%2F..%2F..%2Fpriv%2Fapp.locks%2Ff"),
            format!("pubky://{key}/pub/a.app/x%5C..%5Cf"),
            format!("pubky://{key}/pub//x"),
            format!("pubky://{key}/priv/a.app/x"),
        ] {
            assert_eq!(crate::resolver::classify(&uri), None, "{uri}");
        }
        for uri in [
            format!("pubky://{key}/pub/a.app/a%20b"),
            format!("pubky://{key}/pub/%61.app/x"),
        ] {
            assert_eq!(crate::resolver::classify(&uri), Some("pubky"), "{uri}");
        }
    }

    #[tokio::test]
    async fn an_object_outside_its_own_spelling_is_refused() {
        let owner = Keypair::random();
        let key = owner.public_key().to_z32();
        for uri in [
            // `\` is literal in `pubky:` and a separator in `https:`: this would fetch
            // `/priv/app.locks/f`.
            format!("pubky://{key}/pub/other.app/x\\..\\..\\..\\priv/app.locks/f"),
            format!("pubky://anything.{key}/pub/other.app/item"),
            format!("pubky://{}/pub/other.app/item", key.to_uppercase()),
            // A homeserver decodes the path and resolves dot segments: these would read a
            // pubky.app post or a private file.
            format!("pubky://{key}/pub/a.app/x%2F..%2F..%2Fpubky.app%2Fposts%2F0034A0X7NJ52G"),
            format!("pubky://{key}/pub/%70ubky.app/posts/0034A0X7NJ52G"),
            format!("pubky://{key}/pub/a.app/x%2F..%2F..%2F..%2Fpriv%2Fapp.locks%2Ff"),
            format!("pubky://{key}/pub/a.app/x%5C..%5C..%5Cpriv/f"),
            format!("pubky://{key}/pub/a.app/%2E%2E/%2E%2E/priv/f"),
        ] {
            let homeserver = Arc::new(Homeserver::default());
            let packet = https(&owner, "_pubky", "homeserver.example", 9443, &[]);
            let error = resolver(&[packet], &["8.8.8.8"], homeserver.clone())
                .resolve(&uri)
                .await
                .unwrap_err();
            assert_eq!(
                crate::resolver::error_class(&error),
                "policy",
                "{uri}: {error:#}"
            );
            assert!(homeserver.requests.lock().unwrap().is_empty(), "{uri}");
        }
    }

    #[tokio::test]
    async fn a_path_an_apps_profile_does_not_read_is_missing_and_never_fetched() {
        let owner = Keypair::random();
        let key = owner.public_key().to_z32();
        for path in ["loopky/settings.json", "pubky-rooms/messages/a/b/c"] {
            let homeserver = Arc::new(Homeserver::default());
            let packet = https(&owner, "_pubky", "homeserver.example", 9443, &[]);
            let doc = resolver(&[packet], &["8.8.8.8"], homeserver.clone())
                .resolve(&format!("pubky://{key}/pub/{path}"))
                .await
                .unwrap();
            assert_eq!(doc.status, Status::Missing, "{path}");
            assert_eq!((doc.name, doc.description), (None, None));
            assert!(homeserver.requests.lock().unwrap().is_empty(), "{path}");
        }
    }

    #[tokio::test]
    async fn a_homeserver_redirect_is_refused() {
        let owner = Keypair::random();
        for status in [301, 302, 307, 308] {
            let homeserver = Arc::new(Homeserver {
                status,
                ..Default::default()
            });
            let packet = https(&owner, "_pubky", "homeserver.example", 9443, &[]);
            let error = resolver(&[packet], &["8.8.8.8"], homeserver.clone())
                .resolve(&object(&owner))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("redirect"),
                "{status}: {error:#}"
            );
            assert_eq!(crate::resolver::error_class(&error), "policy", "{status}");
            assert_eq!(homeserver.requests.lock().unwrap().len(), 1, "{status}");
        }
    }

    #[tokio::test]
    async fn a_homeserver_with_only_key_pinned_endpoints_is_unreachable() {
        let owner = Keypair::random();
        let homeserver = Arc::new(Homeserver::default());
        let packet = https(&owner, "_pubky", ".", 443, &["8.8.8.8"]);
        let error = resolver(&[packet], &[], homeserver.clone())
            .resolve(&object(&owner))
            .await
            .unwrap_err();
        assert_eq!(crate::resolver::error_class(&error), "connect");
        assert!(format!("{error:#}").contains("key-pinned"), "{error:#}");
        assert!(homeserver.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_and_closed_objects_keep_their_status() {
        let owner = Keypair::random();
        for (status, expected) in [
            (404, Status::Missing),
            (410, Status::Missing),
            (403, Status::Unreachable),
        ] {
            let homeserver = Arc::new(Homeserver {
                status,
                ..Default::default()
            });
            let packet = https(&owner, "_pubky", "homeserver.example", 9443, &[]);
            let doc = resolver(&[packet], &["8.8.8.8"], homeserver)
                .resolve(&object(&owner))
                .await
                .unwrap();
            assert_eq!(doc.status, expected, "{status}");
            assert_eq!(doc.type_name, "CreativeWork", "{status}");
        }
    }

    /// A homeserver answering 200 with `content_type` and `body`, read by the real egress rules.
    struct Served(&'static str, &'static [u8]);
    #[async_trait]
    impl Transport for Served {
        async fn get(&self, _: &Url, _: &[SocketAddr]) -> Result<Fetched> {
            unreachable!("Pubky fetches carry the pubky-host header")
        }
        async fn get_with(
            &self,
            url: &Url,
            _: &[SocketAddr],
            options: &FetchOptions,
        ) -> Result<Fetched> {
            let mut response = axum::http::Response::builder().status(200);
            if !self.0.is_empty() {
                response = response.header("content-type", self.0);
            }
            let response = reqwest::Response::from(response.body(self.1.to_vec()).unwrap());
            crate::resolver::egress::read_response_with(url.clone(), response, options).await
        }
    }
    fn served(content_type: &'static str, body: &'static [u8]) -> (PubkyResolver, String) {
        let owner = Keypair::random();
        let packet = https(&owner, "_pubky", "homeserver.example", 9443, &[]);
        let resolver = resolver(
            &[packet],
            &["8.8.8.8"],
            Arc::new(Served(content_type, body)),
        );
        (resolver, object(&owner))
    }

    #[tokio::test]
    async fn objects_served_as_octet_stream_are_read_as_json() {
        for content_type in [
            "application/octet-stream",
            "text/plain",
            "",
            "application/json",
        ] {
            let (resolver, uri) = served(content_type, br#"{"name":"A review","rating":5}"#);
            let doc = resolver.resolve(&uri).await.unwrap();
            assert_eq!(doc.status, Status::Ok, "{content_type:?}");
            assert_eq!(doc.name.as_deref(), Some("A review"), "{content_type:?}");
        }
        let (resolver, uri) = served("application/octet-stream", br#"{"content":"A post"}"#);
        let doc = resolver.resolve(&uri).await.unwrap();
        assert_eq!(doc.description.as_deref(), Some("A post"));
        assert_eq!(
            doc.attributes["content_type"], "application/octet-stream",
            "{doc:?}"
        );
    }

    #[tokio::test]
    async fn a_binary_object_is_a_plain_card_and_other_types_are_refused() {
        let (resolver, uri) = served("application/octet-stream", b"\x89PNG\r\n\x1a\n\0\0");
        let doc = resolver.resolve(&uri).await.unwrap();
        assert_eq!(doc.status, Status::Ok);
        assert_eq!(doc.name, None);
        assert_eq!(doc.attributes["content_length"], "10");
        for content_type in ["video/mp4", "application/zip"] {
            let (resolver, uri) = served(content_type, b"x");
            let error = resolver.resolve(&uri).await.unwrap_err();
            assert_eq!(
                crate::resolver::error_class(&error),
                "policy",
                "{content_type}: {error:#}"
            );
        }
    }

    fn direct(owner: &Keypair) -> SignedPacket {
        https(owner, "_pubky", "homeserver.example", 9443, &[])
    }
    fn ok() -> Arc<Homeserver> {
        Arc::new(Homeserver {
            status: 200,
            body: r#"{"name":"A deck"}"#,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn records_come_verified_from_a_relay_and_are_cached() {
        let owner = Keypair::random();
        let homeserver = ok();
        let net = network(&[direct(&owner)], homeserver.clone());
        let resolver = resolver_on(
            net.clone(),
            &["8.8.8.8"],
            "8.8.4.4",
            &["https://relay.example"],
        );
        for _ in 0..3 {
            let doc = resolver.resolve(&object(&owner)).await.unwrap();
            assert_eq!(doc.name.as_deref(), Some("A deck"));
        }
        // One relay request for three objects: the packet lives for its TTL.
        assert_eq!(
            *net.relay_requests.lock().unwrap(),
            [format!(
                "https://relay.example/{}",
                owner.public_key().to_z32()
            )]
        );
        assert_eq!(homeserver.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_packet_that_does_not_verify_is_refused() {
        let owner = Keypair::random();
        let homeserver = ok();
        let mut net = network(&[direct(&owner)], homeserver.clone());
        Arc::get_mut(&mut net).unwrap().tampering = vec!["relay.example"];
        let error = resolver_on(
            net.clone(),
            &["8.8.8.8"],
            "8.8.4.4",
            &["https://relay.example"],
        )
        .resolve(&object(&owner))
        .await
        .unwrap_err();
        assert_eq!(crate::resolver::error_class(&error), "connect", "{error:#}");
        assert!(
            format!("{error:#}").contains("does not verify"),
            "{error:#}"
        );
        assert!(homeserver.requests.lock().unwrap().is_empty());
        // The next relay's genuine packet is used.
        let doc = resolver_on(
            net,
            &["8.8.8.8"],
            "8.8.4.4",
            &["https://relay.example", "https://relay2.example"],
        )
        .resolve(&object(&owner))
        .await
        .unwrap();
        assert_eq!(doc.name.as_deref(), Some("A deck"));
    }

    #[tokio::test]
    async fn a_relay_without_the_record_answers_404() {
        let owner = Keypair::random();
        let homeserver = ok();
        // No packet anywhere: no record, and no request to any homeserver.
        let net = network(&[], homeserver.clone());
        let error = resolver_on(net, &["8.8.8.8"], "8.8.4.4", &["https://relay.example"])
            .resolve(&object(&owner))
            .await
            .unwrap_err();
        assert_eq!(crate::resolver::error_class(&error), "dns", "{error:#}");
        assert!(
            format!("{error:#}").contains("no PKARR record"),
            "{error:#}"
        );
        // A relay that has no record is followed by one that has.
        let net = network(&[direct(&owner)], homeserver.clone());
        *net.empty.lock().unwrap() = vec!["relay.example"];
        let doc = resolver_on(
            net.clone(),
            &["8.8.8.8"],
            "8.8.4.4",
            &["https://relay.example", "https://relay2.example"],
        )
        .resolve(&object(&owner))
        .await
        .unwrap();
        assert_eq!(doc.name.as_deref(), Some("A deck"));
        assert_eq!(net.relay_requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_relay_on_a_private_address_is_refused_by_egress() {
        let owner = Keypair::random();
        for address in ["10.0.0.1", "127.0.0.1", "169.254.169.254"] {
            let homeserver = ok();
            let net = network(&[direct(&owner)], homeserver.clone());
            let error = resolver_on(
                net.clone(),
                &["8.8.8.8"],
                address,
                &["https://relay.example"],
            )
            .resolve(&object(&owner))
            .await
            .unwrap_err();
            assert_eq!(crate::resolver::error_class(&error), "connect", "{address}");
            assert!(net.relay_requests.lock().unwrap().is_empty(), "{address}");
            assert!(homeserver.requests.lock().unwrap().is_empty(), "{address}");
        }
    }

    /// One relay's own (older or newer) packet for the owner.
    fn serve(net: &Net, host: &'static str, packet: &SignedPacket) {
        net.serve.lock().unwrap().insert(
            (host, packet.public_key().to_z32()),
            packet.to_relay_payload().to_vec(),
        );
    }

    #[tokio::test]
    async fn the_newest_packet_any_relay_serves_wins() {
        let owner = Keypair::random();
        // An older record names another homeserver; looking it up would fail the test's DNS.
        let old = https(&owner, "_pubky", "old.example", 9443, &[]);
        let new = direct(&owner);
        assert!(new.timestamp() > old.timestamp());
        let homeserver = ok();
        let net = network(&[], homeserver.clone());
        serve(&net, "relay.example", &old);
        serve(&net, "relay2.example", &new);
        let doc = resolver_on(
            net.clone(),
            &["8.8.8.8"],
            "8.8.4.4",
            &["https://relay.example", "https://relay2.example"],
        )
        .resolve(&object(&owner))
        .await
        .unwrap();
        assert_eq!(doc.name.as_deref(), Some("A deck"));
        // Both relays were asked.
        assert_eq!(net.relay_requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_packet_older_than_one_seen_is_never_used() {
        let owner = Keypair::random();
        let old = https(&owner, "_pubky", "old.example", 9443, &[]);
        let new = direct(&owner);
        let homeserver = ok();
        let net = network(&[], homeserver.clone());
        serve(&net, "relay.example", &new);
        let resolver = resolver_on(
            net.clone(),
            &["8.8.8.8"],
            "8.8.4.4",
            &["https://relay.example"],
        );
        resolver.resolve(&object(&owner)).await.unwrap();
        // The relay now serves the older packet. With the newer one still held, it stays in use.
        serve(&net, "relay.example", &old);
        resolver.relays.expire(false);
        let doc = resolver.resolve(&object(&owner)).await.unwrap();
        assert_eq!(doc.name.as_deref(), Some("A deck"));
        // With the newer packet evicted, the older one is still refused: its timestamp is
        // remembered apart from the cache.
        resolver.relays.expire(true);
        let error = resolver.resolve(&object(&owner)).await.unwrap_err();
        assert_eq!(crate::resolver::error_class(&error), "connect", "{error:#}");
        assert!(format!("{error:#}").contains("older"), "{error:#}");
        assert_eq!(homeserver.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_relay_payload_over_eight_kilobytes_is_refused() {
        let owner = Keypair::random();
        let homeserver = ok();
        let net = network(&[], homeserver.clone());
        net.serve.lock().unwrap().insert(
            ("relay.example", owner.public_key().to_z32()),
            vec![0; 9 * 1024],
        );
        let error = resolver_on(net, &["8.8.8.8"], "8.8.4.4", &["https://relay.example"])
            .resolve(&object(&owner))
            .await
            .unwrap_err();
        assert_eq!(crate::resolver::error_class(&error), "connect", "{error:#}");
        assert!(format!("{error:#}").contains("exceeds"), "{error:#}");
        assert!(homeserver.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_alias_mode_record_names_only_its_target() {
        let owner = Keypair::random();
        // Priority 0 (AliasMode): the port parameter is ignored, so the default port is used.
        let mut alias = SVCB::new(0, "homeserver.example".try_into().unwrap());
        alias.set_port(1234);
        let packet = SignedPacket::builder()
            .https("_pubky".try_into().unwrap(), alias, 3600)
            .sign(&owner)
            .unwrap();
        let homeserver = ok();
        resolver(&[packet], &["8.8.8.8"], homeserver.clone())
            .resolve(&object(&owner))
            .await
            .unwrap();
        assert!(homeserver.requests.lock().unwrap()[0]
            .0
            .starts_with("https://homeserver.example/pub/"));
        // An alias to `.` says the service is unavailable: no endpoint at all.
        let unavailable = SignedPacket::builder()
            .https(
                "_pubky".try_into().unwrap(),
                SVCB::new(0, ".".try_into().unwrap()),
                3600,
            )
            .sign(&owner)
            .unwrap();
        let error = resolver(&[unavailable], &[], ok())
            .resolve(&object(&owner))
            .await
            .unwrap_err();
        assert_eq!(crate::resolver::error_class(&error), "dns", "{error:#}");
    }
}
