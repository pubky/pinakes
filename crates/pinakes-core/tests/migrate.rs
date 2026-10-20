use sqlx::{migrate::Migrator, PgPool};
use std::time::Duration;

/// A newer binary migrates while an older daemon still runs (`docs/operations.md` says to stop
/// it first): the daemon holds the instance lock, and one of its writes is in flight. The
/// revision migration waits for that write and counts it, never waits on the instance lock, and
/// the newer daemon is then refused.
#[sqlx::test(migrations = false)]
async fn the_revision_migration_counts_an_older_daemons_write_in_flight(pool: PgPool) {
    let older = Migrator {
        migrations: pinakes_core::MIGRATOR
            .migrations
            .iter()
            .filter(|m| m.version < 3)
            .cloned()
            .collect::<Vec<_>>()
            .into(),
        ..Migrator::DEFAULT
    };
    older.run(&pool).await.unwrap();
    let revision = || async {
        sqlx::query_scalar::<_, i64>("SELECT revision FROM search_revision WHERE id=1")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = revision().await;
    // Every daemon version takes the same instance key.
    let instance = pinakes_core::locks::own_database(&pool).await.unwrap();
    // What the older `bump_search_revision` does in a write transaction.
    let mut write = pool.begin().await.unwrap();
    sqlx::query("UPDATE search_revision SET revision=revision+1 WHERE id=1")
        .execute(&mut *write)
        .await
        .unwrap();
    let migrating = tokio::spawn({
        let pool = pool.clone();
        async move { pinakes_core::migrate(&pool).await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE NOT granted AND locktype='relation' AND relation=to_regclass('search_revision'))")
                .fetch_one(&pool).await.unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the migration waits for the write in flight");
    assert!(!migrating.is_finished());
    write.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), migrating)
        .await
        .expect("the migration never waits on the instance lock")
        .unwrap()
        .unwrap();
    assert_eq!(revision().await, before + 1);
    assert!(pinakes_core::locks::own_database(&pool).await.is_err());
    // Once the older daemon has stopped, the newer one owns the database.
    instance.close().await.unwrap();
    pinakes_core::locks::own_database(&pool).await.unwrap();
}
