use anyhow::Result;
use sqlx::PgPool;

pub async fn outcome(pool: &PgPool, source: &str, error: Option<&str>) -> Result<()> {
    let error = error.map(|value| match value {
        "dns" | "connect" | "timeout" | "http_4xx" | "http_5xx" | "parse" | "policy" => value,
        _ => "parse",
    });
    sqlx::query("INSERT INTO source_state(source,last_success_at,last_error,consecutive_failures) VALUES($1,CASE WHEN $2::text IS NULL THEN now() END,$2,CASE WHEN $2 IS NULL THEN 0 ELSE 1 END) ON CONFLICT(source) DO UPDATE SET last_success_at=CASE WHEN $2 IS NULL THEN now() ELSE source_state.last_success_at END,last_error=$2,consecutive_failures=CASE WHEN $2 IS NULL THEN 0 ELSE source_state.consecutive_failures+1 END")
        .bind(source).bind(error).execute(pool).await?;
    Ok(())
}
pub async fn cursor(pool: &PgPool, source: &str, cursor: Option<&str>, reset: bool) -> Result<()> {
    sqlx::query("INSERT INTO source_state(source,cursor,cursor_resets_total) VALUES($1,$2,$3) ON CONFLICT(source) DO UPDATE SET cursor=$2,cursor_resets_total=source_state.cursor_resets_total+$3")
        .bind(source).bind(cursor).bind(i64::from(reset)).execute(pool).await?;
    Ok(())
}
