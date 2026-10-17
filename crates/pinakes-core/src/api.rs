use crate::{
    config::Config,
    model::{self, ResourceMeta},
    nexus::Nexus,
    store,
};
use anyhow::Context;
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use hmac::{KeyInit, Mac, SimpleHmac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify};
use utoipa::{IntoParams, OpenApi, ToSchema};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Config,
    pub nexus: Nexus,
    pub wake: Arc<Notify>,
    pub metrics: Arc<crate::telemetry::Metrics>,
    limiter: Arc<Mutex<HashMap<String, (Instant, usize)>>>,
    dumps: Arc<tokio::sync::Semaphore>,
    info_cache: Arc<Mutex<Option<(Instant, Value)>>>,
    pub(crate) cursor_mac: SimpleHmac<blake3::Hasher>,
}
impl AppState {
    pub fn new(pool: PgPool, config: Config) -> anyhow::Result<Self> {
        let mut cursor_key = [0; 32];
        getrandom::fill(&mut cursor_key).context("cursor key entropy unavailable")?;
        let metrics = Arc::new(
            crate::telemetry::Metrics::new()?.with_blocked_hosts(config.api.blocked_metric_hosts),
        );
        Ok(Self {
            pool,
            nexus: Nexus::with_reach_timeout(
                &config.nexus_url,
                Duration::from_secs(config.search.reach_timeout_secs),
            )?,
            config,
            wake: Arc::default(),
            metrics,
            limiter: Arc::default(),
            dumps: Arc::new(tokio::sync::Semaphore::new(4)),
            info_cache: Arc::default(),
            cursor_mac: SimpleHmac::<blake3::Hasher>::new_from_slice(&cursor_key)
                .context("initialize cursor MAC")?,
        })
    }
    async fn budget(
        &self,
        keys: &[(String, usize)],
        capacity: usize,
        message: &str,
        cost: usize,
    ) -> Result<(), ApiError> {
        let mut limits = self.limiter.lock().await;
        limits.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
        if keys.iter().any(|(key, limit)| {
            limits.get(key).map_or(0, |(_, n)| *n).saturating_add(cost) > *limit
        }) {
            return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, message.into()));
        }
        for (key, _) in keys {
            // Memory is bounded by eviction. At >10,000 distinct public source IPs/minute
            // this is intentionally fail-open for evicted entries, not a global quota.
            // IPv6 /64 bucketing could limit rotation but would share budgets across users;
            // keep exact-IP semantics and let the deployment edge enforce network quotas.
            if !limits.contains_key(key) && limits.len() >= capacity {
                if let Some(oldest) = limits
                    .iter()
                    .filter(|(k, _)| !keys.iter().any(|(key, _)| key == *k))
                    .min_by_key(|(_, v)| v.0)
                    .map(|(k, _)| k.clone())
                {
                    limits.remove(&oldest);
                }
            }
            limits
                .entry(key.clone())
                .or_insert_with(|| (Instant::now(), 0))
                .1 += cost;
        }
        Ok(())
    }
    async fn preview_budget(&self, ip: std::net::IpAddr, id: &str) -> Result<(), ApiError> {
        self.budget(
            &[
                (
                    format!("preview-ip:{ip}"),
                    self.config.preview.requests_per_minute,
                ),
                (
                    format!("uri:{id}"),
                    self.config.preview.uri_requests_per_minute,
                ),
            ],
            self.config.preview.cache_capacity.max(2),
            "preview rate limit exceeded",
            1,
        )
        .await
    }
}
pub fn pool_options() -> sqlx::postgres::PgPoolOptions {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET statement_timeout = '5s'")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
}
async fn public_budget(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>() else {
        return ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "missing peer address".into(),
        )
        .into_response();
    };
    let cost = if request.method() != axum::http::Method::OPTIONS
        && request.uri().path() == "/v1/search"
        && url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
            .any(|(key, _)| key == "reach")
    {
        state.config.api.reach_cost
    } else {
        1
    };
    if let Err(error) = state
        .budget(
            &[(
                format!("api-ip:{}", peer.0.ip()),
                state.config.api.requests_per_minute,
            )],
            state.config.api.cache_capacity,
            "API rate limit exceeded",
            cost,
        )
        .await
    {
        return error.into_response();
    }
    next.run(request).await
}

/// Whether `origin` may call the public listener (`api.cors_allow_origins`).
fn cors_allowed(state: &AppState, origin: Option<&header::HeaderValue>) -> bool {
    let origins = &state.config.api.cors_allow_origins;
    origin.is_some_and(|origin| {
        origins.iter().any(|o| o == "*")
            || origin
                .to_str()
                .is_ok_and(|origin| origins.iter().any(|o| o == origin))
    })
}
/// CORS headers for browser clients on the public listener, outermost so that budget refusals
/// (429 with `Retry-After`) are readable cross-origin too. No credentials: the public API takes
/// no cookies or tokens. In `*` mode every response carries them; with an origin list only
/// allowed origins get them, and every response carries `Vary: Origin`.
async fn cors_headers(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    let mut response = next.run(request).await;
    let origins = &state.config.api.cors_allow_origins;
    if origins.is_empty() {
        return response;
    }
    let any = origins.iter().any(|o| o == "*");
    // In `*` mode the headers do not depend on the request, so they go on every response: a
    // shared cache filled by a request without Origin (curl, a server) still serves browsers.
    let allowed = any || cors_allowed(&state, origin.as_ref());
    let headers = response.headers_mut();
    if !any {
        headers.append(header::VARY, header::HeaderValue::from_static("origin"));
    }
    if allowed {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            if any {
                header::HeaderValue::from_static("*")
            } else {
                origin.expect("allowed origin")
            },
        );
        headers.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            header::HeaderValue::from_static("retry-after, etag"),
        );
    }
    response
}
/// Answer CORS preflights inside the budget layer, so they share the per-IP budget. A refused
/// origin gets no CORS headers, which the browser treats as a refusal.
async fn cors_preflight(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let preflight = request.method() == axum::http::Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    if !preflight || state.config.api.cors_allow_origins.is_empty() {
        return next.run(request).await;
    }
    if !cors_allowed(&state, request.headers().get(header::ORIGIN)) {
        return StatusCode::NO_CONTENT.into_response();
    }
    (
        StatusCode::NO_CONTENT,
        [
            (header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST"),
            (header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type"),
            (header::ACCESS_CONTROL_MAX_AGE, "600"),
        ],
    )
        .into_response()
}

/// Swagger UI for the public document, from the vendored bundle; no second spec route.
fn swagger_ui(config: &crate::config::ApiConfig) -> Router<AppState> {
    if !config.swagger_ui {
        return Router::new();
    }
    Router::from(
        utoipa_swagger_ui::SwaggerUi::new("/swagger-ui")
            .config(utoipa_swagger_ui::Config::new(["/v1/openapi.json"])),
    )
    .layer(axum::middleware::map_response(swagger_headers))
}
/// The vendored bundle (Swagger UI 5.17.14) runs no inline script. It injects one fixed
/// `<style>` element, allowed by its hash, and its CSS embeds a data: image. Re-check the hash
/// when the vendored bundle changes (a browser reports the new one as a CSP violation).
const SWAGGER_UI_CSP: &str = "default-src 'self'; style-src 'self' 'sha256-RL3ie0nH+Lzz2YNqQN83mnU0J1ot4QL7b99vMdIX99w='; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";
async fn swagger_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(SWAGGER_UI_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response
}
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/info", get(info))
        .route("/v1/changes", get(crate::change_api::feed))
        .route("/v1/changes/lookup", post(crate::change_api::lookup))
        .route(
            "/v1/resources/{id}/changes",
            get(crate::change_api::resource),
        )
        .route("/v1/search", get(search))
        .route("/v1/groups/{id}", get(group))
        .route("/v1/resources/lookup", post(lookup))
        .route("/v1/resources/{id}", get(resource))
        .route(
            "/v1/resources/{id}/occurrences",
            get(crate::event::storage::list),
        )
        .route("/v1/resolve", post(resolve))
        .route("/v1/openapi.json", get(openapi))
        .route("/v1/dumps/osm_place.ndjson", get(osm_dump))
        .merge(swagger_ui(&state.config.api))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            cors_preflight,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            public_budget,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            cors_headers,
        ))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}
