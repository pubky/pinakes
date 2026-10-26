//! `reextract` with a source switched off, in a process of its own: the switches are global
//! (`sources::configure`), as `pinakesd reextract` sets them from the operator's configuration.
use pinakes_core::{config::Config, queue, store};
use sqlx::PgPool;

/// A switched-off source's rule is only silent, not gone: the spellings it merged before stay
/// merged through `reextract all`. A link no source reads (a comment's permalink) is released
/// as before.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_switched_off_sources_aliases_stay_through_reextract(pool: PgPool) {
    if pinakes_core::sources::by_name("reddit").is_none() {
        return;
    }
    let mut config = Config::default();
    config.sources.0.insert(
        "reddit".into(),
        pinakes_core::sources::Switch {
            enabled: false,
            token: None,
        },
    );
    pinakes_core::configure_sources(&config);
    let post = "https://www.reddit.com/comments/1k3d0pk/";
    let spelling = "https://old.reddit.com/r/announcements/comments/1k3d0pk/a_title/";
    let comment = "https://www.reddit.com/r/announcements/comments/1k3d0pk/a_title/abcdefg/";
    let mut ids = std::collections::BTreeMap::new();
    for uri in [post, spelling, comment] {
        ids.insert(uri, store::ensure_resource(&pool, uri).await.unwrap());
    }
    for alias in [spelling, comment] {
        sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'identifier') ON CONFLICT(alias_id) DO UPDATE SET root_id=excluded.root_id,basis=excluded.basis")
            .bind(&ids[alias])
            .bind(&ids[post])
            .execute(&pool)
            .await
            .unwrap();
    }
    assert!(pinakes_core::sources::switched_off(spelling));
    queue::reextract(&pool, &["all".into()]).await.unwrap();
    let root_of = |uri: &str| {
        let (pool, id) = (pool.clone(), ids[uri].clone());
        async move {
            sqlx::query_scalar::<_, String>("SELECT root_id FROM resource_alias WHERE alias_id=$1")
                .bind(id)
                .fetch_optional(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(root_of(spelling).await, Some(ids[post].clone()));
    assert_eq!(root_of(comment).await, None, "no source reads a comment");
}
