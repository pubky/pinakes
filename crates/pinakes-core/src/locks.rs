//! The database's fixed advisory lock keys, in one place. A database has one key space, so two
//! locks that share a key are one lock: every key here must differ from every other. The keys
//! per adapter (`store`) are 64-bit hashes of its name, so they don't collide with these in
//! practice.
use anyhow::{bail, Result};
use sqlx::{pool::PoolConnection, PgPool, Postgres};

/// One daemon per database: `pinakesd` holds it for its whole life (`own_database`). Every
/// version of the daemon takes this key, so an older and a newer daemon exclude each other.
pub const INSTANCE: i64 = 73822917;
/// The write order (`store::lock_writers`), shared or exclusive, held for one transaction. It
/// differs from `INSTANCE`: the daemon holds that one, so a writer taking it would wait forever.
pub const WRITE_ORDER: i64 = 73822920;
/// Ingested batches and reference rebuilds, one at a time (`ingest`).
pub const INGEST: i64 = 73822916;
/// The change sequence, the last lock a writer takes (`changes::append`).
pub const CHANGE_SEQUENCE: i64 = 73822918;
/// Alias assignments (`alias::lock`).
pub const ALIAS: i64 = 73822919;
/// A group run, before the write order (`groups`).
pub const GROUP: i64 = 73491620;
/// Every key above, with the name `scripts/lock-waits` reports it by.
pub const ALL: [(i64, &str); 6] = [
    (INSTANCE, "instance"),
    (WRITE_ORDER, "write order"),
    (INGEST, "ingest"),
    (CHANGE_SEQUENCE, "change sequence"),
    (ALIAS, "alias"),
    (GROUP, "group"),
];

/// Take the instance lock on a connection of its own, held until the connection is dropped.
/// Host, author and IP budgets are process-local, and the lock keeps their guarantees.
pub async fn own_database(pool: &PgPool) -> Result<PoolConnection<Postgres>> {
    let mut connection = pool.acquire().await?;
    let owns_database: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(INSTANCE)
        .fetch_one(&mut *connection)
        .await?;
    if !owns_database {
        bail!("another pinakesd already owns this database");
    }
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::ALL;
    #[test]
    fn every_key_differs_and_lock_waits_names_it() {
        let mut keys: Vec<_> = ALL.iter().map(|(key, _)| key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), ALL.len());
        let script = include_str!("../../../scripts/lock-waits");
        for (key, name) in ALL {
            assert!(
                script.contains(&format!("WHEN {key} THEN '{name}'")),
                "scripts/lock-waits names {key} {name}"
            );
        }
    }
}
