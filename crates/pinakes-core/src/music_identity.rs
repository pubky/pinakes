//! Read-side reuse within an already reconciled exact music group. No new identity edges,
//! no copied evidence: removing or expiring a source still retracts its keys.
use crate::{
    api::{ApiError, Card, ResourceDetail},
    model::{CreditField, CreditProvider, ResourceMeta},
};
use serde_json::Value;
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

/// Reuse an exact same-type member's open-database artwork, with its whole image credit.
/// No storefront artwork or descriptions are copied, and a card's own image always wins.
/// This is a read-time fallback: leaving the group retracts it without a document rewrite.
pub(crate) async fn images(pool: &PgPool, cards: &mut [&mut Card]) -> Result<(), ApiError> {
    let ids: Vec<&str> = cards
        .iter()
        .filter(|c| {
            c.image.is_none()
                && c.status != "missing"
                && c.group.is_some()
                && music(c.type_name.as_deref())
        })
        .map(|c| c.id.as_str())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "WITH wanted AS (SELECT r.id,r.type,g.group_id FROM resource r \
         JOIN group_unit g ON g.unit_id=r.id WHERE r.id=ANY($1) \
         AND r.doc->>'status' IS DISTINCT FROM 'missing') \
         SELECT w.id,d.doc FROM wanted w CROSS JOIN LATERAL ( \
          SELECT r.doc FROM (SELECT resource_id FROM group_unit WHERE group_id=w.group_id \
            ORDER BY resource_id LIMIT 256) u JOIN resource r ON r.id=u.resource_id \
          WHERE r.type=w.type AND r.status='ok' AND r.doc->>'status' IS DISTINCT FROM 'missing' \
          AND r.doc->>'image' IS NOT NULL AND ( \
            r.doc->'credits' @> '[{\"field\":\"image\",\"provider\":\"wikimedia_commons\"}]'::jsonb \
            OR r.doc->'credits' @> '[{\"field\":\"image\",\"provider\":\"cover_art_archive\"}]'::jsonb) \
          ORDER BY (r.doc->'credits' @> '[{\"field\":\"image\",\"provider\":\"wikimedia_commons\"}]'::jsonb) DESC,r.id \
          LIMIT 1) d",
    ).bind(ids).fetch_all(pool).await?;
    let docs: std::collections::BTreeMap<String, ResourceMeta> = rows
        .into_iter()
        .filter_map(|(id, doc)| serde_json::from_value(doc).ok().map(|doc| (id, doc)))
        .collect();
    for card in cards {
        let Some(doc) = docs.get(&card.id) else {
            continue;
        };
        let credits: Vec<_> = doc
            .credits
            .iter()
            .filter(|c| {
                c.field == CreditField::Image
                    && matches!(
                        c.provider,
                        CreditProvider::WikimediaCommons | CreditProvider::CoverArtArchive
                    )
            })
            .filter_map(|c| c.clone().bounded())
            .collect();
        if card.image.is_none() && !credits.is_empty() {
            card.image.clone_from(&doc.image);
            card.credits.retain(|c| c.field != CreditField::Image);
            card.credits.extend(credits);
        }
    }
    Ok(())
}
