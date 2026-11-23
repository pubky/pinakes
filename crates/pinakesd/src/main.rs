use anyhow::{bail, Context, Result};
use futures_util::FutureExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    ingest, migrate, queue,
};
use std::{path::PathBuf, time::Duration};
use tokio::sync::watch;

fn main() -> Result<()> {
    let (config, command) = parse_args(std::env::args().skip(1))?;
    // The parse worker runs before any runtime starts: its memory limit is for one thread's
    // parse, not for a runtime's threads and their address space.
    if let Command::ParseWorker(secs) = command {
        std::process::exit(pinakes_core::resolver::html::worker_main(secs));
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(serve(config, command));
    // Whatever still runs on a blocking thread (a dump write, a parse) is left: the process
    // ends here, within the shutdown grace.
    runtime.shutdown_background();
    result
}
/// The connections of the daemon's own pool: the workers, ingestion and the background passes.
/// The API has a pool of its own (`api::pool_options`).
const POOL_CONNECTIONS: u32 = 20;
/// How long a stopping daemon waits for its pool's connections to close.
const POOL_CLOSE: Duration = Duration::from_secs(5);
/// How often retention and orphan cleanup run.
const MAINTENANCE_PERIOD: Duration = Duration::from_secs(3600);

async fn serve(config: Option<PathBuf>, command: Command) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "pinakes=info,pinakes_core=info,sqlx=warn".into());
    tracing_subscriber::util::SubscriberInitExt::try_init(subscriber(filter, std::io::stdout))?;
    let (migrate_only, reextract) = match command {
        Command::Serve => (false, None),
        Command::Migrate => (true, None),
        Command::Reextract(routes) => (false, Some(routes)),
        Command::Print(text) => {
            println!("{text}");
            return Ok(());
        }
        Command::Healthcheck => return healthcheck(config.as_deref()).await,
        Command::ParseWorker(_) => unreachable!("handled before the runtime"),
    };
    // The signal handlers come first, so a stop is heeded during startup, a migration and a
    // re-extraction too (as a container's first process the daemon has no default handlers).
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    let stop = async move {
        tokio::select! { _=terminate.recv()=>{}, _=interrupt.recv()=>{} }
    };
    tokio::pin!(stop);
    let one_shot = migrate_only || reextract.is_some();
    let daemon = tokio::select! {
        // Each step so far is a transaction of its own: one cut off is rolled back.
        _ = &mut stop => {
            anyhow::ensure!(!one_shot, "stopped by a signal before it finished");
            tracing::info!("stopped by a signal during startup");
            return Ok(());
        }
        daemon = start(config, migrate_only, reextract) => daemon?,
    };
    match daemon {
        Some(daemon) => daemon.run(stop).await,
        None => Ok(()),
    }
}

/// The running daemon: its background tasks and workers, and what stops them.
struct Daemon {
    tasks: Tasks,
    workers: tokio::task::JoinSet<()>,
    shutdown: watch::Sender<bool>,
    grace: Duration,
    pool: sqlx::PgPool,
    instance_lock: std::sync::Arc<tokio::sync::Mutex<pinakes_core::locks::InstanceLock>>,
    /// Ends once the instance lock is lost, never otherwise.
    lock_lost: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
}
impl Daemon {
    async fn run(self, stop: impl std::future::Future<Output = ()>) -> Result<()> {
        let result = supervise(
            stop,
            self.lock_lost,
            self.tasks,
            self.workers,
            self.shutdown,
            self.grace,
        )
        .await;
        // The lock goes last, after every task that writes.
        drop(self.instance_lock);
        // Closing waits for every connection to come back: no longer than a few seconds, since
        // a database that does not answer may be why the daemon stops.
        let _ = tokio::time::timeout(POOL_CLOSE, self.pool.close()).await;
        result
    }
}