#[derive(Debug)]
pub struct ApiError(pub(crate) StatusCode, pub(crate) String);
impl ApiError {
    pub(crate) fn bad(s: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, s.into())
    }
}
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        tracing::error!(error=%error.into(),"request failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error".into(),
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.0;
        let mut response = (status, Json(json!({"error":self.1}))).into_response();
        if matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        ) {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
        }
        response
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fields {
    Card,
    Facets,
    Document,
}
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct FieldQuery {
    pub fields: Option<Fields>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    pub q: Option<String>,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub category: Option<String>,
    pub scheme: Option<String>,
    pub host: Option<String>,
    pub tags: Option<String>,
    #[serde(rename = "match")]
    pub match_mode: Option<String>,
    pub author: Option<String>,
    pub reach: Option<String>,
    pub viewer: Option<String>,
    pub sort: Option<String>,
    #[param(minimum = 1, maximum = 500)]
    pub limit: Option<usize>,
    /// Authenticated opaque keyset cursor from this daemon; invalid after restart. Restart pagination on HTTP 400.
    pub cursor: Option<String>,
    pub facet: Option<String>,
    pub fields: Option<Fields>,
    /// West,south,east,north in degrees; west > east crosses the antimeridian.
    pub bbox: Option<String>,
    /// Latitude,longitude in degrees; requires radius and excludes bbox.
    pub near: Option<String>,
    /// Positive metres, at most the configured maximum (200000 by default).
    pub radius: Option<String>,
    /// Inclusive start time lower bound: RFC3339 or Unix milliseconds.
    pub from: Option<String>,
    /// Inclusive start time upper bound: RFC3339 or Unix milliseconds.
    pub to: Option<String>,
    /// Finite map zoom; fractional zooms are floored and clamped to the table. Requires bbox; large views coarsen to the configured cell budget.
    pub cluster: Option<String>,
    /// Resource ID of the accepted event venue; filters event units.
    pub venue: Option<String>,
    /// Return one row per verified or curated group; standalone resources remain separate.
    #[param(value_type = Option<CollapseMode>)]
    pub collapse: Option<String>,
}
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CollapseMode {
    Group,
}
#[derive(Deserialize, Serialize)]
struct PageCursor {
    version: u8,
    score: f64,
    id: String,
    at: DateTime<Utc>,
    filter: String,
}
impl PageCursor {
    fn encode(&self, template: &SimpleHmac<blake3::Hasher>) -> anyhow::Result<String> {
        let payload = serde_json::to_vec(self)?;
        let mut mac = template.clone();
        mac.update(&payload);
        let mut signed = mac.finalize().into_bytes().to_vec();
        signed.extend_from_slice(&payload);
        Ok(URL_SAFE_NO_PAD.encode(signed))
    }
    fn decode(s: &str, template: &SimpleHmac<blake3::Hasher>) -> Result<Self, ApiError> {
        let invalid = || ApiError::bad("invalid cursor");
        if s.len() > 2048 {
            return Err(invalid());
        }
        let signed = URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid())?;
        if signed.len() <= 32 {
            return Err(invalid());
        }
        let (tag, payload) = signed.split_at(32);
        let mut mac = template.clone();
        mac.update(payload);
        // RustCrypto's verify_slice compares the complete MAC in constant time.
        mac.verify_slice(tag).map_err(|_| invalid())?;
        serde_json::from_slice(payload).map_err(|_| invalid())
    }
}
#[derive(Serialize, ToSchema)]
pub struct ResourceView {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<crate::groups::GroupCard>)]
    pub group: Option<Value>,
    pub resource_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<crate::event::storage::OccurrenceRef>)]
    pub occurrence: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<crate::event::storage::ReferenceEvent>)]
    pub event: Option<Value>,
    pub uri: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub site: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_path: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<model::Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osm_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<ResourceMeta>,
}
#[derive(Serialize, ToSchema)]
pub struct TagFacet {
    pub label: String,
    pub count: i64,
}
#[derive(Serialize, ToSchema)]
pub struct Facets {
    pub tags: Vec<TagFacet>,
}
#[derive(Serialize, ToSchema)]
pub struct SearchResponse {
    pub items: Vec<ResourceView>,
    pub cursor: Option<String>,
    pub reach_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets: Option<Facets>,
}
#[derive(Serialize, ToSchema)]
pub struct Cluster {
    pub cell: String,
    pub count: i64,
    pub lat: f64,
    pub lon: f64,
    pub top: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct ClusterResponse {
    pub resolution: u8,
    pub clusters: Vec<Cluster>,
    pub points: Vec<ResourceView>,
    pub truncated: bool,
    pub total: i64,
    pub reach_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets: Option<Facets>,
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LookupRequest {
    #[serde(alias = "resource_ids")]
    pub ids: Option<Vec<String>>,
    pub uris: Option<Vec<String>>,
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub uri: String,
}

fn view(row: &Value, fields: Fields, spatial: bool) -> Result<ResourceView, ApiError> {
    let doc: Option<ResourceMeta> = if row["doc"].get("type").is_some() {
        Some(serde_json::from_value(row["doc"].clone())?)
    } else {
        None
    };
    let facets = !matches!(fields, Fields::Card);
    Ok(ResourceView {
        group: row.get("group").filter(|v| !v.is_null()).cloned(),
        resource_id: row["id"].as_str().unwrap_or_default().into(),
        occurrence: row.get("occurrence").filter(|v| !v.is_null()).cloned(),
        event: row.get("event").filter(|v| !v.is_null()).cloned(),
        uri: row["uri"].as_str().unwrap_or_default().into(),
        type_name: row["type"].as_str().map(str::to_owned),
        name: row["name"].as_str().map(str::to_owned),
        description: doc.as_ref().and_then(|d| d.description.clone()),
        image: doc.as_ref().and_then(|d| d.image.clone()),
        site: doc
            .as_ref()
            .map(|d| d.site.clone())
            .unwrap_or_else(|| row["host"].as_str().unwrap_or_default().into()),
        status: row["status"].as_str().unwrap_or("pending").into(),
        type_path: facets.then(|| {
            doc.as_ref()
                .map(|d| d.type_path.clone())
                .unwrap_or_default()
        }),
        category: facets
            .then(|| doc.as_ref().and_then(|d| d.category.clone()))
            .flatten(),
        osm_id: row["osm_type"]
            .as_str()
            .zip(row["osm_id"].as_i64())
            .map(|(kind, id)| format!("{kind}/{id}")),
        attribution: row["osm_id"]
            .as_i64()
            .map(|_| crate::resolver::osm::ATTRIBUTION.into())
            .or_else(|| doc.as_ref().and_then(|d| d.attribution.clone())),
        point: (facets || spatial)
            .then(|| doc.as_ref().and_then(|d| d.location.clone()))
            .flatten(),
        start: doc.as_ref().and_then(|d| d.start),
        end: doc.as_ref().and_then(|d| d.end),
        document: if matches!(fields, Fields::Document) {
            doc
        } else {
            None
        },
    })
}
fn validate_search(q: &SearchQuery) -> Result<Vec<String>, ApiError> {
    if q.venue.as_deref().is_some_and(|v| !model::valid_id(v)) {
        return Err(ApiError::bad("venue must be a resource ID"));
    }
    if q.collapse.as_deref().is_some_and(|v| v != "group") {
        return Err(ApiError::bad("collapse must be group"));
    }
    if !(1..=500).contains(&q.limit.unwrap_or(50)) {
        return Err(ApiError::bad("limit must be 1–500"));
    }
    if !matches!(
        q.sort.as_deref(),
        None | Some("activity" | "recent" | "distance" | "start")
    ) {
        return Err(ApiError::bad(
            "sort must be activity, recent, distance or start",
        ));
    }
    if !matches!(q.match_mode.as_deref(), None | Some("any" | "all")) {
        return Err(ApiError::bad("match must be any or all"));
    }
    if !matches!(q.facet.as_deref(), None | Some("tags")) {
        return Err(ApiError::bad("facet must be tags"));
    }
    if let Some(reach) = &q.reach {
        if reach == "wot_2" {
            return Err(ApiError::bad("wot_2 is not available"));
        }
        if !matches!(reach.as_str(), "following" | "friends") || q.viewer.is_none() {
            return Err(ApiError::bad(
                "reach must be following or friends and requires viewer",
            ));
        }
    }
    for id in [&q.viewer, &q.author].into_iter().flatten() {
        if !crate::nexus::valid_author(id) {
            return Err(ApiError::bad("invalid author/viewer ID"));
        }
    }
    if q.type_name
        .as_ref()
        .is_some_and(|t| model::type_path(t).is_empty())
    {
        return Err(ApiError::bad("unknown schema.org type"));
    }
    for text in [&q.q, &q.category, &q.host, &q.scheme]
        .into_iter()
        .flatten()
    {
        if text.len() > 256 {
            return Err(ApiError::bad("filter exceeds 256 bytes"));
        }
    }
    let mut tags = q
        .tags
        .as_ref()
        .map(|s| s.split(',').map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_default();
    tags.sort();
    tags.dedup();
    if tags.len() > 5
        || (q.match_mode.as_deref() == Some("all") && tags.len() > 3)
        || tags.iter().any(|t| t.is_empty() || t.len() > 128)
    {
        return Err(ApiError::bad(
            "tags allows at most 5 labels, or 3 with match=all; each is 1–128 bytes",
        ));
    }
    Ok(tags)
}
fn scope(
    sql: &mut QueryBuilder<'_, Postgres>,
    q: &SearchQuery,
    reach: &Option<crate::nexus::ReachSet>,
) {
    if let Some(author) = &q.author {
        sql.push(" AND f.author=").push_bind(author.clone());
    }
    if let Some(reach) = reach {
        sql.push(" AND f.author=ANY(")
            .push_bind(reach.authors.clone())
            .push(")");
    }
}
#[utoipa::path(get,path="/v1/search",params(SearchQuery),responses((status=200,body=SearchResult),(status=304,description="ETag unchanged"),(status=400,description="Invalid or unavailable filter"),(status=502,description="Nexus reach unavailable")))]
pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let tags = validate_search(&q)?;
    if q.collapse.is_some() && !state.config.groups.enabled {
        return Err(ApiError::bad("groups are disabled"));
    }
    let spatial = crate::spatial::Filters::parse(&q, &state.config.search)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    let cursor = q
        .cursor
        .as_deref()
        .map(|s| PageCursor::decode(s, &state.cursor_mac))
        .transpose()?;
    let reach = if let (Some(viewer), Some(reach)) = (&q.viewer, &q.reach) {
        Some(state.nexus.reach(viewer, reach).await.map_err(|e| {
            tracing::warn!(error=%e,"reach unavailable");
            ApiError(StatusCode::BAD_GATEWAY, "Nexus reach unavailable".into())
        })?)
    } else {
        None
    };
    let mut fingerprint = q.clone();
    fingerprint.cursor = None;
    fingerprint.limit = None;
    let reach_revision = reach.as_ref().map(|r| {
        let mut authors = r.authors.clone();
        authors.sort();
        authors.dedup();
        (
            blake3::hash(authors.join("\n").as_bytes())
                .to_hex()
                .to_string(),
            r.truncated,
        )
    });
    let filter = blake3::hash(&serde_json::to_vec(&(&fingerprint, &reach_revision))?)
        .to_hex()
        .to_string();
    if let Some(c) = &cursor {
        if c.version != 2
            || c.filter != filter
            || !c.score.is_finite()
            || !model::valid_id(&c.id)
            || c.at > Utc::now() + chrono::Duration::seconds(5)
        {
            return Err(ApiError::bad("cursor does not match this search"));
        }
    }
    let at = cursor.as_ref().map_or_else(Utc::now, |c| c.at);
    let mut sql = QueryBuilder::<Postgres>::new(if q.collapse.is_some() {
        "WITH eligible AS (SELECT r.*, "
    } else {
        "WITH filtered AS (SELECT r.*, "
    });
    if q.sort.as_deref() == Some("distance") {
        sql.push("-");
        spatial.distance(&mut sql);
    } else if q.sort.as_deref() == Some("start") {
        sql.push("-extract(epoch FROM r.start_at)::double precision");
    } else if q.sort.as_deref() == Some("recent") {
        sql.push("coalesce(extract(epoch FROM s.newest_at),0)::double precision");
    } else {
        sql.push("coalesce(s.score * exp(-ln(2.0)*greatest(0,extract(epoch FROM (")
            .push_bind(at)
            .push("::timestamptz-s.updated_at)))/2592000.0),0)::double precision");
    }
    sql.push(
        // Walled pages have no display facts; they stay reachable by id and in /v1/changes.
        " AS rank FROM search_unit r LEFT JOIN resource_score s ON s.resource_id=r.id WHERE r.status<>'blocked'",
    );
    spatial.apply(&mut sql);
    if let Some(venue) = &q.venue {
        sql.push(" AND r.venue_id=").push_bind(venue.clone());
    }
    if let Some(author) = &q.author {
        sql.push(" AND (r.event_author IS NULL OR r.event_author=")
            .push_bind(author.clone())
            .push(")");
    }
    if let Some(reach) = &reach {
        sql.push(" AND (r.event_author IS NULL OR r.event_author=ANY(")
            .push_bind(reach.authors.clone())
            .push("))");
    }
    if q.sort.as_deref() == Some("start") {
        sql.push(" AND r.start_at IS NOT NULL");
    }
    if let Some(t) = &q.type_name {
        let name = model::type_path(t).pop().unwrap();
        sql.push(" AND r.type_path @> ").push_bind(vec![name]);
    }
    for (column, value) in [
        ("r.category", &q.category),
        ("r.scheme", &q.scheme),
        ("r.host", &q.host),
    ] {
        if let Some(value) = value {
            sql.push(" AND ")
                .push(column)
                .push("=")
                .push_bind(value.clone());
        }
    }
    if let Some(text) = q.q.as_ref().filter(|s| !s.is_empty()) {
        let prefix = format!(
            "%{}%",
            text.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        sql.push(" AND (r.search_text @@ websearch_to_tsquery('simple',")
            .push_bind(text.clone())
            .push(") OR r.name ILIKE ")
            .push_bind(prefix)
            .push(" OR r.name % ")
            .push_bind(text.clone())
            .push(")");
    }
    sql.push(" AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id");
    scope(&mut sql, &q, &reach);
    sql.push(")");
    if !tags.is_empty() {
        if q.match_mode.as_deref() == Some("all") {
            for tag in &tags {
                sql.push(
                    " AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id AND f.label=",
                )
                .push_bind(tag.clone());
                scope(&mut sql, &q, &reach);
                sql.push(")");
            }
        } else {
            sql.push(
                " AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id AND f.label=ANY(",
            )
            .push_bind(tags)
            .push(")");
            scope(&mut sql, &q, &reach);
            sql.push(")");
        }
    }
    if q.collapse.is_some() {
        sql.push("), filtered AS (SELECT DISTINCT ON(b.unit_id) b.*, ");
        match q.sort.as_deref() {
            Some("distance") => {
                sql.push("coalesce(-");
                let distance = crate::spatial::Filters::parse(&q, &state.config.search)
                    .map_err(|e| ApiError::bad(e.to_string()))?; // Alias r below exposes the best member's indexed point.
                distance.distance(&mut sql);
                sql.push(",e.rank)");
            }
            Some("start") => {
                sql.push("coalesce(-extract(epoch FROM b.start_at)::double precision,e.rank)");
            }
            Some("recent") => {
                sql.push("coalesce(extract(epoch FROM s.newest_at),e.rank)::double precision");
            }
            _ => {
                sql.push("coalesce(s.score*exp(-ln(2.0)*greatest(0,extract(epoch FROM (")
                    .push_bind(at)
                    .push("::timestamptz-s.updated_at)))/2592000.0),e.rank)::double precision");
            }
        }
        sql.push(" AS rank FROM eligible e LEFT JOIN LATERAL (SELECT group_id FROM group_unit WHERE unit_id=e.unit_id OFFSET 0) u ON true JOIN LATERAL (SELECT b.* FROM group_search_unit b WHERE b.unit_id=u.group_id UNION ALL SELECT e.id,e.uri,e.scheme,e.host,e.type,e.type_path,e.name,e.category,e.point,e.start_at,e.end_at,e.venue_id,e.doc,e.search_text,e.resolver,e.resolved_at,e.refresh_at,e.status,e.osm_type,e.osm_id,e.unit_id,e.occurrence,e.event,e.event_author,NULL::jsonb WHERE u.group_id IS NULL OFFSET 0) b ON true CROSS JOIN LATERAL (SELECT b.point) r LEFT JOIN group_score s ON s.group_id=u.group_id ORDER BY b.unit_id,e.rank DESC,e.unit_id");
    }
    if let Some(resolution) = spatial.resolution {
        sql.push("), cells AS (SELECT *, h3_latlng_to_cell(point(ST_X(point),ST_Y(point)),")
            .push_bind(i32::from(u8::from(resolution)))
            .push(")::text AS cell FROM filtered WHERE point IS NOT NULL), groups AS (SELECT cell,count(*) AS count,avg(ST_Y(point)) AS lat,degrees(atan2(avg(sin(radians(ST_X(point)))),avg(cos(radians(ST_X(point)))))) AS lon,ARRAY(SELECT id FROM (SELECT DISTINCT ON (id) id,rank,unit_id FROM cells t WHERE t.cell=cells.cell ORDER BY id,rank DESC,unit_id DESC) unique_ids ORDER BY rank DESC,unit_id DESC LIMIT 3) AS top,max(rank) AS rank,CASE WHEN bool_or(osm_id IS NOT NULL OR doc->>'attribution' IS NOT NULL) THEN ")
            .push_bind(crate::resolver::osm::ATTRIBUTION)
            .push(" ELSE NULL END AS attribution FROM cells GROUP BY cell LIMIT ")
            .push_bind((state.config.search.max_cluster_cells+1) as i64)
            .push("), entries AS (SELECT 'cluster' AS kind,cell AS id,cell AS unit_id,rank,to_jsonb(g)-'rank' AS item FROM groups g WHERE count>1 AND (SELECT count(*) FROM filtered)>")
            .push_bind(q.limit.unwrap_or(50) as i64)
            .push(" UNION ALL SELECT 'point',c.id,c.unit_id,c.rank,to_jsonb(c) FROM cells c JOIN groups g USING(cell) WHERE g.count=1 OR (SELECT count(*) FROM filtered)<=")
            .push_bind(q.limit.unwrap_or(50) as i64)
            .push("), page AS (SELECT * FROM entries ORDER BY rank DESC,unit_id DESC LIMIT ")
            .push_bind((q.limit.unwrap_or(50)+1) as i64)
            .push(") SELECT coalesce((SELECT jsonb_agg(to_jsonb(p)");
        if q.collapse.is_some() {
            sql.push(" || CASE WHEN p.kind='point' THEN jsonb_build_object('item',p.item||jsonb_build_object('group',group_card(p.unit_id))) ELSE '{}'::jsonb END");
        }
        sql.push(" ORDER BY rank DESC,unit_id DESC) FROM page p),'[]') AS items, (SELECT count(*) FROM filtered) AS total, (SELECT count(*) FROM groups) AS cell_count, ");
    } else {
        sql.push("), page AS (SELECT * FROM filtered");
        if let Some(c) = &cursor {
            sql.push(" WHERE (rank,unit_id)<(")
                .push_bind(c.score)
                .push(",")
                .push_bind(c.id.clone())
                .push(")");
        }
        sql.push(" ORDER BY rank DESC,unit_id DESC LIMIT ")
            .push_bind((q.limit.unwrap_or(50) + 1) as i64)
            .push(") SELECT coalesce((SELECT jsonb_agg(to_jsonb(p)");
        if q.collapse.is_some() {
            sql.push(" || jsonb_build_object('group',group_card(p.unit_id))");
        }
        sql.push(" ORDER BY rank DESC,unit_id DESC) FROM page p),'[]') AS items, ");
    }
    if q.facet.as_deref() == Some("tags") {
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,label) FROM (SELECT f.label,count(DISTINCT r.unit_id) AS count FROM ");
        if q.collapse.is_some() {
            sql.push("(SELECT r.unit_id,u.resource_id FROM filtered r CROSS JOIN LATERAL (SELECT resource_id FROM group_unit WHERE group_id=r.unit_id OFFSET 0) u UNION ALL SELECT r.unit_id,r.id AS resource_id FROM filtered r WHERE NOT EXISTS(SELECT 1 FROM \"group\" g WHERE g.id=r.unit_id)) r JOIN effective_reference f ON f.resource_id=r.resource_id");
        } else {
            sql.push("filtered r JOIN effective_reference f ON r.id=f.resource_id");
        }
        sql.push(" WHERE f.label IS NOT NULL");

        scope(&mut sql, &q, &reach);
        sql.push(" GROUP BY f.label) t),'[]')");
    } else {
        sql.push("'[]'::jsonb");
    }
    sql.push(" AS facets, jsonb_build_array((SELECT jsonb_agg(jsonb_build_array(cursor,epoch) ORDER BY nexus) FROM feed_cursor),(SELECT revision FROM search_revision WHERE id=1)) AS revision");
    let row = sql.build().fetch_one(&state.pool).await?;
    if spatial.resolution.is_some()
        && row.try_get::<i64, _>("cell_count")? > state.config.search.max_cluster_cells as i64
    {
        return Err(ApiError::bad(
            "cluster cell budget exceeded; use a smaller viewport",
        ));
    }
    let mut items: Vec<Value> = serde_json::from_value(row.try_get("items")?)?;
    let has_more = items.len() > q.limit.unwrap_or(50);
    items.truncate(q.limit.unwrap_or(50));
    let next = if has_more && spatial.resolution.is_none() {
        let last = items.last().expect("nonempty page");
        Some(
            PageCursor {
                version: 2,
                score: last["rank"].as_f64().context("missing rank")?,
                id: last["unit_id"]
                    .as_str()
                    .context("missing resource ID")?
                    .into(),
                at,
                filter,
            }
            .encode(&state.cursor_mac)?,
        )
    } else {
        None
    };
    let facets = if q.facet.is_some() {
        let values: Vec<Value> = serde_json::from_value(row.try_get("facets")?)?;
        Some(Facets {
            tags: values
                .into_iter()
                .map(|v| TagFacet {
                    label: v["label"].as_str().unwrap_or_default().into(),
                    count: v["count"].as_i64().unwrap_or(0),
                })
                .collect(),
        })
    } else {
        None
    };
    let revision: Value = row.try_get("revision")?;
    let etag = format!(
        "\"{}\"",
        blake3::hash(&serde_json::to_vec(&(q.clone(), revision, reach_revision))?).to_hex()
    );
    let mut response = if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h.split(',')
                .any(|v| v.trim().trim_start_matches("W/") == etag || v.trim() == "*")
        }) {
        StatusCode::NOT_MODIFIED.into_response()
    } else if spatial.resolution.is_some() {
        let mut clusters = Vec::new();
        let mut points = Vec::new();
        for entry in items {
            let item = &entry["item"];
            if entry["kind"] == "cluster" {
                clusters.push(Cluster {
                    cell: item["cell"].as_str().unwrap_or_default().into(),
                    count: item["count"].as_i64().unwrap_or(0),
                    lat: item["lat"].as_f64().unwrap_or(0.0),
                    lon: item["lon"].as_f64().unwrap_or(0.0),
                    top: serde_json::from_value(item["top"].clone())?,
                    attribution: item["attribution"].as_str().map(str::to_owned),
                });
            } else {
                points.push(view(item, q.fields.unwrap_or(Fields::Card), true)?);
            }
        }
        Json(ClusterResponse {
            resolution: u8::from(spatial.resolution.expect("cluster mode")),
            clusters,
            points,
            truncated: has_more,
            total: row.try_get("total")?,
            reach_truncated: reach.is_some_and(|r| r.truncated),
            facets,
        })
        .into_response()
    } else {
        Json(SearchResponse {
            items: items
                .iter()
                .map(|r| view(r, q.fields.unwrap_or(Fields::Card), spatial.is_spatial()))
                .collect::<Result<_, _>>()?,
            cursor: next,
            reach_truncated: reach.is_some_and(|r| r.truncated),
            facets,
        })
        .into_response()
    };
    response
        .headers_mut()
        .insert(header::ETAG, etag.parse().expect("hash header"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("public, max-age=30"),
    );
    Ok(response)
}
#[derive(ToSchema)]
#[allow(dead_code)]
#[serde(untagged)]
pub enum SearchResult {
    Page(SearchResponse),
    Cluster(ClusterResponse),
}

async fn read(
    pool: &PgPool,
    id: &str,
    fields: Fields,
    groups: bool,
) -> Result<Option<ResourceView>, ApiError> {
    let value: Option<Value> =
        sqlx::query_scalar("SELECT to_jsonb(r)||jsonb_build_object('group',CASE WHEN $2 THEN group_card(u.group_id)||CASE WHEN g.curated THEN '{}'::jsonb ELSE jsonb_build_object('name',r.name,'image',r.doc->>'image') END END) FROM resource_document r LEFT JOIN group_unit u ON u.unit_id=r.id LEFT JOIN \"group\" g ON g.id=u.group_id WHERE r.id=$1")
            .bind(id)
            .bind(groups)
            .fetch_optional(pool)
            .await?;
    value.as_ref().map(|r| view(r, fields, false)).transpose()
}
#[utoipa::path(get,path="/v1/resources/{id}",params(("id"=String,Path,description="32 lowercase hex resource ID"),FieldQuery),responses((status=200,body=ResourceView),(status=400,description="Invalid ID"),(status=404,description="Unknown resource")))]
pub async fn resource(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(fields): Query<FieldQuery>,
) -> Result<Json<ResourceView>, ApiError> {
    if !model::valid_id(&id) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    read(
        &state.pool,
        &id,
        fields.fields.unwrap_or(Fields::Document),
        state.config.groups.enabled,
    )
    .await?
    .map(Json)
    .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "resource not found".into()))
}
#[utoipa::path(post,path="/v1/resources/lookup",request_body=LookupRequest,params(FieldQuery),responses((status=200,body=Vec<ResourceView>),(status=400,description="Exactly one list of 1–500 IDs or URIs")))]
pub async fn lookup(
    State(state): State<AppState>,
    Query(fields): Query<FieldQuery>,
    Json(body): Json<LookupRequest>,
) -> Result<Json<Vec<ResourceView>>, ApiError> {
    let ids = match (body.ids, body.uris) {
        (Some(ids), None) => {
            if ids.iter().any(|s| !model::valid_id(s)) {
                return Err(ApiError::bad("invalid resource ID"));
            }
            ids
        }
        (None, Some(uris)) => uris
            .iter()
            .map(|s| {
                model::identity(s)
                    .map(|v| v.0)
                    .map_err(|e| ApiError::bad(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(ApiError::bad("supply exactly one of ids or uris")),
    };
    if !(1..=500).contains(&ids.len()) {
        return Err(ApiError::bad("lookup requires 1–500 entries"));
    }
    store::expedite(&state.pool, &ids).await?;
    state.wake.notify_waiters();
    let rows:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(r)||jsonb_build_object('group',CASE WHEN $2 THEN group_card(u.group_id)||CASE WHEN g.curated THEN '{}'::jsonb ELSE jsonb_build_object('name',r.name,'image',r.doc->>'image') END END) FROM unnest($1::text[]) WITH ORDINALITY AS wanted(id,n) JOIN resource_document r ON r.id=wanted.id LEFT JOIN group_unit u ON u.unit_id=r.id LEFT JOIN \"group\" g ON g.id=u.group_id ORDER BY wanted.n").bind(ids).bind(state.config.groups.enabled).fetch_all(&state.pool).await?;
    Ok(Json(
        rows.iter()
            .map(|r| view(r, fields.fields.unwrap_or(Fields::Facets), false))
            .collect::<Result<_, _>>()?,
    ))
}
#[utoipa::path(post,path="/v1/resolve",request_body=ResolveRequest,responses((status=200,body=ResourceView),(status=202,body=ResourceView,description="Queued; poll resource by ID"),(status=400,description="Unsupported URI or disabled resolver"),(status=429,description="Preview rate limit")))]
pub async fn resolve(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<ResolveRequest>,
) -> Result<Response, ApiError> {
    let (id, uri, _) = model::identity(&body.uri).map_err(|e| ApiError::bad(e.to_string()))?;
    let resolver = crate::resolver::classify_config(&uri, &state.config)
        .ok_or_else(|| ApiError::bad("no resolver available for this URI"))?;
    if (resolver == "event_platform"
        && crate::event::adapters::detect(&uri)
            .is_none_or(|d| !crate::event::adapters::manifest(d, &state.config.event).enabled))
        || (resolver == "web" && !state.config.web.enabled)
        || (resolver == "pubky" && !state.config.pubky.enabled)
        || (resolver == "geo"
            && (!state.config.geo_enabled
                || (!uri.starts_with("geo:") && !state.config.geo.location_links)))
        || (resolver == "osm" && !state.config.osm.enabled)
        || (resolver == "event" && (!state.config.event.enabled || !state.config.event.calendars))
    {
        return Err(ApiError::bad("resolver is disabled"));
    }
    state.preview_budget(peer.ip(), &id).await?;
    store::ensure_resource(&state.pool, &uri).await?;
    store::expedite(&state.pool, std::slice::from_ref(&id)).await?;
    state.wake.notify_waiters();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let resource = read(
            &state.pool,
            &id,
            Fields::Document,
            state.config.groups.enabled,
        )
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "resource expired".into()))?;
        if resource.status != "pending" {
            return Ok((StatusCode::OK, Json(resource)).into_response());
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok((StatusCode::ACCEPTED, Json(resource)).into_response());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
#[derive(Serialize, ToSchema)]
pub struct Info {
    pub version: String,
    pub manifests: Vec<crate::adapter::Manifest>,
    pub counts: InfoCounts,
    pub groups: crate::groups::GroupConfig,
    pub ingestion: Vec<Value>,
    pub osm_licence: OsmLicence,
    pub resolvers: Value,
}
#[derive(Serialize, ToSchema)]
pub struct InfoCounts {
    pub resources: i64,
    pub references: i64,
    pub occurrences: i64,
}
#[derive(Serialize, ToSchema)]
pub struct OsmLicence {
    pub licence: String,
    pub attribution: String,
    pub url: String,
    pub dump: String,
}
#[utoipa::path(get,path="/v1/info",responses((status=200,body=Info)),description="Public summary only; the same path on admin_listen includes source diagnostics and metrics. Cached for 15 seconds.")]
pub async fn info(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut body = full_info(&state).await?;
    body.as_object_mut().unwrap().remove("sources");
    body.as_object_mut().unwrap().remove("metrics");
    for row in body["ingestion"].as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("nexus");
    }
    for m in body["manifests"].as_array_mut().unwrap() {
        if let Some(url) = m["canary"].as_str().and_then(|s| url::Url::parse(s).ok()) {
            m["canary"] = url.host_str().map(|h| json!(h)).unwrap_or(Value::Null);
        }
    }
    Ok(Json(body))
}
#[utoipa::path(get,path="/v1/info",responses((status=200,body=Value)),description="Admin listener: the public summary plus source diagnostics, ingestion, queue statuses and metrics. Cached for 15 seconds.")]
async fn admin_info(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(full_info(&state).await?))
}
async fn full_info(state: &AppState) -> Result<Value, ApiError> {
    let mut cache = state.info_cache.lock().await;
    if let Some((at, body)) = &*cache {
        if at.elapsed() < Duration::from_secs(15) {
            return Ok(body.clone());
        }
    }
    state.metrics.refresh(&state.pool).await?;
    let sources: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(s)-'keys'-'held_keys'-'missing_keys' FROM source_state s ORDER BY source",
    )
    .fetch_all(&state.pool)
    .await?;
    let row=sqlx::query("SELECT (SELECT count(*) FROM resource) AS resources,(SELECT count(*) FROM reference) AS references,(SELECT count(*) FROM occurrence) AS occurrences").fetch_one(&state.pool).await?;
    let ingestion:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('nexus',nexus,'cursor',cursor,'feed_length',feed_length,'lag',greatest(feed_length-cursor,0),'epoch',epoch,'updated_at',updated_at) FROM feed_cursor ORDER BY nexus").fetch_all(&state.pool).await?;
    let statuses:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('resolver',resolver,'status',status,'count',count(*),'dead_letter',count(*) FILTER (WHERE dead_letter),'leased',count(*) FILTER (WHERE lease_until>now())) FROM resource GROUP BY resolver,status ORDER BY resolver,status").fetch_all(&state.pool).await?;
    let mut body = json!({"groups":state.config.groups,"version":env!("CARGO_PKG_VERSION"),"manifests":crate::adapter::configured(&state.config),"sources":sources,"metrics":state.metrics.summary(),"ingestion":ingestion,"counts":{"resources":row.try_get::<i64,_>("resources")?,"references":row.try_get::<i64,_>("references")?,"occurrences":row.try_get::<i64,_>("occurrences")?},"osm_licence":{"licence":"ODbL-1.0","attribution":crate::resolver::osm::ATTRIBUTION,"url":"https://opendatacommons.org/licenses/odbl/1-0/","dump":"/v1/dumps/osm_place.ndjson"},"resolvers":{"event":{"enabled":state.config.event.enabled,"calendar":state.config.event.calendars,"jsonld":state.config.event.jsonld,"microformats":state.config.event.microformats,"activitypub":state.config.event.activitypub,"geocoding":state.config.event.geocoding,"coordinate_timezone":state.config.event.coordinate_timezone,"months_back":state.config.event.months_back,"months_forward":state.config.event.months_forward},"geo":{"enabled":state.config.geo_enabled,"network":false},"osm":{"enabled":state.config.osm.enabled,"concurrency":1,"batch_size":50,"requests_per_second":state.config.osm.requests_per_second},"web":{"enabled":state.config.web.enabled,"concurrency":state.config.web.concurrency},"pubky":{"enabled":state.config.pubky.enabled,"concurrency":state.config.pubky.concurrency},"statuses":statuses}});
    for kind in crate::resolver::cards::KINDS {
        let c = state.config.cards.resolver(kind);
        body["resolvers"][kind] = json!({"enabled":c.enabled,"concurrency":c.concurrency,"requests_per_second":c.requests_per_second});
    }
    *cache = Some((Instant::now(), body.clone()));
    Ok(body)
}
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Pinakes",
        version = "0.1.0",
        description = "Pinakes ranks, Nexus hydrates. External resource metadata and search.",
        license(name = "MIT")
    ),
    paths(
        group,
        crate::event::storage::list,
        search,
        resource,
        lookup,
        resolve,
        info,
        openapi,
        osm_dump,
        crate::change_api::feed,
        crate::change_api::lookup,
        crate::change_api::resource
    ),
    components(schemas(
        ResourceMeta,
        CollapseMode,
        crate::event::storage::OccurrencePage,
        crate::event::storage::OccurrenceRecord,
        crate::event::storage::OccurrenceRef,
        crate::event::storage::ReferenceEvent,
        crate::change_api::ChangeLine,
        crate::change_api::ChangePage,
        crate::change_api::ChangeLookup,
        model::Point,
        model::Status,
        ResourceView,
        SearchResponse,
        SearchResult,
        ClusterResponse,
        Cluster,
        model::LocationSource,
        Fields,
        LookupRequest,
        ResolveRequest,
        TagFacet,
        Facets,
        OsmDumpLine,
        OsmDumpNotice,
        OsmDumpPlace
    ))
)]
pub struct ApiDoc;
/// Operations served by `admin_listen`, every one behind the bearer check.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Pinakes admin",
        version = "0.1.0",
        description = "Operator API on admin_listen. Every operation requires the admin bearer token when one is configured.",
        license(name = "MIT")
    ),
    paths(curate_group, admin_info, admin_openapi, metrics),
    modifiers(&AdminBearer),
    security(("admin_bearer" = []))
)]
pub struct AdminApiDoc;
struct AdminBearer;
impl utoipa::Modify for AdminBearer {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        openapi
            .components
            .get_or_insert_with(Default::default)
            .add_security_scheme(
                "admin_bearer",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
    }
}
/// The public listener's document: exactly the operations `router` serves.
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
/// The admin listener's document, served there behind the bearer check.
pub fn admin_openapi_document() -> utoipa::openapi::OpenApi {
    AdminApiDoc::openapi()
}
#[utoipa::path(get,path="/v1/openapi.json",responses((status=200,body=Value)))]
async fn openapi() -> Json<Value> {
    Json(serde_json::to_value(openapi_document()).expect("OpenAPI serialization"))
}
#[utoipa::path(get,path="/v1/openapi.json",responses((status=200,body=Value)),description="Admin listener only: this document.")]
async fn admin_openapi() -> Json<Value> {
    Json(serde_json::to_value(admin_openapi_document()).expect("OpenAPI serialization"))
}

