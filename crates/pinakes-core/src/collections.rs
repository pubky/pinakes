//! The public collections that hold a link (TASK-in-collections): the collection posts Nexus
//! indexed whose items name a card's link, its own or an alias's. They come from the reference
//! feed's `collection` lines, one per item (`ingest`), stored in `reference` with kind
//! `collection`; a `del` line, for a deleted collection, a removed item or an edit's old version,
//! drops its row. Pinakes never fetches a collection post: it lists what Nexus saw, and a client
//! hydrates names and counts from Nexus. Served by `GET /v1/resources/{id}/collections`, counted
//! on every card (`collections`), and a search filter (`in_collection`) (docs/api.md, Collections).
use crate::api::{ApiError, AppState, Card};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;

/// The most collections counted for one card: a card with more says this many.
pub const COUNTED: i64 = 1000;
/// The default and largest page of the route.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// The collection post that a `collection` reference's file belongs to: `pubky://<author>` and
/// the file's path, a post's version file (`/pub/<ns>/v1/posts/<id>/<editId>.json`) read as its
/// post.
pub fn uri(author: &str, object_path: &str) -> String {
    let path = match object_path.rsplit_once('/') {
        Some((post, file)) if versioned(post) && file.ends_with(".json") => post,
        _ => object_path,
    };
    format!("pubky://{author}{path}")
}
/// Whether `path` is an app's post (`/pub/<ns>/v1/posts/<id>`), whose files are its versions.
fn versioned(path: &str) -> bool {
    matches!(path.split('/').collect::<Vec<_>>()[..],
        ["", "pub", ns, "v1", "posts", id] if !ns.is_empty() && !id.is_empty())
}
/// A collection's URI (an item's `uri`) as the references that name it are stored: its author
/// and the path of the post, and whether the post's version files name it too.
pub(crate) fn parse(collection: &str) -> Option<(String, String, bool)> {
    let rest = collection.strip_prefix("pubky://")?;
    let (author, path) = rest.split_at(rest.find('/')?);
    (crate::nexus::valid_author(author)
        && path.starts_with("/pub/")
        && path.len() <= 2048
        && !path.ends_with('/')
        && !path.contains(['\0', '?', '#']))
    .then(|| (author.to_owned(), path.to_owned(), versioned(path)))
}
/// Pushes the condition that a reference `f` is an item of the collection whose author, post path
/// and versioning are `parse`'s: a reference by its file, the post's own or (for an app's post)
/// one of its version files, `<path>/<editId>.json`.
pub(crate) fn push_items(
    sql: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>,
    (author, path, versioned): (String, String, bool),
) {
    sql.push("f.kind='collection' AND f.author=")
        .push_bind(author)
        .push(" AND (f.object_path=")
        .push_bind(path.clone());
    if versioned {
        // Characters, as `left` and `substr` count them.
        let prefix = format!("{path}/");
        let n = prefix.chars().count() as i32;
        sql.push(" OR (left(f.object_path,")
            .push_bind(n)
            .push(")=")
            .push_bind(prefix)
            .push(" AND strpos(substr(f.object_path,")
            .push_bind(n + 1)
            .push("),'/')=0 AND right(f.object_path,5)='.json')");
    }
    sql.push(")");
}

