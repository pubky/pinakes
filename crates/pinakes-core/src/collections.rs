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
/// The most aliases of one card whose collections count and are listed: those most recently
/// collected, each found by its newest collection reference alone (as `refs` bounds its aliases).
pub const ALIASES: i64 = 31;
/// The default and largest page of the route.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
/// How many of the viewer's own collections of a card come first: their newest. Any more are
/// listed with the others, by time, so the others' walk steps over this many posts at most.
pub const PINNED: usize = 100;

/// The collection post that a `collection` reference's file belongs to: `pubky://<author>` and
/// the file's [`post`].
pub fn uri(author: &str, object_path: &str) -> String {
    format!("pubky://{author}{}", post(object_path))
}
/// The collection a collection reference's file belongs to: a post's version file
/// (`/pub/<ns>/v1/posts/<id>/<leaf>.json`, no further `/`) belongs to its post, any other file is
/// a collection of its own (a path nested deeper in a post, or not JSON, is never its version).
/// The schema's `collection_post(object_path)` is the same function, in SQL.
fn post(object_path: &str) -> &str {
    match object_path.rsplit_once('/') {
        Some((post, file)) if versioned(post) && file.ends_with(".json") => post,
        _ => object_path,
    }
}
/// A file's name within its collection ([`post`]): a version file's leaf, `""` for any other file.
/// The post and the leaf are the path less one `/`, so an index of both holds the path once. The
/// schema's `collection_file(object_path)` is the same function, in SQL; only SQL orders by it, so
/// this twin is the test's, which checks they agree.
#[cfg(test)]
fn file(object_path: &str) -> &str {
    match object_path.rsplit_once('/') {
        Some((post, file)) if versioned(post) && file.ends_with(".json") => file,
        _ => "",
    }
}
/// Whether `path` is an app's post (`/pub/<ns>/v1/posts/<id>`), whose files are its versions.
fn versioned(path: &str) -> bool {
    matches!(path.split('/').collect::<Vec<_>>()[..],
        ["", "pub", ns, "v1", "posts", id] if !ns.is_empty() && !id.is_empty())
}
/// A collection's URI (an item's `uri`) as the references that name it are stored: its author
/// and its [`post`] (a version file's URI names its post).
pub(crate) fn parse(collection: &str) -> Option<(String, String)> {
    let rest = collection.strip_prefix("pubky://")?;
    let (author, path) = rest.split_at(rest.find('/')?);
    (crate::nexus::valid_author(author)
        && path.starts_with("/pub/")
        && path.len() <= 2048
        && !path.ends_with('/')
        && !path.contains(['\0', '?', '#']))
    .then(|| (author.to_owned(), post(path).to_owned()))
}
/// SQL for the current file of the collection whose author and [`post`] are the expressions
/// `author` and `post`: of its files, the one with the newest reference, and of files equally new
/// the greatest name in byte order (the specs' newest editId: fixed-width timestamp IDs; a
/// collection's files share its post, so their [`file`] leaves order them so). One probe of
/// `reference_collection_post_idx`, which orders a collection's references so; `NULL`
/// when it has none. `settle` keeps this file alone, so readers find one file per collection.
fn current(author: &str, post: &str) -> String {
    format!(
        "(SELECT g.object_path FROM reference g WHERE g.kind='collection' AND g.author={author} AND collection_post(g.object_path)={post} \
         ORDER BY timezone('UTC',g.created_at) DESC,collection_file(g.object_path) COLLATE \"C\" DESC LIMIT 1)"
    )
}
/// Pushes the condition that a reference `f` is an item of the collection whose author and post
/// are `parse`'s: a collection reference of that post, whose one file ingest keeps (`settle`).
/// Read along `reference_collection_post_idx`: the collection's own references, never every
/// reference of its author.
pub(crate) fn push_items(
    sql: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>,
    (author, post): (String, String),
) {
    sql.push("f.kind='collection' AND f.author=")
        .push_bind(author)
        .push(" AND collection_post(f.object_path)=")
        .push_bind(post);
}

/// The most items one collection post holds: the specs' cap (`collection_items_max_count`).
/// Ingest refuses a new item past it (`collection_items`).
pub const ITEMS: i64 = 100;
/// Whether the collection file `object_path` of `author` already holds [`ITEMS`] items. Reads at
/// most that many of its rows along the key.
pub(crate) async fn full(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    author: &str,
    object_path: &str,
) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT count(*)>=$3 FROM (SELECT 1 FROM reference WHERE author=$1 AND object_path=$2 AND kind='collection' LIMIT $3) f")
        .bind(author)
        .bind(object_path)
        .bind(ITEMS)
        .fetch_one(&mut **tx)
        .await
}
/// Brings a collection's references in line after a put of its file `object_path`, towards the
/// state Nexus's stage for that version leaves, and returns the resources whose references
/// changed. Nexus stages a post's version as one batch: a del for each line it drops (every line
/// of the older version, when the post's version file changed, and removed items), then a put
/// for each of its items, all at the stage's instant. So after a put of the file:
/// - the file's rows all take its newest instant: an older row is an item whose put at that
///   instant follows in this stage, or was skipped, or was replayed late (or one whose del was
///   skipped: it was listed before, and stays listed);
/// - of the collection's files ([`post`]), the [`current`] one alone is kept: the others are
///   older versions whose every line was deleted, or a del skipped, or a version replayed late.
///
/// So a collection never stays at two instants or under two files, whatever order its lines
/// arrive in, and readers take one file per collection. What settle cannot recover: an item
/// whose put was skipped is missing until the post is staged again, and an item whose del was
/// skipped stays listed (docs/api.md, Collections). Reads at most [`ITEMS`] rows of the file,
/// along the key, and the collection's files along `reference_collection_post_idx`: its current
/// file and this one.
pub(crate) async fn settle(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    author: &str,
    object_path: &str,
) -> sqlx::Result<Vec<String>> {
    let mut changed: Vec<String> = sqlx::query_scalar(
        "UPDATE reference r SET created_at=m.at FROM (SELECT f.created_at AS at FROM reference f \
           WHERE f.author=$1 AND f.object_path=$2 AND f.kind='collection' ORDER BY timezone('UTC',f.created_at) DESC LIMIT 1) m \
         WHERE r.author=$1 AND r.object_path=$2 AND r.kind='collection' AND r.created_at<>m.at RETURNING r.resource_id",
    )
    .bind(author)
    .bind(object_path)
    .fetch_all(&mut **tx)
    .await?;
    changed.extend(
        sqlx::query_scalar::<_, String>(&format!(
            "DELETE FROM reference WHERE kind='collection' AND author=$1 AND collection_post(object_path)=$2 \
             AND object_path<>{} RETURNING resource_id",
            current("$1", "$2")
        ))
        .bind(author)
        .bind(post(object_path))
        .fetch_all(&mut **tx)
        .await?,
    );
    Ok(changed)
}