/// First line of the OSM dump: the ODbL notice.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct OsmDumpNotice {
    notice: String,
    #[schema(example = "ODbL-1.0")]
    licence: String,
    licence_url: String,
    attribution_url: String,
    #[schema(example = "osm_place")]
    table: String,
}
/// One cached OSM object per line after the notice; every field is present, null where unknown.
/// The projection is explicit (`OSM_DUMP_LINE`): internal columns never reach the dump.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct OsmDumpPlace {
    #[schema(example = "node")]
    osm_type: String,
    #[schema(example = 240109189)]
    osm_id: i64,
    #[schema(required = true)]
    name: Option<String>,
    /// Schema.org type, e.g. `City`.
    r#type: String,
    type_path: Vec<String>,
    /// OSM `key/value` category, e.g. `place/city`.
    #[schema(required = true)]
    category: Option<String>,
    /// Null for cached missing objects.
    #[schema(required = true)]
    point: Option<model::Point>,
    /// `[min_lon, min_lat, max_lon, max_lat]`.
    #[schema(required = true)]
    bbox: Option<Vec<f64>>,
    #[schema(value_type = Object)]
    address: Value,
    /// Allowlisted OSM tags.
    #[schema(value_type = Object)]
    tags: Value,
    #[schema(required = true)]
    display_name: Option<String>,
    /// `ok` or `missing`.
    status: String,
    fetched_at: chrono::DateTime<chrono::Utc>,
    /// Every derived document served publicly, keyed by the version hash that
    /// `/v1/changes` and pinned resource documents name.
    #[schema(value_type = Object)]
    versions: Value,
}
/// A dump line: the notice first, then one place per line.
#[derive(ToSchema)]
#[allow(dead_code, clippy::large_enum_variant)]
#[serde(untagged)]
pub enum OsmDumpLine {
    Notice(OsmDumpNotice),
    Place(OsmDumpPlace),
}
/// The public dump line format (see `OsmDumpPlace`); a new `osm_place` column is not exported
/// until it is added here, to the schema and to docs/api.md.
pub const OSM_DUMP_LINE: &str = "SELECT jsonb_build_object('osm_type',osm_type,'osm_id',osm_id,'name',name,'type',type,'type_path',type_path,'category',category,\
    'point',CASE WHEN point IS NULL THEN NULL ELSE jsonb_build_object('lat',ST_Y(point),'lon',ST_X(point)) END,\
    'bbox',bbox,'address',address,'tags',tags,'display_name',display_name,'status',status,'fetched_at',fetched_at,'versions',versions) \
    FROM osm_place ORDER BY osm_type,osm_id";