/// Fills each card's `collections`: the collection posts holding its own link or an alias's,
/// each once, counted up to [`COUNTED`]. One query for every card of a response.
pub(crate) async fn count<'a>(
    pool: &PgPool,
    cards: impl IntoIterator<Item = &'a mut Card>,
) -> Result<(), ApiError> {
    let mut cards: Vec<&mut Card> = cards.into_iter().collect();
    let ids: Vec<String> = cards.iter().map(|c| c.id.clone()).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT c.id,(SELECT count(*) FROM (SELECT DISTINCT f.author,f.object_path FROM reference f \
           WHERE f.kind='collection' AND f.resource_id IN (SELECT c.id UNION ALL SELECT a.alias_id FROM resource_alias a WHERE a.root_id=c.id) LIMIT $2) x) \
         FROM (SELECT DISTINCT id FROM unnest($1::text[]) id) c",
    )
    .bind(&ids)
    .bind(COUNTED)
    .fetch_all(pool)
    .await?;
    for card in cards.iter_mut() {
        card.collections = counts
            .iter()
            .find(|(id, _)| *id == card.id)
            .map_or(0, |(_, n)| *n);
    }
    Ok(())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct CollectionQuery {
    /// A user's public key (z-base-32): that user's collections come first.
    pub viewer: Option<String>,
    /// 1–100, default 20.
    pub limit: Option<usize>,
    /// The page's `cursor`: signed, bound to this resource and `viewer`; invalid after a restart
    /// (start again on HTTP 400).
    pub cursor: Option<String>,
}
/// A public collection post that holds the card's link.
#[derive(Debug, Serialize, ToSchema)]
pub struct Collection {
    /// The collection post (`pubky://<author>/pub/<ns>/v1/posts/<id>`). Ask Nexus or the
    /// homeserver for its name and items.
    pub uri: String,
    /// Its author's public key.
    pub author: String,
    /// When Nexus indexed the version that holds the link: an edit moves it up.
    pub indexed_at: DateTime<Utc>,
}
#[derive(Serialize, ToSchema)]
pub struct CollectionPage {
    /// The viewer's own first, then newest first.
    pub items: Vec<Collection>,
    /// The next page's cursor; `null` on the last page.
    pub cursor: Option<String>,
}
#[derive(Deserialize, Serialize)]
struct PageCursor {
    scope: String,
    viewer: Option<String>,
    own: bool,
    at: DateTime<Utc>,
    author: String,
    path: String,
}

