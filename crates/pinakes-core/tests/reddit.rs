//! The Reddit source's cards in storage: a post Reddit stopped showing keeps its social
//! classification, so its text leaves the change history once the post is gone.
use anyhow::Result;
use async_trait::async_trait;
use axum::{body::Body, extract::ConnectInfo, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    alias,
    api::{self, AppState},
    config::Config,
    model::{ResourceMeta, Status},
    resolver::{
        cards::CardResolver,
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        Resolver,
    },
    sources::reddit::{self, Target},
    store,
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tower::ServiceExt;
use url::Url;

const POST: &str = "https://www.reddit.com/comments/1k3d0pk/";

fn fixture(file: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/sources/reddit/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

struct PublicDns;
#[async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

/// Reddit answering the post's listing: the post, then, once `deleted`, the stub it shows for a
/// post its author deleted.
#[derive(Default)]
struct Listing {
    deleted: AtomicBool,
}
#[async_trait]
impl Transport for Listing {
    async fn get(&self, u: &Url, addrs: &[SocketAddr]) -> Result<Fetched> {
        self.get_with(u, addrs, &Default::default()).await
    }
    async fn get_with(&self, u: &Url, _: &[SocketAddr], _: &FetchOptions) -> Result<Fetched> {
        anyhow::ensure!(
            u.host_str() == Some("www.reddit.com") && u.path() == "/comments/1k3d0pk.json",
            "no script for {u}"
        );
        let file = if self.deleted.load(Ordering::SeqCst) {
            "post-1k3d0pk-deleted.json"
        } else {
            "post-1k3d0pk.json"
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: u.clone(),
            content_type: "application/json; charset=UTF-8".into(),
            status: 200,
            retry_after_secs: 0,
            location: None,
            body: serde_json::to_vec(&fixture(file)).unwrap(),
        })
    }
}

async fn history(pool: &sqlx::PgPool, id: &str) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(c) FROM resource_change c WHERE resource_id=$1")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A post read, then refused (Reddit's 403: the blocked card), then confirmed gone: the stored
/// card is the blocked one when the deletion is confirmed, and it is still social content, so
/// the post's history, text and all, is purged.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_refused_then_deleted_post_leaves_no_text_in_the_history(pool: sqlx::PgPool) {
    let id = store::ensure_resource(&pool, POST).await.unwrap();
    let target = Target::of_uri(POST).unwrap();
    let read = reddit::post_card(
        ResourceMeta::new(POST, "reddit", 3600),
        "1k3d0pk",
        &fixture("post-1k3d0pk.json"),
    )
    .unwrap()
    .unwrap();
    store::save_document(&pool, &id, 0, &read).await.unwrap();
    let mut refused = reddit::blocked(ResourceMeta::new(POST, "reddit", 3600), &target);
    refused.fetched_at = read.fetched_at + 1;
    assert_eq!(refused.type_name, read.type_name);
    store::save_document(&pool, &id, 0, &refused).await.unwrap();
    assert!(
        history(&pool, &id)
            .await
            .iter()
            .any(|c| c.to_string().contains("Transparency Report")),
        "the read card's text is in the history before the deletion"
    );
    let mut gone = ResourceMeta::new(POST, "reddit", 3600);
    gone.status = Status::Missing;
    for n in 2..4 {
        gone.fetched_at = read.fetched_at + n;
        store::save_document(&pool, &id, 0, &gone).await.unwrap();
    }
    let left = history(&pool, &id).await;
    assert!(
        left.iter()
            .all(|c| !c.to_string().contains("Transparency Report")),
        "{left:?}"
    );
}

/// Reddit's stub of a deleted post (a 200 whose text and author read `[deleted]`) is the post's
/// deletion: the card read from it is missing, and once that is confirmed the post's history,
/// text and all, is purged.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_deleted_posts_stub_leaves_no_text_in_the_history(pool: sqlx::PgPool) {
    if pinakes_core::sources::by_name("reddit").is_none() {
        return;
    }
    let reddit = Arc::new(Listing::default());
    let resolver = CardResolver::new(
        "official",
        Arc::new(Config::default()),
        Egress::with_backends(Default::default(), Arc::new(PublicDns), reddit.clone()),
    );
    let id = store::ensure_resource(&pool, POST).await.unwrap();
    let read = resolver.resolve(POST).await.unwrap();
    assert_eq!(read.status, Status::Ok);
    store::save_document(&pool, &id, 0, &read).await.unwrap();
    assert!(
        history(&pool, &id)
            .await
            .iter()
            .any(|c| c.to_string().contains("Transparency Report")),
        "the read card's text is in the history before the deletion"
    );
    reddit.deleted.store(true, Ordering::SeqCst);
    for n in 1..3 {
        let mut gone = resolver.resolve(POST).await.unwrap();
        assert_eq!(gone.status, Status::Missing);
        gone.fetched_at = read.fetched_at + n;
        store::save_document(&pool, &id, 0, &gone).await.unwrap();
    }
    let left = history(&pool, &id).await;
    assert!(
        left.iter()
            .all(|c| !c.to_string().contains("Transparency Report")),
        "{left:?}"
    );
}

