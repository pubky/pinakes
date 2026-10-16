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
    #[serde(default)]
    pub status: Option<String>,
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
    reach_timeout: Duration,
}
impl Nexus {
    pub fn new(base: &str) -> Result<Self> {
        Self::with_reach_timeout(base, Duration::from_secs(5))
    }
    pub fn with_reach_timeout(base: &str, reach_timeout: Duration) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()?,
            cache: Arc::default(),
            reach_timeout,
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
        {
            let mut cache = self.cache.lock().await;
            cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(300));
            if let Some((_, value)) = cache.get(&key) {
                return Ok(value.clone());
            }
        }
        // Never hold the shared cache lock during network I/O. Concurrent misses
        // may duplicate work; the API prices each reach search for all six pages.
        let result = tokio::time::timeout(self.reach_timeout, self.fetch_reach(viewer, reach))
            .await
            .context("Nexus reach deadline exceeded")??;
        let mut cache = self.cache.lock().await;
        if cache.len() >= 1024 && !cache.contains_key(&key) {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(key, (Instant::now(), result.clone()));
        Ok(result)
    }
    async fn fetch_reach(&self, viewer: &str, reach: &str) -> Result<ReachSet> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Path, routing::get, Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn server(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, task)
    }
    #[tokio::test]
    async fn slow_reach_does_not_lock_out_other_viewers_or_cache_hits() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (e, r, c) = (entered.clone(), release.clone(), calls.clone());
        let app = Router::new().route(
            "/v0/user/{viewer}/following",
            get(move |Path(viewer): Path<String>| {
                let (entered, release, calls) = (e.clone(), r.clone(), c.clone());
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if viewer == "y".repeat(52) {
                        entered.notify_one();
                        release.notified().await;
                    }
                    Json(Vec::<String>::new())
                }
            }),
        );
        let (url, server) = server(app).await;
        let nexus = Nexus::new(&url).unwrap();
        let slow_client = nexus.clone();
        let slow =
            tokio::spawn(async move { slow_client.reach(&"y".repeat(52), "following").await });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        for _ in 0..2 {
            tokio::time::timeout(
                Duration::from_secs(1),
                nexus.reach(&"b".repeat(52), "following"),
            )
            .await
            .unwrap()
            .unwrap();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "fast viewer is cached while slow viewer waits"
        );
        release.notify_one();
        slow.await.unwrap().unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn reach_deadline_covers_all_pages_and_never_caches_a_partial_set() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let app = Router::new().route(
            "/v0/user/{viewer}/following",
            get(move || {
                let calls = c.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    Json(vec!["y".repeat(52); 200])
                }
            }),
        );
        let (url, server) = server(app).await;
        let nexus = Nexus::with_reach_timeout(&url, Duration::from_millis(150)).unwrap();
        let started = Instant::now();
        assert!(nexus
            .reach(&"b".repeat(52), "following")
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!((2..=3).contains(&calls.load(Ordering::SeqCst)));
        assert!(nexus.cache.lock().await.is_empty());
        server.abort();
    }
}
