use crate::model::ResourceMeta;
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

pub mod egress;
pub mod geo;
pub mod osm;
pub mod pubky;
mod pubky_policy;
pub mod web;

#[async_trait]
pub trait Resolver: Send + Sync {
    fn manifest(&self) -> crate::adapter::Manifest {
        crate::adapter::manifest(self.name())
    }
    async fn probe(&self, uri: &str) -> Result<ResourceMeta> {
        self.resolve(uri).await
    }
    fn name(&self) -> &'static str;
    fn matches(&self, uri: &str) -> bool;
    fn canary_allowed(&self) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta>;
    fn batch_size(&self) -> usize {
        1
    }
    async fn resolve_batch(&self, uris: &[String]) -> Vec<Result<ResourceMeta>> {
        let mut out = Vec::with_capacity(uris.len());
        for uri in uris {
            out.push(self.resolve(uri).await);
        }
        out
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration;
}
/// Scheme modules and host adapters go before the generic web fallback.
pub fn classify(uri: &str) -> Option<&'static str> {
    let url = url::Url::parse(uri).ok()?;
    match url.scheme() {
        "pubky"
            if url.path().starts_with("/pub/")
                && url
                    .path()
                    .split('/')
                    .nth(2)
                    .is_some_and(|app| !app.is_empty() && app != "pubky.app") =>
        {
            Some("pubky")
        }
        "http" | "https" if osm::parse_id(uri).is_some() => Some("osm"),
        "http" | "https" => Some("web"),
        "geo" => Some("geo"),
        _ => None,
    }
}

#[derive(Debug)]
pub struct NotModified;
impl std::fmt::Display for NotModified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream not modified")
    }
}
impl std::error::Error for NotModified {}

#[derive(Clone, Debug)]
pub struct RetryLater {
    pub class: &'static str,
    pub seconds: u64,
}
impl std::fmt::Display for RetryLater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; retry later", self.class)
    }
}
impl std::error::Error for RetryLater {}

#[derive(Clone, Copy, Debug)]
pub struct ErrorClass(pub &'static str);
impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ErrorClass {}
pub fn error_class(error: &anyhow::Error) -> &'static str {
    if let Some(class) = error.downcast_ref::<ErrorClass>() {
        return class.0;
    }
    if let Some(retry) = error.downcast_ref::<RetryLater>() {
        return retry.class;
    }
    if error.is::<tokio::time::error::Elapsed>() {
        return "timeout";
    }
    for cause in error.chain() {
        if let Some(request) = cause.downcast_ref::<reqwest::Error>() {
            if request.is_timeout() {
                return "timeout";
            }
            if request.is_connect() {
                return "connect";
            }
            if let Some(status) = request.status() {
                return if status.is_server_error() {
                    "http_5xx"
                } else {
                    "http_4xx"
                };
            }
        }
    }
    "parse"
}
