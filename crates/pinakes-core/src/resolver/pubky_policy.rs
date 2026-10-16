use super::{
    egress::{public_ip, Dns, SystemDns},
    ErrorClass,
};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use pkarr::{dns::rdata::RData, Client, PublicKey, ResolvePolicy};
use std::sync::Arc;

/// A preflight, not connection pinning: the SDK still resolves independently afterwards.
pub(super) struct Preflight {
    pkarr: Client,
    dns: Arc<dyn Dns>,
}
impl Preflight {
    pub fn new(pkarr: Client) -> Self {
        Self {
            pkarr,
            dns: Arc::new(SystemDns),
        }
    }

    pub async fn check(&self, uri: &str) -> Result<()> {
        let url = url::Url::parse(uri)?;
        let owner: PublicKey = url.host_str().context("missing Pubky owner")?.parse()?;
        let packet = self
            .pkarr
            .resolve(&owner, ResolvePolicy::CacheFirst)
            .await
            .context(ErrorClass("dns"))?;
        self.check_endpoints(&format!("_pubky.{owner}")).await?;

        // Pubky 0.16.2 also fetches /info at the first _pubky SVCB/HTTPS target
        // when it is a public key. It can differ from the HTTPS storage target.
        let target = packet
            .resource_records("_pubky")
            .find_map(|rr| match &rr.rdata {
                RData::SVCB(svcb) => Some(svcb.target.to_string()),
                RData::HTTPS(https) => Some(https.0.target.to_string()),
                _ => None,
            });
        if let Some(homeserver) = target.and_then(|target| target.parse::<PublicKey>().ok()) {
            self.check_endpoints(&homeserver.to_string()).await?;
        }
        Ok(())
    }

    async fn check_endpoints(&self, qname: &str) -> Result<()> {
        let endpoints = self.pkarr.resolve_https_endpoints(qname);
        futures_util::pin_mut!(endpoints);
        let mut count = 0;
        while let Some(endpoint) = endpoints.next().await {
            count += 1;
            if count > 64 {
                return Err(ErrorClass("policy").into());
            }
            // Avoid the SDK's synchronous DNS lookup on ICANN fallback endpoints.
            let addresses = if let Some(domain) = endpoint.domain() {
                self.dns
                    .lookup(domain, endpoint.port().unwrap_or(443))
                    .await
                    .context(ErrorClass("dns"))?
            } else {
                endpoint.to_socket_addrs()
            };
            if addresses.is_empty() {
                return Err(ErrorClass("dns").into());
            }
            if addresses.iter().any(|address| !public_ip(address.ip())) {
                return Err(ErrorClass("policy").into());
            }
        }
        if count == 0 {
            return Err(ErrorClass("dns").into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pkarr::{dns::rdata::SVCB, Cache, InMemoryCache, Keypair, SignedPacket};
    use std::{net::SocketAddr, num::NonZeroUsize};

    struct MockDns(Vec<SocketAddr>);
    #[async_trait::async_trait]
    impl Dns for MockDns {
        async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
            assert_eq!(host, "homeserver.example");
            assert_eq!(port, 9443);
            Ok(self.0.clone())
        }
    }
    fn fixture(packets: &[SignedPacket], dns: Vec<SocketAddr>) -> Preflight {
        let cache = Arc::new(InMemoryCache::new(NonZeroUsize::new(8).unwrap()));
        for packet in packets {
            cache.put(&packet.public_key().into(), packet);
        }
        let pkarr = Client::builder()
            .no_default_network()
            .relays(&["http://127.0.0.1:1"])
            .unwrap()
            .cache(cache)
            .build()
            .unwrap();
        Preflight {
            pkarr,
            dns: Arc::new(MockDns(dns)),
        }
    }
    fn direct(key: &Keypair, name: &str, addresses: &[&str]) -> SignedPacket {
        let mut service = SVCB::new(1, ".".try_into().unwrap());
        service.set_port(65535); // No port restrictions: policy applies to the address.
        let mut packet = SignedPacket::builder().https(name.try_into().unwrap(), service, 3600);
        for address in addresses {
            packet = packet.address(".".try_into().unwrap(), address.parse().unwrap(), 3600);
        }
        packet.sign(key).unwrap()
    }
    #[tokio::test]
    async fn all_pkarr_endpoint_addresses_must_be_public() {
        let owner = Keypair::random();
        let uri = format!("pubky://{}/pub/app/item", owner.public_key());
        for addresses in [
            vec!["169.254.169.254"],
            vec!["192.168.1.1"],
            vec!["8.8.8.8", "127.0.0.1"],
            vec!["::ffff:10.0.0.1"],
        ] {
            let check = fixture(&[direct(&owner, "_pubky", &addresses)], vec![]);
            let error = check.check(&uri).await.unwrap_err();
            assert_eq!(crate::resolver::error_class(&error), "policy");
        }
        fixture(
            &[direct(
                &owner,
                "_pubky",
                &["8.8.8.8", "2606:4700:4700::1111"],
            )],
            vec![],
        )
        .check(&uri)
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn icann_endpoints_and_feature_discovery_are_checked() {
        let owner = Keypair::random();
        let mut service = SVCB::new(1, "homeserver.example".try_into().unwrap());
        service.set_port(9443);
        let packet = SignedPacket::builder()
            .https("_pubky".try_into().unwrap(), service, 3600)
            .sign(&owner)
            .unwrap();
        let uri = format!("pubky://{}/pub/app/item", owner.public_key());
        fixture(
            std::slice::from_ref(&packet),
            vec!["8.8.8.8:9443".parse().unwrap()],
        )
        .check(&uri)
        .await
        .unwrap();
        let denied = fixture(
            &[packet],
            vec![
                "8.8.8.8:9443".parse().unwrap(),
                "10.0.0.1:9443".parse().unwrap(),
            ],
        );
        assert_eq!(
            crate::resolver::error_class(&denied.check(&uri).await.unwrap_err()),
            "policy"
        );

        // The SDK's /info target comes from SVCB even when storage HTTPS is public.
        let info = Keypair::random();
        let target = info.public_key().to_string();
        let packet = SignedPacket::builder()
            .svcb(
                "_pubky".try_into().unwrap(),
                SVCB::new(0, target.as_str().try_into().unwrap()),
                3600,
            )
            .https(
                "_pubky".try_into().unwrap(),
                SVCB::new(1, ".".try_into().unwrap()),
                3600,
            )
            .address(".".try_into().unwrap(), "8.8.8.8".parse().unwrap(), 3600)
            .sign(&owner)
            .unwrap();
        let denied = fixture(&[packet, direct(&info, ".", &["10.0.0.2"])], vec![]);
        assert_eq!(
            crate::resolver::error_class(&denied.check(&uri).await.unwrap_err()),
            "policy"
        );
    }
}
