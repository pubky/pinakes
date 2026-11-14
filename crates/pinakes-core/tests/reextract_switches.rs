//! `reextract` with a source switched off, in a process of its own: the switches are global
//! (`sources::configure`), as `pinakesd reextract` sets them from the operator's configuration.
use pinakes_core::{config::Config, queue, store};
use sqlx::PgPool;

/// A switched-off source's rule is only silent, not gone: the spellings it merged before stay
/// merged through `reextract all`, neither released nor moved by a weaker rule (a tracking
/// parameter). A link no source reads (a comment's permalink) is released
/// as before.
#[sqlx::test(migrations = "../../migrations")]
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
    // With Reddit on, a shared spelling with a tracking parameter is the post's; with Reddit
    // off, the tracking-parameter rule alone would name the untracked spelling instead.
    let shared =
        "https://old.reddit.com/r/announcements/comments/1k3d0pk/a_title/?utm_source=share";
    let mut ids = std::collections::BTreeMap::new();
    for uri in [post, spelling, comment, shared] {
        ids.insert(uri, store::ensure_resource(&pool, uri).await.unwrap());
    }
    for alias in [spelling, comment, shared] {
        sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'identifier') ON CONFLICT(alias_id) DO UPDATE SET root_id=excluded.root_id,basis=excluded.basis")
            .bind(&ids[alias])
            .bind(&ids[post])
            .execute(&pool)
            .await
            .unwrap();
    }
    assert!(pinakes_core::sources::switched_off(spelling));
    assert!(pinakes_core::sources::switched_off(shared));
    assert!(
        pinakes_core::alias::root(shared).is_some(),
        "the tracking rule names another root"
    );
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
    assert_eq!(
        root_of(shared).await,
        Some(ids[post].clone()),
        "a weaker rule never moves a switched-off source's alias"
    );
    assert_eq!(root_of(comment).await, None, "no source reads a comment");
}