#[utoipa::path(get,path="/v1/dumps/osm_place.ndjson",responses((status=200,description="Streaming ODbL derivative database, sent as a file download: one JSON object per line, the licence notice first, then one cached OSM object per line",content_type="application/x-ndjson",body=OsmDumpLine,headers(("Content-Disposition"=String,description="`attachment; filename=\"osm_place.ndjson\"`"))),(status=429,description="Dump request budget exhausted")))]
async fn osm_dump(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Response, ApiError> {
    state
        .budget(
            &[(
                format!("dump:{}", peer.ip()),
                state.config.osm.dump_requests_per_minute,
            )],
            state.config.api.cache_capacity,
            "dump rate limit exceeded",
            1,
        )
        .await?;
    let permit = state.dumps.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "dump concurrency limit exceeded".into(),
        )
    })?;
    let (sender, mut receiver) =
        tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
    let (completed, completion) = tokio::sync::oneshot::channel();
    // The producer owns deadlines even when the client stops polling the response body.
    tokio::spawn(async move {
        let _permit = permit;
        let chunk = Duration::from_secs(state.config.osm.dump_chunk_secs);
        let total = Duration::from_secs(state.config.osm.dump_total_secs);
        let produce = async {
            use futures_util::TryStreamExt;
            let mut tx = state.pool.begin().await.map_err(std::io::Error::other)?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await
                .map_err(std::io::Error::other)?;
            // Streaming SELECT stays open while the client drains the rows. Its
            // lifetime is bounded by the producer's chunk/total deadlines, not
            // the API pool's short statement timeout. LOCAL restores the limit
            // when this read-only transaction is dropped.
            sqlx::query("SET LOCAL statement_timeout = 0")
                .execute(&mut *tx)
                .await
                .map_err(std::io::Error::other)?;
            send_dump(&sender,format!("{}\n",json!({"notice":crate::resolver::osm::ATTRIBUTION,"licence":"ODbL-1.0","licence_url":"https://opendatacommons.org/licenses/odbl/1-0/","attribution_url":"https://www.openstreetmap.org/copyright","table":"osm_place"})),chunk).await?;
            let mut rows = sqlx::query_scalar::<_, Value>(OSM_DUMP_LINE).fetch(&mut *tx);
            while let Some(row) = tokio::time::timeout(chunk, rows.try_next())
                .await
                .map_err(std::io::Error::other)?
                .map_err(std::io::Error::other)?
            {
                send_dump(&sender, format!("{row}\n"), chunk).await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let result = tokio::time::timeout(total, produce)
            .await
            .map_err(std::io::Error::other)
            .and_then(|r| r);
        let _ = completed.send(result.map_err(|_| "OSM dump deadline or read failure"));
    });
    let stream = async_stream::stream! {
        while let Some(item)=receiver.recv().await {yield item;}
        if let Ok(Err(message))=completion.await {yield Err(std::io::Error::other(message));}
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::CACHE_CONTROL, "no-store"),
            // A file: browsers and Swagger UI download it instead of parsing it as one JSON value.
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"osm_place.ndjson\"",
            ),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response())
}

