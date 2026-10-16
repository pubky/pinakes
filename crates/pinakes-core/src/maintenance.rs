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
    loop {
        tokio::select! { _=shutdown.changed()=>break, _=tick.tick()=>{} }
        if let Err(e) = crate::changes::retain(&pool, retention_days).await {
            tracing::warn!(error=%e,"change retention failed");
        }
        if let Err(e) = crate::store::cleanup(&pool, grace_days).await {
            tracing::warn!(error=%e,"cleanup failed");
        }
    }
}