/// What a daemon that has lost its instance lock names as the task that ended.
const INSTANCE_LOCK: &str = "instance lock";
/// The daemon's background tasks, by name.
#[derive(Default)]
struct Tasks(tokio::task::JoinSet<Ended>);
/// How a background task ended.
struct Ended {
    name: &'static str,
    panicked: bool,
    /// Whether the task runs until shutdown: ending earlier is a failure.
    lasting: bool,
}
impl Tasks {
    /// A task that runs until shutdown. When it ends before, or panics, the daemon stops.
    fn spawn(
        &mut self,
        name: &'static str,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        self.add(name, true, task);
    }
    /// A task that ends by itself when its feature is off. When it panics, the daemon stops.
    fn spawn_optional(
        &mut self,
        name: &'static str,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        self.add(name, false, task);
    }
    fn add(
        &mut self,
        name: &'static str,
        lasting: bool,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        self.0.spawn(async move {
            let panicked = std::panic::AssertUnwindSafe(task)
                .catch_unwind()
                .await
                .is_err();
            Ended {
                name,
                panicked,
                lasting,
            }
        });
    }
}

/// Run until `stop` (a signal), until `lock_lost` (the instance lock is gone), or until a
/// background task or a worker ends by itself or panics: a daemon that kept answering with a
/// dead ingestion, grouping or cleanup loop would look healthy and fall behind. Then signal
/// shutdown and give everything `grace` to finish; what still runs after it is dropped. From
/// the instant the lock is lost, before the shutdown or during it, what runs has
/// `locks::LOST_GRACE` at most: nothing is finished without the lock. An error names what ended
/// early, so the process exits non-zero and its supervisor starts it again.
async fn supervise(
    stop: impl std::future::Future<Output = ()>,
    lock_lost: impl std::future::Future<Output = ()>,
    mut tasks: Tasks,
    mut workers: tokio::task::JoinSet<()>,
    shutdown: watch::Sender<bool>,
    grace: Duration,
) -> Result<()> {
    tokio::pin!(stop, lock_lost);
    let mut lost = false;
    let failed: Option<&'static str> = loop {
        tokio::select! {
            _ = &mut stop => break None,
            _ = &mut lock_lost => {
                lost = true;
                break Some(INSTANCE_LOCK);
            }
            Some(ended) = tasks.0.join_next() => match ended {
                Ok(Ended { panicked: false, lasting: false, .. }) => {}
                Ok(ended) => break Some(ended.name),
                Err(_) => break Some("background task"),
            },
            Some(_) = workers.join_next() => break Some("queue worker"),
        }
    };
    if let Some(name) = failed {
        tracing::error!(task = name, "a background task ended; stopping the daemon");
    }
    let lost_grace = pinakes_core::locks::LOST_GRACE;
    let stopping = tokio::time::Instant::now();
    let mut ends = stopping + if lost { grace.min(lost_grace) } else { grace };
    let _ = shutdown.send(true);
    // A task or worker that panics while it stops is a failure too.
    let mut panicked: Option<&'static str> = None;
    let drain = async {
        while let Some(ended) = tasks.0.join_next().await {
            match ended {
                Ok(Ended {
                    panicked: false, ..
                }) => {}
                Ok(ended) => panicked = panicked.or(Some(ended.name)),
                Err(_) => panicked = panicked.or(Some("background task")),
            }
        }
        while let Some(ended) = workers.join_next().await {
            if ended.is_err() {
                panicked = panicked.or(Some("queue worker"));
            }
        }
    };
    let finished = {
        tokio::pin!(drain);
        loop {
            tokio::select! {
                finished = tokio::time::timeout_at(ends, &mut drain) => break finished.is_ok(),
                // The lock is watched while the daemon stops too.
                _ = &mut lock_lost, if !lost => {
                    lost = true;
                    ends = ends.min(tokio::time::Instant::now() + lost_grace);
                }
            }
        }
    };
    if !finished {
        // The grace this stop had: after a lost lock, no longer than `LOST_GRACE` from then.
        tracing::warn!(
            grace_secs = ends.duration_since(stopping).as_secs_f64(),
            lock_lost = lost,
            "shutdown grace passed; stopping with work still running"
        );
    }
    match (failed, panicked) {
        (Some(name), _) => bail!("{name} ended before shutdown"),
        (None, Some(name)) => bail!("{name} panicked while stopping"),
        (None, None) => Ok(()),
    }
}

