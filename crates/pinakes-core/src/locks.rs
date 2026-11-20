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
/// and how long one check may take. Fixed: with [`LOST_GRACE`] it bounds how long a daemon
/// works on without its lock, which no deployment needs to tune. A session that is gone
/// without its connection failing is noticed within three times this (the wait and two checks
/// with no answer), and the daemon's workers and background tasks are dropped `LOST_GRACE`
/// later: 32 seconds. The process ends once its database connections are closed, a few
/// seconds more (`pinakesd`), in which a request still open may be answered.
pub const LOCK_CHECK: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a daemon that has lost the instance lock gives its tasks to stop at most, in place
/// of `limits.shutdown_grace_secs` (less when that grace ends sooner): time for a statement in
/// flight and an open response, no more.
/// Fixed, as `LOCK_CHECK` is. Another daemon may already run on the database, so the workers'
/// batches are cut off instead of finished without the lock: a job cut off before its save is
/// read again when its lease runs out; one cut off after it keeps the card it saved, and the
/// steps after the save wait for its next full read (a read the site answers 304 does not run
/// them; a card the save made final waits for a new reference or `pinakesd reextract`).
pub const LOST_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// What one check of the lock's session found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// The session answered: the lock is held.
    Held,
    /// No answer within [`LOCK_CHECK`], or an error that does not end the session. The session
    /// may only be slow (a loaded host, a stalled network), so the lock may still be held.
    Slow,
    /// The session has ended (its connection failed, or the server said so): the lock is gone
    /// with it.
    Gone,
}
/// What a failed check says about the lock's session. The session has ended, and the lock with
/// it, when the connection failed (an I/O or protocol error, a closed pool) or the server said
/// so: SQLSTATE class 08 (connection exception), or 57P01 to 57P03 (the server shuts down or
/// ended this session). Any other error may leave the session open, with its lock (57014, a
/// cancelled statement): it counts as a check with no answer, and the check made at once after
/// it finds a session that did end by its closed connection.
fn failed_check(error: &sqlx::Error) -> Check {
    use sqlx::Error::{Database, Io, PoolClosed, Protocol};
    match error {
        Io(_) | PoolClosed | Protocol(_) => Check::Gone,
        Database(e) => match e.code().as_deref() {
            Some("57P01" | "57P02" | "57P03") => Check::Gone,
            Some(code) if code.starts_with("08") => Check::Gone,
            _ => Check::Slow,
        },
        _ => Check::Slow,
    }
}
impl InstanceLock {
    /// One check of the lock's session. The lock is the session's: a PostgreSQL restart or a
    /// dropped connection ends both, while the daemon's other connections reconnect. An error
    /// that leaves the session open is [`Check::Slow`] ([`failed_check`]).
    pub async fn check(&mut self) -> Check {
        let alive = sqlx::query("SELECT 1").execute(&mut *self.0);
        match tokio::time::timeout(LOCK_CHECK, alive).await {
            Ok(Ok(_)) => Check::Held,
            Ok(Err(error)) => failed_check(&error),
            Err(_) => Check::Slow,
        }
    }
    /// Check the lock every [`LOCK_CHECK`] and end once it is lost; never otherwise (the daemon
    /// drops the watch when it has stopped, so the lock is watched while it stops too). It is
    /// lost at once when its session has ended ([`Check::Gone`]), and when two checks in a row
    /// get no answer in time, or an error that may leave the session open: one of them does
    /// not stop a daemon that still holds its lock, and the check after it is made at once.
    /// Without the lock another `pinakesd`, or a `reextract`, could start on the database, so
    /// a daemon that has lost it gives what it runs [`LOST_GRACE`] and stops (its supervisor
    /// starts it again, and it takes the lock anew).
    pub async fn watch(lock: std::sync::Arc<tokio::sync::Mutex<Self>>) {
        let check = || async { lock.lock().await.check().await };
        watch_checks(check, LOCK_CHECK).await
    }
}
/// [`InstanceLock::watch`] over any check, one every `every`.
async fn watch_checks<F, Fut>(mut check: F, every: std::time::Duration)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Check>,
{
    let mut slow = false;
    loop {
        // The check after a slow one is made at once: the session has had its time.
        if !slow {
            tokio::time::sleep(every).await;
        }
        match check().await {
            Check::Held => slow = false,
            Check::Slow if !slow => slow = true,
            Check::Slow | Check::Gone => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{failed_check, watch_checks, Check, InstanceLock, ALL};

    /// A database's error with SQLSTATE `0`, or none.
    #[derive(Debug)]
    struct Sqlstate(Option<&'static str>);
    impl std::fmt::Display for Sqlstate {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }
    impl std::error::Error for Sqlstate {}
    impl sqlx::error::DatabaseError for Sqlstate {
        fn message(&self) -> &str {
            "a database error"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            self.0.map(Into::into)
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    /// Only an error that says the session ended is a lost lock: a failed connection, or the
    /// server's word for it (class 08, 57P01 to 57P03). Every other error is a check with no
    /// answer, two of which in a row stop the daemon.
    #[test]
    fn only_a_session_that_ended_is_a_lost_lock() {
        use sqlx::Error;
        let database = |code| Error::Database(Box::new(Sqlstate(code)));
        for gone in [
            Error::Io(std::io::ErrorKind::ConnectionReset.into()),
            Error::PoolClosed,
            Error::Protocol("unexpected message".into()),
            database(Some("08000")),
            database(Some("08006")),
            database(Some("08P01")),
            database(Some("57P01")),
            database(Some("57P02")),
            database(Some("57P03")),
        ] {
            assert_eq!(failed_check(&gone), Check::Gone, "{gone:?}");
        }
        for unsaid in [
            // A cancelled statement, a statement timeout: the session goes on.
            database(Some("57014")),
            database(Some("57000")),
            // Not one of the three: when it did end the session, the next check finds that.
            database(Some("57P04")),
            database(Some("53200")),
            database(Some("40001")),
            database(Some("XX000")),
            database(None),
            Error::PoolTimedOut,
            Error::RowNotFound,
        ] {
            assert_eq!(failed_check(&unsaid), Check::Slow, "{unsaid:?}");
        }
    }

    /// The same over a real session: a statement the server cancels (57014) leaves the session
    /// and its lock, and the next check is answered; a session an administrator ends (57P01)
    /// is gone, and stays gone.
    #[sqlx::test]
    async fn a_cancelled_statement_keeps_the_lock_and_an_ended_session_loses_it(
        pool: sqlx::PgPool,
    ) {
        let mut lock = super::own_database(&pool).await.unwrap();
        sqlx::query("SET statement_timeout=50")
            .execute(&mut *lock.0)
            .await
            .unwrap();
        let cancelled = sqlx::query("SELECT pg_sleep(30)")
            .execute(&mut *lock.0)
            .await
            .unwrap_err();
        let code = cancelled.as_database_error().and_then(|e| e.code());
        assert_eq!(code.as_deref(), Some("57014"));
        assert_eq!(failed_check(&cancelled), Check::Slow);
        sqlx::query("SET statement_timeout=0")
            .execute(&mut *lock.0)
            .await
            .unwrap();
        assert_eq!(lock.check().await, Check::Held);
        // Still the one daemon of its database.
        assert!(super::own_database(&pool).await.is_err());

        let session: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock.0)
            .await
            .unwrap();
        let ended: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
            .bind(session)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(ended);
        // Until the server has ended it.
        loop {
            let left: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid=$1")
                    .bind(session)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if left == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(lock.check().await, Check::Gone);
        assert_eq!(lock.check().await, Check::Gone);
        let InstanceLock(connection) = lock;
        // The failed connection is closed, not handed back to the pool.
        drop(connection);
        super::own_database(&pool).await.unwrap();
    }

    /// The lock is lost at once when its session is gone, and after two checks in a row with
    /// no answer in time; one slow check between answered ones stops nothing.
    #[tokio::test]
    async fn one_slow_lock_check_does_not_stop_the_daemon() {
        use Check::{Gone, Held, Slow};
        let every = std::time::Duration::from_millis(1);
        // Each script, and the number of checks made when the watch says the lock is lost.
        for (script, lost_after) in [
            (vec![Gone], 1),
            (vec![Held, Gone], 2),
            (vec![Slow, Gone], 2),
            (vec![Slow, Slow], 2),
            (vec![Slow, Held, Slow, Held, Held, Slow, Slow], 7),
        ] {
            let mut made = 0;
            let check = || {
                let found = script[made];
                made += 1;
                async move { found }
            };
            watch_checks(check, every).await;
            assert_eq!(made, lost_after, "{script:?}");
        }
        // A lock that is held is watched for as long as the watch is: it never ends by itself.
        let held = watch_checks(|| async { Held }, every);
        assert!(tokio::time::timeout(every * 50, held).await.is_err());
    }
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
