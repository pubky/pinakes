use anyhow::{ensure, Result};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet};

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
#[derive(Debug, Serialize)]
pub struct KeyChanges {
    pub held: bool,
    pub created: Vec<String>,
    pub cancelled: Vec<String>,
}
/// Adapter-neutral key-set guard. Future container parsers apply only returned per-key changes.
pub async fn container_keys(
    pool: &PgPool,
    source: &str,
    keys: &[String],
    retry_secs: u64,
) -> Result<KeyChanges> {
    ensure!(
        keys.len() <= 10000 && keys.iter().all(|k| k.len() <= 2048),
        "container key set exceeds cap"
    );
    let keys = keys.iter().cloned().collect::<BTreeSet<_>>();
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO source_state(source) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(source)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT keys,held_keys,missing_keys FROM source_state WHERE source=$1 FOR UPDATE",
    )
    .bind(source)
    .fetch_one(&mut *tx)
    .await?;
    let old = row
        .try_get::<Vec<String>, _>("keys")?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let held = row
        .try_get::<Option<Vec<String>>, _>("held_keys")?
        .map(|v| v.into_iter().collect::<BTreeSet<_>>());
    let losses = old.difference(&keys).cloned().collect::<Vec<_>>();
    let repeated = held.as_ref() == Some(&keys);
    if losses.len() * 2 > old.len() && !repeated {
        sqlx::query("UPDATE source_state SET held_keys=$2,held_until=now()+$3*interval '1 second',feed_held_total=feed_held_total+1 WHERE source=$1")
            .bind(source).bind(keys.iter().cloned().collect::<Vec<_>>()).bind(retry_secs as f64).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(KeyChanges {
            held: true,
            created: vec![],
            cancelled: vec![],
        });
    }
    let mut missing: BTreeMap<String, u32> =
        serde_json::from_value(row.try_get::<Value, _>("missing_keys")?)?;
    missing.retain(|key, _| !keys.contains(key));
    let mut cancelled = Vec::new();
    for key in losses {
        let count = missing.entry(key.clone()).or_default();
        *count += if repeated { 2 } else { 1 };
        if *count >= 2 {
            cancelled.push(key);
        }
    }
    let created = keys.difference(&old).cloned().collect();
    let mut applied = keys.clone();
    for key in missing.keys() {
        if !cancelled.contains(key) {
            applied.insert(key.clone());
        }
    }
    missing.retain(|key, _| !cancelled.contains(key));
    sqlx::query("UPDATE source_state SET keys=$2,missing_keys=$3,held_keys=NULL,held_until=NULL,last_success_at=now(),last_error=NULL,consecutive_failures=0 WHERE source=$1")
        .bind(source).bind(applied.into_iter().collect::<Vec<_>>()).bind(json!(missing)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(KeyChanges {
        held: false,
        created,
        cancelled,
    })
}
