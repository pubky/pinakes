use crate::{
    api::{ApiError, AppState},
    changes::CLASSES,
    model,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder, Row};
use utoipa::{IntoParams, ToSchema};

#[derive(Clone, Debug, Default, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ChangeQuery {
    pub cursor: Option<i64>,
    pub class: Option<String>,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub source: Option<String>,
    #[param(minimum = 1, maximum = 500)]
    pub limit: Option<usize>,
}
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ChangeLine {
    pub seq: i64,
    pub resource_id: String,
    pub at: i64,
    pub class: String,
    pub fields: Vec<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct ChangePage {
    pub lines: Vec<ChangeLine>,
    pub next_cursor: i64,
    pub has_more: bool,
    pub oldest_cursor: i64,
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeLookup {
    pub resource_ids: Vec<String>,
    pub since: Value,
    pub cursor: Option<i64>,
    #[schema(minimum = 1, maximum = 500)]
    pub limit: Option<usize>,
}
async fn query(
    state: &AppState,
    q: ChangeQuery,
    ids: Option<Vec<String>>,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Json<ChangePage>, ApiError> {
    let cursor = q.cursor.unwrap_or(0);
    let limit = q.limit.unwrap_or(100);
    if cursor < 0
        || !(1..=500).contains(&limit)
        || q.class.as_deref().is_some_and(|c| !CLASSES.contains(&c))
        || q.source.as_ref().is_some_and(|s| s.len() > 128)
    {
        return Err(ApiError::bad(
            "invalid change cursor, limit, class or source",
        ));
    }
    let type_name = q
        .type_name
        .as_ref()
        .map(|t| {
            model::type_path(t)
                .pop()
                .ok_or_else(|| ApiError::bad("unknown schema.org type"))
        })
        .transpose()?;
    let mut sql=QueryBuilder::<Postgres>::new("WITH head AS (SELECT coalesce(min(seq)-1,0) AS oldest,coalesce(max(seq),0) AS newest FROM resource_change), filtered AS (SELECT * FROM change_document c WHERE c.seq>");
    sql.push_bind(cursor);
    if let Some(class) = q.class {
        sql.push(" AND c.class=").push_bind(class);
    }
    if let Some(source) = q.source {
        sql.push(" AND c.source=").push_bind(source);
    }
    if let Some(name) = type_name {
        sql.push(" AND c.type_path ? ").push_bind(name);
    }
    if let Some(ids) = ids {
        sql.push(" AND c.resource_id=ANY(").push_bind(ids).push(")");
    }
    if let Some(since) = since {
        sql.push(" AND c.at>=").push_bind(since);
    }
    sql.push(" ORDER BY seq LIMIT ").push_bind((limit+1) as i64).push(") SELECT head.*,coalesce((SELECT jsonb_agg((to_jsonb(c)-'type_path'-'occurrence_key'-'at') || jsonb_build_object('at',floor(extract(epoch FROM at)*1000)::bigint,'occurrence',occurrence_key) ORDER BY seq) FROM filtered c),'[]') AS lines FROM head");
    let row = sql.build().fetch_one(&state.pool).await?;
    let oldest: i64 = row.try_get("oldest")?;
    if cursor > 0 && cursor < oldest {
        return Err(ApiError(
            StatusCode::GONE,
            "change cursor expired; restart with cursor=0 and refresh your resources".into(),
        ));
    }
    let mut lines: Vec<ChangeLine> = serde_json::from_value(row.try_get("lines")?)?;
    let more = lines.len() > limit;
    lines.truncate(limit);
    let next_cursor = if more {
        lines.last().map_or(cursor, |l| l.seq)
    } else {
        row.try_get::<i64, _>("newest")?.max(cursor)
    };
    Ok(Json(ChangePage {
        lines,
        next_cursor,
        has_more: more,
        oldest_cursor: oldest,
    }))
}
#[utoipa::path(get,tag="Changes",path="/v1/changes",params(ChangeQuery),responses((status=200,body=ChangePage),(status=400,description="Invalid filter"),(status=410,description="Cursor no longer retained")))]
pub async fn feed(
    State(state): State<AppState>,
    Query(q): Query<ChangeQuery>,
) -> Result<Json<ChangePage>, ApiError> {
    query(&state, q, None, None).await
}
#[utoipa::path(post,tag="Changes",path="/v1/changes/lookup",request_body=ChangeLookup,responses((status=200,body=ChangePage),(status=400,description="Invalid IDs or since"),(status=410,description="Cursor expired")))]
pub async fn lookup(
    State(state): State<AppState>,
    Json(body): Json<ChangeLookup>,
) -> Result<Json<ChangePage>, ApiError> {
    if !(1..=500).contains(&body.resource_ids.len())
        || body.resource_ids.iter().any(|s| !model::valid_id(s))
    {
        return Err(ApiError::bad(
            "resource_ids requires 1–500 valid resource IDs",
        ));
    }
    let since = body
        .since
        .as_str()
        .map(str::to_owned)
        .or_else(|| body.since.as_i64().map(|n| n.to_string()))
        .ok_or_else(|| ApiError::bad("since must be RFC3339 or Unix milliseconds"))?;
    let since = crate::spatial::timestamp(&since).map_err(|e| ApiError::bad(e.to_string()))?;
    query(
        &state,
        ChangeQuery {
            cursor: body.cursor,
            limit: body.limit,
            ..Default::default()
        },
        Some(body.resource_ids),
        Some(since),
    )
    .await
}
#[utoipa::path(get,tag="Changes",path="/v1/resources/{id}/changes",params(("id"=String,Path,description="Resource ID"),ChangeQuery),responses((status=200,body=ChangePage),(status=400,description="Invalid resource ID"),(status=410,description="Cursor expired")))]
pub async fn resource(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ChangeQuery>,
) -> Result<Json<ChangePage>, ApiError> {
    if !model::valid_id(&id) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    query(&state, q, Some(vec![id]), None).await
}
