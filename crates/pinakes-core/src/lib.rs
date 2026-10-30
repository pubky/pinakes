//! Pinakes indexes resource facts; Nexus remains the social source of truth.
pub mod adapter;
pub mod agents;
pub mod alias;
pub mod also_on;
pub mod api;
pub mod app_ids;
pub mod barcode;
pub mod canary;
pub mod change_api;
pub mod changes;
pub mod config;
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
pub mod wikipedia;

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
/// Refuse to serve when the installed h3 extension lacks `H3_FUNCTION`. The schema only runs
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

/// The process-wide source settings, read once at startup before any resolver or source runs: the
/// official sources' switches, and what each source takes from the configuration (its tokens;
/// `sources::configure`). Every process that resolves links calls it: the daemon and the e2e
/// driver.
pub fn configure_sources(config: &config::Config) {
    sources::configure(config);
}

/// Create the schema (`migrations/0001_schema.sql`) on an empty database; a database that has it
/// is left as it is. A database built from another version of the file is refused with the
/// remedy: until the first deployment a schema change edits that file, and databases are
/// recreated.
pub async fn migrate(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    use sqlx::migrate::MigrateError;
    match MIGRATOR.run(pool).await {
        Err(MigrateError::VersionMismatch(_) | MigrateError::VersionMissing(_)) => anyhow::bail!(
            "this database was built from another version of migrations/0001_schema.sql; \
             recreate it (dropdb, createdb, then pinakesd --migrate) as docs/operations.md \
             describes"
        ),
        result => Ok(result?),
    }
}

#[cfg(test)]
mod tests {
    /// Every database test runs `tests/fixtures/require_h3.sql` after the schema, which makes the
    /// startup check's test for `H3_FUNCTION`: an older h3 fails each one with the version found.
    #[test]
    fn every_database_test_requires_the_h3_function() {
        let fixture = include_str!("../tests/fixtures/require_h3.sql");
        assert!(fixture.contains(&format!("to_regprocedure('{}')", super::H3_FUNCTION)));
        assert!(fixture.contains("h3 4.2.3 or newer required, found %"));
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = vec![];
        for dir in ["src", "tests"] {
            rust_files(
                &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir),
                &mut files,
            );
        }
        let mut tests = 0;
        for file in files {
            // Without whitespace, so an attribute with spaces inside it, or one rustfmt spreads over
            // several lines, reads alike. The needle is built here so this test's own text has none.
            let text: String = std::fs::read_to_string(&file)
                .unwrap()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let needle = ["#[sqlx", "::test("].concat();
            for (at, _) in text.match_indices(&needle) {
                let attribute = &text[at..];
                let attribute = &attribute[..attribute.find(")]").unwrap()];
                if attribute.contains("migrations=false") {
                    continue;
                }
                tests += 1;
                // Anywhere among its fixtures.
                assert!(
                    attribute.contains("require_h3.sql"),
                    "{}: {attribute}",
                    file.display()
                );
            }
        }
        assert!(tests > 100, "{tests}");
    }
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
