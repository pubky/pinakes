//! Pinakes indexes resource facts; Nexus remains the social source of truth.
pub mod adapter;
pub mod api;
pub mod canary;
pub mod change_api;
pub mod changes;
pub mod config;
pub mod ingest;
pub mod maintenance;
pub mod model;
pub mod nexus;
pub mod queue;
pub mod resolver;
pub mod scheduler;
pub mod source;
pub mod spatial;
pub mod store;
pub mod telemetry;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
