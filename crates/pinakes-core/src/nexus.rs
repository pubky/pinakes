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
/// One line of the Nexus reference feed. Nexus also sends its own `resource_id`; Pinakes keys on
/// `uri` and computes its own ID (`identity`), so that field is not read.
pub struct Change {
    pub op: Op,
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct ReachSet {
    pub authors: Vec<String>,
    pub truncated: bool,
}
/// A viewer's set, cached by viewer and kind: when it was read, the set, and the reach-set
/// route's `ETag` it came with (`wot_2` only).
type ReachCache = Arc<Mutex<HashMap<(String, String), (Instant, ReachSet, Option<String>)>>>;
/// How long a set answers searches before it is read again.
const REACH_FRESH: Duration = Duration::from_secs(300);
/// How long a `wot_2` set and its `ETag` are kept to ask Nexus whether it changed.
const REACH_KEPT: Duration = Duration::from_secs(3600);
/// The most authors a reach set holds; Nexus flags a longer one as truncated.
pub const REACH_MAX_AUTHORS: usize = 1000;
/// The reach kinds: the viewer's `following` and `friends` lists, and `wot_2`, the authors the
/// Nexus reach-set route names (two steps of the follow graph).
pub fn reach_kind(kind: &str) -> bool {
    matches!(kind, "following" | "friends" | "wot_2")
}
#[derive(Clone)]
pub struct Nexus {
    base: String,
    /// Every request error drops its URL (`without_url`): the configured base may hold user
    /// info, and a reach request names its viewer.
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
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(reqwest::Error::without_url)?;
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
    /// Nexus's batch resource view of `uris` (`POST /v0/stream/resources/by_ids`, 1–100), for
    /// `viewer` when given: one view per URI it knows, in request order, with its tags and tagger
    /// count. A Nexus that knows none of them answers an empty list; one without this route (a 404)
    /// is an error, as any other failed answer is, never an empty one.
    pub async fn resources_by_uris(
        &self,
        uris: &[String],
        viewer: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let mut body = serde_json::json!({ "uris": uris });
        if let Some(viewer) = viewer {
            body["viewer_id"] = viewer.into();
        }
        let response = self
            .client
            .post(format!("{}/v0/stream/resources/by_ids", self.base))
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(reqwest::Error::without_url)?;
        bounded_json(response).await
    }
    /// A viewer's reach set of one kind (`reach_kind`), cached in memory per viewer and kind for
    /// five minutes; a `wot_2` set is then asked again with its `ETag`, and an unchanged answer
    /// (304) keeps it. Nothing is stored beyond this process.
    pub async fn reach(&self, viewer: &str, reach: &str) -> Result<ReachSet> {
        if !valid_author(viewer) || !reach_kind(reach) {
            bail!("invalid reach request");
        }
        let key = (viewer.to_owned(), reach.to_owned());
        let held = {
            let mut cache = self.cache.lock().await;
            cache.retain(|_, (at, _, etag)| {
                at.elapsed()
                    < if etag.is_some() {
                        REACH_KEPT
                    } else {
                        REACH_FRESH
                    }
            });
            match cache.get(&key) {
                Some((at, value, _)) if at.elapsed() < REACH_FRESH => return Ok(value.clone()),
                Some((_, value, Some(etag))) => Some((value.clone(), etag.clone())),
                _ => None,
            }
        };
        // Never hold the shared cache lock during network I/O. Concurrent misses
        // may duplicate work; the API prices each reach search for all six pages.
        let fetch = async {
            if reach == "wot_2" {
                self.fetch_reach_set(viewer, held).await
            } else {
                Ok((self.fetch_reach(viewer, reach).await?, None))
            }
        };
        let (result, etag) = tokio::time::timeout(self.reach_timeout, fetch)
            .await
            .context("Nexus reach deadline exceeded")??;
        let mut cache = self.cache.lock().await;
        if cache.len() >= 1024 && !cache.contains_key(&key) {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, ..))| *at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(key, (Instant::now(), result.clone(), etag));
        Ok(result)
    }
    /// The `wot_2` set from Nexus's reach-set route (`GET /v0/user/{viewer}/reach-set?depth=2`:
    /// `{authors, truncated}` with an `ETag`), asked with the `ETag` of the set `held`, which a
    /// 304 keeps. An unknown viewer has an empty set.
    async fn fetch_reach_set(
        &self,
        viewer: &str,
        held: Option<(ReachSet, String)>,
    ) -> Result<(ReachSet, Option<String>)> {
        let mut request = self
            .client
            .get(format!("{}/v0/user/{viewer}/reach-set", self.base))
            .query(&[("depth", 2)]);
        if let Some((_, etag)) = &held {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(reqwest::Error::without_url)?;
        match (response.status(), held) {
            (reqwest::StatusCode::NOT_MODIFIED, Some((set, etag))) => return Ok((set, Some(etag))),
            (reqwest::StatusCode::NOT_FOUND, _) => return Ok((ReachSet::default(), None)),
            _ => {}
        }
        let response = response
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 256)
            .map(str::to_owned);
        let mut set: ReachSet = bounded_json(response).await?;
        if set.authors.len() > REACH_MAX_AUTHORS || set.authors.iter().any(|id| !valid_author(id)) {
            bail!("invalid Nexus reach set");
        }
        set.authors.sort();
        set.authors.dedup();
        Ok((set, etag))
    }
    /// Ages a cached set by `by`, so a test reads it as stale.
    #[cfg(test)]
    async fn age(&self, viewer: &str, reach: &str, by: Duration) {
        if let Some((at, ..)) = self
            .cache
            .lock()
            .await
            .get_mut(&(viewer.to_owned(), reach.to_owned()))
        {
            *at = at.checked_sub(by).unwrap();
        }
    }
    async fn fetch_reach(&self, viewer: &str, reach: &str) -> Result<ReachSet> {
        let mut result = ReachSet::default();
        for skip in (0..=1000).step_by(200) {
            let response = self
                .client
                .get(format!("{}/v0/user/{viewer}/{reach}", self.base))
                .query(&[("skip", skip), ("limit", 200)])
                .send()
                .await
                .map_err(reqwest::Error::without_url)?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                break;
            }
            let response = response
                .error_for_status()
                .map_err(reqwest::Error::without_url)?;
            let ids: Vec<String> = bounded_json(response).await?;
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
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
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
    /// A failed Nexus request names no URL: not the base's user info, not the feed's query,
    /// not the viewer a reach request asks for. Refused, and answered 500.
    #[tokio::test]
    async fn a_failed_nexus_request_names_no_url() {
        let app = Router::new()
            .route(
                "/v0/resources/changes",
                get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "down") }),
            )
            .route(
                "/v0/user/{viewer}/following",
                get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "down") }),
            );
        let (url, server) = server(app).await;
        let refused = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let viewer = "y".repeat(52);
        for base in [url, refused] {
            let base = base.replace("http://", "http://feeder:CANARY@");
            let nexus = Nexus::new(&base).unwrap();
            let changes = format!("{:#}", nexus.changes(7).await.unwrap_err());
            let reach = format!("{:#}", nexus.reach(&viewer, "following").await.unwrap_err());
            for line in [changes, reach] {
                assert!(
                    !line.contains("CANARY")
                        && !line.contains("cursor=")
                        && !line.contains(&viewer)
                        && !line.contains("127.0.0.1"),
                    "{line}"
                );
            }
        }
        server.abort();
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

    /// The `wot_2` set comes from the reach-set route, is kept five minutes, then asked again
    /// with its `ETag`: a 304 keeps it, a changed set replaces it. An invalid set is refused.
    #[tokio::test]
    async fn a_wot_2_set_is_asked_again_with_its_etag() {
        use axum::http::{HeaderMap, StatusCode};
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().route(
            "/v0/user/{viewer}/reach-set",
            get(move |Path(viewer): Path<String>, headers: HeaderMap| {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                async move {
                    if viewer.starts_with('b') {
                        return (
                            StatusCode::OK,
                            [("etag", "\"bad\"")],
                            Json(serde_json::json!({"authors": ["not-an-author"], "truncated": false})),
                        );
                    }
                    let asked = headers.get("if-none-match").map(|v| v.to_str().unwrap().to_owned());
                    if n == 1 {
                        assert_eq!(asked.as_deref(), Some("\"v1\""));
                        return (StatusCode::NOT_MODIFIED, [("etag", "\"v1\"")], Json(serde_json::json!(null)));
                    }
                    let (etag, author) = if n == 0 { ("\"v1\"", "y") } else { ("\"v2\"", "o") };
                    (
                        StatusCode::OK,
                        [("etag", etag)],
                        Json(serde_json::json!({"authors": [author.repeat(52)], "truncated": n > 0})),
                    )
                }
            }),
        );
        let (url, task) = server(app).await;
        let nexus = Nexus::new(&url).unwrap();
        let viewer = "y".repeat(52);
        let first = nexus.reach(&viewer, "wot_2").await.unwrap();
        assert_eq!(
            (first.authors.clone(), first.truncated),
            (vec!["y".repeat(52)], false)
        );
        nexus.reach(&viewer, "wot_2").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "fresh for five minutes");
        nexus.age(&viewer, "wot_2", REACH_FRESH).await;
        let kept = nexus.reach(&viewer, "wot_2").await.unwrap();
        assert_eq!(kept.authors, first.authors);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        nexus.age(&viewer, "wot_2", REACH_FRESH).await;
        let changed = nexus.reach(&viewer, "wot_2").await.unwrap();
        assert_eq!(
            (changed.authors, changed.truncated),
            (vec!["o".repeat(52)], true)
        );
        assert!(nexus.reach(&"b".repeat(52), "wot_2").await.is_err());
        assert!(nexus.reach(&viewer, "strangers").await.is_err());
        task.abort();
    }
}
