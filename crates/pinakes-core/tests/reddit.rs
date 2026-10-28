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

/// A spelling of the post read and saved before it became the post's alias keeps the post's text
/// in its own history and card. Once the post is gone, neither the post nor that alias keeps its
/// title or body: not in their histories, not in their stored cards, and so not in the public
/// change lookup either (which answers an alias's own lines too).
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_deleted_posts_aliases_keep_none_of_its_text(pool: sqlx::PgPool) {
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
    // The subreddit spelling, as a build without the Reddit rule stored it: its own row and its
    // own full card, then made an alias of the post.
    let spelling =
        "https://www.reddit.com/r/announcements/comments/1k3d0pk/reddits_2025_transparency_report/";
    let (alias_id, spelling, _) = pinakes_core::model::identity(spelling).unwrap();
    sqlx::query("INSERT INTO resource(id,uri) VALUES($1,$2)")
        .bind(&alias_id)
        .bind(&spelling)
        .execute(&pool)
        .await
        .unwrap();
    let mut early = resolver.resolve(&spelling).await.unwrap();
    early.url = spelling.clone();
    early.fetched_at = read.fetched_at - 1;
    store::save_document(&pool, &alias_id, 0, &early)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let line = alias::assign(
        &mut tx,
        &alias_id,
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

    // The title and the body; the spelling's own slug is in its URI, which is no text.
    let texts = || async {
        sqlx::query_scalar::<_, i64>("SELECT (SELECT count(*) FROM resource_change WHERE resource_id IN ($1,$2) AND (coalesce(before::text,'')||coalesce(after::text,'')) ~* '2025 Transparency Report|content removals') + (SELECT count(*) FROM resource WHERE id IN ($1,$2) AND (coalesce(doc::text,'')||coalesce(name,'')) ~* '2025 Transparency Report|content removals')")
            .bind(&id)
            .bind(&alias_id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let lookup = || {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/changes/lookup")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"uris": [POST, spelling], "since": 0}).to_string(),
            ))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
        ));
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), 200);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            String::from_utf8(body.to_vec()).unwrap()
        }
    };
    assert!(texts().await > 2, "the post and its alias kept the text");

    reddit.deleted.store(true, Ordering::SeqCst);
    for n in 1..3 {
        let mut gone = resolver.resolve(POST).await.unwrap();
        assert_eq!(gone.status, Status::Missing);
        gone.fetched_at = read.fetched_at + n;
        store::save_document(&pool, &id, 0, &gone).await.unwrap();
    }
    assert_eq!(texts().await, 0);
    let answer = lookup().await;
    assert!(
        !answer.contains("content removals") && !answer.contains("2025 Transparency Report"),
        "{answer}"
    );
    // The alias still names its root.
    let aliased: Vec<Option<Value>> = sqlx::query_scalar(
        "SELECT after FROM resource_change WHERE resource_id=$1 AND class='aliased'",
    )
    .bind(&alias_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(aliased.iter().any(Option::is_some), "{aliased:?}");
}