/// SQL for the links of the card whose ID is the expression `card`: its own and its [`ALIASES`]
/// most recently collected aliases, as rows of `link`. An alias's newest collection is one probe
/// in the full order of `reference_resource_collection_idx`, which only that index gives: never a
/// walk through an alias's tags.
fn links(card: &str) -> String {
    let aliases = ALIASES;
    format!(
        "SELECT {card} AS link UNION ALL (SELECT a.alias_id FROM resource_alias a CROSS JOIN LATERAL \
           (SELECT f.created_at FROM reference f WHERE f.resource_id COLLATE \"C\"=a.alias_id AND f.kind='collection' AND f.object_path<>'' ORDER BY timezone('UTC',f.created_at) DESC,f.author COLLATE \"C\",f.object_path COLLATE \"C\" LIMIT 1) n \
         WHERE a.root_id={card} ORDER BY n.created_at DESC,a.alias_id LIMIT {aliases})"
    )
}
/// SQL for the post that follows (`at`, `author`, `path`) among link `link`'s collection
/// references, in the order they are listed (newest first, then author and file), as one row
/// (`created_at`, `author`, `object_path`) or none. Two probes, each an `ORDER BY … LIMIT 1` in
/// the order of `reference_resource_collection_idx` (`resource_id, timezone('UTC', created_at)
/// DESC, author, object_path`, link, author and file in the "C" collation) and bounded on it: a
/// later post of the same instant (an equality on the instant and a range on author and file),
/// else the next older one. Only that index can serve these probes: they compare in its
/// collation, which no index of the default collation (the key by author and file, the
/// resource's by time) has, they compare the instant as its expression, which no index of
/// `created_at` itself (`trending`'s) has, and they state its predicate `object_path <> ''`,
/// which the viewer's index does not have. A step
/// of a walk is one probe or two, whatever the planner thinks of the link's size, and a post named twice in a link (two of
/// its spellings) is stepped over. `filter` narrows the references; a row it drops is stepped
/// over within the probe, so a filter must drop few (the viewer's pinned window). Nothing else
/// is filtered in a walk.
fn next(link: &str, at: &str, author: &str, path: &str, filter: &str) -> String {
    let from = format!(
        "SELECT f.created_at,f.author,f.object_path FROM reference f WHERE f.resource_id COLLATE \"C\"={link} AND f.kind='collection' AND f.object_path<>''{filter}"
    );
    format!(
        "(SELECT s.* FROM (({from} AND timezone('UTC',f.created_at)=timezone('UTC',{at}) AND (f.author COLLATE \"C\",f.object_path COLLATE \"C\")>({author},{path}) \
         ORDER BY timezone('UTC',f.created_at) DESC,f.author COLLATE \"C\",f.object_path COLLATE \"C\" LIMIT 1) UNION ALL ({from} AND timezone('UTC',f.created_at)<timezone('UTC',{at}) \
         ORDER BY timezone('UTC',f.created_at) DESC,f.author COLLATE \"C\",f.object_path COLLATE \"C\" LIMIT 1)) s LIMIT 1)"
    )
}
/// [`next`] among the viewer's own references of `link` (`q.viewer`), down to the pinned
/// boundary (`q.bt`, `q.bp`: the viewer's [`PINNED`]th newest post; `-infinity` when they hold
/// fewer), along `reference_collection_author_idx` (`resource_id, author, timezone('UTC',
/// created_at) DESC, object_path`, in the "C" collation, for references with `author <> ''`, which only these probes
/// state: no other probe reads it, and they read no other index). In listing order, four ranges of that index, each an `ORDER BY … LIMIT 1`:
/// a later file of the same instant (bounded by `q.bp` when the instant is the boundary's), the
/// next older instant above the boundary, then the boundary's own instant up to `q.bp`. Which
/// ranges apply is decided once per probe from the position (`at`), never row by row, so a probe
/// reads no row past the boundary however large a tie, and at the boundary every range is empty.
fn next_own(link: &str, at: &str, path: &str) -> String {
    let from = format!(
        "SELECT f.created_at,f.author,f.object_path FROM reference f WHERE f.resource_id COLLATE \"C\"={link} AND f.kind='collection' \
         AND f.author<>'' AND f.author COLLATE \"C\"=(SELECT viewer FROM q)"
    );
    let (bt, bp) = ("(SELECT bt FROM q)", "(SELECT bp FROM q)");
    format!(
        "(SELECT s.* FROM (\
         ({from} AND {at}>{bt} AND timezone('UTC',f.created_at)=timezone('UTC',{at}) AND f.object_path COLLATE \"C\">{path} ORDER BY timezone('UTC',f.created_at) DESC,f.object_path COLLATE \"C\" LIMIT 1) \
         UNION ALL ({from} AND {at}={bt} AND timezone('UTC',f.created_at)=timezone('UTC',{at}) AND f.object_path COLLATE \"C\">{path} AND f.object_path COLLATE \"C\"<={bp} ORDER BY timezone('UTC',f.created_at) DESC,f.object_path COLLATE \"C\" LIMIT 1) \
         UNION ALL ({from} AND timezone('UTC',f.created_at)<timezone('UTC',{at}) AND timezone('UTC',f.created_at)>timezone('UTC',{bt}) ORDER BY timezone('UTC',f.created_at) DESC,f.object_path COLLATE \"C\" LIMIT 1) \
         UNION ALL ({from} AND {bt}<{at} AND timezone('UTC',f.created_at)=timezone('UTC',{bt}) AND f.object_path COLLATE \"C\"<={bp} ORDER BY timezone('UTC',f.created_at) DESC,f.object_path COLLATE \"C\" LIMIT 1)\
         ) s LIMIT 1)"
    )
}
/// SQL for a recursive walk `name` (`link`, `k`, `at`, `author`, `path`): from each row of
/// `start` (a `link` with the position to start after: `at`, `author`, `path`), the next `n`
/// posts of the link, each the row `step` (over a link, `at`, `author` and `path` expressions)
/// returns.
fn walk(
    name: &str,
    start: &str,
    n: &str,
    step: impl Fn(&str, &str, &str, &str) -> String,
) -> String {
    format!(
        "{name}(link,k,at,author,path) AS (SELECT b.link,1,s.* FROM {start} b CROSS JOIN LATERAL {} s \
         UNION ALL SELECT w.link,w.k+1,s.* FROM {name} w CROSS JOIN LATERAL {} s WHERE w.k<{n})",
        step("b.link", "b.at", "b.author", "b.path"),
        step("w.link", "w.at", "w.author", "w.path"),
    )
}
/// The count of every card in `$1` (IDs), up to `$2` ([`COUNTED`]), over its [`links`]: each
/// link walked from its newest post, at most `$2` posts. A link holding that many already makes
/// the count, and otherwise each link's walk is whole. Each file the walks found is one
/// collection (ingest keeps a collection at one file, `settle`) and counts once, until `$2`
/// have. So a card costs at most `$2` steps per link, however many collections hold it.
fn count_sql() -> String {
    format!(
        "WITH RECURSIVE cards AS (SELECT DISTINCT id FROM unnest($1::text[]) id), \
         starts AS (SELECT c.id AS card,l.link,'infinity'::timestamptz AS at,''::text AS author,''::text AS path \
           FROM cards c CROSS JOIN LATERAL ({}) l), \
         {} \
         SELECT c.id,(SELECT count(*) FROM (SELECT DISTINCT p.author,p.path FROM walk p JOIN starts s ON s.link=p.link WHERE s.card=c.id LIMIT $2) y) FROM cards c",
        links("c.id"),
        walk(
            "walk",
            "(SELECT DISTINCT link,at,author,path FROM starts)",
            "$2",
            |l, a, w, p| next(l, a, w, p, "")
        ),
    )
}

