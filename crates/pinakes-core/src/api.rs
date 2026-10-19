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
use chrono::{DateTime, Utc};
use hmac::{KeyInit, Mac, SimpleHmac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify};

/// A request clock (`AppState::clock`).
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
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
    info_cache: Arc<Mutex<Option<(Instant, Value)>>>,
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
            nexus: Nexus::new(&config.nexus_url)?,
            config,
            wake: Arc::default(),
            metrics,
            limiter: Arc::default(),
            downloads: Arc::default(),
            info_cache: Arc::default(),
            cursor_mac: SimpleHmac::<blake3::Hasher>::new_from_slice(&cursor_key)
                .context("initialize cursor MAC")?,
            clock: None,
        })
    }
    /// Now, for relative times.
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
    if let Err(error) = state
        .budget(
            &[(
                format!("api-ip:{}", peer.0.ip()),
                state.config.api.requests_per_minute,
            )],
            state.config.api.cache_capacity,
            "API rate limit exceeded",
            1,
        )
        .await
    {
        return error.into_response();
    }
    next.run(request).await
}

pub fn router(state: AppState) -> Router {
    let routes = Router::new()
        .route("/v1/info", get(info))
        .route("/v1/changes/lookup", post(crate::change_api::lookup))
        .route("/v1/groups/{id}", get(group))
        .route("/v1/resources/lookup", post(lookup))
        .route("/v1/resources/by-uri", get(by_uri))
        .route("/v1/resources/{id}", get(resource))
        .route(
            "/v1/resources/{id}/occurrences",
            get(crate::event::storage::list),
        )
        .route(
            "/v1/occurrences/lookup",
            post(crate::event::storage::lookup),
        )
        .route("/v1/resolve", post(resolve))
        .route("/v1/dumps/osm_place.ndjson.gz", get(osm_dump));
    with_public_layers(routes, state)
}
/// The public listener's layer stack around `routes`: the panic backstop innermost, then the
/// per-IP budget and the body limit. Public so a test can mount a route under exactly this stack.
pub fn with_public_layers(routes: Router<AppState>, state: AppState) -> Router {
    panic_backstop(routes, &state, "public")
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            public_budget,
        ))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}
/// Marks a response the panic backstop produced, so the layer around it can log the path.
#[derive(Clone, Copy)]
struct CaughtPanic;
/// Backstop: a handler panic answers 500 (counted in `pinakes_api_panics_total`) instead of
/// dropping the connection, and the request path is logged (never the payload). Innermost, so
/// the listener's other layers still shape the response.
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
/// The one card every route returns for a resource, occurrence or reference event. Absent
/// values are null, never "".
#[derive(Serialize)]
pub struct Card {
    pub id: String,
    pub uri: String,
    pub status: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// Plain text, entity-decoded: escape it before rendering it as HTML.
    pub name: Option<String>,
    /// Plain text, entity-decoded: escape it before rendering it as HTML.
    pub description: Option<String>,
    pub image: Option<String>,
    /// The publishing site: the host, or the app for `pubky://` objects (e.g. `mapky.app`).
    /// Plain text, like every text field.
    pub site: Option<String>,
    pub point: Option<model::Point>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub group: Option<GroupRef>,
    /// OpenStreetMap cards, events placed at an OSM venue and events whose zone was derived from
    /// coordinates: the ODbL attribution to display with the card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    /// For a calendar occurrence: which feed instance this card is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<OccurrenceKey>,
    /// For a Nexus event reference: the reference this card is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,
}
/// A hint that the same thing is also elsewhere: the group this card is in. Nothing is merged;
/// every member keeps its own card.
#[derive(Serialize)]
pub struct GroupRef {
    pub id: String,
    /// Members in the group, this one included.
    pub size: i64,
    /// Up to eight other members, by ID and URI; `GET /v1/groups/{id}` lists them all.
    pub members: Vec<GroupMember>,
}
#[derive(Serialize)]
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
/// `GET /v1/resources/{id}` and `/v1/resolve`: the card plus the resource's details.
#[derive(Serialize)]
pub struct ResourceDetail {
    #[serde(flatten)]
    pub card: Card,
    pub details: Details,
}
#[derive(Serialize)]
pub struct Details {
    pub category: Option<String>,
    pub address: Option<Value>,
    pub bbox: Option<[f64; 4]>,
    pub tz: Option<String>,
    pub all_day: Option<bool>,
    pub event_status: Option<String>,
    pub attendance: Option<String>,
    pub venue: Option<VenueRef>,
    pub organizer: Option<OrganizerRef>,
    /// Other spellings of this link that resolve to it (its aliases), by URI, at most 50. A
    /// page's own claims (`url`, `@id`, `sameAs`, canonical links) are not aliases and are not
    /// listed.
    pub aliases: Vec<String>,
    pub updated_at: Option<DateTime<Utc>>,
}
#[derive(Serialize)]
pub struct OrganizerRef {
    pub name: Option<String>,
    pub url: Option<String>,
}
#[derive(Serialize)]
pub struct VenueRef {
    pub name: Option<String>,
    pub id: Option<String>,
}
#[derive(Deserialize)]
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub uri: String,
}

