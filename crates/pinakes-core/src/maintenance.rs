use sqlx::PgPool;
use std::time::Duration;
use tokio::sync::watch;

/// An independent timer cannot be starved by a continuously nonempty ingestion feed.
pub async fn run(
    pool: PgPool,
    grace_days: u32,
    preview_grace_hours: u32,
    retention_days: u32,
    prune_batch: i64,
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
        let retained = crate::changes::retain(&pool, retention_days).await;
        crate::telemetry::background_pass("retention", &retained);
        if let Err(e) = retained {
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
        crate::telemetry::background_pass("cleanup", &cleaned);
        if let Err(e) = cleaned {
            tracing::warn!(error=%crate::resolver::log_error(&e),"cleanup failed");
        }
        if *stop.borrow() {
            break;
        }
        // After the cleanup: the places only deleted cards named go too.
        let pruned = crate::store::prune_osm_places(&pool).await;
        crate::telemetry::background_pass("osm_prune", &pruned);
        if let Err(e) = pruned {
            tracing::warn!(error=%crate::resolver::log_error(&e),"pruning cached OSM objects failed");
        }
        let pruned = crate::also_on::prune(&pool, prune_batch).await;
        crate::telemetry::background_pass("also_on_prune", &pruned);
        if let Err(e) = pruned {
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
        crate::telemetry::background_pass("refill", &refilled);
        if let Err(e) = refilled {
            tracing::warn!(error=%crate::resolver::log_error(&e),"taking stale values out of cards failed");
        }
        // A card that became final or dead-lettered since the start while it held what a
        // switched-off also-on source wrote: nothing to read unless that happened (its marker).
        let unhinted = crate::also_on::unhint(&pool).await;
        crate::telemetry::background_pass("unhint", &unhinted);
        if let Err(e) = unhinted {
            tracing::warn!(error=%crate::resolver::log_error(&e),"taking switched-off sources' values out of cards no refresh reads failed");
        }
    }
}
/// Fold the search revision's rows (`store::compact_search_writes`).
async fn compact(pool: &PgPool) {
    let folded = crate::store::compact_search_writes(pool).await;
    crate::telemetry::background_pass("search_fold", &folded);
    if let Err(e) = folded {
        tracing::warn!(error=%crate::resolver::log_error(&e),"search revision fold failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A maintenance pass whose steps fail (here the database is gone) counts each failed step
    /// in `pinakes_background_failures_total`, so a step that fails every hour shows, and its
    /// last success stands still.
    #[sqlx::test(migrations = "../../migrations")]
    async fn each_failed_maintenance_step_is_counted(pool: PgPool) {
        let steps = [
            "retention",
            "cleanup",
            "osm_prune",
            "also_on_prune",
            "search_fold",
        ];
        let failures = || {
            steps
                .iter()
                .map(|s| crate::telemetry::background_failures(s))
                .collect::<Vec<_>>()
        };
        let before = failures();
        pool.close().await;
        let (stop, shutdown) = watch::channel(false);
        let pass = tokio::spawn(run(
            pool,
            7,
            24,
            30,
            1000,
            Duration::from_secs(3600),
            shutdown,
        ));
        for _ in 0..200 {
            if failures().iter().zip(&before).all(|(now, was)| now > was) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        stop.send(true).unwrap();
        pass.await.unwrap();
        for ((step, now), was) in steps.iter().zip(failures()).zip(before) {
            assert!(now > was, "{step}: {was} -> {now}");
        }
    }
}