/// Fills each card's `collections`: the collection posts holding its own link or one of its
/// [`ALIASES`] most recently collected aliases', each once, counted up to [`COUNTED`]
/// (`count_sql`). One query for every card of a response.
pub(crate) async fn count<'a>(
    pool: &PgPool,
    cards: impl IntoIterator<Item = &'a mut Card>,
) -> Result<(), ApiError> {
    let mut cards: Vec<&mut Card> = cards.into_iter().collect();
    let ids: Vec<String> = cards.iter().map(|c| c.id.clone()).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let counts: Vec<(String, i64)> = sqlx::query_as(&count_sql())
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
    /// A user's public key (z-base-32): that user's collections come first, their 100 newest;
    /// any more are listed with the others.
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
    /// The viewer's own first (their 100 newest), then newest first.
    pub items: Vec<Collection>,
    /// The next page's cursor; `null` on the last page.
    pub cursor: Option<String>,
}
/// A post's place in the listing: its instant and file.
type Place = (DateTime<Utc>, String);
/// A row of a page ([`push_page`]): whether the post is one of the viewer's pinned, its instant,
/// author and file.
type PageRow = (bool, DateTime<Utc>, String, String);
/// The viewer's pinned posts as the first page found them: the window from the instant of their
/// newest post to their [`PINNED`]th. Later pages keep it, so no post moves between the viewer's
/// own and the others while a client pages, and the others' walk steps over this window alone.
#[derive(Clone, Default, Deserialize, Serialize)]
struct Pinned {
    /// Their newest post's instant, the window's top: `None` when they held none. Its instant
    /// alone: a post of theirs added later is newer, unless Nexus appended it later within that
    /// millisecond (a late arrival inside the window, docs/api.md).
    top: Option<DateTime<Utc>>,
    /// Their [`PINNED`]th newest, the window's end: `None` when they held fewer.
    boundary: Option<Place>,
}
/// A page's cursor: where the page ended and the viewer's pinned window, signed. At most two
/// paths (the place's and the boundary's), as bytes ([`encode`]), so the longest fits
/// [`MAX_CURSOR`].
#[derive(Deserialize, Serialize)]
struct PageCursor {
    scope: String,
    viewer: Option<String>,
    own: bool,
    at: DateTime<Utc>,
    author: String,
    path: String,
    /// With a viewer: their pinned window as the first page found it.
    pinned: Pinned,
}
/// The longest cursor the route reads: one of the longest paths ingest accepts
/// (`max_object_path_bytes`, at most 2,048) twice, with the rest, encoded, is under 6,000.
const MAX_CURSOR: usize = 8192;

