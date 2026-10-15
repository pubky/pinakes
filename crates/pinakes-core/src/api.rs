use crate::{
    config::Config,
    model::{self, ResourceMeta},
    nexus::Nexus,
    store,
};
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
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
    limiter: Arc<Mutex<HashMap<String, (Instant, usize)>>>,
}
impl AppState {
    pub fn new(pool: PgPool, config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            pool,
            nexus: Nexus::new(&config.nexus_url)?,
            config,
            wake: Arc::default(),
            limiter: Arc::default(),
        })
    }
    async fn preview_budget(&self, ip: std::net::IpAddr, id: &str) -> Result<(), ApiError> {
        let mut limits = self.limiter.lock().await;
        limits.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
        let keys = [
            (format!("ip:{ip}"), self.config.preview.requests_per_minute),
            (
                format!("uri:{id}"),
                self.config.preview.uri_requests_per_minute,
            ),
        ];
        for (key, limit) in &keys {
            if limits.get(key).is_some_and(|(_, n)| n >= limit)
                || (!limits.contains_key(key) && limits.len() >= self.config.preview.cache_capacity)
            {
                return Err(ApiError(
                    StatusCode::TOO_MANY_REQUESTS,
                    "preview rate limit exceeded".into(),
                ));
            }
        }
        for (key, _) in keys {
            limits.entry(key).or_insert_with(|| (Instant::now(), 0)).1 += 1;
        }
        Ok(())
    }
}
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/info", get(info))
        .route("/v1/search", get(search))
        .route("/v1/resources/lookup", post(lookup))
        .route("/v1/resources/{id}", get(resource))
        .route("/v1/resolve", post(resolve))
        .route("/v1/openapi.json", get(openapi))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}
