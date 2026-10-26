//! The Reddit source's cards in storage: a post Reddit stopped showing keeps its social
//! classification, so its text leaves the change history once the post is gone.
use pinakes_core::{
    model::{ResourceMeta, Status},
    sources::reddit::{self, Target},
    store,
};
use serde_json::Value;

const POST: &str = "https://www.reddit.com/comments/1k3d0pk/";

fn fixture(file: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/sources/reddit/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
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
