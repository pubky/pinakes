use anyhow::{bail, Context, Result};
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    ingest, queue, store, MIGRATOR,
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
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await?;
    MIGRATOR.run(&pool).await?;
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
    let state = AppState::new(pool.clone(), config.clone())?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut workers = queue::spawn(
        pool.clone(),
        config.clone(),
        state.wake.clone(),
        shutdown_rx.clone(),
    )?;
    let ingest_state = state.clone();
    let mut ingest_shutdown = shutdown_rx.clone();
    let ingest_task = tokio::spawn(async move {
        loop {
            let applied = tokio::select! { _=ingest_shutdown.changed()=>break,result=ingest::poll_once(&ingest_state.pool,&ingest_state.config.nexus_url,&ingest_state.nexus)=>result };
            match applied {
                Ok(n) if n > 0 => {
                    tracing::info!(lines = n, "ingested Nexus batch");
                    ingest_state.wake.notify_waiters();
                    continue;
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error=%e,"ingestion will retry"),
            }
            if let Err(e) = store::cleanup(&ingest_state.pool, ingest_state.config.grace_days).await
            {
                tracing::warn!(error=%e,"cleanup failed");
            }
            tokio::select! { _=ingest_shutdown.changed()=>break,_=tokio::time::sleep(Duration::from_secs(ingest_state.config.poll_secs))=>{} }
        }
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
    ingest_task.await?;
    while let Some(result) = workers.join_next().await {
        result?;
    }
    drop(instance_lock);
    pool.close().await;
    Ok(())
}
