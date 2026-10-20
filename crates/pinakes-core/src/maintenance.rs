use sqlx::PgPool;
use std::time::Duration;
use tokio::sync::watch;

/// An independent timer cannot be starved by a continuously nonempty ingestion feed.
pub async fn run(
    pool: PgPool,
    grace_days: u32,
    retention_days: u32,
    period: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The search revision sums one row per write transaction since the last fold.
    let mut fold = tokio::time::interval(Duration::from_secs(60));
    fold.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            _=fold.tick()=>{
                if let Err(e) = crate::store::compact_search_writes(&pool).await {
                    tracing::warn!(error=%e,"search revision fold failed");
                }
                continue;
            }
            _=tick.tick()=>{}
        }
        if let Err(e) = crate::changes::retain(&pool, retention_days).await {
            tracing::warn!(error=%e,"change retention failed");
        }
        if let Err(e) = crate::store::cleanup(&pool, grace_days).await {
            tracing::warn!(error=%e,"cleanup failed");
        }
    }
}
