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
/// one of its version files, `<path>/<editId>.json`. The version files are a range of the path in
/// byte order (`<path>/` up to `<path>0`, `0` following `/`), compared in the "C" collation as
/// `reference_collection_post_idx` orders them, whatever the database's own: an index walk over
/// the post's files alone, never over every reference of its author.
pub(crate) fn push_items(
    sql: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>,
    (author, path, versioned): (String, String, bool),
) {
    sql.push("f.kind='collection' AND f.author=")
        .push_bind(author)
        .push(" AND (f.object_path COLLATE \"C\"=")
        .push_bind(path.clone());
    if versioned {
        // Characters, as `substr` counts them.
        let n = path.chars().count() as i32 + 1;
        sql.push(" OR (f.object_path COLLATE \"C\">=")
            .push_bind(format!("{path}/"))
            .push(" AND f.object_path COLLATE \"C\"<")
            .push_bind(format!("{path}0"))
            .push(" AND strpos(substr(f.object_path,")
            .push_bind(n + 1)
            .push("),'/')=0 AND right(f.object_path,5)='.json')");
    }
    sql.push(")");
}

/// SQL for the links of the card whose ID is the expression `card`: its own and its [`ALIASES`]
/// most recently collected aliases, as rows of `link`.
fn links(card: &str) -> String {
    let aliases = ALIASES;
    format!(
        "SELECT {card} AS link UNION ALL (SELECT a.alias_id FROM resource_alias a CROSS JOIN LATERAL \
           (SELECT f.created_at FROM reference f WHERE f.resource_id=a.alias_id AND f.kind='collection' ORDER BY f.created_at DESC LIMIT 1) n \
         WHERE a.root_id={card} ORDER BY n.created_at DESC,a.alias_id LIMIT {aliases})"
    )
}
/// SQL for the post that follows (`at`, `author`, `path`) among link `link`'s collection
/// references, in the order they are listed (newest first, then author and file), as one row
/// (`created_at`, `author`, `object_path`) or none. Two probes of one index, each an
/// `ORDER BY … LIMIT 1` along it: a later post of the same instant, else the next older one. A
/// step of a walk is one probe or two, whatever the planner thinks of the link's size, and a post
/// named twice in a link (two of its spellings) is stepped over. `filter` narrows the references
/// (`f.author=…` reads `reference_collection_author_idx`, anything else
/// `reference_resource_collection_idx`).
fn next(link: &str, at: &str, author: &str, path: &str, filter: &str) -> String {
    let from = format!(
        "SELECT f.created_at,f.author,f.object_path FROM reference f WHERE f.resource_id={link} AND f.kind='collection'{filter}"
    );
    format!(
        "(SELECT s.* FROM (({from} AND f.created_at={at} AND (f.author,f.object_path)>({author},{path}) \
         ORDER BY f.author,f.object_path LIMIT 1) UNION ALL ({from} AND f.created_at<{at} \
         ORDER BY f.created_at DESC,f.author,f.object_path LIMIT 1)) s LIMIT 1)"
    )
}
/// SQL for a recursive walk `name` (`link`, `k`, `at`, `author`, `path`): from each row of
/// `start` (a `link` with the position to start after: `at`, `author`, `path`), the next `n`
/// posts of the link ([`next`]).
fn walk(name: &str, start: &str, n: &str, filter: &str) -> String {
    format!(
        "{name}(link,k,at,author,path) AS (SELECT b.link,1,s.* FROM {start} b CROSS JOIN LATERAL {} s \
         UNION ALL SELECT w.link,w.k+1,s.* FROM {name} w CROSS JOIN LATERAL {} s WHERE w.k<{n})",
        next("b.link", "b.at", "b.author", "b.path", filter),
        next("w.link", "w.at", "w.author", "w.path", filter),
    )
}
/// The count of every card in `$1` (IDs), up to `$2` ([`COUNTED`]), over its [`links`]: each
/// link walked from its newest post, at most `$2` posts. A link holding that many already makes
/// the count, and otherwise each link's walk is whole. So a card costs at most `$2` steps per
/// link, however many collections hold it.
fn count_sql() -> String {
    format!(
        "WITH RECURSIVE cards AS (SELECT DISTINCT id FROM unnest($1::text[]) id), \
         starts AS (SELECT c.id AS card,l.link,'infinity'::timestamptz AS at,''::text AS author,''::text AS path \
           FROM cards c CROSS JOIN LATERAL ({}) l), \
         {} \
         SELECT c.id,(SELECT count(*) FROM (SELECT DISTINCT p.author,p.path FROM walk p JOIN starts s ON s.link=p.link WHERE s.card=c.id LIMIT $2) x) FROM cards c",
        links("c.id"),
        walk("walk", "(SELECT DISTINCT link,at,author,path FROM starts)", "$2", ""),
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
    let mut sql = sqlx::QueryBuilder::new("");
    push_page(
        &mut sql,
        root.clone(),
        q.viewer.clone(),
        cursor.as_ref(),
        limit + 1,
    );
    let rows: Vec<(bool, DateTime<Utc>, String, String)> =
        sql.build_query_as().fetch_all(&state.pool).await?;
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
/// Pushes one page of `root`'s collections, `n` of them: the viewer's own (author `viewer`), then
/// the others, each newest first, as (own, indexed_at, author, file). Each of the card's
/// [`links`] is walked from the cursor on ([`walk`]), at most `n` posts: the page's posts are among
/// them (a link's earlier posts are earlier on the page too), so a page costs at most `n` steps
/// per link, however deep it is and however many collections hold the card. A segment the cursor
/// is past is not walked.
fn push_page(
    sql: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>,
    root: String,
    viewer: Option<String>,
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
    sql.push("WITH RECURSIVE q AS (SELECT ")
        .push_bind(root)
        .push("::text AS root,")
        .push_bind(viewer)
        .push("::text AS viewer,")
        .push_bind(n as i64)
        .push("::bigint AS n), links AS (SELECT l.link FROM q CROSS JOIN LATERAL (")
        .push(links("q.root"))
        .push(") l), others_start AS (SELECT l.link,coalesce(")
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
                " AND f.author=(SELECT viewer FROM q)",
            ))
            .push(", ");
    }
    sql.push(walk(
        "others",
        "others_start",
        "(SELECT n FROM q)",
        " AND f.author IS DISTINCT FROM (SELECT viewer FROM q)",
    ))
    .push(" SELECT own,max(at),author,path FROM (");
    if own {
        sql.push("SELECT true AS own,at,author,path FROM own UNION ALL ");
    }
    sql.push("SELECT false AS own,at,author,path FROM others) x GROUP BY own,author,path ORDER BY own DESC,max(at) DESC,author,path LIMIT (SELECT n FROM q)");
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

    /// What a statement read of `reference`: the rows its scans of the table produced or
    /// discarded (by filter or recheck), over every loop, and whether one was a sequential scan.
    async fn reads(pool: &PgPool, mut sql: sqlx::QueryBuilder<'_, sqlx::Postgres>) -> (i64, bool) {
        let plan: serde_json::Value = sql.build_query_scalar().fetch_one(pool).await.unwrap();
        read(&plan)
    }
    fn read(plan: &serde_json::Value) -> (i64, bool) {
        fn walk(node: &serde_json::Value, rows: &mut i64, seq: &mut bool) {
            if node["Relation Name"] == "reference" {
                let n = |k: &str| node[k].as_f64().unwrap_or(0.0);
                *rows += ((n("Actual Rows")
                    + n("Rows Removed by Filter")
                    + n("Rows Removed by Index Recheck"))
                    * n("Actual Loops")) as i64;
                *seq |= node["Node Type"] == "Seq Scan";
            }
            for child in node["Plans"].as_array().into_iter().flatten() {
                walk(child, rows, seq);
            }
        }
        let (mut rows, mut seq) = (0, false);
        walk(&plan[0]["Plan"], &mut rows, &mut seq);
        (rows, seq)
    }
    fn explain() -> sqlx::QueryBuilder<'static, sqlx::Postgres> {
        sqlx::QueryBuilder::new("EXPLAIN (ANALYZE, FORMAT JSON) ")
    }

    /// A card 20,000 collections hold, with 40 collected aliases, and an author of 20,000
    /// collections of other links: its count, its pages (the first, a deep one, the viewer's own)
    /// and a search for one collection each read a bounded number of references along their
    /// indexes, never every collection of the card or of the author. Twice: with these links
    /// alone, whose statistics say a link holds hundreds, and with 20,000 links more held once
    /// each, whose statistics say a link holds one (the planner's guess, wrong either way for this
    /// card, chooses nothing here: each step of a walk is an `ORDER BY … LIMIT 1` along an index).
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
            "INSERT INTO resource(id,uri) VALUES (repeat('0',32),'https://example.org/root')",
            "INSERT INTO resource(id,uri) SELECT md5('r'||n),'https://example.org/'||n FROM generate_series(1,40) n",
            "INSERT INTO resource(id,uri) SELECT md5('x'||n),'https://example.org/x'||n FROM generate_series(1,40) n",
            "INSERT INTO resource_alias(alias_id,root_id,basis) SELECT md5('r'||n),repeat('0',32),'canonical' FROM generate_series(1,40) n",
            // The card's own link, in 20,000 posts of as many authors, one a second (the
            // viewer's 3 among them).
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT repeat('0',32),'collection',CASE WHEN n%6000=0 THEN repeat('v',52) ELSE lpad(n::text,52,'y') END,'/pub/social/v1/posts/P'||n||'/P'||n||'.json',timestamptz '2026-01-01'+n*interval '1 second','collection:'||md5(n::text) FROM generate_series(1,20000) n",
            // Each alias in two posts.
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT md5('r'||a),'collection',lpad(a::text,52,'w'),'/pub/social/v1/posts/A'||a||'-'||k||'/A.json',timestamptz '2026-06-01'+(a*2+k)*interval '1 second','collection:'||md5(a::text) FROM generate_series(1,40) a,generate_series(1,2) k",
            // One author's 20,000 collections, of other links.
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT md5('x'||(1+n%40)),'collection',repeat('x',52),'/pub/social/v1/posts/X'||n||'/X'||n||'.json',timestamptz '2026-01-01'+n*interval '1 second','collection:'||md5(n::text) FROM generate_series(1,20000) n",
            "ANALYZE reference",
            "ANALYZE resource_alias",
        ] {
            sqlx::query(statement).execute(&mut *seed).await.unwrap();
        }
        check_bounded_walks(&pool, "few links").await;
        for statement in [
            "INSERT INTO resource(id,uri) SELECT md5('m'||n),'https://example.org/m'||n FROM generate_series(1,20000) n",
            "INSERT INTO reference(resource_id,kind,author,object_path,created_at,unit_key) SELECT md5('m'||n),'collection',lpad(n::text,52,'z'),'/pub/social/v1/posts/M'||n||'/M.json',timestamptz '2026-09-01','collection:m' FROM generate_series(1,20000) n",
            "ANALYZE reference",
            "RESET session_replication_role",
        ] {
            sqlx::query(statement).execute(&mut *seed).await.unwrap();
        }
        check_bounded_walks(&pool, "many links").await;
    }
    async fn check_bounded_walks(pool: &PgPool, phase: &str) {
        let root = "0".repeat(32);
        let viewer = "v".repeat(52);
        // The count: the card's 1,000 newest posts, 2 per alias and each alias's newest.
        let plan: serde_json::Value =
            sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {}", count_sql()))
                .bind(vec![root.clone()])
                .bind(COUNTED)
                .fetch_one(pool)
                .await
                .unwrap();
        let (rows, seq) = read(&plan);
        assert!(!seq && rows <= 1_200, "{phase}, count: {rows} rows");
        let counted: i64 =
            sqlx::query_scalar(&format!("SELECT count FROM ({}) c(id,count)", count_sql()))
                .bind(vec![root.clone()])
                .bind(COUNTED)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(counted, COUNTED, "{phase}");

        // Pages: the first (the aliases' posts are newest), one 15,000 posts deep, the viewer's
        // own first, and a page past two of the viewer's own.
        let deep = PageCursor {
            scope: root.clone(),
            viewer: None,
            own: false,
            at: "2026-01-01T04:10:00Z".parse().unwrap(),
            author: "y".repeat(52),
            path: String::new(),
        };
        let mine = PageCursor {
            scope: root.clone(),
            viewer: Some(viewer.clone()),
            own: true,
            at: "2026-01-01T03:20:00Z".parse().unwrap(),
            author: viewer.clone(),
            path: "/pub/social/v1/posts/P12000/P12000.json".into(),
        };
        let post = |n: u32| format!("/pub/social/v1/posts/P{n}/P{n}.json");
        for (name, viewer, cursor, first, own) in [
            (
                "first",
                None,
                None,
                "/pub/social/v1/posts/A40-2/A.json".to_owned(),
                0,
            ),
            ("deep", None, Some(&deep), post(14_999), 0),
            ("viewer", Some(viewer.clone()), None, post(18_000), 3),
            (
                "viewer's later",
                Some(viewer.clone()),
                Some(&mine),
                post(6_000),
                1,
            ),
        ] {
            let mut sql = explain();
            push_page(&mut sql, root.clone(), viewer.clone(), cursor, 21);
            let (rows, seq) = reads(pool, sql).await;
            assert!(!seq && rows <= 250, "{phase}, {name}: {rows} rows");
            let mut sql = sqlx::QueryBuilder::new("");
            push_page(&mut sql, root.clone(), viewer, cursor, 21);
            let page: Vec<(bool, DateTime<Utc>, String, String)> =
                sql.build_query_as().fetch_all(pool).await.unwrap();
            assert_eq!(page.len(), 21, "{phase}, {name}");
            assert_eq!(page[0].3, first, "{phase}, {name}");
            assert_eq!(page.iter().filter(|r| r.0).count(), own, "{phase}, {name}");
        }

        // A search for one of the prolific author's collections reads that post's files alone.
        let mut sql = explain();
        sql.push("SELECT f.resource_id FROM reference f WHERE ");
        let one = parse(&format!(
            "pubky://{}/pub/social/v1/posts/X15000",
            "x".repeat(52)
        ))
        .unwrap();
        push_items(&mut sql, one.clone());
        let (rows, seq) = reads(pool, sql).await;
        assert!(!seq && rows <= 5, "{phase}, in_collection: {rows} rows");
        let mut sql = sqlx::QueryBuilder::new("SELECT count(*) FROM reference f WHERE ");
        push_items(&mut sql, one);
        let held: i64 = sql.build_query_scalar().fetch_one(pool).await.unwrap();
        assert_eq!(held, 1, "{phase}");
    }
}
