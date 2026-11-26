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
    /// An app's post, which names itself (a social specs' post is rejected: Nexus's): `uri` is the post
    /// (`pubky://<author>/pub/<namespace>/v1/posts/{id}`), `object_path` its version file
    /// (`…/{id}/{editId}.json`).
    Post,
}
impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Reply => "reply",
            Self::Embed => "embed",
            Self::Collection => "collection",
            Self::Post => "post",
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
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Changes {
    pub lines: Vec<Change>,
    pub next_cursor: i64,
    /// The lines of the page this build cannot read, skipped, by reason (`unknown_kind`: a kind
    /// this build does not know, `malformed`: anything else), never their content: the cursor
    /// moves past them (`ingest::apply` counts them with the lines it read).
    #[serde(skip)]
    pub skipped: std::collections::BTreeMap<&'static str, u64>,
}
/// The reasons a feed line is skipped while parsing a page (`Changes::skipped`).
pub const SKIP_REASONS: [&str; 2] = ["unknown_kind", "malformed"];
/// The kinds of the reference feed this build reads. A `kind:event` line, which Nexus reserves,
/// is `unknown_kind`: social events are Nexus's.
const KINDS: &[&str] = &["tag", "reply", "embed", "collection", "post"];
impl Changes {
    /// The lines the page held: those read and those skipped. The cursor moves by this many.
    pub fn read(&self) -> usize {
        self.lines.len() + self.skipped.values().sum::<u64>() as usize
    }
    /// A page parsed line by line: the envelope (`lines`, `next_cursor`) must be well formed, but
    /// a line of a kind this build does not know, or one it cannot parse, is skipped and counted
    /// rather than failing the page, so one line never stops the feed.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct Raw {
            lines: Vec<serde_json::Value>,
            next_cursor: i64,
        }
        let raw: Raw = serde_json::from_slice(bytes)?;
        let mut page = Self {
            lines: Vec::with_capacity(raw.lines.len()),
            next_cursor: raw.next_cursor,
            skipped: Default::default(),
        };
        for line in raw.lines {
            let unknown_kind = line["kind"].as_str().is_some_and(|k| !KINDS.contains(&k));
            match serde_json::from_value::<Change>(line) {
                Ok(change) => page.lines.push(change),
                Err(_) => {
                    let [unknown, malformed] = SKIP_REASONS;
                    let reason = if unknown_kind { unknown } else { malformed };
                    *page.skipped.entry(reason).or_default() += 1;
                }
            }
        }
        Ok(page)
    }
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
/// The feed lines one poll asks for; Nexus's page cap. Fixed: it is the feed's contract, not an
/// operator's choice.
pub const FEED_PAGE: usize = 1000;
/// How long one request to the Nexus may take: a feed page, a reach list, a batch view. Fixed;
/// a search's reach reads have a shorter deadline of their own (`limits.reach_timeout_secs`).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest answer read from the Nexus (a feed page of `FEED_PAGE` lines fits well below).
const BODY_CAP: usize = 8 * 1024 * 1024;
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
    /// Every request error drops its URL (`without_url`): a reach request names its viewer. The
    /// one client every request goes through: it sends the configured `Authorization` header
    /// (marked sensitive), and follows no redirect, so the header never reaches another host.
    client: reqwest::Client,
    cache: ReachCache,
    reach_timeout: Duration,
}
impl Nexus {
    /// The Nexus the configuration names: `nexus_url`, its `Authorization` header and the
    /// search's reach deadline.
    pub fn configured(config: &crate::config::Config) -> Result<Self> {
        Self::with_reach_timeout(
            &config.nexus_url,
            config.nexus_authorization.as_deref(),
            Duration::from_secs(config.search.reach_timeout_secs),
        )
    }
    /// A Nexus at `base`, sent `authorization` (whole, as the `Authorization` header) when given.
    pub fn new(base: &str, authorization: Option<&str>) -> Result<Self> {
        Self::with_reach_timeout(base, authorization, Duration::from_secs(5))
    }
    pub fn with_reach_timeout(
        base: &str,
        authorization: Option<&str>,
        reach_timeout: Duration,
    ) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(value) = authorization.filter(|a| !a.trim().is_empty()) {
            // The error names no value: it is a credential.
            let mut value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| anyhow::anyhow!("invalid Nexus authorization header"))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            client: reqwest::Client::builder()
                .no_proxy()
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            cache: Arc::default(),
            reach_timeout,
        })
    }
    pub async fn changes(&self, cursor: i64) -> Result<FeedPage> {
        let response = self
            .client
            .get(format!("{}/v1/resources/changes", self.base))
            .query(&[("cursor", cursor), ("limit", FEED_PAGE as i64)])
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
            page: Changes::parse(&bounded_bytes(response).await?)?,
            epoch,
            length,
        })
    }
    /// Nexus's batch resource view of `uris` (`POST /v1/stream/resources/by_ids`, 1–100), for
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
            .post(format!("{}/v1/stream/resources/by_ids", self.base))
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(reqwest::Error::without_url)?;
        bounded_json(response).await
    }
    /// When the reach reads of a search that starts now end: `limits.reach_timeout_secs` from
    /// now, for all of them together ([`Nexus::reach_by`]).
    pub fn reach_deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::now() + self.reach_timeout
    }
    /// [`Nexus::reach_by`] with a deadline of its own.
    pub async fn reach(&self, viewer: &str, reach: &str) -> Result<ReachSet> {
        self.reach_by(viewer, reach, self.reach_deadline()).await
    }
    /// A viewer's reach set of one kind (`reach_kind`), cached in memory per viewer and kind for
    /// five minutes; a `wot_2` set is then asked again with its `ETag`, and an unchanged answer
    /// (304) keeps it. Nothing is stored beyond this process. A read that has not ended by
    /// `deadline` is an error: a search that reads two sets (`reach` and `boost`) gives both
    /// one deadline, so the two never take more than `limits.reach_timeout_secs` together.
    pub async fn reach_by(
        &self,
        viewer: &str,
        reach: &str,
        deadline: tokio::time::Instant,
    ) -> Result<ReachSet> {
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
        let (result, etag) = tokio::time::timeout_at(deadline, fetch)
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
    /// The `wot_2` set from Nexus's reach-set route (`GET /v1/user/{viewer}/reach-set?depth=2`:
    /// `{authors, truncated}` with an `ETag`), asked with the `ETag` of the set `held`, which a
    /// 304 keeps. An unknown viewer has an empty set.
    async fn fetch_reach_set(
        &self,
        viewer: &str,
        held: Option<(ReachSet, String)>,
    ) -> Result<(ReachSet, Option<String>)> {
        let mut request = self
            .client
            .get(format!("{}/v1/user/{viewer}/reach-set", self.base))
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
                .get(format!("{}/v1/user/{viewer}/{reach}", self.base))
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
/// The tagger a `/v1` tag's `tag_uri` names: the author of the tag file it addresses
/// (`pubky://<author>/pub/<namespace>/tags/<id>`, the namespace one or more segments, such as
/// `pubky.app` or `social/v1`: the stored address of their tag of that label; the
/// batch view sets it for the viewer's own tag only). `None` for anything else: no tag file, an
/// invalid author, an empty or dot segment, a query or fragment.
pub fn tag_file_author(tag_uri: &str) -> Option<&str> {
    let rest = tag_uri.strip_prefix("pubky://")?;
    let (author, path) = rest.split_at(rest.find('/')?);
    if !valid_author(author) || path.len() > 2048 || path.contains(['?', '#', '\\', '\0']) {
        return None;
    }
    let segments: Vec<&str> = path.split('/').collect();
    let real = |s: &&str| !s.is_empty() && *s != "." && *s != "..";
    match segments.as_slice() {
        ["", "pub", app @ .., "tags", id]
            if !app.is_empty() && app.iter().all(real) && real(id) =>
        {
            Some(author)
        }
        _ => None,
    }
}
async fn bounded_json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    Ok(serde_json::from_slice(&bounded_bytes(response).await?)?)
}
/// A Nexus response's body, at most 8 MB.
async fn bounded_bytes(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        if bytes.len() + chunk.len() > BODY_CAP {
            bail!("Nexus response exceeded 8 MB");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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
    /// A page parses line by line: a line of a kind this build does not know, and one it cannot
    /// parse (a missing field, a wrong type, not an object), is skipped and counted by reason,
    /// and the page still counts every line it held. A malformed envelope fails the page.
    #[test]
    fn a_page_skips_and_counts_the_lines_it_cannot_read() {
        let good = serde_json::json!({"op":"put","uri":"https://example.org/","kind":"tag",
            "author":"y".repeat(52),"object_path":"/pub/pubky.app/tags/1","label":"rust","at":1});
        let mut unknown = good.clone();
        unknown["kind"] = "listen".into();
        // Nexus reserves `kind:event`; social events are Nexus's.
        let mut event = good.clone();
        event["kind"] = "event".into();
        let post = serde_json::json!({"op":"put","kind":"post","author":"y".repeat(52),
            "uri":format!("pubky://{}/pub/maps/v1/posts/0034A", "y".repeat(52)),
            "object_path":"/pub/maps/v1/posts/0034A/0034B.json","at":1});
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove("uri");
        let mut wrong = good.clone();
        wrong["at"] = "yesterday".into();
        let page = serde_json::json!({"lines":[good, unknown, missing, 7, wrong, event, post, good],"next_cursor":12});
        let parsed = Changes::parse(page.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.lines.len(), 3);
        assert!(matches!(parsed.lines[1].kind, Kind::Post));
        assert_eq!(parsed.read(), 8);
        assert_eq!(parsed.next_cursor, 12);
        assert_eq!(
            parsed.skipped,
            std::collections::BTreeMap::from([("malformed", 3), ("unknown_kind", 2)])
        );
        // Every reason is one the rejection metric counts.
        assert!(SKIP_REASONS
            .iter()
            .all(|r| crate::ingest::REJECT_REASONS.contains(r)));
        for envelope in [
            r#"{"lines":"x","next_cursor":1}"#,
            r#"{"lines":[]}"#,
            "[]",
            "{",
        ] {
            assert!(Changes::parse(envelope.as_bytes()).is_err(), "{envelope}");
        }
    }

    /// A `/v1` tag's `tag_uri` names its tagger only when it is a tag file in that tagger's
    /// storage; anything else names no one.
    #[test]
    fn a_tag_uri_names_the_author_of_its_tag_file() {
        let me = "y".repeat(52);
        for path in [
            "/pub/pubky.app/tags/7T0R9X6A2E1L",
            "/pub/social/v1/tags/7T0R9X6A2E1L",
            "/pub/pinakes.e2e/tags/0034A",
        ] {
            let uri = format!("pubky://{me}{path}");
            assert_eq!(tag_file_author(&uri), Some(me.as_str()), "{uri}");
        }
        let short = "y".repeat(51);
        let upper = "Y".repeat(52);
        for uri in [
            String::new(),
            "true".into(),
            format!("https://{me}/pub/pubky.app/tags/X"),
            format!("pubky://{me}"),
            format!("pubky://{me}/"),
            format!("pubky://{short}/pub/pubky.app/tags/X"),
            format!("pubky://{upper}/pub/pubky.app/tags/X"),
            format!("pubky://{me}/pub/pubky.app/posts/X"),
            format!("pubky://{me}/pub/pubky.app/tags/"),
            format!("pubky://{me}/pub/pubky.app/tags/X/Y"),
            format!("pubky://{me}/pub/tags/X"),
            format!("pubky://{me}/pub//tags/X"),
            format!("pubky://{me}/pub/../tags/X"),
            format!("pubky://{me}/pub/pubky.app/tags/.."),
            format!("pubky://{me}/priv/pubky.app/tags/X"),
            format!("pubky://{me}/pub/pubky.app/tags/X?viewer=1"),
            format!("pubky://{me}/pub/pubky.app/tags/X#a"),
            format!("pubky://{me}/pub/pubky.app\\tags/X"),
            format!("pubky://{me}/pub/pubky.app/tags/{}", "X".repeat(2048)),
        ] {
            assert_eq!(tag_file_author(&uri), None, "{uri}");
        }
    }

    /// Every request to the Nexus (the feed, the batch view, each reach list's pages and the
    /// reach-set route) goes to its `/v1` route and carries the configured `Authorization`
    /// header, and none without one. The Nexus here answers on `/v0` too, as one serving both
    /// prefixes does: a request there would be recorded under `v0`.
    #[tokio::test]
    async fn every_nexus_request_is_on_v1_and_carries_the_configured_authorization() {
        use axum::{http::HeaderMap, routing::post};
        // Each call's prefix and route, and the header it carried.
        type Seen = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;
        let seen: Seen = Arc::default();
        let record = |seen: &Seen, version: &str, route: &str, headers: &HeaderMap| {
            let auth = headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned());
            seen.lock()
                .unwrap()
                .push((format!("{version} {route}"), auth));
        };
        let app = {
            let (a, b, c) = (seen.clone(), seen.clone(), seen.clone());
            Router::new()
                .route(
                    "/{version}/resources/changes",
                    get(
                        move |Path(version): Path<String>, headers: HeaderMap| async move {
                            record(&a, &version, "changes", &headers);
                            (
                                [("x-events-length", "0")],
                                Json(serde_json::json!({"lines": [], "next_cursor": 0})),
                            )
                        },
                    ),
                )
                .route(
                    "/{version}/stream/resources/by_ids",
                    post(
                        move |Path(version): Path<String>, headers: HeaderMap| async move {
                            record(&b, &version, "by_ids", &headers);
                            Json(serde_json::json!([]))
                        },
                    ),
                )
                .route(
                    "/{version}/user/{viewer}/{list}",
                    get(
                        move |Path((version, _, list)): Path<(String, String, String)>,
                              headers: HeaderMap| async move {
                            record(&c, &version, &list, &headers);
                            if list == "reach-set" {
                                return Json(
                                    serde_json::json!({"authors": [], "truncated": false}),
                                );
                            }
                            // A full first page, so the list is read on.
                            let route = format!("{version} {list}");
                            let n = c
                                .lock()
                                .unwrap()
                                .iter()
                                .filter(|(p, _)| *p == route)
                                .count();
                            Json(serde_json::json!(if n == 1 {
                                vec!["y".repeat(52); 200]
                            } else {
                                vec![]
                            }))
                        },
                    ),
                )
        };
        let (url, task) = server(app).await;
        let viewer = "y".repeat(52);
        for auth in [Some("Bearer CANARY-NEXUS"), None] {
            seen.lock().unwrap().clear();
            let nexus = Nexus::new(&url, auth).unwrap();
            nexus.changes(0).await.unwrap();
            nexus
                .resources_by_uris(&["https://example.org/".into()], Some(&viewer))
                .await
                .unwrap();
            for kind in ["following", "friends", "wot_2"] {
                nexus.reach(&viewer, kind).await.unwrap();
            }
            let seen = seen.lock().unwrap().clone();
            let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
            assert_eq!(
                paths,
                [
                    "v1 changes",
                    "v1 by_ids",
                    "v1 following",
                    "v1 following",
                    "v1 friends",
                    "v1 friends",
                    "v1 reach-set"
                ]
            );
            assert!(
                seen.iter().all(|(_, a)| a.as_deref() == auth),
                "{auth:?} {seen:?}"
            );
        }
        // No value is none.
        assert!(Nexus::new(&url, Some(" ")).is_ok());
        task.abort();
    }

    /// A Nexus that serves its resource routes on `/v0` only answers none of them: the feed and
    /// the batch view fail (a 404 is no answer, never an empty one).
    #[tokio::test]
    async fn a_nexus_without_the_v1_routes_is_no_answer() {
        use axum::routing::post;
        let app = Router::new()
            .route(
                "/v0/resources/changes",
                get(|| async {
                    (
                        [("x-events-length", "0")],
                        Json(serde_json::json!({"lines": [], "next_cursor": 0})),
                    )
                }),
            )
            .route(
                "/v0/stream/resources/by_ids",
                post(|| async { Json(serde_json::json!([])) }),
            );
        let (url, task) = server(app).await;
        let nexus = Nexus::new(&url, None).unwrap();
        assert!(nexus.changes(0).await.is_err());
        assert!(nexus
            .resources_by_uris(&["https://example.org/".into()], None)
            .await
            .is_err());
        task.abort();
    }

    /// A Nexus answering with a redirect to another host is not followed: the header never
    /// reaches that host, and the answer fails.
    #[tokio::test]
    async fn a_redirect_takes_the_authorization_nowhere() {
        let asked = Arc::new(AtomicUsize::new(0));
        let a = asked.clone();
        let (other, other_task) = server(Router::new().fallback(move || {
            a.fetch_add(1, Ordering::SeqCst);
            async { Json(serde_json::json!({"lines": [], "next_cursor": 0})) }
        }))
        .await;
        let to = other.replace("127.0.0.1", "localhost");
        let (url, task) = server(Router::new().fallback(move || {
            let to = to.clone();
            async move {
                (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [("location", format!("{to}/v1/resources/changes?cursor=0"))],
                )
            }
        }))
        .await;
        let nexus = Nexus::new(&url, Some("Bearer CANARY-NEXUS")).unwrap();
        let error = format!("{:#}", nexus.changes(0).await.unwrap_err());
        assert!(!error.contains("CANARY"), "{error}");
        assert!(nexus
            .resources_by_uris(&["https://example.org/".into()], None)
            .await
            .is_err());
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        task.abort();
        other_task.abort();
    }

    /// A failed Nexus request names no URL: not the base's user info, not the feed's query,
    /// not the viewer a reach request asks for. Refused, and answered 500.
    #[tokio::test]
    async fn a_failed_nexus_request_names_no_url() {
        let app = Router::new()
            .route(
                "/v1/resources/changes",
                get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "down") }),
            )
            .route(
                "/v1/user/{viewer}/following",
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
            let nexus = Nexus::new(&base, Some("Bearer CANARY")).unwrap();
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
            "/v1/user/{viewer}/following",
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
        let nexus = Nexus::new(&url, None).unwrap();
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
            "/v1/user/{viewer}/following",
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
        let nexus = Nexus::with_reach_timeout(&url, None, Duration::from_millis(150)).unwrap();
        // The deadline cut the read: two or three pages of 60 ms were asked, not all of them.
        assert!(nexus
            .reach(&"b".repeat(52), "following")
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline"));
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
            "/v1/user/{viewer}/reach-set",
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
        let nexus = Nexus::new(&url, None).unwrap();
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
