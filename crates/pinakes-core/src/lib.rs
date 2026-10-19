//! Pinakes indexes resource facts; Nexus remains the social source of truth.
pub mod adapter;
pub mod alias;
pub mod api;
pub mod canary;
pub mod change_api;
pub mod changes;
pub mod config;
pub mod event;
pub mod group_keys;
pub mod groups;
pub mod identity;
pub mod ingest;
pub mod maintenance;
pub mod model;
pub mod nexus;
pub mod osm_dump;
pub mod queue;
pub mod resolver;
pub mod scheduler;
pub mod source;
pub mod spatial;
pub mod store;
pub mod telemetry;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Create or update the schema (`migrations/`).
pub async fn migrate(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}
