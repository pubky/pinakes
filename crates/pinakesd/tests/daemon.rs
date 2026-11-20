//! The `pinakesd` binary against a database of its own. The daemon holds the instance lock for its
//! whole life (`locks::own_database`), and its writes and a re-extraction still commit.
use sqlx::PgPool;
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

/// A config for the test database on free loopback ports, with nothing that reaches out: no
/// Nexus listens on the discard port, and Pubky and the canaries are off.
fn config(pool: &PgPool, dir: &Path) -> (PathBuf, String) {
    let mut database = url::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    database.set_path(&format!(
        "/{}",
        pool.connect_options().get_database().unwrap()
    ));
    let port = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let (listen, admin) = (port(), port());
    std::fs::create_dir_all(dir.join("dumps")).unwrap();
    let path = dir.join("pinakes.toml");
    std::fs::write(
        &path,
        format!(
            "database_url = {:?}\nlisten = \"127.0.0.1:{listen}\"\nadmin_listen = \"127.0.0.1:{admin}\"\n\
             nexus_url = \"http://127.0.0.1:9\"\n[pubky]\nenabled = false\n[canary]\nenabled = false\n\
             [osm]\ndump_dir = {:?}\n",
            database.as_str(),
            dir.join("dumps").to_str().unwrap(),
        ),
    )
    .unwrap();
    (path, format!("http://127.0.0.1:{listen}"))
}
fn daemon(config: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pinakesd"));
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .kill_on_drop(true);
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("PINAKES__")) {
        command.env_remove(key);
    }
    // The daemon's default log filter, whatever the caller's.
    command.env_remove("RUST_LOG");
    command
}
/// Run a command that exits by itself, within a minute.
async fn run(config: &Path, args: &[&str]) -> Output {
    let child = daemon(config, args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("pinakesd {args:?} did not return within a minute"))
        .unwrap()
}
async fn stop(mut child: Child) {
    let pid = child.id().unwrap().to_string();
    assert!(std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .expect("pinakesd stops on SIGTERM")
        .unwrap();
    assert!(status.success(), "{status}");
}

#[sqlx::test(migrations = false)]
async fn the_daemon_saves_a_link_and_reextracts_while_it_owns_its_database(pool: PgPool) {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(pool.connect_options().get_database().unwrap());
    let (config, base) = config(&pool, &dir);
    let log = std::fs::File::create(dir.join("pinakesd.log")).unwrap();
    let mut serving = daemon(&config, &[])
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let http = reqwest::Client::new();
    let started = Instant::now();
    while !http
        .get(format!("{base}/v1/info"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
    {
        assert!(
            serving.try_wait().unwrap().is_none(),
            "pinakesd exited; see {}",
            dir.join("pinakesd.log").display()
        );
        assert!(started.elapsed() < Duration::from_secs(60), "not serving");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A second daemon on the same database is refused, a re-extraction included.
    let refused = run(&config, &["reextract", "all"]).await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("another pinakesd already owns this database"));

    // A geo: link resolves offline; its worker saves it under the write order.
    let response = http
        .post(format!("{base}/v1/resolve"))
        .json(&serde_json::json!({"uri": "geo:47.3769,8.5417"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let started = Instant::now();
    loop {
        let saved: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM resource WHERE resolver='geo' AND status='ok')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if saved {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the resolved link was never saved; see {}",
            dir.join("pinakesd.log").display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop(serving).await;

    // Re-extraction takes the instance lock, then the write order exclusively.
    let reextract = run(&config, &["reextract", "all"]).await;
    assert!(
        reextract.status.success(),
        "{}",
        String::from_utf8_lossy(&reextract.stderr)
    );
    assert!(String::from_utf8_lossy(&reextract.stdout).contains("re-queued 1 resources"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// A daemon that loses the instance lock (its lock's database session ends, as in a PostgreSQL
/// restart) stops by itself with a failure, so its supervisor starts it again and no second
/// daemon or re-extraction runs beside it.
#[sqlx::test(migrations = false)]
async fn the_daemon_stops_when_it_loses_the_instance_lock(pool: PgPool) {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(pool.connect_options().get_database().unwrap());
    let (config, base) = config(&pool, &dir);
    let log = std::fs::File::create(dir.join("pinakesd.log")).unwrap();
    let mut serving = daemon(&config, &[])
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let http = reqwest::Client::new();
    let started = Instant::now();
    while !http
        .get(format!("{base}/v1/info"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
    {
        assert!(serving.try_wait().unwrap().is_none(), "pinakesd exited");
        assert!(started.elapsed() < Duration::from_secs(60), "not serving");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // End the session that holds the instance lock, and only that one.
    let ended: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(l.pid) FROM pg_locks l JOIN pg_database d ON d.oid=l.database \
         WHERE d.datname=current_database() AND l.locktype='advisory' AND l.granted \
           AND l.classid=0 AND l.objid::bigint=$1 AND l.pid<>pg_backend_pid()",
    )
    .bind(pinakes_core::locks::INSTANCE)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(ended, [true], "one session holds the instance lock");
    // At most the lock check's interval and its answer's wait, then the stop itself.
    let status = tokio::time::timeout(Duration::from_secs(90), serving.wait())
        .await
        .expect("pinakesd stops once its instance lock is gone")
        .unwrap();
    assert!(!status.success(), "{status}");
    let log = std::fs::read_to_string(dir.join("pinakesd.log")).unwrap();
    assert!(
        log.contains("the instance lock's database session is gone or does not answer"),
        "{log}"
    );
    assert!(log.contains("instance lock ended before shutdown"), "{log}");
    std::fs::remove_dir_all(dir).unwrap();
}
