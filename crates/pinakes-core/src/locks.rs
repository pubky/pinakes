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

/// The instance lock, held on a connection of its own until it is dropped. Only
/// [`own_database`] makes one.
#[derive(Debug)]
pub struct InstanceLock(#[allow(dead_code)] PoolConnection<Postgres>);

/// Take the instance lock on a connection of its own, held until the connection is dropped.
/// Host, author and IP budgets are process-local, and the lock keeps their guarantees.
pub async fn own_database(pool: &PgPool) -> Result<InstanceLock> {
    let mut connection = pool.acquire().await?;
    let owns_database: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(INSTANCE)
        .fetch_one(&mut *connection)
        .await?;
    if !owns_database {
        bail!("another pinakesd already owns this database");
    }
    Ok(InstanceLock(connection))
}
/// How often the daemon checks that it still holds the instance lock ([`InstanceLock::watch`]),
/// and how long one check may take. Fixed: it bounds how long two daemons could run on one
/// database, which no deployment needs to tune.
pub const LOCK_CHECK: std::time::Duration = std::time::Duration::from_secs(10);
impl InstanceLock {
    /// Whether the lock's session is alive and answers within [`LOCK_CHECK`]. The lock is the
    /// session's: a PostgreSQL restart or a dropped connection ends both, while the daemon's
    /// other connections reconnect.
    pub async fn held(&mut self) -> bool {
        let alive = sqlx::query("SELECT 1").execute(&mut *self.0);
        matches!(tokio::time::timeout(LOCK_CHECK, alive).await, Ok(Ok(_)))
    }
    /// Check the lock every [`LOCK_CHECK`] until `shutdown`: `true` once it is lost, `false` at
    /// shutdown. Without the lock another `pinakesd`, or a `reextract`, could start on the
    /// database, so a daemon that has lost it stops (its supervisor starts it again, and it
    /// takes the lock anew).
    pub async fn watch(
        lock: std::sync::Arc<tokio::sync::Mutex<Self>>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> bool {
        while !*shutdown.borrow() {
            tokio::select! {
                _ = shutdown.changed() => return false,
                _ = tokio::time::sleep(LOCK_CHECK) => {}
            }
            if !lock.lock().await.held().await {
                return true;
            }
        }
        false
    }
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

    /// `scripts/lock-waits` never repeats any part of `DATABASE_URL` when it cannot sample:
    /// libpq's parse errors quote the token they could not read, and psql's connection errors
    /// name the host, user or database. A canary in each place never reaches its output, which
    /// is the script's fixed lines only. COMMAND ends at once (`true`): the script waits for the
    /// sampler to stop before it runs COMMAND, so the refusal does not depend on how fast the
    /// sampler starts on this machine.
    #[test]
    fn lock_waits_never_prints_the_database_url() {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/lock-waits");
        // Which of the script's refusals a URL gets is libpq's to decide (a version that cannot
        // read a URL says so, another tries it and cannot connect, one may read a parameter it
        // takes from no variable): the test holds the script to its own words, any of them,
        // and never the URL's parts.
        for url in [
            // An unescaped `%` in the password.
            "postgresql://review:CANARY_PASSWORD%zz@localhost/postgres",
            "postgresql:///postgres?host=/run&bogus=CANARY_VALUE",
            "postgresql://CANARY_USER:CANARY_PASSWORD@/CANARY_DB?host=/nonexistent-CANARY",
            "postgresql://CANARY_USER:CANARY_PASSWORD@127.0.0.1:1/CANARY_DB",
            // A line break in the host makes psql's error a line of the script's own form: psql's
            // stderr is never read, whatever its lines look like.
            "postgresql:///postgres?host=/nonexistent%0Alock-waits:%20CANARY_HOST",
        ] {
            let run = std::process::Command::new("bash")
                .args([script, "true"])
                .env("DATABASE_URL", url)
                .output()
                .unwrap();
            let output = [run.stdout, run.stderr].concat();
            let output = String::from_utf8_lossy(&output);
            assert!(!run.status.success(), "{url}: {output}");
            assert!(!output.contains("CANARY"), "{url}: {output}");
            assert!(
                output.contains("lock-waits: DATABASE_URL is not a connection string")
                    || output.contains("lock-waits: DATABASE_URL sets ")
                    || output.contains("lock-waits: cannot connect to the database"),
                "{url}: {output}"
            );
            assert_eq!(output.lines().count(), 2, "{url}: {output}");
        }
    }
}
