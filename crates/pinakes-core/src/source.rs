use anyhow::Result;
use sqlx::PgPool;

/// The classes a source's last error is stored as (`source_state.last_error`, whose
/// `source_error_class` allows exactly these). Any other failure is stored as `parse`.
pub const ERROR_CLASSES: &[&str] = &[
    "dns",
    "connect",
    "timeout",
    "http_4xx",
    "http_5xx",
    "parse",
    "policy",
    "too_large",
    "too_complex",
];

pub async fn outcome(pool: &PgPool, source: &str, error: Option<&str>) -> Result<()> {
    let error = error.map(|value| {
        if ERROR_CLASSES.contains(&value) {
            value
        } else {
            "parse"
        }
    });
    sqlx::query("INSERT INTO source_state(source,last_success_at,last_error,consecutive_failures) VALUES($1,CASE WHEN $2::text IS NULL THEN now() END,$2,CASE WHEN $2 IS NULL THEN 0 ELSE 1 END) ON CONFLICT(source) DO UPDATE SET last_success_at=CASE WHEN $2 IS NULL THEN now() ELSE source_state.last_success_at END,last_error=$2,consecutive_failures=CASE WHEN $2 IS NULL THEN 0 ELSE source_state.consecutive_failures+1 END")
        .bind(source).bind(error).execute(pool).await?;
    Ok(())
}
