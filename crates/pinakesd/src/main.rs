use anyhow::{bail, Context, Result};
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
    let mut args = std::env::args().skip(1);
    let mut config = None;
    let mut migrate_only = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config = Some(PathBuf::from(
                    args.next().context("--config requires a TOML path")?,
                ))
            }
            "--migrate" => migrate_only = true,
            "--openapi" => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&api::openapi_document())?
                );
                return Ok(());
            }
            "--help" => {
                println!("pinakesd [--config PATH] [--migrate | --openapi]");
                return Ok(());
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    let config = Config::load(config.as_deref())?;
    pinakes_core::resolver::interstitial::install(config.walls()?);
    warn_webcal_fallback(config.event.webcal_http_fallback);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await?;
    migrate(&pool).await?;
    if migrate_only {
        return Ok(());
    }
    // Host, author and IP budgets are process-local; one daemon owns this database.
    let mut instance_lock = pool.acquire().await?;
    let owns_database: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(73822917)")
        .fetch_one(&mut *instance_lock)
        .await?;
    if !owns_database {
        bail!("another pinakesd already owns this database");
    }
    // Only the lock owner may fence leases; a routing change takes effect within the hour.
    let rerouted = queue::reclassify_routes(&pool).await?;
    if rerouted > 0 {
        tracing::info!(
            rerouted,
            "updated routing hints to the current classification"
        );
    }
    let requeued = queue::requeue_rerouted(&pool).await?;
    if requeued > 0 {
        tracing::info!(requeued, "re-queued documents saved under another resolver");
    }
    let remodelled = queue::apply_calendar_settings(&pool, &config.event).await?;
    if remodelled > 0 {
        tracing::info!(
            remodelled,
            "a calendar expansion setting changed: calendars re-parse once"
        );
    }
    let capped = pinakes_core::store::cap_derivation_expiry(&pool, &config.event).await?;
    if capped > 0 {
        tracing::info!(capped, "capped derived-link expiries at the event horizon");
    }
    let extended = pinakes_core::store::extend_derivations(
        &pool,
        pinakes_core::store::derivation_horizon(&config.event),
    )
    .await?;
    if extended > 0 {
        tracing::info!(
            extended,
            "extended derived links whose event entered the horizon"
        );
    }
    if config.queue.reextract_on_upgrade {
        let reextract = queue::reextract_outdated(
            &pool,
            config.queue.reextract_window_secs,
            &pinakes_core::resolver::extractor_versions(),
        )
        .await?;
        if reextract > 0 {
            tracing::info!(
                reextract,
                "re-queued documents from an older extractor version"
            );
        }
    }
    pinakes_core::groups::configure(&pool, &config.groups).await?;
    let api_pool = api::pool_options().connect(&config.database_url).await?;
    let state = AppState::new(api_pool, config.clone())?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut workers = queue::spawn(
        pool.clone(),
        config.clone(),
        state.wake.clone(),
        shutdown_rx.clone(),
        state.metrics.clone(),
    )?;
    let ingest_state = state.clone();
    let ingest_pool = pool.clone();
    let mut ingest_shutdown = shutdown_rx.clone();
    let ingest_task = tokio::spawn(async move {
        loop {
            let applied = tokio::select! { _=ingest_shutdown.changed()=>break,result=ingest::poll_once(&ingest_pool,&ingest_state.config.nexus_url,&ingest_state.nexus,&ingest_state.config.ingest,&ingest_state.metrics)=>result };
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
    let maintenance_task = tokio::spawn(pinakes_core::maintenance::run(
        pool.clone(),
        config.grace_days,
        config.refresh.retention_days,
        Duration::from_secs(3600),
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
    group_task.await?;
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

fn warn_webcal_fallback(enabled: bool) {
    if enabled {
        tracing::warn!("webcal_http_fallback is enabled: webcal feeds may use plaintext HTTP and accept content modified by an active network attacker");
    }
}

#[cfg(test)]
mod tests {
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
    #[test]
    fn warns_only_when_plaintext_webcal_fallback_is_enabled() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            warn_webcal_fallback(false);
            assert!(output.lock().unwrap().is_empty());
            warn_webcal_fallback(true);
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("plaintext HTTP") && logs.contains("active network attacker"));
    }
}