#[utoipa::path(get,path="/v1/resources/{id}/collections",operation_id="list_collections",tag="Previews",params(("id"=String,Path,description="32 lowercase hex resource ID"),CollectionQuery),responses((status=200,body=CollectionPage),(status=400,description="Invalid resource ID, viewer, limit or cursor"),(status=404,description="Resource not found")),description="The public collection posts that hold this card's link, its own or an alias's, as Nexus indexed them: the viewer's own first when `viewer` is given (their 100 newest), then newest first. Each once, however many of the card's links it holds. A card's `collections` counts them. Private collections are never seen.")]
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
    // The first page reads the viewer's pinned window and the page from one snapshot; later
    // pages take the window from their cursor.
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let pinned = match (&q.viewer, &cursor) {
        (Some(_), Some(c)) => c.pinned.clone(),
        (Some(viewer), None) => {
            let mut sql = sqlx::QueryBuilder::new("");
            push_pinned(&mut sql, root.clone(), viewer.clone());
            pinned(sql.build_query_as().fetch_all(&mut *tx).await?)
        }
        (None, _) => Pinned::default(),
    };
    let mut sql = sqlx::QueryBuilder::new("");
    push_page(
        &mut sql,
        root.clone(),
        q.viewer.clone(),
        pinned.clone(),
        cursor.as_ref(),
        limit + 1,
    );
    let rows: Vec<PageRow> = sql.build_query_as().fetch_all(&mut *tx).await?;
    tx.commit().await?;
    // The page is the first `limit` places, and the cursor is the last of them.
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
                pinned,
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
/// Pushes the head of a page query: `q` (`root`, `viewer`, `n`, the pinned window's top `tt` and
/// its boundary `bt`, `bp`) and the card's `links`.
fn push_head(
    sql: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>,
    root: String,
    viewer: Option<String>,
    n: usize,
    pinned: Pinned,
) {
    let tt = pinned.top;
    let (bt, bp) = pinned.boundary.unzip();
    sql.push("WITH RECURSIVE q AS (SELECT ")
        .push_bind(root)
        .push("::text AS root,")
        .push_bind(viewer)
        .push("::text AS viewer,")
        .push_bind(n as i64)
        .push("::bigint AS n,coalesce(")
        .push_bind(tt)
        .push("::timestamptz,'-infinity') AS tt,coalesce(")
        .push_bind(bt)
        .push("::timestamptz,'-infinity') AS bt,coalesce(")
        .push_bind(bp)
        .push("::text,'') AS bp), links AS (SELECT l.link FROM q CROSS JOIN LATERAL (")
        .push(links("q.root"))
        .push(") l)");
}
/// Pushes the query for the viewer's pinned window: their newest post and their [`PINNED`]th
/// newest among `root`'s links (`at`, file), in that order, as many as they hold. At most
/// [`PINNED`] steps per link.
fn push_pinned(sql: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>, root: String, viewer: String) {
    push_head(sql, root, Some(viewer), PINNED, Pinned::default());
    sql.push(
        ", own_start AS (SELECT link,'infinity'::timestamptz AS at,''::text AS author,''::text AS path FROM links), ",
    )
    .push(walk("own", "own_start", "(SELECT n FROM q)", |l, a, _, p| {
        next_own(l, a, p)
    }))
    .push(" SELECT at,path FROM (SELECT at,path,row_number() OVER (ORDER BY at DESC,path COLLATE \"C\") AS i \
      FROM (SELECT path,max(at) AS at FROM own GROUP BY path) x) y WHERE i IN (1,")
    .push_bind(PINNED as i64)
    .push(") ORDER BY i");
}
/// The pinned window from [`push_pinned`]'s rows.
fn pinned(rows: Vec<Place>) -> Pinned {
    let mut rows = rows.into_iter();
    Pinned {
        top: rows.next().map(|(at, _)| at),
        boundary: rows.next(),
    }
}
/// Pushes one page of `root`'s collections, `n` places of them: the viewer's pinned posts (their
/// [`PINNED`] newest, `pinned` the window from [`push_pinned`]), then the others, each newest
/// first, as [`PageRow`]s. Each of the card's [`links`] is walked from
/// the cursor on ([`walk`]), at most `n` posts: the page's posts are among them (a link's earlier
/// posts are earlier on the page too), so a page costs at most `n` steps per link, however deep
/// it is and however many collections hold the card or its viewer made. The others' walk steps
/// over the viewer's pinned window alone; a segment the cursor is past is not walked.
///
/// Every place is its collection's one file at its one instant, as ingest keeps them (`settle`),
/// so a post two of the card's links name is one place and none is left out.
fn push_page(
    sql: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>,
    root: String,
    viewer: Option<String>,
    pinned: Pinned,
    cursor: Option<&PageCursor>,
    n: usize,
) {
    // Where each segment starts: after the cursor in its own segment, else at the top.
    // The top is no instant at all (`infinity` in SQL).
    let top = || (None, String::new(), String::new());
    let after = |c: &PageCursor| (Some(c.at), c.author.clone(), c.path.clone());
    let own_from = match cursor {
        None => Some(top()),
        Some(c) if c.own => Some(after(c)),
        Some(_) => None,
    }
    .filter(|_| viewer.is_some());
    let others_from = match cursor {
        Some(c) if !c.own => after(c),
        _ => top(),
    };
    // The others are every post but the viewer's pinned window: from their newest post's instant
    // as the first page found it (`tt`) to their boundary (`bt`, `bp`). A post they add or edit
    // later is newer than the window, and listed with the others. So the others' walk steps over
    // the window alone: at most [`PINNED`] posts of at most [`ITEMS`] rows each, as the first page
    // found them.
    let others_filter = if viewer.is_some() {
        " AND NOT (f.author=(SELECT viewer FROM q) \
         AND timezone('UTC',f.created_at)<=timezone('UTC',(SELECT tt FROM q)) \
         AND (timezone('UTC',f.created_at)>timezone('UTC',(SELECT bt FROM q)) OR timezone('UTC',f.created_at)=timezone('UTC',(SELECT bt FROM q)) AND f.object_path COLLATE \"C\"<=(SELECT bp FROM q)))"
    } else {
        ""
    };
    push_head(sql, root, viewer, n, pinned);
    sql.push(", others_start AS (SELECT l.link,coalesce(")
        .push_bind(others_from.0)
        .push("::timestamptz,'infinity') AS at,")
        .push_bind(others_from.1)
        .push("::text AS author,")
        .push_bind(others_from.2)
        .push("::text AS path FROM links l), ");
    let own = own_from.is_some();
    if let Some((at, author, path)) = own_from {
        sql.push("own_start AS (SELECT l.link,coalesce(")
            .push_bind(at)
            .push("::timestamptz,'infinity') AS at,")
            .push_bind(author)
            .push("::text AS author,")
            .push_bind(path)
            .push("::text AS path FROM links l), ")
            .push(walk(
                "own",
                "own_start",
                "(SELECT n FROM q)",
                |l, a, _, p| next_own(l, a, p),
            ))
            .push(", ");
    }
    sql.push(walk(
        "others",
        "others_start",
        "(SELECT n FROM q)",
        |l, a, w, p| next(l, a, w, p, others_filter),
    ))
    .push(" SELECT own,max(at) AS at,author,path FROM (");
    if own {
        sql.push("SELECT true AS own,at,author,path FROM own UNION ALL ");
    }
    sql.push("SELECT false AS own,at,author,path FROM others) x GROUP BY own,author,path ORDER BY own DESC,max(at) DESC,author COLLATE \"C\",path COLLATE \"C\" LIMIT (SELECT n FROM q)");
}
fn encode(cursor: &PageCursor, key: &hmac::SimpleHmac<blake3::Hasher>) -> anyhow::Result<String> {
    use base64::Engine;
    use hmac::Mac;
    // Bytes, not JSON: a path is its length and bytes, never escaped.
    let payload = postcard::to_allocvec(cursor)?;
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
    if s.len() > MAX_CURSOR {
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
    postcard::from_bytes(payload).map_err(|_| invalid())
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
            Some((author.clone(), "/pub/social/v1/posts/0035XTZFQH4P2".into()))
        );
        // A version file's URI names its post.
        assert_eq!(
            parse(&format!("{social}/0035XTZFQH4P3.json")),
            parse(&social)
        );
        // A file that is no post version is its own collection.
        let other = uri(&author, "/pub/pubky.app/posts/0035XTZFQH4P2");
        assert_eq!(
            other,
            format!("pubky://{author}/pub/pubky.app/posts/0035XTZFQH4P2")
        );
        assert_eq!(
            parse(&other),
            Some((author.clone(), "/pub/pubky.app/posts/0035XTZFQH4P2".into()))
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

    /// The schema's `collection_post` is [`post`]: a post's version files belong to it, a path
    /// nested deeper in a post or not JSON is a collection of its own.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../tests/fixtures/require_h3.sql")
    )]
    async fn the_schema_reads_a_files_collection_as_post_does(pool: PgPool) {
        let paths = [
            "/pub/social/v1/posts/P/V.json",
            "/pub/social/v1/posts/P/V-label.json",
            "/pub/social/v1/posts/P/.json",
            "/pub/social/v1/posts/P",
            "/pub/social/v1/posts/P/notes",
            "/pub/social/v1/posts/P/x/y.json",
            "/pub/social/v1/posts/P/V.json/w.json",
            "/pub/social/v2/posts/P/V.json",
            "/pub/social/v1/post/P/V.json",
            "/pub//v1/posts/P/V.json",
            "/pub/social/v1/posts//V.json",
            "/pub/ünï/v1/posts/ß/ö.json",
            "/pub/social/v1/posts/P/V.JSON",
            "/pub/pubky.app/posts/P",
            "/pub/a/b/c/v1/posts/P/V.json",
        ];
        for path in paths {
            let (sql, leaf): (String, String) =
                sqlx::query_as("SELECT collection_post($1),collection_file($1)")
                    .bind(path)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(sql, post(path), "{path}");
            assert_eq!(leaf, file(path), "{path}");
            // The post and the leaf are the path, less one `/`.
            match leaf.as_str() {
                "" => assert_eq!(sql, path),
                leaf => assert_eq!(format!("{sql}/{leaf}"), path),
            }
        }
        assert_eq!(post(paths[0]), "/pub/social/v1/posts/P");
        assert_eq!(post(paths[4]), paths[4]);
        assert_eq!(post(paths[5]), paths[5]);
    }

    /// A cursor whose every field is as long as the API accepts (paths of
    /// `max_object_path_bytes`, of the characters JSON would escape six-fold or of four bytes
    /// each) fits the decoder's cap and reads back as it was written; anything longer, or not
    /// signed by this key, is refused.
    #[test]
    fn the_longest_cursor_fits_and_reads_back() {
        use hmac::KeyInit;
        let key = hmac::SimpleHmac::<blake3::Hasher>::new_from_slice(b"key").unwrap();
        let max = crate::config::IngestConfig::default().max_object_path_bytes;
        let at: DateTime<Utc> = "9999-12-31T23:59:59.999999Z".parse().unwrap();
        for unit in ["\u{1}", "\"", "𝄞"] {
            let path = unit.repeat(max / unit.len());
            assert_eq!(path.len(), max);
            let cursor = PageCursor {
                scope: "f".repeat(32),
                viewer: Some("z".repeat(52)),
                own: true,
                at,
                author: "z".repeat(52),
                path: path.clone(),
                pinned: Pinned {
                    top: Some(at),
                    boundary: Some((at, path)),
                },
            };
            let encoded = encode(&cursor, &key).unwrap();
            assert!(encoded.len() < 6_000, "{unit:?}: {}", encoded.len());
            let read = decode(&encoded, &key).unwrap();
            assert_eq!(encode(&read, &key).unwrap(), encoded);
            assert_eq!(read.pinned.boundary, cursor.pinned.boundary);
            let other = hmac::SimpleHmac::<blake3::Hasher>::new_from_slice(b"other").unwrap();
            assert!(decode(&encoded, &other).is_err());
        }
        assert!(decode(&"A".repeat(MAX_CURSOR + 1), &key).is_err());
    }

    /// What a statement did with `reference`: the rows its scans of the table produced or
    /// discarded (by filter or recheck) over every loop, the buffers the whole statement touched
    /// (index pages too, which no row count shows), the indexes it read the table by, and whether
    /// it scanned the table sequentially or sorted a scan of it.
    #[derive(Debug, Default)]
    struct Work {
        rows: i64,
        buffers: i64,
        indexes: std::collections::BTreeSet<String>,
        seq: bool,
        sorted: bool,
    }
    fn work(plan: &serde_json::Value) -> Work {
        fn walk(node: &serde_json::Value, w: &mut Work) {
            let n = |k: &str| node[k].as_f64().unwrap_or(0.0);
            if node["Relation Name"] == "reference" {
                w.rows += ((n("Actual Rows")
                    + n("Rows Removed by Filter")
                    + n("Rows Removed by Index Recheck"))
                    * n("Actual Loops")) as i64;
                w.seq |= node["Node Type"] == "Seq Scan";
            }
            if let Some(index) = node["Index Name"].as_str() {
                if index.starts_with("reference_") {
                    w.indexes.insert(index.to_owned());
                }
            }
            let children: Vec<&serde_json::Value> =
                node["Plans"].as_array().into_iter().flatten().collect();
            if node["Node Type"] == "Sort" || node["Node Type"] == "Incremental Sort" {
                w.sorted |= children.iter().any(|c| c["Relation Name"] == "reference");
            }
            for child in children {
                walk(child, w);
            }
        }
        let top = &plan[0]["Plan"];
        let mut w = Work {
            buffers: top["Shared Hit Blocks"].as_i64().unwrap()
                + top["Shared Read Blocks"].as_i64().unwrap(),
            ..Work::default()
        };
        walk(top, &mut w);
        w
    }
    async fn explained(pool: &PgPool, mut sql: sqlx::QueryBuilder<'_, sqlx::Postgres>) -> Work {
        let plan: serde_json::Value = sql.build_query_scalar().fetch_one(pool).await.unwrap();
        work(&plan)
    }
    fn explain() -> sqlx::QueryBuilder<'static, sqlx::Postgres> {
        sqlx::QueryBuilder::new("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ")
    }
    const MAIN: &str = "reference_resource_collection_idx";
    const OWN: &str = "reference_collection_author_idx";
    const POST: &str = "reference_collection_post_idx";
    /// The indexes a page may read: the walk's and, with a viewer, the viewer's.
    fn page_indexes(viewer: bool) -> std::collections::BTreeSet<String> {
        [MAIN]
            .into_iter()
            .chain(viewer.then_some(OWN))
            .map(str::to_owned)
            .collect()
    }

    /// A card 20,000 collections hold, with 40 collected aliases (one tagged 20,000 times since); a card one author put in
    /// 20,000 collections at 100 instants (200 each, ties throughout); and an author of 20,000
    /// collections of other links. The count, pages (the first, deep ones, in a tie, the viewer's
    /// own, and the viewer who holds everything) and a search for one collection each read a
    /// bounded number of rows and buffers along the walk's indexes, never sorting a scan of the
    /// table or reading a link's or an author's every collection. Twice: with these links alone,
    /// whose statistics say a link holds hundreds, and with 20,000 links more held once each,
    /// whose statistics say a link holds one (the planner's guess, wrong either way for these
    /// cards, chooses nothing here: each step of a walk is an `ORDER BY … LIMIT 1` along an index).
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../tests/fixtures/require_h3.sql")
    )]
    async fn every_collections_query_reads_a_bounded_walk(pool: PgPool) {
        // Seeded on one connection without triggers: what they keep up (grouping, scores) plays
        // no part here, and 60,000 rows of it would take a minute.
        let mut seed = pool.acquire().await.unwrap();
        for statement in [
            "SET session_replication_role = replica",
            "INSERT INTO resource(id,uri) VALUES (repeat('0',32),'https://example.org/root'),(repeat('1',32),'https://example.org/one-author')",
            "INSERT INTO resource(id,uri) SELECT md5('r'||n),'https://example.org/'||n FROM generate_series(1,40) n",
            "INSERT INTO resource(id,uri) SELECT md5('x'||n),'https://example.org/x'||n FROM generate_series(1,40) n",
            "INSERT INTO resource_alias(alias_id,root_id,basis) SELECT md5('r'||n),repeat('0',32),'canonical' FROM generate_series(1,40) n",
            // The card's own link, in 20,000 posts of as many authors, one a second (the
            // viewer's 3 among them).
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT repeat('0',32),'collection',CASE WHEN n%6000=0 THEN repeat('v',52) ELSE lpad(n::text,52,'y') END,'/pub/social/v1/posts/P'||n||'/P'||n||'.json',timestamptz '2026-01-01'+n*interval '1 second','collection:'||md5(n::text) FROM generate_series(1,20000) n",
            // Each alias in two posts.
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT md5('r'||a),'collection',lpad(a::text,52,'w'),'/pub/social/v1/posts/A'||a||'-'||k||'/A.json',timestamptz '2026-06-01'+(a*2+k)*interval '1 second','collection:'||md5(a::text) FROM generate_series(1,40) a,generate_series(1,2) k",
            // One alias tagged 20,000 times since its collections: its newest collection is one
            // probe, never a walk through its tags.
            "INSERT INTO reference(resource_id,kind,author,label,object_path,created_at) SELECT md5('r1'),'tag',lpad(n::text,52,'t'),'music','/pub/app/tags/'||n,timestamptz '2026-07-01'+n*interval '1 second' FROM generate_series(1,20000) n",
            // One author's 20,000 collections, of other links.
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT md5('x'||(1+n%40)),'collection',repeat('x',52),'/pub/social/v1/posts/X'||n||'/X'||n||'.json',timestamptz '2026-01-01'+n*interval '1 second','collection:'||md5(n::text) FROM generate_series(1,20000) n",
            // One author's 20,000 collections of one link, all at one instant.
            "INSERT INTO resource(id,uri) VALUES (repeat('2',32),'https://example.org/one-instant'),(repeat('3',32),'https://example.org/two-links'),(md5('a3'),'https://example.org/two-links-alias')",
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT repeat('2',32),'collection',repeat('d',52),'/pub/social/v1/posts/D'||lpad(n::text,5,'0')||'/D.json',timestamptz '2026-02-01','collection:'||md5(n::text) FROM generate_series(0,19999) n",
            // 20,000 posts each holding a card and its alias, at one instant per post (as ingest
            // settles them).
            "INSERT INTO resource_alias(alias_id,root_id,basis) VALUES (md5('a3'),repeat('3',32),'canonical')",
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT l.id,'collection',lpad(n::text,52,'s'),'/pub/social/v1/posts/S'||n||'/S.json',timestamptz '2026-03-01'+n*interval '1 second','collection:'||md5(l.id) FROM generate_series(1,20000) n,(VALUES (repeat('3',32)),(md5('a3'))) l(id)",
            // One author's 20,000 collections of one link, 200 at each of 100 instants.
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT repeat('1',32),'collection',repeat('c',52),'/pub/social/v1/posts/C'||lpad(n::text,5,'0')||'/C.json',timestamptz '2026-01-01'+(n/200)*interval '1 hour','collection:'||md5(n::text) FROM generate_series(0,19999) n",
            "RESET session_replication_role",
        ] {
            sqlx::query(statement).execute(&mut *seed).await.unwrap();
        }
        sqlx::query("VACUUM ANALYZE reference")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("ANALYZE resource_alias")
            .execute(&pool)
            .await
            .unwrap();
        check_bounded_walks(&pool, "few links").await;
        let mut seed = pool.acquire().await.unwrap();
        for statement in [
            "SET session_replication_role = replica",
            "INSERT INTO resource(id,uri) SELECT md5('m'||n),'https://example.org/m'||n FROM generate_series(1,20000) n",
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT md5('m'||n),'collection',lpad(n::text,52,'z'),'/pub/social/v1/posts/M'||n||'/M.json',timestamptz '2026-09-01','collection:m' FROM generate_series(1,20000) n",
            "RESET session_replication_role",
        ] {
            sqlx::query(statement).execute(&mut *seed).await.unwrap();
        }
        sqlx::query("VACUUM ANALYZE reference")
            .execute(&pool)
            .await
            .unwrap();
        check_bounded_walks(&pool, "many links").await;
    }
    fn cursor(
        card: &str,
        viewer: Option<&str>,
        own: bool,
        at: &str,
        author: &str,
        path: &str,
    ) -> PageCursor {
        PageCursor {
            scope: card.into(),
            viewer: viewer.map(str::to_owned),
            own,
            at: at.parse().unwrap(),
            author: author.into(),
            path: path.into(),
            pinned: Pinned::default(),
        }
    }
    async fn check_bounded_walks(pool: &PgPool, phase: &str) {
        let root = "0".repeat(32);
        let one = "1".repeat(32);
        let (v, c) = ("v".repeat(52), "c".repeat(52));
        // The counts: the card's 1,000 newest posts, 2 per alias and each alias's newest; the
        // single author's 1,000 newest, through their ties.
        for card in [&root, &one] {
            let plan: serde_json::Value = sqlx::query_scalar(&format!(
                "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {}",
                count_sql()
            ))
            .bind(vec![card.clone()])
            .bind(COUNTED)
            .fetch_one(pool)
            .await
            .unwrap();
            let w = work(&plan);
            assert!(
                !w.seq && !w.sorted && w.rows <= 2_400 && w.buffers <= BUDGET_COUNT,
                "{phase}, count {card}: {w:?}"
            );
            assert_eq!(w.indexes, [MAIN.to_owned()].into(), "{phase}, count {card}");
            let counted: i64 =
                sqlx::query_scalar(&format!("SELECT count FROM ({}) c(id,count)", count_sql()))
                    .bind(vec![card.clone()])
                    .bind(COUNTED)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            assert_eq!(counted, COUNTED, "{phase}, {card}");
        }

        let post = |n: u32| format!("/pub/social/v1/posts/P{n}/P{n}.json");
        let tied = |n: u32| format!("/pub/social/v1/posts/C{n:05}/C.json");
        let deep = cursor(
            &root,
            None,
            false,
            "2026-01-01T04:10:00Z",
            &"y".repeat(52),
            "",
        );
        let mine = cursor(
            &root,
            Some(&v),
            true,
            "2026-01-01T03:20:00Z",
            &v,
            &post(12_000),
        );
        // The single author's card: a tie at 50 hours, past its 10,050th post.
        let in_tie = cursor(&one, None, false, "2026-01-03T02:00:00Z", &c, &tied(10_050));
        // The single author as viewer: past their 100 pinned posts, the rest of theirs are others.
        let pinned_end = cursor(
            &one,
            Some(&c),
            true,
            "2026-01-05T03:00:00Z",
            &c,
            &tied(19_899),
        );
        let others_deep = cursor(
            &one,
            Some(&c),
            false,
            "2026-01-03T02:00:00Z",
            &c,
            &tied(10_050),
        );
        let (two, three, d) = ("2".repeat(32), "3".repeat(32), "d".repeat(52));
        let big = |n: u32| format!("/pub/social/v1/posts/D{n:05}/D.json");
        let pair = |n: u32| format!("/pub/social/v1/posts/S{n}/S.json");
        // The one-instant card: its viewer's 100th post is the pinned boundary.
        let big_pinned_end = cursor(&two, Some(&d), true, "2026-02-01T00:00:00Z", &d, &big(99));
        let big_others_deep = cursor(
            &two,
            Some(&d),
            false,
            "2026-02-01T00:00:00Z",
            &d,
            &big(10_000),
        );
        let big_deep = cursor(&two, None, false, "2026-02-01T00:00:00Z", &d, &big(10_000));
        let pair_deep = cursor(
            &three,
            None,
            false,
            "2026-03-01T02:46:40Z",
            &format!("{:s>52}", 10_000),
            &pair(10_000),
        );
        for (name, card, viewer, cursor, first, own) in [
            (
                "one instant, viewer holds all",
                &two,
                Some(&d),
                None,
                big(0),
                21,
            ),
            (
                "one instant, past the pinned",
                &two,
                Some(&d),
                Some(&big_pinned_end),
                big(100),
                0,
            ),
            (
                "one instant, others deep",
                &two,
                Some(&d),
                Some(&big_others_deep),
                big(10_001),
                0,
            ),
            (
                "one instant, deep",
                &two,
                None,
                Some(&big_deep),
                big(10_001),
                0,
            ),
            ("two links", &three, None, None, pair(20_000), 0),
            (
                "two links, deep",
                &three,
                None,
                Some(&pair_deep),
                pair(9_999),
                0,
            ),
            (
                "first",
                &root,
                None,
                None,
                "/pub/social/v1/posts/A40-2/A.json".to_owned(),
                0,
            ),
            ("deep", &root, None, Some(&deep), post(14_999), 0),
            ("viewer", &root, Some(&v), None, post(18_000), 3),
            (
                "viewer's later",
                &root,
                Some(&v),
                Some(&mine),
                post(6_000),
                1,
            ),
            ("one author", &one, None, None, tied(19_800), 0),
            ("in a tie", &one, None, Some(&in_tie), tied(10_051), 0),
            ("viewer holds all", &one, Some(&c), None, tied(19_800), 21),
            (
                "past the pinned",
                &one,
                Some(&c),
                Some(&pinned_end),
                tied(19_900),
                0,
            ),
            (
                "others deep",
                &one,
                Some(&c),
                Some(&others_deep),
                tied(10_051),
                0,
            ),
        ] {
            let pinned = match viewer {
                Some(viewer) => {
                    let mut sql = explain();
                    push_pinned(&mut sql, card.clone(), viewer.clone());
                    let w = explained(pool, sql).await;
                    assert!(
                        !w.seq && !w.sorted && w.buffers <= BUDGET_PINNED,
                        "{phase}, {name}, pinned: {w:?}"
                    );
                    assert!(
                        w.indexes.is_subset(&page_indexes(true)),
                        "{phase}, {name}, pinned: {w:?}"
                    );
                    let mut sql = sqlx::QueryBuilder::new("");
                    push_pinned(&mut sql, card.clone(), viewer.clone());
                    pinned(sql.build_query_as().fetch_all(pool).await.unwrap())
                }
                None => Pinned::default(),
            };
            let mut sql = explain();
            push_page(
                &mut sql,
                card.clone(),
                viewer.cloned(),
                pinned.clone(),
                cursor,
                21,
            );
            let w = explained(pool, sql).await;
            assert!(
                !w.seq && !w.sorted && w.rows <= 400 && w.buffers <= BUDGET_PAGE,
                "{phase}, {name}: {w:?}"
            );
            assert!(
                w.indexes.is_subset(&page_indexes(viewer.is_some())),
                "{phase}, {name}: {w:?}"
            );
            let mut sql = sqlx::QueryBuilder::new("");
            push_page(&mut sql, card.clone(), viewer.cloned(), pinned, cursor, 21);
            let page: Vec<PageRow> = sql.build_query_as().fetch_all(pool).await.unwrap();
            assert_eq!(page.len(), 21, "{phase}, {name}");
            let posts: std::collections::BTreeSet<(&String, &String)> =
                page.iter().map(|r| (&r.2, &r.3)).collect();
            assert_eq!(posts.len(), 21, "{phase}, {name}: a post twice");
            assert_eq!(page[0].3, first, "{phase}, {name}");
            assert_eq!(page.iter().filter(|r| r.0).count(), own, "{phase}, {name}");
        }

        // A search for one of the prolific author's collections reads that post's files alone.
        let mut sql = explain();
        sql.push("SELECT f.resource_id FROM reference f WHERE ");
        let held = parse(&format!(
            "pubky://{}/pub/social/v1/posts/X15000",
            "x".repeat(52)
        ))
        .unwrap();
        push_items(&mut sql, held.clone());
        let w = explained(pool, sql).await;
        assert!(
            !w.seq && w.rows <= 5 && w.buffers <= 20,
            "{phase}, in_collection: {w:?}"
        );
        // The collection's references, along its own index.
        assert_eq!(
            w.indexes,
            [POST.to_owned()].into(),
            "{phase}, in_collection"
        );
        let mut sql = sqlx::QueryBuilder::new("SELECT count(*) FROM reference f WHERE ");
        push_items(&mut sql, held);
        let found: i64 = sql.build_query_scalar().fetch_one(pool).await.unwrap();
        assert_eq!(found, 1, "{phase}");
    }
    /// A card whose pages read a bounded walk however the table changes under them: a viewer
    /// with 150 posts holding a link starts paging, then adds 20,000 more. Pages from the first
    /// page's cursor, in the viewer's own segment and in the others', step over the first page's
    /// pinned window alone: the new posts are newer than it, listed with the others, never
    /// stepped over.
    #[sqlx::test(
        migrations = "../../migrations",
        fixtures("../tests/fixtures/require_h3.sql")
    )]
    async fn a_page_reads_a_bounded_walk_whatever_changed(pool: PgPool) {
        let card = "5".repeat(32);
        let (e, o) = ("e".repeat(52), "o".repeat(52));
        let mine = |n: u32| format!("/pub/social/v1/posts/E{n:05}/E.json");
        let mut seed = pool.acquire().await.unwrap();
        for statement in [
            "SET session_replication_role = replica",
            "INSERT INTO resource(id,uri) VALUES (repeat('5',32),'https://example.org/window')",
            // The viewer's 150 posts, a minute apart, and 300 of others between them.
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT repeat('5',32),'collection',repeat('e',52),'/pub/social/v1/posts/E'||lpad(n::text,5,'0')||'/E.json',timestamptz '2026-04-20'+n*interval '1 minute','collection:5' FROM generate_series(1,150) n",
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT repeat('5',32),'collection',repeat('o',52),'/pub/social/v1/posts/O'||lpad(n::text,5,'0')||'/O.json',timestamptz '2026-04-20'+n*interval '30 seconds','collection:5' FROM generate_series(1,300) n",
            "RESET session_replication_role",
        ] {
            sqlx::query(statement).execute(&mut *seed).await.unwrap();
        }
        sqlx::query("VACUUM ANALYZE reference")
            .execute(&pool)
            .await
            .unwrap();
        let mut sql = sqlx::QueryBuilder::new("");
        push_pinned(&mut sql, card.clone(), e.clone());
        let window = pinned(sql.build_query_as().fetch_all(&pool).await.unwrap());
        let at = |n: u32| {
            (
                timestamp("2026-04-20T00:00:00Z") + chrono::Duration::minutes(n as i64),
                mine(n),
            )
        };
        assert_eq!(window.top, Some(at(150).0));
        assert_eq!(window.boundary, Some(at(51)));
        // The viewer's 20,000 posts more, after their first page.
        let mut seed = pool.acquire().await.unwrap();
        for statement in [
            "SET session_replication_role = replica",
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT repeat('5',32),'collection',repeat('e',52),'/pub/social/v1/posts/N'||lpad(n::text,5,'0')||'/N.json',timestamptz '2026-05-01'+n*interval '1 second','collection:5' FROM generate_series(1,20000) n",
            "RESET session_replication_role",
        ] {
            sqlx::query(statement).execute(&mut *seed).await.unwrap();
        }
        sqlx::query("VACUUM ANALYZE reference")
            .execute(&pool)
            .await
            .unwrap();
        let (t100, p100) = at(100);
        let own_later = cursor(&card, Some(&e), true, &t100.to_rfc3339(), &e, &p100);
        // Past the window's end, with the others: an other's post, at 990 seconds; next is the
        // viewer's 16th post, past their window, at 960 seconds.
        let others_later = cursor(
            &card,
            Some(&e),
            false,
            "2026-04-20T00:16:30Z",
            &o,
            "/pub/social/v1/posts/O00033/O.json",
        );
        let newest = "/pub/social/v1/posts/N20000/N.json".to_owned();
        for (name, cursor, first, own) in [
            ("held cursor, own", &own_later, mine(99), 21),
            ("held cursor, others", &others_later, mine(16), 0),
        ] {
            let mut sql = explain();
            push_page(
                &mut sql,
                card.clone(),
                Some(e.clone()),
                window.clone(),
                Some(cursor),
                21,
            );
            let w = explained(&pool, sql).await;
            assert!(
                !w.seq && !w.sorted && w.rows <= 400 && w.buffers <= BUDGET_PAGE,
                "{name}: {w:?}"
            );
            assert!(w.indexes.is_subset(&page_indexes(true)), "{name}: {w:?}");
            let mut sql = sqlx::QueryBuilder::new("");
            push_page(
                &mut sql,
                card.clone(),
                Some(e.clone()),
                window.clone(),
                Some(cursor),
                21,
            );
            let page: Vec<PageRow> = sql.build_query_as().fetch_all(&pool).await.unwrap();
            assert_eq!(page.len(), 21, "{name}");
            let posts: std::collections::BTreeSet<String> =
                page.iter().map(|r| uri(&r.2, &r.3)).collect();
            assert_eq!(posts.len(), 21, "{name}: a post twice");
            assert_eq!(page[0].3, first, "{name}");
            assert_eq!(page.iter().filter(|r| r.0).count(), own, "{name}");
            assert!(!page.iter().any(|r| r.3 == newest) || own == 0, "{name}");
        }
    }
    fn timestamp(at: &str) -> DateTime<Utc> {
        at.parse().unwrap()
    }
    /// Buffers a count may touch: its walks are at most 1,000 steps a link (here a card and an
    /// alias each holding over 1,000). Its rows are the sharper bound: a probe that read an index
    /// range row by row reads tens of thousands.
    const BUDGET_COUNT: i64 = 24_000;
    /// Buffers the viewer's pinned boundary may touch: at most [`PINNED`] steps a link (here
    /// about 800).
    const BUDGET_PINNED: i64 = 1_200;
    /// Buffers a page of 21 may touch: at most 21 steps a link (here with 41 links).
    const BUDGET_PAGE: i64 = 2_000;
}
