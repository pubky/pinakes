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
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify};
use utoipa::{IntoParams, OpenApi, ToSchema};

/// When each new preview of the last hour was taken and by which client address, oldest first
/// (`AppState::reserve_preview`).
type Previews = Arc<std::sync::Mutex<std::collections::VecDeque<(Instant, std::net::IpAddr)>>>;
/// A place taken in the instance's hour of new previews. It is given back when dropped, unless
/// kept once the link is stored: a request that fails or is cancelled first frees it.
struct PreviewSlot {
    previews: Previews,
    at: Instant,
    by: std::net::IpAddr,
    kept: bool,
}
impl PreviewSlot {
    fn keep(mut self) {
        self.kept = true;
    }
}
impl Drop for PreviewSlot {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        let mut previews = self
            .previews
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = previews
            .iter()
            .rposition(|taken| *taken == (self.at, self.by))
        {
            previews.remove(at);
        }
    }
}
/// A request clock (`AppState::clock`).
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
/// When [`shown_providers`] read, and what it found.
type Shown = (Instant, Vec<model::CreditProvider>);
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Config,
    pub nexus: Nexus,
    pub wake: Arc<Notify>,
    pub metrics: Arc<crate::telemetry::Metrics>,
    limiter: Arc<Mutex<HashMap<String, (Instant, usize)>>>,
    /// ODbL dump downloads per client address in the current hour.
    downloads: Arc<Mutex<HashMap<std::net::IpAddr, (Instant, usize)>>>,
    /// When each new preview of the last hour was taken, oldest first: at most
    /// `limits.preview_new_per_hour` for the whole instance.
    previews: Previews,
    info_cache: Arc<Mutex<Option<(Instant, Value)>>>,
    /// The providers whose values some stored card holds, and when that was read
    /// ([`shown_providers`]).
    shown: Arc<Mutex<Option<Shown>>>,
    pub(crate) cursor_mac: SimpleHmac<blake3::Hasher>,
    /// The request clock for relative times and search cursors; tests set it, the daemon never
    /// does (wall clock).
    pub clock: Option<Clock>,
}
impl AppState {
    pub fn new(pool: PgPool, config: Config) -> anyhow::Result<Self> {
        let mut cursor_key = [0; 32];
        getrandom::fill(&mut cursor_key).context("cursor key entropy unavailable")?;
        let metrics = Arc::new(crate::telemetry::Metrics::new()?);
        Ok(Self {
            pool,
            nexus: Nexus::configured(&config)?,
            config,
            wake: Arc::default(),
            metrics,
            limiter: Arc::default(),
            downloads: Arc::default(),
            previews: Arc::default(),
            info_cache: Arc::default(),
            shown: Arc::default(),
            cursor_mac: SimpleHmac::<blake3::Hasher>::new_from_slice(&cursor_key)
                .context("initialize cursor MAC")?,
            clock: None,
        })
    }
    /// Now, for relative times and search cursors.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.as_ref().map_or_else(Utc::now, |clock| clock())
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
    /// `cost` units of the per-IP budget of the public routes for `ip`: what a request that
    /// does not pass `public_budget` costs (an MCP tool's call to Nexus).
    pub(crate) async fn charge(&self, ip: std::net::IpAddr, cost: usize) -> Result<(), ApiError> {
        self.budget(
            &[(format!("api-ip:{ip}"), self.config.api.requests_per_minute)],
            self.config.api.cache_capacity,
            "API rate limit exceeded",
            cost,
        )
        .await
    }
    /// One ODbL dump download for `ip` in its current hour, at most
    /// `limits.osm_dump_downloads_per_hour`.
    async fn download_budget(&self, ip: std::net::IpAddr) -> Result<(), ApiError> {
        let hour = Duration::from_secs(3600);
        let mut downloads = self.downloads.lock().await;
        downloads.retain(|_, (at, _)| at.elapsed() < hour);
        if !downloads.contains_key(&ip) && downloads.len() >= self.config.api.cache_capacity {
            if let Some(oldest) = downloads.iter().min_by_key(|(_, v)| v.0).map(|(k, _)| *k) {
                downloads.remove(&oldest);
            }
        }
        let (_, count) = downloads.entry(ip).or_insert_with(|| (Instant::now(), 0));
        if *count >= self.config.osm.dump_downloads_per_hour {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "dump download limit exceeded".into(),
            ));
        }
        *count += 1;
        Ok(())
    }
    /// A place for one new preview from `by` in the instance's hour; or the refusal and the
    /// seconds until a place frees: 429 when `by` has taken its share, 503 when the hour is full.
    fn reserve_preview(&self, by: std::net::IpAddr) -> Result<PreviewSlot, (StatusCode, u64)> {
        let hour = Duration::from_secs(3600);
        let wait = |at: &Instant| hour.saturating_sub(at.elapsed()).as_secs().max(1);
        let mut previews = self
            .previews
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while previews.front().is_some_and(|(at, _)| at.elapsed() >= hour) {
            previews.pop_front();
        }
        let own: Vec<&Instant> = previews
            .iter()
            .filter(|(_, taker)| *taker == by)
            .map(|(at, _)| at)
            .collect();
        let share = self.config.preview_new_per_client_per_hour;
        if share > 0 && own.len() >= share {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                own.first().map_or(1, |at| wait(at)),
            ));
        }
        if previews.len() >= self.config.preview_new_per_hour {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                previews.front().map_or(1, |(at, _)| wait(at)),
            ));
        }
        let at = Instant::now();
        previews.push_back((at, by));
        Ok(PreviewSlot {
            previews: self.previews.clone(),
            at,
            by,
            kept: false,
        })
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
    // A reach search (`reach` or `boost`, each a set read from Nexus) and a related search cost
    // their own units, a search with several all of them together; anything else costs one.
    let (mut reach, mut boost, mut related) = (false, false, false);
    if request.method() != axum::http::Method::OPTIONS && request.uri().path() == "/v1/search" {
        let query = request.uri().query().unwrap_or_default();
        for (key, _) in url::form_urlencoded::parse(query.as_bytes()) {
            reach |= key == "reach";
            boost |= key == "boost";
            related |= key == "related";
        }
    }
    let api = &state.config.api;
    let cost = match (reach, boost, related) {
        (false, false, false) => 1,
        _ => {
            (usize::from(reach) + usize::from(boost)) * api.reach_cost
                + usize::from(related) * api.related_cost
        }
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
    // No validator badge: off localhost Swagger UI would load validator.swagger.io as an image.
    Router::from(
        utoipa_swagger_ui::SwaggerUi::new("/swagger-ui")
            .config(utoipa_swagger_ui::Config::new(["/v1/openapi.json"]).validator_url("none")),
    )
}
/// The vendored bundle (Swagger UI 5.17.14) runs no inline script. Its standalone preset renders
/// the logo SVG with one fixed `<style>` element, allowed by its hash (a test recomputes it from
/// the served bundle), and its CSS embeds eight data: images.
pub const SWAGGER_UI_CSP: &str = "default-src 'self'; style-src 'self' 'sha256-RL3ie0nH+Lzz2YNqQN83mnU0J1ot4QL7b99vMdIX99w='; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";
/// Outermost, so `/swagger-ui` redirects, assets, asset 404s and budget 429s all carry the
/// headers, and no other path does (a layer on the merged router would also wrap its fallback,
/// i.e. every unmatched public path).
async fn swagger_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    let swagger = path == "/swagger-ui" || path.starts_with("/swagger-ui/");
    let mut response = next.run(request).await;
    if swagger {
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static(SWAGGER_UI_CSP),
        );
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            header::HeaderValue::from_static("nosniff"),
        );
    }
    response
}
pub fn router(state: AppState) -> Router {
    std::sync::LazyLock::force(&PUBLIC_DOCUMENT);
    let routes = rest_routes();
    let mut all = routes.clone().merge(crate::agents::routes());
    if state.config.api.mcp {
        // The tools answer through the REST routes, under the same layers.
        let rest = with_public_layers(routes, state.clone());
        // A GET (and so a HEAD) is a browser's or a probe's (`agents::mcp_get`); every other
        // method is MCP's.
        let mcp = crate::agents::mcp::service(rest, state.clone());
        all = all.route(
            "/mcp",
            axum::routing::get(crate::agents::mcp_get).fallback_service(mcp),
        );
    }
    with_public_layers(all.merge(swagger_ui(&state.config.api)), state)
}
/// The public `/v1` routes.
fn rest_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/info", get(info))
        .route("/v1/changes/lookup", post(crate::change_api::lookup))
        .route("/v1/search", get(search))
        .route("/v1/groups/{id}", get(group))
        .route("/v1/resources/lookup", post(lookup))
        .route("/v1/resources/by-uri", get(by_uri))
        .route("/v1/resources/{id}", get(resource))
        .route(
            "/v1/resources/{id}/occurrences",
            get(crate::event::storage::list),
        )
        .route("/v1/resources/{id}/episodes", get(crate::episodes::list))
        .route(
            "/v1/resources/{id}/collections",
            get(crate::collections::list),
        )
        .route(
            "/v1/occurrences/lookup",
            post(crate::event::storage::lookup),
        )
        .route("/v1/resolve", post(resolve))
        .route("/v1/openapi.json", get(openapi))
        .route("/v1/dumps/osm_place.ndjson.gz", get(osm_dump))
}
/// The public listener's layer stack around `routes`: the panic backstop innermost, then CORS
/// preflights, the per-IP budget, CORS headers, the body limit and, outermost, the path-scoped
/// Swagger headers. Public so a test can mount a route under exactly this stack.
pub fn with_public_layers(routes: Router<AppState>, state: AppState) -> Router {
    let mut router = panic_backstop(routes, &state, "public")
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
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024));
    if state.config.api.swagger_ui {
        router = router.layer(axum::middleware::from_fn(swagger_headers));
    }
    router.with_state(state)
}
/// Marks a plain-text answer a handler wrote itself, which quotes no input: the rejection layer
/// leaves it as it is.
#[derive(Clone, Copy)]
pub(crate) struct OwnText;
/// Marks a response the panic backstop produced, so the layer around it can log the path.
#[derive(Clone, Copy)]
struct CaughtPanic;
/// An extractor's rejection (a query, path or body axum could not read) answers in plain text
/// with the input it could not read: an unknown parameter's name, a value of the wrong type, a
/// bare string where an object was expected. Any of them may be a link. The answer keeps its
/// status and says what the route takes (`rejection_message`), never the input.
async fn rejection_without_input(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    if response.extensions().get::<OwnText>().is_some() {
        return response;
    }
    let plain = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/plain"));
    if !response.status().is_client_error() || !plain {
        return response;
    }
    let status = response.status();
    let message = rejection_message(&method, &path, status);
    (status, Json(json!({ "error": message }))).into_response()
}
/// What a route takes, for a request axum could not read.
fn rejection_message(method: &axum::http::Method, path: &str, status: StatusCode) -> &'static str {
    match status {
        StatusCode::PAYLOAD_TOO_LARGE => return "the body is over 2 MB",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => {
            return "the body must be JSON (content-type: application/json)"
        }
        _ => {}
    }
    if method == axum::http::Method::GET {
        return "the path or a query parameter could not be read; /v1/openapi.json lists what this route takes";
    }
    match path {
        "/mcp" => "an MCP message is one JSON-RPC body (content-type: application/json) from a client that accepts application/json and text/event-stream",
        "/v1/resolve" => "the body must be a JSON object with a `uri` string",
        "/v1/resources/lookup" => {
            "the body must be a JSON object with `ids` or `uris`, arrays of strings"
        }
        "/v1/changes/lookup" | "/v1/occurrences/lookup" => {
            "the body must be a JSON object with `resource_ids` or `uris`, arrays of strings"
        }
        p if p.starts_with("/v1/groups/") => {
            "the body must be a JSON object with a `members` array"
        }
        _ => "the body must be JSON as /v1/openapi.json describes",
    }
}
/// Backstop: a handler panic answers 500 (counted in `pinakes_api_panics_total`) instead of
/// dropping the connection, and the request path is logged (never the payload). Innermost, so
/// the listener's other layers (CORS, Swagger headers) still shape the response. Around it, an
/// extractor's rejection is answered without the input it quotes (`rejection_without_input`).
fn panic_backstop(
    routes: Router<AppState>,
    state: &AppState,
    listener: &'static str,
) -> Router<AppState> {
    let panics = state.metrics.api_panics.clone();
    routes
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            move |_: Box<dyn std::any::Any + Send>| {
                panics.inc();
                let mut response = ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
                .into_response();
                response.extensions_mut().insert(CaughtPanic);
                response
            },
        ))
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| async move {
                let path = request.uri().path().to_owned();
                let response = next.run(request).await;
                if response.extensions().get::<CaughtPanic>().is_some() {
                    tracing::error!(listener, path = %path, "API handler panicked; answered 500");
                }
                response
            },
        ))
        .layer(axum::middleware::from_fn(rejection_without_input))
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
        let error: anyhow::Error = error.into();
        tracing::error!(error=%crate::resolver::log_error(&error),"request failed");
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    pub q: Option<String>,
    /// One schema.org type, or up to 10 comma-separated: any of them (OR), each with its
    /// subtypes (`Event` matches `MusicEvent`).
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub category: Option<String>,
    pub host: Option<String>,
    /// Links carrying one exact identifier, on any service and in any spelling:
    /// `mbid:{uuid}` (a MusicBrainz recording, release, release group or artist, or the MBID a
    /// MusicBrainz page was merged from),
    /// `wikidata:{Q-id}`, `isrc:{code}`, `upc:{barcode}`, `isbn:{ISBN-10 or ISBN-13}`,
    /// `imdb:{tt…}` (an IMDb title), `podcast:{GUID}` (a podcast's `podcast:guid`),
    /// `osm:{type}/{id}`, `doi:{DOI}`, `gtin:{barcode}` (a product, in any of its lengths), any
    /// key exactly as a resource read lists it in `keys`, or another namespace a source records
    /// (its page in docs/sources names it).
    pub key: Option<String>,
    /// Resource ID: what the people who tagged it also referenced, ranked by how many of them
    /// did, then by the newest of those references (not with `sort`). A resource in a group
    /// stands for the group, whose members are never listed.
    pub related: Option<String>,
    pub tags: Option<String>,
    #[serde(rename = "match")]
    pub match_mode: Option<String>,
    pub author: Option<String>,
    /// `following`, `friends` or `wot_2` of `viewer`: only references by authors in that set
    /// count (a filter). `wot_2` needs a Nexus that serves the reach-set route (else 501).
    pub reach: Option<String>,
    /// `following`, `friends` or `wot_2` of `viewer`: cards that authors in that set referenced
    /// rank higher, the more of them and the more recently they did, and nothing is left out
    /// (with the default ranking, `sort=relevance` or `sort=trending`).
    pub boost: Option<String>,
    pub viewer: Option<String>,
    /// `relevance` (how well `q` matches: the name, then tag labels, then the description;
    /// the activity score breaks ties; needs `q`), `activity`, `recent`, `distance` (needs
    /// `near`), `start` or `trending`. Default: `relevance` when `q` is set (not with `related`),
    /// else `activity`.
    pub sort: Option<String>,
    /// With `sort=trending` or `related`: how far back references count, `1h`–`90d` (`{n}h` or
    /// `{n}d`), default `7d` for trending and `limits.related_window_days` for related.
    pub window: Option<String>,
    #[param(minimum = 1, maximum = 500)]
    pub limit: Option<usize>,
    /// Authenticated opaque keyset cursor from this daemon; invalid after restart. Restart pagination on HTTP 400.
    pub cursor: Option<String>,
    /// Counts over the whole filtered set, comma-separated, any of: `type` (card types), `tags`
    /// (labels), `host` (services, as `host` takes them), `category` (categories such as
    /// OpenStreetMap's `amenity/cafe`, as `category` takes them); `collapse` changes the rows,
    /// not the counts.
    pub facet: Option<String>,
    /// West,south,east,north in degrees; west > east crosses the antimeridian.
    pub bbox: Option<String>,
    /// Latitude,longitude in degrees; requires radius and excludes bbox.
    pub near: Option<String>,
    /// Positive metres, at most the configured maximum (200000 by default).
    pub radius: Option<String>,
    /// Inclusive start time lower bound: RFC3339, Unix milliseconds, or relative to the request
    /// (`now`, `now+7d`, `now-12h`; the current minute).
    pub from: Option<String>,
    /// Inclusive start time upper bound: RFC3339, Unix milliseconds, or relative (`now+30d`).
    pub to: Option<String>,
    /// Finite map zoom; fractional zooms are floored and clamped to the table. Requires bbox; large views coarsen to the configured cell budget.
    pub cluster: Option<String>,
    /// Resource ID of the accepted event venue; filters event units.
    pub venue: Option<String>,
    /// A collection post's URI (an item's `uri` from `GET /v1/resources/{id}/collections`): the
    /// cards whose link, or an alias's, it holds.
    pub in_collection: Option<String>,
    /// `true`: job postings that may be done remotely; `false`: job postings that may not.
    pub remote: Option<bool>,
    /// `true`: job postings past their `validThrough` (closed) are listed too; they are left out
    /// otherwise.
    pub closed: Option<bool>,
    /// Recipes with these ingredients: up to three, comma-separated, each of letters, digits,
    /// spaces and hyphens. Each must match a whole word (or words) of an ingredient's name as the
    /// recipe's card records it, without amounts (`flour` matches "all-purpose flour").
    pub ingredient: Option<String>,
    /// A list view with one row per group: the matching member with the most references, as its
    /// own card; links in no group stay rows of their own. Not with `cluster`.
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
/// The one card every route returns for a resource, occurrence or reference event. Absent
/// values are null, never "".
#[derive(Serialize, ToSchema)]
pub struct Card {
    pub id: String,
    pub uri: String,
    pub status: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// The type's ancestors, root first (`["Thing", "Event", "MusicEvent"]`): a search for
    /// `type=Event` matches this card when `Event` is among them.
    pub type_path: Vec<String>,
    /// Plain text, entity-decoded: escape it before rendering it as HTML.
    pub name: Option<String>,
    /// Plain text, entity-decoded: escape it before rendering it as HTML.
    pub description: Option<String>,
    pub image: Option<String>,
    /// Audio a client can play with one player for every kind: a podcast episode's enclosure,
    /// or a recording's public preview (`preview: true`). Only public, direct links, never
    /// signed, expiring or behind a login (docs/api.md, Stream). Absent when the card has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<model::Stream>,
    /// The frame a client may load, sandboxed, to show the link as its own site does
    /// (docs/api.md, Embed): a service's player, a social post, a map, slides. From the link
    /// itself for the services Pinakes knows, else from the page's oEmbed answer or its own player
    /// tags. Null when there is none, and on a card whose link is gone (`missing`).
    pub embed: Option<crate::embed::Embed>,
    /// On a podcast show's card (`PodcastSeries`, status `ok`): its latest episodes that have a
    /// `stream`, newest first, at most 5, each as `GET /v1/resources/{id}/episodes` lists it (its
    /// group's list when it has none of its own: an Apple Podcasts or Spotify show grouped with
    /// its feed). Read from the list the show's last read stored, never from the feed. Absent
    /// on every other card and on a show without one (docs/api.md, Episodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episodes: Option<Vec<crate::episodes::Episode>>,
    /// The publishing site: the app for `pubky://` objects (e.g. `loopky`); the service a
    /// link's URL names (`Apple Music`, `Wikipedia (de)`, `MusicBrainz`, `Wikidata`,
    /// `OpenStreetMap`); else the site name the document gives, else the host. Plain text, like
    /// every text field.
    pub site: Option<String>,
    pub point: Option<model::Point>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub group: Option<GroupRef>,
    /// The same thing elsewhere: the other members of its group (tagged links, with their ID),
    /// then derived links nobody tagged (`derived: true`), by service name; one per service, and
    /// never another link on the card's own service. Of a recording's or an album's links, the one
    /// on the card's own release, else on the original edition (docs/api.md, Also on). At most 8
    /// on a search, lookup or group card, 256 on a resource read. A hint: every member keeps its
    /// own card.
    pub also_on: Vec<AlsoOn>,
    /// The links people referenced on Pubky that resolve to this card: its own `uri` and its
    /// `aliases` that carry references (the 31 most recently referenced), most referenced first
    /// (counted up to 1,000 each, then by URI), at most 5, normalized as every URI is. A typed
    /// card's `uri` is often not a link anyone wrote (`btc:block/…`, `isbn:…`): ask Nexus by
    /// these to find the tags (docs/api.md, Joining with Nexus). An occurrence's are its feed's.
    /// A gone social post's are its own link's alone.
    /// Empty when nothing references the card.
    pub refs: Vec<String>,
    /// How many public collection posts hold the card's link, its own or one of its aliases' (the
    /// 31 most recently collected), each once, counted up to 1,000 (`1000`: 1,000 or more); 0
    /// when none does. The collections are
    /// `GET /v1/resources/{id}/collections` (docs/api.md, Collections). An occurrence's are its
    /// feed's.
    pub collections: i64,
    /// The clients that open a Pubky app's object, from the app's profile (docs/pubky-apps.md);
    /// on an event card, those of the event object it names. Empty for every other card.
    pub open_in: Vec<model::OpenIn>,
    /// OpenStreetMap cards, events placed at an OSM venue and events whose zone was derived from
    /// coordinates: the ODbL attribution to display with the card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    /// Where `image` and `description` came from when Pinakes added them from another source
    /// than the link itself (a MusicBrainz card's Wikipedia or Wikidata text, its Commons image,
    /// or an album's front cover from the Cover Art Archive; a Wikidata item's card's Commons
    /// image; a film's or series' poster or overview from TMDB): one credit per such field, with
    /// its source page and, when it has them, its author and licence (a cover and TMDB's values
    /// have neither: both are null), to show with the value (docs/api.md, Credits; a value
    /// credited to `tmdb` also with TMDB's logo and notice, `/v1/info` `attributions`). At most
    /// 4; `[]` when none. A field without a credit came from the link itself.
    pub credits: Vec<model::Credit>,
    /// For a calendar occurrence: which feed instance this card is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<OccurrenceKey>,
    /// For a Nexus event reference: the reference this card is.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<crate::event::storage::ReferenceEvent>)]
    pub event: Option<Value>,
    /// On an event card: other events that are possibly the same one, best first (the rule:
    /// "Possibly the same event" in docs/api.md); on an app's store card, other store cards that
    /// are possibly the same app ("Possibly the same app"); on a MusicRecording card, other
    /// recordings that are possibly the same one ("Possibly the same recording"). Up to three on
    /// a search, lookup or group card and up to ten on a resource read. A hint only: never a
    /// group, never counted, and never a member of this card's own group. Absent on other cards,
    /// and on a kind's cards when its hint is off (`[event] same_event_hint` for events and apps,
    /// `[groups] same_recording_hint`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maybe_same: Option<Vec<SameEvent>>,
    /// On a `related` search row only: why it is listed, from what its rank counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub because: Option<Because>,
}
/// Why a `related` row is listed: the resource's taggers who also referenced it.
#[derive(Clone, Serialize, ToSchema)]
pub struct Because {
    /// How many of the resource's sampled taggers referenced this row (with `collapse=group`,
    /// its group's members together, each tagger once): the count the row is ranked by.
    pub shared: i64,
    /// Up to three of them (Pubky public keys), those who tagged the resource most recently
    /// first. Ask Nexus for their names and tags.
    pub taggers: Vec<String>,
}
/// Another event, app or recording that is possibly the same one. An event: it starts together
/// with this one, near it (or, when either has no place, at the same venue or under the same
/// organizer), under a similar name, on another page. On an app's store card: another store card
/// with the same website host and name (`start` is absent, `score` 1). A recording: the same title
/// and first artist, and a length within 3 s when both are known, on another page, with no key
/// that joins them.
#[derive(Clone, Serialize, ToSchema)]
pub struct SameEvent {
    /// Its card's ID (a feed's, for an occurrence).
    pub id: String,
    /// Its page: the occurrence's own page, or the feed's or event's link (for a feed read from a
    /// service's API, that service's page); a recording's link.
    pub uri: String,
    pub site: Option<String>,
    /// An event's start; absent for a recording.
    pub start: Option<DateTime<Utc>>,
    /// An event: how alike the two names are, from 0.5 to 1 (pg_trgm similarity of the normalised
    /// names). An app: 1. A recording: 1 when both lengths are known and within 3 s, 0.7 when one
    /// is unknown.
    pub score: f64,
}
/// The group this card is in (its other members are in `also_on`). Nothing is merged; every
/// member keeps its own card.
#[derive(Serialize, ToSchema)]
pub struct GroupRef {
    pub id: String,
    /// Members in the group, this one included.
    pub size: i64,
}
/// One entry of a card's `also_on`: a tagged member of its group (with `id`) or a derived link.
#[derive(Serialize, ToSchema)]
pub struct AlsoOn {
    /// A tagged member's resource ID; absent on a derived link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub uri: String,
    /// The service: a member's site as its card names it, a derived link's service name.
    pub site: Option<String>,
    /// A link nobody tagged, found by a source: never fetched, counted or ranked.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub derived: bool,
    /// For a derived link: the hub or service it came through (`musicbrainz`, `wikidata`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// For a derived link, a qualifier (`archived`, `work`, `source`, `submitted`, `published`,
    /// `pdf`), or `maybe`: a match by metadata (artist, title, length), possibly the same
    /// thing, never by an exact ID; it joins no group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// For a derived link, a time that belongs to it (an archived copy's capture).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// The frame that shows this link (docs/api.md, Embed), as on a card: from the link itself,
    /// else, for a tagged member, the one its own card stores. Null when there is none.
    pub embed: Option<crate::embed::Embed>,
}
#[derive(Serialize, ToSchema)]
pub struct OccurrenceKey {
    pub feed_id: String,
    /// The feed's URI: the link people tagged. To combine with Nexus, send this, not the card's
    /// own `uri` (the occurrence's URL, which nobody tagged).
    pub feed_uri: String,
    pub uid: String,
    pub recurrence_id: Option<String>,
    /// The instance's lifecycle: `scheduled`, `cancelled`, `postponed`, `rescheduled` or
    /// `moved_online`. The card's own `status` is the feed's fetch health.
    pub status: String,
}
/// `GET /v1/resources/{id}`, `GET /v1/resources/by-uri` and `POST /v1/resolve`: the card with
/// the resource's further fields, one flat object (`api_shape` 2 in `/v1/info`).
#[derive(Serialize, ToSchema)]
pub struct ResourceDetail {
    #[serde(flatten)]
    pub card: Card,
    #[serde(flatten)]
    pub details: Details,
}
/// The fields a resource read adds to the card.
#[derive(Serialize, ToSchema)]
pub struct Details {
    pub category: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub address: Option<Value>,
    pub bbox: Option<[f64; 4]>,
    pub tz: Option<String>,
    pub all_day: Option<bool>,
    pub event_status: Option<String>,
    pub attendance: Option<String>,
    pub venue: Option<VenueRef>,
    pub organizer: Option<OrganizerRef>,
    /// Where the card's metadata came from when it is not the page itself: the official API
    /// that answered (docs/sources).
    pub from: Option<String>,
    /// The fields that API names for the object, or a Pubky app profile's facts, or a
    /// MusicBrainz page's own (`artist`, `duration` in seconds, `genre`, `disambiguation`, an
    /// artist's `country_code`, an album's `release_type`, …; docs/api.md), then the facts the also-on
    /// sources read for it (such as a music link's `isrc`, `upc` and `mbid`; each source's page
    /// in docs/sources lists its own), by name; several values are joined with "; ".
    /// `null` when there are none.
    #[schema(value_type = Option<Object>)]
    pub facts: Option<std::collections::BTreeMap<String, String>>,
    /// The objects the card points at, each with its own card (docs/api.md, Related objects): a
    /// Bitcoin transaction's block, inputs and outputs; a block's previous and next block and
    /// coinbase transaction; an address's latest transactions; a Lightning node's biggest
    /// channels and their peers; a channel's nodes and funding transaction. Each is
    /// `{rel, uri, name?, amount_sat?, at?}`; `uri` is a root `POST /v1/resolve` reads, or null
    /// for an input or output without an address (`name` says what it is). At most 10 of a
    /// `rel`, read with the card; `[]` on every other card.
    pub related: Vec<model::Related>,
    /// How many entries of a `rel` there are when `related` lists fewer of them
    /// (`{"input": 400}`); null when it lists them all.
    #[schema(value_type = Option<Object>)]
    pub related_total: Option<std::collections::BTreeMap<String, u64>>,
    /// The public identifiers this resource is keyed by, as `GET /v1/search?key=` takes them
    /// (`isrc:…`, `upc:…`, `doi:…`, `isbn:…`, `mb:recording:…`, `wikidata:Q…`, `osm:node/…`):
    /// the keys its sources vouch for, sorted; `[]` when there are none.
    pub keys: Vec<String>,
    /// Other spellings of this link that resolve to it (its aliases), by URI, at most 50. A
    /// page's own claims (`url`, `@id`, `sameAs`, canonical links) are not aliases and are not
    /// listed. The card's `uri` is the canonical one: an alias is answered with its root's card.
    /// A gone social post lists none: a spelling's link can carry the post's title.
    pub aliases: Vec<String>,
    pub updated_at: Option<DateTime<Utc>>,
}
#[derive(Serialize, ToSchema)]
pub struct OrganizerRef {
    pub name: Option<String>,
    pub url: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct VenueRef {
    pub name: Option<String>,
    pub id: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct TagFacet {
    pub label: String,
    pub count: i64,
}
#[derive(Serialize, ToSchema)]
pub struct TypeFacet {
    #[serde(rename = "type")]
    pub type_name: String,
    /// Where the type sits in schema.org, from `Thing` down to the type itself
    /// (`["Thing", "CreativeWork", "MusicRecording"]`): enough to build the tree and roll
    /// counts up into families.
    pub type_path: Vec<String>,
    pub count: i64,
}
#[derive(Serialize, ToSchema)]
pub struct HostFacet {
    /// The host as `host` takes it: exactly as the links' URLs name it (`www.youtube.com` and
    /// `youtube.com` are two hosts).
    pub host: String,
    /// The service's name as its cards show it in `site` (`YouTube`, `Wikipedia (en)`), else
    /// the host. Plain text.
    pub site: String,
    pub count: i64,
}
#[derive(Serialize, ToSchema)]
pub struct CategoryFacet {
    /// A category as `category` takes it: a place's OpenStreetMap `key/value` (`amenity/cafe`),
    /// or the `category` a page's schema.org data gives.
    pub category: String,
    pub count: i64,
}
/// The facets `facet` asked for, over the whole filtered set.
#[derive(Serialize, ToSchema)]
pub struct Facets {
    /// `facet=tags`: the labels the filtered links carry, most frequent first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<TagFacet>>,
    /// `facet=type`: the card types of the filtered set, most frequent first.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<TypeFacet>>,
    /// `facet=host`: the services of the filtered set, most frequent first, at most 100.
    /// A host `pubky://` objects use (a user's key) is not a service and is left out.
    #[serde(rename = "host", skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<HostFacet>>,
    /// `facet=category`: the categories of the filtered set (a place's OpenStreetMap
    /// `key/value`, a page's schema.org `category`), most frequent first, at most 100.
    #[serde(rename = "category", skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<CategoryFacet>>,
}
/// The most entries `facet=host` and `facet=category` list each.
pub const FACET_LIST_CAP: i64 = 100;
/// The links per host `facet=host` tries for the service's name (`HostFacet::site`).
const FACET_SITE_SAMPLE: i32 = 8;
/// The facets a search asked for.
#[derive(Clone, Copy, Default)]
struct FacetsAsked {
    tags: bool,
    types: bool,
    hosts: bool,
    categories: bool,
}
impl FacetsAsked {
    fn any(self) -> bool {
        self.tags || self.types || self.hosts || self.categories
    }
}
/// The facets of a search: any of `type`, `tags`, `host` and `category`, comma-separated.
fn facets_asked(facet: Option<&str>) -> Result<FacetsAsked, ApiError> {
    let mut asked = FacetsAsked::default();
    let Some(facet) = facet else {
        return Ok(asked);
    };
    for name in facet.split(',') {
        match name {
            "tags" => asked.tags = true,
            "type" => asked.types = true,
            "host" => asked.hosts = true,
            "category" => asked.categories = true,
            _ => {
                return Err(ApiError::bad(
                    "facet must be one or more of type, tags, host and category, comma-separated",
                ))
            }
        }
    }
    Ok(asked)
}
#[derive(Serialize, ToSchema)]
pub struct SearchResponse {
    pub items: Vec<Card>,
    pub cursor: Option<String>,
    /// The `reach` or `boost` set was capped at 1,000 authors.
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
    pub points: Vec<Card>,
    pub truncated: bool,
    pub total: i64,
    /// The `reach` or `boost` set was capped at 1,000 authors.
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
/// The resource IDs a lookup names: `ids` as given, then each of `uris` as its ID (normalised, as
/// `POST /v1/resources/lookup` reads a link); 1–500 together. An alias ID reads its root where
/// the lookup reads, and an unknown ID adds nothing.
pub(crate) fn lookup_ids(
    ids: Option<Vec<String>>,
    uris: Option<Vec<String>>,
) -> Result<Vec<String>, ApiError> {
    let (ids, uris) = (ids.unwrap_or_default(), uris.unwrap_or_default());
    if !(1..=500).contains(&(ids.len() + uris.len())) {
        return Err(ApiError::bad(
            "resource_ids and uris take 1–500 entries together",
        ));
    }
    if ids.iter().any(|s| !model::valid_id(s)) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    let mut all = ids;
    for uri in &uris {
        all.push(
            model::identity(uri)
                .map_err(|e| ApiError::bad(e.to_string()))?
                .0,
        );
    }
    Ok(all)
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub uri: String,
}

fn present(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
/// The site a card names: the app path segment of a `pubky://` object, else the service a link
/// is on when its URL alone says (a music service, a Wikipedia edition, MusicBrainz, Wikidata, a
/// service the lookups know), so a card not read yet never shows a raw host for it, else the
/// document's site or the host.
pub fn site(uri: &str, doc_site: Option<&str>, host: Option<&str>) -> Option<String> {
    if let Some(path) = uri.strip_prefix("pubky://") {
        let mut parts = path.split('/').skip(1);
        if parts.next() == Some("pub") {
            if let Some(app) = present(parts.next()) {
                return Some(app);
            }
        }
    }
    service_site(uri)
        .or_else(|| present(doc_site))
        .or_else(|| present(host))
}
/// The service a link is on, by its URL alone.
pub(crate) fn service_site(uri: &str) -> Option<String> {
    if let Some(link) = crate::music::parse(uri) {
        return Some(link.service.name().to_owned());
    }
    if let Some(article) = crate::wikipedia::parse(uri) {
        return Some(format!("Wikipedia ({})", article.lang));
    }
    if crate::hubs::musicbrainz::entity_link(uri).is_some()
        || crate::hubs::musicbrainz::isrc_link(uri).is_some()
    {
        return Some("MusicBrainz".into());
    }
    if crate::hubs::wikidata::item_link(uri).is_some() {
        return Some("Wikidata".into());
    }
    crate::lookups::service_name(uri).map(Into::into)
}
/// A card from a row of `search_unit` (or a row shaped like one).
pub(crate) fn card(row: &Value) -> Result<Card, ApiError> {
    let doc: Option<ResourceMeta> = if row["doc"].get("type").is_some() {
        Some(serde_json::from_value(row["doc"].clone())?)
    } else {
        None
    };
    let id = row["id"].as_str().unwrap_or_default().to_owned();
    let occurrence = row
        .get("occurrence")
        .filter(|v| !v.is_null())
        .map(|o| OccurrenceKey {
            feed_id: id.clone(),
            feed_uri: row["uri"].as_str().unwrap_or_default().into(),
            uid: o["uid"].as_str().unwrap_or_default().into(),
            recurrence_id: present(o["recurrence_id"].as_str()),
            status: o["status"].as_str().unwrap_or("scheduled").into(),
        });
    let uri = row["uri"].as_str().unwrap_or_default().to_owned();
    // An occurrence is the feed's row with its own document: its URL, when it has one.
    let uri = match (&occurrence, &doc) {
        (Some(_), Some(d)) => present(Some(&d.url)).unwrap_or(uri),
        _ => uri,
    };
    let also_on = also_on_entries(&row["also_on"], Some(&uri));
    let status: String = row["status"].as_str().unwrap_or("pending").into();
    let type_path: Vec<String> =
        serde_json::from_value(row["type_path"].clone()).unwrap_or_default();
    // A link gone upstream shows nothing of its own.
    let embed = if status == "missing" {
        None
    } else {
        card_embed(
            &uri,
            doc.as_ref().map(EmbedDoc::from).as_ref(),
            row["osm_id"].as_i64().is_some(),
        )
        .map(|e| e.settled(&type_path))
    };
    let event = row.get("event").filter(|v| !v.is_null()).cloned();
    // An event card's clients are its event object's; a read object's are its own, and an object
    // not read yet has its kind's.
    let open_in = match (&event, &doc) {
        (Some(e), _) => match (e["author"].as_str(), e["object_path"].as_str()) {
            (Some(author), Some(path)) => {
                crate::resolver::pubky_apps::open_in(&format!("pubky://{author}{path}"))
            }
            _ => vec![],
        },
        (None, Some(d)) => d.open_in.clone(),
        (None, None) => crate::resolver::pubky_apps::open_in(&uri),
    };
    let description = doc.as_ref().and_then(|d| present(d.description.as_deref()));
    let image = doc.as_ref().and_then(|d| present(d.image.as_deref()));
    // A credit goes with its field's value, never without it.
    let enriched: Vec<model::Credit> = doc
        .as_ref()
        .map(|d| {
            d.credits
                .iter()
                .filter(|c| match c.field {
                    model::CreditField::Image => image.is_some(),
                    model::CreditField::Description => description.is_some(),
                })
                .take(model::CREDITS_CAP)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Ok(Card {
        site: site(
            &uri,
            doc.as_ref().map(|d| d.site.as_str()),
            row["host"].as_str(),
        ),
        id,
        uri,
        status,
        type_name: present(row["type"].as_str()),
        type_path,
        name: present(row["name"].as_str())
            .or_else(|| doc.as_ref().and_then(|d| present(d.name.as_deref()))),
        description,
        image,
        stream: doc.as_ref().and_then(|d| d.stream.clone()),
        embed,
        episodes: None,
        point: doc.as_ref().and_then(|d| d.location.clone()),
        start: doc.as_ref().and_then(|d| d.start),
        end: doc.as_ref().and_then(|d| d.end),
        group: row
            .get("group")
            .and_then(|g| g["id"].as_str().map(|id| (id, g)))
            .map(|(id, g)| GroupRef {
                id: id.into(),
                size: g["size"].as_i64().unwrap_or(1),
            }),
        also_on,
        refs: vec![],
        collections: 0,
        open_in,
        attribution: credits(
            row["osm_id"]
                .as_i64()
                .map(|_| crate::resolver::osm::ATTRIBUTION.to_owned())
                .or_else(|| doc.as_ref().and_then(|d| d.attribution.clone())),
            // A zone derived from coordinates comes from ODbL boundary data: its own credit.
            doc.as_ref()
                .and_then(|d| d.event.timezone_attribution.clone()),
        ),
        credits: enriched,
        occurrence,
        event,
        maybe_same: same_events(row).map(|all| all.into_iter().take(3).collect()),
        because: because(row),
    })
}
/// A card's `embed` (docs/api.md, Embed): the player its link names (`embed::link`), else a
/// Mastodon post's own frame, else an OpenStreetMap card's map (`osm`: the row is an OSM
/// object), else the frame its document stores (its oEmbed answer's or its page's own).
fn card_embed(uri: &str, doc: Option<&EmbedDoc>, osm: bool) -> Option<crate::embed::Embed> {
    // A link found gone, a post among them while a later read fails (`changes::gone_social`: its
    // document stays `missing`), shows nothing of its own.
    if doc.is_some_and(|d| d.status == Some(model::Status::Missing)) {
        return None;
    }
    if let Some(embed) = crate::embed::link(uri) {
        return Some(embed);
    }
    let doc = doc?;
    // A post that Mastodon's own API answered for: another fediverse server may have no `/embed`.
    if doc.source.as_deref() == Some("mastodon")
        && doc.type_name.as_deref() == Some("SocialMediaPosting")
    {
        if let Some(embed) = crate::embed::mastodon(uri) {
            return Some(embed);
        }
    }
    if osm {
        if let Some(embed) = doc
            .location
            .as_ref()
            .and_then(|p| crate::embed::map(p, doc.bbox))
        {
            return Some(embed);
        }
    }
    doc.embed.clone()
}
/// What `card_embed` reads of a stored document: a card's whole one, only these keys of a tagged
/// `also_on` member's (`member_embeds`, `EMBED_DOC`).
#[derive(Default, Deserialize)]
struct EmbedDoc {
    #[serde(default)]
    status: Option<model::Status>,
    #[serde(default)]
    source: Option<String>,
    #[serde(rename = "type", default)]
    type_name: Option<String>,
    #[serde(default)]
    location: Option<model::Point>,
    #[serde(default)]
    bbox: Option<[f64; 4]>,
    #[serde(default)]
    embed: Option<crate::embed::Embed>,
}
impl From<&ResourceMeta> for EmbedDoc {
    fn from(d: &ResourceMeta) -> Self {
        Self {
            status: Some(d.status.clone()),
            source: Some(d.source.clone()),
            type_name: Some(d.type_name.clone()),
            location: d.location.clone(),
            bbox: d.bbox,
            embed: d.embed.clone(),
        }
    }
}
/// The keys of a document `EmbedDoc` reads, as SQL over `resource_document` (an OSM object's
/// document is its `osm_place` version's there).
const EMBED_DOC: &str =
    "jsonb_strip_nulls(jsonb_build_object('status',doc->'status','source',doc->'source',\
'type',doc->'type','location',doc->'location','bbox',doc->'bbox','embed',doc->'embed'))";
/// A related row's `because`, when the row has one.
fn because(row: &Value) -> Option<Because> {
    let b = row.get("because").filter(|v| v.is_object())?;
    Some(Because {
        shared: b["shared"].as_i64().unwrap_or(0),
        taggers: b["taggers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_owned))
                    .take(BECAUSE_TAGGERS)
                    .collect()
            })
            .unwrap_or_default(),
    })
}
/// The most links one card names as `refs`.
const REFS: usize = 5;
/// The most references counted for one link when ranking `refs`: a link past it ranks with the
/// others past it, by URI, and a read never scans a link's every reference.
const REFS_COUNTED: i64 = 1000;
/// The most aliases of one card whose references are counted for `refs`: those most recently
/// referenced, each found by its newest reference alone. A card with many referenced spellings
/// costs a read at most this many counts besides its own link's.
const REFS_ALIASES: i64 = 31;
/// Fills each card's `refs`: the referenced links among its own and its aliases' (the
/// [`REFS_ALIASES`] most recently referenced), most referenced first (counted up to
/// [`REFS_COUNTED`], then by URI), at most [`REFS`]. A gone social post's are its own link's
/// alone: a spelling's link can carry the post's title. One query for every card of a response,
/// and one for the missing cards among them. Then the stored `embed` of tagged `also_on`
/// members (`member_embeds`) and each card's `collections` (`collections::count`), one query
/// more each, and a show's `episodes` (`episodes::on_cards`), only when the response holds a show.
pub(crate) async fn with_refs<'a>(
    pool: &PgPool,
    cards: impl IntoIterator<Item = &'a mut Card>,
) -> Result<(), ApiError> {
    let mut cards: Vec<&mut Card> = cards.into_iter().collect();
    let ids: Vec<String> = cards.iter().map(|c| c.id.clone()).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let missing: Vec<(String, Value)> =
        sqlx::query_as("SELECT id,doc FROM resource WHERE id=ANY($1) AND doc->>'status'='missing'")
            .bind(&ids)
            .fetch_all(pool)
            .await?;
    // A card whose document is `missing` (a link found gone, while a later read failed) takes no
    // frame from its referenced links either: they are its own link's spellings.
    let doc_missing: std::collections::BTreeSet<String> =
        missing.iter().map(|(id, _)| id.clone()).collect();
    let gone: Vec<String> = missing
        .into_iter()
        .filter(|(_, doc)| {
            serde_json::from_value::<ResourceMeta>(doc.clone())
                .is_ok_and(|doc| crate::changes::gone_social(&doc))
        })
        .map(|(id, _)| id)
        .collect();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "WITH cards AS (SELECT DISTINCT id AS card FROM unnest($1::text[]) id), \
         links AS (SELECT card, card AS id FROM cards \
         UNION ALL SELECT c.card, s.alias_id FROM cards c CROSS JOIN LATERAL \
           (SELECT a.alias_id FROM resource_alias a CROSS JOIN LATERAL \
             (SELECT max(f.created_at) AS newest FROM reference f WHERE f.resource_id = a.alias_id) r \
            WHERE a.root_id = c.card AND c.card <> ALL($5) AND r.newest IS NOT NULL ORDER BY r.newest DESC, a.alias_id LIMIT $4) s), \
         counted AS (SELECT l.card, x.uri, c.n FROM links l JOIN resource x ON x.id = l.id \
           CROSS JOIN LATERAL (SELECT count(*) AS n FROM \
             (SELECT 1 FROM reference f WHERE f.resource_id = l.id LIMIT $3) f) c WHERE c.n > 0) \
         SELECT card, uri FROM (SELECT card, uri, row_number() OVER (PARTITION BY card ORDER BY n DESC, uri) AS k FROM counted) s \
         WHERE k <= $2 ORDER BY card, k",
    )
    .bind(&ids)
    .bind(REFS as i64)
    .bind(REFS_COUNTED)
    .bind(REFS_ALIASES)
    .bind(&gone)
    .fetch_all(pool)
    .await?;
    let mut by_card: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (card, uri) in rows {
        by_card.entry(card).or_default().push(uri);
    }
    for card in cards.iter_mut() {
        if let Some(refs) = by_card.get(&card.id) {
            card.refs.clone_from(refs);
        }
    }
    // A card whose canonical link names no player may be read through another link that does
    // (an `http:` spelling's card, a short link): its first referenced link that names one.
    for card in cards.iter_mut() {
        if card.embed.is_none() && card.status != "missing" && !doc_missing.contains(&card.id) {
            card.embed = card
                .refs
                .iter()
                .find_map(|r| crate::embed::link(r))
                .map(|e| e.settled(&card.type_path));
        }
    }
    member_embeds(pool, &mut cards).await?;
    crate::collections::count(pool, cards.iter_mut().map(|c| &mut **c)).await?;
    crate::episodes::on_cards(pool, cards).await
}
/// A tagged member as `member_embeds` reads it: ID, URI, whether it is `missing`, the keys
/// `EmbedDoc` reads (only when its link names no frame), its type path, whether it is an OSM row.
type MemberRow = (String, String, bool, Option<Value>, Vec<String>, bool);
/// Fills the `embed` of each tagged `also_on` member whose link names none (`embed::link`) as its
/// own card shows it (`card_embed`: a Mastodon post's or a map's frame, else the one it stores;
/// docs/api.md, Embed); a `missing` member has none, not even its link's. One query over the
/// tagged members, reading only the keys `card_embed` needs (`EMBED_DOC`) of those whose link
/// names none, and none when no card has a tagged member.
async fn member_embeds(pool: &PgPool, cards: &mut [&mut Card]) -> Result<(), ApiError> {
    let tagged: Vec<String> = cards
        .iter()
        .flat_map(|c| c.also_on.iter())
        .filter_map(|e| e.id.clone())
        .collect();
    if tagged.is_empty() {
        return Ok(());
    }
    let unframed: Vec<String> = cards
        .iter()
        .flat_map(|c| c.also_on.iter())
        .filter(|e| e.embed.is_none())
        .filter_map(|e| e.id.clone())
        .collect();
    // Every tagged member's status (a gone one shows no frame, not even its link's), and the
    // keys `card_embed` reads of those whose link names none.
    let rows: Vec<MemberRow> = sqlx::query_as(&format!(
        "SELECT id, uri, coalesce(status = 'missing' OR doc->>'status' = 'missing', false), \
             CASE WHEN id = ANY($2) THEN {EMBED_DOC} END, coalesce(type_path, '{{}}'), \
             osm_id IS NOT NULL FROM resource_document WHERE id = ANY($1)"
    ))
    .bind(&tagged)
    .bind(&unframed)
    .fetch_all(pool)
    .await?;
    let mut gone = std::collections::BTreeSet::new();
    let mut stored: BTreeMap<String, crate::embed::Embed> = BTreeMap::new();
    for (id, uri, missing, doc, type_path, osm) in rows {
        if missing {
            gone.insert(id);
            continue;
        }
        let Some(doc) = doc.and_then(|d| serde_json::from_value::<EmbedDoc>(d).ok()) else {
            continue;
        };
        if let Some(embed) = card_embed(&uri, Some(&doc), osm)
            .and_then(|e| e.bounded())
            .map(|e| e.settled(&type_path))
        {
            stored.insert(id, embed);
        }
    }
    for entry in cards.iter_mut().flat_map(|c| c.also_on.iter_mut()) {
        let Some(id) = entry.id.as_ref() else {
            continue;
        };
        if gone.contains(id) {
            entry.embed = None;
        } else if entry.embed.is_none() {
            entry.embed = stored.get(id).cloned();
        }
    }
    Ok(())
}
/// A card's `maybe_same` as SQL, over a unit's ID and type path and the two hints' switches (SQL
/// expressions): with `events`, the same-event hint for an event and, for an app's store card, up
/// to `n` other store cards with its `app_match` fact (the same website host and name), each with
/// score 1, by URI; with `recordings`, the same-recording hint for a recording (`maybe_same_hint`);
/// else `NULL`. An alias, a missing card and a member of the card's own group are never named.
pub(crate) fn maybe_same_sql(
    unit: &str,
    type_path: &str,
    n: u32,
    events: &str,
    recordings: &str,
) -> String {
    format!(
        "coalesce(maybe_same_hint({unit},{type_path},{n},{events},{recordings}),CASE WHEN {events} THEN (SELECT jsonb_agg(jsonb_build_object('id',same_app.id,'uri',same_app.uri,'host',same_app.host,'site',same_app.site,'start',NULL,'score',1) ORDER BY same_app.uri) \
         FROM (SELECT DISTINCT same_r.id,same_r.uri,same_r.host,same_r.doc->>'site' AS site FROM resource_fact same_f \
         JOIN resource_fact same_g ON same_g.name='app_match' AND same_g.value=same_f.value AND same_g.resource_id<>same_f.resource_id AND same_g.expires_at>now() AND same_g.source NOT IN (SELECT source FROM also_on_source_off) \
         JOIN resource same_r ON same_r.id=same_g.resource_id \
         WHERE same_f.resource_id={unit} AND same_f.name='app_match' AND same_f.expires_at>now() AND same_f.source NOT IN (SELECT source FROM also_on_source_off) AND same_r.status<>'missing' \
         AND NOT EXISTS (SELECT 1 FROM resource_alias same_alias WHERE same_alias.alias_id=same_r.id) \
         AND NOT EXISTS (SELECT 1 FROM group_unit same_a JOIN group_unit same_b ON same_b.group_id=same_a.group_id WHERE same_a.unit_id={unit} AND same_b.unit_id=same_r.id AND same_a.group_id IS NOT NULL) \
         ORDER BY same_r.uri LIMIT {n}) same_app) END)"
    )
}
/// Whether event cards (and apps' store cards) carry the same-event hint.
pub(crate) fn same_event(config: &Config) -> bool {
    config.event.same_event_hint == crate::config::SameEventHint::On
}
/// Whether recording cards carry the same-recording hint.
pub(crate) fn same_recording(config: &Config) -> bool {
    config.groups.same_recording_hint == crate::config::SameEventHint::On
}
/// The two hints' switches as SQL literals for `maybe_same_sql`, when either hint is on.
fn hint_flags(config: &Config) -> Option<(&'static str, &'static str)> {
    let literal = |on: bool| if on { "true" } else { "false" };
    (same_event(config) || same_recording(config))
        .then(|| (literal(same_event(config)), literal(same_recording(config))))
}
/// The same-event hint a row carries (`same_event_hint` in SQL): absent for a card that is no
/// event, or when the hint is off.
fn same_events(row: &Value) -> Option<Vec<SameEvent>> {
    let entries = row.get("maybe_same")?.as_array()?;
    Some(
        entries
            .iter()
            .map(|m| {
                // An occurrence without a page of its own names its feed: a person's page for it,
                // never an API endpoint.
                let uri = m["uri"].as_str().unwrap_or_default();
                let uri = crate::event::adapters::cursor::human_page(uri)
                    .unwrap_or_else(|| uri.to_owned());
                SameEvent {
                    id: m["id"].as_str().unwrap_or_default().into(),
                    site: site(&uri, m["site"].as_str(), m["host"].as_str()),
                    uri,
                    start: m["start"].as_str().and_then(|s| s.parse().ok()),
                    score: m["score"].as_f64().unwrap_or_default(),
                }
            })
            .collect(),
    )
}
/// Every credit a card owes, once each, joined with "; ".
fn credits(place: Option<String>, zone: Option<String>) -> Option<String> {
    match (place, zone) {
        (Some(p), Some(z)) if p != z => Some(format!("{p}; {z}")),
        (p, z) => p.or(z),
    }
}
/// The serde name of a unit enum value (`EventStatus::Scheduled` → "scheduled").
fn label<T: Serialize>(v: &Option<T>) -> Option<String> {
    v.as_ref()
        .and_then(|v| serde_json::to_value(v).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
}
/// A card's facts: its official API's fields first, then the also-on sources'; a name both
/// give keeps each value once.
fn merge_facts(
    document: Option<std::collections::BTreeMap<String, String>>,
    sources: Option<std::collections::BTreeMap<String, String>>,
) -> Option<std::collections::BTreeMap<String, String>> {
    let mut merged = document.unwrap_or_default();
    for (name, values) in sources.into_iter().flatten() {
        let joined = match merged.remove(&name) {
            Some(first) => {
                let mut all: Vec<&str> = first.split("; ").collect();
                for value in values.split("; ") {
                    if !all.contains(&value) {
                        all.push(value);
                    }
                }
                all.join("; ")
            }
            None => values,
        };
        merged.insert(name, joined);
    }
    (!merged.is_empty()).then_some(merged)
}
/// A card's `also_on` entries from the `also_on()` rows: a member's site as its card names it, a
/// derived link's service name. For a card's view (`card`, its own URI), a music member whose
/// card is not read yet counts for the service its URL names, as the card itself does: no derived
/// link on that service shows beside it (`also_on()` counts only what the stored cards say), and
/// no entry on the card's own service shows at all.
fn also_on_entries(rows: &Value, card: Option<&str>) -> Vec<AlsoOn> {
    let entries: Vec<AlsoOn> = rows
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let uri = e["uri"].as_str().unwrap_or_default().to_owned();
            let derived = e["derived"] == true;
            let embed = crate::embed::link(&uri).map(|e| e.settled(&[]));
            AlsoOn {
                id: e["id"].as_str().map(str::to_owned),
                site: if derived {
                    e["site"].as_str().map(str::to_owned)
                } else {
                    site(&uri, e["site"].as_str(), e["host"].as_str())
                },
                uri,
                derived,
                via: e["via"].as_str().map(str::to_owned),
                kind: e["kind"].as_str().map(str::to_owned),
                at: e["at"].as_str().and_then(|s| s.parse().ok()),
                embed,
            }
        })
        .collect();
    let Some(card) = card else {
        return entries;
    };
    let taken: std::collections::BTreeSet<String> = std::iter::once(card)
        .chain(
            entries
                .iter()
                .filter(|e| !e.derived && crate::music::parse(&e.uri).is_some())
                .map(|e| e.uri.as_str()),
        )
        .filter_map(service_site)
        .map(|s| s.to_lowercase())
        .collect();
    // The card's own service, by its URL: no other page there shows, tagged or derived, its card
    // read or not (a member's stored service may not be known yet).
    let own = service_site(card).map(|s| s.to_lowercase());
    entries
        .into_iter()
        .filter(|e| own.is_none() || service_site(&e.uri).map(|s| s.to_lowercase()) != own)
        .filter(|e| {
            !e.derived
                || !e
                    .site
                    .as_deref()
                    .is_some_and(|s| taken.contains(&s.to_lowercase()))
        })
        .collect()
}
fn detail(row: &Value) -> Result<ResourceDetail, ApiError> {
    let doc: Option<ResourceMeta> = if row["doc"].get("type").is_some() {
        Some(serde_json::from_value(row["doc"].clone())?)
    } else {
        None
    };
    // The alias rows `read` joins in, not the document's claims; the keys its sources vouch for.
    // A gone social post names none: a spelling's link can carry the post's title.
    let aliases: Vec<String> = serde_json::from_value(row["aliases"].clone())
        .ok()
        .filter(|_| !doc.as_ref().is_some_and(crate::changes::gone_social))
        .unwrap_or_default();
    let keys: Vec<String> = serde_json::from_value(row["keys"].clone()).unwrap_or_default();
    let facts: Option<std::collections::BTreeMap<String, String>> =
        serde_json::from_value(row["facts"].clone()).ok().flatten();
    let details = match &doc {
        Some(d) => Details {
            category: present(d.category.as_deref()),
            address: (!d.address.is_empty()).then(|| serde_json::json!(d.address)),
            bbox: d.bbox,
            tz: present(d.event.tz.as_deref()),
            all_day: d.event.all_day,
            event_status: label(&d.event.event_status),
            attendance: label(&d.event.attendance),
            venue: d.event.venue.as_ref().map(|v| VenueRef {
                name: present(v.name.as_deref()).or_else(|| present(Some(&v.text))),
                id: v.resource_id.clone(),
            }),
            organizer: d.event.organizer.as_ref().and_then(|o| {
                let (name, url) = (present(Some(&o.name)), present(o.url.as_deref()));
                (name.is_some() || url.is_some()).then_some(OrganizerRef { name, url })
            }),
            from: crate::sources::from(d).map(str::to_owned),
            facts: merge_facts(
                crate::kinds::facts(
                    crate::sources::facts(d)
                        .or_else(|| crate::resolver::pubky_apps::facts(d))
                        .or_else(|| crate::music_sources::card_facts(d)),
                    d,
                    Utc::now(),
                ),
                facts.clone(),
            ),
            related: d.related.clone(),
            related_total: (!d.related_total.is_empty()).then(|| d.related_total.clone()),
            keys: keys.clone(),
            aliases: aliases.clone(),
            updated_at: row["resolved_at"].as_str().and_then(|s| s.parse().ok()),
        },
        None => Details {
            category: None,
            address: None,
            bbox: None,
            tz: None,
            all_day: None,
            event_status: None,
            attendance: None,
            venue: None,
            organizer: None,
            from: None,
            facts,
            related: vec![],
            related_total: None,
            keys,
            aliases,
            updated_at: None,
        },
    };
    // The read's card, with up to ten possible duplicates.
    let mut card = card(row)?;
    card.maybe_same = same_events(row);
    Ok(ResourceDetail { card, details })
}
/// The types a search's `type` names (1–10, comma-separated), each as its vocabulary spells it.
fn search_types(types: &str) -> Result<Vec<String>, ApiError> {
    let names: Vec<&str> = types.split(',').map(str::trim).collect();
    if names.len() > 10 || names.iter().any(|n| n.is_empty()) {
        return Err(ApiError::bad(
            "type takes 1–10 schema.org types, comma-separated",
        ));
    }
    let mut known = Vec::with_capacity(names.len());
    for name in names {
        let mut path = model::type_path(name);
        let Some(leaf) = path.pop() else {
            // Only a type's name is echoed: a URL or anything else given as a type is not
            // repeated back (it may hold credentials, a query or a fragment).
            let a_name =
                (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric());
            return Err(ApiError::bad(if a_name {
                format!("unknown schema.org type: {name}")
            } else {
                "type names schema.org types (letters and digits, comma-separated)".to_owned()
            }));
        };
        if !known.contains(&leaf) {
            known.push(leaf);
        }
    }
    Ok(known)
}

/// Search's `ingredient` terms, lower case.
fn ingredient_terms(q: &SearchQuery) -> Result<Vec<String>, ApiError> {
    let Some(raw) = &q.ingredient else {
        return Ok(vec![]);
    };
    let terms: Vec<String> = raw
        .split(',')
        .map(|t| {
            t.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        })
        .collect();
    let valid = |t: &String| {
        (1..=64).contains(&t.len())
            && t.chars().any(char::is_alphanumeric)
            && t.chars()
                .all(|c| c.is_alphanumeric() || c == ' ' || c == '-')
    };
    if terms.is_empty() || terms.len() > 3 || !terms.iter().all(valid) {
        return Err(ApiError::bad(
            "ingredient is up to 3 comma-separated words of letters, digits, spaces and hyphens",
        ));
    }
    Ok(terms)
}
fn validate_search(q: &SearchQuery) -> Result<Vec<String>, ApiError> {
    ingredient_terms(q)?;
    if q.venue.as_deref().is_some_and(|v| !model::valid_id(v)) {
        return Err(ApiError::bad("venue must be a resource ID"));
    }
    if q.in_collection
        .as_deref()
        .is_some_and(|c| crate::collections::parse(c).is_none())
    {
        return Err(ApiError::bad(
            "in_collection must be a collection post's pubky:// URI",
        ));
    }
    if q.collapse.as_deref().is_some_and(|v| v != "group") {
        return Err(ApiError::bad("collapse must be group"));
    }
    if !(1..=500).contains(&q.limit.unwrap_or(50)) {
        return Err(ApiError::bad("limit must be 1–500"));
    }
    if !matches!(
        q.sort.as_deref(),
        None | Some("relevance" | "activity" | "recent" | "distance" | "start" | "trending")
    ) {
        return Err(ApiError::bad(
            "sort must be relevance, activity, recent, distance, start or trending",
        ));
    }
    if q.sort.as_deref() == Some("relevance") && query_text(q).is_none() {
        return Err(ApiError::bad(
            "sort=relevance ranks by how well q matches; it needs q",
        ));
    }
    if q.related.as_deref().is_some_and(|id| !model::valid_id(id)) {
        return Err(ApiError::bad("related must be a resource ID"));
    }
    if q.related.is_some() && q.sort.is_some() {
        return Err(ApiError::bad("related ranks the rows; it takes no sort"));
    }
    if q.window.is_some() && q.sort.as_deref() != Some("trending") && q.related.is_none() {
        return Err(ApiError::bad(
            "window applies to sort=trending or related only",
        ));
    }
    trend_window(q.window.as_deref())?;
    if !matches!(q.match_mode.as_deref(), None | Some("any" | "all")) {
        return Err(ApiError::bad("match must be any or all"));
    }
    facets_asked(q.facet.as_deref())?;
    for (name, set) in [("reach", &q.reach), ("boost", &q.boost)] {
        if let Some(set) = set {
            if !crate::nexus::reach_kind(set) || q.viewer.is_none() {
                return Err(ApiError::bad(format!(
                    "{name} must be following, friends or wot_2 and requires viewer"
                )));
            }
        }
    }
    if q.boost.is_some() && q.related.is_some() {
        return Err(ApiError::bad("related ranks the rows; it takes no boost"));
    }
    if q.boost.is_some()
        && !matches!(
            q.sort.as_deref(),
            None | Some("relevance" | "activity" | "trending")
        )
    {
        return Err(ApiError::bad(
            "boost applies to the default ranking, sort=relevance or sort=trending",
        ));
    }
    for id in [&q.viewer, &q.author].into_iter().flatten() {
        if !crate::nexus::valid_author(id) {
            return Err(ApiError::bad("invalid author/viewer ID"));
        }
    }
    if let Some(types) = &q.type_name {
        search_types(types)?;
    }
    for text in [&q.q, &q.category, &q.host].into_iter().flatten() {
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
/// The text of `q`, when there is one to match (an empty `q` filters nothing).
fn query_text(q: &SearchQuery) -> Option<&str> {
    q.q.as_deref().filter(|s| !s.is_empty())
}
/// The ranking a search uses: its `sort`, else `relevance` for a text search (`related` ranks
/// its own rows), else the activity score (`None`).
fn effective_sort(q: &SearchQuery) -> Option<&str> {
    match q.sort.as_deref() {
        Some(sort) => Some(sort),
        None if q.related.is_none() && query_text(q).is_some() => Some("relevance"),
        None => None,
    }
}
/// How much a name match weighs in [`relevance`]: more than the best tag and description
/// matches together (2·[`TAG_WEIGHT`] + 2), so the name always comes first.
pub const NAME_WEIGHT: f64 = 9.0;
/// How much a tag-label match weighs: more than the best description match (2).
pub const TAG_WEIGHT: f64 = 3.0;
/// The labels a tag must equal to match `text` exactly: its words lowercased, joined by a
/// space, a hyphen, an underscore or nothing (`daft punk`, `daft-punk`, `daft_punk`, `daftpunk`).
fn label_spellings(text: &str) -> Vec<String> {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    let mut spellings: Vec<String> = [" ", "-", "_", ""]
        .iter()
        .map(|joint| words.join(joint))
        .collect();
    spellings.dedup();
    spellings
}
/// The longest `q`, in characters (its words and the single spaces between them), that matches
/// a name only at the start of a word: `lol` finds `LOLcats` but not `jimloli`, `ai` finds `AI`
/// and `Airbnb` but not `Rain`, `lnd` finds `lightningnetwork/lnd`. Longer text also matches
/// inside a name and by the trigram fallback (`musical` for `music`). Whole words (name or
/// description) match at any length.
pub const SHORT_QUERY_CHARS: usize = 3;
/// How a short `q` ([`SHORT_QUERY_CHARS`]) of letters and digits matches a name: each word
/// somewhere in it (`ILIKE`, which the name's trigram index serves) and the words, in order and
/// apart only by white space, where a word starts (`~*`: at the start or after a sign that is
/// neither a letter nor a digit, so `/`, `.` and `-` start words too).
struct WordStart {
    patterns: Vec<String>,
    regex: String,
}
/// The [`WordStart`] match of `text`. `None` for longer text, for text with other signs (`c++`
/// and `c#` stay as precise as a match inside the name), and for a script written without spaces
/// between words (Chinese, Japanese, Thai, ...), where only a match inside a run finds a word.
fn word_start(text: &str) -> Option<WordStart> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let chars =
        words.iter().map(|w| w.chars().count()).sum::<usize>() + words.len().saturating_sub(1);
    let plain = |c: char| c.is_alphanumeric() && !unspaced_script(c);
    if words.is_empty() || chars > SHORT_QUERY_CHARS || !words.iter().all(|w| w.chars().all(plain))
    {
        return None;
    }
    // Letters and digits only: nothing to escape in either pattern.
    Some(WordStart {
        patterns: words.iter().map(|w| format!("%{w}%")).collect(),
        regex: format!("(^|[^[:alnum:]]){}", words.join("\\s+")),
    })
}
/// `r.name` matches the short text ([`WordStart`]).
fn push_word_start(sql: &mut QueryBuilder<'_, Postgres>, starts: WordStart) {
    sql.push("(");
    for pattern in starts.patterns {
        sql.push("r.name ILIKE ").push_bind(pattern).push(" AND ");
    }
    sql.push("r.name ~* ").push_bind(starts.regex).push(")");
}
/// A letter of a script written without spaces between words.
fn unspaced_script(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0E00..=0x0EFF // Thai, Lao
            | 0x0F00..=0x0FFF // Tibetan
            | 0x1000..=0x109F // Myanmar
            | 0x1780..=0x17FF // Khmer
            | 0x3005..=0x3007 // ideographic iteration mark, closing mark, zero
            | 0x3040..=0x30FF // Hiragana, Katakana
            | 0x3100..=0x312F // Bopomofo
            | 0x31A0..=0x31BF // Bopomofo extended
            | 0x31F0..=0x31FF // Katakana extensions
            | 0x3400..=0x4DBF // CJK extension A
            | 0x4E00..=0x9FFF // CJK
            | 0xA000..=0xA4CF // Yi
            | 0xA9E0..=0xA9FF // Myanmar extended-B
            | 0xAA60..=0xAA7F // Myanmar extended-A
            | 0xF900..=0xFAFF // CJK compatibility
            | 0xFF66..=0xFF9F // halfwidth Katakana
            | 0x1AFF0..=0x1B16F // Kana extensions and supplement
            | 0x20000..=0x3FFFF // CJK extensions
    )
}
/// `text` anywhere, as an `ILIKE` pattern.
fn like_anywhere(text: &str) -> String {
    format!(
        "%{}%",
        text.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}
/// `sort=relevance`: how well `text` matches a row, in whole points, so the activity score
/// (squashed below 1 by the caller) only breaks ties. Lexicographic: the name's match
/// (exact 4, the whole phrase as words 3, every word 2, a partial or fuzzy match 1, for a short
/// `q` only word starts: [`word_start`]) times
/// [`NAME_WEIGHT`], then a tag label (equal 2, holding the phrase as words 1) times
/// [`TAG_WEIGHT`], then the text (the phrase as words in the description 2, every word in the
/// name and description together 1). Tags count as the filters do: the row's references and its
/// aliases' (posts' own rows left out), in the author/reach scope. They are read by resource
/// through the reference index, never through `effective_reference`'s computed root, which no
/// index serves: a correlated probe of the view in a select list would scan every reference
/// once per row.
fn relevance(
    sql: &mut QueryBuilder<'_, Postgres>,
    q: &SearchQuery,
    reach: &Option<crate::nexus::ReachSet>,
    text: &str,
) {
    let exact = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    // The name's words, read once per row.
    sql.push(
        "(SELECT ((CASE WHEN lower(btrim(regexp_replace(coalesce(r.name,''),'\\s+',' ','g')))=",
    )
    .push_bind(exact)
    .push(" THEN 4 WHEN v.name @@ phraseto_tsquery('simple',")
    .push_bind(text.to_owned())
    .push(") THEN 3 WHEN v.name @@ websearch_to_tsquery('simple',")
    .push_bind(text.to_owned())
    .push(") THEN 2 WHEN ");
    match word_start(text) {
        Some(starts) => push_word_start(sql, starts),
        None => {
            sql.push("r.name ILIKE ")
                .push_bind(like_anywhere(text))
                .push(" OR r.name % ")
                .push_bind(text.to_owned());
        }
    }
    sql.push(format!(" THEN 1 ELSE 0 END)*{NAME_WEIGHT:.1}+coalesce((SELECT max(CASE WHEN f.label=ANY("))
        .push_bind(label_spellings(text))
        .push(") THEN 2 WHEN to_tsvector('simple',f.label) @@ phraseto_tsquery('simple',")
        .push_bind(text.to_owned())
        .push(") THEN 1 ELSE 0 END) FROM reference f WHERE f.resource_id IN (SELECT r.id UNION ALL SELECT a.alias_id FROM resource_alias a WHERE a.root_id=r.id) AND f.kind<>'post' AND f.label IS NOT NULL");
    scope(sql, q, reach);
    sql.push(format!(
        "),0)*{TAG_WEIGHT:.1}+(CASE WHEN to_tsvector('simple',coalesce(r.doc->>'description','')) @@ phraseto_tsquery('simple',"
    ))
    .push_bind(text.to_owned())
    .push(") THEN 2 WHEN r.search_text @@ websearch_to_tsquery('simple',")
    .push_bind(text.to_owned())
    .push(") THEN 1 ELSE 0 END)) FROM (SELECT to_tsvector('simple',coalesce(r.name,'')) AS name) v)::double precision");
}
/// The trending window: `{n}h` or `{n}d` between one hour and 90 days, default seven days.
pub(crate) fn trend_window(window: Option<&str>) -> Result<chrono::Duration, ApiError> {
    let Some(window) = window else {
        return Ok(chrono::Duration::days(7));
    };
    let bad = || ApiError::bad("window is {n}h or {n}d, from 1h to 90d");
    let (number, unit) = window.split_at(window.len().saturating_sub(1));
    if number.is_empty() || number.len() > 4 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: i64 = number.parse().map_err(|_| bad())?;
    let span = match unit {
        "h" => chrono::Duration::hours(n),
        "d" => chrono::Duration::days(n),
        _ => return Err(bad()),
    };
    if span < chrono::Duration::hours(1) || span > chrono::Duration::days(90) {
        return Err(bad());
    }
    Ok(span)
}
/// What `key` matches: resources with one of these join keys (their own, or one an exact lookup
/// named them by), or with this fact.
#[derive(Debug, PartialEq)]
pub(crate) struct KeyFilter {
    keys: Vec<String>,
    fact: Option<(&'static str, String)>,
}
pub(crate) fn key_filter(key: &str) -> Result<KeyFilter, ApiError> {
    let bad = || {
        ApiError::bad("key is mbid:{uuid}, wikidata:{Q-id}, isrc:{ISRC}, upc:{UPC, EAN or GTIN with its check digit}, gtin:{a product's barcode}, isbn:{ISBN-10 or ISBN-13}, imdb:{tt and 7–8 digits}, podcast:{GUID}, osm:{type}/{id}, doi:{DOI}, any key as a resource read lists it (keys), or another namespace a source records (docs/sources)")
    };
    let (kind, value) = key.split_once(':').ok_or_else(bad)?;
    let keys = |keys: Vec<String>| KeyFilter { keys, fact: None };
    // ISRCs and barcodes are keys and facts alike.
    let both = |name: &'static str, value: String| KeyFilter {
        keys: vec![format!("{name}:{value}")],
        fact: Some((name, value)),
    };
    match kind {
        "mbid" => {
            let id = crate::hubs::mbid(value).ok_or_else(bad)?;
            // A merged MusicBrainz page keeps the MBID it was merged from as `mbid_was`.
            Ok(KeyFilter {
                keys: [
                    crate::hubs::MbKind::Recording,
                    crate::hubs::MbKind::Release,
                    crate::hubs::MbKind::ReleaseGroup,
                    crate::hubs::MbKind::Artist,
                ]
                .into_iter()
                .filter_map(|k| crate::hubs::key::mb(k, &id))
                .collect(),
                fact: Some(("mbid_was", id)),
            })
        }
        "wikidata" => Ok(keys(vec![
            crate::hubs::key::wikidata(value).ok_or_else(bad)?
        ])),
        "isrc" => Ok(both("isrc", crate::music::isrc(value).ok_or_else(bad)?)),
        "upc" => Ok(both("upc", crate::barcode::gtin13(value).ok_or_else(bad)?)),
        // A product's barcode (`kinds::products`).
        "gtin" => Ok(both("gtin", crate::barcode::gtin13(value).ok_or_else(bad)?)),
        // The books source records each edition's ISBN-13s as facts.
        "isbn" => crate::resolver::cards::isbn(key)
            .map(|n| both("isbn", crate::resolver::cards::isbn13(&n)))
            .ok_or_else(bad),
        "imdb" if crate::also_on::media::imdb(value) => Ok(keys(vec![format!("imdb:{value}")])),
        // A MusicBrainz key as stored: `mb:{recording|release|release-group|artist}:{mbid}`.
        "mb" => {
            let (kind, id) = value.split_once(':').ok_or_else(bad)?;
            let kind = crate::hubs::MbKind::parse(kind).ok_or_else(bad)?;
            Ok(keys(vec![crate::hubs::key::mb(kind, id).ok_or_else(bad)?]))
        }
        // A show's GUID, or a podcast key as stored: `podcast:guid:…`, `podcast:apple:…`,
        // `podcast:apple_episode:…`, `podcast:item:{show GUID}:{SHA-256 of the item GUID}`,
        // `podcast:enclosure:{SHA-256 of the URL}` (or the enclosure's URL itself).
        "podcast" => {
            let guid = crate::also_on::media::guid;
            let key = match value.split_once(':') {
                Some(("guid", g)) => guid(g).map(|g| format!("podcast:guid:{g}")),
                Some((kind @ ("apple" | "apple_episode"), id)) if crate::lookups::digits(id) => {
                    Some(format!("podcast:{kind}:{id}"))
                }
                Some(("enclosure", file)) => {
                    let hash = file.to_ascii_lowercase();
                    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                        Some(format!("podcast:enclosure:{hash}"))
                    } else {
                        crate::also_on::media::enclosure_key(file)
                    }
                }
                Some(("item", rest)) => rest.split_once(':').and_then(|(g, hash)| {
                    let hash = hash.to_ascii_lowercase();
                    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
                        .then(|| Some(format!("podcast:item:{}:{hash}", guid(g)?)))
                        .flatten()
                }),
                _ => guid(value).map(|g| format!("podcast:guid:{g}")),
            };
            key.map(|k| keys(vec![k])).ok_or_else(bad)
        }
        // The lookups' keys: places, papers, packages, repositories, games, documents.
        _ => crate::lookups::search_key(kind, value)
            .map(|key| keys(vec![key]))
            .ok_or_else(bad),
    }
}
/// How much the `boost` set weighs against a card's own score: `BOOST_WEIGHT · log1p(n)` for
/// `n` authors in the set who referenced it (with the same 30-day half-life), so one recent
/// reference from the set counts as much as about three taggers do in the score.
pub const BOOST_WEIGHT: f64 = 2.0;
/// After this many 30-day halvings (about five years) a boost counts nothing.
pub const BOOST_HALF_LIVES: i64 = 60;
/// The columns of `search_unit` (and `resource_unit`), in order.
pub const UNIT_COLUMNS: &str = "id,uri,host,type,type_path,name,category,point,start_at,end_at,venue_id,doc,search_text,resolver,resolved_at,refresh_at,status,osm_type,osm_id,unit_id,occurrence,event,event_author";
/// A related row's rank from `{counts}.n` and `.at`: its shared taggers, then the newest of
/// their references, in whole seconds after the window's start `since`. Every counted reference
/// is in the window (at most 3,650 days, under 1e10 seconds), so the second term never reaches
/// the first, whatever the dates; exact in a double up to 900,000 taggers.
fn related_rank(sql: &mut QueryBuilder<'_, Postgres>, counts: &str, since: DateTime<Utc>) {
    sql.push(format!(
        "({counts}.n*1e10+floor(extract(epoch FROM {counts}.at-"
    ))
    .push_bind(since)
    .push(")))::double precision");
}

/// A related row's `because` from what its rank counted: `shared`, the count (`n`), and of its
/// taggers (`authors`, space-separated keys, repeats allowed) the [`BECAUSE_TAGGERS`] who tagged
/// the resource most recently, read from the bounded sample (`related_taggers`), never from the
/// reference table. Built in the final projection, for the page's rows alone; the rows before it
/// carry `n` and `authors` as `because_n` and `because_authors`, which the item drops.
fn related_because(sql: &mut QueryBuilder<'_, Postgres>, item: &str, n: &str, authors: &str) {
    sql.push(format!(
        "(({item})-'because_n'-'because_authors')||jsonb_build_object('because',jsonb_build_object('shared',{n},'taggers',to_jsonb(ARRAY(SELECT t.author FROM related_taggers t WHERE t.author IN (SELECT unnest(string_to_array({authors},' '))) ORDER BY t.at DESC,t.author LIMIT {BECAUSE_TAGGERS}))))"
    ));
}
/// The version of a search page's shape, in its ETag: a page cached before a field was added
/// (`because`, then every card's `credits`, then `embed`, then a show's `episodes`) or before its
/// rows' order changed (`q` ranked by relevance by default) or their set (a short `q` matches a
/// name at word starts only) is never answered 304. Move it when a search row gains or loses a
/// field, or the same query finds other rows or the same rows in another order.
pub const SEARCH_SHAPE: u32 = 6;
/// The most taggers a related row's `because` names.
pub const BECAUSE_TAGGERS: usize = 3;

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
/// What a search page's ETag hashes besides its query, read in the page's own snapshot: the
/// Nexus feed cursors and the search revision (`search_write`), which every write that can change
/// a page moves, the hint passes' pairs included (`store::note_search_write`). A page that has not
/// seen a change never carries the revision that includes it.
pub const REVISION: &str = "jsonb_build_array((SELECT jsonb_agg(jsonb_build_array(cursor,epoch) ORDER BY nexus) FROM feed_cursor),(SELECT revision FROM search_revision WHERE id=1))";

#[utoipa::path(get,tag="Search",path="/v1/search",params(SearchQuery),responses((status=200,body=SearchResult),(status=304,description="ETag unchanged"),(status=400,description="Invalid or unavailable filter"),(status=501,description="wot_2 asked of an instance whose Nexus serves no reach set"),(status=502,description="Nexus reach unavailable")))]
pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let tags = validate_search(&q)?;
    let key = q.key.as_deref().map(key_filter).transpose()?;
    if q.collapse.is_some() && !state.config.groups.enabled {
        return Err(ApiError::bad("groups are disabled"));
    }
    if q.collapse.is_some() && q.cluster.is_some() {
        return Err(ApiError::bad(
            "collapse=group is a list view; a map with cluster shows every member",
        ));
    }
    let cursor = q
        .cursor
        .as_deref()
        .map(|s| PageCursor::decode(s, &state.cursor_mac))
        .transpose()?;
    // A later page resolves relative times at the cursor's pinned instant, as page one did.
    let at = cursor.as_ref().map_or_else(|| state.now(), |c| c.at);
    let spatial = crate::spatial::Filters::parse_at(&q, &state.config.search, at)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    if !state.config.nexus_reach_set
        && [&q.reach, &q.boost]
            .iter()
            .any(|set| set.as_deref() == Some("wot_2"))
    {
        return Err(ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "wot_2 is not supported: the Nexus this instance reads serves no reach set".into(),
        ));
    }
    // The `reach` filter's set and the `boost` set, each read from Nexus (cached per viewer).
    let mut sets = vec![];
    for kind in [&q.reach, &q.boost] {
        sets.push(match (&q.viewer, kind) {
            (Some(viewer), Some(kind)) => {
                Some(state.nexus.reach(viewer, kind).await.map_err(|e| {
                    tracing::warn!(error=%crate::resolver::log_error(&e),"reach unavailable");
                    ApiError(StatusCode::BAD_GATEWAY, "Nexus reach unavailable".into())
                })?)
            }
            _ => None,
        });
    }
    let boost = sets.pop().flatten();
    let reach = sets.pop().flatten();
    let mut fingerprint = q.clone();
    fingerprint.cursor = None;
    fingerprint.limit = None;
    // Facets count the filtered set and never change the rows, so a later page may ask for
    // other facets or none.
    fingerprint.facet = None;
    // The ranking itself, so a cursor never pages another order (`q` alone ranks by relevance).
    fingerprint.sort = effective_sort(&q).map(str::to_owned);
    let revision_of = |set: &Option<crate::nexus::ReachSet>| {
        set.as_ref().map(|r| {
            let mut authors = r.authors.clone();
            authors.sort();
            authors.dedup();
            (
                blake3::hash(authors.join("\n").as_bytes())
                    .to_hex()
                    .to_string(),
                r.truncated,
            )
        })
    };
    let reach_revision = (revision_of(&reach), revision_of(&boost));
    let filter = blake3::hash(&serde_json::to_vec(&(&fingerprint, &reach_revision))?)
        .to_hex()
        .to_string();
    if let Some(c) = &cursor {
        if c.version != 2
            || c.filter != filter
            || !c.score.is_finite()
            || !model::valid_id(&c.id)
            || c.at > state.now() + chrono::Duration::seconds(5)
        {
            return Err(ApiError::bad("cursor does not match this search"));
        }
    }
    let sort = effective_sort(&q);
    let trending = sort == Some("trending");
    // `related`: the resource's root (an alias reads as it).
    let related: Option<String> = match &q.related {
        Some(id) => Some(
            sqlx::query_scalar(
                "SELECT coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)",
            )
            .bind(id)
            .fetch_one(&state.pool)
            .await?,
        ),
        None => None,
    };
    // The start of a related search's window.
    let since = match &related {
        Some(_) => Some(
            at - if q.window.is_some() {
                trend_window(q.window.as_deref())?
            } else {
                chrono::Duration::days(state.config.search.related_window_days.into())
            },
        ),
        None => None,
    };
    let mut sql = QueryBuilder::<Postgres>::new("WITH ");
    if let (Some(seed), Some(since)) = (&related, since) {
        // The resource, or its whole group; of those not opted out and their aliases, the links
        // with the newest tags in the window (one probe each), and on each a sample of its most
        // recent tags there, both read through the tags-only index; of the sample's authors in
        // reach the ones with the newest tag; and the most recent references each of them made
        // elsewhere in the window, once per resource.
        let search = &state.config.search;
        sql.push("related_seed AS (SELECT ")
            .push_bind(seed.clone())
            .push("::text AS resource_id UNION SELECT u.resource_id FROM group_unit u WHERE u.group_id=(SELECT group_id FROM group_unit WHERE unit_id=")
            .push_bind(seed.clone())
            .push(" AND group_id IS NOT NULL)), related_sources AS (SELECT s.resource_id FROM related_seed s JOIN resource d ON d.id=s.resource_id WHERE coalesce(d.doc->'attributes'->>'opted_out','')<>'true'), related_ids AS (SELECT i.resource_id FROM (SELECT resource_id FROM related_sources UNION SELECT alias_id FROM resource_alias WHERE root_id IN (SELECT resource_id FROM related_sources)) i CROSS JOIN LATERAL (SELECT f.created_at FROM reference f WHERE f.resource_id=i.resource_id AND f.kind='tag' AND f.created_at>=")
            .push_bind(since)
            .push(" AND f.created_at<=")
            .push_bind(at)
            .push(" ORDER BY f.created_at DESC LIMIT 1) t ORDER BY t.created_at DESC,i.resource_id LIMIT ")
            .push_bind(search.related_seed_links as i64)
            .push("), related_taggers AS (SELECT f.author,max(f.created_at) AS at FROM related_ids i CROSS JOIN LATERAL (SELECT f.author,f.created_at FROM reference f WHERE f.resource_id=i.resource_id AND f.kind='tag' AND f.created_at>=")
            .push_bind(since)
            .push(" AND f.created_at<=")
            .push_bind(at)
            .push(" ORDER BY f.created_at DESC LIMIT ")
            .push_bind(search.related_tag_sample as i64)
            .push(") f WHERE true");
        scope(&mut sql, &q, &reach);
        sql.push(" GROUP BY f.author ORDER BY at DESC,f.author LIMIT ")
            .push_bind(search.related_taggers as i64)
            .push("), related_refs AS (SELECT a.root AS resource_id,t.author,max(x.created_at) AS at FROM related_taggers t CROSS JOIN LATERAL (SELECT f.resource_id,f.created_at FROM reference f WHERE f.author=t.author AND f.kind<>'post' AND f.created_at>=")
            .push_bind(since)
            .push(" AND f.created_at<=")
            .push_bind(at)
            .push(" ORDER BY f.created_at DESC LIMIT ")
            .push_bind(search.related_refs_per_tagger as i64)
            .push(") x CROSS JOIN LATERAL (SELECT coalesce((SELECT root_id FROM resource_alias WHERE alias_id=x.resource_id),x.resource_id) AS root) a WHERE a.root NOT IN (SELECT resource_id FROM related_seed) GROUP BY 1,t.author), related AS (SELECT resource_id,count(*) AS n,max(at) AS at");
        // Its taggers: for its group's count (`eligible`) and the row's `because`.
        sql.push(",string_agg(author,' ') AS authors FROM related_refs GROUP BY resource_id");
        if q.collapse.is_none() {
            // A row needs a crowd, so it never mirrors one person's references.
            sql.push(" HAVING count(*)>=")
                .push_bind(search.related_min_shared as i64);
        }
        sql.push("), ");
    }
    if trending {
        // References made inside the window, counted once for all rows (in the same reach),
        // relative to the instant the first page pinned.
        let since = at - trend_window(q.window.as_deref())?;
        // With `boost`, a reference by an author in the boost set counts twice.
        sql.push("trend AS (SELECT f.resource_id,count(*)");
        if let Some(boost) = &boost {
            sql.push("+count(*) FILTER (WHERE f.author=ANY(")
                .push_bind(boost.authors.clone())
                .push("))");
        }
        sql.push(" AS n FROM effective_reference f WHERE f.created_at>=")
            .push_bind(since)
            .push(" AND f.created_at<=")
            .push_bind(at);
        scope(&mut sql, &q, &reach);
        sql.push(" GROUP BY f.resource_id), ");
        if q.collapse.is_some() {
            // A collapsed row stands for its group: the window's references of every member
            // resource, each counted once (a feed's instances can share a group).
            sql.push("group_trend AS (SELECT m.group_id,sum(t.n) AS n FROM (SELECT DISTINCT group_id,resource_id FROM group_unit WHERE group_id IS NOT NULL) m JOIN trend t USING(resource_id) GROUP BY m.group_id), ");
        }
    }
    if let (Some(boost), false) = (&boost, trending) {
        // The authors in the boost set who referenced each resource, and the newest of their
        // references: read once by author (the set is bounded), never per candidate row.
        sql.push("boost_refs AS (SELECT f.resource_id,count(DISTINCT f.author) AS n,max(f.created_at) AS at FROM effective_reference f WHERE f.author=ANY(")
            .push_bind(boost.authors.clone())
            .push(") GROUP BY f.resource_id), ");
    }
    // With `collapse=group`, a related group counts the shared taggers of its members that pass
    // the filters: those rows first (`related_pass`, each with its taggers and group), ranked
    // after them.
    let related_groups = related.is_some() && q.collapse.is_some();
    sql.push(if related_groups {
        "related_pass AS (SELECT r.*,rel.at AS rel_at,rel.authors AS rel_authors,gu.group_id AS rel_group"
    } else if q.collapse.is_some() {
        "eligible AS (SELECT r.*, "
    } else {
        "filtered AS (SELECT r.*, "
    });
    if related_groups {
        // Ranked once the rows that pass are known (`eligible` below).
    } else if let Some(since) = since {
        related_rank(&mut sql, "rel", since);
    } else if trending && q.collapse.is_some() {
        sql.push("coalesce((SELECT g.n FROM group_unit u JOIN group_trend g USING(group_id) WHERE u.unit_id=r.unit_id),t.n)::double precision");
    } else if trending {
        sql.push("t.n::double precision");
    } else if sort == Some("distance") {
        sql.push("-");
        spatial.distance(&mut sql);
    } else if sort == Some("start") {
        sql.push("-extract(epoch FROM r.start_at)::double precision");
    } else if sort == Some("recent") {
        sql.push("coalesce(extract(epoch FROM s.newest_at),0)::double precision");
    } else {
        let text = query_text(&q).filter(|_| sort == Some("relevance"));
        if let Some(text) = text {
            // The match's whole points, plus the activity score (with any boost) mapped into
            // [0, 1) by a/(1+a), which keeps its order: it only breaks ties between matches
            // that are equally good.
            relevance(&mut sql, &q, &reach, text);
            sql.push("+1.0-1.0/(1.0+greatest(0.0,");
        }
        sql.push("(coalesce(s.score * exp(-ln(2.0)*greatest(0,extract(epoch FROM (")
            .push_bind(at)
            .push("::timestamptz-s.updated_at)))/2592000.0),0)");
        if boost.is_some() {
            // `boost`: BOOST_WEIGHT · log1p(authors in the set who referenced it), halved every
            // 30 days since the newest of their references. Older than BOOST_HALF_LIVES halvings
            // it is nothing: the decay would underflow a double (a reference dated 1900).
            sql.push("+coalesce(CASE WHEN b.at<")
                .push_bind(at)
                .push(format!(
                    "::timestamptz-interval '{} days' THEN 0 ELSE {BOOST_WEIGHT}*ln(1+b.n)*exp(-ln(2.0)*greatest(0,extract(epoch FROM (",
                    BOOST_HALF_LIVES * 30
                ))
                .push_bind(at)
                .push("::timestamptz-b.at)))/2592000.0) END,0)");
        }
        sql.push(")::double precision");
        if text.is_some() {
            sql.push("))");
        }
    }
    if !related_groups {
        sql.push(" AS rank");
    }
    if related.is_some() && !related_groups {
        // For `because`, on the page's rows only (`related_because`).
        sql.push(",rel.n AS because_n,rel.authors AS because_authors");
    }
    if related.is_some() {
        // Each related resource once, as its own card (never once per occurrence or event
        // reference), looked up one by one: the set is bounded, and a join would read every card.
        sql.push(" FROM related rel CROSS JOIN LATERAL (SELECT * FROM resource_unit u WHERE u.id=rel.resource_id OFFSET 0) r");
    } else {
        sql.push(" FROM search_unit r");
    }
    sql.push(" LEFT JOIN resource_score s ON s.resource_id=r.id");
    if boost.is_some() && !trending {
        sql.push(" LEFT JOIN boost_refs b ON b.resource_id=r.id");
    }
    if related_groups {
        sql.push(" LEFT JOIN group_unit gu ON gu.unit_id=r.unit_id AND gu.group_id IS NOT NULL");
    }
    if trending {
        sql.push(" JOIN trend t ON t.resource_id=r.id");
    }
    sql.push(" WHERE true");
    if related.is_some() {
        // A post whose author opted out of indexing is never suggested, as it is never found by
        // its text, nor counts toward its group (`eligible` reads the rows that pass).
        sql.push(" AND coalesce(r.doc->'attributes'->>'opted_out','') <> 'true'");
    }
    spatial.apply(&mut sql);
    if let Some(venue) = &q.venue {
        sql.push(" AND r.venue_id=").push_bind(venue.clone());
    }
    if let Some(collection) = q
        .in_collection
        .as_deref()
        .and_then(crate::collections::parse)
    {
        // The collection's references, read by author and path (the key's prefix); an alias's
        // count for its root.
        sql.push(" AND r.id IN (SELECT coalesce(a.root_id,f.resource_id) FROM reference f LEFT JOIN resource_alias a ON a.alias_id=f.resource_id WHERE ");
        crate::collections::push_items(&mut sql, collection);
        sql.push(")");
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
    if sort == Some("start") {
        sql.push(" AND r.start_at IS NOT NULL");
    }
    if let Some(types) = &q.type_name {
        // Any of the types, each with its subtypes: a path that holds one of them.
        sql.push(" AND r.type_path && ")
            .push_bind(search_types(types)?);
    }
    if let Some(remote) = q.remote {
        sql.push(" AND r.type_path @> ARRAY['JobPosting'] AND (coalesce(r.doc->'attributes'->>'remote','')='true')=")
            .push_bind(remote);
    }
    for term in ingredient_terms(&q)? {
        // Whole words of the recipe's ingredient names (letters, digits, spaces, hyphens only).
        sql.push(" AND r.type_path @> ARRAY['Recipe'] AND coalesce(r.doc->'attributes'->>'ingredients','') ~* ")
            .push_bind(format!("\\m{term}\\M"));
    }
    if q.closed != Some(true) {
        // A job posting past its end is closed: listed only on request.
        sql.push(" AND NOT coalesce(r.type_path @> ARRAY['JobPosting'] AND r.end_at<=")
            .push_bind(at)
            .push(",false)");
    }
    for (column, value) in [("r.category", &q.category), ("r.host", &q.host)] {
        if let Some(value) = value {
            sql.push(" AND ")
                .push(column)
                .push("=")
                .push_bind(value.clone());
        }
    }
    // Exact identifiers: a source's live keys or facts on the resource, or a key an exact lookup
    // named it by (aliases read as roots); never a switched-off source's.
    if let Some(KeyFilter { keys, fact }) = &key {
        sql.push(" AND r.id IN (SELECT resource_id FROM resource_key WHERE key=ANY(")
            .push_bind(keys.clone())
            .push(") AND expires_at>now() AND source NOT IN (SELECT source FROM also_on_source_off) UNION SELECT link_id FROM derived_link WHERE key=ANY(")
            .push_bind(keys.clone())
            .push(") AND expires_at>now() AND source NOT IN (SELECT source FROM also_on_source_off)");
        if let Some((name, value)) = fact {
            sql.push(" UNION SELECT resource_id FROM resource_fact WHERE name=")
                .push_bind(*name)
                .push(" AND value=")
                .push_bind(value.clone())
                .push(" AND expires_at>now() AND source NOT IN (SELECT source FROM also_on_source_off)");
        }
        sql.push(")");
    }
    if let Some(text) = q.q.as_ref().filter(|s| !s.is_empty()) {
        sql.push(" AND (r.search_text @@ websearch_to_tsquery('simple',")
            .push_bind(text.clone())
            .push(") OR ");
        match word_start(text) {
            Some(starts) => push_word_start(&mut sql, starts),
            None => {
                sql.push("r.name ILIKE ")
                    .push_bind(like_anywhere(text))
                    .push(" OR r.name % ")
                    .push_bind(text.clone());
            }
        }
        // A post whose author opted out of search indexing is never found by its text.
        sql.push(") AND coalesce(r.doc->'attributes'->>'opted_out','') <> 'true'");
    }
    if related.is_none() {
        // (A related row is referenced by a sampled tagger, who is in scope.)
        sql.push(" AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id");
        scope(&mut sql, &q, &reach);
        sql.push(")");
    }
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
    if let (true, Some(since)) = (related_groups, since) {
        // Each row's group (or the row alone) over the rows that pass: its members' taggers
        // together, counted once, and the newest of their references. A window over the rows,
        // never a join of two of these sets, whose sizes the planner cannot know.
        sql.push("), eligible AS (SELECT ")
            .push(UNIT_COLUMNS)
            .push(",");
        related_rank(&mut sql, "g", since);
        // For `because`, on the page's rows only (`related_because`).
        sql.push(" AS rank,g.n AS because_n,p.group_authors AS because_authors FROM (SELECT p.*,max(p.rel_at) OVER w AS group_at,string_agg(p.rel_authors,' ') OVER w AS group_authors FROM related_pass p WINDOW w AS (PARTITION BY coalesce(p.rel_group,p.id))) p CROSS JOIN LATERAL (SELECT count(DISTINCT a) AS n,p.group_at AS at FROM unnest(string_to_array(p.group_authors,' ')) a) g WHERE g.n>=")
            .push_bind(state.config.search.related_min_shared as i64);
    }
    if q.collapse.is_some() {
        // One row per group: the member with the most references (ties by ID), as itself.
        // The references are counted once, for the grouped rows only, through the base table's
        // index (their IDs and their aliases'), at most `collapse_refs_per_member` each: a popular
        // member costs the cap, not its history.
        let cap = state.config.search.collapse_refs_per_member as i64;
        sql.push("), grouped AS (SELECT DISTINCT x.id FROM eligible x JOIN group_unit g ON g.unit_id=x.unit_id AND g.group_id IS NOT NULL), grouped_ids AS (SELECT id AS root,id AS base FROM grouped UNION ALL SELECT a.root_id,a.alias_id FROM resource_alias a WHERE a.root_id IN (SELECT id FROM grouped)), grouped_refs AS (SELECT gi.root AS resource_id,least(sum(c.n),")
            .push_bind(cap)
            .push(") AS refs FROM grouped_ids gi CROSS JOIN LATERAL (SELECT count(*) AS n FROM (SELECT 1 FROM reference f WHERE f.resource_id=gi.base AND f.kind<>'post' LIMIT ")
            .push_bind(cap)
            .push(") x) c GROUP BY gi.root), filtered AS (SELECT DISTINCT ON(coalesce(u.group_id,e.unit_id)) e.* FROM eligible e LEFT JOIN group_unit u ON u.unit_id=e.unit_id AND u.group_id IS NOT NULL LEFT JOIN grouped_refs c ON c.resource_id=e.id ORDER BY coalesce(u.group_id,e.unit_id),coalesce(c.refs,0) DESC,e.id,e.unit_id");
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
        // A point's card is its `item`, built once: `||` replaces a key, it never merges two items.
        sql.push(" || CASE WHEN p.kind='point' THEN jsonb_build_object('item',");
        if related.is_some() {
            related_because(
                &mut sql,
                "p.item",
                "(p.item->>'because_n')::bigint",
                "p.item->>'because_authors'",
            );
        } else {
            sql.push("p.item");
        }
        if state.config.groups.enabled {
            sql.push("||jsonb_build_object('group',group_hint(p.unit_id),'also_on',also_on(p.unit_id,p.unit_id,8,true))");
        } else {
            sql.push("||jsonb_build_object('also_on',also_on(p.unit_id,p.unit_id,8,false))");
        }
        if let Some((events, recordings)) = hint_flags(&state.config) {
            sql.push(format!(
                "||jsonb_build_object('maybe_same',{})",
                maybe_same_sql(
                    "p.unit_id",
                    "ARRAY(SELECT jsonb_array_elements_text(p.item->'type_path'))",
                    3,
                    events,
                    recordings
                )
            ));
        }
        sql.push(") ELSE '{}'::jsonb END");
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
            .push(") SELECT coalesce((SELECT jsonb_agg(");
        if related.is_some() {
            related_because(&mut sql, "to_jsonb(p)", "p.because_n", "p.because_authors");
        } else {
            sql.push("to_jsonb(p)");
        }
        if state.config.groups.enabled {
            sql.push(" || jsonb_build_object('group',group_hint(p.unit_id),'also_on',also_on(p.unit_id,p.unit_id,8,true))");
        } else {
            sql.push(" || jsonb_build_object('also_on',also_on(p.unit_id,p.unit_id,8,false))");
        }
        if let Some((events, recordings)) = hint_flags(&state.config) {
            sql.push(format!(
                " || jsonb_build_object('maybe_same',{})",
                maybe_same_sql("p.unit_id", "p.type_path", 3, events, recordings)
            ));
        }
        sql.push(" ORDER BY rank DESC,unit_id DESC) FROM page p),'[]') AS items, ");
    }
    let asked = facets_asked(q.facet.as_deref())?;
    // Facets count every matching link, collapsed or not.
    let counted = if q.collapse.is_some() {
        "eligible"
    } else {
        "filtered"
    };
    sql.push("jsonb_build_object('type',");
    if asked.types {
        sql.push("coalesce((SELECT jsonb_agg(jsonb_build_object('type',type,'count',count) ORDER BY count DESC,type) FROM (SELECT r.type,count(DISTINCT r.unit_id) AS count FROM ")
            .push(counted)
            .push(" r WHERE r.type IS NOT NULL GROUP BY r.type) t),'[]')");
    } else {
        sql.push("NULL");
    }
    sql.push(",'tags',");
    if asked.tags {
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,label) FROM (SELECT f.label,count(DISTINCT r.unit_id) AS count FROM ")
            .push(counted)
            .push(" r JOIN effective_reference f ON r.id=f.resource_id WHERE f.label IS NOT NULL");

        scope(&mut sql, &q, &reach);
        sql.push(" GROUP BY f.label) t),'[]')");
    } else {
        sql.push("NULL");
    }
    sql.push(",'host',");
    if asked.hosts {
        // One pass over the set: per host its count, a few of its links and the site name its
        // documents give most often (an event reference's stand-in names only the host), for
        // the service's name (`HostFacet::site`). A host that `pubky://` objects use is a user's
        // key, not a service; a value over the filter's 256 bytes could not be filtered by.
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,host) FROM (SELECT r.host,count(DISTINCT r.unit_id) AS count,(array_agg(DISTINCT r.uri ORDER BY r.uri))[1:")
            .push_bind(FACET_SITE_SAMPLE)
            .push("] AS uris,mode() WITHIN GROUP (ORDER BY nullif(nullif(r.doc->>'site',''),r.host)) AS site FROM ")
            .push(counted)
            .push(" r WHERE r.host IS NOT NULL AND r.host<>'' AND octet_length(r.host)<=256 GROUP BY r.host HAVING bool_and(r.uri NOT LIKE 'pubky://%') ORDER BY count DESC,r.host LIMIT ")
            .push_bind(FACET_LIST_CAP)
            .push(") t),'[]')");
    } else {
        sql.push("NULL");
    }
    sql.push(",'category',");
    if asked.categories {
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,category) FROM (SELECT r.category,count(DISTINCT r.unit_id) AS count FROM ")
            .push(counted)
            .push(" r WHERE r.category IS NOT NULL AND octet_length(r.category)<=256 GROUP BY r.category ORDER BY count DESC,r.category LIMIT ")
            .push_bind(FACET_LIST_CAP)
            .push(") t),'[]')");
    } else {
        sql.push("NULL");
    }
    sql.push(format!(") AS facets, {REVISION} AS revision"));
    // A related page is planned for its own parameters every time: a cached generic plan
    // guesses its sets hold a row or two and nests loops over thousands.
    let row = sql
        .build()
        .persistent(related.is_none())
        .fetch_one(&state.pool)
        .await?;
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
    let facets = if asked.any() {
        let values: Value = row.try_get("facets")?;
        let list = |key: &str| values[key].as_array().cloned();
        Some(Facets {
            tags: list("tags").map(|v| {
                v.into_iter()
                    .map(|v| TagFacet {
                        label: v["label"].as_str().unwrap_or_default().into(),
                        count: v["count"].as_i64().unwrap_or(0),
                    })
                    .collect()
            }),
            types: list("type").map(|v| {
                v.into_iter()
                    .map(|v| {
                        let type_name = v["type"].as_str().unwrap_or_default();
                        TypeFacet {
                            type_path: model::type_path(type_name),
                            type_name: type_name.into(),
                            count: v["count"].as_i64().unwrap_or(0),
                        }
                    })
                    .collect()
            }),
            hosts: list("host").map(|v| {
                v.into_iter()
                    .map(|v| {
                        let host = v["host"].as_str().unwrap_or_default();
                        let uris = v["uris"].as_array().into_iter().flatten();
                        HostFacet {
                            site: uris
                                .filter_map(|uri| service_site(uri.as_str()?))
                                .next()
                                .or_else(|| present(v["site"].as_str()))
                                .unwrap_or_else(|| host.into()),
                            host: host.into(),
                            count: v["count"].as_i64().unwrap_or(0),
                        }
                    })
                    .collect()
            }),
            categories: list("category").map(|v| {
                v.into_iter()
                    .map(|v| CategoryFacet {
                        category: v["category"].as_str().unwrap_or_default().into(),
                        count: v["count"].as_i64().unwrap_or(0),
                    })
                    .collect()
            }),
        })
    } else {
        None
    };
    let revision: Value = row.try_get("revision")?;
    let etag = format!(
        "\"{}\"",
        // Relative times (`from=now`) resolve per request: hash the resolved window too.
        blake3::hash(&serde_json::to_vec(&(
            q.clone(),
            revision,
            reach_revision,
            spatial.from,
            spatial.to,
            SEARCH_SHAPE
        ))?)
        .to_hex()
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
                points.push(card(item)?);
            }
        }
        with_refs(&state.pool, points.iter_mut()).await?;
        Json(ClusterResponse {
            resolution: u8::from(spatial.resolution.expect("cluster mode")),
            clusters,
            points,
            truncated: has_more,
            total: row.try_get("total")?,
            reach_truncated: [&reach, &boost]
                .iter()
                .any(|set| set.as_ref().is_some_and(|r| r.truncated)),
            facets,
        })
        .into_response()
    } else {
        let mut cards = items.iter().map(card).collect::<Result<Vec<_>, _>>()?;
        with_refs(&state.pool, cards.iter_mut()).await?;
        Json(SearchResponse {
            items: cards,
            cursor: next,
            reach_truncated: [&reach, &boost]
                .iter()
                .any(|set| set.as_ref().is_some_and(|r| r.truncated)),
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
    config: &Config,
) -> Result<Option<ResourceDetail>, ApiError> {
    let value: Option<Value> =
        sqlx::query_scalar(&[
            "SELECT to_jsonb(r)||jsonb_build_object('group',CASE WHEN $2 THEN group_hint(r.id) END,'maybe_same',",
            &maybe_same_sql("r.id", "r.type_path", 10, "$3", "$4"),
            ",'also_on',also_on(r.id,r.id,256,$2),'aliases',coalesce((SELECT jsonb_agg(uri ORDER BY uri) FROM (SELECT x.uri FROM resource_alias a JOIN resource x ON x.id=a.alias_id WHERE a.root_id=r.id ORDER BY x.uri LIMIT 50) s),'[]'),'keys',coalesce((SELECT jsonb_agg(key ORDER BY key COLLATE \"C\") FROM resource_key WHERE resource_id=r.id AND expires_at>now() AND source NOT IN (SELECT source FROM also_on_source_off)),'[]'),'facts',(SELECT jsonb_object_agg(name,vals) FROM (SELECT name,string_agg(value,'; ' ORDER BY value) AS vals FROM resource_fact WHERE resource_id=r.id AND expires_at>now() AND source NOT IN (SELECT source FROM also_on_source_off) GROUP BY name) f)) FROM resource_document r WHERE r.id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)",
        ]
        .concat())
            .bind(id)
            .bind(config.groups.enabled)
            .bind(same_event(config))
            .bind(same_recording(config))
            .fetch_optional(pool)
            .await?;
    let Some(mut detail) = value.as_ref().map(detail).transpose()? else {
        return Ok(None);
    };
    with_refs(pool, std::iter::once(&mut detail.card)).await?;
    Ok(Some(detail))
}
/// A read's JSON, cacheable for 30 seconds like a search page, with an ETag of its content; 304
/// when `If-None-Match` names it.
fn tagged<T: Serialize>(headers: &HeaderMap, value: &T) -> Result<Response, ApiError> {
    let body = serde_json::to_vec(value)?;
    let etag = format!("\"{}\"", &blake3::hash(&body).to_hex()[..32]);
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h.split(',')
                .any(|v| v.trim().trim_start_matches("W/") == etag || v.trim() == "*")
        });
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [(header::CONTENT_TYPE, "application/json")],
            axum::body::Body::from(body),
        )
            .into_response()
    };
    let headers = response.headers_mut();
    headers.insert(header::ETAG, etag.parse().expect("hash header"));
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("public, max-age=30"),
    );
    Ok(response)
}
#[utoipa::path(get,tag="Previews",path="/v1/resources/{id}",params(("id"=String,Path,description="32 lowercase hex resource ID")),responses((status=200,body=ResourceDetail),(status=304,description="ETag unchanged"),(status=400,description="Invalid ID"),(status=404,description="Unknown resource")))]
pub async fn resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if !model::valid_id(&id) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    let detail = read(&state.pool, &id, &state.config)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "resource not found".into()))?;
    tagged(&headers, &detail)
}
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ByUri {
    /// The link as referenced: normalised like every URI, and an alias answers with its root.
    pub uri: String,
}
/// One link's card by its URI, as `POST /v1/resources/lookup` answers for it: a link Pinakes
/// does not know is a 404 and is not fetched (`POST /v1/resolve` resolves it).
#[utoipa::path(get,tag="Previews",path="/v1/resources/by-uri",params(ByUri),responses((status=200,body=ResourceDetail),(status=304,description="ETag unchanged"),(status=400,description="Invalid URI"),(status=404,description="Unknown link; nothing is fetched")))]
pub async fn by_uri(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ByUri>,
) -> Result<Response, ApiError> {
    let id = model::identity(&q.uri)
        .map_err(|e| ApiError::bad(e.to_string()))?
        .0;
    let detail = read(&state.pool, &id, &state.config)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "link not known".into()))?;
    tagged(&headers, &detail)
}
#[utoipa::path(post,tag="Previews",path="/v1/resources/lookup",operation_id="lookup_resources",request_body=LookupRequest,responses((status=200,body=Vec<Card>),(status=400,description="Exactly one list of 1–500 IDs or URIs")))]
pub async fn lookup(
    State(state): State<AppState>,
    Json(body): Json<LookupRequest>,
) -> Result<Json<Vec<Card>>, ApiError> {
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
    let roots: Vec<String> = sqlx::query_scalar("SELECT coalesce(a.root_id,w.id) FROM unnest($1::text[]) w(id) LEFT JOIN resource_alias a ON a.alias_id=w.id").bind(&ids).fetch_all(&state.pool).await?;
    store::expedite(&state.pool, &roots).await?;
    state.wake.notify_waiters();
    let rows:Vec<Value>=sqlx::query_scalar(&[
            "SELECT to_jsonb(r)||jsonb_build_object('group',CASE WHEN $2 THEN group_hint(r.id) END,'maybe_same',",
            &maybe_same_sql("r.id", "r.type_path", 3, "$3", "$4"),
            ",'also_on',also_on(r.id,r.id,8,$2)) FROM unnest($1::text[]) WITH ORDINALITY AS wanted(id,n) LEFT JOIN resource_alias a ON a.alias_id=wanted.id JOIN resource_document r ON r.id=coalesce(a.root_id,wanted.id) ORDER BY wanted.n",
        ]
        .concat()).bind(ids).bind(state.config.groups.enabled).bind(same_event(&state.config)).bind(same_recording(&state.config)).fetch_all(&state.pool).await?;
    let mut cards = rows.iter().map(card).collect::<Result<Vec<_>, _>>()?;
    with_refs(&state.pool, cards.iter_mut()).await?;
    Ok(Json(cards))
}
#[utoipa::path(post,tag="Previews",path="/v1/resolve",request_body=ResolveRequest,responses((status=200,body=ResourceDetail),(status=202,body=ResourceDetail,description="Queued; poll resource by ID",headers(("Retry-After"=u64,description="Seconds until the next attempt, when it is put off (a missing answer waiting for the fetch that confirms it); absent while the job is queued or running"))),(status=400,description="Unsupported URI, disabled resolver, or a link that carries a credential"),(status=429,description="Preview rate limit"),(status=503,description="The instance's new previews for this hour are taken; nothing was stored",headers(("Retry-After"=u64,description="Seconds until a new preview can be taken")))))]
pub async fn resolve(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<ResolveRequest>,
) -> Result<Response, ApiError> {
    let (id, uri, _) = model::identity(&body.uri).map_err(|e| ApiError::bad(e.to_string()))?;
    // A link that carries a credential is never previewed, stored or logged; a card someone's
    // post made is still read by its URI.
    if crate::capability::is_capability(&body.uri) || crate::capability::is_capability(&uri) {
        tracing::info!(
            host = %crate::resolver::log_host(&uri),
            "a link that carries a credential is not previewed"
        );
        return Err(ApiError::bad(
            "this link carries a credential; it is not previewed",
        ));
    }
    // An alias is never fetched: the root's route decides.
    let target = crate::alias::root(&uri).map_or_else(|| uri.clone(), |a| a.root);
    crate::resolver::admit(&target, &state.config).map_err(ApiError::bad)?;
    state.preview_budget(peer.ip(), &id).await?;
    // A link the instance does not know yet takes a place in its hour of new previews; when the
    // hour is full, nothing is stored. Two first requests for one link at once may both take one.
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM resource WHERE id=$1) OR EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1)",
    )
    .bind(&id)
    .fetch_one(&state.pool)
    .await?;
    let slot = if known {
        None
    } else {
        match state.reserve_preview(peer.ip()) {
            Ok(slot) => Some(slot),
            Err((status, seconds)) => {
                let (limit, message) = if status == StatusCode::TOO_MANY_REQUESTS {
                    (
                        "client",
                        "new preview limit for this client reached, try again later",
                    )
                } else {
                    ("instance", "preview capacity reached, try again later")
                };
                state
                    .metrics
                    .preview_refused
                    .with_label_values(&[limit])
                    .inc();
                let mut response = ApiError(status, message.into()).into_response();
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, header::HeaderValue::from(seconds));
                return Ok(response);
            }
        }
    };
    let (_, created) = store::ensure_resource_created(&state.pool, &uri).await?;
    // Stored by this request: the place stays taken for the hour. A request that found the link
    // just stored by another gives its place back.
    if let Some(slot) = slot.filter(|_| created) {
        slot.keep();
    }
    let id = crate::alias::resolve_id(&state.pool, &id).await?;
    store::expedite(&state.pool, std::slice::from_ref(&id)).await?;
    state.wake.notify_waiters();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let resource = read(&state.pool, &id, &state.config)
            .await?
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "resource expired".into()))?;
        if resource.card.status != "pending" {
            return Ok((StatusCode::OK, Json(resource)).into_response());
        }
        if tokio::time::Instant::now() >= deadline {
            // Put off until a later attempt (a missing answer waiting for the fetch that
            // confirms it, say): when to ask again. A job still queued or running has none.
            let wait: Option<i64> = sqlx::query_scalar("SELECT ceil(extract(epoch FROM next_attempt_at-now()))::int8 FROM resource WHERE id=$1 AND next_attempt_at>now()+interval '1 second' AND (lease_until IS NULL OR lease_until<now())")
                .bind(&id)
                .fetch_optional(&state.pool)
                .await?;
            let mut response = (StatusCode::ACCEPTED, Json(resource)).into_response();
            if let Some(seconds) = wait {
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    header::HeaderValue::from(seconds.clamp(1, 86_400)),
                );
            }
            return Ok(response);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
