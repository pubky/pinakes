//! Pinakes indexes resource facts; Nexus remains the social source of truth.
pub mod config;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