async fn send_dump(
    sender: &tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
    line: String,
    deadline: Duration,
) -> Result<(), std::io::Error> {
    tokio::time::timeout(deadline, sender.send(Ok(line.into())))
        .await
        .map_err(std::io::Error::other)?
        .map_err(std::io::Error::other)
}

pub fn admin_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/groups/{id}", axum::routing::put(curate_group))
        .route("/metrics", get(metrics))
        .route("/v1/info", get(admin_info))
        .route("/v1/openapi.json", get(admin_openapi))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_auth,
        ))
        .with_state(state)
}
async fn admin_auth(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(expected) = &state.config.admin_token {
        let supplied = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));
        let valid = supplied.is_some_and(|supplied| {
            // Compare fixed-size MACs with RustCrypto's constant-time verifier, including wrong-length credentials.
            let mut wanted = SimpleHmac::<blake3::Hasher>::new_from_slice(expected.as_bytes())
                .expect("HMAC accepts any key length");
            wanted.update(b"pinakes admin bearer credential");
            let mut got = SimpleHmac::<blake3::Hasher>::new_from_slice(supplied.as_bytes())
                .expect("HMAC accepts any key length");
            got.update(b"pinakes admin bearer credential");
            wanted.verify_slice(&got.finalize().into_bytes()).is_ok()
        });
        if !valid {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "admin credential required",
            )
                .into_response();
        }
    }
    next.run(request).await
}
#[utoipa::path(get,path="/metrics",responses((status=200,content_type="text/plain; version=0.0.4",body=String)),description="Prometheus metrics.")]
async fn metrics(State(state): State<AppState>) -> Result<Response, ApiError> {
    state.metrics.refresh(&state.pool).await?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.encode()?,
    )
        .into_response())
}

