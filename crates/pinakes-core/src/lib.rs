//! Pinakes indexes resource facts; Nexus remains the social source of truth.
pub mod adapter;
pub mod alias;
pub mod also_on;
pub mod api;
pub mod canary;
pub mod change_api;
pub mod changes;
pub mod config;
pub mod event;
pub mod group_keys;
pub mod groups;
pub mod hubs;
pub mod identity;
pub mod ingest;
pub mod locks;
pub mod maintenance;
pub mod model;
pub mod music;
pub mod nexus;
pub mod osm_dump;
pub mod queue;
pub mod recipes;
pub mod resolver;
pub mod same_event;
pub mod scheduler;
pub mod source;
pub mod sources;
pub mod spatial;
pub mod store;
pub mod telemetry;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// h3-pg added `h3_latlng_to_cell(point, integer)`, which clustering calls, in 4.2.3.
pub const H3_FUNCTION: &str = "h3_latlng_to_cell(point,integer)";
/// What startup found of h3: its `extversion`, whether `H3_FUNCTION` resolves on this
/// connection's `search_path`, and the extension's schema when the function exists there.
pub struct H3Install<'a> {
    pub version: &'a str,
    pub callable: bool,
    pub schema_with_function: Option<&'a str>,
}
/// The startup verdict for the installed h3 extension (`None`: not installed). The function
/// decides, not the version string, so development, release-candidate and source builds that
/// have it are accepted. A function outside `search_path` is reported as such, not as too old.
pub fn h3_requirement(installed: Option<H3Install<'_>>) -> anyhow::Result<()> {
    match installed {
        None => anyhow::bail!("the h3 extension is not installed; run `pinakesd --migrate`"),
        Some(H3Install { callable: true, .. }) => Ok(()),
        Some(H3Install {
            version,
            schema_with_function: Some(schema),
            ..
        }) => anyhow::bail!(
            "h3 extension {version} provides {H3_FUNCTION} in schema {schema}, which is not on \
             this connection's search_path; add {schema} to the database's or role's search_path \
             (ALTER DATABASE … SET search_path = …), since Pinakes calls h3 functions unqualified"
        ),
        Some(H3Install { version, .. }) => anyhow::bail!(
            "h3 extension {version} is installed, but Pinakes needs {H3_FUNCTION}, added in h3 \
             4.2.3. Upgrade the h3 package, then run `ALTER EXTENSION h3 UPDATE;` (and \
             `ALTER EXTENSION h3_postgis UPDATE;` if installed) in this database: \
             CREATE EXTENSION IF NOT EXISTS never upgrades an installed extension"
        ),
    }
}
/// Refuse to serve when the installed h3 extension lacks `H3_FUNCTION`. Migrations only run
/// `CREATE EXTENSION IF NOT EXISTS h3`, which never upgrades an installed extension.
pub async fn check_extensions(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let installed: Option<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT e.extversion,to_regprocedure($1) IS NOT NULL,
           CASE WHEN to_regprocedure(quote_ident(n.nspname)||'.'||$1) IS NOT NULL THEN n.nspname END
         FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='h3'",
    )
    .bind(H3_FUNCTION)
    .fetch_optional(pool)
    .await?;
    h3_requirement(
        installed
            .as_ref()
            .map(|(version, callable, schema)| H3Install {
                version,
                callable: *callable,
                schema_with_function: schema.as_deref(),
            }),
    )
}

/// Create or update the schema (`migrations/`).
pub async fn migrate(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn h3_without_the_function_is_refused_with_the_version_and_the_remedy() {
        use super::{h3_requirement, H3Install};
        for version in ["4.2.3", "4.3.0-dev", "4.2.4-rc1", "unpackaged"] {
            h3_requirement(Some(H3Install {
                version,
                callable: true,
                schema_with_function: Some("public"),
            }))
            .unwrap();
        }
        let old = h3_requirement(Some(H3Install {
            version: "4.2.2",
            callable: false,
            schema_with_function: None,
        }))
        .unwrap_err()
        .to_string();
        assert!(
            old.contains("h3 extension 4.2.2") && old.contains("h3_latlng_to_cell(point,integer)"),
            "{old}"
        );
        assert!(old.contains("ALTER EXTENSION h3 UPDATE"), "{old}");
        let elsewhere = h3_requirement(Some(H3Install {
            version: "4.2.3",
            callable: false,
            schema_with_function: Some("extensions"),
        }))
        .unwrap_err()
        .to_string();
        assert!(
            elsewhere.contains("schema extensions") && elsewhere.contains("search_path"),
            "{elsewhere}"
        );
        assert!(!elsewhere.contains("Upgrade"), "{elsewhere}");
        assert!(h3_requirement(None).is_err());
    }
}