/// The post's title, its body and its author (a fact) in the histories, stored cards, names and
/// search text of `ids`. A spelling's slug is in its URI, which is a link and no text.
async fn texts(pool: &sqlx::PgPool, ids: &[&str]) -> i64 {
    const TEXT: &str = "2025 Transparency Report|content removals|reddit_admin";
    sqlx::query_scalar(&format!("SELECT (SELECT count(*) FROM resource_change WHERE resource_id=ANY($1) AND (coalesce(before::text,'')||coalesce(after::text,'')) ~* '{TEXT}') + (SELECT count(*) FROM resource WHERE id=ANY($1) AND ((coalesce(doc::text,'')||coalesce(name,'')) ~* '{TEXT}' OR coalesce(search_text::text,'') ~* 'transparency|removals'))"))
        .bind(ids.iter().map(|id| id.to_string()).collect::<Vec<_>>())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A spelling of the post as a build without the Reddit rule stored it: its own row and its own
/// full card (read at `at`), then made an alias of the post. Its ID.
async fn spelling(pool: &sqlx::PgPool, uri: &str, at: i64) -> String {
    let (id, uri, _) = pinakes_core::model::identity(uri).unwrap();
    sqlx::query("INSERT INTO resource(id,uri) VALUES($1,$2)")
        .bind(&id)
        .bind(&uri)
        .execute(pool)
        .await
        .unwrap();
    let mut card = reddit::post_card(
        ResourceMeta::new(&uri, "reddit", 3600),
        "1k3d0pk",
        &fixture("post-1k3d0pk.json"),
    )
    .unwrap()
    .unwrap();
    card.fetched_at = at;
    store::save_document(pool, &id, 0, &card).await.unwrap();
    assert!(texts(pool, &[&id]).await > 0, "the spelling's own card");
    let mut tx = pool.begin().await.unwrap();
    let line = alias::assign(
        &mut tx,
        &id,
        &alias::Alias {
            root: POST.into(),
            basis: "identifier",
        },
    )
    .await
    .unwrap();
    alias::record(&mut tx, line.into_iter().collect())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    id
}

/// What an alias of a gone post keeps: its `aliased` line, naming the root, and an empty card.
async fn left_of_alias(pool: &sqlx::PgPool, id: &str) -> (Vec<String>, Value) {
    let lines =
        sqlx::query_scalar("SELECT class FROM resource_change WHERE resource_id=$1 ORDER BY seq")
            .bind(id)
            .fetch_all(pool)
            .await
            .unwrap();
    let card = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    (lines, card)
}

const SUBREDDIT_SPELLING: &str =
    "https://www.reddit.com/r/announcements/comments/1k3d0pk/reddits_2025_transparency_report/";
const OLD_SPELLING: &str =
    "https://old.reddit.com/r/announcements/comments/1k3d0pk/reddits_2025_transparency_report/";

/// A public listener request: its status and body.
async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// The post read through the official card resolver from a scripted Reddit, and saved: the
/// scripted Reddit, the resolver, the post's ID and when it was read.
async fn post(pool: &sqlx::PgPool) -> (Arc<Listing>, CardResolver, String, i64) {
    let reddit = Arc::new(Listing::default());
    let resolver = CardResolver::new(
        "official",
        Arc::new(Config::default()),
        Egress::with_backends(Default::default(), Arc::new(PublicDns), reddit.clone()),
    );
    let id = store::ensure_resource(pool, POST).await.unwrap();
    let read = resolver.resolve(POST).await.unwrap();
    assert_eq!(read.status, Status::Ok);
    store::save_document(pool, &id, 0, &read).await.unwrap();
    (reddit, resolver, id, read.fetched_at)
}
/// Reddit's stub of the deleted post, read and saved twice: the post is confirmed gone.
async fn delete(pool: &sqlx::PgPool, reddit: &Listing, resolver: &CardResolver, id: &str, at: i64) {
    reddit.deleted.store(true, Ordering::SeqCst);
    for n in 1..3 {
        let mut gone = resolver.resolve(POST).await.unwrap();
        assert_eq!(gone.status, Status::Missing);
        gone.fetched_at = at + n;
        store::save_document(pool, id, 0, &gone).await.unwrap();
    }
}

/// A spelling of the post read and saved before it became the post's alias keeps the post's text
/// in its own history and card. Once the post is gone, neither the post nor that alias keeps its
/// title, body or facts: not in their histories, not in their stored cards or search text, and
/// so not in the public change lookup (which answers an alias's own lines too) nor in the post's
/// public read, whose `facts` are gone with them.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_deleted_posts_aliases_keep_none_of_its_text(pool: sqlx::PgPool) {
    if pinakes_core::sources::by_name("reddit").is_none() {
        return;
    }
    let (reddit, resolver, id, at) = post(&pool).await;
    let alias_id = spelling(&pool, SUBREDDIT_SPELLING, at - 1).await;
    assert!(texts(&pool, &[&id, &alias_id]).await > 2);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let (_, read) = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    assert_eq!(read["facts"]["author"], "reddit_admin", "{read}");
    assert_eq!(read["type"], "DiscussionForumPosting", "{read}");
    for (target, path) in [(&id, "/pub/app/tags/a"), (&alias_id, "/pub/app/tags/b")] {
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,'author','tag','news',$2,now())")
            .bind(target)
            .bind(path)
            .execute(&pool)
            .await
            .unwrap();
    }
    let (_, read) = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    assert_eq!(read["aliases"], json!([SUBREDDIT_SPELLING]), "{read}");
    assert_eq!(read["refs"].as_array().map(Vec::len), Some(2), "{read}");

    delete(&pool, &reddit, &resolver, &id, at).await;
    assert_eq!(texts(&pool, &[&id, &alias_id]).await, 0);
    let attributes: Value = sqlx::query_scalar(
        "SELECT coalesce(doc->'attributes','{}'::jsonb) FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attributes, json!({}));
    let (code, read) = call(&app, "GET", &format!("/v1/resources/{id}"), None).await;
    assert_eq!(code, 200, "{read}");
    assert_eq!(read["status"], "missing", "{read}");
    // Not the post's own type either: the one every gone post has.
    assert_eq!(read["type"], "SocialMediaPosting", "{read}");
    // Nor its spellings, whose links carry its title, in `aliases` or `refs`.
    assert_eq!(read["aliases"], json!([]), "{read}");
    assert_eq!(read["refs"], json!([POST]), "{read}");
    assert!(
        read["facts"].as_object().is_none_or(|f| f.is_empty()),
        "{read}"
    );
    assert!(read.get("name").is_none_or(Value::is_null), "{read}");
    let (code, answer) = call(
        &app,
        "POST",
        "/v1/changes/lookup",
        Some(json!({"uris": [POST, SUBREDDIT_SPELLING], "since": 0})),
    )
    .await;
    assert_eq!(code, 200);
    let answer = answer.to_string();
    assert!(
        !answer.contains("content removals") && !answer.contains("2025 Transparency Report"),
        "{answer}"
    );
    // The alias keeps only its line naming the root.
    assert_eq!(
        left_of_alias(&pool, &alias_id).await,
        (vec!["aliased".to_owned()], json!({}))
    );
}