/// What a client needs: the version, the User-Agent this instance sends, what it resolves, the
/// limits a client must respect, and the licence of the OSM-derived values its cards carry.
#[derive(Serialize, ToSchema)]
pub struct Info {
    pub version: String,
    /// The shape of the cards this instance returns: 2, one flat object per card (docs/api.md).
    pub api_shape: u8,
    /// The User-Agent this instance's requests to other services carry: its version and the
    /// operator's contact (`http.contact`), when set.
    pub user_agent: String,
    /// The kinds of links this instance resolves: its routes and card kinds that are on.
    pub resolves: Vec<String>,
    pub limits: InfoLimits,
    pub osm_licence: OsmLicence,
    /// What a client must show while it shows values from these services, each the
    /// `credits[].provider` of the values it gave: the service's notice, verbatim, and its logo
    /// (less prominent than the client's own marks). Listed while this instance uses the
    /// service, and after, while a stored card still holds one of its values; absent when none
    /// is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attributions: Vec<InfoAttribution>,
}
#[derive(Serialize, ToSchema)]
pub struct InfoAttribution {
    /// The `credits[].provider` of the service's values (`tmdb`).
    #[schema(example = "tmdb")]
    pub provider: String,
    #[schema(example = "This product uses the TMDB API but is not endorsed or certified by TMDB.")]
    pub notice: String,
    /// The service's logo, an `https:` URL.
    pub logo: String,
    /// The service's site, to link the logo or the notice to.
    #[schema(example = "https://www.themoviedb.org")]
    pub url: String,
}
#[derive(Serialize, ToSchema)]
pub struct InfoLimits {
    /// IDs or URIs in one `POST /v1/resources/lookup`.
    pub lookup: usize,
    /// Resource IDs or URIs in one `POST /v1/changes/lookup`.
    pub change_lookup: usize,
    /// Feed IDs or URIs in one `POST /v1/occurrences/lookup`.
    pub occurrence_lookup: usize,
    /// The largest `limit` of one search page.
    pub search_page: usize,
    /// `related`: the taggers sampled, the authors of the resource's most recent tags, at most this many…
    pub related_taggers: usize,
    /// …and how far back their references count without `window` (days)…
    pub related_window_days: u32,
    /// …at most this many of each tagger's references (their most recent)…
    pub related_refs_per_tagger: usize,
    /// …and a row needs at least this many shared taggers (its group's, when collapsed).
    pub related_min_shared: usize,
    /// `related`: the taggers are drawn from the resource's most recent tags, at most this many…
    pub related_tag_sample: usize,
    /// …on each of at most this many of its links (its group's members and their aliases), the
    /// ones with the newest tags.
    pub related_seed_links: usize,
    /// `collapse=group`: references counted per member to pick a group's row.
    pub collapse_refs_per_member: usize,
    /// Requests per minute per client address, every public route together.
    pub requests_per_minute: usize,
    /// `POST /v1/resolve` requests per minute per client address, and per URI.
    pub previews_per_minute: usize,
    pub previews_per_uri_per_minute: usize,
}
#[derive(Serialize, ToSchema)]
pub struct OsmLicence {
    pub licence: String,
    pub attribution: String,
    pub url: String,
    /// The share-alike download: the ODbL derivative database as a daily file.
    pub dump: String,
    /// When the file was last written; `null` before the first.
    pub updated_at: Option<DateTime<Utc>>,
}
fn dump_updated_at(config: &Config) -> Option<DateTime<Utc>> {
    crate::osm_dump::written_at(std::path::Path::new(&config.osm.dump_dir)).map(DateTime::from)
}
#[utoipa::path(get,tag="Service",path="/v1/info",responses((status=200,body=Info)),description="The version, what this instance resolves and the limits a client must respect. The same path on admin_listen has the operator detail.")]
pub async fn info(State(state): State<AppState>) -> Json<Info> {
    let c = &state.config;
    // A provider's attribution shows while its values may show: while a source that writes them
    // is not switched off (whether or not the worker runs), and after, while a stored card still
    // holds one (until a read or the maintenance pass takes it out).
    let mut attributions = vec![];
    for provider in model::CreditProvider::FROM_FACTS {
        let Some(a) = provider.attribution() else {
            continue;
        };
        let on = crate::also_on::registered().iter().any(|s| {
            s.fills_as() == Some(*provider) && !crate::also_on::switched_off(c, s.as_ref())
        });
        if on || shown_providers(&state).await.contains(provider) {
            attributions.push(InfoAttribution {
                provider: provider.name().into(),
                notice: a.notice.into(),
                logo: a.logo.into(),
                url: a.url.into(),
            });
        }
    }
    let mut resolves: Vec<String> = [
        ("web", c.web.enabled),
        ("pubky", c.pubky.enabled),
        ("geo", c.geo.enabled),
        ("osm", c.osm.enabled),
        ("event", c.event.enabled),
        ("calendar", c.event.enabled && c.event.calendars),
    ]
    .into_iter()
    .filter(|(_, on)| *on)
    .map(|(kind, _)| kind.to_owned())
    .collect();
    resolves.extend(
        crate::resolver::cards::KINDS
            .iter()
            .filter(|kind| c.cards.resolver(kind).enabled)
            .map(|kind| (*kind).to_owned()),
    );
    Json(Info {
        version: env!("CARGO_PKG_VERSION").into(),
        api_shape: 2,
        user_agent: crate::resolver::egress::user_agent(&c.egress.contact),
        resolves,
        limits: InfoLimits {
            lookup: 500,
            change_lookup: 500,
            occurrence_lookup: 500,
            search_page: 500,
            related_taggers: c.search.related_taggers,
            related_window_days: c.search.related_window_days,
            related_refs_per_tagger: c.search.related_refs_per_tagger,
            related_min_shared: c.search.related_min_shared,
            related_tag_sample: c.search.related_tag_sample,
            related_seed_links: c.search.related_seed_links,
            collapse_refs_per_member: c.search.collapse_refs_per_member,
            requests_per_minute: c.api.requests_per_minute,
            previews_per_minute: c.preview.requests_per_minute,
            previews_per_uri_per_minute: c.preview.uri_requests_per_minute,
        },
        osm_licence: OsmLicence {
            licence: "ODbL-1.0".into(),
            attribution: crate::resolver::osm::ATTRIBUTION.into(),
            url: "https://opendatacommons.org/licenses/odbl/1-0/".into(),
            dump: "/v1/dumps/osm_place.ndjson.gz".into(),
            updated_at: dump_updated_at(c),
        },
        attributions,
    })
}
/// How long [`shown_providers`] keeps its answer.
const SHOWN_SECS: u64 = 60;
/// The providers written from facts whose values some stored card holds: one bounded read per
/// provider (an `EXISTS` over the cards of the types it fills, by the `type_path` index), kept
/// [`SHOWN_SECS`]. A failed read counts every provider as shown: an attribution too many, never
/// one too few.
async fn shown_providers(state: &AppState) -> Vec<model::CreditProvider> {
    let mut cache = state.shown.lock().await;
    if let Some((at, shown)) = &*cache {
        if at.elapsed() < Duration::from_secs(SHOWN_SECS) {
            return shown.clone();
        }
    }
    let mut shown = vec![];
    for provider in model::CreditProvider::FROM_FACTS {
        let held: Result<bool, sqlx::Error> = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource WHERE type_path && $1::text[] AND osm_id IS NULL AND EXISTS(SELECT 1 FROM jsonb_array_elements(CASE jsonb_typeof(doc->'credits') WHEN 'array' THEN doc->'credits' ELSE '[]' END) c WHERE c->>'provider'=$2))")
            .bind(provider.filled_types())
            .bind(provider.name())
            .fetch_one(&state.pool)
            .await;
        if held.unwrap_or(true) {
            shown.push(*provider);
        }
    }
    *cache = Some((Instant::now(), shown.clone()));
    shown
}
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
    let sources: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM source_state s ORDER BY source")
            .fetch_all(&state.pool)
            .await?;
    let row=sqlx::query("SELECT (SELECT count(*) FROM resource) AS resources,(SELECT count(*) FROM resource_alias) AS aliases,(SELECT count(*) FROM reference) AS references,(SELECT count(*) FROM occurrence) AS occurrences").fetch_one(&state.pool).await?;
    let ingestion:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('nexus',nexus,'cursor',cursor,'feed_length',feed_length,'lag',greatest(feed_length-cursor,0),'epoch',epoch,'updated_at',updated_at) FROM feed_cursor ORDER BY nexus").fetch_all(&state.pool).await?;
    // Aliases are answered by their roots and have no queue of their own: counted apart.
    let statuses:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('resolver',resolver,'status',status,'count',count(*),'dead_letter',count(*) FILTER (WHERE dead_letter),'leased',count(*) FILTER (WHERE lease_until>now())) FROM resource r WHERE NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id) GROUP BY resolver,status ORDER BY resolver,status").fetch_all(&state.pool).await?;
    let mut body = json!({"groups":state.config.groups,"version":env!("CARGO_PKG_VERSION"),"manifests":crate::adapter::configured(&state.config),"sources":sources,"metrics":state.metrics.summary(),"ingestion":ingestion,"counts":{"resources":row.try_get::<i64,_>("resources")?,"aliases":row.try_get::<i64,_>("aliases")?,"references":row.try_get::<i64,_>("references")?,"occurrences":row.try_get::<i64,_>("occurrences")?},"osm_licence":{"licence":"ODbL-1.0","attribution":crate::resolver::osm::ATTRIBUTION,"url":"https://opendatacommons.org/licenses/odbl/1-0/","dump":"/v1/dumps/osm_place.ndjson.gz","updated_at":dump_updated_at(&state.config)},"resolvers":{"event":{"enabled":state.config.event.enabled,"calendar":state.config.event.calendars,"geocoding":state.config.event.geocoding,"months_back":state.config.event.months_back,"months_forward":state.config.event.months_forward},"geo":{"enabled":state.config.geo.enabled,"network":false},"osm":{"enabled":state.config.osm.enabled,"concurrency":1,"batch_size":50,"requests_per_second":state.config.osm.requests_per_second},"web":{"enabled":state.config.web.enabled,"concurrency":state.config.web.concurrency},"pubky":{"enabled":state.config.pubky.enabled,"concurrency":state.config.pubky.concurrency},"statuses":statuses}});
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
        description = "Pinakes ranks, Nexus hydrates. External resource metadata and search. Every operation carries examples named by their values, covering every kind of card and source; pick one in a dropdown and use Try it out.",
        license(name = "MIT")
    ),
    tags(
        (name = "Previews", description = "Link previews and cards: resolve a pasted link, look up the cards of a page of posts, open one card."),
        (name = "Search", description = "Map viewports, nearby places, events, tag pages, the search box and social reach."),
        (name = "Events", description = "The dates of calendar feeds."),
        (name = "Groups", description = "Links to the same thing on several services: a hint on each card, never a merge."),
        (name = "Changes", description = "Freshness: what changed for the links a client keeps."),
        (name = "Service", description = "Version, resolvers, licence and this document."),
        (name = "Licensing", description = "The ODbL share-alike download of the OpenStreetMap-derived data."),
        (name = "Agents", description = "The usage guide for AI agents (also as a skill) and the index that links it and the MCP endpoint, `POST /mcp`, which docs/api.md describes.")
    ),
    paths(
        group,
        crate::event::storage::list,
        crate::event::storage::lookup,
        crate::episodes::list,
        crate::collections::list,
        search,
        resource,
        by_uri,
        lookup,
        resolve,
        info,
        openapi,
        osm_dump,
        crate::change_api::lookup,
        crate::agents::index,
        crate::agents::llms_txt,
        crate::agents::skill_md
    ),
    components(schemas(
        CollapseMode,
        crate::event::storage::OccurrencePage,
        crate::event::storage::OccurrenceLookup,
        crate::episodes::EpisodePage,
        crate::episodes::Episode,
        crate::collections::CollectionPage,
        crate::collections::Collection,
        model::Stream,
        crate::embed::Embed,
        crate::embed::EmbedKind,
        model::Related,
        model::Credit,
        model::CreditField,
        model::CreditProvider,
        crate::event::storage::ReferenceEvent,
        crate::change_api::ChangeLine,
        crate::change_api::ChangePage,
        crate::change_api::ChangeLookup,
        model::Point,
        Card,
        GroupRef,
        OccurrenceKey,
        ResourceDetail,
        Details,
        GroupDetail,
        AlsoOn,
        VenueRef,
        SearchResponse,
        SearchResult,
        ClusterResponse,
        Cluster,
        LookupRequest,
        ResolveRequest,
        TagFacet,
        TypeFacet,
        HostFacet,
        CategoryFacet,
        Facets,
        OsmDumpLine,
        OsmDumpNotice,
        OsmDumpPlace,
        crate::agents::Index,
        crate::agents::IndexLinks
    ))
)]
pub struct ApiDoc;
/// The stored document of a resource, with the internal fields the public card leaves out
/// (`source`, `ttl_secs`, `group_keys`, `attributes`, `warnings`).
pub async fn admin_document(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !model::valid_id(&id) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    sqlx::query_scalar::<_, Value>("SELECT doc FROM resource_document WHERE id=$1")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "resource not found".into()))
}
/// The public listener's document: exactly the operations `router` serves.
/// Built once: a malformed recipe file fails when the router is built (at startup), not on
/// every `/v1/openapi.json` request.
static PUBLIC_DOCUMENT: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    let mut doc = serde_json::to_value(ApiDoc::openapi()).expect("OpenAPI serialization");
    crate::recipes::apply(&mut doc);
    declare_rejections(&mut doc);
    doc
});
/// The document for each setting of the two hints (`[event] same_event_hint`, `[groups]
/// same_recording_hint`), by `events * 2 + recordings`: without the `maybe_same` field when both
/// are off, and without a hint's examples when it is off.
static PUBLIC_DOCUMENTS: std::sync::LazyLock<[Value; 4]> = std::sync::LazyLock::new(|| {
    std::array::from_fn(|n| {
        let mut doc = PUBLIC_DOCUMENT.clone();
        without_hints(&mut doc, n & 2 != 0, n & 1 != 0);
        doc
    })
});
fn without_hints(doc: &mut Value, events: bool, recordings: bool) {
    if !events && !recordings {
        if let Some(schemas) = doc["components"]["schemas"].as_object_mut() {
            schemas.remove("SameEvent");
            for schema in schemas.values_mut() {
                if let Some(properties) = schema["properties"].as_object_mut() {
                    properties.remove("maybe_same");
                }
            }
        }
    }
    // Examples are keyed by recipe name; a hint's are `same-event-…` and `same-recording-…`.
    fn examples(v: &mut Value, events: bool, recordings: bool) {
        match v {
            Value::Object(map) => {
                if let Some(Value::Object(named)) = map.get_mut("examples") {
                    named.retain(|name, _| {
                        (events || !name.starts_with("same-event-"))
                            && (recordings || !name.starts_with("same-recording-"))
                    });
                }
                map.values_mut()
                    .for_each(|v| examples(v, events, recordings));
            }
            Value::Array(items) => items
                .iter_mut()
                .for_each(|v| examples(v, events, recordings)),
            _ => {}
        }
    }
    examples(&mut doc["paths"], events, recordings);
}
/// Every operation that takes a body answers a body it cannot read with an error object
/// (`rejection_without_input`): declared once here for all of them.
fn declare_rejections(doc: &mut Value) {
    let Some(paths) = doc["paths"].as_object_mut() else {
        return;
    };
    for operations in paths.values_mut() {
        for method in ["post", "put"] {
            // `get_mut`, not indexing: indexing a missing key would create the operation.
            let Some(responses) = operations
                .get_mut(method)
                .and_then(|operation| operation.get_mut("responses"))
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            for (status, description) in [
                ("413", "Body over 2 MB"),
                ("415", "Body not JSON"),
                ("422", "Body of another shape than the route takes"),
            ] {
                responses
                    .entry(status)
                    .or_insert_with(|| json!({ "description": description }));
            }
        }
    }
}
/// The public document as an instance with `config` serves it.
pub fn openapi_document_for(config: &Config) -> &'static Value {
    &PUBLIC_DOCUMENTS[usize::from(same_event(config)) * 2 + usize::from(same_recording(config))]
}
pub fn openapi_document() -> Value {
    PUBLIC_DOCUMENT.clone()
}
/// The document as `docs/openapi.json` keeps it: without each official-API source's examples
/// (`resolve-{source}-{n}`), which its page in docs/sources lists, so adding or removing a
/// source leaves the snapshot as it is.
pub fn openapi_snapshot() -> Value {
    let own: std::collections::BTreeSet<String> = crate::recipes::recipes()
        .into_iter()
        .filter(|r| r.source.is_some())
        .map(|r| r.name)
        .collect();
    fn examples(v: &mut Value, own: &std::collections::BTreeSet<String>) {
        match v {
            Value::Object(map) => {
                if let Some(Value::Object(named)) = map.get_mut("examples") {
                    named.retain(|name, _| !own.contains(name));
                }
                map.values_mut().for_each(|v| examples(v, own));
            }
            Value::Array(items) => items.iter_mut().for_each(|v| examples(v, own)),
            _ => {}
        }
    }
    let mut doc = openapi_document();
    examples(&mut doc["paths"], &own);
    doc
}
#[utoipa::path(get,tag="Service",path="/v1/openapi.json",responses((status=200,body=Value)))]
async fn openapi(State(state): State<AppState>) -> Json<&'static Value> {
    Json(openapi_document_for(&state.config))
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
    /// Every derived document served publicly, keyed by the version hash that change entries
    /// and pinned resource documents name.
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