fn present(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
/// The site a card names: the app path segment of a `pubky://` object, else the document's site
/// or the host.
pub fn site(uri: &str, doc_site: Option<&str>, host: Option<&str>) -> Option<String> {
    if let Some(path) = uri.strip_prefix("pubky://") {
        let mut parts = path.split('/').skip(1);
        if parts.next() == Some("pub") {
            if let Some(app) = present(parts.next()) {
                return Some(app);
            }
        }
    }
    present(doc_site).or_else(|| present(host))
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
    Ok(Card {
        site: site(
            &uri,
            doc.as_ref().map(|d| d.site.as_str()),
            row["host"].as_str(),
        ),
        id,
        uri,
        status: row["status"].as_str().unwrap_or("pending").into(),
        type_name: present(row["type"].as_str()),
        name: present(row["name"].as_str())
            .or_else(|| doc.as_ref().and_then(|d| present(d.name.as_deref()))),
        description: doc.as_ref().and_then(|d| present(d.description.as_deref())),
        image: doc.as_ref().and_then(|d| present(d.image.as_deref())),
        point: doc.as_ref().and_then(|d| d.location.clone()),
        start: doc.as_ref().and_then(|d| d.start),
        end: doc.as_ref().and_then(|d| d.end),
        group: row
            .get("group")
            .and_then(|g| g["id"].as_str().map(|id| (id, g)))
            .map(|(id, g)| GroupRef {
                id: id.into(),
                size: g["size"].as_i64().unwrap_or(1),
                members: g["members"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| {
                        let uri = m["uri"].as_str().unwrap_or_default().to_owned();
                        GroupMember {
                            id: m["id"].as_str().unwrap_or_default().into(),
                            site: site(&uri, m["site"].as_str(), m["host"].as_str()),
                            uri,
                        }
                    })
                    .collect(),
            }),
        attribution: credits(
            row["osm_id"]
                .as_i64()
                .map(|_| crate::resolver::osm::ATTRIBUTION.to_owned())
                .or_else(|| doc.as_ref().and_then(|d| d.attribution.clone())),
            // A zone derived from coordinates comes from ODbL boundary data: its own credit.
            doc.as_ref()
                .and_then(|d| d.event.timezone_attribution.clone()),
        ),
        occurrence,
        event: row.get("event").filter(|v| !v.is_null()).cloned(),
    })
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
fn detail(row: &Value) -> Result<ResourceDetail, ApiError> {
    let doc: Option<ResourceMeta> = if row["doc"].get("type").is_some() {
        Some(serde_json::from_value(row["doc"].clone())?)
    } else {
        None
    };
    // The alias rows `read` joins in, not the document's claims.
    let aliases: Vec<String> = serde_json::from_value(row["aliases"].clone()).unwrap_or_default();
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
            aliases,
            updated_at: None,
        },
    };
    Ok(ResourceDetail {
        card: card(row)?,
        details,
    })
}
async fn read(pool: &PgPool, id: &str, groups: bool) -> Result<Option<ResourceDetail>, ApiError> {
    let value: Option<Value> =
        sqlx::query_scalar("SELECT to_jsonb(r)||jsonb_build_object('group',CASE WHEN $2 THEN group_hint(r.id) END,'aliases',coalesce((SELECT jsonb_agg(uri ORDER BY uri) FROM (SELECT x.uri FROM resource_alias a JOIN resource x ON x.id=a.alias_id WHERE a.root_id=r.id ORDER BY x.uri LIMIT 50) s),'[]')) FROM resource_document r WHERE r.id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)")
            .bind(id)
            .bind(groups)
            .fetch_optional(pool)
            .await?;
    value.as_ref().map(detail).transpose()
}
/// A read's JSON, cacheable for 30 seconds, with an ETag of its content; 304 when
/// `If-None-Match` names it.
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
pub async fn resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if !model::valid_id(&id) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    let detail = read(&state.pool, &id, state.config.groups.enabled)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "resource not found".into()))?;
    tagged(&headers, &detail)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByUri {
    /// The link as referenced: normalised like every URI, and an alias answers with its root.
    pub uri: String,
}
/// One link's card by its URI, as `POST /v1/resources/lookup` answers for it: a link Pinakes
/// does not know is a 404 and is not fetched (`POST /v1/resolve` resolves it).
pub async fn by_uri(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ByUri>,
) -> Result<Response, ApiError> {
    let id = model::identity(&q.uri)
        .map_err(|e| ApiError::bad(e.to_string()))?
        .0;
    let detail = read(&state.pool, &id, state.config.groups.enabled)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "link not known".into()))?;
    tagged(&headers, &detail)
}
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
    let rows:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(r)||jsonb_build_object('group',CASE WHEN $2 THEN group_hint(r.id) END) FROM unnest($1::text[]) WITH ORDINALITY AS wanted(id,n) LEFT JOIN resource_alias a ON a.alias_id=wanted.id JOIN resource_document r ON r.id=coalesce(a.root_id,wanted.id) ORDER BY wanted.n").bind(ids).bind(state.config.groups.enabled).fetch_all(&state.pool).await?;
    Ok(Json(rows.iter().map(card).collect::<Result<_, _>>()?))
}
pub async fn resolve(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<ResolveRequest>,
) -> Result<Response, ApiError> {
    let (id, uri, _) = model::identity(&body.uri).map_err(|e| ApiError::bad(e.to_string()))?;
    // An alias is never fetched: the root's route decides.
    let target = crate::alias::root(&uri).map_or_else(|| uri.clone(), |a| a.root);
    let resolver = crate::resolver::classify_config(&target, &state.config)
        .ok_or_else(|| ApiError::bad("no resolver available for this URI"))?;
    if (resolver == "event_platform"
        && crate::event::adapters::detect(&target)
            .is_none_or(|d| !crate::event::adapters::manifest(d, &state.config.event).enabled))
        || (resolver == "web" && !state.config.web.enabled)
        || (resolver == "pubky" && !state.config.pubky.enabled)
        || (resolver == "geo" && !state.config.geo.enabled)
        || (resolver == "osm" && !state.config.osm.enabled)
        || (resolver == "event" && (!state.config.event.enabled || !state.config.event.calendars))
    {
        return Err(ApiError::bad("resolver is disabled"));
    }
    state.preview_budget(peer.ip(), &id).await?;
    store::ensure_resource(&state.pool, &uri).await?;
    let id = crate::alias::resolve_id(&state.pool, &id).await?;
    store::expedite(&state.pool, std::slice::from_ref(&id)).await?;
    state.wake.notify_waiters();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let resource = read(&state.pool, &id, state.config.groups.enabled)
            .await?
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "resource expired".into()))?;
        if resource.card.status != "pending" {
            return Ok((StatusCode::OK, Json(resource)).into_response());
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok((StatusCode::ACCEPTED, Json(resource)).into_response());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
/// What a client needs: the version, what this instance resolves, the limits a client must
/// respect, and the licence of the OSM-derived values its cards carry.
#[derive(Serialize)]
pub struct Info {
    pub version: String,
    /// The kinds of links this instance resolves: its routes and card kinds that are on.
    pub resolves: Vec<String>,
    pub limits: InfoLimits,
    pub osm_licence: OsmLicence,
}
#[derive(Serialize)]
pub struct InfoLimits {
    /// IDs or URIs in one `POST /v1/resources/lookup`.
    pub lookup: usize,
    /// Resource IDs or URIs in one `POST /v1/changes/lookup`.
    pub change_lookup: usize,
    /// Feed IDs or URIs in one `POST /v1/occurrences/lookup`.
    pub occurrence_lookup: usize,
    /// Requests per minute per client address, every public route together.
    pub requests_per_minute: usize,
    /// `POST /v1/resolve` requests per minute per client address, and per URI.
    pub previews_per_minute: usize,
    pub previews_per_uri_per_minute: usize,
}
#[derive(Serialize)]
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
pub async fn info(State(state): State<AppState>) -> Json<Info> {
    let c = &state.config;
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
        resolves,
        limits: InfoLimits {
            lookup: 500,
            change_lookup: 500,
            occurrence_lookup: 500,
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
    })
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
    let row=sqlx::query("SELECT (SELECT count(*) FROM resource) AS resources,(SELECT count(*) FROM reference) AS references,(SELECT count(*) FROM occurrence) AS occurrences").fetch_one(&state.pool).await?;
    let ingestion:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('nexus',nexus,'cursor',cursor,'feed_length',feed_length,'lag',greatest(feed_length-cursor,0),'epoch',epoch,'updated_at',updated_at) FROM feed_cursor ORDER BY nexus").fetch_all(&state.pool).await?;
    let statuses:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('resolver',resolver,'status',status,'count',count(*),'dead_letter',count(*) FILTER (WHERE dead_letter),'leased',count(*) FILTER (WHERE lease_until>now())) FROM resource GROUP BY resolver,status ORDER BY resolver,status").fetch_all(&state.pool).await?;
    let mut body = json!({"groups":state.config.groups,"version":env!("CARGO_PKG_VERSION"),"manifests":crate::adapter::configured(&state.config),"sources":sources,"metrics":state.metrics.summary(),"ingestion":ingestion,"counts":{"resources":row.try_get::<i64,_>("resources")?,"references":row.try_get::<i64,_>("references")?,"occurrences":row.try_get::<i64,_>("occurrences")?},"osm_licence":{"licence":"ODbL-1.0","attribution":crate::resolver::osm::ATTRIBUTION,"url":"https://opendatacommons.org/licenses/odbl/1-0/","dump":"/v1/dumps/osm_place.ndjson.gz","updated_at":dump_updated_at(&state.config)},"resolvers":{"event":{"enabled":state.config.event.enabled,"calendar":state.config.event.calendars,"geocoding":state.config.event.geocoding,"months_back":state.config.event.months_back,"months_forward":state.config.event.months_forward},"geo":{"enabled":state.config.geo.enabled,"network":false},"osm":{"enabled":state.config.osm.enabled,"concurrency":1,"batch_size":50,"requests_per_second":state.config.osm.requests_per_second},"web":{"enabled":state.config.web.enabled,"concurrency":state.config.web.concurrency},"pubky":{"enabled":state.config.pubky.enabled,"concurrency":state.config.pubky.concurrency},"statuses":statuses}});
    for kind in crate::resolver::cards::KINDS {
        let c = state.config.cards.resolver(kind);
        body["resolvers"][kind] = json!({"enabled":c.enabled,"concurrency":c.concurrency,"requests_per_second":c.requests_per_second});
    }
    *cache = Some((Instant::now(), body.clone()));
    Ok(body)
}
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
/// The public dump line format: the licence notice first, then one cached OSM object per line;
/// a new `osm_place` column is not exported until it is added here.
pub const OSM_DUMP_LINE: &str = "SELECT jsonb_build_object('osm_type',osm_type,'osm_id',osm_id,'name',name,'type',type,'type_path',type_path,'category',category,\
    'point',CASE WHEN point IS NULL THEN NULL ELSE jsonb_build_object('lat',ST_Y(point),'lon',ST_X(point)) END,\
    'bbox',bbox,'address',address,'tags',tags,'display_name',display_name,'status',status,'fetched_at',fetched_at,'versions',versions) \
    FROM osm_place ORDER BY osm_type,osm_id";

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
/// bearer check, so an unauthenticated request never reaches a handler. No CORS. Public so a test
/// can mount a route under exactly this stack.
pub fn with_admin_layers(routes: Router<AppState>, state: AppState) -> Router {
    panic_backstop(routes, &state, "admin")
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
#[derive(Serialize)]
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
/// A group member named on a card: enough to link to it.
#[derive(Serialize)]
pub struct GroupMember {
    pub id: String,
    pub uri: String,
    pub site: Option<String>,
}
pub async fn group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<GroupDetail>, ApiError> {
    if !model::valid_id(&id) || !state.config.groups.enabled {
        return Err(ApiError::bad("invalid or disabled group"));
    }
    group_detail(&state.pool, &id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "group not found".into()))
}
async fn group_detail(pool: &PgPool, id: &str) -> Result<Option<GroupDetail>, ApiError> {
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
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(d)||jsonb_build_object('group',group_hint(d.unit_id)) FROM group_unit u CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=u.unit_id OFFSET 0) d WHERE u.group_id=$1 ORDER BY d.id,d.unit_id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let members = rows.iter().map(card).collect::<Result<Vec<_>, _>>()?;
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
        })?;
    group_detail(&state.pool, &id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "group not found".into()))
}
#[cfg(test)]
mod tests {
    use super::*;
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
}