/// A spelling with its own full card that becomes an alias of the post after the post is gone
/// keeps none of its text either, at once: not in its card, its search text or its history.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_spelling_that_joins_a_deleted_post_keeps_none_of_its_text(pool: sqlx::PgPool) {
    if pinakes_core::sources::by_name("reddit").is_none() {
        return;
    }
    let (reddit, resolver, id, at) = post(&pool).await;
    delete(&pool, &reddit, &resolver, &id, at).await;
    assert_eq!(texts(&pool, &[&id]).await, 0);
    let alias_id = spelling(&pool, OLD_SPELLING, at + 10).await;
    assert_eq!(texts(&pool, &[&id, &alias_id]).await, 0);
    assert_eq!(
        left_of_alias(&pool, &alias_id).await,
        (vec!["aliased".to_owned()], json!({}))
    );
}

/// Rows stored before the scrub ran (a gone post whose card, history and also-on facts stayed,
/// an alias with its own full card and history) are scrubbed by the repair `pinakesd reextract`
/// runs, even though the post is never fetched again: the post keeps nothing of itself and is
/// regrouped, the alias only its line naming the post. A missing page that is no social post
/// keeps its card, and a second run changes nothing.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_repair_scrubs_posts_stored_before_their_scrub(pool: sqlx::PgPool) {
    if pinakes_core::sources::by_name("reddit").is_none() {
        return;
    }
    let (_, _, id, at) = post(&pool).await;
    let alias_id = spelling(&pool, SUBREDDIT_SPELLING, at - 1).await;
    // The post's deletion as an earlier build stored it: missing, with its card and history.
    sqlx::query("UPDATE resource SET status='missing',doc=jsonb_set(doc,'{status}','\"missing\"') WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let page = store::ensure_resource(&pool, "https://example.org/gone-page")
        .await
        .unwrap();
    let mut gone_page = ResourceMeta::new("https://example.org/gone-page", "web", 3600);
    gone_page.name = Some("A page that is gone".into());
    gone_page.status = Status::Missing;
    sqlx::query("UPDATE resource SET status='missing',name=$2,doc=$3 WHERE id=$1")
        .bind(&page)
        .bind(&gone_page.name)
        .bind(serde_json::to_value(&gone_page).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    // What an also-on lookup found for it.
    sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) VALUES($1,'title','Reddit''s 2025 Transparency Report','titles',now()+interval '1 day')")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(texts(&pool, &[&id, &alias_id]).await > 2);

    assert_eq!(pinakes_core::changes::scrub_stored(&pool).await.unwrap(), 1);
    let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_fact WHERE resource_id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(facts, 0);
    let regroup: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM group_dirty WHERE resource_id=$1)")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(regroup);
    assert_eq!(
        left_of_alias(&pool, &alias_id).await,
        (vec!["aliased".to_owned()], json!({}))
    );
    assert_eq!(texts(&pool, &[&id, &alias_id]).await, 0);
    let attributes: Value =
        sqlx::query_scalar("SELECT doc->'attributes' FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attributes, json!({}));
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM resource WHERE id=$1")
        .bind(&page)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name.as_deref(), Some("A page that is gone"));
    assert_eq!(pinakes_core::changes::scrub_stored(&pool).await.unwrap(), 0);

    // A gone card that kept the post's type, as the build before the one gone type stored it,
    // takes that type.
    let types = || {
        sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT type,doc->>'type' FROM resource WHERE id=$1",
        )
        .bind(&id)
        .fetch_one(&pool)
    };
    let gone = Some("SocialMediaPosting".to_owned());
    assert_eq!(types().await.unwrap(), (gone.clone(), gone.clone()));
    sqlx::query("UPDATE resource SET type='DiscussionForumPosting',type_path=ARRAY['Thing','CreativeWork','Article','SocialMediaPosting','DiscussionForumPosting'],doc=doc||'{\"type\":\"DiscussionForumPosting\",\"type_path\":[\"Thing\",\"CreativeWork\",\"Article\",\"SocialMediaPosting\",\"DiscussionForumPosting\"]}' WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(pinakes_core::changes::scrub_stored(&pool).await.unwrap(), 1);
    assert_eq!(types().await.unwrap(), (gone.clone(), gone));
    assert_eq!(pinakes_core::changes::scrub_stored(&pool).await.unwrap(), 0);
}

