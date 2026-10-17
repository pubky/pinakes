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
pub const MIN_H3: [u32; 3] = [4, 2, 3];
/// Whether an h3 `extversion` ("4.2.3", "4.3", …) is at least `MIN_H3`; unparseable is not.
pub fn h3_supported(version: &str) -> bool {
    let parts: Option<Vec<u32>> = version.trim().split('.').map(|p| p.parse().ok()).collect();
    match parts {
        Some(parts) if (1..=3).contains(&parts.len()) => {
            let mut padded = [0u32; 3];
            padded[..parts.len()].copy_from_slice(&parts);
            padded >= MIN_H3
        }
        _ => false,
    }
}
/// The startup verdict for the installed h3 `extversion` (`None`: not installed).
pub fn h3_requirement(installed: Option<&str>) -> anyhow::Result<()> {
    match installed {
        None => anyhow::bail!("the h3 extension is not installed; run `pinakesd --migrate`"),
        Some(found) if !h3_supported(found) => anyhow::bail!(
            "h3 extension {found} is installed, but Pinakes needs h3 >= {}.{}.{} \
             (h3_latlng_to_cell(point, integer)). Upgrade the h3 package, then run \
             `ALTER EXTENSION h3 UPDATE;` (and `ALTER EXTENSION h3_postgis UPDATE;` if installed) \
             in this database: CREATE EXTENSION IF NOT EXISTS never upgrades an installed extension",
            MIN_H3[0],
            MIN_H3[1],
            MIN_H3[2]
        ),
        Some(_) => Ok(()),
    }
}
/// Refuse to serve on an h3 extension older than `MIN_H3`. Migrations only run
/// `CREATE EXTENSION IF NOT EXISTS h3`, which never upgrades an installed extension.
pub async fn check_extensions(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let installed: Option<String> =
        sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname='h3'")
            .fetch_optional(pool)
            .await?;
    h3_requirement(installed.as_deref())
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
    fn h3_below_4_2_3_is_refused_with_the_remedy() {
        for ok in ["4.2.3", "4.2.10", "4.3", "4.3.0", "5", "10.0.0", " 4.2.3 "] {
            assert!(super::h3_supported(ok), "{ok}");
        }
        for old in [
            "4.2.2",
            "4.2",
            "4.1.9",
            "3.9.9",
            "4",
            "",
            "4.2.x",
            "unpackaged",
            "4.2.3.1",
            "4..3",
        ] {
            assert!(!super::h3_supported(old), "{old}");
        }
        let error = super::h3_requirement(Some("4.2.2"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("h3 extension 4.2.2") && error.contains(">= 4.2.3"),
            "{error}"
        );
        assert!(error.contains("ALTER EXTENSION h3 UPDATE"), "{error}");
        assert!(super::h3_requirement(None).is_err());
        super::h3_requirement(Some("4.2.3")).unwrap();
    }
}
