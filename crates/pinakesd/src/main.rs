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
    tokio::runtime::Runtime::new()?.block_on(serve(config, command))
}
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
        Command::ParseWorker(_) => unreachable!("handled before the runtime"),
    };
    // Every HTML parse runs in a child `pinakesd parse-worker`, killed at its limits; a daemon
    // that cannot run one does not serve.
    pinakes_core::resolver::html::set_worker(std::env::current_exe()?, ["parse-worker"]);
    pinakes_core::resolver::html::check_worker()
        .context("the parse worker (`pinakesd parse-worker`) does not run")?;
    let config = Config::load(config.as_deref())?;
    pinakes_core::configure_sources(&config);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await?;
    migrate(&pool).await?;
    if migrate_only {
        return Ok(());
    }
    // After migrations, before serving: clustering needs h3 >= 4.2.3.
    pinakes_core::check_extensions(&pool).await?;
    // Build the public document now, so a malformed recipe file stops startup before workers,
    // ingestion and the listeners start.
    let _ = api::openapi_document();
    let instance_lock = pinakes_core::locks::own_database(&pool).await?;
    if let Some(routes) = reextract {
        let scrubbed = pinakes_core::changes::scrub_stored(&pool).await?;
        println!("scrubbed {scrubbed} posts gone or opted out");
        let requeued = queue::reextract(&pool, &routes).await?;
        println!("re-queued {requeued} resources");
        return Ok(());
    }
    pinakes_core::groups::configure(&pool, &config.groups).await?;
    let api_pool = api::pool_options().connect(&config.database_url).await?;
    let state = AppState::new(api_pool, config.clone())?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // One Egress for the resolvers and the also-on sources: the per-host budgets are shared.
    let egress = pinakes_core::resolver::egress::Egress::new(config.egress.clone());
    let mut workers = queue::spawn_with_egress(
        pool.clone(),
        config.clone(),
        state.wake.clone(),
        shutdown_rx.clone(),
        state.metrics.clone(),
        egress.clone(),
    )?;
    let also_on_task = tokio::spawn(pinakes_core::also_on::run(
        pool.clone(),
        pinakes_core::also_on::Context {
            egress,
            config: std::sync::Arc::new(config.clone()),
        },
        pinakes_core::also_on::registry(),
        shutdown_rx.clone(),
    ));
    let ingest_state = state.clone();
    let ingest_pool = pool.clone();
    let mut ingest_shutdown = shutdown_rx.clone();
    let ingest_task = tokio::spawn(async move {
        loop {
            // A panicking line is skipped inside `apply`; anything else that panics is counted
            // (`pinakes_ingest_panics_total`) and retried like an error with the cursor held.
            let poll = std::panic::AssertUnwindSafe(ingest::poll_once(
                &ingest_pool,
                &ingest_state.config.nexus_url,
                &ingest_state.nexus,
                &ingest_state.config.ingest,
                &ingest_state.metrics,
            ))
            .catch_unwind();
            let applied = tokio::select! { _=ingest_shutdown.changed()=>break,result=poll=>result.unwrap_or_else(|_| { ingest_state.metrics.ingest_panics.inc(); Err(anyhow::anyhow!("ingestion panicked")) }) };
            match applied {
                Ok(n) if n > 0 => {
                    tracing::info!(lines = n, "ingested Nexus batch");
                    ingest_state.wake.notify_waiters();
                    continue;
                }
                Ok(_) => {}
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
    let group_task = tokio::spawn(async move {
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
    let same_event_task = tokio::spawn(pinakes_core::same_event::run(
        pool.clone(),
        config.clone(),
        shutdown_rx.clone(),
    ));
    let same_recording_task = tokio::spawn(pinakes_core::same_recording::run(
        pool.clone(),
        config.clone(),
        shutdown_rx.clone(),
    ));
    let maintenance_task = tokio::spawn(pinakes_core::maintenance::run(
        pool.clone(),
        config.grace_days,
        config.refresh.retention_days,
        Duration::from_secs(3600),
        shutdown_rx.clone(),
    ));
    let dump_task = tokio::spawn(pinakes_core::osm_dump::run(
        pool.clone(),
        PathBuf::from(&config.osm.dump_dir),
        Duration::from_secs(config.osm.dump_every_hours * 3600),
        shutdown_rx.clone(),
    ));
    let admin = tokio::net::TcpListener::bind(&config.admin_listen).await?;
    warn_admin_listener(admin.local_addr()?);
    let admin_app = api::admin_router(state.clone());
    let mut admin_shutdown = shutdown_rx.clone();
    let admin_task = tokio::spawn(async move {
        axum::serve(admin, admin_app)
            .with_graceful_shutdown(async move {
                let _ = admin_shutdown.changed().await;
            })
            .await
    });
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    tracing::info!(listen=%config.listen,"Pinakes started");
    if config.pubky.enabled {
        pinakes_core::resolver::pubky_apps::log_links_off();
    }
    axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _=tokio::signal::ctrl_c()=>{},_=term.recv()=>{} }
        let _ = shutdown_tx.send(true);
    })
    .await?;
    admin_task.await??;
    ingest_task.await?;
    maintenance_task.await?;
    also_on_task.await?;
    dump_task.await?;
    group_task.await?;
    same_event_task.await?;
    same_recording_task.await?;
    while let Some(result) = workers.join_next().await {
        result?;
    }
    drop(instance_lock);
    pool.close().await;
    Ok(())
}

fn warn_admin_listener(address: std::net::SocketAddr) {
    if !address.ip().is_loopback() {
        tracing::warn!(admin_listen=%address, "admin listener is not loopback; bearer authentication is required, use TLS at the network edge");
    }
}

/// The daemon's JSON log: `filter` (`RUST_LOG`, else the default), except that the MCP library
/// logs nothing below warn whatever the filter says. rmcp logs whole requests and results at debug
/// and trace, and notifications at info: a tool's arguments, such as a private calendar URL, before
/// any validation.
fn subscriber<W>(
    filter: tracing_subscriber::EnvFilter,
    writer: W,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::{filter::FilterExt as _, layer::SubscriberExt as _, Layer as _};
    let quiet_mcp = tracing_subscriber::filter::filter_fn(|meta| {
        let target = meta.target();
        !(target == "rmcp" || target.starts_with("rmcp::")) || *meta.level() <= tracing::Level::WARN
    });
    tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_filter(filter.and(quiet_mcp)),
    )
}

enum Command {
    Serve,
    Migrate,
    Reextract(Vec<String>),
    Print(String),
    /// `parse-worker <cpu seconds>`: the HTML parse worker the daemon runs itself in.
    ParseWorker(u64),
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
                        "pinakesd [--config PATH] [--migrate | --openapi | reextract ROUTE…|all|also-on | parse-worker SECS]"
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