/// Everything up to the listeners: `None` for a command that has finished (`--migrate`,
/// `reextract`).
async fn start(
    config: Option<PathBuf>,
    migrate_only: bool,
    reextract: Option<Vec<String>>,
) -> Result<Option<Daemon>> {
    // Every HTML parse runs in a child `pinakesd parse-worker`, killed at its limits; a daemon
    // that cannot run one does not serve.
    pinakes_core::resolver::html::set_worker(std::env::current_exe()?, ["parse-worker"]);
    pinakes_core::resolver::html::check_worker()
        .context("the parse worker (`pinakesd parse-worker`) does not run")?;
    let config = Config::load(config.as_deref())?;
    for key in &config.ignored {
        tracing::warn!(key, "config key ignored: this build no longer reads it");
    }
    pinakes_core::configure_sources(&config);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(POOL_CONNECTIONS)
        .connect(&config.database_url)
        .await?;
    migrate(&pool).await?;
    if migrate_only {
        return Ok(None);
    }
    // Build the public document now, so a malformed recipe file stops startup before workers,
    // ingestion and the listeners start.
    let _ = api::openapi_document();
    let instance_lock = pinakes_core::locks::own_database(&pool).await?;
    if let Some(routes) = reextract {
        let scrubbed = pinakes_core::changes::scrub_stored(&pool).await?;
        println!("scrubbed {scrubbed} posts gone or opted out");
        let requeued = queue::reextract(&pool, &routes).await?;
        println!("re-queued {requeued} resources");
        return Ok(None);
    }
    warn_plain_nexus(&config);
    ingest::check_feed(&pool, &config.nexus_url).await?;
    pinakes_core::groups::configure(&pool, &config.groups).await?;
    pinakes_core::also_on::configure(&pool, &config).await?;
    let api_pool = api::pool_options().connect(&config.database_url).await?;
    let state = AppState::new(api_pool, config.clone())?;
    // Both listeners are bound before any task or worker starts: from here on nothing waits,
    // so a signal during startup never cuts off a worker that has claimed a job.
    let admin = tokio::net::TcpListener::bind(&config.admin_listen).await?;
    let admin_address = admin.local_addr()?;
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // The shutdown starts when a task or worker ends, the lock is lost or a signal comes: from
    // then on the admin `/healthz` answers 503.
    let stopping = state.stopping.clone();
    let mut stop_seen = shutdown_rx.clone();
    tokio::spawn(async move {
        let _ = stop_seen.changed().await;
        stopping.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let mut tasks = Tasks::default();
    let instance_lock = std::sync::Arc::new(tokio::sync::Mutex::new(instance_lock));
    // The lock is watched until the daemon has stopped, through its shutdown too.
    let watched = pinakes_core::locks::InstanceLock::watch(instance_lock.clone());
    let lock_lost = Box::pin(async move {
        watched.await;
        tracing::error!(
            "the instance lock's database session is gone or does not answer; stopping"
        );
    });
    // One Egress for the resolvers and the also-on sources: the per-host budgets are shared.
    let egress = pinakes_core::resolver::egress::Egress::new(config.egress.clone());
    let workers = queue::spawn_with_egress(
        pool.clone(),
        config.clone(),
        state.wake.clone(),
        shutdown_rx.clone(),
        state.metrics.clone(),
        egress.clone(),
    )?;
    tasks.spawn_optional(
        "also-on",
        pinakes_core::also_on::run(
            pool.clone(),
            pinakes_core::also_on::Context {
                egress,
                config: std::sync::Arc::new(config.clone()),
            },
            pinakes_core::also_on::registry(),
            shutdown_rx.clone(),
        ),
    );
    let ingest_state = state.clone();
    let ingest_pool = pool.clone();
    let mut ingest_shutdown = shutdown_rx.clone();
    tasks.spawn("ingestion", async move {
        // An answer that starts the feed over is confirmed by the next read, `poll_secs` later.
        let mut resets =
            ingest::Resets::new(Duration::from_secs(ingest_state.config.poll_secs));
        loop {
            // A panicking line is skipped inside `apply`; anything else that panics is counted
            // (`pinakes_ingest_panics_total`) and retried like an error with the cursor held.
            let poll = std::panic::AssertUnwindSafe(ingest::poll_once(
                &ingest_pool,
                &ingest_state.config.nexus_url,
                &ingest_state.nexus,
                &ingest_state.config.ingest,
                &ingest_state.metrics,
                &mut resets,
            ))
            .catch_unwind();
            let polled = tokio::select! { _=ingest_shutdown.changed()=>break,result=poll=>result.unwrap_or_else(|_| { ingest_state.metrics.ingest_panics.inc(); Err(anyhow::anyhow!("ingestion panicked")) }) };
            match polled {
                Ok(poll) => {
                    if poll.lines > 0 {
                        tracing::info!(lines = poll.lines, "ingested Nexus batch");
                        ingest_state.wake.notify_waiters();
                    }
                    // More may wait: the next page at once. After a reset, while a reset
                    // waits for its confirming read, and when the feed held nothing new, the
                    // next poll waits.
                    if poll.again {
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(error=%pinakes_core::resolver::log_error(&e),"ingestion will retry")
                }
            }
            tokio::select! { _=ingest_shutdown.changed()=>break,_=tokio::time::sleep(Duration::from_secs(ingest_state.config.poll_secs))=>{} }
        }
    });
    let group_pool = pool.clone();
    let group_config = config.groups.clone();
    let mut group_shutdown = shutdown_rx.clone();
    tasks.spawn("grouping", async move {
        loop {
            let batch = tokio::select! { _=group_shutdown.changed()=>break,result=pinakes_core::groups::drain(&group_pool,&group_config,25)=>result };
            match batch {
                Ok(25) => {
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(error = %pinakes_core::resolver::log_error(&error),"group reconciliation will retry")
                }
                _ => {}
            }
            tokio::select! {_=group_shutdown.changed()=>break,_=tokio::time::sleep(Duration::from_secs(2))=>{}}
        }
    });
    tasks.spawn_optional(
        "same-event hints",
        pinakes_core::same_event::run(pool.clone(), config.clone(), shutdown_rx.clone()),
    );
    tasks.spawn_optional(
        "same-recording hints",
        pinakes_core::same_recording::run(pool.clone(), config.clone(), shutdown_rx.clone()),
    );
    tasks.spawn(
        "maintenance",
        pinakes_core::maintenance::run(
            pool.clone(),
            config.grace_days,
            config.preview_grace_hours,
            config.refresh.retention_days,
            MAINTENANCE_PERIOD,
            shutdown_rx.clone(),
        ),
    );
    tasks.spawn(
        "ODbL dump",
        pinakes_core::osm_dump::run(
            pool.clone(),
            PathBuf::from(&config.osm.dump_dir),
            Duration::from_secs(config.osm.dump_every_hours * 3600),
            shutdown_rx.clone(),
        ),
    );
    warn_admin_listener(admin_address);
    warn_admin_without_token(admin_address, config.admin_token.is_some());
    let admin_app = api::admin_router(state.clone());
    let mut admin_shutdown = shutdown_rx.clone();
    tasks.spawn("admin listener", async move {
        let served = axum::serve(admin, admin_app)
            .with_graceful_shutdown(async move {
                let _ = admin_shutdown.changed().await;
            })
            .await;
        if let Err(error) = served {
            tracing::error!(error = %pinakes_core::resolver::log_error(&error), "the admin listener failed");
        }
    });
    tracing::info!(listen=%config.listen,"Pinakes started");
    if config.pubky.enabled {
        pinakes_core::resolver::pubky_apps::log_links_off();
    }
    let public_app =
        api::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let mut public_shutdown = shutdown_rx.clone();
    tasks.spawn("public listener", async move {
        let served = axum::serve(listener, public_app)
            .with_graceful_shutdown(async move {
                let _ = public_shutdown.changed().await;
            })
            .await;
        if let Err(error) = served {
            tracing::error!(error = %pinakes_core::resolver::log_error(&error), "the public listener failed");
        }
    });
    Ok(Some(Daemon {
        tasks,
        workers,
        shutdown: shutdown_tx,
        grace: Duration::from_secs(config.shutdown_grace_secs),
        pool,
        instance_lock,
        lock_lost,
    }))
}

/// A `nexus_authorization` sent to an `http://` Nexus on another machine crosses the network in
/// clear.
fn warn_plain_nexus(config: &Config) {
    if config.nexus_authorization_in_clear() {
        tracing::warn!(nexus = %pinakes_core::resolver::log_host(&config.nexus_url), "nexus_authorization is sent over http to a host that is not local; use an https nexus_url");
    }
}

fn warn_admin_listener(address: std::net::SocketAddr) {
    if !address.ip().is_loopback() {
        tracing::warn!(admin_listen=%address, "admin listener is not loopback; bearer authentication is required, use TLS at the network edge");
    }
}

/// The `Host` check keeps browser pages off the admin listener, not other programs on this
/// machine: without `admin_token` any of them can read it and curate groups through it.
fn warn_admin_without_token(address: std::net::SocketAddr, token: bool) {
    if !token {
        tracing::warn!(admin_listen=%address, "admin listener has no admin_token; any process on this machine can use it, set PINAKES__ADMIN_TOKEN");
    }
}

/// The daemon's JSON log: `filter` (`RUST_LOG`, else the default), except that the MCP library
/// logs nothing below warn whatever the filter says. rmcp logs whole requests and results at debug
/// and trace, and notifications at info: a tool's arguments, such as a private calendar URL, before
/// any validation.
///
/// Both filters are layers of their own, so they decide for the whole subscriber. A filter
/// attached to the output layer alone (`with_filter`) keeps its verdict per thread until the
/// event arrives: a question that no event follows (a library asking whether a record would be
/// logged) leaves a refusal behind, and the next line on that thread is dropped.
fn subscriber<W>(
    filter: tracing_subscriber::EnvFilter,
    writer: W,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt as _;
    let quiet_mcp = tracing_subscriber::filter::filter_fn(|meta| {
        let target = meta.target();
        !(target == "rmcp" || target.starts_with("rmcp::")) || *meta.level() <= tracing::Level::WARN
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(quiet_mcp)
        .with(tracing_subscriber::fmt::layer().json().with_writer(writer))
}

enum Command {
    Serve,
    Migrate,
    Reextract(Vec<String>),
    Print(String),
    /// `parse-worker <cpu seconds>`: the HTML parse worker the daemon runs itself in.
    ParseWorker(u64),
    /// `healthcheck`: `GET /healthz` on the configured admin listener (the image's
    /// `HEALTHCHECK`, which has no HTTP client of its own).
    Healthcheck,
}
/// How long `pinakesd healthcheck` waits for the admin listener's answer, all of it.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// `GET /healthz` on the admin listener of the configuration at `config` (and the environment):
/// an error, so a non-zero exit, unless it answers 200. A listener on every address is asked on
/// loopback. Without `--config` (the image's `HEALTHCHECK`), a configuration that is only valid
/// with its file still names its admin listener: `PINAKES__ADMIN_LISTEN`, else the default.
async fn healthcheck(config: Option<&std::path::Path>) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let admin_listen = match (Config::load(config), config) {
        (Ok(config), _) => config.admin_listen,
        (Err(_), None) => std::env::var("PINAKES__ADMIN_LISTEN")
            .unwrap_or_else(|_| Config::default().admin_listen),
        (Err(error), Some(_)) => return Err(error),
    };
    let mut address: std::net::SocketAddr = admin_listen.parse().context("invalid admin_listen")?;
    if address.ip().is_unspecified() {
        address.set_ip(match address {
            std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
            std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    let ask = async {
        let mut stream = tokio::net::TcpStream::connect(address).await?;
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n",
            api::HEALTHZ
        );
        stream.write_all(request.as_bytes()).await?;
        let mut answer = Vec::new();
        stream.take(4096).read_to_end(&mut answer).await?;
        anyhow::Ok(answer)
    };
    let answer = tokio::time::timeout(HEALTHCHECK_TIMEOUT, ask)
        .await
        .context("the admin listener did not answer in time")?
        .with_context(|| format!("the admin listener at {address} did not answer"))?;
    let status = String::from_utf8_lossy(&answer)
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1).map(str::to_owned))
        .unwrap_or_default();
    anyhow::ensure!(status == "200", "{} answered {status:?}", api::HEALTHZ);
    println!("healthy");
    Ok(())
}
/// Options in any position; every other word after `reextract` is a route, checked here, before
/// any config is read or database touched.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<(Option<PathBuf>, Command)> {
    let mut args = args.into_iter();
    let (mut config, mut command) = (None, Command::Serve);
    let mut routes: Option<Vec<String>> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config = Some(PathBuf::from(
                    args.next().context("--config requires a TOML path")?,
                ))
            }
            "--migrate" => command = Command::Migrate,
            "parse-worker" => {
                let secs = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
                return Ok((config, Command::ParseWorker(secs)));
            }
            "reextract" => routes = Some(vec![]),
            "healthcheck" if routes.is_none() => command = Command::Healthcheck,
            "--openapi" => {
                return Ok((
                    config,
                    Command::Print(serde_json::to_string_pretty(&api::openapi_snapshot())?),
                ))
            }
            "--help" => {
                return Ok((
                    config,
                    Command::Print(
                        "pinakesd [--config PATH] [--migrate | --openapi | healthcheck | reextract ROUTE…|all|also-on | parse-worker SECS]"
                            .into(),
                    ),
                ))
            }
            other if other.starts_with('-') => bail!("unknown argument {arg}"),
            _ => match &mut routes {
                Some(routes) => routes.push(arg),
                None => bail!("unknown argument {arg}"),
            },
        }
    }
    if let Some(routes) = routes {
        anyhow::ensure!(
            matches!(command, Command::Serve),
            "reextract runs alone, not with --migrate"
        );
        queue::check_routes(&routes)?;
        command = Command::Reextract(routes);
    }
    Ok((config, command))
}