#[derive(Debug)]
pub struct ApiError(StatusCode, String);
impl ApiError {
    fn bad(s: impl Into<String>) -> Self {
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
        if status == StatusCode::TOO_MANY_REQUESTS {
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
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub facet: Option<String>,
    pub fields: Option<Fields>,
    pub bbox: Option<String>,
    pub near: Option<String>,
    pub radius: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cluster: Option<String>,
    pub venue: Option<String>,
    pub collapse: Option<String>,
}
#[derive(Deserialize, Serialize)]
struct PageCursor {
    version: u8,
    score: f64,
    id: String,
    at: DateTime<Utc>,
    filter: String,
}
#[derive(Serialize, ToSchema)]
pub struct ResourceView {
    pub resource_id: String,
    pub uri: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub site: String,
    pub status: String,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_path: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<model::Point>,
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

fn view(row: &Value, fields: Fields) -> Result<ResourceView, ApiError> {
    let doc: Option<ResourceMeta> = if row["doc"].get("type").is_some() {
        Some(serde_json::from_value(row["doc"].clone())?)
    } else {
        None
    };
    let facets = !matches!(fields, Fields::Card);
    Ok(ResourceView {
        resource_id: row["id"].as_str().unwrap_or_default().into(),
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
        score: row["rank"].as_f64().unwrap_or(0.0),
        type_path: facets.then(|| {
            doc.as_ref()
                .map(|d| d.type_path.clone())
                .unwrap_or_default()
        }),
        category: facets
            .then(|| doc.as_ref().and_then(|d| d.category.clone()))
            .flatten(),
        point: facets
            .then(|| doc.as_ref().and_then(|d| d.location.clone()))
            .flatten(),
        start: facets.then(|| doc.as_ref().and_then(|d| d.start)).flatten(),
        end: facets.then(|| doc.as_ref().and_then(|d| d.end)).flatten(),
        document: if matches!(fields, Fields::Document) {
            doc
        } else {
            None
        },
    })
}
fn validate_search(q: &SearchQuery) -> Result<Vec<String>, ApiError> {
    for (name, value) in [
        ("bbox", &q.bbox),
        ("near", &q.near),
        ("radius", &q.radius),
        ("from", &q.from),
        ("to", &q.to),
        ("cluster", &q.cluster),
        ("venue", &q.venue),
        ("collapse", &q.collapse),
    ] {
        if value.is_some() {
            return Err(ApiError::bad(format!(
                "{name} is not available in this release"
            )));
        }
    }
    if !(1..=500).contains(&q.limit.unwrap_or(50)) {
        return Err(ApiError::bad("limit must be 1–500"));
    }
    if !matches!(q.sort.as_deref(), None | Some("activity" | "recent")) {
        return Err(ApiError::bad(
            "sort must be activity or recent; distance/start are not available",
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
#[utoipa::path(get,path="/v1/search",params(SearchQuery),responses((status=200,body=SearchResponse),(status=400,description="Invalid or unavailable filter"),(status=502,description="Nexus reach unavailable")))]
pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    let tags = validate_search(&q)?;
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
    let filter = blake3::hash(&serde_json::to_vec(&fingerprint)?)
        .to_hex()
        .to_string();
    let cursor = if let Some(s) = &q.cursor {
        if s.len() > 2048 {
            return Err(ApiError::bad("invalid cursor"));
        }
        let c: PageCursor = URL_SAFE_NO_PAD
            .decode(s)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| ApiError::bad("invalid cursor"))?;
        if c.version != 1
            || c.filter != filter
            || !c.score.is_finite()
            || !model::valid_id(&c.id)
            || c.at > Utc::now() + chrono::Duration::seconds(5)
        {
            return Err(ApiError::bad("cursor does not match this search"));
        }
        Some(c)
    } else {
        None
    };
    let at = cursor.as_ref().map_or_else(Utc::now, |c| c.at);
    let mut sql = QueryBuilder::<Postgres>::new("WITH filtered AS (SELECT r.*, ");
    if q.sort.as_deref() == Some("recent") {
        sql.push("coalesce(extract(epoch FROM s.newest_at),0)::double precision");
    } else {
        sql.push("coalesce(s.score * exp(-ln(2.0)*greatest(0,extract(epoch FROM (")
            .push_bind(at)
            .push("::timestamptz-s.updated_at)))/2592000.0),0)::double precision");
    }
    sql.push(
        " AS rank FROM resource r LEFT JOIN resource_score s ON s.resource_id=r.id WHERE true",
    );
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
    sql.push(" AND EXISTS (SELECT 1 FROM reference f WHERE f.resource_id=r.id");
    scope(&mut sql, &q, &reach);
    sql.push(")");
    if !tags.is_empty() {
        if q.match_mode.as_deref() == Some("all") {
            for tag in &tags {
                sql.push(
                    " AND EXISTS (SELECT 1 FROM reference f WHERE f.resource_id=r.id AND f.label=",
                )
                .push_bind(tag.clone());
                scope(&mut sql, &q, &reach);
                sql.push(")");
            }
        } else {
            sql.push(
                " AND EXISTS (SELECT 1 FROM reference f WHERE f.resource_id=r.id AND f.label=ANY(",
            )
            .push_bind(tags)
            .push(")");
            scope(&mut sql, &q, &reach);
            sql.push(")");
        }
    }
    sql.push("), page AS (SELECT * FROM filtered");
    if let Some(c) = &cursor {
        sql.push(" WHERE (rank,id)<(")
            .push_bind(c.score)
            .push(",")
            .push_bind(c.id.clone())
            .push(")");
    }
    sql.push(" ORDER BY rank DESC,id DESC LIMIT ").push_bind((q.limit.unwrap_or(50)+1) as i64).push(") SELECT coalesce((SELECT jsonb_agg(to_jsonb(p) ORDER BY rank DESC,id DESC) FROM page p),'[]') AS items, ");
    if q.facet.as_deref() == Some("tags") {
        sql.push("coalesce((SELECT jsonb_agg(to_jsonb(t) ORDER BY count DESC,label) FROM (SELECT f.label,count(DISTINCT f.resource_id) AS count FROM reference f JOIN filtered r ON r.id=f.resource_id WHERE f.label IS NOT NULL");
        scope(&mut sql, &q, &reach);
        sql.push(" GROUP BY f.label) t),'[]')");
    } else {
        sql.push("'[]'::jsonb");
    }
    sql.push(" AS facets");
    let row = sql.build().fetch_one(&state.pool).await?;
    let mut items: Vec<Value> = serde_json::from_value(row.try_get("items")?)?;
    let has_more = items.len() > q.limit.unwrap_or(50);
    items.truncate(q.limit.unwrap_or(50));
    let next = if has_more {
        let last = items.last().expect("nonempty page");
        Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&PageCursor {
            version: 1,
            score: last["rank"].as_f64().expect("rank"),
            id: last["id"].as_str().expect("id").into(),
            at,
            filter,
        })?))
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
    Ok(Json(SearchResponse {
        items: items
            .iter()
            .map(|r| view(r, q.fields.unwrap_or(Fields::Card)))
            .collect::<Result<_, _>>()?,
        cursor: next,
        reach_truncated: reach.is_some_and(|r| r.truncated),
        facets,
    }))
}
async fn read(pool: &PgPool, id: &str, fields: Fields) -> Result<Option<ResourceView>, ApiError> {
    let value: Option<Value> = sqlx::query_scalar("SELECT to_jsonb(r) FROM resource r WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    value.as_ref().map(|r| view(r, fields)).transpose()
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
    read(&state.pool, &id, fields.fields.unwrap_or(Fields::Document))
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
    let rows:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(r) FROM unnest($1::text[]) WITH ORDINALITY AS wanted(id,n) JOIN resource r ON r.id=wanted.id ORDER BY wanted.n").bind(ids).fetch_all(&state.pool).await?;
    Ok(Json(
        rows.iter()
            .map(|r| view(r, fields.fields.unwrap_or(Fields::Facets)))
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
    let resolver = crate::resolver::classify(&uri)
        .ok_or_else(|| ApiError::bad("no resolver available for this URI"))?;
    if (resolver == "web" && !state.config.web.enabled)
        || (resolver == "pubky" && !state.config.pubky.enabled)
    {
        return Err(ApiError::bad("resolver is disabled"));
    }
    state.preview_budget(peer.ip(), &id).await?;
    store::ensure_resource(&state.pool, &uri).await?;
    state.wake.notify_waiters();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let resource = read(&state.pool, &id, Fields::Document)
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
#[utoipa::path(get,path="/v1/info",responses((status=200,body=Value)))]
pub async fn info(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let row=sqlx::query("SELECT (SELECT count(*) FROM resource) AS resources,(SELECT count(*) FROM reference) AS references,(SELECT count(*) FROM occurrence) AS occurrences").fetch_one(&state.pool).await?;
    let ingestion:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('nexus',nexus,'cursor',cursor,'feed_length',feed_length,'lag',greatest(feed_length-cursor,0),'epoch',epoch,'updated_at',updated_at) FROM feed_cursor ORDER BY nexus").fetch_all(&state.pool).await?;
    let statuses:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('resolver',resolver,'status',status,'count',count(*),'dead_letter',count(*) FILTER (WHERE dead_letter),'leased',count(*) FILTER (WHERE lease_until>now())) FROM resource GROUP BY resolver,status ORDER BY resolver,status").fetch_all(&state.pool).await?;
    Ok(Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"ingestion":ingestion,"counts":{"resources":row.try_get::<i64,_>("resources")?,"references":row.try_get::<i64,_>("references")?,"occurrences":row.try_get::<i64,_>("occurrences")?},"resolvers":{"web":{"enabled":state.config.web.enabled,"concurrency":state.config.web.concurrency},"pubky":{"enabled":state.config.pubky.enabled,"concurrency":state.config.pubky.concurrency},"statuses":statuses}}),
    ))
}
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Pinakes",
        version = "0.1.0",
        description = "Pinakes ranks, Nexus hydrates. External resource metadata and search.",
        license(name = "MIT")
    ),
    paths(search, resource, lookup, resolve, info, openapi),
    components(schemas(
        ResourceMeta,
        model::Point,
        model::Status,
        ResourceView,
        SearchResponse,
        Fields,
        LookupRequest,
        ResolveRequest,
        TagFacet,
        Facets
    ))
)]
pub struct ApiDoc;
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
#[utoipa::path(get,path="/v1/openapi.json",responses((status=200,body=Value)))]
async fn openapi() -> Json<Value> {
    Json(serde_json::to_value(openapi_document()).expect("OpenAPI serialization"))
}
