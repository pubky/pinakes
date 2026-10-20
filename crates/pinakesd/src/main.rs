use anyhow::{bail, Context, Result};
use futures_util::FutureExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    ingest, migrate, queue,
};
use std::{path::PathBuf, time::Duration};
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pinakes=info,pinakes_core=info,sqlx=warn".into()),
        )
        .init();
    let (config, command) = parse_args(std::env::args().skip(1))?;
    let (migrate_only, reextract) = match command {
        Command::Serve => (false, None),
        Command::Migrate => (true, None),
        Command::Reextract(routes) => (false, Some(routes)),
        Command::Print(text) => {
            println!("{text}");
            return Ok(());
        }
    };
    let config = Config::load(config.as_deref())?;
    pinakes_core::sources::configure(&config.sources);
    pinakes_core::sources::discogs::configure(&config);
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
                Err(e) => tracing::warn!(error=%e,"ingestion will retry"),
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
                Err(error) => tracing::warn!(%error,"group reconciliation will retry"),
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

enum Command {
    Serve,
    Migrate,
    Reextract(Vec<String>),
    Print(String),
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
            "reextract" => routes = Some(vec![]),
            "--openapi" => {
                return Ok((
                    config,
                    Command::Print(serde_json::to_string_pretty(&api::openapi_document())?),
                ))
            }
            "--help" => {
                return Ok((
                    config,
                    Command::Print(
                        "pinakesd [--config PATH] [--migrate | --openapi | reextract ROUTE…|all|also-on]"
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
