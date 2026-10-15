use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use utoipa::ToSchema;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Put,
    Del,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Tag,
    Reply,
    Embed,
    Collection,
    Event,
}
impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Reply => "reply",
            Self::Embed => "embed",
            Self::Collection => "collection",
            Self::Event => "event",
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Change {
    pub op: Op,
    pub resource_id: String,
    pub uri: String,
    pub kind: Kind,
    pub author: String,
    pub object_path: String,
    pub label: Option<String>,
    pub at: i64,
    pub recurrence_id: Option<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Changes {
    pub lines: Vec<Change>,
    pub next_cursor: i64,
}
#[derive(Clone, Debug)]
pub struct FeedPage {
    pub page: Changes,
    pub epoch: Option<String>,
    pub length: i64,
}
#[derive(Clone, Debug, Default, Serialize, ToSchema)]
pub struct ReachSet {
    pub authors: Vec<String>,
    pub truncated: bool,
}
type ReachCache = Arc<Mutex<HashMap<(String, String), (Instant, ReachSet)>>>;
#[derive(Clone)]
pub struct Nexus {
    base: String,
    client: reqwest::Client,
    cache: ReachCache,
}
impl Nexus {
    pub fn new(base: &str) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()?,
            cache: Arc::default(),
        })
    }
    pub async fn changes(&self, cursor: i64) -> Result<FeedPage> {
        let response = self
            .client
            .get(format!("{}/v0/resources/changes", self.base))
            .query(&[("cursor", cursor), ("limit", 1000)])
            .send()
            .await?
            .error_for_status()?;
        let epoch = response
            .headers()
            .get("x-events-epoch")
            .map(|v| v.to_str().map(str::to_owned))
            .transpose()?;
        let length = response
            .headers()
            .get("x-events-length")
            .context("Nexus omitted feed length")?
            .to_str()?
            .parse::<i64>()?;
        if length < 0 {
            bail!("negative Nexus feed length");
        }
        Ok(FeedPage {
            page: bounded_json(response).await?,
            epoch,
            length,
        })
    }
    pub async fn reach(&self, viewer: &str, reach: &str) -> Result<ReachSet> {
        if !valid_author(viewer) || !matches!(reach, "following" | "friends") {
            bail!("invalid reach request");
        }
        let key = (viewer.to_owned(), reach.to_owned());
        let mut cache = self.cache.lock().await;
        cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(300));
        if let Some((_, value)) = cache.get(&key) {
            return Ok(value.clone());
        }
        let mut result = ReachSet::default();
        for skip in (0..=1000).step_by(200) {
            let response = self
                .client
                .get(format!("{}/v0/user/{viewer}/{reach}", self.base))
                .query(&[("skip", skip), ("limit", 200)])
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                break;
            }
            let ids: Vec<String> = bounded_json(response.error_for_status()?).await?;
            if ids.len() > 200 || ids.iter().any(|id| !valid_author(id)) {
                bail!("invalid Nexus reach page");
            }
            if skip == 1000 {
                result.truncated = !ids.is_empty();
                break;
            }
            let done = ids.len() < 200;
            result.authors.extend(ids);
            if done {
                break;
            }
        }
        result.authors.sort();
        result.authors.dedup();
        if cache.len() >= 1024 {
            if let Some(key) = cache.keys().next().cloned() {
                cache.remove(&key);
            }
        }
        cache.insert(key, (Instant::now(), result.clone()));
        Ok(result)
    }
}
pub fn valid_author(id: &str) -> bool {
    id.len() == 52
        && id
            .bytes()
            .all(|c| b"ybndrfg8ejkmcpqxot1uwisza345h769".contains(&c))
}
async fn bounded_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
    let mut bytes = vec![];
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            bail!("Nexus response exceeded 8 MB");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
