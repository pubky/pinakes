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

/// Preserve applied migration checksums while preparing legacy web-fetched OSM rows for 0002.
pub async fn migrate(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    sqlx::query(r#"DO $$ BEGIN
      IF to_regclass('resource') IS NOT NULL AND NOT EXISTS(
        SELECT 1 FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='resource' AND column_name='osm_id') THEN
        UPDATE resource SET resolver=NULL,name=NULL,point=NULL,category=NULL,type=NULL,type_path='{}',
          start_at=NULL,end_at=NULL,venue_id=NULL,doc='{}',status='pending',resolved_at=NULL,refresh_at=NULL,
          attempts=0,dead_letter=false,last_error=NULL,lease_until=NULL,generation=generation+1,next_attempt_at=now()
        WHERE uri ~ '^https://www[.]openstreetmap[.]org/(node|way|relation)/[1-9][0-9]{0,17}$';
      END IF;
    END $$"#).execute(pool).await?;
    MIGRATOR.run(pool).await?;
    Ok(())
}
