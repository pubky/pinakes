//! Pinakes indexes resource facts; Nexus remains the social source of truth.
pub mod api;
pub mod config;
pub mod ingest;
pub mod model;
pub mod nexus;
pub mod queue;
pub mod resolver;
pub mod store;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
