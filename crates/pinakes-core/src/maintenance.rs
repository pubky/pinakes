use sqlx::PgPool;
use std::time::Duration;
use tokio::sync::watch;

/// An independent timer cannot be starved by a continuously nonempty ingestion feed.
pub async fn run(
    pool: PgPool,
    grace_days: u32,
    preview_grace_hours: u32,
    retention_days: u32,
    period: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The search revision sums one row per write transaction since the last fold.
    let mut fold = tokio::time::interval(Duration::from_secs(60));
    fold.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let stop = shutdown.clone();
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            _=fold.tick()=>{
                compact(&pool).await;
                continue;
            }
            _=tick.tick()=>{}
        }
        if let Err(e) = crate::changes::retain(&pool, retention_days).await {
            tracing::warn!(error=%crate::resolver::log_error(&e),"change retention failed");
        }
        // Cleanup may run for minutes: the fold keeps its minute meanwhile, and shutdown stops it
        // between two batches.
        let cleanup =
            crate::changes::delete_orphans_within(&pool, grace_days, preview_grace_hours, || {
                *stop.borrow()
            });
        tokio::pin!(cleanup);
        let cleaned = loop {
            tokio::select! {
                result=&mut cleanup=>break result,
                _=fold.tick()=>compact(&pool).await,
            }
        };
        if let Err(e) = cleaned {
            tracing::warn!(error=%crate::resolver::log_error(&e),"cleanup failed");
        }
        if *stop.borrow() {
            break;
        }
        // After the cleanup: the places only deleted cards named go too.
        if let Err(e) = crate::store::prune_osm_places(&pool).await {
            tracing::warn!(error=%crate::resolver::log_error(&e),"pruning cached OSM objects failed");
        }
        if let Err(e) = crate::also_on::prune(&pool).await {
            tracing::warn!(error=%crate::resolver::log_error(&e),"also-on pruning failed");
        }
        // After the pruning: a card loses what its sources' expired facts gave it. It reads every
        // card that holds such values: the fold keeps its minute meanwhile.
        let refill = crate::also_on::refill(&pool, || *stop.borrow());
        tokio::pin!(refill);
        let refilled = loop {
            tokio::select! {
                result=&mut refill=>break result,
                _=fold.tick()=>compact(&pool).await,
            }
        };
        if let Err(e) = refilled {
            tracing::warn!(error=%crate::resolver::log_error(&e),"taking stale values out of cards failed");
        }
        // A card that became final or dead-lettered since the start while it held what a
        // switched-off also-on source wrote: nothing to read unless that happened (its marker).
        let unhinted = crate::also_on::unhint(&pool).await;
        if let Err(e) = unhinted {
            tracing::warn!(error=%crate::resolver::log_error(&e),"taking switched-off sources' values out of cards no refresh reads failed");
        }
    }
}
/// Fold the search revision's rows (`store::compact_search_writes`).
async fn compact(pool: &PgPool) {
    if let Err(e) = crate::store::compact_search_writes(pool).await {
        tracing::warn!(error=%crate::resolver::log_error(&e),"search revision fold failed");
    }
}
