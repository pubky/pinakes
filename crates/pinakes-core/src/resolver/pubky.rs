use super::Resolver;
use crate::{
    config::ResolverConfig,
    model::{ResourceMeta, Status},
};
use anyhow::{bail, Result};
use async_trait::async_trait;
use std::time::Duration;

pub const ATTRIBUTES: &[&str] = &["content_type", "content_length"];
pub struct PubkyResolver {
    client: pubky::Pubky,
    config: ResolverConfig,
}
impl PubkyResolver {
    pub fn new(config: ResolverConfig) -> Result<Self> {
        let client = pubky::PubkyHttpClient::builder()
            .request_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            client: pubky::Pubky::with_client(client),
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
            // Only an unauthenticated public-storage GET. No social specs or linked objects are fetched.
            let mut response = match self.client.public_storage().get(uri).await {
                Ok(r) => r,
                Err(pubky::Error::Request(pubky::errors::RequestError::Server {
                    status, ..
                })) if matches!(status.as_u16(), 404 | 410 | 401 | 403) => {
                    let mut doc = ResourceMeta::new(uri, "pubky", self.config.ttl_secs);
                    doc.status = if matches!(status.as_u16(), 404 | 410) {
                        Status::Missing
                    } else {
                        Status::Unreachable
                    };
                    return Ok(doc);
                }
                Err(e) => return Err(e.into()),
            };
            if response
                .content_length()
                .is_some_and(|n| n > super::egress::BODY_CAP as u64)
            {
                bail!("Pubky object exceeds 1 MB");
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_owned();
            let mut bytes = vec![];
            while let Some(chunk) = response.chunk().await? {
                if bytes.len() + chunk.len() > super::egress::BODY_CAP {
                    bail!("Pubky object exceeds 1 MB");
                }
                bytes.extend_from_slice(&chunk);
            }
            card(uri, &content_type, &bytes, self.config.ttl_secs).bounded(ATTRIBUTES)
        })
        .await?
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration {
        Duration::from_secs(doc.ttl_secs)
    }
}
pub fn card(uri: &str, content_type: &str, body: &[u8], ttl: u64) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, "pubky", ttl);
    doc.set_type("CreativeWork");
    doc.attributes
        .insert("content_type".into(), content_type.into());
    doc.attributes
        .insert("content_length".into(), body.len().to_string());
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        doc.name = v
            .get("name")
            .or_else(|| v.get("title"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        doc.description = v
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        doc.image = v
            .get("image")
            .and_then(|v| v.as_str())
            .and_then(|s| url::Url::parse(uri).ok()?.join(s).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https" | "pubky"))
            .map(|u| u.to_string());
    }
    doc
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conventional_card_and_namespace_matching() {
        let uri =
            "pubky://ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi/pub/other.app/item";
        let doc=card(uri,"application/json",br#"{"title":"A deck","description":"Cards","image":"https://example.org/cover.png","password":"never keep"}"#,10);
        assert_eq!(doc.name.as_deref(), Some("A deck"));
        assert!(!serde_json::to_string(&doc).unwrap().contains("password"));
        assert_eq!(crate::resolver::classify(uri), Some("pubky"));
        assert_eq!(
            crate::resolver::classify(&uri.replace("other.app", "pubky.app")),
            None
        );
        assert_eq!(crate::resolver::classify("geo:1,2"), None);
        assert_eq!(
            crate::resolver::classify("https://www.openstreetmap.org/node/1"),
            None
        );
    }
}