#[utoipa::path(get,path="/v1/groups/{id}",params(("id"=String,Path)),responses((status=200,body=crate::groups::GroupView),(status=400,description="Invalid ID or disabled module"),(status=404,description="Unknown group")))]
pub async fn group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !model::valid_id(&id) || !state.config.groups.enabled {
        return Err(ApiError::bad("invalid or disabled group"));
    }
    let value: Option<Value> = sqlx::query_scalar("SELECT group_detail($1)")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;
    value
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "group not found".into()))
}
#[utoipa::path(put,path="/v1/groups/{id}",params(("id"=String,Path)),request_body=crate::groups::Curation,responses((status=200,body=crate::groups::GroupView),(status=400,description="Invalid curation")),description="Admin listener only. Replaces the curated member set; never changes IDs or references.")]
pub async fn curate_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<crate::groups::Curation>,
) -> Result<Json<Value>, ApiError> {
    crate::groups::curate(&state.pool, &id, body, &state.config.groups)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::warn!(error=%e,"group curation failed");
            if e.is::<crate::groups::GroupBusy>() {
                return ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "group writer busy; retry later".into(),
                );
            }
            if !e.is::<crate::groups::CurationError>() {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "group curation unavailable".into(),
                )
            } else {
                ApiError::bad("invalid group curation")
            }
        })
}
