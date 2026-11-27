use crate::api::{ApiError, AppState};
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder, Row};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
/// One change: which resource (and occurrence) changed, when, how and in which fields. Clients
/// read the current facts from the resource itself.
pub struct ChangeLine {
    pub seq: i64,
    pub resource_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<String>,
    /// Unix milliseconds.
    pub at: i64,
    pub class: String,
    pub fields: Vec<String>,
    /// For an `aliased` line: the root that answers for this resource from now on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct ChangePage {
    pub lines: Vec<ChangeLine>,
    pub next_cursor: i64,
    pub has_more: bool,
    pub oldest_cursor: i64,
}
/// `since`: a string (RFC 3339, Unix milliseconds as text, or `now-30d`) or an integer (Unix ms).
fn since_schema() -> utoipa::openapi::schema::Schema {
    use utoipa::openapi::schema::{
        KnownFormat, ObjectBuilder, OneOfBuilder, Schema, SchemaFormat, Type,
    };
    Schema::OneOf(
        OneOfBuilder::new()
            .description(Some(
                "Inclusive lower bound on the change time, as a string or as Unix milliseconds",
            ))
            .item(
                ObjectBuilder::new()
                    .schema_type(Type::String)
                    .description(Some(
                        "RFC 3339, Unix milliseconds as text, or relative to the request: now, now-Nd, now-Nh, now+Nd or now+Nh (the current minute); a later page keeps the first page's window",
                    )),
            )
            .item(
                ObjectBuilder::new()
                    .schema_type(Type::Integer)
                    .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int64)))
                    .description(Some("Unix milliseconds")),
            )
            .build(),
    )
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeLookup {
    /// Resource IDs; with `uris`, 1–500 together.
    pub resource_ids: Option<Vec<String>>,
    /// Links, read as `POST /v1/resources/lookup` reads them; an unknown link adds nothing.
    pub uris: Option<Vec<String>>,
    /// RFC3339, Unix milliseconds (number or text), or relative to the request (`now-30d`).
    /// Without it, every retained change of these resources, in pages.
    #[schema(schema_with = since_schema, required = false)]
    pub since: Option<Value>,
    pub cursor: Option<i64>,
    #[schema(minimum = 1, maximum = 500)]
    pub limit: Option<usize>,
}
/// The changes to `ids` since `since`, after `cursor`. Only what a line serves is read: the
/// journal's own columns, never the two documents a line holds.
async fn query(
    state: &AppState,
    cursor: Option<i64>,
    limit: Option<usize>,
    ids: Vec<String>,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Json<ChangePage>, ApiError> {
    let cursor = cursor.unwrap_or(0);
    let limit = limit.unwrap_or(100);
    if cursor < 0 || !(1..=500).contains(&limit) {
        return Err(ApiError::bad("invalid change cursor or limit"));
    }
    let mut sql=QueryBuilder::<Postgres>::new("WITH head AS (SELECT coalesce(min(seq)-1,0) AS oldest,coalesce(max(seq),0) AS newest FROM resource_change), filtered AS (SELECT c.seq,c.resource_id,c.occurrence_key,c.changed_at,c.class,c.fields,CASE WHEN c.class='aliased' THEN c.after->>'root' END AS root FROM resource_change c WHERE c.seq>");
    sql.push_bind(cursor);
    // An alias's changes are its root's, beside its own (the line that made it one).
    sql.push(" AND c.resource_id IN (SELECT x FROM unnest(")
        .push_bind(ids)
        .push("::text[]) w(id) LEFT JOIN resource_alias a ON a.alias_id=w.id CROSS JOIN LATERAL (VALUES (w.id),(a.root_id)) v(x) WHERE x IS NOT NULL)");
    sql.push(" AND c.changed_at>=").push_bind(since);
    sql.push(" ORDER BY seq LIMIT ").push_bind((limit+1) as i64).push(") SELECT head.*,coalesce((SELECT jsonb_agg(jsonb_build_object('seq',seq,'resource_id',resource_id,'class',class,'fields',fields,'at',floor(extract(epoch FROM changed_at)*1000)::bigint,'occurrence',occurrence_key,'root',root) ORDER BY seq) FROM filtered c),'[]') AS lines FROM head");
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
/// A relative `since` on a later lookup page: the first page's window, capped at the cursor
/// line's time less one second. Concurrent inserts can commit `seq` and `clock_timestamp()`
/// microseconds out of order, so a line just after the cursor may be slightly older than it;
/// the second only widens the window, and the `seq` filter still prevents duplicates. At the
/// earliest representable instant the cap is the cursor's time itself, so it cannot panic.
pub fn capped_since(
    since: chrono::DateTime<chrono::Utc>,
    cursor_at: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    since.min(
        cursor_at
            .checked_sub_signed(chrono::Duration::seconds(1))
            .unwrap_or(cursor_at),
    )
}
#[utoipa::path(post,tag="Changes",path="/v1/changes/lookup",operation_id="lookup_changes",request_body=ChangeLookup,responses((status=200,body=ChangePage),(status=400,description="Invalid IDs or since"),(status=410,description="Cursor expired")))]
pub async fn lookup(
    State(state): State<AppState>,
    Json(body): Json<ChangeLookup>,
) -> Result<Json<ChangePage>, ApiError> {
    // No `since`: everything retained (the journal keeps `limits.change_retention_days`).
    let since = match &body.since {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|n| n.to_string()))
                .ok_or_else(|| {
                    ApiError::bad("since must be RFC3339, Unix milliseconds or relative (now-30d)")
                })?,
        ),
    };
    let relative = since.as_deref().is_some_and(|s| s.starts_with("now"));
    let mut since = match since {
        Some(since) => crate::spatial::timestamp_at(&since, state.now())
            .map_err(|e| ApiError::bad(e.to_string()))?,
        None => chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
    };
    let ids = crate::api::lookup_ids(body.resource_ids, body.uris)?;
    // A relative `since` is the window start of the first page. A later page continues after
    // its cursor, a line at or past that start, so cap the window at the cursor line's time:
    // re-resolving `now` would otherwise skip lines from the minutes between the pages.
    if relative && body.cursor.is_some_and(|c| c > 0) {
        let cursor_at: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT changed_at FROM resource_change WHERE seq=$1")
                .bind(body.cursor)
                .fetch_optional(&state.pool)
                .await?;
        if let Some(at) = cursor_at {
            since = capped_since(since, at);
        }
    }
    query(&state, body.cursor, body.limit, ids, since).await
}