#[utoipa::path(get,tag="Licensing",path="/v1/dumps/osm_place.ndjson.gz",responses((status=200,description="The ODbL derivative database, written daily, as a gzip file download. Decompressed it is one JSON object per line: the licence notice first (`OsmDumpNotice`), then one cached OSM object per line (`OsmDumpPlace`). Single byte ranges are served (206)",content_type="application/gzip",headers(("Content-Disposition"=String,description="`attachment; filename=\"osm_place.ndjson.gz\"`"),("ETag"=String),("Last-Modified"=String),("Accept-Ranges"=String,description="`bytes`"))),(status=206,description="The requested byte range of the file",content_type="application/gzip"),(status=304,description="ETag or Last-Modified unchanged"),(status=416,description="Range not satisfiable"),(status=429,description="Over `limits.osm_dump_downloads_per_hour` transfers (200 or 206) to this address in the last hour; HEAD, 304 and 416 are free"),(status=503,description="No dump file written yet; retry later")))]
async fn osm_dump(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let unavailable = |_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "dump file unreadable".into(),
        )
    };
    let dir = std::path::Path::new(&state.config.osm.dump_dir);
    // The open file stays the one served even if the next write renames another into place.
    let mut file = match tokio::fs::File::open(crate::osm_dump::path(dir)).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the dump file is not written yet".into(),
            ))
        }
        Err(e) => return Err(unavailable(e)),
    };
    let meta = file.metadata().await.map_err(unavailable)?;
    let (len, modified) = (meta.len(), meta.modified().map_err(unavailable)?);
    let nanos = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let etag = format!("\"{len:x}-{nanos:x}\"");
    let last_modified = httpdate::fmt_http_date(modified);
    let named = |h: &str| {
        h.split(',')
            .any(|v| v.trim() == "*" || v.trim().trim_start_matches("W/") == etag)
    };
    let not_modified = match headers.get(header::IF_NONE_MATCH) {
        Some(v) => v.to_str().is_ok_and(named),
        None => headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok())
            .is_some_and(|since| {
                modified
                    .duration_since(since)
                    .map_or(true, |d| d < Duration::from_secs(1))
            }),
    };
    let common = [
        (header::ETAG, etag.clone()),
        (header::LAST_MODIFIED, last_modified.clone()),
        (header::CACHE_CONTROL, "public, max-age=3600".to_owned()),
        (header::ACCEPT_RANGES, "bytes".to_owned()),
    ];
    if not_modified {
        return Ok((StatusCode::NOT_MODIFIED, common).into_response());
    }
    // One range, when If-Range (if any) still names this file; otherwise the whole file.
    let current = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v == etag || v == last_modified);
    let range = headers
        .get(header::RANGE)
        .filter(|_| current)
        .and_then(|v| v.to_str().ok())
        .map(|v| byte_range(v, len));
    let (status, start, end) = match range {
        None | Some(Range::Ignored) => (StatusCode::OK, 0, len),
        Some(Range::Bytes(start, end)) => (StatusCode::PARTIAL_CONTENT, start, end),
        Some(Range::Unsatisfiable) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response())
        }
    };
    // Only a transfer counts: not a HEAD, a revalidation or an unsatisfiable range.
    if method != axum::http::Method::HEAD {
        state.download_budget(peer.ip()).await?;
    }
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(unavailable)?;
    let mut body = file.take(end - start);
    let stream = async_stream::stream! {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match body.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => yield Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(&buf[..n])),
                Err(e) => { yield Err(e); break; }
            }
        }
    };
    let mut response = (status, common, axum::body::Body::from_stream(stream)).into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/gzip"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static("attachment; filename=\"osm_place.ndjson.gz\""),
    );
    h.insert(header::CONTENT_LENGTH, (end - start).into());
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{len}", end - 1)
                .parse()
                .expect("range header"),
        );
    }
    Ok(response)
}
enum Range {
    /// `[start, end)`.
    Bytes(u64, u64),
    Unsatisfiable,
    /// Not one byte range (several, or another unit): the whole file is sent.
    Ignored,
}
/// A `Range` header against a file of `len` bytes (RFC 9110 §14.1.2, one range).
fn byte_range(value: &str, len: u64) -> Range {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return Range::Ignored;
    };
    if spec.contains(',') {
        return Range::Ignored;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Range::Unsatisfiable;
    };
    let (first, last) = (first.trim(), last.trim());
    let parsed = if first.is_empty() {
        // The last `last` bytes.
        last.parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .map(|n| (len.saturating_sub(n), len))
    } else {
        first.parse::<u64>().ok().and_then(|start| {
            let end = if last.is_empty() {
                Some(len)
            } else {
                last.parse::<u64>()
                    .ok()
                    .filter(|l| *l >= start)
                    .map(|l| l.saturating_add(1).min(len))
            };
            end.map(|end| (start, end))
        })
    };
    match parsed {
        Some((start, end)) if start < end => Range::Bytes(start, end),
        _ => Range::Unsatisfiable,
    }
}