#[cfg(test)]
mod tests {
    /// `parse-worker SECS` is the parse worker, whatever else is on the line.
    #[test]
    fn parse_worker_takes_its_cpu_seconds() {
        let parse = |a: &[&str]| super::parse_args(a.iter().map(|s| s.to_string()));
        let (_, command) = parse(&["--config", "x.toml", "parse-worker", "3"]).unwrap();
        assert!(matches!(command, super::Command::ParseWorker(3)));
        let (_, command) = parse(&["parse-worker"]).unwrap();
        assert!(matches!(command, super::Command::ParseWorker(1)));
    }
    #[test]
    fn options_parse_in_any_position_and_routes_are_checked_first() {
        let parse = |a: &[&str]| super::parse_args(a.iter().map(|s| s.to_string()));
        for args in [
            &["reextract", "all", "--config", "x.toml"][..],
            &["--config", "x.toml", "reextract", "all"],
        ] {
            let (config, command) = parse(args).unwrap();
            assert_eq!(config.as_deref(), Some(std::path::Path::new("x.toml")));
            assert!(matches!(command, super::Command::Reextract(r) if r == ["all"]));
        }
        let (config, command) = parse(&["healthcheck", "--config", "x.toml"]).unwrap();
        assert_eq!(config.as_deref(), Some(std::path::Path::new("x.toml")));
        assert!(matches!(command, Command::Healthcheck));
        assert!(parse(&["reextract", "healthcheck"]).is_err());
        let (_, command) = parse(&["reextract", "also-on"]).unwrap();
        assert!(matches!(command, Command::Reextract(routes) if routes == ["also-on"]));
        let (_, command) = parse(&["reextract", "web", "--config", "x.toml", "osm"]).unwrap();
        assert!(matches!(command, super::Command::Reextract(r) if r == ["web", "osm"]));
        for bad in [
            &["reextract"][..],
            &["reextract", "nonsense"],
            &["reextract", "all", "--migrate"],
            &["stray"],
            &["--bogus"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    /// Whatever RUST_LOG says, the MCP library's request, result and notification logs (which
    /// carry a tool's arguments) stay out; its warnings and the daemon's own debug logs stay in.
    #[test]
    fn mcp_payloads_are_never_logged() {
        for env in [
            "debug",
            "trace",
            "rmcp=trace",
            "rmcp::service=debug,pinakes_core=debug",
        ] {
            let output = Arc::new(Mutex::new(Vec::new()));
            let writer = LogWriter(output.clone());
            let filter = tracing_subscriber::EnvFilter::new(env);
            tracing::subscriber::with_default(subscriber(filter, move || writer.clone()), || {
                let request = "CallToolRequest { arguments: {\"url\": \"https://calendar.example/private/s3cret.ics\"} }";
                // The calls rmcp makes (service.rs, the streamable HTTP server), by its targets.
                tracing::debug!(target: "rmcp::service", id = 1, request, "received request");
                tracing::debug!(target: "rmcp::service", id = 1, result = request, "response message");
                tracing::info!(target: "rmcp::service", notification = request, "received notification");
                tracing::trace!(target: "rmcp::transport::streamable_http_server::tower", message = request);
                tracing::trace!(target: "rmcp", message = request);
                tracing::warn!(target: "rmcp::service", "mcp warning stays");
                tracing::debug!(target: "pinakes_core::agents", "own debug stays");
            });
            let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
            assert!(!logs.contains("s3cret"), "{env}: {logs}");
            assert!(logs.contains("mcp warning stays"), "{env}: {logs}");
            assert_eq!(
                logs.contains("own debug stays"),
                env != "rmcp=trace",
                "{env}: {logs}"
            );
        }
    }
    /// The tasks of a supervision test: `quiet` run until shutdown, as the daemon's loops do.
    fn quiet(tasks: &mut Tasks, name: &'static str, shutdown: &watch::Receiver<bool>) {
        let mut shutdown = shutdown.clone();
        tasks.spawn(name, async move {
            let _ = shutdown.changed().await;
        });
    }
    /// A background task that ends by itself, or panics, stops the daemon with an error that
    /// names it, after the other tasks were told to stop; a worker that ends does too. A task
    /// whose feature is off may end, and a stop signal is a clean stop.
    #[tokio::test]
    async fn a_background_task_that_ends_stops_the_daemon() {
        let grace = Duration::from_secs(5);
        let never = std::future::pending::<()>;
        for early in ["returns", "panics", "worker"] {
            let (stop, shutdown) = watch::channel(false);
            let mut tasks = Tasks::default();
            let mut workers = tokio::task::JoinSet::new();
            quiet(&mut tasks, "grouping", &shutdown);
            let mut seen = shutdown.clone();
            workers.spawn(async move {
                if early != "worker" {
                    let _ = seen.changed().await;
                }
            });
            match early {
                "returns" => tasks.spawn("ingestion", async {}),
                "panics" => tasks.spawn("ingestion", async { panic!("ingestion panicked") }),
                _ => {}
            }
            let stopped = shutdown.clone();
            let error = supervise(never(), never(), tasks, workers, stop, grace)
                .await
                .unwrap_err()
                .to_string();
            let name = if early == "worker" {
                "queue worker"
            } else {
                "ingestion"
            };
            assert_eq!(error, format!("{name} ended before shutdown"), "{early}");
            assert!(
                *stopped.borrow(),
                "{early}: the other tasks were told to stop"
            );
        }
        // A task whose feature is off ends at once; the daemon runs on until its signal.
        let (stop, shutdown) = watch::channel(false);
        let mut tasks = Tasks::default();
        quiet(&mut tasks, "grouping", &shutdown);
        tasks.spawn_optional("also-on", async {});
        let signal = tokio::time::sleep(Duration::from_millis(200));
        let started = std::time::Instant::now();
        supervise(
            signal,
            never(),
            tasks,
            tokio::task::JoinSet::new(),
            stop,
            grace,
        )
        .await
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(200));
        // But its panic stops the daemon.
        let (stop, _shutdown) = watch::channel(false);
        let mut tasks = Tasks::default();
        tasks.spawn_optional("also-on", async { panic!("also-on panicked") });
        let error = supervise(
            never(),
            never(),
            tasks,
            tokio::task::JoinSet::new(),
            stop,
            grace,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "also-on ended before shutdown");
    }
    /// A task that panics while it stops makes the stop a failure.
    #[tokio::test]
    async fn a_panic_while_stopping_is_a_failure() {
        let (stop, shutdown) = watch::channel(false);
        let mut tasks = Tasks::default();
        let mut seen = shutdown.clone();
        tasks.spawn("maintenance", async move {
            let _ = seen.changed().await;
            panic!("maintenance panicked while stopping");
        });
        let error = supervise(
            async {},
            std::future::pending(),
            tasks,
            tokio::task::JoinSet::new(),
            stop,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "maintenance panicked while stopping");
    }
    /// Shutdown waits for the tasks no longer than its grace: one that does not stop is left.
    #[tokio::test]
    async fn shutdown_waits_no_longer_than_its_grace() {
        let (stop, _shutdown) = watch::channel(false);
        let mut tasks = Tasks::default();
        tasks.spawn("stuck", std::future::pending::<()>());
        let started = std::time::Instant::now();
        supervise(
            async {},
            std::future::pending(),
            tasks,
            tokio::task::JoinSet::new(),
            stop,
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(300) && waited < Duration::from_secs(5));
    }
    /// A daemon that has lost its instance lock gives its tasks `LOST_GRACE`, not the
    /// shutdown grace: a worker in the middle of a batch is cut off there. A task that ends
    /// with the lock held leaves the full grace.
    #[tokio::test]
    async fn a_lost_instance_lock_cuts_the_grace_short() {
        let grace = Duration::from_secs(4);
        let lost = pinakes_core::locks::LOST_GRACE;
        assert!(lost + Duration::from_millis(1500) < grace);
        for (ended, least, most) in [
            (INSTANCE_LOCK, lost, lost + Duration::from_millis(1500)),
            ("ingestion", grace, grace + Duration::from_secs(2)),
        ] {
            let (stop, _shutdown) = watch::channel(false);
            let mut tasks = Tasks::default();
            let lock_lost: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> =
                if ended == INSTANCE_LOCK {
                    Box::pin(async {})
                } else {
                    tasks.spawn(ended, async {});
                    Box::pin(std::future::pending())
                };
            // A worker whose batch does not end.
            let mut workers = tokio::task::JoinSet::new();
            workers.spawn(std::future::pending::<()>());
            let started = std::time::Instant::now();
            let stop_signal = std::future::pending::<()>();
            let error = supervise(stop_signal, lock_lost, tasks, workers, stop, grace)
                .await
                .unwrap_err();
            let waited = started.elapsed();
            assert_eq!(error.to_string(), format!("{ended} ended before shutdown"));
            assert!(waited >= least && waited < most, "{ended}: {waited:?}");
        }
    }
    /// The lock is watched while the daemon stops: one lost half a second into a shutdown
    /// leaves `LOST_GRACE` from then, not the rest of the grace. The stop was asked for, so
    /// it is no failure.
    #[tokio::test]
    async fn a_lock_lost_while_stopping_cuts_the_grace_short() {
        let grace = Duration::from_secs(6);
        let after = Duration::from_millis(500);
        let lost = pinakes_core::locks::LOST_GRACE;
        let (stop, _shutdown) = watch::channel(false);
        let mut workers = tokio::task::JoinSet::new();
        workers.spawn(std::future::pending::<()>());
        let started = std::time::Instant::now();
        supervise(
            async {},
            tokio::time::sleep(after),
            Tasks::default(),
            workers,
            stop,
            grace,
        )
        .await
        .unwrap();
        let waited = started.elapsed();
        assert!(
            waited >= after + lost && waited < after + lost + Duration::from_millis(1500),
            "{waited:?}"
        );
    }
    #[test]
    fn warns_when_the_admin_listener_has_no_token() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            warn_admin_without_token("127.0.0.1:9090".parse().unwrap(), true);
            assert!(output.lock().unwrap().is_empty());
            warn_admin_without_token("127.0.0.1:9090".parse().unwrap(), false);
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(logs.matches("admin listener has no admin_token").count(), 1);
        assert!(logs.contains("127.0.0.1:9090"));
        assert!(logs.contains("PINAKES__ADMIN_TOKEN"));
    }
    #[test]
    fn warns_for_non_loopback_admin_binds_only() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for local in ["127.0.0.1:9090", "[::1]:9090"] {
                warn_admin_listener(local.parse().unwrap());
            }
            assert!(output.lock().unwrap().is_empty());
            for exposed in ["0.0.0.0:9090", "[::]:9090", "192.168.1.1:9090"] {
                warn_admin_listener(exposed.parse().unwrap());
            }
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(logs.matches("bearer authentication is required").count(), 3);
        assert!(logs.contains("0.0.0.0:9090"));
        assert!(logs.contains("[::]:9090"));
    }
}
