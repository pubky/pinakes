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
    /// `api.trusted_proxies`, parsed.
    trusted_proxies: Arc<Vec<ipnet::IpNet>>,
    /// Set by the daemon once a background task or worker has ended or the daemon is stopping:
    /// `GET /healthz` then answers 503.
    pub stopping: Arc<std::sync::atomic::AtomicBool>,
    /// The text index a search with `q` reads its candidates from: one in memory, read up before
    /// each search, until the daemon sets its own on disk (`search_index`).
    pub search_index: Arc<crate::search_index::SearchIndex>,
}
impl AppState {
    pub fn new(pool: PgPool, mut config: Config) -> anyhow::Result<Self> {
        config.queue.catalogues = crate::catalogue::join::joinable(&config.catalogues);
        let config_catalogues = config.catalogues.enabled_names();
        let mut cursor_key = [0; 32];
        getrandom::fill(&mut cursor_key).context("cursor key entropy unavailable")?;
        let metrics = Arc::new(crate::telemetry::Metrics::new()?);
        metrics.catalogue_switches(&config.catalogues);
        metrics.catalogue_contact(&config);
        let trusted_proxies = Arc::new(config.api.trusted_proxy_nets()?);
        Ok(Self {
            pool,
            trusted_proxies,
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
            stopping: Arc::default(),
            search_index: {
                let index = crate::search_index::SearchIndex::in_memory();
                index.set_catalogues(&config_catalogues);
                index
            },
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
/// The address a public request is counted under: its per-IP budgets, its ODbL dump downloads and
/// its share of the hour's new previews. Set by the per-IP budget layer for every public request
/// ([`client_address`]); an MCP tool's requests carry their MCP request's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientAddr(pub std::net::IpAddr);
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for ClientAddr {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<ClientAddr>()
            .copied()
            .ok_or_else(|| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "missing client address".into(),
                )
            })
    }
}
/// The client of a request from `peer`: `peer` itself, unless it is one of `trusted` (the
/// operator's reverse proxies, `api.trusted_proxies`). From a trusted proxy it is the right-most
/// `X-Forwarded-For` entry that is not itself a trusted proxy: each proxy appends the address it
/// was connected from, so the entries left of it are whatever the client sent and are never
/// read. With no such entry (no header, every entry a trusted proxy, or one that is no address)
/// the client is `peer`.
pub fn client_address(
    peer: std::net::IpAddr,
    headers: &HeaderMap,
    trusted: &[ipnet::IpNet],
) -> std::net::IpAddr {
    let peer = peer.to_canonical();
    let is_trusted = |ip: &std::net::IpAddr| trusted.iter().any(|net| net.contains(ip));
    if !is_trusted(&peer) {
        return peer;
    }
    // Split as bytes: an entry that is no text (a byte a client wrote) is that entry alone, and
    // never hides the entries after it.
    let hops: Vec<&[u8]> = headers
        .get_all("x-forwarded-for")
        .iter()
        .flat_map(|value| value.as_bytes().split(|b| *b == b','))
        .collect();
    for hop in hops.into_iter().rev() {
        let hop = std::str::from_utf8(hop).unwrap_or_default().trim();
        // An address, or one with a port (`192.0.2.1:4711`, `[2001:db8::1]:4711`).
        let Some(ip) = hop
            .parse::<std::net::IpAddr>()
            .ok()
            .or_else(|| hop.parse::<SocketAddr>().ok().map(|a| a.ip()))
            .map(|ip| ip.to_canonical())
        else {
            return peer;
        };
        if !is_trusted(&ip) {
            return ip;
        }
    }
    peer
}
async fn public_budget(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let client = match request.extensions().get::<ClientAddr>() {
        Some(client) => client.0,
        None => {
            let Some(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>() else {
                return ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "missing peer address".into(),
                )
                .into_response();
            };
            let client = client_address(peer.0.ip(), request.headers(), &state.trusted_proxies);
            request.extensions_mut().insert(ClientAddr(client));
            client
        }
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
                format!("api-ip:{client}"),
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
        // The tools answer through the REST routes, under the same layers. Their calls are not
        // counted as requests of their own (the MCP message that made them is), and they have
        // no deadline of their own: the tool call that makes them has one for all its calls
        // (`ApiConfig::tool_timeout`, counted from the MCP message's start).
        let rest = public_layers(routes, state.clone(), false);
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
        .route("/v1/map/clusters", get(map_clusters))
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
/// The public listener's layer stack around `routes`: the panic backstop innermost, then the
/// request deadline, CORS preflights, the per-IP budget, CORS headers, the body limit, the
/// request count and, outermost, the path-scoped Swagger headers. Public so a test can mount a
/// route under exactly this stack.
pub fn with_public_layers(routes: Router<AppState>, state: AppState) -> Router {
    public_layers(routes, state, true)
}
/// `with_public_layers` for the listener itself (`outer`: counted, under the request deadline) or
/// for the calls an MCP tool makes through it (not counted, and under the tool call's deadline,
/// which `agents::mcp` keeps).
fn public_layers(routes: Router<AppState>, state: AppState, outer: bool) -> Router {
    let mut routes = panic_backstop(routes, &state, "public");
    if outer {
        routes = routes.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            request_deadline,
        ));
    }
    let mut router = routes
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
    if outer {
        router = router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            public_requests,
        ));
    }
    if state.config.api.swagger_ui {
        router = router.layer(axum::middleware::from_fn(swagger_headers));
    }
    router.with_state(state)
}
/// When a request's deadline started to run (`request_deadline`), as an extension of the
/// request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestStarted(pub(crate) Instant);
/// A request that takes longer than `limits.request_timeout_secs` to answer is answered 504: a
/// slow statement or a pool that is exhausted holds a request no longer than that. The deadline
/// covers the handler up to its response's head; a body it streams (the dump) is not under it.
/// The request carries the instant it started ([`RequestStarted`]), for what must end inside
/// the deadline (an MCP tool call).
async fn request_deadline(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let deadline = Duration::from_secs(state.config.api.request_timeout_secs);
    request
        .extensions_mut()
        .insert(RequestStarted(Instant::now()));
    match tokio::time::timeout(deadline, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "the request took too long".into(),
        )
        .into_response(),
    }
}
/// Counts and times every request on a listener (`pinakes_api_requests_total`,
/// `pinakes_api_request_duration_seconds`) by the route's path template, never the path asked
/// (`unmatched` when no route has it), and by the status answered. Outermost but for the Swagger
/// headers, so a 429 of the budget and a 504 of the deadline are counted too.
async fn count_request(
    state: AppState,
    listener: &'static str,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", |path| path.as_str())
        .to_owned();
    let started = Instant::now();
    let response = next.run(request).await;
    let metrics = &state.metrics;
    metrics
        .api_requests
        .with_label_values(&[listener, &route, response.status().as_str()])
        .inc();
    metrics
        .api_duration
        .with_label_values(&[listener, &route])
        .observe(started.elapsed().as_secs_f64());
    response
}
async fn public_requests(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    count_request(state, "public", request, next).await
}
async fn admin_requests(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    count_request(state, "admin", request, next).await
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
    /// Cards of any type but these: one schema.org type, or up to 50 comma-separated, each with
    /// its subtypes (`Event` leaves `MusicEvent` out too). A link with no type yet (not read, or
    /// unreachable) is left out, as the `type` facet does not count it. Combines with `type`:
    /// `type=CreativeWork&exclude_type=Article`.
    pub exclude_type: Option<String>,
    pub category: Option<String>,
    /// One host, or up to 10 comma-separated: links on any of them (OR), each exactly as the
    /// links' URLs name it (`www.youtube.com` and `youtube.com` are two hosts); spaces around a
    /// host are ignored.
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
    /// own card; links in no group stay rows of their own.
    #[param(value_type = Option<CollapseMode>)]
    pub collapse: Option<String>,
    /// `true`: only things someone referenced on Pubky: no catalogue card nobody referenced.
    pub referenced: Option<bool>,
}
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CollapseMode {
    Group,
}
/// A cursor of `payload`, signed with the daemon's cursor key: the MAC, then the payload, as
/// unpadded URL-safe base64. Every signed cursor (search, collections, occurrences) is made
/// here and read by [`open`].
pub(crate) fn seal(payload: &[u8], key: &SimpleHmac<blake3::Hasher>) -> String {
    let mut mac = key.clone();
    mac.update(payload);
    let mut signed = mac.finalize().into_bytes().to_vec();
    signed.extend_from_slice(payload);
    URL_SAFE_NO_PAD.encode(signed)
}
/// The payload of a cursor of at most `max` bytes that [`seal`] made with this key; `None` for
/// anything else.
pub(crate) fn open(cursor: &str, max: usize, key: &SimpleHmac<blake3::Hasher>) -> Option<Vec<u8>> {
    if cursor.len() > max {
        return None;
    }
    let signed = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    if signed.len() <= 32 {
        return None;
    }
    let (tag, payload) = signed.split_at(32);
    let mut mac = key.clone();
    mac.update(payload);
    // RustCrypto's verify_slice compares the complete MAC in constant time.
    mac.verify_slice(tag).ok()?;
    Some(payload.to_vec())
}
#[derive(Deserialize, Serialize)]
struct PageCursor {
    score: f64,
    id: String,
    at: DateTime<Utc>,
    filter: String,
}
impl PageCursor {
    fn encode(&self, template: &SimpleHmac<blake3::Hasher>) -> anyhow::Result<String> {
        Ok(seal(&serde_json::to_vec(self)?, template))
    }
    fn decode(s: &str, template: &SimpleHmac<blake3::Hasher>) -> Result<Self, ApiError> {
        open(s, 2048, template)
            .and_then(|payload| serde_json::from_slice(&payload).ok())
            .ok_or_else(|| ApiError::bad("invalid cursor"))
    }
}
/// The one card every route returns for a resource or an occurrence. Absent values are null,
/// never "".
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
    /// these to find the tags (docs/api.md, Card). An occurrence's are its feed's.
    /// A gone social post's are its own link's alone.
    /// Empty when nothing references the card.
    pub refs: Vec<String>,
    /// How many public collection posts hold the card's link, its own or one of its aliases' (the
    /// 31 most recently collected), each once, counted up to 1,000 (`1000`: 1,000 or more); 0
    /// when none does. The collections are
    /// `GET /v1/resources/{id}/collections` (docs/api.md, Collections). An occurrence's are its
    /// feed's.
    pub collections: i64,
    /// The clients that open a Pubky app's object, from the app's profile (docs/pubky-apps.md).
    /// On a card whose link is not a web page (`btc:`, `uniprot:`, ...), the web page its source
    /// reads it from, named by its site. Empty for every other card.
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
    /// On a search row, and on a card built from a catalogue card: the labels the catalogue gives
    /// it, as valid tag labels. Kept apart from people's tags, which Nexus serves for the card's
    /// `uri` and `refs`; a client may offer them as suggested tags. `[]` when the card has none.
    /// Absent on other cards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    /// The catalogue the card is built from (`labels` and `from` show only while it is on).
    #[serde(skip)]
    #[schema(ignore)]
    pub catalogue: Option<String>,
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
/// with the same website host and name (`start` is null, `score` 1). A recording: the same title
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
    /// An event's start; null for an app and a recording.
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
    /// An exact, non-derived member's referenced links, selected and ordered as its card's
    /// `refs` (at most 5). Omitted when empty or just `uri`; absent on derived and maybe links.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(max_items = 5)]
    pub refs: Vec<String>,
    /// The service: a member's site as its card names it, a derived link's service name.
    pub site: Option<String>,
    /// A link nobody tagged, found by a source: never fetched, counted or ranked.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub derived: bool,
    /// For a derived link: the hub or service it came through (`musicbrainz`, `wikidata`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// For a derived link, a qualifier (`archived`, `work`, `source`, `submitted`, `published`,
    /// `pdf`), or `maybe`: possibly the same thing, a match by metadata (artist, title, length)
    /// or another publisher's record naming it, never by the thing's own exact ID; it joins no
    /// group.
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
    /// artist's `country_code`, an album's `release_type`, …; docs/api.md), or a YouTube or Vimeo
    /// video's (a label upload's `artist`, else the channel as `author`; `duration`), then the
    /// facts the also-on sources read for it (such as a music link's `isrc`, `upc` and `mbid`;
    /// each source's page in docs/sources lists its own), by name; several values are joined
    /// with "; ". `null` when there are none.
    #[schema(value_type = Option<Object>)]
    pub facts: Option<std::collections::BTreeMap<String, String>>,
    /// The objects the card points at, each with its own card (docs/api.md, Related objects): a
    /// Bitcoin transaction's block, inputs and outputs; a block's previous and next block and
    /// coinbase transaction; an address's latest transactions; a Lightning node's biggest
    /// channels and their peers; a channel's nodes and funding transaction; a podcast episode's
    /// show (`show`). Each is `{rel, uri, name?, amount_sat?, at?}`; `uri` is a root
    /// `POST /v1/resolve` reads, or null for an input or output without an address (`name` says
    /// what it is). At most 10 of a `rel`, read with the card; `[]` on every other card.
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
    /// On an OpenStreetMap place whose name changed to an unrelated one (another business moved
    /// in under the same OSM object): the places it was, oldest first, each with when it was
    /// replaced and how many references the link got before that (those belong to that place).
    /// Absent on every other card (docs/api.md, Places).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced: Option<Vec<Replaced>>,
}
/// A place an OpenStreetMap object was before an unrelated name replaced its own.
#[derive(Serialize, ToSchema)]
pub struct Replaced {
    pub name: String,
    /// Its OSM `key/value` category.
    pub category: Option<String>,
    /// When the new name replaced it.
    pub until: DateTime<Utc>,
    /// The references the link got before `until`.
    pub references: i64,
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
    /// `facet=tags`: the labels the filtered links carry, most frequent first, at most 100.
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
/// The most entries `facet=tags`, `facet=host` and `facet=category` list each.
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
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LookupRequest {
    #[serde(alias = "resource_ids")]
    pub ids: Option<Vec<String>>,
    pub uris: Option<Vec<String>>,
}
/// The resource IDs a lookup names: `ids` as given, then each of `uris` as its ID (normalised);
/// 1–500 together. An alias ID reads its root where
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
/// The web page that opens a card whose own link is not one (`btc:block/…`, `uniprot:P69905`,
/// `fr:2026-07257`): the `page_url` its source read it from, as a client named by the source's
/// site ("mempool.space"), so a client links the card without building a URL. None for a web
/// link (it opens itself), a Pubky object (its app's clients open it, `resolver::pubky_apps`),
/// a page that is not `https:` or names a user or a credential, and one within a character of a
/// stored attribute's cap ([`model::ATTRIBUTE_VALUE_CAP`]; `model::truncate` cuts on a character
/// boundary, up to 3 bytes short of it): it may have been cut, and would link a page that is not
/// there.
fn page_client(uri: &str, doc: &ResourceMeta) -> Option<model::OpenIn> {
    let scheme = uri.split_once(':')?.0.to_ascii_lowercase();
    if matches!(scheme.as_str(), "http" | "https" | "pubky") {
        return None;
    }
    let page = doc.attributes.get("page_url")?;
    let url = url::Url::parse(page).ok()?;
    if url.scheme() != "https"
        || page.len() + 3 >= model::ATTRIBUTE_VALUE_CAP
        || !url.username().is_empty()
        || url.password().is_some()
        || crate::capability::is_capability_url(&url)
    {
        return None;
    }
    let fits = |s: &String| s.len() <= 64;
    let client = present(Some(&doc.site))
        .filter(fits)
        .or_else(|| url.host_str().map(str::to_owned).filter(fits))?;
    Some(model::OpenIn {
        client,
        url: page.clone(),
    })
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
    // A read object's clients are its own, and an object not read yet has its kind's.
    let open_in = match &doc {
        Some(d) if d.open_in.is_empty() && status != "missing" => {
            page_client(&uri, d).into_iter().collect()
        }
        Some(d) => d.open_in.clone(),
        None => crate::resolver::pubky_apps::open_in(&uri),
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
        maybe_same: same_events(row).map(|all| all.into_iter().take(3).collect()),
        because: because(row),
        labels: doc
            .as_ref()
            .filter(|d| d.catalogue.is_some())
            .map(|d| d.labels.clone()),
        catalogue: doc
            .as_ref()
            .and_then(|d| d.catalogue.as_ref())
            .map(|c| c.catalogue.clone()),
    })
}
/// Cards built from a catalogue that is off now show nothing of it (no `labels`): the start's
/// sweep gives their links back to fetching in the background, and until then they read as
/// cards of their link.
fn shown<'a>(cards: impl IntoIterator<Item = &'a mut Card>, config: &Config) {
    let on = crate::catalogue::join::joinable(&config.catalogues);
    for card in cards {
        if card.catalogue.as_deref().is_some_and(|c| !on.contains(&c)) {
            card.labels = None;
        }
    }
}
/// [`shown`] for a read, whose `from` names the catalogue too.
fn shown_detail(detail: &mut ResourceDetail, config: &Config) {
    if let Some(catalogue) = detail.card.catalogue.clone() {
        shown(std::iter::once(&mut detail.card), config);
        if detail.card.labels.is_none()
            && detail.details.from.as_deref() == Some(catalogue.as_str())
        {
            detail.details.from = None;
        }
    }
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
/// alone: a spelling's link can carry the post's title. The same selection fills exact,
/// non-derived `also_on` members, omitting empty or canonical-only lists. One reference query
/// for all distinct card and member IDs of a response, and one for the missing rows among them.
/// Then the stored `embed` of tagged `also_on`
/// members (`member_embeds`) and each card's `collections` (`collections::count`), one query
/// more each, and a show's `episodes` (`episodes::on_cards`), only when the response holds a show.
pub(crate) async fn with_refs<'a>(
    pool: &PgPool,
    cards: impl IntoIterator<Item = &'a mut Card>,
) -> Result<(), ApiError> {
    let mut cards: Vec<&mut Card> = cards.into_iter().collect();
    // Share the bounded reference selection and privacy check with exact group members.
    // Deduplicate across the whole response: no query per card or also-on entry.
    let mut ids: Vec<String> = cards.iter().map(|c| c.id.clone()).collect();
    ids.extend(
        cards
            .iter()
            .flat_map(|c| &c.also_on)
            .filter(|e| !e.derived && e.kind.as_deref() != Some("maybe"))
            .filter_map(|e| e.id.clone()),
    );
    ids.sort();
    ids.dedup();
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
        for entry in &mut card.also_on {
            if entry.derived || entry.kind.as_deref() == Some("maybe") {
                continue;
            }
            if let Some(refs) = entry.id.as_ref().and_then(|id| by_card.get(id)) {
                if refs.len() != 1 || refs[0] != entry.uri {
                    entry.refs.clone_from(refs);
                }
            }
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
    crate::music_identity::images(pool, &mut cards).await?;
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
/// score 1, by URI; with `recordings`, the same-recording hint for a recording
/// ([`same_recording_sql`]); else `NULL`. An alias, a missing card and a member of the card's own
/// group are never named.
pub(crate) fn maybe_same_sql(
    unit: &str,
    type_path: &str,
    n: u32,
    events: &str,
    recordings: &str,
) -> String {
    let recordings = same_recording_sql(unit, type_path, n, recordings);
    format!(
        "coalesce(maybe_same_hint({unit},{type_path},{n},{events},false),{recordings},CASE WHEN {events} THEN (SELECT jsonb_agg(jsonb_build_object('id',same_app.id,'uri',same_app.uri,'host',same_app.host,'site',same_app.site,'start',NULL,'score',1) ORDER BY same_app.uri) \
         FROM (SELECT DISTINCT same_r.id,same_r.uri,same_r.host,same_r.doc->>'site' AS site FROM resource_fact same_f \
         JOIN resource_fact same_g ON same_g.name='app_match' AND same_g.value=same_f.value AND same_g.resource_id<>same_f.resource_id AND same_g.expires_at>now() AND same_g.source NOT IN (SELECT source FROM also_on_source_off) \
         JOIN resource same_r ON same_r.id=same_g.resource_id \
         WHERE same_f.resource_id={unit} AND same_f.name='app_match' AND same_f.expires_at>now() AND same_f.source NOT IN (SELECT source FROM also_on_source_off) AND same_r.status<>'missing' \
         AND NOT EXISTS (SELECT 1 FROM resource_alias same_alias WHERE same_alias.alias_id=same_r.id) \
         AND NOT EXISTS (SELECT 1 FROM group_unit same_a JOIN group_unit same_b ON same_b.group_id=same_a.group_id WHERE same_a.unit_id={unit} AND same_b.unit_id=same_r.id AND same_a.group_id IS NOT NULL) \
         ORDER BY same_r.uri LIMIT {n}) same_app) END)"
    )
}
/// A recording card's same-recording hint as SQL (with `recordings` on, else `NULL`): of its
/// stored pairs (`same_recording_hint`, best first), up to `n`, those whose other recording's
/// artist and length (when it has one) are facts a source states come first, then those whose
/// own page states them. Any page may copy a track's title, artist and length into its markup;
/// that never puts it ahead of a recording a service vouches for.
fn same_recording_sql(unit: &str, type_path: &str, n: u32, recordings: &str) -> String {
    let fact = |name: &str| {
        format!(
            "EXISTS(SELECT 1 FROM resource_fact same_rf WHERE same_rf.resource_id=same_rec.e->>'id' AND same_rf.name='{name}' AND same_rf.expires_at>now() AND same_rf.source NOT IN (SELECT source FROM also_on_source_off))"
        )
    };
    let (artist, duration) = (fact("artist"), fact("duration"));
    format!(
        "CASE WHEN {recordings} AND coalesce({type_path} @> ARRAY['MusicRecording'],false) THEN \
         (SELECT coalesce(jsonb_agg(same_top.e ORDER BY same_top.vouched DESC,same_top.n),'[]'::jsonb) FROM \
         (SELECT same_rec.e,same_rec.n,({artist} AND ({duration} OR NOT EXISTS(SELECT 1 FROM resource same_rr WHERE same_rr.id=same_rec.e->>'id' AND same_rr.doc#>>'{{attributes,duration}}' IS NOT NULL)))::int AS vouched \
         FROM jsonb_array_elements(same_recording_hint({unit},{kept})) WITH ORDINALITY AS same_rec(e,n) \
         ORDER BY vouched DESC,same_rec.n LIMIT {n}) same_top) END",
        kept = crate::same_recording::KEPT,
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
                refs: Vec::new(),
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
            from: crate::sources::from(d)
                .map(str::to_owned)
                .or_else(|| d.catalogue.as_ref().map(|c| c.catalogue.clone())),
            facts: merge_facts(
                crate::kinds::facts(
                    crate::sources::facts(d)
                        .or_else(|| crate::resolver::pubky_apps::facts(d))
                        .or_else(|| crate::music_sources::card_facts(d))
                        .or_else(|| crate::resolver::cards::media_facts(d)),
                    d,
                    Utc::now(),
                ),
                facts,
            ),
            related: d.related.clone(),
            related_total: (!d.related_total.is_empty()).then(|| d.related_total.clone()),
            keys,
            aliases,
            updated_at: row["resolved_at"].as_str().and_then(|s| s.parse().ok()),
            replaced: None,
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
            replaced: None,
        },
    };
    // The read's card, with up to ten possible duplicates.
    let mut card = card(row)?;
    card.maybe_same = same_events(row);
    Ok(ResourceDetail { card, details })
}
/// The most hosts a search's `host` takes.
pub const SEARCH_HOSTS_MAX: usize = 10;
/// The hosts a search's `host` names (1 to [`SEARCH_HOSTS_MAX`], comma-separated, each at most
/// 256 bytes), each as given but for spaces around it (no URL holds a host with one), without
/// repeats. No host is echoed in the refusal.
fn search_hosts(hosts: &str) -> Result<Vec<String>, ApiError> {
    let names: Vec<&str> = hosts.split(',').map(str::trim).collect();
    if names.len() > SEARCH_HOSTS_MAX || names.iter().any(|n| n.is_empty() || n.len() > 256) {
        return Err(ApiError::bad(format!(
            "host takes 1–{SEARCH_HOSTS_MAX} hosts, comma-separated, each at most 256 bytes"
        )));
    }
    let mut known: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        if !known.iter().any(|k| k == name) {
            known.push(name.to_owned());
        }
    }
    Ok(known)
}
/// The most types a search's `type` takes.
pub const SEARCH_TYPES_MAX: usize = 10;
/// The most types a search's `exclude_type` takes: enough for a client to leave out every type its
/// own groups of types stand for.
pub const SEARCH_EXCLUDED_TYPES_MAX: usize = 50;
/// The types a search's `param` (`type` or `exclude_type`) names (1 to `max`, comma-separated),
/// each as its vocabulary spells it.
fn search_types(param: &str, types: &str, max: usize) -> Result<Vec<String>, ApiError> {
    let names: Vec<&str> = types.split(',').map(str::trim).collect();
    if names.len() > max || names.iter().any(|n| n.is_empty()) {
        return Err(ApiError::bad(format!(
            "{param} takes 1–{max} schema.org types, comma-separated"
        )));
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
                format!("{param} names schema.org types (letters and digits, comma-separated)")
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
        search_types("type", types, SEARCH_TYPES_MAX)?;
    }
    if let Some(types) = &q.exclude_type {
        search_types("exclude_type", types, SEARCH_EXCLUDED_TYPES_MAX)?;
    }
    if let Some(hosts) = &q.host {
        search_hosts(hosts)?;
    }
    for text in [&q.q, &q.category].into_iter().flatten() {
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
/// A text search as the text index reads it: its text, the order its candidates are kept in
/// (the page's own ranking), and the filters the index holds. Postgres applies every filter again
/// and those the index does not hold (`key`, `remote`, `closed`, `ingredient`, exact distances).
fn text_query(
    q: &SearchQuery,
    text: &str,
    tags: &[String],
    spatial: &crate::spatial::Filters,
    at: DateTime<Utc>,
    sort: Option<&str>,
) -> Result<crate::search_index::TextQuery, ApiError> {
    use crate::search_index::TextOrder;
    let list = |types: &Option<String>, name: &str, max: usize| -> Result<Vec<String>, ApiError> {
        types
            .as_deref()
            .map(|t| search_types(name, t, max))
            .transpose()
            .map(Option::unwrap_or_default)
    };
    let trending = sort == Some("trending");
    Ok(crate::search_index::TextQuery {
        text: text.to_owned(),
        order: match sort {
            Some("activity") => Some(TextOrder::Activity),
            Some("recent") | Some("trending") => Some(TextOrder::Recent),
            Some("start") => Some(TextOrder::Start),
            Some("distance") => spatial
                .near
                .map(|(lat, lon, _)| TextOrder::Distance(lat, lon)),
            _ => Some(TextOrder::Relevance),
        },
        at,
        types: list(&q.type_name, "type", SEARCH_TYPES_MAX)?,
        exclude_types: list(&q.exclude_type, "exclude_type", SEARCH_EXCLUDED_TYPES_MAX)?,
        tags: tags.to_vec(),
        all_tags: q.match_mode.as_deref() == Some("all"),
        hosts: q
            .host
            .as_deref()
            .map(search_hosts)
            .transpose()?
            .unwrap_or_default(),
        category: q.category.clone(),
        venue: q.venue.clone(),
        from: spatial.from,
        to: spatial.to,
        started: sort == Some("start"),
        near: spatial.near,
        boxes: spatial.bbox.as_ref().map(|b| b.split()).unwrap_or_default(),
        // A trending row has a reference inside the window.
        referenced_since: if trending {
            Some(at - trend_window(q.window.as_deref())?)
        } else {
            None
        },
        referenced: q.referenced == Some(true),
        // As Postgres' `ILIKE` match: a short text of letters and digits only at word starts.
        inside_words: word_start(text).is_none(),
        open_jobs_at: (q.closed != Some(true)).then_some(at),
        catalogues: vec![],
        labels: false,
    })
}
/// `{unit}` has a reference that counts, in the author and reach scope.
fn push_referenced(
    sql: &mut QueryBuilder<'_, Postgres>,
    unit: &str,
    q: &SearchQuery,
    reach: &Option<crate::nexus::ReachSet>,
) {
    sql.push(format!(
        " AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id={unit}.id"
    ));
    scope(sql, q, reach);
    sql.push(")");
}
/// `{unit}` carries the tags (any, or all with `match=all`), in the author and reach scope.
fn push_tags(
    sql: &mut QueryBuilder<'_, Postgres>,
    unit: &str,
    q: &SearchQuery,
    reach: &Option<crate::nexus::ReachSet>,
    tags: &[String],
) {
    if tags.is_empty() {
        return;
    }
    if q.match_mode.as_deref() == Some("all") {
        for tag in tags {
            sql.push(format!(
                " AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id={unit}.id AND f.label="
            ))
            .push_bind(tag.clone());
            scope(sql, q, reach);
            sql.push(")");
        }
    } else {
        sql.push(format!(
            " AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id={unit}.id AND f.label=ANY("
        ))
        .push_bind(tags.to_vec())
        .push(")");
        scope(sql, q, reach);
        sql.push(")");
    }
}
/// [`push_tags`], where the labels a card built from a catalogue card of `labelled` shows count as
/// its tags too (none: exactly [`push_tags`]). Each condition is one set of IDs, so Postgres reads it
/// as it reads people's tags alone: the links tagged so (in scope), and the few built from a card
/// with such a label (`resource_catalogue_card_idx`).
fn push_tags_or_labels(
    sql: &mut QueryBuilder<'_, Postgres>,
    unit: &str,
    q: &SearchQuery,
    reach: &Option<crate::nexus::ReachSet>,
    tags: &[String],
    labelled: &[String],
) {
    if tags.is_empty() {
        return;
    }
    if labelled.is_empty() {
        push_tags(sql, unit, q, reach, tags);
        return;
    }
    let one = |sql: &mut QueryBuilder<'_, Postgres>, value: Vec<String>, any: bool| {
        sql.push(format!(
            " AND {unit}.id IN (SELECT f.resource_id FROM effective_reference f WHERE f.label=ANY("
        ))
        .push_bind(value.clone())
        .push(")");
        scope(sql, q, reach);
        sql.push(" UNION SELECT x.id FROM resource x WHERE x.catalogue_card IS NOT NULL AND x.doc->'catalogue'->>'catalogue'=ANY(")
            .push_bind(labelled.to_vec())
            .push(") AND x.doc->'labels'")
            .push(if any { " ?| " } else { " ?& " })
            .push_bind(value)
            .push("::text[])");
    };
    if q.match_mode.as_deref() == Some("all") {
        for tag in tags {
            one(sql, vec![tag.clone()], false);
        }
    } else {
        one(sql, tags.to_vec(), true);
    }
}
/// Whether a search may list catalogue cards nobody referenced: not with `referenced=true`, a
/// social filter, an identifier, a venue, a category, a time, a job posting's or a recipe's facts,
/// nor ranked by references or a start (`recent`, `trending`, `start`), which such a card has
/// none of.
fn catalogue_cards_allowed(q: &SearchQuery, sort: Option<&str>) -> bool {
    q.referenced != Some(true)
        && q.reach.is_none()
        && q.author.is_none()
        && q.related.is_none()
        && q.in_collection.is_none()
        && q.key.is_none()
        && q.venue.is_none()
        && q.category.is_none()
        && q.remote.is_none()
        && q.ingredient.is_none()
        && q.from.is_none()
        && q.to.is_none()
        && !matches!(sort, Some("recent" | "trending" | "start"))
}
/// A catalogue card as a search unit: its card, its unit ID (the resource ID of its link), its
/// type path, its host, and (from the text index) its relevance and whether its words matched.
struct CardUnit {
    card: i64,
    unit_id: String,
    type_path: String,
    /// The host it is listed under ([`crate::catalogue::card_host`]).
    host: Option<String>,
    /// The web page of a root that is no web link ([`crate::catalogue::root_page`]).
    page: Option<String>,
    /// The units that stand for this card when the search lists them too (`hidden_by`): the
    /// referenced cards built from it or from another catalogue's card sharing its keys, and the
    /// card it is merged into. The page leaves the card out only when one of them is listed.
    hidden_by: Vec<String>,
}
/// The resources of the referenced candidates.
fn candidate_resources(units: &[crate::search_index::Candidate]) -> Vec<String> {
    let mut resources: Vec<String> = units
        .iter()
        .filter(|c| c.card.is_none())
        .map(|c| c.resource_id.clone())
        .collect();
    resources.sort();
    resources.dedup();
    resources
}
/// A catalogue card as [`card_units`] and [`best_known_cards`] read it: ID, link, type,
/// catalogue and keys.
type CardRow = (i64, String, String, String, Vec<String>);
/// The catalogue cards a search may list, each with the units that stand for it (`hidden_by`), and
/// the cards merged into another (`merged`: the card, the card that is the main one).
#[derive(Default)]
struct CardUnits {
    units: Vec<CardUnit>,
    merged: std::collections::HashMap<i64, i64>,
}
/// For catalogue cards `$1`, the referenced cards built from a catalogue card
/// (`resource.catalogue_card`) that stand for each: built from it, or from a card of another
/// catalogue that is on (`$2`) sharing one of its keys (`resource_key` from the catalogue; a card
/// of the same catalogue is another thing there) that names one card in each of the two catalogues
/// (two recordings of one ISRC are two things, and a Wikidata item holding that ISRC stands for
/// neither). Their unit IDs are their IDs. Whether one stands for the card on a page is decided
/// there: only a resource listed on it (a unit of its own, with a reference that counts, through
/// every filter) leaves the card out ([`not_hidden`]).
const REPLACERS: &str = "SELECT c.id,x.id FROM catalogue_card c JOIN resource x ON x.catalogue_card=c.id WHERE c.id=ANY($1) UNION SELECT c.id,x.id FROM catalogue_card c JOIN resource_key k ON k.source='catalogue' AND k.key=ANY(c.keys) JOIN resource x ON x.id=k.resource_id JOIN catalogue_card y ON y.id=x.catalogue_card WHERE c.id=ANY($1) AND y.catalogue<>c.catalogue AND y.catalogue=ANY($2) AND y.keys@>ARRAY[k.key] AND NOT EXISTS(SELECT 1 FROM catalogue_card o WHERE o.keys@>ARRAY[k.key] AND o.catalogue IN (c.catalogue,y.catalogue) AND o.id NOT IN (c.id,y.id))";
/// `units` with the resources that stand for each card ([`REPLACERS`]) added to its `hidden_by`.
async fn with_replacers(
    pool: &PgPool,
    catalogues: &[String],
    mut units: CardUnits,
    deadline: Option<std::time::Instant>,
) -> Result<CardUnits, ApiError> {
    if units.units.is_empty() {
        return Ok(units);
    }
    let ids: Vec<i64> = units.units.iter().map(|u| u.card).collect();
    let joinable: Vec<String> = catalogues
        .iter()
        .filter(|c| c.as_str() != crate::catalogue::Catalogue::Osm.name())
        .cloned()
        .collect();
    let mut tx = begin_read(pool, deadline).await?;
    let pairs: Vec<(i64, String)> = sqlx::query_as(REPLACERS)
        .bind(&ids)
        .bind(&joinable)
        .fetch_all(&mut *tx)
        .await
        .map_err(card_read_error)?;
    tx.commit().await?;
    for (card, resource) in pairs {
        // The row of the card's own link is the card's own unit (`card_unit`): where it is listed
        // the page lists it once already (`DISTINCT ON (unit_id)`, the resource first), and where
        // it is not (resolved but never shared, its last tag removed, an alias) the card is the row.
        if let Some(unit) = units
            .units
            .iter_mut()
            .find(|u| u.card == card && u.unit_id != resource)
        {
            unit.hidden_by.push(resource);
        }
    }
    Ok(units)
}
/// The keys of `rows` (not `url:` keys) that would merge two of them (held by cards of two
/// catalogues among the rows) but name more than one card of one of those catalogues (two
/// MusicBrainz recordings of one ISRC): each with that catalogue. Such a key merges nothing
/// ([`units_of`]). One read for the few keys two catalogues' found cards share, through the key
/// index, at most two cards per key and catalogue of the rows (which are on).
async fn ambiguous_keys(
    pool: &PgPool,
    rows: &[CardRow],
    deadline: Option<std::time::Instant>,
) -> Result<std::collections::HashSet<(String, String)>, ApiError> {
    let mut by_key: std::collections::HashMap<&str, std::collections::BTreeSet<&str>> =
        std::collections::HashMap::new();
    for (_, _, _, catalogue, keys) in rows {
        for key in keys.iter().filter(|k| !k.starts_with("url:")) {
            by_key.entry(key).or_default().insert(catalogue);
        }
    }
    // Each shared key with each catalogue among the rows that holds it (catalogues that are on).
    let (keys, catalogues): (Vec<String>, Vec<String>) = by_key
        .into_iter()
        .filter(|(_, catalogues)| catalogues.len() > 1)
        .flat_map(|(key, catalogues)| {
            catalogues
                .into_iter()
                .map(move |catalogue| (key.to_owned(), catalogue.to_owned()))
        })
        .unzip();
    if keys.is_empty() {
        return Ok(Default::default());
    }
    // Through the key index (the bitmap of a key's cards), at most two cards of the catalogue per
    // key: a key held once is one index entry however many cards the catalogue holds, and a key
    // many cards of the catalogue hold costs two of them. Another catalogue's check of that key
    // goes once through its holders (the key index does not tell catalogues apart).
    let ambiguous: Vec<(String, String)> = read_cards(pool, Plan::KeyIndex, deadline, |sql| {
        sql.push("SELECT p.k,p.catalogue FROM unnest(")
            .push_bind(keys.clone())
            .push("::text[],")
            .push_bind(catalogues.clone())
            .push("::text[]) AS p(k,catalogue) CROSS JOIN LATERAL (SELECT count(*) AS n FROM (SELECT 1 FROM catalogue_card c WHERE c.keys@>ARRAY[p.k] AND c.catalogue=p.catalogue LIMIT 2) two) m WHERE m.n>1");
    })
    .await?;
    Ok(ambiguous.into_iter().collect())
}
/// The cards `rows` names as units, in their order: a card that shares a key naming the thing (not
/// a `url:` key) with a card of a catalogue listed earlier in [`crate::catalogue::Catalogue::ALL`]
/// (Wikidata first) among `rows` is merged into it (it stays a unit, hidden when the card it is
/// merged into is listed, so a filter only the merged card passes still lists the thing), unless
/// the key names more than one card of either catalogue (`ambiguous`, [`ambiguous_keys`]): two
/// MusicBrainz recordings of one ISRC stay two rows beside the Wikidata item holding it. Cards of
/// one catalogue never merge (each is its own thing there), and a merge does not chain.
fn units_of(
    rows: Vec<CardRow>,
    ambiguous: &std::collections::HashSet<(String, String)>,
) -> CardUnits {
    let rank = |catalogue: &str| {
        crate::catalogue::Catalogue::ALL
            .iter()
            .position(|c| c.name() == catalogue)
            .unwrap_or(usize::MAX)
    };
    let one =
        |key: &str, catalogue: &str| !ambiguous.contains(&(key.to_owned(), catalogue.to_owned()));
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| rank(&rows[i].3));
    // Each key of a card kept, with the card and its catalogue.
    let mut held: std::collections::HashMap<&str, (i64, &str)> = std::collections::HashMap::new();
    let mut merged = std::collections::HashMap::new();
    for &i in &order {
        let (id, _, _, catalogue, keys) = &rows[i];
        let named = || keys.iter().filter(|k| !k.starts_with("url:"));
        if let Some(&(into, _)) = named()
            .filter(|k| one(k, catalogue))
            .filter_map(|k| held.get(k.as_str()))
            .find(|(_, other)| other != catalogue)
        {
            merged.insert(*id, into);
            continue;
        }
        for key in named().filter(|k| one(k, catalogue)) {
            held.entry(key.as_str())
                .or_insert((*id, catalogue.as_str()));
        }
    }
    let unit_of: std::collections::HashMap<i64, String> = rows
        .iter()
        .filter_map(|(id, uri, ..)| Some((*id, crate::search_index::card_unit(uri)?)))
        .collect();
    let units = rows
        .iter()
        .filter_map(|(card, uri, type_name, _, _)| {
            Some(CardUnit {
                card: *card,
                unit_id: unit_of.get(card)?.clone(),
                type_path: model::type_path(type_name).join(","),
                host: crate::catalogue::card_host(uri),
                page: crate::catalogue::root_page(uri).map(|(page, _)| page),
                hidden_by: merged
                    .get(card)
                    .and_then(|main| unit_of.get(main))
                    .cloned()
                    .into_iter()
                    .collect(),
            })
        })
        .collect();
    CardUnits { units, merged }
}
/// The catalogue cards among a text search's candidates, of a catalogue that is on.
async fn card_units(
    pool: &PgPool,
    catalogues: &[String],
    found: &[&crate::search_index::Candidate],
) -> Result<CardUnits, ApiError> {
    if found.is_empty() {
        return Ok(CardUnits::default());
    }
    let ids: Vec<i64> = found.iter().filter_map(|c| c.card).collect();
    let mut rows: Vec<CardRow> = sqlx::query_as(
        "SELECT c.id,c.uri,c.type,c.catalogue,c.keys FROM catalogue_card c WHERE c.id=ANY($1) AND c.catalogue=ANY($2)",
    )
    .bind(&ids)
    .bind(catalogues)
    .fetch_all(pool)
    .await?;
    // In the candidates' order: the best of a merged unit's cards gives its relevance.
    let at: std::collections::HashMap<i64, usize> =
        ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    rows.sort_by_key(|row| at.get(&row.0).copied().unwrap_or(usize::MAX));
    let ambiguous = ambiguous_keys(pool, &rows, None).await?;
    with_replacers(pool, catalogues, units_of(rows, &ambiguous), None).await
}
/// How many times `cap` of the best known cards a place search without text reads first, keeping
/// those in its circle or box ([`best_known_cards`]).
pub const FAMOUS_PROBE: usize = 2;
/// How many times `cap` cards nearest to its place a search without text reads when the best
/// known it read first hold fewer than `cap` in it ([`best_known_cards`]).
pub const NEAREST_CARDS: usize = 2;
/// The most branches (a catalogue's type or host) a search without text reads, each in its
/// index's order ([`best_known_cards`]).
const CARD_BRANCHES: usize = 64;
/// The widest box (degrees of longitude or latitude) a search without text with a type or host
/// filter reads the nearest cards of ([`best_known_cards`]): wider, it lists the best known cards
/// in it alone.
pub const NEAREST_FILTERED_DEGREES: f64 = 10.0;
thread_local! {
    /// Tests: the plans of the card reads without text this thread ran, while recorded.
    static CARD_READ_PLANS: std::cell::RefCell<Option<Vec<Value>>> = const { std::cell::RefCell::new(None) };
}
/// Tests: record the plan (`EXPLAIN (ANALYZE, FORMAT JSON)`) of each card read without text this
/// thread runs from now on, until [`card_read_plans`] takes them.
#[doc(hidden)]
pub fn record_card_read_plans() {
    CARD_READ_PLANS.with(|p| *p.borrow_mut() = Some(vec![]));
}
/// Tests: the plans recorded since [`record_card_read_plans`]; recording stops.
#[doc(hidden)]
pub fn card_read_plans() -> Vec<Value> {
    CARD_READ_PLANS.with(|p| p.borrow_mut().take().unwrap_or_default())
}
/// A card row read without text, with its `fame`.
type FamedCard = (i64, String, String, String, Vec<String>, f32);
/// The columns of a card read without text.
const CARD_READ_COLUMNS: &str = "c.id,c.uri,c.type,c.catalogue,c.keys,c.fame";
/// How a card read without text is planned ([`read_cards`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// In its indexes' order, as the planner chooses.
    Ordered,
    /// The nearest to a point in the `point` indexes' nearest-first order: the planner would rather
    /// read every card in the circle or box and sort them, which reads far more.
    Nearest,
    /// Through the key index's bitmap alone (no plain index or table scan): a bounded read per
    /// key that the planner would otherwise take in a btree's order, or across the table.
    KeyIndex,
}
/// The rows the statement `build` writes reads, in a transaction of its own planned as `plan`
/// says, never with parallel workers (they take longer to start than a bounded read takes); its
/// plan recorded first while a test asks ([`record_card_read_plans`]).
async fn read_cards<T>(
    pool: &PgPool,
    plan: Plan,
    deadline: Option<std::time::Instant>,
    build: impl Fn(&mut QueryBuilder<'static, Postgres>),
) -> Result<Vec<T>, ApiError>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    let mut tx = begin_read(pool, deadline).await?;
    // The settings in one statement, local to the transaction.
    sqlx::query(match plan {
        Plan::Ordered => "SELECT set_config('max_parallel_workers_per_gather','0',true)",
        Plan::Nearest => "SELECT set_config('max_parallel_workers_per_gather','0',true),set_config('enable_bitmapscan','off',true),set_config('enable_seqscan','off',true)",
        Plan::KeyIndex => "SELECT set_config('max_parallel_workers_per_gather','0',true),set_config('enable_indexscan','off',true),set_config('enable_seqscan','off',true)",
    })
    .execute(&mut *tx)
    .await?;
    if CARD_READ_PLANS.with(|p| p.borrow().is_some()) {
        let mut sql = QueryBuilder::<Postgres>::new("EXPLAIN (ANALYZE, FORMAT JSON) ");
        build(&mut sql);
        let plan: Value = sql
            .build_query_scalar()
            .fetch_one(&mut *tx)
            .await
            .map_err(card_read_error)?;
        CARD_READ_PLANS.with(|p| {
            if let Some(plans) = p.borrow_mut().as_mut() {
                plans.push(plan);
            }
        });
    }
    let mut sql = QueryBuilder::<Postgres>::new("");
    build(&mut sql);
    let rows = sql
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(card_read_error)?;
    tx.commit().await?;
    Ok(rows)
}
/// A transaction for a search's reads whose every statement stops at `deadline` (a
/// `statement_timeout` of what is left of the search's turn budget, at least 1 ms): a read held
/// up past it is cancelled in Postgres, not left running after the search answered. `None`: as
/// the pool's connections are set.
async fn begin_read(
    pool: &PgPool,
    deadline: Option<std::time::Instant>,
) -> Result<sqlx::Transaction<'static, Postgres>, ApiError> {
    let mut tx = pool.begin().await?;
    if let Some(deadline) = deadline {
        let left = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .max(1);
        sqlx::query("SELECT set_config('statement_timeout',$1,true)")
            .bind(format!("{left}ms"))
            .execute(&mut *tx)
            .await?;
    }
    Ok(tx)
}
/// The error of a read that ran past its search's turn budget ([`begin_read`]): the search
/// answers as a busy text search does, 503 with `Retry-After: 1` ([`text_search_busy`]).
fn card_read_busy() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "text search is busy, try again shortly".into(),
    )
}
/// `error` as a search read's error: cancelled by its `statement_timeout` it is
/// [`card_read_busy`] (counted as `budget`), else an internal error.
fn card_read_error(error: sqlx::Error) -> ApiError {
    match &error {
        sqlx::Error::Database(e) if e.code().as_deref() == Some("57014") => {
            crate::telemetry::search_index_busy("budget");
            card_read_busy()
        }
        _ => error.into(),
    }
}
/// One branch of a card read in fame order: a catalogue, and a column equal to a value in it.
type Branch = (String, Option<(&'static str, String)>);
/// `branches`, each read in fame order with `columns` and the conditions `inner` adds (on `c`),
/// merged in that order: at most `limit` rows in all, as `b`.
fn by_fame(
    sql: &mut QueryBuilder<'static, Postgres>,
    columns: &str,
    branches: &[Branch],
    inner: &dyn Fn(&mut QueryBuilder<'static, Postgres>),
    limit: usize,
) {
    each_by_fame(sql, columns, branches, inner, limit);
    sql.push(" ORDER BY b.fame DESC,b.id LIMIT ")
        .push_bind(limit as i64);
}
/// [`by_fame`] without the merged limit: each branch's `limit` rows, as `b`.
fn each_by_fame(
    sql: &mut QueryBuilder<'static, Postgres>,
    columns: &str,
    branches: &[Branch],
    inner: &dyn Fn(&mut QueryBuilder<'static, Postgres>),
    limit: usize,
) {
    sql.push("SELECT * FROM (");
    for (i, (catalogue, extra)) in branches.iter().enumerate() {
        if i > 0 {
            sql.push(" UNION ALL ");
        }
        sql.push("(SELECT ")
            .push(columns)
            .push(" FROM catalogue_card c WHERE c.catalogue=")
            .push_bind(catalogue.clone());
        if let Some((column, value)) = extra {
            sql.push(" AND ")
                .push(*column)
                .push("=")
                .push_bind(value.clone());
        }
        inner(sql);
        sql.push(" ORDER BY c.fame DESC,c.id LIMIT ")
            .push_bind(limit as i64)
            .push(")");
    }
    sql.push(") b");
}
/// Without text: the best known catalogue cards (by `fame`; by distance with `sort=distance`)
/// the filters let in, at most `cap`. Each read stops after a bounded number of rows, and reads
/// catalogue by catalogue, so the cards of a catalogue switched off but not dropped are never
/// read:
/// - With a type or excluded types: first each catalogue's types its cards have (one probe of the
///   `(catalogue, type, fame, id)` index per type, [`catalogue_types`]); a catalogue none of whose
///   types the filters let in is not read, and the others are read by the types let in.
/// - With `near`, `bbox` or `tags`: the [`FAMOUS_PROBE`] times `cap` best known cards (the
///   `(catalogue, fame, id)` index in order) and those of them through every filter kept, the
///   whole answer when they are `cap`. When they are fewer: with `tags`, also the best known of
///   the cards holding the labels, through the `labels` index (each label's cards, never the
///   catalogue in fame order: a rare label is a short read); else the [`NEAREST_CARDS`] times
///   `cap` cards nearest to the circle's or each box's centre in it (the `point` indexes),
///   unless a type or host filter is set and a box spans more than [`NEAREST_FILTERED_DEGREES`]
///   (the nearest read would pass every point of a wide box its filter turns away): then the
///   best known of each type (or host) let in, in its index's order, kept in the box; and the
///   best known of both. With `sort=distance` and `near`, the `cap` nearest.
/// - With hosts: each catalogue's host in the `(catalogue, catalogue_card_host(uri), fame, id)`
///   index's order.
/// - With types (or excluded types): each catalogue's type let in, in the `(catalogue, type,
///   fame, id)` index's order, while they are at most [`CARD_BRANCHES`].
/// - Else each catalogue in the `(catalogue, fame, id)` index's order.
///
/// The branches of one read are merged in their order, so it stops after `cap` rows. The nearest
/// reads go one branch per catalogue in its own `point` index (partial, `WHERE catalogue=…`).
///
/// With `inside` (a text of one character found inside names, which the text index leaves to
/// Postgres): only the [`FAMOUS_PROBE`] times `cap` best known cards of each catalogue are read
/// (each catalogue's own, not of all together), in fame order, and those whose name holds the
/// character kept (with every filter and the place); other names are not read, as a link's are
/// not.
///
/// Every read stops at `deadline` (the search's turn budget, [`begin_read`]): past it the search
/// answers 503 ([`card_read_busy`]).
#[allow(clippy::too_many_arguments)]
async fn best_known_cards(
    pool: &PgPool,
    q: &SearchQuery,
    tags: &[String],
    spatial: &crate::spatial::Filters,
    catalogues: &[String],
    sort: Option<&str>,
    cap: usize,
    inside: Option<&str>,
    deadline: Option<std::time::Instant>,
) -> Result<CardUnits, ApiError> {
    let types = q
        .type_name
        .as_deref()
        .map(|t| search_types("type", t, SEARCH_TYPES_MAX).map(|t| model::subtypes(&t)))
        .transpose()?;
    let excluded = q
        .exclude_type
        .as_deref()
        .map(|t| {
            search_types("exclude_type", t, SEARCH_EXCLUDED_TYPES_MAX).map(|t| model::subtypes(&t))
        })
        .transpose()?;
    let hosts = q.host.as_deref().map(search_hosts).transpose()?;
    let all_labels = q.match_mode.as_deref() == Some("all");
    // With a type filter or excluded types, each catalogue's types the filters let in, of those
    // its cards have: a catalogue left with none is not read (`exclude_type=Thing` reads no card),
    // and the others by those types. `None` for a catalogue of more types than are probed: read
    // with the filters as they are.
    let mut let_in: Vec<(String, Option<Vec<String>>)> = vec![];
    if types.is_some() || excluded.is_some() {
        let wanted: Option<std::collections::HashSet<&String>> =
            types.as_ref().map(|t| t.iter().collect());
        let unwanted: std::collections::HashSet<&String> = excluded.iter().flatten().collect();
        for catalogue in catalogues {
            match catalogue_types(pool, catalogue, deadline).await? {
                Some(present) => {
                    let kept: Vec<String> = present
                        .into_iter()
                        .filter(|t| wanted.as_ref().is_none_or(|w| w.contains(t)))
                        .filter(|t| !unwanted.contains(t))
                        .collect();
                    if !kept.is_empty() {
                        let_in.push((catalogue.clone(), Some(kept)));
                    }
                }
                None => let_in.push((catalogue.clone(), None)),
            }
        }
    } else {
        let_in = catalogues.iter().map(|c| (c.clone(), None)).collect();
    }
    if let_in.is_empty() {
        return Ok(CardUnits::default());
    }
    // The catalogues read: those whose cards the filters may let in.
    let read_from: Vec<String> = let_in.iter().map(|(c, _)| c.clone()).collect();
    // Every filter but the place and the catalogue, on `c`: the types unless `one_type` (each
    // branch's own), the hosts unless `one_host`.
    let filters = |sql: &mut QueryBuilder<'static, Postgres>, one_type: bool, one_host: bool| {
        if let Some(types) = types.as_ref().filter(|_| !one_type) {
            sql.push(" AND c.type=ANY(")
                .push_bind(types.clone())
                .push(")");
        }
        if let Some(types) = &excluded {
            sql.push(" AND NOT c.type=ANY(")
                .push_bind(types.clone())
                .push(")");
        }
        if !tags.is_empty() {
            sql.push(if all_labels {
                " AND c.labels@>"
            } else {
                " AND c.labels&&"
            })
            .push_bind(tags.to_vec());
        }
        if let Some(hosts) = hosts.as_ref().filter(|_| !one_host) {
            sql.push(" AND catalogue_card_host(c.uri)=ANY(")
                .push_bind(hosts.clone())
                .push(")");
        }
    };
    let circle = |sql: &mut QueryBuilder<'static, Postgres>,
                  (lat, lon, radius): (f64, f64, f64)| {
        sql.push(" AND ST_DWithin(c.point::geography,ST_SetSRID(ST_MakePoint(")
            .push_bind(lon)
            .push(",")
            .push_bind(lat)
            .push("),4326)::geography,")
            .push_bind(radius)
            .push(")");
    };
    // The place, on `c`.
    let place = |sql: &mut QueryBuilder<'static, Postgres>| {
        if let Some(near) = spatial.near {
            circle(sql, near);
        }
        if let Some(bbox) = &spatial.bbox {
            push_boxes(sql, &bbox.split());
        }
    };
    // Each catalogue's name as a literal: the planner matches it to that catalogue's partial
    // `point` indexes, which a bound value (a generic plan) would not.
    let literals: Vec<String> = read_from
        .iter()
        .filter_map(|c| crate::catalogue::Catalogue::parse(c))
        .map(|c| format!("'{}'", c.name()))
        .collect();
    // One branch per catalogue (and box), each its `limit` nearest to the centre in its own index.
    let nearest_branches =
        |sql: &mut QueryBuilder<'static, Postgres>, columns: &str, limit: i64| {
            let mut first = true;
            for literal in &literals {
                let boxes: Vec<Option<[f64; 4]>> = match (&spatial.near, &spatial.bbox) {
                    (None, Some(bbox)) => bbox.split().into_iter().map(Some).collect(),
                    _ => vec![None],
                };
                for b in boxes {
                    if !first {
                        sql.push(" UNION ALL ");
                    }
                    first = false;
                    sql.push("(SELECT ")
                        .push(columns)
                        .push(" FROM catalogue_card c WHERE c.catalogue=")
                        .push(literal.as_str());
                    filters(sql, false, false);
                    match (spatial.near, b) {
                        (Some(near), _) => {
                            circle(sql, near);
                            if let Some(bbox) = &spatial.bbox {
                                push_boxes(sql, &bbox.split());
                            }
                            sql.push(" ORDER BY c.point::geography<->ST_SetSRID(ST_MakePoint(")
                                .push_bind(near.1)
                                .push(",")
                                .push_bind(near.0)
                                .push("),4326)::geography");
                        }
                        (None, Some(b)) => {
                            push_boxes(sql, std::slice::from_ref(&b));
                            sql.push(" ORDER BY c.point<->ST_SetSRID(ST_MakePoint(")
                                .push_bind((b[0] + b[2]) / 2.0)
                                .push(",")
                                .push_bind((b[1] + b[3]) / 2.0)
                                .push("),4326)");
                        }
                        (None, None) => {}
                    }
                    sql.push(" LIMIT ").push_bind(limit).push(")");
                }
            }
        };
    let each: Vec<Branch> = read_from.iter().map(|c| (c.clone(), None)).collect();
    // Each catalogue's type let in, while they are few enough to read one by one.
    let typed: Option<Vec<Branch>> = (types.is_some() || excluded.is_some())
        .then(|| {
            let mut branches = vec![];
            for (catalogue, kept) in &let_in {
                let types = kept.clone().or_else(|| types.clone())?;
                branches.extend(
                    types
                        .into_iter()
                        .map(|t| (catalogue.clone(), Some(("c.type", t)))),
                );
            }
            Some(branches)
        })
        .flatten()
        .filter(|b| b.len() <= CARD_BRANCHES);
    // Each catalogue's host, each its own branch (at most 10 hosts of 7 catalogues).
    let host_branches: Option<Vec<Branch>> = hosts.as_ref().map(|hosts| {
        read_from
            .iter()
            .flat_map(|c| {
                hosts
                    .iter()
                    .map(move |h| (c.clone(), Some(("catalogue_card_host(c.uri)", h.clone()))))
            })
            .collect()
    });
    let rows: Vec<FamedCard> =
        if let (Some("distance"), Some(near), None) = (sort, spatial.near, inside) {
            // The nearest in the circle, each catalogue's in its own geography index, then in that
            // order with the card ID for equal distances.
            read_cards(pool, Plan::Nearest, deadline, |sql| {
                sql.push("SELECT c.id,c.uri,c.type,c.catalogue,c.keys,c.fame FROM (");
                nearest_branches(
                    sql,
                    "c.id,c.uri,c.type,c.catalogue,c.keys,c.fame,c.point",
                    cap as i64,
                );
                sql.push(") c ORDER BY c.point::geography<->ST_SetSRID(ST_MakePoint(")
                    .push_bind(near.1)
                    .push(",")
                    .push_bind(near.0)
                    .push("),4326)::geography,c.id LIMIT ")
                    .push_bind(cap as i64);
            })
            .await?
        } else if spatial.near.is_some()
            || spatial.bbox.is_some()
            || inside.is_some()
            || !tags.is_empty()
        {
            // The best known first, kept when in the place and through every filter.
            let mut rows: Vec<FamedCard> = read_cards(pool, Plan::Ordered, deadline, |sql| {
                sql.push("SELECT ").push(CARD_READ_COLUMNS).push(" FROM (");
                // A place takes the best known of all catalogues together; one character each
                // catalogue's own, so a catalogue of lesser known cards still lists its matches.
                let probe = if inside.is_some() {
                    each_by_fame
                } else {
                    by_fame
                };
                probe(
                    sql,
                    "c.id,c.uri,c.type,c.catalogue,c.keys,c.fame,c.point,c.labels,c.name",
                    &each,
                    &|_| {},
                    FAMOUS_PROBE * cap,
                );
                sql.push(") c WHERE true");
                filters(sql, false, false);
                if let Some(text) = inside {
                    sql.push(" AND c.name ILIKE ")
                        .push_bind(like_anywhere(text));
                }
                place(sql);
                sql.push(" ORDER BY c.fame DESC,c.id LIMIT ")
                    .push_bind(cap as i64);
            })
            .await?;
            let filtered = types.is_some() || excluded.is_some() || hosts.is_some();
            let wide = spatial.bbox.as_ref().is_some_and(|bbox| {
                bbox.split().iter().any(|b| {
                    b[2] - b[0] > NEAREST_FILTERED_DEGREES || b[3] - b[1] > NEAREST_FILTERED_DEGREES
                })
            });
            if rows.len() < cap && inside.is_none() {
                let more: Vec<FamedCard> = if !tags.is_empty() {
                    // Fewer: the best known of the cards holding the labels, through the labels index
                    // alone (a rare label's few cards), in the place.
                    read_cards(pool, Plan::KeyIndex, deadline, |sql| {
                        sql.push("SELECT ")
                            .push(CARD_READ_COLUMNS)
                            .push(" FROM catalogue_card c WHERE c.catalogue=ANY(")
                            .push_bind(read_from.clone())
                            .push(")");
                        filters(sql, false, false);
                        place(sql);
                        sql.push(" ORDER BY c.fame DESC,c.id LIMIT ")
                            .push_bind(cap as i64);
                    })
                    .await?
                } else if !(filtered && wide) {
                    // Fewer: the nearest to the circle's centre, or to each box's, in it too (in the
                    // index's nearest-first order: at equal distances at the end, either may be read).
                    let nearest = (NEAREST_CARDS * cap) as i64;
                    read_cards(pool, Plan::Nearest, deadline, |sql| {
                        sql.push("SELECT * FROM (");
                        nearest_branches(sql, CARD_READ_COLUMNS, nearest);
                        sql.push(") b");
                    })
                    .await?
                } else if let Some(branches) = typed.as_ref().or(host_branches.as_ref()) {
                    // A wide box with a type or host filter: each catalogue's type (or host) let in,
                    // in its index's order, kept in the box.
                    read_cards(pool, Plan::Ordered, deadline, |sql| {
                        by_fame(
                            sql,
                            CARD_READ_COLUMNS,
                            branches,
                            &|sql| {
                                filters(sql, false, false);
                                place(sql);
                            },
                            cap,
                        )
                    })
                    .await?
                } else {
                    vec![]
                };
                rows.extend(more);
                rows.sort_by(|a, b| b.5.total_cmp(&a.5).then(a.0.cmp(&b.0)));
                rows.dedup_by_key(|r| r.0);
                rows.truncate(cap);
            }
            rows
        } else if let Some(branches) = &host_branches {
            read_cards(pool, Plan::Ordered, deadline, |sql| {
                by_fame(
                    sql,
                    CARD_READ_COLUMNS,
                    branches,
                    &|sql| filters(sql, false, true),
                    cap,
                )
            })
            .await?
        } else if let Some(branches) = &typed {
            read_cards(pool, Plan::Ordered, deadline, |sql| {
                by_fame(
                    sql,
                    CARD_READ_COLUMNS,
                    branches,
                    &|sql| filters(sql, true, false),
                    cap,
                )
            })
            .await?
        } else {
            read_cards(pool, Plan::Ordered, deadline, |sql| {
                by_fame(
                    sql,
                    CARD_READ_COLUMNS,
                    &each,
                    &|sql| filters(sql, false, false),
                    cap,
                )
            })
            .await?
        };
    let rows: Vec<CardRow> = rows
        .into_iter()
        .map(|(id, uri, type_name, catalogue, keys, _)| (id, uri, type_name, catalogue, keys))
        .collect();
    let ambiguous = ambiguous_keys(pool, &rows, deadline).await?;
    with_replacers(pool, catalogues, units_of(rows, &ambiguous), deadline).await
}
/// The most types of one catalogue [`catalogue_types`] reads.
const CATALOGUE_TYPES_MAX: i64 = 256;
/// The types `catalogue`'s cards have, one probe of the `(catalogue, type, fame, id)` index per
/// type (each the next type after the last): `None` when it has more than
/// [`CATALOGUE_TYPES_MAX`].
async fn catalogue_types(
    pool: &PgPool,
    catalogue: &str,
    deadline: Option<std::time::Instant>,
) -> Result<Option<Vec<String>>, ApiError> {
    let catalogue = catalogue.to_owned();
    let types: Vec<(String,)> = read_cards(pool, Plan::Ordered, deadline, |sql| {
        sql.push("WITH RECURSIVE t(type) AS ((SELECT c.type FROM catalogue_card c WHERE c.catalogue=")
            .push_bind(catalogue.clone())
            .push(" ORDER BY c.type LIMIT 1) UNION ALL SELECT (SELECT c.type FROM catalogue_card c WHERE c.catalogue=")
            .push_bind(catalogue.clone())
            .push(" AND c.type>t.type ORDER BY c.type LIMIT 1) FROM t WHERE t.type IS NOT NULL) SELECT type FROM t WHERE type IS NOT NULL LIMIT ")
            .push_bind(CATALOGUE_TYPES_MAX + 1);
    })
    .await?;
    Ok((types.len() as i64 <= CATALOGUE_TYPES_MAX)
        .then(|| types.into_iter().map(|(t,)| t).collect()))
}
/// `AND` any of the boxes (west, south, east, north) holds `c.point`.
fn push_boxes(sql: &mut QueryBuilder<'_, Postgres>, boxes: &[[f64; 4]]) {
    sql.push(" AND (");
    for (i, b) in boxes.iter().enumerate() {
        if i > 0 {
            sql.push(" OR ");
        }
        sql.push("c.point && ST_MakeEnvelope(");
        for (j, n) in b.iter().enumerate() {
            if j > 0 {
                sql.push(",");
            }
            sql.push_bind(*n);
        }
        sql.push(",4326)");
    }
    sql.push(")");
}
/// `card_unit`: the catalogue cards `units` as rows of `search_unit`'s columns, with their
/// `labels` and `fame` (a referenced unit has `'{}'` and NULL), of a catalogue that is on. Its
/// document is what a card reads: type, name, description, link, place, its source and, for
/// OpenStreetMap, the ODbL attribution.
fn push_card_units(
    sql: &mut QueryBuilder<'_, Postgres>,
    units: &[CardUnit],
    catalogues: &[String],
) {
    let template = serde_json::to_value(ResourceMeta {
        status: model::Status::Ok,
        ..Default::default()
    })
    .expect("a document");
    sql.push("card_unit AS (SELECT x.unit_id AS id,c.uri,x.host,c.type,string_to_array(x.path,',') AS type_path,c.name,NULL::text AS category,c.point,NULL::timestamptz AS start_at,NULL::timestamptz AS end_at,NULL::text AS venue_id,")
        .push_bind(template)
        .push("::jsonb||jsonb_build_object('type',c.type,'type_path',to_jsonb(string_to_array(x.path,',')),'name',c.name,'description',c.description,'url',c.uri,'source',c.source,'status','ok')||CASE WHEN x.page IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('aliases',jsonb_build_array(x.page),'open_in',jsonb_build_array(jsonb_build_object('client',x.host,'url',x.page))) END||CASE WHEN c.point IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('location',jsonb_build_object('lat',ST_Y(c.point),'lon',ST_X(c.point))) END||CASE WHEN c.catalogue='osm' THEN jsonb_build_object('attribution','© OpenStreetMap contributors (ODbL)') ELSE '{}'::jsonb END AS doc,to_tsvector('simple',c.name||' '||coalesce(c.description,'')) AS search_text,NULL::text AS resolver,c.updated_at AS resolved_at,NULL::timestamptz AS refresh_at,'ok'::text AS status,NULL::text AS osm_type,NULL::bigint AS osm_id,x.unit_id,NULL::jsonb AS occurrence,c.labels,c.fame,string_to_array(x.hidden,',') AS hidden_by FROM unnest(")
        .push_bind(units.iter().map(|u| u.card).collect::<Vec<_>>())
        .push("::bigint[],")
        .push_bind(units.iter().map(|u| u.unit_id.clone()).collect::<Vec<_>>())
        .push("::text[],")
        .push_bind(units.iter().map(|u| u.type_path.clone()).collect::<Vec<_>>())
        .push("::text[],")
        .push_bind(units.iter().map(|u| u.host.clone()).collect::<Vec<_>>())
        .push("::text[],")
        .push_bind(units.iter().map(|u| u.page.clone()).collect::<Vec<_>>())
        .push("::text[],")
        .push_bind(
            units
                .iter()
                .map(|u| (!u.hidden_by.is_empty()).then(|| u.hidden_by.join(",")))
                .collect::<Vec<_>>(),
        )
        .push("::text[]) AS x(card,unit_id,path,host,page,hidden) JOIN catalogue_card c ON c.id=x.card WHERE c.catalogue=ANY(")
        .push_bind(catalogues.to_vec())
        .push(")), ");
}
/// The rows of `set` but the catalogue cards one of whose `hidden_by` units is in `present` (the
/// rows that passed every filter): the units that stand for them are listed instead. The units
/// named by any card are read once, then each card checked against those present.
fn not_hidden(set: &str, present: &str) -> String {
    format!(
        "SELECT * FROM {set} x WHERE x.hidden_by IS NULL OR NOT x.hidden_by && ARRAY(SELECT h.unit FROM (SELECT DISTINCT unnest(y.hidden_by) AS unit FROM {set} y WHERE y.hidden_by IS NOT NULL) h WHERE h.unit IN (SELECT z.unit_id FROM {present} z))"
    )
}
/// `read`, within a held text-search turn's budget (`deadline`; none: as it is): `None` when the
/// budget ran out first (counted as `budget`). Every read a search makes while it holds a turn
/// goes through here and runs under a `statement_timeout` of what is left ([`begin_read`]), so
/// the turn is held no longer and the statement does not run on after the search answered.
async fn within_turn<T>(
    deadline: Option<std::time::Instant>,
    read: impl std::future::Future<Output = T>,
) -> Option<T> {
    match deadline {
        Some(deadline) => match tokio::time::timeout_at(deadline.into(), read).await {
            Ok(done) => Some(done),
            Err(_) => {
                crate::telemetry::search_index_busy("budget");
                None
            }
        },
        None => Some(read.await),
    }
}
/// The answer to a text search the text index could not serve in time: `503` with
/// `Retry-After: 1` (a turn is likely free a second later).
fn text_search_busy() -> Response {
    let mut response = ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "text search is busy, try again shortly".into(),
    )
    .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
    response
}
/// How many times `limits.search_text_candidates` a trending text search reads from the index.
pub const TRENDING_CANDIDATES: usize = 20;
/// The most candidates a trending text search reads, whatever the limit.
pub const TRENDING_MOST: usize = 10_000;
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
/// somewhere in it (`ILIKE`, which the name's trigram index serves for a word of three letters)
/// and the words, in order and apart only by white space, where a word starts: the lowered name
/// (lowered as `ILIKE` lowers it) matches [`word_start_regex`] followed by `words` lowered the
/// same way.
struct WordStart {
    patterns: Vec<String>,
    words: String,
}
/// Where a word starts, for [`WordStart`]: the start of the name or a character that is neither
/// a letter, a digit nor a combining mark ([`word_char`]: white space, punctuation, symbols such
/// as `™` or `・`, emoji), so `/`, `.` and `-` start words (`lightningnetwork/lnd`). The letters
/// and digits are Rust's Unicode tables, the same as [`word_start`]'s own test, listed by code
/// point (never through the database's locale: `[[:alnum:]]` under the C locale would take `É`
/// for a separator). Built once.
fn word_start_regex() -> &'static str {
    static REGEX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| {
        let escape = |c: char| match u32::from(c) {
            n @ 0..=0xFFFF => format!("\\u{n:04x}"),
            n => format!("\\U{n:08x}"),
        };
        let mut class = String::new();
        let mut run: Option<(char, char)> = None;
        let close = |run: (char, char), class: &mut String| {
            class.push_str(&escape(run.0));
            if run.1 != run.0 {
                class.push('-');
                class.push_str(&escape(run.1));
            }
        };
        for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
            match (word_char(c), run) {
                (true, Some((first, last))) if u32::from(last) + 1 == u32::from(c) => {
                    run = Some((first, c))
                }
                (true, _) => {
                    if let Some(done) = run.replace((c, c)) {
                        close(done, &mut class);
                    }
                }
                (false, _) => {
                    if let Some(done) = run.take() {
                        close(done, &mut class);
                    }
                }
            }
        }
        if let Some(done) = run {
            close(done, &mut class);
        }
        format!("(^|[^{class}])")
    })
}
/// A character inside a word, for [`word_start_regex`], outside the scripts written without
/// spaces ([`unspaced_script`]): a letter or digit (`char::is_alphanumeric`, which counts most
/// vowel signs as letters), or a combining mark or joiner a word is written with (combining
/// diacritics, so an accent written apart, Hebrew points, Arabic vowel marks, the viramas and
/// other marks of the Indic scripts, ZWNJ and ZWJ).
pub(crate) fn word_char(c: char) -> bool {
    // A script written without spaces has no word starts of its own: a Latin word right after
    // one starts there (`AI` in `我的AI助手`).
    !unspaced_script(c)
        && (c.is_alphanumeric()
            || matches!(
            u32::from(c),
            0x0300..=0x036F // combining diacritical marks
                | 0x0483..=0x0489 // Cyrillic combining marks
                | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7 // Hebrew points
                | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 // Arabic marks
                | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
                | 0x0900..=0x0963 | 0x0966..=0x0DFF // Indic scripts, but the dandas
                | 0x1AB0..=0x1AFF // combining diacritical marks extended
                | 0x1DC0..=0x1DFF // combining diacritical marks supplement
                | 0x200C..=0x200D // ZWNJ, ZWJ
                | 0x20D0..=0x20FF // combining marks for symbols
                | 0xFE20..=0xFE2F // combining half marks
            ))
}
/// The [`WordStart`] match of `text`. `None` for longer text, for text with other signs (`c++`
/// and `c#` stay as precise as a match inside the name), for a script written without spaces
/// between words (Chinese, Japanese, Thai, ...), where only a match inside a run finds a word,
/// and for Hebrew and Arabic, which write prefixes onto words ([`prefixing_script`]).
fn word_start(text: &str) -> Option<WordStart> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let chars =
        words.iter().map(|w| w.chars().count()).sum::<usize>() + words.len().saturating_sub(1);
    let plain = |c: char| c.is_alphanumeric() && !unspaced_script(c) && !prefixing_script(c);
    if words.is_empty() || chars > SHORT_QUERY_CHARS || !words.iter().all(|w| w.chars().all(plain))
    {
        return None;
    }
    // Letters and digits only: nothing to escape in either pattern.
    Some(WordStart {
        patterns: words.iter().map(|w| format!("%{w}%")).collect(),
        words: words.join("\\s+"),
    })
}
/// `r` matches `text` (Postgres' text search): its words in the name or description
/// (`websearch_to_tsquery`), or for a short text a name word that starts so ([`word_start`]),
/// else the text inside the name (`ILIKE`) or a name similar to it (`pg_trgm`'s `%`).
fn push_text_match(sql: &mut QueryBuilder<'_, Postgres>, text: &str) {
    sql.push("(r.search_text @@ websearch_to_tsquery('simple',")
        .push_bind(text.to_owned())
        .push(") OR ");
    match word_start(text) {
        Some(starts) => push_word_start(sql, starts),
        None => {
            sql.push("r.name ILIKE ")
                .push_bind(like_anywhere(text))
                .push(" OR r.name % ")
                .push_bind(text.to_owned());
        }
    }
    sql.push(")");
}
/// `r.name` matches the short text ([`WordStart`]).
fn push_word_start(sql: &mut QueryBuilder<'_, Postgres>, starts: WordStart) {
    sql.push("(");
    for pattern in starts.patterns {
        sql.push("r.name ILIKE ").push_bind(pattern).push(" AND ");
    }
    sql.push("lower(r.name) ~ (")
        .push_bind(word_start_regex())
        .push("||lower(")
        .push_bind(starts.words)
        .push(")))");
}
/// A letter of a script that writes articles and short words onto the next word (Hebrew `ה`,
/// `ו`; Arabic `ال`), so a short `q` in it keeps the match inside the name: `فن` finds `الفن`.
fn prefixing_script(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0590..=0x08FF // Hebrew, Arabic, Syriac, Thaana, NKo, Samaritan, Mandaic, Arabic extended
            | 0xFB1D..=0xFDFF // Hebrew and Arabic presentation forms A
            | 0xFE70..=0xFEFF // Arabic presentation forms B
    )
}
/// A letter of a script written without spaces between words.
pub(crate) fn unspaced_script(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0E00..=0x0EFF // Thai, Lao
            | 0x0F00..=0x0FFF // Tibetan
            | 0x1000..=0x109F // Myanmar
            | 0x1780..=0x17FF // Khmer
            | 0x1980..=0x19DF // New Tai Lue
            | 0x1A00..=0x1AAF // Buginese, Tai Tham
            | 0x1B00..=0x1B7F // Balinese
            | 0x3005..=0x303C // ideographic marks, Hangzhou numerals, kana repeat marks
            | 0x3040..=0x30FF // Hiragana, Katakana
            | 0x3100..=0x312F // Bopomofo
            | 0x31A0..=0x31BF // Bopomofo extended
            | 0x31F0..=0x31FF // Katakana extensions
            | 0x3400..=0x4DBF // CJK extension A
            | 0x4E00..=0x9FFF // CJK
            | 0xA000..=0xA4CF // Yi
            | 0xA980..=0xA9DF // Javanese
            | 0xA9E0..=0xA9FF // Myanmar extended-B
            | 0xAA60..=0xAA7F // Myanmar extended-A
            | 0xAA80..=0xAADF // Tai Viet
            | 0xF900..=0xFAFF // CJK compatibility
            | 0xFF66..=0xFF9F // halfwidth Katakana
            | 0x1950..=0x197F // Tai Le
            | 0x11700..=0x1174F // Ahom
            | 0x17000..=0x18D7F // Tangut, Khitan
            | 0x1AFF0..=0x1B2FF // Kana extensions and supplement, Nushu
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
/// through the reference index.
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
    // The unit by suffix, never by byte index: the value may end in any character.
    let (number, hours) = match (window.strip_suffix('h'), window.strip_suffix('d')) {
        (Some(number), _) => (number, true),
        (_, Some(number)) => (number, false),
        _ => return Err(bad()),
    };
    if number.is_empty() || number.len() > 4 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: i64 = number.parse().map_err(|_| bad())?;
    let span = if hours {
        chrono::Duration::hours(n)
    } else {
        chrono::Duration::days(n)
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
pub const UNIT_COLUMNS: &str = "id,uri,host,type,type_path,name,category,point,start_at,end_at,venue_id,doc,search_text,resolver,resolved_at,refresh_at,status,osm_type,osm_id,unit_id,occurrence";
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
/// The version of a search page's shape, in its ETag: a page cached under another version is
/// never answered 304. Move it when a search row gains or loses a field, a field reads otherwise
/// for stored rows, or the same query finds other rows or the same rows in another order.
pub const SEARCH_SHAPE: u32 = 12;
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

#[utoipa::path(get,tag="Search",path="/v1/search",params(SearchQuery),responses((status=200,body=SearchResponse),(status=304,description="ETag unchanged"),(status=400,description="Invalid or unavailable filter"),(status=501,description="wot_2 asked of an instance whose Nexus serves no reach set"),(status=502,description="Nexus reach unavailable"),(status=503,description="Text search is busy: the text index had no turn for the search, the search ran past its time (`limits.search_index_budget_ms`), or its client runs its share of turns already",headers(("Retry-After"=u64,description="Seconds to wait before asking again: 1")))))]
pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
    headers: HeaderMap,
    client: Option<axum::Extension<ClientAddr>>,
) -> Result<Response, ApiError> {
    let tags = validate_search(&q)?;
    let key = q.key.as_deref().map(key_filter).transpose()?;
    if q.collapse.is_some() && !state.config.groups.enabled {
        return Err(ApiError::bad("groups are disabled"));
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
    // The `reach` filter's set and the `boost` set, each read from Nexus (cached per viewer),
    // under one deadline: two reads take no longer than one may.
    let reach_deadline = state.nexus.reach_deadline();
    let mut sets = vec![];
    for kind in [&q.reach, &q.boost] {
        sets.push(match (&q.viewer, kind) {
            (Some(viewer), Some(kind)) => {
                let set = state.nexus.reach_by(viewer, kind, reach_deadline).await;
                Some(set.map_err(|e| {
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
        if c.filter != filter
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
    // A text search without a social filter, an exact identifier or a filter on a job posting's
    // or recipe's own facts (`remote`, `ingredient`) reads its best candidates
    // from the text index (`search_index`); the rest, and every search while the daemon's index
    // is not open yet, read Postgres alone.
    let text_route = query_text(&q).filter(|_| {
        q.reach.is_none()
            && q.author.is_none()
            && related.is_none()
            && q.in_collection.is_none()
            && key.is_none()
            && q.remote.is_none()
            && q.ingredient.is_none()
    });
    // Catalogue cards nobody referenced, of the catalogues that are on: listed unless the search
    // asks for what only referenced links have (a social filter, an identifier, a venue, a
    // category, a time, a job posting's or a recipe's facts, references in time).
    let catalogues: Vec<String> = state
        .config
        .catalogues
        .enabled_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let cards_allowed = !catalogues.is_empty() && catalogue_cards_allowed(&q, sort);
    // Held to the end of the search (a binding, not `_`, so it is not dropped at once).
    let mut _postgres_turn = None;
    // The end of that turn's budget: the Postgres read it holds the turn for stops there.
    let mut postgres_deadline: Option<std::time::Instant> = None;
    let candidates = match text_route {
        Some(text) => {
            let mut query = text_query(&q, text, &tags, &spatial, at, sort)?;
            if cards_allowed {
                query.catalogues = catalogues.clone();
                query.labels = true;
            }
            // A trending page ranks by the references inside its window, which only Postgres
            // counts: it is given more candidates (those referenced last inside the window) and
            // ranks them.
            let cap = if trending {
                (state.config.search.text_candidates * TRENDING_CANDIDATES).min(TRENDING_MOST)
            } else {
                state.config.search.text_candidates
            };
            let client = client.map(|axum::Extension(ClientAddr(ip))| ip);
            match state
                .search_index
                .candidates_for(&state.pool, query, cap, client)
                .await
            {
                // A text the index leaves to Postgres keeps its turn while Postgres reads it.
                Ok(found) => {
                    _postgres_turn = found.turn;
                    postgres_deadline = found.deadline;
                    found.candidates
                }
                // Every turn was taken for the search's whole budget, it ran past it, or its client
                // runs its share of turns already: a 503, not a read of Postgres, which the same load
                // would hold up next. Asked again a second later, a turn is likely free.
                Err(error) if error.is::<crate::search_index::Busy>() => {
                    return Ok(text_search_busy());
                }
                Err(error) => {
                    tracing::warn!(error=%crate::resolver::log_error(&error),"text index unavailable; this search reads Postgres");
                    None
                }
            }
        }
        None => None,
    };
    // The catalogue cards among the candidates, or without text the best known of those the
    // filters let in, read as units (`CardUnit`).
    let card_units: CardUnits = match (&candidates, cards_allowed) {
        (Some(found), true) => {
            let found: Vec<&crate::search_index::Candidate> =
                found.units.iter().filter(|c| c.card.is_some()).collect();
            card_units(&state.pool, &catalogues, &found).await?
        }
        // Without text: under a turn of the text searches (their share per client and their
        // budget), each read stopping with that budget.
        (None, true) if query_text(&q).is_none() && related.is_none() => {
            let client = client.map(|axum::Extension(ClientAddr(ip))| ip);
            let Ok((turn, deadline)) = state.search_index.card_read_turn(client).await else {
                return Ok(text_search_busy());
            };
            _postgres_turn = Some(turn);
            postgres_deadline = Some(deadline);
            let read = best_known_cards(
                &state.pool,
                &q,
                &tags,
                &spatial,
                &catalogues,
                sort,
                state.config.search.text_candidates,
                None,
                postgres_deadline,
            );
            match within_turn(postgres_deadline, read).await {
                Some(Err(error)) if error.0 == StatusCode::SERVICE_UNAVAILABLE => {
                    return Ok(text_search_busy())
                }
                Some(units) => units?,
                None => return Ok(text_search_busy()),
            }
        }
        // One character found inside names, which the text index leaves to Postgres: the best
        // known cards that hold it.
        (None, true)
            if _postgres_turn.is_some()
                && related.is_none()
                && query_text(&q).is_some_and(|t| t.trim().chars().count() == 1) =>
        {
            let read = best_known_cards(
                &state.pool,
                &q,
                &tags,
                &spatial,
                &catalogues,
                sort,
                state.config.search.text_candidates,
                query_text(&q),
                postgres_deadline,
            );
            match within_turn(postgres_deadline, read).await {
                Some(Err(error)) if error.0 == StatusCode::SERVICE_UNAVAILABLE => {
                    return Ok(text_search_busy())
                }
                Some(units) => units?,
                None => return Ok(text_search_busy()),
            }
        }
        _ => CardUnits::default(),
    };
    // Cards built from a catalogue card are listed with the labels they show, which count as tags
    // as a card's do (with cards listed, in the referenced branch beside them; else in the plain
    // read, `backed`).
    let backed = cards_allowed && card_units.units.is_empty() && {
        let read = async {
            let mut tx = begin_read(&state.pool, postgres_deadline).await?;
            let backed = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM resource WHERE catalogue_card IS NOT NULL)",
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(card_read_error)?;
            tx.commit().await?;
            Ok::<bool, ApiError>(backed)
        };
        match within_turn(postgres_deadline, read).await {
            Some(Err(error)) if error.0 == StatusCode::SERVICE_UNAVAILABLE => {
                return Ok(text_search_busy())
            }
            Some(backed) => backed?,
            None => return Ok(text_search_busy()),
        }
    };
    let with_cards = !card_units.units.is_empty();
    let labelled: Vec<String> = crate::catalogue::join::joinable(&state.config.catalogues)
        .into_iter()
        .map(str::to_owned)
        .collect();

    let mut sql = QueryBuilder::<Postgres>::new("WITH ");
    if with_cards {
        push_card_units(&mut sql, &card_units.units, &catalogues);
    }
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
        let indexed = text.is_some() && candidates.is_some();
        // By activity, a catalogue card nobody referenced comes after every referenced unit (whose
        // score is 0 or more), the best known first (`fame - 2`, below -1).
        let by_fame = with_cards && !indexed;
        if by_fame {
            sql.push("CASE WHEN r.fame IS NOT NULL THEN r.fame::double precision-2.0 ELSE ");
        }
        if indexed {
            // The text index's relevance times (1 + fame), plus the activity score (with any
            // boost) at its weight (`search_index::ACTIVITY_WEIGHT`).
            sql.push(format!(
                "c.relevance+{:?}*(",
                crate::search_index::ACTIVITY_WEIGHT
            ));
        } else if let Some(text) = text {
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
        if indexed {
            sql.push(")");
        } else if text.is_some() {
            sql.push("))");
        }
        if by_fame {
            sql.push(" END");
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
        // Each related resource once, as its own card (never once per occurrence), looked up one
        // by one: the set is bounded, and a join would read every card.
        sql.push(" FROM related rel CROSS JOIN LATERAL (SELECT * FROM resource_unit u WHERE u.id=rel.resource_id OFFSET 0) r");
    } else if with_cards {
        // The referenced units (the candidates' resources, with text) and the catalogue cards,
        // each branch with its own filters as plain conditions: references and people's tags (and
        // the labels a card built from a catalogue card shows) on the referenced units, labels
        // (their tags) on the cards.
        sql.push(" FROM (SELECT su.*,CASE WHEN su.doc->'catalogue'->>'catalogue'=ANY(")
            .push_bind(labelled.clone())
            .push(") AND jsonb_typeof(su.doc->'labels')='array' THEN ARRAY(SELECT jsonb_array_elements_text(su.doc->'labels')) ELSE '{}'::text[] END AS labels,NULL::real AS fame,NULL::text[] AS hidden_by FROM search_unit su WHERE true");
        if let Some(found) = &candidates {
            sql.push(" AND su.id=ANY(")
                .push_bind(candidate_resources(&found.units))
                .push(")");
        }
        push_referenced(&mut sql, "su", &q, &reach);
        push_tags_or_labels(&mut sql, "su", &q, &reach, &tags, &labelled);
        sql.push(" UNION ALL SELECT * FROM card_unit cu WHERE true");
        if !tags.is_empty() {
            sql.push(if q.match_mode.as_deref() == Some("all") {
                " AND cu.labels@>"
            } else {
                " AND cu.labels&&"
            })
            .push_bind(tags.clone());
        }
        sql.push(") r");
    } else {
        sql.push(" FROM search_unit r");
    }
    if let Some(found) = &candidates {
        // The candidates by unit, each with its relevance: the referenced ones, and the catalogue
        // cards still listed, each unit once (a card whose link a resource has is that resource; a
        // card merged into another is that card's unit, at the better relevance of the two).
        let listed: std::collections::HashMap<i64, &str> = card_units
            .units
            .iter()
            .map(|u| (u.card, u.unit_id.as_str()))
            .collect();
        // Each unit at its first (best) candidate's relevance; found by its words when any of its
        // candidates was (a merged unit's cards each matched in their own way).
        let mut at: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut joined: Vec<(&str, f64, bool)> = vec![];
        // A merged card is its own unit too (listed when the card it is merged into is not), and
        // lends the main card its relevance.
        for (unit, relevance, words) in found.units.iter().flat_map(|c| match c.card {
            None => vec![(c.unit_id.as_str(), c.relevance, c.words)],
            Some(card) => {
                let own = listed.get(&card).map(|u| (*u, c.relevance, c.words));
                let main = card_units
                    .merged
                    .get(&card)
                    .and_then(|main| listed.get(main))
                    .map(|u| (*u, c.relevance, c.words));
                main.into_iter().chain(own).collect()
            }
        }) {
            match at.get(unit) {
                Some(&i) => joined[i].2 |= words,
                None => {
                    at.insert(unit, joined.len());
                    joined.push((unit, relevance, words));
                }
            }
        }
        sql.push(" JOIN unnest(")
            .push_bind(joined.iter().map(|c| c.0.to_owned()).collect::<Vec<_>>())
            .push("::text[],")
            .push_bind(joined.iter().map(|c| c.1).collect::<Vec<_>>())
            .push("::float8[],")
            .push_bind(joined.iter().map(|c| c.2).collect::<Vec<_>>())
            .push("::bool[]) AS c(unit_id,relevance,words) ON c.unit_id=r.unit_id");
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
    if let (Some(found), false) = (&candidates, with_cards) {
        // By resource first: an occurrence's unit ID is computed, its resource's is a key.
        sql.push(" AND r.id=ANY(")
            .push_bind(candidate_resources(&found.units))
            .push(")");
    }
    if related.is_some() {
        // A post whose author has not opted in to search is never suggested, as it is never found by
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
    if sort == Some("start") {
        sql.push(" AND r.start_at IS NOT NULL");
    }
    if let Some(types) = &q.type_name {
        // Any of the types, each with its subtypes: a path that holds one of them.
        sql.push(" AND r.type_path && ")
            .push_bind(search_types("type", types, SEARCH_TYPES_MAX)?);
    }
    if let Some(types) = &q.exclude_type {
        // None of the types, nor their subtypes: a card with a type whose path holds none of them.
        sql.push(" AND r.type IS NOT NULL AND NOT coalesce(r.type_path && ")
            .push_bind(search_types(
                "exclude_type",
                types,
                SEARCH_EXCLUDED_TYPES_MAX,
            )?)
            .push(",false)");
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
    if let Some(category) = &q.category {
        sql.push(" AND r.category=").push_bind(category.clone());
    }
    if let Some(hosts) = &q.host {
        // Any of the hosts, each exactly as the links' URLs name it.
        sql.push(" AND r.host=ANY(")
            .push_bind(search_hosts(hosts)?)
            .push(")");
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
    if let Some(text) = query_text(&q) {
        sql.push(" AND (");
        if candidates.is_some() {
            // A unit the index found by its words still has text (a post found gone or emptied
            // since the index's last pass keeps its tags, not its words); any other candidate
            // matches as without the index.
            sql.push("c.words AND (coalesce(r.name,'')<>'' OR r.search_text<>''::tsvector) OR ");
        }
        push_text_match(&mut sql, text);
        // A post whose author has not opted in to search is never found by its text.
        sql.push(") AND coalesce(r.doc->'attributes'->>'opted_out','') <> 'true'");
    }
    if related.is_none() && !with_cards {
        // (A related row is referenced by a sampled tagger, who is in scope.)
        push_referenced(&mut sql, "r", &q, &reach);
    }
    if !with_cards {
        // People's tags, and the labels of the cards built from a catalogue card when there are any.
        let labelled: &[String] = if backed { &labelled } else { &[] };
        push_tags_or_labels(&mut sql, "r", &q, &reach, &tags, labelled);
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
            .push(") x) c GROUP BY gi.root), filtered AS (SELECT DISTINCT ON(coalesce(u.group_id,e.unit_id)) e.* FROM eligible e LEFT JOIN group_unit u ON u.unit_id=e.unit_id AND u.group_id IS NOT NULL LEFT JOIN grouped_refs c ON c.resource_id=e.id ORDER BY coalesce(u.group_id,e.unit_id),coalesce(c.refs,0) DESC,");
        if with_cards {
            // A card and the listed resource of its link share the ID, the group and the
            // references: the resource is the row.
            sql.push("e.fame IS NOT NULL,");
        }
        sql.push("e.id,e.unit_id");
    }
    if with_cards {
        // A catalogue card whose link a listed resource has is that resource: one row per unit,
        // the resource's. A card that another listed unit stands for (`hidden_by`: a referenced
        // card built from it or from its thing's card in another catalogue, the card it is merged
        // into) is left out, once every filter has passed: a unit that fails them hides nothing.
        let present = if q.collapse.is_some() {
            "eligible"
        } else {
            "filtered"
        };
        sql.push("), page AS (SELECT * FROM (SELECT DISTINCT ON (unit_id) * FROM (")
            .push(not_hidden("filtered", present))
            .push(") v ORDER BY unit_id,fame IS NOT NULL) d");
    } else {
        sql.push("), page AS (SELECT * FROM filtered");
    }
    if let Some(c) = &cursor {
        sql.push(" WHERE (rank,unit_id)<(")
            .push_bind(c.score)
            .push(",")
            .push_bind(c.id.clone())
            .push(")");
    }
    sql.push(" ORDER BY rank DESC,unit_id DESC LIMIT ")
        .push_bind((q.limit.unwrap_or(50) + 1) as i64)
        .push(")");
    // Facets count every matching link, collapsed or not: with cards, each unit once, as the page
    // lists it (the resource of a card's link, not the card).
    let counted = match (q.collapse.is_some(), with_cards) {
        (true, false) => "eligible",
        (false, false) => "filtered",
        (collapsed, true) => {
            let set = if collapsed { "eligible" } else { "filtered" };
            sql.push(", counted AS (SELECT DISTINCT ON (unit_id) * FROM (")
                .push(not_hidden(set, set))
                .push(") v ORDER BY unit_id,fame IS NOT NULL)");
            "counted"
        }
    };
    sql.push(" SELECT coalesce((SELECT jsonb_agg(");
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
    let asked = facets_asked(q.facet.as_deref())?;
    sql.push("jsonb_build_object('type',");
    if asked.types {
        sql.push("coalesce((SELECT jsonb_agg(jsonb_build_object('type',type,'count',count) ORDER BY count DESC,type) FROM (SELECT r.type,count(DISTINCT r.unit_id) AS count FROM ")
            .push(counted)
            .push(" r WHERE r.type IS NOT NULL GROUP BY r.type) t),'[]')");
    } else {
        sql.push("NULL");
    }
    sql.push(",'tags',");
    if asked.tags && with_cards {
        // People's tags and catalogue cards' labels together.
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,label) FROM (SELECT l.label,count(DISTINCT l.unit_id) AS count FROM (SELECT r.unit_id,f.label FROM ")
            .push(counted)
            .push(" r JOIN effective_reference f ON r.id=f.resource_id WHERE f.label IS NOT NULL");
        scope(&mut sql, &q, &reach);
        sql.push(" UNION ALL SELECT r.unit_id,x.label FROM ")
            .push(counted)
            .push(" r CROSS JOIN unnest(r.labels) x(label)) l GROUP BY l.label ORDER BY count DESC,l.label LIMIT ")
            .push_bind(FACET_LIST_CAP)
            .push(") t),'[]')");
    } else if asked.tags && backed {
        // People's tags and the labels of the cards built from a catalogue card together.
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,label) FROM (SELECT l.label,count(DISTINCT l.unit_id) AS count FROM (SELECT r.unit_id,f.label FROM ")
            .push(counted)
            .push(" r JOIN effective_reference f ON r.id=f.resource_id WHERE f.label IS NOT NULL");
        scope(&mut sql, &q, &reach);
        sql.push(" UNION ALL SELECT r.unit_id,x.label FROM ")
            .push(counted)
            .push(" r CROSS JOIN LATERAL jsonb_array_elements_text(CASE WHEN r.doc->'catalogue'->>'catalogue'=ANY(")
            .push_bind(labelled.clone())
            .push(") AND jsonb_typeof(r.doc->'labels')='array' THEN r.doc->'labels' ELSE '[]'::jsonb END) x(label) WHERE (SELECT x.catalogue_card FROM resource x WHERE x.id=r.id) IS NOT NULL) l GROUP BY l.label ORDER BY count DESC,l.label LIMIT ")
            .push_bind(FACET_LIST_CAP)
            .push(") t),'[]')");
    } else if asked.tags {
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,label) FROM (SELECT f.label,count(DISTINCT r.unit_id) AS count FROM ")
            .push(counted)
            .push(" r JOIN effective_reference f ON r.id=f.resource_id WHERE f.label IS NOT NULL");

        scope(&mut sql, &q, &reach);
        sql.push(" GROUP BY f.label ORDER BY count DESC,f.label LIMIT ")
            .push_bind(FACET_LIST_CAP)
            .push(") t),'[]')");
    } else {
        sql.push("NULL");
    }
    sql.push(",'host',");
    if asked.hosts {
        // One pass over the set: per host its count, a few of its links and the site name its
        // documents give most often, for the service's name (`HostFacet::site`). A host that
        // `pubky://` objects use is a user's key, not a service; a value over the filter's 256
        // bytes, or one with a comma (which `host=` reads as two), could not be filtered by.
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,host) FROM (SELECT r.host,count(DISTINCT r.unit_id) AS count,(array_agg(DISTINCT r.uri ORDER BY r.uri))[1:")
            .push_bind(FACET_SITE_SAMPLE)
            .push("] AS uris,mode() WITHIN GROUP (ORDER BY nullif(nullif(r.doc->>'site',''),r.host)) AS site FROM ")
            .push(counted)
            .push(" r WHERE r.host IS NOT NULL AND r.host<>'' AND octet_length(r.host)<=256 AND strpos(r.host,',')=0 GROUP BY r.host HAVING bool_and(r.uri NOT LIKE 'pubky://%') ORDER BY count DESC,r.host LIMIT ")
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
    // A search through the catalogue paths (cards listed, or a link built from a card about) is
    // planned near Postgres' JIT threshold, where compiling its expressions (about 85 ms on
    // stable's data) costs several times the query: read without JIT, in its own transaction.
    // Without catalogues the search reads as before.
    // A read that holds a text-search turn (a text the index left to Postgres) stops with the
    // turn's budget, not the pool's 5 s: past it, the search answers as a busy index does.
    let remaining_ms = postgres_deadline.map(|deadline| {
        deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .max(1)
    });
    let row = if with_cards || backed || remaining_ms.is_some() {
        let mut tx = state.pool.begin().await?;
        if with_cards || backed {
            sqlx::query("SET LOCAL jit = off").execute(&mut *tx).await?;
        }
        if let Some(ms) = remaining_ms {
            sqlx::query("SELECT set_config('statement_timeout',$1,true)")
                .bind(format!("{ms}ms"))
                .execute(&mut *tx)
                .await?;
        }
        let row = match sql
            .build()
            .persistent(related.is_none())
            .fetch_one(&mut *tx)
            .await
        {
            Ok(row) => row,
            // `query_canceled`: the turn's budget ran out during the read.
            Err(sqlx::Error::Database(error))
                if remaining_ms.is_some() && error.code().as_deref() == Some("57014") =>
            {
                crate::telemetry::search_index_busy("budget");
                return Ok(text_search_busy());
            }
            Err(error) => return Err(error.into()),
        };
        tx.commit().await?;
        row
    } else {
        sql.build()
            .persistent(related.is_none())
            .fetch_one(&state.pool)
            .await?
    };
    let mut items: Vec<Value> = serde_json::from_value(row.try_get("items")?)?;
    let has_more = items.len() > q.limit.unwrap_or(50);
    items.truncate(q.limit.unwrap_or(50));
    let next = if has_more {
        let last = items.last().expect("nonempty page");
        Some(
            PageCursor {
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
        // Relative times (`from=now`) resolve per request: hash the resolved window too. A
        // trending or related page counts references in a window that ends at the request's
        // instant: its minute is hashed, so the page is not answered 304 once rows have left
        // the window with no write in between.
        blake3::hash(&serde_json::to_vec(&(
            q.clone(),
            revision,
            reach_revision,
            spatial.from,
            spatial.to,
            (trending || related.is_some()).then(|| at.timestamp().div_euclid(60)),
            SEARCH_SHAPE,
            // The text index lags the database by a pass: its own revision.
            candidates.as_ref().map(|c| c.revision),
            // The catalogues that are on: switching one off changes what is listed.
            (with_cards || backed).then_some(&catalogues),
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
    } else {
        let mut cards = items.iter().map(card).collect::<Result<Vec<_>, _>>()?;
        with_refs(&state.pool, cards.iter_mut()).await?;
        for (card, item) in cards.iter_mut().zip(&items) {
            // A catalogue card's source labels, a resource built from one its card's (from its
            // stored card); `[]` on every other row.
            let listed: Vec<String> = item["labels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect();
            card.labels = Some(if listed.is_empty() {
                card.labels.take().unwrap_or_default()
            } else {
                listed
            });
        }
        shown(cards.iter_mut(), &state.config);
        for card in cards.iter_mut().filter(|c| c.labels.is_none()) {
            card.labels = Some(vec![]);
        }
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
    crate::music_identity::keys(pool, &mut detail).await?;
    shown_detail(&mut detail, config);
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
    let detail = match read(&state.pool, &id, &state.config).await? {
        Some(detail) => {
            opened(&state, &detail).await;
            Some(detail)
        }
        None => read_unshared(&state.pool, &id, &state.config).await?,
    }
    .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "resource not found".into()))?;
    let mut detail = detail;
    place_hints(&state.pool, &state.config, &mut detail).await?;
    tagged(&headers, &detail)
}
/// A card someone opens whose refresh is due is read again first (`store::opened`): the
/// workers are woken for it. A card built from a
/// catalogue card reads its extras again once they are old. Best effort: the read answers
/// whatever these writes do.
async fn opened(state: &AppState, detail: &ResourceDetail) {
    match store::opened(&state.pool, &detail.card.id).await {
        Ok(true) => state.wake.notify_waiters(),
        Ok(false) => {}
        Err(error) if open_warning_due() => {
            tracing::warn!(resource_id = %detail.card.id, kind = crate::resolver::error_class(&error), "an opened card's refresh was not moved forward (logged at most once a minute)")
        }
        Err(_) => {}
    }
    if detail.card.labels.is_some() {
        if let Err(error) = crate::catalogue::extras::opened(&state.pool, &detail.card.id).await {
            if open_warning_due() {
                tracing::warn!(resource_id = %detail.card.id, kind = crate::resolver::error_class(&error), "an opened catalogue card's extras were not marked (logged at most once a minute)");
            }
        }
    }
}
/// Whether a failed write of an open is logged now: at most once a minute, so a database that
/// refuses writes doesn't log every read.
fn open_warning_due() -> bool {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = Utc::now().timestamp();
    let last = LAST.load(Ordering::Relaxed);
    now - last >= 60
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}
/// The card of a catalogue card nobody shared, by the resource ID of its own link
/// (`catalogue::join::unshared`): built from the catalogue as a shared card is, with no tags,
/// references or group. Nothing is stored.
async fn read_unshared(
    pool: &PgPool,
    id: &str,
    config: &Config,
) -> Result<Option<ResourceDetail>, ApiError> {
    read_unshared_with(pool, id, config, UNSHARED_READ_CAPS).await
}
/// The caps of an unshared card's `maybe_same` and `also_on`: a read's (10 and 256) and a lookup
/// card's (3 and 8), as for stored cards.
const UNSHARED_READ_CAPS: (i64, i64) = (10, 256);
const UNSHARED_LOOKUP_CAPS: (i64, i64) = (3, 8);
/// The most catalogue cards nobody shared one lookup answers; the entries past them are skipped
/// as unknown ones are.
pub const LOOKUP_UNSHARED: usize = 8;
/// [`read_unshared`] with `caps`: at most `caps.0` `maybe_same` and `caps.1` `also_on` entries.
async fn read_unshared_with(
    pool: &PgPool,
    id: &str,
    config: &Config,
    caps: (i64, i64),
) -> Result<Option<ResourceDetail>, ApiError> {
    // An imported OSM place nobody shared: the card the OSM route builds from it, as a shared
    // OSM link shows it (its values come from the catalogue, never from a resource row).
    let osm = if config.catalogues.enabled(crate::catalogue::Catalogue::Osm) {
        crate::catalogue::osm::card_by_link(pool, id).await?
    } else {
        None
    };
    let (uri, name, doc, keys, member, category, osm_object) = match osm {
        Some(card) => {
            let Some((doc, _)) = crate::catalogue::osm::document(&card, config.osm.ttl_secs) else {
                return Ok(None);
            };
            let object = crate::resolver::osm::parse_id(&card.uri);
            let category = doc.category.clone();
            (
                card.uri.clone(),
                card.name.clone(),
                doc,
                vec![format!("osm:{}", card.source_id)],
                None,
                category,
                object,
            )
        }
        None => {
            let catalogues = crate::catalogue::join::joinable(&config.catalogues);
            let Some(card) = crate::catalogue::join::unshared(pool, &catalogues, id).await? else {
                return Ok(None);
            };
            let Some(doc) = crate::catalogue::join::document(&card, &card.uri) else {
                return Ok(None);
            };
            // The group of the links built from the card or related to it, when one is stored,
            // and its members as `also_on` (that member first, as another member lists it):
            // their tags show with the card.
            let member = crate::catalogue::join::member(pool, &card).await?;
            let keys = crate::catalogue::join::backed_keys(&card);
            (
                card.uri.clone(),
                card.name.clone(),
                doc,
                keys,
                member,
                None,
                None,
            )
        }
    };
    let host = crate::catalogue::card_host(&uri);
    // A root that is no web link lists its web page as an alias (by-uri answers it by that page).
    let aliases: Vec<String> = crate::catalogue::root_page(&uri)
        .map(|(page, _)| page)
        .into_iter()
        .collect();
    let value: Value = sqlx::query_scalar(&[
        "SELECT jsonb_build_object('id',$1::text,'uri',$5::text,'host',$6::text,'type',$7::text,'type_path',to_jsonb($2::text[]),'name',$8::text,'category',$14::text,'doc',$9::jsonb,'resolver',NULL,'resolved_at',NULL,'refresh_at',NULL,'status','ok','osm_type',$15::text,'osm_id',$16::bigint)||jsonb_build_object('group',CASE WHEN $12 AND $11::text IS NOT NULL THEN group_hint($11) END,'maybe_same',",
        &maybe_same_sql("$1::text", "$2::text[]", caps.0 as u32, "$3", "$4"),
        // The member first, as another member lists it, then the member's own; `caps.1` in all.
        &format!(",'also_on',CASE WHEN $11::text IS NULL OR NOT $12 THEN '[]'::jsonb ELSE (SELECT coalesce(jsonb_agg(e ORDER BY n),'[]'::jsonb) FROM (SELECT e,n FROM jsonb_array_elements(coalesce((SELECT jsonb_agg(e) FROM jsonb_array_elements(also_on((SELECT u.unit_id FROM group_unit u JOIN group_unit me ON me.group_id=u.group_id WHERE me.unit_id=$11 AND u.unit_id<>$11 AND u.group_id IS NOT NULL ORDER BY u.unit_id LIMIT 1),'',{0},true)) e WHERE e->>'id'=$11),'[]'::jsonb)||also_on($11,'',{0},$12)) WITH ORDINALITY AS a(e,n) ORDER BY n LIMIT {0}) x) END,'aliases',to_jsonb($13::text[]),'keys',to_jsonb($10::text[]),'facts',NULL)", caps.1),
    ]
    .concat())
    .bind(id)
    .bind(&doc.type_path)
    .bind(same_event(config))
    .bind(same_recording(config))
    .bind(&uri)
    .bind(host)
    .bind(&doc.type_name)
    .bind(&name)
    .bind(serde_json::to_value(&doc)?)
    .bind(keys)
    .bind(member)
    .bind(config.groups.enabled)
    .bind(aliases)
    .bind(category)
    .bind(osm_object.as_ref().map(|o| o.osm_type.clone()))
    .bind(osm_object.as_ref().map(|o| o.id))
    .fetch_one(pool)
    .await?;
    let mut detail = detail(&value)?;
    with_refs(pool, std::iter::once(&mut detail.card)).await?;
    Ok(Some(detail))
}

/// The catalogue cards a map view counts one by one (`x`, `y`, `n` rows, each branch after a
/// `UNION ALL`): one branch per catalogue, its name a literal, so the planner reads that
/// catalogue's partial point index (a bound list of names matches none of them).
fn push_cluster_cards(
    sql: &mut QueryBuilder<'_, Postgres>,
    catalogues: &[String],
    boxes: &[[f64; 4]],
    types: Option<&Vec<String>>,
    excluded: Option<&Vec<String>>,
    tags: &[String],
    all: bool,
) {
    for catalogue in catalogues
        .iter()
        .filter_map(|c| crate::catalogue::Catalogue::parse(c))
    {
        sql.push(format!(
            " UNION ALL SELECT ST_X(c.point),ST_Y(c.point),1 FROM catalogue_card c WHERE c.catalogue='{}' AND c.point IS NOT NULL AND (",
            catalogue.name()
        ));
        for (i, b) in boxes.iter().enumerate() {
            if i > 0 {
                sql.push(" OR ");
            }
            sql.push("c.point && ST_MakeEnvelope(");
            for (j, n) in b.iter().enumerate() {
                if j > 0 {
                    sql.push(",");
                }
                sql.push_bind(*n);
            }
            sql.push(",4326)");
        }
        sql.push(")");
        if let Some(types) = types {
            sql.push(" AND c.type=ANY(")
                .push_bind(types.clone())
                .push(")");
        }
        if let Some(types) = excluded {
            sql.push(" AND NOT c.type=ANY(")
                .push_bind(types.clone())
                .push(")");
        }
        if !tags.is_empty() {
            sql.push(if all {
                " AND c.labels@>"
            } else {
                " AND c.labels&&"
            })
            .push_bind(tags.to_vec());
        }
    }
}
/// `GET /v1/map/clusters`: the parameters. Filters are search's (`GET /v1/search`).
#[derive(Clone, Debug, Default, Deserialize, Serialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ClusterQuery {
    /// The map view: `min_lon,min_lat,max_lon,max_lat`, as search's `bbox`.
    pub bbox: String,
    /// The map's zoom, 0–20: a cell is 360 / 2^(zoom+2) degrees wide and high (a quarter of a
    /// map tile).
    pub zoom: u8,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub exclude_type: Option<String>,
    pub tags: Option<String>,
    #[serde(rename = "match")]
    pub match_mode: Option<String>,
    pub author: Option<String>,
    pub reach: Option<String>,
    pub viewer: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub referenced: Option<bool>,
}
/// The clusters of one map view.
#[derive(Serialize, ToSchema)]
pub struct MapClusters {
    pub zoom: u8,
    /// A cell's width and height, in degrees.
    pub cell_degrees: f64,
    /// Everything counted in the view.
    pub total: i64,
    /// One per cell that holds anything, the fullest first.
    pub clusters: Vec<MapCluster>,
}
/// One map cell's count.
#[derive(Serialize, ToSchema)]
pub struct MapCluster {
    /// The cell: `[min_lon, min_lat, max_lon, max_lat]`.
    pub bbox: [f64; 4],
    pub count: i64,
    /// The mean of the points counted: where to draw the cluster.
    pub point: model::Point,
    /// The search that lists the cell's cards: these filters with the cell as `bbox`.
    pub search: String,
}
/// The zoom up to which catalogue cards are counted from their cells' totals.
pub const MAP_CELL_ZOOM: u8 = 12;
/// Cluster requests answered at once; one more is a 503. Public for the tests that fill it.
pub static MAP_TURNS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
/// The most of [`MAP_TURNS`] one client holds at once (by IPv4 address or IPv6 /64, as text
/// search counts its share): one client's wide views leave the other turns to others.
pub const MAP_TURNS_PER_CLIENT: usize = 2;
static MAP_CLIENTS: std::sync::Mutex<BTreeMap<std::net::IpAddr, usize>> =
    std::sync::Mutex::new(BTreeMap::new());
/// One map clusters count's turn, given back when dropped.
pub struct MapTurn {
    _permit: tokio::sync::SemaphorePermit<'static>,
    client: Option<std::net::IpAddr>,
}
impl Drop for MapTurn {
    fn drop(&mut self) {
        if let (Some(client), Ok(mut clients)) = (self.client, MAP_CLIENTS.lock()) {
            if let Some(n) = clients.get_mut(&client) {
                *n -= 1;
                if *n == 0 {
                    clients.remove(&client);
                }
            }
        }
    }
}
/// A turn for a count of `client`'s, now: none when all four are taken or the client holds its
/// share.
pub fn map_turn(client: Option<std::net::IpAddr>) -> Option<MapTurn> {
    let client = client.map(crate::search_index::share_key);
    let mut clients = MAP_CLIENTS.lock().ok()?;
    if client.is_some_and(|c| clients.get(&c).copied().unwrap_or(0) >= MAP_TURNS_PER_CLIENT) {
        return None;
    }
    let permit = MAP_TURNS.try_acquire().ok()?;
    if let Some(c) = client {
        *clients.entry(c).or_default() += 1;
    }
    Some(MapTurn {
        _permit: permit,
        client,
    })
}
#[utoipa::path(get,tag="Search",path="/v1/map/clusters",params(ClusterQuery),responses((status=200,body=MapClusters),(status=400,description="Invalid parameters, or a view of more than `limits.map_cluster_cells` cells at its zoom"),(status=502,description="Nexus reach unavailable"),(status=503,description="Busy, or the view too large for its filter (a `tags` filter over a wide view)")))]
pub async fn map_clusters(
    State(state): State<AppState>,
    client: Option<axum::Extension<ClientAddr>>,
    Query(c): Query<ClusterQuery>,
) -> Result<Json<MapClusters>, ApiError> {
    if c.zoom > 20 {
        return Err(ApiError::bad("zoom is 0–20"));
    }
    let q = SearchQuery {
        type_name: c.type_name.clone(),
        exclude_type: c.exclude_type.clone(),
        tags: c.tags.clone(),
        match_mode: c.match_mode.clone(),
        author: c.author.clone(),
        reach: c.reach.clone(),
        viewer: c.viewer.clone(),
        bbox: Some(c.bbox.clone()),
        from: c.from.clone(),
        to: c.to.clone(),
        referenced: c.referenced,
        ..Default::default()
    };
    let tags = validate_search(&q)?;
    let at = state.now();
    let spatial = crate::spatial::Filters::parse_at(&q, &state.config.search, at)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    let cell = 360.0 / f64::from(1u32 << (u32::from(c.zoom) + 2));
    let view = spatial.bbox.as_ref().expect("bbox given").0;
    let width = if view[0] > view[2] {
        view[2] + 360.0 - view[0]
    } else {
        view[2] - view[0]
    };
    let cells_in_view = ((width / cell).ceil() + 1.0) * (((view[3] - view[1]) / cell).ceil() + 1.0);
    let most = state.config.search.map_cluster_cells;
    if cells_in_view > most as f64 {
        return Err(ApiError::bad(format!(
            "the view spans more than {most} cells at this zoom: zoom in or narrow bbox"
        )));
    }
    let reach =
        match (&q.viewer, &q.reach) {
            (Some(viewer), Some(kind)) => {
                if !state.config.nexus_reach_set && kind == "wot_2" {
                    return Err(ApiError(
                        StatusCode::NOT_IMPLEMENTED,
                        "wot_2 is not supported: the Nexus this instance reads serves no reach set"
                            .into(),
                    ));
                }
                let deadline = state.nexus.reach_deadline();
                Some(state.nexus.reach_by(viewer, kind, deadline).await.map_err(|e| {
                tracing::warn!(error=%crate::resolver::log_error(&e),"reach unavailable");
                ApiError(StatusCode::BAD_GATEWAY, "Nexus reach unavailable".into())
            })?)
            }
            _ => None,
        };
    let labelled: Vec<String> = crate::catalogue::join::joinable(&state.config.catalogues)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let catalogues: Vec<String> = state
        .config
        .catalogues
        .enabled_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    // Cards are counted from the per-cell totals at zoom 12 and below (`catalogue_map_cell`),
    // unless a filter needs each card's labels; above it, card by card.
    let cards = !catalogues.is_empty() && catalogue_cards_allowed(&q, None);
    let from_cells = cards && c.zoom <= MAP_CELL_ZOOM && tags.is_empty();
    let boxes = spatial.bbox.as_ref().expect("bbox given").split();
    let push_boxes = |sql: &mut QueryBuilder<'_, Postgres>, column: &str| {
        sql.push(" AND (");
        for (i, b) in boxes.iter().enumerate() {
            if i > 0 {
                sql.push(" OR ");
            }
            sql.push(format!("{column} && ST_MakeEnvelope("));
            for (j, n) in b.iter().enumerate() {
                if j > 0 {
                    sql.push(",");
                }
                sql.push_bind(*n);
            }
            sql.push(",4326)");
        }
        sql.push(")");
    };
    // A point on the last edge (latitude 90, longitude 180) is in the last cell.
    let (most_x, most_y) = ((4i64 << c.zoom) - 1, (2i64 << c.zoom) - 1);
    let mut sql = QueryBuilder::<Postgres>::new("SELECT least(floor((x+180)/");
    sql.push_bind(cell)
        .push(")::bigint,")
        .push_bind(most_x)
        .push(") AS cx,least(floor((y+90)/")
        .push_bind(cell)
        .push(")::bigint,")
        .push_bind(most_y)
        .push(") AS cy,sum(n)::bigint AS n,sum(x*n)/sum(n) AS x,sum(y*n)/sum(n) AS y FROM (");
    // Referenced units with a point (places, events, located posts), as search lists them.
    sql.push("SELECT ST_X(r.point) AS x,ST_Y(r.point) AS y,1 AS n FROM search_unit r WHERE r.point IS NOT NULL AND r.status<>'missing'");
    spatial.apply(&mut sql);
    if let Some(types) = &q.type_name {
        sql.push(" AND r.type_path && ")
            .push_bind(search_types("type", types, SEARCH_TYPES_MAX)?);
    }
    if let Some(types) = &q.exclude_type {
        sql.push(" AND r.type IS NOT NULL AND NOT coalesce(r.type_path && ")
            .push_bind(search_types(
                "exclude_type",
                types,
                SEARCH_EXCLUDED_TYPES_MAX,
            )?)
            .push(",false)");
    }
    push_referenced(&mut sql, "r", &q, &reach);
    push_tags_or_labels(&mut sql, "r", &q, &reach, &tags, &labelled);
    // A job posting past its end is closed: search leaves it out, and so does its cell.
    sql.push(" AND NOT coalesce(r.type_path @> ARRAY['JobPosting'] AND r.end_at<=")
        .push_bind(at)
        .push(",false)");
    // A unit that is a counted card's own link counts once, as the card: on both paths it is
    // left out when its card passes the cards' filters (card by card, the view too).
    let card_types = match &q.type_name {
        Some(t) => Some(model::subtypes(&search_types("type", t, SEARCH_TYPES_MAX)?)),
        None => None,
    };
    let card_excluded = match &q.exclude_type {
        Some(t) => Some(model::subtypes(&search_types(
            "exclude_type",
            t,
            SEARCH_EXCLUDED_TYPES_MAX,
        )?)),
        None => None,
    };
    let push_card_filters = |sql: &mut QueryBuilder<'_, Postgres>, column: &str| {
        if let Some(types) = &card_types {
            sql.push(format!(" AND {column}=ANY("))
                .push_bind(types.clone())
                .push(")");
        }
        if let Some(types) = &card_excluded {
            sql.push(format!(" AND NOT {column}=ANY("))
                .push_bind(types.clone())
                .push(")");
        }
    };
    if cards {
        sql.push(" AND NOT EXISTS (SELECT 1 FROM catalogue_card x WHERE x.uri=r.uri AND x.catalogue=ANY(")
            .push_bind(catalogues.clone())
            .push(") AND x.point IS NOT NULL");
        push_card_filters(&mut sql, "x.type");
        if !from_cells {
            push_boxes(&mut sql, "x.point");
            if !tags.is_empty() {
                sql.push(if q.match_mode.as_deref() == Some("all") {
                    " AND x.labels@>"
                } else {
                    " AND x.labels&&"
                })
                .push_bind(tags.clone());
            }
        }
        sql.push(")");
    }
    if from_cells {
        // The coarsest level of totals whose cells fit in the view's own (zoom 4, 6, 8, 10 or
        // 12): from zoom 5 a view's cell adds up at most 2 x 2 of them.
        let level: u8 = match c.zoom {
            0..=4 => 4,
            5..=6 => 6,
            7..=8 => 8,
            9..=10 => 10,
            _ => 12,
        };
        let fine = 360.0 / f64::from(4u32 << level);
        sql.push(" UNION ALL SELECT m.sum_lon/m.cards,m.sum_lat/m.cards,m.cards FROM catalogue_map_cell m WHERE m.level=")
            .push_bind(i16::from(level))
            .push(" AND m.cards>0 AND m.catalogue=ANY(")
            .push_bind(catalogues.clone())
            .push(") AND (");
        for (i, b) in boxes.iter().enumerate() {
            if i > 0 {
                sql.push(" OR ");
            }
            let index = |v: f64, shift: f64| ((v + shift) / fine).floor() as i64;
            sql.push("(m.cx BETWEEN ")
                .push_bind(index(b[0], 180.0))
                .push(" AND ")
                .push_bind(index(b[2], 180.0))
                .push(" AND m.cy BETWEEN ")
                .push_bind(index(b[1], 90.0))
                .push(" AND ")
                .push_bind(index(b[3], 90.0))
                .push(")");
        }
        sql.push(")");
        push_card_filters(&mut sql, "m.type");
    } else if cards {
        // Catalogue cards with a point, where search lists them, one by one.
        push_cluster_cards(
            &mut sql,
            &catalogues,
            &boxes,
            card_types.as_ref(),
            card_excluded.as_ref(),
            &tags,
            q.match_mode.as_deref() == Some("all"),
        );
    }
    sql.push(") p GROUP BY 1,2 ORDER BY n DESC,cx,cy LIMIT ")
        .push_bind(cells_in_view as i64 + 1024);
    // One at a time per few: a cheap request must not hold every connection; and bounded in
    // time, the view too broad for its filter is a 503.
    let _turn = map_turn(client.map(|axum::Extension(ClientAddr(ip))| ip)).ok_or_else(|| {
        // Why: every turn taken, or this client holding its share.
        let reason = if MAP_TURNS.available_permits() == 0 {
            "turns"
        } else {
            "share"
        };
        crate::telemetry::map_clusters_busy(reason);
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "map clusters are busy; retry in a moment".into(),
        )
    })?;
    let mut tx = state.pool.begin().await?;
    sqlx::query(&format!(
        "SET LOCAL statement_timeout = {}",
        state.config.search.map_cluster_timeout_ms
    ))
    .execute(&mut *tx)
    .await?;
    let rows = match sql.build().fetch_all(&mut *tx).await {
        Ok(rows) => rows,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("57014") => {
            crate::telemetry::map_clusters_busy("timeout");
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the view is too large for this filter: zoom in".into(),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    tx.commit().await?;
    // The cell's search: these filters, with the cell as its bbox.
    let mut base = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in [
        ("type", &c.type_name),
        ("exclude_type", &c.exclude_type),
        ("tags", &c.tags),
        ("match", &c.match_mode),
        ("author", &c.author),
        ("reach", &c.reach),
        ("viewer", &c.viewer),
        ("from", &c.from),
        ("to", &c.to),
    ] {
        if let Some(value) = value {
            base.append_pair(key, value);
        }
    }
    if let Some(referenced) = c.referenced {
        base.append_pair("referenced", if referenced { "true" } else { "false" });
    }
    let rest = base.finish();
    let mut total = 0;
    let mut clusters = Vec::with_capacity(rows.len());
    for row in rows {
        let (cx, cy, n, x, y): (i64, i64, i64, f64, f64) = (
            row.try_get(0)?,
            row.try_get(1)?,
            row.try_get(2)?,
            row.try_get(3)?,
            row.try_get(4)?,
        );
        total += n;
        let round = |v: f64| (v * 1e7).round() / 1e7;
        let bbox = [
            round((cx as f64 * cell - 180.0).max(-180.0)),
            round((cy as f64 * cell - 90.0).max(-90.0)),
            round(((cx + 1) as f64 * cell - 180.0).min(180.0)),
            round(((cy + 1) as f64 * cell - 90.0).min(90.0)),
        ];
        let mut search = url::form_urlencoded::Serializer::new(String::new());
        search.append_pair("bbox", &bbox.map(|v| v.to_string()).join(","));
        let mut search = search.finish();
        if !rest.is_empty() {
            search.push('&');
            search.push_str(&rest);
        }
        clusters.push(MapCluster {
            bbox,
            count: n,
            point: model::Point { lat: y, lon: x },
            search: format!("/v1/search?{search}"),
        });
    }
    Ok(Json(MapClusters {
        zoom: c.zoom,
        cell_degrees: cell,
        total,
        clusters,
    }))
}
/// The place rules on a read, with the OSM catalogue on (`catalogue::osm::rules`): a map link
/// and the one imported place of its name within 50 m list each other as maybe the same (in
/// `maybe_same` and, with the link's references, in `also_on`); a deleted OSM object lists the one
/// imported place of its name and category within 50 m of where it was; a place renamed to an
/// unrelated name lists what it was (`replaced`). Nothing changes with the catalogue off.
async fn place_hints(
    pool: &PgPool,
    config: &Config,
    detail: &mut ResourceDetail,
) -> Result<(), ApiError> {
    use crate::catalogue::osm::rules;
    // Only places, with the catalogue on and holding cards.
    if !config.catalogues.enabled(crate::catalogue::Catalogue::Osm)
        || !detail.card.type_path.iter().any(|t| t == "Place")
        || !rules::imported(pool).await?
    {
        return Ok(());
    }
    let card = &mut detail.card;
    let stored = rules::stored(pool, &card.id).await?;
    let object = match &stored {
        Some(s) => s.osm_type.clone().zip(s.osm_id),
        None => crate::resolver::osm::parse_id(&card.uri).map(|o| (o.osm_type, o.id)),
    };
    let mut maybe: Vec<SameEvent> = vec![];
    let mut also: Vec<AlsoOn> = vec![];
    let place_link = |id: String, uri: String, site: Option<String>, refs: Vec<String>| AlsoOn {
        id: Some(id),
        uri,
        refs,
        site,
        derived: false,
        via: None,
        kind: Some("maybe".into()),
        at: None,
        embed: None,
    };
    if let Some((osm_type, osm_id)) = object {
        if card.status == "ok" {
            if let (Some(point), Some(name)) = (&card.point, &card.name) {
                // The map links that name this place, each with this place as its one match.
                let source_id = format!("{osm_type}/{osm_id}");
                let places = rules::places_near(pool, point.lat, point.lon).await?;
                let me = places.iter().find(|p| p.source_id == source_id);
                let links: Vec<_> = rules::links_near(pool, point.lat, point.lon)
                    .await?
                    .into_iter()
                    .take(rules::LINKS_CHECKED)
                    .collect();
                // A link joined exactly (an exact place key) is no "maybe".
                let exact = rules::exactly_placed(
                    pool,
                    &links.iter().map(|l| l.id.clone()).collect::<Vec<_>>(),
                )
                .await?;
                for link in links.into_iter().filter(|l| !exact.contains(&l.id)) {
                    let named = me.map_or_else(
                        || {
                            crate::catalogue::osm::place::folded(&link.name)
                                == crate::catalogue::osm::place::folded(name)
                        },
                        |me| me.named(&link.name),
                    );
                    if !named {
                        continue;
                    }
                    let around = rules::places_near(pool, link.lat, link.lon).await?;
                    if rules::the_one(&around, &link.name).map(|p| p.source_id.as_str())
                        != Some(source_id.as_str())
                    {
                        continue;
                    }
                    maybe.push(SameEvent {
                        id: link.id.clone(),
                        uri: link.uri.clone(),
                        site: link.site.clone(),
                        start: None,
                        score: 1.0,
                    });
                    let refs = if link.referenced {
                        vec![link.uri.clone()]
                    } else {
                        vec![]
                    };
                    also.push(place_link(link.id, link.uri, link.site, refs));
                }
            }
            let renames = rules::renames(pool, &osm_type, osm_id).await?;
            if !renames.is_empty() {
                let mut replaced = vec![];
                for r in renames {
                    replaced.push(Replaced {
                        references: rules::references_before(pool, &card.id, r.changed_at).await?,
                        name: r.old_name,
                        category: r.old_category,
                        until: r.changed_at,
                    });
                }
                detail.details.replaced = Some(replaced);
            }
        } else if card.status == "missing" {
            // Deleted: the one imported place of its name and category where it was.
            if let Some((name, category, lat, lon)) =
                rules::last_place(pool, &osm_type, osm_id).await?
            {
                let own = format!("{osm_type}/{osm_id}");
                let around: Vec<_> = rules::places_near(pool, lat, lon)
                    .await?
                    .into_iter()
                    .filter(|p| {
                        p.source_id != own && p.category.as_deref() == Some(category.as_str())
                    })
                    .collect();
                if let Some(p) = rules::the_one(&around, &name) {
                    if let Ok((id, uri, _)) = model::identity(&p.uri) {
                        maybe.push(SameEvent {
                            id,
                            uri,
                            site: Some("OpenStreetMap".into()),
                            start: None,
                            score: 1.0,
                        });
                    }
                }
            }
        }
    } else if stored.as_ref().is_some_and(|s| {
        s.status == "ok"
            && s.location_source.as_deref() == Some("uri")
            && s.source
                .as_deref()
                .is_some_and(|src| rules::MAP_LINK_SOURCES.contains(&src))
    }) {
        // A map link: the one imported place of its name within 50 m, unless it is joined
        // exactly already (an exact place key: a Wikidata item, an OSM object).
        let exact = !rules::exactly_placed(pool, std::slice::from_ref(&card.id))
            .await?
            .is_empty();
        if let (Some(point), Some(name), false) = (&card.point, &card.name, exact) {
            if rules::place_name(name) {
                let around = rules::places_near(pool, point.lat, point.lon).await?;
                if let Some(p) = rules::the_one(&around, name) {
                    if let Ok((id, uri, _)) = model::identity(&p.uri) {
                        maybe.push(SameEvent {
                            id: id.clone(),
                            uri: uri.clone(),
                            site: Some("OpenStreetMap".into()),
                            start: None,
                            score: 1.0,
                        });
                        also.push(place_link(id, uri, Some("OpenStreetMap".into()), vec![]));
                    }
                }
            }
        }
    }
    // A place the card already joins exactly (the group's members in `also_on`, an exact
    // Wikidata join among them) is no "maybe": only places not listed yet are added, each once.
    let mut known: std::collections::HashSet<String> = card
        .also_on
        .iter()
        .flat_map(|a| a.id.iter().cloned().chain(std::iter::once(a.uri.clone())))
        .collect();
    known.extend(
        card.maybe_same
            .iter()
            .flatten()
            .flat_map(|m| [m.id.clone(), m.uri.clone()]),
    );
    let new = |id: &str, uri: &str, known: &std::collections::HashSet<String>| {
        !known.contains(id) && !known.contains(uri)
    };
    maybe.retain(|m| new(&m.id, &m.uri, &known));
    also.retain(|a| new(a.id.as_deref().unwrap_or_default(), &a.uri, &known));
    let mut seen = std::collections::HashSet::new();
    also.retain(|a| seen.insert(a.uri.clone()));
    let mut seen = std::collections::HashSet::new();
    maybe.retain(|m| seen.insert(m.uri.clone()));
    if !maybe.is_empty() {
        card.maybe_same.get_or_insert_with(Vec::new).extend(maybe);
    }
    card.also_on.extend(also);
    Ok(())
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
    let mut detail = read(&state.pool, &id, &state.config).await?;
    let root = static_root_id(&q.uri);
    if detail.is_none() {
        if let Some(root) = &root {
            detail = read(&state.pool, root, &state.config).await?;
        }
    }
    match &detail {
        Some(detail) => opened(&state, detail).await,
        // A catalogue card nobody shared, by its own link or the link's root.
        None => {
            detail = read_unshared(&state.pool, &id, &state.config).await?;
            if detail.is_none() {
                if let Some(root) = &root {
                    detail = read_unshared(&state.pool, root, &state.config).await?;
                }
            }
        }
    }
    let mut detail =
        detail.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "link not known".into()))?;
    place_hints(&state.pool, &state.config, &mut detail).await?;
    tagged(&headers, &detail)
}
/// The ID of `uri`'s root by the static alias rule (`alias::root`: tracking parameters, a DOI
/// or ISBN link, a service's other URL forms), when it has one: what a read of a spelling this
/// instance holds neither as a card nor as an alias answers with. Computed from the URI alone:
/// nothing is fetched or written.
fn static_root_id(uri: &str) -> Option<String> {
    let alias = crate::alias::root(uri)?;
    model::identity(&alias.root).ok().map(|(id, _, _)| id)
}
#[utoipa::path(post,tag="Previews",path="/v1/resources/lookup",operation_id="lookup_resources",request_body=LookupRequest,responses((status=200,body=Vec<Card>),(status=400,description="Exactly one list of 1–500 IDs or URIs")))]
pub async fn lookup(
    State(state): State<AppState>,
    Json(body): Json<LookupRequest>,
) -> Result<Json<Vec<Card>>, ApiError> {
    // The count first: no entry is checked or normalised for a request over the cap.
    let entries = match (&body.ids, &body.uris) {
        (Some(ids), None) => ids.len(),
        (None, Some(uris)) => uris.len(),
        _ => return Err(ApiError::bad("supply exactly one of ids or uris")),
    };
    if !(1..=500).contains(&entries) {
        return Err(ApiError::bad("lookup requires 1–500 entries"));
    }
    let ids = match (body.ids, body.uris) {
        (Some(ids), _) => {
            if ids.iter().any(|s| !model::valid_id(s)) {
                return Err(ApiError::bad("invalid resource ID"));
            }
            ids
        }
        (None, uris) => {
            let uris = uris.unwrap_or_default();
            let mut ids = uris
                .iter()
                .map(|s| {
                    model::identity(s)
                        .map(|v| v.0)
                        .map_err(|e| ApiError::bad(e.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            // A spelling held neither as a card nor as an alias reads as its static root.
            let held: Vec<String> = sqlx::query_scalar("SELECT w.id FROM unnest($1::text[]) w(id) WHERE EXISTS(SELECT 1 FROM resource r WHERE r.id=w.id) OR EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=w.id)").bind(&ids).fetch_all(&state.pool).await?;
            let held: std::collections::HashSet<String> = held.into_iter().collect();
            for (id, uri) in ids.iter_mut().zip(&uris) {
                if !held.contains(id) {
                    if let Some(root) = static_root_id(uri) {
                        *id = root;
                    }
                }
            }
            ids
        }
    };
    let roots: Vec<String> = sqlx::query_scalar("SELECT coalesce(a.root_id,w.id) FROM unnest($1::text[]) w(id) LEFT JOIN resource_alias a ON a.alias_id=w.id").bind(&ids).fetch_all(&state.pool).await?;
    store::expedite(&state.pool, &roots).await?;
    state.wake.notify_waiters();
    let rows:Vec<(i64,Value)>=sqlx::query_as(&[
            "SELECT wanted.n,to_jsonb(r)||jsonb_build_object('group',CASE WHEN $2 THEN group_hint(r.id) END,'maybe_same',",
            &maybe_same_sql("r.id", "r.type_path", 3, "$3", "$4"),
            ",'also_on',also_on(r.id,r.id,8,$2)) FROM unnest($1::text[]) WITH ORDINALITY AS wanted(id,n) LEFT JOIN resource_alias a ON a.alias_id=wanted.id JOIN resource_document r ON r.id=coalesce(a.root_id,wanted.id) ORDER BY wanted.n",
        ]
        .concat()).bind(&ids).bind(state.config.groups.enabled).bind(same_event(&state.config)).bind(same_recording(&state.config)).fetch_all(&state.pool).await?;
    let found: std::collections::HashSet<i64> = rows.iter().map(|(n, _)| *n).collect();
    let mut cards = rows
        .iter()
        .map(|(n, value)| Ok((*n, card(value)?)))
        .collect::<Result<Vec<_>, ApiError>>()?;
    with_refs(&state.pool, cards.iter_mut().map(|(_, card)| card)).await?;
    // A catalogue card nobody shared answers as `by-uri` and `/v1/resources/{id}` answer it, in
    // its place (nothing is stored), with a lookup card's caps; at most LOOKUP_UNSHARED of them
    // (the rest are skipped as unknown entries are); with no catalogue on, nothing is read for
    // them.
    let missed: Vec<(i64, &String)> = (1..)
        .zip(&ids)
        .filter(|(n, _)| !found.contains(n))
        .collect();
    if !missed.is_empty() && !crate::catalogue::join::joinable(&state.config.catalogues).is_empty()
    {
        // The misses that are a card's own link, in one read; only those are built.
        let wanted: Vec<String> = missed.iter().map(|(_, id)| format!("url:{id}")).collect();
        let held: std::collections::HashSet<String> = sqlx::query_scalar(
            "SELECT DISTINCT substr(k,5) FROM catalogue_card c CROSS JOIN LATERAL unnest(c.keys) k WHERE c.keys && $1 AND c.catalogue=ANY($2) AND k=ANY($1)",
        )
        .bind(&wanted)
        .bind(crate::catalogue::join::joinable(&state.config.catalogues))
        .fetch_all(&state.pool)
        .await?
        .into_iter()
        .collect();
        let mut read = 0;
        for (n, id) in missed {
            if read == LOOKUP_UNSHARED || !held.contains(id.as_str()) {
                continue;
            }
            read += 1;
            if let Some(detail) =
                read_unshared_with(&state.pool, id, &state.config, UNSHARED_LOOKUP_CAPS).await?
            {
                cards.push((n, detail.card));
            }
        }
    }
    cards.sort_by_key(|(n, _)| *n);
    let mut cards: Vec<Card> = cards.into_iter().map(|(_, card)| card).collect();
    shown(cards.iter_mut(), &state.config);
    Ok(Json(cards))
}
#[utoipa::path(post,tag="Previews",path="/v1/resolve",request_body=ResolveRequest,responses((status=200,body=ResourceDetail),(status=202,body=ResourceDetail,description="Queued; poll resource by ID",headers(("Retry-After"=u64,description="Seconds until the next attempt, when it is put off (a missing answer waiting for the fetch that confirms it); absent while the job is queued or running"))),(status=400,description="Unsupported URI, disabled resolver, or a link that carries a credential"),(status=429,description="Preview rate limit"),(status=503,description="The instance's new previews for this hour are taken; nothing was stored",headers(("Retry-After"=u64,description="Seconds until a new preview can be taken")))))]
pub async fn resolve(
    State(state): State<AppState>,
    ClientAddr(client): ClientAddr,
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
    state.preview_budget(client, &id).await?;
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
        match state.reserve_preview(client) {
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
    // A link a catalogue card holds gets its card from it at once: the wait below finds it.
    crate::catalogue::join::referenced(&state.pool, &state.config.queue, std::slice::from_ref(&id))
        .await;
    store::expedite(&state.pool, std::slice::from_ref(&id)).await?;
    state.wake.notify_waiters();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let expired = || ApiError(StatusCode::NOT_FOUND, "resource expired".into());
    loop {
        // While the card is pending only its status is read, at the row `read` answers with (an
        // alias's root). The card itself is read once it has left pending or the wait is over,
        // and that read decides the answer.
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM resource WHERE id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await?;
        if status.ok_or_else(expired)? == "pending" && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let resource = read(&state.pool, &id, &state.config)
            .await?
            .ok_or_else(expired)?;
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
    /// The version of the URI-only spelling rules, tracking parameters and identifier pages
    /// (docs/api.md, Spelling vectors, Identifier pages): the first 16 hex characters of the
    /// BLAKE3 hash of `data/spellings.json`, the parameters, vectors and pages this build answers
    /// exactly. It changes with every change to that file.
    #[schema(example = "3f9a0c1d2e4b5a69")]
    pub spelling_rules: String,
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
        spelling_rules: crate::alias::spelling_rules().into(),
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
        (name = "Media", description = "A podcast show's episodes."),
        (name = "Groups", description = "Links to the same thing on several services: a hint on each card, never a merge."),
        (name = "Changes", description = "Freshness: what changed for the links a client keeps."),
        (name = "Service", description = "Version, resolvers, licence and this document."),
        (name = "Licensing", description = "The ODbL share-alike download of the OpenStreetMap-derived data."),
        (name = "Agents", description = "The usage guide for AI agents (also as a skill) and the index that links it and the MCP endpoint, `POST /mcp`, which docs/api.md describes.")
    ),
    paths(
        map_clusters,
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
        MapClusters,
        MapCluster,
        Replaced,
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
        OsmDumpImportedPlace,
        OsmDumpRename,
        OsmDumpSuccessor,
        OsmDumpRemoved,
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
    // The crate's version, as `/v1/info` names it.
    doc["info"]["version"] = env!("CARGO_PKG_VERSION").into();
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
/// One place of the OSM catalogue's import per line after the cached objects, while the catalogue
/// is on: an `OsmDumpPlace` (`display_name` null, `status` `ok`, `fetched_at` the card's last
/// write, `versions` empty) with three more fields; every field is present.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct OsmDumpImportedPlace {
    #[serde(flatten)]
    place: OsmDumpPlace,
    /// `catalogue`.
    #[schema(example = "catalogue")]
    from: String,
    /// Its other names (`name:*`, `alt_name`, …).
    other_names: Vec<String>,
    /// Its earlier, unrelated names, oldest first: `name`, `category`, `until` (when it was
    /// replaced).
    #[schema(value_type = Vec<Object>)]
    replaced: Value,
}
/// A dump line: the notice first, then one place per line.
#[derive(ToSchema)]
#[allow(dead_code, clippy::large_enum_variant)]
#[serde(untagged)]
pub enum OsmDumpLine {
    Notice(OsmDumpNotice),
    Place(OsmDumpPlace),
    Imported(OsmDumpImportedPlace),
    Rename(OsmDumpRename),
    Successor(OsmDumpSuccessor),
    Removed(OsmDumpRemoved),
}
/// After the imported places, while the OSM catalogue is on: an earlier name of an OSM object
/// that has no imported place now.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct OsmDumpRename {
    /// `rename`.
    #[schema(example = "rename")]
    from: String,
    osm_type: String,
    osm_id: i64,
    /// The earlier name.
    name: String,
    #[schema(required = true)]
    category: Option<String>,
    /// The name that replaced it.
    new_name: String,
    until: chrono::DateTime<chrono::Utc>,
}
/// After the renames, while the OSM catalogue is on: a deleted OSM object's successor.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct OsmDumpSuccessor {
    /// `successor`.
    #[schema(example = "successor")]
    from: String,
    old_type: String,
    old_id: i64,
    new_type: String,
    new_id: i64,
    /// `wikidata` or `changeset`.
    basis: String,
    found_at: chrono::DateTime<chrono::Utc>,
}
/// After the successors, while the OSM catalogue is on: a named OSM object an update removed (it
/// was deleted), as its card last was.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct OsmDumpRemoved {
    /// `removed`.
    #[schema(example = "removed")]
    from: String,
    osm_type: String,
    osm_id: i64,
    name: String,
    #[schema(required = true)]
    category: Option<String>,
    point: model::Point,
    removed_at: chrono::DateTime<chrono::Utc>,
}
/// The public dump line format (see `OsmDumpPlace`); a new `osm_place` column is not exported
/// until it is added here, to the schema and to docs/api.md.
pub const OSM_DUMP_LINE: &str = "SELECT jsonb_build_object('osm_type',osm_type,'osm_id',osm_id,'name',name,'type',type,'type_path',type_path,'category',category,\
    'point',CASE WHEN point IS NULL THEN NULL ELSE jsonb_build_object('lat',ST_Y(point),'lon',ST_X(point)) END,\
    'bbox',bbox,'address',address,'tags',tags,'display_name',display_name,'status',status,'fetched_at',fetched_at,'versions',versions) \
    FROM osm_place ORDER BY osm_type,osm_id";
/// An earlier name of an OSM object that has no imported place now (a removed card's renames),
/// as a dump line.
pub const OSM_DUMP_RENAME_LINE: &str = "SELECT jsonb_build_object('from','rename','osm_type',r.osm_type,'osm_id',r.osm_id,'name',r.old_name,'category',r.old_category,'new_name',r.new_name,'until',r.changed_at) \
    FROM osm_rename r WHERE NOT EXISTS (SELECT 1 FROM catalogue_card c WHERE c.catalogue='osm' AND c.source_id=r.osm_type||'/'||r.osm_id) ORDER BY r.osm_type,r.osm_id,r.changed_at";
/// A deleted OSM object's successor, as a dump line.
pub const OSM_DUMP_SUCCESSOR_LINE: &str = "SELECT jsonb_build_object('from','successor','old_type',old_type,'old_id',old_id,'new_type',new_type,'new_id',new_id,'basis',basis,'found_at',found_at) \
    FROM osm_successor ORDER BY old_type,old_id";
/// A named OSM object an update removed, as a dump line.
pub const OSM_DUMP_REMOVED_LINE: &str = "SELECT jsonb_build_object('from','removed','osm_type',osm_type,'osm_id',osm_id,'name',name,'category',category,'point',jsonb_build_object('lat',ST_Y(point),'lon',ST_X(point)),'removed_at',removed_at) \
    FROM osm_removed ORDER BY osm_type,osm_id";
/// One imported place of the OSM catalogue as a dump line, in `OSM_DUMP_LINE`'s shape (its
/// `type_path` filled in by the writer), with `from` `catalogue`, its other names and the renames
/// an update saw (`replaced`).
pub const OSM_DUMP_CATALOGUE_LINE: &str = "SELECT jsonb_build_object('osm_type',split_part(c.source_id,'/',1),'osm_id',split_part(c.source_id,'/',2)::bigint,'name',c.name,'type',c.type,'type_path',NULL,'category',c.details->>'category',\
    'point',CASE WHEN c.point IS NULL THEN NULL ELSE jsonb_build_object('lat',ST_Y(c.point),'lon',ST_X(c.point)) END,\
    'bbox',c.details->'bbox','address',coalesce(c.details->'address','{}'::jsonb),'tags',coalesce(c.details->'tags','{}'::jsonb),'display_name',NULL,'status','ok','fetched_at',c.updated_at,'versions','{}'::jsonb,\
    'from','catalogue','other_names',to_jsonb(c.other_names),'replaced',coalesce(r.replaced,'[]'::jsonb)) \
    FROM catalogue_card c LEFT JOIN (SELECT osm_type||'/'||osm_id AS source_id,jsonb_agg(jsonb_build_object('name',old_name,'category',old_category,'until',changed_at) ORDER BY changed_at) AS replaced FROM osm_rename GROUP BY 1) r ON r.source_id=c.source_id \
    WHERE c.catalogue='osm' ORDER BY c.source_id";

#[utoipa::path(get,tag="Licensing",path="/v1/dumps/osm_place.ndjson.gz",responses((status=200,description="The ODbL derivative database, written daily, as a gzip file download. Decompressed it is one JSON object per line: the licence notice first (`OsmDumpNotice`), then one cached OSM object per line (`OsmDumpPlace`), then, while the OpenStreetMap catalogue is on, one imported place per line (`OsmDumpImportedPlace`; an object both cached and imported has a line of each), the earlier names of objects with no imported place (`OsmDumpRename`), the successors found (`OsmDumpSuccessor`) and the named objects an update removed, as their cards last were (`OsmDumpRemoved`). Single byte ranges are served (206)",content_type="application/gzip",headers(("Content-Disposition"=String,description="`attachment; filename=\"osm_place.ndjson.gz\"`"),("ETag"=String),("Last-Modified"=String),("Accept-Ranges"=String,description="`bytes`"))),(status=206,description="The requested byte range of the file",content_type="application/gzip"),(status=304,description="ETag or Last-Modified unchanged"),(status=416,description="Range not satisfiable"),(status=429,description="Over `limits.osm_dump_downloads_per_hour` transfers (200 or 206) to this address in the last hour; HEAD, 304 and 416 are free"),(status=503,description="No dump file written yet; retry later")))]
async fn osm_dump(
    State(state): State<AppState>,
    ClientAddr(client): ClientAddr,
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
        state.download_budget(client).await?;
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
        .route(
            "/v1/groups/{id}",
            axum::routing::put(curate_group).delete(uncurate_group),
        )
        .route("/v1/resources/{id}/document", get(admin_document))
        .route("/metrics", get(metrics))
        .route("/v1/info", get(admin_info))
        .route(HEALTHZ, get(healthz));
    with_admin_layers(routes, state)
}
/// The admin listener's health route.
pub const HEALTHZ: &str = "/healthz";
/// How long the health route waits for the database.
const HEALTHZ_TIMEOUT: Duration = Duration::from_secs(1);
/// `GET /healthz`: 200 when the database answers `SELECT 1` within a second and no background
/// task has ended; 503 otherwise. Only on the admin listener, and the one route there that takes
/// no admin token: it says nothing but whether the daemon is well.
async fn healthz(State(state): State<AppState>) -> Response {
    if state.stopping.load(std::sync::atomic::Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, "stopping").into_response();
    }
    let select = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&state.pool);
    match tokio::time::timeout(HEALTHZ_TIMEOUT, select).await {
        Ok(Ok(_)) => (StatusCode::OK, "ok").into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response(),
    }
}
/// The admin listener's layer stack around `routes`: the panic backstop innermost, then the
/// request deadline, the bearer check, the `Host` check, so a request for another host or
/// without the credential never reaches a handler, and the request count outermost. No CORS.
/// Public so a test can mount a route under exactly this stack.
pub fn with_admin_layers(routes: Router<AppState>, state: AppState) -> Router {
    panic_backstop(routes, &state, "admin")
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            request_deadline,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_auth,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_host,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_requests,
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
    if request.method() == axum::http::Method::GET && request.uri().path() == HEALTHZ {
        return next.run(request).await;
    }
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
/// How long a scrape waits for the database gauges: below a scraper's usual ten seconds.
const METRICS_REFRESH: Duration = Duration::from_secs(8);
/// The registry, whatever the database does: when the gauges read from it cannot be refreshed
/// within `METRICS_REFRESH` they keep their last values and the failure is counted
/// (`pinakes_metrics_refresh_failures_total`), so a slow or failed read never hides the
/// process's own counters from a scrape. Until the gauges have been read once they hold no
/// value: the answer is 503 then, never zeros that would read as facts (an ingestion that
/// never succeeded). The queue's gauges are read last, by a statement of their own
/// (`Metrics::refresh`): when that alone fails, the other gauges hold what was just read and
/// the queue's keep their last values (no series before their first read).
async fn metrics(State(state): State<AppState>) -> Result<Response, ApiError> {
    // Within the request's own deadline too, so the answer is the registry and not a 504.
    let deadline = Duration::from_secs(state.config.api.request_timeout_secs.saturating_sub(1));
    let wait = METRICS_REFRESH.min(deadline);
    let refresh = tokio::time::timeout(wait, state.metrics.refresh(&state.pool))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the database read took too long")));
    if let Err(error) = refresh {
        state.metrics.refresh_failures.inc();
        tracing::warn!(error=%crate::resolver::log_error(&error),"metrics refresh failed");
        if !state.metrics.ever_refreshed() {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "metrics are not ready: the database has not been read yet".into(),
            ));
        }
    }
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
        &state.config,
    )
    .await?
    .map(Json)
    .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "group not found".into()))
}
async fn group_detail(
    pool: &PgPool,
    id: &str,
    hints: (bool, bool),
    config: &Config,
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
    shown(members.iter_mut(), config);
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
        &state.config,
    )
    .await?
    .map(Json)
    .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "group not found".into()))
}
/// `DELETE /v1/groups/{id}` (admin listener only): undo a curation ([`crate::groups::uncurate`]).
/// 204 when it was a curated group, 404 when it is no curated group, 400 for an invalid ID or
/// with groups off, 503 while the group writer or the write order is busy.
pub async fn uncurate_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let undone = crate::groups::uncurate(&state.pool, &id, &state.config.groups)
        .await
        .map_err(|e| {
            tracing::warn!(error=%crate::resolver::log_error(&e),"undoing a group curation failed");
            if e.is::<crate::groups::GroupBusy>() {
                ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "group writer busy; retry later".into(),
                )
            } else if e.is::<crate::groups::CurationError>() {
                ApiError::bad("invalid group curation")
            } else {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "group curation unavailable".into(),
                )
            }
        })?;
    if undone {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            "no curated group with this ID".into(),
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The trending window is `{n}h` or `{n}d`; any other text, whatever its last character,
    /// is refused and never panics.
    #[test]
    fn the_trend_window_refuses_text_that_ends_in_any_character() {
        let hours = |w| trend_window(Some(w)).ok().map(|d| d.num_hours());
        assert_eq!(hours("1h"), Some(1));
        assert_eq!(hours("90d"), Some(90 * 24));
        assert_eq!(trend_window(None).unwrap().num_days(), 7);
        for bad in [
            "", "h", "d", "é", "1é", "1😀", "😀", "0h", "91d", "2161h", "1m", "-1d", "1.5d", "h1",
            "12345h", "1hh",
        ] {
            assert!(trend_window(Some(bad)).is_err(), "{bad:?}");
        }
    }

    /// The card-by-card cluster count reads each catalogue's partial point index, with every
    /// catalogue of the default configuration on (a bound list of names would read none).
    #[sqlx::test(migrations = "../../migrations")]
    async fn cluster_cards_read_each_catalogues_point_index(pool: PgPool) {
        let config = Config::default();
        let catalogues: Vec<String> = config
            .catalogues
            .enabled_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert!(catalogues.len() > 1 && catalogues.iter().any(|c| c == "osm"));
        let mut sql = QueryBuilder::<Postgres>::new(
            "EXPLAIN (FORMAT JSON) SELECT count(*) FROM (SELECT 0::float8 AS x,0::float8 AS y,1 AS n WHERE false",
        );
        let tags = vec!["restaurant".to_owned()];
        push_cluster_cards(
            &mut sql,
            &catalogues,
            &[[8.4, 47.3, 8.7, 47.45]],
            None,
            None,
            &tags,
            false,
        );
        sql.push(") p");
        // Cards of every catalogue, almost all outside the view, as a real table holds them.
        for (i, catalogue) in catalogues.iter().enumerate() {
            sqlx::query(
                "INSERT INTO catalogue_card(catalogue,source,source_id,uri,type,name,point,seen_generation,content_hash) \
                 SELECT $1,$1,$1||n,'https://example.org/'||$1||'/'||n,'Place','X',\
                 ST_SetSRID(ST_MakePoint(CASE WHEN n<=5 THEN 8.5 ELSE -60+(n%100) END,CASE WHEN n<=5 THEN 47.35 ELSE -40+(n%50) END),4326),1,\
                 decode(md5($1||n)||md5(n||$1),'hex') FROM generate_series(1,$2) n",
            )
            .bind(catalogue)
            .bind(2000 + i as i32)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("ANALYZE catalogue_card")
            .execute(&pool)
            .await
            .unwrap();
        let plan: Value = sql.build_query_scalar().fetch_one(&pool).await.unwrap();
        // Every read of the card table is an index read on the view's box, through the
        // catalogue's partial index (a bound list of names proves no partial index's condition,
        // and reads the table).
        fn scans<'a>(node: &'a Value, out: &mut Vec<&'a Value>) {
            if node["Relation Name"] == "catalogue_card" {
                out.push(node);
            }
            for child in node["Plans"].as_array().into_iter().flatten() {
                scans(child, out);
            }
        }
        let mut found = vec![];
        scans(&plan[0]["Plan"], &mut found);
        assert_eq!(found.len(), catalogues.len(), "{plan}");
        for scan in found {
            let cond = scan["Index Cond"]
                .as_str()
                .or_else(|| scan["Recheck Cond"].as_str())
                .unwrap_or_default();
            assert!(cond.contains("point &&"), "{scan}");
        }
        assert!(!plan.to_string().contains("\"Seq Scan\""), "{plan}");
    }

    /// One client holds at most two of the four cluster turns; another client takes the rest;
    /// a turn given back is the client's again.
    #[test]
    fn a_client_holds_at_most_its_share_of_cluster_turns() {
        let (a, b): (std::net::IpAddr, std::net::IpAddr) =
            ("192.0.2.1".parse().unwrap(), "2001:db8::1".parse().unwrap());
        let first = map_turn(Some(a)).unwrap();
        let _second = map_turn(Some(a)).unwrap();
        assert!(map_turn(Some(a)).is_none());
        // The same /64 is the same client.
        let _other = map_turn(Some(b)).unwrap();
        let _fourth = map_turn(Some("2001:db8::2".parse().unwrap())).unwrap();
        assert!(map_turn(Some("2001:db8::3".parse().unwrap())).is_none());
        drop(first);
        assert!(map_turn(Some(a)).is_some());
    }

    /// Exact members use Card.refs' bounded ranking; repeated members in a response share it.
    #[sqlx::test(migrations = "../../migrations")]
    async fn also_on_refs_share_card_bounds_and_exclude_non_exact_links(pool: PgPool) {
        let uri = "https://example.org/member";
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut expected = Vec::new();
        for k in 0..40i32 {
            let alias_uri = format!("https://example.org/member-{k:02}");
            let alias = store::ensure_resource(&pool, &alias_uri).await.unwrap();
            sqlx::query(
                "INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')",
            )
            .bind(&alias)
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
            // The older nine would win if the 31-alias cap were lost. Among recent links,
            // 0 and 1 both cap at 1,000, so URI (not their full counts) breaks the tie.
            let n = match k {
                0 => 1200,
                1 => 1500,
                31.. => 2000,
                _ => 2,
            };
            sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) SELECT $1,$2,'tag','music','/pub/app/tags/'||$1||'-'||n,now()-$3*interval '1 minute' FROM generate_series(1,$4) n")
                .bind(&alias).bind("y".repeat(52)).bind(k).bind(n).execute(&pool).await.unwrap();
            if k < 5 {
                expected.push(alias_uri);
            }
        }
        let blank = store::ensure_resource(&pool, "https://example.net/empty")
            .await
            .unwrap();
        let make = || {
            card(&serde_json::json!({
                "id": blank, "uri": "https://example.net/empty", "status": "ok",
                "also_on": [
                    {"id": id, "uri": uri},
                    {"id": id, "uri": uri, "derived": true},
                    {"id": id, "uri": uri, "kind": "maybe"},
                    {"id": blank, "uri": "https://example.net/empty"}
                ]
            }))
            .unwrap()
        };
        let mut cards: Vec<Card> = (0..50).map(|_| make()).collect();
        cards.push(card(&serde_json::json!({"id": id, "uri": uri, "status": "ok"})).unwrap());
        with_refs(&pool, cards.iter_mut()).await.unwrap();
        assert_eq!(cards.last().unwrap().refs, expected);
        for card in &cards[..50] {
            assert_eq!(card.also_on[0].refs, expected);
            for entry in &card.also_on[1..] {
                assert!(serde_json::to_value(entry).unwrap().get("refs").is_none());
            }
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn also_on_refs_preserve_gone_social_alias_privacy(pool: PgPool) {
        let uri = "https://example.org/post/1";
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let alias = store::ensure_resource(&pool, "https://example.org/post/private-title")
            .await
            .unwrap();
        sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
            .bind(&alias)
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let mut gone = ResourceMeta::new(uri, "activitypub", 3600);
        gone.set_type("SocialMediaPosting");
        gone.status = model::Status::Missing;
        sqlx::query("UPDATE resource SET status='error',doc=$2 WHERE id=$1")
            .bind(&id)
            .bind(serde_json::to_value(gone).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        for rid in [&id, &alias] {
            sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','post','/pub/app/tags/'||$1,now())")
                .bind(rid).bind("y".repeat(52)).execute(&pool).await.unwrap();
        }
        let parent = store::ensure_resource(&pool, "https://example.net/parent")
            .await
            .unwrap();
        let mut c = card(&serde_json::json!({"id":parent,"uri":"https://example.net/parent","status":"ok","also_on":[{"id":id,"uri":uri}]})).unwrap();
        with_refs(&pool, [&mut c]).await.unwrap();
        assert!(
            serde_json::to_value(&c.also_on[0])
                .unwrap()
                .get("refs")
                .is_none(),
            "canonical only; no title-bearing aliases"
        );
    }
    /// A card whose link is not a web page opens in the `https:` page its source read it from,
    /// named by its site (else the page's host); a web link, a Pubky object, an `http:` page and
    /// a card gone upstream get none, and a card's own clients stay as they are.
    #[test]
    fn a_card_whose_link_is_not_a_web_page_opens_in_its_page() {
        let row = |uri: &str, site: &str, page: Option<&str>, status: &str| {
            let mut doc = ResourceMeta::new(uri, "mempool", 3600);
            doc.site = site.into();
            if let Some(page) = page {
                doc.attributes.insert("page_url".into(), page.into());
            }
            json!({"id": "0".repeat(32), "uri": uri, "status": status, "doc": doc})
        };
        let open_in = |row: Value| serde_json::to_value(card(&row).unwrap().open_in).unwrap();
        let page = "https://mempool.space/tx/ab";
        let long = format!(
            "https://commons.wikimedia.org/wiki/File:{}",
            "%D0%96".repeat(40)
        );
        let mut cut = ResourceMeta::new("commons:File:x", "commons", 3600);
        cut.attributes.insert("page_url".into(), long.clone());
        let cut = cut.bounded(&["page_url"]).unwrap();
        let long = cut.attributes["page_url"].clone();
        assert_eq!(long.len(), model::ATTRIBUTE_VALUE_CAP);
        let mut cut = ResourceMeta::new("commons:File:x", "commons", 3600);
        let raw = format!(
            "https://commons.wikimedia.org/wiki/File:x{}",
            "€".repeat(80)
        );
        cut.attributes.insert("page_url".into(), raw);
        let wide = cut.bounded(&["page_url"]).unwrap().attributes["page_url"].clone();
        assert!(wide.len() < model::ATTRIBUTE_VALUE_CAP && url::Url::parse(&wide).is_ok());
        assert_eq!(
            open_in(row("btc:tx/ab", "mempool.space", Some(page), "ok")),
            json!([{"client": "mempool.space", "url": page}])
        );
        assert_eq!(
            open_in(row(
                "uniprot:P69905",
                "",
                Some("https://www.uniprot.org/uniprotkb/P69905"),
                "ok"
            )),
            json!([{"client": "www.uniprot.org", "url": "https://www.uniprot.org/uniprotkb/P69905"}])
        );
        for (uri, page, status) in [
            ("https://example.org/a", Some("https://example.org/a"), "ok"),
            ("pubky://u/pub/app/x", Some("https://app.example/x"), "ok"),
            ("btc:tx/ab", Some("http://mempool.space/tx/ab"), "ok"),
            ("btc:tx/ab", Some("javascript:alert(1)"), "ok"),
            ("btc:tx/ab", Some(page), "missing"),
            // Cut to the attribute cap when it was stored: it may link a page that is not there.
            ("commons:File:x", Some(&*long), "ok"),
            // Cut on a character boundary, short of the cap.
            ("commons:File:x", Some(&*wide), "ok"),
            // A user or a credential in the page.
            ("btc:tx/ab", Some("https://user@mempool.space/tx/ab"), "ok"),
            ("btc:tx/ab", Some("https://u:p@mempool.space/tx/ab"), "ok"),
            (
                "btc:tx/ab",
                Some("https://mempool.space/tx/ab?token=x"),
                "ok",
            ),
            ("isbn:9780140328721", None, "ok"),
        ] {
            assert_eq!(
                open_in(row(uri, "x", page, status)),
                json!([]),
                "{uri} {page:?} {status}"
            );
        }
        // Its own clients win over the page.
        let mut own = row("btc:tx/ab", "mempool.space", Some(page), "ok");
        own["doc"]["open_in"] = json!([{"client": "App", "url": "https://app.example/ab"}]);
        assert_eq!(
            open_in(own),
            json!([{"client": "App", "url": "https://app.example/ab"}])
        );
    }
    /// Up to three letters or digits match at word starts; longer text, other signs and scripts
    /// written without spaces keep the match inside the name.
    #[test]
    fn short_queries_match_word_starts() {
        let start = |text: &str| word_start(text).map(|w| (w.patterns, w.words));
        assert_eq!(start("lol"), Some((vec!["%lol%".into()], "lol".into())));
        assert_eq!(start(" AI "), Some((vec!["%AI%".into()], "AI".into())));
        assert_eq!(
            start("a b"),
            Some((vec!["%a%".into(), "%b%".into()], "a\\s+b".into()))
        );
        // Letters, digits and the marks a word is written with never start a word; signs do.
        for inside in [
            'a',
            'É',
            'ß',
            'Ж',
            '²',
            '①',
            'ℹ',
            '\u{301}',
            '\u{94D}',
            '\u{200D}',
            '\u{10330}',
        ] {
            assert!(word_char(inside), "{inside:?}");
        }
        for sign in [
            ' ', '/', '.', '-', '_', '™', '・', '🔥', '\u{964}', '\u{218A}', '«', '字', 'の', 'ก',
        ] {
            assert!(!word_char(sign), "{sign:?}");
        }
        let regex = word_start_regex();
        assert!(
            regex.starts_with("(^|[^\\u0030-\\u0039\\u0041-\\u005a"),
            "{regex}"
        );
        assert!(
            regex.ends_with("])") && regex.contains("\\U0001") && !regex.contains("\\U0002"),
            "{regex}"
        );
        assert_eq!(start("été").unwrap().1, "été");
        assert_eq!(start("x²").unwrap().1, "x²");
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
            "فن",
            "ספר",
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