/// A refresh that fails sets the row's status to `error` (`queue::failed`), its stored card
/// unchanged.
async fn fail(pool: &sqlx::PgPool, id: &str) {
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM resource WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    let job = pinakes_core::queue::Job {
        id: id.into(),
        uri: POST.into(),
        attempts: 0,
        generation,
    };
    pinakes_core::queue::failed(pool, &job, &Default::default(), "http_5xx")
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(status, "error");
}

/// A deleted post as an earlier build stored it, whose refresh then failed (Reddit's 500): its
/// row says `error`, its stored card still `missing`. The repair finds it by that card and scrubs
/// it and its spelling.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_repair_finds_a_gone_post_whose_refresh_failed(pool: sqlx::PgPool) {
    if pinakes_core::sources::by_name("reddit").is_none() {
        return;
    }
    let (_, _, id, at) = post(&pool).await;
    let alias_id = spelling(&pool, SUBREDDIT_SPELLING, at - 1).await;
    sqlx::query("UPDATE resource SET status='missing',doc=jsonb_set(doc,'{status}','\"missing\"') WHERE id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    fail(&pool, &id).await;
    assert!(texts(&pool, &[&id, &alias_id]).await > 2);

    assert_eq!(pinakes_core::changes::scrub_stored(&pool).await.unwrap(), 1);
    assert_eq!(texts(&pool, &[&id, &alias_id]).await, 0);
    assert_eq!(
        left_of_alias(&pool, &alias_id).await,
        (vec!["aliased".to_owned()], json!({}))
    );
    assert_eq!(pinakes_core::changes::scrub_stored(&pool).await.unwrap(), 0);
}

/// A post this build deleted keeps nothing through a failed refresh: nothing comes back, and the
/// repair finds nothing to do.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_failed_refresh_of_a_gone_post_brings_nothing_back(pool: sqlx::PgPool) {
    if pinakes_core::sources::by_name("reddit").is_none() {
        return;
    }
    let (reddit, resolver, id, at) = post(&pool).await;
    let alias_id = spelling(&pool, SUBREDDIT_SPELLING, at - 1).await;
    delete(&pool, &reddit, &resolver, &id, at).await;
    let card: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    fail(&pool, &id).await;
    let after: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, card);
    assert_eq!(texts(&pool, &[&id, &alias_id]).await, 0);
    assert_eq!(pinakes_core::changes::scrub_stored(&pool).await.unwrap(), 0);
}
