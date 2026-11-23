//! Pinakes indexes resource facts; Nexus remains the social source of truth.
pub mod adapter;
pub mod agents;
pub mod alias;
pub mod also_on;
pub mod api;
pub mod app_ids;
pub mod barcode;
pub mod canary;
pub mod capability;
pub mod change_api;
pub mod changes;
pub mod collections;
pub mod config;
pub mod embed;
pub mod episodes;
pub mod event;
pub mod group_keys;
pub mod groups;
pub mod hints;
pub mod hubs;
pub mod identity;
pub mod ingest;
pub mod kinds;
pub mod locks;
pub mod lookups;
pub mod maintenance;
pub mod matching;
pub mod model;
pub mod music;
pub mod music_enrichment;
mod music_identity;
pub mod music_sources;
pub mod nexus;
pub mod osm_dump;
pub mod podcast_index_api;
pub mod queue;
pub mod recipes;
pub mod resolver;
pub mod same_event;
pub mod same_recording;
pub mod scheduler;
pub mod source;
pub mod sources;
pub mod spatial;
pub mod store;
pub mod telemetry;
pub mod wikimedia;
pub mod wikipedia;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// The process-wide source settings, read once at startup before any resolver or source runs: the
/// official sources' switches, and what each source takes from the configuration (its tokens;
/// `sources::configure`). Every process that resolves links calls it: the daemon and the e2e
/// driver.
pub fn configure_sources(config: &config::Config) {
    sources::configure(config);
}

/// Create the schema (`migrations/0001_schema.sql`) on an empty database; a database that has it
/// is left as it is. A database built from another version of the file is refused, with where
/// the way on is described: nothing is altered or dropped.
pub async fn migrate(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    use sqlx::migrate::MigrateError;
    match MIGRATOR.run(pool).await {
        Err(MigrateError::VersionMismatch(_) | MigrateError::VersionMissing(_)) => anyhow::bail!(
            "this database was built from another version of migrations/0001_schema.sql and is \
             left as it is; docs/operations.md (Database) says how to move to a database of this \
             version"
        ),
        result => Ok(result?),
    }
}
