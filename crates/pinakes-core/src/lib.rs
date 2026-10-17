//! Pinakes indexes resource facts; Nexus remains the social source of truth.
pub mod adapter;
pub mod api;
pub mod canary;
pub mod change_api;
pub mod changes;
pub mod config;
pub mod event;
pub mod group_keys;
pub mod groups;
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

/// h3-pg added `h3_latlng_to_cell(point, integer)`, which clustering calls, in 4.2.3.
pub const H3_FUNCTION: &str = "h3_latlng_to_cell(point,integer)";
/// The startup verdict for the installed h3 extension: `None` when it is not installed, else its
/// `extversion` and whether `H3_FUNCTION` exists. The function decides, not the version string,
/// so development, release-candidate and source builds that have it are accepted.
pub fn h3_requirement(installed: Option<(&str, bool)>) -> anyhow::Result<()> {
    match installed {
        None => anyhow::bail!("the h3 extension is not installed; run `pinakesd --migrate`"),
        Some((found, false)) => anyhow::bail!(
            "h3 extension {found} is installed, but Pinakes needs {H3_FUNCTION}, added in h3 4.2.3. \
             Upgrade the h3 package, then run `ALTER EXTENSION h3 UPDATE;` (and \
             `ALTER EXTENSION h3_postgis UPDATE;` if installed) in this database: \
             CREATE EXTENSION IF NOT EXISTS never upgrades an installed extension"
        ),
        Some((_, true)) => Ok(()),
    }
}
/// Refuse to serve when the installed h3 extension lacks `H3_FUNCTION`. Migrations only run
/// `CREATE EXTENSION IF NOT EXISTS h3`, which never upgrades an installed extension.
pub async fn check_extensions(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let installed: Option<(String, bool)> = sqlx::query_as(
        "SELECT extversion,to_regprocedure($1) IS NOT NULL FROM pg_extension WHERE extname='h3'",
    )
    .bind(H3_FUNCTION)
    .fetch_optional(pool)
    .await?;
    h3_requirement(installed.as_ref().map(|(v, f)| (v.as_str(), *f)))
}

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
    // Preserve durable queue decisions before the already-published broad reroute.
    // The additive review migration consumes this crash-safe snapshot.
    sqlx::query(r#"DO $$ BEGIN
      IF to_regclass('resource') IS NOT NULL AND NOT EXISTS(
        SELECT 1 FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='event_fetch' AND column_name='adapter_cursor') THEN
        CREATE TABLE IF NOT EXISTS adapter_routing_backup AS
          SELECT id,attempts,dead_letter,coalesce((to_jsonb(resource)->>'final')::boolean,false) AS final,next_attempt_at FROM resource;
      END IF;
    END $$"#).execute(pool).await?;
    MIGRATOR.run(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn h3_without_the_function_is_refused_with_the_version_and_the_remedy() {
        for found in ["4.2.3", "4.3.0-dev", "4.2.4-rc1", "unpackaged"] {
            super::h3_requirement(Some((found, true))).unwrap();
        }
        let error = super::h3_requirement(Some(("4.2.2", false)))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("h3 extension 4.2.2")
                && error.contains("h3_latlng_to_cell(point,integer)"),
            "{error}"
        );
        assert!(error.contains("ALTER EXTENSION h3 UPDATE"), "{error}");
        assert!(super::h3_requirement(None).is_err());
    }
}
