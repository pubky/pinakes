use anyhow::{bail, Context, Result};
use axum::{routing::get, Json, Router};
use pinakes_core::{config::Config, MIGRATOR};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut migrate_only = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                path = Some(PathBuf::from(
                    args.next().context("--config requires a path")?,
                ))
            }
            "--migrate" => migrate_only = true,
            _ => bail!("unknown argument {arg}"),
        }
    }
    let config = Config::load(path.as_deref())?;
    let pool = sqlx::PgPool::connect(&config.database_url).await?;
    MIGRATOR.run(&pool).await?;
    if migrate_only {
        return Ok(());
    }
    let app = Router::new().route(
        "/v1/info",
        get(|| async { Json(serde_json::json!({"version":env!("CARGO_PKG_VERSION")})) }),
    );
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    tracing::info!(listen=%config.listen, "Pinakes skeleton started");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
