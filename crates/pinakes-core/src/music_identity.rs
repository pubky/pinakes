//! Read-side reuse within an already reconciled exact music group. No new identity edges,
//! no copied evidence: removing or expiring a source still retracts its keys.
use crate::api::{ApiError, ResourceDetail};
use sqlx::PgPool;

pub(crate) fn music(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("MusicRecording" | "MusicAlbum" | "MusicGroup" | "MusicPlaylist")
    )
}

pub(crate) async fn keys(pool: &PgPool, detail: &mut ResourceDetail) -> Result<(), ApiError> {
    if detail.card.group.is_none() || !music(detail.card.type_name.as_deref()) {
        return Ok(());
    }
    // The group engine has already checked conflicts and its size cap. Keep a read bound too.
    // Include the incoming keyed links: they are evidence even if their target has no own key.
    let keys: Vec<String> = sqlx::query_scalar(
        "WITH owners AS (SELECT u.resource_id FROM group_unit own JOIN group_unit u USING(group_id) \
         JOIN resource r ON r.id=u.resource_id WHERE own.unit_id=$1 AND r.type=$2 \
         ORDER BY u.resource_id LIMIT 256), keys AS ( \
         SELECT k.key FROM resource_key k JOIN owners o USING(resource_id) \
         WHERE k.expires_at>now() AND k.source NOT IN (SELECT source FROM also_on_source_off) \
         UNION SELECT l.key FROM derived_link l JOIN owners o ON o.resource_id=l.link_id \
         WHERE l.key IS NOT NULL AND l.kind IS DISTINCT FROM 'maybe' AND l.expires_at>now() \
         AND l.source NOT IN (SELECT source FROM also_on_source_off)) \
         SELECT key FROM keys ORDER BY key COLLATE \"C\" LIMIT 256",
    ).bind(&detail.card.id).bind(&detail.card.type_name).fetch_all(pool).await?;
    detail.details.keys.extend(keys);
    detail.details.keys.sort();
    detail.details.keys.dedup();
    Ok(())
}