#[utoipa::path(get,path="/v1/resources/{id}/collections",operation_id="list_collections",tag="Previews",params(("id"=String,Path,description="32 lowercase hex resource ID"),CollectionQuery),responses((status=200,body=CollectionPage),(status=400,description="Invalid resource ID, viewer, limit or cursor"),(status=404,description="Resource not found")),description="The public collection posts that hold this card's link, its own or an alias's, as Nexus indexed them: the viewer's own first when `viewer` is given, then newest first. Each once, however many of the card's links it holds. A card's `collections` counts them. Private collections are never seen.")]
pub async fn list(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<CollectionQuery>,
) -> Result<axum::Json<CollectionPage>, ApiError> {
    if !crate::model::valid_id(&id) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    if q.viewer
        .as_deref()
        .is_some_and(|v| !crate::nexus::valid_author(v))
    {
        return Err(ApiError::bad("viewer must be a public key"));
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::bad("limit must be 1–100"));
    }
    let root: Option<String> = sqlx::query_scalar("SELECT r.id FROM resource r WHERE r.id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    let Some(root) = root else {
        return Err(ApiError(
            axum::http::StatusCode::NOT_FOUND,
            "resource not found".into(),
        ));
    };
    let cursor = q
        .cursor
        .as_deref()
        .map(|c| decode(c, &state.cursor_mac))
        .transpose()?;
    if cursor
        .as_ref()
        .is_some_and(|c| c.scope != root || c.viewer != q.viewer)
    {
        return Err(ApiError::bad(
            "cursor belongs to another resource or viewer",
        ));
    }
    let rows: Vec<(bool, DateTime<Utc>, String, String)> = sqlx::query_as(
        "WITH links AS (SELECT $1::text AS id UNION ALL SELECT alias_id FROM resource_alias WHERE root_id=$1), \
         posts AS (SELECT f.author,f.object_path,max(f.created_at) AS at FROM reference f JOIN links l ON l.id=f.resource_id \
           WHERE f.kind='collection' GROUP BY f.author,f.object_path), \
         ranked AS (SELECT (author=$2) IS TRUE AS own,at,author,object_path FROM posts) \
         SELECT own,at,author,object_path FROM ranked \
         WHERE $3::boolean IS NULL OR (NOT own AND $3) OR (own=$3 AND (at<$4 OR (at=$4 AND (author,object_path)>($5,$6)))) \
         ORDER BY own DESC,at DESC,author,object_path LIMIT $7",
    )
    .bind(&root)
    .bind(&q.viewer)
    .bind(cursor.as_ref().map(|c| c.own))
    .bind(cursor.as_ref().map(|c| c.at))
    .bind(cursor.as_ref().map(|c| &c.author))
    .bind(cursor.as_ref().map(|c| &c.path))
    .bind((limit + 1) as i64)
    .fetch_all(&state.pool)
    .await?;
    let more = rows.len() > limit;
    let rows = &rows[..rows.len().min(limit)];
    let cursor = match rows.last() {
        Some((own, at, author, path)) if more => Some(encode(
            &PageCursor {
                scope: root,
                viewer: q.viewer.clone(),
                own: *own,
                at: *at,
                author: author.clone(),
                path: path.clone(),
            },
            &state.cursor_mac,
        )?),
        _ => None,
    };
    let items = rows
        .iter()
        .map(|(_, at, author, path)| Collection {
            uri: uri(author, path),
            author: author.clone(),
            indexed_at: *at,
        })
        .collect();
    Ok(axum::Json(CollectionPage { items, cursor }))
}
fn encode(cursor: &PageCursor, key: &hmac::SimpleHmac<blake3::Hasher>) -> anyhow::Result<String> {
    use base64::Engine;
    use hmac::Mac;
    let payload = serde_json::to_vec(cursor)?;
    let mut mac = key.clone();
    mac.update(&payload);
    let mut signed = mac.finalize().into_bytes().to_vec();
    signed.extend(payload);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signed))
}
fn decode(s: &str, key: &hmac::SimpleHmac<blake3::Hasher>) -> Result<PageCursor, ApiError> {
    use base64::Engine;
    use hmac::Mac;
    let invalid = || ApiError::bad("invalid cursor");
    if s.len() > 8192 {
        return Err(invalid());
    }
    let signed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| invalid())?;
    if signed.len() <= 32 {
        return Err(invalid());
    }
    let (tag, payload) = signed.split_at(32);
    let mut mac = key.clone();
    mac.update(payload);
    mac.verify_slice(tag).map_err(|_| invalid())?;
    serde_json::from_slice(payload).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collections_uri_is_its_post_and_reads_back() {
        let author = "y".repeat(52);
        let social = uri(
            &author,
            "/pub/social/v1/posts/0035XTZFQH4P2/0035XTZFQH4P2.json",
        );
        assert_eq!(
            social,
            format!("pubky://{author}/pub/social/v1/posts/0035XTZFQH4P2")
        );
        assert_eq!(
            parse(&social),
            Some((
                author.clone(),
                "/pub/social/v1/posts/0035XTZFQH4P2".into(),
                true
            ))
        );
        // A file that is no post version is its own collection.
        let other = uri(&author, "/pub/pubky.app/posts/0035XTZFQH4P2");
        assert_eq!(
            other,
            format!("pubky://{author}/pub/pubky.app/posts/0035XTZFQH4P2")
        );
        assert_eq!(
            parse(&other),
            Some((
                author.clone(),
                "/pub/pubky.app/posts/0035XTZFQH4P2".into(),
                false
            ))
        );
        for bad in [
            "https://example.org/pub/x".to_owned(),
            format!("pubky://{}/pub/x", "y".repeat(51)),
            format!("pubky://{author}/priv/x"),
            format!("pubky://{author}/pub/x/"),
            format!("pubky://{author}/pub/x?a=1"),
            format!("pubky://{author}"),
        ] {
            assert_eq!(parse(&bad), None, "{bad}");
        }
    }
}