pub fn admin_router(state: AppState) -> Router {
    let routes = Router::new()
        .route("/v1/groups/{id}", axum::routing::put(curate_group))
        .route("/v1/resources/{id}/document", get(admin_document))
        .route("/metrics", get(metrics))
        .route("/v1/info", get(admin_info));
    with_admin_layers(routes, state)
}
/// The admin listener's layer stack around `routes`: the panic backstop innermost, then the
/// bearer check, then the `Host` check outermost, so a request for another host or without the
/// credential never reaches a handler. No CORS. Public so a test can mount a route under exactly
/// this stack.
pub fn with_admin_layers(routes: Router<AppState>, state: AppState) -> Router {
    panic_backstop(routes, &state, "admin")
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_auth,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_host,
        ))
        .with_state(state)
}
/// Answers 421 to a request whose `Host` (or HTTP/2 authority) the admin listener does not serve
/// ([`admin_host_allowed`]). A browser always sends one, so a request with neither comes from
/// another kind of client and passes.
async fn admin_host(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let authority = match request.headers().get(header::HOST) {
        Some(value) => Some(value.to_str().unwrap_or_default()),
        None => request.uri().authority().map(|a| a.as_str()),
    };
    if authority.is_some_and(|a| !admin_host_allowed(a, &state.config.admin_hosts)) {
        return (StatusCode::MISDIRECTED_REQUEST, "admin host not allowed").into_response();
    }
    next.run(request).await
}
/// Whether the admin listener serves `authority` (`host[:port]`): `localhost`, an IP address, or
/// one of `admin_hosts`. A page that rebinds its own name to this machine still sends that name;
/// a browser sends an IP address only for a page this listener itself served.
fn admin_host_allowed(authority: &str, admin_hosts: &[String]) -> bool {
    let Ok(authority) = authority.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    if authority.as_str().contains('@') {
        return false;
    }
    let host = authority.host();
    let literal = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if literal.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    host == "localhost" || admin_hosts.contains(&host)
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

/// `GET /v1/groups/{id}`: what joined the group and every member's own card. A group is a hint
/// that the members are one thing on several services; nothing about them is merged.
#[derive(Serialize, ToSchema)]
pub struct GroupDetail {
    pub id: String,
    /// The type most members have (ties by name).
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// An operator set the members.
    pub curated: bool,
    /// The identifiers two or more members share (`cite:…`, `ics_feed:…`, `ics_uid:…`).
    pub keys: Vec<String>,
    /// Every member's card, by ID and then member key.
    pub members: Vec<Card>,
}
#[utoipa::path(get,tag="Groups",path="/v1/groups/{id}",params(("id"=String,Path)),responses((status=200,body=GroupDetail),(status=400,description="Invalid ID or disabled module"),(status=404,description="Unknown group")))]
pub async fn group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<GroupDetail>, ApiError> {
    if !model::valid_id(&id) || !state.config.groups.enabled {
        return Err(ApiError::bad("invalid or disabled group"));
    }
    group_detail(
        &state.pool,
        &id,
        (same_event(&state.config), same_recording(&state.config)),
    )
    .await?
    .map(Json)
    .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "group not found".into()))
}
async fn group_detail(
    pool: &PgPool,
    id: &str,
    hints: (bool, bool),
) -> Result<Option<GroupDetail>, ApiError> {
    let curated: Option<bool> = sqlx::query_scalar("SELECT curated FROM \"group\" WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(curated) = curated else {
        return Ok(None);
    };
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM resource_group WHERE group_id=$1 GROUP BY key HAVING count(DISTINCT unit_id)>1 ORDER BY key",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let rows: Vec<Value> = sqlx::query_scalar(&[
            "SELECT to_jsonb(d)||jsonb_build_object('group',group_hint(d.unit_id),'maybe_same',",
            &maybe_same_sql("d.unit_id", "d.type_path", 3, "$2", "$3"),
            ",'also_on',also_on(d.unit_id,d.unit_id,8,true)) FROM group_unit u CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=u.unit_id OFFSET 0) d WHERE u.group_id=$1 ORDER BY d.id,d.unit_id",
        ]
        .concat(),
    )
    .bind(id)
    .bind(hints.0)
    .bind(hints.1)
    .fetch_all(pool)
    .await?;
    let mut members = rows.iter().map(card).collect::<Result<Vec<_>, _>>()?;
    with_refs(pool, members.iter_mut()).await?;
    let mut types = BTreeMap::<&str, usize>::new();
    for t in members.iter().filter_map(|m| m.type_name.as_deref()) {
        *types.entry(t).or_default() += 1;
    }
    let type_name = types
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(t, _)| (*t).to_owned());
    Ok(Some(GroupDetail {
        id: id.into(),
        type_name,
        curated,
        keys,
        members,
    }))
}
pub async fn curate_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<crate::groups::Curation>,
) -> Result<Json<GroupDetail>, ApiError> {
    crate::groups::curate(&state.pool, &id, body, &state.config.groups)
        .await
        .map_err(|e| {
            tracing::warn!(error=%crate::resolver::log_error(&e),"group curation failed");
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
        })?;
    group_detail(
        &state.pool,
        &id,
        (same_event(&state.config), same_recording(&state.config)),
    )
    .await?
    .map(Json)
    .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "group not found".into()))
}
#[cfg(test)]
mod tests {
    use super::*;
    /// Up to three letters or digits match at word starts; longer text, other signs and scripts
    /// written without spaces keep the match inside the name.
    #[test]
    fn short_queries_match_word_starts() {
        let start = |text: &str| word_start(text).map(|w| (w.patterns, w.regex));
        assert_eq!(
            start("lol"),
            Some((vec!["%lol%".into()], "(^|[^[:alnum:]])lol".into()))
        );
        assert_eq!(
            start(" AI "),
            Some((vec!["%AI%".into()], "(^|[^[:alnum:]])AI".into()))
        );
        assert_eq!(
            start("a b"),
            Some((
                vec!["%a%".into(), "%b%".into()],
                "(^|[^[:alnum:]])a\\s+b".into()
            ))
        );
        assert_eq!(start("été").unwrap().regex, "(^|[^[:alnum:]])été");
        assert_eq!(start("x²").unwrap().regex, "(^|[^[:alnum:]])x²");
        for longer_or_other in [
            "lola",
            "ab cd",
            "c++",
            "c#",
            "a'b",
            "a%",
            "a_",
            "a\\",
            "音乐",
            "ไทย",
            "ㄅㄆ",
            "\u{1B001}",
            "",
            "  ",
        ] {
            assert!(start(longer_or_other).is_none(), "{longer_or_other:?}");
        }
    }
    /// A tag equals a text search when it spells the same words joined by a space, a hyphen, an
    /// underscore or nothing, in any case; and an empty `q` ranks nothing by relevance.
    #[test]
    fn relevance_spellings_and_default() {
        assert_eq!(
            label_spellings("  Daft   PUNK "),
            ["daft punk", "daft-punk", "daft_punk", "daftpunk"]
        );
        assert_eq!(label_spellings("Music"), ["music"]);
        let search = |q: Option<&str>, sort: Option<&str>, related: bool| SearchQuery {
            q: q.map(Into::into),
            sort: sort.map(Into::into),
            related: related.then(|| "a".repeat(32)),
            ..Default::default()
        };
        assert_eq!(
            effective_sort(&search(Some("music"), None, false)),
            Some("relevance")
        );
        assert_eq!(effective_sort(&search(Some(""), None, false)), None);
        assert_eq!(effective_sort(&search(None, None, false)), None);
        assert_eq!(effective_sort(&search(Some("music"), None, true)), None);
        assert_eq!(
            effective_sort(&search(Some("music"), Some("recent"), false)),
            Some("recent")
        );
        assert!(validate_search(&search(None, Some("relevance"), false)).is_err());
        assert!(validate_search(&search(Some("music"), Some("relevance"), false)).is_ok());
    }
    /// A place in the hour of new previews is given back when its request ends before the link
    /// is stored (an error, a client gone), and stays once kept.
    #[test]
    fn a_preview_place_is_given_back_unless_kept() {
        let previews: Previews = Arc::default();
        let by: std::net::IpAddr = "127.0.0.2".parse().unwrap();
        let take = || {
            let at = Instant::now();
            previews.lock().unwrap().push_back((at, by));
            PreviewSlot {
                previews: previews.clone(),
                at,
                by,
                kept: false,
            }
        };
        let kept = take();
        drop(take());
        assert_eq!(previews.lock().unwrap().len(), 1);
        kept.keep();
        assert_eq!(previews.lock().unwrap().len(), 1);
    }
    /// The admin listener serves `localhost`, IP addresses and the configured names, on any port,
    /// and nothing else: not a rebinding page's own name, not one dressed as loopback.
    #[test]
    fn the_admin_listener_serves_loopback_addresses_and_its_own_names() {
        let names = ["metrics.internal".to_owned()];
        for allowed in [
            "localhost",
            "localhost:9090",
            "LocalHost.:9090",
            "127.0.0.1",
            "127.0.0.1:9090",
            "[::1]:9090",
            "[::1]",
            "10.1.2.3:9090",
            "metrics.internal:9090",
            "METRICS.internal",
        ] {
            assert!(admin_host_allowed(allowed, &names), "{allowed}");
        }
        for refused in [
            "attacker.example",
            "attacker.example:9090",
            "localhost.attacker.example",
            "127.0.0.1.nip.io:9090",
            "metrics.internal.attacker.example",
            "user@127.0.0.1:9090",
            "",
            "local host",
            "[::1",
        ] {
            assert!(!admin_host_allowed(refused, &names), "{refused}");
        }
        assert!(!admin_host_allowed("metrics.internal", &[]));
    }
    /// Every form of key a registered source records (`also_on::Source::key_forms`) searches
    /// back exactly as recorded: `key=` with the key a read lists finds that key.
    #[test]
    fn every_key_form_searches_back() {
        for source in crate::also_on::registered() {
            let id = source.id();
            // A namespace a source keeps one object of is one it records keys in.
            for namespace in source.single_namespaces() {
                assert!(
                    source.key_forms().iter().any(|(n, _)| n == namespace),
                    "{id}: no key form for {namespace}"
                );
            }
            for (namespace, example) in source.key_forms() {
                let key = format!("{namespace}:{example}");
                assert!(crate::also_on::valid_key(&key), "{id}: {key}");
                let filter = key_filter(&key).unwrap_or_else(|_| panic!("{id}: {key} is a 400"));
                assert!(
                    filter.keys.contains(&key),
                    "{id}: {key} searches {:?}",
                    filter.keys
                );
            }
        }
        // The sources recording a product's barcode (on its card or a derived link), while
        // they are in this build.
        for source in crate::also_on::registered()
            .iter()
            .filter(|s| ["product_keys", "open_food_facts"].contains(&s.id()))
        {
            let id = source.id();
            assert!(source.key_forms().iter().any(|(n, _)| *n == "gtin"), "{id}");
        }
        let apple = [("podcast:apple", "1")];
        assert!(crate::also_on::of_form(&apple, "podcast:apple:1"));
        assert!(!crate::also_on::of_form(&apple, "podcast:apple:"));
        assert!(!crate::also_on::of_form(&apple, "podcast:apple_episode:1"));
    }
    /// A request axum could not read is answered with what its route takes.
    #[test]
    fn a_rejection_says_what_the_route_takes() {
        use axum::http::Method;
        let m = |method: Method, path: &str, status| rejection_message(&method, path, status);
        assert_eq!(
            m(
                Method::POST,
                "/v1/resolve",
                StatusCode::UNPROCESSABLE_ENTITY
            ),
            "the body must be a JSON object with a `uri` string"
        );
        assert!(m(
            Method::POST,
            "/v1/resources/lookup",
            StatusCode::BAD_REQUEST
        )
        .contains("`uris`"));
        assert!(m(
            Method::POST,
            "/v1/occurrences/lookup",
            StatusCode::BAD_REQUEST
        )
        .contains("`resource_ids`"));
        assert!(m(Method::PUT, "/v1/groups/abc", StatusCode::BAD_REQUEST).contains("`members`"));
        assert!(m(Method::GET, "/v1/search", StatusCode::BAD_REQUEST).contains("query parameter"));
        assert_eq!(
            m(Method::POST, "/v1/resolve", StatusCode::PAYLOAD_TOO_LARGE),
            "the body is over 2 MB"
        );
        assert!(m(
            Method::POST,
            "/v1/resolve",
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        )
        .contains("application/json"));
    }
    /// A link on a known service is named by it, even before its card is read.
    #[test]
    fn a_known_service_is_named_before_its_card_is_read() {
        // A member still pending: no document site, only its host.
        let apple = "https://music.apple.com/gb/artist/657515";
        assert_eq!(
            site(apple, None, Some("music.apple.com")).as_deref(),
            Some("Apple Music")
        );
        // A page's own site name never replaces the service's.
        assert_eq!(
            site(apple, Some("Web Player"), Some("music.apple.com")).as_deref(),
            Some("Apple Music")
        );
        assert_eq!(
            site(
                "https://de.m.wikipedia.org/wiki/Paris",
                None,
                Some("de.m.wikipedia.org")
            )
            .as_deref(),
            Some("Wikipedia (de)")
        );
        assert_eq!(
            site(
                "https://musicbrainz.org/isrc/GBUM71029604",
                None,
                Some("musicbrainz.org")
            )
            .as_deref(),
            Some("MusicBrainz")
        );
        assert_eq!(
            site(
                "https://www.wikidata.org/wiki/Q90",
                None,
                Some("www.wikidata.org")
            )
            .as_deref(),
            Some("Wikidata")
        );
        // Any other page: its document's site, else its host.
        assert_eq!(
            site(
                "https://example.org/a",
                Some("Example"),
                Some("example.org")
            )
            .as_deref(),
            Some("Example")
        );
        assert_eq!(
            site("https://example.org/a", None, Some("example.org")).as_deref(),
            Some("example.org")
        );
    }
    /// A card's facts: its official API's fields, then the also-on sources', each value once.
    #[test]
    fn document_and_source_facts_merge() {
        let map = |pairs: &[(&str, &str)]| {
            Some(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<std::collections::BTreeMap<_, _>>(),
            )
        };
        assert_eq!(
            merge_facts(
                map(&[("citation", "1 CFR 2"), ("isrc", "A")]),
                map(&[("isrc", "A; B"), ("mbid", "m")])
            ),
            map(&[("citation", "1 CFR 2"), ("isrc", "A; B"), ("mbid", "m")])
        );
        assert_eq!(merge_facts(None, None), None);
    }
    /// A range header is read without overflow: the largest last byte means "to the end".
    #[test]
    fn byte_ranges_never_overflow() {
        let len = 100;
        assert!(matches!(
            byte_range("bytes=0-18446744073709551615", len),
            Range::Bytes(0, 100)
        ));
        assert!(matches!(
            byte_range("bytes=18446744073709551615-", len),
            Range::Unsatisfiable
        ));
        assert!(matches!(
            byte_range("bytes=-18446744073709551615", len),
            Range::Bytes(0, 100)
        ));
        assert!(matches!(
            byte_range("bytes=10-19", len),
            Range::Bytes(10, 20)
        ));
        assert!(matches!(
            byte_range("bytes=20-10", len),
            Range::Unsatisfiable
        ));
        assert!(matches!(byte_range("bytes=0-1,5-6", len), Range::Ignored));
        assert!(matches!(byte_range("items=0-1", len), Range::Ignored));
    }
    /// A cursor reads its rank back bit for bit: one ULP high, the next page repeats this one.
    #[test]
    fn a_search_cursor_keeps_its_rank_exactly() {
        let template = SimpleHmac::<blake3::Hasher>::new_from_slice(&[7; 32]).unwrap();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..10_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let score = (seed >> 11) as f64 / (1u64 << 53) as f64 * (-((seed % 40) as f64)).exp();
            let cursor = PageCursor {
                version: 2,
                score,
                id: "0".repeat(32),
                at: Utc::now(),
                filter: String::new(),
            };
            let read = PageCursor::decode(&cursor.encode(&template).unwrap(), &template)
                .unwrap_or_else(|_| panic!("{score}"));
            assert_eq!(read.score.to_bits(), score.to_bits(), "{score}");
        }
    }
}
